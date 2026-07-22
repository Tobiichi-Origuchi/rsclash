use std::{
  collections::HashMap,
  fmt,
  sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use futures_util::stream;
use serde_json::Value;

use crate::{
  Error, MihomoApi, MihomoStream, Result,
  models::{
    BaseConfig, Connections, CoreUpdaterChannel, Groups, LogEntry, LogLevel, Memory, Proxies,
    Proxy, ProxyDelay, ProxyProvider, ProxyProviders, RuleProviders, Rules, Traffic, VersionInfo,
  },
};

#[derive(Clone, Debug, Default)]
pub struct FakeMihomoState {
  pub version: VersionInfo,
  pub connections: Connections,
  pub groups: Groups,
  pub proxy_providers: ProxyProviders,
  pub proxies: Proxies,
  pub rules: Rules,
  pub rule_providers: RuleProviders,
  pub base_config: BaseConfig,
  pub group_delays: HashMap<String, HashMap<String, u32>>,
  pub provider_proxy_delays: HashMap<(String, String), ProxyDelay>,
  pub proxy_delays: HashMap<String, ProxyDelay>,
  pub traffic_items: Vec<Traffic>,
  pub memory_items: Vec<Memory>,
  pub connection_items: Vec<Connections>,
  pub log_items: Vec<LogEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MihomoCall {
  Version,
  FlushFakeIpCache,
  FlushDnsCache,
  Connections,
  CloseAllConnections,
  CloseConnection(String),
  Groups,
  Group(String),
  DelayGroup {
    name: String,
    test_url: String,
    timeout_ms: u32,
  },
  ProxyProviders,
  ProxyProvider(String),
  UpdateProxyProvider(String),
  HealthcheckProxyProvider(String),
  HealthcheckProviderProxy {
    provider: String,
    proxy: String,
    test_url: String,
    timeout_ms: u32,
  },
  Proxies,
  Proxy(String),
  SelectProxy {
    group: String,
    proxy: String,
  },
  ClearFixedProxy(String),
  DelayProxy {
    name: String,
    test_url: String,
    timeout_ms: u32,
  },
  Rules,
  RuleProviders,
  UpdateRuleProvider(String),
  BaseConfig,
  ReloadConfig {
    path: String,
    force: bool,
  },
  PatchBaseConfig(Value),
  UpdateGeo,
  Restart,
  UpgradeCore {
    channel: CoreUpdaterChannel,
    force: bool,
  },
  UpgradeUi,
  UpgradeGeo,
  TrafficStream,
  MemoryStream,
  ConnectionsStream,
  LogsStream(LogLevel),
}

#[derive(Default)]
struct FakeInner {
  state: FakeMihomoState,
  calls: Vec<MihomoCall>,
  next_failure: Option<Error>,
}

#[derive(Clone, Default)]
pub struct FakeMihomoApi {
  inner: Arc<Mutex<FakeInner>>,
}

impl FakeMihomoApi {
  pub fn new(state: FakeMihomoState) -> Self {
    Self {
      inner: Arc::new(Mutex::new(FakeInner {
        state,
        ..FakeInner::default()
      })),
    }
  }

  pub fn update(&self, update: impl FnOnce(&mut FakeMihomoState)) -> Result<()> {
    let mut inner = self.lock()?;
    update(&mut inner.state);
    drop(inner);
    Ok(())
  }

  pub fn calls(&self) -> Result<Vec<MihomoCall>> {
    Ok(self.lock()?.calls.clone())
  }

  pub fn clear_calls(&self) -> Result<()> {
    self.lock()?.calls.clear();
    Ok(())
  }

  pub fn fail_next(&self, error: Error) -> Result<()> {
    self.lock()?.next_failure = Some(error);
    Ok(())
  }

  fn record(&self, call: MihomoCall) -> Result<MutexGuard<'_, FakeInner>> {
    let mut inner = self.lock()?;
    inner.calls.push(call);
    if let Some(error) = inner.next_failure.take() {
      return Err(error);
    }
    Ok(inner)
  }

  fn lock(&self) -> Result<MutexGuard<'_, FakeInner>> {
    self
      .inner
      .lock()
      .map_err(|_| Error::Fake("state lock was poisoned".to_string()))
  }
}

