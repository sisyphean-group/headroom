//! the bus filter: a single `pw_filter` node sandwiching the DSP chain.

use std::sync::Arc;

use parking_lot::Mutex;
use pipewire::{
    core::Core,
    keys,
    properties::properties,
    spa::{pod::Pod, utils::Direction},
};
use pipewire_filter::{Filter as PwFilter, FilterFlags, FilterListener, PortData, PortFlags};
use rtrb::{Consumer, Producer, RingBuffer};

use headroom_dsp::{
    AgcGain, AgcGainConfig, Compressor, CompressorConfig, Limiter, LimiterConfig, SetConfigOutcome,
};

use crate::error::DaemonError;
use crate::meters::{BusMetrics, SharedBusMetrics, SharedPlaybackTiming};

/// `node.name` the routing engine looks for to wire the explicit
/// monitor → input and output → real-sink links — wireplumber does
/// not auto-link `pw_filter` nodes.
pub const NODE_NAME: &str = "headroom-filter";

/// rate used until a real sink is known; [`Filter::create`] overrides
/// it from the captured real-sink rate.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// back-compat alias for out-of-tree code.
pub const FILTER_SAMPLE_RATE: u32 = DEFAULT_SAMPLE_RATE;

/// stereo only in v0.
pub const CHANNELS: u32 = 2;

const CMD_RING_CAPACITY: usize = 32;

/// audio→AGC measurement ring, interleaved f32. several controller
/// ticks of slack so a stalled controller doesn't drop coverage.
const MEASUREMENT_RING_CAPACITY: usize = 32_768;

/// control-plane param updates to the rt audio thread. structural
/// limiter changes (oversample, lookahead) can't apply live — see
/// [`headroom_dsp::SetConfigOutcome::StructuralChange`].
#[derive(Debug, Clone, Copy)]
pub enum AudioCmd {
    SetCompressor(CompressorConfig),
    SetLimiter(LimiterConfig),
    /// AGC target dB, pushed by the slow controller each tick.
    SetAgcTargetDb(f32),
    /// toggle AGC; when disabled the smoother unwinds to 0 dB.
    SetAgcEnabled(bool),
    SetAgcConfig(AgcGainConfig),
    /// limiter soft-ceiling program loudness; `None` resets
    SetProgramLoudnessLufs(Option<f32>),
}

/// cheap-to-clone handle for pushing [`AudioCmd`]s into the running
/// filter. held in `DaemonState` so any IPC thread can update params.
#[derive(Clone)]
pub struct FilterControl {
    cmd_producer: Arc<Mutex<Producer<AudioCmd>>>,
}

impl FilterControl {
    /// `false` (and a warn) if the ring is full; the command is dropped.
    pub fn try_send(&self, cmd: AudioCmd) -> bool {
        match self.cmd_producer.lock().push(cmd) {
            Ok(()) => true,
            Err(_) => {
                tracing::warn!(
                    "filter command ring full; dropping parameter update — \
                     audio thread may be stalled or commands arriving faster than the quantum"
                );
                false
            }
        }
    }

    pub fn set_compressor(&self, cfg: CompressorConfig) -> bool {
        self.try_send(AudioCmd::SetCompressor(cfg))
    }

    pub fn set_limiter(&self, cfg: LimiterConfig) -> bool {
        self.try_send(AudioCmd::SetLimiter(cfg))
    }

    pub fn set_agc_target_db(&self, db: f32) -> bool {
        self.try_send(AudioCmd::SetAgcTargetDb(db))
    }

    pub fn set_agc_enabled(&self, enabled: bool) -> bool {
        self.try_send(AudioCmd::SetAgcEnabled(enabled))
    }

    pub fn set_agc_config(&self, cfg: AgcGainConfig) -> bool {
        self.try_send(AudioCmd::SetAgcConfig(cfg))
    }

    pub fn set_program_loudness_lufs(&self, lufs: Option<f32>) -> bool {
        self.try_send(AudioCmd::SetProgramLoudnessLufs(lufs))
    }
}

