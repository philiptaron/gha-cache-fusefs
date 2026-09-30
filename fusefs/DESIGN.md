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
bounded by roughly 200 new files per 40–50 s (ten seconds of creating, then
the wait), whatever the client does.

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

[LAYERS.md](LAYERS.md) specifies the format, format 2. This section is its
outline. Format 1 stored one entry per file, with the path as the key and
the inode in the version. It needed a creation for every file, symlink,
empty directory, and deletion, which the rate limit turned into minutes
(PERFORMANCE.md). Mounts ignore its entries, which expire within a week.

### 3.1 Volumes, layers, and blobs

A mount shows a **volume** (default `default`), whose entries are the keys
under `gha-fs/<volume>/`:

| entry | key | content |
|---|---|---|
| layer | `gha-fs/<volume>/layer/<nonce>` | an EROFS image of one batch of changes |
| blob | `gha-fs/<volume>/blob/<sha256>` | the bytes of one file over 8 MiB |

Keys hold no paths, so paths have no length limit beyond the 255-byte names
EROFS takes. `--root` shows a directory of the volume instead of all of it.
Names must be valid UTF-8.

A **layer** holds files (up to 1 KiB inline in its metadata, up to 8 MiB in
whole blocks after it), symlinks, whiteouts, and directories. Its first M
blocks, the metadata, describe its whole tree, so one ranged GET lists it.
Directories are implicit, as in overlayfs, unless a layer marks them with
the xattr `user.gha-fs.dir`: `keep` after `mkdir` (the directory exists
even when empty), `attrs` after `chmod` or `utimens`, and `drop` after
`rmdir`. Files over 8 MiB live in **blobs**, which layers refer to through
EROFS's device table, by digest; a blob a readable scope already has is not
uploaded again.

### 3.2 Versions say what an entry is

The version field is the only per-entry metadata the service stores and the
REST listing returns. It says which kind of entry this is and, for a layer,
how long its metadata is:

```
bytes  field
0..8   magic  = sha256("gha-cache-fusefs/v2")[0..8]
8      kind   1 = layer, 2 = blob
12..16 layers: metadata size M in 4 KiB blocks (big-endian u32)
24..32 nonce  random; makes every upload a distinct (key, version)
```

The other bytes are zero. Entries from other tools (for example
`actions/cache` tarballs) lack the magic and are ignored, even among the
volume's keys, and an abandoned reservation only burns a nonce nobody will
reuse. Inode metadata (mode, mtime with nanoseconds, size) lives in the
layers' 64-byte EROFS inodes, so once a mount has read the layers' metadata,
`stat` never touches the network.

### 3.3 Scopes and layers form an overlay

A run can read the caches of several scopes: its own ref (`GITHUB_REF`, e.g.
`refs/pull/7/merge`), the pull request's base branch, and the default branch.
It can only write its own ref. The mount lists `gha-fs/<volume>/` in each
readable scope and stacks the layers, oldest at the bottom:

1. **by scope:** default branch, then PR base, then the current ref, so a
   nearer scope shadows farther ones whatever the timestamps;
2. **within a scope, by `(created_at, id)`.**

Applying the layers bottom up gives the tree. A file or symlink replaces
whatever is at its path, and a directory merges with a directory. A whiteout
removes a file or symlink, but leaves a directory alone: whiteouts are only
written for what the deleting job saw, so a concurrent job's new files in
the same directory survive. Deleting on a feature branch hides the default
branch's file on that branch only, which is how overlayfs behaves and how
cache scoping already works.

The service evicts an entry a week after its last *download*. A mount reads
the metadata of each scope's newest snapshot and of the layers it does not
cover (LAYERS.md §8), which keeps them alive while the volume is in use.
Blobs, and covered layers, are downloaded only when read, so a mount
touches (resolves a download URL for) those that visible files refer to
and that were last used more than three days ago, at most 1,000 per mount,
the stalest first. At unmount, a mount that would cover 16 layers of its
own scope writes a snapshot of them: a layer of metadata only, which refers
to their data where it is. Covered layers that nothing refers to then
expire.

## 4. Architecture

```
          kernel FUSE ──► fuser session thread ──► tokio tasks
                                                     │
                            ┌────────────── Vfs core (namespace, handles) ─────────────┐
                            │  tree of inodes = remote view ⊕ local overlay            │
                            └──────┬───────────────────┬───────────────────┬───────────┘
                                   │                   │                   │
                         Index (listing, layer  DataStore (local       Committer (settle,
                          metadata, stacking)    sparse files)          batch, layer)
                                   │                   │                   │
                          GitHub REST API     Azure Blob (SAS, Range)   Twirp CacheService
```

