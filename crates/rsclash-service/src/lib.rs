//! Authenticated local IPC for the rsclash system service.

#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod daemon;
#[cfg(target_os = "linux")]
mod install;
#[cfg(target_os = "linux")]
mod lifecycle;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod transport;

use std::path::PathBuf;

use rsclash_domain::{CoreChannel, CoreState};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(target_os = "linux")]
pub use config::{InstalledServiceConfig, ServiceBinaries};
#[cfg(target_os = "linux")]
pub use daemon::CoreServiceHandler;
#[cfg(target_os = "linux")]
pub use install::{InstallIdentity, InstallLayout, InstallRequest, SystemServiceInstaller};
#[cfg(target_os = "linux")]
pub use lifecycle::LinuxServiceController;
#[cfg(unix)]
pub use server::{ServiceRequestHandler, ServiceServer};
#[cfg(unix)]
pub use transport::ServiceClient;

pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_SERVICE_SOCKET: &str = "/run/rsclash/service.sock";
pub const DEFAULT_INSTALLED_CONFIG: &str = "/etc/rsclash/service.json";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Request {
  pub protocol_version: u16,
  pub request_id: u64,
  pub command: ServiceCommand,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ServiceCommand {
  Ping,
  Status,
  StartCore { channel: CoreChannel },
  StopCore,
  ReloadCore,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Response {
  pub protocol_version: u16,
  pub request_id: u64,
  pub result: std::result::Result<ResponseData, RemoteError>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResponseData {
  Pong { service_version: String },
  Status(ServiceStatus),
  CoreState(CoreState),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
  pub service_version: String,
  pub core: CoreState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteErrorCode {
  InvalidRequest,
  UnsupportedVersion,
  Lifecycle,
  Internal,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
#[error("{message}")]
pub struct RemoteError {
  pub code: RemoteErrorCode,
  pub message: String,
}

impl RemoteError {
  pub fn new(code: RemoteErrorCode, message: impl Into<String>) -> Self {
    Self {
      code,
      message: message.into(),
    }
  }
}

#[derive(Debug, Error)]
pub enum Error {
  #[error("service IPC operation timed out")]
  TimedOut,
  #[error("service IPC frame is too large: {bytes} bytes")]
  FrameTooLarge { bytes: usize },
  #[error("service IPC peer UID {actual} is not authorized; expected {expected}")]
  UnauthorizedPeer { expected: u32, actual: u32 },
  #[error("service IPC protocol mismatch: expected {expected}, received {actual}")]
  ProtocolMismatch { expected: u16, actual: u16 },
  #[error("service IPC response ID mismatch: expected {expected}, received {actual}")]
  ResponseMismatch { expected: u64, actual: u64 },
  #[error("service IPC path is unsafe: {0}")]
  UnsafePath(PathBuf),
  #[error("invalid service installation: {0}")]
  InvalidInstallation(String),
  #[error("service installation command failed: {0}")]
  InstallCommand(String),
  #[error("failed to encode service IPC: {0}")]
  Encode(#[source] serde_json::Error),
  #[error("failed to decode service IPC: {0}")]
  Decode(#[source] serde_json::Error),
  #[error("service request failed: {0}")]
  Remote(#[from] RemoteError),
  #[error("service IPC I/O failed: {0}")]
  Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
