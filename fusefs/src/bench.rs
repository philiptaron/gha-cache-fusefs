//! Benchmarks: workloads driven through the filesystem core (not the kernel)
//! against the fake service, with or without a model of the real one, or
//! against the cache of an Actions job. PERFORMANCE.md explains the scenarios
//! and what their results mean.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail, ensure};
use bytes::Bytes;
use serde::Serialize;

use crate::api::{Api, Requests};
use crate::config::Env;
use crate::data::DataStore;
use crate::entry::{Kind, Volume};
use crate::fake::{FakeConfig, FakeServer};
use crate::index::{self, Index, Layer, Listed};
use crate::vfs::{Attr, DirEntry, FileKind, Ino, ROOT, Summary, Vfs, VfsConfig};

const MAIN: &str = "refs/heads/main";
/// The size of the kernel's read and write requests.
const IO: usize = 128 << 10;

pub const SCENARIOS: [&str; 4] = ["large", "small", "mount", "throttled"];

/// What the benchmarks run against.
pub enum Service {
    /// A fresh fake service for each scenario.
    Fake(FakeConfig),
    /// The cache of the current Actions job, in volumes named `<volume>-<scenario>`.
    Real { env: Env, volume: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct Sizes {
    /// One large file, in MiB.
    pub large_mib: u64,
    /// Cold 4 KiB reads at random offsets of the large file.
    pub random_reads: usize,
    /// Small files of `file_size` bytes, spread over `dirs` directories.
    pub files: usize,
    pub file_size: usize,
    pub dirs: usize,
    /// Files in the volume that `mount` mounts, in layers of `LAYER_FILES`
    /// (the fake service only).
    pub entries: usize,
    /// Files written and fsynced one at a time, to run into the rate limit.
    pub burst: usize,
}

impl Sizes {
    pub fn quick() -> Sizes {
        Sizes {
            large_mib: 64,
            random_reads: 50,
            files: 300,
            file_size: 4096,
            dirs: 10,
            entries: 2_000,
            burst: 250,
        }
    }

    pub fn full() -> Sizes {
        Sizes {
            large_mib: 256,
            random_reads: 200,
            files: 1_000,
            file_size: 4096,
            dirs: 20,
            entries: 20_000,
            burst: 300,
        }
    }

    /// The sizes, for a report.
    pub fn describe(&self) -> String {
        format!(
            "A {} MiB file and {} random reads of it; {} files of {} in {} directories; \
             {} files in layers of {}; a burst of {} fsynced files",
            self.large_mib,
            self.random_reads,
            self.files,
            size(self.file_size as u64),
            self.dirs,
            self.entries,
            LAYER_FILES,
            self.burst
        )
    }
}

/// One measured step.
#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub scenario: &'static str,
    pub step: String,
    pub seconds: f64,
    pub result: String,
    /// `None` if the step's own requests cannot be told apart from others.
    pub requests: Option<Requests>,
    pub rate_limited: u64,
    pub rate_limit_pause_ms: u64,
}

/// Runs the scenarios in `only` (all if empty), printing each step to stderr
/// as it finishes.
pub async fn run(service: &Service, sizes: &Sizes, only: &[String]) -> anyhow::Result<Vec<Row>> {
    for name in only {
        ensure!(
            SCENARIOS.contains(&name.as_str()),
            "unknown scenario {name:?}; there are {SCENARIOS:?}"
        );
    }
    let scratch =
        std::env::temp_dir().join(format!("gha-cache-fusefs-bench-{}", std::process::id()));
    let mut rows = Vec::new();
    let result = async {
        for name in SCENARIOS {
            if !only.is_empty() && !only.iter().any(|o| o == name) {
                continue;
            }
            let mut cache = Cache::new(service, name, scratch.join(name)).await?;
            let mut out = Out {
                rows: &mut rows,
                scenario: name,
            };
            match name {
                "large" => large(&mut cache, sizes, &mut out).await,
                "small" => small(&mut cache, sizes, &mut out).await,
                "mount" => mount(&mut cache, sizes, &mut out).await,
                _ => throttled(&mut cache, sizes, &mut out).await,
            }
            .with_context(|| format!("scenario {name}"))?;
        }
        anyhow::Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&scratch);
    if let Service::Real { env, volume } = service {
        for name in SCENARIOS {
            cleanup(env, &format!("{volume}-{name}")).await;
        }
    }
    result.map(|()| rows)
}

/// Where one scenario's jobs mount: a fresh fake service, or a fresh volume
/// of the real cache.
struct Cache {
    env: Env,
    volume: String,
    fake: Option<FakeServer>,
    /// Whether creating entries is rate limited.
    limited: bool,
    scratch: PathBuf,
    jobs: usize,
}

/// What a job is made of.
type Parts = (VfsConfig, Api, Arc<DataStore>);

impl Cache {
    async fn new(service: &Service, scenario: &str, scratch: PathBuf) -> anyhow::Result<Cache> {
        let (env, volume, fake, limited) = match service {
            Service::Fake(cfg) => {
                let fake = FakeServer::start(cfg.clone()).await?;
                let env = fake.env(MAIN, &[]);
                (
                    env,
                    "default".into(),
                    Some(fake),
                    cfg.create_limit.is_some(),
                )
            }
            Service::Real { env, volume } => {
                (env.clone(), format!("{volume}-{scenario}"), None, true)
            }
        };
        Ok(Cache {
            env,
            volume,
            fake,
            limited,
            scratch,
            jobs: 0,
        })
    }

