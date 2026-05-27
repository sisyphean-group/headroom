//! daemon IPC server (server side of `IPC.md`). one blocking thread per
//! connection. no async runtime.

pub mod broadcast;
mod connection;
mod ops;
mod server;

pub use server::{IpcServer, IpcServerHandle};

/// re-exported so the profile file-watcher reuses the same path as the
/// `profile.reload` op.
pub(crate) use ops::execute_reload;
