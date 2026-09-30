//! The remote view: a volume's layers and blobs, listed in every readable
//! scope, and the tree the layers make when stacked (LAYERS.md §4).

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::future::try_join_all;
use futures_util::{StreamExt, TryStreamExt, stream};

use crate::api::{Api, ApiError, CacheItem, Rest, rest::Direction, rest::PER_PAGE};
use crate::entry::{self, DIR_XATTR, KeyKind, Kind, Mark, Version, Volume};
use crate::erofs;

/// A cache entry this filesystem wrote, as listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub key: String,
    pub version: String,
    pub size: u64,
    pub created: SystemTime,
    /// When the service last saw it used: its last download, or its creation.
    pub accessed: SystemTime,
    pub id: i64,
    /// Index into the index's scopes; 0 is the run's own ref.
    pub scope: usize,
}

/// What a listed item is.
#[derive(Clone, Debug)]
pub enum Item {
    /// A layer, whose metadata is its first `meta_blocks` blocks.
    Layer {
        entry: Listed,
        meta_blocks: u32,
    },
    Blob {
        entry: Listed,
        sha: String,
    },
}

/// A layer and the tree it holds.
#[derive(Debug)]
pub struct Layer {
    pub entry: Listed,
    pub meta_blocks: u32,
    /// Its device slots' tags, in slot order: the blobs and layers it
    /// refers to.
    pub devices: Vec<String>,
    /// Depth first, the root first.
    pub entries: Vec<erofs::Entry>,
    /// The download URL its metadata was read with, if any.
    pub url: Option<String>,
}

impl Layer {
    /// Lists a layer's metadata: at least its first `meta_blocks` blocks.
    pub fn parse(entry: Listed, meta_blocks: u32, metadata: &[u8]) -> erofs::Result<Layer> {
        let listing = erofs::read(metadata)?;
        Ok(Layer {
            entry,
            meta_blocks,
            devices: listing.devices.into_iter().map(|(tag, _)| tag).collect(),
            entries: listing.entries,
            url: None,
        })
    }
}

/// Which node of which layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId {
    pub layer: i64,
    pub nid: u64,
}

/// A file in the view.
#[derive(Debug)]
pub struct FileRef {
    pub id: NodeId,
    pub meta: erofs::Meta,
    pub size: u64,
    pub data: Where,
}

/// Where a file's bytes are.
#[derive(Clone, Debug)]
pub enum Where {
    /// In the layer's metadata, so already at hand.
    Inline(Arc<[u8]>),
    /// At `offset` in a layer.
    Layer { layer: Arc<Layer>, offset: u64 },
    /// At `offset` in a blob.
    Blob { blob: Listed, offset: u64 },
}

/// A node of the merged tree.
#[derive(Clone, Debug)]
pub enum Node {
    Dir {
        meta: erofs::Meta,
        /// The last layer to mark it `keep` or `drop` said `keep`.
        keep: bool,
    },
    File(Arc<FileRef>),
    Symlink {
        id: NodeId,
        meta: erofs::Meta,
        target: String,
    },
}

impl Node {
    pub fn is_dir(&self) -> bool {
        matches!(self, Node::Dir { .. })
    }

