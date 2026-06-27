//! Garbage collection: liveness computation and store path deletion.

use crate::db::{BasenameIndex, NixDb};
use crate::gc_socket::{GcSocketServer, LiveSet};
use crate::roots::{find_roots, find_temp_roots};
use crate::{format_size, make_store_writable};
use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
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

/// Lock a `tmp-*` build dir before deleting. None means a builder still
/// holds it. Caller keeps the fd through deletion to avoid a TOCTOU race.
fn try_lock_dir(path: &Path) -> Option<Flock<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK: a stray FIFO named tmp-* must not block open() forever
    // while we hold the exclusive gc.lock.
    let f = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("cannot open {} for locking: {e}", path.display());
            }
            return None;
        }
    };
    Flock::lock(f, FlockArg::LockExclusiveNonblock).ok()
}

/// Same lock Nix takes. Builders hold it shared while registering temp
/// roots; we take it exclusive so the root set can't change under us.
fn acquire_gc_lock(state_dir: &Path) -> Result<Flock<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock_path = state_dir.join("gc.lock");
    // 0600 like Nix's openLockFile: a world-readable lock would let any
    // local user flock it and block GC and builders indefinitely.
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
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
    /// Skip the post-GC VACUUM. For busy builders, where concurrent
    /// readers keep the VACUUM's database-sized WAL from being
    /// truncated (the problem behind Nix commit 8299aaf).
    pub no_vacuum: bool,
    /// Max dead paths invalidated per DB transaction. Smaller keeps the
    /// WAL (and its disk use) lower; larger means fewer checkpoints.
    /// None uses the default.
    pub chunk_size: Option<usize>,
    /// Extra directories scanned for GC roots, like `gcroots`. Nix only
    /// scans its fixed state dirs, so roots outside them go uncounted.
    pub extra_gc_roots_dirs: Vec<std::path::PathBuf>,
}

