//! amortized-O(1) sliding-window max via monotonic-decreasing deque; bounded capacity, no realloc.

use std::collections::VecDeque;

pub struct SlidingMaxBuffer {
    window: usize,
    counter: u64,
    /// `(index, value)`, monotonically decreasing in value from front.
    deque: VecDeque<(u64, f32)>,
}

impl SlidingMaxBuffer {
    /// 0 clamps to 1.
    #[must_use]
    pub fn new(window: usize) -> Self {
        let window = window.max(1);
        Self {
            window,
            counter: 0,
            deque: VecDeque::with_capacity(window),
        }
    }

    #[must_use]
    pub fn window(&self) -> usize {
        self.window
    }

    /// max over the most recent `window` samples, inclusive of `value`.
    pub fn push_and_max(&mut self, value: f32) -> f32 {
        // drop entries aged out of the window.
        let cutoff = self.counter.saturating_sub(self.window as u64 - 1);
        while let Some(&(idx, _)) = self.deque.front() {
            if idx < cutoff {
                self.deque.pop_front();
            } else {
                break;
            }
        }
        // drop back entries <= value: they can never become the max.
        while let Some(&(_, v)) = self.deque.back() {
            if v <= value {
                self.deque.pop_back();
            } else {
                break;
            }
        }
        self.deque.push_back((self.counter, value));
        self.counter += 1;
        // deque is non-empty (we just pushed).
        self.deque.front().map_or(0.0, |&(_, v)| v)
    }

    pub fn reset(&mut self) {
        self.counter = 0;
        self.deque.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_window_max() {
        let mut s = SlidingMaxBuffer::new(3);
        assert_eq!(s.push_and_max(1.0), 1.0);
        assert_eq!(s.push_and_max(3.0), 3.0);
        assert_eq!(s.push_and_max(2.0), 3.0);
        assert_eq!(s.push_and_max(2.0), 3.0); // 3.0 aged out... actually still in (window=3, last 3 are [3,2,2])
        assert_eq!(s.push_and_max(0.5), 2.0); // window is now [2,2,0.5]
        assert_eq!(s.push_and_max(0.5), 2.0); // [2,0.5,0.5]
        assert_eq!(s.push_and_max(0.5), 0.5); // [0.5,0.5,0.5]
    }

    #[test]
    fn monotonically_decreasing_input() {
        let mut s = SlidingMaxBuffer::new(4);
        for (i, &v) in [5.0_f32, 4.0, 3.0, 2.0, 1.0, 0.5].iter().enumerate() {
            let m = s.push_and_max(v);
            // After window is filled, max is the value `window-1` back.
            let expected = match i {
                0..=3 => 5.0,
                4 => 4.0,
                _ => 3.0,
            };
            assert_eq!(m, expected);
        }
    }

    #[test]
    fn window_one_is_identity() {
        let mut s = SlidingMaxBuffer::new(1);
        for v in [1.0, 2.0, 0.5, 9.0_f32, -3.0] {
            assert_eq!(s.push_and_max(v), v);
        }
    }
}
