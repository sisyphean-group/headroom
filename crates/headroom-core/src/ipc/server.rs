//! unix-domain socket listener + accept loop.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::DaemonError;
use crate::ipc::connection::handle_connection;
use crate::state::SharedState;

/// accept-thread shutdown poll interval.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// owner-only ($XDG_RUNTIME_DIR/headroom/).
const SOCKET_DIR_MODE: u32 = 0o700;

/// owner-only — IPC auth is filesystem perms, as PipeWire/Wayland do.
const SOCKET_MODE: u32 = 0o600;

pub struct IpcServer;

/// drop or [`IpcServerHandle::shutdown`] to stop the server.
pub struct IpcServerHandle {
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    socket_path: PathBuf,
}

impl IpcServer {
    /// bind a [`UnixListener`] at `socket_path` and spawn the accept
    /// thread. stale sockets are unlinked; a reachable path means
    /// another daemon is running → [`DaemonError::Other`].
    ///
    /// # Errors
    /// - [`DaemonError::Io`] for filesystem failures.
    /// - [`DaemonError::Other`] if another daemon owns the path.
    pub fn start(
        socket_path: PathBuf,
        state: SharedState,
    ) -> Result<IpcServerHandle, DaemonError> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
            // best-effort: $XDG_RUNTIME_DIR is already user-owned.
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(SOCKET_DIR_MODE));
        }

        if socket_path.exists() {
            if UnixStream::connect(&socket_path).is_ok() {
                return Err(DaemonError::other(format!(
                    "another headroom daemon is already listening at {}",
                    socket_path.display()
                )));
            }
            std::fs::remove_file(&socket_path)?;
            tracing::debug!(path = %socket_path.display(), "removed stale socket");
        }

        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
        listener.set_nonblocking(true)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = shutdown.clone();
        let state_for_thread = state;
        let accept_thread = thread::Builder::new()
            .name("headroom-ipc-accept".into())
            .spawn(move || accept_loop(listener, state_for_thread, shutdown_for_thread))?;

        tracing::info!(path = %socket_path.display(), "ipc server listening");

        Ok(IpcServerHandle {
            shutdown,
            accept_thread: Some(accept_thread),
            socket_path,
        })
    }
}

impl IpcServerHandle {
    /// stop + join the accept thread and unlink the socket. idempotent.
    /// connection threads outlive this; they exit when peers close.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept_thread.take() {
            if let Err(e) = t.join() {
                tracing::warn!(?e, "ipc accept thread join failed");
            }
        }
        // remove the socket file so the next daemon sees no stale entry.
        let _ = std::fs::remove_file(&self.socket_path);
        tracing::info!(path = %self.socket_path.display(), "ipc server stopped");
    }
}

