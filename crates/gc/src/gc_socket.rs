//! GC roots socket, protocol-compatible with `nix-store --gc` (gc.cc).
//!
//! Builders that fail to acquire a shared `gc.lock` connect to
//! `state/gc-socket/socket`, send newline-terminated store paths, and wait
//! for a single `'1'` ack per line. We mark the closure protected before
//! acking so the builder can't recreate a path we are still unlinking.

use crate::db::StoreGraph;
use crate::{HashMap, HashSet};
use anyhow::{Context, Result};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

/// Liveness state shared between the deletion loop and the socket server.
/// Roots from clients can only flip dead -> protected; `pending` tracks
/// in-flight unlinks so `protect()` waits for them before acking.
pub struct LiveSet {
    inner: Mutex<LiveInner>,
    cond: Condvar,
}

struct LiveInner {
    /// Per-node "do not delete" flag, indexed by graph node id.
    protected: Vec<bool>,
    /// Basenames protected that are not (yet) in the graph. Covers paths a
    /// builder registers after our DB snapshot.
    protected_unknown: HashSet<String>,
    /// Graph nodes currently being deleted.
    pending_nodes: HashSet<u32>,
    /// Unknown-on-disk basenames currently being deleted.
    pending_unknown: HashSet<String>,
}

impl LiveSet {
    pub fn new(n_nodes: usize) -> Self {
        LiveSet {
            inner: Mutex::new(LiveInner {
                protected: vec![false; n_nodes],
                protected_unknown: HashSet::default(),
                pending_nodes: HashSet::default(),
                pending_unknown: HashSet::default(),
            }),
            cond: Condvar::new(),
        }
    }

    /// Atomically check that `node` is unprotected and mark it pending.
    /// Returns `false` (skip deletion) if the node was protected meanwhile.
    pub fn try_begin_delete_node(&self, node: u32) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.protected[node as usize] {
            return false;
        }
        g.pending_nodes.insert(node);
        true
    }

    pub fn end_delete_node(&self, node: u32) {
        let mut g = self.inner.lock().unwrap();
        g.pending_nodes.remove(&node);
        drop(g);
        self.cond.notify_all();
    }

    /// Atomically partition `nodes` into (claimed, skipped): skipped nodes
    /// are protected; claimed ones are marked pending in the same critical
    /// section. Claiming before the DB invalidation closes the race where
    /// `protect()` acks a path whose ValidPaths row is about to be deleted:
    /// any later `protect()` of a claimed node blocks until its unlink
    /// finished, so the builder re-checks validity and re-registers.
    pub fn claim_nodes(&self, nodes: &[u32]) -> (Vec<u32>, Vec<u32>) {
        let mut g = self.inner.lock().unwrap();
        let mut claimed = Vec::with_capacity(nodes.len());
        let mut skipped = Vec::new();
        for &n in nodes {
            if g.protected[n as usize] {
                skipped.push(n);
            } else {
                g.pending_nodes.insert(n);
                claimed.push(n);
            }
        }
        (claimed, skipped)
    }

    /// Same as `try_begin_delete_node` for paths not in the graph.
    pub fn try_begin_delete_unknown(&self, basename: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.protected_unknown.contains(basename) {
            return false;
        }
        g.pending_unknown.insert(basename.to_owned());
        true
    }

    pub fn end_delete_unknown(&self, basename: &str) {
        let mut g = self.inner.lock().unwrap();
        g.pending_unknown.remove(basename);
        drop(g);
        self.cond.notify_all();
    }

    /// Mark the closure of `basename` as protected and wait until none of
    /// it is still being deleted.
    fn protect(&self, basename: &str, graph: &StoreGraph, idx: &HashMap<String, u32>) {
        let mut g = self.inner.lock().unwrap();
        // Wait set is every closure node that is currently pending, not
        // just nodes we are first to protect: an earlier overlapping
        // protect() may have marked a node protected while it is still
        // mid-unlink, and acking before that finishes would let the client
        // recreate a path the deleter is about to remove.
        let mut wait_for: Vec<u32> = Vec::new();
        if let Some(&root) = idx.get(basename) {
            let mut stack = vec![root];
            let mut seen: HashSet<u32> = HashSet::default();
            while let Some(n) = stack.pop() {
                if !seen.insert(n) {
                    continue;
                }
                g.protected[n as usize] = true;
                if g.pending_nodes.contains(&n) {
                    wait_for.push(n);
                }
                stack.extend_from_slice(graph.refs(n));
            }
        } else {
            g.protected_unknown.insert(basename.to_owned());
        }

        let conflict = |g: &LiveInner| {
            wait_for.iter().any(|n| g.pending_nodes.contains(n))
                || g.pending_unknown.contains(basename)
        };
        while conflict(&g) {
            log::debug!("synchronising with deletion of {basename}");
            g = self.cond.wait(g).unwrap();
        }
    }
}

