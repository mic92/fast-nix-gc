// Alloy 6 model of the GC deletion loop (crates/gc/src/gc.rs) and the
// gc-socket protocol (crates/gc/src/gc_socket.rs).
//
// This is an inductive proof: gcInit establishes `inv` and every event
// preserves it, so `inv` holds on traces of any length -- the 2-step bound
// on the commands below is not a limitation, only the number of Path atoms
// is. `inv` is `safety` (the properties we care about) strengthened with
// `reachable`, which pins down the states the implementation can actually
// be in; without it most events could fire from garbage states and break
// safety.
//
// Mapping from code to events:
//
//   read_dir scan                            scanUnknown
//   filter dead set + db.invalidate_paths()  chunkInvalidate
//   live.try_begin_delete_node()             beginDelete
//   unlink + live.end_delete_node()          finishDelete
//   chunk loop done                          chunksDone
//   unlink unknown-on-disk entry             deleteUnknown
//   stale temp root file removed             tempRootStale
//   GC done                                  finishGc
//   gc-socket protect / ack                  protectMark / protectAck
//   builder rebuilds invalid paths           rebuild
//   builder registers a new path             registerFresh
//   SIGKILL mid-GC / next GC run             crash / recover
//
// Builders are assumed to follow the Nix protocol: write to the store only
// after protecting the path (gc-socket or a pre-GC temp root), register
// only paths whose references are valid.
//
// ./scripts/check-alloy.py runs all commands.

module gc_db_consistency

----------------------------------------------------------------------------
-- Static structure
----------------------------------------------------------------------------

-- `refs` abstracts every edge type load_graph() puts into the CSR graph:
-- the Refs table, plus (under keep-derivations / keep-outputs) the
-- drv↔output edges from ValidPaths.deriver, DerivationOutputs and
-- BuildTraceV3. All of these are built with JOINs against ValidPaths, so
-- both endpoints are always in the snapshot (staticStore below).
sig Path { refs: set Path }

-- Paths present in the DB when load_graph() took its snapshot.
sig Snap in Path {}

-- GC roots found by find_roots() (gcroots symlinks, profiles, runtime
-- roots) plus temp roots of paths that are in the snapshot.
sig Root in Path {}

-- Paths with a temp root file written before GC acquired gc.lock.
sig TempRoot in Path {}

fact staticStore {
  Root in Snap
  -- the DB snapshot is closed under references (Refs FK invariant)
  Snap.refs in Snap
  -- temp roots of snapshot paths are added to the root set by gc.rs
  TempRoot & Snap in Root
}

fun closure[p: Path]: set Path { p.*refs }

-- Liveness as computed by StoreGraph::compute_closure().
fun aliveSnap: set Path { Root.*refs }
fun deadSnap: set Path { Snap - aliveSnap }

----------------------------------------------------------------------------
-- Dynamic state
----------------------------------------------------------------------------

var sig dbValid in Path {}      -- rows in the ValidPaths table
-- Temp root files still present (owner alive). find_temp_roots removes a
-- file once it can flock it (owner died); only shield - Snap feeds
-- temp_root_basenames, which shields the unknown-on-disk scan.
var sig shield in TempRoot {}
var sig onDisk in Path {}       -- entries present in /nix/store
var sig protected in Path {}    -- LiveSet.protected (snapshot nodes)
var sig protectedUnknown in Path {} -- LiveSet.protected_unknown (basenames)
var sig pending in Path {}      -- LiveSet.pending_nodes (in-flight unlink)
var sig claimed in Path {}      -- union of all chunks' `claimed` vecs so far
var sig diskDone in Path {}     -- claimed paths whose disk entry has been unlinked
var sig unknownList in Path {}  -- the unknown-on-disk scan result
var sig wantAck in Path {}      -- gc-socket roots received, ack not yet sent
var sig acked in Path {}        -- gc-socket roots acked ('1' written to the builder)
var sig rebuilt in Path {}      -- acked snapshot roots whose builder finished rebuilding

abstract sig Phase {}
one sig Scanning, Deleting, UnknownDeleting, Finished, Crashed, Recovered
  extends Phase {}
one sig PC { var phase: one Phase }

----------------------------------------------------------------------------
-- Initial state: DB and disk agree on the snapshot; disk may additionally
-- hold junk left behind by an earlier crash (unknown-on-disk).
----------------------------------------------------------------------------

