//! Benchmarks fast-nix-optimise against a synthetic store. Vary the unique
//! blob count to control the dedup ratio; cold runs hash and link, warm runs
//! hit the .links/ inode fast path.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use rusqlite::Connection;

struct BenchStore {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    store_dir: PathBuf,
    state_dir: PathBuf,
}

impl BenchStore {
    /// Build a synthetic store.
    ///
    /// - `n_paths`: number of valid store paths
    /// - `files_per_path`: regular files in each path
    /// - `unique_blobs`: pool of distinct file contents; smaller pool means
    ///   more dedup opportunity
    fn new(n_paths: usize, files_per_path: usize, unique_blobs: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&store_dir).unwrap();
        fs::create_dir_all(state_dir.join("db")).unwrap();

        // Distinct payloads, each ~1 KiB.
        let blobs: Vec<Vec<u8>> = (0..unique_blobs)
            .map(|i| format!("blob-{i:08}\n").repeat(80).into_bytes())
            .collect();

        let conn = Connection::open(state_dir.join("db/db.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE ValidPaths (id INTEGER PRIMARY KEY, path TEXT NOT NULL);")
            .unwrap();
        conn.execute_batch("BEGIN;").unwrap();

        // nix32 alphabet excludes e/o/u/t; use a..d as digits.
        const ALPHABET: [u8; 4] = [b'a', b'b', b'c', b'd'];
        let fake_hash = |mut i: usize| -> String {
            let mut s = vec![b'0'; 32];
            for c in s.iter_mut().rev() {
                *c = ALPHABET[i % 4];
                i /= 4;
            }
            String::from_utf8(s).unwrap()
        };

        let mut blob_idx = 0usize;
        for p in 0..n_paths {
            let hash = fake_hash(p);
            let path = store_dir.join(format!("{hash}-pkg{p}"));
            fs::create_dir_all(&path).unwrap();
            for f in 0..files_per_path {
                let fp = path.join(format!("file{f}"));
                fs::write(&fp, &blobs[blob_idx % unique_blobs]).unwrap();
                fs::set_permissions(&fp, fs::Permissions::from_mode(0o444)).unwrap();
                blob_idx += 1;
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o555)).unwrap();
            conn.execute(
                "INSERT INTO ValidPaths (path) VALUES (?)",
                [path.to_str().unwrap()],
            )
            .unwrap();
        }
        conn.execute_batch("COMMIT;").unwrap();

        BenchStore {
            dir,
            store_dir,
            state_dir,
        }
    }

    fn run(&self, extra: &[&str]) {
        let bin = env!("CARGO_BIN_EXE_fast-nix-optimise");
        let out = Command::new(bin)
            .arg("--store-dir")
            .arg(&self.store_dir)
            .arg("--state-dir")
            .arg(&self.state_dir)
            .args(extra)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "fast-nix-optimise failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn make_writable(&self) {
        // Allow tempdir to remove the read-only store paths on drop.
        for e in fs::read_dir(&self.store_dir).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(0o755));
            }
        }
    }
}

fn time(label: &str, f: impl FnOnce()) {
    let t0 = Instant::now();
    f();
    println!(
        "{:<35} {:>8.2}ms",
        label,
        t0.elapsed().as_secs_f64() * 1000.0
    );
}

fn bench(n_paths: usize, files_per_path: usize, unique_blobs: usize) {
    let total_files = n_paths * files_per_path;
    println!(
        "--- {n_paths} paths × {files_per_path} files, {unique_blobs} unique blobs ({total_files} files total) ---"
    );
    let store = BenchStore::new(n_paths, files_per_path, unique_blobs);
    time("dry-run (cold, hash everything)", || {
        store.run(&["--dry-run"])
    });
    time("optimise (cold, link)", || store.run(&[]));
    time("optimise (warm, all in .links)", || store.run(&[]));
    store.make_writable();
    println!();
}

fn main() {
    println!("=== fast-nix-optimise benchmarks ===\n");
    // High dedup: many copies of few blobs, lots of hardlink work.
    bench(200, 25, 100);
    // Low dedup: mostly unique blobs, hashing dominates.
    bench(500, 20, 8_000);
}
