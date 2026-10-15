{
  description = "Application catalog and activation daemon for Shelllist";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  # One current sibling framework, never a vendored copy or deployment pin.
  # Use ../daemon-framework/tools/local-build for Nix builds/checks.
  inputs.daemonFramework = {
    url = "git+file:../daemon-framework";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      daemonFramework,
    }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        system: pkgs:
        let
          appDaemon = daemonFramework.lib.buildRustPackage pkgs {
            pname = "app-daemon";
            version = "0.1.0";
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
                ./test_support
                ./benches
              ];
            };
            postUnpack = ''
              cp -R --no-preserve=mode ${daemonFramework.lib.daemonSource pkgs} "$(dirname "$sourceRoot")/daemon-framework"
            '';
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            nativeCheckInputs = [
              pkgs.bash
              pkgs.coreutils
              pkgs.dbus
            ];
            strictDeps = true;
            postInstall = ''
              install -Dm644 ${./packaging/systemd/app-daemon.service} $out/share/systemd/user/app-daemon.service
              install -Dm644 ${./packaging/dbus/org.laufan.AppDaemon.service} \
                $out/share/dbus-1/services/org.laufan.AppDaemon.service
              substituteInPlace \
                $out/share/systemd/user/app-daemon.service \
                $out/share/dbus-1/services/org.laufan.AppDaemon.service \
                --replace-fail @out@ $out
            '';
            postFixup = ''
              wrapProgram $out/bin/app-daemon \
                --prefix PATH : ${
                  pkgs.lib.makeBinPath [
                    pkgs.coreutils
                    pkgs.gtk3
                    pkgs.util-linux
                    pkgs.systemd
                    pkgs.uwsm
                    pkgs.xdg-terminal-exec
                  ]
                }
            '';
            meta = {
              description = "Application catalog and activation daemon for Shelllist";
              mainProgram = "app-daemon";
              platforms = systems;
            };
          };
        in
        {
          default = appDaemon;
        }
      );

      nixosModules.default = import ./packaging/nixos.nix;

      apps = forAllSystems (
        system: pkgs: {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/app-daemon";
          };
        }
      );

      checks = forAllSystems (
        system: pkgs: {
          default = self.packages.${system}.default;
          modules = let
            cfg = (nixpkgs.lib.nixosSystem {
              inherit system;
              modules = [ self.nixosModules.default { services.app-daemon.resources.enableRaplAccess = true; } ];
            }).config;
          in assert nixpkgs.lib.hasInfix "chmod 0440" cfg.services.udev.extraRules;
          assert nixpkgs.lib.hasInfix "chgrp video" cfg.services.udev.extraRules;
          assert !(nixpkgs.lib.hasInfix "ATTR{enabled}" cfg.services.udev.extraRules);
          pkgs.runCommand "app-daemon-module-contract" { } "touch $out";
        }
      );

      devShells = forAllSystems (
        system: pkgs: {
          default = pkgs.mkShell {
            packages =
              (daemonFramework.lib.toolchain pkgs).packages
              ++ (with pkgs; [
                bash
                coreutils
                dbus
                git
                cargo-llvm-cov
                cargo-machete
                jq
                python3
                rust-analyzer
                llvmPackages.llvm
              ]);
            RUST_BACKTRACE = "1";
            RUST_LOG = "app_daemon=debug";
          };
        }
      );

      formatter = forAllSystems (system: pkgs: pkgs.nixfmt-tree);
    };
}
