//! per-app level control (layer a): pure controller logic, pipewire-free

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use headroom_dsp::{LevelDecision, LevelEnvelopes, LevelEnvelopesConfig};

use crate::profile::{DeferPolicy, PerAppRule, PerAppSection};
use crate::routing;
use crate::routing::PwNodeInfo;

// knob defaults live on `PerAppRule`; these are only the fallback for the synthetic
// `default_enabled` rule.
const FALLBACK_WRITE_DB_THRESHOLD: f32 = 0.5;
const FALLBACK_MIN_WRITE_INTERVAL_MS: f32 = 100.0;
const FALLBACK_SMOOTHER_MS: f32 = 30.0;

/// below this gain we skip compensation: a muted stream (`last_written_lin ≈ 0`) would amplify
/// floor noise ~1000×, peg max-cut, and lock at mute even after unmute. −40 dB is below any
/// realistic `max_cut_db` yet above noise-floor amplification.
const GAIN_COMPENSATION_FLOOR: f32 = 0.01;

/// cap on synthetic silent blocks per `tick_silent` pass; past this the envelopes have fully
/// released anyway, so short-circuit to `reset()` rather than spin.
const MAX_SILENT_CATCHUP_BLOCKS: u32 = 500;

/// echo-suppression window. comparing only `last_written_lin` is racy: an echo of write A can
/// arrive after the controller wrote B (param events lag/reorder under 10 Hz writes), so A is
/// misread as a user ceiling and self-locks the stream. 16 ≈ 1.6 s at the 100 ms min interval.
const ECHO_HISTORY: usize = 16;

/// looser than 1e-4 to absorb the f32 round-trip through pipewire's pod (de)serialization.
const ECHO_TOLERANCE: f32 = 1e-3;

pub struct AppLevelController {
    /// stored by value so the controller is detached from the profile lifetime
    rule: PerAppRule,
    envelopes: LevelEnvelopes,
    /// single-pole, alpha from `rule.smoother_ms`
    smoothed_reduction_db: f32,
    smoother_alpha: f32,
    min_write_interval: Duration,
    /// `1.0` until a write goes out, so the first real change passes the gate
    last_written_lin: f32,
    last_write_at: Option<Instant>,
    /// `Some` triggers ceiling-mode deference (clamp our writes)
    user_ceiling_lin: Option<f32>,
    /// strict-mode lock: stops writes until [`Self::reset_deference`]
    deferred: bool,
    /// last real-audio measurement (not synthetic silence); drives [`Self::tick_silent`]
    last_measurement_at: Option<Instant>,
    /// see [`ECHO_HISTORY`]
    recent_writes: VecDeque<f32>,
}

impl AppLevelController {
    /// `block_dt_s` = expected period between [`Self::process_block`] calls (pw quantum at the
    /// stream's rate); derives envelope alphas.
    #[must_use]
    pub fn new(rule: PerAppRule, block_dt_s: f32) -> Self {
        let envelopes = LevelEnvelopes::new(level_cfg_from_rule(&rule), block_dt_s);
        let smoother_alpha = anti_bounce_alpha(rule.smoother_ms, block_dt_s);
        let min_write_interval = Duration::from_millis(rule.min_write_interval_ms.max(0.0) as u64);
        Self {
            rule,
            envelopes,
            smoothed_reduction_db: 0.0,
            smoother_alpha,
            min_write_interval,
            last_written_lin: 1.0,
            last_write_at: None,
            user_ceiling_lin: None,
            deferred: false,
            last_measurement_at: None,
            recent_writes: VecDeque::with_capacity(ECHO_HISTORY),
        }
    }

    #[must_use]
    pub fn rule(&self) -> &PerAppRule {
        &self.rule
    }

    /// envelope state survives the swap; smoother + rate-limit pick up the new values immediately.
    pub fn set_rule(&mut self, rule: PerAppRule) {
        self.envelopes.set_config(level_cfg_from_rule(&rule));
        self.smoother_alpha = anti_bounce_alpha(rule.smoother_ms, self.envelopes.block_dt_s());
        self.min_write_interval = Duration::from_millis(rule.min_write_interval_ms.max(0.0) as u64);
        self.rule = rule;
    }

    /// recompute alphas after a pw quantum change.
    pub fn set_block_dt(&mut self, dt_s: f32) {
        self.envelopes.set_block_dt(dt_s);
        self.smoother_alpha = anti_bounce_alpha(self.rule.smoother_ms, dt_s);
    }

    #[must_use]
    pub fn user_ceiling_lin(&self) -> Option<f32> {
        self.user_ceiling_lin
    }

    #[must_use]
    pub fn deferred(&self) -> bool {
        self.deferred
    }