* **`api`** holds three small clients on top of one `reqwest` pool: Twirp,
  Blob, and REST. They share retry and backoff logic. Each service, and each
  Twirp method, has a rate-limit gate of its own that honors `Retry-After`,
  so throttled creations never hold up downloads. REST requests fail rather
  than wait more than a minute, since an exhausted `GITHUB_TOKEN` budget can
  take an hour to reset. SAS signatures are redacted from all logs.
* **`index`** lists the volume in every readable scope, reads the metadata
  of each layer, and stacks the layers into the *remote view*: every path
  that exists, with its node. The initial listing pages are fetched in
  parallel. Pages are cut by position, so an entry deleted while we page
  would hide another at a page boundary. Every page reports the total, and a
  listing whose pages disagree is repeated. A refresh is incremental: it
  reads pages sorted by `created_at desc` only until it passes the previous
  high-water mark (minus a margin for clock skew). Any change of the layers
  restacks them all and reports the paths whose nodes changed.
* **`erofs`** writes and reads the EROFS subset of LAYERS.md §3.
* **`vfs`** is the FUSE-agnostic core. It owns the inode tree, the **local
  overlay** (operations not yet committed; one op per path, latest wins),
  open handles, and directory snapshots. Every operation returns
  `Result<_, errno>`, and the tests drive it without a kernel.
* **`data`** manages local backing files. For content we wrote, the backing
  file is authoritative. For remote content, there is one sparse cache per
  layer or blob, with a 1 MiB presence bitmap, and a file is a range of one.
  Caches are evicted LRU above `--cache-size-mb`.
* **`commit`** runs the upload pipeline. It takes operations from a queue
  ordered by due time, so its cost follows what is due, not what is pending,
  and turns each batch of them into a layer.
* **`fuse`** is a thin `fuser::Filesystem` adapter. Requests that stay in
  memory or on local disk (`getattr`, `readdir`, `write`, `create`, …) are
  answered on the FUSE thread; any that may touch the network (`lookup`,
  `open`, `read`, `setattr`, `rename`, `fsync`) are answered from a tokio
  task, so a slow download never stalls the event loop.

## 5. Write path

1. `create`/`mknod` allocates an inode backed by a fresh local file and
   records `Put(ino)` in the overlay. Writes may land at any offset, and
   truncation and seeking work, because the data is an ordinary local file.
   `mkdir`, `chmod` of a directory, and `symlink` record a `Put` too;
   `unlink` and `rmdir` record a `Remove`, if the view has something there
   to hide: a whiteout for a file or symlink, a `drop` for a kept directory,
   whichever the view shows when it commits.
2. When the last writable handle is released, the path is scheduled for
   commit after a **settle delay** (default 1 s). Anything that touches the
   inode in that window (reopen, rename, chmod, utimens, unlink) is simply
   folded in. This is what makes the ubiquitous *write-temp-then-rename*
   pattern (Nix, Bazel, Cargo, editors) upload only the final name.
3. **Commit** takes every due operation, up to 10,000 paths or 256 MiB, as
   one batch, and makes it one layer (LAYERS.md §5):
   * files over 8 MiB are hashed and uploaded as blobs, in parallel, unless
     a readable scope has the blob already; a blob is finalized only if its
     file did not change meanwhile;
   * the layer image is sealed into a local file; a file that changed while
     it was copied waits for the next batch, and the image is built again
     without it;
   * the layer is uploaded: `CreateCacheEntry`, `Put Blob` (≤ 16 MiB) or
     8 MiB `Put Block`s four at a time and `Put Block List`, then
     `FinalizeCacheEntryUpload`.
   On success the batch's overlay ops retire, the sealed file becomes the
   cache of the layer, and a committed large file's local copy becomes the
   cache of its blob. Layers upload one at a time, so they stack in the
   order they were sealed; what becomes due meanwhile makes the next batch.
4. **Retries.** `5xx`, network errors, and `429` back off and retry
   indefinitely while mounted; a `429` pauses further creations (but not
   downloads) until `Retry-After`. An ambiguous `Create`/`Finalize` restarts
   with a new nonce. Permanent errors are recorded and reported at unmount.
5. `fsync` returns at once by default (`--fsync local`): the file is on
   local disk, and it uploads with the next batch, at the latest when the
   job unmounts, which is the only durability that outlives a CI job. With
   `--fsync commit`, it commits the file *now* and waits, even if the file
   is still open for writing. That makes a layer of its own and costs three
   round trips per call; a program that syncs every file, such as SQLite
   after each transaction, re-uploads the file each time and meets the
   rate limit.
