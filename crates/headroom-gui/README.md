# headroom-gui

native, gpu-accelerated monitor for the headroom daemon, built on
[iced](https://iced.rs).

**additive and optional**: the daemon, the `headroom` cli, and the
ratatui tui (`headroom monitor`) are unchanged and stay graphics-free.
the shipped nix package excludes this crate.

## status

a window showing, live from the daemon's event stream:

- header: active profile, bypass / per-app badges, version, uptime;
- bus dsp meters (agc target, compressor gr, limiter gr, true peak);
- BS.1770 loudness (momentary / short-term / integrated);
- the per-stream routing list with per-app reduction.

full control parity with the tui, by **mouse and keyboard**:

- bypass / per-app badges are buttons; a dropdown switches profiles;
- each stream row has route (processed ↔ bypass), per-app, and reset
  actions, and clicking a row selects it;
- hotkeys mirror the tui — `b` bypass, `p` per-app master, `r`/Enter
  route, `a` per-app, `x` reset, `j`/`k` (or ↑/↓) move the selection.

a reader thread feeds state over a channel; iced's `time::every`
subscription drains it at ~20 Hz. control ops run on a separate thread (a
second daemon connection, like the tui) so the ui never blocks on ipc; it
updates optimistically and the event stream + a ~1 Hz `per-app.list` poll
reconcile.

## why iced (and a trimmed feature set)

iced over zed's gpui, for dependency weight: gpui unconditionally drags an
av1 encoder, an http client, and three async runtimes (it's carved out of
the zed editor). the tree here is ~199 crates vs ~525 for gpui-component.

the iced feature set is trimmed on purpose (see `Cargo.toml`):
`default-features = false` drops `linux-theme-detection` (which pulls
`zbus` + `async-process` just to probe the desktop theme — we hardcode
dark). `smol` backs the `time::every` timer without pulling `tokio`.

## toolchain

the workspace pins no toolchain — it builds on whatever rustc nixpkgs
provides. iced's transitive deps want a recent rustc, so build from the
nix dev shell, which supplies both the toolchain and iced's system libs:

```sh
nix develop            # rustc + iced's system libs (Vulkan/Wayland/…)
cargo build -p headroom-gui
```

## running

needs a display / gpu (vulkan via wgpu; falls back to tiny-skia software
rendering). start the daemon, then:

```sh
cargo run -p headroom-gui                 # default socket
cargo run -p headroom-gui -- --socket /path/to/headroom.sock
```

the `nix develop` shell exports `LD_LIBRARY_PATH` for the vulkan /
wayland / xkbcommon libs iced/wgpu dlopen at runtime.
