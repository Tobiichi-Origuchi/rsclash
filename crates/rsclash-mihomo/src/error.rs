use std::time::Duration;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid Mihomo controller configuration: {0}")]
    InvalidConfiguration(String),
    #[error("the controller transport is not supported on this platform: {0}")]
    UnsupportedTransport(&'static str),
    #[error("Mihomo request timed out after {0:?}")]
    Timeout(Duration),
    #[error("Mihomo transport failed: {0}")]
    Transport(String),
    #[error("Mihomo WebSocket failed: {0}")]
    WebSocket(String),
    #[error("Mihomo returned HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },
    #[error("failed to decode the Mihomo {context} response: {source}")]
    Decode {
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to encode a Mihomo request: {0}")]
    Encode(#[from] serde_json::Error),
}
