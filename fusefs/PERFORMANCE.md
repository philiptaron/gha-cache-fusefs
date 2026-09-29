# gha-cache-fusefs: performance

This document covers what limits the filesystem's speed, how fast it is, and
where it falls short. The service facts come from
[DESIGN.md §1](DESIGN.md#1-the-service-as-measured). The measurements come
from `gha-cache-fusefs bench` (§6). It drives the filesystem core through a
model of the service that reproduces those facts, or through the real cache.

## 1. Summary

* **Many small writes wait on the rate limit.** Every file, symlink, empty
  directory, and deletion is a new cache entry, and the service allows about
  200 of them per 45 s per repository. Saving 1,000 small files took 201 s in
  the model, and `rm -r` of them took as long again. Bandwidth only starts to
  matter above ~10–20 MB per file.
* **Every uncached file costs two round trips** (~0.5 s) before its first
  byte. Sibling prefetch brings `cp -r` of small files up to 11 files/s; a
  reader of scattered files gets 2.
* **Large files stream.** The model gives 44–45 MB/s in either direction,
  close to the 52 MB/s the service gives parallel downloads. That is about
  half the 84 MB/s it takes for parallel uploads (§4.6).
* **Metadata costs nothing.** `stat`, `ls -lR`, and lookups of existing names
  stay in memory, at about 1 µs per entry.
* **Mounting costs one REST request per 100 listed entries** per scope, from a
  budget of 1,000 an hour that the whole repository shares. The listing
  counts superseded versions and whiteouts, not just live files.
* The benchmarks also turned up problems in the daemon, since fixed. A 429
  paused reads as well as writes: a cold read waited 26.5 s instead of
  0.5 s (§4.3). Writing *n* files cost O(*n*²) locally, 20 s for 20,000
  (§4.4). Readahead fell back to 1 MiB requests (§4.6). One remains open: a
  large upload keeps only 4 blocks in flight, which the real service has to
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
  enforce it, but it bounds mounting and refreshing (§4.7).

## 3. What operations cost

| operation | requests | new entries |
|---|---|---|
| mount | ⌈*E*/100⌉ REST pages per scope, 8 in flight | – |
| `stat`, `readdir`, lookup of an existing name | none | – |
| lookup of a missing name | at most one re-list per 15 s (≥ 1 REST request per scope) | – |
| first read of a remote file | `GetCacheEntryDownloadURL`, then a ranged GET | – |
| later reads | a ranged GET per missing run of ≤ 8 MiB; readahead grows to 64 MiB | – |
| upload of a file ≤ 16 MiB | `CreateCacheEntry`, `Put Blob`, `FinalizeCacheEntryUpload` | 1 |
| upload of a larger file | the same, with 8 MiB blocks, 4 in flight, and `Put Block List` | 1 |
| empty directory, symlink, empty file | as a small file | 1 |
| `unlink`/`rmdir` of a remote entry | a whiteout, as a small file | 1 |
| `mv` of an uncached remote file | `EXDEV`, so `mv` copies: download, upload, whiteout | 2 |

Several consequences follow.

* **Small objects are bound by the rate limit, large ones by bandwidth.** At
  4.4 creations/s, an entry uses 0.23 s of the sustained budget. At 84 MB/s,
  0.23 s moves 19 MB. For the first ~200 entries of a burst, where 8 uploads
  of 3 round trips each finish ~10 files/s, the figure is 8 MB. Files smaller
  than that cost more budget than bandwidth. This holds for whiteouts and
  directory markers too.
* **The unmount timeout caps the backlog at about 16,000 entries.** `unmount`
  waits an hour by default for what is still pending. After that the post
  step fails, and the runner kills the daemon with its uploads unfinished.
  Uploads during the job (1 s after each close) keep the backlog down, but
  only while the job writes slower than the budget.
* **Reading *n* uncached small files takes 2*n* round trips.** Sibling
  prefetch overlaps six files at a time, so the ceiling is 6 / 0.5 s = 12
  files/s.
* **A cold random read of 4 KiB fetches its 1 MiB chunk**: one round trip
  plus 45 ms of transfer.
* **Mount time and REST cost grow with everything listed.** That includes
  every live file, every superseded version (until `--gc` or eviction), every
  whiteout, and every marker. The list therefore grows with a week of write
  history, not with the size of the tree.

## 4. Measurements

All results below come from an Apple M2 (8 cores) against the fake service in
the same process. The CPU numbers of a 4-vCPU hosted runner will be somewhat
higher.

### 4.1 Against the model of the hosted service

`gha-cache-fusefs bench --scale full`. A "–" under requests means the
step's own requests cannot be told apart from others in the same job.

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
already fetched. Before the fixes of §4.3 and §4.6, the throttled job's read
waited 26.5 s, and the sequential read made 201 requests.

### 4.2 Without delays or limits

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

Before the fixes of §4.4 and §4.6, writing the 20,000 files took 20.2 s,
and the cold sequential read made 969 requests in 233 ms.

### 4.3 A 429 paused every request in the job (fixed)

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

### 4.4 Writing *n* files cost O(*n*²) (fixed)

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

### 4.5 Reading small files is bound by round trips

`cp -r` of 1,000 remote 4 KiB files took 91 s (11 files/s) and made 2,000
requests. That is the ceiling from §3: sibling prefetch overlaps six files,
and each still needs a download URL and a GET. The files hold 4 MB in all,
which one GET moves in 0.4 s.

### 4.6 Large transfers left bandwidth unused

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
limit of about 45 MB/s. Without latency (§4.2), the read got slower, from
4.6 GB/s to about 3 GB/s. A chunk becomes readable only once its whole run
has landed, which matters only when requests cost nothing.

### 4.7 Mounting spends the REST budget

Mounting 20,000 entries took 7.0 s and 200 REST requests: a fifth of the
repository's hourly budget, per scope, per mount. On a pull request with
three scopes, three such mounts use the whole hour. Lookups of missing names
re-list at most every 15 s, which is up to 240 refreshes an hour per scope.
Compilers probing include paths, Python probing for modules, and `git`
looking for `.git` all look up missing names.

Two fixes spend a little more. First, listings are cut into pages by
position, so an entry deleted while a listing pages used to hide another at
a page boundary. A listing whose pages disagree about the total is now
repeated, up to three times. In a busy prefix, that can triple a mount's
REST requests. Second, a mount resolves a download URL, four at a time in
the background, for up to 1,000 stale markers, empty files, and whiteouts it
depends on (DESIGN.md §3.3). Those are Twirp lookups, not REST requests.

### 4.8 What is fine

* Building the tree takes 1.4 µs per listed entry of CPU, so a million
  entries would take a second or two.
* `ls -lR` costs about 1 µs per entry, with no requests.
* Cached reads run at local disk speed.

## 5. Levers

These are ordered by how much of the above they would remove.

* **Pack small objects into shared entries** (DESIGN.md §10). A pack is one
  entry holding many small files, symlinks, markers, and whiteouts, plus a
  manifest. Members take the pack's place in the ordering, so the overlay
  rules stay as they are. By the model, 1,000 files of 4 KiB in one pack
  upload in about a second instead of 201 s. `cp -r` of them needs one
  download URL and four 1 MiB ranges instead of 2,000 requests. `rm -r` is
  one entry. Listings and REST requests shrink by the pack size too.
* **Skip unchanged uploads.** Derive the version's nonce from the content and
  metadata instead of randomly. An upload whose version equals the current
  winner's is already in the cache, and so needs no request at all. That
  covers `cp -a`, `tar -x`, and `rsync` of mostly unchanged trees.
* **Upload 8 blocks at a time**, if the real service confirms §4.6.
* **Mark chunks present as a range streams in**, rather than when all of it
  has landed (§4.6).

## 6. Method

`gha-cache-fusefs bench` drives the filesystem core (`Vfs`), as the FUSE
adapter does, but without the kernel. Every "job" is a new mount with its own
data directory and HTTP client, as on a fresh runner. Nothing is uploaded
before a job unmounts, except in `throttled`. Each scenario gets its own fake
service, or its own prefix of the real cache.

| scenario | steps |
|---|---|
| `large` | write and upload one large file; read it sequentially cold, then cached; random 4 KiB reads cold |
| `small` | write and upload a tree of small files; mount it; `cp -r`, `ls -lR`, and `rm -r` it, each in a new job |
| `mount` | list and build a large tree; build ten times as large a tree from memory (fake service only) |
| `throttled` | a burst of uploads runs into the rate limit; meanwhile, the job and another job each read a small file |

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
environment), the benchmark works below `bench/<run>/`. Afterwards it deletes
the entries larger than 1 MiB. It leaves the small ones to expire, because
deleting each would cost a REST request.
