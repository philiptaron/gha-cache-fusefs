use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use sha2::{Digest, Sha256};

use gha_cache_fusefs::api::Api;
use gha_cache_fusefs::bench;
use gha_cache_fusefs::config::Env;
use gha_cache_fusefs::data::DataStore;
use gha_cache_fusefs::entry::Volume;
use gha_cache_fusefs::fake::{FakeConfig, FakeServer};
use gha_cache_fusefs::vfs::{Summary, Vfs, VfsConfig};

#[derive(Parser)]
#[command(version, about = "Mount the GitHub Actions cache as a FUSE filesystem")]
struct Cli {
    /// Log filter, e.g. `info` or `gha_cache_fusefs=debug`.
    #[arg(
        long,
        global = true,
        env = "GHA_CACHE_FUSEFS_LOG",
        default_value = "info"
    )]
    log: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount the cache and serve it until unmounted.
    Mount(MountArgs),
    /// Unmount, wait for pending uploads to finish, and report what happened.
    Unmount(UnmountArgs),
    /// Run a fake cache service, for tests.
    #[command(hide = true)]
    FakeServer(FakeArgs),
    /// Measure the filesystem core against the fake service or the real cache
    /// (see PERFORMANCE.md).
    #[command(hide = true)]
    Bench(BenchArgs),
}

#[derive(Args)]
struct MountArgs {
    mountpoint: PathBuf,
    /// The volume to mount: its entries are the keys under `gha-fs/<volume>/`.
    #[arg(long, env = "GHA_CACHE_FUSEFS_VOLUME", default_value = "default")]
    volume: String,
    /// The directory of the volume to show at the mountpoint, such as `a/b`.
    #[arg(long, env = "GHA_CACHE_FUSEFS_ROOT", default_value = "")]
    root: String,
    /// Where the log, lock, summary, and cached data live.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    read_only: bool,
    /// Detach once the filesystem is mounted.
    #[arg(long)]
    daemon: bool,
    /// How long a closed file waits before it is uploaded.
    #[arg(long, default_value = "1s", value_parser = humantime::parse_duration)]
    settle: Duration,
    /// Minimum interval between re-listings triggered by lookup misses (0 disables).
    #[arg(long, default_value = "15s", value_parser = humantime::parse_duration)]
    refresh: Duration,
    /// Local disk budget for cached remote content, in MiB.
    #[arg(long, default_value_t = 8192)]
    cache_size_mb: u64,
    #[arg(long, default_value_t = 8)]
    upload_concurrency: usize,
    #[arg(long, default_value_t = 16)]
    download_concurrency: usize,
    /// Let other users access the mount (needs `user_allow_other` or root).
    #[arg(long)]
    allow_other: bool,
    /// The environment variable holding the REST API token.
    #[arg(long, default_value = "GITHUB_TOKEN")]
    token_env: String,
    /// FUSE request loops.
    #[arg(long, default_value_t = 2)]
    threads: usize,
}

#[derive(Args)]
struct UnmountArgs {
    mountpoint: PathBuf,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// How long to wait for pending uploads.
    #[arg(long, default_value = "1h", value_parser = humantime::parse_duration)]
    timeout: Duration,
}

