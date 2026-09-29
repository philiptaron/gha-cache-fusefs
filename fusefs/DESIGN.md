# gha-cache-fusefs: design

`gha-cache-fusefs` mounts the GitHub Actions cache as a POSIX-ish filesystem
using FUSE. Files written into the mount become cache entries. Any later job
that can read the cache — the same branch, or a branch whose runs may read the
default branch's caches — can mount it and read those files back. Reads are
lazy (byte ranges are fetched on demand), and writes are uploaded in the
background.

This document records what the cache service actually does (measured, not
assumed), the mapping from filesystem to cache entries, and the machinery that
makes the mapping behave like a filesystem.

## 1. The service, as measured

The facts below were measured on `ubuntu-24.04` hosted runners in September
2026 by [`probe/probe.sh`](probe/probe.sh) (re-run it with the `fusefs-probe`
workflow) and by the end-to-end jobs. They constrain every later decision.

**RPC surface.** The runner token (`ACTIONS_RUNTIME_TOKEN`) talks Twirp/JSON to
`$ACTIONS_RESULTS_URL/twirp/github.actions.results.api.v1.CacheService/<Method>`.
Only three methods exist: `CreateCacheEntry`, `FinalizeCacheEntryUpload`, and
`GetCacheEntryDownloadURL`. Anything else (`ListCacheEntries`,
`DeleteCacheEntry`, …) is `404 bad_route`. **Listing and deletion are only
available through the REST API** (`/repos/{owner}/{repo}/actions/caches`),
which needs a `GITHUB_TOKEN` with `actions: read` (listing) or `actions: write`
(deletion).

**Keys** are 1–512 characters. Commas, spaces, `%`, `#`, `?`, backslashes,
quotes, newlines, tabs, non-ASCII, `//`, `./..`, and trailing `/` all
round-trip unchanged. Keys are case-sensitive.

**Versions** must be exactly 64 characters (a 29-character string is rejected
with "must be between 1 and 64 characters"). Any 64-hex-digit string is
accepted; the service treats it as opaque. Entries are identified by
*(key, version, scope)*.

**Lookup** (`GetCacheEntryDownloadURL`) tries an exact key match first, then a
*prefix* match on the primary key, then prefix matches on `restore_keys`. The
most recently created match wins. For example, looking up `a/hel` returns
`a/hello` and `a/` returns the newest entry under `a/`. A miss is
`200 {"ok": false}`. Callers doing exact lookups must compare `matched_key`.

**Creation** reserves *(key, version)* and returns an Azure Blob SAS URL
(`sp=cw`, valid for 1 hour). Reserving the same pair again returns
`409 already_exists`. That includes reservations that were never finalized, so
**an abandoned reservation burns its (key, version)**. `Put Blob`, `Put Block`,
and `Put Block List` all work against the SAS URL, with or without
`x-ms-version`.

**Finalization** checks `size_bytes` against the uploaded blob (a mismatch is
`404 not_found`). **Size 0 is rejected** (`400`: size must be ≥ 1), so an empty
file cannot be stored as an empty blob.

**Download URLs** are read-only SAS URLs (`sp=r`) valid for **10 minutes**.
`HEAD` returns `Content-Length`. `Range` works (`206` + `Content-Range`); a
range past EOF is `416`.

**Latency and throughput.** Every request — Twirp, REST, or blob — costs about
220–320 ms. A single download stream reached ~23 MB/s; eight parallel 16 MiB
ranges reached ~52 MB/s. Eight parallel 16 MiB block uploads moved 128 MiB in
1.6 s.

**Rate limit on creation.** With eight uploads in flight, the service sustained
20–23 new entries per second. After roughly 200 `CreateCacheEntry` calls in
about ten seconds, it answered `429` with a `Retry-After` of 30–40 s. The
budget is per repository: back-to-back jobs share it. Lookups and downloads
(200 in 46 s) were not limited. A workload of many small files is therefore
bounded by roughly 200 new files per half minute, whatever the client does.

