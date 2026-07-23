use std::{sync::Arc, time::Duration};

use rsclash_domain::{
  CoreRunMode, CoreState, MihomoConnection, MihomoSnapshot, ProxyGroupSnapshot, ProxyMode,
  ProxyOptionSnapshot, TrafficSnapshot,
};
use rsclash_mihomo::{MihomoApi, models::Connections};
use serde_json::json;
use tokio::{
  sync::mpsc,
  time::{Instant, MissedTickBehavior, interval},
};

const STATS_INTERVAL: Duration = Duration::from_secs(1);
const METADATA_POLL_TICKS: u8 = 5;

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
  SetMode(ProxyMode),
}

pub(crate) enum MihomoBridgeEvent {
  Snapshot(MihomoSnapshot),
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
    let (version, config, groups, proxies) = tokio::join!(
      client.version(),
      client.base_config(),
      client.groups(),
      client.proxies(),
    );
    let result = match (version, config, groups, proxies) {
      (Ok(version), Ok(config), Ok(groups), Ok(proxies)) => {
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
          .into_iter()
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
              name: group.name,
              kind: group.kind,
              selected: group.now.or(group.fixed),
              options,
            }
          })
          .collect();
        Ok(())
      },
      (Err(error), _, _, _)
      | (_, Err(error), _, _)
      | (_, _, Err(error), _)
      | (_, _, _, Err(error)) => Err(error),
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
    match client.select_proxy(&group, &proxy).await {
      Ok(()) => {
        if let Some(candidate) = self.state.groups.iter_mut().find(|item| item.name == group) {
          candidate.selected = Some(proxy);
        }
        self.state.connection = MihomoConnection::Connected;
        self.state.last_error = None;
        self.publish().await;
      },
      Err(error) => self.command_failed(error.to_string()).await,
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
      .send(MihomoBridgeEvent::Snapshot(self.state.clone()))
      .await;
  }
}

pub(crate) async fn run_mihomo_worker(
  access: MihomoAccess,
  command_rx: mpsc::Receiver<MihomoBridgeCommand>,
  event_tx: mpsc::Sender<MihomoBridgeEvent>,
) {
  MihomoWorker::new(access, event_tx).run(command_rx).await;
}
