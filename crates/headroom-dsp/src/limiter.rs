//! two-tier true-peak limiter: parallel soft (dynamic psr ceiling) + hard (absolute contract) tiers.

use crate::delay::DelayLine;
use crate::envelope::AttackRelease;
use crate::oversample::{design_lowpass_blackman, PolyphaseDownsampler, PolyphaseUpsampler};
use crate::sliding_max::SlidingMaxBuffer;
use crate::util::{db_to_lin, lin_to_db, time_to_alpha};

/// soft tier targets a dynamic ceiling `program_loudness_lufs + max_psr_db`; bounds the
/// peak-to-loudness ratio, not a safety contract (that's the hard tier).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoftTierConfig {
    /// max peak-to-shortterm-loudness ratio (dB). effective ceiling = `program_lufs + max_psr_db`.
    pub max_psr_db: f32,
    /// fallback ceiling (dBTP) when no program loudness yet (startup, before first agc window).
    pub static_ceiling_dbtp: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Default for SoftTierConfig {
    fn default() -> Self {
        Self {
            max_psr_db: 14.0,
            static_ceiling_dbtp: -6.0,
            attack_ms: 5.0,
            release_ms: 200.0,
        }
    }
}

impl SoftTierConfig {
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if self.static_ceiling_dbtp > 0.0 {
            self.static_ceiling_dbtp = 0.0;
        }
        if !self.max_psr_db.is_finite() || self.max_psr_db < 0.0 {
            self.max_psr_db = 0.0;
        }
        if self.attack_ms < 0.0 || !self.attack_ms.is_finite() {
            self.attack_ms = 0.0;
        }
        if self.release_ms < 0.0 || !self.release_ms.is_finite() {
            self.release_ms = 0.0;
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterConfig {
    /// hard-tier output ceiling (dBTP). must be `<= 0.0`.
    pub ceiling_dbtp: f32,
    /// lookahead (ms); sets delay-line length + peak-window size. shared by both tiers.
    pub lookahead_ms: f32,
    /// hard-tier exponential release toward unity (ms).
    pub release_ms: f32,
    /// hard-tier hold after a reduction before release begins (ms).
    pub hold_ms: f32,
    /// oversample factor. 1 disables isp detection; 4 is the bs.1770-4 reference.
    pub oversample: usize,
    /// fir taps for the oversampling filter (odd).
    pub fir_taps: usize,
    /// `None` disables the soft tier → pure brickwall.
    pub soft: Option<SoftTierConfig>,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            ceiling_dbtp: -0.1,
            lookahead_ms: 2.0,
            release_ms: 80.0,
            hold_ms: 5.0,
            oversample: 4,
            fir_taps: 31,
            soft: Some(SoftTierConfig::default()),
        }
    }
}

/// internal-rate cap (Hz). detector upsamples to `sample_rate × oversample`; above ~192 kHz
/// fir cost rises linearly with no isp-detection gain, so drop oversample on high base rates.
pub const MAX_INTERNAL_RATE_HZ: f32 = 192_000.0;

impl LimiterConfig {
    /// rate-agnostic. callers that know the sample rate should prefer [`Self::sanitize_for_rate`]
    /// so oversample scales down on high-rate inputs.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if self.ceiling_dbtp > 0.0 {
            self.ceiling_dbtp = 0.0;
        }
        self.oversample = self.oversample.clamp(1, 8);
        if self.fir_taps < 5 {
            self.fir_taps = 5;
        }
        if self.fir_taps % 2 == 0 {
            self.fir_taps += 1;
        }
        if let Some(soft) = self.soft {
            self.soft = Some(soft.sanitized());
        }
        self
    }

    /// caps oversample so post-upsample rate stays ≤ [`MAX_INTERNAL_RATE_HZ`]; always ≥ 1.
    /// at default 4×: 48 kHz → 4×, 96 kHz → 2×, 192 kHz → 1×.
    #[must_use]
    pub fn sanitize_for_rate(self, sample_rate: f32) -> Self {
        let mut s = self.sanitized();
        if sample_rate > 0.0 {
            let max_os =
                (MAX_INTERNAL_RATE_HZ / sample_rate).floor().max(1.0) as usize;
            if s.oversample > max_os {
                s.oversample = max_os;
            }
        }
        s
    }

    /// brickwall only (no soft tier).
    #[must_use]
    pub fn brickwall_only() -> Self {
        Self {
            soft: None,
            ..Self::default()
        }
    }
}

