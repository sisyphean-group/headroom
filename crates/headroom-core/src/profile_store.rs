//! profile store: disk profiles + persisted overlay → effective profile. overlay persists across
//! `profile.use` intentionally (`route.set obs bypass` = "prefer obs bypassed in general")

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use headroom_ipc::{Route, RouteRule, RouteRuleMatch};

use crate::profile::Profile;

pub const BUILTIN_DEFAULT_NAME: &str = "default";

const OVERLAY_FILE: &str = "overlay.toml";
const PROFILES_DIR: &str = "profiles";

#[derive(Debug, Clone)]
pub struct StoredProfile {
    /// matches `profile.name`; file-name mismatch surfaced as a warning on load
    pub name: String,
    pub source: ProfileSource,
    pub profile: Profile,
}

#[derive(Debug, Clone)]
pub enum ProfileSource {
    Builtin,
    Shipped(PathBuf),
    User(PathBuf),
}

impl ProfileSource {
    /// true if a user-authored file can replace this source.
    #[must_use]
    pub fn is_overridable(&self) -> bool {
        !matches!(self, ProfileSource::User(_))
    }
}

/// persisted user choices riding on top of the active profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UserOverlay {
    /// `None` = built-in default
    pub active_name: Option<String>,
    /// keyed by `application.process.binary`
    pub route_overrides: BTreeMap<String, Route>,
    /// `toml::Value` since the overlay is toml on disk; converted to json at materialization
    pub setting_overrides: BTreeMap<String, toml::Value>,
    /// intentionally persisted across restarts
    pub bypass_global: bool,
    /// keyed by app label (process_binary or application_name); see [`apply_per_app_overlay`]
    #[serde(default)]
    pub per_app_overrides: BTreeMap<String, bool>,
    /// `None` keeps the profile's `per_app.enabled`; `Some` forces it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_app_master: Option<bool>,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("profile '{0}' not loaded")]
    ProfileNotFound(String),
    #[error("setting '{0}' not found in active profile")]
    SettingNotFound(String),
    #[error("setting '{key}' rejected: {msg}")]
    SettingInvalid {
        key: String,
        msg: String,
    },
    #[error("no user-set route for '{0}'")]
    NoUserRoute(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("overlay parse: {0}")]
    OverlayParse(toml::de::Error),
    #[error("overlay serialize: {0}")]
    OverlaySerialize(toml::ser::Error),
}

#[derive(Debug, Default, Clone)]
pub struct ReloadReport {
    pub loaded: Vec<String>,
    /// reload is best-effort: a broken file is reported and skipped, not fatal
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct StorePaths {
    /// profiles live in `<config_dir>/profiles/`
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// `profiles/` appended before scanning
    pub share_dirs: Vec<PathBuf>,
    /// scanned directly (no `profiles/` suffix) and last, so they win on name conflict; backs
    /// `--profile-dir`
    pub extra_profile_dirs: Vec<PathBuf>,
}

impl StorePaths {
    /// share dirs from `$XDG_DATA_DIRS` so package-installed profiles resolve regardless of prefix
    /// (incl. NixOS `/run/current-system/sw/share`).
    #[must_use]
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let config_root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let state_root = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("state"));
        let data_dirs = std::env::var_os("XDG_DATA_DIRS").unwrap_or_default();
        Self {
            config_dir: config_root.join("headroom"),
            state_dir: state_root.join("headroom"),
            share_dirs: share_dirs_from_data_dirs(&data_dirs),
            extra_profile_dirs: Vec::new(),
        }
    }
}

/// each entry gets a `headroom/` subdir; empty falls back to `/usr/local/share:/usr/share`.
/// reversed because xdg precedence is first-wins but `load` scans later-shadows-earlier.
fn share_dirs_from_data_dirs(data_dirs: &std::ffi::OsStr) -> Vec<PathBuf> {
    let value = if data_dirs.is_empty() {
        std::ffi::OsString::from("/usr/local/share:/usr/share")
    } else {
        data_dirs.to_os_string()
    };
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&value)
        .map(|p| p.join("headroom"))
        .collect();
    dirs.reverse();
    dirs
}

#[derive(Debug)]
pub struct ProfileStore {
    profiles: BTreeMap<String, StoredProfile>,
    overlay: UserOverlay,
    /// cached materialization of `profiles[active] + overlay`
    effective: Profile,
    /// `None` for in-memory stores (tests, `builtin`)
    overlay_path: Option<PathBuf>,
    /// remembered so `reload` doesn't need them threaded back in
    paths: Option<StorePaths>,
    /// set when `active_name` isn't on disk; `effective` falls back to builtin meanwhile
    active_missing: Option<String>,
    /// one-shot warnings for the runtime to drain at startup
    pending_warnings: Vec<String>,
}

impl ProfileStore {
    /// in-memory store with only the built-in default.
    #[must_use]
    pub fn builtin() -> Self {
        let builtin = StoredProfile {
            name: BUILTIN_DEFAULT_NAME.into(),
            source: ProfileSource::Builtin,
            profile: Profile::default_v0(),
        };
        let mut profiles = BTreeMap::new();
        profiles.insert(builtin.name.clone(), builtin);
        Self {
            profiles,
            overlay: UserOverlay::default(),
            effective: Profile::default_v0(),
            overlay_path: None,
            paths: None,
            active_missing: None,
            pending_warnings: Vec::new(),
        }
    }

