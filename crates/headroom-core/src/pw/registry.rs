//! PipeWire registry subscription + routing decisions. watches new
//! globals (metadata, nodes, ports, links), routes `Stream/Output/Audio`
//! per the active profile, tracks `preferred_real_sink`, and owns the
//! explicit link layer (WP only honours `target.object` at connect time).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crossbeam_channel::Receiver;
use pipewire::{
    core::Core,
    link::Link,
    metadata::{Metadata, MetadataListener},
    node::{Node, NodeListener},
    properties::{properties, Properties},
    registry::{GlobalObject, Listener, Registry},
    spa::{
        param::ParamType,
        pod::{
            deserialize::PodDeserializer, serialize::PodSerializer, Object as PodObject, Pod,
            Property, PropertyFlags, Value, ValueArray,
        },
        utils::{dict::DictRef, SpaTypes},
    },
    types::ObjectType,
};
use rtrb::Consumer;

use headroom_ipc::{Event, LayerASnapshot, Route, Topic};
use serde_json::json;

use crate::app_level::{self, AppLevelController, LayerAEval};
use crate::pw::command::PwCommand;
use crate::pw::metadata::{
    format_sink_target_value, parse_default_sink_name, DEFAULT_AUDIO_SINK_KEY, SPA_JSON_TYPE,
    TARGET_OBJECT_KEY,
};
use crate::pw::sink::NODE_NAME as PROCESSED_SINK_NAME;
use crate::pw::tap::{MeasurementSample, StreamTap};
use crate::routing::{self, PwNodeInfo, RoutingDecision};
use crate::state::{RoutedStream, SharedState};

/// assumed audio-thread quantum for Layer A controllers. nodes may
/// negotiate other values; the smoothing constants tolerate 512–2048.
const LAYER_A_QUANTUM_FRAMES: f32 = 1024.0;

/// Layer A block period (seconds); falls back to 48 kHz when no real
/// sink rate is known.
fn layer_a_block_dt_s(sample_rate: Option<u32>) -> f32 {
    let sr = sample_rate.unwrap_or(crate::pw::filter::DEFAULT_SAMPLE_RATE);
    LAYER_A_QUANTUM_FRAMES / (sr as f32)
}

/// view of a `Port` global. tracked because WP won't wire
/// `Stream/Output → Stream/Input` — we build links from these port ids.
#[derive(Debug, Clone)]
struct PortInfo {
    /// the port's global id (used as `link.{input,output}.port`).
    port_id: u32,
    direction: PortDirection,
    /// per-node ordinal (port.id), pairs FL↔FL / FR↔FR when channel
    /// hints are absent.
    ordinal: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortDirection {
    In,
    Out,
}

/// view of a `Link` global. tracked so routing can find + destroy a
/// WP-created link that conflicts with the declared route — WP only
/// honours `target.object` at connect time, so the daemon owns the
/// link layer for any stream it routes.
#[derive(Debug, Clone, Copy)]
struct LinkInfo {
    output_port: u32,
    input_port: u32,
    /// owner of `output_port`; cached to avoid walking `ports_by_node`.
    output_node: u32,
    /// owner of `input_port`.
    input_node: u32,
}

/// per-stream routing intent recorded by `try_route_stream`, resolved
/// by `apply_pending_routes` once both port sets are on the registry.
#[derive(Debug, Clone)]
struct PendingRoute {
    /// target sink `node.name` (`headroom-processed` or real-sink name).
    target_sink_name: String,
    app_label: String,
    route: Route,
}

/// daemon-owned routing links for one stream + the target they were
/// built for. keeping them alive lets `enqueue_route` skip a
/// destroy+rebuild (and its audio gap) when the target is unchanged.
struct ManagedRoute {
    target_sink_name: String,
    links: Vec<Link>,
}

/// subject for system-wide metadata keys (e.g. `default.audio.sink`).
const METADATA_SUBJECT_GLOBAL: u32 = 0;

/// bus filter `node.name`; WP does not auto-link `pw_filter` nodes.
const FILTER_NODE_NAME: &str = crate::pw::filter::NODE_NAME;

/// per-PipeWire-thread state. proxies aren't `Send`, so they live here
/// behind `Rc<RefCell<_>>` rather than in [`SharedState`].
pub struct RoutingState {
    daemon: SharedState,
    /// `None` until the registry surfaces the `default` metadata.
    default_metadata: Option<Metadata>,
    _default_metadata_listener: Option<MetadataListener>,
    registry: Rc<Registry>,
    core: Core,
    /// global id of `headroom-filter`. routing treats it as both the
    /// sink-side target for the processed monitor and the source-side
    /// target into the real sink; retargeted on default-sink change.
    filter_playback_id: Option<u32>,
    /// `Audio/Sink` node.name → global id.
    sinks_by_name: HashMap<String, u32>,
    /// IPC-originated commands, drained by [`Self::drain_pw_commands`].
    pw_command_rx: Receiver<PwCommand>,
    /// Layer A managed streams, keyed by source node id.
    managed_streams: HashMap<u32, ManagedStream>,
    /// `Port` globals keyed by owning node id; used to build links.
    ports_by_node: HashMap<u32, Vec<PortInfo>>,
    /// port id → owning node id, so `on_global_remove` distinguishes a
    /// port removal from a node removal under PipeWire's id reuse —
    /// the old "retain by port_id across all nodes" pass could wipe a
    /// live node's ports.
    port_owner: HashMap<u32, u32>,
    /// `Link` globals keyed by link global id. see [`LinkInfo`].
    links_by_id: HashMap<u32, LinkInfo>,
    /// outbound link ids per source node.
    outbound_links_by_node: HashMap<u32, Vec<u32>>,
    /// routes declared but not yet linked (ports still arriving).
    /// drained by [`Self::apply_pending_routes`].
    pending_routes: HashMap<u32, PendingRoute>,
    /// daemon-owned `Link` proxies keyed by source node; see
    /// [`ManagedRoute`].
    managed_route_links: HashMap<u32, ManagedRoute>,
    /// `default.audio.sink` re-assertion budget so a hostile WP can't
    /// hot-loop us. `(window_start, attempts)`.
    default_reassertion: Option<(std::time::Instant, u32)>,
    /// `PwNodeInfo` per routable stream, so bypass/profile reapply can
    /// re-run `routing::evaluate` without re-reading PipeWire props.
    known_streams: HashMap<u32, PwNodeInfo>,
    /// owned global per known stream — the Layer A reconciliation path
    /// runs on the drain timer (no live `global`) but `bind` only reads
    /// id + type, which an owned global carries.
    stream_globals: HashMap<u32, GlobalObject<Properties>>,
    /// persisted per-app `user_ceiling`, keyed as `info_app_label`.
    /// without this, an app that respawns a node per track (Strawberry)
    /// reads its inherited daemon-written volume, misreads it as a user
    /// change, and locks the controller at the prior track's cut.
    persisted_ceilings: HashMap<String, f32>,
    /// node proxy + Format listener for the real sink, capturing its
    /// negotiated `audio.rate` (ALSA sinks expose it only via Format).
    real_sink_format_listener: Option<(u32, Node, NodeListener)>,
    /// the bus filter, held here so `PwCommand::RebuildFilter` can swap
    /// it atomically on a rate change.
    bus_filter: Option<crate::pw::filter::Filter>,
    /// slow AGC controller, so a rebuild can rebind it. shared with the
    /// AGC timer in `runtime`; the main loop serialises borrow_mut.
    agc_controller: Option<Rc<RefCell<crate::agc::AgcController>>>,
    /// throttles tap format-error logs off the 5 ms drain
    layer_a_drain_count: u64,
}

/// per-stream Layer A bundle: tap + controller + measurement consumer.
struct ManagedStream {
    /// drop severs the passive link + destroys the tap stream.
    #[allow(dead_code)]
    tap: StreamTap,
    controller: AppLevelController,
    measurement_consumer: Consumer<MeasurementSample>,
    /// bound source `Node` for `Props.channelVolumes` writes; `None` if
    /// the bind failed (controller still runs, writes skipped).
    node: Option<Node>,
    /// param listener: external `channelVolumes` changes (pavucontrol,
    /// hotkey, the app) route through `controller.on_external_change`
    /// (the rule's `DeferPolicy`).
    #[allow(dead_code)]
    node_listener: Option<NodeListener>,
    /// explicit per-channel passive links source → tap; empty until the
    /// drain-timer retry finds both port sets.
    #[allow(dead_code)]
    links: Vec<Link>,
    app_label: String,
    /// last tap format-error total logged for this stream
    last_logged_format_errors: u64,
}

impl RoutingState {
    #[must_use]
    pub fn new(
        daemon: SharedState,
        registry: Rc<Registry>,
        core: Core,
        pw_command_rx: Receiver<PwCommand>,
    ) -> Self {
        Self {
            daemon,
            default_metadata: None,
            _default_metadata_listener: None,
            registry,
            core,
            filter_playback_id: None,
            sinks_by_name: HashMap::new(),
            pw_command_rx,
            managed_streams: HashMap::new(),
            ports_by_node: HashMap::new(),
            port_owner: HashMap::new(),
            links_by_id: HashMap::new(),
            outbound_links_by_node: HashMap::new(),
            pending_routes: HashMap::new(),
            managed_route_links: HashMap::new(),
            default_reassertion: None,
            known_streams: HashMap::new(),
            stream_globals: HashMap::new(),
            persisted_ceilings: HashMap::new(),
            real_sink_format_listener: None,
            bus_filter: None,
            agc_controller: None,
            layer_a_drain_count: 0,
        }
    }

