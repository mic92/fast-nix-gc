# fast-nix-gc

A faster `nix-collect-garbage`.

The stock GC issues one SQLite query per store path while traversing the
reference graph. With ~100K paths this means ~100K B-tree seeks and a lot
of statement-cache churn. fast-nix-gc instead reads `ValidPaths` and `Refs`
once into a CSR adjacency list and runs the liveness BFS over integer node
ids. On a real store with ~30K dead paths this brings the dry-run from
~20s down to ~1s. Disk deletion and `.links` cleanup are parallelized
with rayon.

## Usage

```
fast-nix-gc [OPTIONS]

  -d, --delete-old              Remove old profile generations
      --delete-older-than SPEC  Delete generations older than SPEC (e.g. 30d, 4h)
      --dry-run                 Show what would be done
      --ensure-free SIZE           Free until SIZE is available (e.g. 50G)
      --keep-recent SPEC        Keep paths registered within SPEC (e.g. 1d)
      --store-dir PATH          Nix store directory [default: /nix/store]
      --state-dir PATH          Nix state directory [default: /nix/var/nix]
```

`--store-dir`/`--state-dir` let you point at a separate store for testing.

## NixOS module

Replace `nix.gc` with the bundled module:

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
        }
      ];
    };
  };
}
```

Options:

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

Without flakes, import `nix/module.nix` directly.

## Building

    nix build

or `nix develop -c cargo build --release`. Without flakes:
`nix-build` (uses `default.nix`).

## Testing

    cargo test       # against a synthetic store in a tempdir
    cargo bench      # throughput across several synthetic store sizes

## Behavior

Roots are gathered from `gcroots/`, `profiles/`, `temproots/`, and running
processes (`/proc` on Linux; `libproc` syscalls on macOS instead of
shelling out to `lsof`). Stale temp-root files and dangling auto-roots are
removed. `keep-derivations` and `keep-outputs` are honored, including for
content-addressed derivations (via the `DerivationOutputs` table). The
store is remounted read-write on NixOS where it's bind-mounted read-only.
`tmp-*` build dirs are skipped if a builder still holds the lock.

The GC takes the same `gc.lock` Nix does, so it won't race with
`nix-build` or another GC. If interrupted mid-run the DB stays
conservative: paths gone from the DB but still on disk are picked up as
unknown entries by the next GC.

## Not implemented

- GC roots socket. Builders block on the GC lock instead of registering
  new roots while the GC runs.
- Reading `keep-derivations`/`keep-outputs` from `nix.conf` (defaults are
  used: `keep-derivations=true`, `keep-outputs=false`).