    /// order: built-in → share dirs → user dir → extra dirs; later shadows earlier by name.
    /// per-file parse errors become warnings + skip; only unrecoverable i/o errors out.
    pub fn load(paths: &StorePaths) -> Result<Self, StoreError> {
        let mut store = Self::builtin();
        store.overlay_path = Some(paths.state_dir.join(OVERLAY_FILE));
        store.paths = Some(paths.clone());

        for share in &paths.share_dirs {
            let dir = share.join(PROFILES_DIR);
            scan_dir_into(&mut store.profiles, &dir, true, &mut store.pending_warnings);
        }
        let user_profiles = paths.config_dir.join(PROFILES_DIR);
        scan_dir_into(
            &mut store.profiles,
            &user_profiles,
            false,
            &mut store.pending_warnings,
        );
        // scanned directly and last so they win on name conflict
        for dir in &paths.extra_profile_dirs {
            scan_dir_into(&mut store.profiles, dir, false, &mut store.pending_warnings);
        }

        if let Some(path) = &store.overlay_path {
            match fs::read_to_string(path) {
                Ok(text) => match toml::from_str::<UserOverlay>(&text) {
                    Ok(overlay) => store.overlay = overlay,
                    Err(e) => store.pending_warnings.push(format!(
                        "overlay at {} failed to parse, ignoring: {e}",
                        path.display()
                    )),
                },
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::Io(e)),
            }
        }

