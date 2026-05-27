//! top-level orchestrator: wires pw layer, ipc, agc, routing; runs the main loop

use std::cell::RefCell;
use std::rc::Rc;

use headroom_ipc::{Event, Topic};
use serde_json::json;

use crate::agc::{AgcController, AGC_TICK};
use crate::error::DaemonError;
use crate::ipc::IpcServer;
use crate::profile_store::ProfileStore;
use crate::profile_watcher::ProfileWatcher;
use crate::pw::filter::{Filter, FilterBundle, FilterInit};
use crate::pw::{block_termination_signals, PwContext};
use crate::state::{self, DaemonState, SharedState};

/// blocks until shutdown (SIGTERM/SIGINT).
pub fn run(profiles: ProfileStore) -> Result<(), DaemonError> {
    // snapshot without draining: status/ipc keeps surfacing until next reload clears them
    let pending_warnings = profiles.warnings();
    let active_missing = profiles.is_active_missing().map(|s| s.to_owned());
    tracing::info!(
        profile = profiles.effective().name.as_str(),
        rules = profiles.effective().rules.len(),
        "starting headroom daemon"
    );
    for w in &pending_warnings {
        tracing::warn!(warning = %w, "profile store warning");
    }
    if let Some(name) = active_missing.as_deref() {
        tracing::warn!(missing = name, "selected profile missing; using built-in default");
    }

    // block SIGTERM/SIGINT process-wide BEFORE spawning any threads, so every thread inherits
    // the blocked mask and pipewire's signalfd is the sole receiver. threads spawned before would
    // keep default disposition and dying on SIGTERM would skip our shutdown path. (the ipc accept
    // thread, added later in startup, tripped this once.)
    block_termination_signals()?;

    // captured before `profiles` moves into shared state
    let watch_dir = profiles.primary_profile_watch_dir();

    let daemon_state = state::shared(DaemonState::new(profiles));

    // ipc first so its accept thread is ready before any pipewire work logs through it.
    // handle's Drop cleans the socket.
    let socket_path = headroom_ipc::default_socket_path()
        .ok_or_else(|| DaemonError::other("no default IPC socket path"))?;
    let _ipc = IpcServer::start(socket_path, daemon_state.clone())?;

    // failure to install is non-fatal: manual `profile.reload` over ipc still works
    let _profile_watcher = match watch_dir {
        Some(dir) => match ProfileWatcher::start(dir, daemon_state.clone()) {
            Ok(watcher) => watcher,
            Err(e) => {
                tracing::warn!(error = %e, "profile file-watcher disabled");
                None
            }
        },
        None => None,
    };

    let pw = PwContext::new()?;
    // processed sink and bus filter must run at the real sink's rate to avoid a rate-conversion
    // stage at the monitor → filter boundary. falls back to 48k if the real sink hasn't surfaced;
    // the Format-param listener rebuilds on the first observed rate change.
    let initial_rate = daemon_state
        .lock()
        .real_sink
        .sample_rate
        .unwrap_or(crate::pw::filter::DEFAULT_SAMPLE_RATE);
    tracing::info!(initial_rate, "creating processed sink + filter at real-sink-matched rate");
    pw.create_processed_sink(initial_rate)?;

    // seed the dsp from the effective profile. FilterControl → DaemonState for live updates;
    // measurement consumer → agc controller.
    let filter_init = {
        let s = daemon_state.lock();
        let effective = s.profiles.effective();
        FilterInit {
            compressor: effective.build_compressor_config(),
            limiter: effective.build_limiter_config(),
            agc: headroom_dsp::AgcGainConfig::default(),
            agc_enabled: effective.agc.enabled,
        }
    };

    let FilterBundle {
        filter,
        control: filter_control,
        measurement_consumer,
        bus_metrics,
        timing,
        sample_rate: filter_rate,
    } = Filter::create(pw.core(), filter_init, initial_rate)?;
    {
        let mut s = daemon_state.lock();
        s.filter_control = Some(filter_control.clone());
        s.filter_sample_rate = Some(filter_rate);
    }

    // reads [agc] config each tick so profile.use takes effect next tick
    let agc_controller = AgcController::new(
        filter_rate,
        crate::pw::filter::CHANNELS,
        measurement_consumer,
        filter_control,
        daemon_state.clone(),
        bus_metrics,
        timing,
    )
    .map_err(DaemonError::from)?;
    let agc_controller = Rc::new(RefCell::new(agc_controller));
    let agc_timer = {
        let agc = agc_controller.clone();
        let timer = pw
            .main_loop()
            .loop_()
            .add_timer(move |_| agc.borrow_mut().tick());
        let _ = timer.update_timer(Some(AGC_TICK), Some(AGC_TICK));
        timer
    };

    // matching Stream/Output/Audio nodes get `target.object` via the `default` metadata;
    // wireplumber moves them. bypassed streams point straight at preferred_real_sink the same way.
    pw.start_routing(daemon_state.clone())?;

    // hand the filter + agc handle to routing state so the Format-param listener can ask the
    // registry thread to rebuild at a new rate via PwCommand::RebuildFilter. filter ownership
    // moves here; RoutingState drops it on shutdown via PwContext's drop order.
    if let Some(routing_state) = pw.routing_state() {
        routing_state
            .borrow_mut()
            .install_filter_rebuild_handles(filter, agc_controller.clone());
    } else {
        // start_routing succeeded so this shouldn't fire; keep the filter alive defensively
        tracing::warn!("routing_state unavailable post-start_routing; keeping filter local");
    }

    publish_daemon_started(&daemon_state, &pending_warnings, active_missing.as_deref());

    pw.run_until_signal()?;

    // drop before exiting so they tear down deterministically alongside the pipewire context
    drop(agc_timer);
    drop(agc_controller);

    publish_daemon_shutdown(&daemon_state);

    tracing::info!("headroom daemon stopped");
    Ok(())
}

fn publish_daemon_started(state: &SharedState, warnings: &[String], active_missing: Option<&str>) {
    if let Ok(event) = Event::new(
        Topic::Daemon,
        "started",
        &json!({
            "version": env!("CARGO_PKG_VERSION"),
            "warnings": warnings,
            "active_missing": active_missing,
        }),
    ) {
        state.lock().broadcaster.publish(Topic::Daemon, event);
    }
}

fn publish_daemon_shutdown(state: &SharedState) {
    if let Ok(event) = Event::new(Topic::Daemon, "shutdown", &json!({})) {
        state.lock().broadcaster.publish(Topic::Daemon, event);
    }
}