impl std::fmt::Debug for FilterControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterControl").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl FilterControl {
    pub(crate) fn for_testing(capacity: usize) -> (Self, Consumer<AudioCmd>) {
        let (producer, consumer) = RingBuffer::<AudioCmd>::new(capacity);
        (
            Self {
                cmd_producer: Arc::new(Mutex::new(producer)),
            },
            consumer,
        )
    }
}

/// user-data carried into the rt process callback. ports are mono
/// (`format.dsp = "32 bit float mono audio"`), one per channel per
/// direction — canonical `pw_filter` shape; explicit links pair
/// FL/FR port-by-port.
struct FilterState {
    in_l: PortData,
    in_r: PortData,
    out_l: PortData,
    out_r: PortData,
    cmd_consumer: Consumer<AudioCmd>,
    /// pre-AGC input samples for the slow AGC. over-capacity dropped
    /// silently (controller tolerates gaps).
    measurement_producer: Producer<f32>,
    agc: AgcGain,
    compressor: Compressor,
    limiter: Limiter,
    measurement_dropped: u64,
    bus_metrics: SharedBusMetrics,
    timing: SharedPlaybackTiming,
}

/// owns the `pw_filter` node + listener; drop tears down the audio
/// path. drop order is listener-first, filter-second (field order),
/// required by the `pipewire-filter` wrapper.
pub struct Filter {
    _listener: FilterListener<FilterState>,
    _filter: PwFilter,
}

/// initial DSP config for [`Filter::create`].
#[derive(Debug, Clone, Copy)]
pub struct FilterInit {
    pub compressor: CompressorConfig,
    pub limiter: LimiterConfig,
    pub agc: AgcGainConfig,
    pub agc_enabled: bool,
}

pub struct FilterBundle {
    pub filter: Filter,
    pub control: FilterControl,
    pub measurement_consumer: Consumer<f32>,
    pub bus_metrics: SharedBusMetrics,
    pub timing: SharedPlaybackTiming,
    /// rate the filter runs at — captured from the real sink, or
    /// [`DEFAULT_SAMPLE_RATE`] if none known yet.
    pub sample_rate: u32,
}

impl Filter {
    /// create the node, add four mono ports, register the rt callback,
    /// and connect.
    ///
    /// wireplumber does *not* auto-link `pw_filter` nodes; the routing
    /// layer builds the `processed.monitor → filter.in.*` and
    /// `filter.out.* → real_sink` links explicitly. filter sits in
    /// `Paused` until both land.
    ///
    /// # Errors
    /// [`DaemonError::PipeWire`] on filter/port/connect failure.
    pub fn create(
        core: &Core,
        init: FilterInit,
        sample_rate: u32,
    ) -> Result<FilterBundle, DaemonError> {
        let (cmd_producer, cmd_consumer) = RingBuffer::<AudioCmd>::new(CMD_RING_CAPACITY);
        let (measurement_producer, measurement_consumer) =
            RingBuffer::<f32>::new(MEASUREMENT_RING_CAPACITY);
        let control = FilterControl {
            cmd_producer: Arc::new(Mutex::new(cmd_producer)),
        };
        let bus_metrics = crate::meters::shared();
        let timing = crate::meters::shared_timing();

        // caps the internal (post-oversample) rate: 96 kHz base + 4×
        // auto-drops to 2× → 192 kHz internal, not 384 kHz. bounds FIR
        // cost at higher real-sink rates.
        let limiter_cfg = init.limiter.sanitize_for_rate(sample_rate as f32);
        let compressor = Compressor::new(init.compressor, sample_rate as f32);
        let limiter = Limiter::new(limiter_cfg, sample_rate as f32);
        let mut agc = AgcGain::new(init.agc, sample_rate as f32);
        agc.set_enabled(init.agc_enabled);

        let filter = build_filter(core)?;

        let in_l = add_mono_port(&filter, Direction::Input, "input_FL", "FL")?;
        let in_r = add_mono_port(&filter, Direction::Input, "input_FR", "FR")?;
        let out_l = add_mono_port(&filter, Direction::Output, "output_FL", "FL")?;
        let out_r = add_mono_port(&filter, Direction::Output, "output_FR", "FR")?;

        let listener = filter
            .add_local_listener_with_user_data(FilterState {
                in_l,
                in_r,
                out_l,
                out_r,
                cmd_consumer,
                measurement_producer,
                agc,
                compressor,
                limiter,
                measurement_dropped: 0,
                bus_metrics: bus_metrics.clone(),
                timing: timing.clone(),
            })
            .process(|state, _position| process(state))
            .register()
            .map_err(|e| DaemonError::pipewire(format!("filter listener register: {e}")))?;

        // no top-level params; per-port `format.dsp` declares mono F32.
        let mut connect_params: [&Pod; 0] = [];
        filter
            .connect(FilterFlags::RT_PROCESS, &mut connect_params)
            .map_err(|e| DaemonError::pipewire(format!("filter connect: {e}")))?;

        tracing::info!(
            sample_rate,
            channels = CHANNELS,
            "bus filter (pw_filter) created and connected"
        );

        Ok(FilterBundle {
            filter: Self {
                _listener: listener,
                _filter: filter,
            },
            control,
            measurement_consumer,
            bus_metrics,
            timing,
            sample_rate,
        })
    }
}