/// Main GC: find roots, compute alive closure, delete dead paths.
pub fn collect_garbage(db: &NixDb, opts: &GcOptions) -> Result<(u64, usize)> {
    let dry_run = opts.dry_run;
    let max_freed = opts.max_freed;
    // Acquire the global GC lock before anything else. Builders take a
    // shared lock when adding temp roots; holding the exclusive lock
    // ensures no new roots appear after we scan them.
    // Free the reserved space file first (Nix: deletePath(reservedPath)).
    // On a 100% full disk the SQLite invalidation below needs room to
    // write before any store path has been unlinked.
    if !dry_run {
        let reserved = db.state_dir.join("db/reserved");
        match fs::remove_file(&reserved) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("cannot remove {}: {e}", reserved.display()),
        }
    }

    let _gc_lock = acquire_gc_lock(&db.state_dir)?;

    if !dry_run {
        make_store_writable(&db.real_store_dir)?;
    }

    // Serve the gc-socket immediately after taking the lock, like Nix:
    // builders that lost the shared-lock race retry connecting every
    // 100ms, so the socket must exist before the (potentially long)
    // graph load — and during dry runs, which hold the lock too.
    //
    // Phase 1: no graph yet, so every received root is acked instantly
    // and recorded by basename. That is sound because nothing can be
    // deleted before the graph exists; the roots are replayed below.
    let store_prefix = format!("{}/", db.store_dir.display());
    let early_live = Arc::new(LiveSet::new(0));
    // A dry run on a read-only state dir can still report; without the
    // socket no builder can run anyway (they need temproots).
    let start_socket = |live: Arc<LiveSet>, graph: Arc<crate::db::StoreGraph>| -> Result<_> {
        match GcSocketServer::start(&db.state_dir, live, graph) {
            Ok(s) => Ok(Some(s)),
            Err(e) if dry_run => {
                log::warn!("cannot serve gc-socket: {e:#}");
                Ok(None)
            }
            Err(e) => Err(e),
        }
    };
    let early_socket = start_socket(
        Arc::clone(&early_live),
        Arc::new(crate::db::StoreGraph::empty(store_prefix.clone())),
    )?;

    // Test sync point: block until the named fifo is readable, so tests
    // can deterministically exercise the early-socket window.
    if let Ok(p) = std::env::var("_FAST_NIX_GC_TEST_SYNC_EARLY") {
        let _ = fs::read(&p);
    }

    log::info!("loading store graph...");
    let graph = Arc::new(db.load_graph()?);
    log::info!("{} total valid paths", graph.len());

    // Phase 2: swap to the real server. Builders whose connection drops
    // during the swap reconnect (Nix's addTempRoot restart loop).
    drop(early_socket);
    let early_roots = early_live.protected_unknown_snapshot();
    let live = Arc::new(LiveSet::new(graph.len()));
    let _gc_socket = start_socket(Arc::clone(&live), Arc::clone(&graph))?;

    let bidx = BasenameIndex::new(&graph);

    // Replay phase-1 roots: known paths become ordinary GC roots (their
    // closure stays alive), unknown basenames stay protected.
    let mut early_root_idxs: Vec<u32> = Vec::new();
    for b in &early_roots {
        match bidx.idx_of_basename(b) {
            Some(i) => early_root_idxs.push(i),
            None => live.protect_unknown_basename(b),
        }
    }

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
    let mut roots = find_roots(
        &db.state_dir,
        &db.store_dir,
        &opts.extra_gc_roots_dirs,
        &bidx,
    )?;
    roots.extend(early_root_idxs);

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
    // Raw OsString names: a non-UTF-8 entry can't be in the DB (it stores
    // text), but it is still garbage that must be unlinked by its real
    // bytes — a lossy name would aim remove_file at a nonexistent path.
    let mut unknown_on_disk: Vec<std::ffi::OsString> = Vec::new();
    if let Ok(entries) = fs::read_dir(&db.real_store_dir) {
        for entry in entries.flatten() {
            let raw = entry.file_name();
            // read_dir never yields "." or "..".
            if let Some(name) = raw.to_str() {
                if name == ".links" {
                    continue;
                }
                // An entry shares an active build's hash part if its first
                // 32 chars match (covers `<path>.lock` and friends).
                let hash_part_active = name.len() >= 32
                    && name.is_char_boundary(32)
                    && temp_root_hashes.contains(&name[..32]);
                if bidx.idx_of_basename(name).is_some()
                    || temp_root_basenames.contains(name)
                    || hash_part_active
                {
                    continue;
                }
            }
            unknown_on_disk.push(raw);
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
        // Roots can arrive over the gc-socket while we run; a real GC
        // would honor them, so the report must too.
        let protected = live.protected_snapshot();
        let protected_unknown = live.protected_unknown_snapshot();
        for &node in &dead_indices {
            if estimated >= max {
                break;
            }
            if protected[node as usize] {
                continue;
            }
            writeln!(stdout, "{}", graph.paths[node as usize])?;
            estimated += graph.nar_sizes[node as usize];
            count += 1;
        }
        for name in &unknown_on_disk {
            if name.to_str().is_some_and(|n| protected_unknown.contains(n)) {
                continue;
            }
            writeln!(stdout, "{store_prefix}{}", name.display())?;
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

    // Referrer-first deletion order (Kahn over the dead subgraph), so
    // every prefix is safe to commit: still-valid paths never reference an
    // already-deleted one. in_degree counts a dead node's dead referrers
    // (graph.refs(r) lists what r references, so an edge r->m makes r a
    // referrer of m).
    let dead_ref = |node: u32, m: u32| m != node && !alive[m as usize];
    let mut in_degree = vec![0u32; graph.len()];
    for &node in &dead_indices {
        for &m in graph.refs(node).iter().filter(|&&m| dead_ref(node, m)) {
            in_degree[m as usize] += 1;
        }
    }
    let mut order: Vec<u32> = dead_indices
        .iter()
        .copied()
        .filter(|&m| in_degree[m as usize] == 0)
        .collect();
    let mut head = 0;
    while head < order.len() {
        let node = order[head];
        head += 1;
        for &m in graph.refs(node).iter().filter(|&&m| dead_ref(node, m)) {
            in_degree[m as usize] -= 1;
            if in_degree[m as usize] == 0 {
                order.push(m);
            }
        }
    }
    // Cyclic nodes never reach in-degree 0. They are mutually dependent,
    // so they share one trailing chunk that is never split.
    let acyclic_len = order.len();
    if order.len() < dead_indices.len() {
        order.extend(
            dead_indices
                .iter()
                .copied()
                .filter(|&m| in_degree[m as usize] != 0),
        );
    }

    // Commit in bounded chunks, truncating the WAL after each, so disk use
    // stays bounded and space is reclaimed incrementally.
    let max_chunk = opts.chunk_size.unwrap_or(65_536).max(1);
    let mut cursor = 0usize;
    while cursor < order.len() {
        let freed_so_far = bytes_freed.load(Ordering::Relaxed);
        if freed_so_far >= max {
            log::info!("deleted more than {max} bytes; stopping");
            break;
        }
        let remaining = max - freed_so_far;
        let mut chunk: Vec<u32> = Vec::new();
        // narSize over-reports hard-linked paths, so estimated only
        // bounds the chunk; actual freed bytes are re-checked above.
        let mut estimated = 0u64;
        // Take the cyclic tail (from acyclic_len on) whole and unsplit.
        let take_all = cursor >= acyclic_len;
        while cursor < order.len()
            && (take_all
                || (cursor < acyclic_len && estimated < remaining && chunk.len() < max_chunk))
        {
            let node = order[cursor];
            cursor += 1;
            estimated = estimated.saturating_add(graph.nar_sizes[node as usize]);
            chunk.push(node);
        }
        if chunk.is_empty() {
            break;
        }
        // Claim (mark pending) atomically with the protection check,
        // *before* invalidating DB rows. A protect() arriving later for a
        // claimed node blocks until the unlink finished, so a builder is
        // never acked while the row deletion is still in flight — it sees
        // a consistent "gone" state and re-registers.
        let (mut claimed, skipped) = live.claim_nodes(&chunk);
        // protect() marks closures atomically, but the claim is per node:
        // it may have kept a reference whose referrer got protected a
        // moment earlier. Drop the closures of skipped paths.
        if !skipped.is_empty() {
            let keep_out = graph.compute_closure(&skipped);
            claimed.retain(|&n| {
                if keep_out[n as usize] {
                    live.end_delete_node(n);
                    false
                } else {
                    true
                }
            });
        }
        // Invalidate rows before unlinking: builders trust isValidPath(),
        // so a path must never look valid after its disk entry is gone.
        // See alloy/gc_db_consistency.als.
        db.invalidate_ids(claimed.iter().map(|&n| graph.ids[n as usize]))?;
        claimed.par_iter().for_each(|&node| {
            let path = &graph.paths[node as usize];
            let basename = path.strip_prefix(&store_prefix).unwrap_or(path);
            let real_path = real_store_dir.join(basename);
            log::debug!("deleting '{path}'");
            match delete_store_path(&real_path) {
                Ok(freed) => {
                    bytes_freed.fetch_add(freed, Ordering::Relaxed);
                }
                // Row already invalidated; the leftover is picked up as
                // unknown-on-disk by the next run.
                Err(e) => log::warn!("failed to delete {}: {e:#}", real_path.display()),
            }
            live.end_delete_node(node);
        });
        let done =
            paths_deleted.fetch_add(claimed.len() as u64, Ordering::Relaxed) + claimed.len() as u64;
        log::debug!(
            "deleted {done}/{} dead paths, {} freed",
            order.len(),
            format_size(bytes_freed.load(Ordering::Relaxed)),
        );
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
        unknown_on_disk.retain(|name| {
            // A non-UTF-8 name can't be a DB path (the DB stores text).
            let Some(name) = name.to_str() else {
                return true;
            };
            match db.is_valid_path(&format!("{store_prefix}{name}")) {
                Ok(valid) => !valid,
                Err(e) => {
                    log::warn!("skipping {store_prefix}{name}: validity check failed: {e}");
                    false
                }
            }
        });
        unknown_on_disk.par_iter().for_each(|raw| {
            // The liveset/protocol key is textual; non-UTF-8 names can't
            // collide with anything a builder protects.
            let name = raw.to_string_lossy();
            if !live.try_begin_delete_unknown(&name) {
                return;
            }
            let real_path = real_store_dir.join(raw);
            // Only a directory can be a build temp dir a builder still holds;
            // a stray tmp-* FIFO would fail flock() on macOS and be kept forever.
            let is_locked_candidate = name.starts_with("tmp-")
                && real_path
                    .symlink_metadata()
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
            let _tmp_lock = if is_locked_candidate {
                match try_lock_dir(&real_path) {
                    Some(f) => Some(f),
                    None => {
                        log::debug!("skipping locked tempdir {}", real_path.display());
                        live.end_delete_unknown(&name);
                        return;
                    }
                }
            } else {
                None
            };
            log::debug!("deleting '{store_prefix}{name}'");
            match delete_store_path(&real_path) {
                Ok(freed) => {
                    bytes_freed.fetch_add(freed, Ordering::Relaxed);
                    paths_deleted.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => log::warn!("failed to delete {}: {e:#}", real_path.display()),
            }
            live.end_delete_unknown(&name);
        });
    }

    let bytes_freed = bytes_freed.into_inner();
    let paths_deleted = paths_deleted.into_inner() as usize;

    // Clean up unused hard links in .links
    let bytes_freed = bytes_freed + clean_links(&db.links_dir)?;

    // Reclaim db space freed by the row deletions, still under the
    // exclusive gc.lock. Best effort: a failed vacuum leaves the db valid.
    if !opts.no_vacuum
        && let Err(e) = db.maybe_vacuum()
    {
        log::warn!("vacuuming database failed: {e:#}");
    }

    Ok((bytes_freed, paths_deleted))
}

/// Remove hard links with link count 1 from .links directory, returning
/// bytes freed. The .links dir can contain millions of entries; stat +
/// unlink per entry is disk-bound, so process in parallel. Stream the
/// entries instead of collecting them: a Vec of millions of DirEntries
/// costs gigabytes of RSS.
fn clean_links(links_dir: &Path) -> Result<u64> {
    log::info!("deleting unused links...");
    let entries = match fs::read_dir(links_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            log::warn!("cannot read {}: {e}", links_dir.display());
            return Ok(0);
        }
    };

    // A link entry has one reference from .links plus one per store file.
    // N references total mean hard linking saves (N-2)*size compared to
    // independent copies.
    let saved_bytes = AtomicU64::new(0);
    let freed_bytes = AtomicU64::new(0);

    entries.flatten().par_bridge().for_each(|entry| {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return;
        };
        if meta.nlink() != 1 {
            saved_bytes.fetch_add(
                meta.nlink().saturating_sub(2) * meta.size(),
                Ordering::Relaxed,
            );
            return;
        }
        if fs::remove_file(&path).is_ok() {
            freed_bytes.fetch_add(meta.blocks() * 512, Ordering::Relaxed);
        }
    });

    let saving = saved_bytes.into_inner();
    if saving > 0 {
        log::info!("hard linking is currently saving {}", format_size(saving));
    }

    Ok(freed_bytes.into_inner())
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

        let dead_blocks = fs::symlink_metadata(&dead).unwrap().blocks() * 512;
        let freed = clean_links(&links).unwrap();

        assert!(!dead.exists());
        assert!(shared.exists());
        assert_eq!(freed, dead_blocks, "freed bytes of removed links");
        // Missing .links dir is not an error.
        assert_eq!(clean_links(&tmp.path().join("nope")).unwrap(), 0);
    }
}
