# headroom

automatic loudness and per-app volume control for linux (pipewire).

headroom sits between your apps and your output: it levels the loud,
unpredictable sources to a consistent volume and catches sudden peaks,
and leaves the apps you've already dialled in (music, games, your DAW)
alone.

## what it does

two independent controls; use either or both.

- **the processing bus.** apps you route through it are leveled together
  to one consistent loudness and held under a peak ceiling, so the noisy,
  unpredictable sources (browsers, chat, random video) come out even and
  can't suddenly spike. apps you don't route stay bit-exact, straight to
  your output with nothing added.
- **per-app level.** independent of the bus, headroom can ride a single
  app's volume toward a target. it adds nothing to the signal path, so it
  works even on apps left untouched, e.g. keeping one chatty source in
  check while a game stays on its direct, unprocessed path.

route the chaotic stuff through the bus for group leveling and peak
safety; reach for per-app level to rein in one specific app without
processing it. the rest is glue:

- **profiles.** named configs you switch per context (quiet hours, movie,
  gaming, podcast). edits apply live, no restart or dropout.
- **monitor and control.** a terminal monitor (and an optional desktop
  app) show each stream and how it's handled, and let you move an app
  between processed and untouched while it's playing.
- **cli and socket.** every action is a command or a JSON socket call, so
  it scripts into a status bar, hotkeys, and the like.

## status

pre-release. expect the occasional rough edge around stream routing and
per-app control.

## installing

### nix (flake)

```sh
nix run github:manic-systems/headroom -- daemon          # one-shot run
nix profile install github:manic-systems/headroom        # add to $PATH
```

**NixOS**:

```nix
{
  inputs.headroom.url = "github:manic-systems/headroom";

  # in your configuration:
  imports = [ inputs.headroom.nixosModules.default ];
  programs.headroom.enable = true;          # add programs.headroom.gui.enable for the desktop app
}
```

then `systemctl --user enable --now headroom`.

### other distros

```sh
cargo install --path crates/headroom-cli
mkdir -p ~/.config/headroom/profiles && cp profiles/*.toml ~/.config/headroom/profiles/
install -Dm644 contrib/systemd/headroom.service ~/.config/systemd/user/headroom.service
sed -i "s|@bindir@|$(dirname "$(command -v headroom)")|" ~/.config/systemd/user/headroom.service
systemctl --user daemon-reload && systemctl --user enable --now headroom
```

## usage

with the daemon running:

```sh
headroom status                   # what's playing and how it's handled
headroom profile list
headroom profile use night        # switch context
headroom monitor                  # live terminal monitor
headroom route set firefox bypass # leave firefox untouched
headroom bypass on                # all processing off, instantly
```

`headroom --help` for the rest. control it from a desktop window with the
optional GUI (`headroom-gui`).

## building

```sh
nix develop
cargo build
cargo test --workspace
nix build              # packaged daemon + cli
nix build .#headroom-gui   # the desktop app (separate, optional)
```

daemon + cli are graphics-free; the GUI is a separate crate you only
build if you want it.

## license

EUPL-1.2 for the daemon and CLI (see [LICENSE](LICENSE)). `headroom-dsp`
and `headroom-ipc` are MPL-2.0 (EUPL-compatible) for reuse by third-party
clients and plugin hosts.