const MAX_OVERSAMPLE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetConfigOutcome {
    /// applied in place.
    Applied,
    /// differs in `oversample`/`fir_taps`/`lookahead_ms` (in samples) → would realloc buffers.
    /// limiter unchanged; rebuild from `Limiter::new` on the control thread.
    StructuralChange,
}

pub struct Limiter {
    cfg: LimiterConfig,
    /// captured at construction so try_set_config can recompute time coeffs without a repass.
    sample_rate: f32,
    ceiling_lin: f32,
    os: usize,

    // Per-channel oversampler / downsampler / delay-line paths.
    up_l: PolyphaseUpsampler,
    up_r: PolyphaseUpsampler,
    down_l: PolyphaseDownsampler,
    down_r: PolyphaseDownsampler,
    delay_l: DelayLine,
    delay_r: DelayLine,

    /// sliding-window peak (oversampled domain), shared across channels + tiers.
    peak_buf: SlidingMaxBuffer,

    // ---- Hard tier state (instant attack + hold + release) ----
    hard_gain: f32,
    hold_remaining: u32,
    hold_samples_os: u32,
    hard_release_alpha: f32,

    // ---- Soft tier state (smooth envelope) ----
    soft_envelope: Option<AttackRelease>,
    soft_max_psr_db: f32,
    soft_static_ceiling_lin: f32,
    program_loudness_lufs: Option<f32>,
    /// effective soft ceiling (linear), recomputed when program loudness changes.
    soft_ceiling_lin: f32,

    // scratch buffers (sized for max oversample factor).
    up_buf_l: [f32; MAX_OVERSAMPLE],
    up_buf_r: [f32; MAX_OVERSAMPLE],
    gained_buf_l: [f32; MAX_OVERSAMPLE],
    gained_buf_r: [f32; MAX_OVERSAMPLE],

    // telemetry (sampled per frame).
    last_peak_lin: f32,
    last_gr_db: f32,
    last_soft_gr_db: f32,
    last_hard_gr_db: f32,
}

impl Limiter {
    /// allocates fir coeffs, polyphase tables, delay buffers. not rt-safe.
    #[must_use]
    pub fn new(cfg: LimiterConfig, sample_rate: f32) -> Self {
        let cfg = cfg.sanitized();
        let os = cfg.oversample;
        let lowpass = if os > 1 {
            design_lowpass_blackman(cfg.fir_taps, 0.45 / os as f32)
        } else {
            vec![1.0]
        };

        let os_rate = sample_rate * os as f32;
        let lookahead_samples_os = (cfg.lookahead_ms * 1e-3 * os_rate).round() as usize;
        let lookahead_samples_os = lookahead_samples_os.max(1);
        let hold_samples_os = (cfg.hold_ms * 1e-3 * os_rate).round() as u32;
        let hard_release_alpha = time_to_alpha(cfg.release_ms, os_rate);
        let ceiling_lin = db_to_lin(cfg.ceiling_dbtp);

        let (soft_envelope, soft_max_psr_db, soft_static_ceiling_lin, soft_ceiling_lin) =
            if let Some(soft) = cfg.soft {
                let env = AttackRelease::new(soft.attack_ms, soft.release_ms, os_rate);
                let static_ceiling_lin = db_to_lin(soft.static_ceiling_dbtp);
                (
                    Some(env),
                    soft.max_psr_db,
                    static_ceiling_lin,
                    static_ceiling_lin,
                )
            } else {
                (None, 0.0, 1.0, 1.0)
            };

        let mut me = Self {
            cfg,
            sample_rate,
            ceiling_lin,
            os,
            up_l: PolyphaseUpsampler::new(os, &lowpass),
            up_r: PolyphaseUpsampler::new(os, &lowpass),
            down_l: PolyphaseDownsampler::new(os, &lowpass),
            down_r: PolyphaseDownsampler::new(os, &lowpass),
            delay_l: DelayLine::new(lookahead_samples_os),
            delay_r: DelayLine::new(lookahead_samples_os),
            peak_buf: SlidingMaxBuffer::new(lookahead_samples_os),
            hard_gain: 1.0,
            hold_remaining: 0,
            hold_samples_os,
            hard_release_alpha,
            soft_envelope,
            soft_max_psr_db,
            soft_static_ceiling_lin,
            program_loudness_lufs: None,
            soft_ceiling_lin,
            up_buf_l: [0.0; MAX_OVERSAMPLE],
            up_buf_r: [0.0; MAX_OVERSAMPLE],
            gained_buf_l: [0.0; MAX_OVERSAMPLE],
            gained_buf_r: [0.0; MAX_OVERSAMPLE],
            last_peak_lin: 0.0,
            last_gr_db: 0.0,
            last_soft_gr_db: 0.0,
            last_hard_gr_db: 0.0,
        };
        // seed soft envelope to unity: no phantom gain reduction on the first frames.
        if let Some(env) = &mut me.soft_envelope {
            env.reset(1.0);
        }
        me
    }

