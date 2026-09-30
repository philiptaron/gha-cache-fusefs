//! How the filesystem is stored as cache entries (LAYERS.md §2).
//!
//! A volume's entries live under `gha-fs/<volume>/`: layers, each an EROFS
//! image of one commit's changes, and blobs, each the bytes of one large file.
//! Keys hold no paths. The 64-hex-digit version says which kind an entry is.

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

static MAGIC: LazyLock<[u8; 8]> = LazyLock::new(|| {
    let digest = Sha256::digest(b"gha-cache-fusefs/v2");
    digest[..8].try_into().expect("sha256 is 32 bytes")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An EROFS image whose first `meta_blocks` 4 KiB blocks list its tree.
    Layer {
        meta_blocks: u32,
    },
    Blob,
    /// A layer that stands in for the layers of its scope created before
    /// `covers` (LAYERS.md §8).
    Snapshot {
        meta_blocks: u32,
        covers: SystemTime,
    },
}

/// What an entry's version encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub kind: Kind,
    /// Random: makes every upload a distinct (key, version).
    pub nonce: u64,
}

impl Version {
    pub fn layer(meta_blocks: u32) -> Version {
        Version {
            kind: Kind::Layer { meta_blocks },
            nonce: fresh_nonce(),
        }
    }

    pub fn blob() -> Version {
        Version {
            kind: Kind::Blob,
            nonce: fresh_nonce(),
        }
    }

    pub fn snapshot(meta_blocks: u32, covers: SystemTime) -> Version {
        Version {
            kind: Kind::Snapshot {
                meta_blocks,
                covers,
            },
            nonce: fresh_nonce(),
        }
    }

    /// 64 lowercase hex digits.
    pub fn encode(&self) -> String {
        let mut raw = [0u8; 32];
        raw[..8].copy_from_slice(&*MAGIC);
        match self.kind {
            Kind::Layer { meta_blocks } => {
                raw[8] = 1;
                raw[12..16].copy_from_slice(&meta_blocks.to_be_bytes());
            }
            Kind::Blob => raw[8] = 2,
            Kind::Snapshot {
                meta_blocks,
                covers,
            } => {
                raw[8] = 3;
                raw[12..16].copy_from_slice(&meta_blocks.to_be_bytes());
                let micros = covers
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_micros() as u64);
                raw[16..24].copy_from_slice(&micros.to_be_bytes());
            }
        }
        raw[24..32].copy_from_slice(&self.nonce.to_be_bytes());
        hex::encode(raw)
    }

    /// `None` for entries this filesystem did not write, including format 1.
    pub fn decode(version: &str) -> Option<Version> {
        if version.len() != 64 {
            return None;
        }
        let mut raw = [0u8; 32];
        hex::decode_to_slice(version, &mut raw).ok()?;
        if raw[..8] != *MAGIC {
            return None;
        }
        let kind = match raw[8] {
            1 => Kind::Layer {
                meta_blocks: u32::from_be_bytes(raw[12..16].try_into().ok()?),
            },
            2 => Kind::Blob,
            3 => Kind::Snapshot {
                meta_blocks: u32::from_be_bytes(raw[12..16].try_into().ok()?),
                covers: UNIX_EPOCH
                    + Duration::from_micros(u64::from_be_bytes(raw[16..24].try_into().ok()?)),
            },
            _ => return None,
        };
        let nonce = u64::from_be_bytes(raw[24..32].try_into().ok()?);
        Some(Version { kind, nonce })
    }
}

pub fn fresh_nonce() -> u64 {
    getrandom::u64().expect("the OS random number generator is available")
}

/// The xattr that marks a directory a layer changed (LAYERS.md §3).
pub const DIR_XATTR: &str = "user.gha-fs.dir";
/// The xattr on a layer's root that says who wrote it.
pub const WRITER_XATTR: &str = "user.gha-fs.writer";

/// What a layer says about a directory it changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Made with `mkdir`: these attributes, and it exists even when empty.
    Keep,
    /// `chmod` or `utimens`: these attributes.
    Attrs,
    /// Removed with `rmdir`: it exists only while something below it does.
    Drop,
    /// Written by snapshots, for `attrs` and then `drop`: these attributes,
    /// and it exists only while something below it does.
    AttrsDrop,
}

impl Mark {
    pub fn as_bytes(self) -> &'static [u8] {
        match self {
            Mark::Keep => b"keep",
            Mark::Attrs => b"attrs",
            Mark::Drop => b"drop",
            Mark::AttrsDrop => b"attrs-drop",
        }
    }

    pub fn parse(value: &[u8]) -> Option<Mark> {
        match value {
            b"keep" => Some(Mark::Keep),
            b"attrs" => Some(Mark::Attrs),
            b"drop" => Some(Mark::Drop),
            b"attrs-drop" => Some(Mark::AttrsDrop),
            _ => None,
        }
    }
}

/// What a key under a volume names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Layer,
    /// A blob, by the SHA-256 of its bytes in hex.
    Blob(String),
}

/// One filesystem's namespace in the cache: `gha-fs/<name>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    name: String,
    prefix: String,
}

