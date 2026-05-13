# fast-gc

A faster `nix-collect-garbage`.

The stock GC issues one SQLite query per store path while traversing the
reference graph. With ~100K paths this means ~100K B-tree seeks and a lot
of statement-cache churn. fast-gc instead reads `ValidPaths` and `Refs`
once into a CSR adjacency list and runs the liveness BFS over integer node
ids. On a real store with ~30K dead paths this brings the dry-run from
~20s down to ~1s. Disk deletion and `.links` cleanup are parallelized
with rayon.

## Usage

```
fast-gc [OPTIONS]

  -d, --delete-old              Remove old profile generations
      --delete-older-than SPEC  Delete generations older than SPEC (e.g. 30d, 4h)
      --dry-run                 Show what would be done
      --max-freed BYTES         Maximum bytes to free
      --store-dir PATH          Nix store directory [default: /nix/store]
      --state-dir PATH          Nix state directory [default: /nix/var/nix]
```

`--store-dir`/`--state-dir` let you point at a separate store for testing.

## Building

    nix build

or `nix develop -c cargo build --release`.

## Testing

    cargo test       # against a synthetic store in a tempdir
    cargo bench      # throughput across several synthetic store sizes

## Behavior

Roots are gathered from `gcroots/`, `profiles/`, `temproots/`, and running
processes (`/proc` on Linux; `libproc` syscalls on macOS instead of
shelling out to `lsof`). Stale temp-root files and dangling auto-roots are
removed. `keep-derivations` is honored (`.drv` of an alive output stays
alive). The store is remounted read-write on NixOS where it's bind-mounted
read-only. `tmp-*` build dirs are skipped if a builder still holds the
lock.

The GC takes the same `gc.lock` Nix does, so it won't race with
`nix-build` or another GC. If interrupted mid-run the DB stays
conservative: paths gone from the DB but still on disk are picked up as
unknown entries by the next GC.

## Not implemented

- `keep-outputs` (off by default in Nix). Would need reverse deriver edges.
- GC roots socket. Builders block on the GC lock instead of registering
  new roots while the GC runs.