pred gcInit {
  dbValid = Snap
  shield = TempRoot
  Snap in onDisk
  no protected
  no protectedUnknown
  no pending
  no claimed
  no diskDone
  no unknownList
  no wantAck
  no acked
  no rebuilt
  PC.phase = Scanning
}

----------------------------------------------------------------------------
-- GC events
----------------------------------------------------------------------------

-- read_dir scan: anything on disk that is neither in the basename index
-- (snapshot) nor in temp_root_basenames is recorded for later deletion.
pred scanUnknown {
  PC.phase = Scanning
  unknownList' = onDisk - Snap - shield
  PC.phase' = Deleting
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and wantAck' = wantAck
  acked' = acked and rebuilt' = rebuilt and shield' = shield
}

-- find_temp_roots flocks p's temp root file: the owner died, so the file
-- is stale and removed. The owner may have registered p just before dying.
pred tempRootStale[p: Path] {
  PC.phase = Scanning
  p in shield
  shield' = shield - p
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  PC.phase' = PC.phase
}

-- One chunk: filter unclaimed dead paths against `protected`, then remove
-- their rows in one transaction, before any unlink.
pred chunkInvalidate {
  PC.phase = Deleting
  -- the previous chunk must be fully processed
  claimed in diskDone + protected
  no pending
  -- the new chunk is a nonempty set of unprotected, unclaimed dead paths
  some claimed' - claimed
  claimed' - claimed in deadSnap - protected - claimed
  claimed in claimed'
  -- ...closed under still-valid referrers: gc.rs expands chunks with dead
  -- referrers and prunes closures of paths protected mid-filter
  no ((dbValid - claimed').refs & (claimed' - claimed))
  dbValid' = dbValid - (claimed' - claimed)
  -- frame
  onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  shield' = shield
  PC.phase' = PC.phase
}

-- live.try_begin_delete_node(p) succeeds: p is claimed and not protected.
pred beginDelete[p: Path] {
  PC.phase = Deleting
  p in claimed - protected - pending - diskDone
  pending' = pending + p
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and claimed' = claimed
  diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  shield' = shield
  PC.phase' = PC.phase
}

-- delete_store_path(p) + live.end_delete_node(p): disk entry gone.
pred finishDelete[p: Path] {
  PC.phase = Deleting
  p in pending
  pending' = pending - p
  diskDone' = diskDone + p
  onDisk' = onDisk - p
  -- frame
  dbValid' = dbValid and protected' = protected
  protectedUnknown' = protectedUnknown and claimed' = claimed
  unknownList' = unknownList and wantAck' = wantAck and acked' = acked
  rebuilt' = rebuilt and shield' = shield and PC.phase' = PC.phase
}

-- Every claimed path has been unlinked or skipped (protected); move on to
-- the unknown-on-disk entries. Also models the --ensure-free early stop:
-- not all dead paths need to have been claimed.
pred chunksDone {
  PC.phase = Deleting
  claimed in diskDone + protected
  no pending
  PC.phase' = UnknownDeleting
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  shield' = shield
}

-- live.try_begin_delete_unknown(p) + unlink: deletes a scanned entry unless
-- a builder protected it through the gc-socket in the meantime or
-- registered it after the graph snapshot (gc.rs re-checks ValidPaths
-- before the unknown unlinks).
pred deleteUnknown[p: Path] {
  PC.phase = UnknownDeleting
  p in (unknownList & onDisk) - protectedUnknown - dbValid
  onDisk' = onDisk - p
  -- frame
  dbValid' = dbValid and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  shield' = shield
  PC.phase' = PC.phase
}

-- All unprotected unknown-on-disk entries are gone; GC finishes.
pred finishGc {
  PC.phase = UnknownDeleting
  unknownList & onDisk in protectedUnknown + dbValid
  PC.phase' = Finished
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  shield' = shield
}

----------------------------------------------------------------------------
-- Builder events (gc-socket clients, nix-daemon registrations)
----------------------------------------------------------------------------

fun gcActive: set Phase { Scanning + Deleting + UnknownDeleting }

-- gc-socket receives root r. Snapshot paths get their whole closure
-- protected; unknown basenames only get a protected_unknown entry.
pred protectMark[r: Path] {
  PC.phase in gcActive
  r not in wantAck + acked
  r in Snap implies {
    protected' = protected + closure[r]
    protectedUnknown' = protectedUnknown
  } else {
    protected' = protected
    protectedUnknown' = protectedUnknown + r
  }
  wantAck' = wantAck + r
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  acked' = acked and rebuilt' = rebuilt and shield' = shield
  PC.phase' = PC.phase
}

