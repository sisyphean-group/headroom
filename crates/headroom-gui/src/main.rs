//! `headroom-gui` — native gpu-accelerated monitor for the headroom
//! daemon, built on [iced](https://iced.rs). additive and optional, with
//! full control parity with the ratatui tui (route/bypass/per-app/profile)
//! by mouse + keyboard; ships as its own nix package, separate from the
//! daemon. build from the nix dev shell (supplies iced's system libs).

mod app;
mod io;

use std::path::PathBuf;

use crate::app::HeadroomApp;

fn main() -> anyhow::Result<()> {
    init_tracing();

    let socket = parse_socket_arg();

    // connect + initial fetch before opening any window, so a dead daemon
    // fails fast. the reader thread lives inside the returned `Bootstrap`;
    // the iced `subscription` poll tick drains its channel.
    let bootstrap = io::start(socket)?;

    // iced's `BootFn` is `Fn` (callable repeatedly) but `Bootstrap` owns a
    // receiver and is single-use; stash it and `take()` on the first boot.
    let boot = std::sync::Mutex::new(Some(bootstrap));

    iced::application(
        move || HeadroomApp::new(boot.lock().unwrap().take().expect("boot runs once")),
        HeadroomApp::update,
        HeadroomApp::view,
    )
    .title(HeadroomApp::title)
    .subscription(HeadroomApp::subscription)
    .theme(|_state: &HeadroomApp| iced::Theme::Dark)
    .window_size(iced::Size::new(900.0, 680.0))
    .run()
    .map_err(|e| anyhow::anyhow!("iced: {e}"))?;

    Ok(())
}

/// minimal `--socket <path>` parsing — the only flag, so no clap.
fn parse_socket_arg() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => return args.next().map(PathBuf::from),
            other => {
                if let Some(path) = other.strip_prefix("--socket=") {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }
    None
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("headroom_gui=info")),
        )
        .with_writer(std::io::stderr)
        .init();
}
