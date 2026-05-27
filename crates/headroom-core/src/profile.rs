//! profile types; mirrors the toml schema

use serde::{Deserialize, Serialize};

use headroom_dsp::{CompressorConfig, Detector, LimiterConfig, SoftTierConfig};

/// serde-capable mirror of [`Detector`]; keeps `headroom-dsp` dep-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DetectorChoice {
    #[default]
    Peak,
    Rms,
}

impl From<DetectorChoice> for Detector {
    fn from(c: DetectorChoice) -> Self {
        match c {
            DetectorChoice::Peak => Detector::Peak,
            DetectorChoice::Rms => Detector::Rms,
        }
    }
}
use headroom_ipc::{Route, RouteRule, RouteRuleMatch};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// unique within the profiles directory
    pub name: String,
    pub description: String,

    #[serde(default)]
    pub agc: AgcSection,
    #[serde(default)]
    pub compressor: CompressorSection,
    #[serde(default)]
    pub limiter: LimiterSection,
    #[serde(default)]
    pub meters: MetersSection,

    /// evaluated in order; first match wins
    #[serde(default)]
    pub rules: Vec<RouteRule>,
    #[serde(default)]
    pub default_route: DefaultRouteSection,

    #[serde(default)]
    pub per_app: PerAppSection,
}

impl Profile {
    /// mirrors the `default.toml` shipped profile.
    #[must_use]
    pub fn default_v0() -> Self {
        Self {
            name: "default".into(),
            description: "Gentle transparent processing for everyday use.".into(),
            agc: AgcSection::default(),
            compressor: CompressorSection::default(),
            limiter: LimiterSection::default(),
            meters: MetersSection::default(),
            rules: vec![
                RouteRule {
                    match_: RouteRuleMatch {
                        process_binary: vec![
                            "spotify".into(),
                            "mpv".into(),
                            "vlc".into(),
                            "ardour".into(),
                            "reaper".into(),
                            "qpwgraph".into(),
                            "carla".into(),
                            "bitwig-studio".into(),
                        ],
                        ..Default::default()
                    },
                    route: Route::Bypass,
                },
                RouteRule {
                    match_: RouteRuleMatch {
                        process_binary: vec![
                            "firefox".into(),
                            "chromium".into(),
                            "google-chrome".into(),
                            "Discord".into(),
                            "discord".into(),
                            "element-desktop".into(),
                            "Slack".into(),
                            "zoom".into(),
                            "WEBRTC VoiceEngine".into(),
                        ],
                        ..Default::default()
                    },
                    route: Route::Processed,
                },
            ],
            default_route: DefaultRouteSection {
                route: Route::Processed,
            },
            per_app: PerAppSection::default(),
        }
    }

    #[must_use]
    pub fn build_limiter_config(&self) -> LimiterConfig {
        let soft = self.limiter.soft.as_ref().map(|s| SoftTierConfig {
            max_psr_db: s.max_psr_db,
            static_ceiling_dbtp: s.static_ceiling_dbtp,
            attack_ms: s.attack_ms,
            release_ms: s.release_ms,
        });
        LimiterConfig {
            ceiling_dbtp: self.limiter.ceiling_dbtp,
            lookahead_ms: self.limiter.lookahead_ms,
            release_ms: self.limiter.release_ms,
            hold_ms: self.limiter.hold_ms,
            oversample: self.limiter.oversample,
            fir_taps: 31,
            soft,
        }
        .sanitized()
    }

