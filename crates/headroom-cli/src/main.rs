//! `headroom` cli — connects to the daemon over its uds; `daemon` runs core in-process.

#![forbid(unsafe_code)]

#[cfg(feature = "tui")]
mod tui;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use headroom_client::{Client, ClientError, Route, Topic};

// debug-only: arms core's audio-thread `assert_no_alloc` blocks; no-op in release.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// headroom cli.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// override the daemon control socket path.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// run the daemon in the foreground.
    Daemon {
        /// extra `*.toml` profile dir (repeatable). precedes shipped/user
        /// profiles; first dir is watched for live edits.
        #[arg(long = "profile-dir", value_name = "DIR")]
        profile_dir: Vec<PathBuf>,
    },

    /// show daemon status.
    Status,

    /// profile management.
    #[command(subcommand)]
    Profile(ProfileCmd),

    /// routing rules and per-stream decisions.
    #[command(subcommand)]
    Route(RouteCmd),

    /// per-app level control (layer a).
    #[command(subcommand)]
    PerApp(PerAppCmd),

    /// get a setting value from the active profile.
    Get {
        /// dotted setting key.
        key: String,
    },

    /// set a setting override (or clear with `--clear`). overrides persist
    /// across profile switches; `reset` clears all.
    Set {
        /// dotted setting key.
        key: String,
        /// new value, json-encoded. omit with `--clear`.
        // allow_hyphen_values so negatives aren't parsed as flags.
        #[arg(allow_hyphen_values = true)]
        value: Option<String>,
        /// clear this key's override instead of setting one.
        #[arg(long, conflicts_with = "value")]
        clear: bool,
    },

    /// toggle the global bypass kill switch.
    Bypass {
        /// `on` or `off`.
        #[arg(value_enum)]
        state: BypassState,
    },

    /// clear all setting overrides. leaves route/per-app/bypass intact.
    Reset,

    /// reload profile files from disk.
    Reload,

    /// live monitor. defaults to a tui; `--json` is the line-delimited stream.
    Monitor {
        /// topics to subscribe to (comma-separated). `--json` only; the tui
        /// subscribes to all.
        #[arg(value_delimiter = ',', default_value = "meters")]
        topics: Vec<MonitorTopic>,

        /// emit one json event per line instead of the tui.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MonitorTopic {
    Meters,
    Profile,
    Routing,
    Daemon,
}