        store.refresh_active_missing();
        store.rematerialize();
        Ok(store)
    }

    #[must_use]
    pub fn effective(&self) -> &Profile {
        &self.effective
    }

    /// watch dir: first `--profile-dir` override if present, else user config `profiles/`.
    #[must_use]
    pub fn primary_profile_watch_dir(&self) -> Option<PathBuf> {
        let paths = self.paths.as_ref()?;
        Some(
            paths
                .extra_profile_dirs
                .first()
                .cloned()
                .unwrap_or_else(|| paths.config_dir.join(PROFILES_DIR)),
        )
    }

    /// returns the *requested* name even when missing on disk (so operators see the mismatch).
    #[must_use]
    pub fn active_name(&self) -> &str {
        self.overlay
            .active_name
            .as_deref()
            .unwrap_or(BUILTIN_DEFAULT_NAME)
    }

    #[must_use]
    pub fn bypass_global(&self) -> bool {
        self.overlay.bypass_global
    }

    /// overlay setting overrides as json; values that fail toml→json conversion are dropped.
    #[must_use]
    pub fn setting_overrides(&self) -> BTreeMap<String, Value> {
        self.overlay
            .setting_overrides
            .iter()
            .filter_map(|(k, v)| toml_to_json(v).ok().map(|jv| (k.clone(), jv)))
            .collect()
    }

    pub fn list(&self) -> impl Iterator<Item = &StoredProfile> {
        self.profiles.values()
    }

    /// `Some(name)` if the overlay-selected profile is unknown to the store.
    #[must_use]
    pub fn is_active_missing(&self) -> Option<&str> {
        self.active_missing.as_deref()
    }

    /// consuming drain; use [`Self::warnings`] for a non-consuming snapshot.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_warnings)
    }

    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.pending_warnings.clone()
    }

    /// overlay overrides are preserved across the switch — the whole point of the overlay.
    pub fn use_profile(&mut self, name: &str) -> Result<(), StoreError> {
        if !self.profiles.contains_key(name) {
            return Err(StoreError::ProfileNotFound(name.to_owned()));
        }
        self.overlay.active_name = Some(name.to_owned());
        self.active_missing = None;
        self.rematerialize();
        self.persist_overlay()?;
        Ok(())
    }

    /// prepended to the effective rule list, shadowing profile rules for the same app.
    pub fn set_route(&mut self, app: &str, route: Route) -> Result<(), StoreError> {
        self.overlay.route_overrides.insert(app.to_owned(), route);
        self.rematerialize();
        self.persist_overlay()?;
        Ok(())
    }

    pub fn unset_route(&mut self, app: &str) -> Result<(), StoreError> {
        if self.overlay.route_overrides.remove(app).is_none() {
            return Err(StoreError::NoUserRoute(app.to_owned()));
        }
        self.rematerialize();
        self.persist_overlay()?;
        Ok(())
    }

    /// validated by trial materialization; rejected overrides leave the overlay unchanged.
    pub fn set_setting(&mut self, key: &str, value: Value) -> Result<(), StoreError> {
        let mut trial = self.overlay.clone();
        let toml_value = json_to_toml(&value).map_err(|msg| StoreError::SettingInvalid {
            key: key.to_owned(),
            msg,
        })?;
        trial.setting_overrides.insert(key.to_owned(), toml_value);
        let probe = materialize(&self.profiles, &trial);
        match probe {
            Materialized::Ok(_) => {}
            Materialized::SettingTypeError { offending_key, err } if offending_key == key => {
                return Err(StoreError::SettingInvalid {
                    key: key.to_owned(),
                    msg: err,
                });
            }
            Materialized::SettingMissing { offending_key } if offending_key == key => {
                return Err(StoreError::SettingNotFound(key.to_owned()));
            }
            // shouldn't happen (trial = overlay + one key), but surface defensively
            Materialized::SettingTypeError { offending_key, err } => {
                return Err(StoreError::SettingInvalid {
                    key: format!("{key} (caused {offending_key} to fail)"),
                    msg: err,
                });
            }
            Materialized::SettingMissing { offending_key } => {
                return Err(StoreError::SettingNotFound(format!(
                    "{key} (caused {offending_key} to be unreachable)"
                )));
            }
        }
        self.overlay = trial;
        self.rematerialize();
        self.persist_overlay()?;
        Ok(())
    }

    /// idempotent; returns whether an override was present.
    pub fn clear_setting(&mut self, key: &str) -> Result<bool, StoreError> {
        let existed = self.overlay.setting_overrides.remove(key).is_some();
        if existed {
            self.rematerialize();
            self.persist_overlay()?;
        }
        Ok(existed)
    }

    /// leaves route / per-app / bypass overrides intact; returns the count cleared.
    pub fn clear_all_settings(&mut self) -> Result<usize, StoreError> {
        let n = self.overlay.setting_overrides.len();
        if n > 0 {
            self.overlay.setting_overrides.clear();
            self.rematerialize();
            self.persist_overlay()?;
        }
        Ok(n)
    }

    pub fn set_bypass(&mut self, enabled: bool) -> Result<(), StoreError> {
        self.overlay.bypass_global = enabled;
        self.persist_overlay()?;
        Ok(())
    }

    /// registry thread reconciles managed taps on the matching `PwCommand::ReevaluateLayerA`.
    pub fn set_per_app_enabled(&mut self, app: &str, enabled: bool) -> Result<(), StoreError> {
        self.overlay
            .per_app_overrides
            .insert(app.to_owned(), enabled);
        self.rematerialize();
        self.persist_overlay()?;
        Ok(())
    }

    pub fn set_per_app_master(&mut self, enabled: bool) -> Result<(), StoreError> {
        self.overlay.per_app_master = Some(enabled);
        self.rematerialize();
        self.persist_overlay()?;
        Ok(())
    }

    #[must_use]
    pub fn per_app_master(&self) -> bool {
        self.effective.per_app.enabled
    }

    /// atomic: on fatal i/o the in-memory state is left untouched; per-file parse errors warn+skip.
    pub fn reload(&mut self) -> Result<ReloadReport, StoreError> {
        let Some(paths) = self.paths.clone() else {
            return Ok(ReloadReport {
                loaded: vec![BUILTIN_DEFAULT_NAME.into()],
                warnings: vec!["store has no disk paths; nothing to reload".into()],
            });
        };
        // clean slate so `warnings()` reflects current state, not the union of all past reloads
        self.pending_warnings.clear();
        let mut warnings = Vec::new();
        let mut new_profiles: BTreeMap<String, StoredProfile> = BTreeMap::new();
        new_profiles.insert(
            BUILTIN_DEFAULT_NAME.into(),
            StoredProfile {
                name: BUILTIN_DEFAULT_NAME.into(),
                source: ProfileSource::Builtin,
                profile: Profile::default_v0(),
            },
        );
        for share in &paths.share_dirs {
            let dir = share.join(PROFILES_DIR);
            scan_dir_into(&mut new_profiles, &dir, true, &mut warnings);
        }
        let user_profiles = paths.config_dir.join(PROFILES_DIR);
        scan_dir_into(&mut new_profiles, &user_profiles, false, &mut warnings);
        for dir in &paths.extra_profile_dirs {
            scan_dir_into(&mut new_profiles, dir, false, &mut warnings);
        }

        let loaded: Vec<String> = new_profiles.keys().cloned().collect();
        self.profiles = new_profiles;
        self.refresh_active_missing();
        self.rematerialize();
        // rematerialize may have dropped invalid overrides — persist to keep disk consistent
        if let Err(e) = self.persist_overlay() {
            warnings.push(format!("could not persist overlay after reload: {e}"));
        }
        self.pending_warnings.extend(warnings.iter().cloned());
        Ok(ReloadReport { loaded, warnings })
    }

    /// overrides that no longer typecheck warn but stay in the overlay (user may switch back).
    fn rematerialize(&mut self) {
        let mut warnings = Vec::new();
        let mat = materialize(&self.profiles, &self.overlay);
        let profile = match mat {
            Materialized::Ok(p) => p,
            // override failed against the active profile — keep it stored, materialize skipping it
            Materialized::SettingMissing { offending_key }
            | Materialized::SettingTypeError {
                offending_key,
                err: _,
            } => {
                warnings.push(format!(
                    "setting override '{offending_key}' doesn't apply to active profile; \
                     keeping the override for future use"
                ));
                materialize_skipping(&self.profiles, &self.overlay, &[offending_key])
            }
        };
        self.pending_warnings.extend(warnings);
        self.effective = profile;
    }

    fn refresh_active_missing(&mut self) {
        match self.overlay.active_name.as_deref() {
            Some(name) if !self.profiles.contains_key(name) => {
                self.active_missing = Some(name.to_owned());
                self.pending_warnings.push(format!(
                    "active profile '{name}' is not on disk; using built-in default until \
                     the profile is restored or `profile.use` selects a known one"
                ));
            }
            _ => self.active_missing = None,
        }
    }

    fn persist_overlay(&self) -> Result<(), StoreError> {
        let Some(path) = self.overlay_path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(&self.overlay).map_err(StoreError::OverlaySerialize)?;
        atomic_write(path, body.as_bytes())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Materialization
// ---------------------------------------------------------------------------

enum Materialized {
    Ok(Profile),
    SettingMissing {
        offending_key: String,
    },
    SettingTypeError {
        offending_key: String,
        err: String,
    },
}

fn pick_base<'a>(profiles: &'a BTreeMap<String, StoredProfile>, overlay: &UserOverlay) -> &'a Profile {
    let name = overlay
        .active_name
        .as_deref()
        .unwrap_or(BUILTIN_DEFAULT_NAME);
    profiles
        .get(name)
        .or_else(|| profiles.get(BUILTIN_DEFAULT_NAME))
        .map(|sp| &sp.profile)
        .expect("ProfileStore always contains the built-in default")
}

