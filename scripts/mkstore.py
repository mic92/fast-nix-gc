#!/usr/bin/env python3
"""Build a synthetic Nix store for profiling fast-nix-gc."""

import os
import sqlite3
import sys

# Nix's schema (nix/src/libstore/schema.sql), trimmed to the tables the
# GC touches. Inlined because the repo ships no schema.sql file.
SCHEMA = """
create table if not exists ValidPaths (
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

create table if not exists Refs (
    referrer  integer not null,
    reference integer not null,
    primary key (referrer, reference),
    foreign key (referrer) references ValidPaths(id) on delete cascade,
    foreign key (reference) references ValidPaths(id) on delete restrict
);

create index if not exists IndexReferrer on Refs(referrer);
create index if not exists IndexReference on Refs(reference);

create table if not exists DerivationOutputs (
    drv  integer not null,
    id   text not null,
    path text not null,
    primary key (drv, id),
    foreign key (drv) references ValidPaths(id) on delete cascade
);

create index if not exists IndexDerivationOutputs on DerivationOutputs(path);
"""


def main() -> None:
    if len(sys.argv) != 5:
        print("usage: mkstore.py <dir> <n_paths> <n_roots> <refs_per_path>", file=sys.stderr)
        sys.exit(1)

    base = sys.argv[1]
    n_paths = int(sys.argv[2])
    n_roots = int(sys.argv[3])
    refs_per_path = int(sys.argv[4])

    store = os.path.join(base, "store")
    state = os.path.join(base, "state")
    os.makedirs(store, exist_ok=True)
    os.makedirs(os.path.join(store, ".links"), exist_ok=True)
    os.makedirs(os.path.join(state, "db"), exist_ok=True)
    os.makedirs(os.path.join(state, "gcroots"), exist_ok=True)
    os.makedirs(os.path.join(state, "profiles"), exist_ok=True)
    os.makedirs(os.path.join(state, "temproots"), exist_ok=True)

    conn = sqlite3.connect(os.path.join(state, "db/db.sqlite"))
    conn.executescript(SCHEMA)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=OFF")

    paths = []
    for i in range(n_paths):
        h = f"{i:032x}"
        name = f"pkg-{i}"
        full = f"{store}/{h}-{name}"
        disk = full
        os.makedirs(disk, exist_ok=True)
        with open(os.path.join(disk, "out"), "wb") as f:
            f.write(b"\0" * 64)
        paths.append((full, h))

    conn.executemany(
        "INSERT INTO ValidPaths (path, hash, registrationTime, narSize) VALUES (?, ?, 1000, 1024)",
        [(p, f"sha256:{h}") for p, h in paths],
    )

    refs = []
    for i in range(n_paths):
        referrer = i + 1  # autoincrement
        for j in range(1, refs_per_path + 1):
            if i >= j:
                refs.append((referrer, i - j + 1))
    conn.executemany("INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?, ?)", refs)
    conn.commit()
    conn.close()

    for i in range(n_paths - n_roots, n_paths):
        h = f"{i:032x}"
        target = f"{store}/{h}-pkg-{i}"
        link = os.path.join(state, "gcroots", f"root-{i}")
        os.symlink(target, link)

    print(f"created {n_paths} paths, {n_roots} roots, {len(refs)} refs in {base}")


if __name__ == "__main__":
    main()
