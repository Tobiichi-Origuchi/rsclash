use std::sync::Arc;

use rsclash_domain::SystemProxyView;
use rsclash_platform::SystemProxyService;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct SystemProxyAccess {
  service: Arc<SystemProxyService>,
}

impl SystemProxyAccess {
  pub const fn new(service: Arc<SystemProxyService>) -> Self {
    Self { service }
  }
}

#[derive(Clone, Debug)]
pub(crate) enum SystemProxyBridgeCommand {
  Refresh,
  SetEnabled {
    enabled: bool,
    port: u16,
    bypass: Vec<String>,
    pac_url: Option<String>,
  },
}

pub(crate) enum SystemProxyBridgeEvent {
  Snapshot(SystemProxyView),
  CommandFailed(String),
}

struct SystemProxyWorker {
  access: SystemProxyAccess,
  state: SystemProxyView,
  event_tx: mpsc::Sender<SystemProxyBridgeEvent>,
}

impl SystemProxyWorker {
  fn new(access: SystemProxyAccess, event_tx: mpsc::Sender<SystemProxyBridgeEvent>) -> Self {
    Self {
      access,
      state: SystemProxyView::default(),
      event_tx,
    }
  }

  async fn run(mut self, mut command_rx: mpsc::Receiver<SystemProxyBridgeCommand>) {
    self.refresh().await;
    while let Some(command) = command_rx.recv().await {
      match command {
        SystemProxyBridgeCommand::Refresh => self.refresh().await,
        SystemProxyBridgeCommand::SetEnabled {
          enabled,
          port,
          bypass,
          pac_url,
        } => {
          self.set_busy(true).await;
          let result = if let Some(url) = pac_url.filter(|_| enabled) {
            self.access.service.enable_pac(&url, bypass).await
          } else if enabled {
            self.access.service.enable("127.0.0.1", port, bypass).await
          } else {
            self.access.service.disable().await.map(|_| ())
          };
          match result {
            Ok(()) => self.refresh().await,
            Err(error) => {
              self.state.busy = false;
              self.state.detail = Some(error.to_string());
              self.publish().await;
              let _ = self
                .event_tx
                .send(SystemProxyBridgeEvent::CommandFailed(error.to_string()))
                .await;
            },
          }
        },
      }
    }
  }

  async fn refresh(&mut self) {
    match self.access.service.status().await {
      Ok(status) => {
        self.state = SystemProxyView {
          available: true,
          enabled: status.enabled_by_app,
          applied: status.applied,
          busy: false,
          backend: Some(status.backend),
          detail: (!status.applied && status.enabled_by_app)
            .then(|| "system proxy settings were changed externally".to_string()),
        };
      },
      Err(error) => {
        self.state.available = false;
        self.state.busy = false;
        self.state.detail = Some(error.to_string());
      },
    }
    self.publish().await;
  }

  async fn set_busy(&mut self, busy: bool) {
    self.state.busy = busy;
    self.publish().await;
  }

  async fn publish(&self) {
    let _ = self
      .event_tx
      .send(SystemProxyBridgeEvent::Snapshot(self.state.clone()))
      .await;
  }
}

pub(crate) async fn run_system_proxy_worker(
  access: SystemProxyAccess,
  command_rx: mpsc::Receiver<SystemProxyBridgeCommand>,
  event_tx: mpsc::Sender<SystemProxyBridgeEvent>,
) {
  SystemProxyWorker::new(access, event_tx)
    .run(command_rx)
    .await;
}

#[cfg(test)]
#[allow(
  clippy::expect_used,
  clippy::panic,
  reason = "tests use explicit failures for clear diagnostics"
)]
mod tests {
  use std::{
    fs,
    path::PathBuf,
    sync::{
      Arc,
      atomic::{AtomicU64, Ordering},
    },
  };

  use async_trait::async_trait;
  use rsclash_platform::{
    PendingSystemRecovery, Result, SystemProxyBackend, SystemProxyService, SystemProxySnapshot,
    SystemRecoveryBackend,
  };
  use tokio::sync::{Mutex, mpsc};