impl fmt::Debug for FakeMihomoApi {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let call_count = self.inner.lock().map(|inner| inner.calls.len()).ok();
    formatter
      .debug_struct("FakeMihomoApi")
      .field("call_count", &call_count)
      .finish_non_exhaustive()
  }
}

#[async_trait]
impl MihomoApi for FakeMihomoApi {
  async fn version(&self) -> Result<VersionInfo> {
    Ok(self.record(MihomoCall::Version)?.state.version.clone())
  }

  async fn flush_fake_ip_cache(&self) -> Result<()> {
    self.record(MihomoCall::FlushFakeIpCache).map(|_| ())
  }

  async fn flush_dns_cache(&self) -> Result<()> {
    self.record(MihomoCall::FlushDnsCache).map(|_| ())
  }

  async fn connections(&self) -> Result<Connections> {
    Ok(
      self
        .record(MihomoCall::Connections)?
        .state
        .connections
        .clone(),
    )
  }

  async fn close_all_connections(&self) -> Result<()> {
    let mut inner = self.record(MihomoCall::CloseAllConnections)?;
    inner.state.connections.connections = Some(Vec::new());
    drop(inner);
    Ok(())
  }

  async fn close_connection(&self, connection_id: &str) -> Result<()> {
    let mut inner = self.record(MihomoCall::CloseConnection(connection_id.to_string()))?;
    if let Some(connections) = &mut inner.state.connections.connections {
      connections.retain(|connection| connection.id != connection_id);
    }
    drop(inner);
    Ok(())
  }

  async fn groups(&self) -> Result<Groups> {
    Ok(self.record(MihomoCall::Groups)?.state.groups.clone())
  }

  async fn group(&self, name: &str) -> Result<Proxy> {
    self
      .record(MihomoCall::Group(name.to_string()))?
      .state
      .groups
      .proxies
      .iter()
      .find(|proxy| proxy.name == name)
      .cloned()
      .ok_or_else(|| missing_response("group", name))
  }

  async fn delay_group(
    &self,
    name: &str,
    test_url: &str,
    timeout_ms: u32,
  ) -> Result<HashMap<String, u32>> {
    Ok(
      self
        .record(MihomoCall::DelayGroup {
          name: name.to_string(),
          test_url: test_url.to_string(),
          timeout_ms,
        })?
        .state
        .group_delays
        .get(name)
        .cloned()
        .unwrap_or_default(),
    )
  }

  async fn proxy_providers(&self) -> Result<ProxyProviders> {
    Ok(
      self
        .record(MihomoCall::ProxyProviders)?
        .state
        .proxy_providers
        .clone(),
    )
  }

  async fn proxy_provider(&self, name: &str) -> Result<ProxyProvider> {
    self
      .record(MihomoCall::ProxyProvider(name.to_string()))?
      .state
      .proxy_providers
      .providers
      .get(name)
      .cloned()
      .ok_or_else(|| missing_response("proxy provider", name))
  }

  async fn update_proxy_provider(&self, name: &str) -> Result<()> {
    self
      .record(MihomoCall::UpdateProxyProvider(name.to_string()))
      .map(|_| ())
  }

  async fn healthcheck_proxy_provider(&self, name: &str) -> Result<()> {
    self
      .record(MihomoCall::HealthcheckProxyProvider(name.to_string()))
      .map(|_| ())
  }

  async fn healthcheck_provider_proxy(
    &self,
    provider: &str,
    proxy: &str,
    test_url: &str,
    timeout_ms: u32,
  ) -> Result<ProxyDelay> {
    Ok(
      self
        .record(MihomoCall::HealthcheckProviderProxy {
          provider: provider.to_string(),
          proxy: proxy.to_string(),
          test_url: test_url.to_string(),
          timeout_ms,
        })?
        .state
        .provider_proxy_delays
        .get(&(provider.to_string(), proxy.to_string()))
        .copied()
        .unwrap_or_default(),
    )
  }

