#!/usr/bin/env python3
"""Run alloy/gc_db_consistency.als: checks must hold, runs must be satisfiable."""

import json
import subprocess
import sys
import tempfile
from pathlib import Path

MODEL = Path(__file__).parent.parent / "alloy" / "gc_db_consistency.als"


def main() -> int:
    with tempfile.TemporaryDirectory() as out:
        subprocess.run(
            [
                "nix",
                "run",
                "nixpkgs#alloy6",
                "--",
                "exec",
                "-f",
                "-o",
                out,
                "-c",
                "*",
                str(MODEL),
            ],
            check=True,
        )
        receipt = json.loads((Path(out) / "receipt.json").read_text())

    ok = True
    for name, cmd in receipt["commands"].items():
        is_check = cmd["source"].startswith("check")
        sat = any(s.get("instances") for s in cmd.get("solution", []))
        passed = sat != is_check
        ok &= passed
        kind = "check" if is_check else "run"
        if is_check:
            detail = "counterexample found" if sat else "holds"
        else:
            detail = "satisfiable" if sat else "vacuous"
        print(f"{'ok' if passed else 'FAIL':4} {kind:5} {name}: {detail}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
