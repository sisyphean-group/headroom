//! protocol message types. spec: `IPC.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ProtoError;

/// subscription topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Topic {
    /// loudness / peak / GR telemetry.
    Meters,
    Profile,
    Routing,
    Daemon,
    /// synthetic, for `hello`; clients never subscribe.
    Control,
}

impl Topic {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Topic::Meters => "meters",
            Topic::Profile => "profile",
            Topic::Routing => "routing",
            Topic::Daemon => "daemon",
            Topic::Control => "control",
        }
    }

    /// topics clients may subscribe to (excludes `Control`).
    #[must_use]
    pub const fn subscribable() -> &'static [Topic] {
        &[Topic::Meters, Topic::Profile, Topic::Routing, Topic::Daemon]
    }
}

impl std::fmt::Display for Topic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// routing decision for one app or stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    /// through the processed (filtered) sink.
    Processed,
    /// straight to the real hardware sink; no processing, no extra graph hop.
    Bypass,
}

impl Route {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Processed => "processed",
            Route::Bypass => "bypass",
        }
    }
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// all operations. wire form `{ "op": "<name>", "args": <args> }`; arg-less ops omit `args`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args")]
#[non_exhaustive]
pub enum Op {
    #[serde(rename = "status")]
    Status,

    #[serde(rename = "profile.list")]
    ProfileList,

    #[serde(rename = "profile.use")]
    ProfileUse { name: String },

    /// `name` omitted ⇒ active profile.
    #[serde(rename = "profile.show")]
    ProfileShow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// reload all profile files from disk.
    #[serde(rename = "profile.reload")]
    ProfileReload,

    #[serde(rename = "route.list")]
    RouteList,

    /// add/replace a persistent routing rule.
    #[serde(rename = "route.set")]
    RouteSet {
        /// typically `application.process.binary`.
        app: String,
        to: Route,
    },

    #[serde(rename = "route.unset")]
    RouteUnset { app: String },

    /// one-shot reroute of a live stream.
    #[serde(rename = "route.stream")]
    RouteStream { node_id: u32, to: Route },

    #[serde(rename = "setting.get")]
    SettingGet {
        /// dotted key, e.g. `compressor.threshold_db`.
        key: String,
    },

    #[serde(rename = "setting.set")]
    SettingSet { key: String, value: Value },

    #[serde(rename = "setting.list")]
    SettingList,

    /// revert one key to the active profile's value.
    #[serde(rename = "setting.clear")]
    SettingClear { key: String },

    /// revert all settings to the active profile; leaves route/per-app/bypass overrides intact.
    #[serde(rename = "setting.reset")]
    SettingReset,

    /// global bypass kill switch.
    #[serde(rename = "bypass.set")]
    BypassSet { enabled: bool },

    /// per-app (Layer A) controller state for managed streams.
    #[serde(rename = "per-app.list")]
    LayerAList,

    /// Layer A enable for one app (persistent overlay override).
    #[serde(rename = "per-app.set")]
    PerAppSet {
        /// process_binary or application_name.
        app: String,
        enabled: bool,
    },

    /// Layer A master switch (persistent overlay override).
    #[serde(rename = "per-app.master")]
    PerAppMaster { enabled: bool },

    /// clear a stream's deference lock (user-ceiling / strict-mode) so the controller resumes.
    #[serde(rename = "per-app.reset")]
    LayerAReset { node_id: u32 },

    #[serde(rename = "subscribe")]
    Subscribe { topics: Vec<Topic> },

    #[serde(rename = "unsubscribe")]
    Unsubscribe { topics: Vec<Topic> },

    /// optional client protocol handshake
    #[serde(rename = "hello")]
    Hello { protocol: u32 },
}

