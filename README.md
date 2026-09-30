# gha-cache-fusefs

**The GitHub Actions cache, mounted as a filesystem.** Write files in one
job; read them in any later job that can see the cache. Reads are lazy:
only the bytes a program touches come down, so a multi-gigabyte squashfs,
sqlite, or zip works in place.

```yaml
permissions:
  actions: read      # listing the cache needs it
  contents: read

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: philiptaron/gha-cache-fusefs/mount@v0.1.0
        with:
          path: /mnt/cache
      - run: |
          make
          cp -r out/ /mnt/cache/builds/${{ github.sha }}/

  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: philiptaron/gha-cache-fusefs/mount@v0.1.0
        with:
          path: /mnt/cache
          read-only: true
      - run: /mnt/cache/builds/${{ github.sha }}/bin/tests
```

Files upload in the background as they close. The post step waits for the
last one and fails the job if anything didn't make it.

## What works

| | |
|---|---|
| Reading | `cat`, `cp`, `tar`, `mmap`, random access, executing binaries |
| Writing | any pattern, seeks and sparse files included; `cp -a`, `tar -x`, `rsync`, `dd conv=fsync` |
| Metadata | permission bits, nanosecond mtimes, symlinks, empty files, empty directories |
| Namespace | `mkdir`, `rmdir`, `rm -r`, and `mv` of anything, without downloading it |
| Not supported | hard links, FIFOs, device nodes, xattrs, ownership (always the mounting user) |

Renaming a cached file, or changing its mode or mtime, costs nothing.
Changing its *contents* downloads it first: entries are immutable, so every
edit writes the whole file again.

## The cache protocol

The cache service is a narrow thing. A job gets three calls, and the REST
API adds a fourth:

| call | what it does |
|---|---|
| `CreateCacheEntry(key, version)` | reserves an entry; returns a write-only Azure blob URL |
| `FinalizeCacheEntryUpload` | seals it: immutable from here on |
| `GetCacheEntryDownloadURL` | a read-only URL, good for 10 minutes, ranged GETs welcome |
| `GET /repos/…/actions/caches` | lists entries: the only way to see what's there |

No mutation, no runtime listing, no deletion. Creating entries is rate
limited to about 200 per 45 s per repository, and an entry is evicted a
week after its last download. Everything below grows out of those
constraints.

### Rings

A volume grows the way a tree does: in rings. Each batch of changes becomes
one immutable entry, a **layer**, keyed `gha-fs/<volume>/layer/<nonce>`.
It is an EROFS image of the batch's files, symlinks, directories, and
deletions (**whiteouts**). The mount stacks layers like overlayfs; the
outermost ring wins. A thousand small files are one creation, not a
thousand.

The version, 64 hex digits the service stores without looking, is the only
metadata a listing returns. So that's where an entry says what it is:

```
bytes   field
0..8    magic   sha256("gha-cache-fusefs/v2")[0..8]
8       kind    1 layer · 2 blob · 3 snapshot
12..16  M       layer metadata size, in 4 KiB blocks
16..24  T       snapshots: what they cover
24..32  nonce   random
```

A layer's whole tree sits in its first M blocks, so mounting costs one
ranged GET per layer, and after that `ls` and `stat` never touch the
network. Files up to 1 KiB ride inline in the metadata; files up to 8 MiB
follow it, in path order, so a directory of small files is one range.
Larger files are **seeds**: entries of their own at
`gha-fs/<volume>/blob/<sha256>`. They're content addressed, so a file the
cache already has is referred to, never planted twice.

Writes land on local disk. A closed file settles for a second, so
write-then-rename uploads only the final name, and then joins the next
batch: its seeds upload in parallel, the layer is sealed locally, and it
goes up as Create → Put → Finalize. Layers go up one at a time, so they
stack in the order they were sealed. A rename, `chmod`, or `touch` of a
cached file commits a new node that points at the data where it already
grows.

### Grafts

Cache scoping is grafting. The default branch is the rootstock, a pull
request's base is grafted onto it, and the pull request is grafted onto
that. A run draws on every scope beneath it and writes only its own. The
mount stacks them in that order, each scope's layers by
`(created_at, id)`, so a nearer scope shadows a farther one whatever the
clocks say.

