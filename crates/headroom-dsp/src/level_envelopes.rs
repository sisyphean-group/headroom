//! block-rate two-tier (peak + rms) level detector for layer a (per-app level control).
//!
//! recovery is implicit: each envelope releases at its own constant, so neither path sticks
//! once input drops.

use crate::util::{lin_to_db, time_to_alpha};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelEnvelopesConfig {
    /// peak envelope above this → cut. catches transient bursts.
    pub peak_threshold_db: f32,
    /// smoothed rms above this → cut. catches sustained loudness mismatch.
    pub rms_target_db: f32,
    /// cap on max(peak, rms) reduction.
    pub max_cut_db: f32,
    pub peak_attack_ms: f32,
    pub peak_release_ms: f32,
    /// one-pole tau on the mean-square input.
    pub rms_window_ms: f32,
}

impl Default for LevelEnvelopesConfig {
    fn default() -> Self {
        Self {
            peak_threshold_db: -6.0,
            rms_target_db: -20.0,
            max_cut_db: 12.0,
            peak_attack_ms: 5.0,
            peak_release_ms: 500.0,
            rms_window_ms: 1500.0,
        }
    }
}

impl LevelEnvelopesConfig {
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if self.peak_threshold_db > 0.0 {
            self.peak_threshold_db = 0.0;
        }
        if self.rms_target_db > 0.0 {
            self.rms_target_db = 0.0;
        }
        if !self.max_cut_db.is_finite() || self.max_cut_db < 0.0 {
            self.max_cut_db = 0.0;
        }
        for v in [
            &mut self.peak_attack_ms,
            &mut self.peak_release_ms,
            &mut self.rms_window_ms,
        ] {
            if !v.is_finite() || *v < 0.0 {
                *v = 0.0;
            }
        }
        self
    }
}

pub struct LevelEnvelopes {
    cfg: LevelEnvelopesConfig,
    block_dt_s: f32,
    peak_attack_alpha: f32,
    peak_release_alpha: f32,
    rms_alpha: f32,
    /// starts at floor so the first push doesn't trip the threshold.
    peak_env_db: f32,
    rms_smoothed_mean_sq: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelDecision {
    pub peak_reduction_db: f32,
    pub rms_reduction_db: f32,
    /// `min(max_cut, max(peak, rms))`, always >= 0. 0 = leave channelVolumes alone.
    pub total_reduction_db: f32,
}

impl LevelEnvelopes {
    /// `block_dt_s` = samples_per_block / sample_rate; up to ~100 ms tracks fine for v0.
    #[must_use]
    pub fn new(cfg: LevelEnvelopesConfig, block_dt_s: f32) -> Self {
        let cfg = cfg.sanitized();
        let (peak_attack_alpha, peak_release_alpha, rms_alpha) = compute_alphas(&cfg, block_dt_s);
        Self {
            cfg,
            block_dt_s,
            peak_attack_alpha,
            peak_release_alpha,
            rms_alpha,
            peak_env_db: -200.0,
            rms_smoothed_mean_sq: 0.0,
        }
    }

    #[must_use]
    pub fn config(&self) -> LevelEnvelopesConfig {
        self.cfg
    }

    #[must_use]
    pub fn block_dt_s(&self) -> f32 {
        self.block_dt_s
    }

    /// recomputes alphas; doesn't reset envelope state (live tweaks stay artefact-free).
    pub fn set_config(&mut self, cfg: LevelEnvelopesConfig) {
        let cfg = cfg.sanitized();
        let (a_a, a_r, a_rms) = compute_alphas(&cfg, self.block_dt_s);
        self.cfg = cfg;
        self.peak_attack_alpha = a_a;
        self.peak_release_alpha = a_r;
        self.rms_alpha = a_rms;
    }

    /// re-derives alphas. call when the audio thread's quantum changes.
    pub fn set_block_dt(&mut self, dt_s: f32) {
        if dt_s <= 0.0 || !dt_s.is_finite() || (dt_s - self.block_dt_s).abs() < 1e-9 {
            return;
        }
        self.block_dt_s = dt_s;
        let (a_a, a_r, a_rms) = compute_alphas(&self.cfg, dt_s);
        self.peak_attack_alpha = a_a;
        self.peak_release_alpha = a_r;
        self.rms_alpha = a_rms;
    }