    /// hand over the bus filter + AGC controller so a rate change can
    /// rebuild + rebind both atomically. called once from `runtime`.
    pub fn install_filter_rebuild_handles(
        &mut self,
        filter: crate::pw::filter::Filter,
        agc: Rc<RefCell<crate::agc::AgcController>>,
    ) {
        self.bus_filter = Some(filter);
        self.agc_controller = Some(agc);
    }

    /// drain queued [`PwCommand`]s then run a routing-link enforcement
    /// pass. called by the 50 ms timer in
    /// [`crate::pw::PwContext::run_until_signal`].
    pub fn drain_pw_commands(&mut self, back: &Rc<RefCell<Self>>) {
        while let Ok(cmd) = self.pw_command_rx.try_recv() {
            self.apply_pw_command(cmd, back);
        }
        self.apply_pending_routes();
    }

    fn apply_pw_command(&mut self, cmd: PwCommand, back: &Rc<RefCell<Self>>) {
        match cmd {
            PwCommand::RouteStream {
                node_id,
                to,
                app_label,
            } => {
                let target_name = match to {
                    Route::Processed => PROCESSED_SINK_NAME.to_owned(),
                    Route::Bypass => {
                        let Some(name) = self.daemon.lock().real_sink.name.clone() else {
                            tracing::warn!(
                                node_id,
                                "route.stream bypass requested but no real sink known yet — skipping metadata write"
                            );
                            return;
                        };
                        name
                    }
                };
                self.write_stream_target(node_id, &target_name, &app_label);
                self.enqueue_route(node_id, target_name, app_label, to);
            }
            PwCommand::ReevaluateAll => {
                self.reevaluate_all();
            }
            PwCommand::RebuildFilter { sample_rate } => {
                self.rebuild_filter(sample_rate);
            }
            PwCommand::ReevaluateLayerA => {
                self.reevaluate_layer_a(back);
            }
            PwCommand::LayerAResetDeference { node_id } => {
                if let Some(managed) = self.managed_streams.get_mut(&node_id) {
                    managed.controller.reset_deference();
                    tracing::info!(node_id, "Layer A deference reset");
                } else {
                    tracing::debug!(
                        node_id,
                        "Layer A reset requested for an unmanaged stream — ignoring"
                    );
                }
            }
        }
    }

