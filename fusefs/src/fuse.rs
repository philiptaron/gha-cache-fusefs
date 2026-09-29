//! The `fuser` adapter. In-memory operations are answered on the FUSE thread;
//! anything that may touch the network is answered from a tokio task, so a
//! slow download never stalls the session.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::Duration;

use fuser::{
    FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request, TimeOrNow, WriteFlags,
};

use crate::vfs::{self, Attr, FileKind, RenameMode, SetAttr, SetTime, Vfs};

pub struct FuseFs {
    vfs: Vfs,
    rt: tokio::runtime::Handle,
    ttl: Duration,
}

impl FuseFs {
    pub fn new(vfs: Vfs, rt: tokio::runtime::Handle) -> FuseFs {
        FuseFs {
            vfs,
            rt,
            ttl: Duration::from_secs(1),
        }
    }
}

fn kind(k: FileKind) -> FileType {
    match k {
        FileKind::Dir => FileType::Directory,
        FileKind::File => FileType::RegularFile,
        FileKind::Symlink => FileType::Symlink,
    }
}

fn fattr(a: &Attr) -> FileAttr {
    FileAttr {
        ino: INodeNo(a.ino),
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: a.mtime,
        mtime: a.mtime,
        ctime: a.mtime,
        crtime: a.mtime,
        kind: kind(a.kind),
        perm: a.perm,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: 0,
        blksize: 128 * 1024,
        flags: 0,
    }
}

fn errno(e: vfs::Errno) -> fuser::Errno {
    fuser::Errno::from_i32(e.0)
}

fn name(n: &OsStr) -> Result<String, fuser::Errno> {
    n.to_str().map(str::to_string).ok_or(fuser::Errno::EINVAL)
}

fn time(t: TimeOrNow) -> SetTime {
    match t {
        TimeOrNow::Now => SetTime::Now,
        TimeOrNow::SpecificTime(t) => SetTime::At(t),
    }
}

macro_rules! try_reply {
    ($reply:ident, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return $reply.error(e),
        }
    };
}

impl Filesystem for FuseFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let wanted = InitFlags::FUSE_DO_READDIRPLUS
            | InitFlags::FUSE_READDIRPLUS_AUTO
            | InitFlags::FUSE_PARALLEL_DIROPS
            | InitFlags::FUSE_ATOMIC_O_TRUNC;
        let _ = config.add_capabilities(wanted & config.capabilities());
        let _ = config.set_max_background(64);
        if let Err(max) = config.set_max_readahead(1 << 20) {
            let _ = config.set_max_readahead(max);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEntry) {
        let Ok(n) = name(n) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let (vfs, ttl) = (self.vfs.clone(), self.ttl);
        self.rt.spawn(async move {
            match vfs.lookup(parent.0, &n).await {
                Ok(a) => reply.entry(&ttl, &fattr(&a), Generation(0)),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.vfs.getattr(ino.0) {
            Ok(a) => reply.attr(&self.ttl, &fattr(&a)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Ownership is always the mounting user; chown is accepted and ignored.
        let set = SetAttr {
            mode,
            size,
            mtime: mtime.map(time),
        };
        let (vfs, ttl) = (self.vfs.clone(), self.ttl);
        self.rt.spawn(async move {
            match vfs.setattr(ino.0, fh.map(|f| f.0), set).await {
                Ok(a) => reply.attr(&ttl, &fattr(&a)),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.readlink(ino.0).await {
                Ok(t) => reply.data(t.as_bytes()),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // mode_t is u32 on Linux and u16 on macOS.
        #[allow(clippy::unnecessary_cast)]
        let (ifmt, ifreg) = (libc::S_IFMT as u32, libc::S_IFREG as u32);
        if mode & ifmt != ifreg {
            return reply.error(fuser::Errno::EPERM);
        }
        let n = try_reply!(reply, name(n));
        let (a, fh) = try_reply!(
            reply,
            self.vfs
                .create(parent.0, &n, mode, libc::O_WRONLY | libc::O_EXCL)
                .map_err(errno)
        );
        let _ = self.vfs.release(fh);
        reply.entry(&self.ttl, &fattr(&a), Generation(0));
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let n = try_reply!(reply, name(n));
        match self.vfs.mkdir(parent.0, &n, mode) {
            Ok(a) => reply.entry(&self.ttl, &fattr(&a), Generation(0)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        let n = try_reply!(reply, name(n));
        match self.vfs.unlink(parent.0, &n) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        let n = try_reply!(reply, name(n));
        match self.vfs.rmdir(parent.0, &n) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let link = try_reply!(reply, name(link));
        let Some(target) = target.to_str() else {
            return reply.error(fuser::Errno::EINVAL);
        };
        match self.vfs.symlink(parent.0, &link, target) {
            Ok(a) => reply.entry(&self.ttl, &fattr(&a), Generation(0)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let n = try_reply!(reply, name(n));
        let newname = try_reply!(reply, name(newname));
        #[cfg(target_os = "linux")]
        let mode = {
            if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
                return reply.error(fuser::Errno::EINVAL);
            }
            if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                RenameMode::NoReplace
            } else {
                RenameMode::Replace
            }
        };
        #[cfg(not(target_os = "linux"))]
        let mode = {
            let _ = flags;
            RenameMode::Replace
        };
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.rename(parent.0, &n, newparent.0, &newname, mode).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.open(ino.0, flags.0).await {
                Ok((fh, keep)) => reply.opened(
                    FileHandle(fh),
                    if keep {
                        FopenFlags::FOPEN_KEEP_CACHE
                    } else {
                        FopenFlags::empty()
                    },
                ),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.read(fh.0, offset, size).await {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        // A local pwrite: cheap enough to do inline, and it avoids copying `data`.
        match self.vfs.write(fh.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.vfs.release(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let vfs = self.vfs.clone();
        self.rt.spawn(async move {
            match vfs.fsync(fh.0).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.vfs.opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = try_reply!(reply, self.vfs.readdir(ino.0, fh.0, offset).map_err(errno));
        for (i, e) in entries.iter().enumerate() {
            if reply.add(INodeNo(e.ino), offset + i as u64 + 1, kind(e.kind), &e.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let entries = try_reply!(reply, self.vfs.readdir(ino.0, fh.0, offset).map_err(errno));
        for (i, e) in entries.iter().enumerate() {
            if reply.add(
                INodeNo(e.ino),
                offset + i as u64 + 1,
                &e.name,
                &self.ttl,
                &fattr(&e.attr),
                Generation(0),
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.vfs.releasedir(fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        const BS: u64 = 4096;
        let (total, free, files) = self.vfs.statfs();
        reply.statfs(
            total / BS,
            free / BS,
            free / BS,
            files,
            1 << 32,
            BS as u32,
            255,
            BS as u32,
        );
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let n = try_reply!(reply, name(n));
        match self.vfs.create(parent.0, &n, mode, flags) {
            Ok((a, fh)) => reply.created(
                &self.ttl,
                &fattr(&a),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(errno(e)),
        }
    }
}
