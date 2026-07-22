use std::path::Path;

use async_trait::async_trait;
use rsclash_config::{Error, Result, RuntimeActivator};
use rsclash_core::CoreHandle;

#[derive(Clone, Debug)]
pub struct CoreRuntimeActivator {
  core: CoreHandle,
}

impl CoreRuntimeActivator {
  pub const fn new(core: CoreHandle) -> Self {
    Self { core }
  }
}

#[async_trait]
impl RuntimeActivator for CoreRuntimeActivator {
  async fn reload(&self, _runtime_path: &Path) -> Result<()> {
    self
      .core
      .reload()
      .await
      .map(|_| ())
      .map_err(|error| Error::RuntimeActivation(error.to_string()))
  }

  async fn restart(&self, _runtime_path: &Path) -> Result<()> {
    self
      .core
      .restart_current()
      .await
      .map(|_| ())
      .map_err(|error| Error::RuntimeActivation(error.to_string()))
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    path::Path,
    sync::{Arc, Mutex},
  };

  use async_trait::async_trait;
  use rsclash_config::RuntimeActivator as _;
  use rsclash_core::{ControllerError, CoreRuntime, LifecycleController, RunningCore};
  use rsclash_domain::{CoreChannel, CoreRunMode};

  use super::CoreRuntimeActivator;

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  enum Call {
    Start(CoreChannel),
    Stop,
    Reload,
  }

  struct FakeController {
    calls: Arc<Mutex<Vec<Call>>>,
  }

  impl FakeController {
    fn record(&self, call: Call) {
      self
        .calls
        .lock()
        .expect("call log lock should be available")
        .push(call);
    }

    fn running() -> RunningCore {
      RunningCore::new(CoreRunMode::Sidecar, Some("1.0.0".to_string()))
    }
  }

  #[async_trait]
  impl LifecycleController for FakeController {
    async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
      self.record(Call::Start(channel));
      Ok(Self::running())
    }

    async fn stop(&mut self) -> Result<(), ControllerError> {
      self.record(Call::Stop);
      Ok(())
    }

    async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
      self.record(Call::Reload);
      Ok(Self::running())
    }

    async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
      Ok(Self::running())
    }
  }

  #[tokio::test]
  async fn activation_routes_reload_and_restart_through_the_lifecycle_actor() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = CoreRuntime::spawn(
      &tokio::runtime::Handle::current(),
      FakeController {
        calls: Arc::clone(&calls),
      },
    );
    let core = runtime.handle();
    core
      .start(CoreChannel::Alpha)
      .await
      .expect("core should start");
    let activator = CoreRuntimeActivator::new(core);

    activator
      .reload(Path::new("/tmp/runtime.yaml"))
      .await
      .expect("reload should succeed");
    activator
      .restart(Path::new("/tmp/runtime.yaml"))
      .await
      .expect("restart should succeed");

    assert_eq!(
      *calls.lock().expect("call log lock should be available"),
      vec![
        Call::Start(CoreChannel::Alpha),
        Call::Reload,
        Call::Stop,
        Call::Start(CoreChannel::Alpha),
      ]
    );
    assert!(runtime.shutdown().await.is_ok());
  }
}
