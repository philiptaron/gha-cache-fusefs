{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "gha-cache-fusefs";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./tests
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  # The integration tests talk to an in-process fake cache service on 127.0.0.1.
  __darwinAllowLocalNetworking = true;

  meta = {
    description = "Mount the GitHub Actions cache as a FUSE filesystem";
    homepage = "https://github.com/philiptaron/gha-cache-fusefs";
    license = lib.licenses.mit;
    mainProgram = "gha-cache-fusefs";
  };
}
