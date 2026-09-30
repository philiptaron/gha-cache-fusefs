# gha-cache-fusefs: performance

This document covers what limits the filesystem's speed, how fast it is, and
where it falls short. The service facts come from
[DESIGN.md §1](DESIGN.md#1-the-service-as-measured). The measurements come
from `gha-cache-fusefs bench` (§6). It drives the filesystem core through a
model of the service that reproduces those facts, or through the real cache.

## 1. Summary

This analysis first measured format 1, one cache entry per file, and led to
format 2, [layers](LAYERS.md), in which a batch of changes is one entry.
§4.1 and §4.2 measure format 2; §4.3 on measured format 1, and the fixes
they led to carry over.

* **Small files no longer wait on the rate limit.** Saving 1,000 files of
  4 KiB takes 1.1 s in the model and one creation, where format 1 took 204 s
  and 1,000 creations. `rm -r` of them takes 0.8 s instead of 204 s.
  `fsync` returns at once by default. With `--fsync commit`, programs that
  `fsync` every file still make an entry per file, each waiting three round
  trips, about 1.3 a second; it takes several such jobs at once to meet the
  limit (§4.1).
* **Reading small files costs bandwidth, not round trips.** `cp -r` of the
  1,000 files takes 1.2 s and 4 requests, where format 1 took 91 s and
  2,000: a directory's files are one range of a layer, and files of up to
  1 KiB arrive with the layer's metadata.
* **Large files stream.** The model gives 43 MB/s for downloads, close to the
  52 MB/s the service gives parallel downloads, and 39 MB/s for uploads,
  a little below format 1's 45 MB/s: a large file is hashed before it is
  uploaded as a blob, and its layer costs three more round trips. The
  service allows 84 MB/s for parallel uploads (§4.8).
* **Metadata costs nothing.** `stat`, `ls -lR`, `readlink`, and lookups of
  existing names stay in memory, at about 1 µs per entry.
* **Mounting costs one REST request per 100 layers and blobs** per scope,
  plus a download URL and a ranged GET per layer. 20,000 files in 20 layers
  mount in 1.3 s with one REST request, where format 1 took 7.2 s and 200.
  But layers are never dropped yet, so this grows with a volume's history
  until snapshots exist (§5).
* The benchmarks also turned up problems in the daemon, since fixed. A 429
  paused reads as well as writes: a cold read waited 26.5 s instead of
  0.5 s (§4.5). Writing *n* files cost O(*n*²) locally, 20 s for 20,000
  (§4.6). Readahead fell back to 1 MiB requests (§4.8). Hashing blobs with
  the `sha2` crate ran at 360 MB/s on ARM (§4.2). One remains open: a large
  upload keeps only 4 blocks in flight, which the real service has to
  confirm matters.

## 2. What the service allows

| | measured from hosted runners | used by the model |
|---|---|---|
| latency | 220–320 ms per request (Twirp, REST, and blob alike) | 250 ms |
| download | ~23 MB/s per stream; ~52 MB/s for 8 parallel ranges | the same |
| upload | 128 MiB in 1.6 s as 8 parallel blocks (~84 MB/s) | 84 MB/s total, 23 MB/s per request (assumed) |
| new entries | ~200 in ~10 s, then 429 with `Retry-After` 30–40 s | 200 per fixed 45 s window |
| REST | 100 entries per listing page | not limited |

Two numbers need care.

* **The creation budget.** DESIGN.md and the README describe it as "about
  200 per half minute". But the probe saw 200 calls in about ten seconds,
  then a `Retry-After` of 30–40 s, which is a cycle of 40–50 s. The model uses
  45 s, or ≈ 4.4 creations/s sustained. The budget belongs to the repository,
  so concurrent jobs share it, and so do `actions/cache` saves in other
  workflows. The `fusefs-bench` workflow measures it against the real
  service.
* **The REST budget.** GitHub documents it: "The rate limit for
  `GITHUB_TOKEN` is 1,000 requests per hour per repository". Every job
  shares it, and an exhausted budget answers 403 or 429 until
  `x-ratelimit-reset`. The probe has not measured it, and the model does not
  enforce it, but it bounds mounting and refreshing (§4.9).

## 3. What operations cost

In format 2, where *L* is the number of layers and blobs:

| operation | requests | new entries |
|---|---|---|
| mount | ⌈*L*/100⌉ REST pages per scope, 8 in flight; per layer, a download URL and a ranged GET of its metadata, 16 layers at a time | – |
| `stat`, `readdir`, `readlink`, lookup of an existing name | none | – |
| lookup of a missing name | at most one re-list per 15 s (≥ 1 REST request per scope), and the metadata of new layers | – |
| read of a file up to 1 KiB | none: it came with the metadata | – |
| first read of another file | a ranged GET on its layer (whose URL came with the metadata) or on its blob (after a download URL) | – |
| later reads | a ranged GET per missing run of ≤ 8 MiB; readahead grows to 64 MiB | – |
| a batch of changes: files ≤ 8 MiB, symlinks, directories, deletions | `CreateCacheEntry`, `Put Blob` or 8 MiB blocks, `FinalizeCacheEntryUpload` | 1 |
| a file over 8 MiB | the same, for its blob, unless a readable scope has it | 1 more |
| `fsync` | nothing; with `--fsync commit`, a batch of its own if nothing else is due | 0 (1) |
| `mv` of a remote file or directory, `chmod` or `touch` of a remote file | a layer of metadata that refers to the data where it is | 1 |

Several consequences follow.

* **Creations follow batches, not files.** A batch holds everything due when
  the previous layer finished uploading, up to 10,000 paths or 256 MiB, so
  the harder a job writes, the larger its batches. A mount makes at most
  about 1.3 layers a second (three round trips each), well below the 4.4 a
  second the service sustains; several mounts at once can still reach the
  limit.
* **Large files cost bandwidth, and a hash.** At 84 MB/s, the model's upload
  link, 1 GiB takes 13 s; hashing it first takes about half a second more
  (2.3 GB/s here, with the CPU's SHA-256 instructions).
* **Reading uncached small files costs bandwidth, not round trips.** A
  directory's small files are one range of a layer, so the first read
  fetches them all in 8 MiB requests.
* **A cold random read of 4 KiB fetches its 1 MiB chunk**: one round trip
  plus 45 ms of transfer.
* **Mount time and REST cost grow with every layer**, including layers
  whose files later layers replaced or deleted, until snapshots let old
  layers expire.

Format 1 made every file, symlink, empty directory, and deletion an entry
of its own. At 4.4 creations/s, each used 0.23 s of the sustained budget,
the time 84 MB/s takes to move 19 MB, so files smaller than that cost more
budget than bandwidth. It read *n* uncached small files in 2*n* round
trips, and its mounts listed every superseded version and whiteout.

## 4. Measurements

All results below come from an Apple M2 (8 cores) against the fake service in
the same process. The CPU numbers of a 4-vCPU hosted runner will be somewhat
higher.

### 4.1 Format 2 against the model of the hosted service

`gha-cache-fusefs bench --scale full`. A "–" under requests means the
step's own requests cannot be told apart from others in the same job.

| scenario | step | time | result | requests: cache service / blob / REST | rate limited |
|---|---|---:|---|---:|---:|
| large | write 256 MiB (local) | 63 ms | 4271.0 MB/s | 0 / 0 / 0 | – |
| large | upload it (unmount) | 6.8 s | 39.4 MB/s | 4 / 34 / 0 | – |
| large | read it sequentially, cold | 6.2 s | 43.4 MB/s, first byte after 559 ms | 1 / 35 / 0 | – |
| large | read it again, cached | 32 ms | 8336.7 MB/s | 0 / 0 / 0 | – |
| large | 200 random 4 KiB reads, cold | 43.7 s | p50 303 ms, p90 306 ms, max 557 ms | 1 / 143 / 0 | – |
| small | write 1000 files of 4 KiB (local) | 99 ms | 10091.9 files/s | 0 / 0 / 0 | – |
| small | upload them (unmount) | 1.1 s | 933.3 files/s | 2 / 1 / 0 | – |
| small | mount, stacking 1000 files | 770 ms | | 1 / 1 / 1 | – |
| small | read them all, cold (cp -r) | 1.2 s | 817.3 files/s | 0 / 4 / 0 | – |
| small | stat them all (ls -lR) | 0.78 ms | 0.8 µs per entry | 0 / 0 / 0 | – |
| small | remove them (rm -r, unmount) | 799 ms | 1251.1 whiteouts/s | 2 / 1 / 0 | – |
| mount | list 20 layers, read their metadata, and build the tree of 20000 files | 1.3 s | 14993.3 files/s | 20 / 20 / 1 | – |
| mount | stack 200000 files in 200 layers and build the tree (CPU only) | 238 ms | 1.2 µs per file | – | – |
| throttled | write and fsync files in 6 jobs until the first 429 | 25.5 s | 198 layers | 67 / 34 / 0 | – |
| throttled | a cold 4 KiB read in one of them | 255 ms | | – | – |
| throttled | the same read in another job | 255 ms | | 0 / 1 / 0 | – |
| throttled | write and fsync the rest, and unmount | 25.7 s | 300 files in all | 33 / 16 / 0 | 1 × 429, paused 12.0 s |

Against format 1 (§4.3):

* **Small files.** Uploading 1,000 files is one layer: 2 cache-service
  requests and 1 upload in 1.1 s, instead of 3,032 requests in 204 s.
  `rm -r` of them is one layer of 1,000 whiteouts and a `drop` per directory,
  0.8 s instead of 204 s. `cp -r` makes 4 range requests in 1.2 s, instead
  of 2,000 requests in 91 s: the first read fetches the directory's range of
  the layer, and nothing else needs the network.
* **Mounting** 20,000 files takes one REST request and 1.3 s, instead of 200
  and 7.2 s. The 20 layers' metadata arrive in two rounds of sixteen
  parallel reads, each a download URL and a GET.
* **A cold small read** takes one round trip, 255 ms instead of 504: the
  layer's download URL came with its metadata.
* **Large uploads are slower**, 39 MB/s instead of 45. The file is hashed
  before it is uploaded as a blob (0.1 s for 256 MiB), and its layer adds a
  creation, an upload, and a finalization, 0.75 s in all. Large reads are
  unchanged: they are ranges of the blob, as they were ranges of the file's
  own entry.
* **The rate limit takes several jobs now.** With `--fsync commit`, one job
  that writes and `fsync`s one file at a time makes a layer per file, one after another,
  about 1.3 a second; the quick-scale run of 250 files never met the limit
  in 180 s. Six such jobs at once met it after 198 layers in 25 s. As
  before, a throttled job's reads do not wait. The 429 landed in another of
  the six jobs this time, so the first row counts no pause.

### 4.2 Format 2 without delays or limits

With `--service local`, what remains is the daemon's own cost. This run used
`--files 20000 --entries 20000 --large-mib 1024 --random-reads 1000`:

| scenario | step | time | result | requests: cache service / blob / REST |
|---|---|---:|---|---:|
| large | write 1024 MiB (local) | 292 ms | 3683.2 MB/s | 0 / 0 / 0 |
| large | upload it (unmount) | 800 ms | 1341.8 MB/s | 4 / 130 / 0 |
| large | read it sequentially, cold | 312 ms | 3441.1 MB/s, first byte after 1.42 ms | 1 / 131 / 0 |
| large | read it again, cached | 78 ms | 13740.2 MB/s | 0 / 0 / 0 |
| large | 1000 random 4 KiB reads, cold | 236 ms | p50 0.34 ms, p90 0.39 ms, max 1.15 ms | 1 / 634 / 0 |
| small | write 20000 files of 4 KiB (local) | 1.6 s | 12795.6 files/s | 0 / 0 / 0 |
| small | upload them (unmount) | 1.6 s | 12423.3 files/s | 6 / 13 / 0 |
| small | mount, stacking 20000 files | 24 ms | | 3 / 3 / 1 |
| small | read them all, cold (cp -r) | 86 ms | 231325.9 files/s | 0 / 71 / 0 |
| small | stat them all (ls -lR) | 6.49 ms | 0.3 µs per entry | 0 / 0 / 0 |
| small | remove them (rm -r, unmount) | 130 ms | 153834.4 whiteouts/s | 6 / 3 / 0 |
| mount | list 20 layers, read their metadata, and build the tree of 20000 files | 25 ms | 797416.4 files/s | 20 / 20 / 1 |
| mount | stack 200000 files in 200 layers and build the tree (CPU only) | 234 ms | 1.2 µs per file | – |

The 20,000 files upload as three layers, capped by the 10,000 paths a batch
may hold, in 1.6 s: most of it sealing, which copies every file into the
image. The 1 GiB upload spends 0.47 s hashing. That was 2.9 s with the
`sha2` crate, which uses the CPU's SHA-256 instructions only on x86_64, so
blobs are hashed with `ring`, which rustls already brings along.

### 4.3 Format 1 against the model of the hosted service

`gha-cache-fusefs bench --scale full`, before layers. A "–" under requests
means the step's own requests cannot be told apart from others in the same
job.

| scenario | step | time | result | requests: cache service / blob / REST | rate limited |
|---|---|---:|---|---:|---:|
| large | write 256 MiB (local) | 58 ms | 4625.3 MB/s | 0 / 0 / 0 | – |
| large | upload it (unmount) | 6.0 s | 44.9 MB/s | 2 / 33 / 0 | – |
| large | read it sequentially, cold | 6.0 s | 44.6 MB/s, first byte after 556 ms | 1 / 35 / 0 | – |
| large | read it again, cached | 23 ms | 11836.1 MB/s | 0 / 0 / 0 | – |
| large | 200 random 4 KiB reads, cold | 43.5 s | p50 302 ms, p90 305 ms, max 555 ms | 1 / 143 / 0 | – |
| small | write 1000 files of 4 KiB (local) | 88 ms | 11315.6 files/s | 0 / 0 / 0 | – |
| small | upload them (unmount) | 204 s | 4.9 files/s | 2032 / 1000 / 0 | 32 × 429, paused 108 s |
| small | mount, listing 1000 entries | 765 ms | | 0 / 0 / 10 | – |
| small | read them all, cold (cp -r) | 91.3 s | 10.9 files/s | 1000 / 1000 / 0 | – |
| small | stat them all (ls -lR) | 1.61 ms | 1.6 µs per entry | 0 / 0 / 0 | – |
| small | remove them (rm -r, unmount) | 204 s | 4.9 whiteouts/s | 2032 / 1000 / 0 | 32 × 429, paused 108 s |
| mount | list 20000 entries and build the tree | 7.2 s | 2789.9 entries/s | 0 / 0 / 200 | – |
| mount | build the tree for 200000 listed entries (CPU only) | 273 ms | 1.4 µs per entry | – | – |
| throttled | write 300 files until the first 429 | 18.5 s | 192 uploaded | 392 / 199 / 0 | 1 × 429, paused 26.0 s |
| throttled | a cold 4 KiB read in that job | 504 ms | | – | – |
| throttled | the same read in another job | 503 ms | | 1 / 1 / 0 | – |
| throttled | upload the rest (unmount) | 35.4 s | 300 files in all | 202 / 101 / 0 | – |

The upload and `rm -r` of small files are all rate limit: 1,000 creations
at 200 per 45 s is four full windows plus the time to upload the last 200.
Random reads fetched 143 chunks for 200 reads, because some landed in chunks
already fetched. Before the fixes of §4.5 and §4.8, the throttled job's read
waited 26.5 s, and the sequential read made 201 requests.

### 4.4 Format 1 without delays or limits

With `--service local`, what remains is the daemon's own cost. This run used
`--files 20000 --entries 20000 --large-mib 1024 --random-reads 1000`:

| scenario | step | time | result | requests: cache service / blob / REST |
|---|---|---:|---|---:|
| large | write 1024 MiB (local) | 348 ms | 3082.2 MB/s | 0 / 0 / 0 |
| large | upload it (unmount) | 391 ms | 2744.6 MB/s | 2 / 129 / 0 |
| large | read it sequentially, cold | 361 ms | 2972.1 MB/s, first byte after 1.18 ms | 1 / 131 / 0 |
| large | read it again, cached | 86 ms | 12430.1 MB/s | 0 / 0 / 0 |
| large | 1000 random 4 KiB reads, cold | 292 ms | p50 0.39 ms, p90 0.50 ms, max 2.04 ms | 1 / 634 / 0 |
| small | write 20000 files of 4 KiB (local) | 1.8 s | 10975.0 files/s | 0 / 0 / 0 |
| small | upload them (unmount) | 1.8 s | 11422.0 files/s | 40000 / 20000 / 0 |
| small | mount, listing 20000 entries | 277 ms | | 0 / 0 / 200 |
| small | read them all, cold (cp -r) | 4.7 s | 4249.7 files/s | 20000 / 20000 / 0 |
| small | stat them all (ls -lR) | 8.40 ms | 0.4 µs per entry | 0 / 0 / 0 |
| small | remove them (rm -r, unmount) | 3.0 s | 6760.5 whiteouts/s | 40000 / 20000 / 0 |
| mount | list 20000 entries and build the tree | 168 ms | 119058.5 entries/s | 0 / 0 / 200 |
| mount | build the tree for 200000 listed entries (CPU only) | 273 ms | 1.4 µs per entry | – |

Before the fixes of §4.6 and §4.8, writing the 20,000 files took 20.2 s,
and the cold sequential read made 969 requests in 233 ms.

### 4.5 A 429 paused every request in the job (fixed)

Twirp, blob, and REST clients shared one `RateGate`. Of the requests the
probe measured, only entry creation is rate limited. Yet while creations
waited out a `Retry-After`, so did every download, download-URL lookup, and
listing. In the `throttled` scenario, a cold 4 KiB read waited 26.5 s in the
job whose uploads were throttled: the whole `Retry-After`. The same read took
0.5 s in another job at the same moment. Now each service, and each Twirp
method, has a gate of its own (`Http::gated` in `src/api/mod.rs`), and the
read takes 0.5 s in both jobs.

An exhausted REST budget had a worse version of the problem. It answers 403
until a reset up to an hour away, and the refresh that hit it slept until the
reset while holding `refresh_lock`. Every lookup of a missing name waited
behind it. Now three things change:

* REST requests fail rather than wait more than a minute (`Retry::rest`).
* Refreshes run in the background, and a lookup waits for one at most 2 s.
* A mount against an exhausted budget fails at once, and says when to try
  again.

### 4.6 Writing *n* files cost O(*n*²) (fixed)

Every `release` wakes the committer. On every wake-up, `collect`
(`src/vfs/commit.rs`) cloned every pending key and walked all of them, while
it held the state lock that every filesystem operation needs. So each new
file cost time in proportion to the files already pending. 200 files took
15 ms to write, and 20,000 took 20 s, a hundred times the files for over a
thousand times the time. A profile of the 20,000-file write put half the
samples in `collect`, cloning keys, and the writer in `RawMutex::lock_slow`,
waiting for it. Through FUSE, the stalled thread is the one that serves every
request, so `tar -x` of a large tree slowed down as it went, and so did
everything else on the mount.

Pending operations now wait in a queue ordered by due time (`State::queue`),
and a wake-up looks only at what is due. Unmount checks for completion at
most every 20 ms instead of after every upload. Writing is linear
(`--service local --only small --files N`):

| files | writing them, before | after | uploading at unmount, before | after |
|---:|---:|---:|---:|---:|
| 1,000 | 119 ms | 73 ms | 48 ms | 43 ms |
| 5,000 | 1.5 s | 0.40 s | 0.51 s | 0.23 s |
| 10,000 | 5.4 s | 0.77 s | 1.6 s | 0.53 s |
| 20,000 | 20.0 s | 1.5 s | 4.9 s | 1.5 s |

What remains superlinear in the upload column is the fake service, which
scans its list of entries on every request.

### 4.7 Reading small files was bound by round trips (fixed by layers)

`cp -r` of 1,000 remote 4 KiB files took 91 s (11 files/s) and made 2,000
requests. That is the ceiling from §3: sibling prefetch overlaps six files,
and each still needs a download URL and a GET. The files hold 4 MB in all,
which one GET moves in 0.4 s.

### 4.8 Large transfers left bandwidth unused

**Uploads.** A 256 MiB upload ran at 45 MB/s. Four blocks are in flight at
once, and each pays a round trip before its bytes flow:
4 × 8 MiB / (0.25 s + 8 MiB / 23 MB/s) ≈ 55 MB/s, less the create and
finalize. The probe moved 84 MB/s with 8 blocks in flight. The per-request
upload rate here is the model's assumption, so the real run should confirm
this before anything changes.

**Downloads (fixed).** A cold sequential read of 256 MiB made 201 range
requests. DESIGN.md §6 expects 8 MiB ranges, which would be 32. Readahead
grows its window to 64 MiB, and `start` (`src/data.rs`) coalesces missing
chunks into runs of up to 8 MiB. But once the window was full, each 128 KiB
read slid it forward by at most one chunk, so from then on every request was
a single 1 MiB chunk. With 16 fetch slots, that capped a reader at 16 MiB
per round trip (≈ 57 MB/s), whatever the link allows. The first byte came
after 0.65 s, because the first request fetched 3 MiB rather than the one
chunk the reader waited for.

Now the chunk the reader needs goes first, in a request of its own, and the
window is topped up in whole 8 MiB runs. The same read makes 35 requests,
and its first byte comes after 0.57 s. Throughput stays at the model's link
limit of about 45 MB/s. Without latency (§4.4), the read got slower, from
4.6 GB/s to about 3 GB/s. A chunk becomes readable only once its whole run
has landed, which matters only when requests cost nothing.

### 4.9 Mounting spends the REST budget

In format 1, mounting 20,000 entries took 7.0 s and 200 REST requests: a
fifth of the repository's hourly budget, per scope, per mount. On a pull
request with three scopes, three such mounts used the whole hour. In
format 2, 20,000 files written in batches of 1,000 are 20 layers, one REST
request (§4.1). The listing now grows with the number of batches, and
until snapshots exist (§5), with every batch the volume ever had. Lookups of missing names
re-list at most every 15 s, which is up to 240 refreshes an hour per scope.
Compilers probing include paths, Python probing for modules, and `git`
looking for `.git` all look up missing names.

Two fixes spend a little more. First, listings are cut into pages by
position, so an entry deleted while a listing pages used to hide another at
a page boundary. A listing whose pages disagree about the total is now
repeated, up to three times. In a busy prefix, that can triple a mount's
REST requests. Second, a mount resolves a download URL, four at a time in
the background, for up to 1,000 stale entries it depends on but does not
read (DESIGN.md §3.3): in format 1, markers, empty files, and whiteouts; in
format 2, only blobs, since reading a layer's metadata already counts as
use. Those are Twirp lookups, not REST requests.

### 4.10 What is fine

* Stacking layers and building the tree takes 1.2 µs of CPU per file, so
  a million files would take a second or two.
* `ls -lR` costs about 1 µs per entry, with no requests.
* Cached reads run at local disk speed.

## 5. Levers

Packing small objects into shared entries, the first lever of this
analysis, became format 2. These are ordered by how much of what remains
they would remove.

* **Snapshots** (LAYERS.md §8). Every mount reads every layer, and nothing
  lets old layers go, so mount time and REST cost grow with a volume's
  history. A snapshot holds the merged tree of its scope, and mounts read
  only the layers it does not cover.
* **Not pipelining layers.** Creating and uploading the next layer while
  the previous one finalizes looked like it would take a mount from 1.3
  layers a second to about 4. It would not help the program it was for: an
  `fsync` in `commit` mode waits for its layer, so a program that syncs
  every file has one file due at a time, and each sync costs three round
  trips however the uploads overlap. Several programs syncing at once
  already share the next batch; overlapping would only make batches
  smaller and spend more creations. `fsync` in the default `local` mode
  waits for nothing instead.
* **Skip unchanged uploads.** A file whose digest and attributes equal what
  the view already shows needs no upload. That covers `cp -a`, `tar -x`,
  and `rsync` of mostly unchanged trees; for large files, the blob is
  already skipped.
* **Upload 8 blocks at a time**, if the real service confirms §4.8.
* **Mark chunks present as a range streams in**, rather than when all of it
  has landed (§4.8).

## 6. Method

`gha-cache-fusefs bench` drives the filesystem core (`Vfs`), as the FUSE
adapter does, but without the kernel. Every "job" is a new mount with its own
data directory and HTTP client, as on a fresh runner. Nothing is uploaded
before a job unmounts, except in `throttled`. Each scenario gets its own fake
service, or its own volume of the real cache.

| scenario | steps |
|---|---|
| `large` | write and upload one large file; read it sequentially cold, then cached; random 4 KiB reads cold |
| `small` | write and upload a tree of small files; mount it; `cp -r`, `ls -lR`, and `rm -r` it, each in a new job |
| `mount` | mount a volume of many files in layers of 1,000; build ten times as large a tree from layers in memory (fake service only) |
| `throttled` | six jobs with `--fsync commit` write and `fsync` files one at a time, a layer each, into the rate limit; meanwhile, one of them and another job each read a small file |

The model (`FakeConfig::hosted`) adds these, and only these:

* 250 ms to every request;
* per-request and total transfer rates for blob bodies, where transfers
  queue for the shared link in arrival order (a real link shares bandwidth
  fairly, which matters when large and small transfers mix);
* a fixed window on creations: it opens with the first call, admits 200, and
  answers the rest with a `Retry-After` of the time until it closes.

It does not model latency variance, the REST budget, or Azure-side
throttling. The benchmarks do not measure the kernel either. FUSE adds a
context switch per request of up to 128 KiB, and 1 s attribute and entry
caches. The kernel page cache also makes cached re-reads faster than here. To
measure those, run `fusefs/tests/e2e.sh` or any other tool against
`gha-cache-fusefs fake-server --profile hosted`.

## 7. Running the benchmarks

```sh
nix run . -- bench                               # the model, quick sizes: ~5 min
nix run . -- bench --scale full --json out.json  # ~15 min
nix run . -- bench --service local --files 20000 # the daemon's own costs
nix run . -- bench --only small,throttled        # some scenarios
gh workflow run fusefs-bench -f scale=full       # the real cache, and the model on a runner
```

Against the real cache (`--service real`, which needs the Actions
environment), the benchmark works in volumes named `bench-<run>-<scenario>`.
Afterwards it deletes the entries larger than 1 MiB. It leaves the small ones to expire, because
deleting each would cost a REST request.
