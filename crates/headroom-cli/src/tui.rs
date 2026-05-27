//! `headroom monitor` tui — bus gauges, loudness, per-stream routing.
//!
//! main thread owns terminal + draw loop; reader thread owns the `Client`,
//! forwards events over a channel. no graceful reader shutdown — process
//! exit tears the socket down.

use std::collections::BTreeMap;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{select, tick, unbounded, Receiver};
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use headroom_client::{Client, ClientError};
use headroom_ipc::{
    DaemonEvent, Event, LayerALevel, LayerASnapshot, MeterTick, ProfileEvent, Route, RoutingEvent,
    Status, StreamRoute, Topic,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, Wrap},
    Frame, Terminal,
};

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("client: {0}")]
    Client(#[from] ClientError),

    #[error("terminal: {0}")]
    Io(#[from] io::Error),
}

/// runs initial rpcs, then hands the client to the reader thread and
/// enters the draw loop.
pub fn run(mut client: Client) -> Result<(), TuiError> {
    // subscribe + initial state before raw mode.
    let topics = [Topic::Meters, Topic::Routing, Topic::Profile, Topic::Daemon];
    client.subscribe(&topics)?;
    let status = client.status()?;
    let route_list = client.route_list()?;

    // client is single-connection + the reader owns it for events; open a
    // second connection for control (keypress ops).
    let mut control = Client::connect_at(client.socket_path())?;

    let (tx, rx) = unbounded::<Msg>();
    let reader_handle = thread::Builder::new()
        .name("headroom-monitor-rx".into())
        .spawn(move || reader_loop(client, tx))
        .map_err(TuiError::Io)?;

    let mut terminal = ratatui::init();
    let outcome = draw_loop(&mut terminal, status, route_list, rx, &mut control);
    ratatui::restore();

    // detach: process exit / dropped channel tears the connection down.
    drop(reader_handle);

    outcome
}

// ---------------------------------------------------------------------------
// Reader thread
// ---------------------------------------------------------------------------

enum Msg {
    Event(Event),
    Disconnected(String),
}

fn reader_loop(mut client: Client, tx: crossbeam_channel::Sender<Msg>) {
    loop {
        match client.next_event() {
            Ok(ev) => {
                if tx.send(Msg::Event(ev)).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(Msg::Disconnected(e.to_string()));
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct UiState {
    daemon_version: String,
    profile: String,
    bypass: bool,
    /// layer a master switch (per-app level control enabled globally).
    per_app_master: bool,
    /// overlay overrides shadowing the profile; flagged so a stale
    /// `headroom set` isn't invisible.
    setting_overrides: BTreeMap<String, serde_json::Value>,
    /// daemon uptime at connect, plus local elapsed.
    base_uptime_s: u64,
    connected_at: Instant,
    default_route: Route,
    streams: BTreeMap<u32, StreamRoute>,
    /// per-stream layer a. presence = tap attached; inner = latest smoothed
    /// reduction (none until the first `layer_a_level`).
    layer_a: BTreeMap<u32, Option<f32>>,
    /// richer layer a snapshots (ceiling/deferred/managed), polled from
    /// `per-app.list`; feeds the detail line.
    la_snapshots: BTreeMap<u32, LayerASnapshot>,
    /// selected stream node id; resolved against `streams` at draw time.
    selected: Option<u32>,
    meters: Option<MeterTick>,
    /// last tick arrival; drives staleness when the source goes silent.
    last_meter_at: Option<Instant>,
    overflow_total: u64,
    last_error: Option<String>,
    disconnected: Option<String>,
    /// modal profile picker; while `Some` it intercepts all keyboard input.
    picker: Option<ProfilePicker>,
}

/// modal profile-switcher; `selected` indexes `profiles`.
struct ProfilePicker {
    profiles: Vec<headroom_ipc::ProfileInfo>,
    selected: usize,
}

impl ProfilePicker {
    /// move the highlight by `delta` (negative = up), wrapping.
    fn move_sel(&mut self, delta: isize) {
        if self.profiles.is_empty() {
            return;
        }
        let n = self.profiles.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
    }
}

/// initial highlight: active profile, else the daemon's current name, else top.
fn initial_picker_selection(profiles: &[headroom_ipc::ProfileInfo], current: &str) -> usize {
    profiles
        .iter()
        .position(|p| p.active)
        .or_else(|| profiles.iter().position(|p| p.name == current))
        .unwrap_or(0)
}

impl UiState {
    fn new(status: Status, route_list: headroom_ipc::RouteList) -> Self {
        let mut streams = BTreeMap::new();
        for s in route_list.current {
            streams.insert(s.node_id, s);
        }
        // `status.streams` is a superset; merge.
        for s in status.streams.iter() {
            streams.entry(s.node_id).or_insert_with(|| s.clone());
        }
        // seed layer a from status so table/detail are populated pre-poll.
        let mut la_snapshots = BTreeMap::new();
        let mut layer_a = BTreeMap::new();
        for snap in status.layer_a {
            layer_a.insert(snap.node_id, Some(snap.reduction_db));
            la_snapshots.insert(snap.node_id, snap);
        }
        let selected = streams.keys().next().copied();
        Self {
            daemon_version: status.version,
            profile: status.profile,
            bypass: status.bypass,
            per_app_master: status.per_app,
            setting_overrides: status.setting_overrides,
            base_uptime_s: status.uptime_s,
            connected_at: Instant::now(),
            default_route: route_list.default_route,
            streams,
            layer_a,
            la_snapshots,
            selected,
            meters: None,
            last_meter_at: None,
            overflow_total: 0,
            last_error: None,
            disconnected: None,
            picker: None,
        }
    }

    fn uptime_s(&self) -> u64 {
        self.base_uptime_s
            .saturating_add(self.connected_at.elapsed().as_secs())
    }

    /// stream node ids in row order (`BTreeMap` keys).
    fn ordered_nodes(&self) -> Vec<u32> {
        self.streams.keys().copied().collect()
    }

    /// `selected` when still live, else the first row, else `None`.
    fn effective_selection(&self) -> Option<u32> {
        match self.selected {
            Some(id) if self.streams.contains_key(&id) => Some(id),
            _ => self.streams.keys().next().copied(),
        }
    }

    /// move the selection by `delta` rows (negative = up), wrapping.
    fn move_selection(&mut self, delta: isize) {
        let nodes = self.ordered_nodes();
        if nodes.is_empty() {
            self.selected = None;
            return;
        }
        let cur = self
            .effective_selection()
            .and_then(|id| nodes.iter().position(|&n| n == id))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(nodes.len() as isize) as usize;
        self.selected = Some(nodes[next]);
    }

    fn apply_event(&mut self, ev: Event) {
        match ev.topic {
            Topic::Meters if ev.event == "tick" => {
                if let Ok(m) = serde_json::from_value::<MeterTick>(ev.data) {
                    self.meters = Some(m);
                    self.last_meter_at = Some(Instant::now());
                }
            }
            Topic::Meters if ev.event == "layer_a_level" => {
                if let Ok(l) = serde_json::from_value::<LayerALevel>(ev.data) {
                    self.layer_a.insert(l.node_id, Some(l.reduction_db));
                }
            }
            Topic::Routing => {
                if let Ok(re) = serde_json::from_value::<RoutingEvent>(routing_payload(&ev)) {
                    match re {
                        RoutingEvent::StreamRouted { node_id, app, to } => {
                            self.streams.insert(
                                node_id,
                                StreamRoute {
                                    node_id,
                                    app,
                                    route: to,
                                },
                            );
                        }
                        RoutingEvent::StreamRemoved { node_id } => {
                            self.streams.remove(&node_id);
                            self.layer_a.remove(&node_id);
                            self.la_snapshots.remove(&node_id);
                        }
                        RoutingEvent::LayerAAttached { node_id, .. } => {
                            // reduction unknown until the first `layer_a_level`.
                            self.layer_a.entry(node_id).or_insert(None);
                        }
                        RoutingEvent::LayerADetached { node_id } => {
                            self.layer_a.remove(&node_id);
                            self.la_snapshots.remove(&node_id);
                        }
                        RoutingEvent::RuleChanged => { /* tui doesn't show rules */ }
                        _ => {}
                    }
                }
            }
            Topic::Profile => {
                if let Ok(ProfileEvent::Changed { name, .. }) =
                    serde_json::from_value::<ProfileEvent>(profile_payload(&ev))
                {
                    self.profile = name;
                }
            }
            Topic::Daemon => {
                if let Ok(de) = serde_json::from_value::<DaemonEvent>(daemon_payload(&ev)) {
                    match de {
                        DaemonEvent::Overflow {
                            lost, total_lost, ..
                        } => {
                            self.overflow_total = total_lost.max(self.overflow_total + lost as u64);
                        }
                        DaemonEvent::Error { code, message } => {
                            self.last_error = Some(format!("{code}: {message}"));
                        }
                        DaemonEvent::Shutdown => {
                            self.disconnected = Some("daemon shutdown".into());
                        }
                        DaemonEvent::Started { version } => {
                            self.daemon_version = version;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// typed event enums are `#[serde(tag = "event")]` but the wire frame keeps
/// `event` outside `data`, so re-inject it before deserializing.
fn routing_payload(ev: &Event) -> serde_json::Value {
    inject_event(&ev.event, &ev.data)
}
fn profile_payload(ev: &Event) -> serde_json::Value {
    inject_event(&ev.event, &ev.data)
}
fn daemon_payload(ev: &Event) -> serde_json::Value {
    inject_event(&ev.event, &ev.data)
}

fn inject_event(event: &str, data: &serde_json::Value) -> serde_json::Value {
    let mut obj = match data {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.insert("event".into(), serde_json::Value::String(event.to_string()));
    serde_json::Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Draw loop
// ---------------------------------------------------------------------------

fn draw_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    status: Status,
    route_list: headroom_ipc::RouteList,
    rx: Receiver<Msg>,
    control: &mut Client,
) -> Result<(), TuiError> {
    let mut state = UiState::new(status, route_list);
    // 10 Hz redraw floor so uptime/staleness tick with no events flowing.
    let ticker = tick(Duration::from_millis(100));
    let input_rx = spawn_input_thread();
    // ~1 Hz poll of the richer layer a snapshot.
    let mut poll_ticks: u32 = 0;

    loop {
        terminal.draw(|f| draw(f, &state))?;

        select! {
            recv(rx) -> msg => match msg {
                Ok(Msg::Event(ev)) => state.apply_event(ev),
                Ok(Msg::Disconnected(reason)) => {
                    state.disconnected = Some(reason);
                    // final paint, then linger so the banner is seen.
                    terminal.draw(|f| draw(f, &state))?;
                    thread::sleep(Duration::from_millis(800));
                    return Ok(());
                }
                Err(_) => return Ok(()),
            },
            recv(input_rx) -> msg => match msg {
                Ok(InputMsg::Key(k)) => {
                    if handle_key(&mut state, control, k) {
                        return Ok(());
                    }
                }
                Ok(InputMsg::Redraw) => {}
                Err(_) => return Ok(()),
            },
            recv(ticker) -> _ => {
                poll_ticks = poll_ticks.wrapping_add(1);
                if poll_ticks % 10 == 0 {
                    poll_layer_a(&mut state, control);
                }
            }
        }
    }
}

/// refresh layer a snapshots from the control connection; errors go to the
/// footer (non-fatal — the event stream keeps the ui live).
fn poll_layer_a(state: &mut UiState, control: &mut Client) {
    match control.layer_a_list() {
        Ok(list) => {
            state.la_snapshots = list.into_iter().map(|s| (s.node_id, s)).collect();
        }
        Err(e) => {
            state.last_error = Some(format!("layer-a poll: {e}"));
        }
    }
}

/// apply a keypress: nav + toggles + per-row actions. control ops run
/// synchronously on the control connection; failures land in the footer.
/// returns `true` to quit.
fn handle_key(state: &mut UiState, control: &mut Client, k: KeyEvent) -> bool {
    // ctrl-c: hard exit, even with the picker open.
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return true;
    }
    // picker is modal: while open it swallows every other key.
    if state.picker.is_some() {
        handle_picker_key(state, control, k);
        return false;
    }
    if is_quit(&k) {
        return true;
    }
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => state.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => state.move_selection(-1),
        KeyCode::Char('P') => open_profile_picker(state, control),
        KeyCode::Char('b') => {
            let target = !state.bypass;
            match control.bypass_set(target) {
                Ok(()) => state.bypass = target,
                Err(e) => state.last_error = Some(format!("bypass: {e}")),
            }
        }
        KeyCode::Char('p') => {
            let target = !state.per_app_master;
            match control.per_app_master(target) {
                Ok(()) => state.per_app_master = target,
                Err(e) => state.last_error = Some(format!("per-app master: {e}")),
            }
        }
        KeyCode::Char('r') | KeyCode::Enter => {
            let Some(node) = state.effective_selection() else {
                return false;
            };
            let Some(stream) = state.streams.get(&node) else {
                return false;
            };
            let app = stream.app.clone();
            let to = match stream.route {
                Route::Processed => Route::Bypass,
                Route::Bypass => Route::Processed,
            };
            // prefer a persistent per-app rule (keyed by app label) so the
            // route survives the app recreating its stream between tracks;
            // fall back to a one-shot per-stream reroute when no label.
            let result = if app.is_empty() {
                control.route_stream(node, to)
            } else {
                control.route_set(&app, to)
            };
            match result {
                Ok(()) => {
                    if let Some(s) = state.streams.get_mut(&node) {
                        s.route = to;
                    }
                }
                Err(e) => state.last_error = Some(format!("route: {e}")),
            }
        }
        KeyCode::Char('a') => {
            let Some(node) = state.effective_selection() else {
                return false;
            };
            let Some(app) = state.streams.get(&node).map(|s| s.app.clone()) else {
                return false;
            };
            if app.is_empty() {
                state.last_error = Some("per-app: selected stream has no app label".into());
                return false;
            }
            let managed = state.la_snapshots.get(&node).is_some_and(|s| s.managed);
            if let Err(e) = control.per_app_set(&app, !managed) {
                state.last_error = Some(format!("per-app set: {e}"));
            }
        }
        KeyCode::Char('x') => {
            let Some(node) = state.effective_selection() else {
                return false;
            };
            if let Err(e) = control.layer_a_reset(node) {
                state.last_error = Some(format!("reset: {e}"));
            }
        }
        KeyCode::Char('c') => {
            // clear all overlay overrides; no-op when none.
            if state.setting_overrides.is_empty() {
                return false;
            }
            match control.setting_reset() {
                Ok(_) => state.setting_overrides.clear(),
                Err(e) => state.last_error = Some(format!("clear overrides: {e}")),
            }
        }
        _ => {}
    }
    false
}

/// open the modal profile picker from `profile.list`; errors → footer.
fn open_profile_picker(state: &mut UiState, control: &mut Client) {
    match control.profile_list() {
        Ok(profiles) if profiles.is_empty() => {
            state.last_error = Some("profile list is empty".into());
        }
        Ok(profiles) => {
            let selected = initial_picker_selection(&profiles, &state.profile);
            state.picker = Some(ProfilePicker { profiles, selected });
        }
        Err(e) => state.last_error = Some(format!("profile list: {e}")),
    }
}

/// handle a keypress while the profile picker is open.
fn handle_picker_key(state: &mut UiState, control: &mut Client, k: KeyEvent) {
    let Some(picker) = state.picker.as_mut() else {
        return;
    };
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => picker.move_sel(1),
        KeyCode::Char('k') | KeyCode::Up => picker.move_sel(-1),
        KeyCode::Esc | KeyCode::Char('q') => state.picker = None,
        KeyCode::Enter => {
            let Some(name) = picker.profiles.get(picker.selected).map(|p| p.name.clone()) else {
                state.picker = None;
                return;
            };
            match control.profile_use(&name) {
                Ok(applied) => {
                    // set eagerly; the `profile.changed` event also lands.
                    state.profile = applied;
                    state.picker = None;
                }
                Err(e) => {
                    // keep the picker open to retry/cancel.
                    state.last_error = Some(format!("profile use: {e}"));
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Input thread
// ---------------------------------------------------------------------------

enum InputMsg {
    Key(KeyEvent),
    Redraw,
}

fn spawn_input_thread() -> Receiver<InputMsg> {
    let (tx, rx) = unbounded::<InputMsg>();
    thread::Builder::new()
        .name("headroom-monitor-input".into())
        .spawn(move || loop {
            let Ok(ev) = event::read() else { return };
            // forward keys to the draw loop; quit/modal decisions need ui state.
            let msg = match ev {
                CtEvent::Key(k) => InputMsg::Key(k),
                CtEvent::Resize(_, _) => InputMsg::Redraw,
                _ => continue,
            };
            if tx.send(msg).is_err() {
                return;
            }
        })
        .expect("spawn input thread");
    rx
}

fn is_quit(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
        || (k.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C')))
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, state: &UiState) {
    let area = f.area();
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " headroom monitor ",
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .title_top(Line::from(header_status(state)).right_aligned())
        .title_bottom(Line::from(footer_text(state)).right_aligned());
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // bus gauges
            Constraint::Length(5), // loudness
            Constraint::Min(4),    // streams table
            Constraint::Length(3), // layer A detail (selected stream)
        ])
        .split(inner);

    draw_bus(f, chunks[0], state);
    draw_loudness(f, chunks[1], state);
    draw_streams(f, chunks[2], state);
    draw_layer_a_detail(f, chunks[3], state);

    // modal overlay, drawn last so it sits on top.
    if let Some(picker) = &state.picker {
        draw_profile_picker(f, area, picker);
    }
}

/// centered rect, at most `width`×`height`, clamped to `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// modal profile switcher: centered list, active marked (●), highlighted
/// reversed, plus a description line.
fn draw_profile_picker(f: &mut Frame, area: Rect, picker: &ProfilePicker) {
    let rows = picker.profiles.len() as u16;
    // rows + 2 borders + 2 description lines.
    let rect = centered_rect(60, rows + 4, area);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" switch profile ")
        .title_bottom(Line::from(" j/k move · Enter apply · Esc cancel ").right_aligned());
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);

    let lines: Vec<Line> = picker
        .profiles
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let marker = if p.active { "●" } else { " " };
            let style = if i == picker.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if p.active {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::raw(format!(" {marker} ")),
                Span::styled(p.name.clone(), style),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), parts[0]);

    let desc = picker
        .profiles
        .get(picker.selected)
        .map(|p| p.description.clone())
        .unwrap_or_default();
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {desc}"),
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: true }),
        parts[1],
    );
}

fn header_status(state: &UiState) -> Vec<Span<'static>> {
    let bypass_span = if state.bypass {
        Span::styled(
            " BYPASS ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else {
        Span::styled(" processed ", Style::default().fg(Color::Green))
    };
    let per_app_span = if state.per_app_master {
        Span::styled(" per-app ", Style::default().fg(Color::Cyan))
    } else {
        Span::styled(" per-app off ", Style::default().fg(Color::DarkGray))
    };
    vec![
        Span::raw(" profile: "),
        Span::styled(state.profile.clone(), Style::default().bold()),
        Span::raw("  "),
        bypass_span,
        Span::raw(" "),
        per_app_span,
        Span::raw(format!(
            "  v{}  uptime {}  ",
            state.daemon_version,
            fmt_uptime(state.uptime_s())
        )),
    ]
}

fn footer_text(state: &UiState) -> Vec<Span<'static>> {
    let sep = || Span::styled("·", Style::default().fg(Color::DarkGray));
    let mut parts: Vec<Span> = vec![
        Span::raw(" j/k select "),
        sep(),
        Span::raw(" r route "),
        sep(),
        Span::raw(" a per-app "),
        sep(),
        Span::raw(" x reset "),
        sep(),
        Span::raw(" b bypass "),
        sep(),
        Span::raw(" p per-app "),
        sep(),
        Span::raw(" P profile "),
        sep(),
        Span::raw(" q quit "),
    ];
    if state.overflow_total > 0 {
        parts.push(Span::styled("·", Style::default().fg(Color::DarkGray)));
        parts.push(Span::styled(
            format!(" dropped: {} ", state.overflow_total),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(err) = &state.last_error {
        parts.push(Span::styled("·", Style::default().fg(Color::DarkGray)));
        parts.push(Span::styled(
            format!(" daemon error: {err} "),
            Style::default().fg(Color::Red),
        ));
    }
    if let Some(reason) = &state.disconnected {
        parts.push(Span::styled("·", Style::default().fg(Color::DarkGray)));
        parts.push(Span::styled(
            format!(" disconnected: {reason} "),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if !state.setting_overrides.is_empty() {
        parts.push(Span::styled("·", Style::default().fg(Color::DarkGray)));
        parts.push(Span::styled(
            format!(
                " overrides: {} (c clear) ",
                fmt_overrides(&state.setting_overrides)
            ),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    parts
}

/// active overrides as `key=value, …` for the footer (strings unquoted,
/// else json).
fn fmt_overrides(map: &BTreeMap<String, serde_json::Value>) -> String {
    map.iter()
        .map(|(k, v)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{k}={val}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn draw_bus(f: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" bus dsp ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let m = state.meters;
    draw_gauge_row(
        f,
        rows[0],
        GaugeRow {
            label: "AGC target",
            value: m.map(|t| t.agc_gain_db),
            min: -12.0,
            max: 12.0,
            unit: "dB",
            color: Color::Cyan,
            enabled: m.map(|t| t.agc_enabled).unwrap_or(true),
        },
    );
    draw_gauge_row(
        f,
        rows[1],
        GaugeRow {
            label: "Compressor GR",
            value: m.map(|t| t.compressor_gr_db),
            min: -24.0,
            max: 0.0,
            unit: "dB",
            color: Color::Magenta,
            enabled: m.map(|t| t.compressor_enabled).unwrap_or(true),
        },
    );
    draw_gauge_row(
        f,
        rows[2],
        GaugeRow {
            label: "Limiter GR",
            value: m.map(|t| t.limiter_gr_db),
            min: -24.0,
            max: 0.0,
            unit: "dB",
            color: Color::Red,
            enabled: true, // always-on safety backstop
        },
    );
    draw_gauge_row(
        f,
        rows[3],
        GaugeRow {
            label: "True peak",
            value: m.map(|t| t.true_peak_dbtp),
            min: -60.0,
            max: 3.0,
            unit: "dBTP",
            color: Color::Green,
            enabled: true, // a measurement, always meaningful
        },
    );
}

struct GaugeRow<'a> {
    label: &'a str,
    value: Option<f32>,
    min: f32,
    max: f32,
    unit: &'a str,
    color: Color,
    /// stage enabled in the active profile; when false the row renders
    /// "disabled" + a greyed bar so a bypassed stage isn't read as 0.
    enabled: bool,
}

/// one gauge row: `LABEL   VALUE   [████░░░░] min..max`.
fn draw_gauge_row(f: &mut Frame, area: Rect, row: GaugeRow<'_>) {
    let GaugeRow {
        label,
        value,
        min,
        max,
        unit,
        color,
        enabled,
    } = row;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(16),
            Constraint::Length(14),
            Constraint::Min(8),
            Constraint::Length(14),
        ])
        .split(area);

    // disabled stage: grey the row + say so, so it can't read as 0.
    if !enabled {
        let dim = Style::default().fg(Color::DarkGray);
        f.render_widget(Paragraph::new(format!(" {label}")).style(dim), cols[0]);
        f.render_widget(
            Paragraph::new("disabled").alignment(Alignment::Right).style(dim),
            cols[1],
        );
        f.render_widget(
            Gauge::default()
                .gauge_style(dim)
                .ratio(0.0)
                .label("off"),
            cols[2],
        );
        f.render_widget(
            Paragraph::new(format!("{min:.0}..{max:.0} "))
                .alignment(Alignment::Right)
                .style(dim),
            cols[3],
        );
        return;
    }

    f.render_widget(Paragraph::new(format!(" {label}")), cols[0]);

    let value_str = value
        .map(|v| format!("{v:+7.2} {unit}"))
        .unwrap_or_else(|| "    -- ".to_string());
    f.render_widget(
        Paragraph::new(value_str).alignment(Alignment::Right),
        cols[1],
    );

    let pct = match value {
        Some(v) => {
            let clamped = v.clamp(min, max);
            ((clamped - min) / (max - min)).clamp(0.0, 1.0) as f64
        }
        None => 0.0,
    };
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(color))
        .ratio(pct)
        .label("");
    f.render_widget(gauge, cols[2]);

    f.render_widget(
        Paragraph::new(format!("{min:.0}..{max:.0} ")).alignment(Alignment::Right),
        cols[3],
    );
}

fn draw_loudness(f: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" loudness (BS.1770) ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let staleness = state
        .last_meter_at
        .map(|t| t.elapsed())
        .unwrap_or(Duration::ZERO);
    let stale = staleness > Duration::from_millis(500);

    let (mom, st, intg) = match state.meters {
        Some(m) => (Some(m.momentary_lufs), Some(m.shortterm_lufs), Some(m.integrated_lufs)),
        None => (None, None, None),
    };

    let lines = vec![
        lufs_line("Momentary  (400 ms)", mom, stale),
        lufs_line("Short-term (3 s)", st, stale),
        lufs_line("Integrated (gated)", intg, stale),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn lufs_line(label: &str, v: Option<f32>, stale: bool) -> Line<'static> {
    let val = match v {
        Some(x) if x > headroom_core::agc::LOUDNESS_FLOOR_LUFS + 0.5 => {
            format!("{x:+7.2} LUFS")
        }
        Some(_) => "    -- LUFS".to_string(),
        None => "    -- LUFS".to_string(),
    };
    let style = if stale {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::raw(format!(" {label:<24}")),
        Span::styled(val, style),
    ])
}

fn draw_streams(f: &mut Frame, area: Rect, state: &UiState) {
    let title = format!(
        " streams ({}) — default: {} ",
        state.streams.len(),
        state.default_route
    );
    let block = Block::default().borders(Borders::ALL).title(title);

    let header = Row::new(vec!["", "node", "app", "route", "per-app"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let selected = state.effective_selection();
    let rows: Vec<Row> = state
        .streams
        .values()
        .map(|s| {
            let is_sel = selected == Some(s.node_id);
            let route_cell = match s.route {
                Route::Processed => Cell::from("processed").style(Style::default().fg(Color::Green)),
                Route::Bypass => Cell::from("bypass").style(Style::default().fg(Color::Yellow)),
            };
            let la_cell = match state.layer_a.get(&s.node_id) {
                Some(Some(db)) => Cell::from(format!("{db:+5.1} dB"))
                    .style(Style::default().fg(Color::Magenta)),
                Some(None) => Cell::from("attached")
                    .style(Style::default().fg(Color::DarkGray)),
                None => Cell::from("—").style(Style::default().fg(Color::DarkGray)),
            };
            let marker = if is_sel { "▶" } else { " " };
            let row = Row::new(vec![
                Cell::from(marker),
                Cell::from(s.node_id.to_string()),
                Cell::from(s.app.clone()),
                route_cell,
                la_cell,
            ]);
            if is_sel {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Length(8),
        Constraint::Min(20),
        Constraint::Length(12),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

/// read-only layer a detail for the selected stream: managed flag,
/// smoothed reduction, user ceiling, deference lock.
fn draw_layer_a_detail(f: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" per-app level (selected) ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let line = match state.effective_selection() {
        None => Line::from(Span::styled(
            " no stream selected",
            Style::default().fg(Color::DarkGray),
        )),
        Some(node) => {
            let app = state
                .streams
                .get(&node)
                .map(|s| s.app.clone())
                .unwrap_or_default();
            match state.la_snapshots.get(&node) {
                Some(snap) => {
                    let ceiling = snap
                        .user_ceiling_lin
                        .map(|c| format!("{c:.2}"))
                        .unwrap_or_else(|| "—".to_string());
                    let deferred = if snap.deferred {
                        Span::styled("deferred", Style::default().fg(Color::Yellow))
                    } else {
                        Span::styled("active", Style::default().fg(Color::Green))
                    };
                    Line::from(vec![
                        Span::raw(format!(" node {node}  {app}  ")),
                        Span::styled(
                            if snap.managed { "managed" } else { "unmanaged" },
                            Style::default().fg(if snap.managed {
                                Color::Cyan
                            } else {
                                Color::DarkGray
                            }),
                        ),
                        Span::raw(format!(
                            "  reduction {:+.1} dB  ceiling {ceiling}  ",
                            snap.reduction_db
                        )),
                        deferred,
                    ])
                }
                None => Line::from(Span::styled(
                    format!(" node {node}  {app}  not managed per-app"),
                    Style::default().fg(Color::DarkGray),
                )),
            }
        }
    };
    f.render_widget(Paragraph::new(line), inner);
}

fn fmt_uptime(s: u64) -> String {
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h{m:02}m{sec:02}s")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_ipc::{Sinks, Status};

    fn empty_state() -> UiState {
        let status = Status {
            version: "test".into(),
            protocol: 1,
            uptime_s: 0,
            profile: "default".into(),
            bypass: false,
            per_app: false,
            sinks: Sinks::default(),
            streams: vec![],
            layer_a: vec![],
            warnings: vec![],
            setting_overrides: Default::default(),
        };
        let route_list = headroom_ipc::RouteList {
            rules: vec![],
            current: vec![],
            default_route: Route::Processed,
        };
        UiState::new(status, route_list)
    }

    #[test]
    fn meter_tick_event_updates_state() {
        let mut state = empty_state();
        let tick = MeterTick {
            momentary_lufs: -19.3,
            shortterm_lufs: -20.1,
            integrated_lufs: -19.8,
            true_peak_dbtp: -1.4,
            gain_reduction_db: -2.1,
            compressor_gr_db: -0.8,
            limiter_gr_db: -1.3,
            agc_gain_db: 0.5,
            agc_enabled: true,
            compressor_enabled: true,
        };
        let ev = Event::new(Topic::Meters, "tick", &tick).unwrap();
        state.apply_event(ev);
        let got = state.meters.expect("meters set");
        assert!((got.momentary_lufs - tick.momentary_lufs).abs() < f32::EPSILON);
        assert!((got.true_peak_dbtp - tick.true_peak_dbtp).abs() < f32::EPSILON);
        assert!(state.last_meter_at.is_some());
    }

    #[test]
    fn stream_removed_prunes_state() {
        let mut state = empty_state();
        // insert via stream_routed first.
        state.apply_event(
            Event::new(
                Topic::Routing,
                "stream_routed",
                &serde_json::json!({ "node_id": 7, "app": "x", "to": "processed" }),
            )
            .unwrap(),
        );
        state.apply_event(
            Event::new(
                Topic::Routing,
                "layer_a_attached",
                &serde_json::json!({ "node_id": 7, "app": "x" }),
            )
            .unwrap(),
        );
        assert!(state.streams.contains_key(&7));
        assert!(state.layer_a.contains_key(&7));

        state.apply_event(
            Event::new(
                Topic::Routing,
                "stream_removed",
                &serde_json::json!({ "node_id": 7 }),
            )
            .unwrap(),
        );
        assert!(!state.streams.contains_key(&7));
        assert!(!state.layer_a.contains_key(&7));
    }

    #[test]
    fn layer_a_level_updates_reduction() {
        let mut state = empty_state();
        state.apply_event(
            Event::new(
                Topic::Routing,
                "layer_a_attached",
                &serde_json::json!({ "node_id": 11, "app": "loud-app" }),
            )
            .unwrap(),
        );
        assert_eq!(state.layer_a.get(&11), Some(&None));

        state.apply_event(
            Event::new(
                Topic::Meters,
                "layer_a_level",
                &serde_json::json!({
                    "node_id": 11,
                    "app": "loud-app",
                    "volume_lin": 0.256_f32,
                    "reduction_db": -11.8_f32,
                }),
            )
            .unwrap(),
        );
        let r = state.layer_a.get(&11).copied().flatten().unwrap();
        assert!((r - -11.8).abs() < 1e-4);
    }

    #[test]
    fn routing_event_inserts_stream() {
        let mut state = empty_state();
        let ev = Event::new(
            Topic::Routing,
            "stream_routed",
            &serde_json::json!({
                "node_id": 42,
                "app": "firefox",
                "to": "bypass",
            }),
        )
        .unwrap();
        state.apply_event(ev);
        let s = state.streams.get(&42).expect("stream tracked");
        assert_eq!(s.app, "firefox");
        assert_eq!(s.route, Route::Bypass);
    }

    #[test]
    fn profile_changed_updates_active() {
        let mut state = empty_state();
        let ev = Event::new(
            Topic::Profile,
            "changed",
            &serde_json::json!({
                "name": "night",
                "previous": "default",
            }),
        )
        .unwrap();
        state.apply_event(ev);
        assert_eq!(state.profile, "night");
    }

    #[test]
    fn daemon_overflow_accumulates() {
        let mut state = empty_state();
        let ev = Event::new(
            Topic::Daemon,
            "overflow",
            &serde_json::json!({
                "lost_topic": "meters",
                "lost": 3u32,
                "total_lost": 5u64,
            }),
        )
        .unwrap();
        state.apply_event(ev);
        assert_eq!(state.overflow_total, 5);
    }

    fn pinfo(name: &str, active: bool) -> headroom_ipc::ProfileInfo {
        headroom_ipc::ProfileInfo {
            name: name.into(),
            active,
            description: format!("{name} desc"),
        }
    }

    #[test]
    fn picker_initial_selection_prefers_active() {
        let profiles = vec![
            pinfo("default", false),
            pinfo("night", true),
            pinfo("speech", false),
        ];
        assert_eq!(initial_picker_selection(&profiles, "default"), 1);
    }

    #[test]
    fn picker_initial_selection_falls_back_to_current_name_then_top() {
        let profiles = vec![pinfo("default", false), pinfo("night", false)];
        // no active flag: match the daemon's reported current name.
        assert_eq!(initial_picker_selection(&profiles, "night"), 1);
        // neither active nor a name match: default to the top.
        assert_eq!(initial_picker_selection(&profiles, "ghost"), 0);
    }

    #[test]
    fn picker_move_sel_wraps_both_ways() {
        let mut p = ProfilePicker {
            profiles: vec![pinfo("a", false), pinfo("b", false), pinfo("c", false)],
            selected: 0,
        };
        p.move_sel(-1);
        assert_eq!(p.selected, 2); // wrap up from top
        p.move_sel(1);
        assert_eq!(p.selected, 0); // wrap down from bottom
        p.move_sel(2);
        assert_eq!(p.selected, 2);
    }

    #[test]
    fn fmt_uptime_buckets() {
        assert_eq!(fmt_uptime(5), "5s");
        assert_eq!(fmt_uptime(75), "1m15s");
        assert_eq!(fmt_uptime(3725), "1h02m05s");
    }
}
