//! SQLite store database access and in-memory reference graph.

use crate::HashMap;
use anyhow::{Context, Result, anyhow};
use harmonia_store_core::store_path::{StoreDir, StorePath};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Handle to the Nix store SQLite database.
pub struct NixDb {
    pub conn: Connection,
    pub store_dir: PathBuf,
    pub state_dir: PathBuf,
    pub real_store_dir: PathBuf,
    pub links_dir: PathBuf,
    /// Mirror of Nix's `keep-derivations` setting (default: true).
    /// When set, .drv files of alive outputs are kept alive, and alive
    /// .drv files keep their outputs alive.
    pub keep_derivations: bool,
    /// Mirror of Nix's `keep-outputs` setting (default: false).
    /// When set, outputs of alive derivations are kept alive, and alive
    /// outputs keep their derivers alive.
    pub keep_outputs: bool,
}

impl NixDb {
    pub fn open(store_dir: &Path, state_dir: &Path) -> Result<Self> {
        Self::open_with_mode(store_dir, state_dir, false)
    }

    /// Read-only open for dry runs: no journal-mode flip, no write lock,
    /// works on a read-only filesystem.
    pub fn open_read_only(store_dir: &Path, state_dir: &Path) -> Result<Self> {
        Self::open_with_mode(store_dir, state_dir, true)
    }

    fn open_with_mode(store_dir: &Path, state_dir: &Path, read_only: bool) -> Result<Self> {
        // "/nix/store/" must equal "/nix/store": every prefix comparison
        // against DB paths appends its own '/'. A trailing slash would make
        // the basename index empty and the whole store look dead.
        let store_dir = normalize_dir(store_dir);
        let store_dir = store_dir.as_path();
        let db_path = state_dir.join("db/db.sqlite");
        let rw_flag = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let conn = Connection::open_with_flags(&db_path, rw_flag | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("opening {}", db_path.display()))?;

        if !read_only {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        // Nix's schema relies on FK actions (DerivationOutputs/Refs rows
        // cascade when a ValidPaths row goes away), but SQLite only honors
        // them with the pragma on. Without it every invalidation leaks
        // orphaned DerivationOutputs rows.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // nix-daemon may hold short write locks; fail with SQLITE_BUSY only
        // after a grace period, like Nix's busy handler.
        conn.busy_timeout(std::time::Duration::from_secs(60))?;

        Ok(NixDb {
            conn,
            store_dir: store_dir.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            real_store_dir: store_dir.to_path_buf(),
            links_dir: store_dir.join(".links"),
            // Read the resolved nix.conf via `nix config show`; defaults
            // match Nix if it's not in PATH.
            keep_derivations: crate::nix_config::bool_setting("keep-derivations", true),
            keep_outputs: crate::nix_config::bool_setting("keep-outputs", false),
        })
    }

    /// Check whether a table exists in the SQLite database.
    fn has_table(conn: &Connection, name: &str) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            [name],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Load the full reference graph into memory. Walking it as an
    /// in-memory CSR is far cheaper than N point queries.
    pub fn load_graph(&self) -> Result<StoreGraph> {
        // Both queries must see the same snapshot, otherwise a path
        // registered between them ends up with missing edges.
        self.conn.execute_batch("BEGIN")?;
        let result = self.load_graph_in_txn();
        if result.is_err() {
            // Leave the connection usable for the caller.
            self.conn.execute_batch("ROLLBACK").ok();
        } else {
            self.conn.execute_batch("COMMIT")?;
        }
        result
    }

    fn load_graph_in_txn(&self) -> Result<StoreGraph> {
        // ids are dense autoincrement, so a Vec works as the id->idx map.
        let max_id: i64 =
            self.conn
                .query_row("SELECT IFNULL(MAX(id), 0) FROM ValidPaths", [], |r| {
                    r.get(0)
                })?;
        const MISSING: u32 = u32::MAX;
        let mut id_to_idx = vec![MISSING; (max_id as usize) + 1];

        let mut paths: Vec<String> = Vec::new();
        let mut nar_sizes: Vec<u64> = Vec::new();
        let mut registration_times: Vec<i64> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, path, narSize, registrationTime FROM ValidPaths")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                let nar: Option<i64> = row.get(2)?;
                let reg_time: i64 = row.get(3)?;
                let idx = paths.len() as u32;
                if (id as usize) < id_to_idx.len() {
                    id_to_idx[id as usize] = idx;
                }
                paths.push(path);
                nar_sizes.push(nar.unwrap_or(0).max(0) as u64);
                registration_times.push(reg_time);
            }
        }