    /// A new mount, as a new job on a fresh runner would make it. Nothing is
    /// uploaded before `drain`, unless `settle` is changed.
    async fn job_with(&mut self, tweak: impl FnOnce(&mut VfsConfig)) -> anyhow::Result<Vfs> {
        let (mut cfg, api, store) = self.parts()?;
        tweak(&mut cfg);
        self.load((cfg, api, store)).await
    }

    async fn job(&mut self) -> anyhow::Result<Vfs> {
        self.job_with(|_| {}).await
    }

    /// A job's parts, with a data directory of its own. Setting up the TLS
    /// client (Api) takes a while and is not what mount timings measure.
    fn parts(&mut self) -> anyhow::Result<Parts> {
        self.jobs += 1;
        let store = DataStore::new(&self.scratch.join(format!("job{}", self.jobs)), 8 << 30, 16)?;
        let mut cfg = VfsConfig::new(Volume::new(&self.volume).map_err(anyhow::Error::msg)?);
        cfg.settle = Duration::from_secs(3600);
        Ok((cfg, Api::new(&self.env)?, store))
    }

    /// Lists the cache and builds the tree.
    async fn load(&self, (cfg, api, store): Parts) -> anyhow::Result<Vfs> {
        Vfs::load(cfg, api, store, self.env.scopes()).await
    }
}

/// Collects a scenario's rows.
struct Out<'a> {
    rows: &'a mut Vec<Row>,
    scenario: &'static str,
}

impl Out<'_> {
    fn push(&mut self, m: &Measured, step: impl Into<String>, result: impl Into<String>) {
        let row = Row {
            scenario: self.scenario,
            step: step.into(),
            seconds: m.elapsed.as_secs_f64(),
            result: result.into(),
            requests: m.requests,
            rate_limited: m.rate_limited,
            rate_limit_pause_ms: m.pause_ms,
        };
        eprintln!(
            "bench: {} / {}: {} ({})",
            row.scenario,
            row.step,
            secs(m.elapsed),
            row.result
        );
        self.rows.push(row);
    }
}

/// Measures a step: its wall time, and what one job did during it.
struct Meter {
    started: Instant,
    before: Summary,
}

struct Measured {
    elapsed: Duration,
    requests: Option<Requests>,
    rate_limited: u64,
    pause_ms: u64,
}

impl Measured {
    /// Just the time.
    fn time(elapsed: Duration) -> Measured {
        Measured {
            elapsed,
            requests: None,
            rate_limited: 0,
            pause_ms: 0,
        }
    }
}

impl Meter {
    fn start(before: Summary) -> Meter {
        Meter {
            started: Instant::now(),
            before,
        }
    }