**REST listing** returns `id, ref, key, version, size_in_bytes, created_at,
last_accessed_at`. It is immediately consistent and includes only finalized
entries. `key=` filters by prefix, `ref=` by scope, and `sort=created_at`
orders the results. `created_at` is the finalization time; `id` is assigned at
reservation.

**Deletion** by id (`204`) or by key+ref (`200`, echoes the deleted entries)
frees the key for re-creation immediately.

**Runners.** `/dev/fuse` is `0666` and `fusermount3` is setuid-root (fuse3
3.14), so an unprivileged daemon can mount. `/dev/kvm` exists
(`root:kvm 0660`). A standard runner has 4 vCPUs, 16 GB RAM, and ~86 GB free
disk. `ACTIONS_CACHE_MODE` is exported (`write` on `push`; it can be `read`,
`write-only`, or `none`).

## 2. Goals and non-goals

Goals:

* `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `ln -s`, `tar`, `rsync`, and `dd`
  work with unsurprising semantics.
* Reads are lazy and random-access. A squashfs or sqlite image stored in the
  cache can be used in place without downloading all of it first.
* Writes never block on the network. Uploads happen in the background and are
  complete when the filesystem is unmounted; the unmount reports failures.
* Branch isolation matches the cache's own rules. A branch sees its own writes
  layered over the default branch's entries and can never modify the default
  branch's entries.
* The unprivileged runner user can mount, with no root and no extra packages.
* Builds and tests go through Nix.

Non-goals:

* Byte-granular in-place updates of large files. Every change to a file is a
  full re-upload, because entries are immutable blobs.
* Strong consistency between concurrently mounted jobs. Each mount sees a
  snapshot that is refreshed periodically, and the last writer wins.
* Hard links, device nodes, sockets, FIFOs, extended attributes, and ownership.

## 3. Mapping the filesystem onto cache entries

### 3.1 Keys

A mount is rooted at a key prefix `P` (default `fusefs/`; always normalized to
end in `/`). Paths map to keys like this:

| Filesystem object     | Cache key          |
|-----------------------|--------------------|
| file or symlink `a/b` | `P` + `a/b`        |
| directory `a/b`       | `P` + `a/b/`       |

Directories are *implicit*: `a/` exists because `P/a/b` exists. An explicit
directory entry (a "marker") is only written for directories that would
otherwise vanish, such as empty ones. Keys over 512 characters give
`ENAMETOOLONG`. Names must be valid UTF-8; listed keys with empty, `.` or `..`
components are ignored.

### 3.2 Versions carry metadata

The version field is the only per-entry metadata the service stores and the
REST listing returns. Since any 64-hex string is accepted, the version encodes
the inode:

```
bytes  field
0..8   magic   = sha256("gha-cache-fusefs/v1")[0..8]
8      kind    1 = file, 2 = directory marker, 3 = symlink, 4 = whiteout
9      flags   bit 0 = EMPTY (blob is a 1-byte placeholder; logical size 0)
10..12 mode    permission bits, big-endian u16
12..16 reserved (zero)
16..24 mtime   nanoseconds since the Unix epoch, big-endian i64
24..32 nonce   random; makes every write a distinct (key, version)
```

This has several consequences:

* `stat` never touches the network. Kind, mode, mtime, and size all come from
  the listing.
* **Empty files** are stored as a 1-byte placeholder blob with `EMPTY` set.
* **Symlinks** are entries whose blob is the link target.
* **Overwriting** a file never conflicts. The new write has a fresh nonce, so
  it is a new entry, and the newest one wins (§3.3).
* **Deleting** needs no extra permissions. It writes a *whiteout* (kind 4),
  as overlayfs does.
* Entries from other tools (for example `actions/cache` tarballs) lack the
  magic and are ignored.
* An abandoned reservation only burns a nonce nobody will reuse.

### 3.3 Scopes form an overlay

A run can read the caches of several scopes: its own ref (`GITHUB_REF`, e.g.
`refs/pull/7/merge`), the pull request's base branch, and the default branch.
It can only write its own ref. The mount lists each readable scope via REST
and resolves every key with two rules:

1. **Scope precedence:** current ref > PR base > default branch. The upper
   layer shadows the lower ones, whatever the timestamps.
2. **Within a scope, the newest entry wins**, ordered by
   `(created_at, id)`.

If the winner is a whiteout, the key does not exist. Deleting on a feature
branch hides the default branch's file on that branch only, which is how
overlayfs behaves and how cache scoping already works. With `--gc` (and a
token with `actions: write`), entries in the current scope that a newer write
superseded are deleted in the background. It is off by default: another job
may still be reading an old version, and unread garbage is what the cache's
LRU eviction removes first anyway.

A file entry and a directory can collide, for example when two jobs write
`a` and `a/b` concurrently. The directory wins and the file is hidden.

## 4. Architecture

```
          kernel FUSE ──► fuser session thread ──► tokio tasks
                                                     │
                            ┌────────────── Vfs core (namespace, handles) ─────────────┐
                            │  tree of inodes = remote view ⊕ local overlay            │
                            └──────┬───────────────────┬───────────────────┬───────────┘
                                   │                   │                   │
                         Index (REST listing,   DataStore (local       Committer (settle,
                          scopes, refresh)       sparse files)          upload, whiteout)
                                   │                   │                   │
                          GitHub REST API     Azure Blob (SAS, Range)   Twirp CacheService