/// add a mono DSP port. `channel` is `audio.channel` ("FL"/"FR"),
/// which the routing engine pairs on. `format.dsp` declares the
/// format up-front so no SPA POD.
fn add_mono_port(
    filter: &PwFilter,
    direction: Direction,
    port_name: &str,
    channel: &str,
) -> Result<PortData, DaemonError> {
    let props = properties! {
        *keys::FORMAT_DSP => "32 bit float mono audio",
        *keys::PORT_NAME => port_name,
        *keys::AUDIO_CHANNEL => channel,
    };
    let mut params: [&Pod; 0] = [];
    filter
        .add_port(direction, PortFlags::MAP_BUFFERS, props, &mut params)
        .map_err(|e| DaemonError::pipewire(format!("filter add_port ({port_name}): {e}")))
}

/// kept from the dual-stream era so external policy that special-cased
/// it still applies.
const FILTER_LINK_GROUP: &str = "headroom.filter";

/// latency target; pipewire rounds up to `max(this, driver_quantum)`.
/// small value avoids the ~250 ms pipewire picks when unset.
const NODE_LATENCY_HINT: &str = "256/48000";

fn build_filter(core: &Core) -> Result<PwFilter, DaemonError> {
    let props = properties! {
        *keys::MEDIA_TYPE => "Audio",
        *keys::MEDIA_ROLE => "DSP",
        *keys::NODE_NAME => NODE_NAME,
        *keys::NODE_DESCRIPTION => "Headroom bus filter",
        // we own our linking; routing must not move us, wireplumber
        // must not re-target us on default-sink changes.
        *keys::NODE_DONT_RECONNECT => "true",
        "node.dont-move" => "true",
        "node.link-group" => FILTER_LINK_GROUP,
        // the real sink drives, not us.
        "node.passive" => "false",
        *keys::NODE_LATENCY => NODE_LATENCY_HINT,
    };
    PwFilter::new(core, NODE_NAME, props)
        .map_err(|e| DaemonError::pipewire(format!("pw_filter new: {e}")))
}

/// apply one [`AudioCmd`] to the DSP kernels. allocation-free.
fn apply_audio_cmd(
    cmd: AudioCmd,
    compressor: &mut Compressor,
    limiter: &mut Limiter,
    agc: &mut AgcGain,
) {
    match cmd {
        AudioCmd::SetCompressor(cfg) => {
            compressor.set_config(cfg);
        }
        AudioCmd::SetLimiter(cfg) => match limiter.try_set_config(cfg) {
            SetConfigOutcome::Applied => {}
            SetConfigOutcome::StructuralChange => {
                tracing::warn!(
                    "limiter structural change (oversample / lookahead / fir_taps) cannot be \
                     applied live; daemon restart required to pick up the new value"
                );
            }
        },
        AudioCmd::SetAgcTargetDb(db) => {
            agc.set_target_db(db);
        }
        AudioCmd::SetAgcEnabled(enabled) => {
            agc.set_enabled(enabled);
        }
        AudioCmd::SetAgcConfig(cfg) => {
            agc.set_config(cfg);
        }
        AudioCmd::SetProgramLoudnessLufs(Some(lufs)) => {
            limiter.set_program_loudness_lufs(lufs);
        }
        AudioCmd::SetProgramLoudnessLufs(None) => {
            limiter.clear_program_loudness();
        }
    }
}

