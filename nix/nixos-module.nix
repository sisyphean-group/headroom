# NixOS module — system-wide install. headroom is a user-scope daemon
# (talks to the user's pipewire session), so this module just:
#   1. binary on every login's PATH
#   2. systemd user unit in the system-wide location (`systemctl --user enable --now headroom`)
#   3. bundled profiles into the system profile's share/ (daemon finds via $XDG_DATA_DIRS)
#   4. assert pipewire+wireplumber are enabled
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkEnableOption
    mkOption
    mkIf
    types
    literalExpression
    ;

  cfg = config.programs.headroom;
in
{
  options.programs.headroom = {
    enable = mkEnableOption "Headroom — automatic loudness and per-app volume control (pipewire)";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.headroom;
      defaultText = literalExpression "headroom.packages.\${pkgs.system}.headroom";
      description = ''
        The headroom package to install system-wide.
      '';
    };

    gui = {
      enable = mkEnableOption "the Headroom GUI (iced monitor + control) and its application-launcher entry";

      package = mkOption {
        type = types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.headroom-gui;
        defaultText = literalExpression "headroom.packages.\${pkgs.system}.headroom-gui";
        description = ''
          headroom-gui package installed when `programs.headroom.gui.enable`
          is set. ships the `headroom-gui` binary + `.desktop` entry, so it
          appears in application launchers automatically.
        '';
      };
    };

    autoStart = mkOption {
      type = types.bool;
      default = true;
      description = ''
        pull the headroom user service into every login's
        `graphical-session.target` so it autostarts without `systemctl
        --user enable headroom`.

        `systemd.packages` installs the unit but doesn't process its
        `[Install]` section, so the `graphical-session.target.wants`
        symlink is never created (unit sits `linked-runtime`). this
        materialises that symlink at the system level. false = unit
        install-only, manage activation yourself.
      '';
    };
  };

  config = mkIf cfg.enable {
    # binary on PATH; gui package too when enabled (drops a
    # share/applications/.desktop entry NixOS aggregates into the menu)
    environment.systemPackages = [ cfg.package ] ++ lib.optional cfg.gui.enable cfg.gui.package;

    # make the user unit discoverable by `systemctl --user`; materialises
    # /etc/systemd/user/headroom.service → package's lib/systemd/user/headroom.service
    systemd.packages = [ cfg.package ];

    # systemd.packages doesn't process [Install] → add a `wantedBy` so NixOS
    # emits the graphical-session.target.wants symlink. unit body comes from the
    # package, so we must NOT author headroom.service here (duplicate-unit error);
    # overrideStrategy="asDropin" makes this a drop-in (empty bar the wants link).
    # not via environment.etc: /etc/systemd/user is a read-only store symlink.
    systemd.user.services.headroom = mkIf cfg.autoStart {
      overrideStrategy = "asDropin";
      wantedBy = [ "graphical-session.target" ];
    };

    # daemon scans each $XDG_DATA_DIRS entry's headroom/profiles/. NixOS only
    # links an allowlist of share/ subdirs into the system profile
    # (environment.pathsToLink), so opt share/headroom in or the shipped
    # profiles get dropped and the daemon sees only the built-in `default`.
    environment.pathsToLink = [ "/share/headroom" ];

    # fail eval (not a confusing runtime error) if headroom is on but pipewire isn't
    assertions = [
      {
        assertion = config.services.pipewire.enable;
        message = ''
          programs.headroom.enable requires services.pipewire.enable = true;
          headroom is a PipeWire-only daemon.
        '';
      }
    ];
  };
}