    #[must_use]
    pub fn config(&self) -> LimiterConfig {
        self.cfg
    }

    #[must_use]
    pub fn ceiling_dbtp(&self) -> f32 {
        self.cfg.ceiling_dbtp
    }

    /// applied total reduction `min(soft_gain, hard_gain)` (dB, negative when limiting).
    #[must_use]
    pub fn gain_reduction_db(&self) -> f32 {
        self.last_gr_db
    }

    #[must_use]
    pub fn soft_gain_reduction_db(&self) -> f32 {
        self.last_soft_gr_db
    }

    /// non-zero ⇒ soft tier didn't hold the ceiling and the brickwall engaged. routinely
    /// non-zero on benign material ⇒ soft under-configured (psr too high / attack too slow /
    /// lookahead too short).
    #[must_use]
    pub fn hard_gain_reduction_db(&self) -> f32 {
        self.last_hard_gr_db
    }

    #[must_use]
    pub fn true_peak_dbtp(&self) -> f32 {
        lin_to_db(self.last_peak_lin.max(1e-20))
    }

    /// `program_loudness_lufs + soft.max_psr_db` when both known, else `static_ceiling_dbtp`.
    /// `None` if soft tier disabled.
    #[must_use]
    pub fn effective_soft_ceiling_dbtp(&self) -> Option<f32> {
        self.cfg.soft.map(|_| lin_to_db(self.soft_ceiling_lin))
    }

    /// live-update non-structural params (ceiling, hard release/hold, soft toggle + scalars).
    /// allocation-free, rt-safe. structural changes return [`SetConfigOutcome::StructuralChange`]
    /// and leave the limiter unchanged — see that variant.
    pub fn try_set_config(&mut self, cfg: LimiterConfig) -> SetConfigOutcome {
        let cfg = cfg.sanitized();
        let os_rate = self.sample_rate * cfg.oversample as f32;
        let new_lookahead_samples_os =
            ((cfg.lookahead_ms * 1e-3 * os_rate).round() as usize).max(1);
        let cur_lookahead_samples_os = self.peak_buf.window();
        if cfg.oversample != self.os
            || cfg.fir_taps != self.cfg.fir_taps
            || new_lookahead_samples_os != cur_lookahead_samples_os
        {
            return SetConfigOutcome::StructuralChange;
        }

        self.ceiling_lin = db_to_lin(cfg.ceiling_dbtp);
        self.hard_release_alpha = time_to_alpha(cfg.release_ms, os_rate);
        self.hold_samples_os = (cfg.hold_ms * 1e-3 * os_rate).round() as u32;

        match (cfg.soft, self.cfg.soft) {
            (Some(new_soft), Some(_old_soft)) => {
                self.soft_max_psr_db = new_soft.max_psr_db;
                self.soft_static_ceiling_lin = db_to_lin(new_soft.static_ceiling_dbtp);
                if let Some(env) = &mut self.soft_envelope {
                    env.set_times(new_soft.attack_ms, new_soft.release_ms, os_rate);
                }
            }
            (Some(new_soft), None) => {
                // re-enable: seed envelope to unity, no phantom gain reduction.
                let mut env = AttackRelease::new(new_soft.attack_ms, new_soft.release_ms, os_rate);
                env.reset(1.0);
                self.soft_envelope = Some(env);
                self.soft_max_psr_db = new_soft.max_psr_db;
                self.soft_static_ceiling_lin = db_to_lin(new_soft.static_ceiling_dbtp);
            }
            (None, Some(_)) => {
                self.soft_envelope = None;
                self.soft_max_psr_db = 0.0;
                self.soft_static_ceiling_lin = 1.0;
            }
            (None, None) => {}
        }

        self.cfg = cfg;
        self.recompute_soft_ceiling();
        SetConfigOutcome::Applied
    }

