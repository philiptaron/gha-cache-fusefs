{
  description = "Mount the GitHub Actions cache as a FUSE filesystem";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        {
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.gha-cache-fusefs;
          gha-cache-fusefs = pkgs.callPackage ./fusefs/package.nix { };
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # A fully static (musl) binary that runs on any Linux runner without Nix.
          static = pkgs.pkgsStatic.callPackage ./fusefs/package.nix { };
        }
      );

      checks = forAllSystems (
        pkgs:
        let
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.gha-cache-fusefs;
        in
        {
          # The package build runs `cargo test`, including the fake-server integration tests.
          package = pkg;

          clippy = pkg.overrideAttrs (old: {
            pname = "gha-cache-fusefs-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --all-targets --offline -- --deny warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = "touch $out";
            dontFixup = true;
          });

          fmt =
            pkgs.runCommand "gha-cache-fusefs-fmt"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                ];
              }
              ''
                cd ${pkg.src}
                cargo fmt --check
                touch $out
              '';
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vm = pkgs.testers.runNixOSTest (import ./fusefs/nixos-test.nix { package = pkg; });
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.gha-cache-fusefs ];
          packages = [
            pkgs.cargo
            pkgs.clippy
            pkgs.rustc
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
