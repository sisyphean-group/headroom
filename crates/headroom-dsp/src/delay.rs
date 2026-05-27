//! fixed-length fifo sample delay.

pub struct DelayLine {
    buf: Vec<f32>,
    write_idx: usize,
}

impl DelayLine {
    /// 0 clamps to 1 (one-sample identity minimum).
    #[must_use]
    pub fn new(samples: usize) -> Self {
        Self {
            buf: vec![0.0; samples.max(1)],
            write_idx: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// write `x`, return the sample written `len` calls ago.
    pub fn push_pop(&mut self, x: f32) -> f32 {
        let out = self.buf[self.write_idx];
        self.buf[self.write_idx] = x;
        self.write_idx = (self.write_idx + 1) % self.buf.len();
        out
    }

    pub fn reset(&mut self) {
        for v in &mut self.buf {
            *v = 0.0;
        }
        self.write_idx = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_exactly_n_samples() {
        let mut d = DelayLine::new(4);
        let expected = [0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0];
        let inputs = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        for (i, &x) in inputs.iter().enumerate() {
            let y = d.push_pop(x);
            assert!((y - expected[i]).abs() < 1e-9, "i={i} y={y}");
        }
    }

    #[test]
    fn zero_length_clamps_to_one() {
        let mut d = DelayLine::new(0);
        assert_eq!(d.len(), 1);
        assert_eq!(d.push_pop(1.0), 0.0);
        assert_eq!(d.push_pop(2.0), 1.0);
    }
}