    /// tear down + recreate the bus filter at `new_sample_rate` and
    /// rebind AGC. posted by the Format listener on a rate mismatch.
    /// ~50–100 ms audio gap during the swap.
    fn rebuild_filter(&mut self, new_sample_rate: u32) {
        let Some(agc) = self.agc_controller.clone() else {
            tracing::warn!(
                new_sample_rate,
                "filter rebuild requested but agc handle not installed yet"
            );
            return;
        };
        let current_rate = self.daemon.lock().filter_sample_rate;
        if current_rate == Some(new_sample_rate) {
            tracing::debug!(
                new_sample_rate,
                "filter rebuild requested but rate is already current — no-op"
            );
            return;
        }
        // snapshot DSP config under the lock; rebuild runs without it.
        let filter_init = {
            let s = self.daemon.lock();
            let effective = s.profiles.effective();
            crate::pw::filter::FilterInit {
                compressor: effective.build_compressor_config(),
                limiter: effective.build_limiter_config(),
                agc: headroom_dsp::AgcGainConfig::default(),
                agc_enabled: effective.agc.enabled,
            }
        };
        tracing::info!(
            old_rate = ?current_rate,
            new_rate = new_sample_rate,
            "rebuilding bus filter at new sample rate"
        );
        // clear the cached filter id BEFORE dropping the old filter:
        // if the new filter's `global_add` ever beats the old one's
        // remove, `try_capture_filter_playback`'s early-exit would skip
        // the new id and never re-link. stale pending entries are
        // harmless (apply_pending_routes no-ops on a missing id).
        let old_filter_id = self.filter_playback_id.take();
        if let Some(id) = old_filter_id {
            self.pending_routes.remove(&id);
            self.managed_route_links.remove(&id);
        }
        // drop the old filter before creating the new one (short silence).
        self.bus_filter = None;
        let bundle = match crate::pw::filter::Filter::create(
            &self.core,
            filter_init,
            new_sample_rate,
        ) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    new_sample_rate,
                    "filter rebuild failed; daemon will run without a filter until the next rate change"
                );
                return;
            }
        };
        {
            let mut s = self.daemon.lock();
            s.filter_control = Some(bundle.control.clone());
            s.filter_sample_rate = Some(bundle.sample_rate);
        }
        agc.borrow_mut().rebind(
            bundle.measurement_consumer,
            bundle.control,
            bundle.sample_rate,
        );
        self.bus_filter = Some(bundle.filter);
        // re-run routing to re-anchor links on the new filter's ports.
        self.reevaluate_all();
    }

    /// true iff the default metadata has been bound.
    #[must_use]
    pub fn has_default_metadata(&self) -> bool {
        self.default_metadata.is_some()
    }

    fn on_global(&mut self, global: &GlobalObject<&DictRef>, back: &Rc<RefCell<Self>>) {
        match &global.type_ {
            ObjectType::Metadata => self.try_bind_default_metadata(global, back),
            ObjectType::Node => {
                self.try_capture_processed_sink_id(global);
                self.try_capture_real_sink(global);
                self.try_capture_filter_playback(global);
                self.try_route_stream(global, back);
            }
            ObjectType::Port => self.try_capture_port(global),
            ObjectType::Link => self.try_capture_link(global),
            _ => {}
        }
    }

    /// track a Link global + vigilance check: if it originates from a
    /// stream we route and lands off its declared target, destroy it
    /// now. WP links streams faster than our node callback fires, so
    /// this fast path backs up the slower `apply_pending_routes` retry.
    fn try_capture_link(&mut self, global: &GlobalObject<&DictRef>) {
        let Some(props) = &global.props else {
            tracing::debug!(link_id = global.id, "link global without props");
            return;
        };
        let dict: &DictRef = props;
        let parse = |k: &str| dict.get(k).and_then(|s| s.parse::<u32>().ok());
        let (Some(output_port), Some(input_port), Some(output_node), Some(input_node)) = (
            parse("link.output.port"),
            parse("link.input.port"),
            parse("link.output.node"),
            parse("link.input.node"),
        ) else {
            tracing::debug!(
                link_id = global.id,
                out_port = ?parse("link.output.port"),
                in_port = ?parse("link.input.port"),
                out_node = ?parse("link.output.node"),
                in_node = ?parse("link.input.node"),
                "link global with incomplete props"
            );
            return;
        };

        let info = LinkInfo {
            output_port,
            input_port,
            output_node,
            input_node,
        };
        tracing::debug!(
            link_id = global.id,
            output_port,
            input_port,
            output_node,
            input_node,
            "captured link global"
        );
        self.links_by_id.insert(global.id, info);
        self.outbound_links_by_node
            .entry(output_node)
            .or_default()
            .push(global.id);

        self.enforce_link_for_managed_stream(global.id, &info);
    }

    /// destroy `link` if it leaves a routed stream for a *different*
    /// Audio/Sink than the declared target. links to non-sinks (Layer A
    /// taps) are left alone — Layer A owns those.
    fn enforce_link_for_managed_stream(&mut self, link_id: u32, info: &LinkInfo) {
        let intent = self.intent_for_node(info.output_node);
        let Some((target_sink_node_id, target_input_ports)) = intent else {
            return;
        };
        if info.input_node == target_sink_node_id
            && target_input_ports.iter().any(|p| *p == info.input_port)
        {
            return; // link lands on the intended target — keep
        }
        // If the destination isn't one of our routing targets,
        // leave it alone — it's likely a Layer A tap or some
        // other downstream consumer the daemon doesn't own.
        if !self.is_routing_target(info.input_node) {
            return;
        }
        match self.registry.destroy_global(link_id).into_result() {
            Ok(_) => tracing::debug!(
                link_id,
                output_node = info.output_node,
                input_node = info.input_node,
                "destroyed conflicting link for managed stream"
            ),
            Err(e) => tracing::warn!(
                link_id,
                output_node = info.output_node,
                error = ?e,
                "failed to destroy conflicting link"
            ),
        }
    }

    /// resolve a routing-target name to its node id. targets are
    /// `Audio/Sink`s + the bus filter; the filter is special-cased so
    /// `sinks_by_name` stays sink-only.
    fn resolve_routing_target(&self, name: &str) -> Option<u32> {
        if name == FILTER_NODE_NAME {
            return self.filter_playback_id;
        }
        self.sinks_by_name.get(name).copied()
    }

    /// does `node_id` belong to a routing target the daemon links into?
    /// (link-teardown vigilance: leave non-targets like Layer A taps.)
    fn is_routing_target(&self, node_id: u32) -> bool {
        if self.filter_playback_id == Some(node_id) {
            return true;
        }
        self.sinks_by_name.values().any(|&id| id == node_id)
    }

    /// `(target_sink_node_id, target_input_port_ids)` if the daemon
    /// intends to route `source_node`. for the link-vigilance fast path.
    fn intent_for_node(&self, source_node: u32) -> Option<(u32, Vec<u32>)> {
        let target_name = if let Some(p) = self.pending_routes.get(&source_node) {
            p.target_sink_name.clone()
        } else if self.managed_route_links.contains_key(&source_node) {
            let s = self.daemon.lock();
            let entry = s.streams.get(&source_node)?;
            match entry.route {
                Route::Processed => PROCESSED_SINK_NAME.to_owned(),
                Route::Bypass => s.real_sink.name.clone()?,
            }
        } else {
            return None;
        };
        let target_node = self.resolve_routing_target(&target_name)?;
        let target_inputs: Vec<u32> = self
            .ports_by_node
            .get(&target_node)?
            .iter()
            .filter(|p| p.direction == PortDirection::In)
            .map(|p| p.port_id)
            .collect();
        if target_inputs.is_empty() {
            None
        } else {
            Some((target_node, target_inputs))
        }
    }

    fn try_capture_port(&mut self, global: &GlobalObject<&DictRef>) {
        let Some(props) = &global.props else { return };
        let dict: &DictRef = props;
        let Some(node_id) = dict.get("node.id").and_then(|s| s.parse::<u32>().ok()) else {
            return;
        };
        let direction = match dict.get("port.direction") {
            Some("in") => PortDirection::In,
            Some("out") => PortDirection::Out,
            _ => return,
        };
        let ordinal = dict.get("port.id").and_then(|s| s.parse::<u32>().ok());
        let info = PortInfo {
            port_id: global.id,
            direction,
            ordinal,
        };
        let entry = self.ports_by_node.entry(node_id).or_default();
        entry.retain(|p| p.port_id != info.port_id);
        entry.push(info);
        // owning node so `on_global_remove` distinguishes port vs node
        // removal under id reuse.
        self.port_owner.insert(global.id, node_id);
    }

    /// record non-processed `Audio/Sink` nodes in `sinks_by_name` and,
    /// if the name matches the real sink, populate its node_id.
    ///
    /// **cold-boot fallback (F4):** with no real sink known yet, adopt
    /// the first non-processed sink, else `real_sink.name` stays `None`
    /// forever and bypass routes log "no real sink known". a later
    /// `default.audio.sink` event refines it via `adopt_new_real_sink`.
    fn try_capture_real_sink(&mut self, global: &GlobalObject<&DictRef>) {
        let Some(props) = &global.props else { return };
        let dict: &DictRef = props;
        if dict.get("media.class") != Some("Audio/Sink") {
            return;
        }
        let Some(name) = dict.get("node.name") else {
            return;
        };
        if name == PROCESSED_SINK_NAME {
            return; // tracked elsewhere
        }
        let rate = dict.get("audio.rate").and_then(|s| s.parse::<u32>().ok());
        self.sinks_by_name.insert(name.to_owned(), global.id);
        let mut became_real_sink = false;
        {
            let mut s = self.daemon.lock();
            if s.real_sink.name.is_none() {
                tracing::info!(
                    node_id = global.id,
                    name,
                    ?rate,
                    "no preferred_real_sink yet; adopting first available Audio/Sink as fallback"
                );
                s.real_sink.name = Some(name.to_owned());
                s.real_sink.node_id = Some(global.id);
                s.real_sink.sample_rate = rate;
                // adopted sink is "ready" (matches the metadata path);
                // else status/GUI report it not-ready while linked.
                s.real_sink.ready = true;
                became_real_sink = true;
            } else if s.real_sink.name.as_deref() == Some(name) {
                // re-sighting the current real sink.
                s.real_sink.ready = true;
                if s.real_sink.node_id != Some(global.id) {
                    tracing::info!(
                        node_id = global.id,
                        name,
                        "resolved preferred_real_sink node id"
                    );
                    s.real_sink.node_id = Some(global.id);
                    became_real_sink = true;
                }
                // refresh rate on re-sight; ALSA sinks first register
                // without `audio.rate` (filled by the Format listener).
                if rate.is_some() && s.real_sink.sample_rate != rate {
                    tracing::info!(
                        node_id = global.id,
                        name,
                        old_rate = ?s.real_sink.sample_rate,
                        new_rate = ?rate,
                        "real sink rate updated"
                    );
                    s.real_sink.sample_rate = rate;
                }
            }
        }
        // ALSA sinks carry `audio.rate` only in the Format param, not
        // the props dict; subscribe to pull it. only for the current
        // real sink so we don't accumulate proxies.
        if became_real_sink {
            self.install_real_sink_format_listener(global);
            // F4 can surface the sink after filter capture (which then
            // skipped the output leg); pin now — order-independent with
            // `try_capture_filter_playback`.
            self.pin_filter_to_real_sink();
        }
    }

    /// bind `sink_global` and subscribe to its `Format` param (the
    /// initial value is replayed on subscribe) to track
    /// `real_sink.sample_rate`. replaces a listener on a different node.
    fn install_real_sink_format_listener(&mut self, sink_global: &GlobalObject<&DictRef>) {
        let node_id = sink_global.id;
        if let Some((prev_id, _, _)) = &self.real_sink_format_listener {
            if *prev_id == node_id {
                return; // already bound, nothing to do
            }
        }
        let node = match self.registry.bind::<Node, _>(sink_global) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    node_id,
                    error = %e,
                    "failed to bind real sink Node proxy; sample rate will fall back to default"
                );
                self.real_sink_format_listener = None;
                return;
            }
        };
        let daemon = self.daemon.clone();
        let listener = node
            .add_listener_local()
            .param(move |_seq, id, _index, _next, param_opt| {
                if id != ParamType::Format {
                    return;
                }
                let Some(param) = param_opt else { return };
                let Some(rate) = extract_audio_rate(param) else {
                    return;
                };
                let (need_rebuild, tx) = {
                    let mut s = daemon.lock();
                    if s.real_sink.sample_rate == Some(rate) {
                        return;
                    }
                    tracing::info!(
                        node_id,
                        old_rate = ?s.real_sink.sample_rate,
                        new_rate = rate,
                        "real sink Format negotiated; updating sample_rate"
                    );
                    s.real_sink.sample_rate = Some(rate);
                    // filter on a different rate → ask for a rebuild.
                    let need = s.filter_sample_rate != Some(rate);
                    (need, s.pw_command_tx.clone())
                };
                if !need_rebuild {
                    return;
                }
                let Some(tx) = tx else {
                    tracing::debug!(
                        "no PipeWire command channel; filter rebuild deferred (test mode?)"
                    );
                    return;
                };
                if tx
                    .send(PwCommand::RebuildFilter { sample_rate: rate })
                    .is_err()
                {
                    tracing::warn!(
                        "PipeWire command channel closed; filter rate-match lost"
                    );
                }
            })
            .register();
        node.subscribe_params(&[ParamType::Format]);
        self.real_sink_format_listener = Some((node_id, node, listener));
    }

    /// capture `headroom-filter`'s global id. match on `node.name`
    /// alone — `pw_filter` publishes no `Stream/*` media class.
    fn try_capture_filter_playback(&mut self, global: &GlobalObject<&DictRef>) {
        if self.filter_playback_id.is_some() {
            return;
        }
        let Some(props) = &global.props else { return };
        let dict: &DictRef = props;
        if dict.get("node.name") != Some(FILTER_NODE_NAME) {
            return;
        }
        tracing::info!(node_id = global.id, "captured bus filter node id");
        self.filter_playback_id = Some(global.id);
        // not registered in `sinks_by_name` (that map is sink-only);
        // resolved via `resolve_routing_target`/`is_routing_target`.

        // both link legs are enqueued idempotently; whichever of the
        // filter / processed-sink / real-sink surfaces last wins the
        // race. (the output leg via `pin_filter_to_real_sink` is also
        // called from the real-sink adoption paths.)
        self.enqueue_filter_input_link();
        self.pin_filter_to_real_sink();
    }

    /// enqueue the `filter → real_sink` output link once both the
    /// filter and a real sink are known. idempotent — callable from any
    /// site that learns one half, regardless of arrival order; the
    /// intent survives until the (maybe-suspended) sink's ports surface.
    fn pin_filter_to_real_sink(&mut self) {
        let Some(filter_id) = self.filter_playback_id else {
            return;
        };
        let Some(name) = self.daemon.lock().real_sink.name.clone() else {
            return;
        };
        self.enqueue_route(filter_id, name, FILTER_NODE_NAME.to_owned(), Route::Bypass);
    }

    /// enqueue the `processed.monitor → filter.in.*` link pair. source
    /// = processed sink (its `Out` ports = the monitor); target = the
    /// filter's `In` ports. paired by ordinal in `apply_pending_routes`.
    fn enqueue_filter_input_link(&mut self) {
        let processed_id = match self.daemon.lock().processed_sink_id {
            Some(id) => id,
            None => {
                tracing::debug!(
                    "filter input link deferred: processed sink id not yet captured"
                );
                return;
            }
        };
        self.enqueue_route(
            processed_id,
            FILTER_NODE_NAME.to_owned(),
            "headroom-processed.monitor".to_owned(),
            Route::Processed,
        );
    }

    fn try_bind_default_metadata(
        &mut self,
        global: &GlobalObject<&DictRef>,
        back: &Rc<RefCell<Self>>,
    ) {
        if self.default_metadata.is_some() {
            return;
        }
        let Some(props) = &global.props else { return };
        let dict: &DictRef = props;
        if dict.get("metadata.name") != Some("default") {
            return;
        }
        let md = match self.registry.bind::<Metadata, _>(global) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "failed to bind default metadata");
                return;
            }
        };
        tracing::info!(global_id = global.id, "bound default metadata");
        // install the listener BEFORE our promotion write so it sees the
        // prior server state then our write, in order.
        let listener_back = back.clone();
        let listener = md
            .add_listener_local()
            .property(move |subject, key, _type, value| {
                // re-entrancy invariant: callbacks are serial on this loop
                // no sync pipewire round-trips while this `RefCell` borrow lives
                listener_back.borrow_mut().on_metadata_property(subject, key, value);
                0
            })
            .register();
        self.default_metadata = Some(md);
        self._default_metadata_listener = Some(listener);
        // promote headroom-processed as the system default.
        self.write_default_audio_sink(PROCESSED_SINK_NAME);
    }

    fn try_capture_processed_sink_id(&mut self, global: &GlobalObject<&DictRef>) {
        let Some(props) = &global.props else { return };
        let dict: &DictRef = props;
        if dict.get("node.name") != Some(PROCESSED_SINK_NAME) {
            return;
        }
        let mut s = self.daemon.lock();
        if s.processed_sink_id != Some(global.id) {
            tracing::info!(node_id = global.id, "captured headroom-processed node id");
            s.processed_sink_id = Some(global.id);
        }
        drop(s);
        // expose in `sinks_by_name` so routing resolves it to a node id.
        self.sinks_by_name
            .insert(PROCESSED_SINK_NAME.to_owned(), global.id);
        // registry replay order isn't guaranteed; if the filter came
        // first, retry the monitor → filter.in.* enqueue now.
        if self.filter_playback_id.is_some() {
            self.enqueue_filter_input_link();
        }
    }

    fn try_route_stream(
        &mut self,
        global: &GlobalObject<&DictRef>,
        back: &Rc<RefCell<Self>>,
    ) {
        let Some(props) = &global.props else { return };
        let dict: &DictRef = props;
        if dict.get("media.class") != Some("Stream/Output/Audio") {
            return;
        }
        // don't route the daemon's own streams (filter + taps) back in
        // — feedback loop. `node.dont-move` isn't always in the registry
        // view, so match on name prefix too.
        if dict
            .get("node.name")
            .is_some_and(|n| n.starts_with("headroom-filter") || n.starts_with("headroom-tap"))
        {
            tracing::trace!(node_id = global.id, "skipping headroom-internal stream");
            return;
        }

        let info = build_node_info(global.id, dict);

        // cache before routing so reapply paths can re-`evaluate`
        // without re-reading props; owned global lets the Layer A
        // reconcile path bind without a live callback. cleared in
        // `on_global_remove`.
        self.known_streams.insert(info.node_id, info.clone());
        self.stream_globals.insert(info.node_id, global.to_owned());

        let app_label = info_app_label(&info);
        self.apply_bus_route(&info, &app_label);

        // Layer A is orthogonal to the bus route — taps the source
        // wherever it routes.
        self.maybe_spawn_layer_a(&info, &app_label, back);
    }

    /// apply the bus-routing decision for `info`. reads bypass +
    /// profile + real-sink name at call time, then enqueues a route or
    /// unmanages. no proxies touched while the lock is held.
    fn apply_bus_route(&mut self, info: &PwNodeInfo, app_label: &str) {
        let (decision, real_sink_name) = {
            let s = self.daemon.lock();
            let bypass = s.profiles.bypass_global();
            let d = routing::evaluate(info, s.profiles.effective(), bypass);
            (d, s.real_sink.name.clone())
        };

        match decision {
            RoutingDecision::Route(Route::Processed) => {
                self.write_stream_target(info.node_id, PROCESSED_SINK_NAME, app_label);
                self.enqueue_route(
                    info.node_id,
                    PROCESSED_SINK_NAME.to_owned(),
                    app_label.to_owned(),
                    Route::Processed,
                );
                self.record_route(info.node_id, app_label.to_owned(), Route::Processed);
            }
            RoutingDecision::Route(Route::Bypass) => {
                if let Some(name) = real_sink_name.as_deref() {
                    self.write_stream_target(info.node_id, name, app_label);
                    self.enqueue_route(
                        info.node_id,
                        name.to_owned(),
                        app_label.to_owned(),
                        Route::Bypass,
                    );
                } else {
                    // no real sink known yet (early boot); record the
                    // route, leave the stream at PipeWire's default.
                    tracing::warn!(
                        node_id = info.node_id,
                        app = app_label,
                        "bypass route with no known real sink — leaving stream at PipeWire default"
                    );
                }
                self.record_route(info.node_id, app_label.to_owned(), Route::Bypass);
            }
            RoutingDecision::Skip => {
                // not a managed bus stream; drop its links + intent,
                // leave Layer A alone.
                tracing::trace!(node_id = info.node_id, "skip (not routable)");
                self.unmanage(info.node_id);
            }
        }
    }

    /// tear down bus-routing state for `node_id` (links via
    /// `object.linger = "false"`, pending intent, `state.streams`).
    /// Layer A entries untouched — keyed on source node, own lifecycle.
    fn unmanage(&mut self, node_id: u32) {
        self.pending_routes.remove(&node_id);
        self.managed_route_links.remove(&node_id);
        let mut s = self.daemon.lock();
        if s.streams.remove(&node_id).is_some() {
            tracing::debug!(node_id, "bus route unmanaged");
        }
    }

    /// re-apply routing policy to every known stream (one
    /// `routing::evaluate` each). also re-asserts `default.audio.sink`
    /// for the current bypass state — what makes "bypass on" a real
    /// kill switch for apps that follow `default` rather than
    /// `target.object`.
    fn reevaluate_all(&mut self) {
        let (bypass, real_sink_name) = {
            let s = self.daemon.lock();
            (s.profiles.bypass_global(), s.real_sink.name.clone())
        };
        match (bypass, real_sink_name.as_deref()) {
            (true, Some(name)) => {
                tracing::info!(
                    sink = name,
                    "bypass on: setting default.audio.sink to real sink"
                );
                self.write_default_audio_sink(name);
            }
            (true, None) => {
                tracing::warn!(
                    "bypass on but no real sink known yet — leaving default.audio.sink alone"
                );
            }
            (false, _) => {
                // unconditional write (not the rate-limited reassert):
                // an explicit operator action, not a fight with WP.
                self.write_default_audio_sink(PROCESSED_SINK_NAME);
            }
        }

        let snapshot: Vec<PwNodeInfo> = self.known_streams.values().cloned().collect();
        tracing::info!(streams = snapshot.len(), "reevaluating all known streams");
        for info in snapshot {
            let app_label = info_app_label(&info);
            self.apply_bus_route(&info, &app_label);
        }
    }

    /// spawn a Layer A tap + controller if the stream matches an
    /// enabled `[[per_app.rules]]` entry. no-op if already managed or
    /// unmatched; every no-op path logs *why* at debug. wiring is in
    /// [`Self::spawn_layer_a`].
    fn maybe_spawn_layer_a(
        &mut self,
        info: &PwNodeInfo,
        app_label: &str,
        back: &Rc<RefCell<Self>>,
    ) {
        if self.managed_streams.contains_key(&info.node_id) {
            tracing::debug!(
                node_id = info.node_id,
                app = app_label,
                "Layer A spawn skipped: already managed"
            );
            return;
        }
        let eval = {
            let s = self.daemon.lock();
            app_level::evaluate(info, &s.profiles.effective().per_app)
        };
        let rule = match eval {
            LayerAEval::Spawn(rule) => rule,
            other => {
                tracing::debug!(
                    node_id = info.node_id,
                    app = app_label,
                    reason = other.skip_reason(),
                    "Layer A spawn skipped"
                );
                return;
            }
        };
        self.spawn_layer_a(info.node_id, rule, app_label, back);
    }

    /// create the tap, bind the source node (from the cached owned
    /// global, so both the callback + reconcile paths can call this),
    /// restore any persisted ceiling, register the managed stream.
    fn spawn_layer_a(
        &mut self,
        node_id: u32,
        rule: crate::profile::PerAppRule,
        app_label: &str,
        back: &Rc<RefCell<Self>>,
    ) {
        let block_dt_s = layer_a_block_dt_s(self.daemon.lock().real_sink.sample_rate);
        let (tap, consumer) = match StreamTap::start(&self.core, node_id) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(
                    node_id,
                    app = app_label,
                    error = %e,
                    "Layer A tap start failed; stream will be left unmanaged (retry next drain)"
                );
                return;
            }
        };
        let mut controller = AppLevelController::new(rule, block_dt_s);

        // restore any persisted user_ceiling for this app; lookup
        // before bind keeps the ordering below clean.
        let persisted_ceiling = if app_label.is_empty() {
            None
        } else {
            self.persisted_ceilings.get(app_label).copied()
        };
        if let Some(ceiling) = persisted_ceiling {
            controller.restore_state(ceiling, std::time::Instant::now());
            tracing::debug!(
                node_id,
                app = app_label,
                ceiling,
                "Layer A: restored persisted user_ceiling for new instance"
            );
        }

        // bind → install_listener → write baseline → subscribe(Props),
        // in that order. writing a known baseline (persisted ceiling or
        // unity) BEFORE subscribing means the Props echo equals the
        // controller's `last_written_lin`, so the echo check ignores it
        // instead of misreading a stale/inherited value as a user
        // gesture and locking the ceiling (the "hangs until I tweak it
        // again" bug on re-enable). bind failure still registers the tap.
        let baseline = persisted_ceiling.unwrap_or(1.0);
        let (node, node_listener) = match self.stream_globals.get(&node_id) {
            Some(global) => match self.registry.bind::<Node, _>(global) {
                Ok(n) => {
                    let listener = install_param_listener(&n, node_id, back);
                    write_channel_volumes(&n, baseline);
                    n.subscribe_params(&[ParamType::Props]);
                    (Some(n), Some(listener))
                }
                Err(e) => {
                    tracing::warn!(
                        node_id,
                        error = %e,
                        "Layer A: failed to bind Node proxy; volume writes + deference will be skipped"
                    );
                    (None, None)
                }
            },
            None => {
                tracing::warn!(
                    node_id,
                    "Layer A: no cached global for source node; volume writes + deference skipped"
                );
                (None, None)
            }
        };
        self.managed_streams.insert(
            node_id,
            ManagedStream {
                tap,
                controller,
                measurement_consumer: consumer,
                node,
                node_listener,
                links: Vec::new(),
                app_label: app_label.to_owned(),
                last_logged_format_errors: 0,
            },
        );
        tracing::info!(node_id, app = app_label, "Layer A tap spawned");
        if let Ok(event) = Event::new(
            Topic::Routing,
            "layer_a_attached",
            &json!({ "node_id": node_id, "app": app_label }),
        ) {
            self.daemon.lock().broadcaster.publish(Topic::Routing, event);
        }
    }

    /// self-healing: spawn a tap for any known stream that should be
    /// managed but isn't, regardless of why it missed its one
    /// `try_route_stream` spawn. runs every drain so a miss recovers in
    /// one tick.
    fn reconcile_layer_a(&mut self, back: &Rc<RefCell<Self>>) {
        let per_app = self.daemon.lock().profiles.effective().per_app.clone();
        if !per_app.enabled {
            return; // master off
        }
        let candidates: Vec<PwNodeInfo> = self
            .known_streams
            .values()
            .filter(|info| !self.managed_streams.contains_key(&info.node_id))
            .cloned()
            .collect();
        for info in candidates {
            if let LayerAEval::Spawn(rule) = app_level::evaluate(&info, &per_app) {
                let app_label = info_app_label(&info);
                tracing::debug!(
                    node_id = info.node_id,
                    app = app_label.as_str(),
                    "Layer A reconcile: re-attaching a stream that missed its spawn"
                );
                self.spawn_layer_a(info.node_id, rule, &app_label, back);
            }
        }
    }

    /// re-evaluate managed + known streams against `[per_app]`: tear
    /// down taps that no longer match, spawn ones that now do. posted as
    /// `PwCommand::ReevaluateLayerA` by the per-app / master setters.
    fn reevaluate_layer_a(&mut self, back: &Rc<RefCell<Self>>) {
        let per_app = self.daemon.lock().profiles.effective().per_app.clone();
        let managed_ids: Vec<u32> = self.managed_streams.keys().copied().collect();
        for node_id in managed_ids {
            let Some(info) = self.known_streams.get(&node_id).cloned() else {
                continue;
            };
            match app_level::evaluate(&info, &per_app) {
                // still managed: push the (maybe new) rule into the live
                // controller — thresholds change in place, no tap churn,
                // ceiling + deference + envelope preserved.
                LayerAEval::Spawn(rule) => {
                    if let Some(managed) = self.managed_streams.get_mut(&node_id) {
                        managed.controller.set_rule(rule);
                    }
                }
                // no longer managed: release the tap + gain.
                other => {
                    tracing::info!(
                        node_id,
                        reason = other.skip_reason(),
                        "Layer A: stopping per-app management for this stream (per-app disabled \
                         for the app or globally)"
                    );
                    self.teardown_managed_stream(node_id, true);
                }
            }
        }
        self.reconcile_layer_a(back);
    }

    /// restore every managed stream's pre-management volume (ceiling or
    /// unity) on graceful shutdown so attenuated apps aren't left
    /// reduced. best-effort; caller pumps the loop to flush the writes.
    pub fn restore_all_managed_volumes(&self) {
        for (&node_id, managed) in &self.managed_streams {
            let Some(node) = managed.node.as_ref() else {
                continue;
            };
            let restore_to = managed.controller.user_ceiling_lin().unwrap_or(1.0);
            write_channel_volumes(node, restore_to);
            tracing::info!(
                node_id,
                restore_to,
                app = managed.app_label.as_str(),
                "restoring managed stream volume on shutdown"
            );
        }
    }

    /// tear down a managed Layer A stream: optionally restore volume
    /// (so a policy-disable releases the gain), persist the ceiling for
    /// the app's next instance, drop the tap, emit `layer_a_detached`.
    /// `restore_volume = false` from `on_global_remove` (stream gone),
    /// `true` from `reevaluate_layer_a` (policy disable).
    fn teardown_managed_stream(&mut self, node_id: u32, restore_volume: bool) {
        let Some(managed) = self.managed_streams.remove(&node_id) else {
            return;
        };
        if restore_volume {
            if let Some(node) = managed.node.as_ref() {
                let restore_to = managed.controller.user_ceiling_lin().unwrap_or(1.0);
                write_channel_volumes(node, restore_to);
                tracing::debug!(
                    node_id,
                    restore_to,
                    "Layer A: restored stream volume on teardown"
                );
            }
        }
        if let Some(ceiling) = managed.controller.user_ceiling_lin() {
            if !managed.app_label.is_empty() {
                self.persisted_ceilings
                    .insert(managed.app_label.clone(), ceiling);
                tracing::debug!(
                    node_id,
                    app = managed.app_label.as_str(),
                    ceiling,
                    "Layer A: persisted user_ceiling for next instance"
                );
            }
        }
        {
            let mut s = self.daemon.lock();
            s.layer_a.remove(&node_id);
            if let Ok(event) = Event::new(
                Topic::Routing,
                "layer_a_detached",
                &json!({ "node_id": node_id }),
            ) {
                s.broadcaster.publish(Topic::Routing, event);
            }
        }
        tracing::info!(node_id, "Layer A tap torn down");
        drop(managed);
    }

    /// drain each managed stream's measurement ring, advance its
    /// controller, write `Props.channelVolumes` on change. also retries
    /// explicit-link creation. called by the 5 ms timer in
    /// [`crate::pw::PwContext::run_until_signal`].
    pub fn drain_layer_a(&mut self, back: &Rc<RefCell<Self>>) {
        self.attempt_pending_links();

        // 5 ms drain cadence -> about 1 hz
        const FORMAT_ERR_LOG_EVERY: u64 = 200;
        self.layer_a_drain_count = self.layer_a_drain_count.wrapping_add(1);
        let log_format_errors = self.layer_a_drain_count % FORMAT_ERR_LOG_EVERY == 0;

        // collect meter events to emit after the iter_mut borrow drops
        // (broadcaster is behind the daemon mutex; avoid nested borrows).
        let mut meters: Vec<(u32, String, f32, f32)> = Vec::new();
        let mut snapshots: Vec<LayerASnapshot> = Vec::with_capacity(self.managed_streams.len());

        let now = std::time::Instant::now();
        for (&source_node_id, managed) in self.managed_streams.iter_mut() {
            while let Ok(sample) = managed.measurement_consumer.pop() {
                let Some(volume_lin) =
                    managed.controller.process_block(sample.peak, sample.mean_sq, now)
                else {
                    continue;
                };
                let Some(node) = managed.node.as_ref() else {
                    tracing::trace!(
                        target_volume = volume_lin,
                        "Layer A wanted to write volume but no Node proxy was bound"
                    );
                    continue;
                };
                write_channel_volumes(node, volume_lin);
                meters.push((
                    source_node_id,
                    managed.app_label.clone(),
                    volume_lin,
                    managed.controller.smoothed_reduction_db(),
                ));
            }
            // advance envelopes through a silent gap (source suspension,
            // e.g. Strawberry between tracks, stops the audio thread);
            // else the gain freezes at the last value. no-op when
            // measurements flow.
            if let Some(volume_lin) = managed.controller.tick_silent(now) {
                if let Some(node) = managed.node.as_ref() {
                    write_channel_volumes(node, volume_lin);
                    meters.push((
                        source_node_id,
                        managed.app_label.clone(),
                        volume_lin,
                        managed.controller.smoothed_reduction_db(),
                    ));
                }
            }

            if log_format_errors {
                let total = managed.tap.format_error_count();
                let new = total.saturating_sub(managed.last_logged_format_errors);
                if new > 0 {
                    managed.last_logged_format_errors = total;
                    tracing::warn!(
                        source = source_node_id,
                        app = %managed.app_label,
                        new_format_errors = new,
                        total_format_errors = total,
                        "Layer A tap skipped short/misaligned audio buffer(s)"
                    );
                }
            }

            snapshots.push(LayerASnapshot {
                node_id: source_node_id,
                app: managed.app_label.clone(),
                managed: true,
                volume_lin: managed.controller.last_written_lin(),
                reduction_db: managed.controller.smoothed_reduction_db(),
                user_ceiling_lin: managed.controller.user_ceiling_lin(),
                deferred: managed.controller.deferred(),
            });
        }

        {
            let mut s = self.daemon.lock();
            for snap in snapshots {
                s.layer_a.insert(snap.node_id, snap);
            }
            for (node_id, app, volume, reduction_db) in meters {
                if let Ok(event) = Event::new(
                    Topic::Meters,
                    "layer_a_level",
                    &json!({
                        "node_id": node_id,
                        "app": app,
                        "volume_lin": volume,
                        "reduction_db": reduction_db,
                    }),
                ) {
                    s.broadcaster.publish(Topic::Meters, event);
                }
            }
        }

        self.reconcile_layer_a(back);
    }

    /// for each managed stream lacking links, if both port sets are in
    /// the cache, pair by ordinal and create one passive `Link` per
    /// channel. idempotent.
    fn attempt_pending_links(&mut self) {
        // snapshot first so we don't hold &mut self across the
        // link-factory calls (which need &self.core).
        let pending: Vec<(u32, u32)> = self
            .managed_streams
            .iter()
            .filter(|(_, m)| m.links.is_empty())
            .map(|(&src_id, m)| (src_id, m.tap.tap_node_id()))
            .collect();

        for (source_node_id, tap_node_id) in pending {
            if tap_node_id == 0 {
                // Stream isn't bound yet; retry next tick.
                continue;
            }
            let Some(src_outs) = collect_ports(
                &self.ports_by_node,
                source_node_id,
                PortDirection::Out,
            ) else {
                continue;
            };
            let Some(tap_ins) =
                collect_ports(&self.ports_by_node, tap_node_id, PortDirection::In)
            else {
                continue;
            };

            // Stereo only in v0 — pair by per-node ordinal. If counts
            // mismatch, skip and retry (ports may still be arriving).
            if src_outs.len() < 2 || tap_ins.len() < 2 {
                continue;
            }

            let mut created = Vec::with_capacity(2);
            let mut all_ok = true;
            for (out, inp) in src_outs.iter().take(2).zip(tap_ins.iter().take(2)) {
                match create_explicit_link(&self.core, out.port_id, inp.port_id) {
                    Ok(link) => created.push(link),
                    Err(e) => {
                        tracing::warn!(
                            source = source_node_id,
                            tap = tap_node_id,
                            out_port = out.port_id,
                            in_port = inp.port_id,
                            error = %e,
                            "Layer A explicit link creation failed; will retry next tick"
                        );
                        all_ok = false;
                        break;
                    }
                }
            }
            if all_ok && !created.is_empty() {
                if let Some(m) = self.managed_streams.get_mut(&source_node_id) {
                    tracing::info!(
                        source = source_node_id,
                        tap = tap_node_id,
                        links = created.len(),
                        "Layer A explicit passive links created"
                    );
                    m.links = created;
                }
            }
        }
    }

    /// record routing intent for `node_id`; `apply_pending_routes`
    /// later builds the explicit links. last intent wins.
    fn enqueue_route(
        &mut self,
        node_id: u32,
        target_sink_name: String,
        app_label: String,
        route: Route,
    ) {
        // already-correct links to the same sink are left alone (the
        // apply pass no-ops), avoiding the 21-42 ms gap an
        // unconditional rebuild would cost. only drop the proxies when
        // the target actually changed.
        let already_at_target = self
            .managed_route_links
            .get(&node_id)
            .is_some_and(|m| m.target_sink_name == target_sink_name);
        if !already_at_target {
            self.managed_route_links.remove(&node_id);
        }
        self.pending_routes.insert(
            node_id,
            PendingRoute {
                target_sink_name,
                app_label,
                route,
            },
        );
    }

    /// drain `pending_routes`: for each stream whose source + target
    /// ports are both on the registry, tear down conflicting outbound
    /// links and create the explicit ones. not-ready intents stay queued.
    fn apply_pending_routes(&mut self) {
        // snapshot keys: we mutate `managed_route_links` while iterating.
        let candidates: Vec<u32> = self.pending_routes.keys().copied().collect();
        if !candidates.is_empty() {
            tracing::debug!(
                pending = candidates.len(),
                "apply_pending_routes pass"
            );
        }
        for node_id in candidates {
            let Some(intent) = self.pending_routes.get(&node_id).cloned() else {
                continue;
            };

            let Some(target_node) = self.resolve_routing_target(&intent.target_sink_name) else {
                tracing::debug!(
                    node_id,
                    target = intent.target_sink_name.as_str(),
                    "pending route: target not yet on registry"
                );
                continue; // target not yet on registry
            };
            let Some(src_outs) =
                collect_ports(&self.ports_by_node, node_id, PortDirection::Out)
            else {
                tracing::debug!(node_id, "pending route: source has no output ports yet");
                continue;
            };
            let Some(target_ins) =
                collect_ports(&self.ports_by_node, target_node, PortDirection::In)
            else {
                tracing::debug!(node_id, target_node, "pending route: target has no input ports yet");
                continue;
            };
            // pair by ordinal up to the narrower side (all N channels,
            // so surround isn't truncated to stereo). mono (N=1) is
            // intentionally left to WP's upmix adapter — 1→N fanout +
            // stereo-link limiter semantics don't generalise, so
            // `route.set` on a mono app is a hint, not enforcement
            // (v0; fixed in the v1 multichannel pipeline).
            let pair_count = src_outs.len().min(target_ins.len());
            if pair_count < 2 {
                tracing::debug!(
                    node_id,
                    src_outs = src_outs.len(),
                    target_ins = target_ins.len(),
                    "pending route: not enough ports for stereo+ pairing (mono left to WP)"
                );
                continue;
            }
            let want: Vec<(u32, u32)> = src_outs
                .iter()
                .take(pair_count)
                .zip(target_ins.iter().take(pair_count))
                .map(|(o, i)| (o.port_id, i.port_id))
                .collect();
            let want_set: std::collections::HashSet<(u32, u32)> = want.iter().copied().collect();

            // 1) destroy outbound links to a *different* sink; leave
            // non-sink links (Layer A taps) alone.
            let existing: Vec<u32> = self
                .outbound_links_by_node
                .get(&node_id)
                .cloned()
                .unwrap_or_default();
            for link_id in existing {
                let Some(info) = self.links_by_id.get(&link_id).copied() else {
                    continue;
                };
                if want_set.contains(&(info.output_port, info.input_port)) {
                    continue; // already correct — keep
                }
                if !self.is_routing_target(info.input_node) {
                    continue; // probably a Layer A tap or similar
                }
                if let Err(e) = self.registry.destroy_global(link_id).into_result() {
                    tracing::warn!(
                        link_id,
                        node_id,
                        target = intent.target_sink_name.as_str(),
                        error = ?e,
                        "apply_pending_routes: destroy_global failed"
                    );
                }
            }

            // 2) Create any missing wanted links.
            let already_wanted: std::collections::HashSet<(u32, u32)> = self
                .outbound_links_by_node
                .get(&node_id)
                .into_iter()
                .flatten()
                .filter_map(|id| self.links_by_id.get(id))
                .map(|info| (info.output_port, info.input_port))
                .collect();
            let mut created: Vec<Link> = self
                .managed_route_links
                .remove(&node_id)
                .map(|m| m.links)
                .unwrap_or_default();
            let mut all_ok = true;
            for (out_port, in_port) in &want {
                if already_wanted.contains(&(*out_port, *in_port)) {
                    continue;
                }
                match create_routing_link(&self.core, *out_port, *in_port) {
                    Ok(link) => created.push(link),
                    Err(e) => {
                        tracing::warn!(
                            node_id,
                            out_port,
                            in_port,
                            target = intent.target_sink_name.as_str(),
                            error = %e,
                            "apply_pending_routes: create_object failed; retry next tick"
                        );
                        all_ok = false;
                        break;
                    }
                }
            }
            if !created.is_empty() {
                self.managed_route_links.insert(
                    node_id,
                    ManagedRoute {
                        target_sink_name: intent.target_sink_name.clone(),
                        links: created,
                    },
                );
            }
            if all_ok {
                tracing::info!(
                    node_id,
                    app = intent.app_label.as_str(),
                    target = intent.target_sink_name.as_str(),
                    route = intent.route.as_str(),
                    "explicit routing link established"
                );
                self.pending_routes.remove(&node_id);
            }
        }
    }

    /// write `target.object = {"name":"<sink_name>"}` for `node_id`.
    fn write_stream_target(&self, node_id: u32, sink_name: &str, app_label: &str) {
        let Some(md) = &self.default_metadata else {
            tracing::warn!(node_id, "no default metadata bound; cannot apply target.object");
            return;
        };
        let value = format_sink_target_value(sink_name);
        md.set_property(node_id, TARGET_OBJECT_KEY, Some(SPA_JSON_TYPE), Some(&value));
        tracing::info!(node_id, app = app_label, target = sink_name, "routed");
    }

    /// write `default.audio.sink` (subject 0, system-wide).
    fn write_default_audio_sink(&self, sink_name: &str) {
        let Some(md) = &self.default_metadata else {
            tracing::warn!("no default metadata bound; cannot write default.audio.sink");
            return;
        };
        let value = format_sink_target_value(sink_name);
        md.set_property(
            METADATA_SUBJECT_GLOBAL,
            DEFAULT_AUDIO_SINK_KEY,
            Some(SPA_JSON_TYPE),
            Some(&value),
        );
        tracing::info!(sink_name, "wrote default.audio.sink");
    }

    /// handle a `default` metadata property change; only
    /// `default.audio.sink` matters.
    fn on_metadata_property(&mut self, subject: u32, key: Option<&str>, value: Option<&str>) {
        if subject != METADATA_SUBJECT_GLOBAL {
            return;
        }
        if key != Some(DEFAULT_AUDIO_SINK_KEY) {
            return;
        }
        let Some(raw) = value else {
            // key removed; keep the last-known real sink for retargets.
            tracing::warn!("default.audio.sink cleared on server side");
            return;
        };
        let Some(name) = parse_default_sink_name(raw) else {
            tracing::warn!(raw, "failed to parse default.audio.sink value");
            return;
        };
        if name == PROCESSED_SINK_NAME {
            // our own promotion echo.
            tracing::debug!("default.audio.sink is headroom-processed (expected)");
            return;
        }
        self.adopt_new_real_sink(name);
    }

    /// re-assert `default.audio.sink = headroom-processed`, capped at
    /// `MAX_PER_WINDOW` per `WINDOW` so a WP that keeps rewriting our
    /// value back can't hot-loop us. explicit links still enforce
    /// routing whichever side wins the default.
    fn reassert_default_processed(&mut self) {
        // under bypass, stop fighting WP for the default so
        // `default`-following apps land at the real sink — else
        // "bypass on" wouldn't bypass for them.
        if self.daemon.lock().profiles.bypass_global() {
            return;
        }
        const WINDOW: std::time::Duration = std::time::Duration::from_secs(1);
        const MAX_PER_WINDOW: u32 = 10;
        let now = std::time::Instant::now();
        match &mut self.default_reassertion {
            Some((started, n)) if now.duration_since(*started) < WINDOW => {
                if *n >= MAX_PER_WINDOW {
                    tracing::debug!(
                        attempts = *n,
                        "default.audio.sink re-assertion budget exhausted for this window"
                    );
                    return;
                }
                *n += 1;
            }
            _ => {
                self.default_reassertion = Some((now, 1));
            }
        }
        self.write_default_audio_sink(PROCESSED_SINK_NAME);
    }

    /// update `preferred_real_sink`, retarget every bypass-routed
    /// stream + the filter, re-assert headroom-processed as default.
    fn adopt_new_real_sink(&mut self, new_sink_name: String) {
        let (bypass_targets, resolved_node_id) = {
            let mut s = self.daemon.lock();
            let Some(targets) = s.apply_real_sink_change(&new_sink_name) else {
                // sink unchanged, WP just moved default away; re-assert.
                drop(s);
                self.reassert_default_processed();
                return;
            };
            // resolve node_id now if known; else try_capture_real_sink
            // fills it on the next global.
            let resolved = self.sinks_by_name.get(&new_sink_name).copied();
            if let Some(id) = resolved {
                s.real_sink.node_id = Some(id);
            }
            (targets, resolved)
        };
        tracing::info!(
            sink = new_sink_name.as_str(),
            node_id = ?resolved_node_id,
            "preferred_real_sink updated"
        );

        for (node_id, app_label) in &bypass_targets {
            self.write_stream_target(*node_id, &new_sink_name, app_label);
            self.enqueue_route(
                *node_id,
                new_sink_name.clone(),
                app_label.clone(),
                Route::Bypass,
            );
        }
        if !bypass_targets.is_empty() {
            tracing::info!(
                retargeted = bypass_targets.len(),
                sink = new_sink_name.as_str(),
                "retargeted bypass streams"
            );
        }

        // retarget the filter (no `target.object` — we own its
        // linking; WP wouldn't honour it for a pw_filter anyway).
        self.pin_filter_to_real_sink();

        // re-assert headroom-processed as default for new streams.
        self.reassert_default_processed();

        let event = Event::new(
            Topic::Routing,
            "real_sink_changed",
            &json!({ "name": new_sink_name, "node_id": resolved_node_id }),
        );
        if let Ok(event) = event {
            self.daemon.lock().broadcaster.publish(Topic::Routing, event);
        }
    }

    fn record_route(&self, node_id: u32, app: String, route: Route) {
        let mut s = self.daemon.lock();
        s.streams.insert(
            node_id,
            RoutedStream {
                node_id,
                app: app.clone(),
                route,
            },
        );
        if let Ok(event) = Event::new(
            Topic::Routing,
            "stream_routed",
            &json!({
                "node_id": node_id,
                "app": app,
                "to": route.as_str(),
            }),
        ) {
            s.broadcaster.publish(Topic::Routing, event);
        }
    }

    fn on_global_remove(&mut self, node_id: u32) {
        // best-effort cleanup over a mixed id namespace; missing-key
        // removes are harmless. port cleanup is scoped via `port_owner`
        // so a node removal isn't conflated with a port removal under
        // id reuse (the old "retain by port_id across all nodes" could
        // wipe a live node's ports).
        if let Some(owner) = self.port_owner.remove(&node_id) {
            if let Some(ports) = self.ports_by_node.get_mut(&owner) {
                ports.retain(|p| p.port_id != node_id);
                if ports.is_empty() {
                    self.ports_by_node.remove(&owner);
                }
            }
        } else if let Some(ports) = self.ports_by_node.remove(&node_id) {
            for p in ports {
                self.port_owner.remove(&p.port_id);
            }
        }

        // node_id may be a link global, or a node whose links we forget.
        if let Some(info) = self.links_by_id.remove(&node_id) {
            if let Some(v) = self.outbound_links_by_node.get_mut(&info.output_node) {
                v.retain(|&id| id != node_id);
                if v.is_empty() {
                    self.outbound_links_by_node.remove(&info.output_node);
                }
            }
        }
        self.outbound_links_by_node.remove(&node_id);
        self.links_by_id
            .retain(|_, info| info.output_node != node_id && info.input_node != node_id);

        // stream gone — drop intent + cache so reapply doesn't route a
        // dead node.
        self.pending_routes.remove(&node_id);
        self.managed_route_links.remove(&node_id);
        self.known_streams.remove(&node_id);
        self.stream_globals.remove(&node_id);

        if self.filter_playback_id == Some(node_id) {
            tracing::debug!(node_id, "filter playback removed from registry");
            self.filter_playback_id = None;
        }
        self.sinks_by_name.retain(|name, &mut id| {
            if id == node_id {
                tracing::debug!(node_id, name, "real sink removed from registry");
                let mut s = self.daemon.lock();
                // clear BOTH name and node_id: nulling only node_id
                // leaves the name pinned to a dead sink, so every
                // bypass route queues forever against a stale target.
                if s.real_sink.name.as_deref() == Some(name.as_str()) {
                    s.real_sink.name = None;
                    s.real_sink.node_id = None;
                    s.real_sink.sample_rate = None;
                    drop(s);
                    // the Format listener points at a dead node.
                    if let Some((prev_id, _, _)) = &self.real_sink_format_listener {
                        if *prev_id == node_id {
                            self.real_sink_format_listener = None;
                        }
                    }
                } else if s.real_sink.node_id == Some(node_id) {
                    // defensive: id matched but name didn't.
                    s.real_sink.node_id = None;
                }
                false
            } else {
                true
            }
        });
        // node gone, so don't restore volume — just persist the ceiling
        // and tear down the tap.
        self.teardown_managed_stream(node_id, false);
        let mut s = self.daemon.lock();
        let removed = s.streams.remove(&node_id);
        if removed.is_some() {
            tracing::debug!(node_id, "stream removed");
            if let Ok(event) = Event::new(
                Topic::Routing,
                "stream_removed",
                &json!({ "node_id": node_id }),
            ) {
                s.broadcaster.publish(Topic::Routing, event);
            }
        }
    }
}

