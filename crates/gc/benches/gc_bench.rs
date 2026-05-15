//! Benchmarks for fast-nix-gc using a synthetic Nix store.
//!
//! Creates a fake store with configurable number of paths and reference
//! density, then measures root finding, closure computation, and full
//! GC dry-run time.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use rusqlite::Connection;

use harmonia_store_db::{OpenMode, StoreDb};

struct BenchStore {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    store_dir: PathBuf,
    state_dir: PathBuf,
}

impl BenchStore {
    /// Build a synthetic store.
    ///
    /// - `n_paths`: total valid store paths
    /// - `n_roots`: how many are GC roots
    /// - `refs_per_path`: average references per path (capped to existing paths)
    /// - `roots_at_start`: refs go i -> i-1, i-2, so tail roots keep
    ///   everything alive. Start roots leave the rest dead, for deletion
    ///   benchmarks.
    fn new(n_paths: usize, n_roots: usize, refs_per_path: usize, roots_at_start: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let state_dir = dir.path().join("state");

        fs::create_dir_all(&store_dir).unwrap();
        fs::create_dir_all(state_dir.join("db")).unwrap();
        fs::create_dir_all(state_dir.join("gcroots")).unwrap();
        fs::create_dir_all(state_dir.join("profiles")).unwrap();
        fs::create_dir_all(state_dir.join("temproots")).unwrap();
        fs::create_dir_all(store_dir.join(".links")).unwrap();

        let db_path = state_dir.join("db/db.sqlite");
        let db = StoreDb::open(&db_path, OpenMode::Create).unwrap();
        db.create_schema().unwrap();
        drop(db);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA synchronous=OFF;").unwrap();

        // Batch insert paths
        conn.execute_batch("BEGIN;").unwrap();
        for i in 0..n_paths {
            let hash = format!("{:0>32x}", i);
            let name = format!("pkg-{}", i);
            let full = format!("{}/{}-{}", store_dir.display(), hash, name);
            let disk = store_dir.join(format!("{}-{}", hash, name));

            fs::create_dir_all(&disk).unwrap();
            // Small file so disk ops are fast
            fs::write(disk.join("out"), [0u8; 64]).unwrap();

            conn.execute(
                "INSERT INTO ValidPaths (path, hash, registrationTime, narSize) VALUES (?, ?, 1000, 1024)",
                rusqlite::params![full, format!("sha256:{}", hash)],
            )
            .unwrap();
        }
        conn.execute_batch("COMMIT;").unwrap();

        // Add references: path i references paths [i-1, i-2, ..., i-refs_per_path]
        conn.execute_batch("BEGIN;").unwrap();
        for i in 0..n_paths {
            let referrer_id = (i + 1) as i64; // autoincrement starts at 1
            for j in 1..=refs_per_path {
                if i >= j {
                    let ref_id = (i - j + 1) as i64;
                    conn.execute(
                        "INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?, ?)",
                        rusqlite::params![referrer_id, ref_id],
                    )
                    .unwrap();
                }
            }
        }
        conn.execute_batch("COMMIT;").unwrap();

        let root_range = if roots_at_start {
            0..n_roots
        } else {
            (n_paths - n_roots)..n_paths
        };
        for i in root_range {
            let hash = format!("{:0>32x}", i);
            let name = format!("pkg-{}", i);
            let target = store_dir.join(format!("{}-{}", hash, name));
            let link = state_dir.join("gcroots").join(format!("root-{}", i));
            std::os::unix::fs::symlink(&target, &link).unwrap();
        }

        drop(conn);

        BenchStore {
            dir,
            store_dir,
            state_dir,
        }
    }

    fn run_gc(&self, extra_args: &[&str]) -> std::process::Output {
        let bin = env!("CARGO_BIN_EXE_fast-nix-gc");
        Command::new(bin)
            .arg("--store-dir")
            .arg(&self.store_dir)
            .arg("--state-dir")
            .arg(&self.state_dir)
            .args(extra_args)
            .output()
            .unwrap()
    }
}

fn bench_run(label: &str, iterations: u32, f: impl Fn()) {
    // Warmup
    f();

    let start = Instant::now();
    for _ in 0..iterations {
        f();
    }
    let elapsed = start.elapsed();
    let per_iter = elapsed / iterations;
    println!(
        "{:<40} {:>8.2}ms avg ({} iters, {:.2}s total)",
        label,
        per_iter.as_secs_f64() * 1000.0,
        iterations,
        elapsed.as_secs_f64(),
    );
}

