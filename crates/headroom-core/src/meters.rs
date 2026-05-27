//! bus meter snapshot shared audio-thread → agc; audio side writes via try_lock, never blocks

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BusMetrics {
    /// negative when reducing
    pub compressor_gr_db: f32,
    /// min of soft and hard gain, in dB
    pub limiter_total_gr_db: f32,
    pub limiter_soft_gr_db: f32,
    /// non-zero only when the soft tier wasn't enough — alarm condition
    pub limiter_hard_gr_db: f32,
    /// peak the limiter saw on its input (not the bounded output); for tuning soft-tier headroom
    pub true_peak_dbtp: f32,
}

pub type SharedBusMetrics = Arc<Mutex<BusMetrics>>;

#[must_use]
pub fn shared() -> SharedBusMetrics {
    Arc::new(Mutex::new(BusMetrics::default()))
}

/// rolling timing stats for the bus filter's `playback_process`; lock-free atomics from the
/// audio thread, read on the agc tick. detects BUSY spikes + ring imbalance.
#[derive(Debug, Default)]
pub struct PlaybackTiming {
    pub call_count: AtomicU64,
    pub sum_us: AtomicU64,
    pub max_us: AtomicU64,
    pub spike_count: AtomicU64,
    pub last_spike_us: AtomicU64,
    /// `call_count` when the last spike fired; reader compares against its prior snapshot
    pub last_spike_at_call: AtomicU64,
    /// samples zero-filled because the capture→playback ring was empty. non-zero delta = bug
    /// (producer/consumer not lined up within a quantum → audible drop-outs).
    pub samples_starved: AtomicU64,
    /// samples dropped because the ring was full; mirror of `samples_starved` (ring imbalance)
    pub samples_dropped: AtomicU64,
    /// short/misaligned rt buffers; logged off-thread
    pub format_errors: AtomicU64,
}

impl PlaybackTiming {
    /// steady-state cost scales with quantum (~240 μs @ 1024 frames, ~2.2 ms @ 8192 release);
    /// 5 ms sits above both, fires only on real outliers.
    pub const SPIKE_THRESHOLD_US: u64 = 5_000;

    #[inline]
    pub fn record(&self, dur_us: u64) {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(dur_us, Ordering::Relaxed);
        let mut cur_max = self.max_us.load(Ordering::Relaxed);
        while dur_us > cur_max {
            match self.max_us.compare_exchange_weak(
                cur_max,
                dur_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur_max = v,
            }
        }
        if dur_us > Self::SPIKE_THRESHOLD_US {
            let count = self.call_count.load(Ordering::Relaxed);
            self.spike_count.fetch_add(1, Ordering::Relaxed);
            self.last_spike_us.store(dur_us, Ordering::Relaxed);
            self.last_spike_at_call.store(count, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_starved(&self, n: u64) {
        if n > 0 {
            self.samples_starved.fetch_add(n, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_dropped(&self, n: u64) {
        if n > 0 {
            self.samples_dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_format_error(&self) {
        self.format_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> PlaybackTimingSnapshot {
        PlaybackTimingSnapshot {
            call_count: self.call_count.load(Ordering::Relaxed),
            sum_us: self.sum_us.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
            spike_count: self.spike_count.load(Ordering::Relaxed),
            last_spike_us: self.last_spike_us.load(Ordering::Relaxed),
            last_spike_at_call: self.last_spike_at_call.load(Ordering::Relaxed),
            samples_starved: self.samples_starved.load(Ordering::Relaxed),
            samples_dropped: self.samples_dropped.load(Ordering::Relaxed),
            format_errors: self.format_errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PlaybackTimingSnapshot {
    pub call_count: u64,
    pub sum_us: u64,
    pub max_us: u64,
    pub spike_count: u64,
    pub last_spike_us: u64,
    pub last_spike_at_call: u64,
    pub samples_starved: u64,
    pub samples_dropped: u64,
    pub format_errors: u64,
}

pub type SharedPlaybackTiming = Arc<PlaybackTiming>;

#[must_use]
pub fn shared_timing() -> SharedPlaybackTiming {
    Arc::new(PlaybackTiming::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_zero() {
        let m = BusMetrics::default();
        assert_eq!(m.compressor_gr_db, 0.0);
        assert_eq!(m.limiter_total_gr_db, 0.0);
        assert_eq!(m.limiter_soft_gr_db, 0.0);
        assert_eq!(m.limiter_hard_gr_db, 0.0);
        assert_eq!(m.true_peak_dbtp, 0.0);
    }

    #[test]
    fn record_format_error_accumulates_in_snapshot() {
        let t = PlaybackTiming::default();
        assert_eq!(t.snapshot().format_errors, 0);
        t.record_format_error();
        t.record_format_error();
        assert_eq!(t.snapshot().format_errors, 2);
    }

    #[test]
    fn shared_is_cheap_to_clone() {
        let a = shared();
        let b = a.clone();
        *a.lock() = BusMetrics {
            compressor_gr_db: -3.0,
            limiter_total_gr_db: -1.0,
            limiter_soft_gr_db: -1.0,
            limiter_hard_gr_db: 0.0,
            true_peak_dbtp: -0.5,
        };
        let snap = *b.lock();
        assert!((snap.compressor_gr_db - -3.0).abs() < 1e-6);
        assert!((snap.true_peak_dbtp - -0.5).abs() < 1e-6);
    }
}
