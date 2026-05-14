//! Hardlink-based store dedup, on-disk compatible with `nix-store --optimise`.

use crate::hash::nar_hash_nix32;
use anyhow::{Context, Result, bail};
use fast_nix_common::{HashSet, db::NixDb, format_size};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use walkdir::WalkDir;

#[derive(Default, Debug)]
pub struct Stats {
    pub files_linked: AtomicU64,
    pub bytes_freed: AtomicU64,
    pub files_skipped: AtomicU64,
}

pub struct Options {
    pub store_dir: PathBuf,
    pub state_dir: PathBuf,
    pub dry_run: bool,
    /// Skip files below this size; the extra .links inode costs more.
    pub min_size: u64,
    pub jobs: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            store_dir: PathBuf::from("/nix/store"),
            state_dir: PathBuf::from("/nix/var/nix"),
            dry_run: false,
            min_size: 0,
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
        }
    }
}

/// Inodes already in `.links/`. Files with these inodes are already deduped
/// and can be skipped without hashing.
fn load_link_inodes(links_dir: &Path) -> Result<HashSet<u64>> {
    let mut set = HashSet::default();
    match fs::read_dir(links_dir) {
        Ok(rd) => {
            for entry in rd {
                let entry = entry?;
                set.insert(entry.metadata()?.ino());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", links_dir.display())),
    }
    Ok(set)
}

/// Replace `path` with a hardlink to `link_path`, preserving the
/// store-invariant that parent directories are read-only and have mtime 0.
fn replace_with_link(path: &Path, link_path: &Path, store_dir: &Path) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    let must_toggle = parent != store_dir;

    if must_toggle {
        let st = fs::metadata(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(st.mode() | 0o200))?;
    }

    let restore = scopeguard(parent, must_toggle);

    let tmp = parent.join(format!(
        ".tmp-link-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    fs::hard_link(link_path, &tmp)
        .with_context(|| format!("hardlink {} -> {}", link_path.display(), tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()));
    }
    drop(restore);
    Ok(())
}

/// Per-call random suffix for temp links. Concurrent tasks in this
/// process must not collide, so a global counter is mixed in.
fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = foldhash::fast::RandomState::default().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.finish()
}

struct ParentRestore<'a> {
    dir: &'a Path,
    active: bool,
}
impl Drop for ParentRestore<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::set_permissions(self.dir, fs::Permissions::from_mode(0o555));
            let _ = filetime_set_zero(self.dir);
        }
    }
}
fn scopeguard(dir: &Path, active: bool) -> ParentRestore<'_> {
    ParentRestore { dir, active }
}

fn filetime_set_zero(path: &Path) -> std::io::Result<()> {
    use nix::sys::stat::{UtimensatFlags, utimensat};
    use nix::sys::time::TimeSpec;
    utimensat(
        nix::fcntl::AT_FDCWD,
        path,
        &TimeSpec::new(1, 0),
        &TimeSpec::new(1, 0),
        UtimensatFlags::NoFollowSymlink,
    )
    .map_err(std::io::Error::from)
}