    /// always `>= 0`; `0` means no cut.
    #[must_use]
    pub fn smoothed_reduction_db(&self) -> f32 {
        self.smoothed_reduction_db
    }

    #[must_use]
    pub fn last_written_lin(&self) -> f32 {
        self.last_written_lin
    }

    #[must_use]
    pub fn last_decision(&self) -> LevelDecision {
        // can't borrow the envelope's stored decision (by-value); smoothed_reduction_db is canonical
        LevelDecision {
            peak_reduction_db: 0.0,
            rms_reduction_db: 0.0,
            total_reduction_db: self.smoothed_reduction_db,
        }
    }

    /// `Some(new_volume_lin)` if a write is warranted now, else `None` (sub-threshold,
    /// rate-limited, or deferred).
    ///
    /// gain compensation: pw's adapter applies `channelVolumes` *before* the audio leaves the
    /// source, so the tap reads post-attenuation. uncompensated, any reduction looks like the
    /// source got quieter → envelope releases → freezes gain at the user's slider. divide
    /// peak/mean_sq by `last_written_lin` (and its square) to recover pre-attenuation signal.
    pub fn process_block(
        &mut self,
        peak_lin: f32,
        mean_sq_lin: f32,
        now: Instant,
    ) -> Option<f32> {
        if !self.rule.enabled || self.deferred {
            return None;
        }
        self.process_envelopes(peak_lin, mean_sq_lin);
        self.last_measurement_at = Some(now);
        self.decide_write(now)
    }

    fn process_envelopes(&mut self, peak_lin: f32, mean_sq_lin: f32) {
        let g = self.last_written_lin;
        let (recovered_peak, recovered_mean_sq) = if g >= GAIN_COMPENSATION_FLOOR {
            (peak_lin / g, mean_sq_lin / (g * g))
        } else {
            // below the floor (≈ muted): pass through, see GAIN_COMPENSATION_FLOOR
            (peak_lin, mean_sq_lin)
        };
        let decision = self
            .envelopes
            .process_block(recovered_peak, recovered_mean_sq);
        self.smoothed_reduction_db +=
            self.smoother_alpha * (decision.total_reduction_db - self.smoothed_reduction_db);
    }

    fn decide_write(&mut self, now: Instant) -> Option<f32> {
        let mut target_lin = headroom_dsp::util::db_to_lin(-self.smoothed_reduction_db);
        // ceiling-mode deference: never above the user's value
        if let Some(ceiling) = self.user_ceiling_lin {
            if target_lin > ceiling {
                target_lin = ceiling;
            }
        }
        target_lin = target_lin.clamp(0.0, 1.0);

        let diff_db = lin_diff_db(target_lin, self.last_written_lin);
        if diff_db < self.rule.write_db_threshold {
            return None;
        }
        if let Some(prev) = self.last_write_at {
            if now.duration_since(prev) < self.min_write_interval {
                return None;
            }
        }
        self.last_written_lin = target_lin;
        self.last_write_at = Some(now);
        self.note_write(target_lin);
        Some(target_lin)
    }

    /// remember a written volume so its later param-listener echo is recognised as ours.
    fn note_write(&mut self, v: f32) {
        if self.recent_writes.len() == ECHO_HISTORY {
            self.recent_writes.pop_front();
        }
        self.recent_writes.push_back(v);
    }

    /// true if `v` is an echo of our own write (within [`ECHO_TOLERANCE`]), not a user adjustment.
    fn is_own_echo(&self, v: f32) -> bool {
        (v - self.last_written_lin).abs() < ECHO_TOLERANCE
            || self
                .recent_writes
                .iter()
                .any(|&w| (w - v).abs() < ECHO_TOLERANCE)
    }

    /// advance envelopes through silent periods, then decide once. without this, a suspended source
    /// (pw stops delivering buffers — Strawberry between tracks) leaves the envelopes pinned at the
    /// last value and applies stale attenuation on resume.
    pub fn tick_silent(&mut self, now: Instant) -> Option<f32> {
        if !self.rule.enabled || self.deferred {
            return None;
        }
        let last = self.last_measurement_at?;
        let block_dt_s = self.envelopes.block_dt_s();
        if block_dt_s <= 0.0 {
            return None;
        }
        let block_dt = Duration::from_secs_f32(block_dt_s);
        let elapsed = now.saturating_duration_since(last);
        if elapsed < block_dt {
            return None; // source is producing normally
        }
        let n_blocks_f = elapsed.as_secs_f32() / block_dt_s;
        if n_blocks_f > MAX_SILENT_CATCHUP_BLOCKS as f32 {
            // long silence — envelopes fully released anyway; short-circuit
            self.envelopes.reset();
            self.smoothed_reduction_db = 0.0;
        } else {
            // bounded by the branch above, so the u32 truncation is safe
            let n_blocks = n_blocks_f as u32;
            for _ in 0..n_blocks {
                self.process_envelopes(0.0, 0.0);
            }
        }
        // count synthetic silence as a measurement so we don't re-tick next pass
        self.last_measurement_at = Some(now);
        self.decide_write(now)
    }

