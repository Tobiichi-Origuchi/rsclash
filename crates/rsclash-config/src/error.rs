use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to decode YAML: {0}")]
    DecodeYaml(#[source] serde_yaml_ng::Error),
    #[error("failed to encode YAML: {0}")]
    EncodeYaml(#[source] serde_yaml_ng::Error),
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
}
