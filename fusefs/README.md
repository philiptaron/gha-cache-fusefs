# gha-cache-fusefs

Mount the GitHub Actions cache as a filesystem.

```yaml
permissions:
  actions: read      # listing cache entries needs it; `write` also allows --gc
  contents: read

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: philiptaron/gha-cache-fusefs/mount@main
        with:
          path: /mnt/cache
      - run: |
          make
          cp -r out/ /mnt/cache/builds/${{ github.sha }}/

  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: philiptaron/gha-cache-fusefs/mount@main
        with:
          path: /mnt/cache
          read-only: true
      - run: /mnt/cache/builds/${{ github.sha }}/bin/tests
```

Files written into the mount become cache entries. They are uploaded in the
background once closed, and all of them are uploaded by the time the job
ends; the action's post step waits and fails the job if anything could not be
saved. Any later job that can read the cache sees them. Reads are lazy: only
the byte ranges a program touches are downloaded, so a multi-gigabyte
squashfs, sqlite, or zip file can be used in place.

## What works

| | |
|---|---|
| Reading | `cat`, `cp`, `tar`, `mmap`, random access, executing binaries |
| Writing | any write pattern, including seeking and sparse files; `cp -a`, `tar -x`, `rsync`, `dd conv=fsync` |
| Metadata | permission bits (so `chmod +x` survives), mtimes, symlinks, empty files, empty directories |
| Namespace | `mkdir`, `rmdir`, `rm`, `rm -r`, and `mv` of anything written in this job; `mv` of other remote files falls back to copy + delete, as across filesystems |
| Overwrites | last writer wins; overwriting or deleting a file on a branch hides the default branch's copy on that branch only |
| Not supported | hard links, FIFOs, device nodes, extended attributes, file ownership (always the mounting user) |

Changing a file that exists only in the cache (appending, `chmod`, `truncate`,
opening it read-write) downloads it first: entries are immutable, so every
change is a new upload.

## How it works

Each file is one cache entry whose key is the path under a prefix (default
`fusefs/`). Inode metadata is encoded in the entry's 64-hex-digit *version*,
so listing the cache is enough to `stat` everything. Deletions are
*whiteouts*. The scopes a run can read — its own ref, the pull request's base,
and the default branch — are layered like overlayfs. [DESIGN.md](DESIGN.md)
covers the details, including what the cache service actually does as
measured with a probe workflow.

## Permissions and limits

* The REST API is the only way to list the cache, so the token (default
  `${{ github.token }}`) needs `actions: read`.
* Runs whose cache token is read-only (for example, some pull requests from
  forks) are mounted read-only.
* The cache service's limits apply: the repository's cache quota (10 GB by
  default) and eviction after 7 days without access. Creating entries is
  rate limited, too. In practice about 200 new files per ~30 s per
  repository are allowed; beyond that the daemon waits as instructed
  (`Retry-After`), so saving thousands of small files takes minutes. Pack
  them (tar, squashfs, zip) if that matters: one large file is one entry,
  and reads of it stay lazy.

## The action

| input | default | |
|---|---|---|
| `path` | *(required)* | where to mount; created if missing |
| `prefix` | `fusefs/` | the cache key prefix of the mount root; mount a subdirectory with e.g. `fusefs/builds/` |
| `read-only` | `false` | |
| `token` | `${{ github.token }}` | for listing (and, with `gc`, deleting) entries |
| `gc` | `false` | delete entries that newer writes superseded |
| `settle` | `1s` | how long a closed file waits before uploading, so write-then-rename uploads only the final name |
| `cache-size-mb` | `8192` | local disk budget for downloaded data |
| `binary` | | a prebuilt binary; otherwise it is built with Nix, or downloaded from a release |
| `fail-on-error` | `true` | fail the job if changes could not be saved |
| `log` | `info` | daemon log filter |

## Without the action

The runtime token the cache service needs is only exported to actions, not
to `run:` steps. Export it yourself, then drive the binary directly:

```yaml
- uses: actions/github-script@v8
  with:
    script: |
      core.exportVariable('ACTIONS_RUNTIME_TOKEN', process.env.ACTIONS_RUNTIME_TOKEN)
      core.exportVariable('ACTIONS_RESULTS_URL', process.env.ACTIONS_RESULTS_URL)
- run: |
    nix build github:philiptaron/gha-cache-fusefs#static
    ./result/bin/gha-cache-fusefs mount /mnt/cache --daemon
    ...
    ./result/bin/gha-cache-fusefs unmount /mnt/cache   # waits for uploads
  env:
    GITHUB_TOKEN: ${{ github.token }}
```

`gha-cache-fusefs mount --help` lists the options.

## Development

```sh
nix develop            # cargo, clippy, rustfmt, rust-analyzer
cd fusefs && cargo test
nix flake check        # tests, clippy, rustfmt, and on Linux a NixOS VM test
```

The tests run against `gha-cache-fusefs fake-server`, a local imitation of
the cache service with the semantics measured against the real one. The core
filesystem logic (`src/vfs`) is independent of FUSE, so everything except the
kernel adapter is tested on macOS too. `tests/e2e.sh` runs real tools through
the kernel; CI runs it against the fake service in a NixOS VM, as an
unprivileged user on a runner, and across three dependent jobs against the
real cache.