    /// program loudness for the dynamic soft ceiling; called by the agc at tick rate.
    /// non-finite ignored.
    pub fn set_program_loudness_lufs(&mut self, lufs: f32) {
        if !lufs.is_finite() {
            return;
        }
        self.program_loudness_lufs = Some(lufs);
        self.recompute_soft_ceiling();
    }

    /// forget program loudness; soft tier falls back to its static ceiling.
    pub fn clear_program_loudness(&mut self) {
        self.program_loudness_lufs = None;
        self.recompute_soft_ceiling();
    }

    fn recompute_soft_ceiling(&mut self) {
        self.soft_ceiling_lin = match (self.cfg.soft, self.program_loudness_lufs) {
            (Some(_), Some(lufs)) => {
                let dynamic_dbtp = (lufs + self.soft_max_psr_db).min(0.0);
                db_to_lin(dynamic_dbtp)
            }
            (Some(_), None) => self.soft_static_ceiling_lin,
            (None, _) => 1.0,
        };
    }

    /// allocation-free. output guaranteed within `±ceiling_dbtp` (the hard contract).
    pub fn process_frame(&mut self, left: f32, right: f32) -> (f32, f32) {
        // sanitize NaN/Inf to zero; never let garbage into limiter state.
        let left = if left.is_finite() { left } else { 0.0 };
        let right = if right.is_finite() { right } else { 0.0 };

        self.up_l.process(left, &mut self.up_buf_l[..self.os]);
        self.up_r.process(right, &mut self.up_buf_r[..self.os]);

        let mut frame_peak = 0.0_f32;
        let mut min_soft_gain = 1.0_f32;
        let mut min_total_gain = 1.0_f32;

        for k in 0..self.os {
            let s_l = self.up_buf_l[k];
            let s_r = self.up_buf_r[k];

            let peak = s_l.abs().max(s_r.abs());
            frame_peak = frame_peak.max(peak);

            let window_peak = self.peak_buf.push_and_max(peak);

            // ---- Soft tier --------------------------------------
            let soft_gain = if let Some(env) = &mut self.soft_envelope {
                let target = if window_peak > self.soft_ceiling_lin && window_peak > 1e-20 {
                    self.soft_ceiling_lin / window_peak
                } else {
                    1.0
                };
                env.process_gain(target)
            } else {
                1.0
            };
            if soft_gain < min_soft_gain {
                min_soft_gain = soft_gain;
            }

            // ---- Hard tier --------------------------------------
            // size hard gain against the peak *after* the soft tier acts, so it doesn't do
            // redundant work. predicted_post_soft = max(immediate: current soft gain applied;
            // asymptotic: soft converged to target) — the larger/more conservative.
            let predicted_post_soft = if self.soft_envelope.is_some() {
                let asymptotic = window_peak.min(self.soft_ceiling_lin);
                let immediate = window_peak * soft_gain;
                asymptotic.max(immediate)
            } else {
                window_peak
            };
            let hard_target =
                if predicted_post_soft > self.ceiling_lin && predicted_post_soft > 1e-20 {
                    self.ceiling_lin / predicted_post_soft
                } else {
                    1.0
                };

            if hard_target < self.hard_gain {
                self.hard_gain = hard_target;
                self.hold_remaining = self.hold_samples_os;
            } else if self.hold_remaining > 0 {
                self.hold_remaining -= 1;
            } else {
                self.hard_gain += self.hard_release_alpha * (hard_target - self.hard_gain);
                if self.hard_gain > hard_target {
                    self.hard_gain = hard_target;
                }
            }

            // ---- Combine ----------------------------------------
            let total_gain = soft_gain.min(self.hard_gain);
            if total_gain < min_total_gain {
                min_total_gain = total_gain;
            }

            let d_l = self.delay_l.push_pop(s_l);
            let d_r = self.delay_r.push_pop(s_r);

            let mut out_l = d_l * total_gain;
            let mut out_r = d_r * total_gain;

            // defense-in-depth #1: clip in the oversampled domain so overshoots can't enter
            // the downsampler.
            out_l = out_l.clamp(-self.ceiling_lin, self.ceiling_lin);
            out_r = out_r.clamp(-self.ceiling_lin, self.ceiling_lin);

            self.gained_buf_l[k] = out_l;
            self.gained_buf_r[k] = out_r;
        }

        let mut out_l = self.down_l.process(&self.gained_buf_l[..self.os]);
        let mut out_r = self.down_r.process(&self.gained_buf_r[..self.os]);

        // defense-in-depth #2: clip post-downsample, guarding against fir ringing nudging
        // the output above the ceiling.
        out_l = out_l.clamp(-self.ceiling_lin, self.ceiling_lin);
        out_r = out_r.clamp(-self.ceiling_lin, self.ceiling_lin);

        self.last_peak_lin = frame_peak;
        self.last_soft_gr_db = lin_to_db(min_soft_gain.max(1e-12));
        self.last_hard_gr_db = lin_to_db(self.hard_gain.max(1e-12));
        self.last_gr_db = lin_to_db(min_total_gain.max(1e-12));

        (out_l, out_r)
    }

