use std::{
  fmt,
  path::{Path, PathBuf},
  time::Duration,
};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControllerEndpoint {
  UnixSocket(PathBuf),
  NamedPipe(String),
  Http { host: String, port: u16 },
}

impl ControllerEndpoint {
  pub fn unix_socket(path: impl Into<PathBuf>) -> Self {
    Self::UnixSocket(path.into())
  }

  pub fn named_pipe(path: impl Into<String>) -> Self {
    Self::NamedPipe(path.into())
  }

  pub fn http(host: impl Into<String>, port: u16) -> Self {
    Self::Http {
      host: host.into(),
      port,
    }
  }

  pub const fn is_local(&self) -> bool {
    matches!(self, Self::UnixSocket(_) | Self::NamedPipe(_))
  }

  pub fn socket_path(&self) -> Option<&Path> {
    match self {
      Self::UnixSocket(path) => Some(path),
      Self::NamedPipe(_) | Self::Http { .. } => None,
    }
  }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct ControllerSecret(String);

impl ControllerSecret {
  pub fn new(value: impl Into<String>) -> Self {
    Self(value.into())
  }

  pub fn expose(&self) -> &str {
    &self.0
  }

  pub const fn is_empty(&self) -> bool {
    self.0.is_empty()
  }
}

impl fmt::Debug for ControllerSecret {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("ControllerSecret([REDACTED])")
  }
}

impl fmt::Display for ControllerSecret {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("[REDACTED]")
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControllerConfig {
  pub endpoint: ControllerEndpoint,
  pub secret: ControllerSecret,
  pub request_timeout: Duration,
  pub max_safe_retries: u8,
}

impl ControllerConfig {
  pub fn local(endpoint: ControllerEndpoint) -> Self {
    debug_assert!(endpoint.is_local());
    Self::new(endpoint)
  }

  pub fn http(host: impl Into<String>, port: u16, secret: ControllerSecret) -> Self {
    Self::new(ControllerEndpoint::http(host, port)).with_secret(secret)
  }

  pub fn new(endpoint: ControllerEndpoint) -> Self {
    Self {
      endpoint,
      secret: ControllerSecret::default(),
      request_timeout: DEFAULT_REQUEST_TIMEOUT,
      max_safe_retries: 2,
    }
  }

  pub fn with_secret(mut self, secret: ControllerSecret) -> Self {
    self.secret = secret;
    self
  }

  pub const fn with_request_timeout(mut self, timeout: Duration) -> Self {
    self.request_timeout = timeout;
    self
  }

  pub const fn with_max_safe_retries(mut self, retries: u8) -> Self {
    self.max_safe_retries = retries;
    self
  }
}

#[cfg(test)]
mod tests {
  use super::{ControllerConfig, ControllerEndpoint, ControllerSecret};

  #[test]
  fn secrets_are_redacted() {
    let secret = ControllerSecret::new("do-not-log-this");

    assert_eq!(secret.expose(), "do-not-log-this");
    assert!(!format!("{secret:?}").contains("do-not-log-this"));
    assert!(!secret.to_string().contains("do-not-log-this"));
  }

  #[test]
  fn local_configuration_has_no_implicit_tcp_fallback() {
    let config = ControllerConfig::local(ControllerEndpoint::unix_socket("/tmp/mihomo.sock"));

    assert!(config.endpoint.is_local());
    assert!(config.secret.is_empty());
  }
}
