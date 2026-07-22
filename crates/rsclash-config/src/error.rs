use std::{io, path::PathBuf};

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
  #[error("script execution failed: {0}")]
  ScriptExecution(String),
  #[error("Mihomo configuration validation failed: {0}")]
  RuntimeValidation(String),
  #[error("runtime activation failed: {0}")]
  RuntimeActivation(String),
  #[error(
    "runtime activation failed: {activation_error}; compensation also failed: {compensation_error}"
  )]
  DeploymentCompensation {
    activation_error: String,
    compensation_error: String,
  },
  #[error("invalid profile path: {0}")]
  InvalidProfilePath(String),
  #[error("invalid draft state: expected {expected}, found {actual}")]
  InvalidDraftState {
    expected: &'static str,
    actual: &'static str,
  },
  #[error("failed to {action} {path}: {source}")]
  Io {
    action: &'static str,
    path: PathBuf,
    #[source]
    source: io::Error,
  },
  #[error("commit failed: {commit_error}; rollback also failed: {rollback_error}")]
  CommitRollback {
    commit_error: String,
    rollback_error: String,
  },
}

impl Error {
  pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
    Self::Io {
      action,
      path: path.into(),
      source,
    }
  }
}
