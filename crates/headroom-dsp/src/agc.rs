//! anti-zipper smoother toward the control thread's agc target.

use crate::util::{db_to_lin, time_to_alpha};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgcGainConfig {
    /// smoother tau (ms). small enough to chase a 50 ms control tick without zippering,
    /// large enough not to itself act as a gain envelope.
    pub anti_zipper_ms: f32,
}

impl Default for AgcGainConfig {
    fn default() -> Self {
        Self {
            anti_zipper_ms: 5.0,
        }
    }
}

pub struct AgcGain {
    cfg: AgcGainConfig,
    sample_rate: f32,
    target_db: f32,
    current_db: f32,
    alpha: f32,
    enabled: bool,
}

impl AgcGain {
    #[must_use]
    pub fn new(cfg: AgcGainConfig, sample_rate: f32) -> Self {
        Self {
            cfg,
            sample_rate,
            target_db: 0.0,
            current_db: 0.0,
            alpha: time_to_alpha(cfg.anti_zipper_ms, sample_rate),
            enabled: true,
        }
    }

    pub fn set_target_db(&mut self, db: f32) {
        if db.is_finite() {
            self.target_db = db;
        }
    }

    /// disabling pushes target to 0 dB so an active boost/cut unwinds at the smoother rate
    /// rather than snapping.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.target_db = 0.0;
        }
    }

    pub fn set_config(&mut self, cfg: AgcGainConfig) {
        self.cfg = cfg;
        self.alpha = time_to_alpha(cfg.anti_zipper_ms, self.sample_rate);
    }

    #[must_use]
    pub fn config(&self) -> AgcGainConfig {
        self.cfg
    }

    #[must_use]
    pub fn current_db(&self) -> f32 {
        self.current_db
    }

    #[must_use]
    pub fn target_db(&self) -> f32 {
        self.target_db
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn process_frame(&mut self, l: f32, r: f32) -> (f32, f32) {
        self.current_db += self.alpha * (self.target_db - self.current_db);
        let gain = db_to_lin(self.current_db);
        (l * gain, r * gain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::lin_to_db;

    const SR: f32 = 48_000.0;

    #[test]
    fn unity_at_zero_db() {
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        for _ in 0..100 {
            let (l, r) = agc.process_frame(0.5, -0.3);
            assert!((l - 0.5).abs() < 1e-6);
            assert!((r - -0.3).abs() < 1e-6);
        }
    }

    #[test]
    fn smooths_toward_target() {
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        agc.set_target_db(6.0);
        // After ~5 ms (one anti-zipper tau), current_db should be in
        // the ~63% region.
        let samples = (0.005 * SR) as usize;
        for _ in 0..samples {
            let _ = agc.process_frame(0.0, 0.0);
        }
        let cur = agc.current_db();
        assert!(
            (cur - 6.0 * 0.63).abs() < 0.5,
            "expected ~3.8 dB after one tau, got {cur}"
        );
        // Settle.
        for _ in 0..(SR as usize) {
            let _ = agc.process_frame(0.0, 0.0);
        }
        assert!((agc.current_db() - 6.0).abs() < 0.01);
    }

    #[test]
    fn applies_gain_to_signal() {
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        agc.set_target_db(6.0);
        // Run long enough to settle.
        for _ in 0..(SR as usize) {
            let _ = agc.process_frame(0.0, 0.0);
        }
        let (l, r) = agc.process_frame(0.5, 0.5);
        // +6 dB = factor of ~2.0.
        assert!((l / 0.5 - 2.0).abs() < 0.05, "got {l}");
        assert!((r / 0.5 - 2.0).abs() < 0.05);
    }

    #[test]
    fn disable_unwinds_back_to_unity() {
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        agc.set_target_db(6.0);
        for _ in 0..(SR as usize) {
            let _ = agc.process_frame(0.0, 0.0);
        }
        assert!((agc.current_db() - 6.0).abs() < 0.01);

        agc.set_enabled(false);
        for _ in 0..(SR as usize) {
            let _ = agc.process_frame(0.0, 0.0);
        }
        assert!(agc.current_db().abs() < 0.01, "got {}", agc.current_db());
    }

    #[test]
    fn rejects_non_finite_target() {
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        agc.set_target_db(3.0);
        agc.set_target_db(f32::NAN);
        assert!((agc.target_db() - 3.0).abs() < 1e-6);
        agc.set_target_db(f32::INFINITY);
        assert!((agc.target_db() - 3.0).abs() < 1e-6);
    }

    #[test]
    fn lin_round_trip_check() {
        // Sanity: after settling, gain at target_db should produce
        // peak that matches lin_to_db.
        let mut agc = AgcGain::new(AgcGainConfig::default(), SR);
        agc.set_target_db(-6.0);
        for _ in 0..(SR as usize) {
            let _ = agc.process_frame(0.0, 0.0);
        }
        let (l, _) = agc.process_frame(1.0, 1.0);
        assert!((lin_to_db(l) - -6.0).abs() < 0.05);
    }
}
