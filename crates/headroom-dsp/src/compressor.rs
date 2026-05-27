//! feed-forward log-domain compressor with soft-knee static curve.

use crate::util::{db_to_lin, lin_to_db, time_to_alpha};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detector {
    /// max(|l|,|r|). slightly more percussive on transients.
    Peak,
    /// one-pole low-passed mean square. smoother on percussive material.
    Rms,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorConfig {
    /// envelope state is NOT reset while disabled; with typical release constants any residual
    /// transient on re-enable is sub-audible.
    pub enabled: bool,
    pub threshold_db: f32,
    /// >= 1.0
    pub ratio: f32,
    /// 0 = hard knee.
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// `None` = automatic mild boost.
    pub makeup_db: Option<f32>,
    pub detector: Detector,
    /// only used when detector == Rms.
    pub rms_window_ms: f32,
}

impl Default for CompressorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_db: -24.0,
            ratio: 2.5,
            knee_db: 6.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: None,
            detector: Detector::Peak,
            rms_window_ms: 5.0,
        }
    }
}

impl CompressorConfig {
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if self.ratio < 1.0 {
            self.ratio = 1.0;
        }
        self.knee_db = self.knee_db.max(0.0);
        self.attack_ms = self.attack_ms.max(0.0);
        self.release_ms = self.release_ms.max(0.0);
        self.rms_window_ms = self.rms_window_ms.max(0.1);
        self
    }
}

/// stereo-linked.
pub struct Compressor {
    cfg: CompressorConfig,
    sample_rate: f32,
    envelope_db: f32,
    attack_alpha: f32,
    release_alpha: f32,
    rms_state: f32,
    rms_alpha: f32,
    last_gr_db: f32,
}

impl Compressor {
    #[must_use]
    pub fn new(cfg: CompressorConfig, sample_rate: f32) -> Self {
        let cfg = cfg.sanitized();
        Self {
            cfg,
            sample_rate,
            envelope_db: -200.0,
            attack_alpha: time_to_alpha(cfg.attack_ms, sample_rate),
            release_alpha: time_to_alpha(cfg.release_ms, sample_rate),
            rms_state: 0.0,
            rms_alpha: time_to_alpha(cfg.rms_window_ms, sample_rate),
            last_gr_db: 0.0,
        }
    }

    #[must_use]
    pub fn config(&self) -> CompressorConfig {
        self.cfg
    }

    /// negative when compressing.
    #[must_use]
    pub fn gain_reduction_db(&self) -> f32 {
        self.last_gr_db
    }

    /// envelope kept across same-enabled tweaks (don't pop); reset on disabled → enabled so a
    /// stale envelope doesn't duck the first ~release_ms of audio on resume.
    pub fn set_config(&mut self, cfg: CompressorConfig) {
        let cfg = cfg.sanitized();
        let was_disabled = !self.cfg.enabled;
        self.cfg = cfg;
        if was_disabled && self.cfg.enabled {
            self.envelope_db = -200.0;
            self.rms_state = 0.0;
            self.last_gr_db = 0.0;
        }
        self.attack_alpha = time_to_alpha(cfg.attack_ms, self.sample_rate);
        self.release_alpha = time_to_alpha(cfg.release_ms, self.sample_rate);
        self.rms_alpha = time_to_alpha(cfg.rms_window_ms, self.sample_rate);
    }

    pub fn process_frame(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.cfg.enabled {
            // report 0 so meters show "off", not the stale last value.
            self.last_gr_db = 0.0;
            return (left, right);
        }
        let det_lin = match self.cfg.detector {
            Detector::Peak => left.abs().max(right.abs()),
            Detector::Rms => {
                let sq = 0.5 * left.mul_add(left, right * right);
                self.rms_state += self.rms_alpha * (sq - self.rms_state);
                self.rms_state.max(0.0).sqrt()
            }
        };
        let det_db = lin_to_db(det_lin.max(1e-20));

        if det_db > self.envelope_db {
            self.envelope_db += self.attack_alpha * (det_db - self.envelope_db);
        } else {
            self.envelope_db += self.release_alpha * (det_db - self.envelope_db);
        }

        let gr_db = static_curve_gain_reduction(
            self.envelope_db,
            self.cfg.threshold_db,
            self.cfg.ratio,
            self.cfg.knee_db,
        );
        let makeup_db = self
            .cfg
            .makeup_db
            .unwrap_or_else(|| auto_makeup(self.cfg.threshold_db, self.cfg.ratio));

        let lin_gain = db_to_lin(-gr_db + makeup_db);
        self.last_gr_db = -gr_db;

        (left * lin_gain, right * lin_gain)
    }

    pub fn reset(&mut self) {
        self.envelope_db = -200.0;
        self.rms_state = 0.0;
        self.last_gr_db = 0.0;
    }
}

/// returns positive gain reduction (dB) to subtract from the input level.
fn static_curve_gain_reduction(
    input_db: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
) -> f32 {
    let over = input_db - threshold_db;
    if knee_db > 0.0 && over > -knee_db * 0.5 && over < knee_db * 0.5 {
        // quadratic soft knee
        let x = over + knee_db * 0.5;
        let factor = (x * x) / (2.0 * knee_db);
        factor * (1.0 - 1.0 / ratio)
    } else if over > 0.0 {
        over * (1.0 - 1.0 / ratio)
    } else {
        0.0
    }
}