  async fn proxies(&self) -> Result<Proxies> {
    Ok(self.record(MihomoCall::Proxies)?.state.proxies.clone())
  }

  async fn proxy(&self, name: &str) -> Result<Proxy> {
    self
      .record(MihomoCall::Proxy(name.to_string()))?
      .state
      .proxies
      .proxies
      .get(name)
      .cloned()
      .ok_or_else(|| missing_response("proxy", name))
  }

  async fn select_proxy(&self, group: &str, proxy: &str) -> Result<()> {
    let mut inner = self.record(MihomoCall::SelectProxy {
      group: group.to_string(),
      proxy: proxy.to_string(),
    })?;
    let mut changed = false;
    for candidate in &mut inner.state.groups.proxies {
      if candidate.name == group {
        candidate.now = Some(proxy.to_string());
        changed = true;
      }
    }
    if let Some(candidate) = inner.state.proxies.proxies.get_mut(group) {
      candidate.now = Some(proxy.to_string());
      changed = true;
    }
    drop(inner);
    if changed {
      Ok(())
    } else {
      Err(missing_response("proxy group", group))
    }
  }

  async fn clear_fixed_proxy(&self, group: &str) -> Result<()> {
    let mut inner = self.record(MihomoCall::ClearFixedProxy(group.to_string()))?;
    for candidate in &mut inner.state.groups.proxies {
      if candidate.name == group {
        candidate.fixed = None;
      }
    }
    if let Some(candidate) = inner.state.proxies.proxies.get_mut(group) {
      candidate.fixed = None;
    }
    drop(inner);
    Ok(())
  }

  async fn delay_proxy(&self, name: &str, test_url: &str, timeout_ms: u32) -> Result<ProxyDelay> {
    Ok(
      self
        .record(MihomoCall::DelayProxy {
          name: name.to_string(),
          test_url: test_url.to_string(),
          timeout_ms,
        })?
        .state
        .proxy_delays
        .get(name)
        .copied()
        .unwrap_or_default(),
    )
  }

  async fn rules(&self) -> Result<Rules> {
    Ok(self.record(MihomoCall::Rules)?.state.rules.clone())
  }

  async fn rule_providers(&self) -> Result<RuleProviders> {
    Ok(
      self
        .record(MihomoCall::RuleProviders)?
        .state
        .rule_providers
        .clone(),
    )
  }

  async fn update_rule_provider(&self, name: &str) -> Result<()> {
    self
      .record(MihomoCall::UpdateRuleProvider(name.to_string()))
      .map(|_| ())
  }

  async fn base_config(&self) -> Result<BaseConfig> {
    Ok(
      self
        .record(MihomoCall::BaseConfig)?
        .state
        .base_config
        .clone(),
    )
  }

  async fn reload_config(&self, path: &str, force: bool) -> Result<()> {
    self
      .record(MihomoCall::ReloadConfig {
        path: path.to_string(),
        force,
      })
      .map(|_| ())
  }

  async fn patch_base_config(&self, patch: Value) -> Result<()> {
    self.record(MihomoCall::PatchBaseConfig(patch)).map(|_| ())
  }

  async fn update_geo(&self) -> Result<()> {
    self.record(MihomoCall::UpdateGeo).map(|_| ())
  }

  async fn restart(&self) -> Result<()> {
    self.record(MihomoCall::Restart).map(|_| ())
  }

  async fn upgrade_core(&self, channel: CoreUpdaterChannel, force: bool) -> Result<()> {
    self
      .record(MihomoCall::UpgradeCore { channel, force })
      .map(|_| ())
  }

  async fn upgrade_ui(&self) -> Result<()> {
    self.record(MihomoCall::UpgradeUi).map(|_| ())
  }

  async fn upgrade_geo(&self) -> Result<()> {
    self.record(MihomoCall::UpgradeGeo).map(|_| ())
  }