/// drain control-plane param updates into the DSP kernels. allocation-free.
fn drain_audio_commands(state: &mut FilterState) {
    while let Ok(cmd) = state.cmd_consumer.pop() {
        apply_audio_cmd(
            cmd,
            &mut state.compressor,
            &mut state.limiter,
            &mut state.agc,
        );
    }
}

/// rt process callback. allocation-free, guarded by `assert_no_alloc`
/// in debug builds. timed into [`PlaybackTiming`].
fn process(state: &mut FilterState) {
    let start = std::time::Instant::now();
    assert_no_alloc::assert_no_alloc(|| process_inner(state));
    let dur_us = start.elapsed().as_micros() as u64;
    state.timing.record(dur_us);
}

fn process_inner(state: &mut FilterState) {
    drain_audio_commands(state);

    // any missing buffer this quantum: bail, pipewire re-fires us.
    let Some(mut in_l_buf) = state.in_l.dequeue_buffer() else {
        return;
    };
    let Some(mut in_r_buf) = state.in_r.dequeue_buffer() else {
        return;
    };
    let Some(mut out_l_buf) = state.out_l.dequeue_buffer() else {
        return;
    };
    let Some(mut out_r_buf) = state.out_r.dequeue_buffer() else {
        return;
    };

    let sample_bytes = std::mem::size_of::<f32>();

    let in_l_samples = match read_mono_input(in_l_buf.datas_mut(), &state.timing) {
        Some(s) => s,
        None => return,
    };
    let in_r_samples = match read_mono_input(in_r_buf.datas_mut(), &state.timing) {
        Some(s) => s,
        None => return,
    };
    let in_frames = in_l_samples.len().min(in_r_samples.len());
    if in_frames == 0 {
        return;
    }

    let out_l_datas = out_l_buf.datas_mut();
    let Some(out_l_data) = out_l_datas.first_mut() else {
        return;
    };
    let Some(out_l_bytes) = out_l_data.data() else {
        return;
    };
    let out_l_max = out_l_bytes.len() / sample_bytes;

    let out_r_datas = out_r_buf.datas_mut();
    let Some(out_r_data) = out_r_datas.first_mut() else {
        return;
    };
    let Some(out_r_bytes) = out_r_data.data() else {
        return;
    };
    let out_r_max = out_r_bytes.len() / sample_bytes;

    let frames = in_frames.min(out_l_max).min(out_r_max);
    if frames == 0 {
        return;
    }

    // rt cast failures are logged off-thread
    let out_l_samples: &mut [f32] =
        match bytemuck::try_cast_slice_mut::<u8, f32>(&mut out_l_bytes[..frames * sample_bytes]) {
            Ok(s) => s,
            Err(_) => {
                state.timing.record_format_error();
                return;
            }
        };
    let out_r_samples: &mut [f32] =
        match bytemuck::try_cast_slice_mut::<u8, f32>(&mut out_r_bytes[..frames * sample_bytes]) {
            Ok(s) => s,
            Err(_) => {
                state.timing.record_format_error();
                return;
            }
        };

    let mut measurement_dropped = 0_u64;
    for frame_idx in 0..frames {
        let left_in = in_l_samples[frame_idx];
        let right_in = in_r_samples[frame_idx];
        // feed the slow AGC, best-effort (gaps fine, never block here).
        if state.measurement_producer.push(left_in).is_err()
            || state.measurement_producer.push(right_in).is_err()
        {
            measurement_dropped = measurement_dropped.saturating_add(2);
        }
        let (la, ra) = state.agc.process_frame(left_in, right_in);
        let (lc, rc) = state.compressor.process_frame(la, ra);
        let (lo, ro) = state.limiter.process_frame(lc, rc);
        out_l_samples[frame_idx] = lo;
        out_r_samples[frame_idx] = ro;
    }
    if measurement_dropped > 0 {
        state.measurement_dropped = state.measurement_dropped.saturating_add(measurement_dropped);
    }

    // `frames < in_frames` shouldn't happen (output maxsize is sized
    // by `clock.quantum-limit` ≫ typical quanta); recorded as a
    // regression signal.
    if frames < in_frames {
        let dropped_frames = in_frames - frames;
        state
            .timing
            .record_dropped((dropped_frames * CHANNELS as usize) as u64);
    }

    // `try_lock` so we never block on a daemon-thread reader; a
    // contended quantum drops this update, next one lands.
    if frames > 0 {
        if let Some(mut metrics) = state.bus_metrics.try_lock() {
            *metrics = BusMetrics {
                compressor_gr_db: state.compressor.gain_reduction_db(),
                limiter_total_gr_db: state.limiter.gain_reduction_db(),
                limiter_soft_gr_db: state.limiter.soft_gain_reduction_db(),
                limiter_hard_gr_db: state.limiter.hard_gain_reduction_db(),
                true_peak_dbtp: state.limiter.true_peak_dbtp(),
            };
        }
    }

    for chunk_data in [out_l_data, out_r_data] {
        let chunk = chunk_data.chunk_mut();
        *chunk.size_mut() = (frames * sample_bytes) as u32;
        *chunk.stride_mut() = sample_bytes as i32;
        *chunk.offset_mut() = 0;
    }
}

