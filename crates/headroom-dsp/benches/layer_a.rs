//! layer a (per-app level control) microbenchmarks vs the plan §4.7 budget.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use headroom_dsp::{
    Compressor, CompressorConfig, LevelEnvelopes, LevelEnvelopesConfig, Limiter, LimiterConfig,
};

/// 1024-frame quantum at 48 kHz stereo: 2048 interleaved samples, 21.3 ms/block.
const FRAMES: usize = 1024;
const CHANNELS: usize = 2;
const SR: f32 = 48_000.0;
const BLOCK_DT_S: f32 = FRAMES as f32 / SR;

/// noisy-but-bounded synthetic block; realistic value range so branch predictors / fpu
/// exercise the same paths as real audio.
fn make_block() -> Vec<f32> {
    let mut buf = Vec::with_capacity(FRAMES * CHANNELS);
    // two sine partials + tiny dc: peak not pegged to one sample, mean-square not trivially zero.
    let f1 = 220.0 / SR;
    let f2 = 1730.0 / SR;
    for n in 0..FRAMES {
        let t = n as f32;
        let s = 0.4 * (2.0 * std::f32::consts::PI * f1 * t).sin()
            + 0.18 * (2.0 * std::f32::consts::PI * f2 * t).sin()
            + 0.005;
        buf.push(s);
        buf.push(s * 0.92); // slight L/R difference
    }
    buf
}

/// what the audio-thread layer a callback computes per block.
/// hand-rolled loop so the bench measures the candidate code, not iterator combinators.
#[inline]
fn analysis_scan(samples: &[f32]) -> (f32, f32) {
    let mut peak = 0.0_f32;
    let mut sumsq = 0.0_f32;
    for &s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sumsq += s * s;
    }
    let mean_sq = sumsq / samples.len() as f32;
    (peak, mean_sq)
}

fn bench_analysis_scan(c: &mut Criterion) {
    let block = make_block();
    let mut group = c.benchmark_group("layer_a_audio_thread");
    group.throughput(Throughput::Elements((FRAMES * CHANNELS) as u64));
    group.bench_function("analysis_scan_stereo_1024", |b| {
        b.iter(|| {
            let (p, m) = analysis_scan(black_box(&block));
            black_box((p, m));
        });
    });
    group.finish();
}

fn bench_level_envelopes(c: &mut Criterion) {
    let mut env = LevelEnvelopes::new(LevelEnvelopesConfig::default(), BLOCK_DT_S);
    let block = make_block();
    let (peak, mean_sq) = analysis_scan(&block);

    let mut group = c.benchmark_group("layer_a_daemon_thread");
    group.bench_function("level_envelopes_process_block", |b| {
        b.iter(|| {
            let d = env.process_block(black_box(peak), black_box(mean_sq));
            black_box(d);
        });
    });
    group.finish();
}

fn bench_filter_kernels(c: &mut Criterion) {
    // context only: layer a relative to the rt filter's existing per-frame cost.
    let mut comp = Compressor::new(CompressorConfig::default(), SR);
    let mut lim = Limiter::new(LimiterConfig::default(), SR);

    let mut group = c.benchmark_group("filter_reference_per_frame");
    group.throughput(Throughput::Elements(1));
    group.bench_function("compressor_process_frame", |b| {
        b.iter(|| {
            let (l, r) = comp.process_frame(black_box(0.3), black_box(-0.2));
            black_box((l, r));
        });
    });
    group.bench_function("limiter_process_frame", |b| {
        b.iter(|| {
            let (l, r) = lim.process_frame(black_box(0.3), black_box(-0.2));
            black_box((l, r));
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_analysis_scan,
    bench_level_envelopes,
    bench_filter_kernels
);
criterion_main!(benches);
