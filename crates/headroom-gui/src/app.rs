//! `HeadroomApp` — iced application state + view; mirrors the tui's
//! read-only `UiState` (`crates/headroom-cli/src/tui.rs`).
//!
//! control ops never block the ui thread: sent over `cmd_tx` to the
//! control thread (`io.rs`), which replies over the same `rx` the ui
//! drains each tick. ui updates optimistically; daemon events + the
//! ~1 Hz `per-app.list` refresh reconcile.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use iced::alignment::Horizontal;
use iced::widget::{button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, text};
use iced::{keyboard, time, Alignment, Background, Border, Color, Element, Length, Subscription};

use headroom_ipc::{
    DaemonEvent, Event, LayerALevel, LayerASnapshot, MeterTick, ProfileInfo, Route, RoutingEvent,
    StreamRoute, Topic,
};

use crate::io::{AppMsg, Bootstrap, ControlCmd, Snapshot};

/// loudness at/below this floor renders as "--". mirrors
/// `headroom_core::agc::LOUDNESS_FLOOR_LUFS`, inlined so the gui doesn't
/// pull in the audio/dsp crate.
const LOUDNESS_FLOOR_LUFS: f32 = -200.0;

/// meters older than this are dimmed (source went silent / suspended).
const STALE_AFTER: Duration = Duration::from_millis(500);

/// ui drain + repaint interval (~20 Hz); also advances uptime/staleness
/// text with no events flowing.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

// palette — explicit colors mirroring the tui's intent.
const BG: u32 = 0x1e1e1e;
const PANEL_BG: u32 = 0x252526;
const TRACK_BG: u32 = 0x3a3a3a;
const FG: u32 = 0xd4d4d4;
const DIM: u32 = 0x808080;
const CYAN: u32 = 0x4ec9b0;
const MAGENTA: u32 = 0xc586c0;
const RED: u32 = 0xf44747;
const GREEN: u32 = 0x6a9955;
const YELLOW: u32 = 0xdcdcaa;
const BLACK: u32 = 0x101010;
/// tint behind the selected stream row.
const SEL_BG: u32 = 0x094771;
/// hover/press tint for header + row buttons.
const HOVER_BG: u32 = 0x37373a;

/// poll ticks between `per-app.list` refreshes — ~1 Hz at [`POLL_INTERVAL`].
const REFRESH_EVERY_TICKS: u32 = 20;

/// poll ticks between `profile.list` refreshes — ~10 s. the profile *set*
/// only changes on `profile reload`; the active profile updates instantly
/// via `profile.changed`.
const REFRESH_PROFILES_EVERY_TICKS: u32 = 200;

#[derive(Debug, Clone)]
pub enum Message {
    /// poll tick: drain `rx`, repaint, periodically refresh.
    Tick,
    /// a keypress / event we don't act on.
    Noop,
    /// select a stream row by node id (mouse click).
    Select(u32),
    /// move the selection up / down one row (k/↑, j/↓).
    SelectUp,
    SelectDown,
    ToggleBypass,
    TogglePerAppMaster,
    /// toggle a stream's route (processed ↔ bypass).
    ToggleRoute(u32),
    /// toggle per-app management for a stream's app.
    TogglePerApp(u32),
    /// reset a stream's layer a deference.
    ResetLayerA(u32),
    /// hotkey variants acting on the current selection.
    RouteSelected,
    PerAppSelected,
    ResetSelected,
    /// switch to the named profile (pick_list).
    ProfileSelected(String),
    /// clear all overlay setting overrides.
    ClearOverrides,
}

/// mirrored daemon snapshot plus the live event channel it drains.
pub struct HeadroomApp {
    daemon_version: String,
    profile: String,
    bypass: bool,
    /// layer a master switch (per-app level control enabled globally).
    per_app_master: bool,
    /// overlay overrides (dotted key → value) shadowing the profile;
    /// surfaced so a stale `headroom set` isn't invisible.
    setting_overrides: BTreeMap<String, serde_json::Value>,
    /// daemon uptime at connect, plus local elapsed.
    base_uptime_s: u64,
    connected_at: Instant,
    default_route: Route,
    streams: BTreeMap<u32, StreamRoute>,
    /// per-stream layer a reduction (dB). presence = managed; inner `None`
    /// until the first `layer_a_level`.
    layer_a: BTreeMap<u32, Option<f32>>,
    /// richer per-stream snapshots (managed/ceiling/deferred), polled from
    /// `per-app.list`; drives the per-app toggle target + row label.
    la_snapshots: BTreeMap<u32, LayerASnapshot>,
    /// profiles for the switcher (seeded at connect).
    profiles: Vec<ProfileInfo>,
    /// selected stream node id; resolved via [`Self::effective_selection`].
    selected: Option<u32>,
    meters: Option<MeterTick>,
    last_meter_at: Option<Instant>,
    overflow_total: u64,
    last_error: Option<String>,
    disconnected: Option<String>,
    /// live event + worker stream from the reader / control threads.
    rx: Receiver<AppMsg>,
    /// hands control ops to the control thread.
    cmd_tx: Sender<ControlCmd>,
    /// poll-tick counter for the paced `per-app.list` refresh.
    tick_count: u32,
}