    #[must_use]
    pub fn build_compressor_config(&self) -> CompressorConfig {
        let makeup_db = match self.compressor.makeup_db {
            MakeupGain::Auto => None,
            MakeupGain::Db(v) => Some(v),
        };
        CompressorConfig {
            enabled: self.compressor.enabled,
            threshold_db: self.compressor.threshold_db,
            ratio: self.compressor.ratio,
            knee_db: self.compressor.knee_db,
            attack_ms: self.compressor.attack_ms,
            release_ms: self.compressor.release_ms,
            makeup_db,
            detector: self.compressor.detector.into(),
            rms_window_ms: self.compressor.rms_window_ms,
        }
        .sanitized()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgcSection {
    pub enabled: bool,
    pub target_lufs: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// below this momentary loudness the agc stops adjusting
    pub silence_threshold_lufs: f32,
    pub max_boost_db: f32,
    pub max_cut_db: f32,
}

impl Default for AgcSection {
    fn default() -> Self {
        Self {
            enabled: true,
            target_lufs: -18.0,
            attack_ms: 2000.0,
            release_ms: 800.0,
            silence_threshold_lufs: -70.0,
            max_boost_db: 12.0,
            max_cut_db: 12.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressorSection {
    pub enabled: bool,
    pub detector: DetectorChoice,
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: MakeupGain,
    /// ignored when `detector == Peak`
    pub rms_window_ms: f32,
}

impl Default for CompressorSection {
    fn default() -> Self {
        Self {
            enabled: true,
            detector: DetectorChoice::Peak,
            threshold_db: -24.0,
            ratio: 2.5,
            knee_db: 6.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: MakeupGain::Auto,
            rms_window_ms: 5.0,
        }
    }
}

/// de/serialize hand-rolled: derived `#[serde(untagged)]` can't deserialize unit `Auto` from
/// `"auto"` (untagged matches unit variants against null, not a name), so every shipped profile
/// with `makeup_db = "auto"` silently failed to parse under toml and got skipped.
#[derive(Debug, Clone, Copy, Default)]
pub enum MakeupGain {
    Db(f32),
    /// compute conservative auto-makeup from threshold and ratio
    #[default]
    Auto,
}

impl Serialize for MakeupGain {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            MakeupGain::Db(v) => ser.serialize_f32(*v),
            MakeupGain::Auto => ser.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for MakeupGain {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Num(f32),
            Str(String),
        }
        match Repr::deserialize(de)? {
            Repr::Num(v) => Ok(MakeupGain::Db(v)),
            Repr::Str(s) if s.eq_ignore_ascii_case("auto") => Ok(MakeupGain::Auto),
            Repr::Str(s) => Err(serde::de::Error::custom(format!(
                "invalid makeup_db {s:?}: expected a number or \"auto\""
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimiterSection {
    pub ceiling_dbtp: f32,
    pub lookahead_ms: f32,
    pub release_ms: f32,
    pub hold_ms: f32,
    /// 1/2/4/8
    pub oversample: usize,
    pub link: LinkMode,
    /// omit for pure brickwall
    pub soft: Option<LimiterSoftSection>,
}

impl Default for LimiterSection {
    fn default() -> Self {
        Self {
            ceiling_dbtp: -0.1,
            lookahead_ms: 2.0,
            release_ms: 80.0,
            hold_ms: 5.0,
            oversample: 4,
            link: LinkMode::Stereo,
            soft: Some(LimiterSoftSection::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimiterSoftSection {
    /// max peak-to-loudness ratio (dB)
    pub max_psr_db: f32,
    /// static fallback ceiling (dBTP)
    pub static_ceiling_dbtp: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Default for LimiterSoftSection {
    fn default() -> Self {
        Self {
            max_psr_db: 14.0,
            static_ceiling_dbtp: -6.0,
            attack_ms: 5.0,
            release_ms: 200.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LinkMode {
    /// one envelope shared across channels (no image shift)
    #[default]
    Stereo,
    DualMono,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetersSection {
    /// max publish rate (Hz); server may publish slower
    pub publish_hz: f32,
}

impl Default for MetersSection {
    fn default() -> Self {
        Self { publish_hz: 20.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DefaultRouteSection {
    pub route: Route,
}

impl Default for DefaultRouteSection {
    fn default() -> Self {
        Self {
            route: Route::Processed,
        }
    }
}

/// per-app level control (layer a)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PerAppSection {
    pub enabled: bool,
    /// default state for streams matched by no rule
    pub default_enabled: bool,
    pub rules: Vec<PerAppRule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerAppRule {
    #[serde(rename = "match", default)]
    pub match_: RouteRuleMatch,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_peak_threshold_db")]
    pub peak_threshold_db: f32,
    #[serde(default = "default_rms_target_db")]
    pub rms_target_db: f32,
    #[serde(default = "default_max_cut_db")]
    pub max_cut_db: f32,
    #[serde(default = "default_peak_attack_ms")]
    pub peak_attack_ms: f32,
    #[serde(default = "default_peak_release_ms")]
    pub peak_release_ms: f32,
    #[serde(default = "default_rms_window_ms")]
    pub rms_window_ms: f32,
    /// damps switching between peak-path and rms-path winning post-combine
    #[serde(default = "default_smoother_ms")]
    pub smoother_ms: f32,
    /// below this dB change, smoother updates internally but no `Props.channelVolumes` write
    #[serde(default = "default_write_db_threshold")]
    pub write_db_threshold: f32,
    /// hard per-stream rate limit
    #[serde(default = "default_min_write_interval_ms")]
    pub min_write_interval_ms: f32,
    #[serde(default)]
    pub defer_to_user: DeferPolicy,
}

impl PerAppRule {
    /// synthesises a per-app enable/disable override for an app with no authored rule
    /// (see `profile_store::apply_per_app_overlay`).
    #[must_use]
    pub fn defaulted(match_: RouteRuleMatch, enabled: bool) -> Self {
        Self {
            match_,
            enabled,
            peak_threshold_db: default_peak_threshold_db(),
            rms_target_db: default_rms_target_db(),
            max_cut_db: default_max_cut_db(),
            peak_attack_ms: default_peak_attack_ms(),
            peak_release_ms: default_peak_release_ms(),
            rms_window_ms: default_rms_window_ms(),
            smoother_ms: default_smoother_ms(),
            write_db_threshold: default_write_db_threshold(),
            min_write_interval_ms: default_min_write_interval_ms(),
            defer_to_user: DeferPolicy::default(),
        }
    }
}

const fn default_true() -> bool {
    true
}
const fn default_peak_threshold_db() -> f32 {
    -6.0
}
const fn default_rms_target_db() -> f32 {
    -20.0
}
const fn default_max_cut_db() -> f32 {
    12.0
}
const fn default_peak_attack_ms() -> f32 {
    5.0
}
const fn default_peak_release_ms() -> f32 {
    500.0
}
const fn default_rms_window_ms() -> f32 {
    1500.0
}
const fn default_smoother_ms() -> f32 {
    30.0
}
const fn default_write_db_threshold() -> f32 {
    0.5
}
const fn default_min_write_interval_ms() -> f32 {
    100.0
}

/// policy when the user changes volume on a managed stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeferPolicy {
    /// user value is a ceiling: keep cutting on spikes, never raise above it
    #[default]
    Ceiling,
    /// stop adjusting entirely until the user opts back in
    Strict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_v0_builds_sane_dsp_configs() {
        let p = Profile::default_v0();
        let lim = p.build_limiter_config();
        assert!((lim.ceiling_dbtp - (-0.1)).abs() < 1e-6);
        assert_eq!(lim.oversample, 4);
        assert!(lim.soft.is_some());

        let comp = p.build_compressor_config();
        assert!((comp.threshold_db - (-24.0)).abs() < 1e-6);
        assert!((comp.ratio - 2.5).abs() < 1e-6);
        // Auto-makeup translates to `None`.
        assert!(comp.makeup_db.is_none());
    }

    #[test]
    fn default_v0_has_expected_routing_rules() {
        let p = Profile::default_v0();
        assert_eq!(p.default_route.route, Route::Processed);
        // First rule should be the bypass list.
        assert_eq!(p.rules[0].route, Route::Bypass);
        assert!(p.rules[0].match_.process_binary.iter().any(|s| s == "mpv"));
        // Second the processed list.
        assert_eq!(p.rules[1].route, Route::Processed);
        assert!(p.rules[1]
            .match_
            .process_binary
            .iter()
            .any(|s| s == "firefox"));
    }

    #[test]
    fn makeup_gain_serialises_as_string_or_number() {
        // Auto serialises to the lowercase string `"auto"` — the same
        // token profiles use on disk — and round-trips.
        let auto = serde_json::to_string(&MakeupGain::Auto).unwrap();
        assert_eq!(auto, "\"auto\"");
        let back: MakeupGain = serde_json::from_str(&auto).unwrap();
        assert!(matches!(back, MakeupGain::Auto));

        let db = serde_json::to_string(&MakeupGain::Db(3.0)).unwrap();
        let back: MakeupGain = serde_json::from_str(&db).unwrap();
        assert!(matches!(back, MakeupGain::Db(v) if (v - 3.0).abs() < 1e-6));
    }

    #[test]
    fn makeup_gain_parses_auto_from_toml_case_insensitively() {
        #[derive(Deserialize)]
        struct Holder {
            makeup_db: MakeupGain,
        }
        for tok in ["\"auto\"", "\"Auto\"", "\"AUTO\""] {
            let h: Holder = toml::from_str(&format!("makeup_db = {tok}")).unwrap();
            assert!(matches!(h.makeup_db, MakeupGain::Auto), "token {tok}");
        }
        let h: Holder = toml::from_str("makeup_db = -3.5").unwrap();
        assert!(matches!(h.makeup_db, MakeupGain::Db(v) if (v + 3.5).abs() < 1e-6));
        // A bogus string is a hard error, not a silent skip.
        assert!(toml::from_str::<Holder>("makeup_db = \"loud\"").is_err());
    }
}