    /// process an interleaved stereo buffer in place.
    pub fn process_interleaved_stereo(&mut self, buf: &mut [f32]) {
        debug_assert!(buf.len() % 2 == 0);
        for frame in buf.chunks_exact_mut(2) {
            let (l, r) = self.process_frame(frame[0], frame[1]);
            frame[0] = l;
            frame[1] = r;
        }
    }

    /// reset all internal state; program loudness is also cleared.
    pub fn reset(&mut self) {
        self.up_l.reset();
        self.up_r.reset();
        self.down_l.reset();
        self.down_r.reset();
        self.delay_l.reset();
        self.delay_r.reset();
        self.peak_buf.reset();
        self.hard_gain = 1.0;
        self.hold_remaining = 0;
        if let Some(env) = &mut self.soft_envelope {
            env.reset(1.0);
        }
        self.program_loudness_lufs = None;
        self.recompute_soft_ceiling();
        self.last_peak_lin = 0.0;
        self.last_gr_db = 0.0;
        self.last_soft_gr_db = 0.0;
        self.last_hard_gr_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    // ----------------------------------------------------------------
    // sanitize_for_rate: oversample factor scales down so the
    // internal (post-upsample) rate stays bounded.
    // ----------------------------------------------------------------

    #[test]
    fn sanitize_for_rate_caps_oversample_at_internal_192k() {
        // Default config has oversample = 4.
        let default = LimiterConfig::default();
        assert_eq!(default.oversample, 4);

        // At 48 kHz: 4× = 192 kHz, at the cap, untouched.
        assert_eq!(default.sanitize_for_rate(48_000.0).oversample, 4);
        // At 44.1 kHz: 4× = 176.4 kHz, under the cap.
        assert_eq!(default.sanitize_for_rate(44_100.0).oversample, 4);
        // At 96 kHz: 4× = 384 kHz, exceeds; drop to 2× = 192 kHz.
        assert_eq!(default.sanitize_for_rate(96_000.0).oversample, 2);
        // At 192 kHz: cap forces oversample = 1.
        assert_eq!(default.sanitize_for_rate(192_000.0).oversample, 1);
        // Pathological rate above the cap still leaves at least 1.
        assert_eq!(default.sanitize_for_rate(384_000.0).oversample, 1);
    }

    #[test]
    fn sanitize_for_rate_preserves_user_lower_oversample() {
        // User who explicitly set oversample = 2 at 48 kHz should
        // keep it; the rate cap doesn't push the value *up*.
        let cfg = LimiterConfig {
            oversample: 2,
            ..LimiterConfig::default()
        };
        assert_eq!(cfg.sanitize_for_rate(48_000.0).oversample, 2);
    }

    // ----------------------------------------------------------------
    // try_set_config: scalar updates apply in place, structural
    // changes are rejected.
    // ----------------------------------------------------------------

    #[test]
    fn try_set_config_applies_scalar_changes() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        let cfg = LimiterConfig {
            ceiling_dbtp: -3.0,
            release_ms: 200.0,
            hold_ms: 10.0,
            ..LimiterConfig::default()
        };
        assert_eq!(l.try_set_config(cfg), SetConfigOutcome::Applied);
        assert!((l.ceiling_dbtp() - -3.0).abs() < 1e-6);
        let active = l.config();
        assert!((active.release_ms - 200.0).abs() < 1e-6);
        assert!((active.hold_ms - 10.0).abs() < 1e-6);
    }

    #[test]
    fn try_set_config_can_toggle_soft_tier() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        // Start with soft on. Disable it.
        let mut cfg = LimiterConfig {
            soft: None,
            ..LimiterConfig::default()
        };
        assert_eq!(l.try_set_config(cfg), SetConfigOutcome::Applied);
        assert!(l.config().soft.is_none());
        assert!(l.effective_soft_ceiling_dbtp().is_none());