#[derive(Args)]
struct FakeArgs {
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: std::net::SocketAddr,
    /// Write the Actions environment for this server to a file (KEY=VALUE lines).
    #[arg(long)]
    env_file: Option<PathBuf>,
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
    url_ttl: Duration,
    #[arg(long)]
    fail_blob_every: Option<u64>,
    /// `hosted` adds the real service's latency, bandwidth, and rate limit.
    #[arg(long, value_enum, default_value_t = Profile::Local)]
    profile: Profile,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Profile {
    /// Answer at once, as fast as possible, with no limits.
    Local,
    /// Behave like the real service seen from a hosted runner.
    Hosted,
}

impl Profile {
    fn config(self) -> FakeConfig {
        match self {
            Profile::Local => FakeConfig::default(),
            Profile::Hosted => FakeConfig::hosted(),
        }
    }
}

#[derive(Args)]
struct BenchArgs {
    /// The fake service with the real one's latency, bandwidth, and rate
    /// limit (`hosted`) or without them (`local`), or the cache of the
    /// current Actions job (`real`).
    #[arg(long, value_enum, default_value_t = BenchService::Hosted)]
    service: BenchService,
    #[arg(long, value_enum, default_value_t = Scale::Quick)]
    scale: Scale,
    /// Scenarios to run, comma-separated: large, small, mount, throttled.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// The number of small files.
    #[arg(long)]
    files: Option<usize>,
    /// The size of the large file, in MiB.
    #[arg(long)]
    large_mib: Option<u64>,
    /// The number of entries in the listing that `mount` mounts.
    #[arg(long)]
    entries: Option<usize>,
    #[arg(long)]
    random_reads: Option<usize>,
    /// With `--service real`: the start of the volume names to use; the
    /// run and the scenario are added.
    #[arg(long, default_value = "bench")]
    volume: String,
    /// Also write the results as JSON.
    #[arg(long)]
    json: Option<PathBuf>,
    /// The environment variable holding the REST API token.
    #[arg(long, default_value = "GITHUB_TOKEN")]
    token_env: String,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BenchService {
    Hosted,
    Local,
    Real,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Scale {
    /// A few minutes against the hosted model.
    Quick,
    /// Ten minutes or so.
    Full,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Mount(args) => mount(args, &cli.log),
        Cmd::Unmount(args) => unmount(args),
        Cmd::FakeServer(args) => fake_server(args, &cli.log),
        Cmd::Bench(args) => bench(args, &cli.log),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("gha-cache-fusefs: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(filter: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

/// A per-mountpoint directory, so that `unmount` finds what `mount` wrote.
fn default_state_dir(mountpoint: &Path) -> PathBuf {
    let base = ["RUNNER_TEMP", "XDG_RUNTIME_DIR", "TMPDIR"]
        .iter()
        .find_map(|v| std::env::var_os(v).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let abs = std::path::absolute(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    let abs: PathBuf = abs.components().collect();
    let hash = hex::encode(&Sha256::digest(abs.as_os_str().as_encoded_bytes())[..6]);
    let leaf = abs
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect::<String>()
        })
        .unwrap_or_default();
    base.join("gha-cache-fusefs").join(format!("{leaf}-{hash}"))
}

/// Tells the parent of a daemonized mount whether mounting worked.
struct Ready(Option<OwnedFd>);

impl Ready {
    fn send(&mut self, msg: &str) {
        if let Some(fd) = self.0.take() {
            let mut f = File::from(fd);
            let _ = f.write_all(msg.as_bytes());
        }
    }
}

/// Forks; the parent exits once the child reports readiness. Must run
/// before any threads exist.
fn daemonize(log: &Path) -> anyhow::Result<Ready> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!("pipe: {}", std::io::Error::last_os_error());
    }
    // Children such as fusermount3 must not inherit the pipe, or the parent
    // would wait for them too.
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    match unsafe { libc::fork() } {
        -1 => bail!("fork: {}", std::io::Error::last_os_error()),
        0 => {
            unsafe {
                libc::close(fds[0]);
                libc::setsid();
                // Do not keep whatever directory we started in busy.
                libc::chdir(c"/".as_ptr());
            }
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .with_context(|| format!("opening {}", log.display()))?;
            let null = File::open("/dev/null")?;
            unsafe {
                libc::dup2(null.as_raw_fd(), 0);
                libc::dup2(log.as_raw_fd(), 1);
                libc::dup2(log.as_raw_fd(), 2);
            }
            Ok(Ready(Some(unsafe { OwnedFd::from_raw_fd(fds[1]) })))
        }
        _ => {
            unsafe { libc::close(fds[1]) };
            let mut msg = String::new();
            let _ = unsafe { File::from_raw_fd(fds[0]) }.read_to_string(&mut msg);
            match msg.strip_prefix("ok ") {
                Some(info) => {
                    println!("{}", info.trim());
                    std::process::exit(0);
                }
                None => {
                    let msg = if msg.is_empty() {
                        "the daemon exited unexpectedly".into()
                    } else {
                        msg
                    };
                    eprintln!("gha-cache-fusefs: mount failed: {}", msg.trim());
                    eprintln!("gha-cache-fusefs: see {}", log.display());
                    std::process::exit(1);
                }
            }
        }
    }
}

/// An exclusive lock held for the daemon's lifetime; `unmount` waits on it.
fn try_lock(path: &Path) -> anyhow::Result<Option<File>> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(Some(f))
    } else {
        Ok(None)
    }
}

fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(tmp, path)
}

