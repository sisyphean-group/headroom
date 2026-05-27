//! db <-> linear conversions, time constants.

/// lower bound to avoid `log10(0)`.
pub const PEAK_FLOOR: f32 = 1e-20;

/// inputs at or below [`PEAK_FLOOR`] clamp to `-200 dB`.
#[must_use]
pub fn lin_to_db(x: f32) -> f32 {
    if x <= PEAK_FLOOR {
        -200.0
    } else {
        20.0 * x.log10()
    }
}

#[must_use]
pub fn db_to_lin(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// one-pole smoother coefficient for `y[n] = y[n-1] + alpha * (x[n] - y[n-1])`.
/// alpha = `1 - exp(-1 / (tau * fs))`, tau = `time_ms / 1000`. time_ms <= 0 → 1.0 (instant).
#[must_use]
pub fn time_to_alpha(time_ms: f32, sample_rate: f32) -> f32 {
    if time_ms <= 0.0 || sample_rate <= 0.0 {
        1.0
    } else {
        let tau_samples = (time_ms * 1e-3) * sample_rate;
        1.0 - (-1.0 / tau_samples).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_round_trips() {
        for db in [-60.0, -20.0, -6.0, -0.1, 0.0, 3.0, 6.0_f32] {
            let lin = db_to_lin(db);
            let back = lin_to_db(lin);
            assert!((back - db).abs() < 1e-3, "db={db} back={back}");
        }
    }

    #[test]
    fn time_to_alpha_endpoints() {
        assert_eq!(time_to_alpha(0.0, 48_000.0), 1.0);
        assert!(time_to_alpha(1000.0, 48_000.0) < 0.01);
        // Very fast attack: alpha approaches 1.
        let a_fast = time_to_alpha(0.01, 48_000.0);
        assert!(a_fast > 0.05, "fast alpha was {a_fast}");
    }
}