/// `param` listener forwarding external `channelVolumes` changes to
/// the controller's `on_external_change` — the user-volume-deference
/// loop.
fn install_param_listener(
    node: &Node,
    source_node_id: u32,
    back: &Rc<RefCell<RoutingState>>,
) -> NodeListener {
    let back = back.clone();
    node.add_listener_local()
        .param(move |_seq, id, _index, _next, param_opt| {
            if id != ParamType::Props {
                return;
            }
            let Some(param) = param_opt else { return };
            let Some(new_volume) = extract_channel_volume(param) else {
                return;
            };
            // same re-entrancy invariant as the metadata listener
            let mut state = back.borrow_mut();
            let Some(managed) = state.managed_streams.get_mut(&source_node_id) else {
                return;
            };
            managed.controller.on_external_change(new_volume);
            tracing::debug!(
                source = source_node_id,
                new_volume,
                user_ceiling = ?managed.controller.user_ceiling_lin(),
                deferred = managed.controller.deferred(),
                "Layer A observed external Props.channelVolumes change"
            );
        })
        .register()
}

/// pull `SPA_FORMAT_AUDIO_rate` from a `Format` POD (ALSA sinks expose
/// rate only here, not in the props dict). `None` if absent.
fn extract_audio_rate(pod: &Pod) -> Option<u32> {
    let bytes = pod.as_bytes();
    let (_, value) = PodDeserializer::deserialize_any_from(bytes).ok()?;
    let Value::Object(obj) = value else { return None };
    if obj.id != ParamType::Format.as_raw() {
        return None;
    }
    for prop in obj.properties {
        if prop.key == libspa_sys::SPA_FORMAT_AUDIO_rate {
            if let Value::Int(rate) = prop.value {
                if rate > 0 {
                    return Some(rate as u32);
                }
            }
        }
    }
    None
}

