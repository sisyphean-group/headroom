//! op dispatch + handlers. each locks state briefly and returns a
//! [`Response`]; mutations route through [`ProfileStore`].

use serde::Serialize;
use serde_json::{json, Value};

use headroom_ipc::{
    ErrorCode, Event, HelloData, Op, ProfileInfo, ProtoError, Request, Response, Route, RouteList,
    SinkInfo, Sinks, Status, StreamRoute, Topic, PROTOCOL_VERSION,
};

use crate::profile_store::StoreError;
use crate::pw::command::PwCommand;
use crate::pw::filter::FilterControl;
use crate::state::SharedState;

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn dispatch(req: &Request, state: &SharedState) -> Response {
    match &req.op {
        Op::Status => status(req.id, state),
        Op::ProfileList => profile_list(req.id, state),
        Op::ProfileShow { name } => profile_show(req.id, name.as_deref(), state),
        Op::ProfileUse { name } => profile_use(req.id, name, state),
        Op::ProfileReload => profile_reload(req.id, state),
        Op::RouteList => route_list(req.id, state),
        Op::RouteSet { app, to } => route_set(req.id, app, *to, state),
        Op::RouteUnset { app } => route_unset(req.id, app, state),
        Op::RouteStream { node_id, to } => route_stream(req.id, *node_id, *to, state),
        Op::SettingGet { key } => setting_get(req.id, key, state),
        Op::SettingSet { key, value } => setting_set(req.id, key, value.clone(), state),
        Op::SettingList => setting_list(req.id, state),
        Op::SettingClear { key } => setting_clear(req.id, key, state),
        Op::SettingReset => setting_reset(req.id, state),
        Op::BypassSet { enabled } => bypass_set(req.id, *enabled, state),
        Op::LayerAList => layer_a_list(req.id, state),
        Op::PerAppSet { app, enabled } => per_app_set(req.id, app, *enabled, state),
        Op::PerAppMaster { enabled } => per_app_master(req.id, *enabled, state),
        Op::LayerAReset { node_id } => layer_a_reset(req.id, *node_id, state),
        Op::Hello { protocol } => hello(req.id, *protocol),
        Op::Subscribe { .. } | Op::Unsubscribe { .. } => not_yet(req, "Phase 4d"),
        // Op is #[non_exhaustive]; future ops look unknown here.
        _ => err(
            req.id,
            ErrorCode::UnknownOp,
            format!("op '{}' is not recognised by this daemon", req.op.name()),
        ),
    }
}

// ---------------------------------------------------------------------------
// Read-only ops
// ---------------------------------------------------------------------------

fn status(id: u64, state: &SharedState) -> Response {
    let s = state.lock();
    let effective = s.profiles.effective();
    let snapshot = Status {
        version: DAEMON_VERSION.into(),
        protocol: PROTOCOL_VERSION,
        uptime_s: s.started_at.elapsed().as_secs(),
        profile: effective.name.clone(),
        bypass: s.profiles.bypass_global(),
        per_app: effective.per_app.enabled,
        sinks: Sinks {
            processed: SinkInfo {
                node_id: s.processed_sink_id,
                name: Some(crate::pw::sink::NODE_NAME.to_owned()),
                ready: s.processed_sink_id.is_some(),
                // filter's current rate; `None` only pre-`Filter::create`.
                sample_rate: s.filter_sample_rate,
            },
            real: s.real_sink.clone(),
        },
        streams: s
            .streams
            .values()
            .map(|r| StreamRoute {
                node_id: r.node_id,
                app: r.app.clone(),
                route: r.route,
            })
            .collect(),
        layer_a: s.layer_a.values().cloned().collect(),
        warnings: s.profiles.warnings(),
        setting_overrides: s.profiles.setting_overrides(),
    };
    ok(id, &snapshot)
}

fn hello(id: u64, client_protocol: u32) -> Response {
    if client_protocol != PROTOCOL_VERSION {
        tracing::warn!(
            client_protocol,
            daemon_protocol = PROTOCOL_VERSION,
            "IPC client protocol version differs from daemon; \
             continuing best-effort — some ops or fields may not line up"
        );
    }
    ok(
        id,
        &HelloData {
            daemon: "headroom".into(),
            version: DAEMON_VERSION.into(),
            protocol: PROTOCOL_VERSION,
        },
    )
}

