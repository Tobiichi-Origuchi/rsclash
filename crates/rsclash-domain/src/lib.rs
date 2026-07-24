//! Stable, UI-independent application protocol and state models.

use std::{collections::BTreeMap, fmt};

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
    #[serde(default)]
    channel: CoreChannel,
    version: Option<String>,
  },
  Reloading,
  Stopping,
  Failed {
    message: String,
  },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoreChannel {
  #[default]
  Stable,
  Alpha,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CoreRunMode {
  Sidecar,
  Service,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum MihomoConnection {
  #[default]
  Offline,
  Connecting,
  Connected,
  Degraded,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
  #[default]
  Rule,
  Global,
  Direct,
  Unknown(String),
}

impl ProxyMode {
  pub const fn as_str(&self) -> &str {
    match self {
      Self::Rule => "rule",
      Self::Global => "global",
      Self::Direct => "direct",
      Self::Unknown(value) => value.as_str(),
    }
  }
}

impl From<&str> for ProxyMode {
  fn from(value: &str) -> Self {
    match value.to_ascii_lowercase().as_str() {
      "rule" => Self::Rule,
      "global" => Self::Global,
      "direct" => Self::Direct,
      _ => Self::Unknown(value.to_string()),
    }
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrafficSnapshot {
  pub upload_bytes_per_second: u64,
  pub download_bytes_per_second: u64,
  pub upload_total: u64,
  pub download_total: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyOptionSnapshot {
  pub name: String,
  pub alive: bool,
  pub delay_ms: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyGroupSnapshot {
  pub name: String,
  pub kind: String,
  pub selected: Option<String>,
  pub options: Vec<ProxyOptionSnapshot>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProxyViewOrderSource {
  Runtime,
  #[default]
  Fallback,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProxyViewProviderState {
  Ready,
  #[default]
  Unavailable,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyCapabilities {
  pub udp: bool,
  pub uot: bool,
  pub xudp: bool,
  pub tfo: bool,
  pub mptcp: bool,
  pub smux: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProxyNodeSource {
  Core {
    proxy_name: String,
  },
  Provider {
    provider_name: String,
    proxy_name: String,
  },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProxyMemberUnresolvedReason {
  Missing,
  Ambiguous,
  ProviderUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProxyMemberSnapshot {
  Group {
    name: String,
  },
  Node {
    name: String,
    record_id: String,
  },
  Unresolved {
    name: String,
    reason: ProxyMemberUnresolvedReason,
  },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyNodeSnapshot {
  pub record_id: String,
  pub name: String,
  pub kind: String,
  pub alive: bool,
  pub delay_ms: Option<u32>,
  pub hidden: bool,
  pub icon: Option<String>,
  pub test_url: Option<String>,
  pub interface: Option<String>,
  pub dialer_proxy: Option<String>,
  pub capabilities: ProxyCapabilities,
  pub source: Option<ProxyNodeSource>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyGroupView {
  pub name: String,
  pub kind: String,
  pub alive: bool,
  pub selected: Option<String>,
  pub fixed: Option<String>,
  pub hidden: bool,
  pub icon: Option<String>,
  pub test_url: Option<String>,
  pub delay_ms: Option<u32>,
  pub capabilities: ProxyCapabilities,
  pub members: Vec<ProxyMemberSnapshot>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyProviderView {
  pub name: String,
  pub kind: String,
  pub vehicle_type: String,
  pub updated_at: Option<String>,
  pub test_url: Option<String>,
  pub proxy_record_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyViewV1 {
  pub schema_version: u8,
  pub order_source: ProxyViewOrderSource,
  pub provider_state: ProxyViewProviderState,
  pub global: Option<ProxyGroupView>,
  pub direct: Option<String>,
  pub groups: Vec<ProxyGroupView>,
  pub records: BTreeMap<String, ProxyNodeSnapshot>,
  pub standalone: Vec<String>,
  pub providers: Vec<ProxyProviderView>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyChainSnapshot {
  pub group: Option<String>,
  pub nodes: Vec<String>,
  pub connected: bool,
}

impl Default for ProxyViewV1 {
  fn default() -> Self {
    Self {
      schema_version: 1,
      order_source: ProxyViewOrderSource::Fallback,
      provider_state: ProxyViewProviderState::Unavailable,
      global: None,
      direct: None,
      groups: Vec::new(),
      records: BTreeMap::new(),
      standalone: Vec::new(),
      providers: Vec::new(),
    }
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MihomoSnapshot {
  pub connection: MihomoConnection,
  pub version: Option<String>,
  pub mixed_port: Option<u16>,
  pub tun_enabled: bool,
  pub mode: ProxyMode,
  pub traffic: TrafficSnapshot,
  pub memory_bytes: u64,
  pub connection_count: u64,
  pub groups: Vec<ProxyGroupSnapshot>,
  pub proxy_view: ProxyViewV1,
  pub proxy_busy: bool,
  pub proxy_chain: ProxyChainSnapshot,
  pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SystemProxyView {
  pub available: bool,
  pub enabled: bool,
  pub applied: bool,
  pub busy: bool,
  pub backend: Option<String>,
  pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileSourceKind {
  Local,
  Remote,
  Merge,
  Rules,
  Proxies,
  Groups,
  Other,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileDownloadProxy {
  #[default]
  Direct,
  System,
  Mihomo,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteProfileOptions {
  pub user_agent: Option<String>,
  pub update_interval_minutes: Option<u64>,
  pub timeout_seconds: u64,
  pub download_proxy: ProfileDownloadProxy,
  pub accept_invalid_certs: bool,
  pub allow_auto_update: bool,
}

impl Default for RemoteProfileOptions {
  fn default() -> Self {
    Self {
      user_agent: None,
      update_interval_minutes: None,
      timeout_seconds: 30,
      download_proxy: ProfileDownloadProxy::Direct,
      accept_invalid_certs: false,
      allow_auto_update: true,
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionUsage {
  pub upload: u64,
  pub download: u64,
  pub total: u64,
  pub expire: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileEnhancementRefs {
  pub merge: Option<String>,
  pub rules: Option<String>,
  pub proxies: Option<String>,
  pub groups: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileSummary {
  pub uid: String,
  pub name: String,
  pub source: ProfileSourceKind,
  pub location: Option<String>,
  pub updated_at: Option<u64>,
  pub home_page: Option<String>,
  pub usage: Option<SubscriptionUsage>,
  pub remote_options: Option<RemoteProfileOptions>,
  pub enhancements: ProfileEnhancementRefs,
  pub active: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfilesSnapshot {
  pub items: Vec<ProfileSummary>,
  pub busy: bool,
  pub diagnostics: ProfileDiagnostics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProfileOperationKind {
  Import,
  Activate,
  Update,
  Edit,
  Manage,
  AutomaticUpdate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProfileDiagnosticStage {
  Download,
  Enhancement,
  Validation,
  Deployment,
  Storage,
  Completed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileOperationDiagnostic {
  pub operation: ProfileOperationKind,
  pub stage: ProfileDiagnosticStage,
  pub success: bool,
  pub message: String,
  pub timestamp: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileDiagnostics {
  pub native_transforms: Vec<String>,
  pub pipeline_order: Vec<String>,
  pub last_operation: Option<ProfileOperationDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileQrCode {
  pub uid: String,
  pub name: String,
  pub width: usize,
  pub modules: Vec<bool>,
}

impl ProfilesSnapshot {
  pub fn current(&self) -> Option<&ProfileSummary> {
    self.items.iter().find(|profile| profile.active)
  }
}

impl MihomoSnapshot {
  pub fn current_proxy(&self) -> Option<&str> {
    self
      .groups
      .iter()
      .find(|group| group.name.eq_ignore_ascii_case("GLOBAL"))
      .or_else(|| self.groups.first())
      .and_then(|group| group.selected.as_deref())
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppSnapshot {
  pub revision: u64,
  pub status: AppStatus,
  pub page: Page,
  pub theme: ThemeMode,
  pub window_visible: bool,
  pub core: CoreState,
  pub mihomo: MihomoSnapshot,
  pub profiles: ProfilesSnapshot,
  pub system_proxy: SystemProxyView,
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
      mihomo: MihomoSnapshot::default(),
      profiles: ProfilesSnapshot::default(),
      system_proxy: SystemProxyView::default(),
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
  StartCore(CoreChannel),
  StopCore,
  RestartCore(CoreChannel),
  ReloadCore,
  RefreshMihomo,
  SelectProxy {
    group: String,
    proxy: String,
  },
  TestProxy {
    record_id: String,
  },
  TestProxyGroup {
    name: String,
  },
  TestAllProxies,
  UpdateProxyProvider {
    name: String,
  },
  UpdateAllProxyProviders,
  HealthcheckProxyProvider {
    name: String,
  },
  SetProxyChain {
    group: String,
    nodes: Vec<String>,
  },
  SetProxyMode(ProxyMode),
  RefreshProfiles,
  ImportLocalProfile {
    name: String,
    path: String,
  },
  ImportRemoteProfile {
    name: String,
    url: String,
    options: RemoteProfileOptions,
  },
  ImportProfileQr {
    name: String,
    path: String,
    options: RemoteProfileOptions,
  },
  RequestProfileQr {
    uid: String,
  },
  ActivateProfile {
    uid: String,
  },
  RenameProfile {
    uid: String,
    name: String,
  },
  DuplicateProfile {
    uid: String,
  },
  DeleteProfiles {
    uids: Vec<String>,
  },
  ReorderProfile {
    uid: String,
    new_index: usize,
  },
  SetRemoteProfileOptions {
    uid: String,
    options: RemoteProfileOptions,
  },
  LoadProfileContent {
    uid: String,
  },
  SaveProfileContent {
    uid: String,
    content: SensitiveString,
  },
  UpdateProfile {
    uid: String,
  },
  UpdateAllProfiles,
  RefreshSystemProxy,
  SetSystemProxy(bool),
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
  CoreStateChanged(CoreState),
  MihomoStateChanged,
  ProfilesChanged,
  ProfileContentLoaded {
    uid: String,
    content: SensitiveString,
  },
  ProfileContentSaved {
    uid: String,
  },
  ProfileQrReady(ProfileQrCode),
  SystemProxyChanged,
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

  pub const fn is_empty(&self) -> bool {
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
