use std::{sync::Arc, time::Duration};

use rsclash_domain::{
  CoreRunMode, CoreState, MihomoConnection, MihomoSnapshot, ProxyGroupSnapshot, ProxyMode,
  ProxyNodeSource, ProxyOptionSnapshot, TrafficSnapshot,
};
use rsclash_mihomo::{MihomoApi, models::Connections};
use serde_json::json;
use tokio::{
  sync::mpsc,
  time::{Instant, MissedTickBehavior, interval},
};

use crate::profiles::{ProfileRuntimeSync, StoredProxySelection};
use crate::proxy_view::{ProxyViewBuilder, ProxyViewInput};

const STATS_INTERVAL: Duration = Duration::from_secs(1);
const METADATA_POLL_TICKS: u8 = 5;
const DEFAULT_DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
const DEFAULT_DELAY_TIMEOUT_MS: u32 = 5_000;

#[derive(Clone)]
pub struct MihomoAccess {
  sidecar: Arc<dyn MihomoApi>,
  service: Arc<dyn MihomoApi>,
}

impl MihomoAccess {
  pub fn new(sidecar: Arc<dyn MihomoApi>, service: Arc<dyn MihomoApi>) -> Self {
    Self { sidecar, service }
  }

  pub fn same(api: Arc<dyn MihomoApi>) -> Self {
    Self {
      sidecar: Arc::clone(&api),
      service: api,
    }
  }