impl HeadroomApp {
    /// seed initial state from the snapshot, take ownership of the channel.
    pub fn new(boot: Bootstrap) -> Self {
        let Bootstrap {
            snapshot,
            rx,
            cmd_tx,
        } = boot;

        let mut app = Self {
            daemon_version: String::new(),
            profile: String::new(),
            bypass: false,
            per_app_master: false,
            setting_overrides: BTreeMap::new(),
            base_uptime_s: 0,
            connected_at: Instant::now(),
            default_route: Route::Processed,
            streams: BTreeMap::new(),
            layer_a: BTreeMap::new(),
            la_snapshots: BTreeMap::new(),
            profiles: Vec::new(),
            selected: None,
            meters: None,
            last_meter_at: None,
            overflow_total: 0,
            last_error: None,
            disconnected: None,
            rx,
            cmd_tx,
            tick_count: 0,
        };
        app.apply_snapshot(snapshot);
        // initial selection: first stream row (reconnects preserve the
        // user's selection, so this only runs at startup).
        app.selected = app.streams.keys().next().copied();
        app
    }

    /// replace mirrored daemon state from a fresh [`Snapshot`]. shared by
    /// [`Self::new`] + [`Self::reseed`]; leaves `selected` / channels /
    /// tick counter to the caller.
    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        let Snapshot {
            status,
            route_list,
            profiles,
        } = snapshot;

        let mut streams = BTreeMap::new();
        for s in route_list.current {
            streams.insert(s.node_id, s);
        }
        // `status.streams` is a superset; merge any not already present.
        for s in status.streams.iter() {
            streams.entry(s.node_id).or_insert_with(|| s.clone());
        }
        // seed meter column + richer snapshots from status (mirrors tui).
        let mut layer_a = BTreeMap::new();
        let mut la_snapshots = BTreeMap::new();
        for snap in status.layer_a {
            layer_a.insert(snap.node_id, Some(snap.reduction_db));
            la_snapshots.insert(snap.node_id, snap);
        }

