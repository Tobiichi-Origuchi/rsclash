//! Stable, UI-independent application protocol and state models.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum AppStatus {
  #[default]
  Booting,
  Ready,
  ShuttingDown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ThemeMode {
  #[default]
  System,
  Light,
  Dark,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Page {
  #[default]
  Home,
  Proxies,
  Profiles,
  Connections,
  Rules,
  Logs,
  Unlock,
  Settings,
}

impl Page {
  pub const ALL: [Self; 8] = [
    Self::Home,
    Self::Proxies,
    Self::Profiles,
    Self::Connections,
    Self::Rules,
    Self::Logs,
    Self::Unlock,
    Self::Settings,
  ];

  pub const fn label(self) -> &'static str {
    match self {
      Self::Home => "首页",
      Self::Proxies => "代理",
      Self::Profiles => "订阅",
      Self::Connections => "连接",
      Self::Rules => "规则",
      Self::Logs => "日志",
      Self::Unlock => "测试",
      Self::Settings => "设置",
    }
  }

  pub const fn symbol(self) -> &'static str {
    match self {
      Self::Home => "⌂",
      Self::Proxies => "◉",
      Self::Profiles => "☷",
      Self::Connections => "⇄",
      Self::Rules => "⑂",
      Self::Logs => "≡",
      Self::Unlock => "✓",
      Self::Settings => "⚙",
    }
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum CoreState {
  #[default]
  Stopped,
  Starting,
  Running {
    mode: CoreRunMode,
    version: Option<String>,
  },
  Reloading,
  Stopping,
  Failed {
    message: String,
  },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CoreRunMode {
  Sidecar,
  Service,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppSnapshot {
  pub revision: u64,
  pub status: AppStatus,
  pub page: Page,
  pub theme: ThemeMode,
  pub window_visible: bool,
  pub core: CoreState,
  pub last_error: Option<ErrorView>,
}

impl Default for AppSnapshot {
  fn default() -> Self {
    Self {
      revision: 0,
      status: AppStatus::Booting,
      page: Page::Home,
      theme: ThemeMode::System,
      window_visible: true,
      core: CoreState::Stopped,
      last_error: None,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorView {
  pub title: String,
  pub detail: String,
  pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum UiCommand {
  Ping,
  Navigate(Page),
  SetTheme(ThemeMode),
  SetWindowVisible(bool),
  ToggleWindow,
  ClearError,
  Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandOutput {
  Accepted,
  Pong,
  ShutdownAccepted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AppEvent {
  BackendReady,
  NavigationChanged(Page),
  ThemeChanged(ThemeMode),
  WindowVisibilityChanged(bool),
  ShuttingDown,
}

#[derive(Clone, Debug, Error, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandError {
  #[error("the command is not available while the application is shutting down")]
  ShuttingDown,
  #[error("invalid application state: {0}")]
  InvalidState(String),
}

pub type CommandResult = Result<CommandOutput, CommandError>;

/// A serializable secret whose `Debug` and `Display` implementations never expose its value.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SensitiveString(String);

impl SensitiveString {
  pub fn new(value: impl Into<String>) -> Self {
    Self(value.into())
  }

  pub fn expose(&self) -> &str {
    &self.0
  }

  pub fn into_inner(self) -> String {
    self.0
  }

  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }
}

impl fmt::Debug for SensitiveString {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("SensitiveString([REDACTED])")
  }
}

impl fmt::Display for SensitiveString {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("[REDACTED]")
  }
}

#[cfg(test)]
mod tests {
  use super::{AppSnapshot, AppStatus, Page, SensitiveString, ThemeMode};

  #[test]
  fn default_snapshot_is_safe_and_visible() {
    let snapshot = AppSnapshot::default();

    assert_eq!(snapshot.status, AppStatus::Booting);
    assert_eq!(snapshot.page, Page::Home);
    assert_eq!(snapshot.theme, ThemeMode::System);
    assert!(snapshot.window_visible);
  }

  #[test]
  fn navigation_has_stable_order() {
    assert_eq!(Page::ALL.first(), Some(&Page::Home));
    assert_eq!(Page::ALL.last(), Some(&Page::Settings));
  }

  #[test]
  fn sensitive_strings_are_redacted_in_human_facing_formats() {
    let secret = SensitiveString::new("controller-secret");

    assert_eq!(secret.expose(), "controller-secret");
    assert!(!format!("{secret:?}").contains("controller-secret"));
    assert!(!secret.to_string().contains("controller-secret"));
  }
}
