//! SQLite store database access and in-memory reference graph.

use crate::HashMap;
use anyhow::{Context, Result};
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
        let db_path = state_dir.join("db/db.sqlite");
        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", db_path.display()))?;

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        Ok(NixDb {
            conn,
            store_dir: store_dir.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            real_store_dir: store_dir.to_path_buf(),
            links_dir: store_dir.join(".links"),
            keep_derivations: true,
            keep_outputs: false,
        })
    }

    /// Load the full reference graph into memory. Walking it as an
    /// in-memory CSR is far cheaper than N point queries.
    pub fn load_graph(&self) -> Result<StoreGraph> {
        // Both queries must see the same snapshot, otherwise a path
        // registered between them ends up with missing edges.
        self.conn.execute_batch("BEGIN")?;

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

        // output → deriver via ValidPaths.deriver (input-addressed)
        if self.keep_derivations {
            add_edges(
                &self.conn,
                "SELECT v.id, d.id FROM ValidPaths v \
                 JOIN ValidPaths d ON d.path = v.deriver \
                 WHERE v.deriver IS NOT NULL",
            )?;
        }

        // output ↔ drv via DerivationOutputs (content-addressed).
        // keep-derivations: output→drv and drv→output.
        // keep-outputs: output→drv and drv→output.
        // Both need the same two edge sets, so add each at most once.
        if self.keep_derivations || self.keep_outputs {
            add_edges(
                &self.conn,
                "SELECT o.id, do2.drv FROM ValidPaths o \
                 JOIN DerivationOutputs do2 ON do2.path = o.path",
            )?;
            add_edges(
                &self.conn,
                "SELECT do2.drv, o.id FROM DerivationOutputs do2 \
                 JOIN ValidPaths o ON o.path = do2.path",
            )?;
        }

        self.conn.execute_batch("COMMIT")?;

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
                let mut ins = self.conn.prepare(
                    "INSERT INTO DeadPaths SELECT id FROM ValidPaths WHERE path = ?",
                )?;
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
            self.conn.execute_batch(
                "DELETE FROM ValidPaths WHERE id IN (SELECT id FROM DeadPaths)",
            )?;
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
