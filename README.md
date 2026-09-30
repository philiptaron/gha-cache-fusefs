# gha-cache-fusefs

Mount the GitHub Actions cache as a filesystem.

```yaml
permissions:
  actions: read      # listing cache entries needs it
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

Files written into the mount are saved to the cache in the background once
closed, and all of them are saved by the time the job ends; the action's post
step waits and fails the job if anything could not be saved. Any later job
that can read the cache sees them. Reads are lazy: only the byte ranges a
program touches are downloaded, so a multi-gigabyte squashfs, sqlite, or zip
file can be used in place.

## What works

| | |
|---|---|
| Reading | `cat`, `cp`, `tar`, `mmap`, random access, executing binaries |
| Writing | any write pattern, including seeking and sparse files; `cp -a`, `tar -x`, `rsync`, `dd conv=fsync` |
| Metadata | permission bits (so `chmod +x` survives), mtimes to the nanosecond, of files and directories; symlinks, empty files, empty directories |
| Namespace | `mkdir`, `rmdir`, `rm`, `rm -r`, and `mv` of anything, remote files and directories included, without downloading |
| Overwrites | last writer wins; overwriting or deleting a file on a branch hides the default branch's copy on that branch only |
| Not supported | hard links, FIFOs, device nodes, extended attributes, file ownership (always the mounting user) |

Changing the contents of a file that exists only in the cache (appending,
`truncate`, opening it read-write) downloads it first: entries are
immutable, so every change writes the whole file again. Renaming it, or
changing its mode or mtime, does not: the new layer refers to the data
where it is.

## How it works

Each batch of changes becomes one cache entry, a *layer*: an EROFS image
holding the files, symlinks, directories, and deletions (*whiteouts*) of
that batch. Files over 8 MiB become entries of their own, *blobs*, which
layers refer to by digest. A mount reads the metadata of every layer, one
ranged request each, and stacks them like overlayfs, the newest on top;
the scopes a run can read (its own ref, the pull request's base, and the
default branch) stack the same way. All of a *volume*'s entries have keys
starting with `gha-fs/<volume>/`. [LAYERS.md](fusefs/LAYERS.md) specifies
the format, and [DESIGN.md](fusefs/DESIGN.md) covers the rest, including
what the cache service actually does as measured with a probe workflow.

## Next to actions/cache

The mount and `actions/cache` share the repository's cache storage and
nothing else:

* **`actions/cache` entries never appear in the mount**, even when their keys
  start with a volume's `gha-fs/<volume>/`. Their versions (a hash of the
  cached paths) lack the filesystem's marker, so the listing skips them.
* **`actions/cache` never restores what the mount wrote**, not even through
  `restore-keys`. Its lookups include its own version, which never matches.
* **The mount never changes or deletes any entry.** Deletions are whiteouts
  in its own layers.
* Both draw on the same quota, LRU eviction, and entry-creation rate limit,
  and both follow the same branch scoping.

They can be used in the same job. CI checks all of this against the real
service with `actions/cache/save` and `actions/cache/restore`.

## Permissions and limits

* The REST API is the only way to list the cache, so the token (default
  `${{ github.token }}`) needs `actions: read`.
* Runs whose cache token is read-only (for example, some pull requests from
  forks) are mounted read-only.
* The cache service's limits apply: the repository's cache quota (10 GB by
  default) and eviction after 7 days without access. A mount reads each
  scope's newest snapshot and the layers written since, which keeps a
  volume in use alive; a job that finds 16 layers of its own scope since
  the last snapshot writes a new one when it unmounts
  (`GHA_CACHE_FUSEFS_SNAPSHOT_AFTER`, 0 to never). Older layers then expire
  once no visible file needs their data. Creating entries is rate limited, to about
  200 per ~45 s per repository. A batch of changes is one entry however
  many files it has, so this only matters for many files over 8 MiB, or
  with `fsync: commit` for programs that `fsync` every file; the daemon
  then waits as instructed (`Retry-After`).
* The REST budget of `GITHUB_TOKEN` (1,000 requests an hour, per
  repository) pays for listings: one request per 100 layers and blobs per
  scope at every mount.

## The action

| input | default | |
|---|---|---|
| `path` | *(required)* | where to mount; created if missing |
| `volume` | `default` | the filesystem to mount; separate volumes share nothing but the cache's quota |
| `root` | | a directory of the volume to mount instead of all of it, such as `builds/linux` |
| `read-only` | `false` | |
| `token` | `${{ github.token }}` | for listing entries |
| `settle` | `1s` | how long a closed file waits before it is saved, so write-then-rename saves only the final name |
| `fsync` | `local` | what `fsync` waits for: `local` returns at once, and the file uploads with the next batch; `commit` uploads it and waits, a layer per call |
| `cache-size-mb` | `8192` | local disk budget for downloaded data |
| `binary` | | a prebuilt binary; otherwise one is downloaded from a release, or built with Nix or cargo |
| `release` | | the release to download the binary from (`vX.Y.Z`, `main-latest`); by default, the one for the action's ref (`main-latest` for `@main`) |
| `fail-on-error` | `true` | fail the job if changes could not be saved |
| `log` | `info` | daemon log filter |

## Without the action

The runtime token the cache service needs is only exported to actions, not
to `run:` steps. Export it yourself, then drive the binary directly:

```yaml
- uses: actions/github-script@v9
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
nix develop            # cargo, clippy, rustfmt, rust-analyzer, erofs-utils
(cd fusefs && cargo test)
nix flake check        # tests, clippy, rustfmt, and on Linux a NixOS VM test
```

The tests run against `gha-cache-fusefs fake-server`, a local imitation of
the cache service with the semantics measured against the real one. The core
filesystem logic (`fusefs/src/vfs`) is independent of FUSE, so everything
except the kernel adapter is tested on macOS too. `fusefs/tests/e2e.sh` runs
real tools through the kernel; CI runs it against the fake service in a
NixOS VM, as an unprivileged user on a runner, and across three dependent
jobs against the real cache.

[PERFORMANCE.md](fusefs/PERFORMANCE.md) analyzes what the service allows and
what the filesystem achieves. Its benchmarks run against a model of the
service (`nix run . -- bench`), without it (`--service local`), or against the
real cache (the manual `fusefs-bench` workflow).
