//! Garbage collection: liveness computation and store path deletion.

use crate::db::{BasenameIndex, NixDb};
use crate::roots::{find_roots, find_temp_roots};
use crate::{format_size, make_store_writable};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Delete a store path from disk, returning bytes freed.
/// Store paths are read-only (chmod a-w on registration); make directories
/// writable before removal, mirroring Nix's `deletePath`.
fn delete_store_path(real_path: &Path) -> Result<u64> {
    use std::os::unix::fs::PermissionsExt;

    let meta = match fs::symlink_metadata(real_path) {
        Ok(m) => m,
        // Already gone (another process won the race).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("stat {}", real_path.display())),
    };

    if !meta.file_type().is_dir() {
        let bytes = meta.blocks() * 512;
        fs::remove_file(real_path).with_context(|| format!("removing {}", real_path.display()))?;
        return Ok(bytes);
    }

    let mut bytes_freed = 0u64;
    // Store paths are r-x; chmod and retry on permission errors instead
    // of aborting the whole GC.
    for entry in walkdir::WalkDir::new(real_path).contents_first(true) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                if let Some(parent) = e.path().and_then(Path::parent) {
                    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
                }
                continue;
            }
        };
        let p = entry.path();
        if let Ok(m) = entry.metadata() {
            bytes_freed += m.blocks() * 512;
        }
        if entry.file_type().is_dir() {
            if fs::remove_dir(p).is_err() {
                let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o755));
                let _ = fs::remove_dir(p);
            }
        } else if fs::remove_file(p).is_err() {
            if let Some(parent) = p.parent() {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
            }
            let _ = fs::remove_file(p);
        }
    }

    Ok(bytes_freed)
}

use nix::fcntl::{Flock, FlockArg};

/// Lock a `tmp-*` build dir before deleting. None means a builder still
/// holds it. Caller keeps the fd through deletion to avoid a TOCTOU race.
fn try_lock_dir(path: &Path) -> Option<Flock<fs::File>> {
    let f = fs::File::open(path).ok()?;
    Flock::lock(f, FlockArg::LockExclusiveNonblock).ok()
}

/// Same lock Nix takes. Builders hold it shared while registering temp
/// roots; we take it exclusive so the root set can't change under us.
fn acquire_gc_lock(state_dir: &Path) -> Result<Flock<fs::File>> {
    let lock_path = state_dir.join("gc.lock");
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening GC lock {}", lock_path.display()))?;

    match Flock::lock(f, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((f, _)) => {
            log::info!("waiting for the big garbage collector lock...");
            Flock::lock(f, FlockArg::LockExclusive)
                .map_err(|(_, e)| e)
                .with_context(|| format!("acquiring GC lock {}", lock_path.display()))
        }
    }
}

#[derive(Default)]
pub struct GcOptions {
    pub dry_run: bool,
    pub max_freed: Option<u64>,
    /// Keep paths registered at or after this Unix timestamp.
    pub keep_recent_after: Option<i64>,
}

