//! Property-based tests for GC correctness.
//!
//! Tests the core graph logic (load, closure, topo sort, invalidate)
//! directly without subprocess overhead or disk deletion.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;

use fast_nix_gc::db::NixDb;
use proptest::prelude::*;
use rusqlite::Connection;

use harmonia_store_db::{OpenMode, StoreDb};

fn fake_hash(i: usize) -> String {
    format!("{i:032x}")
}

fn setup_db(
    n: usize,
    edges: &[(usize, usize)],
    reg_times: &[i64],
) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let state_dir = dir.path().join("state");

    fs::create_dir_all(&store_dir).unwrap();
    for d in ["db", "gcroots", "profiles", "temproots"] {
        fs::create_dir_all(state_dir.join(d)).unwrap();
    }
    fs::create_dir_all(store_dir.join(".links")).unwrap();

    let db_path = state_dir.join("db/db.sqlite");
    let db = StoreDb::open(&db_path, OpenMode::Create).unwrap();
    db.create_schema().unwrap();
    drop(db);
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("BEGIN").unwrap();

    for (i, &reg_time) in reg_times.iter().enumerate().take(n) {
        let hash = fake_hash(i);
        let basename = format!("{hash}-pkg-{i}");
        let full = format!("{}/{basename}", store_dir.display());
        let path = store_dir.join(&basename);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("file"), "x").unwrap();

        conn.execute(
            "INSERT INTO ValidPaths (path, hash, registrationTime, narSize) \
             VALUES (?1, ?2, ?3, 100)",
            rusqlite::params![full, format!("sha256:{hash}"), reg_time],
        )
        .unwrap();
    }

    for &(from, to) in edges {
        conn.execute(
            "INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?1, ?2)",
            rusqlite::params![(from + 1) as i64, (to + 1) as i64],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();

    (dir, store_dir, state_dir)
}

/// Reference BFS.
fn reference_alive(n: usize, edges: &[(usize, usize)], roots: &[usize]) -> Vec<bool> {
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for &(from, to) in edges {
        adj[from].push(to);
    }
    let mut alive = vec![false; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &r in roots {
        if !alive[r] {
            alive[r] = true;
            queue.push_back(r);
        }
    }
    while let Some(node) = queue.pop_front() {
        for &next in &adj[node] {
            if !alive[next] {
                alive[next] = true;
                queue.push_back(next);
            }
        }
    }
    alive
}

type GraphInput = (usize, Vec<(usize, usize)>, Vec<usize>, Vec<i64>, i64);

fn graph_strategy() -> impl Strategy<Value = GraphInput> {
    (2usize..50).prop_flat_map(|n| {
        let edges = prop::collection::vec((0..n, 0..n), 0..n * 2);
        let roots = prop::collection::vec(0..n, 1..=n.min(10));
        let reg_times = prop::collection::vec(0i64..1000, n);
        let cutoff = 0i64..1000;
        (Just(n), edges, roots, reg_times, cutoff)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Core graph logic: load_graph + compute_closure produce correct
    /// alive/dead sets, and invalidate_paths handles cycles.
    #[test]
    fn graph_closure_matches_reference(
        (n, edges, roots, reg_times, cutoff) in graph_strategy()
    ) {
        let (_dir, store_dir, state_dir) = setup_db(n, &edges, &reg_times);
        let db = NixDb::open(&store_dir, &state_dir).unwrap();
        let graph = db.load_graph().unwrap();

        let bidx = fast_nix_gc::db::BasenameIndex::new(&graph);

        // Map root node indices, plus paths registered at/after the cutoff
        // (mirroring --keep-recent).
        let root_indices: Vec<u32> = roots.iter()
            .filter_map(|&r| {
                let hash = fake_hash(r);
                let basename = format!("{hash}-pkg-{r}");
                bidx.idx_of_basename(&basename)
            })
            .chain(
                graph.registration_times.iter().enumerate()
                    .filter(|&(_, &t)| t >= cutoff)
                    .map(|(i, _)| i as u32)
            )
            .collect();

        let alive = graph.compute_closure(&root_indices);

        // Reference model: roots + recently registered nodes.
        let recent_roots: Vec<usize> = roots.iter().copied()
            .chain((0..n).filter(|&i| reg_times[i] >= cutoff))
            .collect();
        let expected = reference_alive(n, &edges, &recent_roots);

        // Safety + completeness of closure
        for (i, &want) in expected.iter().enumerate().take(n) {
            let hash = fake_hash(i);
            let full = format!("{}/{hash}-pkg-{i}", store_dir.display());
            if let Some(idx) = bidx.idx_of(&full) {
                prop_assert!(
                    alive[idx as usize] == want,
                    "mismatch for pkg-{}: got alive={}, expected={}",
                    i, alive[idx as usize], want
                );
            }
        }

        // Invalidation must not violate FK constraints (cycles etc.)
        let dead_paths: Vec<String> = (0..n)
            .filter(|&i| !expected[i])
            .map(|i| {
                let hash = fake_hash(i);
                format!("{}/{hash}-pkg-{i}", store_dir.display())
            })
            .collect();

        db.invalidate_paths(dead_paths.iter().map(|s| s.as_str())).unwrap();

        // Verify DB state
        let remaining: Vec<String> = {
            let mut stmt = db.conn.prepare("SELECT path FROM ValidPaths").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        let alive_count = expected.iter().filter(|&&a| a).count();
        prop_assert_eq!(remaining.len(), alive_count);
    }
}