-- protect() returns and '1' is written: no closure node is mid-unlink.
pred protectAck[r: Path] {
  PC.phase in gcActive
  r in wantAck
  r in Snap implies no closure[r] & pending
  acked' = acked + r
  wantAck' = wantAck - r
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  rebuilt' = rebuilt and shield' = shield and PC.phase' = PC.phase
}

-- The builder rebuilds what it can see is missing, walking top-down and
-- stopping at paths the DB reports valid: isValidPath() implies the whole
-- closure exists, so it never looks behind a valid path.
fun toRebuild[r: Path]: set Path {
  r.*((Path - dbValid) <: refs) - dbValid
}

pred rebuild[r: Path] {
  PC.phase in gcActive + Finished
  r in (acked & Snap) - rebuilt
  let missing = toRebuild[r] {
    onDisk' = onDisk + missing
    dbValid' = dbValid + missing
  }
  rebuilt' = rebuilt + r
  -- frame
  protected' = protected and protectedUnknown' = protectedUnknown
  pending' = pending and claimed' = claimed and diskDone' = diskDone
  unknownList' = unknownList and wantAck' = wantAck and acked' = acked
  shield' = shield
  PC.phase' = PC.phase
}

-- A builder registers a path that is not in the snapshot: store dir entry
-- plus ValidPaths row. Needs a live temp root file (owner still alive) or
-- an acked protection, and valid references.
pred registerFresh[p: Path] {
  PC.phase in gcActive + Finished
  p in Path - Snap
  p in shield + (acked - Snap)
  p not in onDisk
  p.refs in dbValid
  onDisk' = onDisk + p
  dbValid' = dbValid + p
  -- frame
  protected' = protected and protectedUnknown' = protectedUnknown
  pending' = pending and claimed' = claimed and diskDone' = diskDone
  unknownList' = unknownList and wantAck' = wantAck and acked' = acked
  rebuilt' = rebuilt and shield' = shield and PC.phase' = PC.phase
}

----------------------------------------------------------------------------
-- Failure events
----------------------------------------------------------------------------

-- Process killed mid-GC. Each chunk's invalidate transaction is atomic in
-- SQLite, so the DB reflects the chunks committed so far; disk reflects the
-- unlinks done so far.
pred crash {
  PC.phase in gcActive
  PC.phase' = Crashed
  -- frame
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  shield' = shield
}

-- The next GC run after a crash, as one atomic step and without builders
-- (run-1 builders are dead, their protections and temp roots gone): reload
-- the graph from the surviving DB, collect dead and unknown-on-disk paths.
pred recover {
  PC.phase = Crashed
  let aliveDb = (Root & dbValid).*(refs & dbValid -> dbValid) {
    dbValid' = aliveDb
    onDisk' = onDisk & aliveDb
  }
  PC.phase' = Recovered
  -- frame (run-1 in-memory state is irrelevant from here on)
  protected' = protected and protectedUnknown' = protectedUnknown
  pending' = pending and claimed' = claimed and diskDone' = diskDone
  unknownList' = unknownList and wantAck' = wantAck and acked' = acked
  rebuilt' = rebuilt and shield' = shield
}

pred stutter {
  dbValid' = dbValid and onDisk' = onDisk and protected' = protected
  protectedUnknown' = protectedUnknown and pending' = pending
  claimed' = claimed and diskDone' = diskDone and unknownList' = unknownList
  wantAck' = wantAck and acked' = acked and rebuilt' = rebuilt
  shield' = shield
  PC.phase' = PC.phase
}

pred anyEvent {
  (some p: Path | beginDelete[p] or finishDelete[p] or deleteUnknown[p]
                  or protectMark[p] or protectAck[p] or rebuild[p]
                  or registerFresh[p] or tempRootStale[p])
  or scanUnknown or chunkInvalidate or chunksDone or finishGc
  or crash or recover or stutter
}

----------------------------------------------------------------------------
-- Safety properties
----------------------------------------------------------------------------

