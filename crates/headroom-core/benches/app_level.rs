//! microbench: one `AppLevelController::process_block` call

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use headroom_core::app_level::AppLevelController;
use headroom_core::profile::{DeferPolicy, PerAppRule};
use headroom_ipc::RouteRuleMatch;

const BLOCK_DT_S: f32 = 1024.0 / 48_000.0;

fn aggressive_rule() -> PerAppRule {
    PerAppRule {
        match_: RouteRuleMatch::default(),
        enabled: true,
        peak_threshold_db: -6.0,
        rms_target_db: -20.0,
        max_cut_db: 12.0,
        peak_attack_ms: 5.0,
        peak_release_ms: 500.0,
        rms_window_ms: 1500.0,
        smoother_ms: 30.0,
        write_db_threshold: 0.5,
        min_write_interval_ms: 100.0,
        defer_to_user: DeferPolicy::Ceiling,
    }
}

fn bench_process_block(c: &mut Criterion) {
    let mut ctrl = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
    // Hot signal: 0 dBFS peak, ~-3 dB RMS.
    let peak = 1.0_f32;
    let mean_sq = 0.25_f32;

    // Time advances at one block per call to keep the rate-limit gate
    // behaviour realistic — it'd otherwise be `now` reused every iter.
    let mut t = Instant::now();
    let step = Duration::from_millis(21);

    let mut group = c.benchmark_group("app_level_controller");
    group.bench_function("process_block_hot_signal", |b| {
        b.iter(|| {
            t += step;
            let v = ctrl.process_block(black_box(peak), black_box(mean_sq), t);
            black_box(v);
        });
    });

    // A second variant where the signal is below all thresholds —
    // this exercises the "no write" fast path the controller takes
    // most of the time on a quiet system.
    let mut quiet_ctrl = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
    let quiet_peak = 0.01_f32;
    let quiet_mean_sq = 0.0001_f32;
    let mut t2 = Instant::now();
    group.bench_function("process_block_quiet_signal", |b| {
        b.iter(|| {
            t2 += step;
            let v = quiet_ctrl.process_block(
                black_box(quiet_peak),
                black_box(quiet_mean_sq),
                t2,
            );
            black_box(v);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_process_block);
criterion_main!(benches);
