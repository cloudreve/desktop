/// Platform-agnostic error types for the sync provider abstraction layer.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("operation not supported on this platform")]
    NotSupported,
    #[error("operation failed: {0}")]
    Failed(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type PlatformResult<T> = Result<T, PlatformError>;
