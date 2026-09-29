//! EROFS images: the subset LAYERS.md §3 uses, written and read.
//!
//! The on-disk format is the kernel's (`fs/erofs/erofs_fs.h`). Images we
//! write have 4 KiB blocks, uncompressed data, and 64-byte inodes only. The
//! reader also accepts compact inodes, flat inline files with whole blocks,
//! and shared xattrs, so that it can read what `mkfs.erofs` writes.

mod read;
mod write;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use read::{Chunk, Data, Entry, EntryKind, Listing, read};
pub use write::{Device, FileData, INLINE_MAX, Image, Node, NodeKind, Written};

/// Block size.
pub const BLOCK: u64 = 4096;
const BLKSZBITS: u8 = 12;

const SUPER_OFFSET: usize = 1024;
const SUPER_SIZE: usize = 128;
const MAGIC: u32 = 0xE0F5_E1E2;
const SLOT_SIZE: usize = 128;
const INODE_COMPACT: usize = 32;
const INODE_EXTENDED: usize = 64;
const DIRENT_SIZE: usize = 12;
const XATTR_HEADER: usize = 12;
const NAME_MAX: usize = 255;
/// A hole, or no block at all.
const NULL_ADDR: u32 = u32::MAX;

const COMPAT_MTIME: u32 = 0x0002;
const INCOMPAT_CHUNKED_FILE: u32 = 0x0004;
const INCOMPAT_DEVICE_TABLE: u32 = 0x0008;
/// What the reader understands: the above, and nothing compressed.
const INCOMPAT_KNOWN: u32 = INCOMPAT_CHUNKED_FILE | INCOMPAT_DEVICE_TABLE;

/// `i_format`: bit 0 is the inode version, bits 1–3 the data layout.
const I_EXTENDED: u16 = 1;
const I_NLINK_1: u16 = 1 << 4;
const LAYOUT_FLAT_PLAIN: u16 = 0;
const LAYOUT_FLAT_INLINE: u16 = 2;
const LAYOUT_CHUNK_BASED: u16 = 4;
const CHUNK_BLKBITS_MASK: u16 = 0x1f;
const CHUNK_INDEXES: u16 = 0x20;

// Directory entry file types, as the kernel's `FT_*`.
const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHRDEV: u8 = 3;
const FT_SYMLINK: u8 = 7;

const S_IFMT: u16 = 0o170000;
const S_IFSOCK: u16 = 0o140000;
const S_IFLNK: u16 = 0o120000;
const S_IFREG: u16 = 0o100000;
const S_IFBLK: u16 = 0o060000;
const S_IFDIR: u16 = 0o040000;
const S_IFCHR: u16 = 0o020000;
const S_IFIFO: u16 = 0o010000;

/// Xattr name prefixes, by EROFS name index.
const XATTR_PREFIXES: [(u8, &str); 6] = [
    (1, "user."),
    (2, "system.posix_acl_access"),
    (3, "system.posix_acl_default"),
    (4, "trusted."),
    (5, "lustre."),
    (6, "security."),
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not an EROFS image")]
    NotErofs,
    #[error("unsupported EROFS feature: {0}")]
    Unsupported(String),
    #[error("corrupt EROFS image: {0}")]
    Corrupt(String),
    /// The tree given to the writer cannot be written.
    #[error("{0}")]
    Tree(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// An inode's attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Permission bits (`& 0o7777`).
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub mtime: SystemTime,
}

fn time_to_parts(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            // Before 1970: whole seconds down, nanoseconds up.
            let d = e.duration();
            let (s, n) = (d.as_secs() as i64, d.subsec_nanos());
            if n == 0 {
                (-s, 0)
            } else {
                (-s - 1, 1_000_000_000 - n)
            }
        }
    }
}

fn parts_to_time(secs: i64, nsec: u32) -> SystemTime {
    let base = if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    };
    base + Duration::from_nanos(nsec.min(999_999_999) as u64)
}

#[cfg(test)]
mod tests;
