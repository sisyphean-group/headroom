//! daemon bridge: two `Client` connections off the ui thread — a reader
//! thread forwarding subscription events, a control thread running
//! request/response ops fed by a `ControlCmd` channel.
//!
//! initial rpcs run on the calling thread so a dead daemon fails fast
//! before any window opens. both workers survive the daemon going away
//! (shutdown / crash / `systemctl restart`): the reader reconnects with
//! capped backoff and pushes a fresh [`Snapshot`] so the ui re-seeds
//! (node ids change across a restart); the control thread reconnects
//! lazily with a one-shot retry across a dropped connection.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use headroom_client::{Client, ClientError};
use headroom_ipc::{Event, LayerASnapshot, ProfileInfo, Route, RouteList, Status, Topic};

/// topics the monitor subscribes to. shared by the initial connect and
/// every reconnect.
const TOPICS: [Topic; 4] = [Topic::Meters, Topic::Routing, Topic::Profile, Topic::Daemon];

/// reconnect backoff: first retry after the min, doubling to the cap.
const RECONNECT_MIN_DELAY: Duration = Duration::from_millis(500);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(3);

/// a control op the ui asks the control thread to run; each maps to one
/// `Client` request/response call.
pub enum ControlCmd {
    /// `bypass.set`
    SetBypass(bool),
    /// `per-app.master`
    SetPerAppMaster(bool),
    /// `route.stream` — one-shot reroute of a live stream by node id
    /// (doesn't survive the stream being torn down).
    RouteStream { node_id: u32, to: Route },
    /// `route.set` — persistent per-app rule, keyed by app label (matches
    /// `application.name` / `process.binary`). survives the app recreating
    /// its stream between tracks.
    SetRoute { app: String, to: Route },
    /// `per-app.set`
    SetPerApp { app: String, enabled: bool },
    /// `per-app.reset`
    ResetLayerA { node_id: u32 },
    /// `profile.use`
    UseProfile(String),
    /// `setting.reset` — clear all overlay setting overrides.
    ClearOverrides,
    /// `per-app.list` → [`AppMsg::LayerASnapshots`].
    RefreshLayerA,
    /// `profile.list` → [`AppMsg::Profiles`].
    RefreshProfiles,
}

/// the daemon snapshot the ui seeds (and re-seeds) from; taken at connect
/// and again on every reconnect.
pub struct Snapshot {
    pub status: Status,
    pub route_list: RouteList,
    /// seeds the switcher.
    pub profiles: Vec<ProfileInfo>,
}

/// a message from a worker thread to the ui.
pub enum AppMsg {
    /// a subscription event off the wire (reader thread).
    Event(Event),
    /// the event stream ended; carries a reason for the footer banner.
    /// the reader starts reconnecting immediately; [`AppMsg::Reconnected`]
    /// follows once it succeeds.
    Disconnected(String),
    /// the reader reconnected after a drop. carries a fresh snapshot so
    /// the ui re-seeds (node ids/uptime/profile change across a restart).
    /// boxed to keep the enum small.
    Reconnected(Box<Snapshot>),
    /// a control op failed; surfaced in the footer.
    ControlError(String),
    /// fresh `profile.list` snapshot.
    Profiles(Vec<ProfileInfo>),
    /// fresh `per-app.list` snapshot (managed / ceiling / deferred).
    LayerASnapshots(Vec<LayerASnapshot>),
}

/// everything `main` needs to seed the window.
pub struct Bootstrap {
    pub snapshot: Snapshot,
    /// live event + worker channel, drained each tick.
    pub rx: Receiver<AppMsg>,
    /// hands control ops to the control thread.
    pub cmd_tx: Sender<ControlCmd>,
}

/// connect, subscribe, fetch the initial snapshot, open a second control
/// connection, then spawn the reader + control threads.
///
/// errors here are returned to the caller (fail fast before any window).
/// once the threads run, a stream failure is reported in-band as
/// [`AppMsg::Disconnected`] / [`AppMsg::Reconnected`]; control failures
/// as [`AppMsg::ControlError`].
pub fn start(socket: Option<PathBuf>) -> Result<Bootstrap> {
    let mut client = match socket.as_deref() {
        Some(p) => Client::connect_at(p)
            .with_context(|| format!("connecting to headroom daemon at {}", p.display()))?,
        None => Client::connect().context("connecting to headroom daemon (is it running?)")?,
    };

    // subscribe + initial snapshot before the client moves to the reader.
    // a dead/half-broken daemon surfaces here.
    let snapshot = fetch_snapshot(&mut client).context("initial daemon fetch")?;

    // the resolved socket path drives reconnects on both worker threads.
    let socket_path = client.socket_path().to_path_buf();

    // second connection for control, on the same resolved socket.
    let control = Client::connect_at(&socket_path).context("opening control connection")?;

    let (tx, rx) = unbounded::<AppMsg>();
    let (cmd_tx, cmd_rx) = unbounded::<ControlCmd>();

    let reader_tx = tx.clone();
    let reader_socket = socket_path.clone();
    thread::Builder::new()
        .name("headroom-gui-rx".into())
        .spawn(move || reader_loop(reader_socket, client, reader_tx))
        .context("spawning reader thread")?;

    thread::Builder::new()
        .name("headroom-gui-ctl".into())
        .spawn(move || control_loop(socket_path, control, cmd_rx, tx))
        .context("spawning control thread")?;

    Ok(Bootstrap {
        snapshot,
        rx,
        cmd_tx,
    })
}