`rm` on a branch prunes that branch only: its whiteout hides the default
branch's file there and nowhere else. The rootstock is never cut. Whiteouts
never cover directories, and only hide what the pruning job saw, so a
concurrent job's new files in the same directory survive.

### Heartwood

Rings pile up, and every mount reads every one. So a job that unmounts
after finding 16 rings of its scope since the last snapshot lays down
heartwood: a **snapshot**, one metadata-only layer that stands in for
everything its scope grew before time T and points at the data where it
lives. Mounts read the newest snapshot and the rings after it. Covered
rings go unread, and once no visible file needs their data, they fall like
leaves a week later.

### Water

Reading a layer's metadata is a download, so every mount waters the rings
it reads, and a volume in use stays alive. Seeds, and covered rings that
visible files still need, are watered once they're three days dry.

[LAYERS.md](fusefs/LAYERS.md) is the full specification.
[DESIGN.md](fusefs/DESIGN.md) records what the service does as measured,
and the machinery around it. [PERFORMANCE.md](fusefs/PERFORMANCE.md) says
how fast it all is.

## Next to actions/cache

They share the repository's cache storage and nothing else.

* **Neither sees the other's entries.** `actions/cache` versions lack the
  magic, so the mount skips them even under `gha-fs/`; its own lookups
  include a version the mount never writes.
* **The mount never changes or deletes an entry.** Deletions are
  whiteouts.
* Both draw on the same quota, eviction, and creation rate limit, and follow
  the same branch scoping. CI checks all of this against the real service.

## Limits

* **`actions: read`** is required: listing goes through the REST API.
  Runs with a read-only cache token (some fork PRs) mount read-only.
* **Quota and eviction** are the service's: 10 GB per repository by
  default, and a week without a download. `GHA_CACHE_FUSEFS_SNAPSHOT_AFTER`
  moves the snapshot threshold (0 never snapshots).
* **Creation rate.** A batch is one entry however many files it holds, so
  the ~200 per 45 s limit only bites for many files over 8 MiB, or with
  `fsync: commit` and a program that syncs every file. The daemon waits as
  told (`Retry-After`).
* **REST budget.** `GITHUB_TOKEN` gets 1,000 requests an hour per
  repository; every mount spends one per 100 entries per scope.

## The action

| input | default | |
|---|---|---|
| `path` | *(required)* | where to mount; created if missing |
| `volume` | `default` | which filesystem; volumes share nothing but quota |
| `root` | | mount one directory of the volume, such as `builds/linux` |
| `read-only` | `false` | |
| `token` | `${{ github.token }}` | for listing entries |
| `settle` | `1s` | how long a closed file waits before it's saved |
| `fsync` | `local` | `local` returns at once, and the file rides the next batch; `commit` uploads it and waits, a layer per call |
| `cache-size-mb` | `8192` | local disk budget for downloaded data |
| `binary` | | a prebuilt binary; otherwise one is downloaded from a release, or built with Nix or cargo |
| `release` | | release to download the binary from (`vX.Y.Z`, `main-latest`); defaults to the action's ref |
| `fail-on-error` | `true` | fail the job if changes could not be saved |
| `log` | `info` | daemon log filter |

Each [release](https://github.com/philiptaron/gha-cache-fusefs/releases)
carries static binaries for x86_64 and aarch64 Linux, and the action
downloads the one for its ref: `@v0.1.0` gets v0.1.0's, and `@main` gets
the rolling `main-latest` prerelease's. Nothing is built on the runner.

## Development

```sh
nix develop            # cargo, clippy, rustfmt, rust-analyzer, erofs-utils
(cd fusefs && cargo test)
nix flake check        # tests, clippy, rustfmt, and on Linux a NixOS VM test
```

Tests run against `gha-cache-fusefs fake-server`, an imitation of the cache
service built from measurements of the real one. The filesystem core
(`fusefs/src/vfs`) knows nothing of FUSE, so it's tested on macOS too, and
against a model of POSIX. `fusefs/tests/e2e.sh` runs real tools through the
kernel: in a NixOS VM, as an unprivileged user on a runner, and across three
dependent jobs against the real cache. Benchmarks: `nix run . -- bench`.
