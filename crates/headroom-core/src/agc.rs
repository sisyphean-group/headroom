//! control-thread piece of the slow agc; not spike-reactive (time constants are seconds)

use std::time::Duration;

use ebur128::{EbuR128, Mode};
use headroom_ipc::{Event, MeterTick, Topic};

use crate::meters::SharedBusMetrics;
use crate::pw::filter::FilterControl;
use crate::state::SharedState;

/// hardcoded for v0; not a profile knob
pub const AGC_TICK: Duration = Duration::from_millis(50);

/// ~50 ms of stereo at 48k (4800 samples) plus slack; bounds `ebur128.add_frames_f32` work
const TICK_BUF_SAMPLES: usize = 8192;

/// "no usable measurement yet" — `ebur128` before its window fills, or during silence. published
/// verbatim in `MeterTick.*_lufs` so clients can recognise it without hardcoding the number.
pub const LOUDNESS_FLOOR_LUFS: f32 = -200.0;

pub struct AgcController {
    sample_rate: u32,
    channels: u32,
    ebu: EbuR128,
    measurement_consumer: rtrb::Consumer<f32>,
    filter_control: FilterControl,
    daemon: SharedState,
    smoothed_target_db: f32,
    /// cached so we push the audio-thread enable flag exactly on transition
    last_enabled: bool,
    last_short_term_lufs: f32,
    /// last successfully queued limiter program loudness
    last_program_lufs: Option<f32>,
    bus_metrics: SharedBusMetrics,
    /// wraps freely
    meter_tick_counter: u32,
    timing: crate::meters::SharedPlaybackTiming,
    /// previous values, for computing per-log deltas (only warn on new events)
    last_logged_spike_count: u64,
    last_logged_starved: u64,
    last_logged_dropped: u64,
    last_logged_format_errors: u64,
    timing_log_counter: u32,
}

impl AgcController {
    pub fn new(
        sample_rate: u32,
        channels: u32,
        measurement_consumer: rtrb::Consumer<f32>,
        filter_control: FilterControl,
        daemon: SharedState,
        bus_metrics: SharedBusMetrics,
        timing: crate::meters::SharedPlaybackTiming,
    ) -> Result<Self, AgcInitError> {
        // Mode::I costs a histogram walk per `loudness_global()` — bounded, fine at 20 Hz.
        // lets the `meters` topic surface integrated lufs without a second ebur128 instance.
        let ebu = EbuR128::new(
            channels,
            sample_rate,
            Mode::S | Mode::M | Mode::I | Mode::TRUE_PEAK,
        )
        .map_err(AgcInitError::from)?;
        Ok(Self {
            sample_rate,
            channels,
            ebu,
            measurement_consumer,
            filter_control,
            daemon,
            smoothed_target_db: 0.0,
            last_enabled: true,
            last_short_term_lufs: LOUDNESS_FLOOR_LUFS,
            last_program_lufs: None,
            bus_metrics,
            meter_tick_counter: 0,
            timing,
            last_logged_spike_count: 0,
            last_logged_starved: 0,
            last_logged_dropped: 0,
            last_logged_format_errors: 0,
            timing_log_counter: 0,
        })
    }

    /// `LOUDNESS_FLOOR_LUFS` before the short-term window fills.
    #[must_use]
    pub fn last_short_term_lufs(&self) -> f32 {
        self.last_short_term_lufs
    }

    #[must_use]
    pub fn current_target_db(&self) -> f32 {
        self.smoothed_target_db
    }