```

* **`api`** holds three small clients on top of one `reqwest` pool: Twirp,
  Blob, and REST. They share retry and backoff logic, and a rate-limit gate
  that honors `Retry-After`. SAS signatures are redacted from all logs.
* **`index`** keeps the per-scope listings and computes the merged *remote
  view*. The initial listing pages are fetched in parallel. A refresh is
  incremental: it reads pages sorted by `created_at desc` only until it
  passes the previous high-water mark (minus a margin for clock skew).
* **`vfs`** is the FUSE-agnostic core. It owns the inode tree, the **local
  overlay** (operations not yet committed; one op per key, latest wins), open
  handles, and directory snapshots. Every operation returns
  `Result<_, errno>`, and the tests drive it without a kernel.
* **`data`** manages local backing files. For content we wrote, the backing
  file is authoritative. For remote content, it is a sparse cache with a 1 MiB
  presence bitmap. Clean files are evicted LRU above `--cache-size-mb`.
* **`commit`** runs the upload pipeline.
* **`fuse`** is a thin `fuser::Filesystem` adapter. Requests that stay in
  memory or on local disk (`getattr`, `readdir`, `write`, `create`, …) are
  answered on the FUSE thread; any that may touch the network (`lookup`,
  `open`, `read`, `setattr`, `rename`, `fsync`) are answered from a tokio
  task, so a slow download never stalls the event loop.

## 5. Write path

1. `create`/`mknod` allocates an inode backed by a fresh local file and
   records `Put(ino)` in the overlay. Writes may land at any offset, and
   truncation and seeking work, because the data is an ordinary local file.
2. When the last writable handle is released, the key is scheduled for commit
   after a **settle delay** (default 1 s). Anything that touches the inode in
   that window (reopen, rename, chmod, utimens, unlink) is simply folded in.
   This is what makes the ubiquitous *write-temp-then-rename* pattern (Nix,
   Bazel, Cargo, editors) upload only the final name.
3. **Commit** snapshots the inode's metadata and a content generation counter,
   encodes a version, then:
   * calls `CreateCacheEntry`,
   * uploads with `Put Blob` (≤ 16 MiB) or with 8 MiB `Put Block`s, four in
     flight per file, followed by `Put Block List`,
   * re-checks the generation (a write during the upload aborts this
     attempt; the burned nonce is harmless),
   * calls `FinalizeCacheEntryUpload`.
   On success the overlay op retires and the backing file becomes clean cache.
4. **Retries.** `5xx`, network errors, and `429` back off and retry
   indefinitely while mounted; `429` pauses all traffic until `Retry-After`.
   An ambiguous `Create`/`Finalize` restarts with a new nonce. Permanent
   errors are recorded and reported at unmount.
5. `fsync` commits the file *now* and waits, even if it is still open for
   writing; a write that races with the upload makes the attempt start over.
   It is the explicit durability point.
6. **Unmount drains.** Every pending op becomes due immediately, markers are
   written for empty directories, and whiteouts are written last. Whiteouts
   produced by `rename` wait for the corresponding `Put` so that a failed
   upload cannot lose data. The daemon exits non-zero if anything failed.

Opening a *remote* file for writing is copy-on-write. `O_TRUNC` starts empty;
otherwise the whole file is fetched first. The same applies to `truncate`,
`chmod`, and `utimens` on a remote file: the metadata lives in the version, so
changing it means writing a new entry.

## 6. Read path

* `open` resolves a download URL (`GetCacheEntryDownloadURL` with the exact
  key and version). URLs are cached until one minute before their `se=`
  expiry; a `403` re-resolves once.
* `read` maps the byte range onto 1 MiB chunks. Missing chunks are coalesced
  into range GETs of up to 8 MiB and fetched concurrently (at most 16 in
  flight globally). Concurrent readers share in-flight fetches. The bytes land
  in the sparse backing file and are served with `pread`.
* **Readahead** is tracked per handle. A sequential reader doubles its window
  from 2 MiB up to 64 MiB and prefetches that far ahead, which turns the
  ~250 ms first-byte latency into streaming throughput. Random readers only
  fetch what they touch.
* **Sibling prefetch.** The first read of a small remote file (≤ 1 MiB) starts
  background fetches of the other small files in its directory, six at a
  time. `cp -r`, `diff -r`, and `tar c` over a tree of small files then read
  mostly local data instead of paying two round trips (URL, then bytes) per
  file.
* `FOPEN_KEEP_CACHE` is set only if the inode's content has not changed since
  the previous open, so the kernel page cache stays warm for immutable data
  without ever serving stale bytes.
* An open handle pins the content version it opened (snapshot-per-open). A
  refresh that brings in a newer version affects later opens only.

## 7. Namespace operations

| Operation | Behavior |
|-----------|----------|
| `lookup`, `getattr`, `readdir(plus)` | In memory. A miss triggers at most one incremental refresh per `--refresh` interval (default 15 s), so polling for a file another job is writing works. |
| `readdir` | Served from a snapshot taken at `opendir`/rewind, so `rm -r` does not skip entries. |
| `mkdir` / `rmdir` | Local directory. A marker is written at unmount only if it is still empty; `rmdir` whites out an existing marker. `ENOTEMPTY` as usual. |
| `unlink` | Drops a pending upload. If a remote entry is visible, whites it out. Open handles keep working (unlinked-but-open). |
| `rename` | Free if the source's content is local (pending, or cached in full): the target gets a `Put`, the source a whiteout that waits for it. Directories rename if their subtree is purely local. Otherwise `EXDEV`, so `mv` falls back to copy + unlink. `RENAME_NOREPLACE` is honored; `RENAME_EXCHANGE` is `EINVAL`. |
| `symlink` / `readlink` | Supported; the target is the blob, cached after the first read. |
| `chmod`, `utimens` | Stored in the version (copy-on-write for remote files). `chown` is accepted and ignored; ownership is always the mounting user. |
| `link`, `mknod` (non-regular) | `EPERM`. |
| xattrs | `ENOSYS`, so the kernel stops asking (and skips its per-write `security.capability` check); programs see `EOPNOTSUPP`. |
| `statfs` | Capacity is the repository cache quota; "used" is the sum of listed entries. |

## 8. Deployment

The binary is `gha-cache-fusefs`. It reads `ACTIONS_RESULTS_URL`,
`ACTIONS_RUNTIME_TOKEN`, `ACTIONS_CACHE_MODE`, `GITHUB_TOKEN`,
`GITHUB_REPOSITORY`, `GITHUB_REF`, `GITHUB_BASE_REF`, `GITHUB_API_URL`, and
`GITHUB_EVENT_PATH` (for the default branch).

* `mount <dir> [--daemon]` lists the cache, mounts, and runs. With `--daemon`
  it forks *before* starting any threads, and the parent exits `0` only once
  the mount is live, so `gha-cache-fusefs mount … --daemon && ls <dir>` is
  race-free. State (log, lock, `summary.json`, cached data) lives under
  `--state-dir`, which defaults to a directory derived from the mountpoint.
* `unmount <dir>` sends `SIGTERM`. The daemon unmounts (lazily if the mount is
  busy), drains uploads, and writes `summary.json`. `unmount` blocks on the
  daemon's lock file until then, prints the summary, and exits non-zero if
  anything was lost.
* `ACTIONS_CACHE_MODE=read` mounts read-only; `none` refuses to mount.
* Runner tokens are only visible to actions, not to `run:` steps. The
  `mount/` action (a dependency-free `node24` action) passes them to the
  daemon in `main` and unmounts in `post`, which also writes a job summary. It finds the binary in the
  `binary` input, the release for its ref (published by `fusefs-release`),
  or builds it from its own checkout with Nix or with the runner's cargo:

  ```yaml
  permissions:
    actions: write   # read suffices; write enables garbage collection
    contents: read
  steps:
    - uses: philiptaron/gha-cache-fusefs/mount@main
      with:
        path: /mnt/cache
    - run: cp -r build/ /mnt/cache/build-${{ github.sha }}
  ```

## 9. Testing

* **Unit tests** cover the version codec, key/path mapping, scope merging,
  error classification, and the parsing of listings and SAS expiries.
* **A fake cache service** (`gha-cache-fusefs fake-server`) implements the
  three Twirp methods, SAS-style blob endpoints (Put Blob/Block/BlockList,
  ranged GET/HEAD, expiring URLs, injectable 503s), and the REST list/delete
  endpoints, with the probe's observed semantics: prefix fallback, 409s,
  size validation, size ≥ 1, 64-character versions, and scopes encoded in the
  fake token. It lets the whole stack run offline.
* **Integration tests** drive the `Vfs` core against the fake service, one
  "job" after another. They cover persistence, overwrite, copy-on-write,
  whiteouts across branch scopes, the three kinds of rename (pending, cached,
  `EXDEV`), directory markers, symlinks, empty files, metadata, block
  uploads, ranged reads, injected failures, `fsync`, refresh on lookup miss,
  `readdir` snapshots, unlinked-but-open files, and sibling prefetch.
* **[`tests/e2e.sh`](tests/e2e.sh)** runs real tools through the kernel in
  three phases, each a fresh mount: write (coreutils, `cp -a`, `tar -x`,
  `rsync`, `dd`, `mksquashfs`, exec, write-then-rename), read and modify
  (checksums, random reads, a squashfs image mounted with `squashfuse`
  straight out of the cache, `mv`/`chmod`/append on remote files, `rm -r`),
  and verify and remove. CI runs it three ways: in a NixOS VM against the
  fake service (`nix flake check`), on a runner as the unprivileged user
  against the fake service, and across three dependent jobs against the real
  cache. A final job deletes what the run created.

## 10. Future work

* Packing small files into shared entries, to get past the ~200 creations
  per half minute that the service allows.
* Lazy copy-on-write: opening a remote file read-write downloads it at once,
  even if nothing is written.
* A read-only view of foreign entries (for example `actions/cache` archives).
* Kernel passthrough (`FOPEN_PASSTHROUGH`) for fully-local files when running
  as root.
* A control socket (`gha-cache-fusefs sync`) for mid-job checkpoints across
  the whole mount.
* Using the mount as a Nix binary cache for this repository's own CI, whose
  builds currently compile every crate from scratch.