/// mono input data as f32 samples; misalignment is counted off-thread
fn read_mono_input<'a>(
    datas: &'a mut [libspa::buffer::Data],
    timing: &SharedPlaybackTiming,
) -> Option<&'a [f32]> {
    let data = datas.first_mut()?;
    let n_bytes = data.chunk().size() as usize;
    if n_bytes == 0 {
        return None;
    }
    let bytes = data.data()?;
    let n = n_bytes.min(bytes.len());
    match bytemuck::try_cast_slice::<u8, f32>(&bytes[..n]) {
        Ok(s) => Some(s),
        Err(_) => {
            timing.record_format_error();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    //! audio-thread leg (apply_audio_cmd) + control-side send leg
    //! (FilterControl); pw_filter halves need a running PipeWire.

    use super::*;
    use headroom_dsp::{
        AgcGain, AgcGainConfig, Compressor, CompressorConfig, Limiter, LimiterConfig,
        SoftTierConfig,
    };

    const SR: f32 = 48_000.0;

    #[test]
    fn apply_audio_cmd_updates_compressor_scalars() {
        let mut compressor = Compressor::new(CompressorConfig::default(), SR);
        let mut limiter = Limiter::new(LimiterConfig::default(), SR);
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        let new_cfg = CompressorConfig {
            threshold_db: -12.0,
            ratio: 4.0,
            ..CompressorConfig::default()
        };
        apply_audio_cmd(
            AudioCmd::SetCompressor(new_cfg),
            &mut compressor,
            &mut limiter,
            &mut agc,
        );
        let active = compressor.config();
        assert!((active.threshold_db - -12.0).abs() < 1e-6);
        assert!((active.ratio - 4.0).abs() < 1e-6);
    }

    #[test]
    fn apply_audio_cmd_updates_limiter_scalars() {
        let mut compressor = Compressor::new(CompressorConfig::default(), SR);
        let mut limiter = Limiter::new(LimiterConfig::default(), SR);
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        let new_cfg = LimiterConfig {
            ceiling_dbtp: -1.5,
            release_ms: 250.0,
            soft: Some(SoftTierConfig {
                max_psr_db: 10.0,
                ..SoftTierConfig::default()
            }),
            ..LimiterConfig::default()
        };
        apply_audio_cmd(
            AudioCmd::SetLimiter(new_cfg),
            &mut compressor,
            &mut limiter,
            &mut agc,
        );
        assert!((limiter.ceiling_dbtp() - -1.5).abs() < 1e-6);
        assert!((limiter.config().release_ms - 250.0).abs() < 1e-6);
        let soft = limiter.config().soft.expect("soft preserved");
        assert!((soft.max_psr_db - 10.0).abs() < 1e-6);
    }

    #[test]
    fn apply_audio_cmd_skips_structural_limiter_change_silently() {
        let mut compressor = Compressor::new(CompressorConfig::default(), SR);
        let mut limiter = Limiter::new(LimiterConfig::default(), SR);
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        let bad = LimiterConfig {
            // structural; can't apply in place
            oversample: 8,
            ..LimiterConfig::default()
        };
        // Should not panic, should not change the limiter.
        apply_audio_cmd(
            AudioCmd::SetLimiter(bad),
            &mut compressor,
            &mut limiter,
            &mut agc,
        );
        assert_eq!(limiter.config().oversample, LimiterConfig::default().oversample);
    }

    #[test]
    fn filter_control_send_reaches_consumer() {
        let (control, mut consumer) = FilterControl::for_testing(8);
        assert!(control.set_compressor(CompressorConfig::default()));
        assert!(control.set_limiter(LimiterConfig::default()));
        // Two commands queued.
        let c1 = consumer.pop().expect("first cmd");
        let c2 = consumer.pop().expect("second cmd");
        assert!(matches!(c1, AudioCmd::SetCompressor(_)));
        assert!(matches!(c2, AudioCmd::SetLimiter(_)));
        assert!(consumer.pop().is_err(), "ring drained");
    }

    #[test]
    fn filter_control_returns_false_on_full_ring() {
        // Capacity 2: third push should fail.
        let (control, _consumer) = FilterControl::for_testing(2);
        assert!(control.set_compressor(CompressorConfig::default()));
        assert!(control.set_limiter(LimiterConfig::default()));
        assert!(!control.set_compressor(CompressorConfig::default()));
    }

    #[test]
    fn filter_control_send_then_drain_applies_to_dsp_kernels() {
        // End-to-end on the cmd plane: push via FilterControl, drain
        // via apply_audio_cmd, observe DSP state.
        let (control, mut consumer) = FilterControl::for_testing(8);
        let mut compressor = Compressor::new(CompressorConfig::default(), SR);
        let mut limiter = Limiter::new(LimiterConfig::default(), SR);
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);

        control.set_compressor(CompressorConfig {
            threshold_db: -8.0,
            ..CompressorConfig::default()
        });
        control.set_limiter(LimiterConfig {
            ceiling_dbtp: -2.0,
            ..LimiterConfig::default()
        });

        while let Ok(cmd) = consumer.pop() {
            apply_audio_cmd(cmd, &mut compressor, &mut limiter, &mut agc);
        }
        assert!((compressor.config().threshold_db - -8.0).abs() < 1e-6);
        assert!((limiter.ceiling_dbtp() - -2.0).abs() < 1e-6);
    }

    #[test]
    fn apply_audio_cmd_updates_agc_target_and_enable() {
        let mut compressor = Compressor::new(CompressorConfig::default(), SR);
        let mut limiter = Limiter::new(LimiterConfig::default(), SR);
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        apply_audio_cmd(
            AudioCmd::SetAgcTargetDb(4.5),
            &mut compressor,
            &mut limiter,
            &mut agc,
        );
        assert!((agc.target_db() - 4.5).abs() < 1e-6);
        apply_audio_cmd(
            AudioCmd::SetAgcEnabled(false),
            &mut compressor,
            &mut limiter,
            &mut agc,
        );
        assert!(!agc.enabled());
        // Disable resets target to 0 (smoother unwinds gracefully).
        assert!((agc.target_db()).abs() < 1e-6);
    }
}
