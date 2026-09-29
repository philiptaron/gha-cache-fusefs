//! Mount the GitHub Actions cache as a FUSE filesystem. See DESIGN.md.

pub mod api;
pub mod bench;
pub mod config;
pub mod data;
pub mod entry;
pub mod erofs;
pub mod fake;
pub mod index;
pub mod vfs;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod fuse;