fn mount(mut args: MountArgs, log: &str) -> anyhow::Result<ExitCode> {
    // The daemon changes to /, so relative paths must be resolved first.
    args.mountpoint = std::path::absolute(&args.mountpoint)?;
    let state_dir = match &args.state_dir {
        Some(dir) => std::path::absolute(dir)?,
        None => default_state_dir(&args.mountpoint),
    };
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    let mut ready = if args.daemon {
        daemonize(&state_dir.join("daemon.log"))?
    } else {
        Ready(None)
    };
    init_logging(log);
    match serve(&args, &state_dir, &mut ready) {
        Ok(code) => Ok(code),
        Err(e) => {
            tracing::error!("{e:#}");
            ready.send(&format!("{e:#}"));
            Err(e)
        }
    }
}

fn serve(args: &MountArgs, state_dir: &Path, ready: &mut Ready) -> anyhow::Result<ExitCode> {
    let _lock = try_lock(&state_dir.join("lock"))?.with_context(|| {
        format!(
            "{} is already in use by another daemon",
            state_dir.display()
        )
    })?;
    std::fs::write(state_dir.join("pid"), std::process::id().to_string())?;
    let _ = std::fs::remove_file(state_dir.join("summary.json"));

    let mut env = Env::from_process(&args.token_env)?;
    env.check_mode()?;
    if std::env::var_os("ACTIONS_CACHE_SERVICE_V2").is_none() {
        tracing::warn!(
            "ACTIONS_CACHE_SERVICE_V2 is not set; only the v2 cache service (github.com) is supported"
        );
    }
    let read_only = args.read_only || !env.cache_mode.writable();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let signals = signals(&rt)?;
    let started = Instant::now();
    let vfs = rt.block_on(async {
        let api = Api::new(&env)?;
        if env.default_branch.is_none() {
            env.default_branch = api.rest.default_branch().await.ok();
        }
        let store = DataStore::new(&state_dir.join("data"), args.cache_size_mb << 20, args.download_concurrency)?;
        let volume = Volume::new(&args.volume).map_err(anyhow::Error::msg)?;
        let mut cfg = VfsConfig::new(volume)
            .with_root(&args.root)
            .map_err(anyhow::Error::msg)?;
        cfg.read_only = read_only;
        cfg.settle = args.settle;
        cfg.refresh = (!args.refresh.is_zero()).then_some(args.refresh);
        cfg.upload_concurrency = args.upload_concurrency;
        tracing::info!("scopes: {:?}", env.scopes());
        Vfs::load(cfg, api, store, env.scopes()).await.context(
            "listing the cache failed; the REST API token needs `actions: read` (see `permissions:`)",
        )
    })?;
    let session = mount_fuse(args, &vfs, &rt, read_only)?;
    let info = format!(
        "mounted volume {:?}{} at {} ({}) in {:.1}s",
        args.volume,
        if args.root.is_empty() {
            String::new()
        } else {
            format!(" at {:?}", args.root)
        },
        args.mountpoint.display(),
        if read_only { "read-only" } else { "read-write" },
        started.elapsed().as_secs_f64()
    );
    tracing::info!("{info}");
    ready.send(&format!("ok {info}\n"));
    if !args.daemon {
        println!("{info}");
    }
    run_session(session, &args.mountpoint, &rt, signals)?;

    tracing::info!("unmounted; uploading pending changes");
    let summary: Summary = rt.block_on(vfs.drain());
    write_atomically(
        &state_dir.join("summary.json"),
        &serde_json::to_vec_pretty(&summary)?,
    )?;
    tracing::info!(
        "uploaded {} files ({} bytes), {} whiteouts, and {} directory marks in {} layers and {} blobs; {} failures",
        summary.uploaded_files,
        summary.uploaded_bytes,
        summary.whiteouts,
        summary.dir_markers,
        summary.layers,
        summary.blobs,
        summary.failures.len()
    );
    for f in &summary.failures {
        tracing::error!("not saved: {}: {}", f.key, f.error);
    }
    rt.shutdown_timeout(Duration::from_secs(5));
    Ok(if summary.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

#[cfg(target_os = "linux")]
type Session = fuser::Session<gha_cache_fusefs::fuse::FuseFs>;
#[cfg(not(target_os = "linux"))]
struct Session;

#[cfg(target_os = "linux")]
fn mount_fuse(
    args: &MountArgs,
    vfs: &Vfs,
    rt: &tokio::runtime::Runtime,
    read_only: bool,
) -> anyhow::Result<Session> {
    use fuser::{MountOption, SessionACL};
    let fs = gha_cache_fusefs::fuse::FuseFs::new(vfs.clone(), rt.handle().clone());
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::FSName("gha-cache".into()),
        MountOption::Subtype("gha-cache-fusefs".into()),
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
        if read_only {
            MountOption::RO
        } else {
            MountOption::RW
        },
    ];
    if args.allow_other {
        config.acl = SessionACL::All;
    }
    config.n_threads = Some(args.threads.max(1));
    std::fs::create_dir_all(&args.mountpoint)?;
    fuser::Session::new(fs, &args.mountpoint, &config)
        .with_context(|| format!("mounting FUSE at {}", args.mountpoint.display()))
}

#[cfg(not(target_os = "linux"))]
fn mount_fuse(
    _: &MountArgs,
    _: &Vfs,
    _: &tokio::runtime::Runtime,
    _: bool,
) -> anyhow::Result<Session> {
    bail!("mounting is only supported on Linux")
}

/// SIGTERM and SIGINT, registered as soon as the runtime exists: a signal
/// that arrives before the session runs is remembered, not fatal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

fn signals(rt: &tokio::runtime::Runtime) -> anyhow::Result<Signals> {
    use tokio::signal::unix::{SignalKind, signal};
    let _guard = rt.enter();
    Ok(Signals {
        term: signal(SignalKind::terminate())?,
        int: signal(SignalKind::interrupt())?,
    })
}

/// Serves until unmounted; SIGTERM or SIGINT unmount.
#[cfg(target_os = "linux")]
fn run_session(
    mut session: Session,
    mountpoint: &Path,
    rt: &tokio::runtime::Runtime,
    mut signals: Signals,
) -> anyhow::Result<()> {
    let mut unmounter = session.unmount_callable();
    let mountpoint = mountpoint.to_path_buf();
    rt.spawn(async move {
        tokio::select! {
            _ = signals.term.recv() => {}
            _ = signals.int.recv() => {}
        }
        tracing::info!("signal received; unmounting {}", mountpoint.display());
        if let Err(e) = unmounter.unmount() {
            tracing::warn!("unmount failed ({e}); detaching lazily");
            lazy_unmount(&mountpoint);
        }
    });
    session.run().context("FUSE session failed")
}

#[cfg(not(target_os = "linux"))]
fn run_session(
    _: Session,
    _: &Path,
    _: &tokio::runtime::Runtime,
    _: Signals,
) -> anyhow::Result<()> {
    Ok(())
}

fn lazy_unmount(mountpoint: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(c) = std::ffi::CString::new(mountpoint.as_os_str().as_bytes()) {
            if unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) } == 0 {
                return;
            }
        }
    }
    for bin in ["fusermount3", "fusermount"] {
        let status = std::process::Command::new(bin)
            .args(["-u", "-z", "--"])
            .arg(mountpoint)
            .status();
        if status.is_ok_and(|s| s.success()) {
            return;
        }
    }
}

