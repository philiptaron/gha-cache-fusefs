# Mounts the filesystem through the kernel against the fake cache service and
# runs the end-to-end script's three phases as separate mounts.
{ package }:
{ pkgs, ... }:
{
  name = "gha-cache-fusefs";

  nodes.machine = {
    environment.systemPackages = [
      package
      pkgs.openssl
      pkgs.rsync
      pkgs.squashfsTools
      pkgs.squashfuse
    ];
    boot.kernelModules = [ "fuse" ];
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.succeed(
        "systemd-run --unit fake-cache ${package}/bin/gha-cache-fusefs fake-server"
        + " --listen 127.0.0.1:8123 --env-file /run/fake-cache.env"
    )
    machine.wait_until_succeeds("test -s /run/fake-cache.env")

    def phase(name):
        with subtest(name):
            machine.succeed(
                "set -a; . /run/fake-cache.env; set +a;"
                + f" MNT=/mnt/cache RUN=vm RUNNER_TEMP=/tmp bash ${./tests/e2e.sh} {name} >&2"
            )

    phase("write")
    phase("read")
    phase("verify")

    with subtest("the daemon leaves nothing behind"):
        machine.fail("mountpoint -q /mnt/cache")
        # The brackets keep pgrep from matching the shell that runs it.
        machine.fail("pgrep -f '[g]ha-cache-fusefs mount'")
  '';
}