/// Running GC roots socket server. Dropping it tears down the listener,
/// removes the socket file, and joins the accept thread.
pub struct GcSocketServer {
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl GcSocketServer {
    /// Bind `state_dir/gc-socket/socket` and spawn the accept thread.
    pub fn start(state_dir: &Path, live: Arc<LiveSet>, graph: Arc<StoreGraph>) -> Result<Self> {
        let dir = state_dir.join("gc-socket");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let socket_path = dir.join("socket");
        // A previous GC may have crashed without cleaning up.
        let _ = fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;
        // Builders may run as a different user; mirror Nix's 0666.
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))?;

        // Owned basename -> node index. BasenameIndex borrows from the graph
        // and can't be sent across threads alongside an Arc<StoreGraph>.
        let mut idx: HashMap<String, u32> = HashMap::default();
        idx.reserve(graph.len());
        for (i, p) in graph.paths.iter().enumerate() {
            if let Some(b) = p.strip_prefix(&graph.store_prefix) {
                idx.insert(b.to_owned(), i as u32);
            }
        }
        let idx = Arc::new(idx);
        let store_prefix = graph.store_prefix.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("gc-socket".into())
            .spawn(move || accept_loop(listener, store_prefix, live, graph, idx, accept_shutdown))
            .context("spawning gc-socket thread")?;

        Ok(GcSocketServer {
            socket_path,
            shutdown,
            handle: Some(handle),
        })
    }
}