        // CSR adjacency: flat target array + per-node offsets.
        let n = paths.len();
        let mut edges: Vec<(u32, u32)> = Vec::new();

        let mut add_edges = |conn: &Connection, sql: &str| -> Result<()> {
            let mut stmt = conn.prepare(sql)?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let from_id: i64 = row.get(0)?;
                let to_id: i64 = row.get(1)?;
                let from = *id_to_idx.get(from_id as usize).unwrap_or(&MISSING);
                let to = *id_to_idx.get(to_id as usize).unwrap_or(&MISSING);
                if from != MISSING && to != MISSING {
                    edges.push((from, to));
                }
            }
            Ok(())
        };

        // Refs table: direct references between store paths.
        add_edges(&self.conn, "SELECT referrer, reference FROM Refs")?;

        // Edge directions mirror Nix's computeFSClosure (misc.cc), which
        // the GC calls with includeOutputs = keep-outputs and
        // includeDerivers = keep-derivations:
        //   keep-derivations: alive output keeps its drv alive (output→drv)
        //   keep-outputs:     alive drv keeps its outputs alive (drv→output)
        //
        // drv↔output mappings come from three places:
        // - ValidPaths.deriver (all Nix versions; also covers CA outputs of
        //   Nix ≤2.34, whose Realisations table keys drvPath by content
        //   hash and so can't be joined against ValidPaths)
        // - DerivationOutputs (input-addressed)
        // - BuildTraceV3 (CA/dynamic derivations, Nix ≥2.35): drvPath is a
        //   store path *basename*, outputPath a full store path. Created
        //   lazily; skip if absent.
        let has_build_trace = (self.keep_derivations || self.keep_outputs)
            && Self::has_table(&self.conn, "BuildTraceV3")?;
        let store_prefix = format!("{}/", self.store_dir.display());

        if self.keep_derivations {
            // output → drv via ValidPaths.deriver
            add_edges(
                &self.conn,
                "SELECT v.id, d.id FROM ValidPaths v \
                 JOIN ValidPaths d ON d.path = v.deriver \
                 WHERE v.deriver IS NOT NULL",
            )?;
            // output → drv via DerivationOutputs (covers outputs whose
            // deriver column is unset)
            add_edges(
                &self.conn,
                "SELECT o.id, do2.drv FROM ValidPaths o \
                 JOIN DerivationOutputs do2 ON do2.path = o.path",
            )?;
            if has_build_trace {
                // output → drv via BuildTraceV3
                add_edges(
                    &self.conn,
                    &format!(
                        "SELECT o.id, d.id FROM BuildTraceV3 bt \
                         JOIN ValidPaths o ON o.path = bt.outputPath \
                         JOIN ValidPaths d ON d.path = '{store_prefix}' || bt.drvPath"
                    ),
                )?;
            }
        }

        if self.keep_outputs {
            // drv → output via DerivationOutputs
            add_edges(
                &self.conn,
                "SELECT do2.drv, o.id FROM DerivationOutputs do2 \
                 JOIN ValidPaths o ON o.path = do2.path",
            )?;
            // drv → output via ValidPaths.deriver
            add_edges(
                &self.conn,
                "SELECT d.id, v.id FROM ValidPaths v \
                 JOIN ValidPaths d ON d.path = v.deriver \
                 WHERE v.deriver IS NOT NULL",
            )?;
            if has_build_trace {
                // drv → output via BuildTraceV3
                add_edges(
                    &self.conn,
                    &format!(
                        "SELECT d.id, o.id FROM BuildTraceV3 bt \
                         JOIN ValidPaths d ON d.path = '{store_prefix}' || bt.drvPath \
                         JOIN ValidPaths o ON o.path = bt.outputPath"
                    ),
                )?;
            }
        }


        let mut ref_offsets = vec![0u32; n + 1];
        for &(from, _) in &edges {
            ref_offsets[from as usize + 1] += 1;
        }
        for i in 0..n {
            ref_offsets[i + 1] += ref_offsets[i];
        }
        let mut ref_targets = vec![0u32; edges.len()];
        let mut cursor = ref_offsets.clone();
        for &(from, to) in &edges {
            let pos = cursor[from as usize];
            ref_targets[pos as usize] = to;
            cursor[from as usize] += 1;
        }

        Ok(StoreGraph {
            paths,
            nar_sizes,
            registration_times,
            ref_offsets,
            ref_targets,
            store_prefix: format!("{}/", self.store_dir.display()),
        })
    }

    /// Return all valid store paths (full paths, e.g. `/nix/store/xxx-foo`).
    pub fn valid_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM ValidPaths")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `StoreDir` view of `store_dir`. Fails if it isn't valid UTF-8.
    pub fn store_dir_typed(&self) -> Result<StoreDir> {
        StoreDir::new(self.store_dir.clone())
            .map_err(|e| anyhow!("{}: {e}", self.store_dir.display()))
    }

    /// Like `valid_paths` but parsed into `StorePath`. Rows that don't parse
    /// (corrupt DB) are returned as an error rather than silently dropped.
    pub fn valid_store_paths(&self) -> Result<Vec<StorePath>> {
        let store_dir = self.store_dir_typed()?;
        let mut stmt = self.conn.prepare("SELECT path FROM ValidPaths")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| {
            let s = r?;
            store_dir
                .parse::<StorePath>(&s)
                .map_err(|e| anyhow!("invalid store path '{s}': {e}"))
        })
        .collect()
    }

    pub fn is_valid_path(&self, path: &str) -> Result<bool> {
        // Cached: called once per unknown-on-disk entry, which can be
        // many thousands after an interrupted nixos-install or nix copy.
        let mut stmt = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM ValidPaths WHERE path = ?")?;
        let n: i64 = stmt.query_row([path], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Remove paths from the DB in a single transaction.
    pub fn invalidate_paths<'a>(&self, paths: impl Iterator<Item = &'a str>) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            // Collect the ids of paths to delete into a temp table so
            // we can batch-delete their Refs in two statements instead
            // of one subquery per path.
            self.conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS DeadPaths (id INTEGER PRIMARY KEY)",
            )?;
            self.conn.execute_batch("DELETE FROM DeadPaths")?;
            {
                let mut ins = self
                    .conn
                    .prepare("INSERT INTO DeadPaths SELECT id FROM ValidPaths WHERE path = ?")?;
                for p in paths {
                    ins.execute([p])?;
                }
            }
            // Delete reference edges involving dead paths first.
            // Cycles among dead paths (A→B, B→A) would otherwise
            // violate the FK `ON DELETE RESTRICT` on Refs.reference.
            self.conn.execute_batch(
                "DELETE FROM Refs WHERE referrer IN (SELECT id FROM DeadPaths) \
                 OR reference IN (SELECT id FROM DeadPaths)",
            )?;
            // Same for the CA realisations of Nix ≤2.34:
            // RealisationsRefs.realisationReference is ON DELETE RESTRICT,
            // and our bulk DELETE is unordered (Nix avoids the constraint
            // by deleting one path at a time in topological order).
            if Self::has_table(&self.conn, "Realisations")? {
                self.conn.execute_batch(
                    "DELETE FROM RealisationsRefs WHERE referrer IN \
                         (SELECT id FROM Realisations WHERE outputPath IN \
                             (SELECT id FROM DeadPaths)) \
                     OR realisationReference IN \
                         (SELECT id FROM Realisations WHERE outputPath IN \
                             (SELECT id FROM DeadPaths)); \
                     DELETE FROM Realisations WHERE outputPath IN \
                         (SELECT id FROM DeadPaths)",
                )?;
            }
            self.conn
                .execute_batch("DELETE FROM ValidPaths WHERE id IN (SELECT id FROM DeadPaths)")?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(e)
            }
        }
    }
}

