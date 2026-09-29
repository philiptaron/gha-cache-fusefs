//! Writing images.
//!
//! An image is laid out in the order LAYERS.md §3 gives: the superblock,
//! device slots, every inode (with its xattrs, inline data, and chunk
//! indexes), the directory and symlink blocks that do not fit inline, and
//! then file data in path order. Everything before the file data is the
//! image's metadata, which a reader can take in one piece.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::time::SystemTime;

use super::*;

/// Files up to this size are stored inline, right after their inode.
pub const INLINE_MAX: u64 = 1024;
/// Data on extra devices is addressed in chunks of `BLOCK << CHUNK_BITS`,
/// 16 MiB.
const CHUNK_BITS: u16 = 12;

pub struct Node {
    pub meta: Meta,
    pub kind: NodeKind,
    /// Full names (`user.…`, `trusted.…`, or `security.…`) and values.
    pub xattrs: Vec<(String, Vec<u8>)>,
}

pub enum NodeKind {
    Dir,
    File(FileData),
    Symlink(String),
    /// An overlayfs whiteout: a character device with device number 0:0.
    Whiteout,
}

pub enum FileData {
    /// Stored in the image: inline up to `INLINE_MAX` bytes, else in whole
    /// blocks after the metadata. `source` must yield exactly `len` bytes.
    Here {
        len: u64,
        source: Box<dyn Read + Send>,
    },
    /// Stored on extra device `device` (1 is the first), from block `start`.
    Device { len: u64, device: u16, start: u64 },
}

/// An extra device: a blob that files' data can live in.
pub struct Device {
    /// At most 64 bytes, such as a SHA-256 in hex.
    pub tag: String,
    pub blocks: u64,
}

/// What `Image::write_to` wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// Blocks before the first block of file data.
    pub meta_blocks: u64,
    pub blocks: u64,
}

/// An image to write: nodes by path, with "" for the root.
pub struct Image {
    nodes: BTreeMap<String, Node>,
    devices: Vec<Device>,
    /// Recorded as the image's build time.
    pub built: SystemTime,
    pub uuid: [u8; 16],
}

impl Image {
    pub fn new(root: Meta) -> Image {
        let mut uuid = [0u8; 16];
        getrandom::fill(&mut uuid).expect("the OS random number generator is available");
        let mut nodes = BTreeMap::new();
        nodes.insert(
            String::new(),
            Node {
                meta: root,
                kind: NodeKind::Dir,
                xattrs: Vec::new(),
            },
        );
        Image {
            nodes,
            devices: Vec::new(),
            built: SystemTime::now(),
            uuid,
        }
    }

    /// Adds or replaces the node at `path`. Its parent must be a directory in
    /// the image by the time it is written.
    pub fn insert(&mut self, path: &str, node: Node) -> Result<()> {
        if path.is_empty() {
            if !matches!(node.kind, NodeKind::Dir) {
                return Err(Error::Tree("the root must be a directory".into()));
            }
        } else if !path.split('/').all(valid_name) {
            return Err(Error::Tree(format!("{path:?} is not a valid path")));
        }
        self.nodes.insert(path.to_string(), node);
        Ok(())
    }

    /// Adds an extra device and returns its number, for `FileData::Device`.
    pub fn add_device(&mut self, device: Device) -> Result<u16> {
        if device.tag.len() > 64 {
            return Err(Error::Tree(format!(
                "device tag {:?} is too long",
                device.tag
            )));
        }
        // The root's nid follows the device slots and must fit in 16 bits.
        if self.devices.len() >= 16_000 {
            return Err(Error::Tree("too many devices".into()));
        }
        self.devices.push(device);
        Ok(self.devices.len() as u16)
    }