    /// ceiling mode caps our writes at the user's value; strict mode stops adjustment until
    /// [`Self::reset_deference`].
    pub fn on_external_change(&mut self, new_volume_lin: f32) {
        // matches a recent write → our own echo, not a user change (window, not just last_written,
        // to catch delayed/reordered echoes that would otherwise self-lock the stream)
        if self.is_own_echo(new_volume_lin) {
            return;
        }
        match self.rule.defer_to_user {
            DeferPolicy::Ceiling => {
                self.user_ceiling_lin = Some(new_volume_lin.clamp(0.0, 1.0));
            }
            DeferPolicy::Strict => {
                self.deferred = true;
            }
        }
    }

    pub fn reset_deference(&mut self) {
        self.user_ceiling_lin = None;
        self.deferred = false;
    }

    /// seed a persisted ceiling when an app respawns its stream (Strawberry makes a fresh node per
    /// track). without it, the inherited `channelVolumes` (often our own prior attenuated value)
    /// hits a fresh controller at `last_written_lin = 1.0`, is misread as a user change, and locks
    /// the ceiling at the daemon's reduced value.
    pub fn restore_state(&mut self, ceiling_lin: f32, now: Instant) {
        let v = ceiling_lin.clamp(0.0, 1.0);
        self.user_ceiling_lin = Some(v);
        self.last_written_lin = v;
        self.last_write_at = Some(now);
        self.note_write(v);
    }
}

/// non-`Spawn` variants distinguish *why* a stream is unmanaged (config vs bug) for spawn-skip logs.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerAEval {
    MasterOff,
    NotPlayback,
    NoMatch,
    RuleDisabled,
    Spawn(PerAppRule),
}

impl LayerAEval {
    #[must_use]
    pub fn rule(self) -> Option<PerAppRule> {
        match self {
            LayerAEval::Spawn(rule) => Some(rule),
            _ => None,
        }
    }

    #[must_use]
    pub fn skip_reason(&self) -> &'static str {
        match self {
            LayerAEval::MasterOff => "per_app master disabled",
            LayerAEval::NotPlayback => "not a routable playback stream",
            LayerAEval::NoMatch => "no matching rule",
            LayerAEval::RuleDisabled => "matching rule disabled",
            LayerAEval::Spawn(_) => "spawn",
        }
    }
}

/// orthogonal to `routing::evaluate` (the bus-routing sibling)
#[must_use]
pub fn evaluate(info: &PwNodeInfo, per_app: &PerAppSection) -> LayerAEval {
    if !per_app.enabled {
        return LayerAEval::MasterOff;
    }
    if !info.is_routable_playback() {
        return LayerAEval::NotPlayback;
    }
    for rule in &per_app.rules {
        if routing::matches(info, &rule.match_) {
            return if rule.enabled {
                LayerAEval::Spawn(rule.clone())
            } else {
                LayerAEval::RuleDisabled
            };
        }
    }
    if per_app.default_enabled {
        return LayerAEval::Spawn(default_rule());
    }
    LayerAEval::NoMatch
}

fn default_rule() -> PerAppRule {
    let cfg = LevelEnvelopesConfig::default();
    PerAppRule {
        match_: headroom_ipc::RouteRuleMatch::default(),
        enabled: true,
        peak_threshold_db: cfg.peak_threshold_db,
        rms_target_db: cfg.rms_target_db,
        max_cut_db: cfg.max_cut_db,
        peak_attack_ms: cfg.peak_attack_ms,
        peak_release_ms: cfg.peak_release_ms,
        rms_window_ms: cfg.rms_window_ms,
        smoother_ms: FALLBACK_SMOOTHER_MS,
        write_db_threshold: FALLBACK_WRITE_DB_THRESHOLD,
        min_write_interval_ms: FALLBACK_MIN_WRITE_INTERVAL_MS,
        defer_to_user: DeferPolicy::default(),
    }
}

fn level_cfg_from_rule(rule: &PerAppRule) -> LevelEnvelopesConfig {
    LevelEnvelopesConfig {
        peak_threshold_db: rule.peak_threshold_db,
        rms_target_db: rule.rms_target_db,
        max_cut_db: rule.max_cut_db,
        peak_attack_ms: rule.peak_attack_ms,
        peak_release_ms: rule.peak_release_ms,
        rms_window_ms: rule.rms_window_ms,
    }
}

