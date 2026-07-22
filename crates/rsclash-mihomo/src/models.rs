use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type ExtraFields = HashMap<String, Value>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VersionInfo {
  pub meta: bool,
  pub version: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct BaseConfig {
  pub port: u16,
  pub socks_port: u16,
  pub redir_port: u16,
  pub tproxy_port: u16,
  pub mixed_port: u16,
  pub allow_lan: bool,
  pub bind_address: String,
  pub mode: String,
  pub log_level: String,
  pub ipv6: bool,
  pub unified_delay: bool,
  pub tcp_concurrent: bool,
  pub find_process_mode: String,
  pub interface_name: String,
  pub routing_mark: i64,
  pub tun: Value,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DelayHistory {
  pub time: String,
  pub delay: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxy {
  pub name: String,
  #[serde(rename = "type")]
  pub kind: String,
  pub alive: bool,
  pub udp: bool,
  pub uot: bool,
  pub xudp: bool,
  pub tfo: bool,
  pub mptcp: bool,
  pub smux: bool,
  pub interface: String,
  #[serde(rename = "dialer-proxy")]
  pub dialer_proxy: String,
  #[serde(rename = "routing-mark")]
  pub routing_mark: i64,
  #[serde(rename = "provider-name")]
  pub provider_name: String,
  pub all: Option<Vec<String>>,
  pub now: Option<String>,
  pub fixed: Option<String>,
  pub hidden: Option<bool>,
  pub icon: Option<String>,
  #[serde(rename = "testUrl")]
  pub test_url: Option<String>,
  pub history: Vec<DelayHistory>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxies {
  pub proxies: HashMap<String, Proxy>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Groups {
  pub proxies: Vec<Proxy>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProxyDelay {
  pub delay: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SubscriptionInfo {
  pub upload: i64,
  pub download: i64,
  pub total: i64,
  pub expire: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProxyProvider {
  pub name: String,
  #[serde(rename = "type")]
  pub kind: String,
  pub vehicle_type: String,
  pub proxies: Vec<Proxy>,
  pub test_url: String,
  pub expected_status: String,
  pub updated_at: Option<String>,
  pub subscription_info: Option<SubscriptionInfo>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyProviders {
  pub providers: HashMap<String, ProxyProvider>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
  #[serde(rename = "type")]
  pub kind: String,
  pub index: i32,
  pub payload: String,
  pub proxy: String,
  pub size: i32,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
  pub rules: Vec<Rule>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RuleProvider {
  pub behavior: String,
  pub format: String,
  pub name: String,
  pub rule_count: u32,
  #[serde(rename = "type")]
  pub kind: String,
  pub updated_at: String,
  pub vehicle_type: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleProviders {
  pub providers: HashMap<String, RuleProvider>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConnectionMetadata {
  pub network: String,
  #[serde(rename = "type")]
  pub kind: String,
  pub source_ip: String,
  pub destination_ip: String,
  pub source_port: String,
  pub destination_port: String,
  pub inbound_ip: String,
  pub inbound_port: String,
  pub inbound_name: String,
  pub inbound_user: String,
  pub host: String,
  pub dns_mode: String,
  pub uid: u32,
  pub process: String,
  pub process_path: String,
  pub special_proxy: String,
  pub special_rules: String,
  pub remote_destination: String,
  pub dscp: u8,
  pub sniff_host: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Connection {
  pub id: String,
  pub metadata: ConnectionMetadata,
  pub upload: u64,
  pub download: u64,
  pub start: String,
  pub chains: Vec<String>,
  pub provider_chains: Option<Vec<String>>,
  pub rule: String,
  pub rule_payload: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Connections {
  pub download_total: u64,
  pub upload_total: u64,
  pub connections: Option<Vec<Connection>>,
  pub memory: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Traffic {
  pub up: u64,
  pub down: u64,
  pub up_total: u64,
  pub down_total: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
  pub inuse: u64,
  pub oslimit: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogEntry {
  #[serde(rename = "type")]
  pub level: String,
  pub payload: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
  Debug,
  #[default]
  Info,
  Warning,
  Error,
  Silent,
}

impl LogLevel {
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Debug => "debug",
      Self::Info => "info",
      Self::Warning => "warning",
      Self::Error => "error",
      Self::Silent => "silent",
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CoreUpdaterChannel {
  #[default]
  Stable,
  Alpha,
}

impl CoreUpdaterChannel {
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Stable => "stable",
      Self::Alpha => "alpha",
    }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use serde_json::json;

  use super::{Connections, Proxy, VersionInfo};

  #[test]
  fn version_preserves_unknown_fields() {
    let version: VersionInfo = serde_json::from_value(json!({
        "meta": true,
        "version": "1.20.0",
        "future": 42
    }))
    .expect("test payload should deserialize");

    assert_eq!(version.extra.get("future"), Some(&json!(42)));
  }

  #[test]
  fn connections_accept_missing_connection_list() {
    let connections: Connections = serde_json::from_value(json!({
        "downloadTotal": 1,
        "uploadTotal": 2,
        "memory": 3
    }))
    .expect("test payload should deserialize");

    assert!(connections.connections.is_none());
  }

  #[test]
  fn proxies_accept_unknown_types_and_fields() {
    let proxy: Proxy = serde_json::from_value(json!({
        "name": "future proxy",
        "type": "FutureTransport",
        "newCapability": true
    }))
    .expect("test payload should deserialize");

    assert_eq!(proxy.kind, "FutureTransport");
    assert_eq!(proxy.extra.get("newCapability"), Some(&json!(true)));
  }
}
