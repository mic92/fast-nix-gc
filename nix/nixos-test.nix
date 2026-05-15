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
      # nix-store --optimise (and ours) cannot rename hardlinks across an
      # overlayfs lower/upper boundary: ESTALE on the 9p host store. A
      # store image avoids that, but the overlay upper is still tmpfs and
      # .links/ hardlinks the entire system closure into it; give the VM
      # enough RAM for that.
      virtualisation.useNixStoreImage = true;
      virtualisation.writableStore = true;
      virtualisation.memorySize = 2048;
      environment.systemPackages = [
        pkgs.hello
        pkgs.sqlite
      ];
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

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