    /// one control-loop iteration; invoked at [`AGC_TICK`] cadence by a main-loop timer.
    pub fn tick(&mut self) {
        // snapshot config out from under the lock
        let (cfg, compressor_enabled, publish_hz) = {
            let s = self.daemon.lock();
            let p = s.profiles.effective();
            (p.agc.clone(), p.compressor.enabled, p.meters.publish_hz)
        };

        if cfg.enabled != self.last_enabled {
            self.filter_control.set_agc_enabled(cfg.enabled);
            self.last_enabled = cfg.enabled;
        }

        // unconditional so `meters` keeps surfacing lufs even with only compressor/limiter on
        self.consume_measurements();
        let short_term = finite_or_floor(
            self.ebu.loudness_shortterm().map(|v| v as f32).ok(),
        );
        self.last_short_term_lufs = short_term;

        if cfg.enabled
            && short_term > cfg.silence_threshold_lufs
            && short_term.is_finite()
        {
            let raw_target = cfg.target_lufs - short_term;
            let clamped = raw_target.clamp(-cfg.max_cut_db, cfg.max_boost_db);

            // leaky integrator: attack when target drops, release when it rises
            let dt_ms = AGC_TICK.as_secs_f32() * 1000.0;
            let alpha = if clamped < self.smoothed_target_db {
                alpha_for_dt(cfg.attack_ms, dt_ms)
            } else {
                alpha_for_dt(cfg.release_ms, dt_ms)
            };
            self.smoothed_target_db += alpha * (clamped - self.smoothed_target_db);
            self.filter_control
                .set_agc_target_db(self.smoothed_target_db);
        }

        // limiter sees post-agc loudness; ebur128 measures pre-agc
        let program_lufs = if short_term.is_finite() && short_term > cfg.silence_threshold_lufs {
            let makeup_db = if cfg.enabled { self.smoothed_target_db } else { 0.0 };
            Some(short_term + makeup_db)
        } else {
            None
        };
        self.push_program_loudness(program_lufs);

        self.publish_meters(publish_hz, cfg.enabled, compressor_enabled);
        self.log_playback_timing();
    }

    fn push_program_loudness(&mut self, program_lufs: Option<f32>) {
        const EPSILON_DB: f32 = 0.1;
        let changed = match (self.last_program_lufs, program_lufs) {
            (Some(prev), Some(next)) => (prev - next).abs() >= EPSILON_DB,
            (None, None) => false,
            _ => true,
        };
        if changed && self.filter_control.set_program_loudness_lufs(program_lufs) {
            self.last_program_lufs = program_lufs;
        }
    }

    /// throttled (~1 Hz) log of playback callback timing; lock-free atomic loads.
    fn log_playback_timing(&mut self) {
        // 20 Hz tick → every 20 ticks ≈ 1 Hz
        self.timing_log_counter = self.timing_log_counter.wrapping_add(1);
        if self.timing_log_counter % 20 != 0 {
            return;
        }
        let snap = self.timing.snapshot();
        if snap.call_count == 0 {
            return;
        }
        let avg_us = snap.sum_us / snap.call_count.max(1);
        let new_spikes = snap.spike_count.saturating_sub(self.last_logged_spike_count);
        self.last_logged_spike_count = snap.spike_count;
        let new_starved = snap.samples_starved.saturating_sub(self.last_logged_starved);
        self.last_logged_starved = snap.samples_starved;
        let new_dropped = snap.samples_dropped.saturating_sub(self.last_logged_dropped);
        self.last_logged_dropped = snap.samples_dropped;
        if new_spikes > 0 {
            tracing::warn!(
                avg_us,
                max_us = snap.max_us,
                new_spikes,
                total_spikes = snap.spike_count,
                last_spike_us = snap.last_spike_us,
                last_spike_at_call = snap.last_spike_at_call,
                call_count = snap.call_count,
                "playback callback BUSY spike(s) since last log"
            );
        } else {
            tracing::debug!(
                avg_us,
                max_us = snap.max_us,
                call_count = snap.call_count,
                "playback callback timing"
            );
        }
        // ring-imbalance diagnostic: steady-state is all zeros. non-zero delta = capture→playback
        // ring drained/stuffed within a quantum (the "tremolo every quantum" mechanism). warn so
        // it shows at default tracing level.
        if new_starved > 0 || new_dropped > 0 {
            tracing::warn!(
                new_starved,
                total_starved = snap.samples_starved,
                new_dropped,
                total_dropped = snap.samples_dropped,
                call_count = snap.call_count,
                "filter ring imbalance — playback zero-filled and/or capture dropped samples"
            );
        }
        // rt format errors are logged here to keep the callback allocation-free
        let new_format_errors = snap.format_errors.saturating_sub(self.last_logged_format_errors);
        self.last_logged_format_errors = snap.format_errors;
        if new_format_errors > 0 {
            tracing::warn!(
                new_format_errors,
                total_format_errors = snap.format_errors,
                "filter skipped short/misaligned audio buffer(s)"
            );
        }
    }