/// Strip trailing slashes (but keep a bare "/").
fn normalize_dir(dir: &Path) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = dir.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()))
}

/// In-memory snapshot of the store reference graph in CSR layout.
pub struct StoreGraph {
    /// node idx -> store path string
    pub paths: Vec<String>,
    /// node idx -> narSize (bytes)
    pub nar_sizes: Vec<u64>,
    /// node idx -> registrationTime (Unix epoch seconds)
    pub registration_times: Vec<i64>,
    /// CSR row offsets: refs of node i are ref_targets[ref_offsets[i]..ref_offsets[i+1]]
    pub ref_offsets: Vec<u32>,
    /// CSR column indices: flat array of referenced node idxs
    pub ref_targets: Vec<u32>,
    /// store dir prefix including trailing slash, e.g. "/nix/store/"
    pub store_prefix: String,
}

/// Basename -> idx index for the lookups during root finding and the
/// unknown-on-disk scan. Borrows from the StoreGraph; built once.
pub struct BasenameIndex<'g> {
    pub map: HashMap<&'g str, u32>,
    pub store_prefix: &'g str,
}

impl<'g> BasenameIndex<'g> {
    pub fn new(graph: &'g StoreGraph) -> Self {
        let mut map: HashMap<&'g str, u32> = HashMap::default();
        map.reserve(graph.paths.len());
        for (i, p) in graph.paths.iter().enumerate() {
            if let Some(b) = p.strip_prefix(&graph.store_prefix) {
                map.insert(b, i as u32);
            }
        }
        BasenameIndex {
            map,
            store_prefix: &graph.store_prefix,
        }
    }

