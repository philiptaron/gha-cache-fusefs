//! Reading images.

use std::collections::HashSet;

use super::*;

/// Everything reachable from an image's root.
#[derive(Debug)]
pub struct Listing {
    /// Extra devices, in slot order, as `(tag, blocks)`; device 1 is the first.
    pub devices: Vec<(String, u64)>,
    /// Depth first in name order, the root first.
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Components joined by `/`; "" for the root.
    pub path: String,
    pub nid: u64,
    pub kind: EntryKind,
    pub meta: Meta,
    pub nlink: u32,
    pub size: u64,
    pub xattrs: Vec<(String, Vec<u8>)>,
    /// Where a regular file's bytes are; `Data::None` for everything else.
    pub data: Data,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink(String),
    CharDevice(u32),
    BlockDevice(u32),
    Fifo,
    Socket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    None,
    /// `blocks` whole blocks from block `start` of the image, then `tail`.
    Flat {
        start: u64,
        blocks: u64,
        tail: Vec<u8>,
    },
    /// Chunks of `chunk_size` bytes, the last one short.
    Chunks {
        chunk_size: u64,
        chunks: Vec<Chunk>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// 0 is the image itself; extra devices count from 1.
    pub device: u16,
    /// The first block, or `None` for a hole.
    pub start: Option<u64>,
}

impl Entry {
    /// Whether this is an overlayfs whiteout.
    pub fn is_whiteout(&self) -> bool {
        self.kind == EntryKind::CharDevice(0)
    }

    pub fn xattr(&self, name: &str) -> Option<&[u8]> {
        self.xattrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }
}

impl Data {
    /// A flat file's bytes, from an image that holds them.
    pub fn bytes(&self, image: &[u8], size: u64) -> Result<Vec<u8>> {
        match self {
            Data::None => Ok(Vec::new()),
            Data::Flat {
                start,
                blocks,
                tail,
            } => {
                let whole = (size - tail.len() as u64) as usize;
                debug_assert!(whole as u64 <= blocks * BLOCK);
                let mut out = at(image, (start * BLOCK) as usize, whole)?.to_vec();
                out.extend_from_slice(tail);
                Ok(out)
            }
            Data::Chunks { .. } => Err(Error::Unsupported("reading chunked data".into())),
        }
    }
}

/// Lists an image. `image` must hold at least the image's metadata: all of
/// it but the whole blocks of files, which are only located.
pub fn read(image: &[u8]) -> Result<Listing> {
    let r = Reader::new(image)?;
    let mut devices = Vec::new();
    for i in 0..r.extra_devices as usize {
        let slot = at(image, r.devt_slotoff * SLOT_SIZE + i * SLOT_SIZE, SLOT_SIZE)?;
        let tag_len = slot[..64].iter().position(|&b| b == 0).unwrap_or(64);
        let tag = String::from_utf8_lossy(&slot[..tag_len]).into_owned();
        let blocks = u64::from(le32(slot, 64)) | u64::from(le16(slot, 72)) << 32;
        devices.push((tag, blocks));
    }
    let mut entries = Vec::new();
    let mut dirs = HashSet::new();
    let mut stack = vec![(r.root_nid, String::new())];
    while let Some((nid, path)) = stack.pop() {
        let ino = r.inode(nid)?;
        let xattrs = r.xattrs(&ino)?;
        let (kind, data) = match ino.mode & S_IFMT {
            S_IFDIR => {
                if !dirs.insert(nid) {
                    return Err(Error::Corrupt(format!("{path:?}: a directory loop")));
                }
                let bytes = r.contents(&ino)?;
                let mut children = parse_dir(&bytes, &path)?;
                children.retain(|(name, _)| name != "." && name != "..");
                for (name, child) in children.into_iter().rev() {
                    let child_path = if path.is_empty() {
                        name
                    } else {
                        format!("{path}/{name}")
                    };
                    stack.push((child, child_path));
                }
                (EntryKind::Dir, Data::None)
            }
            S_IFREG => (EntryKind::File, r.data(&ino)?),
            S_IFLNK => {
                let target = String::from_utf8(r.contents(&ino)?)
                    .map_err(|_| Error::Unsupported(format!("{path:?}: a non-UTF-8 symlink")))?;
                (EntryKind::Symlink(target), Data::None)
            }
            S_IFCHR => (EntryKind::CharDevice(ino.i_u), Data::None),
            S_IFBLK => (EntryKind::BlockDevice(ino.i_u), Data::None),
            S_IFIFO => (EntryKind::Fifo, Data::None),
            S_IFSOCK => (EntryKind::Socket, Data::None),
            other => return Err(Error::Corrupt(format!("{path:?}: file type {other:#o}"))),
        };
        entries.push(Entry {
            path,
            nid,
            kind,
            meta: ino.meta,
            nlink: ino.nlink,
            size: ino.size,
            xattrs,
            data,
        });
    }
    Ok(Listing { devices, entries })
}

struct Reader<'a> {
    image: &'a [u8],
    compat: u32,
    root_nid: u64,
    epoch: i64,
    fixed_nsec: u32,
    meta_start: usize,
    xattr_start: usize,
    extra_devices: u16,
    devt_slotoff: usize,
}

