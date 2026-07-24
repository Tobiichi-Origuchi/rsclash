use std::{
  collections::{BTreeSet, VecDeque},
  future::pending,
  sync::Arc,
  time::Duration,
};

use futures_util::StreamExt as _;
use rsclash_domain::{
  ConnectionSnapshot, CoreRunMode, CoreState, LogSnapshot, MetricPoint, MihomoConnection,
  MihomoSnapshot, Page, ProxyGroupSnapshot, ProxyMode, ProxyNodeSource, ProxyOptionSnapshot,
  RuleProviderSnapshot, RuleSnapshot, StreamLogLevel, TrafficSnapshot,
};
use rsclash_mihomo::{
  MihomoApi, MihomoStream,
  models::{Connection, Connections, LogEntry, LogLevel, Memory, Traffic},
};
use serde_json::json;
use tokio::{
  sync::mpsc,
  time::{MissedTickBehavior, interval},
};

use crate::profiles::{ProfileRuntimeSync, StoredProxySelection};
use crate::proxy_view::{ProxyViewBuilder, ProxyViewInput};

const STATS_INTERVAL: Duration = Duration::from_secs(1);
const METADATA_POLL_TICKS: u8 = 5;
const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const METRIC_CAPACITY: usize = 300;
const CLOSED_CONNECTION_CAPACITY: usize = 2_048;
const LOG_CAPACITY: usize = 10_000;
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
  ProxyChainChanged { group: String, nodes: Vec<String> },
  SetPresentation { page: Page, visible: bool },
  UpdateRuleProvider { name: String },
  CloseConnection { id: String },
  CloseAllConnections,
  ClearClosedConnections,
  SetConnectionsPaused(bool),
  ClearLogs,
  SetLogsPaused(bool),
  SetLogLevel(StreamLogLevel),
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

struct MihomoWorker {
  access: MihomoAccess,
  active: Option<(CoreRunMode, Arc<dyn MihomoApi>)>,
  state: MihomoSnapshot,
  metadata_ticks: u8,
  pending_profile_sync: Option<ProfileRuntimeSync>,
  page: Page,
  visible: bool,
  log_level: StreamLogLevel,
  traffic_stream: Option<MihomoStream<Traffic>>,
  memory_stream: Option<MihomoStream<Memory>>,
  connections_stream: Option<MihomoStream<Connections>>,
  logs_stream: Option<MihomoStream<LogEntry>>,
  metrics: VecDeque<MetricPoint>,
  connections: Vec<ConnectionSnapshot>,
  closed_connections: VecDeque<ConnectionSnapshot>,
  logs: VecDeque<LogSnapshot>,
  stream_sequence: u64,
  stream_dirty: bool,
  event_tx: mpsc::Sender<MihomoBridgeEvent>,
}

impl MihomoWorker {
  fn new(access: MihomoAccess, event_tx: mpsc::Sender<MihomoBridgeEvent>) -> Self {
    Self {
      access,
      active: None,
      state: MihomoSnapshot::default(),
      metadata_ticks: 0,
      pending_profile_sync: None,
      page: Page::Home,
      visible: true,
      log_level: StreamLogLevel::Info,
      traffic_stream: None,
      memory_stream: None,
      connections_stream: None,
      logs_stream: None,
      metrics: VecDeque::with_capacity(METRIC_CAPACITY),
      connections: Vec::new(),
      closed_connections: VecDeque::with_capacity(CLOSED_CONNECTION_CAPACITY),
      logs: VecDeque::with_capacity(LOG_CAPACITY),
      stream_sequence: 0,
      stream_dirty: false,
      event_tx,
    }
  }