impl Volume {
    /// Names are 1–64 characters of `[A-Za-z0-9._-]`.
    pub fn new(name: &str) -> Result<Volume, String> {
        let ok = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !ok {
            return Err(format!(
                "{name:?} is not a volume name: use 1 to 64 of A-Z, a-z, 0-9, '.', '_', and '-'"
            ));
        }
        Ok(Volume {
            name: name.to_string(),
            prefix: format!("gha-fs/{name}/"),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Every key of the volume starts with this.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn layer_key(&self, nonce: u64) -> String {
        format!("{}layer/{nonce:016x}", self.prefix)
    }

    pub fn blob_key(&self, sha256: &str) -> String {
        format!("{}blob/{sha256}", self.prefix)
    }

    /// The tag of a device slot that refers to this entry (LAYERS.md §3):
    /// `layer/<nonce>` for a layer, the digest for a blob.
    pub fn device_tag(&self, key: &str) -> Option<String> {
        match self.parse(key)? {
            KeyKind::Layer => Some(key[self.prefix.len()..].to_string()),
            KeyKind::Blob(sha) => Some(sha),
        }
    }

    pub fn parse(&self, key: &str) -> Option<KeyKind> {
        let rest = key.strip_prefix(&self.prefix)?;
        if let Some(nonce) = rest.strip_prefix("layer/") {
            return (nonce.len() == 16 && nonce.bytes().all(|b| b.is_ascii_hexdigit()))
                .then_some(KeyKind::Layer);
        }
        let sha = rest.strip_prefix("blob/")?;
        let hex = sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        hex.then(|| KeyKind::Blob(sha.to_string()))
    }
}

/// The longest name EROFS takes, in bytes.
pub const NAME_MAX: usize = 255;

/// Whether `name` can be a path component (a single file name).
pub fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= NAME_MAX
        && !name.contains('/')
        && !name.contains('\0')
}

/// Joins a parent path and a child name; either may be "".
pub fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else if name.is_empty() {
        parent.to_string()
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

/// The parent of a path; "" for a top-level name.
pub fn parent(path: &str) -> &str {
    split(path).0
}

/// Whether `path` is `dir` or below it.
pub fn is_within(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_round_trip() {
        let covers = UNIX_EPOCH + Duration::from_micros(1_790_000_000_123_456);
        for kind in [
            Kind::Layer { meta_blocks: 7 },
            Kind::Blob,
            Kind::Snapshot {
                meta_blocks: 9,
                covers,
            },
        ] {
            let v = Version {
                kind,
                nonce: 0xdead_beef_cafe_f00d,
            };
            let hex = v.encode();
            assert_eq!(hex.len(), 64);
            assert!(
                hex.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert_eq!(Version::decode(&hex), Some(v));
        }
        assert_ne!(Version::blob().encode(), Version::blob().encode());
    }

    #[test]
    fn foreign_versions_are_rejected() {
        // What actions/cache produces: sha256 of paths + compression method.
        let foreign = hex::encode(Sha256::digest(b"/home/runner/.cargo|zstd-without-long|1.0"));
        assert_eq!(Version::decode(&foreign), None);
        assert_eq!(Version::decode("not hex"), None);
        // Format 1's magic.
        let v1 = hex::encode(Sha256::digest(b"gha-cache-fusefs/v1"));
        assert_eq!(Version::decode(&v1), None);
        let mut ours = Version::blob().encode();
        ours.replace_range(16..18, "09");
        assert_eq!(Version::decode(&ours), None);
    }

    #[test]
    fn volumes_name_their_keys() {
        let v = Volume::new("default").unwrap();
        assert_eq!(v.prefix(), "gha-fs/default/");
        let layer = v.layer_key(0xab);
        assert_eq!(layer, "gha-fs/default/layer/00000000000000ab");
        assert_eq!(v.parse(&layer), Some(KeyKind::Layer));
        assert_eq!(v.device_tag(&layer).unwrap(), "layer/00000000000000ab");
        let sha = "c".repeat(64);
        assert_eq!(v.device_tag(&v.blob_key(&sha)).unwrap(), sha);
        assert_eq!(v.parse(&v.blob_key(&sha)), Some(KeyKind::Blob(sha)));
        assert_eq!(v.parse("gha-fs/other/layer/00000000000000ab"), None);
        assert_eq!(v.parse("gha-fs/default/layer/xyz"), None);
        assert_eq!(v.parse("gha-fs/default/blob/C"), None);
        for bad in ["", "a/b", "with space", &"x".repeat(65)] {
            assert!(Volume::new(bad).is_err(), "{bad:?}");
        }
        assert!(Volume::new("ci-123-1.x_y").is_ok());
    }

    #[test]
    fn paths() {
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", "b"), "a/b");
        assert_eq!(split("a/b/c"), ("a/b", "c"));
        assert_eq!(split("c"), ("", "c"));
        assert!(is_within("a/b", "a"));
        assert!(is_within("a", "a"));
        assert!(is_within("a", ""));
        assert!(!is_within("ab", "a"));
        assert!(!valid_component(&"x".repeat(256)));
        assert_eq!(join("a", ""), "a");
        for m in [Mark::Keep, Mark::Attrs, Mark::Drop] {
            assert_eq!(Mark::parse(m.as_bytes()), Some(m));
        }
        assert_eq!(Mark::parse(b"other"), None);
    }
}
