//! `headroom-processed` virtual sink (the bus filter captures from its
//! monitor; bypassed streams skip it).

use pipewire::{core::Core, node::Node, properties::properties};

use crate::error::DaemonError;

/// stable, user-visible in `pavucontrol`, `pw-cli list-objects`, etc.
pub const NODE_NAME: &str = "headroom-processed";

pub const NODE_DESCRIPTION: &str = "Headroom (processed)";

pub struct VirtualSink {
    /// holds the sink alive on the server; drop destroys it. `None`
    /// until [`Self::create`].
    proxy: Option<Node>,
}

impl VirtualSink {
    #[must_use]
    pub fn new() -> Self {
        Self { proxy: None }
    }

    /// create the virtual sink. uses the generic `adapter` factory
    /// (always present) wrapping `support.null-audio-sink` (the SPA
    /// factory that yields a null sink with a monitor port).
    ///
    /// `sample_rate` → `audio.rate` so the sink clocks at the real
    /// sink's rate; otherwise it defaults to 48 kHz and the capture
    /// adapter resamples — that buffering across two drivers caused
    /// the per-quantum tremolo seen in soak.
    ///
    /// # Errors
    /// [`DaemonError::PipeWire`] if the server rejects create-object.
    pub fn create(&mut self, core: &Core, sample_rate: u32) -> Result<(), DaemonError> {
        let rate_str = sample_rate.to_string();
        let props = properties! {
            // SPA factory the adapter wraps → null sink with monitor.
            "factory.name" => "support.null-audio-sink",
            "node.name" => NODE_NAME,
            "node.description" => NODE_DESCRIPTION,
            // monitor we can capture from.
            "media.class" => "Audio/Sink",
            // stereo; >2ch bypasses entirely.
            "audio.position" => "FL,FR",
            // lock to real-sink rate — no resampling at monitor → filter.
            "audio.rate" => rate_str.as_str(),
            // follower, not driver: land both filter halves on the
            // real sink's driver.
            "node.passive" => "true",
            "node.suspend-on-idle" => "true",
        };

        let proxy: Node = core
            .create_object("adapter", &props)
            .map_err(|e| DaemonError::pipewire(format!("create_object: {e}")))?;

        self.proxy = Some(proxy);
        tracing::debug!(
            node.name = NODE_NAME,
            "create_object(adapter, factory.name=support.null-audio-sink) queued"
        );
        Ok(())
    }

    #[must_use]
    pub fn is_created(&self) -> bool {
        self.proxy.is_some()
    }
}

impl Default for VirtualSink {
    fn default() -> Self {
        Self::new()
    }
}