struct Inode {
    offset: usize,
    layout: u16,
    /// Where xattrs end: the start of inline data or chunk indexes.
    after_xattrs: usize,
    xattr_start: usize,
    mode: u16,
    size: u64,
    i_u: u32,
    nlink: u32,
    meta: Meta,
}

impl<'a> Reader<'a> {
    fn new(image: &'a [u8]) -> Result<Reader<'a>> {
        let sb = image
            .get(SUPER_OFFSET..SUPER_OFFSET + SUPER_SIZE)
            .ok_or(Error::NotErofs)?;
        if le32(sb, 0) != MAGIC {
            return Err(Error::NotErofs);
        }
        if sb[12] != BLKSZBITS {
            return Err(Error::Unsupported(format!(
                "{}-byte blocks",
                1u64 << sb[12]
            )));
        }
        let incompat = le32(sb, 80);
        if incompat & !INCOMPAT_KNOWN != 0 {
            return Err(Error::Unsupported(format!(
                "incompatible features {incompat:#x}"
            )));
        }
        Ok(Reader {
            image,
            compat: le32(sb, 8),
            root_nid: u64::from(le16(sb, 14)),
            epoch: le64(sb, 24) as i64,
            fixed_nsec: le32(sb, 32),
            meta_start: le32(sb, 40) as usize * BLOCK as usize,
            xattr_start: le32(sb, 44) as usize * BLOCK as usize,
            extra_devices: le16(sb, 86),
            devt_slotoff: le16(sb, 88) as usize,
        })
    }

    fn inode(&self, nid: u64) -> Result<Inode> {
        let offset = self.meta_start + nid as usize * 32;
        let format = le16(at(self.image, offset, 2)?, 0);
        let extended = format & I_EXTENDED != 0;
        let isize = if extended {
            INODE_EXTENDED
        } else {
            INODE_COMPACT
        };
        let b = at(self.image, offset, isize)?;
        let icount = le16(b, 2) as usize;
        let xattr_size = if icount == 0 {
            0
        } else {
            XATTR_HEADER + 4 * (icount - 1)
        };
        let (size, i_u, nlink, uid, gid, mtime) = if extended {
            let mtime = parts_to_time(le64(b, 32) as i64, le32(b, 40));
            (
                le64(b, 8),
                le32(b, 16),
                le32(b, 44),
                le32(b, 24),
                le32(b, 28),
                mtime,
            )
        } else {
            let nlink = if format & I_NLINK_1 != 0 {
                1
            } else {
                u32::from(le16(b, 6))
            };
            let secs = if self.compat & COMPAT_MTIME != 0 {
                self.epoch + i64::from(le32(b, 12))
            } else {
                self.epoch
            };
            let mtime = parts_to_time(secs, self.fixed_nsec);
            let (uid, gid) = (u32::from(le16(b, 24)), u32::from(le16(b, 26)));
            (u64::from(le32(b, 8)), le32(b, 16), nlink, uid, gid, mtime)
        };
        let mode = le16(b, 4);
        Ok(Inode {
            offset,
            layout: (format >> 1) & 0x7,
            xattr_start: offset + isize,
            after_xattrs: offset + isize + xattr_size,
            mode,
            size,
            i_u,
            nlink,
            meta: Meta {
                mode: mode & 0o7777,
                uid,
                gid,
                mtime,
            },
        })
    }

    fn xattrs(&self, ino: &Inode) -> Result<Vec<(String, Vec<u8>)>> {
        let mut out = Vec::new();
        if ino.after_xattrs == ino.xattr_start {
            return Ok(out);
        }
        let header = at(self.image, ino.xattr_start, XATTR_HEADER)?;
        let mut pos = ino.xattr_start + XATTR_HEADER;
        for _ in 0..header[4] {
            let id = le32(at(self.image, pos, 4)?, 0) as usize;
            out.push(self.xattr_entry(self.xattr_start + id * 4)?.0);
            pos += 4;
        }
        while pos + 4 <= ino.after_xattrs {
            let (xattr, len) = self.xattr_entry(pos)?;
            out.push(xattr);
            pos += len;
        }
        Ok(out)
    }

    fn xattr_entry(&self, pos: usize) -> Result<((String, Vec<u8>), usize)> {
        let head = at(self.image, pos, 4)?;
        let (name_len, index, value_len) = (head[0] as usize, head[1], le16(head, 2) as usize);
        let body = at(self.image, pos + 4, name_len + value_len)?;
        let prefix = if index == 0 {
            ""
        } else {
            XATTR_PREFIXES
                .iter()
                .find(|(i, _)| *i == index)
                .map(|(_, p)| *p)
                .ok_or_else(|| Error::Unsupported(format!("xattr name index {index}")))?
        };
        let suffix = std::str::from_utf8(&body[..name_len])
            .map_err(|_| Error::Unsupported("a non-UTF-8 xattr name".into()))?;
        let len = (4 + name_len + value_len).next_multiple_of(4);
        Ok((
            (format!("{prefix}{suffix}"), body[name_len..].to_vec()),
            len,
        ))
    }

    /// Where a regular file's bytes are.
    fn data(&self, ino: &Inode) -> Result<Data> {
        if ino.size == 0 {
            return Ok(Data::None);
        }
        match ino.layout {
            LAYOUT_FLAT_PLAIN => Ok(Data::Flat {
                start: u64::from(ino.i_u),
                blocks: ino.size.div_ceil(BLOCK),
                tail: Vec::new(),
            }),
            LAYOUT_FLAT_INLINE => {
                // Every block but the last is whole; the last is inline.
                let blocks = ino.size.div_ceil(BLOCK) - 1;
                let tail_len = (ino.size - blocks * BLOCK) as usize;
                if ino.after_xattrs % BLOCK as usize + tail_len > BLOCK as usize {
                    return Err(Error::Corrupt(format!(
                        "nid {}: inline data crosses a block",
                        self.nid(ino)
                    )));
                }
                Ok(Data::Flat {
                    start: if blocks == 0 { 0 } else { u64::from(ino.i_u) },
                    blocks,
                    tail: at(self.image, ino.after_xattrs, tail_len)?.to_vec(),
                })
            }
            LAYOUT_CHUNK_BASED => {
                let format = ino.i_u as u16;
                let chunk_size = BLOCK << (format & CHUNK_BLKBITS_MASK);
                let count = ino.size.div_ceil(chunk_size) as usize;
                let mut chunks = Vec::with_capacity(count);
                if format & CHUNK_INDEXES != 0 {
                    let mask = match self.extra_devices {
                        0 => 0,
                        n => (u32::from(n) + 1).next_power_of_two() as u16 - 1,
                    };
                    let base = ino.after_xattrs.next_multiple_of(8);
                    let indexes = at(self.image, base, 8 * count)?;
                    for index in indexes.chunks_exact(8) {
                        let device = le16(index, 2) & mask;
                        if device > self.extra_devices {
                            return Err(Error::Corrupt(format!("no device {device}")));
                        }
                        let start = le32(index, 4);
                        chunks.push(Chunk {
                            device,
                            start: (start != NULL_ADDR).then_some(u64::from(start)),
                        });
                    }
                } else {
                    let base = ino.after_xattrs.next_multiple_of(4);
                    for entry in at(self.image, base, 4 * count)?.chunks_exact(4) {
                        let start = le32(entry, 0);
                        chunks.push(Chunk {
                            device: 0,
                            start: (start != NULL_ADDR).then_some(u64::from(start)),
                        });
                    }
                }
                Ok(Data::Chunks { chunk_size, chunks })
            }
            other => Err(Error::Unsupported(format!("data layout {other}"))),
        }
    }

    /// The whole contents of a directory or symlink.
    fn contents(&self, ino: &Inode) -> Result<Vec<u8>> {
        match self.data(ino)? {
            Data::Chunks { .. } => {
                Err(Error::Unsupported("chunked directories or symlinks".into()))
            }
            data => data.bytes(self.image, ino.size),
        }
    }

    fn nid(&self, ino: &Inode) -> u64 {
        ((ino.offset - self.meta_start) / 32) as u64
    }
}

/// A directory's entries, as `(name, nid)`.
fn parse_dir(bytes: &[u8], path: &str) -> Result<Vec<(String, u64)>> {
    let corrupt = |what: &str| Error::Corrupt(format!("directory {path:?}: {what}"));
    let mut out = Vec::new();
    for block in bytes.chunks(BLOCK as usize) {
        if block.len() < DIRENT_SIZE {
            return Err(corrupt("a short block"));
        }
        let first = le16(block, 8) as usize;
        if first == 0 || first % DIRENT_SIZE != 0 || first > block.len() {
            return Err(corrupt("a bad first name offset"));
        }
        let count = first / DIRENT_SIZE;
        for i in 0..count {
            let d = &block[i * DIRENT_SIZE..];
            let nameoff = le16(d, 8) as usize;
            let end = if i + 1 < count {
                le16(&block[(i + 1) * DIRENT_SIZE..], 8) as usize
            } else {
                block.len()
            };
            if nameoff > end || end > block.len() {
                return Err(corrupt("a bad name offset"));
            }
            let mut name = &block[nameoff..end];
            if i + 1 == count {
                // The last name ends where the block's padding starts.
                name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
            }
            if name.is_empty() || name.contains(&b'/') {
                return Err(corrupt("a bad name"));
            }
            let name = String::from_utf8(name.to_vec())
                .map_err(|_| Error::Unsupported(format!("{path:?}: a non-UTF-8 name")))?;
            out.push((name, le64(d, 0)));
        }
    }
    Ok(out)
}

fn at(image: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    image.get(offset..offset + len).ok_or_else(|| {
        Error::Corrupt(format!(
            "{len} bytes at {offset} are past the end ({} bytes)",
            image.len()
        ))
    })
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().expect("2 bytes"))
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}