/// subscribe to [`TOPICS`] and pull `status` + `route.list` +
/// `profile.list`. the `Client` correlates rpc responses while a
/// subscription is active (buffers interleaved events), so doing this on
/// the event connection is fine — one path seeds connect + every reconnect.
fn fetch_snapshot(client: &mut Client) -> Result<Snapshot, ClientError> {
    client.subscribe(&TOPICS)?;
    let status = client.status()?;
    let route_list = client.route_list()?;
    let profiles = client.profile_list()?;
    Ok(Snapshot {
        status,
        route_list,
        profiles,
    })
}

/// forward each event to the ui. on stream failure, announce the drop,
/// reconnect (with backoff), and resume — returning only when the ui
/// channel closes (window gone).
fn reader_loop(socket: PathBuf, mut client: Client, tx: Sender<AppMsg>) {
    loop {
        match client.next_event() {
            Ok(ev) => {
                if tx.send(AppMsg::Event(ev)).is_err() {
                    return; // ui gone.
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "event stream lost; reconnecting to daemon");
                if tx.send(AppMsg::Disconnected(e.to_string())).is_err() {
                    return;
                }
                match reconnect_event_client(&socket, &tx) {
                    Some(c) => {
                        tracing::info!("reconnected to daemon; re-seeded UI snapshot");
                        client = c;
                    }
                    None => return, // ui gone while reconnecting.
                }
            }
        }
    }
}

/// reconnect the event stream after a drop. retries `connect_at` with
/// capped backoff until the daemon answers with a fresh snapshot, then
/// sends [`AppMsg::Reconnected`] and returns the live client. a daemon
/// that's up but can't complete the snapshot (half-open / mid-restart)
/// counts as a failed attempt. returns `None` only if the ui closed.
fn reconnect_event_client(socket: &Path, tx: &Sender<AppMsg>) -> Option<Client> {
    let mut delay = RECONNECT_MIN_DELAY;
    loop {
        if let Ok(mut client) = Client::connect_at(socket) {
            if let Ok(snapshot) = fetch_snapshot(&mut client) {
                if tx.send(AppMsg::Reconnected(Box::new(snapshot))).is_err() {
                    return None;
                }
                return Some(client);
            }
        }
        thread::sleep(delay);
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

/// block on the command channel, dispatching each [`ControlCmd`] to its
/// `Client` call. refresh ops reply with a snapshot `AppMsg`; failures
/// become [`AppMsg::ControlError`]. returns when either channel closes.
///
/// the control connection reconnects lazily: if torn down (daemon
/// restart) the next command opens a fresh one, and a command that fails
/// mid-flight on a connection error is retried once before being reported.
fn control_loop(socket: PathBuf, initial: Client, cmd_rx: Receiver<ControlCmd>, tx: Sender<AppMsg>) {
    let mut client: Option<Client> = Some(initial);
    while let Ok(cmd) = cmd_rx.recv() {
        // reconnect if a prior op tore the connection down.
        if client.is_none() {
            client = Client::connect_at(&socket).ok();
        }
        let Some(c) = client.as_mut() else {
            if tx
                .send(AppMsg::ControlError("daemon unreachable".into()))
                .is_err()
            {
                return;
            }
            continue;
        };

        let mut result = run_cmd(c, &cmd);
        // one retry across a dropped connection — the daemon may have
        // restarted since the last op left the socket stale.
        if matches!(&result, Err(e) if is_connection_error(e)) {
            client = Client::connect_at(&socket).ok();
            if let Some(c) = client.as_mut() {
                result = run_cmd(c, &cmd);
            }
        }

        let msg = match result {
            Ok(Some(reply)) => reply,
            Ok(None) => continue,
            Err(e) => {
                // drop a dead connection so the next command reconnects.
                if is_connection_error(&e) {
                    client = None;
                }
                AppMsg::ControlError(e.to_string())
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
}

/// run one [`ControlCmd`] against `client`. borrows the command so the
/// caller can retry on a fresh connection. `Ok(Some(_))` for refreshes
/// that reply, `Ok(None)` for fire-and-forget toggles.
fn run_cmd(client: &mut Client, cmd: &ControlCmd) -> Result<Option<AppMsg>, ClientError> {
    match cmd {
        ControlCmd::SetBypass(enabled) => client.bypass_set(*enabled).map(|()| None),
        ControlCmd::SetPerAppMaster(enabled) => client.per_app_master(*enabled).map(|()| None),
        ControlCmd::RouteStream { node_id, to } => {
            client.route_stream(*node_id, *to).map(|()| None)
        }
        ControlCmd::SetRoute { app, to } => client.route_set(app, *to).map(|()| None),
        ControlCmd::SetPerApp { app, enabled } => client.per_app_set(app, *enabled).map(|()| None),
        ControlCmd::ResetLayerA { node_id } => client.layer_a_reset(*node_id).map(|()| None),
        ControlCmd::UseProfile(name) => client.profile_use(name).map(|_| None),
        ControlCmd::ClearOverrides => client.setting_reset().map(|_| None),
        ControlCmd::RefreshLayerA => client.layer_a_list().map(|l| Some(AppMsg::LayerASnapshots(l))),
        ControlCmd::RefreshProfiles => client.profile_list().map(|p| Some(AppMsg::Profiles(p))),
    }
}

/// whether an error means the socket is gone (vs a server-side rejection
/// of a delivered op). only the former warrants a reconnect; a `Protocol`
/// error is a logical failure to surface as-is.
fn is_connection_error(e: &ClientError) -> bool {
    matches!(e, ClientError::Ipc(_))
}