/// pull the first channel of `SPA_PROP_channelVolumes` from a `Props`
/// POD. `None` if absent or the wrong shape.
fn extract_channel_volume(pod: &Pod) -> Option<f32> {
    let bytes = pod.as_bytes();
    let (_, value) = PodDeserializer::deserialize_any_from(bytes).ok()?;
    let Value::Object(obj) = value else { return None };
    if obj.id != ParamType::Props.as_raw() {
        return None;
    }
    for prop in obj.properties {
        if prop.key == libspa_sys::SPA_PROP_channelVolumes {
            if let Value::ValueArray(ValueArray::Float(values)) = prop.value {
                return values.first().copied();
            }
        }
    }
    None
}

/// ports of a given direction owned by `node_id`, sorted by ordinal so
/// pairs line up across source/tap calls.
fn collect_ports(
    cache: &HashMap<u32, Vec<PortInfo>>,
    node_id: u32,
    direction: PortDirection,
) -> Option<Vec<PortInfo>> {
    let ports = cache.get(&node_id)?;
    let mut filtered: Vec<PortInfo> = ports
        .iter()
        .filter(|p| p.direction == direction)
        .cloned()
        .collect();
    if filtered.is_empty() {
        return None;
    }
    // ordinal 0 ↔ FL, 1 ↔ FR (PipeWire convention); port_id breaks
    // ties for ports lacking `port.id`.
    filtered.sort_by_key(|p| (p.ordinal.unwrap_or(u32::MAX), p.port_id));
    Some(filtered)
}