    /// `peak_lin` = per-block max|x|, `mean_sq_lin` = per-block Σx²/N.
    pub fn process_block(&mut self, peak_lin: f32, mean_sq_lin: f32) -> LevelDecision {
        let peak_lin = peak_lin.max(0.0);
        let mean_sq_lin = mean_sq_lin.max(0.0);

        let target_db = lin_to_db(peak_lin);
        if target_db > self.peak_env_db {
            self.peak_env_db += self.peak_attack_alpha * (target_db - self.peak_env_db);
        } else {
            self.peak_env_db += self.peak_release_alpha * (target_db - self.peak_env_db);
        }

        // smooth in the linear-power domain (canonical R128/IEC rms detector), then to dB.
        self.rms_smoothed_mean_sq += self.rms_alpha * (mean_sq_lin - self.rms_smoothed_mean_sq);
        // 20*log10(sqrt(mean_sq)) = 10*log10(mean_sq)
        let rms_db = 10.0 * self.rms_smoothed_mean_sq.max(1e-30).log10();

        let peak_reduction_db = (self.peak_env_db - self.cfg.peak_threshold_db).max(0.0);
        let rms_reduction_db = (rms_db - self.cfg.rms_target_db).max(0.0);
        let combined = peak_reduction_db.max(rms_reduction_db);
        let total_reduction_db = combined.min(self.cfg.max_cut_db);

        LevelDecision {
            peak_reduction_db,
            rms_reduction_db,
            total_reduction_db,
        }
    }