    /// Writes the image and consumes it.
    pub fn write_to<W: Write>(self, out: &mut W) -> Result<Written> {
        let Image {
            mut nodes,
            devices,
            built,
            uuid,
        } = self;
        let order = walk_order(&nodes)?;
        let index: BTreeMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i))
            .collect();

        // Take the nodes out in walk order, reading inline file data now.
        let mut plans = Vec::with_capacity(order.len());
        let mut sources = Vec::with_capacity(order.len());
        for path in &order {
            let node = nodes.remove(path.as_str()).expect("walked");
            let (plan, source) = Plan::new(node, &devices)?;
            plans.push(plan);
            sources.push(source);
        }

        // Directory entries, and the space they need, which does not depend
        // on nids. Dirents sort by name, "." and ".." included.
        let mut kids: Vec<Vec<usize>> = vec![Vec::new(); order.len()];
        for (i, path) in order.iter().enumerate().skip(1) {
            kids[index[parent_of(path)]].push(i);
        }
        let mut dirents: Vec<Vec<DirEnt>> = vec![Vec::new(); order.len()];
        for (i, path) in order.iter().enumerate() {
            if plans[i].kind != Kind::Dir {
                continue;
            }
            let parent = if path.is_empty() {
                0
            } else {
                index[parent_of(path)]
            };
            let mut ents = vec![
                DirEnt::new(".", FT_DIR, i),
                DirEnt::new("..", FT_DIR, parent),
            ];
            for &j in &kids[i] {
                ents.push(DirEnt::new(
                    name_of(&order[j]),
                    plans[j].kind.file_type(),
                    j,
                ));
            }
            ents.sort_by(|a, b| a.name.cmp(&b.name));
            let subdirs = kids[i]
                .iter()
                .filter(|&&j| plans[j].kind == Kind::Dir)
                .count();
            plans[i].nlink = 2 + subdirs as u32;
            plans[i].size = encode_dir(&ents, &[]).len() as u64;
            dirents[i] = ents;
        }
        for plan in &mut plans {
            plan.decide_layout();
        }

        // Inodes, packed from just after the device slots.
        let mut cursor = SUPER_OFFSET + SUPER_SIZE + devices.len() * SLOT_SIZE;
        for plan in &mut plans {
            let size = plan.meta_size();
            cursor = cursor.next_multiple_of(32);
            let offset = cursor % BLOCK as usize;
            if size <= BLOCK as usize && offset + size > BLOCK as usize {
                cursor = cursor.next_multiple_of(BLOCK as usize);
            }
            plan.nid = (cursor / 32) as u64;
            cursor += size;
        }
        let root_nid = u16::try_from(plans[0].nid)
            .map_err(|_| Error::Tree("the root's nid does not fit in 16 bits".into()))?;

        // Blocks: first directories and symlinks that did not fit inline,
        // which belong to the metadata, then file data.
        let mut block = (cursor as u64).div_ceil(BLOCK);
        for plan in plans.iter_mut().filter(|p| p.layout == Layout::MetaBlocks) {
            plan.start = block_addr(block)?;
            block += plan.size.div_ceil(BLOCK);
        }
        let meta_blocks = block;
        for plan in plans.iter_mut().filter(|p| p.layout == Layout::DataBlocks) {
            plan.start = block_addr(block)?;
            block += plan.size.div_ceil(BLOCK);
        }
        let blocks = block;
        block_addr(blocks)?;

        // Encode the metadata.
        let mut meta = vec![0u8; (meta_blocks * BLOCK) as usize];
        let incompat = if devices.is_empty() {
            0
        } else {
            INCOMPAT_CHUNKED_FILE | INCOMPAT_DEVICE_TABLE
        };
        let sb = &mut meta[SUPER_OFFSET..SUPER_OFFSET + SUPER_SIZE];
        put32(sb, 0, MAGIC);
        sb[12] = BLKSZBITS;
        put16(sb, 14, root_nid);
        put64(sb, 16, plans.len() as u64);
        put64(sb, 24, time_to_parts(built).0.max(0) as u64);
        put32(sb, 36, blocks as u32);
        sb[48..64].copy_from_slice(&uuid);
        put32(sb, 80, incompat);
        put16(sb, 86, devices.len() as u16);
        if !devices.is_empty() {
            put16(sb, 88, ((SUPER_OFFSET + SUPER_SIZE) / SLOT_SIZE) as u16);
        }
        for (i, dev) in devices.iter().enumerate() {
            let at = SUPER_OFFSET + SUPER_SIZE + i * SLOT_SIZE;
            let slot = &mut meta[at..at + SLOT_SIZE];
            slot[..dev.tag.len()].copy_from_slice(dev.tag.as_bytes());
            put32(slot, 64, dev.blocks as u32);
            put16(slot, 72, (dev.blocks >> 32) as u16);
        }
        let nids: Vec<u64> = plans.iter().map(|p| p.nid).collect();
        for (i, plan) in plans.iter_mut().enumerate() {
            if plan.kind == Kind::Dir {
                plan.data = encode_dir(&dirents[i], &nids);
            }
        }
        for (i, plan) in plans.iter().enumerate() {
            plan.encode(&mut meta, i as u32 + 1)?;
        }
        out.write_all(&meta)?;

        // Then file data, block by block.
        for (plan, source) in plans.iter().zip(sources) {
            if plan.layout != Layout::DataBlocks {
                continue;
            }
            let mut source = source.expect("files in blocks have a source");
            copy_exactly(&mut source, out, plan.size)?;
            let pad = plan.size.next_multiple_of(BLOCK) - plan.size;
            io::copy(&mut io::repeat(0).take(pad), out)?;
        }
        Ok(Written {
            meta_blocks,
            blocks,
        })
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && n.len() <= NAME_MAX && !n.contains('\0')
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