fn profile_list(id: u64, state: &SharedState) -> Response {
    let s = state.lock();
    let active = s.profiles.effective().name.clone();
    let profiles: Vec<ProfileInfo> = s
        .profiles
        .list()
        .map(|sp| ProfileInfo {
            name: sp.name.clone(),
            active: sp.name == active,
            description: sp.profile.description.clone(),
        })
        .collect();
    ok(id, &json!({ "profiles": profiles }))
}

fn profile_show(id: u64, name: Option<&str>, state: &SharedState) -> Response {
    let s = state.lock();
    let effective = s.profiles.effective();
    match name {
        None => ok(id, effective),
        Some(requested) if requested == effective.name => ok(id, effective),
        Some(requested) => match s.profiles.list().find(|sp| sp.name == requested) {
            Some(found) => ok(id, &found.profile),
            None => err(
                id,
                ErrorCode::NotFound,
                format!("profile '{requested}' not loaded"),
            ),
        },
    }
}

fn route_list(id: u64, state: &SharedState) -> Response {
    let s = state.lock();
    let effective = s.profiles.effective();
    let body = RouteList {
        rules: effective.rules.clone(),
        current: s
            .streams
            .values()
            .map(|r| StreamRoute {
                node_id: r.node_id,
                app: r.app.clone(),
                route: r.route,
            })
            .collect(),
        default_route: effective.default_route.route,
    };
    ok(id, &body)
}

fn setting_get(id: u64, key: &str, state: &SharedState) -> Response {
    let s = state.lock();
    let json_value = match serde_json::to_value(s.profiles.effective()) {
        Ok(v) => v,
        Err(e) => {
            return err(
                id,
                ErrorCode::Internal,
                format!("serialise profile: {e}"),
            );
        }
    };
    drop(s);

    let Some(found) = lookup_dotted(&json_value, key) else {
        return err(
            id,
            ErrorCode::NotFound,
            format!("setting '{key}' not found in active profile"),
        );
    };
    ok(id, &json!({ "key": key, "value": found }))
}

fn setting_list(id: u64, state: &SharedState) -> Response {
    let s = state.lock();
    let json_value = match serde_json::to_value(s.profiles.effective()) {
        Ok(v) => v,
        Err(e) => {
            return err(
                id,
                ErrorCode::Internal,
                format!("serialise profile: {e}"),
            );
        }
    };
    drop(s);

    let mut flat = serde_json::Map::new();
    flatten(&json_value, "", &mut flat);
    ok(id, &json!({ "settings": flat }))
}

// ---------------------------------------------------------------------------
// Mutating ops
// ---------------------------------------------------------------------------