    /// reset envelope state; call when re-attaching after a deference period.
    pub fn reset(&mut self) {
        self.peak_env_db = -200.0;
        self.rms_smoothed_mean_sq = 0.0;
    }
}

fn compute_alphas(cfg: &LevelEnvelopesConfig, block_dt_s: f32) -> (f32, f32, f32) {
    let block_dt_ms = block_dt_s * 1000.0;
    let block_rate = if block_dt_s > 0.0 { 1.0 / block_dt_s } else { 1.0 };
    // time_to_alpha against block rate, not sample rate: smoothers run at block boundaries,
    // one (peak, mean_sq) pair per block. time_to_alpha is rate-agnostic.
    let attack = time_to_alpha(cfg.peak_attack_ms, block_rate);
    let release = time_to_alpha(cfg.peak_release_ms, block_rate);
    let rms = time_to_alpha(cfg.rms_window_ms, block_rate);
    let _ = block_dt_ms; // currently informational
    (attack, release, rms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::db_to_lin;

    /// 1024-frame quantum at 48 kHz.
    const BLOCK_DT_S: f32 = 1024.0 / 48_000.0;

    fn run_steady(env: &mut LevelEnvelopes, peak_lin: f32, mean_sq_lin: f32, blocks: usize) -> LevelDecision {
        let mut last = env.process_block(peak_lin, mean_sq_lin);
        for _ in 1..blocks {
            last = env.process_block(peak_lin, mean_sq_lin);
        }
        last
    }

    #[test]
    fn quiet_signal_produces_no_reduction() {
        let mut env = LevelEnvelopes::new(LevelEnvelopesConfig::default(), BLOCK_DT_S);
        let quiet = db_to_lin(-30.0);
        let mean_sq = quiet * quiet;
        let dec = run_steady(&mut env, quiet, mean_sq, 200);
        assert_eq!(dec.peak_reduction_db, 0.0);
        assert_eq!(dec.rms_reduction_db, 0.0);
        assert_eq!(dec.total_reduction_db, 0.0);
    }

    #[test]
    fn peak_above_threshold_requests_cut() {
        let cfg = LevelEnvelopesConfig {
            peak_threshold_db: -6.0,
            // Long RMS window so the slow path doesn't dominate.
            rms_target_db: 0.0,
            rms_window_ms: 5_000.0,
            ..Default::default()
        };
        let mut env = LevelEnvelopes::new(cfg, BLOCK_DT_S);
        // 0 dBFS peak: 6 dB over threshold.
        let peak = db_to_lin(0.0);
        let mean_sq = (peak * peak) * 0.05; // low rms (intermittent peak)
        let dec = run_steady(&mut env, peak, mean_sq, 200);
        assert!(
            (dec.peak_reduction_db - 6.0).abs() < 0.5,
            "expected ~6 dB peak cut, got {}",
            dec.peak_reduction_db
        );
        assert_eq!(dec.rms_reduction_db, 0.0);
        assert!((dec.total_reduction_db - 6.0).abs() < 0.5);
    }

    #[test]
    fn rms_above_target_requests_cut() {
        let cfg = LevelEnvelopesConfig {
            // Push peak threshold up so only RMS engages.
            peak_threshold_db: 0.0,
            rms_target_db: -20.0,
            rms_window_ms: 200.0, // shorter so test converges quickly
            ..Default::default()
        };
        let mut env = LevelEnvelopes::new(cfg, BLOCK_DT_S);
        // Sustained -10 dBFS RMS: 10 dB above target.
        let rms_lin = db_to_lin(-10.0);
        let mean_sq = rms_lin * rms_lin;
        // Peak set just below threshold so peak detector stays asleep.
        let peak = db_to_lin(-1.0);
        let dec = run_steady(&mut env, peak, mean_sq, 200);
        assert_eq!(dec.peak_reduction_db, 0.0);
        assert!(
            (dec.rms_reduction_db - 10.0).abs() < 0.5,
            "expected ~10 dB RMS cut, got {}",
            dec.rms_reduction_db
        );
    }

    #[test]
    fn combined_takes_max_of_peak_and_rms() {
        let cfg = LevelEnvelopesConfig {
            peak_threshold_db: -6.0,
            rms_target_db: -20.0,
            rms_window_ms: 200.0,
            max_cut_db: 100.0,
            ..Default::default()
        };
        let mut env = LevelEnvelopes::new(cfg, BLOCK_DT_S);
        let peak = db_to_lin(0.0); // 6 dB over
        let rms_lin = db_to_lin(-10.0); // 10 dB over
        let mean_sq = rms_lin * rms_lin;
        let dec = run_steady(&mut env, peak, mean_sq, 200);
        assert!((dec.peak_reduction_db - 6.0).abs() < 0.5);
        assert!((dec.rms_reduction_db - 10.0).abs() < 0.5);
        assert!(
            (dec.total_reduction_db - 10.0).abs() < 0.5,
            "max(6, 10) ≈ 10, got {}",
            dec.total_reduction_db
        );
    }

    #[test]
    fn total_reduction_is_clamped_to_max_cut_db() {
        let cfg = LevelEnvelopesConfig {
            peak_threshold_db: -30.0,
            rms_target_db: -30.0,
            rms_window_ms: 50.0,
            max_cut_db: 3.0, // tight cap
            ..Default::default()
        };
        let mut env = LevelEnvelopes::new(cfg, BLOCK_DT_S);
        let peak = db_to_lin(0.0); // 30 dB over
        let rms_lin = db_to_lin(-5.0);
        let mean_sq = rms_lin * rms_lin;
        let dec = run_steady(&mut env, peak, mean_sq, 200);
        assert!(dec.peak_reduction_db > 20.0);
        assert!(
            (dec.total_reduction_db - 3.0).abs() < 1e-3,
            "total clamped to max_cut_db, got {}",
            dec.total_reduction_db
        );
    }

    #[test]
    fn peak_envelope_releases_after_burst() {
        let cfg = LevelEnvelopesConfig {
            peak_threshold_db: -6.0,
            rms_target_db: 0.0,
            rms_window_ms: 5_000.0,
            peak_attack_ms: 5.0,
            peak_release_ms: 100.0,
            ..Default::default()
        };
        let mut env = LevelEnvelopes::new(cfg, BLOCK_DT_S);
        // Burst.
        for _ in 0..20 {
            env.process_block(db_to_lin(0.0), 0.0);
        }
        let burst = env.process_block(db_to_lin(0.0), 0.0);
        assert!(burst.peak_reduction_db > 5.0);

        // Silence.
        for _ in 0..200 {
            env.process_block(0.0, 0.0);
        }
        let quiet = env.process_block(0.0, 0.0);
        assert!(
            quiet.peak_reduction_db < 0.5,
            "expected ~0 after release, got {}",
            quiet.peak_reduction_db
        );
    }

    #[test]
    fn set_config_updates_alphas_without_reset() {
        let mut env = LevelEnvelopes::new(LevelEnvelopesConfig::default(), BLOCK_DT_S);
        for _ in 0..100 {
            env.process_block(db_to_lin(-3.0), 0.0);
        }
        let before = env.process_block(db_to_lin(-3.0), 0.0);
        // Tighter threshold; envelope state preserved across the swap.
        env.set_config(LevelEnvelopesConfig {
            peak_threshold_db: -12.0,
            ..LevelEnvelopesConfig::default()
        });
        let after = env.process_block(db_to_lin(-3.0), 0.0);
        assert!(
            after.peak_reduction_db > before.peak_reduction_db,
            "tighter threshold should request more cut"
        );
    }

    #[test]
    fn set_block_dt_recomputes_alphas() {
        let mut env = LevelEnvelopes::new(LevelEnvelopesConfig::default(), BLOCK_DT_S);
        let original_attack = env.peak_attack_alpha;
        // Double the block period — slower block rate → smaller alpha
        // for the same time constant.
        env.set_block_dt(BLOCK_DT_S * 2.0);
        assert!(env.peak_attack_alpha > original_attack);
    }

    #[test]
    fn reset_returns_to_idle_state() {
        let mut env = LevelEnvelopes::new(LevelEnvelopesConfig::default(), BLOCK_DT_S);
        for _ in 0..200 {
            env.process_block(db_to_lin(0.0), db_to_lin(-3.0));
        }
        env.reset();
        let dec = env.process_block(0.0, 0.0);
        assert_eq!(dec.peak_reduction_db, 0.0);
        assert_eq!(dec.rms_reduction_db, 0.0);
    }
}