    pub fn idx_of(&self, path: &str) -> Option<u32> {
        let b = path.strip_prefix(self.store_prefix)?;
        self.map.get(b).copied()
    }

    pub fn idx_of_basename(&self, basename: &str) -> Option<u32> {
        self.map.get(basename).copied()
    }
}

impl StoreGraph {
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    #[inline]
    pub fn refs(&self, node: u32) -> &[u32] {
        let start = self.ref_offsets[node as usize] as usize;
        let end = self.ref_offsets[node as usize + 1] as usize;
        &self.ref_targets[start..end]
    }

    /// Mark all nodes reachable from the given root indices.
    pub fn compute_closure(&self, roots: &[u32]) -> Vec<bool> {
        let mut alive = vec![false; self.len()];
        let mut stack: Vec<u32> = Vec::with_capacity(roots.len());
        for &r in roots {
            if !alive[r as usize] {
                alive[r as usize] = true;
                stack.push(r);
            }
        }
        while let Some(node) = stack.pop() {
            for &next in self.refs(node) {
                if !alive[next as usize] {
                    alive[next as usize] = true;
                    stack.push(next);
                }
            }
        }
        alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const H2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const H3: &str = "cccccccccccccccccccccccccccccccc";

    struct TestDb {
        #[allow(dead_code)] // keep tempdir alive
        dir: tempfile::TempDir,
        db: NixDb,
    }

    fn full(name: &str) -> String {
        format!("/nix/store/{name}")
    }

    fn setup() -> TestDb {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(state.join("db")).unwrap();
        let conn = Connection::open(state.join("db/db.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE ValidPaths (
                 id integer primary key autoincrement not null,
                 path text unique not null,
                 hash text not null,
                 registrationTime integer not null,
                 deriver text,
                 narSize integer
             );
             CREATE TABLE Refs (
                 referrer integer not null,
                 reference integer not null,
                 primary key (referrer, reference),
                 foreign key (reference) references ValidPaths(id) on delete restrict
             );
             CREATE TABLE DerivationOutputs (
                 drv integer not null,
                 id text not null,
                 path text not null,
                 primary key (drv, id),
                 foreign key (drv) references ValidPaths(id) on delete cascade
             );",
        )
        .unwrap();
        drop(conn);
        let mut db = NixDb::open(Path::new("/nix/store"), &state).unwrap();
        // Pin settings; NixDb::open reads them from the host's nix.conf.
        db.keep_derivations = false;
        db.keep_outputs = false;
        TestDb { dir, db }
    }

    fn add_path(db: &NixDb, name: &str, nar_size: i64, reg_time: i64) -> i64 {
        db.conn
            .execute(
                "INSERT INTO ValidPaths (path, hash, registrationTime, narSize) \
                 VALUES (?, 'sha256:x', ?, ?)",
                rusqlite::params![full(name), reg_time, nar_size],
            )
            .unwrap();
        db.conn.last_insert_rowid()
    }

    fn add_ref(db: &NixDb, referrer: i64, reference: i64) {
        db.conn
            .execute(
                "INSERT INTO Refs (referrer, reference) VALUES (?, ?)",
                rusqlite::params![referrer, reference],
            )
            .unwrap();
    }

    #[test]
    fn has_table_checks_existence() {
        let t = setup();
        assert!(NixDb::has_table(&t.db.conn, "ValidPaths").unwrap());
        assert!(!NixDb::has_table(&t.db.conn, "NoSuchTable").unwrap());
    }

    #[test]
    fn load_graph_builds_csr() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 100, 1000);
        let b = add_path(&t.db, &format!("{H2}-b"), 200, 2000);
        let c = add_path(&t.db, &format!("{H3}-c"), 300, 3000);
        // Two edges from one node exercise the CSR cursor advance.
        add_ref(&t.db, a, b);
        add_ref(&t.db, a, c);