fn is_mounted(mountpoint: &Path) -> bool {
    let abs: PathBuf = std::path::absolute(mountpoint)
        .unwrap_or_else(|_| mountpoint.to_path_buf())
        .components()
        .collect();
    std::fs::read_to_string("/proc/self/mounts").is_ok_and(|m| {
        m.lines().any(|l| {
            let mut f = l.split(' ');
            let (_, mp, fstype) = (f.next(), f.next(), f.next());
            mp == Some(&*abs.to_string_lossy()) && fstype.is_some_and(|t| t.starts_with("fuse"))
        })
    })
}

fn unmount(args: UnmountArgs) -> anyhow::Result<ExitCode> {
    let state_dir = args
        .state_dir
        .clone()
        .unwrap_or_else(|| default_state_dir(&args.mountpoint));
    let lock_path = state_dir.join("lock");
    let pid: Option<i32> = std::fs::read_to_string(state_dir.join("pid"))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    if lock_path.exists() && try_lock(&lock_path)?.is_none() {
        let Some(pid) = pid else {
            bail!("no pid file in {}", state_dir.display())
        };
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            bail!(
                "signalling daemon {pid}: {}",
                std::io::Error::last_os_error()
            );
        }
        eprintln!("gha-cache-fusefs: waiting for daemon {pid} to finish uploading");
        let deadline = Instant::now() + args.timeout;
        loop {
            if try_lock(&lock_path)?.is_some() {
                break;
            }
            if Instant::now() > deadline {
                bail!("timed out after {:?} waiting for the daemon", args.timeout);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    } else if is_mounted(&args.mountpoint) {
        // The daemon is gone but the kernel still has the mount.
        eprintln!("gha-cache-fusefs: no daemon; detaching stale mount");
        lazy_unmount(&args.mountpoint);
    }
    let summary_path = state_dir.join("summary.json");
    let Ok(text) = std::fs::read_to_string(&summary_path) else {
        eprintln!(
            "gha-cache-fusefs: no summary at {}; the daemon may have crashed (see {})",
            summary_path.display(),
            state_dir.join("daemon.log").display()
        );
        return Ok(ExitCode::from(2));
    };
    let summary: Summary = serde_json::from_str(&text)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(if summary.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn fake_server(args: FakeArgs, log: &str) -> anyhow::Result<ExitCode> {
    init_logging(log);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let cfg = FakeConfig {
            url_ttl: args.url_ttl,
            fail_blob_every: args.fail_blob_every,
            ..args.profile.config()
        };
        let server = FakeServer::bind(args.listen, cfg).await?;
        let env = server.env("refs/heads/main", &[]);
        // Single-quoted, so the file can be sourced by a shell.
        let vars: String = [
            ("ACTIONS_RESULTS_URL", env.results_url.as_str()),
            ("ACTIONS_RUNTIME_TOKEN", &env.runtime_token),
            ("ACTIONS_CACHE_SERVICE_V2", "true"),
            ("GITHUB_API_URL", &env.api_url),
            ("GITHUB_TOKEN", "fake"),
            ("GITHUB_REPOSITORY", &env.repository),
            ("GITHUB_REF", &env.git_ref),
        ]
        .iter()
        .map(|(k, v)| format!("{k}='{}'\n", v.replace('\'', r"'\''")))
        .collect();
        if let Some(path) = &args.env_file {
            write_atomically(path, vars.as_bytes())?;
        }
        println!("fake cache service listening on {}", server.base_url());
        server.wait().await;
        anyhow::Ok(ExitCode::SUCCESS)
    })
}

fn bench(args: BenchArgs, log: &str) -> anyhow::Result<ExitCode> {
    init_logging(log);
    let mut sizes = match args.scale {
        Scale::Quick => bench::Sizes::quick(),
        Scale::Full => bench::Sizes::full(),
    };
    sizes.files = args.files.unwrap_or(sizes.files);
    sizes.large_mib = args.large_mib.unwrap_or(sizes.large_mib);
    sizes.entries = args.entries.unwrap_or(sizes.entries);
    sizes.random_reads = args.random_reads.unwrap_or(sizes.random_reads);
    let name = args.service.to_possible_value().expect("not skipped");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let rows = rt.block_on(async {
        let service = match args.service {
            BenchService::Hosted => bench::Service::Fake(FakeConfig::hosted()),
            BenchService::Local => bench::Service::Fake(FakeConfig::default()),
            BenchService::Real => {
                let mut env = Env::from_process(&args.token_env)?;
                env.check_mode()?;
                if !env.cache_mode.writable() {
                    bail!("this run may not write to the cache");
                }
                if env.default_branch.is_none() {
                    env.default_branch = Api::new(&env)?.rest.default_branch().await.ok();
                }
                let run = match (
                    std::env::var("GITHUB_RUN_ID"),
                    std::env::var("GITHUB_RUN_ATTEMPT"),
                ) {
                    (Ok(id), Ok(attempt)) => format!("{id}-{attempt}"),
                    _ => std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs()
                        .to_string(),
                };
                let volume = format!("{}-{run}", args.volume);
                Volume::new(&volume).map_err(anyhow::Error::msg)?;
                bench::Service::Real { env, volume }
            }
        };
        bench::run(&service, &sizes, &args.only).await
    })?;
    println!(
        "### gha-cache-fusefs bench: {}\n\n{}.\n\n{}",
        name.get_name(),
        sizes.describe(),
        bench::markdown(&rows)
    );
    if let Some(path) = &args.json {
        let doc = serde_json::json!({
            "service": name.get_name(),
            "sizes": sizes,
            "rows": rows,
        });
        write_atomically(path, &serde_json::to_vec_pretty(&doc)?)?;
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    Ok(ExitCode::SUCCESS)
}
