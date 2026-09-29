//! How filesystem objects are encoded as cache entries.
//!
//! A cache entry is identified by `(key, version)`. The key is the path under a
//! configurable prefix; the 64-hex-digit version carries the inode metadata
//! (see DESIGN.md §3.2), so that listing the cache is enough to `stat` it.

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Longest key the cache service accepts.
pub const MAX_KEY_LEN: usize = 512;

static MAGIC: LazyLock<[u8; 8]> = LazyLock::new(|| {
    let digest = Sha256::digest(b"gha-cache-fusefs/v1");
    digest[..8].try_into().expect("sha256 is 32 bytes")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File = 1,
    Dir = 2,
    Symlink = 3,
    Whiteout = 4,
}

impl Kind {
    fn from_byte(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::File,
            2 => Kind::Dir,
            3 => Kind::Symlink,
            4 => Kind::Whiteout,
            _ => return None,
        })
    }
}

const FLAG_EMPTY: u8 = 1;

/// The metadata stored in an entry's version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    /// The blob is a one-byte placeholder; the logical content is empty.
    pub empty: bool,
    /// Permission bits (`& 0o7777`).
    pub mode: u16,
    pub mtime: SystemTime,
    pub nonce: u64,
}

impl Meta {
    pub fn new(kind: Kind, mode: u16, mtime: SystemTime) -> Meta {
        Meta {
            kind,
            empty: false,
            mode: mode & 0o7777,
            mtime,
            nonce: fresh_nonce(),
        }
    }

    /// Encodes the metadata as a cache version: 64 lowercase hex digits.
    pub fn encode(&self) -> String {
        let mut raw = [0u8; 32];
        raw[..8].copy_from_slice(&*MAGIC);
        raw[8] = self.kind as u8;
        raw[9] = if self.empty { FLAG_EMPTY } else { 0 };
        raw[10..12].copy_from_slice(&self.mode.to_be_bytes());
        raw[16..24].copy_from_slice(&system_time_to_nanos(self.mtime).to_be_bytes());
        raw[24..32].copy_from_slice(&self.nonce.to_be_bytes());
        hex::encode(raw)
    }

    /// Decodes a cache version, returning `None` for entries that were not
    /// written by this filesystem.
    pub fn decode(version: &str) -> Option<Meta> {
        if version.len() != 64 {
            return None;
        }
        let mut raw = [0u8; 32];
        hex::decode_to_slice(version, &mut raw).ok()?;
        if raw[..8] != *MAGIC {
            return None;
        }
        let kind = Kind::from_byte(raw[8])?;
        let mode = u16::from_be_bytes([raw[10], raw[11]]);
        let nanos = i64::from_be_bytes(raw[16..24].try_into().ok()?);
        let nonce = u64::from_be_bytes(raw[24..32].try_into().ok()?);
        Some(Meta {
            kind,
            empty: raw[9] & FLAG_EMPTY != 0,
            mode: mode & 0o7777,
            mtime: nanos_to_system_time(nanos),
            nonce,
        })
    }
}

pub fn fresh_nonce() -> u64 {
    getrandom::u64().expect("the OS random number generator is available")
}