    fn stop(&self, after: &Summary) -> Measured {
        let (a, b) = (&after.requests, &self.before.requests);
        Measured {
            elapsed: self.started.elapsed(),
            requests: Some(Requests {
                cache_service: a.cache_service - b.cache_service,
                blob: a.blob - b.blob,
                rest: a.rest - b.rest,
            }),
            rate_limited: after.rate_limited - self.before.rate_limited,
            pause_ms: after.rate_limit_pause_ms - self.before.rate_limit_pause_ms,
        }
    }
}

// ---- scenarios ----------------------------------------------------------

/// One large file: upload, sequential reads, and random reads.
async fn large(cache: &mut Cache, sizes: &Sizes, out: &mut Out<'_>) -> anyhow::Result<()> {
    let bytes = sizes.large_mib << 20;
    let a = cache.job().await?;
    let m = Meter::start(a.summary());
    write_file(&a, ROOT, "large", bytes as usize, 1)?;
    let w = m.stop(&a.summary());
    out.push(
        &w,
        format!("write {} MiB (local)", sizes.large_mib),
        rate(bytes, w.elapsed),
    );
    let m = Meter::start(a.summary());
    let u = m.stop(&drained(&a).await?);
    out.push(&u, "upload it (unmount)", rate(bytes, u.elapsed));

    let b = cache.job().await?;
    let ino = lookup(&b, "large").await?.ino;
    let m = Meter::start(b.summary());
    let (n, first) = read_all(&b, ino).await?;
    ensure!(n == bytes, "read {n} of {bytes} bytes");
    let r = m.stop(&b.summary());
    out.push(
        &r,
        "read it sequentially, cold",
        format!("{}, first byte after {}", rate(n, r.elapsed), secs(first)),
    );
    let m = Meter::start(b.summary());
    read_all(&b, ino).await?;
    let r = m.stop(&b.summary());
    out.push(&r, "read it again, cached", rate(n, r.elapsed));

    let c = cache.job().await?;
    let ino = lookup(&c, "large").await?.ino;
    let (fh, _) = c.open(ino, libc::O_RDONLY).await?;
    let m = Meter::start(c.summary());
    let mut latencies = Vec::new();
    let mut x = 0x5eed;
    for _ in 0..sizes.random_reads {
        x = xorshift(x);
        let offset = (x % (bytes - 4096)) & !4095;
        let t = Instant::now();
        let got = c.read(fh, offset, 4096).await?;
        ensure!(got.len() == 4096, "short read at {offset}");
        latencies.push(t.elapsed());
    }
    c.release(fh)?;
    let r = m.stop(&c.summary());
    out.push(
        &r,
        format!("{} random 4 KiB reads, cold", sizes.random_reads),
        percentiles(&mut latencies),
    );
    Ok(())
}

/// A tree of small files: upload, mount, `cp -r`, `ls -lR`, and `rm -r`.
async fn small(cache: &mut Cache, sizes: &Sizes, out: &mut Out<'_>) -> anyhow::Result<()> {
    let n = sizes.files;
    let a = cache.job().await?;
    let m = Meter::start(a.summary());
    let top = a.mkdir(ROOT, "small", 0o755)?.ino;
    let mut dirs = Vec::new();
    for d in 0..sizes.dirs.max(1) {
        dirs.push(a.mkdir(top, &format!("d{d:03}"), 0o755)?.ino);
    }
    for i in 0..n {
        let dir = dirs[i % dirs.len()];
        write_file(&a, dir, &format!("f{i:05}"), sizes.file_size, i as u64)?;
    }
    let w = m.stop(&a.summary());
    out.push(
        &w,
        format!(
            "write {n} files of {} (local)",
            size(sizes.file_size as u64)
        ),
        per_sec(n, w.elapsed, "files"),
    );
    let m = Meter::start(a.summary());
    let u = m.stop(&drained(&a).await?);
    out.push(&u, "upload them (unmount)", per_sec(n, u.elapsed, "files"));

    let parts = cache.parts()?;
    let m = Meter::start(Summary::default());
    let b = cache.load(parts).await?;
    let l = m.stop(&b.summary());
    out.push(&l, format!("mount, stacking {n} files"), "");
    let m = Meter::start(b.summary());
    let files = read_tree(&b, lookup(&b, "small").await?.ino).await?;
    ensure!(files == n, "read {files} of {n} files");
    let r = m.stop(&b.summary());
    out.push(
        &r,
        "read them all, cold (cp -r)",
        per_sec(n, r.elapsed, "files"),
    );

    let c = cache.job().await?;
    let m = Meter::start(c.summary());
    let entries = stat_tree(&c, lookup(&c, "small").await?.ino).await?;
    let s = m.stop(&c.summary());
    out.push(
        &s,
        "stat them all (ls -lR)",
        format!(
            "{:.1} µs per entry",
            s.elapsed.as_secs_f64() * 1e6 / entries as f64
        ),
    );

    let d = cache.job().await?;
    let m = Meter::start(d.summary());
    remove_tree(&d, ROOT, "small").await?;
    let summary = drained(&d).await?;
    let r = m.stop(&summary);
    out.push(
        &r,
        "remove them (rm -r, unmount)",
        per_sec(summary.whiteouts as usize, r.elapsed, "whiteouts"),
    );
    Ok(())
}