6. **Unmount drains.** Every pending op becomes due immediately, and empty
   directories that exist only locally get a `keep` mark. A rename's
   whiteout goes in the layer of its new name, or a later one, so that a
   failed upload cannot lose data. Then, if it would cover 16 layers, the
   mount writes a snapshot of its scope (LAYERS.md §8); failing to is not a
   failure. The daemon exits non-zero if anything failed.

Opening a *remote* file for writing is copy-on-write. `O_TRUNC` starts empty;
otherwise the whole file is fetched first. The same applies to `truncate` of
a remote file: a layer holds whole files, so changing one means writing it
again. Renaming a remote file, or changing its mode or mtime, needs no data:
the next layer commits it again, referring to its data where it is
(LAYERS.md §3).

## 6. Read path

* **Mounting** lists the volume and reads each layer's metadata with one
  download URL and one ranged GET, sixteen layers at a time. Files of up to
  1 KiB and symlink targets arrive with it.
* `read` of other files maps the byte range onto 1 MiB chunks of the layer
  or blob that holds them. Missing chunks are coalesced into range GETs of
  up to 8 MiB and fetched concurrently (at most 16 in flight globally).
  Concurrent readers share in-flight fetches. The bytes land in the entry's
  sparse backing file and are served with `pread`. Download URLs are cached
  until one minute before their `se=` expiry; a layer's first data read
  reuses the URL its metadata was read with, and a `403` re-resolves once.
* **Readahead** is tracked per handle. A sequential reader doubles its window
  from 2 MiB up to 64 MiB and prefetches that far ahead, which turns the
  ~250 ms first-byte latency into streaming throughput. The chunk the reader
  needs goes first, in a request of its own. The window is then topped up in
  whole 8 MiB ranges, since sliding it one read at a time would fetch a chunk
  per request. Random readers only fetch what they touch.
* **Sibling prefetch.** The first read of a small remote file (≤ 1 MiB)
  fetches the other small files of its directory. A layer holds them in path
  order, so per layer they are one range, fetched in 8 MiB requests if it is
  at most 32 MiB long, and file by file, six at a time, otherwise. `cp -r`,
  `diff -r`, and `tar c` over a tree of small files then read local data.
* `FOPEN_KEEP_CACHE` is set only if the inode's content has not changed since
  the previous open, so the kernel page cache stays warm for immutable data
  without ever serving stale bytes.
* An open handle pins the content version it opened (snapshot-per-open). A
  refresh that brings in a newer version affects later opens only.

## 7. Namespace operations

| Operation | Behavior |
|-----------|----------|
| `lookup`, `getattr`, `readdir(plus)` | In memory. A miss triggers at most one incremental refresh per `--refresh` interval (default 15 s), so polling for a file another job is writing works. The refresh runs in the background, and a lookup waits for it at most 2 s. |
| `readdir` | Served from a snapshot taken at `opendir`/rewind, so `rm -r` does not skip entries. |
| `mkdir` / `rmdir` | `mkdir` commits a `keep` mark, so the directory exists even when empty. `rmdir` commits a `drop` if a layer keeps the directory, which then exists only while something below it does. `ENOTEMPTY` as usual. |
| `unlink` | Drops a pending upload. If the view shows a file there, whites it out. Open handles keep working (unlinked-but-open). |
| `rename` | Never needs the network: the target gets a `Put`, the source a whiteout in the same layer or a later one. A remote file's `Put` refers to its data where it is, and inline data is copied. A directory takes everything below it along: a `Put` under the new name for each (pending ones keep their due time), a `keep` mark for each directory, and a whiteout or `drop` for each old name. `RENAME_NOREPLACE` is honored; `RENAME_EXCHANGE` is `EINVAL`. |
| `symlink` / `readlink` | Supported; the target lives in the layer's metadata. |
| `chmod`, `utimens` | Committed: files are written again (a remote file refers to its data), directories get an `attrs` mark, symlinks are written again. `chown` is accepted and ignored; ownership is always the mounting user. |
| `link`, `mknod` (non-regular) | `EPERM`. |
| xattrs | `ENOSYS`, so the kernel stops asking (and skips its per-write `security.capability` check); programs see `EOPNOTSUPP`. |
| `statfs` | Capacity is the repository cache quota; "used" is the size of the visible files. |

## 8. Deployment

The binary is `gha-cache-fusefs`. It reads `ACTIONS_RESULTS_URL`,
`ACTIONS_RUNTIME_TOKEN`, `ACTIONS_CACHE_MODE`, `GITHUB_TOKEN`,
`GITHUB_REPOSITORY`, `GITHUB_REF`, `GITHUB_BASE_REF`, `GITHUB_API_URL`, and
`GITHUB_EVENT_PATH` (for the default branch).