impl Op {
    /// canonical wire name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Op::Status => "status",
            Op::ProfileList => "profile.list",
            Op::ProfileUse { .. } => "profile.use",
            Op::ProfileShow { .. } => "profile.show",
            Op::ProfileReload => "profile.reload",
            Op::RouteList => "route.list",
            Op::RouteSet { .. } => "route.set",
            Op::RouteUnset { .. } => "route.unset",
            Op::RouteStream { .. } => "route.stream",
            Op::SettingGet { .. } => "setting.get",
            Op::SettingSet { .. } => "setting.set",
            Op::SettingList => "setting.list",
            Op::SettingClear { .. } => "setting.clear",
            Op::SettingReset => "setting.reset",
            Op::BypassSet { .. } => "bypass.set",
            Op::LayerAList => "per-app.list",
            Op::PerAppSet { .. } => "per-app.set",
            Op::PerAppMaster { .. } => "per-app.master",
            Op::LayerAReset { .. } => "per-app.reset",
            Op::Subscribe { .. } => "subscribe",
            Op::Unsubscribe { .. } => "unsubscribe",
            Op::Hello { .. } => "hello",
        }
    }
}

/// client-to-server request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// client-chosen; echoed in the paired response.
    pub id: u64,
    /// flattened: contributes `op` and optional `args` alongside `id`.
    #[serde(flatten)]
    pub op: Op,
}

impl Request {
    #[must_use]
    pub fn new(id: u64, op: Op) -> Self {
        Self { id, op }
    }
}

/// server-to-client response, paired to a request by `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub payload: ResponsePayload,
}

impl Response {
    /// # Errors
    /// serde error if `value` fails to serialize.
    pub fn ok<T: Serialize>(id: u64, value: &T) -> Result<Self, serde_json::Error> {
        Ok(Self {
            id,
            payload: ResponsePayload::Ok {
                result: serde_json::to_value(value)?,
            },
        })
    }

    #[must_use]
    pub fn err(id: u64, error: ProtoError) -> Self {
        Self {
            id,
            payload: ResponsePayload::Err { error },
        }
    }
}

/// body of a `Response`. on the wire: a single `result` or `error` field inlined alongside `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsePayload {
    Ok { result: Value },
    Err { error: ProtoError },
}

/// server-to-client event on a subscribed topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// e.g. `tick`, `changed`.
    pub event: String,
    pub topic: Topic,
    /// shape depends on `topic` and `event`.
    pub data: Value,
}

impl Event {
    /// # Errors
    /// serde error if `data` fails to serialize.
    pub fn new<T: Serialize>(
        topic: Topic,
        event: impl Into<String>,
        data: &T,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            event: event.into(),
            topic,
            data: serde_json::to_value(data)?,
        })
    }
}

/// one frame as received from the server: paired response or fan-out event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerFrame {
    Response(Response),
    Event(Event),
}

/// result body of `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// semver.
    pub version: String,
    pub protocol: u32,
    pub uptime_s: u64,
    pub profile: String,
    pub bypass: bool,
    /// Layer A master switch (per-app level control, global).
    #[serde(default)]
    pub per_app: bool,
    pub sinks: Sinks,
    pub streams: Vec<StreamRoute>,
    /// per-app (Layer A) controller state; empty when not managing anything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layer_a: Vec<LayerASnapshot>,
    /// non-fatal warnings (typically profile-load: TOML parse errors, active profile missing on disk).
    /// reflects the last successful load/reload; empty when healthy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// user-overlay setting overrides (dotted key → value). these shadow the active profile and
    /// persist across `profile.use`, so a stale `headroom set` can silently override it
    /// (e.g. `agc.enabled=false`). surfaced so clients can flag it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub setting_overrides: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sinks {
    /// the processed virtual sink; the only sink headroom creates.
    pub processed: SinkInfo,
    /// hardware sink headroom forwards to, and where bypassed streams go directly.
    pub real: SinkInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SinkInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// linked and accepting audio.
    #[serde(default)]
    pub ready: bool,
    /// native rate (Hz). filter matches the *real* sink's rate to skip the output-edge resample;
    /// the processed sink advertises whatever rate the filter currently runs at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
}