pred safety {
  -- no ValidPaths row for a path missing from disk, at any point
  dbValid in onDisk

  -- valid paths only reference valid paths (the Refs FK invariant)
  dbValid.refs in dbValid

  -- rooted paths are never unlinked or invalidated
  aliveSnap in onDisk
  aliveSnap in dbValid

  -- after rebuilding, an acked builder's closure is on disk and in the DB
  -- (recovery exempt: run-1 builders are dead by then)
  PC.phase != Recovered implies
    all r: rebuilt | closure[r] in onDisk and closure[r] in dbValid

  -- with no builders, no temp roots and no early stop, GC leaves exactly
  -- the alive snapshot
  (PC.phase = Finished and no wantAck and no acked and no TempRoot
   and deadSnap in claimed)
    implies (onDisk in aliveSnap and dbValid in aliveSnap)
}

----------------------------------------------------------------------------
-- Strengthening invariant: rules out states the implementation cannot
-- reach. `safety` alone is not inductive; these constraints close the gap.
----------------------------------------------------------------------------

pred reachable {
  -- GC only claims and unlinks dead snapshot paths
  claimed in deadSnap
  diskDone in claimed
  pending in claimed
  no pending & diskDone

  -- gc-socket request lifecycle
  rebuilt in acked
  no wantAck & acked
  -- protect() marks whole closures of snapshot roots...
  all r: (wantAck + acked) & Snap | closure[r] in protected
  -- ...and only basenames of non-snapshot roots
  (wantAck + acked) - Snap in protectedUnknown
  -- nothing is protected without a corresponding request
  protected in closure[(wantAck + acked) & Snap]
  protectedUnknown in (wantAck + acked) - Snap

  -- what an ack means: nothing in an acked closure is mid-unlink
  all r: acked & Snap | no closure[r] & pending

  -- claimed paths re-enter the DB only through rebuild, which requires
  -- protection
  claimed & dbValid in protected
  no pending & dbValid

  -- unlinked paths re-enter the disk only together with their row
  diskDone & onDisk in dbValid

  -- fresh paths enter the DB only via a temp root or an acked protection
  dbValid - Snap in TempRoot + (acked - Snap)

  -- the scan never lists snapshot paths or paths whose temp root file was
  -- still live at scan time
  unknownList in Path - Snap - shield

  -- after the scan, disk entries outside the snapshot are scanned junk,
  -- temp-rooted, or registered by an acked builder
  PC.phase in Deleting + UnknownDeleting + Finished implies
    onDisk - Snap in unknownList + TempRoot + (acked - Snap)

  -- phase bookkeeping
  PC.phase = Scanning implies
    (no unknownList and no claimed and no diskDone and no pending)
  PC.phase in UnknownDeleting + Finished implies
    (claimed in diskDone + protected and no pending)
  PC.phase = Finished implies unknownList & onDisk in protectedUnknown + dbValid
}

pred inv { safety and reachable }

----------------------------------------------------------------------------
-- Sanity checks: make sure the invariant is satisfiable and no event is
-- accidentally disabled (a vacuous induction step would prove nothing)
----------------------------------------------------------------------------

run initialStateExists { gcInit } for 6 but 2 steps expect 1

run richStateExists {
  inv
  PC.phase = Deleting
  some pending and some diskDone and some acked & Snap and some rebuilt
  some acked - Snap and some unknownList
} for 6 but 2 steps expect 1

run stepChunkInvalidate { inv and chunkInvalidate } for 6 but 2 steps expect 1
run stepFinishDelete { inv and some p: Path | finishDelete[p] } for 6 but 2 steps expect 1
run stepDeleteUnknown { inv and some p: Path | deleteUnknown[p] } for 6 but 2 steps expect 1
run stepRebuild { inv and some p: Path | rebuild[p] } for 6 but 2 steps expect 1
run stepRegisterFresh { inv and some p: Path | registerFresh[p] } for 6 but 2 steps expect 1
run stepRecover { inv and recover } for 6 but 2 steps expect 1
run stepTempRootStale { inv and some p: Path | tempRootStale[p] } for 6 but 2 steps expect 1

----------------------------------------------------------------------------
-- The proof
----------------------------------------------------------------------------

-- Base case of the induction.
check InitEstablishesInv {
  gcInit implies inv
} for 6 but 2 steps

-- Induction step: any single event starting from `inv` lands back in
-- `inv`. Together with the base case this covers traces of any length.
check InvIsInductive {
  (inv and anyEvent) implies after inv
} for 6 but 2 steps

-- Once a root is acked, no step unlinks anything the builder was promised
-- (recovery exempt).
check AckedPathsNeverUnlinked {
  (inv and anyEvent and PC.phase' != Recovered) implies
    all r: acked |
      (((r in Snap) implies closure[r] else r) & onDisk) in onDisk'
} for 6 but 2 steps
