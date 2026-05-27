//! daemon core: pipewire main loop, filter pipeline, registry, routing, agc, ipc

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agc;
pub mod app_level;
pub mod error;
pub mod ipc;
pub mod meters;
pub mod profile;
pub mod profile_store;
pub mod profile_watcher;
pub mod pw;
pub mod routing;
pub mod runtime;
pub mod state;

pub use error::DaemonError;
pub use profile::Profile;
pub use profile_store::{ProfileStore, StorePaths, StoreError, UserOverlay};

/// blocks until shutdown (SIGTERM/SIGINT) or fatal failure. profiles + overlay from xdg paths.
pub fn run() -> Result<(), DaemonError> {
    run_with_profile_dirs(Vec::new())
}

/// like [`run`] but also scans `extra_profile_dirs` directly for `*.toml` and watches
/// the first for live edits. backs the daemon's `--profile-dir` flag.
pub fn run_with_profile_dirs(extra_profile_dirs: Vec<std::path::PathBuf>) -> Result<(), DaemonError> {
    let mut paths = StorePaths::from_env();
    paths.extra_profile_dirs = extra_profile_dirs;
    let store = ProfileStore::load(&paths)
        .map_err(|e| DaemonError::Profile(format!("loading profiles: {e}")))?;
    runtime::run(store)
}
