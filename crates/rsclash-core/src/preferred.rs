use async_trait::async_trait;
use rsclash_domain::{CoreChannel, CoreRunMode};

use crate::{ControllerError, LifecycleController, RunningCore};

#[async_trait]
pub trait ServiceLifecycleController: LifecycleController {
  async fn is_available(&mut self) -> Result<bool, ControllerError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveBackend {
  Service,
  Sidecar,
}

enum ServiceAttempt {
  Started(RunningCore),
  FallBack(Option<String>),
}

pub struct PreferredController<C> {
  service: Option<Box<dyn ServiceLifecycleController>>,
  sidecar: C,
  active: Option<ActiveBackend>,
}

impl<C> PreferredController<C> {
  pub const fn new(sidecar: C) -> Self {
    Self {
      service: None,
      sidecar,
      active: None,
    }
  }

  #[must_use]
  pub fn with_service<S>(mut self, service: S) -> Self
  where
    S: ServiceLifecycleController,
  {
    self.service = Some(Box::new(service));
    self
  }
}

impl<C> PreferredController<C>
where
  C: LifecycleController,
{
  async fn try_service(&mut self, channel: CoreChannel) -> Result<ServiceAttempt, ControllerError> {
    let Some(service) = self.service.as_mut() else {
      return Ok(ServiceAttempt::FallBack(None));
    };
    let unavailable_reason = match service.is_available().await {
      Ok(true) => None,
      Ok(false) => Some("the privileged service is unavailable".to_string()),
      Err(error) => Some(format!("probe the privileged service: {error}")),
    };
    if let Some(reason) = unavailable_reason {
      return Ok(ServiceAttempt::FallBack(Some(reason)));
    }

    let failure = match service.start(channel).await {
      Ok(running) if running.mode == CoreRunMode::Service => {
        return Ok(ServiceAttempt::Started(running));
      },
      Ok(running) => format!(
        "the privileged service reported the wrong running mode: {:?}",
        running.mode
      ),
      Err(error) => format!("start through the privileged service: {error}"),
    };
    service.stop().await.map_err(|cleanup_error| {
      ControllerError::new(format!(
        "{failure}; refusing sidecar fallback because service cleanup failed: {cleanup_error}"
      ))
    })?;
    Ok(ServiceAttempt::FallBack(Some(failure)))
  }

  fn validate_running_mode(
    running: RunningCore,
    expected: CoreRunMode,
  ) -> Result<RunningCore, ControllerError> {
    if running.mode == expected {
      Ok(running)
    } else {
      Err(ControllerError::new(format!(
        "the active backend reported the wrong running mode: expected {expected:?}, got {:?}",
        running.mode
      )))
    }
  }

  fn no_active_backend(operation: &str) -> ControllerError {
    ControllerError::new(format!("cannot {operation}: no core backend is active"))
  }
}

#[async_trait]
impl<C> LifecycleController for PreferredController<C>
where
  C: LifecycleController,
{
  async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
    if self.active.is_some() {
      return Err(ControllerError::new("a core backend is already active"));
    }

    let fallback_reason = match self.try_service(channel).await? {
      ServiceAttempt::Started(running) => {
        self.active = Some(ActiveBackend::Service);
        return Ok(running);
      },
      ServiceAttempt::FallBack(reason) => reason,
    };
    match self.sidecar.start(channel).await {
      Ok(running) => {
        self.active = Some(ActiveBackend::Sidecar);
        Self::validate_running_mode(running, CoreRunMode::Sidecar)
      },
      Err(error) => match fallback_reason {
        Some(reason) => Err(ControllerError::new(format!(
          "{reason}; sidecar fallback also failed: {error}"
        ))),
        None => Err(error),
      },
    }
  }

  async fn stop(&mut self) -> Result<(), ControllerError> {
    let result = match self.active {
      Some(ActiveBackend::Service) => {
        self
          .service
          .as_mut()
          .ok_or_else(|| ControllerError::new("the active service backend is missing"))?
          .stop()
          .await
      },
      Some(ActiveBackend::Sidecar) => self.sidecar.stop().await,
      None => return Ok(()),
    };
    if result.is_ok() {
      self.active = None;
    }
    result
  }

