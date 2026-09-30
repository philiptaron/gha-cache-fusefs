//! Snapshots: at unmount, a layer that stands in for the older layers of
//! the mount's scope, so that they need not be read, and can expire
//! (LAYERS.md §8).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::Vfs;
use super::commit::{DataWriter, meta, reference, upload_image, writer};
use crate::api::CacheItem;
use crate::entry::{DIR_XATTR, Mark, Version, Volume, WRITER_XATTR};
use crate::erofs::{self, FileData, Image, Node, NodeKind};
use crate::index::{Index, Item, Layer, Listed, Node as ViewNode, Stacked, Where};

impl Vfs {
    /// Writes a snapshot of the mount's scope, if it would cover enough
    /// layers. Returns whether it did.
    pub(super) async fn snapshot(&self) -> anyhow::Result<bool> {
        let inner = &self.0;
        let cfg = &inner.cfg;
        if cfg.read_only || cfg.snapshot_after == 0 {
            return Ok(false);
        }
        let cutoff = SystemTime::now()
            .checked_sub(cfg.snapshot_margin)
            .unwrap_or(UNIX_EPOCH);
        // Listing costs REST requests, so only when the layers the mount
        // knows of already make a snapshot worth it.
        if inner.st.lock().index.uncovered(0, cutoff) < cfg.snapshot_after {
            return Ok(false);
        }
        // Every layer the snapshot covers must have been read, and every
        // blob and layer they refer to listed; and they stack in the order
        // of the times the service gives them. So refresh, and list the
        // scope afresh.
        self.refresh().await?;
        let scope = inner.st.lock().index.scopes()[0].clone();
        let items = crate::index::list_scope(&inner.api.rest, cfg.volume.prefix(), &scope).await?;
        let built = {
            let st = inner.st.lock();
            let Some((covers, layers)) = plan(&st.index, &items, cutoff, cfg.snapshot_after) else {
                return Ok(false);
            };
            let (tree, exists) = st.index.stack_scope(&layers);
            let bottom = st.index.scopes().len() == 1;
            let n = layers.iter().filter(|l| l.covers.is_none()).count();
            (
                covers,
                n,
                tree.len(),
                image(&cfg.volume, &tree, &exists, bottom),
            )
        };
        let (covers, n, paths, image) = built;
        let image = image?;
        let sealed = inner.store.create()?;
        let file = sealed.clone();
        let written = tokio::task::spawn_blocking(move || {
            image.write_to(&mut DataWriter {
                file: &file,
                pos: 0,
            })
        })
        .await
        .expect("sealing does not panic")?;
        let size = written.blocks * erofs::BLOCK;
        let (key, _, _) = upload_image(inner, &sealed, size, || {
            Version::snapshot(written.meta_blocks as u32, covers)
        })
        .await?;
        inner.stats.snapshots.fetch_add(1, Ordering::Relaxed);
        tracing::info!("wrote snapshot {key}: {n} layers, {paths} paths, {size} bytes");
        Ok(true)
    }
}

/// What a snapshot of scope 0 would cover and be made of: its T, and the
/// layers to stack, the mount's own snapshot first. `None` if it would
/// cover fewer than `min` layers that the mount's snapshot does not.
fn plan(
    index: &Index,
    items: &[CacheItem],
    cutoff: SystemTime,
    min: usize,
) -> Option<(SystemTime, Vec<Arc<Layer>>)> {
    let base = index.covered_before(0);
    let mut listed: Vec<Listed> = items
        .iter()
        .filter_map(|i| match index.describe(i)? {
            Item::Layer { entry, .. } if entry.scope == 0 => Some(entry),
            _ => None,
        })
        .filter(|e| e.created < cutoff && base.is_none_or(|t| e.created >= t))
        .collect();
    listed.sort_by_key(|e| (e.created, e.id));
    // A layer this mount could not read ends what the snapshot covers.
    let mut covers = None;
    let mut layers = Vec::new();
    for e in &listed {
        match index.layer(e.id) {
            Some(l) if !l.covered && l.covers.is_none() => layers.push((e.created, l.clone())),
            _ => {
                tracing::debug!("{}: not read here; the snapshot stops before it", e.key);
                covers = Some(e.created);
                break;
            }
        }
    }
    let covers = match covers {
        Some(t) => t,
        None => listed.last()?.created + Duration::from_micros(1),
    };
    layers.retain(|(created, _)| *created < covers);
    if layers.len() < min {
        return None;
    }
    let base = index.snapshot(0).cloned();
    Some((
        covers,
        base.into_iter()
            .chain(layers.into_iter().map(|(_, l)| l))
            .collect(),
    ))
}

/// The snapshot's image: the stack, with what the scopes below need
/// (LAYERS.md §8). The default branch's scope, `bottom`, has none below it.
fn image(
    volume: &Volume,
    tree: &BTreeMap<String, Stacked>,
    exists: &HashSet<String>,
    bottom: bool,
) -> erofs::Result<Image> {
    let root = match tree.get("") {
        Some(Stacked::Dir { meta, .. }) => *meta,
        _ => meta(0o755, SystemTime::now()),
    };
    let mut image = Image::new(root);
    let mut slots: HashMap<String, u16> = HashMap::new();
    for (path, s) in tree {
        let (kind, meta, mark) = match s {
            Stacked::Leaf(ViewNode::File(f)) => {
                let data = match &f.data {
                    Where::Inline(bytes) => FileData::Here {
                        len: f.size,
                        source: Box::new(std::io::Cursor::new(bytes.clone())),
                    },
                    Where::Layer { layer, offset } => reference(
                        &mut image,
                        &mut slots,
                        volume,
                        &layer.entry,
                        *offset,
                        f.size,
                    )?,
                    Where::Blob { blob, offset } => {
                        reference(&mut image, &mut slots, volume, blob, *offset, f.size)?
                    }
                };
                (NodeKind::File(data), f.meta, None)
            }
            Stacked::Leaf(ViewNode::Symlink { meta, target, .. }) => {
                (NodeKind::Symlink(target.clone()), *meta, None)
            }
            Stacked::Leaf(ViewNode::Dir { .. }) => unreachable!("stacks hold directories as Dir"),
            Stacked::Gone | Stacked::Whiteout if bottom => continue,
            Stacked::Gone | Stacked::Whiteout => {
                (NodeKind::Whiteout, meta(0, SystemTime::now()), None)
            }
            Stacked::Dir {
                meta,
                attrs_marked,
                mark,
            } => {
                let mut mark = match (attrs_marked, mark) {
                    (_, Some(Mark::Keep)) => Some(Mark::Keep),
                    (true, Some(_)) => Some(Mark::AttrsDrop),
                    (false, Some(_)) => Some(Mark::Drop),
                    (true, None) => Some(Mark::Attrs),
                    (false, None) => None,
                };
                if bottom {
                    if !exists.contains(path) {
                        continue;
                    }
                    mark = match mark {
                        Some(Mark::AttrsDrop) => Some(Mark::Attrs),
                        Some(Mark::Drop) => None,
                        m => m,
                    };
                }
                (NodeKind::Dir, *meta, mark)
            }
        };
        let mut xattrs: Vec<(String, Vec<u8>)> = mark
            .map(|m| vec![(DIR_XATTR.to_string(), m.as_bytes().to_vec())])
            .unwrap_or_default();
        if path.is_empty() {
            xattrs.push((WRITER_XATTR.to_string(), writer().into_bytes()));
        }
        image.insert(path, Node { meta, kind, xattrs })?;
    }
    Ok(image)
}