/// Files per layer in the volume `mount` mounts: about what one batch of
/// `cp -r` makes.
const LAYER_FILES: usize = 1_000;

/// Layers holding `n` one-byte files, as `(image, version)`.
fn layers_of(n: usize) -> anyhow::Result<Vec<(Vec<u8>, crate::entry::Version)>> {
    (0..n.div_ceil(LAYER_FILES))
        .map(|l| {
            let files: Vec<(String, Vec<u8>)> = (l * LAYER_FILES..n.min((l + 1) * LAYER_FILES))
                .map(|i| (entry_path(i), b"x".to_vec()))
                .collect();
            Ok(index::layer_image(&files)?)
        })
        .collect()
}

/// Mounting a volume of many files in many layers, and the CPU cost of
/// stacking them.
async fn mount(cache: &mut Cache, sizes: &Sizes, out: &mut Out<'_>) -> anyhow::Result<()> {
    let n = sizes.entries;
    let Some(fake) = &cache.fake else {
        eprintln!("bench: mount: skipped; it needs the fake service to create entries quickly");
        return Ok(());
    };
    let volume = Volume::new(&cache.volume).map_err(anyhow::Error::msg)?;
    let layers = layers_of(n)?;
    let count = layers.len();
    for (image, version) in layers {
        fake.insert(
            &volume.layer_key(version.nonce),
            &version.encode(),
            MAIN,
            Bytes::from(image),
        );
    }
    let parts = cache.parts()?;
    let m = Meter::start(Summary::default());
    let a = cache.load(parts).await?;
    let l = m.stop(&a.summary());
    out.push(
        &l,
        format!("list {count} layers, read their metadata, and build the tree of {n} files"),
        per_sec(n, l.elapsed, "files"),
    );

    // The same, ten times larger, from memory: the CPU cost alone.
    let big = n * 10;
    let parsed: Vec<Layer> = layers_of(big)?
        .into_iter()
        .enumerate()
        .map(|(i, (image, version))| {
            let Kind::Layer { meta_blocks } = version.kind else {
                unreachable!("a layer")
            };
            let now = SystemTime::now();
            let entry = Listed {
                key: volume.layer_key(version.nonce),
                version: version.encode(),
                size: image.len() as u64,
                created: now,
                accessed: now,
                id: i as i64 + 1,
                scope: 0,
            };
            Ok(Layer::parse(entry, meta_blocks, &image)?)
        })
        .collect::<anyhow::Result<_>>()?;
    let (cfg, api, store) = cache.parts()?;
    let t = Instant::now();
    let mut ix = Index::new(volume.clone(), vec![MAIN.into()]);
    for layer in parsed {
        ix.insert_layer(layer);
    }
    ix.restack();
    let vfs = Vfs::with_index(cfg, api, store, ix);
    let elapsed = t.elapsed();
    ensure!(!list(&vfs, ROOT)?.is_empty(), "the tree is empty");
    out.push(
        &Measured::time(elapsed),
        format!(
            "stack {big} files in {} layers and build the tree (CPU only)",
            big.div_ceil(LAYER_FILES)
        ),
        format!(
            "{:.1} µs per file",
            elapsed.as_secs_f64() * 1e6 / big as f64
        ),
    );
    Ok(())
}

