//! Garbage collection: liveness computation and store path deletion.

use crate::db::{BasenameIndex, NixDb};
use crate::gc_socket::{GcSocketServer, LiveSet};
use crate::roots::{find_roots, find_temp_roots};
use crate::{format_size, make_store_writable};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
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
                // rmdir needs write permission on the parent, not on p.
                if let Some(parent) = p.parent() {
                    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
                }
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
    let graph = Arc::new(db.load_graph()?);
    log::info!("{} total valid paths", graph.len());

    // Start the gc-socket as soon as the graph is loaded so builders stop
    // busy-polling gc.lock. Roots received only shrink the dead set.
    let live = Arc::new(LiveSet::new(graph.len(), crate::HashSet::default()));
    let _gc_socket = if dry_run {
        None
    } else {
        Some(GcSocketServer::start(
            &db.state_dir,
            Arc::clone(&live),
            Arc::clone(&graph),
        )?)
    };

    let bidx = BasenameIndex::new(&graph);

    // A --store-dir that doesn't match the DB contents (wrong directory)
    // would make every root lookup miss and every DB path look dead,
    // wiping the store. Refuse to proceed.
    if !graph.is_empty() && bidx.map.is_empty() {
        anyhow::bail!(
            "store dir {} does not match any path in the Nix database \
             (e.g. {}); refusing to collect garbage",
            db.store_dir.display(),
            graph.paths[0],
        );
    }

    log::info!("finding garbage collector roots...");
    let mut roots = find_roots(&db.state_dir, &db.store_dir, &bidx)?;

    // Add temp roots. Some may reference paths registered after our
    // graph snapshot (a builder can register paths while we hold
    // gc.lock as long as it wrote its temp root before we acquired it).
    // Track those by basename so the unknown-on-disk scan won't
    // delete them.
    let mut temp_root_basenames: crate::HashSet<String> = crate::HashSet::default();
    // Hash parts of all temp roots. Nix matches temp roots by hash part so
    // that sibling files of an active build (`<path>.lock`, `<path>.chroot`,
    // `<path>.check`) are protected too; the unknown-on-disk scan must not
    // delete a lock file another builder currently holds.
    let mut temp_root_hashes: crate::HashSet<String> = crate::HashSet::default();
    for tr in find_temp_roots(&db.state_dir)? {
        if let Some(b) = tr.strip_prefix(graph.store_prefix.as_str()) {
            if b.len() > 32 && b.as_bytes()[32] == b'-' {
                temp_root_hashes.insert(b[..32].to_owned());
            }
            if bidx.idx_of_basename(b).is_none() {
                temp_root_basenames.insert(b.to_owned());
            }
        }
        if let Some(i) = bidx.idx_of(&tr) {
            roots.push(i);
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
            // read_dir never yields "." or "..".
            if name == ".links" {
                continue;
            }
            // An entry shares an active build's hash part if its first 32
            // chars match (covers `<path>.lock` and friends).
            let hash_part_active = name.len() >= 32
                && name.is_char_boundary(32)
                && temp_root_hashes.contains(&name[..32]);
            if bidx.idx_of_basename(name.as_ref()).is_none()
                && !temp_root_basenames.contains(name.as_ref())
                && !hash_part_active
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

    // Test sync point: block until the named fifo is readable. The NixOS
    // test connects to gc-socket here so the protect() path is exercised
    // deterministically rather than racing the delete loop.
    if let Ok(p) = std::env::var("_FAST_NIX_GC_TEST_SYNC") {
        let _ = fs::read(&p);
    }

    // Bulk-invalidate, then delete from disk in parallel. Safe to crash
    // mid-delete: leftover dirs are picked up as unknown-on-disk next run.
    let real_store_dir = db.real_store_dir.clone();
    let bytes_freed = AtomicU64::new(0);
    let paths_deleted = AtomicU64::new(0);

    // Reverse edges among dead paths. A chunk has to take the dead
    // referrers of everything it invalidates with it, otherwise a
    // surviving row keeps references to deleted rows. Only needed when
    // --ensure-free splits deletion into chunks; a single chunk is the
    // whole dead set. Alive paths never reference dead ones.
    let dead_referrers: Option<Vec<Vec<u32>>> = max_freed.map(|_| {
        let mut rev = vec![Vec::new(); graph.len()];
        for &n in &dead_indices {
            for &m in graph.refs(n) {
                if !alive[m as usize] && m != n {
                    rev[m as usize].push(n);
                }
            }
        }
        rev
    });

    // Fill each chunk by cumulative narSize up to the remaining --ensure-free
    // budget, then re-check actual freed bytes (narSize over-reports for
    // hard-linked paths). Without --ensure-free, max is u64::MAX: one chunk.
    let mut in_chunk = vec![false; graph.len()];
    let mut cursor = 0usize;
    while cursor < dead_indices.len() {
        let freed_so_far = bytes_freed.load(Ordering::Relaxed);
        if freed_so_far >= max {
            log::info!("deleted more than {max} bytes; stopping");
            break;
        }
        let remaining = max - freed_so_far;
        let mut chunk: Vec<u32> = Vec::new();
        let mut estimated = 0u64;
        while cursor < dead_indices.len() && estimated < remaining {
            let n = dead_indices[cursor];
            cursor += 1;
            if !in_chunk[n as usize] {
                in_chunk[n as usize] = true;
                estimated = estimated.saturating_add(graph.nar_sizes[n as usize]);
                chunk.push(n);
            }
        }
        if chunk.is_empty() {
            break;
        }
        // Close the chunk under dead referrers (see dead_referrers above).
        if let Some(rev) = &dead_referrers {
            let mut i = 0;
            while i < chunk.len() {
                for &r in &rev[chunk[i] as usize] {
                    if !in_chunk[r as usize] {
                        in_chunk[r as usize] = true;
                        chunk.push(r);
                    }
                }
                i += 1;
            }
        }
        // Snapshot-filter, don't pre-claim: protect() should wait for the
        // rayon-bounded in-flight set, not the whole chunk.
        let (mut claimed, skipped): (Vec<u32>, Vec<u32>) =
            chunk.iter().copied().partition(|&n| !live.is_protected(n));
        // protect() marks closures atomically, but this filter reads one
        // node at a time: it may have kept a reference whose referrer got
        // protected a moment later. Drop the closures of skipped paths.
        if !skipped.is_empty() {
            let keep_out = graph.compute_closure(&skipped);
            claimed.retain(|&n| !keep_out[n as usize]);
        }
        // Invalidate rows before unlinking: builders trust isValidPath(),
        // so a path must never look valid after its disk entry is gone.
        // Paths protected after this point stay on disk; their builder
        // re-registers them or the next run collects them as
        // unknown-on-disk. See alloy/gc_db_consistency.als.
        db.invalidate_paths(claimed.iter().map(|&n| graph.paths[n as usize].as_str()))?;
        let n_deleted = claimed
            .par_iter()
            .copied()
            .filter(|&node| {
                if !live.try_begin_delete_node(node) {
                    return false;
                }
                let path = &graph.paths[node as usize];
                let basename = path.strip_prefix(&store_prefix).unwrap_or(path);
                let real_path = real_store_dir.join(basename);
                log::debug!("deleting '{path}'");
                if let Ok(freed) = delete_store_path(&real_path) {
                    bytes_freed.fetch_add(freed, Ordering::Relaxed);
                }
                live.end_delete_node(node);
                true
            })
            .count();
        paths_deleted.fetch_add(n_deleted as u64, Ordering::Relaxed);
    }

    // Unknown-on-disk paths: also parallel. tmp-* dirs hold flock through
    // deletion to avoid TOCTOU race with a builder.
    if bytes_freed.load(Ordering::Relaxed) < max {
        // A builder may have registered a scanned path after our graph
        // snapshot and then exited, leaving its temp root file stale.
        // Unlinking such a path would orphan its ValidPaths row, so
        // re-check the DB first. Checking once up front is enough: any
        // registration from here on goes through the gc-socket and is
        // shielded by protected_unknown. See tempRootStale in
        // alloy/gc_db_consistency.als.
        let mut unknown_on_disk = unknown_on_disk;
        unknown_on_disk.retain(
            |name| match db.is_valid_path(&format!("{store_prefix}{name}")) {
                Ok(valid) => !valid,
                Err(e) => {
                    log::warn!("skipping {store_prefix}{name}: validity check failed: {e}");
                    false
                }
            },
        );
        unknown_on_disk.par_iter().for_each(|name| {
            if !live.try_begin_delete_unknown(name) {
                return;
            }
            let real_path = real_store_dir.join(name);
            let _tmp_lock = if name.starts_with("tmp-") {
                match try_lock_dir(&real_path) {
                    Some(f) => Some(f),
                    None => {
                        log::debug!("skipping locked tempdir {}", real_path.display());
                        live.end_delete_unknown(name);
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
            live.end_delete_unknown(name);
            paths_deleted.fetch_add(1, Ordering::Relaxed);
        });
    }

    let bytes_freed = bytes_freed.into_inner();
    let paths_deleted = paths_deleted.into_inner() as usize;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn delete_store_path_missing_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(delete_store_path(&tmp.path().join("gone")).unwrap(), 0);
    }

    #[test]
    fn delete_store_path_other_stat_errors_propagate() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("file");
        fs::write(&f, b"x").unwrap();
        // ENOTDIR, not ENOENT: must not be treated as "already gone".
        assert!(delete_store_path(&f.join("sub")).is_err());
    }

    #[test]
    fn delete_store_path_file_reports_disk_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("file");
        fs::write(&f, vec![1u8; 5000]).unwrap();
        let expected = fs::symlink_metadata(&f).unwrap().blocks() * 512;
        assert!(expected > 0);
        assert_eq!(delete_store_path(&f).unwrap(), expected);
        assert!(!f.exists());
    }

    #[test]
    fn delete_store_path_removes_readonly_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pkg");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/file"), vec![1u8; 5000]).unwrap();
        let mut expected = 0;
        for e in walkdir::WalkDir::new(&dir) {
            expected += e.unwrap().metadata().unwrap().blocks() * 512;
        }
        // Store paths are read-only on disk.
        fs::set_permissions(dir.join("sub/file"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(dir.join("sub"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        assert_eq!(delete_store_path(&dir).unwrap(), expected);
        assert!(!dir.exists());
    }

    #[test]
    fn try_lock_dir_none_while_held() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = try_lock_dir(tmp.path()).expect("unheld dir is lockable");
        assert!(try_lock_dir(tmp.path()).is_none(), "held dir must not lock");
        drop(lock);
    }

    #[test]
    fn clean_links_removes_only_unreferenced() {
        let tmp = tempfile::tempdir().unwrap();
        let links = tmp.path().join(".links");
        fs::create_dir_all(&links).unwrap();
        let dead = links.join("dead");
        fs::write(&dead, b"unreferenced").unwrap();
        let shared = links.join("shared");
        fs::write(&shared, b"referenced").unwrap();
        fs::hard_link(&shared, tmp.path().join("user")).unwrap();

        clean_links(&links).unwrap();

        assert!(!dead.exists());
        assert!(shared.exists());
        // Missing .links dir is not an error.
        clean_links(&tmp.path().join("nope")).unwrap();
    }
}
