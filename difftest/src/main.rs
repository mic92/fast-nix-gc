//! Differential fuzz of fast-nix-gc against `nix-store --gc`.
//!
//! Each iteration generates a random DAG of store paths
//! (input-addressed, multi-output, fixed-output, content-addressed,
//! text-addressed, source-path and impure nodes), builds it
//! with real Nix in a throwaway store, pins a random subset via gcroots
//! (output and .drv roots) and picks a random
//! keep-outputs/keep-derivations configuration. Both tools must then
//! agree on the dry-run dead set and on what survives a real GC.
//!
//! Reproduce a failure with: fuzz-nix-diff --seed <seed> --iterations 1

use anyhow::{Context, Result, bail};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// sha256 of an empty file, used for all flat fixed-output derivations.
/// Distinct names still yield distinct store paths.
const EMPTY_SHA256: &str = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

/// NAR sha256 of a file containing "hi\n", for recursive fixed-output
/// derivations.
const HI_NAR_SHA256: &str = "sha256-EFUdrtf6Rn0LWIJufrmg8q99aT3jGfLvd1//zaJEufY=";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Plain,
    Multi,
    Fod,
    FodRec,
    Ca,
    Text,
    Src,
    Impure,
}

impl Kind {
    /// Store path created during evaluation, not by building a derivation.
    fn is_eval(self) -> bool {
        matches!(self, Kind::Text | Kind::Src)
    }
}

const KINDS: &[Kind] = &[
    Kind::Plain,
    Kind::Plain,
    Kind::Plain,
    Kind::Multi,
    Kind::Fod,
    Kind::FodRec,
    Kind::Ca,
    Kind::Text,
    Kind::Src,
    Kind::Impure,
];