/// Parse "<N> store paths deleted" from fast-nix-gc stdout.
fn deleted_count(out: &std::process::Output) -> usize {
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|l| l.strip_suffix(" freed").and_then(|l| l.split(' ').next()))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("could not parse deleted count from: {stdout}"))
}

/// Build stores up front so only the GC run is timed, and assert the
/// deleted count so a graph with 0 dead paths fails instead of silently
/// benchmarking a no-op.
fn bench_delete(label: &str, iterations: u32, n_paths: usize, n_roots: usize, args: &[&str]) {
    let expected_dead = n_paths - n_roots;
    // +1 for warmup.
    let mut stores: Vec<BenchStore> = (0..iterations + 1)
        .map(|_| BenchStore::new(n_paths, n_roots, 2, true))
        .collect();
    let warmup = stores.pop().unwrap();
    let out = warmup.run_gc(args);
    assert!(out.status.success());
    assert_eq!(
        deleted_count(&out),
        expected_dead,
        "benchmark store has unexpected dead set; deletion not exercised"
    );

    let start = Instant::now();
    for s in &stores {
        let out = s.run_gc(args);
        assert!(out.status.success());
        assert_eq!(deleted_count(&out), expected_dead);
    }
    let elapsed = start.elapsed();
    println!(
        "{:<40} {:>8.2}ms avg ({} iters, {:.2}s total, {} deleted/iter)",
        label,
        elapsed.as_secs_f64() * 1000.0 / iterations as f64,
        iterations,
        elapsed.as_secs_f64(),
        expected_dead,
    );
}

const SCENARIOS: &[&str] = &["small", "medium", "large", "dense", "delete", "ensure-free"];

fn main() {
    // `cargo bench` always appends `--bench`; ignore it.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "usage: gc_bench [SCENARIO ...]\nscenarios: {}",
            SCENARIOS.join(", ")
        );
        return;
    }
    for a in &args {
        if !SCENARIOS.contains(&a.as_str()) {
            eprintln!(
                "unknown scenario {a:?}; available: {}",
                SCENARIOS.join(", ")
            );
            std::process::exit(1);
        }
    }
    let want = |s: &str| args.is_empty() || args.iter().any(|a| a == s);

    println!("=== fast-nix-gc benchmarks ===\n");

    if want("small") {
        println!("--- small (1,000 paths, 10 roots, 3 refs/path) ---");
        let store = BenchStore::new(1_000, 10, 3, false);
        bench_run("dry-run (find dead)", 10, || {
            assert!(store.run_gc(&["--dry-run"]).status.success());
        });
    }

    if want("medium") {
        println!("\n--- medium (10,000 paths, 100 roots, 5 refs/path) ---");
        let store = BenchStore::new(10_000, 100, 5, false);
        bench_run("dry-run (find dead)", 5, || {
            assert!(store.run_gc(&["--dry-run"]).status.success());
        });
    }

    if want("large") {
        println!("\n--- large (50,000 paths, 500 roots, 5 refs/path) ---");
        let store = BenchStore::new(50_000, 500, 5, false);
        bench_run("dry-run (find dead)", 3, || {
            assert!(store.run_gc(&["--dry-run"]).status.success());
        });
    }

    if want("dense") {
        println!("\n--- dense (10,000 paths, 50 roots, 20 refs/path) ---");
        let store = BenchStore::new(10_000, 50, 20, false);
        bench_run("dry-run (find dead)", 5, || {
            assert!(store.run_gc(&["--dry-run"]).status.success());
        });
    }

    if want("delete") {
        println!("\n--- delete (50,000 paths, 5 roots, 2 refs/path) ---");
        bench_delete("full GC (delete)", 3, 50_000, 5, &[]);
    }

    if want("ensure-free") {
        println!("\n--- ensure-free (50,000 paths, 5 roots, 2 refs/path) ---");
        // Target must exceed available space or --ensure-free is a no-op.
        bench_delete(
            "GC --ensure-free 9999T",
            3,
            50_000,
            5,
            &["--ensure-free", "9999T"],
        );
    }

    println!("\n=== done ===");
}
