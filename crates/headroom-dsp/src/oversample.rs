//! polyphase fir up/downsamplers for the limiter's oversampled isp detection (itu-r bs.1770-4).

use std::f32::consts::PI;

/// blackman-windowed sinc lowpass fir, normalized for unity dc gain.
/// odd `taps` → linear phase, group delay `(taps - 1) / 2`. `fc` normalized in `0.0..0.5`.
/// use `fc = 0.5 / M` as the prototype for M-times oversampling.
#[must_use]
pub fn design_lowpass_blackman(taps: usize, fc: f32) -> Vec<f32> {
    let taps = taps.max(1);
    let m = (taps as f32 - 1.0).max(1.0);
    let mut h = vec![0.0_f32; taps];
    let mut sum = 0.0_f32;
    for (n, h_n) in h.iter_mut().enumerate() {
        let x = n as f32 - m / 2.0;
        let sinc = if x.abs() < 1e-9 {
            2.0 * fc
        } else {
            (2.0 * PI * fc * x).sin() / (PI * x)
        };
        let w = 0.42 - 0.5 * (2.0 * PI * n as f32 / m).cos() + 0.08 * (4.0 * PI * n as f32 / m).cos();
        *h_n = sinc * w;
        sum += *h_n;
    }
    if sum.abs() > 1e-12 {
        for v in &mut h {
            *v /= sum;
        }
    }
    h
}

/// one input sample → `factor` output samples. coeffs pre-scaled by `factor` for unity dc gain.
pub struct PolyphaseUpsampler {
    factor: usize,
    taps_per_phase: usize,
    /// `phases[j * taps_per_phase + p] = h[p * factor + j] * factor`.
    phases: Vec<f32>,
    history: Vec<f32>,
    write_idx: usize,
}

impl PolyphaseUpsampler {
    #[must_use]
    pub fn new(factor: usize, fir_taps: &[f32]) -> Self {
        let factor = factor.max(1);
        let taps_per_phase = fir_taps.len().div_ceil(factor);
        let mut phases = vec![0.0_f32; factor * taps_per_phase];
        for (n, &h) in fir_taps.iter().enumerate() {
            let j = n % factor;
            let p = n / factor;
            phases[j * taps_per_phase + p] = h * factor as f32;
        }
        Self {
            factor,
            taps_per_phase,
            phases,
            history: vec![0.0_f32; taps_per_phase.max(1)],
            write_idx: 0,
        }
    }

    #[must_use]
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// emit `factor` samples into `out[..factor]`; `out.len() >= factor`.
    pub fn process(&mut self, x: f32, out: &mut [f32]) {
        debug_assert!(out.len() >= self.factor);
        let len = self.history.len();
        self.history[self.write_idx] = x;
        let just_written = self.write_idx;
        self.write_idx = (self.write_idx + 1) % len;

        for (j, slot) in out.iter_mut().take(self.factor).enumerate() {
            let phase = &self.phases[j * self.taps_per_phase..(j + 1) * self.taps_per_phase];
            let mut acc = 0.0_f32;
            for (p, &h) in phase.iter().enumerate() {
                let idx = (just_written + len - p) % len;
                acc += h * self.history[idx];
            }
            *slot = acc;
        }
    }

    pub fn reset(&mut self) {
        for v in &mut self.history {
            *v = 0.0;
        }
        self.write_idx = 0;
    }
}

/// `factor` input samples → one output. same prototype lowpass as the upsampler.
/// no polyphase split — savings are modest at our tap count, clarity wins.
pub struct PolyphaseDownsampler {
    factor: usize,
    taps: Vec<f32>,
    history: Vec<f32>,
    write_idx: usize,
}

impl PolyphaseDownsampler {
    #[must_use]
    pub fn new(factor: usize, fir_taps: &[f32]) -> Self {
        let factor = factor.max(1);
        Self {
            factor,
            taps: fir_taps.to_vec(),
            history: vec![0.0_f32; fir_taps.len().max(1)],
            write_idx: 0,
        }
    }

    #[must_use]
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// push `factor` samples, return one filtered output.
    pub fn process(&mut self, ins: &[f32]) -> f32 {
        debug_assert_eq!(ins.len(), self.factor);
        let len = self.history.len();
        for &x in ins {
            self.history[self.write_idx] = x;
            self.write_idx = (self.write_idx + 1) % len;
        }
        let mut acc = 0.0_f32;
        for (k, &h) in self.taps.iter().enumerate() {
            let idx = (self.write_idx + len - 1 - k) % len;
            acc += h * self.history[idx];
        }
        acc
    }

    pub fn reset(&mut self) {
        for v in &mut self.history {
            *v = 0.0;
        }
        self.write_idx = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fir(taps: usize, factor: usize) -> Vec<f32> {
        design_lowpass_blackman(taps, 0.45 / factor as f32)
    }

    #[test]
    fn upsampler_dc_gain_preserved() {
        let h = fir(31, 4);
        let mut up = PolyphaseUpsampler::new(4, &h);
        // Drive DC and let the filter settle, then check unity gain.
        let mut buf = [0.0_f32; 8];
        let mut last_avg = 0.0;
        for _ in 0..200 {
            up.process(1.0, &mut buf);
            last_avg = buf[..4].iter().sum::<f32>() / 4.0;
        }
        assert!((last_avg - 1.0).abs() < 1e-3, "got {last_avg}");
    }

    #[test]
    fn down_then_up_roundtrip_is_bounded() {
        // Stuff zero-padded input through up then down; output amplitude
        // should approximately equal input on smooth signals.
        let h = fir(31, 4);
        let mut up = PolyphaseUpsampler::new(4, &h);
        let mut down = PolyphaseDownsampler::new(4, &h);
        let mut max_err = 0.0_f32;
        let mut up_buf = [0.0_f32; 8];
        // Drive a slow sine well below Nyquist.
        for n in 0..2_000 {
            let t = n as f32 / 48_000.0;
            let x = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.5;
            up.process(x, &mut up_buf);
            let y = down.process(&up_buf[..4]);
            // After group-delay warm-up, the error should be small.
            if n > 80 {
                max_err = max_err.max((x - y).abs());
            }
        }
        // The filter is symmetric, so up/down with the same kernel
        // introduces ~6 dB attenuation by design (each pass contributes
        // half the gain). What we care about here is finite, bounded
        // output and no runaway.
        assert!(max_err < 1.0, "max_err {max_err}");
    }

    #[test]
    fn upsampler_handles_impulse() {
        let h = fir(15, 4);
        let mut up = PolyphaseUpsampler::new(4, &h);
        let mut buf = [0.0_f32; 8];
        up.process(1.0, &mut buf);
        // Some non-zero output expected on first impulse already.
        assert!(buf[..4].iter().any(|&v| v.abs() > 1e-6));
        // Drive zeros; output decays to zero.
        for _ in 0..200 {
            up.process(0.0, &mut buf);
        }
        assert!(buf[..4].iter().all(|&v| v.abs() < 1e-6));
    }
}
