//! Coverage-guided differential fuzz of the GC graph logic.
//!
//! Builds a random store database fully in memory (no temp dirs, no
//! subprocesses), including deriver columns, DerivationOutputs rows,
//! id gaps from deleted rows and random keep-outputs/keep-derivations
//! settings. Then checks that load_graph + compute_closure match an
//! independent fixpoint model and that invalidate_paths removes exactly
//! the dead rows.
//!
//! The semantic oracle against real Nix is difftest/ (fuzz-nix-diff);
//! this target covers DB loading, CSR construction and invalidation at
//! libFuzzer speed.

#![no_main]

use arbitrary::Arbitrary;
use fast_nix_gc::db::{BasenameIndex, NixDb};
use libfuzzer_sys::fuzz_target;
use rusqlite::Connection;
use std::collections::VecDeque;
use std::path::PathBuf;

const STORE_DIR: &str = "/fuzz/store";

// Nix's database schema, reduced to the tables the GC reads.
const SCHEMA: &str = "
CREATE TABLE ValidPaths (
    id               integer primary key autoincrement not null,
    path             text unique not null,
    hash             text not null,
    registrationTime integer not null,
    deriver          text,
    narSize          integer,
    ultimate         integer,
    sigs             text,
    ca               text
);
CREATE TABLE Refs (
    referrer  integer not null,
    reference integer not null,
    primary key (referrer, reference),
    foreign key (referrer) references ValidPaths(id) on delete cascade,
    foreign key (reference) references ValidPaths(id) on delete restrict
);
CREATE TABLE DerivationOutputs (
    drv  integer not null,
    id   text not null,
    path text not null,
    primary key (drv, id),
    foreign key (drv) references ValidPaths(id) on delete cascade
);
";

#[derive(Arbitrary, Debug)]
struct Input {
    n: u8,
    /// Refs edges (referrer, reference), indices mod n.
    refs: Vec<(u8, u8)>,
    /// (output, drv): sets ValidPaths.deriver and a DerivationOutputs
    /// row. drv index mod (n+1); index n means a path that is not in
    /// ValidPaths (unbuilt deriver).
    derivers: Vec<(u8, u8)>,
    /// (output, drv): DerivationOutputs row only (CA-style mapping;
    /// no deriver column). output index mod (n+1): index n means an
    /// unbuilt output path.
    drv_outputs: Vec<(u8, u8)>,
    roots: Vec<u8>,
    /// Per-node registrationTime; missing entries default to 0.
    reg_times: Vec<i64>,
    /// Mirror of --keep-recent: nodes with reg_time >= cutoff are roots.
    cutoff: i64,
    /// Nodes whose insertion is preceded by an inserted-then-deleted
    /// dummy row, leaving a hole in the autoincrement id sequence.
    id_gap_before: Vec<u8>,
    keep_derivations: bool,
    keep_outputs: bool,
}

fn basename_of(i: usize) -> String {
    // Zero-padded decimal: all 32 hash chars stay in Nix's base32
    // alphabet so StorePath parsing accepts them.
    format!("{i:032}-pkg-{i}")
}

fn path_of(i: usize) -> String {
    format!("{STORE_DIR}/{}", basename_of(i))
}

/// Input decoded into in-range node indices.
struct Model {
    n: usize,
    refs: Vec<(usize, usize)>,
    derivers: Vec<(usize, usize)>,
    drv_outputs: Vec<(usize, usize)>,
    roots: Vec<usize>,
    reg_times: Vec<i64>,
    id_gaps: Vec<bool>,
    keep_derivations: bool,
    keep_outputs: bool,
}

impl Model {
    fn new(input: &Input) -> Self {
        let n = 1 + (input.n as usize) % 64;
        let node = |v: u8| v as usize % n;
        // `node_or_missing` may yield index n: a path missing from
        // ValidPaths, i.e. an unbuilt deriver (input-addressed) or
        // unbuilt output (CA). The GC must ignore such dangling rows.
        let node_or_missing = |v: u8| v as usize % (n + 1);

        let reg_times: Vec<i64> = (0..n)
            .map(|i| input.reg_times.get(i).copied().unwrap_or(0))
            .collect();
        // Explicit roots plus recently registered paths (--keep-recent).
        let roots = input
            .roots
            .iter()
            .map(|&r| node(r))
            .chain((0..n).filter(|&i| reg_times[i] >= input.cutoff))
            .collect();

        Model {
            n,
            refs: input
                .refs
                .iter()
                .map(|&(a, b)| (node(a), node(b)))
                .collect(),
            derivers: input
                .derivers
                .iter()
                .map(|&(out, drv)| (node(out), node_or_missing(drv)))
                .collect(),
            drv_outputs: input
                .drv_outputs
                .iter()
                .map(|&(out, drv)| (node_or_missing(out), node(drv)))
                .collect(),
            roots,
            reg_times,
            id_gaps: (0..n)
                .map(|i| input.id_gap_before.get(i).is_some_and(|&b| b & 1 == 1))
                .collect(),
            keep_derivations: input.keep_derivations,
            keep_outputs: input.keep_outputs,
        }
    }

    /// deriver column values: last write wins per output row.
    fn deriver_of(&self) -> Vec<Option<usize>> {
        let mut deriver_of = vec![None; self.n];
        for &(out, drv) in &self.derivers {
            deriver_of[out] = Some(drv);
        }
        deriver_of
    }