async fn optimise_file(
    path: PathBuf,
    links_dir: Arc<PathBuf>,
    store_dir: Arc<PathBuf>,
    opts: Arc<Options>,
    stats: Arc<Stats>,
) -> Result<()> {
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        // Path GC'd between listing and here.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("lstat {}", path.display())),
    };
    let ft = meta.file_type();
    // On macOS link(2) dereferences symlinks; only Linux can hardlink
    // them directly. Same gate as Nix's CAN_LINK_SYMLINK.
    let can_link_symlink = cfg!(target_os = "linux");
    let linkable = ft.is_file() || (ft.is_symlink() && can_link_symlink);
    if !linkable {
        return Ok(());
    }
    // HFS/APFS refuse hardlinks for some files inside .app bundles.
    // See https://github.com/NixOS/nix/issues/1443.
    #[cfg(target_os = "macos")]
    if path.to_str().is_some_and(|s| s.contains(".app/Contents/")) {
        return Ok(());
    }
    // Writable files in the store are suspect (e.g. fontconfig caches
    // mutated by root); skip them like nix does.
    if ft.is_file() && meta.mode() & 0o200 != 0 {
        log::warn!("skipping suspicious writable file {}", path.display());
        stats.files_skipped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    if ft.is_file() && meta.len() < opts.min_size {
        stats.files_skipped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    let hash = nar_hash_nix32(&path).await?;
    let link_path = links_dir.join(&hash);

    if opts.dry_run {
        let already = fs::symlink_metadata(&link_path)
            .map(|lm| lm.ino() == meta.ino())
            .unwrap_or(false);
        if !already {
            stats.files_linked.fetch_add(1, Ordering::Relaxed);
            stats.bytes_freed.fetch_add(meta.len(), Ordering::Relaxed);
        }
        return Ok(());
    }

    // symlink_metadata, not exists(): a dangling symlink in .links/
    // (corrupt state) would otherwise read as missing.
    if fs::symlink_metadata(&link_path).is_err() {
        match fs::hard_link(&path, &link_path) {
            Ok(()) => {}
            // Lost a race to another worker.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            // ext4 dir index full; just skip dedup for this file.
            Err(e) if e.raw_os_error() == Some(libc_enospc()) => {
                log::info!("cannot link {}: {}", link_path.display(), e);
                return Ok(());
            }
            // Path GC'd between lstat and link.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("hardlink {} -> {}", path.display(), link_path.display())
                });
            }
        }
    }

    let lmeta = fs::symlink_metadata(&link_path)?;
    if lmeta.ino() == meta.ino() {
        return Ok(());
    }
    // Size mismatch means a corrupt .links entry; don't merge.
    if lmeta.len() != meta.len() {
        log::warn!(
            "link {} has size {} but file {} has size {}; skipping",
            link_path.display(),
            lmeta.len(),
            path.display(),
            meta.len()
        );
        return Ok(());
    }

    let store_dir = store_dir.clone();
    let path2 = path.clone();
    let link_path2 = link_path.clone();
    let res =
        tokio::task::spawn_blocking(move || replace_with_link(&path2, &link_path2, &store_dir))
            .await?;
    match res {
        Ok(()) => {
            stats.files_linked.fetch_add(1, Ordering::Relaxed);
            stats.bytes_freed.fetch_add(meta.len(), Ordering::Relaxed);
        }
        Err(e) => {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
                // EMLINK: link count cap hit, typically empty files.
                if io.raw_os_error() == Some(libc_emlink()) {
                    if meta.len() > 0 {
                        log::info!(
                            "{} has reached maximum number of links",
                            link_path.display()
                        );
                    }
                    return Ok(());
                }
                // Path or parent dir GC'd while we worked on it.
                if io.kind() == std::io::ErrorKind::NotFound {
                    return Ok(());
                }
            }
            return Err(e);
        }
    }
    Ok(())
}

fn libc_enospc() -> i32 {
    nix::errno::Errno::ENOSPC as i32
}
fn libc_emlink() -> i32 {
    nix::errno::Errno::EMLINK as i32
}

/// Hold gc.lock shared for the whole run so the GC (which takes it
/// exclusive) cannot delete paths from under us.
fn shared_gc_lock(state_dir: &Path) -> Result<Flock<fs::File>> {
    let lock_path = state_dir.join("gc.lock");
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    match Flock::lock(f, FlockArg::LockSharedNonblock) {
        Ok(l) => Ok(l),
        Err((f, _)) => {
            log::info!("waiting for garbage collector to finish...");
            Flock::lock(f, FlockArg::LockShared)
                .map_err(|(_, e)| e)
                .context("acquiring shared gc.lock")
        }
    }
}