impl Drop for GcSocketServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn accept_loop(
    listener: UnixListener,
    store_prefix: String,
    live: Arc<LiveSet>,
    graph: Arc<StoreGraph>,
    idx: Arc<HashMap<String, u32>>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        let pfd = PollFd::new(listener.as_fd(), PollFlags::POLLIN);
        match poll(&mut [pfd], PollTimeout::from(10_u8)) {
            Ok(0) => continue,
            Err(e) => {
                // Don't hot-spin if poll fails persistently.
                log::debug!("gc-socket poll: {e}");
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            _ => {}
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let store_prefix = store_prefix.clone();
                let live = Arc::clone(&live);
                let graph = Arc::clone(&graph);
                let idx = Arc::clone(&idx);
                // Each builder keeps its connection open for the duration of
                // its build; handle them concurrently.
                let _ = std::thread::Builder::new()
                    .name("gc-socket-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle_client(stream, &store_prefix, &live, &graph, &idx) {
                            log::debug!("gc-socket client: {e}");
                        }
                    });
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                // Transient failures (EMFILE/ENFILE/ECONNABORTED) must not
                // kill the server: builders would stall for the rest of the
                // GC. Back off briefly and keep accepting.
                log::warn!("gc-socket accept: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

fn handle_client(
    stream: UnixStream,
    store_prefix: &str,
    live: &LiveSet,
    graph: &StoreGraph,
    idx: &HashMap<String, u32>,
) -> Result<()> {
    // Paths are bounded; a peer that streams gigabytes without a newline
    // must not OOM the GC (the socket is world-writable, like Nix's).
    const MAX_LINE: u64 = 64 * 1024;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.by_ref().take(MAX_LINE).read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        if n as u64 == MAX_LINE && !line.ends_with('\n') {
            anyhow::bail!("gc-socket: line too long");
        }
        let path = line.trim_end_matches('\n');
        if let Some(basename) = path.strip_prefix(store_prefix).filter(|b| !b.is_empty()) {
            log::debug!("got new GC root '{path}'");
            live.protect(basename, graph, idx);
        } else {
            log::warn!("gc-socket: received garbage instead of a root: {path:?}");
        }
        writer.write_all(b"1")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn graph(prefix: &str, paths: &[(&str, &[u32])]) -> StoreGraph {
        let n = paths.len();
        let mut ref_offsets = vec![0u32; n + 1];
        let mut ref_targets = Vec::new();
        for (i, (_, refs)) in paths.iter().enumerate() {
            ref_offsets[i + 1] = ref_offsets[i] + refs.len() as u32;
            ref_targets.extend_from_slice(refs);
        }
        StoreGraph {
            paths: paths.iter().map(|(p, _)| format!("{prefix}{p}")).collect(),
            nar_sizes: vec![0; n],
            registration_times: vec![0; n],
            ref_offsets,
            ref_targets,
            store_prefix: prefix.to_owned(),
        }
    }

    #[test]
    fn drop_returns_when_idle_accept_loop_is_waiting() {
        let g = Arc::new(graph("/nix/store/", &[]));
        let live = Arc::new(LiveSet::new(0));
        let dir = tempfile::tempdir().unwrap();
        let server = GcSocketServer::start(dir.path(), live, g).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            drop(server);
            let _ = tx.send(());
        });

        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("GcSocketServer::drop did not finish");
        handle.join().unwrap();
    }

    #[test]
    fn protect_marks_closure_and_blocks_deletion() {
        // 0 -> 1 -> 2, 3 standalone
        let g = Arc::new(graph(
            "/nix/store/",
            &[("a", &[1]), ("b", &[2]), ("c", &[]), ("d", &[])],
        ));
        let live = Arc::new(LiveSet::new(g.len()));
        let dir = tempfile::tempdir().unwrap();
        let server = GcSocketServer::start(dir.path(), Arc::clone(&live), Arc::clone(&g)).unwrap();
        let sock = dir.path().join("gc-socket/socket");

        let mut conn = UnixStream::connect(&sock).unwrap();
        conn.write_all(b"/nix/store/a\n").unwrap();
        let mut ack = [0u8; 1];
        conn.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [b'1']);

        // Closure of a (0,1,2) is protected; d (3) is still deletable.
        assert!(!live.try_begin_delete_node(0));
        assert!(!live.try_begin_delete_node(1));
        assert!(!live.try_begin_delete_node(2));
        assert!(live.try_begin_delete_node(3));
        live.end_delete_node(3);

        drop(server);
        assert!(!sock.exists());
    }

    #[test]
    fn protect_blocks_until_pending_delete_finishes() {
        let g = Arc::new(graph("/nix/store/", &[("x", &[])]));
        let live = Arc::new(LiveSet::new(g.len()));
        let dir = tempfile::tempdir().unwrap();
        let _server = GcSocketServer::start(dir.path(), Arc::clone(&live), Arc::clone(&g)).unwrap();
        let sock = dir.path().join("gc-socket/socket");

        // Simulate the deleter claiming node 0.
        assert!(live.try_begin_delete_node(0));

        let mut conn = UnixStream::connect(&sock).unwrap();
        conn.write_all(b"/nix/store/x\n").unwrap();
        // The ack must not arrive until end_delete_node is called.
        conn.set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let mut ack = [0u8; 1];
        assert!(conn.read_exact(&mut ack).is_err(), "ack arrived too early");

        live.end_delete_node(0);
        conn.set_read_timeout(None).unwrap();
        conn.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [b'1']);
        // After protection, a fresh delete attempt is refused.
        assert!(!live.try_begin_delete_node(0));
    }

    #[test]
    fn protect_waits_for_already_protected_pending_node() {
        // Two roots a -> c and b -> c share a leaf. While c is mid-unlink,
        // protecting a marks c protected and waits. A second protect for b
        // must also wait for c, not ack early just because c is already
        // protected.
        let g = Arc::new(graph(
            "/nix/store/",
            &[("a", &[2]), ("b", &[2]), ("c", &[])],
        ));
        let live = Arc::new(LiveSet::new(g.len()));
        let dir = tempfile::tempdir().unwrap();
        let _server = GcSocketServer::start(dir.path(), Arc::clone(&live), Arc::clone(&g)).unwrap();
        let sock = dir.path().join("gc-socket/socket");

        // c is in flight.
        assert!(live.try_begin_delete_node(2));

        let mut conn_a = UnixStream::connect(&sock).unwrap();
        conn_a.write_all(b"/nix/store/a\n").unwrap();
        conn_a
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let mut ack = [0u8; 1];
        assert!(conn_a.read_exact(&mut ack).is_err(), "a acked too early");

        // c is now protected (by a's protect) but still pending. b's
        // protect must not ack yet.
        let mut conn_b = UnixStream::connect(&sock).unwrap();
        conn_b.write_all(b"/nix/store/b\n").unwrap();
        conn_b
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        assert!(conn_b.read_exact(&mut ack).is_err(), "b acked too early");

        // c finishes; both unblock.
        live.end_delete_node(2);
        for conn in [&mut conn_a, &mut conn_b] {
            conn.set_read_timeout(None).unwrap();
            conn.read_exact(&mut ack).unwrap();
            assert_eq!(ack, [b'1']);
        }
    }

    #[test]
    fn claim_nodes_partitions_and_blocks_protect() {
        let g = Arc::new(graph("/nix/store/", &[("a", &[]), ("b", &[])]));
        let live = Arc::new(LiveSet::new(g.len()));
        let dir = tempfile::tempdir().unwrap();
        let _server = GcSocketServer::start(dir.path(), Arc::clone(&live), Arc::clone(&g)).unwrap();
        let sock = dir.path().join("gc-socket/socket");

        // Protect b up front; claim must skip it and claim a.
        let mut conn = UnixStream::connect(&sock).unwrap();
        conn.write_all(b"/nix/store/b\n").unwrap();
        let mut ack = [0u8; 1];
        conn.read_exact(&mut ack).unwrap();

        let (claimed, skipped) = live.claim_nodes(&[0, 1]);
        assert_eq!(claimed, vec![0]);
        assert_eq!(skipped, vec![1]);

        // A protect for the claimed node must block until its unlink is
        // done (the DB row is already gone; acking earlier would tell the
        // builder a deleted row is protected).
        conn.write_all(b"/nix/store/a\n").unwrap();
        conn.set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        assert!(conn.read_exact(&mut ack).is_err(), "acked mid-deletion");

        live.end_delete_node(0);
        conn.set_read_timeout(None).unwrap();
        conn.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [b'1']);
    }

    #[test]
    fn unknown_path_protected_by_basename() {
        let g = Arc::new(graph("/nix/store/", &[]));
        let live = Arc::new(LiveSet::new(0));
        let dir = tempfile::tempdir().unwrap();
        let _server = GcSocketServer::start(dir.path(), Arc::clone(&live), Arc::clone(&g)).unwrap();
        let sock = dir.path().join("gc-socket/socket");

        let mut conn = UnixStream::connect(&sock).unwrap();
        conn.write_all(b"/nix/store/zzz-fresh\n").unwrap();
        let mut ack = [0u8; 1];
        conn.read_exact(&mut ack).unwrap();

        assert!(!live.try_begin_delete_unknown("zzz-fresh"));
        assert!(live.try_begin_delete_unknown("other"));
        live.end_delete_unknown("other");
    }
}