fn anti_bounce_alpha(time_ms: f32, block_dt_s: f32) -> f32 {
    if block_dt_s <= 0.0 || time_ms <= 0.0 {
        return 1.0;
    }
    let block_rate = 1.0 / block_dt_s;
    headroom_dsp::util::time_to_alpha(time_ms, block_rate)
}

fn lin_diff_db(a: f32, b: f32) -> f32 {
    let a = a.max(1e-6);
    let b = b.max(1e-6);
    (20.0 * (a / b).log10()).abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::PerAppRule;
    use headroom_dsp::util::db_to_lin;
    use headroom_ipc::RouteRuleMatch;

    /// 1024-frame quantum @ 48 kHz.
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
            rms_window_ms: 200.0, // shorter so tests converge
            smoother_ms: FALLBACK_SMOOTHER_MS,
            write_db_threshold: FALLBACK_WRITE_DB_THRESHOLD,
            min_write_interval_ms: FALLBACK_MIN_WRITE_INTERVAL_MS,
            defer_to_user: DeferPolicy::Ceiling,
        }
    }

    fn playback_info(binary: &str) -> PwNodeInfo {
        PwNodeInfo {
            node_id: 1,
            media_class: Some("Stream/Output/Audio".into()),
            application_process_binary: Some(binary.into()),
            ..Default::default()
        }
    }

    #[test]
    fn set_rule_swaps_thresholds_preserving_deference() {
        // reevaluate_layer_a pushes a profile's new rule into a live
        // controller via set_rule (no tap churn). The new thresholds
        // must take effect while user_ceiling / deferred state survive.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        assert!((c.rule().peak_threshold_db - -6.0).abs() < 1e-6);
        // Establish a user ceiling (Ceiling policy) so we can check it
        // survives the rule swap.
        c.on_external_change(0.4);
        assert_eq!(c.user_ceiling_lin(), Some(0.4));

        let mut new_rule = aggressive_rule();
        new_rule.peak_threshold_db = -18.0;
        new_rule.max_cut_db = 6.0;
        c.set_rule(new_rule);

        assert!((c.rule().peak_threshold_db - -18.0).abs() < 1e-6);
        assert!((c.rule().max_cut_db - 6.0).abs() < 1e-6);
        // Runtime deference state is preserved across the swap.
        assert_eq!(c.user_ceiling_lin(), Some(0.4));
    }

    #[test]
    fn disabled_rule_returns_no_write() {
        let mut rule = aggressive_rule();
        rule.enabled = false;
        let mut c = AppLevelController::new(rule, BLOCK_DT_S);
        let now = Instant::now();
        assert!(c.process_block(db_to_lin(0.0), 1.0, now).is_none());
    }

    #[test]
    fn first_write_after_settling_emits_volume_below_unity() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let now = Instant::now();
        // Drive a hot signal until the envelopes settle and the
        // anti-bounce smoother converges.
        let mut last = None;
        for i in 0..1000 {
            let t = now + Duration::from_millis(i as u64 * 21); // ~block_dt
            if let Some(v) = c.process_block(db_to_lin(0.0), db_to_lin(-3.0).powi(2), t) {
                last = Some(v);
            }
        }
        let v = last.expect("controller should issue at least one write");
        assert!(v < 1.0, "expected sub-unity volume, got {v}");
        assert!(v > 0.0);
    }

    #[test]
    fn rate_limit_blocks_back_to_back_writes() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let t0 = Instant::now();
        // Drive convergence first so a write happens.
        let mut wrote = false;
        for i in 0..200 {
            let t = t0 + Duration::from_millis(i as u64 * 21);
            if c.process_block(db_to_lin(0.0), db_to_lin(-3.0).powi(2), t).is_some() {
                wrote = true;
                break;
            }
        }
        assert!(wrote, "first write expected during convergence");
        // Immediately after the write, force a different reduction —
        // the rate limit must suppress any further write within 100 ms.
        let t1 = c.last_write_at.unwrap() + Duration::from_millis(10);
        c.smoothed_reduction_db += 6.0; // synthetic kick
        let v = c.process_block(db_to_lin(0.0), db_to_lin(-3.0).powi(2), t1);
        assert!(v.is_none(), "rate limit should have blocked the follow-up write");
    }

    #[test]
    fn threshold_blocks_microscopic_changes() {
        // Strategy: drive the controller to a steady state at a
        // specific reduction, let it write, then nudge inputs by an
        // amount that produces a sub-`WRITE_DB_THRESHOLD` change at
        // the smoothed output. The threshold gate must suppress.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let t0 = Instant::now();
        // 0 dBFS peak → 6 dB cut requested by the peak path.
        let hot_peak = db_to_lin(0.0);
        let hot_mean_sq = db_to_lin(-3.0).powi(2);

        // Burn in until convergence.
        let mut last_write_t = t0;
        for i in 0..2_000 {
            let t = t0 + Duration::from_millis(i as u64 * 21);
            if c.process_block(hot_peak, hot_mean_sq, t).is_some() {
                last_write_t = t;
            }
        }
        // Move past the rate limit window so the threshold is the only
        // active gate, then feed an essentially-identical input. The
        // smoothed reduction barely budges, so the dB diff against
        // last_written_lin must stay under WRITE_DB_THRESHOLD.
        let t_after = last_write_t + Duration::from_millis(500);
        let v = c.process_block(hot_peak * 1.001, hot_mean_sq * 1.001, t_after);
        assert!(
            v.is_none(),
            "near-identical input should fall inside the threshold dead band, got {v:?}"
        );
    }

    #[test]
    fn ceiling_mode_caps_target_at_user_value() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        // User pulls the slider down to 0.6 externally.
        c.on_external_change(0.6);
        assert_eq!(c.user_ceiling_lin(), Some(0.6));
        let mut last = None;
        let t0 = Instant::now();
        // No signal yet — proposed reduction is 0 → target is unity →
        // but ceiling forces it down to 0.6 → expect a write below
        // unity even with no detection activity.
        for i in 0..400 {
            let t = t0 + Duration::from_millis(i as u64 * 21);
            if let Some(v) = c.process_block(0.0, 0.0, t) {
                last = Some(v);
            }
        }
        let v = last.expect("should write at least once to reach ceiling");
        assert!((v - 0.6).abs() < 0.01, "expected ~0.6, got {v}");
    }

    #[test]
    fn strict_mode_stops_writes_after_external_change() {
        let mut rule = aggressive_rule();
        rule.defer_to_user = DeferPolicy::Strict;
        let mut c = AppLevelController::new(rule, BLOCK_DT_S);
        c.on_external_change(0.7);
        assert!(c.deferred());
        let t = Instant::now();
        // Drive a hot signal — strict deference must not write.
        for i in 0..400 {
            let t = t + Duration::from_millis(i as u64 * 21);
            assert!(c
                .process_block(db_to_lin(0.0), db_to_lin(-3.0).powi(2), t)
                .is_none());
        }
    }

    #[test]
    fn reset_deference_clears_strict_lock() {
        let mut rule = aggressive_rule();
        rule.defer_to_user = DeferPolicy::Strict;
        let mut c = AppLevelController::new(rule, BLOCK_DT_S);
        c.on_external_change(0.7);
        assert!(c.deferred());
        c.reset_deference();
        assert!(!c.deferred());
        assert!(c.user_ceiling_lin().is_none());
    }

    #[test]
    fn ignores_external_change_that_matches_our_write() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        c.last_written_lin = 0.5;
        c.on_external_change(0.5);
        // Should not register as external — no ceiling, no defer.
        assert!(c.user_ceiling_lin().is_none());
        assert!(!c.deferred());
    }

    #[test]
    fn reenable_unity_baseline_echo_does_not_lock_ceiling() {
        // On per-app re-enable, the registry writes a unity baseline
        // BEFORE subscribing to Props (spawn_layer_a). A fresh
        // controller's `last_written_lin` is 1.0, so the initial echo
        // of that baseline is recognised as our own write — no spurious
        // ceiling, no deference, so management restarts cleanly instead
        // of hanging at the previous session's value.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        assert!((c.last_written_lin() - 1.0).abs() < 1e-6);
        c.on_external_change(1.0);
        assert!(c.user_ceiling_lin().is_none());
        assert!(!c.deferred());
    }

    #[test]
    fn stale_attenuation_echo_without_baseline_locks_ceiling() {
        // Documents *why* the baseline write matters: a fresh controller
        // can't tell the node's stale value (the daemon's own prior cut,
        // left over from a just-torn-down session) from a real user
        // gesture, so an un-suppressed 0.5 echo locks a bogus 0.5
        // ceiling — exactly the "value hangs until I tweak it" symptom
        // the spawn-time unity write prevents.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        c.on_external_change(0.5);
        assert_eq!(c.user_ceiling_lin(), Some(0.5));
    }

    #[test]
    fn delayed_echo_of_earlier_write_is_not_a_user_ceiling() {
        // Regression for the self-lock bug: the controller wrote 0.31,
        // then 0.35; the echo of the *earlier* 0.31 arrives after
        // last_written has moved to 0.35. With only a single
        // last_written comparison, 0.31 would be misread as a user-set
        // ceiling and (sitting below the content's natural target)
        // permanently clamp the stream. The recent-writes window
        // recognises 0.31 as ours.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        c.note_write(0.31);
        c.last_written_lin = 0.31;
        c.note_write(0.35);
        c.last_written_lin = 0.35;
        c.on_external_change(0.31);
        assert!(
            c.user_ceiling_lin().is_none(),
            "a delayed echo of our own write must not become a ceiling"
        );
        assert!(!c.deferred());
    }

    #[test]
    fn genuine_user_change_after_writes_still_registers() {
        // A value the controller never wrote is a real user action and
        // must still take effect, even with a full write history.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        for v in [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.35, 0.31] {
            c.note_write(v);
            c.last_written_lin = v;
        }
        // 0.55 was never written.
        c.on_external_change(0.55);
        assert_eq!(c.user_ceiling_lin(), Some(0.55));
    }

    // -----------------------------------------------------------------
    // Gain compensation + silent ticks
    // -----------------------------------------------------------------

    /// Run enough warm-up blocks (with the given input) to converge the
    /// envelopes + smoother, returning the controller in steady state.
    fn warm_to_steady(
        c: &mut AppLevelController,
        peak: f32,
        mean_sq: f32,
        start: Instant,
    ) -> Instant {
        let mut t = start;
        for _ in 0..2_000 {
            let _ = c.process_block(peak, mean_sq, t);
            t += Duration::from_millis(21);
        }
        t
    }

    #[test]
    fn gain_compensation_recovers_pre_attenuation_signal() {
        // Source true peak = 0 dBFS, applied gain = 0.5 (so the tap
        // would measure 0.5). With compensation enabled, the envelope
        // must see the pre-attenuation 0 dBFS — i.e. the controller's
        // computed reduction must match what an uncompensated 0 dBFS
        // input produces on a fresh controller.
        let mut compensated = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        compensated.last_written_lin = 0.5; // simulate prior write
        let _ = warm_to_steady(&mut compensated, 0.5, 0.5_f32.powi(2), Instant::now());

        let mut baseline = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        // No prior write — compensation is a no-op since last_written = 1.0.
        let _ = warm_to_steady(&mut baseline, 1.0, 1.0, Instant::now());

        let diff = (compensated.smoothed_reduction_db - baseline.smoothed_reduction_db).abs();
        assert!(
            diff < 0.1,
            "compensated controller should see the same effective signal as the baseline at full scale; \
             compensated={}, baseline={}",
            compensated.smoothed_reduction_db,
            baseline.smoothed_reduction_db,
        );
    }

    #[test]
    fn gain_compensation_disabled_below_floor() {
        // last_written_lin below GAIN_COMPENSATION_FLOOR → compensation
        // must NOT amplify. Feed a small post-attenuation peak that
        // would blow up to clipping if divided by 0.005, and verify
        // the envelopes don't spike accordingly.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        c.last_written_lin = 0.005; // below GAIN_COMPENSATION_FLOOR
        let now = Instant::now();
        let _ = warm_to_steady(&mut c, 0.01, 0.01_f32.powi(2), now);
        // Without compensation, 0.01 peak = −40 dB, well under the
        // −6 dB threshold → smoothed reduction stays ≈ 0.
        assert!(
            c.smoothed_reduction_db < 1.0,
            "below-floor compensation should pass-through; got {} dB",
            c.smoothed_reduction_db
        );
    }

    #[test]
    fn tick_silent_is_noop_with_recent_measurement() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let t = Instant::now();
        // Establish a recent measurement.
        c.process_block(0.5, 0.25, t);
        // tick_silent within the block window must be a no-op
        // (returns None, smoothed_reduction unchanged).
        let before = c.smoothed_reduction_db;
        let out = c.tick_silent(t + Duration::from_millis(1));
        assert!(out.is_none());
        assert!((c.smoothed_reduction_db - before).abs() < 1e-6);
    }

    #[test]
    fn tick_silent_is_noop_without_prior_measurement() {
        // Controller has never seen a real measurement → no idea what
        // wall-clock the envelopes are anchored to → must skip.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let now = Instant::now();
        let out = c.tick_silent(now + Duration::from_secs(60));
        assert!(out.is_none());
    }

    #[test]
    fn tick_silent_releases_envelope_over_extended_idle() {
        // Aggressive max_cut + gain compensation pegs both paths at
        // the cap during full-scale input, so a few hundred ms of
        // release isn't enough — the RMS envelope sees the
        // compensation-amplified mean_sq and takes ~rms_window × 4–5
        // to drop below threshold. Use a multi-second idle that
        // matches Strawberry's actual between-track pause.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let t = Instant::now();
        let after_warm = warm_to_steady(&mut c, 1.0, 1.0, t);
        let reduced = c.smoothed_reduction_db;
        assert!(reduced > 1.0, "expected sustained reduction, got {reduced}");

        let _ = c.tick_silent(after_warm + Duration::from_secs(3));
        assert!(
            c.smoothed_reduction_db < reduced - 0.5,
            "tick_silent should release the envelope; before={reduced}, after={}",
            c.smoothed_reduction_db
        );
    }

    #[test]
    fn tick_silent_long_idle_short_circuits_to_full_release() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let t = Instant::now();
        let after_warm = warm_to_steady(&mut c, 1.0, 1.0, t);
        assert!(c.smoothed_reduction_db > 1.0);

        // > MAX_SILENT_CATCHUP_BLOCKS × block_dt of silence triggers
        // the reset shortcut.
        let long_gap = Duration::from_secs_f32(
            (MAX_SILENT_CATCHUP_BLOCKS as f32 + 100.0) * BLOCK_DT_S,
        );
        let _ = c.tick_silent(after_warm + long_gap);
        assert!(
            c.smoothed_reduction_db.abs() < 1e-6,
            "long silence should reset envelopes; got {}",
            c.smoothed_reduction_db
        );
    }

    #[test]
    fn tick_silent_writes_volume_back_up_when_envelope_releases() {
        // Setup: signal was loud, controller wrote a reduced volume.
        // Source then pauses indefinitely. After enough silent ticks,
        // smoothed_reduction → 0 and the controller should write
        // back up toward unity (or user_ceiling). Idle is measured
        // since the last *process_block call*, not the last write —
        // in steady state the controller keeps consuming measurements
        // but stops writing once target == last_written.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let t = Instant::now();
        let mut last_block_t = t;
        for i in 0..2_000 {
            let bt = t + Duration::from_millis(i as u64 * 21);
            let _ = c.process_block(1.0, 1.0, bt);
            last_block_t = bt;
        }
        let written = c.last_written_lin;
        assert!(
            written < 1.0,
            "controller should have written sub-unity volume during convergence; got {written}"
        );

        // Long silence → reset shortcut → envelopes at zero → target
        // computes to 1.0. Past the rate-limit window so the write
        // can fire.
        let later = last_block_t + Duration::from_secs(20);
        let out = c.tick_silent(later);
        let v = out.expect("tick_silent should fire a write back toward unity");
        assert!(
            v > written,
            "tick_silent write must raise volume; before={written}, after={v}"
        );
        assert!((v - 1.0).abs() < 0.05, "expected ~1.0, got {v}");
    }

    #[test]
    fn restore_state_seeds_ceiling_and_suppresses_first_echo() {
        // Simulates the spawn path: the new managed_stream restores
        // a persisted ceiling, then the param listener fires with
        // the (now-overwritten) channelVolumes. The echo must be
        // recognized as ours, not misattributed as a user change.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let now = Instant::now();
        c.restore_state(0.7, now);
        assert_eq!(c.user_ceiling_lin(), Some(0.7));
        assert!((c.last_written_lin() - 0.7).abs() < 1e-6);
        // Now simulate PipeWire echoing the just-written 0.7.
        c.on_external_change(0.7);
        // Ceiling must not change; the echo was recognized.
        assert_eq!(c.user_ceiling_lin(), Some(0.7));
        assert!(!c.deferred());
    }

    #[test]
    fn restore_state_does_not_block_genuine_user_changes_afterwards() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        c.restore_state(0.7, Instant::now());
        // User actually adjusts to 0.5 in pavucontrol.
        c.on_external_change(0.5);
        assert_eq!(c.user_ceiling_lin(), Some(0.5));
    }

    #[test]
    fn restore_state_clamps_out_of_range_inputs() {
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        c.restore_state(1.5, Instant::now());
        assert!((c.last_written_lin() - 1.0).abs() < 1e-6);
        let mut c2 = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        c2.restore_state(-0.2, Instant::now());
        assert!((c2.last_written_lin() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn tick_silent_respects_user_ceiling() {
        // Same as above but with a user ceiling set; after release the
        // controller must clamp the write at the ceiling.
        let mut c = AppLevelController::new(aggressive_rule(), BLOCK_DT_S);
        let t = Instant::now();
        c.on_external_change(0.5); // user_ceiling = 0.5
        let mut last_block_t = t;
        for i in 0..2_000 {
            let bt = t + Duration::from_millis(i as u64 * 21);
            let _ = c.process_block(1.0, 1.0, bt);
            last_block_t = bt;
        }
        // Long silence — envelopes release.
        let later = last_block_t + Duration::from_secs(20);
        if let Some(v) = c.tick_silent(later) {
            assert!(
                (v - 0.5).abs() < 0.01,
                "tick_silent write must clamp at user_ceiling; got {v}"
            );
        }
        // After release, last_written must be at the ceiling (whether
        // via the write above or because steady state already pinned
        // it there).
        assert!(
            (c.last_written_lin - 0.5).abs() < 0.01,
            "expected last_written ≈ 0.5, got {}",
            c.last_written_lin
        );
    }

    // -----------------------------------------------------------------
    // Rule matching
    // -----------------------------------------------------------------

    #[test]
    fn evaluate_returns_master_off_when_layer_a_disabled() {
        let per_app = PerAppSection {
            enabled: false,
            ..Default::default()
        };
        assert_eq!(
            evaluate(&playback_info("firefox"), &per_app),
            LayerAEval::MasterOff
        );
    }

    #[test]
    fn evaluate_returns_matching_rule() {
        let per_app = PerAppSection {
            enabled: true,
            default_enabled: false,
            rules: vec![PerAppRule {
                match_: RouteRuleMatch {
                    process_binary: vec!["firefox".into()],
                    ..Default::default()
                },
                ..aggressive_rule()
            }],
        };
        let r = evaluate(&playback_info("firefox"), &per_app)
            .rule()
            .expect("match");
        assert_eq!(r.peak_threshold_db, aggressive_rule().peak_threshold_db);
    }

    #[test]
    fn evaluate_returns_rule_disabled_for_disabled_matching_rule() {
        let per_app = PerAppSection {
            enabled: true,
            default_enabled: false,
            rules: vec![PerAppRule {
                match_: RouteRuleMatch {
                    process_binary: vec!["spotify".into()],
                    ..Default::default()
                },
                enabled: false,
                ..aggressive_rule()
            }],
        };
        assert_eq!(
            evaluate(&playback_info("spotify"), &per_app),
            LayerAEval::RuleDisabled
        );
    }

    #[test]
    fn evaluate_returns_default_rule_when_default_enabled_and_no_match() {
        let per_app = PerAppSection {
            enabled: true,
            default_enabled: true,
            rules: vec![],
        };
        let r = evaluate(&playback_info("unmatched"), &per_app)
            .rule()
            .expect("default");
        // Default rule honours LevelEnvelopesConfig::default().
        let cfg = LevelEnvelopesConfig::default();
        assert!((r.peak_threshold_db - cfg.peak_threshold_db).abs() < 1e-6);
        assert_eq!(r.defer_to_user, DeferPolicy::default());
    }

    #[test]
    fn evaluate_returns_no_match_for_unmatched_when_default_off() {
        let per_app = PerAppSection {
            enabled: true,
            default_enabled: false,
            rules: vec![],
        };
        assert_eq!(
            evaluate(&playback_info("unmatched"), &per_app),
            LayerAEval::NoMatch
        );
    }

    #[test]
    fn spawn_predicate_matches_only_spawn_variant() {
        // Mirrors the reconciliation predicate in `RoutingState`:
        // `matches!(evaluate(..), LayerAEval::Spawn(_))` is the single
        // gate that decides whether a known-but-unmanaged stream gets a
        // tap. Confirm it's true exactly when a controller should run.
        let per_app = PerAppSection {
            enabled: true,
            default_enabled: false,
            rules: vec![PerAppRule {
                match_: RouteRuleMatch {
                    process_binary: vec!["firefox".into()],
                    ..Default::default()
                },
                ..aggressive_rule()
            }],
        };
        let should_manage = |info: &PwNodeInfo| {
            matches!(evaluate(info, &per_app), LayerAEval::Spawn(_))
        };
        assert!(should_manage(&playback_info("firefox")));
        assert!(!should_manage(&playback_info("unmatched")));
    }

    #[test]
    fn skip_reason_strings_are_distinct() {
        assert_eq!(LayerAEval::MasterOff.skip_reason(), "per_app master disabled");
        assert_eq!(LayerAEval::NoMatch.skip_reason(), "no matching rule");
        assert_eq!(LayerAEval::RuleDisabled.skip_reason(), "matching rule disabled");
        assert_eq!(LayerAEval::NotPlayback.skip_reason(), "not a routable playback stream");
    }

    #[test]
    fn evaluate_skips_non_playback_streams() {
        let mut info = playback_info("firefox");
        info.media_class = Some("Stream/Input/Audio".into());
        let per_app = PerAppSection {
            enabled: true,
            default_enabled: true,
            rules: vec![],
        };
        assert_eq!(evaluate(&info, &per_app), LayerAEval::NotPlayback);
    }
}
