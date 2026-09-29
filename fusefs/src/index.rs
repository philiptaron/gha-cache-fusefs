//! The remote view: listed cache entries, merged across scopes.
//!
//! Per key, the highest-precedence scope that has the key decides, and within
//! a scope the newest entry wins (DESIGN.md §3.3).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, SystemTime};

use futures_util::future::try_join_all;

use crate::api::{ApiError, CacheItem, Rest, rest::Direction, rest::PER_PAGE};
use crate::entry::{Kind, Meta};

/// A cache entry written by this filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    pub key: String,
    pub version: String,
    pub meta: Meta,
    /// Blob size (1 for placeholders).
    pub size: u64,
    pub created: SystemTime,
    /// When the service last saw it used: its last download, or its creation.
    pub accessed: SystemTime,
    pub id: i64,
    /// Index into the index's scopes; 0 is the run's own ref.
    pub scope: usize,
}

impl RemoteEntry {
    /// Size of the file this entry represents.
    pub fn logical_size(&self) -> u64 {
        if self.meta.empty { 0 } else { self.size }
    }

    pub fn is_visible(&self) -> bool {
        self.meta.kind != Kind::Whiteout
    }

    fn newer_than(&self, other: &RemoteEntry) -> bool {
        (self.created, self.id) > (other.created, other.id)
    }
}

#[derive(Debug, Default)]
pub struct Index {
    scopes: Vec<String>,
    /// Per scope: key → newest entry.
    winners: Vec<BTreeMap<String, RemoteEntry>>,
    /// Per scope: entries superseded by a newer one with the same key.
    superseded: Vec<Vec<RemoteEntry>>,
    seen: HashSet<i64>,
    /// Per scope: newest `created` seen, for incremental refreshes.
    watermark: Vec<Option<SystemTime>>,
}

/// How far behind the watermark an incremental refresh looks, to tolerate
/// entries that are listed slightly out of order.
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

impl Index {
    pub fn new(scopes: Vec<String>) -> Index {
        let n = scopes.len();
        Index {
            scopes,
            winners: vec![BTreeMap::new(); n],
            superseded: vec![Vec::new(); n],
            seen: HashSet::new(),
            watermark: vec![None; n],
        }
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// Converts a listed item, ignoring entries this filesystem did not write.
    pub fn entry_from_item(&self, item: &CacheItem) -> Option<RemoteEntry> {
        let meta = Meta::decode(&item.version)?;
        let scope = self.scopes.iter().position(|s| *s == item.git_ref)?;
        Some(RemoteEntry {
            key: item.key.clone(),
            version: item.version.clone(),
            meta,
            size: item.size_in_bytes,
            created: item.created(),
            accessed: item.accessed(),
            id: item.id,
            scope,
        })
    }

    /// Records an entry. Returns whether the merged winner for its key may have changed.
    pub fn insert(&mut self, e: RemoteEntry) -> bool {
        if e.scope >= self.scopes.len() || !self.seen.insert(e.id) {
            return false;
        }
        let wm = &mut self.watermark[e.scope];
        if wm.is_none_or(|w| w < e.created) {
            *wm = Some(e.created);
        }
        let winners = &mut self.winners[e.scope];
        match winners.get(&e.key) {
            Some(cur) if !e.newer_than(cur) => {
                self.superseded[e.scope].push(e);
                false
            }
            _ => {
                let scope = e.scope;
                if let Some(old) = winners.insert(e.key.clone(), e) {
                    self.superseded[scope].push(old);
                }
                true
            }
        }
    }

    /// Forgets an entry (after deleting it, or when it turns out to be gone).
    /// Returns whether the merged winner for its key may have changed.
    pub fn remove(&mut self, key: &str, id: i64) -> bool {
        let mut changed = false;
        for scope in 0..self.scopes.len() {
            self.superseded[scope].retain(|e| e.id != id);
            if self.winners[scope].get(key).is_some_and(|e| e.id == id) {
                self.winners[scope].remove(key);
                // Promote the newest superseded entry, if any.
                let best = self.superseded[scope]
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.key == key)
                    .max_by(|(_, a), (_, b)| (a.created, a.id).cmp(&(b.created, b.id)))
                    .map(|(i, _)| i);
                if let Some(i) = best {
                    let e = self.superseded[scope].swap_remove(i);
                    self.winners[scope].insert(key.to_string(), e);
                }
                changed = true;
            }
        }
        changed
    }