/// passive `link-factory` link with explicit port ids. `passive` so the
/// tap rides alongside without driving the source.
fn create_explicit_link(core: &Core, output_port: u32, input_port: u32) -> Result<Link, pipewire::Error> {
    let out_str = output_port.to_string();
    let in_str = input_port.to_string();
    let props = properties! {
        "link.output.port" => out_str.as_str(),
        "link.input.port" => in_str.as_str(),
        "link.passive" => "true",
        "object.linger" => "false",
    };
    core.create_object::<Link>("link-factory", &props)
}

/// active `link-factory` link that drives the sink — forces routing
/// where WP's `target.object` is unreliable for already-linked streams.
fn create_routing_link(core: &Core, output_port: u32, input_port: u32) -> Result<Link, pipewire::Error> {
    let out_str = output_port.to_string();
    let in_str = input_port.to_string();
    let props = properties! {
        "link.output.port" => out_str.as_str(),
        "link.input.port" => in_str.as_str(),
        "object.linger" => "false",
    };
    core.create_object::<Link>("link-factory", &props)
}

/// write `Props.channelVolumes = [vol, vol]` (stereo) to the node for
/// Layer A attenuation. allocates a POD on the heap; not on the rt
/// thread.
fn write_channel_volumes(node: &Node, volume_lin: f32) {
    let obj = PodObject {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties: vec![Property {
            key: libspa_sys::SPA_PROP_channelVolumes,
            flags: PropertyFlags::empty(),
            value: Value::ValueArray(ValueArray::Float(vec![volume_lin, volume_lin])),
        }],
    };
    let serialised = match PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    ) {
        Ok((cursor, _)) => cursor.into_inner(),
        Err(e) => {
            tracing::warn!(error = %e, "channelVolumes POD serialize failed");
            return;
        }
    };
    let Some(pod) = Pod::from_bytes(&serialised) else {
        tracing::warn!("channelVolumes Pod::from_bytes returned None");
        return;
    };
    node.set_param(ParamType::Props, 0, pod);
    tracing::debug!(volume = volume_lin, "Layer A wrote channelVolumes");
}