  use super::{
    SystemProxyAccess, SystemProxyBridgeCommand, SystemProxyBridgeEvent, run_system_proxy_worker,
  };

  struct FakeProxyBackend {
    current: Mutex<SystemProxySnapshot>,
  }

  impl FakeProxyBackend {
    fn new(current: SystemProxySnapshot) -> Self {
      Self {
        current: Mutex::new(current),
      }
    }
  }

  #[async_trait]
  impl SystemRecoveryBackend for FakeProxyBackend {
    async fn restore(&self, pending: &PendingSystemRecovery) -> Result<()> {
      if let Some(snapshot) = &pending.system_proxy {
        *self.current.lock().await = snapshot.clone();
      }
      Ok(())
    }
  }

  #[async_trait]
  impl SystemProxyBackend for FakeProxyBackend {
    fn name(&self) -> &'static str {
      "fake"
    }

    async fn current(&self) -> Result<SystemProxySnapshot> {
      Ok(self.current.lock().await.clone())
    }

    async fn apply(&self, snapshot: &SystemProxySnapshot) -> Result<()> {
      *self.current.lock().await = snapshot.clone();
      Ok(())
    }
  }

  fn recovery_path() -> (PathBuf, PathBuf) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
      "rsclash-system-proxy-worker-{}-{}",
      std::process::id(),
      NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).expect("the recovery directory should be created");
    let path = root.join("system-recovery.json");
    (root, path)
  }

  async fn next_stable_snapshot(
    event_rx: &mut mpsc::Receiver<SystemProxyBridgeEvent>,
  ) -> rsclash_domain::SystemProxyView {
    loop {
      match event_rx
        .recv()
        .await
        .expect("the system proxy worker should keep publishing")
      {
        SystemProxyBridgeEvent::Snapshot(snapshot) if !snapshot.busy => return snapshot,
        SystemProxyBridgeEvent::Snapshot(_) => {},
        SystemProxyBridgeEvent::CommandFailed(message) => {
          panic!("the system proxy command should succeed: {message}");
        },
      }
    }
  }

  #[tokio::test]
  async fn worker_enables_and_restores_the_system_proxy() {
    let original = SystemProxySnapshot {
      mode: Some("none".to_string()),
      ..SystemProxySnapshot::default()
    };
    let backend = Arc::new(FakeProxyBackend::new(original.clone()));
    let (recovery_root, recovery_path) = recovery_path();
    let service = Arc::new(SystemProxyService::new(
      recovery_path,
      Arc::<FakeProxyBackend>::clone(&backend),
    ));
    let (command_tx, command_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_system_proxy_worker(
      SystemProxyAccess::new(service),
      command_rx,
      event_tx,
    ));

    let initial = next_stable_snapshot(&mut event_rx).await;
    assert!(initial.available);
    assert!(!initial.enabled);
    command_tx
      .send(SystemProxyBridgeCommand::SetEnabled {
        enabled: true,
        port: 17897,
        bypass: vec!["localhost".to_string()],
        pac_url: None,
      })
      .await
      .expect("the enable command should be queued");
    let enabled = next_stable_snapshot(&mut event_rx).await;
    assert!(enabled.enabled, "{enabled:?}");
    assert!(enabled.applied);
    assert_eq!(
      backend
        .current()
        .await
        .expect("current proxy should load")
        .http_proxy,
      Some("127.0.0.1:17897".to_string())
    );

    command_tx
      .send(SystemProxyBridgeCommand::SetEnabled {
        enabled: false,
        port: 0,
        bypass: Vec::new(),
        pac_url: None,
      })
      .await
      .expect("the disable command should be queued");
    let disabled = next_stable_snapshot(&mut event_rx).await;
    assert!(!disabled.enabled);
    assert_eq!(
      backend.current().await.expect("current proxy should load"),
      original
    );
    drop(command_tx);
    worker.await.expect("the worker should stop cleanly");
    fs::remove_dir_all(recovery_root).expect("the recovery directory should be removed");
  }
}