        self.daemon_version = status.version;
        self.profile = status.profile;
        self.bypass = status.bypass;
        self.per_app_master = status.per_app;
        self.setting_overrides = status.setting_overrides;
        self.base_uptime_s = status.uptime_s;
        self.connected_at = Instant::now();
        self.default_route = route_list.default_route;
        self.streams = streams;
        self.layer_a = layer_a;
        self.la_snapshots = la_snapshots;
        self.profiles = profiles;
    }

    /// re-seed after the reader reconnected to a (possibly new) daemon.
    /// clears the disconnect/error banners + per-instance overflow counter
    /// (a restarted daemon resets its drop count). `selected` is preserved —
    /// `effective_selection` reconciles it against the new stream set.
    fn reseed(&mut self, snapshot: Snapshot) {
        self.apply_snapshot(snapshot);
        self.overflow_total = 0;
        self.last_error = None;
        self.disconnected = None;
        self.last_meter_at = None;
    }

    pub fn title(&self) -> String {
        "headroom monitor".to_string()
    }

    /// poll tick (`smol` backs `time::every`) + global keyboard input.
    /// `keyboard::listen` only surfaces keys not consumed by a focused
    /// widget, so the pick_list keeps its own arrow/enter handling.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            time::every(POLL_INTERVAL).map(|_| Message::Tick),
            keyboard::listen().map(map_key),
        ])
    }

    pub fn update(&mut self, message: Message) {
        match message {
            Message::Noop => {}
            Message::Tick => {
                self.pump();
                self.tick_count = self.tick_count.wrapping_add(1);
                if self.tick_count.is_multiple_of(REFRESH_EVERY_TICKS) {
                    // reconcile managed/ceiling/deferred state the event
                    // stream doesn't push (mirrors the tui's poll).
                    self.send_cmd(ControlCmd::RefreshLayerA);
                }
                if self.tick_count.is_multiple_of(REFRESH_PROFILES_EVERY_TICKS) {
                    // pick up profiles added/removed by `profile reload`.
                    self.send_cmd(ControlCmd::RefreshProfiles);
                }
            }
            Message::Select(id) => self.selected = Some(id),
            Message::SelectUp => self.move_selection(-1),
            Message::SelectDown => self.move_selection(1),
            Message::ToggleBypass => {
                let target = !self.bypass;
                self.send_cmd(ControlCmd::SetBypass(target));
                self.bypass = target; // optimistic; no bypass event from daemon
            }
            Message::TogglePerAppMaster => {
                let target = !self.per_app_master;
                self.send_cmd(ControlCmd::SetPerAppMaster(target));
                self.per_app_master = target;
            }
            Message::ToggleRoute(id) => self.toggle_route(id),
            Message::TogglePerApp(id) => self.toggle_per_app(id),
            Message::ResetLayerA(id) => self.reset_layer_a(id),
            Message::RouteSelected => {
                if let Some(id) = self.effective_selection() {
                    self.toggle_route(id);
                }
            }
            Message::PerAppSelected => {
                if let Some(id) = self.effective_selection() {
                    self.toggle_per_app(id);
                }
            }
            Message::ResetSelected => {
                if let Some(id) = self.effective_selection() {
                    self.reset_layer_a(id);
                }
            }
            Message::ProfileSelected(name) => {
                // optimistic: update the header now; the daemon's "used"
                // event reconciles a frame or two later (mirrors the tui).
                self.profile = name.clone();
                self.send_cmd(ControlCmd::UseProfile(name));
            }
            Message::ClearOverrides => {
                // optimistic: drop the indicator now; the daemon clears the
                // overlay and reapplies the profile's values.
                self.setting_overrides.clear();
                self.send_cmd(ControlCmd::ClearOverrides);
            }
        }
    }

    /// drain + apply every queued worker message.
    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle_msg(msg);
        }
    }

    fn handle_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::Event(ev) => self.apply_event(ev),
            AppMsg::Disconnected(reason) => self.disconnected = Some(reason),
            AppMsg::Reconnected(snapshot) => self.reseed(*snapshot),
            AppMsg::ControlError(e) => self.last_error = Some(e),
            AppMsg::Profiles(p) => {
                // reconcile the displayed profile from the `active` flag —
                // covers external `profile use` even if "used" was missed.
                if let Some(active) = p.iter().find(|x| x.active) {
                    self.profile = active.name.clone();
                }
                self.profiles = p;
            }
            AppMsg::LayerASnapshots(list) => {
                self.la_snapshots = list.into_iter().map(|s| (s.node_id, s)).collect();
            }
        }
    }

    /// queue a control op; a dead control thread lands in the footer.
    fn send_cmd(&mut self, cmd: ControlCmd) {
        if self.cmd_tx.send(cmd).is_err() {
            self.last_error = Some("control thread gone".into());
        }
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

    /// flip a stream's route, optimistically updating local state.
    ///
    /// prefer a *persistent* per-app rule (`route.set`, keyed by app label)
    /// so the route survives the app recreating its stream between tracks
    /// (Strawberry, mpv, …); fall back to a one-shot `route.stream` only
    /// when there's no label to key on. the optimistic update flips just
    /// this row; the daemon's reevaluate emits `stream_routed` for sibling
    /// streams of the same app, which `apply_event` reconciles.
    fn toggle_route(&mut self, node: u32) {
        let Some((app, cur)) = self.streams.get(&node).map(|s| (s.app.clone(), s.route)) else {
            return;
        };
        let to = match cur {
            Route::Processed => Route::Bypass,
            Route::Bypass => Route::Processed,
        };
        if app.is_empty() {
            self.send_cmd(ControlCmd::RouteStream { node_id: node, to });
        } else {
            self.send_cmd(ControlCmd::SetRoute { app, to });
        }
        if let Some(s) = self.streams.get_mut(&node) {
            s.route = to;
        }
    }

    /// toggle per-app management for a stream's app. guards an empty
    /// label; reconciliation comes from the poll.
    fn toggle_per_app(&mut self, node: u32) {
        let Some(app) = self.streams.get(&node).map(|s| s.app.clone()) else {
            return;
        };
        if app.is_empty() {
            self.last_error = Some("per-app: selected stream has no app label".into());
            return;
        }
        let managed = self.la_snapshots.get(&node).is_some_and(|s| s.managed);
        self.send_cmd(ControlCmd::SetPerApp {
            app,
            enabled: !managed,
        });
    }

    /// reset a stream's layer a deference.
    fn reset_layer_a(&mut self, node: u32) {
        self.send_cmd(ControlCmd::ResetLayerA { node_id: node });
    }

    fn uptime_s(&self) -> u64 {
        self.base_uptime_s
            .saturating_add(self.connected_at.elapsed().as_secs())
    }

    fn meters_stale(&self) -> bool {
        self.last_meter_at
            .map(|t| t.elapsed() > STALE_AFTER)
            .unwrap_or(true)
    }

    /// apply one wire event. ported from the tui's `apply_event`, minus
    /// the selection/snapshot bookkeeping the read-only gui doesn't need.
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
                if let Ok(re) = serde_json::from_value::<RoutingEvent>(inject_event(&ev)) {
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
                            self.layer_a.entry(node_id).or_insert(None);
                        }
                        RoutingEvent::LayerADetached { node_id } => {
                            self.layer_a.remove(&node_id);
                            self.la_snapshots.remove(&node_id);
                        }
                        RoutingEvent::RuleChanged => {}
                        _ => {}
                    }
                }
            }
            Topic::Profile => {
                // the daemon emits `profile` "used" `{name}` on apply (not
                // the typed `ProfileEvent::Changed` "changed"/`previous`
                // shape), so read `name` directly and accept either spelling
                // — keeps the header in sync with a cli `profile use` too.
                // on "reloaded" the profile *set* may have changed.
                match ev.event.as_str() {
                    "used" | "changed" => {
                        if let Some(name) = ev.data.get("name").and_then(|v| v.as_str()) {
                            self.profile = name.to_string();
                        }
                    }
                    "reloaded" => self.send_cmd(ControlCmd::RefreshProfiles),
                    _ => {}
                }
            }
            Topic::Daemon => {
                if let Ok(de) = serde_json::from_value::<DaemonEvent>(inject_event(&ev)) {
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

    // -----------------------------------------------------------------
    // View
    // -----------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        let content = column![
            self.header(),
            self.bus_panel(),
            self.loudness_panel(),
            self.streams_panel(),
            self.footer(),
        ]
        .spacing(8)
        .padding(12)
        .width(Length::Fill)
        .height(Length::Fill);

        // paint the window with the base background.
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(col(BG))),
                ..container::Style::default()
            })
            .into()
    }

    fn header(&self) -> Element<'static, Message> {
        let bypass = if self.bypass {
            badge_button("BYPASS", BLACK, YELLOW, Message::ToggleBypass)
        } else {
            badge_button("processed", GREEN, PANEL_BG, Message::ToggleBypass)
        };
        // state semantics (not action): label = current state, click
        // toggles — same convention as the per-app row chip.
        let per_app = if self.per_app_master {
            badge_button("per-app on", CYAN, PANEL_BG, Message::TogglePerAppMaster)
        } else {
            badge_button("per-app off", DIM, PANEL_BG, Message::TogglePerAppMaster)
        };

        // profile switcher: dropdown over the profile names.
        let names: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
        let picker = pick_list(names, Some(self.profile.clone()), Message::ProfileSelected)
            .text_size(14)
            .padding(iced::Padding::from([2.0, 8.0]));

        // left group fills the width so version/uptime is pushed right.
        let mut left = row![
            text("profile:").color(col(DIM)),
            picker,
            bypass,
            per_app,
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        // flag overlay overrides shadowing the profile — a stale
        // `headroom set` is otherwise invisible (e.g. agc.enabled pinned
        // off under a profile that enables it). click the pill to clear.
        if !self.setting_overrides.is_empty() {
            left = left.push(pill_button(
                format!("overrides: {}  ✕", fmt_overrides(&self.setting_overrides)),
                BLACK,
                YELLOW,
                Message::ClearOverrides,
            ));
        }

        let right = row![
            text(format!("v{}", self.daemon_version))
                .size(13)
                .color(col(DIM)),
            text(format!("uptime {}", fmt_uptime(self.uptime_s())))
                .size(13)
                .color(col(DIM)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        row![container(left).width(Length::Fill), right]
            .align_y(Alignment::Center)
            .into()
    }

    fn bus_panel(&self) -> Element<'static, Message> {
        let m = self.meters;
        // per-stage enable flags from the tick (missing ⇒ assume enabled).
        // agc + compressor are per-profile; limiter is the always-on
        // backstop and true peak is a measurement, so both are always on.
        let agc_on = m.map(|t| t.agc_enabled).unwrap_or(true);
        let comp_on = m.map(|t| t.compressor_enabled).unwrap_or(true);
        let body = column![
            meter_row(
                "AGC target",
                m.map(|t| t.agc_gain_db),
                -12.0,
                12.0,
                "dB",
                CYAN,
                agc_on,
            ),
            meter_row(
                "Compressor GR",
                m.map(|t| t.compressor_gr_db),
                -24.0,
                0.0,
                "dB",
                MAGENTA,
                comp_on,
            ),
            meter_row(
                "Limiter GR",
                m.map(|t| t.limiter_gr_db),
                -24.0,
                0.0,
                "dB",
                RED,
                true,
            ),
            meter_row(
                "True peak",
                m.map(|t| t.true_peak_dbtp),
                -60.0,
                3.0,
                "dBTP",
                GREEN,
                true,
            ),
        ]
        .spacing(4);
        panel("bus dsp", body.into())
    }

    fn loudness_panel(&self) -> Element<'static, Message> {
        let stale = self.meters_stale();
        let (mom, st, intg) = match self.meters {
            Some(m) => (
                Some(m.momentary_lufs),
                Some(m.shortterm_lufs),
                Some(m.integrated_lufs),
            ),
            None => (None, None, None),
        };
        let body = column![
            lufs_row("Momentary  (400 ms)", mom, stale),
            lufs_row("Short-term (3 s)", st, stale),
            lufs_row("Integrated (gated)", intg, stale),
        ]
        .spacing(4);
        panel("loudness (BS.1770)", body.into())
    }

    fn streams_panel(&self) -> Element<'static, Message> {
        let header = row![
            container(text("node").size(12).color(col(DIM))).width(Length::Fixed(64.0)),
            container(text("app").size(12).color(col(DIM))).width(Length::Fill),
            container(text("route").size(12).color(col(DIM))).width(Length::Fixed(96.0)),
            container(text("per-app").size(12).color(col(DIM))).width(Length::Fixed(96.0)),
            container(text("actions").size(12).color(col(DIM))).width(Length::Fixed(180.0)),
        ]
        .spacing(8);

        let selected = self.effective_selection();
        let rows = self.streams.values().map(|s| {
            stream_row(
                s,
                &self.layer_a,
                &self.la_snapshots,
                selected == Some(s.node_id),
            )
        });
        let list = scrollable(column(rows).spacing(4).width(Length::Fill)).height(Length::Fill);

        let title = format!(
            "streams ({}) — default: {}",
            self.streams.len(),
            self.default_route
        );

        container(
            column![text(title).size(12).color(col(DIM)), header, list]
                .spacing(4)
                .height(Length::Fill),
        )
        .padding(8)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(panel_style)
        .into()
    }

    fn footer(&self) -> Element<'static, Message> {
        let mut items: Vec<Element<'static, Message>> = Vec::new();
        if self.overflow_total > 0 {
            items.push(
                text(format!("dropped: {}", self.overflow_total))
                    .size(12)
                    .color(col(YELLOW))
                    .into(),
            );
        }
        if let Some(err) = &self.last_error {
            items.push(
                text(format!("daemon error: {err}"))
                    .size(12)
                    .color(col(RED))
                    .into(),
            );
        }
        if let Some(reason) = &self.disconnected {
            items.push(
                text(format!("disconnected: {reason}"))
                    .size(12)
                    .color(col(RED))
                    .into(),
            );
        } else if self.overflow_total == 0 && self.last_error.is_none() {
            items.push(text("connected").size(12).color(col(DIM)).into());
        }
        row(items).spacing(12).into()
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// `0xRRGGBB` → iced [`Color`].
fn col(hex: u32) -> Color {
    Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `radius`-rounded border, no stroke.
fn rounded(radius: f32) -> Border {
    Border {
        radius: radius.into(),
        ..Border::default()
    }
}

/// shared panel container style: dark fill, rounded corners.
fn panel_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(col(PANEL_BG))),
        border: rounded(6.0),
        ..container::Style::default()
    }
}