fn materialize(
    profiles: &BTreeMap<String, StoredProfile>,
    overlay: &UserOverlay,
) -> Materialized {
    let base = pick_base(profiles, overlay);
    let mut json = match serde_json::to_value(base) {
        Ok(v) => v,
        Err(e) => {
            return Materialized::SettingTypeError {
                offending_key: "<profile>".into(),
                err: format!("base profile failed to serialise: {e}"),
            };
        }
    };
    for (key, value) in &overlay.setting_overrides {
        let json_value = match toml_to_json(value) {
            Ok(v) => v,
            Err(e) => {
                return Materialized::SettingTypeError {
                    offending_key: key.clone(),
                    err: e,
                };
            }
        };
        if !set_dotted(&mut json, key, json_value) {
            return Materialized::SettingMissing {
                offending_key: key.clone(),
            };
        }
    }
    let mut materialised: Profile = match serde_json::from_value(json) {
        Ok(p) => p,
        Err(e) => {
            return Materialized::SettingTypeError {
                offending_key: "<profile>".into(),
                err: e.to_string(),
            };
        }
    };
    apply_route_overrides(&mut materialised, &overlay.route_overrides);
    apply_per_app_overlay(
        &mut materialised,
        overlay.per_app_master,
        &overlay.per_app_overrides,
    );
    Materialized::Ok(materialised)
}

fn materialize_skipping(
    profiles: &BTreeMap<String, StoredProfile>,
    overlay: &UserOverlay,
    skip_keys: &[String],
) -> Profile {
    let base = pick_base(profiles, overlay);
    // already-validated path: an error here means the base profile is malformed — fall back
    let mut json = serde_json::to_value(base).unwrap_or_else(|_| {
        serde_json::to_value(Profile::default_v0()).expect("default_v0 always serialises")
    });
    for (key, value) in &overlay.setting_overrides {
        if skip_keys.iter().any(|s| s == key) {
            continue;
        }
        if let Ok(jv) = toml_to_json(value) {
            let _ = set_dotted(&mut json, key, jv);
        }
    }
    let mut materialised: Profile =
        serde_json::from_value(json).unwrap_or_else(|_| Profile::default_v0());
    apply_route_overrides(&mut materialised, &overlay.route_overrides);
    apply_per_app_overlay(
        &mut materialised,
        overlay.per_app_master,
        &overlay.per_app_overrides,
    );
    materialised
}

fn apply_route_overrides(profile: &mut Profile, overrides: &BTreeMap<String, Route>) {
    // two single-field rules per override, not one AND-shape rule: the matcher ANDs non-empty
    // fields, and many tools (pw-cat, electron/flatpak wrappers) set only `application.name`, so a
    // both-fields rule would miss them. two rules = OR across identity fields; first-match wins.
    //
    // no retain pre-pass: `materialize` is stateless so overlay rules can't accumulate; a retain
    // would only drop base-profile rules that coincidentally match the overlay shape (data loss).
    let mut new_rules: Vec<RouteRule> = Vec::with_capacity(overrides.len() * 2);
    for (app, route) in overrides {
        new_rules.push(RouteRule {
            match_: RouteRuleMatch {
                process_binary: vec![app.clone()],
                ..Default::default()
            },
            route: *route,
        });
        new_rules.push(RouteRule {
            match_: RouteRuleMatch {
                application_name: vec![app.clone()],
                ..Default::default()
            },
            route: *route,
        });
    }
    new_rules.extend(std::mem::take(&mut profile.rules));
    profile.rules = new_rules;
}

/// master first (`per_app_master` wins over profile's `enabled`); then per-app: flip a matching
/// authored rule in place (preserving thresholds), else prepend two synthetic single-field rules
/// (process_binary + application_name OR-shape) so the override wins first-match iteration.
fn apply_per_app_overlay(
    profile: &mut Profile,
    master: Option<bool>,
    overrides: &BTreeMap<String, bool>,
) {
    if let Some(enabled) = master {
        profile.per_app.enabled = enabled;
    }
    if overrides.is_empty() {
        return;
    }
    let mut prepend: Vec<crate::profile::PerAppRule> = Vec::new();
    for (app, enabled) in overrides {
        let mut matched_existing = false;
        for rule in &mut profile.per_app.rules {
            let m = &rule.match_;
            if m.process_binary.iter().any(|p| p == app)
                || m.application_name.iter().any(|n| n == app)
            {
                rule.enabled = *enabled;
                matched_existing = true;
            }
        }
        if matched_existing {
            continue;
        }
        prepend.push(crate::profile::PerAppRule::defaulted(
            RouteRuleMatch {
                process_binary: vec![app.clone()],
                ..Default::default()
            },
            *enabled,
        ));
        prepend.push(crate::profile::PerAppRule::defaulted(
            RouteRuleMatch {
                application_name: vec![app.clone()],
                ..Default::default()
            },
            *enabled,
        ));
    }
    prepend.extend(std::mem::take(&mut profile.per_app.rules));
    profile.per_app.rules = prepend;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn scan_dir_into(
    out: &mut BTreeMap<String, StoredProfile>,
    dir: &Path,
    shipped: bool,
    warnings: &mut Vec<String>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => {
            warnings.push(format!("can't scan {}: {e}", dir.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("read {}: {e}", path.display()));
                continue;
            }
        };
        let profile: Profile = match toml::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warnings.push(format!("parse {}: {e}", path.display()));
                continue;
            }
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        if stem != profile.name {
            warnings.push(format!(
                "{}: file name '{stem}' doesn't match profile.name '{}' — using profile.name",
                path.display(),
                profile.name
            ));
        }
        let name = profile.name.clone();
        let source = if shipped {
            ProfileSource::Shipped(path)
        } else {
            ProfileSource::User(path)
        };
        out.insert(name.clone(), StoredProfile { name, source, profile });
    }
}