    /// The same node, for deciding what a new view changed.
    fn same(&self, other: &Node) -> bool {
        match (self, other) {
            (Node::Dir { meta: a, keep: x }, Node::Dir { meta: b, keep: y }) => a == b && x == y,
            (Node::File(a), Node::File(b)) => {
                let blob = |f: &FileRef| match &f.data {
                    Where::Blob { blob, .. } => Some(blob.id),
                    _ => None,
                };
                a.id == b.id && blob(a) == blob(b)
            }
            (Node::Symlink { id: a, .. }, Node::Symlink { id: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// The attributes of a directory no layer has.
pub fn default_dir_meta() -> erofs::Meta {
    erofs::Meta {
        mode: 0o755,
        uid: 0,
        gid: 0,
        mtime: SystemTime::UNIX_EPOCH,
    }
}

#[derive(Debug)]
pub struct Index {
    volume: Volume,
    scopes: Vec<String>,
    layers: Vec<Arc<Layer>>,
    /// Layers by the tag a device slot refers to them with.
    by_tag: HashMap<String, Arc<Layer>>,
    /// By digest: the entry to read a blob from.
    blobs: HashMap<String, Listed>,
    seen: HashSet<i64>,
    /// Per scope: newest `created` seen, for incremental refreshes.
    watermark: Vec<Option<SystemTime>>,
    /// The merged tree: every node that exists, by path, the root "" first.
    view: BTreeMap<String, Node>,
}

/// How far behind the watermark an incremental refresh looks, to tolerate
/// entries that are listed slightly out of order.
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

impl Index {
    pub fn new(volume: Volume, scopes: Vec<String>) -> Index {
        let n = scopes.len();
        let mut view = BTreeMap::new();
        view.insert(
            String::new(),
            Node::Dir {
                meta: default_dir_meta(),
                keep: false,
            },
        );
        Index {
            volume,
            scopes,
            layers: Vec::new(),
            by_tag: HashMap::new(),
            blobs: HashMap::new(),
            seen: HashSet::new(),
            watermark: vec![None; n],
            view,
        }
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// What a listed item is; `None` for entries this filesystem did not
    /// write, including format 1's, and for entries already known.
    pub fn classify(&self, item: &CacheItem) -> Option<Item> {
        if self.seen.contains(&item.id) {
            return None;
        }
        let version = Version::decode(&item.version)?;
        let scope = self.scopes.iter().position(|s| *s == item.git_ref)?;
        let entry = Listed {
            key: item.key.clone(),
            version: item.version.clone(),
            size: item.size_in_bytes,
            created: item.created(),
            accessed: item.accessed(),
            id: item.id,
            scope,
        };
        match (self.volume.parse(&item.key)?, version.kind) {
            (KeyKind::Layer, Kind::Layer { meta_blocks }) => {
                Some(Item::Layer { entry, meta_blocks })
            }
            (KeyKind::Blob(sha), Kind::Blob) => Some(Item::Blob { entry, sha }),
            _ => None,
        }
    }

    fn saw(&mut self, e: &Listed) -> bool {
        if e.scope >= self.scopes.len() || !self.seen.insert(e.id) {
            return false;
        }
        let wm = &mut self.watermark[e.scope];
        if wm.is_none_or(|w| w < e.created) {
            *wm = Some(e.created);
        }
        true
    }

    /// Records a layer. `restack` then shows it.
    pub fn insert_layer(&mut self, layer: Layer) -> bool {
        if !self.saw(&layer.entry) {
            return false;
        }
        let layer = Arc::new(layer);
        if let Some(tag) = self.volume.device_tag(&layer.entry.key) {
            self.by_tag.insert(tag, layer.clone());
        }
        self.layers.push(layer);
        true
    }

    /// Records a blob. `restack` then shows the files that refer to it.
    pub fn insert_blob(&mut self, entry: Listed, sha: String) -> bool {
        if !self.saw(&entry) {
            return false;
        }
        // Of several copies, read the one used last: the one least likely
        // to be evicted.
        match self.blobs.get(&sha) {
            Some(cur) if cur.accessed >= entry.accessed => {}
            _ => {
                self.blobs.insert(sha, entry);
            }
        }
        true
    }

    /// A blob with this digest, if any readable scope has one.
    pub fn blob(&self, sha: &str) -> Option<&Listed> {
        self.blobs.get(sha)
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Stacks the layers again, and returns the paths whose nodes changed,
    /// sorted, so parents come before their children.
    pub fn restack(&mut self) -> Vec<String> {
        let new = self.stack();
        let mut changed = Vec::new();
        let (mut a, mut b) = (self.view.iter().peekable(), new.iter().peekable());
        loop {
            match (a.peek(), b.peek()) {
                (None, None) => break,
                (Some((pa, _)), None) => {
                    changed.push((*pa).clone());
                    a.next();
                }
                (None, Some((pb, _))) => {
                    changed.push((*pb).clone());
                    b.next();
                }
                (Some((pa, na)), Some((pb, nb))) => match pa.cmp(pb) {
                    std::cmp::Ordering::Less => {
                        changed.push((*pa).clone());
                        a.next();
                    }
                    std::cmp::Ordering::Greater => {
                        changed.push((*pb).clone());
                        b.next();
                    }
                    std::cmp::Ordering::Equal => {
                        if !na.same(nb) {
                            changed.push((*pa).clone());
                        }
                        a.next();
                        b.next();
                    }
                },
            }
        }
        self.view = new;
        changed
    }

    /// Applies the layers bottom up, as LAYERS.md §4 says.
    fn stack(&self) -> BTreeMap<String, Node> {
        enum Stacked {
            Dir {
                meta: erofs::Meta,
                /// Some layer marked it `keep` or `attrs`.
                attrs_marked: bool,
                /// The last `keep` or `drop` mark.
                mark: Option<Mark>,
            },
            Leaf(Node),
            /// A file whose blob is gone: it hides what is below, but is
            /// not there itself.
            Gone,
        }
        let mut order: Vec<&Arc<Layer>> = self.layers.iter().collect();
        order.sort_by_key(|l| (Reverse(l.entry.scope), l.entry.created, l.entry.id));
        let mut tree: BTreeMap<String, Stacked> = BTreeMap::new();
        tree.insert(
            String::new(),
            Stacked::Dir {
                meta: default_dir_meta(),
                attrs_marked: false,
                mark: None,
            },
        );
        let remove_below = |tree: &mut BTreeMap<String, Stacked>, path: &str| {
            let prefix = format!("{path}/");
            let below: Vec<String> = tree
                .range(prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&prefix))
                .map(|(k, _)| k.clone())
                .collect();
            for k in below {
                tree.remove(&k);
            }
        };
        for layer in order {
            let mut unreadable = 0;
            for e in &layer.entries {
                let id = NodeId {
                    layer: layer.entry.id,
                    nid: e.nid,
                };
                match &e.kind {
                    _ if e.is_whiteout() => {
                        if matches!(tree.get(&e.path), Some(Stacked::Leaf(_) | Stacked::Gone)) {
                            tree.remove(&e.path);
                        }
                    }
                    erofs::EntryKind::Dir => {
                        let marked = e.xattr(DIR_XATTR).and_then(Mark::parse);
                        let sets_attrs = matches!(marked, Some(Mark::Keep | Mark::Attrs));
                        let new_mark = marked.filter(|m| *m != Mark::Attrs);
                        match tree.get_mut(&e.path) {
                            Some(Stacked::Dir {
                                meta,
                                attrs_marked,
                                mark,
                            }) => {
                                if sets_attrs || !*attrs_marked {
                                    *meta = e.meta;
                                }
                                *attrs_marked |= sets_attrs;
                                if new_mark.is_some() {
                                    *mark = new_mark;
                                }
                            }
                            _ => {
                                tree.insert(
                                    e.path.clone(),
                                    Stacked::Dir {
                                        meta: e.meta,
                                        attrs_marked: sets_attrs,
                                        mark: new_mark,
                                    },
                                );
                            }
                        }
                    }
                    erofs::EntryKind::File | erofs::EntryKind::Symlink(_) if !e.path.is_empty() => {
                        let leaf = match &e.kind {
                            erofs::EntryKind::Symlink(target) => {
                                Some(Stacked::Leaf(Node::Symlink {
                                    id,
                                    meta: e.meta,
                                    target: target.clone(),
                                }))
                            }
                            _ => match self.locate(layer, e) {
                                Ok(Some(data)) => {
                                    Some(Stacked::Leaf(Node::File(Arc::new(FileRef {
                                        id,
                                        meta: e.meta,
                                        size: e.size,
                                        data,
                                    }))))
                                }
                                Ok(None) => Some(Stacked::Gone),
                                Err(()) => {
                                    unreadable += 1;
                                    None
                                }
                            },
                        };
                        if let Some(leaf) = leaf {
                            remove_below(&mut tree, &e.path);
                            tree.insert(e.path.clone(), leaf);
                        }
                    }
                    // Nothing we write, and nothing a mount can show.
                    _ => {}
                }
            }
            if unreadable > 0 {
                tracing::warn!(
                    "{}: {unreadable} files are laid out in ways this version cannot read",
                    layer.entry.key
                );
            }
        }

        // A directory exists while it is kept, or anything below it exists.
        let mut needed: HashSet<&str> = HashSet::new();
        needed.insert("");
        for (path, s) in &tree {
            let anchor = match s {
                Stacked::Leaf(_) => true,
                Stacked::Dir { mark, .. } => *mark == Some(Mark::Keep),
                Stacked::Gone => false,
            };
            if !anchor {
                continue;
            }
            let mut p = path.as_str();
            while !p.is_empty() {
                p = entry::parent(p);
                if !needed.insert(p) {
                    break;
                }
            }
        }
        let mut view = BTreeMap::new();
        for (path, s) in &tree {
            match s {
                Stacked::Leaf(node) => {
                    view.insert(path.clone(), node.clone());
                }
                Stacked::Dir { meta, mark, .. } => {
                    let keep = *mark == Some(Mark::Keep);
                    if keep || needed.contains(path.as_str()) {
                        view.insert(path.clone(), Node::Dir { meta: *meta, keep });
                    }
                }
                Stacked::Gone => {}
            }
        }
        view
    }

    /// Where a file's bytes are: `Ok(None)` if the blob or layer it refers
    /// to is gone, and `Err` for layouts our writer never uses.
    fn locate(&self, layer: &Arc<Layer>, e: &erofs::Entry) -> Result<Option<Where>, ()> {
        match &e.data {
            erofs::Data::None => Ok(Some(Where::Inline(Arc::from(&[][..])))),
            erofs::Data::Flat {
                blocks: 0, tail, ..
            } => Ok(Some(Where::Inline(Arc::from(tail.as_slice())))),
            erofs::Data::Flat { start, tail, .. } if tail.is_empty() => Ok(Some(Where::Layer {
                layer: layer.clone(),
                offset: start * erofs::BLOCK,
            })),
            erofs::Data::Flat { .. } => Err(()),
            erofs::Data::Chunks { chunk_size, chunks } => {
                // One device, and consecutive chunks: one range of one entry.
                let first = chunks.first().ok_or(())?;
                let start = first.start.ok_or(())?;
                let blocks_per_chunk = chunk_size / erofs::BLOCK;
                let contiguous = chunks.iter().enumerate().all(|(i, c)| {
                    c.device == first.device && c.start == Some(start + i as u64 * blocks_per_chunk)
                });
                if !contiguous {
                    return Err(());
                }
                let offset = start * erofs::BLOCK;
                if first.device == 0 {
                    return Ok(Some(Where::Layer {
                        layer: layer.clone(),
                        offset,
                    }));
                }
                let tag = layer.devices.get(first.device as usize - 1).ok_or(())?;
                if tag.starts_with("layer/") {
                    return Ok(self.by_tag.get(tag).map(|layer| Where::Layer {
                        layer: layer.clone(),
                        offset,
                    }));
                }
                Ok(self.blobs.get(tag).map(|blob| Where::Blob {
                    blob: blob.clone(),
                    offset,
                }))
            }
        }
    }

    /// The node at a path of the volume.
    pub fn get(&self, path: &str) -> Option<&Node> {
        self.view.get(path)
    }

    /// Whether a file or symlink is at `path`: what a whiteout would hide.
    pub fn leaf_at(&self, path: &str) -> bool {
        self.view.get(path).is_some_and(|n| !n.is_dir())
    }

    /// Whether a layer keeps the directory at `path`.
    pub fn kept(&self, path: &str) -> bool {
        matches!(self.view.get(path), Some(Node::Dir { keep: true, .. }))
    }

    /// Every path in the view, sorted, parents first.
    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.view.keys()
    }

    /// Visible files' total size, for `statfs`.
    pub fn visible_bytes(&self) -> u64 {
        self.view
            .values()
            .map(|n| match n {
                Node::File(f) => f.size,
                _ => 0,
            })
            .sum()
    }

    /// Blobs that visible files refer to, last used before `cutoff`, the
    /// stalest first. Nothing else keeps them alive (LAYERS.md §7).
    pub fn stale_blobs(&self, cutoff: SystemTime, max: usize) -> Vec<Listed> {
        let mut seen = HashSet::new();
        let mut out: Vec<&Listed> = self
            .view
            .values()
            .filter_map(|n| match n {
                Node::File(f) => match &f.data {
                    Where::Blob { blob, .. } if blob.accessed < cutoff => Some(blob),
                    _ => None,
                },
                _ => None,
            })
            .filter(|b| seen.insert(b.id))
            .collect();
        out.sort_by_key(|b| b.accessed);
        out.into_iter().take(max).cloned().collect()
    }

    pub fn watermarks(&self) -> Vec<Option<SystemTime>> {
        self.watermark.clone()
    }

    /// A creation time for our own new layer that stacks it above every
    /// layer of our scope.
    pub fn fresh_created(&self) -> SystemTime {
        let now = SystemTime::now();
        let newest = self
            .layers
            .iter()
            .filter(|l| l.entry.scope == 0)
            .map(|l| l.entry.created)
            .max();
        match newest {
            Some(t) if t >= now => t + Duration::from_micros(1),
            _ => now,
        }
    }
}

/// Reads the metadata of layers, a few at a time. Layers that are gone, or
/// that cannot be read, are left out; errors of the service are not.
pub async fn read_layers(api: &Api, layers: Vec<(Listed, u32)>) -> Result<Vec<Layer>, ApiError> {
    let read = stream::iter(layers)
        .map(|(entry, meta_blocks)| read_layer(api, entry, meta_blocks))
        .buffer_unordered(LAYER_READS)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(read.into_iter().flatten().collect())
}

/// Layers whose metadata is read at once.
const LAYER_READS: usize = 16;

async fn read_layer(api: &Api, entry: Listed, meta_blocks: u32) -> Result<Option<Layer>, ApiError> {
    let len = (u64::from(meta_blocks) * erofs::BLOCK).min(entry.size);
    for attempt in 0..3 {
        let Some(url) = api.twirp.download_url(&entry.key, &entry.version).await? else {
            tracing::debug!("{}: gone before its metadata was read", entry.key);
            return Ok(None);
        };
        let bytes = match api.blob.get_range(&url, 0, len).await {
            Ok(b) => b,
            // Expired URLs, or a blob that moved: resolve again.
            Err(ApiError::Expired | ApiError::NotFound) if attempt < 2 => continue,
            Err(ApiError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        return match Layer::parse(entry.clone(), meta_blocks, &bytes) {
            Ok(mut layer) => {
                layer.url = Some(url);
                Ok(Some(layer))
            }
            Err(e) => {
                tracing::warn!("{}: not a layer this version can read: {e}", entry.key);
                Ok(None)
            }
        };
    }
    Err(ApiError::Server(format!(
        "{}: download URL keeps being rejected",
        entry.key
    )))
}

/// Lists every entry under `prefix` in each scope.
pub async fn list_all(
    rest: &Rest,
    prefix: &str,
    scopes: &[String],
) -> Result<Vec<CacheItem>, ApiError> {
    let per_scope = scopes.iter().map(|scope| list_scope(rest, prefix, scope));
    Ok(try_join_all(per_scope)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// Listing again when the entries changed while we paged, at most this often.
const LISTINGS: usize = 3;

async fn list_scope(rest: &Rest, prefix: &str, scope: &str) -> Result<Vec<CacheItem>, ApiError> {
    // Pages are cut by position, so an entry deleted while we page (by
    // eviction, or by another job's gc) moves an entry across a page boundary
    // that we have already read, and we never see it. Every page reports the
    // total; when they disagree, list again. If no listing is steady, keep
    // everything any of them saw.
    let mut seen: HashMap<i64, CacheItem> = HashMap::new();
    for _ in 0..LISTINGS {
        let (items, steady) = list_pages(rest, prefix, scope).await?;
        if steady {
            return Ok(items);
        }
        tracing::debug!("{scope}: entries changed while listing them; listing again");
        seen.extend(items.into_iter().map(|i| (i.id, i)));
    }
    Ok(seen.into_values().collect())
}

/// One listing, and whether every page saw the same total.
async fn list_pages(
    rest: &Rest,
    prefix: &str,
    scope: &str,
) -> Result<(Vec<CacheItem>, bool), ApiError> {
    // Ascending order keeps pages stable while new entries are appended;
    // anything appended while we page is picked up by the next refresh.
    let (total, mut items) = rest.list_page(prefix, scope, Direction::Asc, 1).await?;
    let pages = (total as usize).div_ceil(PER_PAGE);
    let mut steady = true;
    let mut page = 2;
    while page <= pages {
        // A handful of pages in flight at once.
        let last = pages.min(page + 7);
        let batch = (page..=last).map(|p| rest.list_page(prefix, scope, Direction::Asc, p));
        for (page_total, page_items) in try_join_all(batch).await? {
            steady &= page_total == total;
            items.extend(page_items);
        }
        page = last + 1;
    }
    let steady = steady && items.len() as u64 == total;
    Ok((items, steady))
}

/// Lists entries created since `watermark` (minus a margin), newest first.
pub async fn list_since(
    rest: &Rest,
    prefix: &str,
    scope: &str,
    watermark: Option<SystemTime>,
) -> Result<Vec<CacheItem>, ApiError> {
    let Some(watermark) = watermark else {
        return list_scope(rest, prefix, scope).await;
    };
    let cutoff = watermark
        .checked_sub(REFRESH_MARGIN)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut out = Vec::new();
    for page in 1.. {
        let (_, items) = rest.list_page(prefix, scope, Direction::Desc, page).await?;
        let n = items.len();
        let older = items.iter().all(|i| i.created() < cutoff);
        out.extend(items.into_iter().filter(|i| i.created() >= cutoff));
        if n < PER_PAGE || older {
            break;
        }
    }
    Ok(out)
}

/// A layer image holding `files` (paths and contents) in directories that
/// pass through, for tests and benchmarks that need layers quickly.
pub fn layer_image(files: &[(String, Vec<u8>)]) -> erofs::Result<(Vec<u8>, Version)> {
    use erofs::{FileData, Image, Node as ImageNode, NodeKind};
    let meta = |mode| erofs::Meta {
        mode,
        uid: 0,
        gid: 0,
        mtime: SystemTime::now(),
    };
    let mut image = Image::new(meta(0o755));
    for (path, _) in files {
        let mut p = entry::parent(path);
        while !p.is_empty() {
            image.insert(
                p,
                ImageNode {
                    meta: meta(0o755),
                    kind: NodeKind::Dir,
                    xattrs: Vec::new(),
                },
            )?;
            p = entry::parent(p);
        }
    }
    for (path, data) in files {
        image.insert(
            path,
            ImageNode {
                meta: meta(0o644),
                kind: NodeKind::File(FileData::Here {
                    len: data.len() as u64,
                    source: Box::new(std::io::Cursor::new(data.clone())),
                }),
                xattrs: Vec::new(),
            },
        )?;
    }
    let mut out = Vec::new();
    let written = image.write_to(&mut out)?;
    Ok((out, Version::layer(written.meta_blocks as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erofs::{FileData, Image, Node as ImageNode, NodeKind};

    const SECS: u64 = 1_790_000_000;

    fn meta(mode: u16, secs: u64) -> erofs::Meta {
        erofs::Meta {
            mode,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(SECS + secs),
        }
    }

    enum N<'a> {
        Dir(Option<Mark>),
        File(&'a [u8]),
        Link(&'a str),
        Whiteout,
        /// A file on the first device.
        OnBlob(u64),
    }

    /// A parsed layer from `(path, node)`s. Ancestors must be listed.
    fn layer(id: i64, scope: usize, secs: u64, nodes: &[(&str, N)], blobs: &[&str]) -> Layer {
        let mut image = Image::new(meta(0o755, secs));
        for sha in blobs {
            image
                .add_device(erofs::Device {
                    tag: sha.to_string(),
                    blocks: 1 << 12,
                })
                .unwrap();
        }
        for (path, n) in nodes {
            let (kind, xattrs, mode) = match n {
                N::Dir(mark) => (
                    NodeKind::Dir,
                    mark.map(|m| vec![(DIR_XATTR.to_string(), m.as_bytes().to_vec())])
                        .unwrap_or_default(),
                    0o700,
                ),
                N::File(data) => (
                    NodeKind::File(FileData::Here {
                        len: data.len() as u64,
                        source: Box::new(std::io::Cursor::new(data.to_vec())),
                    }),
                    Vec::new(),
                    0o644,
                ),
                N::Link(t) => (NodeKind::Symlink(t.to_string()), Vec::new(), 0o777),
                N::Whiteout => (NodeKind::Whiteout, Vec::new(), 0),
                N::OnBlob(len) => (
                    NodeKind::File(FileData::Device {
                        len: *len,
                        device: 1,
                        start: 0,
                    }),
                    Vec::new(),
                    0o644,
                ),
            };
            image
                .insert(
                    path,
                    ImageNode {
                        meta: meta(mode, secs),
                        kind,
                        xattrs,
                    },
                )
                .unwrap();
        }
        let mut out = Vec::new();
        let written = image.write_to(&mut out).unwrap();
        let entry = Listed {
            key: format!("gha-fs/default/layer/{id:016x}"),
            version: String::new(),
            size: out.len() as u64,
            created: SystemTime::UNIX_EPOCH + Duration::from_secs(SECS + secs),
            accessed: SystemTime::UNIX_EPOCH + Duration::from_secs(SECS + secs),
            id,
            scope,
        };
        Layer::parse(entry, written.meta_blocks as u32, &out).unwrap()
    }

    fn index() -> Index {
        Index::new(
            Volume::new("default").unwrap(),
            vec!["refs/heads/feature".into(), "refs/heads/main".into()],
        )
    }

    fn paths(ix: &Index) -> Vec<&str> {
        ix.paths().map(String::as_str).collect()
    }

    fn content(ix: &Index, path: &str) -> Option<Vec<u8>> {
        match ix.get(path)? {
            Node::File(f) => match &f.data {
                Where::Inline(b) => Some(b.to_vec()),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn newer_layers_and_nearer_scopes_win() {
        let mut ix = index();
        ix.insert_layer(layer(1, 0, 10, &[("f", N::File(b"own, old"))], &[]));
        ix.insert_layer(layer(2, 1, 99, &[("f", N::File(b"main, newest"))], &[]));
        ix.insert_layer(layer(3, 0, 20, &[("f", N::File(b"own, new"))], &[]));
        assert_eq!(ix.restack(), ["", "f"]);
        assert_eq!(content(&ix, "f").unwrap(), b"own, new");
        assert!(
            !ix.insert_layer(layer(3, 0, 20, &[], &[])),
            "duplicates are ignored"
        );
    }

    #[test]
    fn whiteouts_hide_files_but_leave_directories_alone() {
        let mut ix = index();
        ix.insert_layer(layer(
            1,
            1,
            10,
            &[
                ("f", N::File(b"x")),
                ("d", N::Dir(None)),
                ("d/g", N::File(b"y")),
                ("l", N::Link("f")),
            ],
            &[],
        ));
        ix.insert_layer(layer(
            2,
            0,
            20,
            &[("d", N::Whiteout), ("f", N::Whiteout), ("l", N::Whiteout)],
            &[],
        ));
        ix.restack();
        assert_eq!(paths(&ix), ["", "d", "d/g"]);
    }

    #[test]
    fn files_replace_directories_and_directories_replace_files() {
        let mut ix = index();
        ix.insert_layer(layer(
            1,
            0,
            10,
            &[
                ("d", N::Dir(None)),
                ("d/f", N::File(b"x")),
                ("g", N::File(b"y")),
            ],
            &[],
        ));
        ix.insert_layer(layer(
            2,
            0,
            20,
            &[
                ("d", N::File(b"now a file")),
                ("g", N::Dir(Some(Mark::Keep))),
            ],
            &[],
        ));
        let changed = ix.restack();
        assert_eq!(paths(&ix), ["", "d", "g"]);
        assert!(!ix.get("d").unwrap().is_dir());
        assert!(ix.kept("g"));
        // The first stack reports everything; a restack only what changed.
        assert_eq!(changed, ["", "d", "g"]);
        ix.insert_layer(layer(3, 0, 30, &[("d", N::Whiteout)], &[]));
        assert_eq!(ix.restack(), ["", "d"]);
    }

    #[test]
    fn directory_marks() {
        let mut ix = index();
        // Kept and emptied; only passed through; kept, then dropped; and
        // dropped with something still below it.
        ix.insert_layer(layer(
            1,
            1,
            10,
            &[
                ("kept", N::Dir(Some(Mark::Keep))),
                ("kept/f", N::File(b"x")),
                ("passed", N::Dir(None)),
                ("passed/f", N::File(b"x")),
                ("dropped", N::Dir(Some(Mark::Keep))),
                ("busy", N::Dir(Some(Mark::Keep))),
                ("busy/f", N::File(b"x")),
            ],
            &[],
        ));
        ix.insert_layer(layer(
            2,
            0,
            20,
            &[
                ("kept", N::Dir(None)),
                ("kept/f", N::Whiteout),
                ("passed", N::Dir(None)),
                ("passed/f", N::Whiteout),
                ("dropped", N::Dir(Some(Mark::Drop))),
                ("busy", N::Dir(Some(Mark::Drop))),
            ],
            &[],
        ));
        ix.restack();
        assert_eq!(paths(&ix), ["", "busy", "busy/f", "kept"]);
        assert!(!ix.kept("busy"));
    }

    #[test]
    fn attributes_come_from_the_last_mark() {
        let mut ix = index();
        ix.insert_layer(layer(
            1,
            0,
            10,
            &[("d", N::Dir(Some(Mark::Attrs))), ("d/f", N::File(b""))],
            &[],
        ));
        ix.insert_layer(layer(
            2,
            0,
            20,
            &[("d", N::Dir(None)), ("d/g", N::File(b""))],
            &[],
        ));
        ix.restack();
        let Some(Node::Dir { meta: m, .. }) = ix.get("d") else {
            panic!("no directory")
        };
        assert_eq!(
            m.mtime,
            meta(0, 10).mtime,
            "a layer passing through does not change them"
        );
        ix.insert_layer(layer(
            3,
            0,
            30,
            &[("e", N::Dir(None)), ("e/f", N::File(b""))],
            &[],
        ));
        ix.insert_layer(layer(
            4,
            0,
            40,
            &[("e", N::Dir(None)), ("e/g", N::File(b""))],
            &[],
        ));
        ix.restack();
        let Some(Node::Dir { meta: m, .. }) = ix.get("e") else {
            panic!("no directory")
        };
        assert_eq!(
            m.mtime,
            meta(0, 40).mtime,
            "without marks, the last layer that has it"
        );
    }

    #[test]
    fn files_on_blobs_need_their_blob() {
        let sha = "c".repeat(64);
        let mut ix = index();
        ix.insert_layer(layer(1, 1, 10, &[("f", N::File(b"old"))], &[]));
        ix.insert_layer(layer(2, 0, 20, &[("f", N::OnBlob(20 << 20))], &[&sha]));
        ix.restack();
        assert_eq!(
            paths(&ix),
            [""],
            "a missing blob hides the file, and what it replaced"
        );
        let blob = Listed {
            key: format!("gha-fs/default/blob/{sha}"),
            version: String::new(),
            size: 20 << 20,
            created: SystemTime::now(),
            accessed: SystemTime::UNIX_EPOCH,
            id: 7,
            scope: 1,
        };
        ix.insert_blob(blob, sha.clone());
        assert_eq!(ix.restack(), ["f"]);
        match ix.get("f") {
            Some(Node::File(f)) => {
                assert_eq!(f.size, 20 << 20);
                assert!(matches!(&f.data, Where::Blob { blob, offset: 0 } if blob.id == 7));
            }
            other => panic!("{other:?}"),
        }
        let stale = ix.stale_blobs(SystemTime::now(), 10);
        assert_eq!(stale.len(), 1);
        assert_eq!(ix.visible_bytes(), 20 << 20);
    }

    #[test]
    fn what_is_a_leaf() {
        let mut ix = index();
        ix.insert_layer(layer(
            1,
            0,
            10,
            &[("a", N::Dir(None)), ("a/b", N::File(b"x"))],
            &[],
        ));
        ix.restack();
        assert!(ix.leaf_at("a/b"));
        assert!(!ix.leaf_at("a"));
    }

    #[test]
    fn classifying_listed_items() {
        let ix = index();
        let item = |key: &str, version: String, git_ref: &str| CacheItem {
            id: 1,
            git_ref: git_ref.into(),
            key: key.into(),
            version,
            size_in_bytes: 5,
            created_at: "2026-01-01T00:00:00Z".into(),
            last_accessed_at: String::new(),
        };
        let layer_key = ix.volume().layer_key(1);
        let blob_key = ix.volume().blob_key(&"a".repeat(64));
        let main = "refs/heads/main";
        assert!(matches!(
            ix.classify(&item(&layer_key, Version::layer(3).encode(), main)),
            Some(Item::Layer { meta_blocks: 3, entry }) if entry.scope == 1
        ));
        assert!(matches!(
            ix.classify(&item(&blob_key, Version::blob().encode(), main)),
            Some(Item::Blob { .. })
        ));
        // Mismatched kinds, foreign versions, other volumes, other refs.
        assert!(
            ix.classify(&item(&blob_key, Version::layer(3).encode(), main))
                .is_none()
        );
        assert!(
            ix.classify(&item(&layer_key, "a".repeat(64), main))
                .is_none()
        );
        let other = Volume::new("other").unwrap().layer_key(1);
        assert!(
            ix.classify(&item(&other, Version::layer(3).encode(), main))
                .is_none()
        );
        assert!(
            ix.classify(&item(
                &layer_key,
                Version::layer(3).encode(),
                "refs/heads/x"
            ))
            .is_none()
        );
    }

    #[test]
    fn test_layers_parse() {
        let (image, version) = layer_image(&[
            ("a/b/c".into(), b"hello".to_vec()),
            ("d".into(), vec![7; 5000]),
        ])
        .unwrap();
        let Kind::Layer { meta_blocks } = version.kind else {
            panic!()
        };
        let entry = Listed {
            key: String::new(),
            version: version.encode(),
            size: image.len() as u64,
            created: SystemTime::now(),
            accessed: SystemTime::now(),
            id: 1,
            scope: 0,
        };
        let parsed =
            Layer::parse(entry, meta_blocks, &image[..meta_blocks as usize * 4096]).unwrap();
        let mut ix = index();
        ix.insert_layer(parsed);
        ix.restack();
        assert_eq!(paths(&ix), ["", "a", "a/b", "a/b/c", "d"]);
        assert_eq!(content(&ix, "a/b/c").unwrap(), b"hello");
        assert!(
            matches!(ix.get("d"), Some(Node::File(f)) if matches!(f.data, Where::Layer { .. }))
        );
    }
}