  async fn run(mut self, mut command_rx: mpsc::Receiver<MihomoBridgeCommand>) {
    let mut stats_interval = interval(STATS_INTERVAL);
    stats_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut flush_interval = interval(STREAM_FLUSH_INTERVAL);
    flush_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

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
        _ = flush_interval.tick() => {
          self.flush_stream_state();
        },
        item = next_stream_item(&mut self.traffic_stream) => {
          self.handle_traffic_item(item);
        },
        item = next_stream_item(&mut self.memory_stream) => {
          self.handle_memory_item(item);
        },
        item = next_stream_item(&mut self.connections_stream) => {
          self.handle_connections_item(item);
        },
        item = next_stream_item(&mut self.logs_stream) => {
          self.handle_log_item(item);
        },
      }
    }
  }

  async fn handle_command(&mut self, command: MihomoBridgeCommand) {
    match command {
      MihomoBridgeCommand::CoreState(state) => self.handle_core_state(state).await,
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
      MihomoBridgeCommand::ProxyChainChanged { group, nodes } => {
        self.state.proxy_chain.group = (!nodes.is_empty()).then_some(group);
        self.state.proxy_chain.connected = !nodes.is_empty();
        self.state.proxy_chain.nodes = nodes;
        self.publish();
      },
      MihomoBridgeCommand::SetPresentation { page, visible } => {
        self.page = page;
        self.visible = visible;
        self.configure_streams().await;
      },
      MihomoBridgeCommand::UpdateRuleProvider { name } => {
        self.update_rule_provider(name).await;
      },
      MihomoBridgeCommand::CloseConnection { id } => {
        self.close_connection(id).await;
      },
      MihomoBridgeCommand::CloseAllConnections => {
        self.close_all_connections().await;
      },
      MihomoBridgeCommand::ClearClosedConnections => {
        self.closed_connections.clear();
        self.state.closed_connections = Arc::new(Vec::new());
        self.publish();
      },
      MihomoBridgeCommand::SetConnectionsPaused(paused) => {
        self.state.connections_paused = paused;
        self.publish();
      },
      MihomoBridgeCommand::ClearLogs => {
        self.logs.clear();
        self.state.logs = Arc::new(Vec::new());
        self.publish();
      },
      MihomoBridgeCommand::SetLogsPaused(paused) => {
        self.state.logs_paused = paused;
        self.publish();
      },
      MihomoBridgeCommand::SetLogLevel(level) => {
        self.log_level = level;
        self.logs_stream = None;
        self.configure_streams().await;
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

  async fn handle_core_state(&mut self, state: CoreState) {
    if let CoreState::Running { mode, .. } = state {
      if self.active.as_ref().map(|(active, _)| *active) != Some(mode) {
        self.active = Some((mode, self.access.client(mode)));
        self.state = MihomoSnapshot {
          connection: MihomoConnection::Connecting,
          ..MihomoSnapshot::default()
        };
        self.metadata_ticks = 0;
        self.publish();
        self.refresh_all().await;
        if let Some(sync) = self.pending_profile_sync.take() {
          self.synchronize_profile(sync).await;
        }
      }
    } else if self.active.take().is_some() || self.state != MihomoSnapshot::default() {
      self.state = MihomoSnapshot::default();
      self.drop_streams();
      self.metrics.clear();
      self.connections.clear();
      self.closed_connections.clear();
      self.logs.clear();
      self.metadata_ticks = 0;
      self.publish();
    }
  }

  async fn poll(&mut self) {
    self.configure_streams().await;
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
    self.configure_streams().await;
    self.refresh_stats().await;
  }

  async fn refresh_metadata(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    let (version, config, groups, proxies, providers, rules, rule_providers) = tokio::join!(
      client.version(),
      client.base_config(),
      client.groups(),
      client.proxies(),
      client.proxy_providers(),
      client.rules(),
      client.rule_providers(),
    );
    let result = match (
      version,
      config,
      groups,
      proxies,
      providers,
      rules,
      rule_providers,
    ) {
      (Ok(version), Ok(config), Ok(groups), Ok(proxies), providers, rules, rule_providers) => {
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
        self.state.proxy_view = Arc::new(ProxyViewBuilder::build(ProxyViewInput {
          runtime_group_order: groups
            .proxies
            .iter()
            .map(|group| group.name.clone())
            .collect(),
          proxies,
          providers: providers.ok(),
        }));
        if let Ok(rules) = rules {
          self.state.rules = Arc::new(
            rules
              .rules
              .into_iter()
              .map(|rule| RuleSnapshot {
                index: rule.index,
                kind: rule.kind,
                payload: rule.payload,
                proxy: rule.proxy,
                size: rule.size,
              })
              .collect(),
          );
        }
        if let Ok(providers) = rule_providers {
          let mut providers = providers.providers.into_iter().collect::<Vec<_>>();
          providers.sort_by(|(left, _), (right, _)| left.cmp(right));
          self.state.rule_providers = Arc::new(
            providers
              .into_iter()
              .map(|(name, provider)| RuleProviderSnapshot {
                name,
                kind: provider.kind,
                behavior: provider.behavior,
                format: provider.format,
                rule_count: provider.rule_count,
                updated_at: provider.updated_at,
                vehicle_type: provider.vehicle_type,
              })
              .collect(),
          );
        }
        Ok(())
      },
      (Err(error), _, _, _, _, _, _)
      | (_, Err(error), _, _, _, _, _)
      | (_, _, Err(error), _, _, _, _)
      | (_, _, _, Err(error), _, _, _) => Err(error),
    };
    self.finish_refresh(result);
  }

  async fn refresh_stats(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    match client.connections().await {
      Ok(connections) => {
        self.update_connections(connections);
        self.state.connection = MihomoConnection::Connected;
        self.state.last_error = None;
        self.flush_stream_state();
      },
      Err(error) => self.mark_degraded(error.to_string()),
    }
  }

  fn update_connections(&mut self, connections: Connections) {
    let previous = self
      .connections
      .iter()
      .map(|connection| (connection.id.as_str(), connection))
      .collect::<std::collections::HashMap<_, _>>();
    let active = connections
      .connections
      .unwrap_or_default()
      .into_iter()
      .map(connection_snapshot)
      .collect::<Vec<_>>();
    let active_ids = active
      .iter()
      .map(|connection| connection.id.as_str())
      .collect::<BTreeSet<_>>();
    for connection in previous
      .into_values()
      .filter(|connection| !active_ids.contains(connection.id.as_str()))
    {
      push_bounded(
        &mut self.closed_connections,
        connection.clone(),
        CLOSED_CONNECTION_CAPACITY,
      );
    }
    self.connections = active;
    self.state.traffic.upload_total = connections.upload_total;
    self.state.traffic.download_total = connections.download_total;
    self.state.memory_bytes = connections.memory;
    self.state.connection_count = self.connections.len() as u64;
    self.stream_dirty = true;
  }

  async fn configure_streams(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      self.drop_streams();
      return;
    };
    if !self.visible {
      self.drop_streams();
      return;
    }

    if self.page == Page::Home {
      if self.traffic_stream.is_none() {
        match client.traffic_stream().await {
          Ok(stream) => self.traffic_stream = Some(stream),
          Err(error) => self.mark_degraded(error.to_string()),
        }
      }
      if self.memory_stream.is_none() {
        match client.memory_stream().await {
          Ok(stream) => self.memory_stream = Some(stream),
          Err(error) => self.mark_degraded(error.to_string()),
        }
      }
    } else {
      self.traffic_stream = None;
      self.memory_stream = None;
    }

    if self.page == Page::Connections {
      if self.connections_stream.is_none() {
        match client.connections_stream().await {
          Ok(stream) => self.connections_stream = Some(stream),
          Err(error) => self.mark_degraded(error.to_string()),
        }
      }
    } else {
      self.connections_stream = None;
    }

    if self.page == Page::Logs {
      if self.logs_stream.is_none() {
        match client.logs_stream(mihomo_log_level(self.log_level)).await {
          Ok(stream) => self.logs_stream = Some(stream),
          Err(error) => self.mark_degraded(error.to_string()),
        }
      }
    } else {
      self.logs_stream = None;
    }
  }

  fn drop_streams(&mut self) {
    self.traffic_stream = None;
    self.memory_stream = None;
    self.connections_stream = None;
    self.logs_stream = None;
  }

  fn handle_traffic_item(&mut self, item: Option<rsclash_mihomo::Result<Traffic>>) {
    match item {
      Some(Ok(traffic)) => {
        self.state.traffic = TrafficSnapshot {
          upload_bytes_per_second: traffic.up,
          download_bytes_per_second: traffic.down,
          upload_total: traffic.up_total,
          download_total: traffic.down_total,
        };
        self.stream_sequence = self.stream_sequence.saturating_add(1);
        push_bounded(
          &mut self.metrics,
          MetricPoint {
            sequence: self.stream_sequence,
            upload_bytes_per_second: traffic.up,
            download_bytes_per_second: traffic.down,
            memory_bytes: self.state.memory_bytes,
          },
          METRIC_CAPACITY,
        );
        self.stream_dirty = true;
      },
      Some(Err(error)) => {
        self.traffic_stream = None;
        self.mark_degraded(error.to_string());
      },
      None => self.traffic_stream = None,
    }
  }

  fn handle_memory_item(&mut self, item: Option<rsclash_mihomo::Result<Memory>>) {
    match item {
      Some(Ok(memory)) => {
        self.state.memory_bytes = memory.inuse;
        self.stream_dirty = true;
      },
      Some(Err(error)) => {
        self.memory_stream = None;
        self.mark_degraded(error.to_string());
      },
      None => self.memory_stream = None,
    }
  }

  fn handle_connections_item(&mut self, item: Option<rsclash_mihomo::Result<Connections>>) {
    match item {
      Some(Ok(connections)) if !self.state.connections_paused => {
        self.update_connections(connections);
      },
      Some(Ok(_)) => {},
      Some(Err(error)) => {
        self.connections_stream = None;
        self.mark_degraded(error.to_string());
      },
      None => self.connections_stream = None,
    }
  }

  fn handle_log_item(&mut self, item: Option<rsclash_mihomo::Result<LogEntry>>) {
    match item {
      Some(Ok(log)) if !self.state.logs_paused => {
        self.stream_sequence = self.stream_sequence.saturating_add(1);
        push_bounded(
          &mut self.logs,
          LogSnapshot {
            sequence: self.stream_sequence,
            level: log.level,
            payload: bounded_log_payload(log.payload),
          },
          LOG_CAPACITY,
        );
        self.stream_dirty = true;
      },
      Some(Ok(_)) => {},
      Some(Err(error)) => {
        self.logs_stream = None;
        self.mark_degraded(error.to_string());
      },
      None => self.logs_stream = None,
    }
  }

  fn flush_stream_state(&mut self) {
    if !self.stream_dirty {
      return;
    }
    self.state.metrics = Arc::new(self.metrics.iter().cloned().collect());
    self.state.connections = Arc::new(self.connections.clone());
    self.state.closed_connections = Arc::new(self.closed_connections.iter().cloned().collect());
    self.state.logs = Arc::new(self.logs.iter().cloned().collect());
    self.state.connection = MihomoConnection::Connected;
    self.state.last_error = None;
    self.stream_dirty = !self.publish();
  }

  async fn update_rule_provider(&mut self, name: String) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
      return;
    };
    match client.update_rule_provider(&name).await {
      Ok(()) => self.refresh_metadata().await,
      Err(error) => self.command_failed(error.to_string()),
    }
  }

  async fn close_connection(&mut self, id: String) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    match client.close_connection(&id).await {
      Ok(()) => self.refresh_stats().await,
      Err(error) => self.command_failed(error.to_string()),
    }
  }

  async fn close_all_connections(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    match client.close_all_connections().await {
      Ok(()) => self.refresh_stats().await,
      Err(error) => self.command_failed(error.to_string()),
    }
  }

  async fn select_proxy(&mut self, group: String, proxy: String) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
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
        self.publish();
        let _ = self
          .event_tx
          .send(MihomoBridgeEvent::ProxySelected {
            group,
            proxy,
            previous,
          })
          .await;
      },
      Err(error) => self.command_failed(error.to_string()),
    }
  }

  async fn test_proxy(&mut self, record_id: String) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
      return;
    };
    let Some(record) = self.state.proxy_view.records.get(&record_id).cloned() else {
      self.command_failed(format!("proxy record {record_id} no longer exists"));
      return;
    };
    self.set_proxy_busy(true);
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
        self.set_proxy_busy(false);
        self.command_failed(format!("proxy record {record_id} has no source"));
        return;
      },
    };
    match result {
      Ok(delay) => {
        if let Some(candidate) = Arc::make_mut(&mut self.state.proxy_view)
          .records
          .get_mut(&record_id)
        {
          candidate.delay_ms = (delay.delay > 0).then_some(delay.delay);
          candidate.alive = delay.delay > 0;
        }
        self.state.last_error = None;
        self.set_proxy_busy(false);
      },
      Err(error) => {
        self.set_proxy_busy(false);
        self.command_failed(error.to_string());
      },
    }
  }

  async fn test_proxy_group(&mut self, name: String) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
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
    self.set_proxy_busy(true);
    match client
      .delay_group(&name, &test_url, DEFAULT_DELAY_TIMEOUT_MS)
      .await
    {
      Ok(delays) => {
        self.apply_proxy_delays(&delays);
        self.state.last_error = None;
        self.set_proxy_busy(false);
      },
      Err(error) => {
        self.set_proxy_busy(false);
        self.command_failed(error.to_string());
      },
    }
  }

  async fn test_all_proxies(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
      return;
    };
    self.set_proxy_busy(true);
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
    self.set_proxy_busy(false);
    if !failures.is_empty() {
      self.command_failed(failures.join("; "));
    }
  }

  async fn update_proxy_provider(&mut self, name: String) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
      return;
    };
    self.set_proxy_busy(true);
    let result = client.update_proxy_provider(&name).await;
    if result.is_ok() {
      self.refresh_metadata().await;
    }
    self.set_proxy_busy(false);
    if let Err(error) = result {
      self.command_failed(error.to_string());
    }
  }

  async fn healthcheck_proxy_provider(&mut self, name: String) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
      return;
    };
    self.set_proxy_busy(true);
    let result = client.healthcheck_proxy_provider(&name).await;
    if result.is_ok() {
      self.refresh_metadata().await;
    }
    self.set_proxy_busy(false);
    if let Err(error) = result {
      self.command_failed(error.to_string());
    }
  }

  async fn update_all_proxy_providers(&mut self) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
      return;
    };
    self.set_proxy_busy(true);
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
    self.set_proxy_busy(false);
    if !failures.is_empty() {
      self.command_failed(failures.join("; "));
    }
  }

  fn apply_proxy_delays(&mut self, delays: &std::collections::HashMap<String, u32>) {
    for record in Arc::make_mut(&mut self.state.proxy_view)
      .records
      .values_mut()
    {
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

  fn set_proxy_busy(&mut self, busy: bool) {
    if self.state.proxy_busy != busy {
      self.state.proxy_busy = busy;
      self.publish();
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
      self.command_failed(failures.join("; "));
    }
  }

  async fn close_connections_for_proxy(&mut self, proxy: &str) {
    let Some((_, client)) = self.active.clone() else {
      return;
    };
    let connections = match client.connections().await {
      Ok(connections) => connections.connections.unwrap_or_default(),
      Err(error) => {
        self.command_failed(format!("load connections for cleanup: {error}"));
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
      self.command_failed(failures.join("; "));
    }
  }

  async fn set_mode(&mut self, mode: ProxyMode) {
    let Some((_, client)) = self.active.clone() else {
      self.command_failed("the Mihomo controller is offline".to_string());
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
        self.publish();
      },
      Err(error) => self.command_failed(error.to_string()),
    }
  }

  fn finish_refresh(&mut self, result: rsclash_mihomo::Result<()>) {
    match result {
      Ok(()) => {
        self.state.connection = MihomoConnection::Connected;
        self.state.last_error = None;
        self.publish();
      },
      Err(error) => self.mark_degraded(error.to_string()),
    }
  }

  fn mark_degraded(&mut self, error: String) {
    if self.state.connection != MihomoConnection::Degraded
      || self.state.last_error.as_deref() != Some(error.as_str())
    {
      self.state.connection = MihomoConnection::Degraded;
      self.state.last_error = Some(error);
      self.publish();
    }
  }

  fn command_failed(&self, error: String) {
    let _ = self
      .event_tx
      .try_send(MihomoBridgeEvent::CommandFailed(error));
  }

  fn publish(&self) -> bool {
    self
      .event_tx
      .try_send(MihomoBridgeEvent::Snapshot(Box::new(self.state.clone())))
      .is_ok()
  }
}

async fn next_stream_item<T>(
  stream: &mut Option<MihomoStream<T>>,
) -> Option<rsclash_mihomo::Result<T>> {
  match stream {
    Some(stream) => stream.next().await,
    None => pending().await,
  }
}

fn push_bounded<T>(buffer: &mut VecDeque<T>, value: T, capacity: usize) {
  if buffer.len() == capacity {
    buffer.pop_front();
  }
  buffer.push_back(value);
}

fn connection_snapshot(connection: Connection) -> ConnectionSnapshot {
  let metadata = connection.metadata;
  let source = endpoint(&metadata.source_ip, &metadata.source_port);
  let destination_host = if metadata.host.is_empty() {
    metadata.destination_ip.as_str()
  } else {
    metadata.host.as_str()
  };
  ConnectionSnapshot {
    id: connection.id,
    network: metadata.network,
    source,
    destination: endpoint(destination_host, &metadata.destination_port),
    host: metadata.host,
    process: if metadata.process.is_empty() {
      metadata.process_path
    } else {
      metadata.process
    },
    upload: connection.upload,
    download: connection.download,
    start: connection.start,
    chains: connection.chains,
    rule: connection.rule,
    rule_payload: connection.rule_payload,
  }
}

fn endpoint(host: &str, port: &str) -> String {
  if port.is_empty() {
    host.to_string()
  } else if host.contains(':') && !host.starts_with('[') {
    format!("[{host}]:{port}")
  } else {
    format!("{host}:{port}")
  }
}

fn bounded_log_payload(mut payload: String) -> String {
  const MAX_LOG_PAYLOAD_BYTES: usize = 16 * 1024;
  if payload.len() <= MAX_LOG_PAYLOAD_BYTES {
    return payload;
  }
  let mut boundary = MAX_LOG_PAYLOAD_BYTES;
  while !payload.is_char_boundary(boundary) {
    boundary -= 1;
  }
  payload.truncate(boundary);
  payload.push('…');
  payload
}

const fn mihomo_log_level(level: StreamLogLevel) -> LogLevel {
  match level {
    StreamLogLevel::Debug => LogLevel::Debug,
    StreamLogLevel::Info => LogLevel::Info,
    StreamLogLevel::Warning => LogLevel::Warning,
    StreamLogLevel::Error => LogLevel::Error,
    StreamLogLevel::Silent => LogLevel::Silent,
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

  use rsclash_domain::{CoreRunMode, Page};
  use rsclash_mihomo::{
    FakeMihomoApi, FakeMihomoState, MihomoApi, MihomoCall,
    models::{Connection, Connections, Groups, LogEntry, Proxy},
  };
  use tokio::sync::mpsc;

  use super::{
    LOG_CAPACITY, MihomoAccess, MihomoBridgeCommand, MihomoWorker, ProfileRuntimeSync,
    StoredProxySelection,
  };

  #[test]
  fn log_stream_buffer_stays_bounded_under_large_input() {
    let api: Arc<dyn MihomoApi> = Arc::new(FakeMihomoApi::default());
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut worker = MihomoWorker::new(MihomoAccess::same(api), event_tx);
    for index in 0..100_000 {
      worker.handle_log_item(Some(Ok(LogEntry {
        level: "info".to_string(),
        payload: format!("line {index}"),
        ..LogEntry::default()
      })));
    }
    worker.flush_stream_state();

    assert_eq!(worker.logs.len(), LOG_CAPACITY);
    assert_eq!(worker.state.logs.len(), LOG_CAPACITY);
    assert_eq!(worker.state.logs[0].payload, "line 90000");
  }

  #[test]
  fn closed_connection_history_has_a_fixed_capacity() {
    let api: Arc<dyn MihomoApi> = Arc::new(FakeMihomoApi::default());
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut worker = MihomoWorker::new(MihomoAccess::same(api), event_tx);
    worker.update_connections(Connections {
      connections: Some(
        (0..3_000)
          .map(|index| Connection {
            id: format!("connection-{index}"),
            ..Connection::default()
          })
          .collect(),
      ),
      ..Connections::default()
    });
    worker.update_connections(Connections::default());
    worker.flush_stream_state();

    assert_eq!(
      worker.closed_connections.len(),
      super::CLOSED_CONNECTION_CAPACITY
    );
    assert_eq!(
      worker.state.closed_connections.len(),
      super::CLOSED_CONNECTION_CAPACITY
    );
  }

  #[tokio::test]
  async fn presentation_opens_only_streams_for_the_visible_page() {
    let fake = FakeMihomoApi::default();
    let api: Arc<dyn MihomoApi> = Arc::new(fake.clone());
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut worker = MihomoWorker::new(MihomoAccess::same(Arc::clone(&api)), event_tx);
    worker.active = Some((CoreRunMode::Sidecar, api));

    worker
      .handle_command(MihomoBridgeCommand::SetPresentation {
        page: Page::Home,
        visible: true,
      })
      .await;
    worker
      .handle_command(MihomoBridgeCommand::SetPresentation {
        page: Page::Logs,
        visible: true,
      })
      .await;
    worker
      .handle_command(MihomoBridgeCommand::SetPresentation {
        page: Page::Logs,
        visible: false,
      })
      .await;

    let calls = fake.calls().expect("the fake calls should be available");
    assert!(calls.contains(&MihomoCall::TrafficStream));
    assert!(calls.contains(&MihomoCall::MemoryStream));
    assert!(
      calls
        .iter()
        .any(|call| matches!(call, MihomoCall::LogsStream(_)))
    );
    assert!(worker.traffic_stream.is_none());
    assert!(worker.memory_stream.is_none());
    assert!(worker.logs_stream.is_none());
  }

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
