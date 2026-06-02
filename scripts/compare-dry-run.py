#!/usr/bin/env python3
"""Compare fast-nix-gc against nix-store --gc on a throwaway store.

Builds one of every store-path kind Nix can produce (input-addressed,
multi-output, fixed-output, text-addressed, source paths, content-addressed,
impure), wires them together with references and pins a few via gcroots.
Then checks that both tools agree on the dead set (--dry-run vs
--print-dead) and, after a real GC on separate clones of the store, on
what survives on disk and in the DB.
"""

from __future__ import annotations

import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import textwrap
from collections.abc import Callable
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# fast-nix-gc defaults to keep-derivations=true, keep-outputs=false, matching
# Nix. Leave them at defaults so both tools agree on liveness rules.
NIX_CONFIG = textwrap.dedent("""\
    experimental-features = nix-command ca-derivations impure-derivations
    sandbox = false
    substituters =
    builders =
    max-jobs = auto
""")

# Attribute -> kept alive via gcroot?
ATTRS: dict[str, bool] = {
    "base": False,
    "midA": False,
    "midB": False,
    "topRoot": True,
    "topDead": False,
    "multi": False,
    "multiUserRoot": True,
    "fodFlat": False,
    "fodRec": False,
    "fodUserRoot": True,
    "fodUserDead": False,
    "textUserRoot": True,
    "textUserDead": False,
    "caBase": False,
    "caUserRoot": True,
    "caUserDead": False,
    "impure": False,
}


def expr(fod_rec_hash: str) -> str:
    return textwrap.dedent(f"""\
        let
          system = builtins.currentSystem;
          sh = "/bin/sh";
          mk = name: extra: derivation ({{
            inherit name system;
            builder = sh;
            args = [ "-c" "echo $name > $out; for d in $deps; do echo dep $d >> $out; done" ];
            deps = [];
          }} // extra);
        in rec {{
          # Plain input-addressed derivations forming a diamond.
          base    = mk "base"     {{ }};
          midA    = mk "mid-a"    {{ deps = [ base ]; }};
          midB    = mk "mid-b"    {{ deps = [ base ]; }};
          topRoot = mk "top-root" {{ deps = [ midA midB ]; }}; # kept via gcroot
          topDead = mk "top-dead" {{ deps = [ midA ]; }};      # garbage

          # Multi-output derivation; only "out" is referenced by the root,
          # so "dev" becomes garbage independently.
          multi = derivation {{
            name = "multi";
            inherit system;
            builder = sh;
            args = [ "-c" "echo o > $out; echo d > $dev" ];
            outputs = [ "out" "dev" ];
          }};
          multiUserRoot = mk "multi-user-root" {{ deps = [ multi.out ]; }}; # kept

          # Fixed-output derivations: flat (file hash) and recursive (NAR hash).
          fodFlat = derivation {{
            name = "fod-flat";
            inherit system;
            builder = sh;
            args = [ "-c" ": > $out" ];
            outputHashMode = "flat";
            outputHashAlgo = "sha256";
            outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
          }};
          fodRec = derivation {{
            name = "fod-rec";
            inherit system;
            builder = sh;
            args = [ "-c" "echo hi > $out" ];
            outputHashMode = "recursive";
            outputHashAlgo = "sha256";
            outputHash = "{fod_rec_hash}";
          }};
          fodUserRoot = mk "fod-user-root" {{ deps = [ fodFlat fodRec ]; }}; # kept
          fodUserDead = mk "fod-user-dead" {{ deps = [ fodFlat ]; }};        # garbage

          # Text-addressed store path (builtins.toFile) and a plain source
          # path (builtins.path) - neither is built by a derivation.
          textFile = builtins.toFile "a-text-file" "hello text\\n";
          srcDir   = builtins.path {{ path = ./src-dir; name = "src-dir"; }};
          textUserRoot = mk "text-user-root" {{ deps = [ textFile srcDir ]; }}; # kept
          textUserDead = mk "text-user-dead" {{ deps = [ textFile ]; }};        # garbage

          # Floating content-addressed derivations.
          caBase = mk "ca-base" {{
            __contentAddressed = true;
            outputHashMode = "recursive";
            outputHashAlgo = "sha256";
          }};
          caUserRoot = mk "ca-user-root" {{
            deps = [ caBase ];
            __contentAddressed = true;
            outputHashMode = "recursive";
            outputHashAlgo = "sha256";
          }};
          # Input-addressed drv depending on a CA drv -> deferred output path.
          caUserDead = mk "ca-user-dead" {{ deps = [ caBase ]; }};

          # Impure derivation. Output is content-addressed after the fact;
          # nothing else can depend on it without resolving, so it's a leaf.
          impure = derivation {{
            name = "impure";
            inherit system;
            builder = sh;
            args = [ "-c" "read -r u < /proc/sys/kernel/random/uuid; echo $u > $out" ];
            __impure = true;
          }};
        }}
    """)


