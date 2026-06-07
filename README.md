# fast-nix-gc

Faster `nix-collect-garbage` and `nix-store --optimise`. See
[real-world timings](#real-world-timings) for a rough idea of how much
faster.

## fast-nix-gc

The stock GC issues one SQLite query per store path while traversing the
reference graph. With ~100K paths this means ~100K B-tree seeks and a lot
of statement-cache churn. fast-nix-gc instead reads `ValidPaths` and `Refs`
once into a CSR adjacency list and runs the liveness BFS over integer node
ids. On a real store with ~30K dead paths this brings the dry-run from
~20s down to ~1s. Disk deletion and `.links` cleanup are parallelized
with rayon.

```
fast-nix-gc [OPTIONS]

  -d, --delete-old              Remove old profile generations
      --delete-older-than SPEC  Delete generations older than SPEC (e.g. 30d, 4h)
      --dry-run                 Show what would be done
      --ensure-free SIZE        Free until SIZE is available (e.g. 50G)
      --keep-recent SPEC        Keep paths registered within SPEC (e.g. 1d)
      --keep-outputs BOOL       Override the keep-outputs nix.conf setting
      --keep-derivations BOOL   Override the keep-derivations nix.conf setting
      --store-dir PATH          Nix store directory [default: /nix/store]
      --state-dir PATH          Nix state directory [default: /nix/var/nix]
```

## fast-nix-optimise

Hardlink-based store dedup, on-disk compatible with `nix-store --optimise`:
same `.links/` layout, same NAR-SHA-256 filenames, the two can be mixed
freely. Hashing and linking run as concurrent tokio tasks; a steady-state
store where most files are already deduped is skipped via `d_ino` from
readdir without rehashing. ~2x faster than upstream on a warm store.

```
fast-nix-optimise [OPTIONS]

      --dry-run             Show what would be done
      --min-size BYTES      Skip files smaller than BYTES
  -j, --jobs N              Concurrency [default: num CPUs]
      --store-dir PATH      Nix store directory [default: /nix/store]
      --state-dir PATH      Nix state directory [default: /nix/var/nix]
```

Both tools take a shared `gc.lock` so they don't race each other or Nix.
While fast-nix-gc deletes, it serves the GC roots socket
(`state/gc-socket/socket`, same protocol as `nix-store --gc`), so concurrent
`nix build`s register temp roots without blocking on the lock.
`--store-dir`/`--state-dir` let you point at a separate store for testing.

## NixOS module

Replaces `nix.gc` and `nix.optimise`:

```nix
{
  inputs.fast-nix-gc.url = "github:Mic92/fast-nix-gc";

  outputs = { nixpkgs, fast-nix-gc, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        fast-nix-gc.nixosModules.default
        {
          services.fast-nix-gc = {
            enable = true;
            automatic = true;
            dates = "weekly";
            deleteOlderThan = "30d";
            ensureFree = "50G";
            keepRecent = "1d";
          };
          services.fast-nix-optimise = {
            enable = true;
            automatic = true;
            dates = "weekly";
          };
        }
      ];
    };
  };
}
```

`services.fast-nix-gc` options:

| Option | Default | Description |
|---|---|---|
| `enable` | `false` | Enable the systemd service |
| `automatic` | `false` | Run on a schedule via systemd timer |
| `dates` | `"03:15"` | When to run (`systemd.time(7)` calendar event) |
| `randomizedDelaySec` | `"0"` | Random delay before each run |
| `persistent` | `true` | Run on next boot if a scheduled run was missed |
| `deleteOlderThan` | `null` | Remove profile generations older than e.g. `"30d"` |
| `ensureFree` | `null` | Stop once this much disk is free, e.g. `"50G"` |
| `keepRecent` | `null` | Pin paths registered within e.g. `"1d"` |
| `package` | this flake's package | Override the binary |
| `extraArgs` | `[ ]` | Extra CLI arguments |

`services.fast-nix-optimise` options:

| Option | Default | Description |
|---|---|---|
| `enable` | `false` | Enable the systemd service |
| `automatic` | `false` | Run on a schedule via systemd timer |
| `dates` | `"04:15"` | When to run; ordered after fast-nix-gc.service when both run |
| `randomizedDelaySec` | `"0"` | Random delay before each run |
| `persistent` | `true` | Run on next boot if a scheduled run was missed |
| `minSize` | `null` | Skip files smaller than this many bytes |
| `jobs` | `null` | Concurrency, defaults to the CPU count |
| `package` | `services.fast-nix-gc.package` | Override the binary |
| `extraArgs` | `[ ]` | Extra CLI arguments |

Without flakes, import `nix/module.nix` directly.

## Building

    nix build

or `nix develop -c cargo build --release`. Without flakes:
`nix-build` (uses `default.nix`).

## Testing

    cargo test       # against a synthetic store in a tempdir
    cargo bench      # throughput across several synthetic store sizes

Two fuzzers back the behavioral claims below:

    # differential: random store graphs, roots, configs and corruption
    # vs nix-store --gc, deterministic per seed
    cargo run --release --bin fuzz-nix-diff -- --iterations 20

    # graph logic vs a reference model, no Nix (stable Rust)
    cargo fuzz run -s none --fuzz-dir fuzz gc_graph

## Behavior

Roots are gathered from `gcroots/`, `profiles/`, `temproots/`, and running
processes (`/proc` on Linux; `libproc` syscalls on macOS instead of
shelling out to `lsof`). Stale temp-root files and dangling auto-roots are
removed. `keep-derivations` and `keep-outputs` are honored with the same
edge semantics as `nix-store --gc`: an alive output keeps its derivation
(`keep-derivations`), an alive derivation keeps its outputs
(`keep-outputs`). This includes content-addressed / dynamic derivations:
drv↔output mappings are read from `ValidPaths.deriver`,
`DerivationOutputs`, and the `BuildTraceV3` table (Nix ≥2.35). The
store is remounted read-write on NixOS where it's bind-mounted read-only.
`tmp-*` build dirs are skipped if a builder still holds the lock.

### Corrupted stores

Disk and database can disagree after crashes or tampering. The GC
repairs both directions: store entries without a database row are
deleted, and rows whose disk entry is missing are removed once the path
is garbage. Nix keeps a `.drv` row forever if its file is gone from
disk, and if that path is reachable from a gcroot, `nix-store --gc`
aborts entirely until the store is repaired by hand. fast-nix-gc
decides liveness from the database alone and handles both.

### Known limitation: keep-outputs with ca-derivations

On Nix without the `BuildTraceV3` table (older than 2.35), a derivation
depending on a floating content-addressed output has deferred output
paths: nothing in the database links it to its outputs until Nix
re-parses the `.drv` at GC time, which fast-nix-gc doesn't do. With
`keep-outputs = true` such a rooted derivation therefore does not keep
its outputs alive. This only loses cache, the outputs stay
rebuildable. Nix itself has no consistent behavior here: `--print-dead`,
the real GC and repeat runs all disagree, since the re-parsing happens
during the GC walk and is order-dependent. Revisit once upstream
defines stable semantics.

The GC takes the same `gc.lock` Nix does, so it won't race with
`nix-build` or another GC. If interrupted mid-run the DB stays
conservative: paths gone from the DB but still on disk are picked up as
unknown entries by the next GC.

## FAQ

**Could this have been implemented in upstream Nix?**

Yes, but C++ and concurrency is a scary combination,
which is why I went with this implementation first.

## Real-world timings

This is not a scientific benchmark, just `journalctl` data from a few of my
machines after switching the daily GC timer from `nix-gc.service` to
`fast-nix-gc.service`. Workloads differ between runs, sample sizes are
small, and hardware varies. Take it as a rough indication of scale, not a
controlled measurement.

| Host | Store profile | `nix-gc` wall clock | `fast-nix-gc` wall clock | Rough speedup |
| --- | --- | --- | --- | --- |
| Laptop (large store, daily dev churn) | ~100k+ paths | 1m25s – 19m, median ≈ 7m | 5s – 20s | ~25–60× |
| Build server (huge store, CI churn) | very large | 19m – 30m, avg ≈ 22m | 7s – 17s | ~80–180× |
| Small VPS (tiny store) | small | 4s – 23s | 15s | ~1–1.5× |

The speedup grows with store size: stock `nix-gc` pays a large fixed cost
walking the whole live closure even when there's almost nothing to delete
(near-idle runs still took minutes on the bigger machines), while
`fast-nix-gc` stays in the single-digit-seconds range. On a tiny store
the overhead is negligible either way and the two are roughly comparable.