impl Drop for IpcServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: UnixListener,
    state: SharedState,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // connection threads use blocking I/O.
                if let Err(e) = stream.set_nonblocking(false) {
                    tracing::warn!(error = %e, "set_nonblocking(false) failed; dropping conn");
                    continue;
                }
                let shutdown_for_conn = shutdown.clone();
                let state_for_conn = state.clone();
                if let Err(e) = thread::Builder::new()
                    .name("headroom-ipc-conn".into())
                    .spawn(move || handle_connection(stream, state_for_conn, shutdown_for_conn))
                {
                    tracing::warn!(error = %e, "ipc conn spawn failed");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) => {
                tracing::error!(error = %e, "ipc accept failed; stopping accept loop");
                break;
            }
        }
    }
    tracing::debug!("ipc accept loop exited");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store::ProfileStore;
    use crate::state::{self, DaemonState};
    use headroom_client::Client;
    use headroom_ipc::Route;
    use std::process;
    use std::sync::atomic::AtomicU64;

    static NEXT_TEST_SOCKET: AtomicU64 = AtomicU64::new(0);

    fn temp_socket_path() -> PathBuf {
        let n = NEXT_TEST_SOCKET.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("headroom-test-{}-{}.sock", process::id(), n))
    }

    fn test_state() -> SharedState {
        state::shared(DaemonState::new(ProfileStore::builtin()))
    }

    #[test]
    fn start_and_shutdown_cleanly() {
        let path = temp_socket_path();
        let _ = std::fs::remove_file(&path);

        let mut handle = IpcServer::start(path.clone(), test_state()).expect("server should start");
        assert!(path.exists(), "socket file should exist while server runs");
        handle.shutdown();
        assert!(!path.exists(), "shutdown should unlink the socket");
    }

    #[test]
    fn rejects_double_start() {
        let path = temp_socket_path();
        let _ = std::fs::remove_file(&path);

        let _first = IpcServer::start(path.clone(), test_state()).expect("first server should start");
        let second = IpcServer::start(path.clone(), test_state());
        assert!(
            second.is_err(),
            "second server should refuse to take over a live socket"
        );
    }

    #[test]
    fn stale_socket_is_reclaimed() {
        let path = temp_socket_path();
        let _ = std::fs::remove_file(&path);

        // Create a "stale" socket file with no listener behind it.
        {
            let _l = UnixListener::bind(&path).expect("bind for stale fixture");
        }
        // Listener dropped; the file remains but is unreachable.

        let handle = IpcServer::start(path.clone(), test_state());
        assert!(
            handle.is_ok(),
            "should reclaim a stale socket (file present, no listener)"
        );
    }

    #[test]
    fn client_can_status_and_setting_get() {
        let path = temp_socket_path();
        let _ = std::fs::remove_file(&path);
        let _server = IpcServer::start(path.clone(), test_state()).expect("server should start");

        let mut client = Client::connect_at(&path).expect("client connect");
        let hello = client.hello();
        assert_eq!(hello.daemon, "headroom");
        assert_eq!(hello.protocol, headroom_ipc::PROTOCOL_VERSION);

        // Read-only ops land in 4b.
        let status = client.status().expect("status should succeed");
        assert_eq!(status.profile, "default");
        assert_eq!(status.protocol, headroom_ipc::PROTOCOL_VERSION);

        let value = client
            .setting_get("limiter.ceiling_dbtp")
            .expect("setting.get should succeed");
        let n = value.as_f64().unwrap();
        assert!((n - -0.1).abs() < 1e-6);
    }

    /// End-to-end through the IPC: load a store with a second profile
    /// on disk, switch to it via `profile.use`, and confirm that an
    /// overlay tweak made on the original profile carries across.
    #[test]
    fn client_profile_use_preserves_overlay() {
        use crate::profile_store::{ProfileStore, StorePaths};
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let base = std::env::temp_dir().join(format!(
            "headroom-e2e-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _guard = scopeguard_remove(base.clone());
        fs::create_dir_all(base.join("config/profiles")).unwrap();
        fs::create_dir_all(base.join("state")).unwrap();
        fs::write(
            base.join("config/profiles/night.toml"),
            "name = \"night\"\ndescription = \"loud night\"\n[limiter]\nceiling_dbtp = -2.0\n",
        )
        .unwrap();
        let paths = StorePaths {
            config_dir: base.join("config"),
            state_dir: base.join("state"),
            share_dirs: vec![],
            extra_profile_dirs: vec![],
        };
        let store = ProfileStore::load(&paths).expect("store load");
        let state = state::shared(DaemonState::new(store));

        let sock = temp_socket_path();
        let _ = std::fs::remove_file(&sock);
        let _server = IpcServer::start(sock.clone(), state).expect("server should start");

        let mut client = Client::connect_at(&sock).expect("client connect");

        // Apply an overlay tweak while on `default`.
        client
            .route_set("obs", Route::Bypass)
            .expect("route.set obs");
        client
            .setting_set("agc.target_lufs", serde_json::json!(-22.0))
            .expect("setting.set agc.target_lufs");

        // Switch to `night`.
        let switched_to = client.profile_use("night").expect("profile.use night");
        assert_eq!(switched_to, "night");
        let status = client.status().unwrap();
        assert_eq!(status.profile, "night");

        // Overlay survived: route override is still visible in route.list,
        // and the setting override still wins over night.toml's value.
        let routes = client.route_list().unwrap();
        let user_rule = routes
            .rules
            .iter()
            .find(|r| r.match_.process_binary == vec!["obs".to_string()])
            .expect("obs override carried across profile switch");
        assert_eq!(user_rule.route, Route::Bypass);

        let lufs = client.setting_get("agc.target_lufs").unwrap();
        assert!((lufs.as_f64().unwrap() - -22.0).abs() < 1e-6);

        // night.toml's limiter ceiling shows through where there's no override.
        let ceiling = client.setting_get("limiter.ceiling_dbtp").unwrap();
        assert!((ceiling.as_f64().unwrap() - -2.0).abs() < 1e-6);
    }

    fn scopeguard_remove(path: PathBuf) -> impl Drop {
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        Cleanup(path)
    }
}