        // Re-enable with custom params.
        let new_soft = SoftTierConfig {
            max_psr_db: 10.0,
            static_ceiling_dbtp: -4.0,
            attack_ms: 8.0,
            release_ms: 300.0,
        };
        cfg.soft = Some(new_soft);
        assert_eq!(l.try_set_config(cfg), SetConfigOutcome::Applied);
        let active_soft = l.config().soft.expect("soft re-enabled");
        assert!((active_soft.max_psr_db - 10.0).abs() < 1e-6);
        assert!((active_soft.static_ceiling_dbtp - -4.0).abs() < 1e-6);
    }

    #[test]
    fn try_set_config_rejects_oversample_change() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        let cfg = LimiterConfig {
            oversample: 8,
            ..LimiterConfig::default()
        };
        assert_eq!(l.try_set_config(cfg), SetConfigOutcome::StructuralChange);
        // Limiter unchanged.
        assert_eq!(l.config().oversample, LimiterConfig::default().oversample);
    }

    #[test]
    fn try_set_config_rejects_lookahead_change() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        let cfg = LimiterConfig {
            // resizes delay + peak buffer
            lookahead_ms: 5.0,
            ..LimiterConfig::default()
        };
        assert_eq!(l.try_set_config(cfg), SetConfigOutcome::StructuralChange);
    }

    #[test]
    fn try_set_config_rejects_fir_taps_change() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        let cfg = LimiterConfig {
            fir_taps: 63,
            ..LimiterConfig::default()
        };
        assert_eq!(l.try_set_config(cfg), SetConfigOutcome::StructuralChange);
    }

    fn run_sine(
        limiter: &mut Limiter,
        freq: f32,
        amp_db: f32,
        samples: usize,
        sr: f32,
    ) -> Vec<f32> {
        let amp = db_to_lin(amp_db);
        let mut out = Vec::with_capacity(samples * 2);
        for n in 0..samples {
            let t = n as f32 / sr;
            let s = amp * (2.0 * PI * freq * t).sin();
            let (l, r) = limiter.process_frame(s, s);
            out.push(l);
            out.push(r);
        }
        out
    }

    // ----------------------------------------------------------------
    // Hard-tier contract: holds with or without the soft tier present.
    // ----------------------------------------------------------------

    #[test]
    fn passes_signal_below_both_ceilings_unchanged() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        // -18 dBFS is below the default static soft ceiling of -6 dBTP
        // and the hard ceiling. Neither tier should engage.
        let out = run_sine(&mut l, 440.0, -18.0, 4_800, sr);
        let max_abs = out.iter().skip(1_000).fold(0.0_f32, |a, &b| a.max(b.abs()));
        let max_db = lin_to_db(max_abs);
        assert!(
            (max_db - (-18.0)).abs() < 0.5,
            "expected ~-18 dB, got {max_db}"
        );
        assert!(
            l.gain_reduction_db().abs() < 0.5,
            "expected ~0 GR, got {}",
            l.gain_reduction_db()
        );
    }

    #[test]
    fn enforces_hard_ceiling_on_hot_signal_with_soft_tier() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        let out = run_sine(&mut l, 440.0, 6.0, 9_600, sr);
        let ceiling_lin = db_to_lin(-0.1);
        let max_abs = out
            .iter()
            .skip(2_000)
            .fold(0.0_f32, |a, &b| a.max(b.abs()));
        assert!(
            max_abs <= ceiling_lin + 1e-6,
            "above hard ceiling: max_abs={max_abs}, ceiling_lin={ceiling_lin}"
        );
    }

    #[test]
    fn enforces_hard_ceiling_on_intersample_peak_with_soft_tier() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        let ceiling_lin = db_to_lin(-0.1);
        let mut max_abs = 0.0_f32;
        let mut sign = 1.0_f32;
        let amp = 0.95_f32;
        for n in 0..9_600 {
            let s = sign * amp;
            sign = -sign;
            let (lo, ro) = l.process_frame(s, s);
            if n > 1_500 {
                max_abs = max_abs.max(lo.abs()).max(ro.abs());
            }
        }
        assert!(
            max_abs <= ceiling_lin + 1e-6,
            "ISP: above hard ceiling: max_abs={max_abs}, ceiling_lin={ceiling_lin}"
        );
    }

    #[test]
    fn enforces_hard_ceiling_on_transient_impulse_with_soft_tier() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        let ceiling_lin = db_to_lin(-0.1);
        let mut max_abs = 0.0_f32;
        for n in 0..4_800_usize {
            let s = if n == 1_000 { 4.0 } else { 0.0 };
            let (lo, ro) = l.process_frame(s, s);
            max_abs = max_abs.max(lo.abs()).max(ro.abs());
        }
        assert!(
            max_abs <= ceiling_lin + 1e-6,
            "impulse: above hard ceiling: max_abs={max_abs}, ceiling_lin={ceiling_lin}"
        );
    }

    #[test]
    fn brickwall_only_skips_soft_tier_entirely() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::brickwall_only(), sr);
        assert!(l.effective_soft_ceiling_dbtp().is_none());
        // Drive a hot signal; brickwall must still hold.
        let out = run_sine(&mut l, 440.0, 6.0, 4_800, sr);
        let ceiling_lin = db_to_lin(-0.1);
        let max_abs = out.iter().skip(800).fold(0.0_f32, |a, &b| a.max(b.abs()));
        assert!(max_abs <= ceiling_lin + 1e-6);
        // No soft gain reduction should ever have been recorded.
        assert!(l.soft_gain_reduction_db().abs() < 1e-6);
    }

    // ----------------------------------------------------------------
    // Soft tier: static fallback ceiling
    // ----------------------------------------------------------------

    #[test]
    fn soft_tier_static_ceiling_engages_before_hard() {
        let sr = 48_000.0;
        // Static soft ceiling at -6 dBTP, attack short enough to
        // settle inside the lookahead.
        let cfg = LimiterConfig {
            lookahead_ms: 5.0,
            soft: Some(SoftTierConfig {
                static_ceiling_dbtp: -6.0,
                attack_ms: 1.0,
                release_ms: 100.0,
                ..SoftTierConfig::default()
            }),
            ..LimiterConfig::default()
        };
        let mut l = Limiter::new(cfg, sr);
        // Drive a +6 dB sine — well above the soft ceiling.
        let out = run_sine(&mut l, 440.0, 6.0, 9_600, sr);

        // Output should sit near the soft ceiling, well below hard.
        let soft_ceiling_lin = db_to_lin(-6.0);
        let max_abs = out
            .iter()
            .skip(2_000)
            .fold(0.0_f32, |a, &b| a.max(b.abs()));
        // Allow small overshoot during soft attack (gain hasn't fully
        // settled when the peak arrives), but it must be well under
        // the hard ceiling.
        assert!(
            max_abs <= soft_ceiling_lin * 1.1,
            "output above soft ceiling: max_abs={max_abs}, soft_lin={soft_ceiling_lin}"
        );
        // Soft tier should report meaningful GR; hard tier ideally
        // does very little once the soft tier has settled.
        assert!(
            l.soft_gain_reduction_db() < -3.0,
            "soft GR too small: {}",
            l.soft_gain_reduction_db()
        );
    }

    // ----------------------------------------------------------------
    // Soft tier: dynamic ceiling from program loudness
    // ----------------------------------------------------------------

    #[test]
    fn dynamic_ceiling_tracks_program_loudness() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        // Default max_psr_db = 14.
        l.set_program_loudness_lufs(-18.0);
        let dyn_ceiling = l.effective_soft_ceiling_dbtp().expect("soft tier active");
        assert!(
            (dyn_ceiling - (-4.0)).abs() < 1e-3,
            "expected -4 dBTP, got {dyn_ceiling}"
        );

        // Move the program louder; ceiling rises (and clamps at 0).
        l.set_program_loudness_lufs(-2.0);
        let dyn_ceiling = l.effective_soft_ceiling_dbtp().unwrap();
        assert!(
            (-0.1..=0.0).contains(&dyn_ceiling),
            "expected clamp near 0 dBTP, got {dyn_ceiling}"
        );

        // Clear it; falls back to static.
        l.clear_program_loudness();
        let fallback = l.effective_soft_ceiling_dbtp().unwrap();
        assert!(
            (fallback - (-6.0)).abs() < 1e-3,
            "expected static -6 dBTP, got {fallback}"
        );
    }

    #[test]
    fn dynamic_ceiling_bounds_psr_on_hot_transient() {
        let sr = 48_000.0;
        // Long lookahead and fast soft attack so the soft tier
        // demonstrably catches the transient before the hard tier
        // needs to.
        let cfg = LimiterConfig {
            lookahead_ms: 5.0,
            soft: Some(SoftTierConfig {
                max_psr_db: 14.0,
                static_ceiling_dbtp: -6.0,
                attack_ms: 1.0,
                release_ms: 100.0,
            }),
            ..LimiterConfig::default()
        };
        let mut l = Limiter::new(cfg, sr);
        l.set_program_loudness_lufs(-18.0);
        // Expected dynamic ceiling: -18 + 14 = -4 dBTP ≈ 0.631 lin.
        let dyn_ceil_lin = db_to_lin(-4.0);

        // Slam a +6 dBFS impulse.
        let mut max_after = 0.0_f32;
        for n in 0..4_800_usize {
            let s = if n == 800 { db_to_lin(6.0) } else { 0.0 };
            let (lo, _) = l.process_frame(s, s);
            if n > 700 {
                max_after = max_after.max(lo.abs());
            }
        }
        // Output should be at or below the dynamic soft ceiling with
        // a small ringing margin. Critically, the hard tier should
        // *not* be the thing that catches it — its GR should be small.
        assert!(
            max_after <= dyn_ceil_lin * 1.15,
            "soft tier didn't bound the transient: max={max_after}, dyn_ceil={dyn_ceil_lin}"
        );
        // The hard tier may snap briefly at peak entry (soft envelope
        // hasn't ramped yet), then take its release time to recover.
        // We don't require zero hard engagement here — only that it
        // isn't doing the majority of the work.
        assert!(
            l.hard_gain_reduction_db().abs() < 4.0,
            "hard tier engaged unreasonably: {}",
            l.hard_gain_reduction_db()
        );
    }

    // ----------------------------------------------------------------
    // Misc
    // ----------------------------------------------------------------

    #[test]
    fn nan_inputs_do_not_propagate_with_soft_tier() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        for _ in 0..1_000 {
            let (lo, ro) = l.process_frame(f32::NAN, f32::INFINITY);
            assert!(lo.is_finite() && ro.is_finite());
        }
    }

    #[test]
    fn ceiling_clamps_positive_config_to_zero() {
        let cfg = LimiterConfig {
            ceiling_dbtp: 3.0,
            ..LimiterConfig::default()
        }
        .sanitized();
        assert_eq!(cfg.ceiling_dbtp, 0.0);
    }

    #[test]
    fn set_program_loudness_ignores_non_finite() {
        let sr = 48_000.0;
        let mut l = Limiter::new(LimiterConfig::default(), sr);
        // Establish a baseline.
        l.set_program_loudness_lufs(-20.0);
        let baseline = l.effective_soft_ceiling_dbtp().unwrap();
        // NaN / Inf should be ignored.
        l.set_program_loudness_lufs(f32::NAN);
        assert_eq!(l.effective_soft_ceiling_dbtp().unwrap(), baseline);
        l.set_program_loudness_lufs(f32::INFINITY);
        assert_eq!(l.effective_soft_ceiling_dbtp().unwrap(), baseline);
    }

    #[test]
    fn soft_tier_reduces_perceived_peak_to_loudness_ratio() {
        // The whole point of the soft tier: a transient on top of a
        // quieter program should NOT come out near the hard ceiling.
        let sr = 48_000.0;
        let cfg = LimiterConfig {
            lookahead_ms: 5.0,
            soft: Some(SoftTierConfig {
                max_psr_db: 12.0,
                static_ceiling_dbtp: -8.0,
                attack_ms: 1.0,
                release_ms: 100.0,
            }),
            ..LimiterConfig::default()
        };
        let mut brickwall = Limiter::new(LimiterConfig::brickwall_only(), sr);
        let mut two_tier = Limiter::new(cfg, sr);
        two_tier.set_program_loudness_lufs(-20.0);

        let mut bw_peak = 0.0_f32;
        let mut tt_peak = 0.0_f32;
        for n in 0..4_800_usize {
            // Quiet program with a single big spike.
            let s = if n == 1_200 { db_to_lin(3.0) } else { 0.01 };
            let (lo_bw, _) = brickwall.process_frame(s, s);
            let (lo_tt, _) = two_tier.process_frame(s, s);
            if n > 1_000 {
                bw_peak = bw_peak.max(lo_bw.abs());
                tt_peak = tt_peak.max(lo_tt.abs());
            }
        }
        // Brickwall lets the spike through near the hard ceiling.
        // Two-tier holds it much lower.
        assert!(
            tt_peak < bw_peak * 0.6,
            "soft tier did not meaningfully reduce peak: bw={bw_peak}, tt={tt_peak}"
        );
    }
}