/// one playback stream and its route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamRoute {
    pub node_id: u32,
    /// typically `application.process.binary`.
    pub app: String,
    pub route: Route,
}

/// per-app (Layer A) controller state for one managed stream; on `status` and `per-app.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerASnapshot {
    pub node_id: u32,
    pub app: String,
    /// true while a tap + controller is actively managing the stream.
    pub managed: bool,
    /// last linear volume written (1.0 = unity).
    pub volume_lin: f32,
    /// asserted gain reduction in dB (`>= 0`; `0` = no cut).
    pub reduction_db: f32,
    /// user-set ceiling (linear) when ceiling-mode deference is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_ceiling_lin: Option<f32>,
    /// strict-mode deference has locked the controller until `per-app.reset`.
    pub deferred: bool,
}

/// entry in `profile.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub name: String,
    pub active: bool,
    pub description: String,
}

/// result body of `route.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteList {
    /// in evaluation order.
    pub rules: Vec<RouteRule>,
    pub current: Vec<StreamRoute>,
    /// fallback when no rule matches.
    pub default_route: Route,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRule {
    #[serde(rename = "match")]
    pub match_: RouteRuleMatch,
    pub route: Route,
}

/// present fields are AND'd; values within a field are OR'd; empty match is true.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteRuleMatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_binary: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub application_name: Vec<String>,
    /// `pipewire.access.portal.app_id` (Flatpak).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub portal_app_id: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_role: Vec<String>,
}

/// `hello` payload (sent on connect).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloData {
    /// always `"headroom"`.
    pub daemon: String,
    /// semver.
    pub version: String,
    pub protocol: u32,
}

/// `meters` tick payload.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MeterTick {
    /// BS.1770 M, 400 ms, LUFS.
    pub momentary_lufs: f32,
    /// BS.1770 S, 3 s, LUFS.
    pub shortterm_lufs: f32,
    /// BS.1770 I, gated, LUFS.
    pub integrated_lufs: f32,
    /// max true peak across channels, dBTP.
    pub true_peak_dbtp: f32,
    /// compressor + limiter, dB.
    pub gain_reduction_db: f32,
    pub compressor_gr_db: f32,
    pub limiter_gr_db: f32,
    /// AGC contribution (positive = boost), dB.
    pub agc_gain_db: f32,
    /// when `false`, AGC is bypassed and `agc_gain_db` is inert (0) — render as "disabled",
    /// not an active 0 dB. absent ⇒ `true`.
    #[serde(default = "default_true")]
    pub agc_enabled: bool,
    /// same as [`Self::agc_enabled`] for `compressor_gr_db`. (limiter has no flag — always-on backstop.)
    #[serde(default = "default_true")]
    pub compressor_enabled: bool,
}

/// absent ⇒ enabled, matching the always-on assumption before these fields existed.
fn default_true() -> bool {
    true
}

/// `profile` topic events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProfileEvent {
    Changed { name: String, previous: String },
    /// `changed`: profiles whose definitions changed on disk.
    Reloaded { changed: Vec<String> },
}

/// `routing` topic events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RoutingEvent {
    StreamRouted {
        node_id: u32,
        app: String,
        to: Route,
    },
    /// stream's PipeWire node disappeared; drop state keyed by `node_id`.
    StreamRemoved { node_id: u32 },
    /// Layer A tap attached; daemon now manages `Props.channelVolumes` and publishes `meters/layer_a_level`.
    LayerAAttached { node_id: u32, app: String },
    /// Layer A tap torn down (usually stream gone); drop Layer A state for `node_id`.
    LayerADetached { node_id: u32 },
    RuleChanged,
}

/// `meters/layer_a_level` payload — emitted when the Layer A controller writes a new `channelVolumes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerALevel {
    pub node_id: u32,
    pub app: String,
    /// linear volume written (1.0 = unity).
    pub volume_lin: f32,
    /// asserted gain reduction, dB; ≤ 0 when reducing.
    pub reduction_db: f32,
}

