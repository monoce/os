use thiserror::Error;

#[derive(Debug, Error)]
pub enum OsError {
    #[error("VM not found: {0}")]
    VmNotFound(String),

    #[error("VM already exists: {0}")]
    VmAlreadyExists(String),

    #[error("VM is in invalid state {current} for operation {operation}")]
    InvalidState { current: String, operation: String },

    #[error("resource limit exceeded: {resource} (limit: {limit}, requested: {requested})")]
    ResourceLimitExceeded {
        resource: String,
        limit: String,
        requested: String,
    },

    #[error("firecracker error: {0}")]
    Firecracker(String),

    #[error("firecracker API error: {status} {body}")]
    FirecrackerApi { status: u16, body: String },

    #[error("kernel image not found: {0}")]
    KernelNotFound(String),

    #[error("rootfs not found: {0}")]
    RootfsNotFound(String),

    #[error("network configuration error: {0}")]
    Network(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("image error: {0}")]
    Image(String),

    #[error("socket error: {0}")]
    Socket(String),

    #[error("timeout waiting for VM {0}")]
    Timeout(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("monoce error: {0}")]
    Monoce(#[from] monoce_common::Error),
}

pub type OsResult<T> = std::result::Result<T, OsError>;