fn set_dotted(value: &mut Value, key: &str, new: Value) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return false;
    };
    let mut cur = value;
    for part in parents {
        cur = match cur.get_mut(*part) {
            Some(v) => v,
            None => return false,
        };
    }
    let Some(map) = cur.as_object_mut() else {
        return false;
    };
    if !map.contains_key(*last) {
        return false;
    }
    map.insert((*last).to_string(), new);
    true
}

fn toml_to_json(v: &toml::Value) -> Result<Value, String> {
    match v {
        toml::Value::String(s) => Ok(Value::String(s.clone())),
        toml::Value::Integer(i) => Ok(Value::from(*i)),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .ok_or_else(|| "non-finite float in setting override".into()),
        toml::Value::Boolean(b) => Ok(Value::Bool(*b)),
        toml::Value::Datetime(d) => Ok(Value::String(d.to_string())),
        toml::Value::Array(arr) => arr.iter().map(toml_to_json).collect::<Result<Vec<_>, _>>().map(Value::Array),
        toml::Value::Table(t) => {
            let mut map = serde_json::Map::new();
            for (k, v) in t {
                map.insert(k.clone(), toml_to_json(v)?);
            }
            Ok(Value::Object(map))
        }
    }
}

fn json_to_toml(v: &Value) -> Result<toml::Value, String> {
    match v {
        Value::Null => Err("null is not representable in TOML overlay".into()),
        Value::Bool(b) => Ok(toml::Value::Boolean(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(toml::Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(toml::Value::Float(f))
            } else {
                Err("number out of range for TOML".into())
            }
        }
        Value::String(s) => Ok(toml::Value::String(s.clone())),
        Value::Array(arr) => arr
            .iter()
            .map(json_to_toml)
            .collect::<Result<Vec<_>, _>>()
            .map(toml::Value::Array),
        Value::Object(obj) => {
            let mut t = toml::value::Table::new();
            for (k, v) in obj {
                t.insert(k.clone(), json_to_toml(v)?);
            }
            Ok(toml::Value::Table(t))
        }
    }
}

/// stage to a tmp file then `rename` over the target (atomic within a filesystem on linux).
fn atomic_write(path: &Path, body: &[u8]) -> io::Result<()> {
    let pid = std::process::id();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(
        "{}.tmp-{pid}-{stamp}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("overlay")
    );
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, body)?;
    match fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_paths() -> (StorePaths, tempdir::TempDir) {
        // We don't actually depend on the `tempdir` crate in this
        // crate — use std::env::temp_dir + a unique subdir.
        let base = std::env::temp_dir().join(format!(
            "headroom-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(base.join("config/profiles")).unwrap();
        fs::create_dir_all(base.join("state")).unwrap();
        let paths = StorePaths {
            config_dir: base.join("config"),
            state_dir: base.join("state"),
            share_dirs: vec![],
            extra_profile_dirs: vec![],
        };
        // We hand back a guard that removes the dir on drop.
        // The `tempdir` alias is faked via the wrapper below.
        (paths, tempdir::TempDir(base))
    }

    // Tiny inline tempdir guard to avoid a new dependency.
    mod tempdir {
        use std::path::PathBuf;
        pub struct TempDir(pub PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn share_dirs_from_xdg_data_dirs_appends_headroom_and_reverses() {
        // First data dir must end up scanned last (so it wins under
        // `load`'s later-shadows-earlier rule), and each entry gains a
        // `headroom` segment.
        let dirs = share_dirs_from_data_dirs(std::ffi::OsStr::new(
            "/run/current-system/sw/share:/usr/share",
        ));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/usr/share/headroom"),
                PathBuf::from("/run/current-system/sw/share/headroom"),
            ]
        );
    }

    #[test]
    fn share_dirs_empty_falls_back_to_fhs_default() {
        let dirs = share_dirs_from_data_dirs(std::ffi::OsStr::new(""));
        // Reversed `/usr/local/share:/usr/share` default.
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/usr/share/headroom"),
                PathBuf::from("/usr/local/share/headroom"),
            ]
        );
    }

    #[test]
    fn builtin_store_has_default_profile() {
        let s = ProfileStore::builtin();
        assert_eq!(s.active_name(), "default");
        assert_eq!(s.effective().name, "default");
        assert!(!s.bypass_global());
        assert!(s.list().any(|p| p.name == "default"));
    }

    #[test]
    fn load_with_empty_dirs_yields_builtin_only() {
        let (paths, _g) = tmp_paths();
        let s = ProfileStore::load(&paths).unwrap();
        assert_eq!(s.list().count(), 1);
        assert_eq!(s.effective().name, "default");
    }

    #[test]
    fn user_profile_overrides_shipped_by_name() {
        let (paths, _g) = tmp_paths();
        let shipped = paths.config_dir.parent().unwrap().join("share/profiles");
        fs::create_dir_all(&shipped).unwrap();
        fs::write(
            shipped.join("night.toml"),
            "name = \"night\"\ndescription = \"shipped night\"\n",
        )
        .unwrap();
        fs::write(
            paths.config_dir.join("profiles/night.toml"),
            "name = \"night\"\ndescription = \"user night\"\n",
        )
        .unwrap();
        let paths2 = StorePaths {
            share_dirs: vec![shipped.parent().unwrap().to_path_buf()],
            ..paths
        };
        let s = ProfileStore::load(&paths2).unwrap();
        let night = s.list().find(|p| p.name == "night").unwrap();
        assert!(matches!(night.source, ProfileSource::User(_)));
        assert_eq!(night.profile.description, "user night");
    }

    #[test]
    fn extra_profile_dir_scanned_directly_and_wins() {
        let (paths, _g) = tmp_paths();
        // A user profile that a same-named --profile-dir entry overrides.
        fs::write(
            paths.config_dir.join("profiles/movie.toml"),
            "name = \"movie\"\ndescription = \"user movie\"\n",
        )
        .unwrap();
        // The override dir holds movie + an extra profile, scanned with
        // no `profiles/` suffix.
        let extra = paths.config_dir.parent().unwrap().join("repo-profiles");
        fs::create_dir_all(&extra).unwrap();
        fs::write(
            extra.join("movie.toml"),
            "name = \"movie\"\ndescription = \"repo movie\"\n",
        )
        .unwrap();
        fs::write(
            extra.join("party.toml"),
            "name = \"party\"\ndescription = \"repo party\"\n",
        )
        .unwrap();
        let paths2 = StorePaths {
            extra_profile_dirs: vec![extra.clone()],
            ..paths
        };
        let s = ProfileStore::load(&paths2).unwrap();
        // Both override profiles are present...
        assert!(s.list().any(|p| p.name == "party"));
        // ...and the override wins over the user profile of the same name.
        let movie = s.list().find(|p| p.name == "movie").unwrap();
        assert_eq!(movie.profile.description, "repo movie");
        // The watch dir is the override dir, not the config dir.
        assert_eq!(s.primary_profile_watch_dir(), Some(extra));
    }

    #[test]
    fn parse_error_is_warning_not_fatal() {
        let (paths, _g) = tmp_paths();
        fs::write(
            paths.config_dir.join("profiles/broken.toml"),
            "this is not = valid = toml",
        )
        .unwrap();
        let mut s = ProfileStore::load(&paths).unwrap();
        let warnings = s.take_warnings();
        assert!(warnings.iter().any(|w| w.contains("broken.toml")));
        // Built-in still present and active.
        assert_eq!(s.effective().name, "default");
    }

    #[test]
    fn use_profile_unknown_errors() {
        let mut s = ProfileStore::builtin();
        assert!(matches!(
            s.use_profile("does-not-exist"),
            Err(StoreError::ProfileNotFound(_))
        ));
    }

    #[test]
    fn set_route_appears_in_effective_rules() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.set_route("obs", Route::Bypass).unwrap();
        // First rule should now be the override.
        let rule = &s.effective().rules[0];
        assert_eq!(rule.match_.process_binary, vec!["obs".to_string()]);
        assert_eq!(rule.route, Route::Bypass);
    }

    #[test]
    fn set_route_emits_both_process_binary_and_application_name_rules() {
        // The route.set CLI verb accepts a single app identifier
        // but streams can advertise themselves via either
        // `application.process.binary` or `application.name`
        // (or neither — those go through default_route). The
        // overlay materialises BOTH single-field rules so each
        // possible identity field is covered.
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.set_route("pw-cat", Route::Bypass).unwrap();
        let rules = &s.effective().rules;
        let proc_rule = rules
            .iter()
            .find(|r| r.match_.process_binary == vec!["pw-cat".to_string()])
            .expect("process_binary rule");
        assert_eq!(proc_rule.route, Route::Bypass);
        assert!(proc_rule.match_.application_name.is_empty());
        let name_rule = rules
            .iter()
            .find(|r| r.match_.application_name == vec!["pw-cat".to_string()])
            .expect("application_name rule");
        assert_eq!(name_rule.route, Route::Bypass);
        assert!(name_rule.match_.process_binary.is_empty());
    }

    #[test]
    fn set_route_then_unset_leaves_no_residual_rules() {
        // Both the process_binary and application_name variants
        // of a single-app override must clear on unset; otherwise
        // a re-add would stack rules and the matcher would carry
        // dead entries indefinitely.
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.set_route("pw-cat", Route::Bypass).unwrap();
        s.unset_route("pw-cat").unwrap();
        let residual: Vec<_> = s
            .effective()
            .rules
            .iter()
            .filter(|r| {
                r.match_.process_binary == vec!["pw-cat".to_string()]
                    || r.match_.application_name == vec!["pw-cat".to_string()]
            })
            .collect();
        assert!(residual.is_empty(), "leftover override rules: {residual:#?}");
    }

    #[test]
    fn user_rule_with_overlay_shape_survives_set_route_for_same_app() {
        // Regression for Codex audit Q5: an earlier retain pre-pass in
        // `apply_route_overrides` would silently drop any base-profile
        // rule whose single-field shape coincided with the overlay's
        // emit pattern. The fix is to delete the retain entirely —
        // prepending already makes the overlay win first-match
        // iteration, and removing the retain closes the data-loss
        // surface. This test pins the surviving-rule behaviour so a
        // future refactor can't quietly reintroduce the prune.
        let (paths, _g) = tmp_paths();
        fs::write(
            paths.config_dir.join("profiles/custom.toml"),
            r#"
name = "custom"
description = "user custom"
default_route = { route = "processed" }
[[rules]]
match = { process_binary = ["obs"] }
route = "processed"
"#,
        )
        .unwrap();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.use_profile("custom").unwrap();
        // Sanity: user rule is loaded once.
        assert_eq!(s.effective().rules.len(), 1);

        s.set_route("obs", Route::Bypass).unwrap();

        let rules = &s.effective().rules;
        // Two overlay rules (process_binary + application_name) plus
        // the preserved user rule.
        assert_eq!(rules.len(), 3, "rules: {rules:#?}");
        assert_eq!(rules[0].route, Route::Bypass);
        assert_eq!(rules[0].match_.process_binary, vec!["obs".to_string()]);
        assert_eq!(rules[1].route, Route::Bypass);
        assert_eq!(rules[1].match_.application_name, vec!["obs".to_string()]);
        assert_eq!(rules[2].route, Route::Processed);
        assert_eq!(rules[2].match_.process_binary, vec!["obs".to_string()]);
    }

    #[test]
    fn set_route_updates_existing_override() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.set_route("obs", Route::Bypass).unwrap();
        s.set_route("obs", Route::Processed).unwrap();
        let obs_rules: Vec<_> = s
            .effective()
            .rules
            .iter()
            .filter(|r| r.match_.process_binary == vec!["obs".to_string()])
            .collect();
        assert_eq!(obs_rules.len(), 1);
        assert_eq!(obs_rules[0].route, Route::Processed);
    }

    #[test]
    fn unset_route_missing_errors() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        assert!(matches!(
            s.unset_route("never-set"),
            Err(StoreError::NoUserRoute(_))
        ));
    }

    #[test]
    fn set_setting_changes_effective_profile() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.set_setting("limiter.ceiling_dbtp", serde_json::json!(-1.0))
            .unwrap();
        assert!((s.effective().limiter.ceiling_dbtp - -1.0).abs() < 1e-6);
    }

    #[test]
    fn clear_setting_reverts_to_profile() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        let profile_default = s.effective().limiter.ceiling_dbtp;
        s.set_setting("limiter.ceiling_dbtp", serde_json::json!(-3.0))
            .unwrap();
        assert!((s.effective().limiter.ceiling_dbtp - -3.0).abs() < 1e-6);

        // Clearing a present override reverts the effective value and
        // reports it existed; clearing again is a no-op.
        assert!(s.clear_setting("limiter.ceiling_dbtp").unwrap());
        assert!((s.effective().limiter.ceiling_dbtp - profile_default).abs() < 1e-6);
        assert!(!s.clear_setting("limiter.ceiling_dbtp").unwrap());
    }

    #[test]
    fn clear_all_settings_reverts_everything_and_counts() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        let ceiling0 = s.effective().limiter.ceiling_dbtp;
        let target0 = s.effective().agc.target_lufs;
        s.set_setting("limiter.ceiling_dbtp", serde_json::json!(-3.0))
            .unwrap();
        s.set_setting("agc.target_lufs", serde_json::json!(-9.0))
            .unwrap();

        assert_eq!(s.clear_all_settings().unwrap(), 2);
        assert!((s.effective().limiter.ceiling_dbtp - ceiling0).abs() < 1e-6);
        assert!((s.effective().agc.target_lufs - target0).abs() < 1e-6);
        assert!(s.setting_overrides().is_empty());
        // Idempotent: nothing left to clear.
        assert_eq!(s.clear_all_settings().unwrap(), 0);
    }

    #[test]
    fn set_setting_rejects_wrong_type() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        let err = s
            .set_setting("limiter.ceiling_dbtp", serde_json::json!("nope"))
            .unwrap_err();
        assert!(matches!(err, StoreError::SettingInvalid { .. }));
        // Effective unchanged.
        assert!((s.effective().limiter.ceiling_dbtp - -0.1).abs() < 1e-6);
    }

    #[test]
    fn set_setting_rejects_unknown_key() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        let err = s
            .set_setting("limiter.no_such_field", serde_json::json!(1))
            .unwrap_err();
        assert!(matches!(err, StoreError::SettingNotFound(_)));
    }

    #[test]
    fn overlay_survives_profile_use() {
        let (paths, _g) = tmp_paths();
        // Ship a second profile.
        fs::write(
            paths.config_dir.join("profiles/night.toml"),
            "name = \"night\"\ndescription = \"loud night\"\n",
        )
        .unwrap();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.set_route("obs", Route::Bypass).unwrap();
        s.set_setting("limiter.ceiling_dbtp", serde_json::json!(-2.0))
            .unwrap();
        s.use_profile("night").unwrap();
        assert_eq!(s.effective().name, "night");
        assert_eq!(s.effective().rules[0].match_.process_binary, vec!["obs".to_string()]);
        assert!((s.effective().limiter.ceiling_dbtp - -2.0).abs() < 1e-6);
    }

    #[test]
    fn overlay_is_persisted_and_reloaded() {
        let (paths, _g) = tmp_paths();
        {
            let mut s = ProfileStore::load(&paths).unwrap();
            s.set_route("obs", Route::Bypass).unwrap();
            s.set_bypass(true).unwrap();
            s.set_setting("limiter.ceiling_dbtp", serde_json::json!(-3.0))
                .unwrap();
        }
        let mut s2 = ProfileStore::load(&paths).unwrap();
        // Drop the load warnings — they should be empty here.
        assert!(s2.take_warnings().is_empty());
        assert!(s2.bypass_global());
        assert!((s2.effective().limiter.ceiling_dbtp - -3.0).abs() < 1e-6);
        assert_eq!(s2.effective().rules[0].match_.process_binary, vec!["obs".to_string()]);
    }

    #[test]
    fn missing_active_profile_falls_back_to_builtin() {
        let (paths, _g) = tmp_paths();
        // Write an overlay pointing at a profile that doesn't exist.
        let overlay_text =
            "active_name = \"night\"\nbypass_global = false\n[route_overrides]\n[setting_overrides]\n";
        fs::write(paths.state_dir.join(OVERLAY_FILE), overlay_text).unwrap();

        let mut s = ProfileStore::load(&paths).unwrap();
        assert_eq!(s.is_active_missing(), Some("night"));
        // effective() falls back to default_v0 via pick_base.
        assert_eq!(s.effective().name, "default");
        let warnings = s.take_warnings();
        assert!(warnings.iter().any(|w| w.contains("night")));
    }

    #[test]
    fn reload_picks_up_new_profile() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        assert_eq!(s.list().count(), 1);

        fs::write(
            paths.config_dir.join("profiles/extra.toml"),
            "name = \"extra\"\ndescription = \"hot reloaded\"\n",
        )
        .unwrap();
        let report = s.reload().unwrap();
        assert!(report.loaded.iter().any(|n| n == "extra"));
        assert!(report.warnings.is_empty());
        assert!(s.list().any(|p| p.name == "extra"));
    }

    #[test]
    fn per_app_master_override_forces_enabled() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        // default profile ships per_app.enabled = false.
        assert!(!s.effective().per_app.enabled);
        s.set_per_app_master(true).unwrap();
        assert!(s.effective().per_app.enabled);
        assert!(s.per_app_master());
        s.set_per_app_master(false).unwrap();
        assert!(!s.effective().per_app.enabled);
    }

    #[test]
    fn per_app_override_prepends_synthetic_rule_when_no_match() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.set_per_app_enabled("strawberry", true).unwrap();
        let rules = &s.effective().per_app.rules;
        // Two synthetic single-field rules (process_binary + application_name).
        let proc_rule = rules
            .iter()
            .find(|r| r.match_.process_binary == vec!["strawberry".to_string()])
            .expect("process_binary rule");
        assert!(proc_rule.enabled);
        let name_rule = rules
            .iter()
            .find(|r| r.match_.application_name == vec!["strawberry".to_string()])
            .expect("application_name rule");
        assert!(name_rule.enabled);
    }

    #[test]
    fn per_app_override_flips_existing_rule_preserving_thresholds() {
        let (paths, _g) = tmp_paths();
        fs::write(
            paths.config_dir.join("profiles/la.toml"),
            r#"
name = "la"
description = "layer a custom"
[per_app]
enabled = true
[[per_app.rules]]
match = { process_binary = ["strawberry"] }
enabled = true
max_cut_db = 18.0
"#,
        )
        .unwrap();
        let mut s = ProfileStore::load(&paths).unwrap();
        s.use_profile("la").unwrap();
        s.set_per_app_enabled("strawberry", false).unwrap();
        // No synthetic prepend; the existing rule's enabled flips and
        // its custom max_cut is preserved.
        let strawberry: Vec<_> = s
            .effective()
            .per_app
            .rules
            .iter()
            .filter(|r| r.match_.process_binary == vec!["strawberry".to_string()])
            .collect();
        assert_eq!(strawberry.len(), 1, "should not have prepended a duplicate");
        assert!(!strawberry[0].enabled);
        assert!((strawberry[0].max_cut_db - 18.0).abs() < 1e-6);
    }

    #[test]
    fn per_app_overlay_persists_across_reload() {
        let (paths, _g) = tmp_paths();
        {
            let mut s = ProfileStore::load(&paths).unwrap();
            s.set_per_app_master(true).unwrap();
            s.set_per_app_enabled("strawberry", false).unwrap();
        }
        let s2 = ProfileStore::load(&paths).unwrap();
        assert!(s2.effective().per_app.enabled);
        let disabled = s2
            .effective()
            .per_app
            .rules
            .iter()
            .find(|r| r.match_.process_binary == vec!["strawberry".to_string()])
            .expect("override rule");
        assert!(!disabled.enabled);
    }

    #[test]
    fn reload_with_broken_file_keeps_daemon_running() {
        let (paths, _g) = tmp_paths();
        let mut s = ProfileStore::load(&paths).unwrap();
        fs::write(
            paths.config_dir.join("profiles/broken.toml"),
            "this == not valid",
        )
        .unwrap();
        let report = s.reload().unwrap();
        assert!(report.warnings.iter().any(|w| w.contains("broken.toml")));
        // Default still active.
        assert_eq!(s.effective().name, "default");
    }
}