/// `daemon` topic events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DaemonEvent {
    Started { version: String },
    Shutdown,
    /// events dropped on this connection.
    Overflow {
        lost_topic: Topic,
        /// lost in this batch.
        lost: u32,
        /// total lost on this connection.
        total_lost: u64,
    },
    /// non-fatal error.
    Error {
        /// matches `ErrorCode` when applicable.
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(v: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let s = serde_json::to_string(v).expect("serialize");
        serde_json::from_str(&s).expect("deserialize")
    }

    #[test]
    fn op_status_serializes_without_args() {
        let req = Request::new(1, Op::Status);
        let s = serde_json::to_string(&req).unwrap();
        // Must be the flat form, no `kind` wrapper, no `args` field.
        assert_eq!(s, r#"{"id":1,"op":"status"}"#);

        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn op_profile_use_round_trips_flat() {
        let req = Request::new(
            7,
            Op::ProfileUse {
                name: "night".into(),
            },
        );
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(s, r#"{"id":7,"op":"profile.use","args":{"name":"night"}}"#);

        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn route_set_serializes_canonical() {
        let req = Request::new(
            12,
            Op::RouteSet {
                app: "firefox".into(),
                to: Route::Processed,
            },
        );
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(
            s,
            r#"{"id":12,"op":"route.set","args":{"app":"firefox","to":"processed"}}"#
        );
    }

    #[test]
    fn response_ok_shape() {
        let resp = Response::ok(3, &serde_json::json!({ "name": "default" })).unwrap();
        let s = serde_json::to_string(&resp).unwrap();
        assert_eq!(s, r#"{"id":3,"result":{"name":"default"}}"#);
    }

    #[test]
    fn response_err_shape() {
        let resp = Response::err(4, ProtoError::new(crate::ErrorCode::NotFound, "missing"));
        let s = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            s,
            r#"{"id":4,"error":{"code":"NOT_FOUND","message":"missing"}}"#
        );
    }

    #[test]
    fn server_frame_distinguishes_response_from_event() {
        let resp = Response::ok(1, &serde_json::json!(null)).unwrap();
        let s = serde_json::to_string(&resp).unwrap();
        let frame: ServerFrame = serde_json::from_str(&s).unwrap();
        assert!(matches!(frame, ServerFrame::Response(_)));

        let ev = Event::new(Topic::Meters, "tick", &serde_json::json!({})).unwrap();
        let s = serde_json::to_string(&ev).unwrap();
        let frame: ServerFrame = serde_json::from_str(&s).unwrap();
        assert!(matches!(frame, ServerFrame::Event(_)));
    }

    #[test]
    fn meter_tick_roundtrip() {
        let m = MeterTick {
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
        let back = roundtrip(&m);
        assert_eq!(back, m);
    }

    #[test]
    fn topic_string_canonical() {
        assert_eq!(Topic::Meters.as_str(), "meters");
        assert_eq!(serde_json::to_string(&Topic::Meters).unwrap(), "\"meters\"");
        let t: Topic = serde_json::from_str("\"profile\"").unwrap();
        assert_eq!(t, Topic::Profile);
    }

    #[test]
    fn route_string_canonical() {
        let r: Route = serde_json::from_str("\"bypass\"").unwrap();
        assert_eq!(r, Route::Bypass);
        assert_eq!(serde_json::to_string(&r).unwrap(), "\"bypass\"");
    }

    #[test]
    fn error_code_screaming_snake() {
        let s = serde_json::to_string(&crate::ErrorCode::InvalidFrame).unwrap();
        assert_eq!(s, "\"INVALID_FRAME\"");
        let c: crate::ErrorCode = serde_json::from_str("\"UNKNOWN_OP\"").unwrap();
        assert_eq!(c, crate::ErrorCode::UnknownOp);
    }

    #[test]
    fn subscribe_op_roundtrip() {
        let req = Request::new(
            5,
            Op::Subscribe {
                topics: vec![Topic::Meters, Topic::Profile],
            },
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }
}
