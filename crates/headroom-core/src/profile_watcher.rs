//! debounced profile-dir watcher; fires the same reload path as ipc `profile.reload`

use std::path::PathBuf;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

use crate::error::DaemonError;
use crate::ipc::execute_reload;
use crate::state::SharedState;

/// quiet period before firing; past the typical editor save→rename storm
const DEBOUNCE: Duration = Duration::from_millis(500);

/// drop to stop the background thread.
pub struct ProfileWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

impl ProfileWatcher {
    /// `Ok(None)` if `profiles_dir` doesn't exist yet (user authored no custom profiles).
    pub fn start(profiles_dir: PathBuf, state: SharedState) -> Result<Option<Self>, DaemonError> {
        if !profiles_dir.exists() {
            tracing::debug!(
                path = %profiles_dir.display(),
                "profile dir not present; file-watch reload disabled"
            );
            return Ok(None);
        }

        let state_for_cb = state;
        let mut debouncer = new_debouncer(
            DEBOUNCE,
            move |result: DebounceEventResult| match result {
                Ok(events) if !events.is_empty() => {
                    tracing::info!(events = events.len(), "profile dir changed; auto-reloading");
                    match execute_reload(&state_for_cb) {
                        Ok(report) => {
                            for w in &report.warnings {
                                tracing::warn!(warning = %w, "auto-reload warning");
                            }
                        }
                        Err(e) => tracing::error!(error = %e, "auto-reload failed"),
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "profile watcher backend error");
                }
            },
        )
        .map_err(|e| DaemonError::other(format!("debouncer init: {e}")))?;

        debouncer
            .watcher()
            .watch(&profiles_dir, RecursiveMode::NonRecursive)
            .map_err(|e| {
                DaemonError::other(format!("watch {}: {e}", profiles_dir.display()))
            })?;

        tracing::info!(
            path = %profiles_dir.display(),
            debounce_ms = DEBOUNCE.as_millis() as u64,
            "profile dir watcher armed"
        );
        Ok(Some(Self {
            _debouncer: debouncer,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store::{ProfileStore, StorePaths};
    use crate::state::{self, DaemonState};
    use std::fs;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    /// Build an isolated config/state tree and load a `ProfileStore`
    /// against it. Returns the paths and a guard that cleans up the
    /// dir on drop.
    fn tmp_paths() -> (StorePaths, TmpGuard) {
        let base = std::env::temp_dir().join(format!(
            "headroom-watcher-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(base.join("config/profiles")).unwrap();
        fs::create_dir_all(base.join("state")).unwrap();
        let paths = StorePaths {
            config_dir: base.join("config"),
            state_dir: base.join("state"),
            share_dirs: vec![],
            extra_profile_dirs: vec![],
        };
        (paths, TmpGuard(base))
    }

    struct TmpGuard(std::path::PathBuf);
    impl Drop for TmpGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_profile_dir_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-no-dir-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        // dir does not exist.
        let store = ProfileStore::builtin();
        let state = state::shared(DaemonState::new(store));
        let watcher = ProfileWatcher::start(dir, state).expect("graceful no-op");
        assert!(watcher.is_none());
    }

    #[test]
    fn dropping_a_new_profile_triggers_reload() {
        let (paths, _g) = tmp_paths();
        let store = ProfileStore::load(&paths).unwrap();
        let state = state::shared(DaemonState::new(store));
        let profiles_dir = paths.config_dir.join("profiles");

        let _watcher = ProfileWatcher::start(profiles_dir.clone(), state.clone())
            .expect("watcher start")
            .expect("dir present");

        // Initially: only builtin "default" is known.
        assert_eq!(state.lock().profiles.list().count(), 1);

        // Drop a new profile in. The debouncer waits 500 ms; allow up
        // to 5 s before declaring failure (CI fs latency).
        fs::write(
            profiles_dir.join("hot.toml"),
            "name = \"hot\"\ndescription = \"hot-reloaded\"\n",
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_new = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            if state.lock().profiles.list().any(|p| p.name == "hot") {
                saw_new = true;
                break;
            }
        }
        assert!(saw_new, "watcher should have reloaded after file appeared");
    }
}