/// a titled panel: header label + body, on a rounded dark card.
fn panel(title: &'static str, body: Element<'static, Message>) -> Element<'static, Message> {
    container(
        column![text(title).size(12).color(col(DIM)), body]
            .spacing(4)
            .width(Length::Fill),
    )
    .padding(8)
    .width(Length::Fill)
    .style(panel_style)
    .into()
}

/// like [`badge_button`] but owns its text, for runtime-built labels.
fn pill_button(label: String, fg: u32, bg: u32, msg: Message) -> Element<'static, Message> {
    button(text(label).size(12).color(col(fg)))
        .padding(iced::Padding::from([2.0, 8.0]))
        .on_press(msg)
        .style(pill_style(fg, bg))
        .into()
}

/// active overrides as `key=value, …` (strings unquoted, else json) for
/// the header indicator.
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

/// a small colored pill that's also a button.
fn badge_button(
    label: &'static str,
    fg: u32,
    bg: u32,
    msg: Message,
) -> Element<'static, Message> {
    button(text(label).size(12).color(col(fg)))
        .padding(iced::Padding::from([2.0, 8.0]))
        .on_press(msg)
        .style(pill_style(fg, bg))
        .into()
}

/// button style closure: flat `bg` pill, `fg` text, lightening on
/// hover/press.
fn pill_style(fg: u32, bg: u32) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => col(HOVER_BG),
            _ => col(bg),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: col(fg),
            border: rounded(4.0),
            ..button::Style::default()
        }
    }
}

