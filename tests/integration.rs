use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;

const SCHEMA: &str = include_str!("schema.sql");

struct TestStore {
    #[allow(dead_code)] // keep tempdir alive
    dir: tempfile::TempDir,
    store_dir: PathBuf,
    state_dir: PathBuf,
}

#[derive(Clone)]
struct Pkg {
    id: i64,
    path: PathBuf,
    full: String,
}

/// Derive a store hash from the package name so tests don't have to spell
/// out 32 chars by hand. Not a real hash, just stable and unique per name.
fn fake_hash(name: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let mut s = String::with_capacity(32);
    let mut x = h;
    for _ in 0..32 {
        s.push(b"0123456789abcdfghijklmnpqrsvwxyz"[(x & 0x1f) as usize] as char);
        x = x.rotate_left(5) ^ h;
    }
    s
}

impl TestStore {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let state_dir = dir.path().join("state");

        fs::create_dir_all(&store_dir).unwrap();
        for d in ["db", "gcroots", "profiles", "temproots"] {
            fs::create_dir_all(state_dir.join(d)).unwrap();
        }
        fs::create_dir_all(store_dir.join(".links")).unwrap();

        let conn = Connection::open(state_dir.join("db/db.sqlite")).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();

        TestStore {
            dir,
            store_dir,
            state_dir,
        }
    }

    fn db(&self) -> Connection {
        Connection::open(self.state_dir.join("db/db.sqlite")).unwrap()
    }

    fn add_path(&self, name: &str, nar_size: u64) -> Pkg {
        let hash = fake_hash(name);
        let basename = format!("{hash}-{name}");
        let path = self.store_dir.join(&basename);
        let full = format!("{}/{basename}", self.store_dir.display());

        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("file"), "x".repeat(nar_size as usize)).unwrap();

        let conn = self.db();
        conn.execute(
            "INSERT INTO ValidPaths (path, hash, registrationTime, narSize) VALUES (?, ?, 1000, ?)",
            rusqlite::params![full, format!("sha256:{hash}"), nar_size as i64],
        )
        .unwrap();
        Pkg {
            id: conn.last_insert_rowid(),
            path,
            full,
        }
    }

    fn add_ref(&self, referrer: &Pkg, reference: &Pkg) {
        self.db()
            .execute(
                "INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?, ?)",
                rusqlite::params![referrer.id, reference.id],
            )
            .unwrap();
    }

    fn set_deriver(&self, out: &Pkg, drv: &Pkg) {
        self.db()
            .execute(
                "UPDATE ValidPaths SET deriver = ? WHERE path = ?",
                rusqlite::params![drv.full, out.full],
            )
            .unwrap();
    }

    fn add_root(&self, root_name: &str, target: &Pkg) {
        let link = self.state_dir.join("gcroots").join(root_name);
        std::os::unix::fs::symlink(&target.path, &link).unwrap();
    }

    fn in_db(&self, p: &Pkg) -> bool {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM ValidPaths WHERE path = ?",
                [&p.full],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0
    }

    fn run_gc(&self, extra_args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_fast-nix-gc"))
            .arg("--store-dir")
            .arg(&self.store_dir)
            .arg("--state-dir")
            .arg(&self.state_dir)
            .args(extra_args)
            .output()
            .unwrap()
    }

    fn run_gc_ok(&self, extra_args: &[&str]) -> std::process::Output {
        let out = self.run_gc(extra_args);
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

#[test]
fn gc_deletes_unreferenced_paths() {
    let store = TestStore::new();

    let lib = store.add_path("lib", 100);
    let app = store.add_path("app", 200);
    store.add_ref(&app, &lib);
    store.add_root("app-root", &app);
    let dead = store.add_path("dead", 500);

    store.run_gc_ok(&[]);

    assert!(lib.path.exists() && app.path.exists());
    assert!(store.in_db(&lib) && store.in_db(&app));
    assert!(!dead.path.exists());
    assert!(!store.in_db(&dead));
}

#[test]
fn gc_dry_run_does_not_delete() {
    let store = TestStore::new();
    let dead = store.add_path("dead", 500);

    let out = store.run_gc_ok(&["--dry-run"]);

    assert!(dead.path.exists() && store.in_db(&dead));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 store paths would be deleted"),
        "stdout: {stdout}"
    );
}

#[test]
fn gc_keeps_transitive_closure() {
    let store = TestStore::new();

    let c = store.add_path("c", 10);
    let b = store.add_path("b", 10);
    let a = store.add_path("a", 10);
    store.add_ref(&a, &b);
    store.add_ref(&b, &c);
    store.add_root("root", &a);
    let trash = store.add_path("trash", 10);

    store.run_gc_ok(&[]);

    assert!(a.path.exists() && b.path.exists() && c.path.exists());
    assert!(!trash.path.exists());
}

#[test]
fn gc_handles_self_references() {
    let store = TestStore::new();

    let s = store.add_path("self-ref", 100);
    store.add_ref(&s, &s);
    store.add_root("root", &s);
    let trash = store.add_path("trash", 50);

    store.run_gc_ok(&[]);

    assert!(s.path.exists());
    assert!(!trash.path.exists());
}

#[test]
fn gc_removes_unknown_disk_entries() {
    let store = TestStore::new();

    let orphan = store
        .store_dir
        .join(format!("{}-orphan", fake_hash("orphan")));
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("stuff"), "data").unwrap();

    store.run_gc_ok(&[]);

    assert!(!orphan.exists());
}

