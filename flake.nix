{
  description = "Application catalog and activation daemon for Shelllist";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        system: pkgs:
        let
          appDaemon = pkgs.rustPlatform.buildRustPackage {
            pname = "app-daemon";
            version = "0.1.0";
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes = {
                "shelllist-daemon-core-0.1.0" = "sha256-3tXlcM+PtEExZ91mtTIy/3Jpcj6o9MBlypPd3Z1ioVw=";
                "shelllist-daemon-tokio-0.1.0" = "sha256-3tXlcM+PtEExZ91mtTIy/3Jpcj6o9MBlypPd3Z1ioVw=";
              };
            };
            nativeBuildInputs = [ pkgs.makeWrapper ];
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
                    pkgs.hyprland
                    pkgs.util-linux
                    pkgs.uwsm
                    pkgs.xdg-terminal-exec
                  ]
                }
            '';
            meta = {
              description = "Application catalog and activation daemon for Shelllist";
              mainProgram = "app-daemon";
              platforms = pkgs.lib.platforms.linux;
            };
          };
        in
        {
          default = appDaemon;
        }
      );

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
        }
      );

      devShells = forAllSystems (
        system: pkgs: {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              cargo-llvm-cov
              clippy
              jq
              rust-analyzer
              rustc
              rustfmt
              llvmPackages.llvm
            ];
            RUST_BACKTRACE = "1";
            RUST_LOG = "app_daemon=debug";
          };
        }
      );

      formatter = forAllSystems (system: pkgs: pkgs.nixfmt-tree);
    };
}
