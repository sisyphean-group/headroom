//! error types.

use serde::{Deserialize, Serialize};

/// wire error code in `error.code`. adding variants is non-breaking; removing/renaming bumps `PROTOCOL_VERSION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ErrorCode {
    /// malformed framing or non-json payload; connection is closed.
    InvalidFrame,
    /// valid json, no known message shape.
    InvalidMessage,
    /// `op` names no known operation.
    UnknownOp,
    /// `args` missing a field, wrong type, or out of range.
    InvalidArgs,
    /// named profile / app / stream / setting key does not exist.
    NotFound,
    /// would violate an invariant.
    Conflict,
    /// daemon transiently cannot serve the request.
    Busy,
    /// server-side bug; `message` carries detail.
    Internal,
}

impl ErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidFrame => "INVALID_FRAME",
            ErrorCode::InvalidMessage => "INVALID_MESSAGE",
            ErrorCode::UnknownOp => "UNKNOWN_OP",
            ErrorCode::InvalidArgs => "INVALID_ARGS",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::Conflict => "CONFLICT",
            ErrorCode::Busy => "BUSY",
            ErrorCode::Internal => "INTERNAL",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// error payload inside a `Response`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtoError {
    pub code: ErrorCode,
    /// not stable; do not pattern match.
    pub message: String,
}

impl ProtoError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtoError {}

/// errors from the codec and helpers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("frame too large: {actual} bytes (limit {limit})")]
    FrameTooLarge { actual: usize, limit: usize },

    #[error("protocol: {0}")]
    Protocol(#[from] ProtoError),

    /// e.g. a response with a mismatched id.
    #[error("unexpected frame: {0}")]
    UnexpectedFrame(String),

    #[error("connection closed")]
    Closed,
}

impl Error {
    /// true when the connection should be torn down.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::Io(_) | Error::FrameTooLarge { .. } | Error::Closed
        )
    }
}
