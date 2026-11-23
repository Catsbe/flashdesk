pub mod auth;
pub mod framing;
pub mod noise;
pub mod relay;
pub mod session;
pub mod transfer;

pub use auth::DeviceCert;
pub use relay::ChannelKind;
pub use session::VideoCodec;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("noise transport error: {0}")]
    Noise(String),

    #[error("framing error: {0}")]
    Framing(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

pub fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