/// Main GC: find roots, compute alive closure, delete dead paths.
pub fn collect_garbage(db: &NixDb, opts: &GcOptions) -> Result<(u64, usize)> {
    let dry_run = opts.dry_run;
    let max_freed = opts.max_freed;
    // Acquire the global GC lock before anything else. Builders take a
    // shared lock when adding temp roots; holding the exclusive lock
    // ensures no new roots appear after we scan them.
    let _gc_lock = acquire_gc_lock(&db.state_dir)?;

    if !dry_run {
        make_store_writable(&db.real_store_dir)?;
    }

    log::info!("loading store graph...");
    let graph = db.load_graph()?;
    log::info!("{} total valid paths", graph.len());

    let bidx = BasenameIndex::new(&graph);

    log::info!("finding garbage collector roots...");
    let mut roots = find_roots(&db.state_dir, &db.store_dir, &bidx);

    // Add temp roots. Some may reference paths registered after our
    // graph snapshot (a builder can register paths while we hold
    // gc.lock as long as it wrote its temp root before we acquired it).
    // Track those by basename so the unknown-on-disk scan won't
    // delete them.
    let mut temp_root_basenames: crate::HashSet<String> = crate::HashSet::default();
    for tr in find_temp_roots(&db.state_dir)? {
        if let Some(i) = bidx.idx_of(&tr) {
            roots.push(i);
        } else if let Some(b) = tr.strip_prefix(graph.store_prefix.as_str()) {
            temp_root_basenames.insert(b.to_owned());
        }
    }
    // --keep-recent: treat recently registered paths as roots.
    if let Some(cutoff) = opts.keep_recent_after {
        let n_before = roots.len();
        for (i, &t) in graph.registration_times.iter().enumerate() {
            if t >= cutoff {
                roots.push(i as u32);
            }
        }
        log::info!("{} recent paths kept", roots.len() - n_before);
    }

    roots.sort_unstable();
    roots.dedup();
    log::info!("found {} roots", roots.len());

    log::info!("computing alive closure...");
    let alive = graph.compute_closure(&roots);
    let n_alive = alive.iter().filter(|&&a| a).count();
    log::info!("{} alive paths", n_alive);

    log::info!("{} dead paths", graph.len() - n_alive);

    // Also find entries on disk that aren't in the DB at all.
    // Compare by basename to avoid allocating a full-path string per entry.
    let store_prefix = graph.store_prefix.clone();
    let mut unknown_on_disk: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&db.real_store_dir) {
        for entry in entries.flatten() {
            let raw = entry.file_name();
            let name = raw.to_string_lossy();
            if name == "." || name == ".." || name == ".links" {
                continue;
            }
            if bidx.idx_of_basename(name.as_ref()).is_none()
                && !temp_root_basenames.contains(name.as_ref())
            {
                unknown_on_disk.push(name.into_owned());
            }
        }
    }
    if !unknown_on_disk.is_empty() {
        log::info!("{} unknown paths on disk not in DB", unknown_on_disk.len());
    }

    let dead_indices: Vec<u32> = (0..graph.len() as u32)
        .filter(|&i| !alive[i as usize])
        .collect();

    let max = max_freed.unwrap_or(u64::MAX);

    if dry_run {
        use std::io::Write;
        let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
        let mut estimated = 0u64;
        let mut count = 0usize;
        for &node in &dead_indices {
            if estimated >= max {
                break;
            }
            writeln!(stdout, "{}", graph.paths[node as usize])?;
            estimated += graph.nar_sizes[node as usize];
            count += 1;
        }
        for name in &unknown_on_disk {
            writeln!(stdout, "{store_prefix}{name}")?;
            count += 1;
        }
        return Ok((estimated, count));
    }

    log::info!("deleting garbage...");

    // Bulk-invalidate, then delete from disk in parallel. Safe to crash
    // mid-delete: leftover dirs are picked up as unknown-on-disk next run.
    //
    // --max-freed needs actual freed bytes (narSize lies when paths share
    // hard links), so we go one path at a time there. It's rare (auto-GC).
    let real_store_dir = db.real_store_dir.clone();
    let bytes_freed = AtomicU64::new(0);
    let mut paths_deleted = 0usize;

    let chunk_size = if max_freed.is_some() {
        1
    } else {
        dead_indices.len().max(1)
    };

    'outer: for chunk in dead_indices.chunks(chunk_size) {
        if bytes_freed.load(Ordering::Relaxed) >= max {
            log::info!("deleted more than {max} bytes; stopping");
            break 'outer;
        }
        db.invalidate_paths(chunk.iter().map(|&n| graph.paths[n as usize].as_str()))?;
        chunk.par_iter().for_each(|&node| {
            let path = &graph.paths[node as usize];
            let basename = path.strip_prefix(&store_prefix).unwrap_or(path);
            let real_path = real_store_dir.join(basename);
            log::debug!("deleting '{path}'");
            if let Ok(freed) = delete_store_path(&real_path) {
                bytes_freed.fetch_add(freed, Ordering::Relaxed);
            }
        });
        paths_deleted += chunk.len();
    }

    // Unknown-on-disk paths: also parallel. tmp-* dirs hold flock through
    // deletion to avoid TOCTOU race with a builder.
    let unknown_deleted = AtomicU64::new(0);
    if bytes_freed.load(Ordering::Relaxed) < max {
        unknown_on_disk.par_iter().for_each(|name| {
            let real_path = real_store_dir.join(name);
            let _tmp_lock = if name.starts_with("tmp-") {
                match try_lock_dir(&real_path) {
                    Some(f) => Some(f),
                    None => {
                        log::debug!("skipping locked tempdir {}", real_path.display());
                        return;
                    }
                }
            } else {
                None
            };
            log::debug!("deleting '{store_prefix}{name}'");
            if let Ok(freed) = delete_store_path(&real_path) {
                bytes_freed.fetch_add(freed, Ordering::Relaxed);
            }
            unknown_deleted.fetch_add(1, Ordering::Relaxed);
        });
    }

    let bytes_freed = bytes_freed.into_inner();
    paths_deleted += unknown_deleted.into_inner() as usize;

    // Clean up unused hard links in .links
    if !dry_run {
        clean_links(&db.links_dir)?;
    }

    Ok((bytes_freed, paths_deleted))
}

/// Remove hard links with link count 1 from .links directory.
/// The .links dir can contain millions of entries; stat + unlink per entry
/// is disk-bound, so process in parallel.
fn clean_links(links_dir: &Path) -> Result<()> {
    use std::sync::atomic::AtomicI64;

    log::info!("deleting unused links...");
    let entries: Vec<_> = match fs::read_dir(links_dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return Ok(()),
    };

    // For each surviving link with N references, hard linking saves
    // (N-1)*size bytes compared to N independent copies.
    let saved_bytes = AtomicI64::new(0);

    entries.par_iter().for_each(|entry| {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return;
        };
        if meta.nlink() != 1 {
            saved_bytes.fetch_add(
                (meta.nlink() as i64 - 1) * meta.size() as i64,
                Ordering::Relaxed,
            );
            return;
        }
        fs::remove_file(&path).ok();
    });

    let saving = saved_bytes.into_inner();
    if saving > 0 {
        log::info!(
            "hard linking is currently saving {}",
            format_size(saving as u64)
        );
    }

    Ok(())
}