pub fn system_time_to_nanos(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

pub fn nanos_to_system_time(n: i64) -> SystemTime {
    if n >= 0 {
        UNIX_EPOCH + Duration::from_nanos(n as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(n.unsigned_abs())
    }
}

/// What a listed key names, relative to the mount's prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyPath {
    /// A file, symlink, or whiteout at this path (components joined by `/`).
    Node(String),
    /// The marker of the directory at this path.
    DirMarker(String),
}

/// The mapping between paths below the mount root and cache keys.
#[derive(Clone, Debug)]
pub struct KeySpace {
    prefix: String,
}

impl KeySpace {
    /// `prefix` is normalized to end in `/` unless it is empty.
    pub fn new(prefix: &str) -> KeySpace {
        let mut prefix = prefix.to_string();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        KeySpace { prefix }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The key of a file or symlink. `path` is relative, without a leading `/`.
    pub fn node_key(&self, path: &str) -> String {
        format!("{}{}", self.prefix, path)
    }

    /// The key of a directory's marker.
    pub fn dir_key(&self, path: &str) -> String {
        format!("{}{}/", self.prefix, path)
    }

    /// Interprets a listed key. Keys outside the prefix, the prefix itself,
    /// and keys with empty, `.` or `..` components are rejected.
    pub fn parse(&self, key: &str) -> Option<KeyPath> {
        let rest = key.strip_prefix(&self.prefix)?;
        let (rest, dir) = match rest.strip_suffix('/') {
            Some(r) => (r, true),
            None => (rest, false),
        };
        if rest.is_empty() || !rest.split('/').all(valid_component) {
            return None;
        }
        Some(if dir {
            KeyPath::DirMarker(rest.to_string())
        } else {
            KeyPath::Node(rest.to_string())
        })
    }
}

/// Whether `name` can be a path component (a single file name).
pub fn valid_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Joins a parent path and a child name.
pub fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Splits a path into its parent path and final component.
pub fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_round_trips() {
        let mtime = UNIX_EPOCH + Duration::new(1_790_000_000, 123_456_789);
        for kind in [Kind::File, Kind::Dir, Kind::Symlink, Kind::Whiteout] {
            for empty in [false, true] {
                let meta = Meta {
                    kind,
                    empty,
                    mode: 0o4755,
                    mtime,
                    nonce: 0xdead_beef_cafe_f00d,
                };
                let v = meta.encode();
                assert_eq!(v.len(), 64);
                assert!(
                    v.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                );
                assert_eq!(Meta::decode(&v), Some(meta));
            }
        }
    }

    #[test]
    fn negative_mtimes_round_trip() {
        let mtime = UNIX_EPOCH - Duration::new(86_400, 5);
        let meta = Meta::new(Kind::File, 0o644, mtime);
        assert_eq!(Meta::decode(&meta.encode()).unwrap().mtime, mtime);
    }

    #[test]
    fn nonces_differ() {
        let t = SystemTime::now();
        assert_ne!(
            Meta::new(Kind::File, 0o644, t).encode(),
            Meta::new(Kind::File, 0o644, t).encode()
        );
    }

    #[test]
    fn foreign_versions_are_rejected() {
        // What actions/cache produces: sha256 of paths + compression method.
        let foreign = hex::encode(Sha256::digest(b"/home/runner/.cargo|zstd-without-long|1.0"));
        assert_eq!(Meta::decode(&foreign), None);
        assert_eq!(Meta::decode("not hex"), None);
        assert_eq!(Meta::decode(&"0".repeat(64)), None);
        let mut ours = Meta::new(Kind::File, 0o644, SystemTime::now()).encode();
        ours.replace_range(16..18, "09"); // unknown kind
        assert_eq!(Meta::decode(&ours), None);
    }

    #[test]
    fn keys_map_to_paths() {
        let ks = KeySpace::new("fusefs");
        assert_eq!(ks.prefix(), "fusefs/");
        assert_eq!(ks.node_key("a/b"), "fusefs/a/b");
        assert_eq!(ks.dir_key("a/b"), "fusefs/a/b/");
        assert_eq!(ks.parse("fusefs/a/b"), Some(KeyPath::Node("a/b".into())));
        assert_eq!(
            ks.parse("fusefs/a/b/"),
            Some(KeyPath::DirMarker("a/b".into()))
        );
        assert_eq!(ks.parse("fusefs/"), None);
        assert_eq!(ks.parse("fusefs"), None);
        assert_eq!(ks.parse("other/a"), None);
        assert_eq!(ks.parse("fusefs/a//b"), None);
        assert_eq!(ks.parse("fusefs/a/./b"), None);
        assert_eq!(ks.parse("fusefs/../b"), None);
        assert_eq!(
            ks.parse("fusefs/with space/日本"),
            Some(KeyPath::Node("with space/日本".into()))
        );
    }

    #[test]
    fn empty_prefix_is_the_whole_key_space() {
        let ks = KeySpace::new("");
        assert_eq!(ks.node_key("a"), "a");
        assert_eq!(ks.parse("a/b"), Some(KeyPath::Node("a/b".into())));
        assert_eq!(ks.parse("/a"), None);
    }

    #[test]
    fn join_and_split() {
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", "b"), "a/b");
        assert_eq!(split("a/b/c"), ("a/b", "c"));
        assert_eq!(split("c"), ("", "c"));
    }
}
