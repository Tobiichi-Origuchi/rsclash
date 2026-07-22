use std::{collections::HashMap, pin::Pin};

use async_trait::async_trait;
use futures_util::Stream;
use serde_json::Value;

use crate::{
  Result,
  models::{
    BaseConfig, Connections, CoreUpdaterChannel, Groups, LogEntry, LogLevel, Memory, Proxies,
    Proxy, ProxyDelay, ProxyProvider, ProxyProviders, RuleProviders, Rules, Traffic, VersionInfo,
  },
};

pub type MihomoStream<T> = Pin<Box<dyn Stream<Item = Result<T>> + Send + 'static>>;

#[async_trait]
pub trait MihomoApi: Send + Sync {
  async fn version(&self) -> Result<VersionInfo>;
  async fn flush_fake_ip_cache(&self) -> Result<()>;
  async fn flush_dns_cache(&self) -> Result<()>;

  async fn connections(&self) -> Result<Connections>;
  async fn close_all_connections(&self) -> Result<()>;
  async fn close_connection(&self, connection_id: &str) -> Result<()>;

  async fn groups(&self) -> Result<Groups>;
  async fn group(&self, name: &str) -> Result<Proxy>;
  async fn delay_group(
    &self,
    name: &str,
    test_url: &str,
    timeout_ms: u32,
  ) -> Result<HashMap<String, u32>>;

  async fn proxy_providers(&self) -> Result<ProxyProviders>;
  async fn proxy_provider(&self, name: &str) -> Result<ProxyProvider>;
  async fn update_proxy_provider(&self, name: &str) -> Result<()>;
  async fn healthcheck_proxy_provider(&self, name: &str) -> Result<()>;
  async fn healthcheck_provider_proxy(
    &self,
    provider: &str,
    proxy: &str,
    test_url: &str,
    timeout_ms: u32,
  ) -> Result<ProxyDelay>;

  async fn proxies(&self) -> Result<Proxies>;
  async fn proxy(&self, name: &str) -> Result<Proxy>;
  async fn select_proxy(&self, group: &str, proxy: &str) -> Result<()>;
  async fn clear_fixed_proxy(&self, group: &str) -> Result<()>;
  async fn delay_proxy(&self, name: &str, test_url: &str, timeout_ms: u32) -> Result<ProxyDelay>;

  async fn rules(&self) -> Result<Rules>;
  async fn rule_providers(&self) -> Result<RuleProviders>;
  async fn update_rule_provider(&self, name: &str) -> Result<()>;

  async fn base_config(&self) -> Result<BaseConfig>;
  async fn reload_config(&self, path: &str, force: bool) -> Result<()>;
  async fn patch_base_config(&self, patch: Value) -> Result<()>;
  async fn update_geo(&self) -> Result<()>;
  async fn restart(&self) -> Result<()>;

  async fn upgrade_core(&self, channel: CoreUpdaterChannel, force: bool) -> Result<()>;
  async fn upgrade_ui(&self) -> Result<()>;
  async fn upgrade_geo(&self) -> Result<()>;

  async fn traffic_stream(&self) -> Result<MihomoStream<Traffic>>;
  async fn memory_stream(&self) -> Result<MihomoStream<Memory>>;
  async fn connections_stream(&self) -> Result<MihomoStream<Connections>>;
  async fn logs_stream(&self, level: LogLevel) -> Result<MihomoStream<LogEntry>>;
}