fn profile_use(id: u64, name: &str, state: &SharedState) -> Response {
    let mut s = state.lock();
    if name == s.profiles.effective().name {
        let body = json!({ "name": name });
        return ok(id, &body);
    }
    match s.profiles.use_profile(name) {
        Ok(()) => {
            tracing::info!(name, "profile.use applied");
            publish_profile_changed(&mut s, name);
            let control = s.filter_control.clone();
            let snap = build_dsp_configs(&s);
            post_reevaluate(&s);
            // `[per_app]` must re-apply to already-managed streams, not
            // just new ones.
            post_reevaluate_layer_a(&s);
            drop(s);
            push_dsp_update(control.as_ref(), snap);
            ok(id, &json!({ "name": name }))
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn profile_reload(id: u64, state: &SharedState) -> Response {
    match execute_reload(state) {
        Ok(report) => ok(
            id,
            &json!({ "reloaded": report.loaded, "warnings": report.warnings }),
        ),
        Err(e) => store_err_to_response(id, e),
    }
}

/// scan disk, publish events, push fresh DSP configs to the filter.
/// used by [`Op::ProfileReload`] and the file-watcher.
///
/// # Errors
/// fatal disk I/O from [`ProfileStore::reload`].
pub(crate) fn execute_reload(
    state: &SharedState,
) -> Result<crate::profile_store::ReloadReport, StoreError> {
    let mut s = state.lock();
    let report = s.profiles.reload()?;
    tracing::info!(
        loaded = report.loaded.len(),
        warnings = report.warnings.len(),
        "profile reload applied"
    );
    for w in &report.warnings {
        tracing::warn!(warning = %w, "profile reload warning");
    }
    publish_profile_reloaded(&mut s, &report.loaded);
    let control = s.filter_control.clone();
    let snap = build_dsp_configs(&s);
    post_reevaluate(&s);
    // re-apply `[per_app]` to already-managed streams (as profile.use).
    post_reevaluate_layer_a(&s);
    drop(s);
    push_dsp_update(control.as_ref(), snap);
    Ok(report)
}

fn route_set(id: u64, app: &str, to: Route, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.set_route(app, to) {
        Ok(()) => {
            tracing::info!(app, ?to, "route.set applied");
            publish_rule_changed(&mut s);
            post_reevaluate(&s);
            drop(s);
            ok(id, &Value::Null)
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn route_unset(id: u64, app: &str, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.unset_route(app) {
        Ok(()) => {
            tracing::info!(app, "route.unset applied");
            publish_rule_changed(&mut s);
            post_reevaluate(&s);
            drop(s);
            ok(id, &Value::Null)
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn publish_rule_changed(state: &mut crate::state::DaemonState) {
    if let Ok(event) = Event::new(Topic::Routing, "rule_changed", &json!({})) {
        state.broadcaster.publish(Topic::Routing, event);
    }
}

/// ask the main loop to re-run `routing::evaluate` against every known
/// stream. without it new policy only applies to future streams. stale
/// post harmless (idempotent, reads state at apply time).
fn post_reevaluate(state: &crate::state::DaemonState) {
    let Some(tx) = state.pw_command_tx.as_ref() else {
        tracing::debug!("no PipeWire command channel; reevaluation skipped (test mode)");
        return;
    };
    if tx.send(PwCommand::ReevaluateAll).is_err() {
        tracing::warn!("PipeWire command channel closed; reevaluation lost");
    }
}

fn publish_profile_changed(state: &mut crate::state::DaemonState, name: &str) {
    if let Ok(event) = Event::new(Topic::Profile, "used", &json!({ "name": name })) {
        state.broadcaster.publish(Topic::Profile, event);
    }
}

fn publish_profile_reloaded(state: &mut crate::state::DaemonState, loaded: &[String]) {
    if let Ok(event) = Event::new(Topic::Profile, "reloaded", &json!({ "loaded": loaded })) {
        state.broadcaster.publish(Topic::Profile, event);
    }
}

fn setting_set(id: u64, key: &str, value: Value, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.set_setting(key, value) {
        Ok(()) => {
            tracing::info!(key, "setting.set applied");
            let control = s.filter_control.clone();
            let snap = build_dsp_configs(&s);
            drop(s);
            push_dsp_update(control.as_ref(), snap);
            ok(id, &Value::Null)
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn setting_clear(id: u64, key: &str, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.clear_setting(key) {
        Ok(existed) => {
            tracing::info!(key, existed, "setting.clear applied");
            let control = s.filter_control.clone();
            let snap = build_dsp_configs(&s);
            // a cleared override may touch any subsystem; reapply all.
            post_reevaluate(&s);
            post_reevaluate_layer_a(&s);
            drop(s);
            push_dsp_update(control.as_ref(), snap);
            ok(id, &json!({ "key": key, "cleared": existed }))
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn setting_reset(id: u64, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.clear_all_settings() {
        Ok(cleared) => {
            tracing::info!(cleared, "setting.reset applied");
            let control = s.filter_control.clone();
            let snap = build_dsp_configs(&s);
            post_reevaluate(&s);
            post_reevaluate_layer_a(&s);
            drop(s);
            push_dsp_update(control.as_ref(), snap);
            ok(id, &json!({ "cleared": cleared }))
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn route_stream(id: u64, node_id: u32, to: Route, state: &SharedState) -> Response {
    let mut s = state.lock();
    let Some(stream) = s.streams.get_mut(&node_id) else {
        return err(
            id,
            ErrorCode::NotFound,
            format!("no stream with node_id {node_id} is currently routed by the daemon"),
        );
    };
    let app_label = stream.app.clone();
    let prior = stream.route;
    stream.route = to;
    // record synchronously so `status`/`route.list` reflect it now;
    // the metadata write is async on the main loop (≤ ~50 ms).
    let event = Event::new(
        Topic::Routing,
        "stream_routed",
        &json!({ "node_id": node_id, "app": app_label, "to": to.as_str() }),
    );
    if let Ok(event) = event {
        s.broadcaster.publish(Topic::Routing, event);
    }
    let tx = s.pw_command_tx.clone();
    drop(s);
    if let Some(tx) = tx {
        if tx
            .send(PwCommand::RouteStream {
                node_id,
                to,
                app_label: app_label.clone(),
            })
            .is_err()
        {
            tracing::warn!(node_id, "PipeWire command channel closed; metadata write skipped");
        }
    } else {
        tracing::debug!(
            node_id,
            "no PipeWire command channel; state updated but no metadata write (test mode)"
        );
    }
    tracing::info!(
        node_id,
        app = app_label.as_str(),
        ?prior,
        new = ?to,
        "route.stream applied"
    );
    ok(id, &Value::Null)
}

fn bypass_set(id: u64, enabled: bool, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.set_bypass(enabled) {
        Ok(()) => {
            tracing::info!(enabled, "bypass.set applied");
            let tx = s.pw_command_tx.clone();
            drop(s);
            // bypass is a real graph op, not just a flag: re-evaluate
            // every stream (now routing Bypass) so links move to the
            // real sink. reassert_default_processed is also gated on
            // bypass so WP's default sticks for "default"-routed apps.
            if let Some(tx) = tx {
                if tx.send(PwCommand::ReevaluateAll).is_err() {
                    tracing::warn!("PipeWire command channel closed; bypass toggle had no graph effect");
                }
            }
            ok(id, &Value::Null)
        }
        Err(e) => store_err_to_response(id, e),
    }
}

// ---------------------------------------------------------------------------
// Per-app (Layer A) ops
// ---------------------------------------------------------------------------

fn layer_a_list(id: u64, state: &SharedState) -> Response {
    let s = state.lock();
    let mut list: Vec<headroom_ipc::LayerASnapshot> = s.layer_a.values().cloned().collect();
    drop(s);
    list.sort_by_key(|snap| snap.node_id);
    ok(id, &json!({ "layer_a": list }))
}

fn per_app_set(id: u64, app: &str, enabled: bool, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.set_per_app_enabled(app, enabled) {
        Ok(()) => {
            tracing::info!(app, enabled, "per-app.set applied");
            publish_rule_changed(&mut s);
            post_reevaluate_layer_a(&s);
            drop(s);
            ok(id, &Value::Null)
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn per_app_master(id: u64, enabled: bool, state: &SharedState) -> Response {
    let mut s = state.lock();
    match s.profiles.set_per_app_master(enabled) {
        Ok(()) => {
            tracing::info!(enabled, "per-app.master applied");
            publish_rule_changed(&mut s);
            post_reevaluate_layer_a(&s);
            drop(s);
            ok(id, &Value::Null)
        }
        Err(e) => store_err_to_response(id, e),
    }
}

fn layer_a_reset(id: u64, node_id: u32, state: &SharedState) -> Response {
    let s = state.lock();
    let tx = s.pw_command_tx.clone();
    drop(s);
    if let Some(tx) = tx {
        if tx
            .send(PwCommand::LayerAResetDeference { node_id })
            .is_err()
        {
            tracing::warn!(node_id, "PipeWire command channel closed; layer-a reset lost");
        }
    } else {
        tracing::debug!(node_id, "no PipeWire command channel; layer-a reset skipped (test mode)");
    }
    tracing::info!(node_id, "per-app.reset applied");
    ok(id, &Value::Null)
}

/// reconcile Layer A taps after a per-app/master change. Layer A mirror
/// of [`post_reevaluate`]; stale post harmless.
fn post_reevaluate_layer_a(state: &crate::state::DaemonState) {
    let Some(tx) = state.pw_command_tx.as_ref() else {
        tracing::debug!("no PipeWire command channel; layer-a reevaluation skipped (test mode)");
        return;
    };
    if tx.send(PwCommand::ReevaluateLayerA).is_err() {
        tracing::warn!("PipeWire command channel closed; layer-a reevaluation lost");
    }
}

/// profile-driven DSP configs to push at the filter. built under the
/// lock; pushed after it drops so the audio-thread hand-off never
/// contends with the daemon mutex.
struct DspSnapshot {
    compressor: headroom_dsp::CompressorConfig,
    limiter: headroom_dsp::LimiterConfig,
    agc_enabled: bool,
}

fn build_dsp_configs(state: &crate::state::DaemonState) -> DspSnapshot {
    let effective = state.profiles.effective();
    DspSnapshot {
        compressor: effective.build_compressor_config(),
        limiter: effective.build_limiter_config(),
        agc_enabled: effective.agc.enabled,
    }
}

/// push compressor + limiter + AGC-enable into the filter ring, if up.
/// AGC *target_db* still comes from the slow controller's ticks — this
/// only flips the enable flag. no-op headless.
fn push_dsp_update(control: Option<&FilterControl>, snap: DspSnapshot) {
    let Some(c) = control else { return };
    c.set_compressor(snap.compressor);
    c.set_limiter(snap.limiter);
    c.set_agc_enabled(snap.agc_enabled);
}

fn store_err_to_response(id: u64, e: StoreError) -> Response {
    let code = match &e {
        StoreError::ProfileNotFound(_)
        | StoreError::SettingNotFound(_)
        | StoreError::NoUserRoute(_) => ErrorCode::NotFound,
        StoreError::SettingInvalid { .. } => ErrorCode::InvalidArgs,
        StoreError::Io(_)
        | StoreError::OverlayParse(_)
        | StoreError::OverlaySerialize(_) => ErrorCode::Internal,
    };
    err(id, code, e.to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lookup_dotted<'v>(value: &'v Value, key: &str) -> Option<&'v Value> {
    let mut cur = value;
    for part in key.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn flatten(value: &Value, prefix: &str, out: &mut serde_json::Map<String, Value>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(v, &next, out);
            }
        }
        // arrays + primitives surfaced wholesale at their key prefix;
        // rule arrays etc. are better read via route.list/profile.show.
        _ => {
            out.insert(prefix.to_string(), value.clone());
        }
    }
}

fn ok<T: Serialize>(id: u64, body: &T) -> Response {
    Response::ok(id, body).unwrap_or_else(|e| {
        Response::err(
            id,
            ProtoError::new(ErrorCode::Internal, format!("serialise reply: {e}")),
        )
    })
}

/// used by the connection handler for subscribe/unsubscribe ops.
pub(crate) fn ok_value<T: Serialize>(id: u64, body: &T) -> Response {
    ok(id, body)
}

fn err(id: u64, code: ErrorCode, msg: impl Into<String>) -> Response {
    Response::err(id, ProtoError::new(code, msg))
}

fn not_yet(req: &Request, phase: &str) -> Response {
    err(
        req.id,
        ErrorCode::UnknownOp,
        format!("op '{}' not implemented yet ({})", req.op.name(), phase),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store::ProfileStore;
    use crate::state::{self, RoutedStream};
    use headroom_ipc::{Op, Request, ResponsePayload, Route};

    fn shared_with_default_profile() -> SharedState {
        state::shared(crate::state::DaemonState::new(ProfileStore::builtin()))
    }

    fn extract_ok(resp: Response) -> Value {
        match resp.payload {
            ResponsePayload::Ok { result } => result,
            ResponsePayload::Err { error } => panic!("expected ok, got {error}"),
        }
    }

    #[test]
    fn hello_echoes_daemon_handshake_and_serves_on_mismatch() {
        let state = shared_with_default_profile();
        let resp = dispatch(&Request::new(1, Op::Hello { protocol: PROTOCOL_VERSION }), &state);
        let body = extract_ok(resp);
        assert_eq!(body["daemon"], "headroom");
        assert_eq!(body["protocol"], PROTOCOL_VERSION);
        let resp = dispatch(
            &Request::new(2, Op::Hello { protocol: PROTOCOL_VERSION + 1 }),
            &state,
        );
        let body = extract_ok(resp);
        assert_eq!(body["protocol"], PROTOCOL_VERSION);
    }

    #[test]
    fn status_reports_active_profile_and_zero_streams() {
        let state = shared_with_default_profile();
        let req = Request::new(1, Op::Status);
        let resp = dispatch(&req, &state);
        let body = extract_ok(resp);
        assert_eq!(body["profile"], "default");
        assert_eq!(body["bypass"], false);
        assert_eq!(body["protocol"], PROTOCOL_VERSION);
        assert!(body["streams"].as_array().unwrap().is_empty());
        // Builtin store with no overlay → no warnings.
        assert!(
            body.get("warnings")
                .and_then(|w| w.as_array())
                .is_none_or(|a| a.is_empty()),
            "expected empty/absent warnings on healthy startup"
        );
    }

    #[test]
    fn status_surfaces_store_warnings() {
        use crate::profile_store::ProfileStore;
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        // Build a load-from-disk store with a broken TOML so a warning
        // is recorded, then point Status at it.
        let base = std::env::temp_dir().join(format!(
            "headroom-warntest-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(base.join("config/profiles")).unwrap();
        fs::create_dir_all(base.join("state")).unwrap();
        fs::write(
            base.join("config/profiles/broken.toml"),
            "this is not = valid",
        )
        .unwrap();
        let paths = crate::profile_store::StorePaths {
            config_dir: base.join("config"),
            state_dir: base.join("state"),
            share_dirs: vec![],
            extra_profile_dirs: vec![],
        };
        let store = ProfileStore::load(&paths).unwrap();
        let state = state::shared(crate::state::DaemonState::new(store));

        let resp = dispatch(&Request::new(1, Op::Status), &state);
        let body = extract_ok(resp);
        let warnings = body["warnings"].as_array().expect("warnings field");
        assert!(
            warnings.iter().any(|w| w.as_str().unwrap_or("").contains("broken.toml")),
            "expected warning mentioning broken.toml, got {warnings:?}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn status_surfaces_routed_streams() {
        let state = shared_with_default_profile();
        state.lock().streams.insert(
            42,
            RoutedStream {
                node_id: 42,
                app: "firefox".into(),
                route: Route::Processed,
            },
        );
        let resp = dispatch(&Request::new(1, Op::Status), &state);
        let body = extract_ok(resp);
        let streams = body["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["app"], "firefox");
        assert_eq!(streams[0]["route"], "processed");
    }

    #[test]
    fn profile_list_returns_active() {
        let state = shared_with_default_profile();
        let resp = dispatch(&Request::new(1, Op::ProfileList), &state);
        let body = extract_ok(resp);
        let list = body["profiles"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "default");
        assert_eq!(list[0]["active"], true);
    }

    #[test]
    fn profile_show_default_returns_active_profile() {
        let state = shared_with_default_profile();
        let resp = dispatch(&Request::new(1, Op::ProfileShow { name: None }), &state);
        let body = extract_ok(resp);
        assert_eq!(body["name"], "default");
    }

    #[test]
    fn profile_show_unknown_returns_not_found() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::ProfileShow {
                    name: Some("nightclub-mix".into()),
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::NotFound),
            ResponsePayload::Ok { .. } => panic!("expected NotFound"),
        }
    }

    #[test]
    fn route_list_returns_profile_rules_and_default_route() {
        let state = shared_with_default_profile();
        let resp = dispatch(&Request::new(1, Op::RouteList), &state);
        let body = extract_ok(resp);
        // default profile carries the bypass + processed rule sets.
        let rules = body["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(body["default_route"], "processed");
    }

    #[test]
    fn setting_get_dotted_path() {
        let state = shared_with_default_profile();
        let req = Request::new(
            1,
            Op::SettingGet {
                key: "limiter.ceiling_dbtp".into(),
            },
        );
        let resp = dispatch(&req, &state);
        let body = extract_ok(resp);
        assert_eq!(body["key"], "limiter.ceiling_dbtp");
        let v = body["value"].as_f64().unwrap();
        assert!((v - -0.1).abs() < 1e-6);
    }

    #[test]
    fn setting_get_unknown_key_is_not_found() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::SettingGet {
                    key: "completely.not.a.key".into(),
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::NotFound),
            ResponsePayload::Ok { .. } => panic!("expected NotFound"),
        }
    }

    #[test]
    fn setting_list_flattens_to_dotted_keys() {
        let state = shared_with_default_profile();
        let resp = dispatch(&Request::new(1, Op::SettingList), &state);
        let body = extract_ok(resp);
        let settings = body["settings"].as_object().unwrap();
        assert!(settings.contains_key("agc.target_lufs"));
        assert!(settings.contains_key("limiter.ceiling_dbtp"));
        assert!(settings.contains_key("compressor.ratio"));
    }

    // -----------------------------------------------------------------
    // 4c mutating ops
    // -----------------------------------------------------------------

    #[test]
    fn bypass_set_toggles_flag() {
        let state = shared_with_default_profile();
        assert!(!state.lock().profiles.bypass_global());

        dispatch(
            &Request::new(1, Op::BypassSet { enabled: true }),
            &state,
        );
        assert!(state.lock().profiles.bypass_global());

        dispatch(
            &Request::new(2, Op::BypassSet { enabled: false }),
            &state,
        );
        assert!(!state.lock().profiles.bypass_global());
    }

    #[test]
    fn route_set_inserts_user_rule_at_top() {
        let state = shared_with_default_profile();
        dispatch(
            &Request::new(
                1,
                Op::RouteSet {
                    app: "obs".into(),
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        let s = state.lock();
        let rules = &s.profiles.effective().rules;
        // First rule is now the user-set one.
        assert_eq!(rules[0].match_.process_binary, vec!["obs".to_string()]);
        assert_eq!(rules[0].route, Route::Bypass);
    }

    #[test]
    fn route_set_replaces_existing_user_rule() {
        let state = shared_with_default_profile();
        // First set: bypass.
        dispatch(
            &Request::new(
                1,
                Op::RouteSet {
                    app: "obs".into(),
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        // Second set on the same app: processed. Should replace, not stack.
        dispatch(
            &Request::new(
                2,
                Op::RouteSet {
                    app: "obs".into(),
                    to: Route::Processed,
                },
            ),
            &state,
        );
        let s = state.lock();
        let rules = &s.profiles.effective().rules;
        let user_rules: Vec<_> = rules
            .iter()
            .filter(|r| {
                r.match_.process_binary.len() == 1 && r.match_.process_binary[0] == "obs"
            })
            .collect();
        assert_eq!(user_rules.len(), 1);
        assert_eq!(user_rules[0].route, Route::Processed);
    }

    #[test]
    fn route_unset_removes_user_rule() {
        let state = shared_with_default_profile();
        dispatch(
            &Request::new(
                1,
                Op::RouteSet {
                    app: "obs".into(),
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        dispatch(
            &Request::new(
                2,
                Op::RouteUnset {
                    app: "obs".into(),
                },
            ),
            &state,
        );
        let s = state.lock();
        let still_there = s
            .profiles
            .effective()
            .rules
            .iter()
            .any(|r| r.match_.process_binary.len() == 1 && r.match_.process_binary[0] == "obs");
        assert!(!still_there);
    }

    #[test]
    fn route_unset_unknown_app_is_not_found() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::RouteUnset {
                    app: "no-such-app".into(),
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::NotFound),
            ResponsePayload::Ok { .. } => panic!("expected NotFound"),
        }
    }

    #[test]
    fn route_unset_does_not_remove_shipped_rules() {
        let state = shared_with_default_profile();
        // "firefox" is in a shipped multi-app rule; route.unset must
        // refuse to touch it.
        let resp = dispatch(
            &Request::new(
                1,
                Op::RouteUnset {
                    app: "firefox".into(),
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::NotFound),
            ResponsePayload::Ok { .. } => panic!("expected NotFound"),
        }
        // And firefox is still in the rules (via the shipped rule).
        let s = state.lock();
        let still_firefox = s
            .profiles
            .effective()
            .rules
            .iter()
            .any(|r| r.match_.process_binary.iter().any(|p| p == "firefox"));
        assert!(still_firefox);
    }

    #[test]
    fn setting_set_mutates_value() {
        let state = shared_with_default_profile();
        dispatch(
            &Request::new(
                1,
                Op::SettingSet {
                    key: "limiter.ceiling_dbtp".into(),
                    value: json!(-1.0),
                },
            ),
            &state,
        );
        let v = state.lock().profiles.effective().limiter.ceiling_dbtp;
        assert!((v - -1.0).abs() < 1e-6);
    }

    #[test]
    fn setting_set_rejects_wrong_type() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::SettingSet {
                    key: "limiter.ceiling_dbtp".into(),
                    value: json!("not a number"),
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::InvalidArgs),
            ResponsePayload::Ok { .. } => panic!("expected InvalidArgs"),
        }
        // Profile unchanged.
        assert!((state.lock().profiles.effective().limiter.ceiling_dbtp - -0.1).abs() < 1e-6);
    }

    #[test]
    fn setting_set_unknown_key_is_not_found() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::SettingSet {
                    key: "limiter.does_not_exist".into(),
                    value: json!(1),
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::NotFound),
            ResponsePayload::Ok { .. } => panic!("expected NotFound"),
        }
    }

    #[test]
    fn profile_use_active_is_noop_success() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::ProfileUse {
                    name: "default".into(),
                },
            ),
            &state,
        );
        let body = extract_ok(resp);
        assert_eq!(body["name"], "default");
    }

    #[test]
    fn profile_use_unknown_returns_not_found() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::ProfileUse {
                    name: "night".into(),
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::NotFound),
            ResponsePayload::Ok { .. } => panic!("expected NotFound"),
        }
    }

    #[test]
    fn profile_reload_built_in_only_returns_default() {
        // Built-in stores have no disk paths; reload returns just the
        // built-in default and a warning saying there's nothing to scan.
        let state = shared_with_default_profile();
        let resp = dispatch(&Request::new(1, Op::ProfileReload), &state);
        let body = extract_ok(resp);
        let reloaded = body["reloaded"].as_array().unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0], "default");
    }

    #[test]
    fn setting_set_pushes_dsp_update() {
        use crate::pw::filter::{AudioCmd, FilterControl};
        let state = shared_with_default_profile();
        let (control, mut consumer) = FilterControl::for_testing(8);
        state.lock().filter_control = Some(control);

        dispatch(
            &Request::new(
                1,
                Op::SettingSet {
                    key: "limiter.ceiling_dbtp".into(),
                    value: json!(-1.5),
                },
            ),
            &state,
        );

        // Expect a compressor cmd and a limiter cmd (we push both for
        // simplicity even when only one field changed).
        let mut saw_limiter = false;
        while let Ok(cmd) = consumer.pop() {
            if let AudioCmd::SetLimiter(cfg) = cmd {
                assert!((cfg.ceiling_dbtp - -1.5).abs() < 1e-6);
                saw_limiter = true;
            }
        }
        assert!(saw_limiter, "setting.set should push a SetLimiter cmd");
    }

    #[test]
    fn route_set_does_not_push_dsp_update() {
        // Routing changes don't touch DSP. Filter must be left alone.
        use crate::pw::filter::FilterControl;
        let state = shared_with_default_profile();
        let (control, mut consumer) = FilterControl::for_testing(8);
        state.lock().filter_control = Some(control);

        dispatch(
            &Request::new(
                1,
                Op::RouteSet {
                    app: "obs".into(),
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        assert!(consumer.pop().is_err(), "route.set must not push DSP cmds");
    }

    #[test]
    fn route_stream_unknown_node_id_returns_not_found() {
        let state = shared_with_default_profile();
        let resp = dispatch(
            &Request::new(
                1,
                Op::RouteStream {
                    node_id: 9999,
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        match resp.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, ErrorCode::NotFound),
            ResponsePayload::Ok { .. } => panic!("expected NotFound"),
        }
    }

    #[test]
    fn route_stream_updates_state_synchronously() {
        let state = shared_with_default_profile();
        // Seed: a known stream currently routed Processed.
        state.lock().streams.insert(
            42,
            RoutedStream {
                node_id: 42,
                app: "firefox".into(),
                route: Route::Processed,
            },
        );

        let resp = dispatch(
            &Request::new(
                1,
                Op::RouteStream {
                    node_id: 42,
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        assert!(matches!(resp.payload, ResponsePayload::Ok { .. }));
        assert_eq!(state.lock().streams[&42].route, Route::Bypass);
    }

    #[test]
    fn route_stream_pushes_command_when_channel_present() {
        use crate::pw::command::PwCommand;
        let state = shared_with_default_profile();
        let (tx, rx) = crossbeam_channel::unbounded::<PwCommand>();
        state.lock().pw_command_tx = Some(tx);
        state.lock().streams.insert(
            42,
            RoutedStream {
                node_id: 42,
                app: "firefox".into(),
                route: Route::Processed,
            },
        );

        dispatch(
            &Request::new(
                1,
                Op::RouteStream {
                    node_id: 42,
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        let cmd = rx.try_recv().expect("command should arrive");
        let PwCommand::RouteStream {
            node_id,
            to,
            app_label,
        } = cmd
        else {
            panic!("expected RouteStream, got {cmd:?}");
        };
        assert_eq!(node_id, 42);
        assert_eq!(to, Route::Bypass);
        assert_eq!(app_label, "firefox");
    }

    #[test]
    fn route_stream_no_channel_is_still_success() {
        // Tests / pre-PipeWire startup: no tx is fine, state still
        // updates and the op returns Ok.
        let state = shared_with_default_profile();
        state.lock().streams.insert(
            42,
            RoutedStream {
                node_id: 42,
                app: "mpv".into(),
                route: Route::Processed,
            },
        );
        let resp = dispatch(
            &Request::new(
                1,
                Op::RouteStream {
                    node_id: 42,
                    to: Route::Bypass,
                },
            ),
            &state,
        );
        assert!(matches!(resp.payload, ResponsePayload::Ok { .. }));
        assert_eq!(state.lock().streams[&42].route, Route::Bypass);
    }
}
