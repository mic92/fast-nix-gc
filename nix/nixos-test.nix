{
  pkgs,
}:
pkgs.testers.runNixOSTest {
  name = "fast-nix-gc";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ./module.nix ];
      services.fast-nix-gc = {
        enable = true;
        keepRecent = "1h";
      };
      services.fast-nix-optimise.enable = true;
      # The CA test phases root a .drv and expect its output to survive,
      # which requires keep-outputs (mirrors nix-store --gc semantics).
      nix.settings.keep-outputs = true;
      # nix-store --optimise (and ours) cannot rename hardlinks across an
      # overlayfs lower/upper boundary: ESTALE on the 9p host store. A
      # store image avoids that, but the overlay upper is still tmpfs and
      # .links/ hardlinks the entire system closure into it; give the VM
      # enough RAM for that.
      virtualisation.useNixStoreImage = true;
      virtualisation.writableStore = true;
      virtualisation.memorySize = 2048;
      # nix from git for the BuildTraceV3 test phase; referenced by store
      # path in the test script, so it must be in the VM's closure.
      virtualisation.additionalPaths = [ pkgs.nixVersions.git ];
      environment.systemPackages = [
        pkgs.hello
        pkgs.sqlite
        pkgs.socat
      ];
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # --- CA derivations: realisation tables must keep drv↔output alive ---
    # Build a CA derivation directly in the system store.
    ca_drv_expr = (
        'derivation { '
        'name = "ca-test"; system = "${pkgs.system}"; '
        'builder = "/bin/sh"; args = ["-c" "echo hello > $out"]; '
        '__contentAddressed = true; outputHashMode = "recursive"; '
        'outputHashAlgo = "sha256"; }'
    )
    ca_out = machine.succeed(
        "nix-build"
        ' --option experimental-features "nix-command ca-derivations"'
        " --no-out-link"
        f" -E '{ca_drv_expr}'"
    ).strip()
    ca_db = "/nix/var/nix/db/db.sqlite"

    ca_drv = machine.succeed(
        f"sqlite3 {ca_db} "
        f"\"SELECT path FROM ValidPaths WHERE path LIKE '%ca-test.drv'\""
    ).strip()
    assert ca_drv, "drv not found in DB"

    # Verify a CA realisation table was populated (Realisations or BuildTraceV3).
    real_count = machine.succeed(
        f"sqlite3 {ca_db} "
        "\"SELECT COUNT(*) FROM (SELECT 1 FROM sqlite_master WHERE type='table' "
        "AND name IN ('Realisations','BuildTraceV3'))\""
    ).strip()
    assert int(real_count) > 0, "no CA realisation table found"

    # Backdate both so --keep-recent doesn't pin them.
    machine.succeed(
        f"sqlite3 {ca_db} "
        f"\"UPDATE ValidPaths SET registrationTime = 1 "
        f"WHERE path IN ('{ca_drv}', '{ca_out}')\""
    )

    # Root the output only; drv should survive via realisation edges.
    machine.succeed(f"nix-store --add-root /tmp/ca-out-root --indirect -r {ca_out}")
    machine.succeed("systemctl start fast-nix-gc.service")
    machine.succeed(f"test -e {ca_out}")
    machine.succeed(f"test -e {ca_drv}")

    # Now root only the drv; output should survive.
    machine.succeed(
        "rm /tmp/ca-out-root",
        f"ln -sf {ca_drv} /nix/var/nix/gcroots/ca-drv-root",
    )
    machine.succeed("systemctl start fast-nix-gc.service")
    machine.succeed(f"test -e {ca_drv}")
    machine.succeed(f"test -e {ca_out}")
    machine.succeed("rm -f /nix/var/nix/gcroots/ca-drv-root")

    # --- BuildTraceV3 (nix from git / unreleased) ---
    # Use nix-from-git to build a second CA derivation; it populates
    # BuildTraceV3 instead of Realisations.
    machine.succeed(
        "cat > /tmp/ca-test2.nix <<'EOF'\n"
        "derivation {\n"
        '  name = "ca-test2";\n'
        '  system = "${pkgs.system}";\n'
        '  builder = "/bin/sh";\n'
        '  args = ["-c" "echo hello2 > $out"];\n'
        "  __contentAddressed = true;\n"
        '  outputHashMode = "recursive";\n'
        '  outputHashAlgo = "sha256";\n'
        "}\nEOF"
    )
    # Use --store local to bypass the system daemon (which is Nix 2.34
    # and would populate Realisations instead of BuildTraceV3).
    ca_out2 = machine.succeed(
        "${pkgs.nixVersions.git}/bin/nix-build --store local"
        ' --option experimental-features "nix-command ca-derivations"'
        " --no-out-link /tmp/ca-test2.nix"
    ).strip()

    ca_drv2 = machine.succeed(
        f"sqlite3 {ca_db} "
        f"\"SELECT path FROM ValidPaths WHERE path LIKE '%ca-test2.drv'\""
    ).strip()
    assert ca_drv2, "ca-test2 drv not found in DB"

    # Verify BuildTraceV3 was populated.
    # BuildTraceV3.drvPath stores the basename (no /nix/store/ prefix).
    ca_drv2_base = ca_drv2.split("/")[-1]
    bt_count = int(machine.succeed(
        f"sqlite3 {ca_db} "
        f"\"SELECT COUNT(*) FROM BuildTraceV3 WHERE drvPath = '{ca_drv2_base}'\""
    ).strip())
    assert bt_count > 0, f"BuildTraceV3 has no entry for {ca_drv2_base}"

    machine.succeed(
        f"sqlite3 {ca_db} "
        f"\"UPDATE ValidPaths SET registrationTime = 1 "
        f"WHERE path IN ('{ca_drv2}', '{ca_out2}')\""
    )

    # Root output; drv should survive via BuildTraceV3.
    machine.succeed(f"ln -sf {ca_out2} /nix/var/nix/gcroots/ca-out2-root")
    machine.succeed("systemctl start fast-nix-gc.service")
    machine.succeed(f"test -e {ca_out2}")
    machine.succeed(f"test -e {ca_drv2}")

    # Root drv; output should survive via BuildTraceV3.
    machine.succeed(
        "rm /nix/var/nix/gcroots/ca-out2-root",
        f"ln -sf {ca_drv2} /nix/var/nix/gcroots/ca-drv2-root",
    )
    machine.succeed("systemctl start fast-nix-gc.service")
    machine.succeed(f"test -e {ca_drv2}")
    machine.succeed(f"test -e {ca_out2}")
    machine.succeed("rm -f /nix/var/nix/gcroots/ca-drv2-root")

    # Create a dead store path: add a file with no roots.
    machine.succeed("echo gc-victim > /tmp/gc-dead")
    dead = machine.succeed("nix-store --add /tmp/gc-dead").strip()

    # Backdate so --keep-recent doesn't pin it.
    machine.succeed(
        f"sqlite3 /nix/var/nix/db/db.sqlite "
        f"\"UPDATE ValidPaths SET registrationTime = 1 WHERE path = '{dead}'\""
    )

    machine.succeed(f"test -e {dead}")

    machine.succeed("systemctl start fast-nix-gc.service")

    machine.fail(f"test -e {dead}")

    # Pinned by a profile root: hello stays.
    machine.succeed("hello --version")

    # gc-socket: a path registered as a root mid-GC must survive.
    # _FAST_NIX_GC_TEST_SYNC blocks the delete loop on a fifo until we've
    # talked to the socket; no timing race.
    machine.succeed("echo s > /tmp/saved", "echo o > /tmp/other")
    saved = machine.succeed("nix-store --add /tmp/saved").strip()
    other = machine.succeed("nix-store --add /tmp/other").strip()
    machine.succeed(
        "sqlite3 /nix/var/nix/db/db.sqlite "
        "'UPDATE ValidPaths SET registrationTime = 1'"
    )

    machine.succeed(
        "mkfifo /tmp/gc-sync",
        "mkdir -p /run/systemd/system/fast-nix-gc.service.d",
        "printf '[Service]\\nEnvironment=_FAST_NIX_GC_TEST_SYNC=/tmp/gc-sync\\n' "
        "> /run/systemd/system/fast-nix-gc.service.d/sync.conf",
        "systemctl daemon-reload",
        "systemctl start --no-block fast-nix-gc.service",
    )
    sock = "/nix/var/nix/gc-socket/socket"
    machine.wait_until_succeeds(f"test -S {sock}", timeout=30)
    ack = machine.succeed(f"echo {saved} | socat -t5 - UNIX-CONNECT:{sock}")
    assert ack == "1", f"expected ack '1', got {ack!r}"
    machine.succeed("echo go > /tmp/gc-sync")
    machine.wait_until_fails("systemctl is-active fast-nix-gc.service", timeout=60)

    machine.succeed(f"test -e {saved}")
    machine.fail(f"test -e {other}")

    # Two store paths with an identical inner file; optimise should
    # collapse them into one inode.
    machine.succeed(
        "mkdir /tmp/p1 /tmp/p2",
        "echo same-content > /tmp/p1/data",
        "echo same-content > /tmp/p2/data",
    )
    p1 = machine.succeed("nix-store --add /tmp/p1").strip()
    p2 = machine.succeed("nix-store --add /tmp/p2").strip()
    # Force a read-only store so optimise must remount rw (issue #7).
    machine.succeed("mount -o remount,ro,bind /nix/store")
    machine.succeed("systemctl start fast-nix-optimise.service")
    out = machine.succeed(f"stat -c %i {p1}/data {p2}/data")
    i1, i2 = out.split()
    assert i1 == i2, f"expected shared inode after optimise, got {i1} {i2}"
  '';
}