    /// All (output, drv) pairs that get a DerivationOutputs row.
    fn derivation_outputs(&self) -> impl Iterator<Item = &(usize, usize)> {
        self.derivers.iter().chain(&self.drv_outputs)
    }

    /// Fixpoint reachability over the modeled liveness edges, mirroring
    /// the keep-derivations/keep-outputs rules from the GC's perspective.
    fn alive(&self) -> Vec<bool> {
        let n = self.n;
        let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
        let mut edge = |from: usize, to: usize| {
            if from < n && to < n {
                adj[from].push(to);
            }
        };
        for &(from, to) in &self.refs {
            edge(from, to);
        }
        // keep-derivations: alive output keeps its deriver alive, via
        // the deriver column only (Nix requires deriver == drv; a
        // DerivationOutputs row alone does not keep the drv).
        if self.keep_derivations {
            for (out, drv) in self.deriver_of().into_iter().enumerate() {
                if let Some(drv) = drv {
                    edge(out, drv);
                }
            }
        }
        // keep-outputs: alive derivation keeps its outputs alive.
        if self.keep_outputs {
            for &(out, drv) in self.derivation_outputs() {
                edge(drv, out);
            }
        }

        let mut alive = vec![false; n];
        let mut queue: VecDeque<usize> = self.roots.iter().copied().collect();
        for &r in &self.roots {
            alive[r] = true;
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

    fn setup_db(&self) -> NixDb {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch("BEGIN").unwrap();

        let deriver_of = self.deriver_of();
        let mut node_ids = vec![0i64; self.n];
        for i in 0..self.n {
            // Real stores have holes in the id sequence from earlier
            // GCs; insert-and-delete a dummy row to reproduce that.
            if self.id_gaps[i] {
                let gap = format!("/fuzz/gap-{i}");
                conn.execute(
                    "INSERT INTO ValidPaths (path, hash, registrationTime) VALUES (?1, 'x', 0)",
                    [&gap],
                )
                .unwrap();
                conn.execute("DELETE FROM ValidPaths WHERE path = ?1", [&gap])
                    .unwrap();
            }
            conn.execute(
                "INSERT INTO ValidPaths (path, hash, registrationTime, narSize, deriver) \
                 VALUES (?1, ?2, ?3, 100, ?4)",
                rusqlite::params![
                    path_of(i),
                    format!("sha256:{i:032}"),
                    self.reg_times[i],
                    deriver_of[i].map(path_of)
                ],
            )
            .unwrap();
            node_ids[i] = conn.last_insert_rowid();
        }

        for &(from, to) in &self.refs {
            conn.execute(
                "INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?1, ?2)",
                rusqlite::params![node_ids[from], node_ids[to]],
            )
            .unwrap();
        }

        // Unique output name per row sidesteps the (drv, id) primary
        // key; the GC only joins on drv and path.
        for (row, &(out, drv)) in self.derivation_outputs().enumerate() {
            if drv >= self.n {
                continue; // FK requires the drv row to exist
            }
            conn.execute(
                "INSERT INTO DerivationOutputs (drv, id, path) VALUES (?1, ?2, ?3)",
                rusqlite::params![node_ids[drv], format!("out{row}"), path_of(out)],
            )
            .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();

        NixDb {
            conn,
            store_dir: PathBuf::from(STORE_DIR),
            state_dir: PathBuf::from("/fuzz/state"),
            real_store_dir: PathBuf::from(STORE_DIR),
            links_dir: PathBuf::from(STORE_DIR).join(".links"),
            keep_derivations: self.keep_derivations,
            keep_outputs: self.keep_outputs,
        }
    }
}

fuzz_target!(|input: Input| {
    let model = Model::new(&input);
    let n = model.n;
    let db = model.setup_db();

    // Cheap consistency checks on the query helpers.
    assert_eq!(db.valid_store_paths().unwrap().len(), n);
    assert!(db.is_valid_path(&path_of(0)).unwrap());
    assert!(!db.is_valid_path(&path_of(n)).unwrap());

    let graph = db.load_graph().unwrap();
    assert_eq!(graph.len(), n);
    assert!(!graph.is_empty());

    let bidx = BasenameIndex::new(&graph);
    assert!(bidx.idx_of_basename(&basename_of(n)).is_none());
    assert_eq!(
        bidx.idx_of(&path_of(0)),
        bidx.idx_of_basename(&basename_of(0))
    );
    assert!(bidx.idx_of("/elsewhere/x-pkg").is_none());
    let idx_of = |i: usize| bidx.idx_of_basename(&basename_of(i)).unwrap() as usize;

    let root_indices: Vec<u32> = model.roots.iter().map(|&r| idx_of(r) as u32).collect();
    let alive = graph.compute_closure(&root_indices);

    let expected = model.alive();
    for (i, &want) in expected.iter().enumerate() {
        assert_eq!(
            alive[idx_of(i)],
            want,
            "pkg-{i}: alive={}, expected={want} (kd={}, ko={})",
            alive[idx_of(i)],
            model.keep_derivations,
            model.keep_outputs
        );
    }

    // Deleting the dead set must not trip FK constraints (cycles) and
    // must leave exactly the alive rows.
    let dead: Vec<String> = (0..n).filter(|&i| !expected[i]).map(path_of).collect();
    db.invalidate_paths(dead.iter().map(|s| s.as_str()))
        .unwrap();
    let remaining: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM ValidPaths", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining as usize, expected.iter().filter(|&&a| a).count());
});