  async fn traffic_stream(&self) -> Result<MihomoStream<Traffic>> {
    let items = self
      .record(MihomoCall::TrafficStream)?
      .state
      .traffic_items
      .clone();
    Ok(Box::pin(stream::iter(items.into_iter().map(Ok))))
  }

  async fn memory_stream(&self) -> Result<MihomoStream<Memory>> {
    let items = self
      .record(MihomoCall::MemoryStream)?
      .state
      .memory_items
      .clone();
    Ok(Box::pin(stream::iter(items.into_iter().map(Ok))))
  }

  async fn connections_stream(&self) -> Result<MihomoStream<Connections>> {
    let items = self
      .record(MihomoCall::ConnectionsStream)?
      .state
      .connection_items
      .clone();
    Ok(Box::pin(stream::iter(items.into_iter().map(Ok))))
  }

  async fn logs_stream(&self, level: LogLevel) -> Result<MihomoStream<LogEntry>> {
    let items = self
      .record(MihomoCall::LogsStream(level))?
      .state
      .log_items
      .clone();
    Ok(Box::pin(stream::iter(items.into_iter().map(Ok))))
  }
}

fn missing_response(kind: &str, name: &str) -> Error {
  Error::Fake(format!("no {kind} response is configured for {name}"))
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::sync::Arc;

  use futures_util::StreamExt as _;

  use super::{FakeMihomoApi, MihomoCall};
  use crate::{
    Error, MihomoApi,
    models::{Groups, Proxy, Traffic, VersionInfo},
  };

  #[tokio::test]
  async fn fake_can_be_used_through_the_api_trait() {
    let fake = FakeMihomoApi::default();
    fake
      .update(|state| {
        state.version = VersionInfo {
          meta: true,
          version: "fake-version".to_string(),
          ..VersionInfo::default()
        };
      })
      .expect("fake should update");
    let api: Arc<dyn MihomoApi> = Arc::new(fake.clone());

    let version = api.version().await.expect("version should be returned");
    assert_eq!(version.version, "fake-version");
    assert_eq!(
      fake.calls().expect("calls should be available"),
      vec![MihomoCall::Version]
    );
  }

  #[tokio::test]
  async fn fake_records_mutations_and_updates_selected_proxy() {
    let fake = FakeMihomoApi::default();
    fake
      .update(|state| {
        state.groups = Groups {
          proxies: vec![Proxy {
            name: "GLOBAL".to_string(),
            ..Proxy::default()
          }],
          ..Groups::default()
        };
      })
      .expect("fake should update");

    fake
      .select_proxy("GLOBAL", "Node A")
      .await
      .expect("selection should succeed");
    let group = fake.group("GLOBAL").await.expect("group should exist");
    assert_eq!(group.now.as_deref(), Some("Node A"));
    assert_eq!(
      fake.calls().expect("calls should be available"),
      vec![
        MihomoCall::SelectProxy {
          group: "GLOBAL".to_string(),
          proxy: "Node A".to_string(),
        },
        MihomoCall::Group("GLOBAL".to_string()),
      ]
    );
  }

  #[tokio::test]
  async fn fake_streams_are_deterministic_and_finite() {
    let fake = FakeMihomoApi::default();
    fake
      .update(|state| {
        state.traffic_items = vec![Traffic {
          up: 7,
          down: 9,
          ..Traffic::default()
        }];
      })
      .expect("fake should update");
    let mut stream = fake
      .traffic_stream()
      .await
      .expect("stream should be created");

    let item = stream
      .next()
      .await
      .expect("one item should exist")
      .expect("item should succeed");
    assert_eq!((item.up, item.down), (7, 9));
    assert!(stream.next().await.is_none());
  }

  #[tokio::test]
  async fn injected_failure_is_consumed_once() {
    let fake = FakeMihomoApi::default();
    fake
      .fail_next(Error::Fake("injected".to_string()))
      .expect("failure should be configured");

    assert!(fake.version().await.is_err());
    assert!(fake.version().await.is_ok());
  }
}
