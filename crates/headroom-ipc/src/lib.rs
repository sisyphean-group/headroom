//! headroom control-protocol types and framing. spec: `IPC.md` (repo root); this is its rust binding.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod codec;
mod error;
mod proto;

pub use codec::{Codec, DEFAULT_MAX_FRAME_BYTES, MIN_MAX_FRAME_BYTES};
pub use error::{Error, ErrorCode, ProtoError};
pub use proto::{
    DaemonEvent, Event, HelloData, LayerALevel, LayerASnapshot, MeterTick, Op, ProfileEvent,
    ProfileInfo, Request, Response, ResponsePayload, Route, RouteList, RouteRule, RouteRuleMatch,
    RoutingEvent, ServerFrame, SinkInfo, Sinks, Status, StreamRoute, Topic,
};

/// wire version; bumped only on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// socket dir stem; full path `${XDG_RUNTIME_DIR}/headroom/control.sock`, falling back to `/run/user/$UID/...`.
pub const DEFAULT_SOCKET_DIR: &str = "headroom";

pub const DEFAULT_SOCKET_NAME: &str = "control.sock";

/// honours `XDG_RUNTIME_DIR`, else falls back to `/run/user/$UID/...`. `None` when neither is determinable.
#[must_use]
pub fn default_socket_path() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return Some(
                PathBuf::from(dir)
                    .join(DEFAULT_SOCKET_DIR)
                    .join(DEFAULT_SOCKET_NAME),
            );
        }
    }
    // uid from /proc/self/status: nix-free, dependency-light
    let uid = read_self_uid()?;
    Some(
        PathBuf::from(format!("/run/user/{uid}"))
            .join(DEFAULT_SOCKET_DIR)
            .join(DEFAULT_SOCKET_NAME),
    )
}

fn read_self_uid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let first = rest.split_whitespace().next()?;
            return first.parse().ok();
        }
    }
    None
}
