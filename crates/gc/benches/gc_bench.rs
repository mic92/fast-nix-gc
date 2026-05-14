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

const SCHEMA: &str = include_str!("../tests/schema.sql");

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
    fn new(n_paths: usize, n_roots: usize, refs_per_path: usize) -> Self {
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
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;")
            .unwrap();

        // Batch insert paths
        conn.execute_batch("BEGIN;").unwrap();
        for i in 0..n_paths {
            let hash = format!("{:0>32x}", i);
            let name = format!("pkg-{}", i);
            let full = format!("{}/{}-{}", store_dir.display(), hash, name);
            let disk = store_dir.join(format!("{}-{}", hash, name));

            fs::create_dir_all(&disk).unwrap();
            // Small file so disk ops are fast
            fs::write(disk.join("out"), &[0u8; 64]).unwrap();

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

        // Create GC root symlinks for the last n_roots paths
        for i in (n_paths - n_roots)..n_paths {
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

fn main() {
    println!("=== fast-nix-gc benchmarks ===\n");

    // Small store: 1000 paths, 10 roots, 3 refs each
    // ~500 alive (roots + transitive closure), ~500 dead
    println!("--- Small store (1,000 paths, 10 roots, 3 refs/path) ---");
    {
        let store = BenchStore::new(1_000, 10, 3);
        bench_run("dry-run (find dead)", 10, || {
            let out = store.run_gc(&["--dry-run"]);
            assert!(out.status.success());
        });
    }

    // Medium store: 10,000 paths, 100 roots, 5 refs each
    println!("\n--- Medium store (10,000 paths, 100 roots, 5 refs/path) ---");
    {
        let store = BenchStore::new(10_000, 100, 5);
        bench_run("dry-run (find dead)", 5, || {
            let out = store.run_gc(&["--dry-run"]);
            assert!(out.status.success());
        });
    }

    // Large store: 50,000 paths, 500 roots, 5 refs each
    println!("\n--- Large store (50,000 paths, 500 roots, 5 refs/path) ---");
    {
        let store = BenchStore::new(50_000, 500, 5);
        bench_run("dry-run (find dead)", 3, || {
            let out = store.run_gc(&["--dry-run"]);
            assert!(out.status.success());
        });
    }

    // Dense refs: 10,000 paths, 50 roots, 20 refs each
    // Most paths alive through dense connectivity
    println!("\n--- Dense refs (10,000 paths, 50 roots, 20 refs/path) ---");
    {
        let store = BenchStore::new(10_000, 50, 20);
        bench_run("dry-run (find dead)", 5, || {
            let out = store.run_gc(&["--dry-run"]);
            assert!(out.status.success());
        });
    }

    // Mostly dead: 10,000 paths, 5 roots, 2 refs each
    println!("\n--- Mostly dead (10,000 paths, 5 roots, 2 refs/path) ---");
    {
        let store = BenchStore::new(10_000, 5, 2);
        bench_run("dry-run (find dead)", 5, || {
            let out = store.run_gc(&["--dry-run"]);
            assert!(out.status.success());
        });

        // Actual deletion benchmark (separate store each time)
        bench_run("full GC (delete)", 3, || {
            let s = BenchStore::new(10_000, 5, 2);
            let out = s.run_gc(&[]);
            assert!(out.status.success());
        });
    }

    println!("\n=== done ===");
}