    /// The entry that decides `key`: from the highest-precedence scope that has
    /// it. It may be a whiteout.
    pub fn winner(&self, key: &str) -> Option<&RemoteEntry> {
        self.winners.iter().find_map(|w| w.get(key))
    }

    /// The winner for `key` if it is not a whiteout.
    pub fn visible(&self, key: &str) -> Option<&RemoteEntry> {
        self.winner(key).filter(|e| e.is_visible())
    }

    /// Whether any scope below the run's own has an entry (of any kind) for `key`.
    pub fn in_lower_scope(&self, key: &str) -> bool {
        self.winners.iter().skip(1).any(|w| w.contains_key(key))
    }

    /// Every key some scope has an entry for.
    pub fn keys(&self) -> BTreeSet<String> {
        self.winners
            .iter()
            .flat_map(|w| w.keys().cloned())
            .collect()
    }

    /// Whether some visible entry has a key starting with `prefix`.
    pub fn any_visible_with_prefix(&self, prefix: &str) -> bool {
        let keys: BTreeSet<&String> = self
            .winners
            .iter()
            .flat_map(|w| {
                w.range(prefix.to_string()..)
                    .take_while(|(k, _)| k.starts_with(prefix))
            })
            .map(|(k, _)| k)
            .collect();
        keys.into_iter().any(|k| self.visible(k).is_some())
    }

    /// What the view depends on although no read ever uses it, last used
    /// before `cutoff`, the stalest first: directory markers, empty files,
    /// and whiteouts that hide something.
    pub fn unused_dependencies(&self, cutoff: SystemTime, max: usize) -> Vec<RemoteEntry> {
        // Per scope, keys with older entries that a whiteout of that scope hides.
        let hidden: Vec<HashSet<&str>> = self
            .superseded
            .iter()
            .map(|s| {
                s.iter()
                    .filter(|e| e.is_visible())
                    .map(|e| e.key.as_str())
                    .collect()
            })
            .collect();
        let mut out = Vec::new();
        for (scope, winners) in self.winners.iter().enumerate() {
            for (key, e) in winners {
                let shadowed = self.winners[..scope].iter().any(|w| w.contains_key(key));
                if e.accessed >= cutoff || shadowed {
                    continue;
                }
                let depended_on = match e.meta.kind {
                    Kind::Dir => true,
                    Kind::File => e.meta.empty,
                    Kind::Symlink => false,
                    Kind::Whiteout => {
                        hidden[scope].contains(key.as_str())
                            || self.winners[scope + 1..]
                                .iter()
                                .any(|w| w.get(key).is_some_and(RemoteEntry::is_visible))
                    }
                };
                if depended_on {
                    out.push(e);
                }
            }
        }
        out.sort_by_key(|e| e.accessed);
        out.into_iter().take(max).cloned().collect()
    }

    /// Superseded entries of the run's own scope, for garbage collection.
    pub fn take_superseded_own(&mut self) -> Vec<RemoteEntry> {
        std::mem::take(&mut self.superseded[0])
    }

    /// Visible entries' total size, for `statfs`.
    pub fn visible_bytes(&self) -> u64 {
        self.keys()
            .iter()
            .filter_map(|k| self.visible(k))
            .map(|e| e.size)
            .sum()
    }