/// one bus meter row: `LABEL   VALUE   [====----]   min..max`. when
/// `!enabled` the stage is bypassed, so render "disabled" + a greyed bar
/// — a disabled stage must not look like one reading 0.
fn meter_row(
    label: &'static str,
    value: Option<f32>,
    min: f32,
    max: f32,
    unit: &'static str,
    color: u32,
    enabled: bool,
) -> Element<'static, Message> {
    // normalize into 0..100 for `progress_bar`, clamped. disabled ⇒ empty.
    let pct = match value {
        Some(v) if enabled => ((v.clamp(min, max) - min) / (max - min)).clamp(0.0, 1.0) * 100.0,
        _ => 0.0,
    };
    let bar_color = if enabled { color } else { DIM };
    let value_str = if !enabled {
        "disabled".to_string()
    } else {
        value
            .map(|v| format!("{v:+7.2} {unit}"))
            .unwrap_or_else(|| "    --".to_string())
    };
    let value_color = if enabled { FG } else { DIM };

    row![
        container(text(label).color(col(DIM))).width(Length::Fixed(130.0)),
        container(text(value_str).color(col(value_color)))
            .width(Length::Fixed(96.0))
            .align_x(Horizontal::Right),
        progress_bar(0.0..=100.0, pct)
            .length(Length::Fill)
            .girth(Length::Fixed(10.0))
            .style(move |_theme: &iced::Theme| progress_bar::Style {
                background: Background::Color(col(TRACK_BG)),
                bar: Background::Color(col(bar_color)),
                border: rounded(3.0),
            }),
        container(text(format!("{min:.0}..{max:.0}")).size(11).color(col(DIM)))
            .width(Length::Fixed(90.0))
            .align_x(Horizontal::Right),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// one BS.1770 loudness row.
fn lufs_row(label: &'static str, v: Option<f32>, stale: bool) -> Element<'static, Message> {
    let val = match v {
        Some(x) if x > LOUDNESS_FLOOR_LUFS + 0.5 => format!("{x:+7.2} LUFS"),
        _ => "    -- LUFS".to_string(),
    };
    row![
        container(text(label).color(col(DIM))).width(Length::Fixed(200.0)),
        text(val).color(col(if stale { DIM } else { FG })),
    ]
    .spacing(8)
    .into()
}

/// one stream routing row: status cells + route/per-app/reset actions,
/// wrapped in a `mouse_area` so clicking the row selects it. owns its
/// text, so it's `'static`.
fn stream_row(
    s: &StreamRoute,
    layer_a: &BTreeMap<u32, Option<f32>>,
    la_snapshots: &BTreeMap<u32, LayerASnapshot>,
    selected: bool,
) -> Element<'static, Message> {
    let node = s.node_id;
    let (route_text, route_color) = match s.route {
        Route::Processed => ("processed", GREEN),
        Route::Bypass => ("bypass", YELLOW),
    };
    let (la_text, la_color) = match layer_a.get(&node) {
        Some(Some(db)) => (format!("{db:+5.1} dB"), MAGENTA),
        Some(None) => ("attached".to_string(), DIM),
        None => ("—".to_string(), DIM),
    };

    // route button: always available, flips processed ↔ bypass.
    let (route_btn_label, route_btn_to) = match s.route {
        Route::Processed => ("→ bypass", BLACK),
        Route::Bypass => ("→ processed", BLACK),
    };
    let route_btn = action_button(route_btn_label, route_btn_to, CYAN, Message::ToggleRoute(node));

    // per-app + reset need an app label; omit them otherwise so the row
    // can't fire a no-op control op.
    let actions: Element<'static, Message> = if s.app.is_empty() {
        route_btn
    } else {
        // state semantics (like the master badge): label = whether managed,
        // click toggles.
        let managed = la_snapshots.get(&node).is_some_and(|snap| snap.managed);
        let (pa_label, pa_bg) = if managed {
            ("per-app on", CYAN)
        } else {
            ("per-app off", DIM)
        };
        let per_app_btn = action_button(pa_label, BLACK, pa_bg, Message::TogglePerApp(node));
        let reset_btn = action_button("reset", FG, TRACK_BG, Message::ResetLayerA(node));
        row![route_btn, per_app_btn, reset_btn].spacing(4).into()
    };

    let body = row![
        container(text(node.to_string()).color(col(DIM))).width(Length::Fixed(64.0)),
        container(text(s.app.clone())).width(Length::Fill),
        container(text(route_text).color(col(route_color))).width(Length::Fixed(96.0)),
        container(text(la_text).color(col(la_color))).width(Length::Fixed(96.0)),
        container(actions).width(Length::Fixed(180.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // tint the selected row; wrap it so a click anywhere selects it
    // (buttons still take their own clicks first).
    let tinted = container(body).padding(iced::Padding::from([2.0, 4.0])).style(
        move |_theme: &iced::Theme| container::Style {
            background: selected.then(|| Background::Color(col(SEL_BG))),
            border: rounded(4.0),
            ..container::Style::default()
        },
    );
    mouse_area(tinted).on_press(Message::Select(node)).into()
}

/// compact row-action button: `bg` pill, `fg` text, lightening on
/// hover/press.
fn action_button(
    label: &'static str,
    fg: u32,
    bg: u32,
    msg: Message,
) -> Element<'static, Message> {
    button(text(label).size(11).color(col(fg)))
        .padding(iced::Padding::from([1.0, 6.0]))
        .on_press(msg)
        .style(pill_style(fg, bg))
        .into()
}

/// the wire frame carries `{event, topic, data}` but the typed enums are
/// `#[serde(tag = "event")]` inside `data`, so re-inject the event name
/// before deserializing. same dance as the tui.
fn inject_event(ev: &Event) -> serde_json::Value {
    let mut obj = match &ev.data {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.insert("event".into(), serde_json::Value::String(ev.event.clone()));
    serde_json::Value::Object(obj)
}

/// `12345` seconds → `3h25m45s` / `25m45s` / `45s`.
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

/// map a keyboard event to a [`Message`], mirroring the tui's
/// `handle_key`. unhandled keys / non-press events become
/// [`Message::Noop`]; selection-targeted messages no-op in `update` when
/// nothing is selected.
fn map_key(event: keyboard::Event) -> Message {
    use keyboard::key::Named;
    let keyboard::Event::KeyPressed { key, .. } = event else {
        return Message::Noop;
    };
    match key {
        keyboard::Key::Character(c) => match c.as_str() {
            "b" => Message::ToggleBypass,
            "p" => Message::TogglePerAppMaster,
            "r" => Message::RouteSelected,
            "a" => Message::PerAppSelected,
            "x" => Message::ResetSelected,
            "j" => Message::SelectDown,
            "k" => Message::SelectUp,
            _ => Message::Noop,
        },
        keyboard::Key::Named(Named::Enter) => Message::RouteSelected,
        keyboard::Key::Named(Named::ArrowDown) => Message::SelectDown,
        keyboard::Key::Named(Named::ArrowUp) => Message::SelectUp,
        _ => Message::Noop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{unbounded, Receiver};
    use headroom_ipc::{RouteList, Sinks, Status};

    /// build an app over the given streams + layer a snapshots, keeping
    /// the control receiver alive so toggle tests can drain it.
    fn build(
        streams: Vec<StreamRoute>,
        layer_a: Vec<LayerASnapshot>,
    ) -> (HeadroomApp, Receiver<ControlCmd>) {
        let status = Status {
            version: "test".into(),
            protocol: 1,
            uptime_s: 0,
            profile: "default".into(),
            bypass: false,
            per_app: false,
            sinks: Sinks::default(),
            streams,
            layer_a,
            warnings: vec![],
            setting_overrides: Default::default(),
        };
        let route_list = RouteList {
            rules: vec![],
            current: vec![],
            default_route: Route::Processed,
        };
        let (_tx, rx) = unbounded();
        let (cmd_tx, cmd_rx) = unbounded();
        let app = HeadroomApp::new(Bootstrap {
            snapshot: Snapshot {
                status,
                route_list,
                profiles: vec![],
            },
            rx,
            cmd_tx,
        });
        (app, cmd_rx)
    }

    /// build a `status` snapshot for reconnect tests.
    fn status_with(profile: &str, streams: Vec<StreamRoute>) -> Status {
        Status {
            version: "test".into(),
            protocol: 1,
            uptime_s: 0,
            profile: profile.into(),
            bypass: false,
            per_app: false,
            sinks: Sinks::default(),
            streams,
            layer_a: vec![],
            warnings: vec![],
            setting_overrides: Default::default(),
        }
    }

    fn app() -> HeadroomApp {
        build(vec![], vec![]).0
    }

    fn stream(node_id: u32, app: &str, route: Route) -> StreamRoute {
        StreamRoute {
            node_id,
            app: app.into(),
            route,
        }
    }

    fn snap(node_id: u32, app: &str, managed: bool) -> LayerASnapshot {
        LayerASnapshot {
            node_id,
            app: app.into(),
            managed,
            volume_lin: 1.0,
            reduction_db: 0.0,
            user_ceiling_lin: None,
            deferred: false,
        }
    }

    #[test]
    fn meter_tick_event_updates_state() {
        let mut s = app();
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
        s.apply_event(Event::new(Topic::Meters, "tick", &tick).unwrap());
        assert!(s.meters.is_some());
        assert!(s.last_meter_at.is_some());
        assert!(!s.meters_stale());
    }

    #[test]
    fn routing_event_inserts_then_removes_stream() {
        let mut s = app();
        s.apply_event(
            Event::new(
                Topic::Routing,
                "stream_routed",
                &serde_json::json!({ "node_id": 42, "app": "firefox", "to": "bypass" }),
            )
            .unwrap(),
        );
        let st = s.streams.get(&42).expect("tracked");
        assert_eq!(st.app, "firefox");
        assert_eq!(st.route, Route::Bypass);

        s.apply_event(
            Event::new(
                Topic::Routing,
                "stream_removed",
                &serde_json::json!({ "node_id": 42 }),
            )
            .unwrap(),
        );
        assert!(!s.streams.contains_key(&42));
    }

    #[test]
    fn layer_a_level_updates_reduction() {
        let mut s = app();
        s.apply_event(
            Event::new(
                Topic::Routing,
                "layer_a_attached",
                &serde_json::json!({ "node_id": 11, "app": "loud" }),
            )
            .unwrap(),
        );
        assert_eq!(s.layer_a.get(&11), Some(&None));
        s.apply_event(
            Event::new(
                Topic::Meters,
                "layer_a_level",
                &serde_json::json!({
                    "node_id": 11, "app": "loud",
                    "volume_lin": 0.25_f32, "reduction_db": -11.8_f32,
                }),
            )
            .unwrap(),
        );
        let r = s.layer_a.get(&11).copied().flatten().unwrap();
        assert!((r - -11.8).abs() < 1e-4);
    }

    #[test]
    fn profile_changed_updates_active() {
        let mut s = app();
        s.apply_event(
            Event::new(
                Topic::Profile,
                "changed",
                &serde_json::json!({ "name": "night", "previous": "default" }),
            )
            .unwrap(),
        );
        assert_eq!(s.profile, "night");
    }

    #[test]
    fn profile_used_event_updates_active() {
        // the shape the daemon actually emits on `profile.use`.
        let mut s = app();
        s.apply_event(
            Event::new(Topic::Profile, "used", &serde_json::json!({ "name": "movie" })).unwrap(),
        );
        assert_eq!(s.profile, "movie");
    }

    #[test]
    fn profile_selected_updates_header_optimistically() {
        let (mut s, _cmd) = build(vec![], vec![]);
        assert_eq!(s.profile, "default");
        s.update(Message::ProfileSelected("party".into()));
        assert_eq!(s.profile, "party");
    }

    #[test]
    fn profiles_refresh_syncs_active_profile() {
        let (mut s, _tx) = build(vec![], vec![]);
        s.handle_msg(AppMsg::Profiles(vec![
            ProfileInfo {
                name: "default".into(),
                active: false,
                description: String::new(),
            },
            ProfileInfo {
                name: "gaming".into(),
                active: true,
                description: String::new(),
            },
        ]));
        assert_eq!(s.profile, "gaming");
        assert_eq!(s.profiles.len(), 2);
    }

    #[test]
    fn reconnect_reseeds_state_and_clears_disconnect() {
        // start with one stream, then simulate the daemon dropping and a
        // *new* instance coming back with a different stream set + active
        // profile (node ids change across a restart).
        let (mut s, _cmd) = build(vec![stream(42, "firefox", Route::Bypass)], vec![]);
        s.handle_msg(AppMsg::Disconnected("daemon shutdown".into()));
        s.overflow_total = 7;
        s.last_error = Some("stale".into());
        assert!(s.disconnected.is_some());

        s.handle_msg(AppMsg::Reconnected(Box::new(Snapshot {
            status: status_with("music", vec![stream(99, "mpv", Route::Processed)]),
            route_list: RouteList {
                rules: vec![],
                current: vec![stream(99, "mpv", Route::Processed)],
                default_route: Route::Processed,
            },
            profiles: vec![ProfileInfo {
                name: "music".into(),
                active: true,
                description: String::new(),
            }],
        })));

        // banners cleared, per-instance counter reset.
        assert!(s.disconnected.is_none());
        assert!(s.last_error.is_none());
        assert_eq!(s.overflow_total, 0);
        // mirrored state replaced with the new daemon's snapshot.
        assert_eq!(s.profile, "music");
        assert!(!s.streams.contains_key(&42));
        assert_eq!(s.streams.get(&99).map(|r| r.route), Some(Route::Processed));
        assert_eq!(s.profiles.len(), 1);
    }

    #[test]
    fn daemon_overflow_accumulates() {
        let mut s = app();
        s.apply_event(
            Event::new(
                Topic::Daemon,
                "overflow",
                &serde_json::json!({ "lost_topic": "meters", "lost": 3u32, "total_lost": 5u64 }),
            )
            .unwrap(),
        );
        assert_eq!(s.overflow_total, 5);
    }

    #[test]
    fn fmt_uptime_buckets() {
        assert_eq!(fmt_uptime(5), "5s");
        assert_eq!(fmt_uptime(75), "1m15s");
        assert_eq!(fmt_uptime(3725), "1h02m05s");
    }

    // -- selection -------------------------------------------------------

    #[test]
    fn selection_moves_in_key_order_and_wraps() {
        let (mut s, _cmd) = build(
            vec![
                stream(30, "c", Route::Processed),
                stream(10, "a", Route::Processed),
                stream(20, "b", Route::Processed),
            ],
            vec![],
        );
        // seeded selection is the first key (BTreeMap order: 10).
        assert_eq!(s.effective_selection(), Some(10));
        s.update(Message::SelectDown);
        assert_eq!(s.selected, Some(20));
        s.update(Message::SelectDown);
        assert_eq!(s.selected, Some(30));
        s.update(Message::SelectDown);
        assert_eq!(s.selected, Some(10)); // wrap to top
        s.update(Message::SelectUp);
        assert_eq!(s.selected, Some(30)); // wrap to bottom
    }

    #[test]
    fn selection_empty_is_none() {
        let (mut s, _cmd) = build(vec![], vec![]);
        s.update(Message::SelectDown);
        assert_eq!(s.selected, None);
        assert_eq!(s.effective_selection(), None);
    }

    // -- control commands emitted by toggles -----------------------------

    fn drain(cmd: &Receiver<ControlCmd>) -> ControlCmd {
        cmd.try_recv().expect("a command was emitted")
    }

    #[test]
    fn toggle_bypass_emits_and_updates_optimistically() {
        let (mut s, cmd) = build(vec![], vec![]);
        assert!(!s.bypass);
        s.update(Message::ToggleBypass);
        assert!(matches!(drain(&cmd), ControlCmd::SetBypass(true)));
        assert!(s.bypass); // optimistic
        s.update(Message::ToggleBypass);
        assert!(matches!(drain(&cmd), ControlCmd::SetBypass(false)));
        assert!(!s.bypass);
    }

    #[test]
    fn toggle_per_app_master_emits_and_updates() {
        let (mut s, cmd) = build(vec![], vec![]);
        s.update(Message::TogglePerAppMaster);
        assert!(matches!(drain(&cmd), ControlCmd::SetPerAppMaster(true)));
        assert!(s.per_app_master);
    }

    #[test]
    fn toggle_route_sets_persistent_per_app_rule_and_flips_optimistically() {
        // a labelled stream toggles via a persistent per-app rule
        // (route.set), not a one-shot per-stream reroute — so it
        // survives the app recreating its stream between tracks.
        let (mut s, cmd) = build(vec![stream(42, "firefox", Route::Processed)], vec![]);
        s.update(Message::ToggleRoute(42));
        match drain(&cmd) {
            ControlCmd::SetRoute { app, to } => {
                assert_eq!(app, "firefox");
                assert_eq!(to, Route::Bypass);
            }
            _ => panic!("expected SetRoute"),
        }
        assert_eq!(s.streams.get(&42).unwrap().route, Route::Bypass); // optimistic
    }

    #[test]
    fn toggle_route_empty_label_falls_back_to_per_stream() {
        // a stream with no app label can't key a per-app rule, so it
        // falls back to the one-shot per-stream reroute.
        let (mut s, cmd) = build(vec![stream(5, "", Route::Processed)], vec![]);
        s.update(Message::ToggleRoute(5));
        match drain(&cmd) {
            ControlCmd::RouteStream { node_id, to } => {
                assert_eq!(node_id, 5);
                assert_eq!(to, Route::Bypass);
            }
            _ => panic!("expected RouteStream fallback"),
        }
    }

    #[test]
    fn route_selected_acts_on_selection() {
        let (mut s, cmd) = build(vec![stream(7, "mpv", Route::Bypass)], vec![]);
        s.update(Message::RouteSelected);
        match drain(&cmd) {
            ControlCmd::SetRoute { app, to } => {
                assert_eq!(app, "mpv");
                assert_eq!(to, Route::Processed);
            }
            _ => panic!("expected SetRoute"),
        }
    }

    #[test]
    fn toggle_per_app_targets_inverse_of_managed() {
        // node 1 managed → toggling asks to disable; node 2 unmanaged →
        // toggling asks to enable.
        let (mut s, cmd) = build(
            vec![
                stream(1, "spotify", Route::Processed),
                stream(2, "discord", Route::Processed),
            ],
            vec![snap(1, "spotify", true)],
        );
        s.update(Message::TogglePerApp(1));
        match drain(&cmd) {
            ControlCmd::SetPerApp { app, enabled } => {
                assert_eq!(app, "spotify");
                assert!(!enabled);
            }
            _ => panic!("expected SetPerApp"),
        }
        s.update(Message::TogglePerApp(2));
        match drain(&cmd) {
            ControlCmd::SetPerApp { app, enabled } => {
                assert_eq!(app, "discord");
                assert!(enabled);
            }
            _ => panic!("expected SetPerApp"),
        }
    }

    #[test]
    fn toggle_per_app_empty_label_errors_no_command() {
        let (mut s, cmd) = build(vec![stream(5, "", Route::Processed)], vec![]);
        s.update(Message::TogglePerApp(5));
        assert!(cmd.try_recv().is_err()); // no command emitted
        assert!(s.last_error.is_some());
    }

    #[test]
    fn reset_selected_emits_reset() {
        let (mut s, cmd) = build(vec![stream(9, "game", Route::Processed)], vec![]);
        s.update(Message::ResetSelected);
        assert!(matches!(drain(&cmd), ControlCmd::ResetLayerA { node_id: 9 }));
    }

    #[test]
    fn profile_selected_emits_use_profile() {
        let (mut s, cmd) = build(vec![], vec![]);
        s.update(Message::ProfileSelected("night".into()));
        match drain(&cmd) {
            ControlCmd::UseProfile(name) => assert_eq!(name, "night"),
            _ => panic!("expected UseProfile"),
        }
    }

    #[test]
    fn clear_overrides_emits_and_clears_optimistically() {
        let (mut s, cmd) = build(vec![], vec![]);
        s.setting_overrides
            .insert("agc.enabled".into(), serde_json::json!(false));
        s.update(Message::ClearOverrides);
        assert!(matches!(drain(&cmd), ControlCmd::ClearOverrides));
        assert!(s.setting_overrides.is_empty()); // optimistic
    }

    #[test]
    fn tick_refreshes_layer_a_on_cadence() {
        let (mut s, cmd) = build(vec![], vec![]);
        for _ in 0..(REFRESH_EVERY_TICKS - 1) {
            s.update(Message::Tick);
        }
        assert!(cmd.try_recv().is_err()); // not yet
        s.update(Message::Tick); // the REFRESH_EVERY_TICKS-th tick
        assert!(matches!(drain(&cmd), ControlCmd::RefreshLayerA));
    }

    // -- key mapping -----------------------------------------------------

    fn press(c: &str) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(c.into()),
            modified_key: keyboard::Key::Character(c.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::default(),
            text: None,
            repeat: false,
        }
    }

    #[test]
    fn key_mapping_matches_tui_hotkeys() {
        assert!(matches!(map_key(press("b")), Message::ToggleBypass));
        assert!(matches!(map_key(press("p")), Message::TogglePerAppMaster));
        assert!(matches!(map_key(press("r")), Message::RouteSelected));
        assert!(matches!(map_key(press("a")), Message::PerAppSelected));
        assert!(matches!(map_key(press("x")), Message::ResetSelected));
        assert!(matches!(map_key(press("j")), Message::SelectDown));
        assert!(matches!(map_key(press("k")), Message::SelectUp));
        assert!(matches!(map_key(press("z")), Message::Noop));
    }
}