fn info_app_label(info: &PwNodeInfo) -> String {
    info.application_process_binary
        .clone()
        .or_else(|| info.application_name.clone())
        .unwrap_or_default()
}

fn build_node_info(node_id: u32, dict: &DictRef) -> PwNodeInfo {
    PwNodeInfo {
        node_id,
        media_class: dict.get("media.class").map(str::to_owned),
        application_process_binary: dict.get("application.process.binary").map(str::to_owned),
        application_name: dict.get("application.name").map(str::to_owned),
        portal_app_id: dict
            .get("pipewire.access.portal.app_id")
            .map(str::to_owned),
        media_role: dict.get("media.role").map(str::to_owned),
        dont_move: dict.get("node.dont-move") == Some("true"),
        audio_channels: dict.get("audio.channels").and_then(|s| s.parse::<u32>().ok()),
    }
}

fn install_listener(registry: &Registry, state: Rc<RefCell<RoutingState>>) -> Listener {
    let state_for_global = state.clone();
    let state_for_remove = state;
    registry
        .add_listener_local()
        .global(move |global| {
            let back = state_for_global.clone();
            state_for_global.borrow_mut().on_global(global, &back);
        })
        .global_remove(move |id| {
            state_for_remove.borrow_mut().on_global_remove(id);
        })
        .register()
}

/// owns the registry, routing state, and listener. drop order matters:
/// listener before registry (field order ensures it).
pub struct RegistryWatcher {
    _listener: Listener,
    state: Rc<RefCell<RoutingState>>,
    _registry: Rc<Registry>,
}

impl RegistryWatcher {
    /// builds the IPC → PipeWire command channel and writes its sender
    /// into `daemon.pw_command_tx` so IPC handlers can post from any
    /// thread.
    pub fn new(registry: Rc<Registry>, core: Core, daemon: SharedState) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<PwCommand>();
        daemon.lock().pw_command_tx = Some(tx);
        let state = Rc::new(RefCell::new(RoutingState::new(
            daemon,
            registry.clone(),
            core,
            rx,
        )));
        let listener = install_listener(&registry, state.clone());
        Self {
            _listener: listener,
            state,
            _registry: registry,
        }
    }

    /// per-thread routing state; mostly for tests + instrumentation.
    #[must_use]
    pub fn state(&self) -> &Rc<RefCell<RoutingState>> {
        &self.state
    }
}