pub async fn optimise_store(opts: Options) -> Result<Stats> {
    let links_dir = opts.store_dir.join(".links");
    if !opts.dry_run {
        fs::create_dir_all(&links_dir)
            .with_context(|| format!("creating {}", links_dir.display()))?;
    }

    let _gc_lock = shared_gc_lock(&opts.state_dir)?;

    let db = NixDb::open(&opts.store_dir, &opts.state_dir)?;
    let paths = db.valid_paths()?;
    drop(db);
    log::info!("optimising {} store paths", paths.len());

    let known_inodes = Arc::new(load_link_inodes(&links_dir)?);
    log::debug!("loaded {} known link inodes", known_inodes.len());

    let opts = Arc::new(opts);
    let links_dir = Arc::new(links_dir);
    let store_dir = Arc::new(opts.store_dir.clone());
    let stats = Arc::new(Stats::default());
    let sem = Arc::new(Semaphore::new(opts.jobs));
    let mut tasks: JoinSet<Result<()>> = JoinSet::new();

    for store_path in paths {
        // walkdir is sync; run on the blocking pool, then spawn one task
        // per file. Files are independent until the link/rename step.
        let known = known_inodes.clone();
        let store_path = PathBuf::from(store_path);
        let entries = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>> {
            let mut out = Vec::new();
            for entry in WalkDir::new(&store_path).follow_links(false) {
                let entry = match entry {
                    Ok(e) => e,
                    // Path can vanish under us (concurrent GC).
                    Err(e)
                        if e.io_error()
                            .is_some_and(|ioe| ioe.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                let ft = entry.file_type();
                if !ft.is_file() && !ft.is_symlink() {
                    continue;
                }
                if let Ok(m) = entry.metadata()
                    && known.contains(&m.ino())
                {
                    continue;
                }
                out.push(entry.into_path());
            }
            Ok(out)
        })
        .await??;

        for file in entries {
            let permit = sem.clone().acquire_owned().await?;
            let links_dir = links_dir.clone();
            let store_dir = store_dir.clone();
            let opts = opts.clone();
            let stats = stats.clone();
            tasks.spawn(async move {
                let _p = permit;
                optimise_file(file, links_dir, store_dir, opts, stats).await
            });
        }

        // Drain finished tasks to bound memory.
        while let Some(res) = tasks.try_join_next() {
            res??;
        }
    }

    while let Some(res) = tasks.join_next().await {
        res??;
    }

    Ok(Arc::into_inner(stats).expect("all tasks joined"))
}

pub fn cli_main() -> Result<()> {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(log::LevelFilter::Info);
    log::set_boxed_logger(Box::new(StderrLogger(level))).unwrap();
    log::set_max_level(level);

    let opts = parse_args()?;
    let dry = opts.dry_run;
    let jobs = opts.jobs;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(jobs)
        .enable_all()
        .build()?;
    let stats = rt.block_on(optimise_store(opts))?;

    let linked = stats.files_linked.load(Ordering::Relaxed);
    let freed = stats.bytes_freed.load(Ordering::Relaxed);
    if dry {
        println!(
            "{} would be freed by hard-linking {} files",
            format_size(freed),
            linked
        );
    } else {
        println!(
            "{} freed by hard-linking {} files",
            format_size(freed),
            linked
        );
    }
    Ok(())
}

fn parse_args() -> Result<Options> {
    let mut p = pico_args::Arguments::from_env();
    if p.contains("--help") {
        eprintln!("Usage: fast-nix-optimise [OPTIONS]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("      --dry-run             Show what would be done");
        eprintln!("      --min-size BYTES      Skip files smaller than BYTES");
        eprintln!("  -j, --jobs N              Concurrency [default: num CPUs]");
        eprintln!("      --store-dir PATH      Nix store directory [default: /nix/store]");
        eprintln!("      --state-dir PATH      Nix state directory [default: /nix/var/nix]");
        std::process::exit(0);
    }
    let mut opts = Options {
        dry_run: p.contains("--dry-run"),
        ..Options::default()
    };
    if let Some(v) = p.opt_value_from_str("--min-size")? {
        opts.min_size = v;
    }
    if let Some(v) = p
        .opt_value_from_str("--jobs")?
        .or(p.opt_value_from_str("-j")?)
    {
        opts.jobs = v;
    }
    if let Some(v) = p.opt_value_from_str("--store-dir")? {
        opts.store_dir = v;
    }
    if let Some(v) = p.opt_value_from_str("--state-dir")? {
        opts.state_dir = v;
    }
    let rest = p.finish();
    if !rest.is_empty() {
        bail!("unexpected arguments: {:?}", rest);
    }
    Ok(opts)
}

struct StderrLogger(log::LevelFilter);
impl log::Log for StderrLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{:5}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedups_identical_files() {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(state.join("db")).unwrap();
        fs::create_dir_all(&store).unwrap();

        // Two store paths, each with an identical file.
        let p1 = store.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a");
        let p2 = store.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        for p in [&p1, &p2] {
            let f = p.join("data");
            fs::write(&f, b"hello world hello world hello world\n").unwrap();
            fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o555)).unwrap();
        }

        // Minimal ValidPaths schema.
        let conn = rusqlite_open(&state.join("db/db.sqlite"));
        conn.execute_batch("CREATE TABLE ValidPaths (id INTEGER PRIMARY KEY, path TEXT NOT NULL);")
            .unwrap();
        for p in [&p1, &p2] {
            conn.execute(
                "INSERT INTO ValidPaths (path) VALUES (?)",
                [p.to_str().unwrap()],
            )
            .unwrap();
        }
        drop(conn);

        let stats = optimise_store(Options {
            store_dir: store.clone(),
            state_dir: state,
            jobs: 2,
            ..Options::default()
        })
        .await
        .unwrap();

        // First file *becomes* the canonical link (same inode, no rename),
        // so only the second one counts as "linked".
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 1);
        let i1 = fs::metadata(p1.join("data")).unwrap().ino();
        let i2 = fs::metadata(p2.join("data")).unwrap().ino();
        assert_eq!(i1, i2, "files should share an inode after optimise");
        assert!(store.join(".links").read_dir().unwrap().count() == 1);

        // Cleanup so tempdir can remove (dirs were made read-only).
        for p in [&p1, &p2] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn rusqlite_open(p: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(p).unwrap()
    }

    #[test]
    fn shared_gc_lock_blocks_exclusive() {
        use nix::fcntl::{Flock, FlockArg};
        let tmp = tempdir().unwrap();
        let _shared = shared_gc_lock(tmp.path()).unwrap();
        // GC takes the lock exclusive; while we hold it shared, the
        // exclusive non-blocking attempt must fail.
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path().join("gc.lock"))
            .unwrap();
        assert!(Flock::lock(f, FlockArg::LockExclusiveNonblock).is_err());
    }
}