impl From<MonitorTopic> for Topic {
    fn from(t: MonitorTopic) -> Self {
        match t {
            MonitorTopic::Meters => Topic::Meters,
            MonitorTopic::Profile => Topic::Profile,
            MonitorTopic::Routing => Topic::Routing,
            MonitorTopic::Daemon => Topic::Daemon,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProfileCmd {
    /// list known profiles.
    List,
    /// activate the named profile.
    Use {
        /// profile name.
        name: String,
    },
    /// show a profile in full.
    Show {
        /// profile name (defaults to the active profile).
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum RouteCmd {
    /// list routing rules and current per-stream decisions.
    List,
    /// add or replace a routing rule for an app.
    Set {
        /// app identifier (e.g. `application.process.binary`).
        app: String,
        /// where to route.
        #[arg(value_enum)]
        to: RouteArg,
    },
    /// remove an app's user routing rule.
    Unset {
        /// app identifier.
        app: String,
    },
    /// reroute a specific live stream by node id.
    Stream {
        /// pipewire node id.
        node_id: u32,
        /// where to route.
        #[arg(value_enum)]
        to: RouteArg,
    },
}

#[derive(Debug, Subcommand)]
enum PerAppCmd {
    /// show per-app layer a state for managed streams.
    Status {
        /// emit the snapshot list as json instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// enable the layer a master switch (persisted).
    On,
    /// disable the layer a master switch (persisted).
    Off,
    /// enable or disable layer a for a specific app (persisted).
    Set {
        /// app identifier (process_binary or application_name).
        app: String,
        /// `on` or `off`.
        #[arg(value_enum)]
        state: BypassState,
    },
    /// clear a managed stream's deference lock.
    Reset {
        /// pipewire node id of the managed stream.
        node_id: u32,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BypassState {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RouteArg {
    Processed,
    Bypass,
}

impl From<RouteArg> for Route {
    fn from(r: RouteArg) -> Self {
        match r {
            RouteArg::Processed => Route::Processed,
            RouteArg::Bypass => Route::Bypass,
        }
    }
}

fn init_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("headroom=info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .init();
}

fn run() -> Result<(), CliError> {
    let cli = Cli::parse();

    // tui owns the terminal; keep `tracing` off it.
    let tui_mode = matches!(&cli.cmd, Cmd::Monitor { json: false, .. });
    if !tui_mode {
        init_tracing();
    }

    match cli.cmd {
        Cmd::Daemon { profile_dir } => {
            headroom_core::run_with_profile_dirs(profile_dir)
                .map_err(|e| CliError::Daemon(e.to_string()))?;
            Ok(())
        }
        Cmd::Monitor { json: false, .. } => {
            #[cfg(feature = "tui")]
            {
                // connect on the main thread so initial rpcs run before raw mode.
                let client = match cli.socket.as_deref() {
                    Some(p) => Client::connect_at(p)?,
                    None => Client::connect()?,
                };
                tui::run(client).map_err(CliError::Tui)
            }
            // no `tui` feature: point at the json stream, not an opaque failure.
            #[cfg(not(feature = "tui"))]
            {
                let _ = cli.socket.as_deref();
                Err(CliError::Other(
                    "this build was compiled without the TUI (`tui` feature disabled); \
                     rerun `headroom monitor --json` for the line-delimited event stream"
                        .into(),
                ))
            }
        }
        cmd => with_client(cli.socket.as_deref(), |c| dispatch(c, cmd)),
    }
}

fn with_client<F>(socket: Option<&std::path::Path>, f: F) -> Result<(), CliError>
where
    F: FnOnce(&mut Client) -> Result<(), CliError>,
{
    let mut client = match socket {
        Some(p) => Client::connect_at(p)?,
        None => Client::connect()?,
    };
    f(&mut client)
}

fn dispatch(client: &mut Client, cmd: Cmd) -> Result<(), CliError> {
    match cmd {
        Cmd::Daemon { .. } => unreachable!("handled in `run`"),

        Cmd::Status => {
            let status = client.status()?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }

        Cmd::Profile(ProfileCmd::List) => {
            let profiles = client.profile_list()?;
            for p in profiles {
                let marker = if p.active { '*' } else { ' ' };
                println!("{marker} {:<16} {}", p.name, p.description);
            }
        }
        Cmd::Profile(ProfileCmd::Use { name }) => {
            let active = client.profile_use(&name)?;
            println!("active profile: {active}");
        }
        Cmd::Profile(ProfileCmd::Show { name }) => {
            let body = client.profile_show(name.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&body)?);
        }

        Cmd::Route(RouteCmd::List) => {
            let list = client.route_list()?;
            println!("{}", serde_json::to_string_pretty(&list)?);
        }
        Cmd::Route(RouteCmd::Set { app, to }) => {
            client.route_set(&app, to.into())?;
        }
        Cmd::Route(RouteCmd::Unset { app }) => {
            client.route_unset(&app)?;
        }
        Cmd::Route(RouteCmd::Stream { node_id, to }) => {
            client.route_stream(node_id, to.into())?;
        }

        Cmd::PerApp(PerAppCmd::Status { json }) => {
            let list = client.layer_a_list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("no streams under Layer A management");
            } else {
                println!(
                    "{:<8} {:<24} {:>10} {:>9} {:>9}",
                    "node", "app", "reduction", "ceiling", "deferred"
                );
                for s in &list {
                    let app = if s.app.len() > 24 {
                        format!("{}…", &s.app[..23])
                    } else {
                        s.app.clone()
                    };
                    let ceiling = s
                        .user_ceiling_lin
                        .map(|c| format!("{c:.2}"))
                        .unwrap_or_else(|| "—".to_string());
                    println!(
                        "{:<8} {:<24} {:>8.1}dB {:>9} {:>9}",
                        s.node_id, app, s.reduction_db, ceiling, s.deferred
                    );
                }
            }
        }
        Cmd::PerApp(PerAppCmd::On) => {
            client.per_app_master(true)?;
        }
        Cmd::PerApp(PerAppCmd::Off) => {
            client.per_app_master(false)?;
        }
        Cmd::PerApp(PerAppCmd::Set { app, state }) => {
            client.per_app_set(&app, matches!(state, BypassState::On))?;
        }
        Cmd::PerApp(PerAppCmd::Reset { node_id }) => {
            client.layer_a_reset(node_id)?;
        }

        Cmd::Get { key } => {
            let v = client.setting_get(&key)?;
            println!("{}", serde_json::to_string(&v)?);
        }
        Cmd::Set { key, value, clear } => {
            if clear {
                let existed = client.setting_clear(&key)?;
                if existed {
                    println!("cleared override for '{key}'");
                } else {
                    println!("no override set for '{key}'");
                }
            } else {
                let value = value.ok_or_else(|| {
                    CliError::Other("provide a value, or use --clear to remove the override".into())
                })?;
                let parsed: serde_json::Value = serde_json::from_str(&value)
                    .map_err(|e| CliError::Other(format!("value is not valid JSON: {e}")))?;
                client.setting_set(&key, parsed)?;
            }
        }
        Cmd::Bypass { state } => {
            client.bypass_set(matches!(state, BypassState::On))?;
        }
        Cmd::Reset => {
            let cleared = client.setting_reset()?;
            println!("cleared {cleared} setting override(s)");
        }
        Cmd::Reload => {
            let reloaded = client.profile_reload()?;
            println!("reloaded: {reloaded:?}");
        }
        Cmd::Monitor { topics, json } => {
            if json {
                let pw_topics: Vec<Topic> =
                    topics.iter().copied().map(Topic::from).collect();
                client.subscribe(&pw_topics)?;
                loop {
                    let ev = client.next_event()?;
                    println!(
                        "{} {}/{} {}",
                        chrono_like_now(),
                        ev.topic,
                        ev.event,
                        serde_json::to_string(&ev.data)?,
                    );
                }
            } else {
                unreachable!("TUI monitor is dispatched before `with_client`")
            }
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("client: {0}")]
    Client(#[from] ClientError),

    #[error("daemon: {0}")]
    Daemon(String),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[cfg(feature = "tui")]
    #[error("tui: {0}")]
    Tui(tui::TuiError),

    #[error("{0}")]
    Other(String),
}

/// timestamp label for monitor output; `SystemTime` to avoid chrono.
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", t.as_secs(), t.subsec_millis())
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("headroom: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
