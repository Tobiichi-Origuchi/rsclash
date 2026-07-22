use async_trait::async_trait;
use rsclash_core::{ControllerError, LifecycleController, RunningCore, ServiceLifecycleController};
use rsclash_domain::{CoreChannel, CoreRunMode, CoreState};

use crate::{ServiceClient, ServiceCommand};

pub struct LinuxServiceController {
  client: ServiceClient,
}

impl LinuxServiceController {
  pub const fn new(client: ServiceClient) -> Self {
    Self { client }
  }

  fn running(state: CoreState) -> Result<RunningCore, ControllerError> {
    match state {
      CoreState::Running { version, .. } => Ok(RunningCore::new(CoreRunMode::Service, version)),
      CoreState::Failed { message } => Err(ControllerError::process_exited(message)),
      state => Err(ControllerError::process_exited(format!(
        "the service core is not running: {state:?}"
      ))),
    }
  }
}

#[async_trait]
impl ServiceLifecycleController for LinuxServiceController {
  async fn is_available(&mut self) -> Result<bool, ControllerError> {
    match self.client.ping().await {
      Ok(version) if version == env!("CARGO_PKG_VERSION") => Ok(true),
      Ok(version) => Err(ControllerError::new(format!(
        "service version mismatch: GUI {}, service {version}",
        env!("CARGO_PKG_VERSION")
      ))),
      Err(_) => Ok(false),
    }
  }
}

#[async_trait]
impl LifecycleController for LinuxServiceController {
  async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
    let state = self
      .client
      .command(ServiceCommand::StartCore { channel })
      .await
      .map_err(|error| ControllerError::new(format!("start core through service: {error}")))?;
    Self::running(state)
  }

  async fn stop(&mut self) -> Result<(), ControllerError> {
    let state = self
      .client
      .command(ServiceCommand::StopCore)
      .await
      .map_err(|error| ControllerError::new(format!("stop core through service: {error}")))?;
    if state == CoreState::Stopped {
      Ok(())
    } else {
      Err(ControllerError::new(format!(
        "service returned an unexpected stop state: {state:?}"
      )))
    }
  }

  async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
    let state = self
      .client
      .command(ServiceCommand::ReloadCore)
      .await
      .map_err(|error| ControllerError::new(format!("reload core through service: {error}")))?;
    Self::running(state)
  }

  async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
    let status =
      self.client.status().await.map_err(|error| {
        ControllerError::unhealthy(format!("query service core status: {error}"))
      })?;
    Self::running(status.core)
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{
      Arc,
      atomic::{AtomicU64, Ordering},
    },
    time::Duration,
  };

  use async_trait::async_trait;
  use rsclash_core::{
    ControllerError, CoreRuntime, LifecycleController, RunningCore, ServiceLifecycleController as _,
  };
  use rsclash_domain::{CoreChannel, CoreRunMode};
  use tokio::sync::oneshot;

  use super::LinuxServiceController;
  use crate::{CoreServiceHandler, ServiceClient, ServiceServer};

  struct FakeController;

  #[async_trait]
  impl LifecycleController for FakeController {
    async fn start(&mut self, _channel: CoreChannel) -> Result<RunningCore, ControllerError> {
      Ok(RunningCore::new(
        CoreRunMode::Sidecar,
        Some("test-core".to_string()),
      ))
    }

    async fn stop(&mut self) -> Result<(), ControllerError> {
      Ok(())
    }

    async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
      Ok(RunningCore::new(
        CoreRunMode::Sidecar,
        Some("test-core".to_string()),
      ))
    }

    async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
      Ok(RunningCore::new(
        CoreRunMode::Sidecar,
        Some("test-core".to_string()),
      ))
    }
  }

  #[tokio::test]
  async fn routes_the_full_lifecycle_through_the_service() {
    let directory = TestDirectory::new();
    let socket = directory.path().join("service.sock");
    let uid = fs::metadata("/proc/self")
      .expect("process metadata should exist")
      .uid();
    let core_runtime = CoreRuntime::spawn(&tokio::runtime::Handle::current(), FakeController);
    let handler = Arc::new(CoreServiceHandler::new(core_runtime.handle()));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = ServiceServer::new(&socket, uid, handler);
    let server_task = tokio::spawn(server.run_until(async move {
      let _ = shutdown_rx.await;
    }));
    wait_for_socket(&socket).await;
    let mut controller = LinuxServiceController::new(ServiceClient::new(&socket));

    assert_eq!(controller.is_available().await.ok(), Some(true));
    let running = controller
      .start(CoreChannel::Stable)
      .await
      .expect("service start should succeed");
    assert_eq!(running.mode, CoreRunMode::Service);
    assert!(controller.reload().await.is_ok());
    assert!(controller.health_check().await.is_ok());
    assert!(controller.stop().await.is_ok());

    let _ = shutdown_tx.send(());
    assert!(
      server_task
        .await
        .expect("server task should finish")
        .is_ok()
    );
    assert!(core_runtime.shutdown().await.is_ok());
  }

  async fn wait_for_socket(path: &Path) {
    tokio::time::timeout(Duration::from_secs(1), async {
      while !path.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .expect("socket should appear");
  }

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-service-lifecycle-test-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&path).expect("test directory should be created");
      Self(path)
    }

    fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.0);
    }
  }
}