        let g = t.db.load_graph().unwrap();
        assert_eq!(g.len(), 3);
        assert!(!g.is_empty());
        let ia = g
            .paths
            .iter()
            .position(|p| p == &full(&format!("{H1}-a")))
            .unwrap();
        let ib = g
            .paths
            .iter()
            .position(|p| p == &full(&format!("{H2}-b")))
            .unwrap();
        let ic = g
            .paths
            .iter()
            .position(|p| p == &full(&format!("{H3}-c")))
            .unwrap();
        assert_eq!(g.nar_sizes[ia], 100);
        assert_eq!(g.registration_times[ic], 3000);
        let mut refs_a = g.refs(ia as u32).to_vec();
        refs_a.sort();
        let mut expected = vec![ib as u32, ic as u32];
        expected.sort();
        assert_eq!(refs_a, expected);
        assert!(g.refs(ib as u32).is_empty());
        assert!(g.refs(ic as u32).is_empty());
    }

    #[test]
    fn empty_graph() {
        let t = setup();
        let g = t.db.load_graph().unwrap();
        assert_eq!(g.len(), 0);
        assert!(g.is_empty());
    }

    #[test]
    fn store_dir_typed_uses_configured_dir() {
        let t = setup();
        // Re-open with a non-default store dir; the typed view must not
        // fall back to /nix/store.
        let db = NixDb::open(Path::new("/custom/store"), &t.db.state_dir).unwrap();
        assert_eq!(db.store_dir_typed().unwrap().to_string(), "/custom/store");
    }

    #[test]
    fn load_graph_ignores_edges_with_unknown_ids() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 100, 1000);
        // Dangling edges must be dropped, not crash or corrupt the CSR.
        // FK enforcement default varies by SQLite build; this test wants
        // the corrupt rows.
        t.db.conn
            .pragma_update(None, "foreign_keys", "OFF")
            .unwrap();
        t.db.conn
            .execute_batch(&format!("INSERT INTO Refs VALUES ({a}, 9999), (9999, {a})"))
            .unwrap();
        let g = t.db.load_graph().unwrap();
        assert!(g.refs(0).is_empty());
    }

    #[test]
    fn load_graph_keep_derivations_adds_output_to_drv_edge() {
        let t = setup();
        let mut db = t.db;
        let drv = add_path(&db, &format!("{H1}-pkg.drv"), 10, 1000);
        let out = add_path(&db, &format!("{H2}-pkg"), 100, 1000);
        db.conn
            .execute(
                "UPDATE ValidPaths SET deriver = ? WHERE id = ?",
                rusqlite::params![full(&format!("{H1}-pkg.drv")), out],
            )
            .unwrap();
        let _ = drv;

        db.keep_derivations = true;
        let g = db.load_graph().unwrap();
        let iout = g.paths.iter().position(|p| p.ends_with("-pkg")).unwrap() as u32;
        let idrv = g.paths.iter().position(|p| p.ends_with(".drv")).unwrap() as u32;
        assert_eq!(g.refs(iout), &[idrv]);
        assert!(g.refs(idrv).is_empty());

        // keep-outputs adds the reverse edge.
        db.keep_outputs = true;
        let g = db.load_graph().unwrap();
        assert_eq!(g.refs(idrv), &[iout]);
    }

    #[test]
    fn load_graph_build_trace_edges() {
        let t = setup();
        let mut db = t.db;
        let drv_base = format!("{H1}-pkg.drv");
        add_path(&db, &drv_base, 10, 1000);
        add_path(&db, &format!("{H2}-pkg"), 100, 1000);
        db.conn
            .execute_batch(&format!(
                "CREATE TABLE BuildTraceV3 (
                     id integer primary key autoincrement not null,
                     drvPath text not null,
                     outputName text not null,
                     outputPath text not null,
                     signatures text
                 );
                 INSERT INTO BuildTraceV3 (drvPath, outputName, outputPath)
                 VALUES ('{drv_base}', 'out', '{}');",
                full(&format!("{H2}-pkg"))
            ))
            .unwrap();

        db.keep_derivations = true;
        let g = db.load_graph().unwrap();
        let iout = g.paths.iter().position(|p| p.ends_with("-pkg")).unwrap() as u32;
        let idrv = g.paths.iter().position(|p| p.ends_with(".drv")).unwrap() as u32;
        assert_eq!(g.refs(iout), &[idrv]);

        db.keep_derivations = false;
        db.keep_outputs = true;
        let g = db.load_graph().unwrap();
        assert_eq!(g.refs(idrv), &[iout]);
    }

    #[test]
    fn valid_paths_and_typed_variants() {
        let t = setup();
        add_path(&t.db, &format!("{H1}-a"), 1, 1);
        add_path(&t.db, &format!("{H2}-b"), 1, 1);

        let mut paths = t.db.valid_paths().unwrap();
        paths.sort();
        assert_eq!(
            paths,
            vec![full(&format!("{H1}-a")), full(&format!("{H2}-b"))]
        );

        let sd = t.db.store_dir_typed().unwrap();
        assert_eq!(sd.to_string(), "/nix/store");

        let sps = t.db.valid_store_paths().unwrap();
        assert_eq!(sps.len(), 2);
        let names: Vec<_> = sps.iter().map(|p| p.to_string()).collect();
        assert!(names.iter().any(|n| n.ends_with("-a")), "{names:?}");
    }

    #[test]
    fn is_valid_path_checks_db() {
        let t = setup();
        add_path(&t.db, &format!("{H1}-a"), 1, 1);
        assert!(t.db.is_valid_path(&full(&format!("{H1}-a"))).unwrap());
        assert!(!t.db.is_valid_path(&full(&format!("{H2}-b"))).unwrap());
    }

    #[test]
    fn invalidate_paths_handles_ref_cycles() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 1, 1);
        let b = add_path(&t.db, &format!("{H2}-b"), 1, 1);
        add_path(&t.db, &format!("{H3}-c"), 1, 1);
        // Cycle between the two dead paths; FK on delete restrict would
        // reject row-by-row deletion.
        add_ref(&t.db, a, b);
        add_ref(&t.db, b, a);

        let dead = [full(&format!("{H1}-a")), full(&format!("{H2}-b"))];
        t.db.invalidate_paths(dead.iter().map(|s| s.as_str()))
            .unwrap();

        assert_eq!(t.db.valid_paths().unwrap(), vec![full(&format!("{H3}-c"))]);
        let refs: i64 =
            t.db.conn
                .query_row("SELECT COUNT(*) FROM Refs", [], |r| r.get(0))
                .unwrap();
        assert_eq!(refs, 0);
    }

    #[test]
    fn invalidate_paths_cascades_derivation_outputs() {
        let t = setup();
        let drv = add_path(&t.db, &format!("{H1}-pkg.drv"), 1, 1);
        let out = add_path(&t.db, &format!("{H2}-pkg"), 1, 1);
        t.db.conn
            .execute(
                "INSERT INTO DerivationOutputs (drv, id, path) VALUES (?, 'out', ?)",
                rusqlite::params![drv, full(&format!("{H2}-pkg"))],
            )
            .unwrap();
        let _ = out;

        let dead = [full(&format!("{H1}-pkg.drv"))];
        t.db.invalidate_paths(dead.iter().map(|s| s.as_str()))
            .unwrap();

        let n: i64 =
            t.db.conn
                .query_row("SELECT COUNT(*) FROM DerivationOutputs", [], |r| r.get(0))
                .unwrap();
        assert_eq!(n, 0, "DerivationOutputs row orphaned");
    }

    #[test]
    fn invalidate_paths_clears_realisations_with_restrict_fk() {
        // Nix ≤2.34 CA schema: RealisationsRefs.realisationReference is ON
        // DELETE RESTRICT. Bulk-deleting two dead paths whose realisations
        // reference each other must not trip the constraint.
        let t = setup();
        t.db.conn
            .execute_batch(
                "CREATE TABLE Realisations (
                     id integer primary key autoincrement not null,
                     drvPath text not null,
                     outputName text not null,
                     outputPath integer not null references ValidPaths(id) on delete cascade,
                     signatures text
                 );
                 CREATE TABLE RealisationsRefs (
                     referrer integer not null,
                     realisationReference integer,
                     foreign key (referrer) references Realisations(id) on delete cascade,
                     foreign key (realisationReference) references Realisations(id) on delete restrict
                 );",
            )
            .unwrap();
        let a = add_path(&t.db, &format!("{H1}-a"), 1, 1);
        let b = add_path(&t.db, &format!("{H2}-b"), 1, 1);
        t.db.conn
            .execute_batch(&format!(
                "INSERT INTO Realisations (drvPath, outputName, outputPath) \
                     VALUES ('sha256:a!out', 'out', {a}), ('sha256:b!out', 'out', {b});
                 INSERT INTO RealisationsRefs (referrer, realisationReference) VALUES (1, 2);"
            ))
            .unwrap();

        let dead = [full(&format!("{H1}-a")), full(&format!("{H2}-b"))];
        t.db.invalidate_paths(dead.iter().map(|s| s.as_str()))
            .unwrap();

        for table in ["Realisations", "RealisationsRefs", "ValidPaths"] {
            let n: i64 =
                t.db.conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap();
            assert_eq!(n, 0, "{table} not cleared");
        }
    }

    #[test]
    fn basename_index_lookups() {
        let t = setup();
        add_path(&t.db, &format!("{H1}-a"), 1, 1);
        add_path(&t.db, &format!("{H2}-b"), 1, 1);
        add_path(&t.db, &format!("{H3}-c"), 1, 1);
        let g = t.db.load_graph().unwrap();
        let bidx = BasenameIndex::new(&g);

        let ic = g.paths.iter().position(|p| p.ends_with("-c")).unwrap() as u32;
        assert_eq!(bidx.idx_of(&full(&format!("{H3}-c"))), Some(ic));
        assert_eq!(bidx.idx_of_basename(&format!("{H3}-c")), Some(ic));
        assert_eq!(bidx.idx_of("/elsewhere/x"), None);
        assert_eq!(bidx.idx_of_basename("nope"), None);
    }

    #[test]
    fn compute_closure_marks_reachable_only() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 1, 1);
        let b = add_path(&t.db, &format!("{H2}-b"), 1, 1);
        let c = add_path(&t.db, &format!("{H3}-c"), 1, 1);
        add_ref(&t.db, a, b);
        // Cycle must terminate.
        add_ref(&t.db, b, a);
        let _ = c;

        let g = t.db.load_graph().unwrap();
        let ia = g.paths.iter().position(|p| p.ends_with("-a")).unwrap() as u32;
        let ic = g.paths.iter().position(|p| p.ends_with("-c")).unwrap();
        // Duplicate roots must not double-visit.
        let alive = g.compute_closure(&[ia, ia]);
        assert_eq!(alive.len(), 3);
        assert_eq!(alive.iter().filter(|&&x| x).count(), 2);
        assert!(!alive[ic]);
    }
}