struct Node {
    name: String,
    kind: Kind,
    /// (dependency index, output) pairs; output is "out" or "dev" (multi only).
    deps: Vec<(usize, &'static str)>,
    /// Output pinned via gcroot, if any.
    root_output: Option<&'static str>,
    /// Pin the .drv via gcroot.
    root_drv: bool,
}

fn gen_graph(rng: &mut StdRng, keep_outputs: bool) -> Vec<Node> {
    let n = rng.random_range(6..=16);
    let mut nodes: Vec<Node> = Vec::with_capacity(n);
    for i in 0..n {
        let kind = KINDS[rng.random_range(0..KINDS.len())];
        let mut deps = Vec::new();
        // Eval-time paths (toFile, builtins.path) cannot reference
        // derivation outputs, and nothing may depend on an impure
        // derivation without resolving it first. FODs may use deps at
        // build time but their outputs carry no references; skip
        // eval-time deps for them to keep the input hash stable.
        if !kind.is_eval() && kind != Kind::Impure {
            for (j, dep) in nodes.iter().enumerate() {
                if dep.kind == Kind::Impure {
                    continue;
                }
                if dep.kind.is_eval() && matches!(kind, Kind::Fod | Kind::FodRec) {
                    continue;
                }
                if rng.random_bool(0.35) {
                    let output = if dep.kind == Kind::Multi && rng.random_bool(0.5) {
                        "dev"
                    } else {
                        "out"
                    };
                    deps.push((j, output));
                }
            }
        }
        nodes.push(Node {
            name: format!("n{i}"),
            kind,
            deps,
            root_output: None,
            root_drv: false,
        });
    }
    // A node depending (transitively) on a floating CA output has
    // deferred output paths itself.
    let mut deferred = vec![false; nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        deferred[i] = node.kind == Kind::Ca || node.deps.iter().any(|&(j, _)| deferred[j]);
    }

    for (i, node) in nodes.iter_mut().enumerate() {
        // Never root impure paths: Nix itself is inconsistent about
        // whether the .drv of a rooted impure output survives
        // keep-derivations (--print-dead and the real GC can disagree
        // between runs), so there is no stable oracle. Unrooted impure
        // paths are always garbage, which both tools must agree on.
        if node.kind == Kind::Impure {
            continue;
        }
        if rng.random_bool(0.3) {
            node.root_output = Some(if node.kind == Kind::Multi && rng.random_bool(0.5) {
                "dev"
            } else {
                "out"
            });
        }
        // No .drv roots for deferred outputs under keep-outputs: Nix
        // resolves drv -> output dynamically (the DB row has no path
        // until the resolved derivation registers it), and its real GC
        // is then order-dependent and disagrees with its own
        // --print-dead (keep-outputs deletion cycles).
        // There is no stable oracle to compare against.
        let no_drv_root = keep_outputs && deferred[i];
        if !node.kind.is_eval() && !no_drv_root && rng.random_bool(0.1) {
            node.root_drv = true;
        }
    }
    nodes
}

fn dep_list(nodes: &[Node], node: &Node) -> String {
    node.deps
        .iter()
        .map(|&(j, output)| {
            let name = &nodes[j].name;
            if output == "out" {
                name.clone()
            } else {
                format!("({name}.{output})")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_expr(nodes: &[Node], src_dir: &Path) -> String {
    let mut body = String::new();
    for node in nodes {
        let name = &node.name;
        let deps = dep_list(nodes, node);
        let line = match node.kind {
            Kind::Plain => format!("{name} = mk \"{name}\" {{ deps = [ {deps} ]; }};"),
            Kind::Ca => format!(
                "{name} = mk \"{name}\" {{ deps = [ {deps} ]; \
                 __contentAddressed = true; outputHashMode = \"recursive\"; \
                 outputHashAlgo = \"sha256\"; }};"
            ),
            Kind::Multi => format!(
                r#"{name} = derivation {{
    name = "{name}";
    inherit system;
    builder = sh;
    args = [ "-c" "echo $name-out > $out; echo $name-dev > $dev; for d in $deps; do echo dep $d >> $out; echo dep $d >> $dev; done" ];
    deps = [ {deps} ];
    outputs = [ "out" "dev" ];
  }};"#
            ),
            Kind::Fod => format!(
                "{name} = derivation {{ name = \"{name}\"; inherit system; \
                 builder = sh; args = [ \"-c\" \": > $out\" ]; deps = [ {deps} ]; \
                 outputHashMode = \"flat\"; outputHashAlgo = \"sha256\"; \
                 outputHash = \"{EMPTY_SHA256}\"; }};"
            ),
            Kind::FodRec => format!(
                "{name} = derivation {{ name = \"{name}\"; inherit system; \
                 builder = sh; args = [ \"-c\" \"echo hi > $out\" ]; deps = [ {deps} ]; \
                 outputHashMode = \"recursive\"; outputHashAlgo = \"sha256\"; \
                 outputHash = \"{HI_NAR_SHA256}\"; }};"
            ),
            Kind::Text => format!("{name} = builtins.toFile \"{name}\" \"text {name}\\n\";"),
            Kind::Src => format!(
                "{name} = builtins.path {{ path = {}; name = \"{name}\"; }};",
                src_dir.display()
            ),
            // Impure derivation: output is content-addressed after the
            // fact and nothing else may depend on it, so it's a leaf.
            // Fixed output content keeps store paths reproducible per
            // seed; Nix treats the derivation as impure regardless.
            Kind::Impure => format!(
                "{name} = derivation {{ name = \"{name}\"; inherit system; builder = sh; \
                 args = [ \"-c\" \"echo impure-{name} > $out\" ]; \
                 __impure = true; }};"
            ),
        };
        writeln!(body, "  {line}").unwrap();
    }
    format!(
        r#"let
  system = builtins.currentSystem;
  sh = "/bin/sh";
  mk = name: extra: derivation ({{
    inherit name system;
    builder = sh;
    args = [ "-c" "echo $name > $out; for d in $deps; do echo dep $d >> $out; done" ];
    deps = [];
  }} // extra);
in rec {{
{body}}}
"#
    )
}

fn nix_config(rng: &mut StdRng, keep_outputs: bool) -> String {
    format!(
        "experimental-features = nix-command ca-derivations impure-derivations\n\
         sandbox = false\n\
         substituters =\n\
         builders =\n\
         max-jobs = auto\n\
         keep-outputs = {keep_outputs}\n\
         keep-derivations = {}\n",
        rng.random_bool(0.5)
    )
}

/// Random store/state corruptions applied before the GC comparison.
/// Both tools must agree on how to handle them.
#[derive(Debug, Clone, Copy)]
enum Corruption {
    /// Remove a store path from disk while keeping its DB row.
    DeleteOnDisk,
    /// Store directory entry (dir) with a valid-looking name but no DB row.
    JunkDir,
    /// Same, but a regular file.
    JunkFile,
    /// Regular file whose name doesn't parse as a store path.
    JunkFileBadName,
    /// gcroot symlink to a parseable but nonexistent store path.
    DanglingRoot,
    /// gcroot symlink pointing outside the store.
    OutsideRoot,
    /// temproots file from a dead process referencing a store path.
    StaleTempRoot,
}

const CORRUPTIONS: &[Corruption] = &[
    Corruption::DeleteOnDisk,
    Corruption::JunkDir,
    Corruption::JunkFile,
    Corruption::JunkFileBadName,
    Corruption::DanglingRoot,
    Corruption::OutsideRoot,
    Corruption::StaleTempRoot,
];

/// 32 valid base32 hash chars so Nix parses the name as a store path.
fn junk_basename(i: u32, suffix: &str) -> String {
    format!("{:z>32}-{suffix}{i}", i % 10)
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).with_context(|| format!("renaming {} to {}", from.display(), to.display()))
}

/// Store basenames whose handling legitimately differs between the
/// tools and is therefore excluded from the set comparisons.
#[derive(Default)]
struct CorruptionEffects {
    /// Unknown disk entries: Nix doesn't list them consistently in
    /// --print-dead, but both tools must delete them from disk.
    junk: Vec<String>,
    /// Valid DB rows whose disk entry was removed: Nix conservatively
    /// keeps a .drv row it cannot read from disk, fast-nix-gc removes
    /// dead rows based on the DB alone. Both are defensible, so these
    /// are excluded from the dead-set and DB comparisons.
    deleted: Vec<String>,
}

/// Apply random corruptions.
fn apply_corruptions(ctx: &Ctx, rng: &mut StdRng) -> Result<CorruptionEffects> {
    let store = Path::new(&ctx.store_dir);
    let mut entries: Vec<String> = store_listing(store)?.into_iter().collect();
    entries.sort();
    let mut fx = CorruptionEffects::default();

    for i in 0..rng.random_range(0..=3u32) {
        let op = CORRUPTIONS[rng.random_range(0..CORRUPTIONS.len())];
        println!("    corruption: {op:?}");
        match op {
            Corruption::DeleteOnDisk => {
                if entries.is_empty() {
                    continue;
                }
                // Remove from `entries` so a second DeleteOnDisk can't
                // draw the same victim.
                let name = entries.remove(rng.random_range(0..entries.len()));
                println!("      victim: {name}");
                let victim = store.join(&name);
                make_writable(&victim)?;
                let res = if victim.is_dir() {
                    fs::remove_dir_all(&victim)
                } else {
                    fs::remove_file(&victim)
                };
                res.with_context(|| format!("removing {}", victim.display()))?;
                fx.deleted.push(name);
            }
            Corruption::JunkDir => {
                let name = junk_basename(i, "junkdir");
                let dir = store.join(&name);
                fs::create_dir(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
                write_file(&dir.join("file"), "junk\n")?;
                fx.junk.push(name);
            }
            Corruption::JunkFile => {
                let name = junk_basename(i, "junkfile");
                write_file(&store.join(&name), "junk\n")?;
                fx.junk.push(name);
            }
            Corruption::JunkFileBadName => {
                let name = format!("junkfile-{i}");
                write_file(&store.join(&name), "junk\n")?;
                fx.junk.push(name);
            }
            Corruption::DanglingRoot => {
                symlink(
                    &store.join(junk_basename(i, "missing")),
                    &Path::new(&ctx.state_dir).join(format!("gcroots/dangling-{i}")),
                )?;
            }
            Corruption::OutsideRoot => {
                symlink(
                    &ctx.tmp,
                    &Path::new(&ctx.state_dir).join(format!("gcroots/outside-{i}")),
                )?;
            }
            Corruption::StaleTempRoot => {
                if entries.is_empty() {
                    continue;
                }
                // Nobody holds the flock, so both tools must treat the
                // file as stale: remove it and ignore its contents.
                let target = &entries[rng.random_range(0..entries.len())];
                println!("      temproot target: {target}");
                write_file(
                    &Path::new(&ctx.state_dir).join(format!("temproots/{}", 4000000 + i)),
                    &format!("{}/{target}\0", ctx.store_dir),
                )?;
            }
        }
    }
    Ok(fx)
}

/// Owned temp directory, force-removed on drop (store outputs are
/// read-only until made writable).
struct TmpDir(PathBuf);

impl TmpDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = make_writable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One iteration's throwaway store and tool invocations.
struct Ctx {
    tmp: PathBuf,
    binary: String,
    store_dir: String,
    state_dir: String,
    store_uri: String,
    config: String,
}

impl Ctx {
    /// Run a command with NIX_CONFIG set, returning stdout.
    fn run(&self, argv: &[&str]) -> Result<String> {
        let out = Command::new(argv[0])
            .args(&argv[1..])
            .env("NIX_CONFIG", &self.config)
            .output()
            .with_context(|| format!("spawning {}", argv[0]))?;
        if !out.status.success() {
            bail!(
                "{} failed ({}):\n{}",
                argv.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8(out.stdout)?)
    }

    fn nix(&self, args: &[&str]) -> Result<String> {
        let mut argv = vec!["nix", "--store", &self.store_uri, "--offline"];
        argv.extend_from_slice(args);
        self.run(&argv)
    }

    fn fast_gc(&self, dry_run: bool) -> Result<String> {
        let mut argv = vec![
            self.binary.as_str(),
            "--store-dir",
            &self.store_dir,
            "--state-dir",
            &self.state_dir,
        ];
        if dry_run {
            argv.push("--dry-run");
        }
        self.run(&argv)
    }

    fn nix_gc(&self, print_dead: bool) -> Result<String> {
        let mut argv = vec!["nix-store", "--store", &self.store_uri, "--gc"];
        if print_dead {
            argv.push("--print-dead");
        }
        self.run(&argv)
    }

    /// fast-nix-gc dry-run dead set. The output ends with a
    /// human-readable summary line; keep only the store paths.
    fn fast_dead_set(&self) -> Result<BTreeSet<String>> {
        let prefix = format!("{}/", self.store_dir);
        Ok(self
            .fast_gc(true)?
            .lines()
            .filter(|l| l.starts_with(&prefix))
            .map(str::to_owned)
            .collect())
    }
}

/// Locate the fast-nix-gc binary: $FAST_NIX_GC or next to our own exe.
fn find_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("FAST_NIX_GC") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    let candidate = exe.parent().unwrap().join("fast-nix-gc");
    if candidate.is_file() {
        return Ok(candidate);
    }
    bail!(
        "fast-nix-gc not found at {} (build it or set FAST_NIX_GC)",
        candidate.display()
    );
}

/// Store entries on disk, excluding internal `.links`/lock files.
fn store_listing(store_dir: &Path) -> Result<BTreeSet<String>> {
    let mut set = BTreeSet::new();
    for entry in fs::read_dir(store_dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name != ".links" && name != ".gc-lock" && !name.ends_with(".lock") {
            set.insert(name);
        }
    }
    Ok(set)
}

fn valid_paths(state_dir: &Path) -> Result<BTreeSet<String>> {
    let db = state_dir.join("db/db.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", db.display()))?;
    let mut stmt = conn.prepare("SELECT path FROM ValidPaths")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn diff_sets(
    label: &str,
    a_name: &str,
    a: &BTreeSet<String>,
    b_name: &str,
    b: &BTreeSet<String>,
) -> bool {
    if a == b {
        println!("OK: {label} identical ({} entries)", a.len());
        return true;
    }
    eprintln!("FAIL: {label} differ");
    for p in a.difference(b) {
        eprintln!("  only {a_name}:  {p}");
    }
    for p in b.difference(a) {
        eprintln!("  only {b_name}: {p}");
    }
    false
}

/// Recursively copy a tree, preserving symlinks. Skips `gc-socket`.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("mkdir {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "gc-socket" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let meta =
            fs::symlink_metadata(&from).with_context(|| format!("stat {}", from.display()))?;
        if meta.file_type().is_symlink() {
            let target =
                fs::read_link(&from).with_context(|| format!("readlink {}", from.display()))?;
            symlink(&target, &to)?;
        } else if meta.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Builders chmod -w outputs; restore user write so copy/cleanup work.
fn make_writable(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    perms.set_mode(perms.mode() | 0o200);
    fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;
    if meta.is_dir() {
        for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
            make_writable(&entry?.path())?;
        }
    }
    Ok(())
}

/// When Nix cannot GC the corrupted store at all, check fast-nix-gc on
/// its own: it must run, its real GC must match its dry-run prediction,
/// and disk must equal the database afterwards (junk gone).
fn validate_fast_alone(ctx: &Ctx, fx: &CorruptionEffects) -> Result<bool> {
    let state_dir = Path::new(&ctx.state_dir);
    let valid_before = valid_paths(state_dir)?;

    let fast_dead = ctx.fast_dead_set()?;
    ctx.fast_gc(false)?;

    let mut ok = true;
    let db_after = valid_paths(state_dir)?;
    let expected: BTreeSet<String> = valid_before.difference(&fast_dead).cloned().collect();
    ok &= diff_sets(
        "fast DB vs own dry-run",
        "expected",
        &expected,
        "fast",
        &db_after,
    );

    // Disk must mirror the DB, except for rows whose disk entry we
    // removed ourselves: if such a path is alive (e.g. rooted), the GC
    // rightly keeps the row but cannot resurrect the file.
    let store_prefix = format!("{}/", ctx.store_dir);
    let disk = store_listing(Path::new(&ctx.store_dir))?;
    let not_deleted = |n: &String| !fx.deleted.contains(n);
    let db_names: BTreeSet<String> = db_after
        .iter()
        .filter_map(|p| p.strip_prefix(&store_prefix).map(str::to_owned))
        .filter(not_deleted)
        .collect();
    let disk_cmp: BTreeSet<String> = disk.iter().filter(|n| not_deleted(n)).cloned().collect();
    ok &= diff_sets("fast disk vs DB", "db", &db_names, "disk", &disk_cmp);

    for name in fx.junk.iter().chain(&fx.deleted) {
        if disk.contains(name) {
            eprintln!("FAIL: fast kept corrupted store entry {name}");
            ok = false;
        }
    }
    Ok(ok)
}

/// Compare nix-store --gc and fast-nix-gc on the store under `<tmp>/nix`.
/// Corruption-affected paths are excluded from the set comparisons and
/// validated separately.
fn compare_gc(ctx: &Ctx, fx: &CorruptionEffects) -> Result<bool> {
    let tmp = ctx.tmp.as_path();
    let excluded_names: BTreeSet<&String> = fx.junk.iter().chain(&fx.deleted).collect();
    let excluded_paths: BTreeSet<String> = excluded_names
        .iter()
        .map(|name| format!("{}/{name}", ctx.store_dir))
        .collect();

    eprintln!("running nix-store --gc --print-dead...");
    let nix_dry = match ctx.nix_gc(true) {
        Ok(out) => out,
        Err(e) => {
            // Nix refuses to GC some corrupted stores (e.g. a rooted
            // .drv missing from disk aborts with "store path does not
            // exist"). No oracle then; still require fast-nix-gc to
            // cope and clean up.
            println!("nix-store --gc failed on this corrupted store, validating fast alone:");
            println!("    {}", e.to_string().lines().last().unwrap_or(""));
            return validate_fast_alone(ctx, fx);
        }
    };
    let nix_dead: BTreeSet<String> = nix_dry
        .lines()
        .map(str::to_owned)
        .filter(|p| !excluded_paths.contains(p))
        .collect();

    eprintln!("running fast-nix-gc --dry-run...");
    let fast_dead: BTreeSet<String> = ctx
        .fast_dead_set()?
        .into_iter()
        .filter(|p| !excluded_paths.contains(p))
        .collect();

    let mut ok = true;
    println!();
    println!("nix dry-run:  {} dead paths", nix_dead.len());
    println!("fast dry-run: {} dead paths", fast_dead.len());
    ok &= diff_sets("dry-run dead sets", "nix", &nix_dead, "fast", &fast_dead);

    // Real GC: clone the store, run nix on one copy and fast-nix-gc on
    // the other, then compare what survives on disk and in the DB.
    println!();
    eprintln!("cloning store for real GC comparison...");
    make_writable(&tmp.join("nix"))?;
    let nix_clone = tmp.join("clone-nix");
    let fast_clone = tmp.join("clone-fast");
    copy_tree(&tmp.join("nix"), &nix_clone.join("nix"))?;
    copy_tree(&tmp.join("nix"), &fast_clone.join("nix"))?;

    // The DB records absolute store paths under the original prefix, so
    // pointing either tool at a clone's own path would not match any
    // registered path. Instead move each clone into the original
    // location for the duration of its GC run and move it back.
    let original = tmp.join("nix");
    let backup = tmp.join("nix.orig");
    rename(&original, &backup)?;

    let with_tree = |clone: &Path, f: &dyn Fn() -> Result<String>| -> Result<()> {
        rename(&clone.join("nix"), &original)?;
        let res = f();
        rename(&original, &clone.join("nix"))?;
        res.map(|_| ())
    };

    eprintln!("running real nix-store --gc...");
    with_tree(&nix_clone, &|| ctx.nix_gc(false))?;

    eprintln!("running real fast-nix-gc...");
    with_tree(&fast_clone, &|| ctx.fast_gc(false))?;

    rename(&backup, &original)?;

    let strip_names = |s: BTreeSet<String>| -> BTreeSet<String> {
        s.into_iter()
            .filter(|n| !excluded_names.contains(n))
            .collect()
    };
    let nix_disk_raw = store_listing(&nix_clone.join("nix/store"))?;
    let fast_disk_raw = store_listing(&fast_clone.join("nix/store"))?;
    let nix_disk = strip_names(nix_disk_raw.clone());
    let fast_disk = strip_names(fast_disk_raw.clone());
    let nix_db: BTreeSet<String> = valid_paths(&nix_clone.join("nix/var/nix"))?
        .into_iter()
        .filter(|p| !excluded_paths.contains(p))
        .collect();
    let fast_db: BTreeSet<String> = valid_paths(&fast_clone.join("nix/var/nix"))?
        .into_iter()
        .filter(|p| !excluded_paths.contains(p))
        .collect();

    println!();
    ok &= diff_sets(
        "on-disk store contents",
        "nix",
        &nix_disk,
        "fast",
        &fast_disk,
    );

    // Junk entries must be cleaned up and disk-deleted entries must
    // stay gone, in both tools, even when Nix's dry-run reporting for
    // them is inconsistent.
    for name in fx.junk.iter().chain(&fx.deleted) {
        for (tool, disk) in [("nix", &nix_disk_raw), ("fast", &fast_disk_raw)] {
            if disk.contains(name) {
                eprintln!("FAIL: {tool} kept corrupted store entry {name}");
                ok = false;
            }
        }
    }
    ok &= diff_sets("ValidPaths after GC", "nix", &nix_db, "fast", &fast_db);

    // Sanity: surviving paths must equal the original minus the dry-run
    // dead set.
    let expected: BTreeSet<String> = valid_paths(Path::new(&ctx.state_dir))?
        .difference(&nix_dead)
        .filter(|p| !excluded_paths.contains(*p))
        .cloned()
        .collect();
    ok &= diff_sets(
        "survivors vs expected",
        "expected",
        &expected,
        "nix",
        &nix_db,
    );

    // Make everything removable for tempdir cleanup.
    make_writable(tmp)?;

    Ok(ok)
}

fn iteration(binary: &Path, seed: u64) -> Result<bool> {
    let mut rng = StdRng::seed_from_u64(seed);
    // Drawn before the graph: root placement depends on it.
    let keep_outputs = rng.random_bool(0.5);
    let nodes = gen_graph(&mut rng, keep_outputs);
    let config = nix_config(&mut rng, keep_outputs);

    println!("--- seed {seed}: {} nodes ---", nodes.len());
    for node in &nodes {
        println!(
            "    {}: {:?} deps={:?} root_output={:?} root_drv={}",
            node.name, node.kind, node.deps, node.root_output, node.root_drv
        );
    }
    for line in config.lines() {
        println!("    {line}");
    }

    // Deterministic per-seed directory: the store prefix is hashed
    // into every store path, so a random tempdir would change all path
    // basenames between runs of the same seed.
    let tmp_path = std::env::temp_dir().join(format!("fuzz-nix-diff-{seed}"));
    if tmp_path.exists() {
        make_writable(&tmp_path)?;
        fs::remove_dir_all(&tmp_path)
            .with_context(|| format!("removing {}", tmp_path.display()))?;
    }
    fs::create_dir(&tmp_path).with_context(|| format!("mkdir {}", tmp_path.display()))?;
    let tmp = TmpDir(tmp_path);
    // Non-chroot local store with explicit dirs so logical and physical
    // store paths coincide; fast-nix-gc has no logical/real split.
    let store_dir = tmp.path().join("nix/store").to_str().unwrap().to_owned();
    let state_dir = tmp.path().join("nix/var/nix").to_str().unwrap().to_owned();
    let ctx = Ctx {
        tmp: tmp.path().to_owned(),
        binary: binary.to_str().unwrap().to_owned(),
        store_uri: format!(
            "local?store={store_dir}&state={state_dir}&log={}/nix/var/log/nix",
            tmp.path().display()
        ),
        store_dir,
        state_dir,
        config,
    };
    let src_dir = tmp.path().join("src-dir");
    fs::create_dir(&src_dir).with_context(|| format!("mkdir {}", src_dir.display()))?;
    write_file(&src_dir.join("file"), "source content\n")?;

    let exprs = tmp.path().join("exprs.nix");
    write_file(&exprs, &render_expr(&nodes, &src_dir))?;
    let exprs_str = exprs.to_str().unwrap();

    let drv_attrs: Vec<&str> = nodes
        .iter()
        .filter(|n| !n.kind.is_eval())
        .map(|n| n.name.as_str())
        .collect();
    eprintln!("building {} derivations...", drv_attrs.len());
    let mut build_args = vec!["build", "-f", exprs_str];
    build_args.extend_from_slice(&drv_attrs);
    build_args.push("--no-link");
    ctx.nix(&build_args)?;
    // toFile/builtins.path entries are written to the store during
    // evaluation.
    for node in nodes.iter().filter(|n| n.kind.is_eval()) {
        ctx.nix(&["eval", "-f", exprs_str, "--raw", &node.name])?;
    }

    let roots_dir = Path::new(&ctx.state_dir).join("gcroots/fuzz");
    fs::create_dir_all(&roots_dir).with_context(|| format!("mkdir {}", roots_dir.display()))?;
    let mut n_roots = 0;
    for node in &nodes {
        if let Some(output) = node.root_output {
            let target = if node.kind.is_eval() {
                ctx.nix(&["eval", "-f", exprs_str, "--raw", &node.name])?
            } else {
                let attr = if output == "out" {
                    node.name.clone()
                } else {
                    format!("{}.{output}", node.name)
                };
                ctx.nix(&[
                    "build",
                    "-f",
                    exprs_str,
                    &attr,
                    "--no-link",
                    "--print-out-paths",
                ])?
            };
            symlink(
                Path::new(target.trim()),
                &roots_dir.join(format!("{}-{output}", node.name)),
            )?;
            n_roots += 1;
        }
        if node.root_drv {
            let attr = format!("{}.drvPath", node.name);
            let drv = ctx.nix(&["eval", "-f", exprs_str, "--raw", &attr])?;
            symlink(
                Path::new(drv.trim()),
                &roots_dir.join(format!("{}-drv", node.name)),
            )?;
            n_roots += 1;
        }
    }
    eprintln!("{n_roots} gcroots");

    let temproots = Path::new(&ctx.state_dir).join("temproots");
    fs::create_dir_all(&temproots).with_context(|| format!("mkdir {}", temproots.display()))?;
    let fx = apply_corruptions(&ctx, &mut rng)?;

    compare_gc(&ctx, &fx)
}

fn main() -> Result<()> {
    let mut pargs = pico_args::Arguments::from_env();
    if pargs.contains("--help") {
        eprintln!("Usage: fuzz-nix-diff [--seed N] [--iterations N]");
        return Ok(());
    }
    let seed: u64 = pargs
        .opt_value_from_str("--seed")?
        .unwrap_or_else(rand::random);
    let iterations: u64 = pargs.opt_value_from_str("--iterations")?.unwrap_or(10);

    let binary = find_binary()?;
    for i in 0..iterations {
        let s = seed + i;
        if !iteration(&binary, s)? {
            eprintln!("\nFAIL at seed {s}; reproduce with:");
            eprintln!("  fuzz-nix-diff --seed {s} --iterations 1");
            std::process::exit(1);
        }
        println!();
    }
    println!("all {iterations} iterations passed (base seed {seed})");
    Ok(())
}