* `mount <dir> [--daemon] [--volume V] [--root R]` lists the cache, mounts,
  and runs. With `--daemon` it forks *before* starting any threads, and the
  parent exits `0` only once the mount is live, so
  `gha-cache-fusefs mount … --daemon && ls <dir>` is race-free. State (log,
  lock, `summary.json`, cached data) lives under `--state-dir`, which
  defaults to a directory derived from the mountpoint.
* `unmount <dir>` sends `SIGTERM`. The daemon unmounts (lazily if the mount is
  busy), drains uploads, and writes `summary.json`. `unmount` blocks on the
  daemon's lock file until then, prints the summary, and exits non-zero if
  anything was lost.
* `ACTIONS_CACHE_MODE=read` mounts read-only; `none` refuses to mount.
* Runner tokens are only visible to actions, not to `run:` steps. The
  `mount/` action (a dependency-free `node24` action) passes them to the
  daemon in `main` and unmounts in `post`, which also writes a job summary.
  It finds the binary in the `binary` input, the release for its ref
  (published by `fusefs-release`), or builds it from its own checkout with
  Nix or with the runner's cargo:

  ```yaml
  permissions:
    actions: read
    contents: read
  steps:
    - uses: philiptaron/gha-cache-fusefs/mount@v0.1.0
      with:
        path: /mnt/cache
    - run: cp -r build/ /mnt/cache/build-${{ github.sha }}
  ```

## 9. Testing

* **Unit tests** cover the version codec, keys and volumes, layer stacking
  (whiteouts, marks, attributes, missing blobs), EROFS writing and reading,
  error classification, and the parsing of listings and SAS expiries.
* **erofs-utils** checks the EROFS code: `fsck.erofs` must accept and
  extract what we write, and we must read what `mkfs.erofs` writes. The
  integration tests also run `fsck.erofs` on layers the filesystem wrote.
* **A fake cache service** (`gha-cache-fusefs fake-server`) implements the
  three Twirp methods, SAS-style blob endpoints (Put Blob/Block/BlockList,
  ranged GET/HEAD, expiring URLs, injectable 503s), and the REST list/delete
  endpoints, with the probe's observed semantics: prefix fallback, 409s,
  size validation, size ≥ 1, 64-character versions, and scopes encoded in the
  fake token. It lets the whole stack run offline. With `--profile hosted` it
  also imitates the latency, bandwidth, and creation rate limit above, which
  the benchmarks in [PERFORMANCE.md](PERFORMANCE.md) rely on.
* **Integration tests** drive the `Vfs` core against the fake service, one
  "job" after another. They cover persistence, overwrite, copy-on-write,
  whiteouts across branch scopes and between concurrent jobs, the kinds of
  rename (pending, remote, inline, symlink, directories), directory marks and
  attributes, symlinks, empty files, metadata, one layer per batch, blobs
  and their deduplication and loss, block uploads, ranged reads, injected
  failures, rate limits, `fsync`, refresh on lookup miss, volumes, mounting
  a directory of the volume, `readdir` snapshots, unlinked-but-open files,
  and sibling prefetch.
* **[`tests/e2e.sh`](tests/e2e.sh)** runs real tools through the kernel in
  three phases, each a fresh mount: write (coreutils, `cp -a`, `tar -x`,
  `rsync`, `dd`, `mksquashfs`, exec, write-then-rename), read and modify
  (checksums, random reads, a squashfs image mounted with `squashfuse`
  straight out of the cache, `mv`/`chmod`/append on remote files, `rm -r`),
  and verify and remove. CI runs it three ways: in a NixOS VM against the
  fake service (`nix flake check`), on a runner as the unprivileged user
  against the fake service, and across three dependent jobs against the real
  cache, in a volume of the run's own. A final job deletes the volume.

## 10. Future work

* Compacting covered layers that are mostly overwritten, so that they can
  expire (LAYERS.md §8). Deleting them instead is not safe, since other
  scopes may refer to their data.
* Skipping unchanged uploads: `cp -a`, `tar -x`, and `rsync` of mostly
  unchanged trees rewrite every file.
* Lazy copy-on-write: opening a remote file read-write downloads it at once,
  even if nothing is written.
* A read-only view of foreign entries (for example `actions/cache` archives).
* Kernel passthrough (`FOPEN_PASSTHROUGH`) for fully-local files when running
  as root.
* A control socket (`gha-cache-fusefs sync`) for mid-job checkpoints across
  the whole mount.
* Using the mount as a Nix binary cache for this repository's own CI, whose
  builds currently compile every crate from scratch.