  fn client(&self, mode: CoreRunMode) -> Arc<dyn MihomoApi> {
    match mode {
      CoreRunMode::Sidecar => Arc::clone(&self.sidecar),
      CoreRunMode::Service => Arc::clone(&self.service),
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) enum MihomoBridgeCommand {
  CoreState(CoreState),
  Refresh,
  SelectProxy { group: String, proxy: String },
  TestProxy { record_id: String },
  TestProxyGroup { name: String },
  TestAllProxies,
  UpdateProxyProvider { name: String },
  UpdateAllProxyProviders,
  HealthcheckProxyProvider { name: String },
  SynchronizeProfile(ProfileRuntimeSync),
  CloseConnectionsForProxy { proxy: String },
  SetMode(ProxyMode),
}

pub(crate) enum MihomoBridgeEvent {
  Snapshot(Box<MihomoSnapshot>),
  ProxySelected {
    group: String,
    proxy: String,
    previous: Option<String>,
  },
  CommandFailed(String),
}

struct TrafficSample {
  sampled_at: Instant,
  upload_total: u64,
  download_total: u64,
}

struct MihomoWorker {
  access: MihomoAccess,
  active: Option<(CoreRunMode, Arc<dyn MihomoApi>)>,
  state: MihomoSnapshot,
  traffic_sample: Option<TrafficSample>,
  metadata_ticks: u8,
  pending_profile_sync: Option<ProfileRuntimeSync>,
  event_tx: mpsc::Sender<MihomoBridgeEvent>,
}

impl MihomoWorker {
  fn new(access: MihomoAccess, event_tx: mpsc::Sender<MihomoBridgeEvent>) -> Self {
    Self {
      access,
      active: None,
      state: MihomoSnapshot::default(),
      traffic_sample: None,
      metadata_ticks: 0,
      pending_profile_sync: None,
      event_tx,
    }
  }

  async fn run(mut self, mut command_rx: mpsc::Receiver<MihomoBridgeCommand>) {
    let mut stats_interval = interval(STATS_INTERVAL);
    stats_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
      tokio::select! {
        command = command_rx.recv() => {
          let Some(command) = command else {
            return;
          };
          self.handle_command(command).await;
        },
        _ = stats_interval.tick(), if self.active.is_some() => {
          self.poll().await;
        },
      }
    }
  }

  async fn handle_command(&mut self, command: MihomoBridgeCommand) {
    match command {
      MihomoBridgeCommand::CoreState(CoreState::Running { mode, .. }) => {
        if self.active.as_ref().map(|(active, _)| *active) != Some(mode) {
          self.active = Some((mode, self.access.client(mode)));
          self.state = MihomoSnapshot {
            connection: MihomoConnection::Connecting,
            ..MihomoSnapshot::default()
          };
          self.traffic_sample = None;
          self.metadata_ticks = 0;
          self.publish().await;
          self.refresh_all().await;
          if let Some(sync) = self.pending_profile_sync.take() {
            self.synchronize_profile(sync).await;
          }
        }
      },
      MihomoBridgeCommand::CoreState(_) => {
        if self.active.take().is_some() || self.state != MihomoSnapshot::default() {
          self.state = MihomoSnapshot::default();
          self.traffic_sample = None;
          self.metadata_ticks = 0;
          self.publish().await;
        }
      },
      MihomoBridgeCommand::Refresh => self.refresh_all().await,
      MihomoBridgeCommand::SelectProxy { group, proxy } => {
        self.select_proxy(group, proxy).await;
      },
      MihomoBridgeCommand::TestProxy { record_id } => {
        self.test_proxy(record_id).await;
      },
      MihomoBridgeCommand::TestProxyGroup { name } => {
        self.test_proxy_group(name).await;
      },
      MihomoBridgeCommand::TestAllProxies => self.test_all_proxies().await,
      MihomoBridgeCommand::UpdateProxyProvider { name } => {
        self.update_proxy_provider(name).await;
      },
      MihomoBridgeCommand::UpdateAllProxyProviders => {
        self.update_all_proxy_providers().await;
      },
      MihomoBridgeCommand::HealthcheckProxyProvider { name } => {
        self.healthcheck_proxy_provider(name).await;
      },
      MihomoBridgeCommand::SynchronizeProfile(sync) => {
        if self.active.is_some() {
          self.synchronize_profile(sync).await;
        } else {
          self.pending_profile_sync = Some(sync);
        }
      },
      MihomoBridgeCommand::CloseConnectionsForProxy { proxy } => {
        self.close_connections_for_proxy(&proxy).await;
      },
      MihomoBridgeCommand::SetMode(mode) => self.set_mode(mode).await,
    }
  }

  async fn poll(&mut self) {
    self.refresh_stats().await;
    self.metadata_ticks = self.metadata_ticks.saturating_add(1);
    if self.metadata_ticks >= METADATA_POLL_TICKS {
      self.metadata_ticks = 0;
      self.refresh_metadata().await;
    }
  }

  async fn refresh_all(&mut self) {
    if self.active.is_none() {
      return;
    }
    self.refresh_metadata().await;
    self.refresh_stats().await;
  }

  async fn refresh_metadata(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    let (version, config, groups, proxies, providers) = tokio::join!(
      client.version(),
      client.base_config(),
      client.groups(),
      client.proxies(),
      client.proxy_providers(),
    );
    let result = match (version, config, groups, proxies, providers) {
      (Ok(version), Ok(config), Ok(groups), Ok(proxies), providers) => {
        self.state.version = Some(version.version);
        self.state.mixed_port = (config.mixed_port > 0).then_some(config.mixed_port);
        self.state.tun_enabled = config
          .tun
          .get("enable")
          .and_then(serde_json::Value::as_bool)
          .unwrap_or(false);
        self.state.mode = ProxyMode::from(config.mode.as_str());
        self.state.groups = groups
          .proxies
          .iter()
          .map(|group| {
            let options = group
              .all
              .clone()
              .unwrap_or_default()
              .into_iter()
              .map(|name| {
                let proxy = proxies.proxies.get(&name);
                ProxyOptionSnapshot {
                  name,
                  alive: proxy.is_none_or(|proxy| proxy.alive),
                  delay_ms: proxy
                    .and_then(|proxy| proxy.history.last())
                    .map(|history| history.delay)
                    .filter(|delay| *delay > 0),
                }
              })
              .collect();
            ProxyGroupSnapshot {
              name: group.name.clone(),
              kind: group.kind.clone(),
              selected: group.now.clone().or_else(|| group.fixed.clone()),
              options,
            }
          })
          .collect();
        self.state.proxy_view = ProxyViewBuilder::build(ProxyViewInput {
          runtime_group_order: groups
            .proxies
            .iter()
            .map(|group| group.name.clone())
            .collect(),
          proxies,
          providers: providers.ok(),
        });
        Ok(())
      },
      (Err(error), _, _, _, _)
      | (_, Err(error), _, _, _)
      | (_, _, Err(error), _, _)
      | (_, _, _, Err(error), _) => Err(error),
    };
    self.finish_refresh(result).await;
  }

  async fn refresh_stats(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    match client.connections().await {
      Ok(connections) => {
        self.update_stats(&connections);
        self.state.connection = MihomoConnection::Connected;
        self.state.last_error = None;
        self.publish().await;
      },
      Err(error) => self.mark_degraded(error.to_string()).await,
    }
  }

  fn update_stats(&mut self, connections: &Connections) {
    let sampled_at = Instant::now();
    let (upload_rate, download_rate) = self.traffic_sample.as_ref().map_or((0, 0), |previous| {
      let elapsed_ms = sampled_at
        .saturating_duration_since(previous.sampled_at)
        .as_millis()
        .max(1);
      let upload = u128::from(
        connections
          .upload_total
          .saturating_sub(previous.upload_total),
      )
      .saturating_mul(1_000)
        / elapsed_ms;
      let download = u128::from(
        connections
          .download_total
          .saturating_sub(previous.download_total),
      )
      .saturating_mul(1_000)
        / elapsed_ms;
      (
        u64::try_from(upload).unwrap_or(u64::MAX),
        u64::try_from(download).unwrap_or(u64::MAX),
      )
    });
    self.traffic_sample = Some(TrafficSample {
      sampled_at,
      upload_total: connections.upload_total,
      download_total: connections.download_total,
    });
    self.state.traffic = TrafficSnapshot {
      upload_bytes_per_second: upload_rate,
      download_bytes_per_second: download_rate,
      upload_total: connections.upload_total,
      download_total: connections.download_total,
    };
    self.state.memory_bytes = connections.memory;
    self.state.connection_count = connections
      .connections
      .as_ref()
      .map_or(0, |connections| connections.len() as u64);
  }

  async fn select_proxy(&mut self, group: String, proxy: String) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    let previous = self
      .state
      .groups
      .iter()
      .find(|item| item.name == group)
      .and_then(|item| item.selected.clone())
      .filter(|previous| previous != &proxy);
    match client.select_proxy(&group, &proxy).await {
      Ok(()) => {
        if let Some(candidate) = self.state.groups.iter_mut().find(|item| item.name == group) {
          candidate.selected = Some(proxy.clone());
        }
        self.state.connection = MihomoConnection::Connected;
        self.state.last_error = None;
        self.publish().await;
        let _ = self
          .event_tx
          .send(MihomoBridgeEvent::ProxySelected {
            group,
            proxy,
            previous,
          })
          .await;
      },
      Err(error) => self.command_failed(error.to_string()).await,
    }
  }