def run(*argv: str | Path, env: dict[str, str]) -> str:
    proc = subprocess.run(
        [str(a) for a in argv],
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    return proc.stdout


def store_listing(store_dir: Path) -> set[str]:
    """Store entries on disk, excluding internal `.links`/lock files."""
    return {
        p.name
        for p in store_dir.iterdir()
        if p.name not in (".links", ".gc-lock") and not p.name.endswith(".lock")
    }


def valid_paths(state_dir: Path) -> set[str]:
    db = state_dir / "db/db.sqlite"
    with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
        return {row[0] for row in conn.execute("SELECT path FROM ValidPaths")}


def diff_sets(label: str, a_name: str, a: set[str], b_name: str, b: set[str]) -> bool:
    if a == b:
        print(f"OK: {label} identical ({len(a)} entries)")
        return True
    print(f"FAIL: {label} differ", file=sys.stderr)
    for p in sorted(a - b):
        print(f"  only {a_name}:  {p}", file=sys.stderr)
    for p in sorted(b - a):
        print(f"  only {b_name}: {p}", file=sys.stderr)
    return False


def main() -> int:
    binary = Path(
        os.environ.get("FAST_NIX_GC", REPO_ROOT / "target/release/fast-nix-gc")
    )
    if not binary.is_file():
        print("building fast-nix-gc...", file=sys.stderr)
        subprocess.run(
            ["cargo", "build", "--release", "--bin", "fast-nix-gc"],
            cwd=REPO_ROOT,
            check=True,
        )

    with tempfile.TemporaryDirectory() as tmp_str:
        tmp = Path(tmp_str)
        # Use a non-chroot local store with explicit store/state dirs so the
        # logical store path equals the physical path. fast-nix-gc has no
        # logical/real split, and we want path lists to compare 1:1.
        store_dir = tmp / "nix/store"
        state_dir = tmp / "nix/var/nix"
        log_dir = tmp / "nix/var/log/nix"
        store_uri = f"local?store={store_dir}&state={state_dir}&log={log_dir}"

        env = os.environ | {"NIX_CONFIG": NIX_CONFIG}

        def nix(*argv: str | Path) -> str:
            return run("nix", "--store", store_uri, "--offline", *argv, env=env)

        (tmp / "src-dir").mkdir()
        (tmp / "src-dir/file").write_text("source content\n")

        # Recursive FOD output hash is the NAR hash of its build result;
        # compute it up front so the expression is reproducible.
        fod_rec_out = tmp / "fod-rec-out"
        fod_rec_out.write_text("hi\n")
        fod_rec_hash = run(
            "nix", "hash", "path", "--type", "sha256", "--sri", fod_rec_out, env=env
        ).strip()

        exprs = tmp / "exprs.nix"
        exprs.write_text(expr(fod_rec_hash))

        print(f"building {len(ATTRS)} derivations...", file=sys.stderr)
        for attr in ATTRS:
            nix("build", "-f", exprs, attr, "--no-link")

        roots_dir = state_dir / "gcroots/test"
        roots_dir.mkdir(parents=True)
        for attr, keep in ATTRS.items():
            if not keep:
                continue
            out = nix(
                "build", "-f", exprs, attr, "--no-link", "--print-out-paths"
            ).strip()
            (roots_dir / attr).symlink_to(out)

        print("running nix-store --gc --print-dead...", file=sys.stderr)
        nix_dead = sorted(
            run(
                "nix-store", "--store", store_uri, "--gc", "--print-dead", env=env
            ).splitlines()
        )

        print("running fast-nix-gc --dry-run...", file=sys.stderr)
        fast_out = run(
            binary,
            "--dry-run",
            "--store-dir",
            store_dir,
            "--state-dir",
            state_dir,
            env=env,
        )
        # fast-nix-gc prints a human-readable summary line after the path
        # list; keep only the actual store paths.
        store_prefix = f"{store_dir}/"
        fast_dead = sorted(
            p for p in fast_out.splitlines() if p.startswith(store_prefix)
        )

        ok = True
        print()
        print(f"nix dry-run:  {len(nix_dead)} dead paths")
        print(f"fast dry-run: {len(fast_dead)} dead paths")
        ok &= diff_sets(
            "dry-run dead sets", "nix", set(nix_dead), "fast", set(fast_dead)
        )

        # Real GC: clone the store, run nix on one copy and fast-nix-gc on the
        # other, then compare what survives on disk and in the DB.
        print()
        print("cloning store for real GC comparison...", file=sys.stderr)
        # Builders chmod -w outputs; restore writability so copytree/rmtree work.
        subprocess.run(["chmod", "-R", "u+w", tmp / "nix"], check=True)
        nix_clone = tmp / "clone-nix"
        fast_clone = tmp / "clone-fast"
        ignore = shutil.ignore_patterns("gc-socket")
        for clone in (nix_clone, fast_clone):
            shutil.copytree(tmp / "nix", clone / "nix", symlinks=True, ignore=ignore)

        nix_clone_store = nix_clone / "nix/store"
        nix_clone_state = nix_clone / "nix/var/nix"
        fast_clone_store = fast_clone / "nix/store"
        fast_clone_state = fast_clone / "nix/var/nix"

        # The DB records absolute store paths under the original prefix, so
        # pointing either tool at a clone's own path would not match any
        # registered path. Instead move each clone into the original location
        # for the duration of its GC run and move it back afterwards.
        original = tmp / "nix"
        backup = tmp / "nix.orig"
        original.rename(backup)

        def with_tree(src: Path, fn: Callable[[], object]) -> None:
            (src / "nix").rename(original)
            try:
                fn()
            finally:
                original.rename(src / "nix")

        print("running real nix-store --gc...", file=sys.stderr)
        with_tree(
            nix_clone,
            lambda: run("nix-store", "--store", store_uri, "--gc", env=env),
        )

        print("running real fast-nix-gc...", file=sys.stderr)
        with_tree(
            fast_clone,
            lambda: run(
                binary, "--store-dir", store_dir, "--state-dir", state_dir, env=env
            ),
        )

        backup.rename(original)

        nix_disk = store_listing(nix_clone_store)
        fast_disk = store_listing(fast_clone_store)
        nix_db = valid_paths(nix_clone_state)
        fast_db = valid_paths(fast_clone_state)

        print()
        ok &= diff_sets("on-disk store contents", "nix", nix_disk, "fast", fast_disk)
        ok &= diff_sets("ValidPaths after GC", "nix", nix_db, "fast", fast_db)

        # Sanity: surviving paths must equal the original minus the dry-run
        # dead set.
        expected = set(valid_paths(state_dir)) - set(nix_dead)
        ok &= diff_sets("survivors vs expected", "expected", expected, "nix", nix_db)

        # Make everything removable for TemporaryDirectory cleanup.
        subprocess.run(["chmod", "-R", "u+w", tmp], check=False)

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