    fn consume_measurements(&mut self) {
        let mut buf = [0.0_f32; TICK_BUF_SAMPLES];
        let mut n = 0;
        while n < buf.len() {
            match self.measurement_consumer.pop() {
                Ok(s) => {
                    buf[n] = s;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        if n == 0 {
            return;
        }
        // ebur128 wants whole frames; drop any odd trailing sample.
        let usable = (n / self.channels as usize) * self.channels as usize;
        if usable == 0 {
            return;
        }
        if let Err(e) = self.ebu.add_frames_f32(&buf[..usable]) {
            tracing::warn!(error = %e, "ebur128 add_frames_f32 failed");
        }
    }

    fn publish_meters(&mut self, publish_hz: f32, agc_enabled: bool, compressor_enabled: bool) {
        if !self.should_publish(publish_hz) {
            return;
        }
        let bus = *self.bus_metrics.lock();
        // ebur128 returns -inf (not Err) for "no measurement yet"; serde_json renders non-finite
        // f32 as null, so floor here.
        let momentary = finite_or_floor(
            self.ebu.loudness_momentary().map(|v| v as f32).ok(),
        );
        let integrated = finite_or_floor(
            self.ebu.loudness_global().map(|v| v as f32).ok(),
        );

        let tick = MeterTick {
            momentary_lufs: momentary,
            shortterm_lufs: self.last_short_term_lufs,
            integrated_lufs: integrated,
            true_peak_dbtp: bus.true_peak_dbtp,
            // path GR is additive in log domain; both ≤ 0 dB when reducing
            gain_reduction_db: bus.compressor_gr_db + bus.limiter_total_gr_db,
            compressor_gr_db: bus.compressor_gr_db,
            limiter_gr_db: bus.limiter_total_gr_db,
            agc_gain_db: self.smoothed_target_db,
            agc_enabled,
            compressor_enabled,
        };

        if let Ok(event) = Event::new(Topic::Meters, "tick", &tick) {
            self.daemon.lock().broadcaster.publish(Topic::Meters, event);
        }
    }

    /// tick-rate gate for `meters`; caps at [`AGC_TICK`]'s 20 Hz, higher `publish_hz` clamped.
    fn should_publish(&mut self, publish_hz: f32) -> bool {
        if publish_hz <= 0.0 {
            return false;
        }
        let agc_hz = 1000.0 / AGC_TICK.as_millis() as f32;
        if publish_hz >= agc_hz {
            self.meter_tick_counter = self.meter_tick_counter.wrapping_add(1);
            return true;
        }
        let skip = (agc_hz / publish_hz).round().max(1.0) as u32;
        let now = self.meter_tick_counter;
        self.meter_tick_counter = self.meter_tick_counter.wrapping_add(1);
        now % skip == 0
    }

    pub fn reset(&mut self) {
        self.smoothed_target_db = 0.0;
        self.last_short_term_lufs = LOUDNESS_FLOOR_LUFS;
        self.last_program_lufs = None;
        // ebur128 has no public reset; rebuild with the same mode set as new()
        if let Ok(fresh) = EbuR128::new(
            self.channels,
            self.sample_rate,
            Mode::S | Mode::M | Mode::I | Mode::TRUE_PEAK,
        ) {
            self.ebu = fresh;
        }
        self.filter_control.set_agc_target_db(0.0);
        self.filter_control.set_program_loudness_lufs(None);
    }

    /// rebind to a freshly-built filter after rate-change. old consumer/control point at rtrbs
    /// whose producers were just dropped (sends would fail), so swap handles + rebuild ebur128 at
    /// the new rate.
    pub fn rebind(
        &mut self,
        measurement_consumer: rtrb::Consumer<f32>,
        filter_control: FilterControl,
        sample_rate: u32,
    ) {
        self.measurement_consumer = measurement_consumer;
        self.filter_control = filter_control;
        self.sample_rate = sample_rate;
        self.reset();
    }
}

/// -inf (ebur128 "no reading" sentinel) and NaN collapse to [`LOUDNESS_FLOOR_LUFS`].
fn finite_or_floor(v: Option<f32>) -> f32 {
    match v {
        Some(x) if x.is_finite() => x,
        _ => LOUDNESS_FLOOR_LUFS,
    }
}

/// leaky-integrator alpha: `1 - exp(-dt / tau)`, clamped to `[0, 1]`.
fn alpha_for_dt(tau_ms: f32, dt_ms: f32) -> f32 {
    if tau_ms <= 0.0 || dt_ms <= 0.0 {
        return 1.0;
    }
    (1.0 - (-dt_ms / tau_ms).exp()).clamp(0.0, 1.0)
}

/// construction-time failures only; tick-time errors are logged and the tick skipped.
#[derive(Debug, thiserror::Error)]
pub enum AgcInitError {
    #[error("ebur128: {0}")]
    Ebu(#[from] ebur128::Error),
}

impl From<AgcInitError> for crate::error::DaemonError {
    fn from(e: AgcInitError) -> Self {
        crate::error::DaemonError::other(format!("agc init: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meters;
    use crate::profile_store::ProfileStore;
    use crate::pw::filter::{AudioCmd, FilterControl};
    use crate::state::{self, DaemonState};
    use rtrb::RingBuffer;

    const SR: u32 = 48_000;
    const CH: u32 = 2;

    fn fixture() -> (
        AgcController,
        rtrb::Producer<f32>,
        rtrb::Consumer<AudioCmd>,
        SharedState,
        SharedBusMetrics,
    ) {
        let (m_prod, m_cons) = RingBuffer::<f32>::new(8192);
        let (control, cmd_cons) = FilterControl::for_testing(32);
        let state = state::shared(DaemonState::new(ProfileStore::builtin()));
        let bus = meters::shared();
        let timing = meters::shared_timing();
        let agc = AgcController::new(
            SR,
            CH,
            m_cons,
            control,
            state.clone(),
            bus.clone(),
            timing,
        )
        .unwrap();
        (agc, m_prod, cmd_cons, state, bus)
    }

    fn push_silence(prod: &mut rtrb::Producer<f32>, frames: usize) {
        for _ in 0..frames {
            let _ = prod.push(0.0);
            let _ = prod.push(0.0);
        }
    }

    fn push_sine(prod: &mut rtrb::Producer<f32>, frames: usize, amp: f32) {
        // Constant amplitude impulse-like — not a real sine but it
        // produces a measurable loudness in ebur128 well above silence.
        for _ in 0..frames {
            let _ = prod.push(amp);
            let _ = prod.push(-amp);
        }
    }

    #[test]
    fn tick_with_no_samples_does_nothing() {
        let (mut agc, _prod, mut cmd_cons, _state, _bus) = fixture();
        agc.tick();
        assert!(cmd_cons.pop().is_err(), "no samples → no target push");
        assert_eq!(agc.current_target_db(), 0.0);
    }

    #[test]
    fn tick_under_silence_threshold_holds_target() {
        let (mut agc, mut prod, mut cmd_cons, _state, _bus) = fixture();
        push_silence(&mut prod, 4800); // 100ms of silence
        agc.tick();
        // ebur128 may report -inf or values below the silence
        // threshold; either way we should not push.
        assert!(
            cmd_cons.pop().is_err(),
            "below silence threshold — no target push expected"
        );
    }

    #[test]
    fn tick_with_audible_signal_pushes_target() {
        let (mut agc, mut prod, mut cmd_cons, _state, _bus) = fixture();
        // Pump multiple ticks worth so ebur128's short-term window
        // (~3 s) starts producing values.
        for _ in 0..40 {
            push_sine(&mut prod, 4800, 0.3);
            agc.tick();
        }
        // We expect at least one SetAgcTargetDb to have been pushed
        // once short-term loudness became finite.
        let mut saw = false;
        while let Ok(cmd) = cmd_cons.pop() {
            if matches!(cmd, AudioCmd::SetAgcTargetDb(_)) {
                saw = true;
            }
        }
        assert!(saw, "expected at least one AGC target push after pumping");
    }

    #[test]
    fn tick_with_audible_signal_pushes_program_loudness() {
        let (mut agc, mut prod, mut cmd_cons, _state, _bus) = fixture();
        for _ in 0..40 {
            push_sine(&mut prod, 4800, 0.3);
            agc.tick();
        }
        let mut program: Option<f32> = None;
        while let Ok(cmd) = cmd_cons.pop() {
            if let AudioCmd::SetProgramLoudnessLufs(v) = cmd {
                program = v;
            }
        }
        let lufs = program.expect("expected a SetProgramLoudnessLufs(Some) push");
        assert!(
            lufs.is_finite() && lufs > -200.0,
            "program loudness should be a real reading, got {lufs}"
        );
    }

    #[test]
    fn silence_then_audio_clears_then_sets_program_loudness() {
        let (mut agc, mut prod, mut cmd_cons, _state, _bus) = fixture();
        for _ in 0..5 {
            push_silence(&mut prod, 4800);
            agc.tick();
        }
        while let Ok(cmd) = cmd_cons.pop() {
            if let AudioCmd::SetProgramLoudnessLufs(Some(v)) = cmd {
                panic!("silence should not set a program loudness, got {v}");
            }
        }
    }

    #[test]
    fn reset_clears_program_loudness() {
        let (mut agc, mut prod, mut cmd_cons, _state, _bus) = fixture();
        for _ in 0..40 {
            push_sine(&mut prod, 4800, 0.3);
            agc.tick();
        }
        while cmd_cons.pop().is_ok() {}
        agc.reset();
        let mut saw_clear = false;
        while let Ok(cmd) = cmd_cons.pop() {
            if matches!(cmd, AudioCmd::SetProgramLoudnessLufs(None)) {
                saw_clear = true;
            }
        }
        assert!(saw_clear, "reset should clear the limiter's program loudness");
    }

    #[test]
    fn agc_disable_in_profile_flips_audio_thread() {
        let (mut agc, _prod, mut cmd_cons, state, _bus) = fixture();
        // First tick with the default-enabled profile.
        agc.tick();
        // Drain any commands.
        while cmd_cons.pop().is_ok() {}

        // Disable AGC in the profile.
        state
            .lock()
            .profiles
            .set_setting("agc.enabled", serde_json::json!(false))
            .unwrap();
        agc.tick();

        // Expect a SetAgcEnabled(false) command.
        let mut saw_disable = false;
        while let Ok(cmd) = cmd_cons.pop() {
            if matches!(cmd, AudioCmd::SetAgcEnabled(false)) {
                saw_disable = true;
            }
        }
        assert!(saw_disable, "expected SetAgcEnabled(false) on profile flip");
    }

    #[test]
    fn alpha_endpoints() {
        // tau == 0 → instantaneous.
        assert_eq!(alpha_for_dt(0.0, 50.0), 1.0);
        // dt == 0 → no progress.
        assert_eq!(alpha_for_dt(1000.0, 0.0), 1.0); // we clamp dt<=0 to 1.0 too
        // Sanity: shorter tau → larger alpha for same dt.
        let a_fast = alpha_for_dt(100.0, 50.0);
        let a_slow = alpha_for_dt(2000.0, 50.0);
        assert!(a_fast > a_slow);
    }
}
