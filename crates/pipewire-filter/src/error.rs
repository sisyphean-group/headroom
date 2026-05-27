//! error type for the [`pipewire-filter`](crate) crate.

#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    /// `pw_filter_new` returned NULL — alloc failure or, since we own
    /// every arg, an internal bug.
    #[error("pw_filter_new returned NULL")]
    CreationFailed,

    /// `pw_filter_add_port` returned NULL — malformed props or format POD.
    #[error("pw_filter_add_port returned NULL")]
    AddPortFailed,

    /// `pw_filter_connect` returned a negative error code.
    #[error("pw_filter_connect failed: {0}")]
    ConnectFailed(std::io::Error),

}