    pub fn watermarks(&self) -> Vec<Option<SystemTime>> {
        self.watermark.clone()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Kind;

    fn meta(kind: Kind) -> Meta {
        Meta::new(kind, 0o644, SystemTime::UNIX_EPOCH)
    }

    fn entry(key: &str, id: i64, scope: usize, secs: u64, kind: Kind) -> RemoteEntry {
        let meta = meta(kind);
        RemoteEntry {
            key: key.into(),
            version: meta.encode(),
            meta,
            size: 1,
            created: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            accessed: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            id,
            scope,
        }
    }

    fn index() -> Index {
        Index::new(vec!["refs/heads/feature".into(), "refs/heads/main".into()])
    }

    #[test]
    fn newest_entry_in_a_scope_wins() {
        let mut ix = index();
        assert!(ix.insert(entry("k", 1, 0, 10, Kind::File)));
        assert!(ix.insert(entry("k", 2, 0, 20, Kind::File)));
        assert!(!ix.insert(entry("k", 3, 0, 15, Kind::File)));
        assert_eq!(ix.winner("k").unwrap().id, 2);
        assert!(
            !ix.insert(entry("k", 2, 0, 20, Kind::File)),
            "duplicates are ignored"
        );
        let mut sup: Vec<i64> = ix.take_superseded_own().iter().map(|e| e.id).collect();
        sup.sort();
        assert_eq!(sup, [1, 3]);
    }

    #[test]
    fn own_scope_shadows_lower_scopes_regardless_of_age() {
        let mut ix = index();
        ix.insert(entry("k", 1, 0, 10, Kind::File));
        ix.insert(entry("k", 2, 1, 99, Kind::File));
        assert_eq!(ix.winner("k").unwrap().id, 1);
        assert!(ix.in_lower_scope("k"));
    }

    #[test]
    fn whiteouts_hide_lower_scopes() {
        let mut ix = index();
        ix.insert(entry("k", 1, 1, 10, Kind::File));
        assert!(ix.visible("k").is_some());
        ix.insert(entry("k", 2, 0, 20, Kind::Whiteout));
        assert!(ix.visible("k").is_none());
        assert!(ix.winner("k").is_some());
    }

    #[test]
    fn removing_the_winner_promotes_the_next_newest() {
        let mut ix = index();
        ix.insert(entry("k", 1, 0, 10, Kind::File));
        ix.insert(entry("k", 2, 0, 30, Kind::File));
        ix.insert(entry("k", 3, 0, 20, Kind::File));
        assert!(ix.remove("k", 2));
        assert_eq!(ix.winner("k").unwrap().id, 3);
        assert!(!ix.remove("k", 99));
    }

    #[test]
    fn what_nothing_reads_but_the_view_needs() {
        let mut ix = index();
        // A whiteout hiding the default branch's file, and one hiding nothing.
        ix.insert(entry("p/hidden", 1, 1, 10, Kind::File));
        ix.insert(entry("p/hidden", 2, 0, 20, Kind::Whiteout));
        ix.insert(entry("p/nothing", 3, 0, 20, Kind::Whiteout));
        // A whiteout hiding an older version in its own scope.
        ix.insert(entry("p/rewritten", 4, 0, 10, Kind::File));
        ix.insert(entry("p/rewritten", 5, 0, 20, Kind::Whiteout));
        ix.insert(entry("p/dir/", 6, 0, 20, Kind::Dir));
        let mut empty = entry("p/empty", 7, 0, 20, Kind::File);
        empty.meta.empty = true;
        ix.insert(empty);
        ix.insert(entry("p/file", 8, 0, 20, Kind::File));
        ix.insert(entry("p/fresh/", 9, 0, 1000, Kind::Dir));
        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
        let mut ids: Vec<i64> = ix
            .unused_dependencies(cutoff, 10)
            .iter()
            .map(|e| e.id)
            .collect();
        ids.sort();
        assert_eq!(ids, [2, 5, 6, 7]);
        assert_eq!(ix.unused_dependencies(cutoff, 2).len(), 2);
    }

    #[test]
    fn prefix_visibility() {
        let mut ix = index();
        ix.insert(entry("p/a/x", 1, 1, 10, Kind::File));
        ix.insert(entry("p/b/y", 2, 1, 10, Kind::File));
        ix.insert(entry("p/b/y", 3, 0, 20, Kind::Whiteout));
        assert!(ix.any_visible_with_prefix("p/a/"));
        assert!(!ix.any_visible_with_prefix("p/b/"));
        assert!(!ix.any_visible_with_prefix("p/c/"));
    }

    #[test]
    fn foreign_items_are_ignored() {
        let ix = index();
        let item = CacheItem {
            id: 1,
            git_ref: "refs/heads/main".into(),
            key: "Linux-cargo-abc".into(),
            version: "a".repeat(64),
            size_in_bytes: 5,
            created_at: "2026-01-01T00:00:00Z".into(),
            last_accessed_at: String::new(),
        };
        assert!(ix.entry_from_item(&item).is_none());
        let ours = CacheItem {
            version: meta(Kind::File).encode(),
            ..item.clone()
        };
        assert_eq!(ix.entry_from_item(&ours).unwrap().scope, 1);
        let other_ref = CacheItem {
            git_ref: "refs/heads/elsewhere".into(),
            ..ours
        };
        assert!(ix.entry_from_item(&other_ref).is_none());
    }
}
