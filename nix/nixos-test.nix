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
      virtualisation.writableStore = true;
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
  '';
}