  async fn test_proxy(&mut self, record_id: String) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    let Some(record) = self.state.proxy_view.records.get(&record_id).cloned() else {
      self
        .command_failed(format!("proxy record {record_id} no longer exists"))
        .await;
      return;
    };
    self.set_proxy_busy(true).await;
    let test_url = record.test_url.as_deref().unwrap_or(DEFAULT_DELAY_TEST_URL);
    let result = match record.source.as_ref() {
      Some(ProxyNodeSource::Core { proxy_name }) => {
        client
          .delay_proxy(proxy_name, test_url, DEFAULT_DELAY_TIMEOUT_MS)
          .await
      },
      Some(ProxyNodeSource::Provider {
        provider_name,
        proxy_name,
      }) => {
        client
          .healthcheck_provider_proxy(
            provider_name,
            proxy_name,
            test_url,
            DEFAULT_DELAY_TIMEOUT_MS,
          )
          .await
      },
      None => {
        self.set_proxy_busy(false).await;
        self
          .command_failed(format!("proxy record {record_id} has no source"))
          .await;
        return;
      },
    };
    match result {
      Ok(delay) => {
        if let Some(candidate) = self.state.proxy_view.records.get_mut(&record_id) {
          candidate.delay_ms = (delay.delay > 0).then_some(delay.delay);
          candidate.alive = delay.delay > 0;
        }
        self.state.last_error = None;
        self.set_proxy_busy(false).await;
      },
      Err(error) => {
        self.set_proxy_busy(false).await;
        self.command_failed(error.to_string()).await;
      },
    }
  }

  async fn test_proxy_group(&mut self, name: String) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    let test_url = self
      .state
      .proxy_view
      .groups
      .iter()
      .chain(self.state.proxy_view.global.iter())
      .find(|group| group.name == name)
      .and_then(|group| group.test_url.clone())
      .unwrap_or_else(|| DEFAULT_DELAY_TEST_URL.to_string());
    self.set_proxy_busy(true).await;
    match client
      .delay_group(&name, &test_url, DEFAULT_DELAY_TIMEOUT_MS)
      .await
    {
      Ok(delays) => {
        self.apply_proxy_delays(&delays);
        self.state.last_error = None;
        self.set_proxy_busy(false).await;
      },
      Err(error) => {
        self.set_proxy_busy(false).await;
        self.command_failed(error.to_string()).await;
      },
    }
  }

  async fn test_all_proxies(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    self.set_proxy_busy(true).await;
    let providers = self
      .state
      .proxy_view
      .providers
      .iter()
      .map(|provider| provider.name.clone())
      .collect::<Vec<_>>();
    let groups = self
      .state
      .proxy_view
      .groups
      .iter()
      .chain(self.state.proxy_view.global.iter())
      .map(|group| {
        (
          group.name.clone(),
          group
            .test_url
            .clone()
            .unwrap_or_else(|| DEFAULT_DELAY_TEST_URL.to_string()),
        )
      })
      .collect::<Vec<_>>();
    let mut failures = Vec::new();
    for provider in providers {
      if let Err(error) = client.healthcheck_proxy_provider(&provider).await {
        failures.push(format!("healthcheck provider {provider}: {error}"));
      }
    }
    for (group, url) in groups {
      match client
        .delay_group(&group, &url, DEFAULT_DELAY_TIMEOUT_MS)
        .await
      {
        Ok(delays) => self.apply_proxy_delays(&delays),
        Err(error) => failures.push(format!("test group {group}: {error}")),
      }
    }
    self.refresh_metadata().await;
    self.set_proxy_busy(false).await;
    if !failures.is_empty() {
      self.command_failed(failures.join("; ")).await;
    }
  }

  async fn update_proxy_provider(&mut self, name: String) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    self.set_proxy_busy(true).await;
    let result = client.update_proxy_provider(&name).await;
    if result.is_ok() {
      self.refresh_metadata().await;
    }
    self.set_proxy_busy(false).await;
    if let Err(error) = result {
      self.command_failed(error.to_string()).await;
    }
  }

  async fn healthcheck_proxy_provider(&mut self, name: String) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    self.set_proxy_busy(true).await;
    let result = client.healthcheck_proxy_provider(&name).await;
    if result.is_ok() {
      self.refresh_metadata().await;
    }
    self.set_proxy_busy(false).await;
    if let Err(error) = result {
      self.command_failed(error.to_string()).await;
    }
  }

  async fn update_all_proxy_providers(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    self.set_proxy_busy(true).await;
    let providers = self
      .state
      .proxy_view
      .providers
      .iter()
      .map(|provider| provider.name.clone())
      .collect::<Vec<_>>();
    let mut failures = Vec::new();
    for provider in providers {
      if let Err(error) = client.update_proxy_provider(&provider).await {
        failures.push(format!("update provider {provider}: {error}"));
      }
    }
    self.refresh_metadata().await;
    self.set_proxy_busy(false).await;
    if !failures.is_empty() {
      self.command_failed(failures.join("; ")).await;
    }
  }

  fn apply_proxy_delays(&mut self, delays: &std::collections::HashMap<String, u32>) {
    for record in self.state.proxy_view.records.values_mut() {
      if let Some(delay) = delays.get(&record.name) {
        record.delay_ms = (*delay > 0).then_some(*delay);
        record.alive = *delay > 0;
      }
    }
    for group in &mut self.state.groups {
      for option in &mut group.options {
        if let Some(delay) = delays.get(&option.name) {
          option.delay_ms = (*delay > 0).then_some(*delay);
          option.alive = *delay > 0;
        }
      }
    }
  }

  async fn set_proxy_busy(&mut self, busy: bool) {
    if self.state.proxy_busy != busy {
      self.state.proxy_busy = busy;
      self.publish().await;
    }
  }

  async fn synchronize_profile(&mut self, sync: ProfileRuntimeSync) {
    let Some((_, client)) = self.active.clone() else {
      self.pending_profile_sync = Some(sync);
      return;
    };
    let mut failures = Vec::new();
    match client.groups().await {
      Ok(groups) => {
        for selection in sync.selections {
          let Some(group) = groups
            .proxies
            .iter()
            .find(|group| group.name == selection.group)
          else {
            continue;
          };
          if group.now.as_deref() == Some(selection.proxy.as_str())
            || !selection_is_available(group.all.as_deref(), &selection)
          {
            continue;
          }
          if let Err(error) = client
            .select_proxy(&selection.group, &selection.proxy)
            .await
          {
            failures.push(format!(
              "restore {} -> {}: {error}",
              selection.group, selection.proxy
            ));
          }
        }
      },
      Err(error) => failures.push(format!("load proxy groups for profile restore: {error}")),
    }
    if sync.close_connections
      && let Err(error) = client.close_all_connections().await
    {
      failures.push(format!("close connections after profile change: {error}"));
    }
    self.refresh_all().await;
    if !failures.is_empty() {
      self.command_failed(failures.join("; ")).await;
    }
  }

  async fn close_connections_for_proxy(&mut self, proxy: &str) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    let connections = match client.connections().await {
      Ok(connections) => connections.connections.unwrap_or_default(),
      Err(error) => {
        self
          .command_failed(format!("load connections for cleanup: {error}"))
          .await;
        return;
      },
    };
    let mut failures = Vec::new();
    for connection in connections
      .into_iter()
      .filter(|connection| connection.chains.iter().any(|node| node == proxy))
    {
      if let Err(error) = client.close_connection(&connection.id).await {
        failures.push(format!("close connection {}: {error}", connection.id));
      }
    }
    self.refresh_stats().await;
    if !failures.is_empty() {
      self.command_failed(failures.join("; ")).await;
    }
  }

  async fn set_mode(&mut self, mode: ProxyMode) {
    let Some((_, client)) = self.active.clone() else {
      self
        .command_failed("the Mihomo controller is offline".to_string())
        .await;
      return;
    };
    match client
      .patch_base_config(json!({ "mode": mode.as_str() }))
      .await
    {
      Ok(()) => {
        self.state.mode = mode;
        self.state.connection = MihomoConnection::Connected;
        self.state.last_error = None;
        self.publish().await;
      },
      Err(error) => self.command_failed(error.to_string()).await,
    }
  }

  async fn finish_refresh(&mut self, result: rsclash_mihomo::Result<()>) {
    match result {
      Ok(()) => {
        self.state.connection = MihomoConnection::Connected;
        self.state.last_error = None;
        self.publish().await;
      },
      Err(error) => self.mark_degraded(error.to_string()).await,
    }
  }

  async fn mark_degraded(&mut self, error: String) {
    if self.state.connection != MihomoConnection::Degraded
      || self.state.last_error.as_deref() != Some(error.as_str())
    {
      self.state.connection = MihomoConnection::Degraded;
      self.state.last_error = Some(error);
      self.publish().await;
    }
  }

  async fn command_failed(&self, error: String) {
    let _ = self
      .event_tx
      .send(MihomoBridgeEvent::CommandFailed(error))
      .await;
  }

  async fn publish(&self) {
    let _ = self
      .event_tx
      .send(MihomoBridgeEvent::Snapshot(Box::new(self.state.clone())))
      .await;
  }
}