  async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
    let (running, expected) = match self.active {
      Some(ActiveBackend::Service) => (
        self
          .service
          .as_mut()
          .ok_or_else(|| ControllerError::new("the active service backend is missing"))?
          .reload()
          .await?,
        CoreRunMode::Service,
      ),
      Some(ActiveBackend::Sidecar) => (self.sidecar.reload().await?, CoreRunMode::Sidecar),
      None => return Err(Self::no_active_backend("reload")),
    };
    Self::validate_running_mode(running, expected)
  }

  async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
    let (running, expected) = match self.active {
      Some(ActiveBackend::Service) => (
        self
          .service
          .as_mut()
          .ok_or_else(|| ControllerError::process_exited("the active service backend is missing"))?
          .health_check()
          .await?,
        CoreRunMode::Service,
      ),
      Some(ActiveBackend::Sidecar) => (self.sidecar.health_check().await?, CoreRunMode::Sidecar),
      None => {
        return Err(ControllerError::process_exited("no core backend is active"));
      },
    };
    Self::validate_running_mode(running, expected)
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  };

  use async_trait::async_trait;
  use rsclash_domain::{CoreChannel, CoreRunMode};

  use super::{PreferredController, ServiceLifecycleController};
  use crate::{ControllerError, LifecycleController, RunningCore};

  #[derive(Clone, Debug, Eq, PartialEq)]
  enum Call {
    Available(&'static str),
    Start(&'static str, CoreChannel),
    Stop(&'static str),
    Reload(&'static str),
    Health(&'static str),
  }

  struct FakeController {
    name: &'static str,
    mode: CoreRunMode,
    calls: Arc<Mutex<Vec<Call>>>,
    available: Arc<AtomicBool>,
    fail_start: bool,
    fail_stop: bool,
  }

  impl FakeController {
    fn new(
      name: &'static str,
      mode: CoreRunMode,
      calls: Arc<Mutex<Vec<Call>>>,
      available: Arc<AtomicBool>,
    ) -> Self {
      Self {
        name,
        mode,
        calls,
        available,
        fail_start: false,
        fail_stop: false,
      }
    }

    fn record(&self, call: Call) {
      self
        .calls
        .lock()
        .expect("call log lock should be available")
        .push(call);
    }

    fn running(&self) -> RunningCore {
      RunningCore::new(self.mode, Some(self.name.to_string()))
    }
  }

  #[async_trait]
  impl LifecycleController for FakeController {
    async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
      self.record(Call::Start(self.name, channel));
      if self.fail_start {
        Err(ControllerError::new("planned start failure"))
      } else {
        Ok(self.running())
      }
    }

    async fn stop(&mut self) -> Result<(), ControllerError> {
      self.record(Call::Stop(self.name));
      if self.fail_stop {
        Err(ControllerError::new("planned stop failure"))
      } else {
        Ok(())
      }
    }

    async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
      self.record(Call::Reload(self.name));
      Ok(self.running())
    }

    async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
      self.record(Call::Health(self.name));
      Ok(self.running())
    }
  }

  #[async_trait]
  impl ServiceLifecycleController for FakeController {
    async fn is_available(&mut self) -> Result<bool, ControllerError> {
      self.record(Call::Available(self.name));
      Ok(self.available.load(Ordering::SeqCst))
    }
  }

  fn controller(
    service: FakeController,
    sidecar: FakeController,
  ) -> PreferredController<FakeController> {
    PreferredController::new(sidecar).with_service(service)
  }

  #[tokio::test]
  async fn prefers_service_and_routes_active_operations() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let available = Arc::new(AtomicBool::new(true));
    let service = FakeController::new(
      "service",
      CoreRunMode::Service,
      Arc::clone(&calls),
      Arc::clone(&available),
    );
    let sidecar = FakeController::new(
      "sidecar",
      CoreRunMode::Sidecar,
      Arc::clone(&calls),
      Arc::new(AtomicBool::new(false)),
    );
    let mut controller = controller(service, sidecar);

    let running = controller
      .start(CoreChannel::Stable)
      .await
      .expect("service startup should succeed");
    assert_eq!(running.mode, CoreRunMode::Service);
    assert!(controller.reload().await.is_ok());
    assert!(controller.health_check().await.is_ok());
    assert!(controller.stop().await.is_ok());
    assert_eq!(
      *calls.lock().expect("call log lock should be available"),
      vec![
        Call::Available("service"),
        Call::Start("service", CoreChannel::Stable),
        Call::Reload("service"),
        Call::Health("service"),
        Call::Stop("service"),
      ]
    );
  }

  #[tokio::test]
  async fn falls_back_and_switches_safely_on_the_next_start() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let available = Arc::new(AtomicBool::new(false));
    let service = FakeController::new(
      "service",
      CoreRunMode::Service,
      Arc::clone(&calls),
      Arc::clone(&available),
    );
    let sidecar = FakeController::new(
      "sidecar",
      CoreRunMode::Sidecar,
      Arc::clone(&calls),
      Arc::new(AtomicBool::new(false)),
    );
    let mut controller = controller(service, sidecar);

    let running = controller
      .start(CoreChannel::Alpha)
      .await
      .expect("sidecar fallback should succeed");
    assert_eq!(running.mode, CoreRunMode::Sidecar);
    available.store(true, Ordering::SeqCst);
    assert!(controller.stop().await.is_ok());
    let running = controller
      .start(CoreChannel::Alpha)
      .await
      .expect("service should be selected after a clean stop");
    assert_eq!(running.mode, CoreRunMode::Service);
    assert_eq!(
      *calls.lock().expect("call log lock should be available"),
      vec![
        Call::Available("service"),
        Call::Start("sidecar", CoreChannel::Alpha),
        Call::Stop("sidecar"),
        Call::Available("service"),
        Call::Start("service", CoreChannel::Alpha),
      ]
    );
  }

  #[tokio::test]
  async fn cleans_up_a_failed_service_before_falling_back() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let available = Arc::new(AtomicBool::new(true));
    let mut service = FakeController::new(
      "service",
      CoreRunMode::Service,
      Arc::clone(&calls),
      available,
    );
    service.fail_start = true;
    let sidecar = FakeController::new(
      "sidecar",
      CoreRunMode::Sidecar,
      Arc::clone(&calls),
      Arc::new(AtomicBool::new(false)),
    );
    let mut controller = controller(service, sidecar);

    assert!(controller.start(CoreChannel::Stable).await.is_ok());
    assert_eq!(
      *calls.lock().expect("call log lock should be available"),
      vec![
        Call::Available("service"),
        Call::Start("service", CoreChannel::Stable),
        Call::Stop("service"),
        Call::Start("sidecar", CoreChannel::Stable),
      ]
    );
  }

  #[tokio::test]
  async fn refuses_fallback_when_service_cleanup_fails() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let available = Arc::new(AtomicBool::new(true));
    let mut service = FakeController::new(
      "service",
      CoreRunMode::Service,
      Arc::clone(&calls),
      available,
    );
    service.fail_start = true;
    service.fail_stop = true;
    let sidecar = FakeController::new(
      "sidecar",
      CoreRunMode::Sidecar,
      Arc::clone(&calls),
      Arc::new(AtomicBool::new(false)),
    );
    let mut controller = controller(service, sidecar);

    let error = controller
      .start(CoreChannel::Stable)
      .await
      .expect_err("unsafe fallback should be rejected");
    assert!(error.to_string().contains("refusing sidecar fallback"));
    assert_eq!(
      *calls.lock().expect("call log lock should be available"),
      vec![
        Call::Available("service"),
        Call::Start("service", CoreChannel::Stable),
        Call::Stop("service"),
      ]
    );
  }
}