/// Every path, depth first in name order, the root first. Checks that each
/// node's parent is a directory.
fn walk_order(nodes: &BTreeMap<String, Node>) -> Result<Vec<String>> {
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for path in nodes.keys().filter(|p| !p.is_empty()) {
        let parent = parent_of(path);
        match nodes.get(parent) {
            Some(Node {
                kind: NodeKind::Dir,
                ..
            }) => children.entry(parent).or_default().push(path),
            Some(_) => return Err(Error::Tree(format!("{parent:?} is not a directory"))),
            None => return Err(Error::Tree(format!("{path:?} has no parent directory"))),
        }
    }
    let mut order = Vec::with_capacity(nodes.len());
    let mut stack = vec![""];
    while let Some(path) = stack.pop() {
        order.push(path.to_string());
        if let Some(kids) = children.get(path) {
            stack.extend(kids.iter().rev());
        }
    }
    Ok(order)
}

fn block_addr(block: u64) -> Result<u32> {
    u32::try_from(block)
        .ok()
        .filter(|&b| b != NULL_ADDR)
        .ok_or_else(|| Error::Tree("the image is too large".into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
    Whiteout,
}

impl Kind {
    fn file_type(self) -> u8 {
        match self {
            Kind::Dir => FT_DIR,
            Kind::File => FT_REG,
            Kind::Symlink => FT_SYMLINK,
            Kind::Whiteout => FT_CHRDEV,
        }
    }
}

/// Where a node's data goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// No data at all.
    Empty,
    /// All of it right after the inode.
    Inline,
    /// Whole blocks in the metadata (directories and symlinks too large to inline).
    MetaBlocks,
    /// Whole blocks after the metadata (files).
    DataBlocks,
    /// Chunks on an extra device.
    Chunked,
}

/// A node on its way into the image.
struct Plan {
    kind: Kind,
    meta: Meta,
    size: u64,
    xattrs: Vec<u8>,
    /// Inline or metadata-block data: file bytes, a symlink's target, or dirents.
    data: Vec<u8>,
    /// For chunked files: the device and first block.
    device: (u16, u64),
    layout: Layout,
    nlink: u32,
    nid: u64,
    start: u32,
}

impl Plan {
    fn new(node: Node, devices: &[Device]) -> Result<(Plan, Option<Box<dyn Read + Send>>)> {
        let xattrs = encode_xattrs(&node.xattrs)?;
        let mut plan = Plan {
            kind: Kind::Dir,
            meta: node.meta,
            size: 0,
            xattrs,
            data: Vec::new(),
            device: (0, 0),
            layout: Layout::Empty,
            nlink: 1,
            nid: 0,
            start: 0,
        };
        let mut source = None;
        match node.kind {
            NodeKind::Dir => {}
            NodeKind::File(FileData::Here { len, source: mut s }) => {
                plan.kind = Kind::File;
                plan.size = len;
                if len <= INLINE_MAX {
                    let mut data = Vec::with_capacity(len as usize);
                    copy_exactly(&mut s, &mut data, len)?;
                    plan.data = data;
                } else {
                    source = Some(s);
                }
            }
            NodeKind::File(FileData::Device { len, device, start }) => {
                let known = device >= 1 && usize::from(device) <= devices.len();
                if !known {
                    return Err(Error::Tree(format!("no device {device}")));
                }
                plan.kind = Kind::File;
                plan.size = len;
                plan.device = (device, start);
            }
            NodeKind::Symlink(target) => {
                if target.is_empty() || target.len() > BLOCK as usize {
                    return Err(Error::Tree(format!("bad symlink target {target:?}")));
                }
                plan.kind = Kind::Symlink;
                plan.size = target.len() as u64;
                plan.data = target.into_bytes();
            }
            NodeKind::Whiteout => plan.kind = Kind::Whiteout,
        }
        Ok((plan, source))
    }

    /// Where the data goes, once the size (of dirents, too) is known.
    fn decide_layout(&mut self) {
        let fits_inline = INODE_EXTENDED + self.xattrs.len() + self.size as usize <= BLOCK as usize;
        self.layout = match self.kind {
            Kind::Whiteout => Layout::Empty,
            Kind::File if self.device.0 != 0 => Layout::Chunked,
            Kind::File if self.size == 0 => Layout::Empty,
            Kind::File if self.size <= INLINE_MAX => Layout::Inline,
            Kind::File => Layout::DataBlocks,
            Kind::Dir | Kind::Symlink if fits_inline => Layout::Inline,
            Kind::Dir | Kind::Symlink => Layout::MetaBlocks,
        };
    }

    fn chunks(&self) -> u64 {
        self.size.div_ceil(BLOCK << CHUNK_BITS)
    }

    /// Bytes the inode and what follows it take in the metadata.
    fn meta_size(&self) -> usize {
        let base = INODE_EXTENDED + self.xattrs.len();
        match self.layout {
            // A directory's dirents are only encoded once nids are known.
            Layout::Inline => base + self.size as usize,
            // Chunk indexes are 8-byte aligned, and so is every inode.
            Layout::Chunked => base.next_multiple_of(8) + 8 * self.chunks() as usize,
            _ => base,
        }
    }

    fn encode(&self, meta: &mut [u8], ino: u32) -> Result<()> {
        let at = self.nid as usize * 32;
        debug_assert!(
            !matches!(self.layout, Layout::Inline | Layout::MetaBlocks)
                || self.data.len() as u64 == self.size
        );
        let (layout, i_u) = match self.layout {
            // A whiteout's rdev, 0:0, or an empty file's first block, as mkfs.erofs writes it.
            Layout::Empty => (LAYOUT_FLAT_PLAIN, 0),
            Layout::Inline => (LAYOUT_FLAT_INLINE, NULL_ADDR),
            Layout::MetaBlocks | Layout::DataBlocks => (LAYOUT_FLAT_PLAIN, self.start),
            Layout::Chunked => (LAYOUT_CHUNK_BASED, u32::from(CHUNK_BITS | CHUNK_INDEXES)),
        };
        let type_bits = match self.kind {
            Kind::Dir => S_IFDIR,
            Kind::File => S_IFREG,
            Kind::Symlink => S_IFLNK,
            Kind::Whiteout => S_IFCHR,
        };
        let perm = match self.kind {
            Kind::Symlink => 0o777,
            _ => self.meta.mode & 0o7777,
        };
        let (secs, nsec) = time_to_parts(self.meta.mtime);
        let inode = &mut meta[at..at + INODE_EXTENDED];
        put16(inode, 0, (layout << 1) | I_EXTENDED);
        put16(inode, 2, xattr_icount(self.xattrs.len()));
        put16(inode, 4, type_bits | perm);
        put64(inode, 8, self.size);
        put32(inode, 16, i_u);
        put32(inode, 20, ino);
        put32(inode, 24, self.meta.uid);
        put32(inode, 28, self.meta.gid);
        put64(inode, 32, secs as u64);
        put32(inode, 40, nsec);
        put32(inode, 44, self.nlink);
        let mut pos = at + INODE_EXTENDED;
        meta[pos..pos + self.xattrs.len()].copy_from_slice(&self.xattrs);
        pos += self.xattrs.len();
        match self.layout {
            Layout::Inline => meta[pos..pos + self.data.len()].copy_from_slice(&self.data),
            Layout::MetaBlocks => {
                let start = self.start as usize * BLOCK as usize;
                meta[start..start + self.data.len()].copy_from_slice(&self.data);
            }
            Layout::Chunked => {
                let (device, first) = self.device;
                let per_chunk = 1u64 << CHUNK_BITS;
                pos = pos.next_multiple_of(8);
                for c in 0..self.chunks() {
                    let index = &mut meta[pos..pos + 8];
                    put16(index, 2, device);
                    put32(index, 4, block_addr(first + c * per_chunk)?);
                    pos += 8;
                }
            }
            Layout::Empty | Layout::DataBlocks => {}
        }
        Ok(())
    }
}

/// A directory entry before encoding; `node` indexes the walk order.
#[derive(Clone)]
struct DirEnt {
    name: Vec<u8>,
    ft: u8,
    node: usize,
}

impl DirEnt {
    fn new(name: &str, ft: u8, node: usize) -> DirEnt {
        DirEnt {
            name: name.as_bytes().to_vec(),
            ft,
            node,
        }
    }
}

/// A directory's data: blocks of dirents, each followed by its names, every
/// block but the last padded to the block size. With no `nids`, the nids are
/// zero; the size is the same.
fn encode_dir(ents: &[DirEnt], nids: &[u64]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut i = 0;
    while i < ents.len() {
        let (mut n, mut used) = (0, 0);
        while i + n < ents.len() {
            let add = DIRENT_SIZE + ents[i + n].name.len();
            if used + add > BLOCK as usize {
                break;
            }
            used += add;
            n += 1;
        }
        let mut block = vec![0u8; used];
        let mut nameoff = DIRENT_SIZE * n;
        for (j, e) in ents[i..i + n].iter().enumerate() {
            let d = &mut block[j * DIRENT_SIZE..(j + 1) * DIRENT_SIZE];
            put64(d, 0, nids.get(e.node).copied().unwrap_or(0));
            put16(d, 8, nameoff as u16);
            d[10] = e.ft;
            block[nameoff..nameoff + e.name.len()].copy_from_slice(&e.name);
            nameoff += e.name.len();
        }
        i += n;
        if i < ents.len() {
            block.resize(BLOCK as usize, 0);
        }
        data.extend_from_slice(&block);
    }
    data
}

/// The inline xattr area: a 12-byte header, then entries padded to 4 bytes.
fn encode_xattrs(xattrs: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    if xattrs.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = vec![0u8; XATTR_HEADER];
    for (name, value) in xattrs {
        let (index, suffix) = [(1u8, "user."), (4, "trusted."), (6, "security.")]
            .iter()
            .find_map(|(i, p)| name.strip_prefix(p).map(|s| (*i, s)))
            .ok_or_else(|| Error::Tree(format!("xattr {name:?} has an unsupported prefix")))?;
        if suffix.is_empty() || suffix.len() > 255 || value.len() > u16::MAX as usize {
            return Err(Error::Tree(format!("xattr {name:?} is too long or empty")));
        }
        out.push(suffix.len() as u8);
        out.push(index);
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.extend_from_slice(suffix.as_bytes());
        out.extend_from_slice(value);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    if out.len() > BLOCK as usize - INODE_EXTENDED {
        return Err(Error::Tree("xattrs do not fit next to their inode".into()));
    }
    Ok(out)
}

fn xattr_icount(ibody: usize) -> u16 {
    if ibody == 0 {
        0
    } else {
        ((ibody - XATTR_HEADER) / 4 + 1) as u16
    }
}

fn copy_exactly(source: &mut dyn Read, out: &mut dyn Write, len: u64) -> Result<()> {
    let copied = io::copy(&mut source.take(len), out)?;
    let mut extra = [0u8; 1];
    if copied != len || source.read(&mut extra)? != 0 {
        return Err(Error::Tree(format!(
            "a file said it had {len} bytes but did not"
        )));
    }
    Ok(())
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
