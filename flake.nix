{
  description = "automatic loudness and per-app volume control (pipewire)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
        };

      perSystem =
        system:
        let
          pkgs = pkgsFor system;

          # native libs the audio crates link against
          nativeAudioBuildInputs = with pkgs; [
            pipewire
            pipewire.dev
          ];

          nativeBuildTools = with pkgs; [
            pkg-config
            clang
          ];

          # system libs iced (wgpu/winit/cosmic-text) compiles + dlopens at runtime.
          # wayland-only (x11 feature off); re-add libx11/libxcursor/libxi/libxrandr/libxcb if x11 re-enabled
          guiLibs = with pkgs; [
            vulkan-loader
            libxkbcommon
            wayland
            libGL
            fontconfig
          ];

          commonEnv = {
            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
            PKG_CONFIG_PATH = "${pkgs.pipewire.dev}/lib/pkgconfig";
          };
        in
        {
          # nix develop — dev environment
          devShell = pkgs.mkShell (
            {
              name = "headroom-dev";

              nativeBuildInputs = nativeBuildTools ++ [
                pkgs.cargo
                pkgs.clippy
                pkgs.rust-analyzer
                pkgs.rustc
                pkgs.rustfmt
              ];

              buildInputs =
                nativeAudioBuildInputs
                ++ guiLibs
                ++ (with pkgs; [
                  socat # poke the ipc socket
                  jq # pretty-print json
                  pipewire # pw-cli, pw-cat, etc.
                  wireplumber
                ]);

              shellHook = ''
                echo "headroom dev shell — rustc $(rustc --version | cut -d' ' -f2)"
                echo "  cargo build / cargo test for iteration."
                echo "  nix build  .#headroom  for the packaged binary."
                echo "  cargo run -p headroom-gui  for the GUI (needs a display)."
                export RUST_BACKTRACE=1
                export RUST_LOG=headroom=debug,info
                # iced/wgpu dlopen vulkan/wayland/xkbcommon at runtime
                export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath guiLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              '';
            }
            // commonEnv
          );

          # nix build — packaged daemon + cli
          headroom = pkgs.rustPlatform.buildRustPackage (
            {
              pname = "headroom";
              # workspace version (per-crate manifests use version.workspace = true)
              version =
                (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version
                + "-"
                + (toString self.lastModifiedDate);

              src = ./.;

              cargoLock = {
                lockFile = ./Cargo.lock;
                # allowBuiltinFetchGit = true;
              };

              nativeBuildInputs = nativeBuildTools;
              buildInputs = nativeAudioBuildInputs;

              # one binary: `headroom` (cli + daemon)
              cargoBuildFlags = [
                "-p"
                "headroom-cli"
              ];
              doCheck = true;
              # exclude headroom-gui from the test sweep — pulls iced's gpu stack, not part of the daemon
              cargoTestFlags = [
                "--workspace"
                "--exclude"
                "headroom-gui"
              ];

              # systemd user unit (@bindir@ templated to this derivation's bin, not PATH)
              # + canonical profiles under share/headroom/profiles (daemon finds via $XDG_DATA_DIRS)
              postInstall = ''
                install -Dm644 contrib/systemd/headroom.service \
                  "$out/lib/systemd/user/headroom.service"
                substituteInPlace "$out/lib/systemd/user/headroom.service" \
                  --replace-fail '@bindir@' "$out/bin"

                mkdir -p "$out/share/headroom/profiles"
                cp -r profiles/. "$out/share/headroom/profiles/"
              '';

              meta = with pkgs.lib; {
                description = "automatic loudness and per-app volume control (pipewire)";
                license = licenses.gpl3Plus;
                platforms = platforms.linux;
                mainProgram = "headroom";
              };
            }
            // commonEnv
          );

          # `nix build .#headroom-gui` — optional iced GUI monitor.
          # separate package so the default build stays graphics-free. iced/wgpu
          # dlopen vulkan/libGL/wayland/xkbcommon at runtime → wrap with LD_LIBRARY_PATH.
          # also ships the .desktop entry.
          headroom-gui = pkgs.rustPlatform.buildRustPackage (
            {
              pname = "headroom-gui";
              version =
                (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version
                + "-"
                + (toString self.lastModifiedDate);

              src = ./.;

              cargoLock = {
                lockFile = ./Cargo.lock;
              };

              # makeWrapper for the LD_LIBRARY_PATH wrap. gui doesn't link pipewire
              # (talks to the daemon over the socket via headroom-client) → no pipewire/clang
              nativeBuildInputs = [
                pkgs.pkg-config
                pkgs.makeWrapper
              ];
              buildInputs = guiLibs;

              cargoBuildFlags = [
                "-p"
                "headroom-gui"
              ];
              # gui tests drive iced view/update; no display in the sandbox.
              # run them via `cargo test -p headroom-gui` in the dev shell instead
              doCheck = false;

              postInstall = ''
                install -Dm644 contrib/desktop/headroom-gui.desktop \
                  "$out/share/applications/headroom-gui.desktop"
                substituteInPlace "$out/share/applications/headroom-gui.desktop" \
                  --replace-fail '@bindir@' "$out/bin"
              '';

              postFixup = ''
                wrapProgram "$out/bin/headroom-gui" \
                  --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath guiLibs}"
              '';

              meta = with pkgs.lib; {
                description = "automatic loudness and per-app volume control (pipewire)";
                license = licenses.eupl12;
                platforms = platforms.linux;
                mainProgram = "headroom-gui";
              };
            }
            // commonEnv
          );
        };
    in
    {
      devShells = forAllSystems (system: {
        default = (perSystem system).devShell;
      });

      packages = forAllSystems (
        system:
        let
          ps = perSystem system;
        in
        rec {
          default = headroom;
          headroom = ps.headroom;
          headroom-gui = ps.headroom-gui;
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixpkgs-fmt);

      # System-independent outputs — modules.
      nixosModules.default = import ./nix/nixos-module.nix self;
    };
}
