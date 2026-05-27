//! cross-thread daemon state; non-Send pw state stays on the pw thread in `pw::registry`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Sender;
use parking_lot::Mutex;

use headroom_ipc::{LayerASnapshot, Route, SinkInfo};

use crate::ipc::broadcast::Broadcaster;
use crate::profile_store::ProfileStore;
use crate::pw::command::PwCommand;
use crate::pw::filter::FilterControl;

/// per-stream routing decision the daemon has applied (or attempted).
#[derive(Debug, Clone)]
pub struct RoutedStream {
    pub node_id: u32,
    /// `application.process.binary`, else `application.name`, else empty
    pub app: String,
    pub route: Route,
}

/// shared state behind a single mutex. lock held briefly; no nested locks, no awaits.
#[derive(Debug)]
pub struct DaemonState {
    pub started_at: Instant,
    pub profiles: ProfileStore,
    /// global id of `headroom-processed`; `None` until the registry surfaces it
    pub processed_sink_id: Option<u32>,
    /// rate the filter runs at. matches the real sink's native rate at last (re)build; drives
    /// status reporting + layer a's block-period. `None` only at very early boot.
    pub filter_sample_rate: Option<u32>,
    /// user's preferred hardware sink, kept fresh from `default.audio.sink`
    pub real_sink: SinkInfo,
    pub streams: HashMap<u32, RoutedStream>,
    /// layer a controller state mirrored from the pipewire thread's `managed_streams` each drain
    /// pass, so ipc threads can read it without reaching into the `Rc<RefCell>` pw-thread state.
    /// keyed by source node id; removed on teardown.
    pub layer_a: HashMap<u32, LayerASnapshot>,
    pub broadcaster: Broadcaster,
    /// `None` between startup and `Filter::create`, and in tests with no audio path. cloned under
    /// the daemon lock then dropped before pushing, so the lock is never held during an
    /// audio-thread hand-off.
    pub filter_control: Option<FilterControl>,
    /// commands that must run on the pipewire main-loop thread (e.g. `route.stream` metadata
    /// writes). `None` until `start_routing`. cloned + dropped before send so the lock is never
    /// held while crossbeam pushes.
    pub pw_command_tx: Option<Sender<PwCommand>>,
}

impl DaemonState {
    #[must_use]
    pub fn new(profiles: ProfileStore) -> Self {
        Self {
            started_at: Instant::now(),
            profiles,
            processed_sink_id: None,
            filter_sample_rate: None,
            real_sink: SinkInfo::default(),
            streams: HashMap::new(),
            layer_a: HashMap::new(),
            broadcaster: Broadcaster::new(),
            filter_control: None,
            pw_command_tx: None,
        }
    }

    /// apply a `default.audio.sink` change to `real_sink`, returning bypass-routed streams that
    /// need their `target.object` rewritten to follow the new sink. `None` on idempotent no-op.
    /// touches only in-memory state, so it's safe under the lock (pw writes happen after unlock).
    pub fn apply_real_sink_change(&mut self, new_name: &str) -> Option<Vec<(u32, String)>> {
        if self.real_sink.name.as_deref() == Some(new_name) {
            return None;
        }
        self.real_sink = SinkInfo {
            // node_id + sample_rate stay unknown; `try_capture_real_sink` resolves both when it
            // sees the matching `Audio/Sink` global. the routing path operates on name alone.
            node_id: None,
            name: Some(new_name.to_owned()),
            ready: true,
            sample_rate: None,
        };
        Some(
            self.streams
                .values()
                .filter(|r| r.route == Route::Bypass)
                .map(|r| (r.node_id, r.app.clone()))
                .collect(),
        )
    }
}

pub type SharedState = Arc<Mutex<DaemonState>>;

#[must_use]
pub fn shared(state: DaemonState) -> SharedState {
    Arc::new(Mutex::new(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store::ProfileStore;

    fn state() -> DaemonState {
        DaemonState::new(ProfileStore::builtin())
    }

    fn add_stream(s: &mut DaemonState, node_id: u32, app: &str, route: Route) {
        s.streams.insert(
            node_id,
            RoutedStream {
                node_id,
                app: app.into(),
                route,
            },
        );
    }

    #[test]
    fn apply_real_sink_change_first_time_returns_empty_retarget_list() {
        let mut s = state();
        let to_retarget = s.apply_real_sink_change("alsa_output.usb-foo").unwrap();
        assert!(to_retarget.is_empty(), "no streams yet — nothing to retarget");
        assert_eq!(s.real_sink.name.as_deref(), Some("alsa_output.usb-foo"));
        assert!(s.real_sink.ready);
    }

    #[test]
    fn apply_real_sink_change_returns_bypass_streams_only() {
        let mut s = state();
        // Seed: two streams routed, one bypass, one processed.
        add_stream(&mut s, 100, "mpv", Route::Bypass);
        add_stream(&mut s, 101, "firefox", Route::Processed);
        let mut retarget = s.apply_real_sink_change("alsa_output.usb-foo").unwrap();
        retarget.sort_by_key(|(id, _)| *id);
        assert_eq!(retarget.len(), 1);
        assert_eq!(retarget[0].0, 100);
        assert_eq!(retarget[0].1, "mpv");
    }

    #[test]
    fn apply_real_sink_change_idempotent_on_same_name() {
        let mut s = state();
        add_stream(&mut s, 100, "mpv", Route::Bypass);
        assert!(s.apply_real_sink_change("alsa_output.usb-foo").is_some());
        assert!(s.apply_real_sink_change("alsa_output.usb-foo").is_none());
    }

    #[test]
    fn apply_real_sink_change_returns_targets_on_subsequent_switches() {
        let mut s = state();
        add_stream(&mut s, 100, "mpv", Route::Bypass);
        add_stream(&mut s, 101, "ardour", Route::Bypass);
        s.apply_real_sink_change("speakers").unwrap();
        let mut t = s.apply_real_sink_change("headphones").unwrap();
        t.sort_by_key(|(id, _)| *id);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].0, 100);
        assert_eq!(t[1].0, 101);
        assert_eq!(s.real_sink.name.as_deref(), Some("headphones"));
    }
}