#[test]
fn gc_max_freed_stops_early() {
    let store = TestStore::new();
    store.add_path("dead1", 100);
    store.add_path("dead2", 100);

    let out = store.run_gc_ok(&["--max-freed", "1"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 store paths deleted"), "stdout: {stdout}");
}

#[test]
fn gc_keeps_runtime_roots_from_open_fd() {
    use std::os::unix::process::CommandExt;
    let store = TestStore::new();

    let held = store.add_path("held", 100);
    let trash = store.add_path("trash", 100);

    // Inherit an fd into the child so the path shows up in the GC's
    // own /proc/<pid>/fd. Sandboxes can't always inspect other PIDs.
    let f = fs::File::open(held.path.join("file")).unwrap();
    let raw_fd = f.as_raw_fd();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fast-nix-gc"));
    cmd.arg("--store-dir")
        .arg(&store.store_dir)
        .arg("--state-dir")
        .arg(&store.state_dir);
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(raw_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(raw_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    drop(f);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(held.path.exists() && store.in_db(&held));
    assert!(!trash.path.exists());
}

#[test]
fn gc_keeps_runtime_roots_from_environ() {
    let store = TestStore::new();

    let held = store.add_path("held", 100);
    let trash = store.add_path("trash", 100);

    // Pin via the child's environment, scanned from /proc/<pid>/environ.
    let out = Command::new(env!("CARGO_BIN_EXE_fast-nix-gc"))
        .arg("--store-dir")
        .arg(&store.store_dir)
        .arg("--state-dir")
        .arg(&store.state_dir)
        .env("FAST_GC_TEST_PIN", &held.full)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(held.path.exists() && store.in_db(&held));
    assert!(!trash.path.exists());
}

#[test]
fn gc_runtime_roots_validated_against_db() {
    let store = TestStore::new();

    // On disk and held open, but not in the DB. Runtime-root scan must
    // not pin an invalid path.
    let orphan = store
        .store_dir
        .join(format!("{}-orphan", fake_hash("orphan")));
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("file"), "data").unwrap();
    let _fd = fs::File::open(orphan.join("file")).unwrap();

    store.run_gc_ok(&[]);

    assert!(!orphan.exists());
}

#[test]
fn gc_skips_locked_tmp_dir() {
    let store = TestStore::new();

    let tmp = store.store_dir.join("tmp-build-1234");
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("in-progress"), "data").unwrap();

    let f = fs::File::open(&tmp).unwrap();
    assert_eq!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    store.run_gc_ok(&[]);

    assert!(tmp.exists(), "locked tmp dir should survive");
    drop(f);
}

#[test]
fn gc_deletes_readonly_paths() {
    use std::os::unix::fs::PermissionsExt;
    let store = TestStore::new();

    let ro = store.add_path("readonly", 100);
    fs::set_permissions(&ro.path, fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(ro.path.join("file"), fs::Permissions::from_mode(0o444)).unwrap();

    store.run_gc_ok(&[]);

    assert!(!ro.path.exists());
}

#[test]
fn gc_ignores_invalid_store_basenames() {
    let store = TestStore::new();

    // gcroot pointing at something inside the store dir that isn't a
    // store path (no hash-name prefix) — must not become a root.
    let bogus = store.store_dir.join("not-a-store-path");
    fs::create_dir_all(&bogus).unwrap();
    std::os::unix::fs::symlink(&bogus, store.state_dir.join("gcroots/bogus")).unwrap();

    let trash = store.add_path("trash", 100);

    store.run_gc_ok(&[]);

    assert!(!trash.path.exists());
    assert!(!bogus.exists());
}

#[test]
fn gc_keeps_deriver_of_alive_path() {
    let store = TestStore::new();

    let drv = store.add_path("pkg.drv", 50);
    let out = store.add_path("pkg", 200);
    store.set_deriver(&out, &drv);
    store.add_root("out-root", &out);
    let trash = store.add_path("trash", 100);

    store.run_gc_ok(&[]);

    // keep-derivations: rooted output keeps its .drv alive.
    assert!(out.path.exists() && drv.path.exists());
    assert!(!trash.path.exists());
}

#[test]
fn gc_drops_partial_temp_root() {
    let store = TestStore::new();

    let complete = store.add_path("complete", 100);
    let partial = store.add_path("partial", 100);

    // Trailing entry without a NUL is a partial write — must not pin.
    let tmp_file = store.state_dir.join("temproots/99999");
    let mut data = complete.full.clone().into_bytes();
    data.push(0);
    data.extend_from_slice(partial.full.as_bytes());
    fs::write(&tmp_file, &data).unwrap();

    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tmp_file)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );

    store.run_gc_ok(&[]);

    assert!(complete.path.exists());
    assert!(!partial.path.exists());
    drop(f);
}

#[test]
fn gc_removes_stale_temp_roots_file() {
    let store = TestStore::new();

    // Temp roots file with no live owner — its roots are ignored.
    let dead = store.add_path("dead", 100);
    let tmp_file = store.state_dir.join("temproots/12345");
    let mut data = dead.full.clone().into_bytes();
    data.push(0);
    fs::write(&tmp_file, &data).unwrap();

    store.run_gc_ok(&[]);

    assert!(!tmp_file.exists());
    assert!(!dead.path.exists());
}

#[test]
fn gc_empty_store_is_noop() {
    let store = TestStore::new();
    let out = store.run_gc_ok(&[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("0 store paths deleted"), "stdout: {stdout}");
}