fn selection_is_available(available: Option<&[String]>, selection: &StoredProxySelection) -> bool {
  available.is_some_and(|available| {
    available
      .iter()
      .any(|candidate| candidate == &selection.proxy)
  })
}

pub(crate) async fn run_mihomo_worker(
  access: MihomoAccess,
  command_rx: mpsc::Receiver<MihomoBridgeCommand>,
  event_tx: mpsc::Sender<MihomoBridgeEvent>,
) {
  MihomoWorker::new(access, event_tx).run(command_rx).await;
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::sync::Arc;

  use rsclash_domain::CoreRunMode;
  use rsclash_mihomo::{
    FakeMihomoApi, FakeMihomoState, MihomoApi, MihomoCall,
    models::{Connection, Connections, Groups, Proxy},
  };
  use tokio::sync::mpsc;

  use super::{MihomoAccess, MihomoWorker, ProfileRuntimeSync, StoredProxySelection};

  #[tokio::test]
  async fn profile_sync_restores_available_nodes_before_closing_connections() {
    let fake = FakeMihomoApi::new(FakeMihomoState {
      groups: Groups {
        proxies: vec![Proxy {
          name: "Primary".to_string(),
          kind: "Selector".to_string(),
          all: Some(vec!["Node A".to_string(), "Node B".to_string()]),
          now: Some("Node A".to_string()),
          ..Proxy::default()
        }],
        ..Groups::default()
      },
      connections: Connections {
        connections: Some(vec![Connection {
          id: "connection-a".to_string(),
          chains: vec!["Node A".to_string()],
          ..Connection::default()
        }]),
        ..Connections::default()
      },
      ..FakeMihomoState::default()
    });
    let api: Arc<dyn MihomoApi> = Arc::new(fake.clone());
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut worker = MihomoWorker::new(MihomoAccess::same(Arc::clone(&api)), event_tx);
    worker.active = Some((CoreRunMode::Sidecar, api));

    worker
      .synchronize_profile(ProfileRuntimeSync {
        selections: vec![
          StoredProxySelection {
            group: "Primary".to_string(),
            proxy: "Node B".to_string(),
          },
          StoredProxySelection {
            group: "Missing".to_string(),
            proxy: "Node C".to_string(),
          },
        ],
        close_connections: true,
      })
      .await;

    let calls = fake.calls().expect("the fake calls should be available");
    let select_index = calls
      .iter()
      .position(|call| matches!(call, MihomoCall::SelectProxy { group, proxy } if group == "Primary" && proxy == "Node B"))
      .expect("the saved selection should be restored");
    let close_index = calls
      .iter()
      .position(|call| matches!(call, MihomoCall::CloseAllConnections))
      .expect("profile switching should close connections");
    assert!(select_index < close_index);
    assert!(
      !calls
        .iter()
        .any(|call| matches!(call, MihomoCall::SelectProxy { group, .. } if group == "Missing"))
    );
  }

  #[tokio::test]
  async fn proxy_change_closes_only_connections_using_the_previous_node() {
    let fake = FakeMihomoApi::new(FakeMihomoState {
      connections: Connections {
        connections: Some(vec![
          Connection {
            id: "old-node".to_string(),
            chains: vec!["Node A".to_string()],
            ..Connection::default()
          },
          Connection {
            id: "other-node".to_string(),
            chains: vec!["Node B".to_string()],
            ..Connection::default()
          },
        ]),
        ..Connections::default()
      },
      ..FakeMihomoState::default()
    });
    let api: Arc<dyn MihomoApi> = Arc::new(fake.clone());
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut worker = MihomoWorker::new(MihomoAccess::same(Arc::clone(&api)), event_tx);
    worker.active = Some((CoreRunMode::Sidecar, api));

    worker.close_connections_for_proxy("Node A").await;

    let calls = fake.calls().expect("the fake calls should be available");
    assert!(
      calls
        .iter()
        .any(|call| matches!(call, MihomoCall::CloseConnection(id) if id == "old-node"))
    );
    assert!(
      !calls
        .iter()
        .any(|call| matches!(call, MihomoCall::CloseConnection(id) if id == "other-node"))
    );
    assert!(
      !calls
        .iter()
        .any(|call| matches!(call, MihomoCall::CloseAllConnections))
    );
  }
}
