//! daemon error types.

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DaemonError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("pipewire: {0}")]
    PipeWire(String),

    #[error("pipewire object not found: {0}")]
    PipeWireNotFound(String),

    #[error("profile: {0}")]
    Profile(String),

    #[error("daemon shutting down")]
    Shutdown,

    #[error("{0}")]
    Other(String),
}

impl DaemonError {
    pub fn pipewire(msg: impl std::fmt::Display) -> Self {
        DaemonError::PipeWire(msg.to_string())
    }

    pub fn other(msg: impl std::fmt::Display) -> Self {
        DaemonError::Other(msg.to_string())
    }
}