/// Jobs that write and fsync files at once in `throttled`.
const WRITERS: usize = 6;

/// Writes and fsyncs `n` files one at a time, each a layer of its own.
async fn write_synced(job: Vfs, dir: Ino, first: usize, n: usize) -> anyhow::Result<()> {
    for i in first..first + n {
        let name = format!("f{i:04}");
        let (_, fh) = job.create(dir, &name, 0o644, libc::O_WRONLY | libc::O_CREAT)?;
        job.write(fh, 0, &noise(64, i as u64))?;
        job.fsync(fh).await?;
        job.release(fh)?;
    }
    Ok(())
}

/// Files written and fsynced one at a time make a layer each. Several jobs
/// doing that at once run into the rate limit; meanwhile, one of them reads
/// a file it does not have yet, and so does another job.
async fn throttled(cache: &mut Cache, sizes: &Sizes, out: &mut Out<'_>) -> anyhow::Result<()> {
    if !cache.limited {
        eprintln!("bench: throttled: skipped; this service has no rate limit");
        return Ok(());
    }
    let a = cache.job().await?;
    write_file(&a, ROOT, "target", 4096, 3)?;
    a.mkdir(ROOT, "burst", 0o755)?;
    drained(&a).await?;

    let mut writers = Vec::new();
    for _ in 0..WRITERS {
        writers.push(cache.job_with(|c| c.settle = Duration::ZERO).await?);
    }
    let other = cache.job().await?;
    let mut handles = Vec::new();
    for job in [&writers[0], &other] {
        let ino = lookup(job, "target").await?.ino;
        handles.push(job.open(ino, libc::O_RDONLY).await?.0);
    }
    let summaries = |writers: &[Vfs]| writers.iter().map(Vfs::summary).collect::<Vec<_>>();
    let limited = |writers: &[Vfs]| {
        writers
            .iter()
            .map(|w| w.summary().rate_limited)
            .sum::<u64>()
    };
    let m = Meter::start(writers[0].summary());
    let per = sizes.burst.div_ceil(WRITERS);
    let mut writing = Vec::new();
    for (n, w) in writers.iter().enumerate() {
        let dir = lookup(w, "burst").await?.ino;
        writing.push(tokio::spawn(write_synced(w.clone(), dir, n * per, per)));
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    while limited(&writers) == 0 {
        if Instant::now() > deadline || writing.iter().all(|t| t.is_finished()) {
            let w = m.stop(&writers[0].summary());
            out.push(
                &w,
                format!("write and fsync {} files in {WRITERS} jobs", per * WRITERS),
                "never rate limited",
            );
            for t in writing {
                t.await??;
            }
            for w in &writers {
                drained(w).await?;
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let uploaded: u64 = summaries(&writers).iter().map(|s| s.layers).sum();
    let w = m.stop(&writers[0].summary());
    out.push(
        &w,
        format!("write and fsync files in {WRITERS} jobs until the first 429"),
        format!("{uploaded} layers"),
    );

    let timed = |job: &Vfs, fh: u64| {
        let job = job.clone();
        async move {
            let t = Instant::now();
            let got = job.read(fh, 0, 4096).await?;
            ensure!(got.len() == 4096, "short read");
            anyhow::Ok(t.elapsed())
        }
    };
    // The other job does nothing else, so its counters are the read's; the
    // writer's also count its uploads.
    let m = Meter::start(other.summary());
    let (same, elsewhere) = tokio::join!(timed(&writers[0], handles[0]), timed(&other, handles[1]));
    out.push(
        &Measured::time(same?),
        "a cold 4 KiB read in one of them",
        "",
    );
    let r = Measured {
        elapsed: elsewhere?,
        ..m.stop(&other.summary())
    };
    out.push(&r, "the same read in another job", "");

    let m = Meter::start(writers[0].summary());
    for t in writing {
        t.await??;
    }
    for w in &writers[1..] {
        drained(w).await?;
    }
    let d = m.stop(&drained(&writers[0]).await?);
    out.push(
        &d,
        "write and fsync the rest, and unmount",
        format!("{} files in all", per * WRITERS),
    );
    Ok(())
}

/// Deletes what a benchmark against the real cache left behind, except
/// small entries: each deletion is a REST request, and the repository's
/// `GITHUB_TOKEN` budget (1,000 an hour) is worth more than a few MB of
/// quota that eviction frees within a week.
async fn cleanup(env: &Env, volume: &str) {
    let prefix = format!("gha-fs/{volume}/");
    let result = async {
        let api = Api::new(env)?;
        let items = index::list_all(&api.rest, &prefix, std::slice::from_ref(&env.git_ref)).await?;
        let mut deleted = 0;
        for item in items.iter().filter(|i| i.size_in_bytes >= 1 << 20) {
            api.rest.delete(item.id).await?;
            deleted += 1;
        }
        anyhow::Ok((deleted, items.len()))
    }
    .await;
    match result {
        Ok((deleted, of)) => {
            eprintln!(
                "bench: deleted {deleted} of {of} entries below {prefix}; eviction takes the rest"
            )
        }
        Err(e) => eprintln!("bench: cleaning up below {prefix} failed: {e:#}"),
    }
}

// ---- filesystem helpers ---------------------------------------------------

async fn drained(vfs: &Vfs) -> anyhow::Result<Summary> {
    let summary = vfs.drain().await;
    if let Some(f) = summary.failures.first() {
        bail!(
            "{} changes were not saved, such as {}: {}",
            summary.failures.len(),
            f.key,
            f.error
        );
    }
    Ok(summary)
}

async fn lookup(vfs: &Vfs, path: &str) -> anyhow::Result<Attr> {
    let mut attr = vfs.getattr(ROOT)?;
    for part in path.split('/') {
        attr = vfs.lookup(attr.ino, part).await.context(path.to_string())?;
    }
    Ok(attr)
}

fn list(vfs: &Vfs, dir: Ino) -> anyhow::Result<Vec<DirEntry>> {
    let fh = vfs.opendir(dir)?;
    let entries = vfs.readdir(dir, fh, 0);
    vfs.releasedir(fh);
    Ok(entries?
        .into_iter()
        .filter(|e| e.name != "." && e.name != "..")
        .collect())
}

/// Writes `len` bytes in the kernel's request size.
fn write_file(vfs: &Vfs, dir: Ino, name: &str, len: usize, seed: u64) -> anyhow::Result<Ino> {
    let (attr, fh) = vfs.create(dir, name, 0o644, libc::O_WRONLY | libc::O_CREAT)?;
    // One pattern, repeated: nothing between here and the blob compresses.
    let pattern = noise(len.min(1 << 20), seed);
    let mut offset = 0;
    while offset < len {
        let start = offset % pattern.len();
        let n = IO.min(len - offset).min(pattern.len() - start);
        vfs.write(fh, offset as u64, &pattern[start..start + n])?;
        offset += n;
    }
    vfs.release(fh)?;
    Ok(attr.ino)
}

/// Reads a file in the kernel's request size; returns its length and the
/// time until the first byte.
async fn read_all(vfs: &Vfs, ino: Ino) -> anyhow::Result<(u64, Duration)> {
    let t = Instant::now();
    let (fh, _) = vfs.open(ino, libc::O_RDONLY).await?;
    let mut offset = 0;
    let mut first = None;
    loop {
        let chunk = vfs.read(fh, offset, IO as u32).await?;
        first.get_or_insert_with(|| t.elapsed());
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as u64;
    }
    vfs.release(fh)?;
    Ok((offset, first.unwrap_or_default()))
}

/// Reads every file below `dir`, one at a time, as `cp -r` does.
async fn read_tree(vfs: &Vfs, dir: Ino) -> anyhow::Result<usize> {
    let mut files = 0;
    let mut dirs = vec![dir];
    while let Some(dir) = dirs.pop() {
        for e in list(vfs, dir)? {
            let attr = vfs.lookup(dir, &e.name).await?;
            match attr.kind {
                FileKind::Dir => dirs.push(attr.ino),
                FileKind::File => {
                    read_all(vfs, attr.ino).await?;
                    files += 1;
                }
                FileKind::Symlink => {}
            }
        }
    }
    Ok(files)
}

/// Looks up and stats everything below `dir`, as `ls -lR` does.
async fn stat_tree(vfs: &Vfs, dir: Ino) -> anyhow::Result<usize> {
    let mut entries = 0;
    let mut dirs = vec![dir];
    while let Some(dir) = dirs.pop() {
        for e in list(vfs, dir)? {
            let attr = vfs.lookup(dir, &e.name).await?;
            vfs.getattr(attr.ino)?;
            entries += 1;
            if attr.kind == FileKind::Dir {
                dirs.push(attr.ino);
            }
        }
    }
    Ok(entries)
}

/// Removes `name` in `parent` and everything below it, as `rm -r` does.
async fn remove_tree(vfs: &Vfs, parent: Ino, name: &str) -> anyhow::Result<()> {
    // Every node in pre-order; removing in reverse removes children first.
    let mut nodes = vec![(parent, name.to_string(), FileKind::Dir)];
    let mut i = 0;
    while i < nodes.len() {
        let (p, n, kind) = nodes[i].clone();
        if kind == FileKind::Dir {
            let ino = vfs.lookup(p, &n).await?.ino;
            nodes.extend(list(vfs, ino)?.into_iter().map(|e| (ino, e.name, e.kind)));
        }
        i += 1;
    }
    for (p, n, kind) in nodes.into_iter().rev() {
        match kind {
            FileKind::Dir => vfs.rmdir(p, &n)?,
            _ => vfs.unlink(p, &n)?,
        }
    }
    Ok(())
}

// ---- data and formatting --------------------------------------------------

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x = xorshift(x);
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A path for the `i`th of many entries: a hundred directories of files.
fn entry_path(i: usize) -> String {
    format!("d{:02}/f{i:07}", i % 100)
}

fn secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 100.0 {
        format!("{s:.0} s")
    } else if s >= 1.0 {
        format!("{s:.1} s")
    } else if s >= 0.01 {
        format!("{:.0} ms", s * 1e3)
    } else {
        format!("{:.2} ms", s * 1e3)
    }
}

fn rate(bytes: u64, d: Duration) -> String {
    format!("{:.1} MB/s", bytes as f64 / d.as_secs_f64() / 1e6)
}

fn per_sec(n: usize, d: Duration, what: &str) -> String {
    format!("{:.1} {what}/s", n as f64 / d.as_secs_f64())
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 && b % (1 << 20) == 0 => format!("{} MiB", b >> 20),
        b if b >= 1 << 10 && b % (1 << 10) == 0 => format!("{} KiB", b >> 10),
        b => format!("{b} B"),
    }
}

fn percentiles(latencies: &mut [Duration]) -> String {
    if latencies.is_empty() {
        return String::new();
    }
    latencies.sort();
    let at = |q: f64| secs(latencies[((latencies.len() - 1) as f64 * q).round() as usize]);
    format!("p50 {}, p90 {}, max {}", at(0.5), at(0.9), at(1.0))
}

/// The rows as a Markdown table.
pub fn markdown(rows: &[Row]) -> String {
    let mut s = String::from(
        "| scenario | step | time | result | requests: cache service / blob / REST | rate limited |\n\
         |---|---|---:|---|---:|---:|\n",
    );
    for r in rows {
        let requests = match &r.requests {
            Some(q) => format!("{} / {} / {}", q.cache_service, q.blob, q.rest),
            None => "–".to_string(),
        };
        let limited = if r.rate_limited == 0 {
            "–".to_string()
        } else {
            format!(
                "{} × 429, paused {}",
                r.rate_limited,
                secs(Duration::from_millis(r.rate_limit_pause_ms))
            )
        };
        s.push_str(&format!(
            "| {} | {} | {} | {} | {requests} | {limited} |\n",
            r.scenario,
            r.step,
            secs(Duration::from_secs_f64(r.seconds)),
            r.result,
        ));
    }
    s
}
