//! exponential attack/release envelope follower.

use crate::util::time_to_alpha;

pub struct AttackRelease {
    attack_alpha: f32,
    release_alpha: f32,
    state: f32,
}

impl AttackRelease {
    #[must_use]
    pub fn new(attack_ms: f32, release_ms: f32, sample_rate: f32) -> Self {
        Self {
            attack_alpha: time_to_alpha(attack_ms, sample_rate),
            release_alpha: time_to_alpha(release_ms, sample_rate),
            state: 0.0,
        }
    }

    pub fn set_times(&mut self, attack_ms: f32, release_ms: f32, sample_rate: f32) {
        self.attack_alpha = time_to_alpha(attack_ms, sample_rate);
        self.release_alpha = time_to_alpha(release_ms, sample_rate);
    }

    /// attack on rising input, release on falling.
    pub fn process_peak(&mut self, target: f32) -> f32 {
        if target > self.state {
            self.state += self.attack_alpha * (target - self.state);
        } else {
            self.state += self.release_alpha * (target - self.state);
        }
        self.state
    }

    /// inverse of process_peak: attack on falling gain, release on rising (recover to unity).
    pub fn process_gain(&mut self, target: f32) -> f32 {
        if target < self.state {
            self.state += self.attack_alpha * (target - self.state);
        } else {
            self.state += self.release_alpha * (target - self.state);
        }
        self.state
    }

    #[must_use]
    pub fn state(&self) -> f32 {
        self.state
    }

    pub fn reset(&mut self, value: f32) {
        self.state = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_mode_attacks_fast_releases_slow() {
        let fs = 48_000.0;
        let mut env = AttackRelease::new(0.1, 100.0, fs);
        // Drive to 1.0 and let it settle.
        for _ in 0..100 {
            env.process_peak(1.0);
        }
        assert!(env.state() > 0.99);
        // Drop input to 0.0 and verify slow decay.
        env.process_peak(0.0);
        assert!(env.state() > 0.999);
        for _ in 0..10 {
            env.process_peak(0.0);
        }
        // Still well above zero on the release time scale.
        assert!(env.state() > 0.8);
    }

    #[test]
    fn gain_mode_attacks_on_drop() {
        let fs = 48_000.0;
        let mut env = AttackRelease::new(0.1, 100.0, fs);
        env.reset(1.0);
        // Demand a gain drop. Should snap down quickly.
        for _ in 0..100 {
            env.process_gain(0.5);
        }
        assert!(env.state() < 0.51);
    }
}
