//! cross-thread command channel from IPC handlers to the pipewire main loop.
//!
//! pipewire proxies are tied to the loop's thread; IPC handlers post a
//! [`PwCommand`] drained by a 50 ms timer source instead. ~50 ms
//! worst-case dispatch — do NOT route anything spike-reactive here
//! (Layer A `channelVolumes`, rt gain reduction); that breaks the §4.5
//! reaction-time contract. filter DSP updates bypass this via
//! [`crate::pw::filter::FilterControl`]'s wait-free rtrb.

use headroom_ipc::Route;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PwCommand {
    /// set `target.object` for one stream, overriding the routing rule.
    RouteStream {
        node_id: u32,
        to: Route,
        app_label: String,
    },
    /// re-run `routing::evaluate` against every known stream. stale post harmless.
    ReevaluateAll,
    /// rebuild the bus filter at a new rate. posted when the real sink's
    /// Format listener sees a rate the filter isn't running at (ALSA
    /// sinks publish rate only via Format, not the props dict → initial
    /// filter is at fallback rate; also sink hot-swap). ~50–100 ms gap.
    RebuildFilter {
        sample_rate: u32,
    },
    /// reconcile Layer A taps against `[per_app]`. stale post harmless.
    ReevaluateLayerA,
    /// clear a managed stream's deference (user-ceiling / strict lock).
    LayerAResetDeference {
        node_id: u32,
    },
}