/// half the static gr at 0 dBFS. conservative — the limiter is downstream, don't push it.
fn auto_makeup(threshold_db: f32, ratio: f32) -> f32 {
    let gr_at_zero = (-threshold_db).max(0.0) * (1.0 - 1.0 / ratio);
    gr_at_zero * 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_threshold_is_unity_minus_makeup() {
        let mut c = Compressor::new(CompressorConfig::default(), 48_000.0);
        // Drive a long, low signal and check we land at expected gain.
        let mut last = (0.0_f32, 0.0_f32);
        for _ in 0..10_000 {
            last = c.process_frame(0.01, 0.01);
        }
        // Below threshold: gain reduction is zero, only makeup applied.
        let makeup_db = auto_makeup(-24.0, 2.5);
        let expected = 0.01 * db_to_lin(makeup_db);
        assert!(
            (last.0 - expected).abs() < 1e-3,
            "got {} expected {}",
            last.0,
            expected
        );
        assert!(c.gain_reduction_db().abs() < 0.1);
    }

    #[test]
    fn above_threshold_reduces_gain() {
        let cfg = CompressorConfig {
            threshold_db: -20.0,
            ratio: 4.0,
            knee_db: 0.0, // hard knee for clean math
            attack_ms: 0.1,
            release_ms: 0.1,
            makeup_db: Some(0.0), // no makeup so we test pure reduction
            ..CompressorConfig::default()
        };
        let mut c = Compressor::new(cfg, 48_000.0);
        // Drive ~-6 dBFS = 0.5 in linear.
        let target = 0.5_f32;
        let mut last_out = 0.0;
        for _ in 0..2_000 {
            let (l, _) = c.process_frame(target, target);
            last_out = l;
        }
        // Input is 14 dB above threshold. With ratio 4, GR = 14*(1-0.25) = 10.5 dB.
        // Expected output: -6 - 10.5 = -16.5 dB linear = 0.1496.
        let expected_db = -6.0 - 14.0 * (1.0 - 0.25);
        let expected_lin = db_to_lin(expected_db);
        let got_db = lin_to_db(last_out);
        assert!(
            (got_db - expected_db).abs() < 0.5,
            "got {got_db} expected {expected_db}"
        );
        assert!(c.gain_reduction_db() < -5.0, "gr was {}", c.gain_reduction_db());
        let _ = expected_lin;
    }

    #[test]
    fn ratio_below_one_is_clamped() {
        let cfg = CompressorConfig {
            ratio: 0.5,
            ..CompressorConfig::default()
        }
        .sanitized();
        assert_eq!(cfg.ratio, 1.0);
    }

    #[test]
    fn disabled_compressor_passes_signal_through_unchanged() {
        // Same hot input that would compress hard in the enabled
        // test above. With `enabled: false`, output equals input
        // exactly (no makeup gain, no reduction), and the reporter
        // shows zero GR — so the `transparent` and `bypass-all`
        // profiles actually do what their name claims.
        let cfg = CompressorConfig {
            enabled: false,
            threshold_db: -20.0,
            ratio: 4.0,
            makeup_db: Some(12.0),
            ..CompressorConfig::default()
        };
        let mut c = Compressor::new(cfg, 48_000.0);
        for _ in 0..1_000 {
            let (l, r) = c.process_frame(0.5, 0.5);
            assert_eq!(l, 0.5);
            assert_eq!(r, 0.5);
        }
        assert_eq!(c.gain_reduction_db(), 0.0);
    }

    #[test]
    fn enable_transition_resets_stale_envelope() {
        // Run a loud signal through an enabled compressor to wind
        // the envelope up, then disable + re-enable via set_config.
        // The first sample after re-enable must NOT see the stale
        // envelope (which would otherwise duck the signal until
        // release_ms wound it down). Concretely: with a quiet input
        // after re-enable, the envelope should be at the floor, so
        // GR is zero — same as a freshly-constructed compressor.
        let loud_cfg = CompressorConfig {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 0.1,
            release_ms: 1000.0, // slow release so stale state would otherwise stick
            knee_db: 0.0,
            makeup_db: Some(0.0),
            ..CompressorConfig::default()
        };
        let mut c = Compressor::new(loud_cfg, 48_000.0);
        // Drive hot signal to wind envelope up.
        for _ in 0..2_000 {
            c.process_frame(0.5, 0.5);
        }
        assert!(
            c.gain_reduction_db() < -5.0,
            "precondition: envelope should be wound up; gr={}",
            c.gain_reduction_db()
        );

        // Disable, then re-enable — should reset.
        let mut disabled_cfg = loud_cfg;
        disabled_cfg.enabled = false;
        c.set_config(disabled_cfg);
        c.set_config(loud_cfg);

        // Now drive a quiet signal. With reset envelope, GR should
        // ride near zero; without reset, the stale envelope would
        // bleed gain reduction out over ~release_ms.
        let (l, r) = c.process_frame(0.001, 0.001);
        assert!(
            c.gain_reduction_db().abs() < 0.01,
            "envelope didn't reset across enable transition; gr={}",
            c.gain_reduction_db()
        );
        // Output should be quiet (within makeup-applied scale).
        assert!(l.abs() < 0.01);
        assert!(r.abs() < 0.01);
    }

    #[test]
    fn static_curve_at_threshold_with_soft_knee() {
        // At exactly threshold, soft knee contributes exactly half the
        // ratio's compression amount at the upper knee shoulder.
        let gr = static_curve_gain_reduction(-24.0, -24.0, 4.0, 6.0);
        // At over==0 inside the knee, x = knee/2, factor = knee/8.
        // GR = knee/8 * (1 - 1/4) = 6/8 * 0.75 = 0.5625
        assert!((gr - 0.5625).abs() < 1e-4, "gr={gr}");
    }
}
