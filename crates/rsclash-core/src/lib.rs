//! Serialized Mihomo lifecycle coordination.

#[cfg(target_os = "linux")]
mod linux;
mod preferred;

use std::{fmt, future::pending, sync::Arc, time::Duration};

use async_trait::async_trait;
use rsclash_domain::{CoreChannel, CoreRunMode, CoreState};
use thiserror::Error;
use tokio::{
  runtime::Handle,
  sync::{mpsc, oneshot, watch},
  task::JoinHandle,
  time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout},
};

#[cfg(target_os = "linux")]
pub use linux::{
  CoreBinaries, CoreLogEntry, CoreLogStore, CoreOutputStream, LinuxSidecarConfig,
  LinuxSidecarController,
};
pub use preferred::{PreferredController, ServiceLifecycleController};

const COMMAND_CAPACITY: usize = 16;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisionConfig {
  pub health_interval: Duration,
  pub max_consecutive_health_failures: u8,
  pub initial_restart_delay: Duration,
  pub max_restart_delay: Duration,
  pub max_restart_attempts: u8,
  pub stable_reset_after: Duration,
}

impl Default for SupervisionConfig {
  fn default() -> Self {
    Self {
      health_interval: Duration::from_secs(5),
      max_consecutive_health_failures: 3,
      initial_restart_delay: Duration::from_secs(1),
      max_restart_delay: Duration::from_secs(30),
      max_restart_attempts: 5,
      stable_reset_after: Duration::from_secs(60),
    }
  }
}

impl SupervisionConfig {
  fn validate(self) -> Result<Self, SupervisionConfigError> {
    if self.health_interval.is_zero() {
      return Err(SupervisionConfigError::ZeroHealthInterval);
    }
    if self.max_consecutive_health_failures == 0 {
      return Err(SupervisionConfigError::ZeroHealthFailures);
    }
    if self.initial_restart_delay.is_zero() {
      return Err(SupervisionConfigError::ZeroRestartDelay);
    }
    if self.max_restart_delay < self.initial_restart_delay {
      return Err(SupervisionConfigError::InvalidMaximumRestartDelay);
    }
    if self.stable_reset_after.is_zero() {
      return Err(SupervisionConfigError::ZeroStableReset);
    }
    Ok(self)
  }

  fn restart_delay(self, attempt: u8) -> Duration {
    let exponent = u32::from(attempt.saturating_sub(1).min(31));
    self
      .initial_restart_delay
      .saturating_mul(1_u32 << exponent)
      .min(self.max_restart_delay)
  }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SupervisionConfigError {
  #[error("the health interval must not be zero")]
  ZeroHealthInterval,
  #[error("at least one consecutive health failure must be allowed")]
  ZeroHealthFailures,
  #[error("the initial restart delay must not be zero")]
  ZeroRestartDelay,
  #[error("the maximum restart delay must be at least the initial delay")]
  InvalidMaximumRestartDelay,
  #[error("the stable restart-budget reset duration must not be zero")]
  ZeroStableReset,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunningCore {
  pub mode: CoreRunMode,
  pub version: Option<String>,
}

impl RunningCore {
  pub const fn new(mode: CoreRunMode, version: Option<String>) -> Self {
    Self { mode, version }
  }

  fn into_state(self, channel: CoreChannel) -> CoreState {
    CoreState::Running {
      mode: self.mode,
      channel,
      version: self.version,
    }
  }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct ControllerError {
  kind: ControllerErrorKind,
  message: String,
}

impl ControllerError {
  pub fn new(message: impl Into<String>) -> Self {
    Self {
      kind: ControllerErrorKind::Operation,
      message: message.into(),
    }
  }

  pub fn unhealthy(message: impl Into<String>) -> Self {
    Self {
      kind: ControllerErrorKind::Unhealthy,
      message: message.into(),
    }
  }

  pub fn process_exited(message: impl Into<String>) -> Self {
    Self {
      kind: ControllerErrorKind::ProcessExited,
      message: message.into(),
    }
  }

  pub const fn kind(&self) -> ControllerErrorKind {
    self.kind
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerErrorKind {
  Operation,
  Unhealthy,
  ProcessExited,
}

#[async_trait]
pub trait LifecycleController: Send + 'static {
  async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError>;

  async fn stop(&mut self) -> Result<(), ControllerError>;

  async fn reload(&mut self) -> Result<RunningCore, ControllerError>;

  async fn health_check(&mut self) -> Result<RunningCore, ControllerError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleOperation {
  Start,
  Stop,
  Restart,
  Reload,
  Shutdown,
}

impl fmt::Display for LifecycleOperation {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(match self {
      Self::Start => "start",
      Self::Stop => "stop",
      Self::Restart => "restart",
      Self::Reload => "reload",
      Self::Shutdown => "shut down",
    })
  }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LifecycleError {
  #[error("the core lifecycle coordinator is closed")]
  CoordinatorClosed,
  #[error("the core lifecycle coordinator dropped a response")]
  ResponseDropped,
  #[error("cannot {operation} the core while it is in state {state:?}")]
  InvalidTransition {
    operation: LifecycleOperation,
    state: CoreState,
  },
  #[error("failed to {operation} the core: {message}")]
  Controller {
    operation: LifecycleOperation,
    message: String,
  },
}

#[derive(Debug, Error)]
pub enum RuntimeError {
  #[error(transparent)]
  Lifecycle(#[from] LifecycleError),
  #[error("the core lifecycle coordinator did not stop within the timeout")]
  ShutdownTimedOut,
  #[error("the core lifecycle coordinator task failed: {0}")]
  TaskFailed(String),
}

enum LifecycleCommand {
  Start(CoreChannel),
  Stop,
  Restart(CoreChannel),
  RestartCurrent,
  Reload,
  Shutdown,
}

struct CommandEnvelope {
  command: LifecycleCommand,
  reply: oneshot::Sender<Result<CoreState, LifecycleError>>,
}

#[derive(Clone)]
pub struct CoreHandle {
  command_tx: mpsc::Sender<CommandEnvelope>,
  state_rx: watch::Receiver<Arc<CoreState>>,
}

impl CoreHandle {
  pub fn current_state(&self) -> Arc<CoreState> {
    self.state_rx.borrow().clone()
  }

  pub async fn changed(&mut self) -> Result<Arc<CoreState>, LifecycleError> {
    self
      .state_rx
      .changed()
      .await
      .map_err(|_| LifecycleError::CoordinatorClosed)?;
    Ok(self.state_rx.borrow_and_update().clone())
  }

  pub async fn start(&self, channel: CoreChannel) -> Result<CoreState, LifecycleError> {
    self.request(LifecycleCommand::Start(channel)).await
  }

  pub async fn stop(&self) -> Result<CoreState, LifecycleError> {
    self.request(LifecycleCommand::Stop).await
  }

  pub async fn restart(&self, channel: CoreChannel) -> Result<CoreState, LifecycleError> {
    self.request(LifecycleCommand::Restart(channel)).await
  }

  pub async fn restart_current(&self) -> Result<CoreState, LifecycleError> {
    self.request(LifecycleCommand::RestartCurrent).await
  }

  pub async fn reload(&self) -> Result<CoreState, LifecycleError> {
    self.request(LifecycleCommand::Reload).await
  }

  async fn shutdown(&self) -> Result<CoreState, LifecycleError> {
    self.request(LifecycleCommand::Shutdown).await
  }

  async fn request(&self, command: LifecycleCommand) -> Result<CoreState, LifecycleError> {
    let (reply, response) = oneshot::channel();
    self
      .command_tx
      .send(CommandEnvelope { command, reply })
      .await
      .map_err(|_| LifecycleError::CoordinatorClosed)?;
    response
      .await
      .map_err(|_| LifecycleError::ResponseDropped)?
  }
}

impl fmt::Debug for CoreHandle {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("CoreHandle")
      .field("state", &self.current_state())
      .finish_non_exhaustive()
  }
}

pub struct CoreRuntime {
  handle: CoreHandle,
  coordinator: Option<JoinHandle<()>>,
}

impl CoreRuntime {
  pub fn spawn<C>(runtime: &Handle, controller: C) -> Self
  where
    C: LifecycleController,
  {
    Self::spawn_inner(runtime, controller, SupervisionConfig::default())
  }

  pub fn spawn_with_config<C>(
    runtime: &Handle,
    controller: C,
    supervision: SupervisionConfig,
  ) -> Result<Self, SupervisionConfigError>
  where
    C: LifecycleController,
  {
    Ok(Self::spawn_inner(
      runtime,
      controller,
      supervision.validate()?,
    ))
  }

  fn spawn_inner<C>(runtime: &Handle, controller: C, supervision: SupervisionConfig) -> Self
  where
    C: LifecycleController,
  {
    let initial_state = Arc::new(CoreState::Stopped);
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (state_tx, state_rx) = watch::channel(Arc::clone(&initial_state));
    let coordinator = Coordinator {
      controller,
      state: (*initial_state).clone(),
      active_channel: None,
      command_rx,
      state_tx,
      supervision,
      consecutive_health_failures: 0,
      restart_attempts: 0,
      restart_at: None,
      started_at: None,
    };

    Self {
      handle: CoreHandle {
        command_tx,
        state_rx,
      },
      coordinator: Some(runtime.spawn(coordinator.run())),
    }
  }

  pub fn handle(&self) -> CoreHandle {
    self.handle.clone()
  }

  pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
    let lifecycle_result = self.handle.shutdown().await;
    let task_result = if let Some(mut coordinator) = self.coordinator.take() {
      match timeout(SHUTDOWN_TIMEOUT, &mut coordinator).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(RuntimeError::TaskFailed(error.to_string())),
        Err(_) => {
          coordinator.abort();
          let _ = coordinator.await;
          Err(RuntimeError::ShutdownTimedOut)
        },
      }
    } else {
      Ok(())
    };

    task_result?;
    lifecycle_result.map(|_| ()).map_err(RuntimeError::from)
  }
}

impl Drop for CoreRuntime {
  fn drop(&mut self) {
    if let Some(coordinator) = self.coordinator.take() {
      coordinator.abort();
    }
  }
}

struct Coordinator<C> {
  controller: C,
  state: CoreState,
  active_channel: Option<CoreChannel>,
  command_rx: mpsc::Receiver<CommandEnvelope>,
  state_tx: watch::Sender<Arc<CoreState>>,
  supervision: SupervisionConfig,
  consecutive_health_failures: u8,
  restart_attempts: u8,
  restart_at: Option<Instant>,
  started_at: Option<Instant>,
}

impl<C> Coordinator<C>
where
  C: LifecycleController,
{
  async fn run(mut self) {
    let first_health_check = Instant::now() + self.supervision.health_interval;
    let mut health_timer = interval_at(first_health_check, self.supervision.health_interval);
    health_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
      let restart_at = self.restart_at;
      tokio::select! {
        biased;
        envelope = self.command_rx.recv() => {
          let Some(envelope) = envelope else {
            break;
          };
          let should_stop = matches!(envelope.command, LifecycleCommand::Shutdown);
          let result = self.handle_command(envelope.command).await;
          let _ = envelope.reply.send(result);
          if should_stop {
            return;
          }
        },
        () = wait_for_restart(restart_at), if restart_at.is_some() => {
          self.run_scheduled_restart().await;
        },
        _ = health_timer.tick() => {
          self.supervise().await;
        },
      }
    }

    self.restart_at = None;
    let _ = self.stop(LifecycleOperation::Shutdown).await;
  }

  async fn handle_command(
    &mut self,
    command: LifecycleCommand,
  ) -> Result<CoreState, LifecycleError> {
    match command {
      LifecycleCommand::Start(channel) => {
        self.reset_supervision();
        self.start(channel, LifecycleOperation::Start).await
      },
      LifecycleCommand::Stop => {
        self.reset_supervision();
        self.stop(LifecycleOperation::Stop).await
      },
      LifecycleCommand::Restart(channel) => {
        self.reset_supervision();
        self.restart(channel).await
      },
      LifecycleCommand::RestartCurrent => {
        let channel = self
          .active_channel
          .ok_or_else(|| Self::invalid_transition(LifecycleOperation::Restart, &self.state))?;
        self.reset_supervision();
        self.restart(channel).await
      },
      LifecycleCommand::Reload => self.reload().await,
      LifecycleCommand::Shutdown => {
        self.reset_supervision();
        self.stop(LifecycleOperation::Shutdown).await
      },
    }
  }

  async fn start(
    &mut self,
    channel: CoreChannel,
    operation: LifecycleOperation,
  ) -> Result<CoreState, LifecycleError> {
    let needs_cleanup = matches!(self.state, CoreState::Failed { .. });
    match &self.state {
      CoreState::Stopped | CoreState::Failed { .. } => {},
      CoreState::Running {
        channel: active_channel,
        ..
      } if *active_channel == channel => return Ok(self.state.clone()),
      state => return Err(Self::invalid_transition(operation, state)),
    }

    if needs_cleanup {
      self
        .controller
        .stop()
        .await
        .map_err(|error| self.fail(operation, error))?;
      self.active_channel = None;
    }
    self.publish(CoreState::Starting);
    match self.controller.start(channel).await {
      Ok(running) => {
        self.active_channel = Some(channel);
        self.mark_started();
        self.publish(running.into_state(channel));
        Ok(self.state.clone())
      },
      Err(error) => Err(self.fail(operation, error)),
    }
  }

  async fn stop(&mut self, operation: LifecycleOperation) -> Result<CoreState, LifecycleError> {
    match self.state {
      CoreState::Stopped => return Ok(self.state.clone()),
      CoreState::Running { .. } | CoreState::Failed { .. } => {},
      _ => return Err(Self::invalid_transition(operation, &self.state)),
    }

    self.publish(CoreState::Stopping);
    match self.controller.stop().await {
      Ok(()) => {
        self.active_channel = None;
        self.started_at = None;
        self.consecutive_health_failures = 0;
        self.publish(CoreState::Stopped);
        Ok(self.state.clone())
      },
      Err(error) => Err(self.fail(operation, error)),
    }
  }

  async fn restart(&mut self, channel: CoreChannel) -> Result<CoreState, LifecycleError> {
    if !matches!(self.state, CoreState::Stopped) {
      self.stop(LifecycleOperation::Restart).await?;
    }
    self.start(channel, LifecycleOperation::Restart).await
  }

  async fn reload(&mut self) -> Result<CoreState, LifecycleError> {
    let channel = match (&self.state, self.active_channel) {
      (CoreState::Running { channel, .. }, Some(active_channel)) if *channel == active_channel => {
        active_channel
      },
      (state, _) => return Err(Self::invalid_transition(LifecycleOperation::Reload, state)),
    };

    self.publish(CoreState::Reloading);
    match self.controller.reload().await {
      Ok(running) => {
        self.publish(running.into_state(channel));
        Ok(self.state.clone())
      },
      Err(error) => Err(self.fail(LifecycleOperation::Reload, error)),
    }
  }

  async fn supervise(&mut self) {
    let CoreState::Running { channel, .. } = self.state else {
      return;
    };
    match self.controller.health_check().await {
      Ok(running) => self.record_healthy(channel, running),
      Err(error) => self.record_health_failure(channel, error).await,
    }
  }

  fn record_healthy(&mut self, channel: CoreChannel, running: RunningCore) {
    self.consecutive_health_failures = 0;
    if self
      .started_at
      .is_some_and(|started| started.elapsed() >= self.supervision.stable_reset_after)
    {
      self.restart_attempts = 0;
    }
    let state = running.into_state(channel);
    if self.state != state {
      self.publish(state);
    }
  }

  async fn record_health_failure(&mut self, channel: CoreChannel, error: ControllerError) {
    self.consecutive_health_failures = self.consecutive_health_failures.saturating_add(1);
    let should_restart = error.kind() == ControllerErrorKind::ProcessExited
      || self.consecutive_health_failures >= self.supervision.max_consecutive_health_failures;
    if !should_restart {
      return;
    }

    let mut message = error.to_string();
    if let Err(cleanup_error) = self.controller.stop().await {
      message = format!("{message}; cleanup failed: {cleanup_error}");
    }
    self.started_at = None;
    self.consecutive_health_failures = 0;
    self.active_channel = Some(channel);
    self.schedule_restart(channel, message);
  }

  async fn run_scheduled_restart(&mut self) {
    let Some(channel) = self.active_channel else {
      self.restart_at = None;
      return;
    };
    self.restart_at = None;
    if let Err(error) = self.controller.stop().await {
      self.schedule_restart(
        channel,
        format!("automatic restart cleanup failed: {error}"),
      );
      return;
    }
    self.publish(CoreState::Starting);
    match self.controller.start(channel).await {
      Ok(running) => {
        self.mark_started();
        self.publish(running.into_state(channel));
      },
      Err(error) => self.schedule_restart(channel, error.to_string()),
    }
  }

  fn schedule_restart(&mut self, channel: CoreChannel, cause: String) {
    if self.restart_attempts >= self.supervision.max_restart_attempts {
      self.restart_at = None;
      self.publish(CoreState::Failed {
        message: format!(
          "{cause}; automatic restart exhausted after {} attempts",
          self.restart_attempts
        ),
      });
      return;
    }

    self.restart_attempts = self.restart_attempts.saturating_add(1);
    let delay = self.supervision.restart_delay(self.restart_attempts);
    self.active_channel = Some(channel);
    self.restart_at = Some(Instant::now() + delay);
    self.publish(CoreState::Failed {
      message: format!(
        "{cause}; automatic restart {}/{} in {delay:?}",
        self.restart_attempts, self.supervision.max_restart_attempts
      ),
    });
  }

  fn mark_started(&mut self) {
    self.started_at = Some(Instant::now());
    self.consecutive_health_failures = 0;
  }

  const fn reset_supervision(&mut self) {
    self.restart_at = None;
    self.restart_attempts = 0;
    self.consecutive_health_failures = 0;
    self.started_at = None;
  }

  fn publish(&mut self, state: CoreState) {
    self.state = state;
    self.state_tx.send_replace(Arc::new(self.state.clone()));
  }

  fn fail(&mut self, operation: LifecycleOperation, error: ControllerError) -> LifecycleError {
    let message = error.to_string();
    self.publish(CoreState::Failed {
      message: message.clone(),
    });
    LifecycleError::Controller { operation, message }
  }

  fn invalid_transition(operation: LifecycleOperation, state: &CoreState) -> LifecycleError {
    LifecycleError::InvalidTransition {
      operation,
      state: state.clone(),
    }
  }
}

async fn wait_for_restart(deadline: Option<Instant>) {
  if let Some(deadline) = deadline {
    sleep_until(deadline).await;
  } else {
    pending::<()>().await;
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    sync::{
      Arc, Mutex,
      atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
  };

  use async_trait::async_trait;
  use rsclash_domain::{CoreChannel, CoreRunMode, CoreState};
  use tokio::time::{sleep, timeout};

  use super::{
    ControllerError, CoreRuntime, LifecycleController, LifecycleError, LifecycleOperation,
    RunningCore, SupervisionConfig,
  };

  #[derive(Clone, Debug, Eq, PartialEq)]
  enum Call {
    Start(CoreChannel),
    Stop,
    Reload,
    Health,
  }

  #[derive(Default)]
  struct FakeState {
    calls: Mutex<Vec<Call>>,
    fail_next_start: AtomicBool,
    fail_start: AtomicBool,
    health_failure: AtomicU8,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
  }

  struct FakeController {
    state: Arc<FakeState>,
    delay: Duration,
  }

  impl FakeController {
    fn new(state: Arc<FakeState>, delay: Duration) -> Self {
      Self { state, delay }
    }

    async fn record(&self, call: Call) {
      let in_flight = self.state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
      self
        .state
        .max_in_flight
        .fetch_max(in_flight, Ordering::SeqCst);
      sleep(self.delay).await;
      self
        .state
        .calls
        .lock()
        .expect("call log lock should be available")
        .push(call);
      self.state.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
  }

  #[async_trait]
  impl LifecycleController for FakeController {
    async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
      self.record(Call::Start(channel)).await;
      if self.state.fail_start.load(Ordering::SeqCst)
        || self.state.fail_next_start.swap(false, Ordering::SeqCst)
      {
        Err(ControllerError::new("planned start failure"))
      } else {
        Ok(RunningCore::new(
          CoreRunMode::Sidecar,
          Some("1.0.0".to_string()),
        ))
      }
    }

    async fn stop(&mut self) -> Result<(), ControllerError> {
      self.record(Call::Stop).await;
      Ok(())
    }

    async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
      self.record(Call::Reload).await;
      Ok(RunningCore::new(
        CoreRunMode::Sidecar,
        Some("1.0.1".to_string()),
      ))
    }

    async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
      self.record(Call::Health).await;
      match self.state.health_failure.load(Ordering::SeqCst) {
        1 => Err(ControllerError::unhealthy("planned health failure")),
        2 => Err(ControllerError::process_exited("planned process exit")),
        _ => Ok(RunningCore::new(
          CoreRunMode::Sidecar,
          Some("1.0.1".to_string()),
        )),
      }
    }
  }

  fn spawn(state: Arc<FakeState>, delay: Duration) -> CoreRuntime {
    CoreRuntime::spawn(
      &tokio::runtime::Handle::current(),
      FakeController::new(state, delay),
    )
  }

  #[tokio::test]
  async fn publishes_transitions_and_runs_the_full_lifecycle() {
    let state = Arc::new(FakeState::default());
    let runtime = spawn(Arc::clone(&state), Duration::from_millis(20));
    let handle = runtime.handle();
    let mut observer = handle.clone();

    let start_task = tokio::spawn(async move { handle.start(CoreChannel::Stable).await });
    timeout(Duration::from_secs(1), observer.changed())
      .await
      .expect("starting state should arrive before the timeout")
      .expect("state channel should remain open");
    assert_eq!(*observer.current_state(), CoreState::Starting);

    let running = start_task
      .await
      .expect("start task should finish")
      .expect("start should succeed");
    assert_eq!(
      running,
      CoreState::Running {
        mode: CoreRunMode::Sidecar,
        channel: CoreChannel::Stable,
        version: Some("1.0.0".to_string()),
      }
    );

    let reloaded = observer.reload().await.expect("reload should succeed");
    assert!(matches!(
      reloaded,
      CoreState::Running {
        version: Some(version),
        ..
      } if version == "1.0.1"
    ));
    let restarted = observer
      .restart(CoreChannel::Alpha)
      .await
      .expect("restart should succeed");
    assert!(matches!(
      restarted,
      CoreState::Running {
        channel: CoreChannel::Alpha,
        ..
      }
    ));
    assert_eq!(observer.stop().await.ok(), Some(CoreState::Stopped));

    assert_eq!(
      *state
        .calls
        .lock()
        .expect("call log lock should be available"),
      vec![
        Call::Start(CoreChannel::Stable),
        Call::Reload,
        Call::Stop,
        Call::Start(CoreChannel::Alpha),
        Call::Stop,
      ]
    );
    assert!(runtime.shutdown().await.is_ok());
  }

  #[tokio::test]
  async fn serializes_concurrent_requests() {
    let state = Arc::new(FakeState::default());
    let runtime = spawn(Arc::clone(&state), Duration::from_millis(20));
    let handle = runtime.handle();
    let first = handle.clone();
    let second = handle.clone();

    let start = tokio::spawn(async move { first.start(CoreChannel::Stable).await });
    sleep(Duration::from_millis(1)).await;
    let reload = tokio::spawn(async move { second.reload().await });
    assert!(start.await.expect("start task should finish").is_ok());
    assert!(reload.await.expect("reload task should finish").is_ok());
    assert_eq!(state.max_in_flight.load(Ordering::SeqCst), 1);

    assert!(runtime.shutdown().await.is_ok());
  }

  #[tokio::test]
  async fn rejects_invalid_transitions_and_can_recover_from_failure() {
    let state = Arc::new(FakeState::default());
    state.fail_next_start.store(true, Ordering::SeqCst);
    let runtime = spawn(Arc::clone(&state), Duration::ZERO);
    let handle = runtime.handle();

    assert_eq!(
      handle.reload().await,
      Err(LifecycleError::InvalidTransition {
        operation: LifecycleOperation::Reload,
        state: CoreState::Stopped,
      })
    );
    assert!(matches!(
      handle.start(CoreChannel::Stable).await,
      Err(LifecycleError::Controller {
        operation: LifecycleOperation::Start,
        ..
      })
    ));
    assert!(matches!(
      handle.current_state().as_ref(),
      CoreState::Failed { .. }
    ));

    assert!(handle.start(CoreChannel::Alpha).await.is_ok());
    assert!(matches!(
      handle.current_state().as_ref(),
      CoreState::Running {
        channel: CoreChannel::Alpha,
        ..
      }
    ));
    assert!(runtime.shutdown().await.is_ok());
  }

  fn test_supervision(max_restart_attempts: u8) -> SupervisionConfig {
    SupervisionConfig {
      health_interval: Duration::from_secs(1),
      max_consecutive_health_failures: 2,
      initial_restart_delay: Duration::from_secs(1),
      max_restart_delay: Duration::from_secs(4),
      max_restart_attempts,
      stable_reset_after: Duration::from_secs(60),
    }
  }

  async fn settle_tasks() {
    for _ in 0..4 {
      tokio::task::yield_now().await;
    }
  }

  #[tokio::test(start_paused = true)]
  async fn restarts_an_exited_core_and_stop_cancels_supervision() {
    let state = Arc::new(FakeState::default());
    let runtime = CoreRuntime::spawn_with_config(
      &tokio::runtime::Handle::current(),
      FakeController::new(Arc::clone(&state), Duration::ZERO),
      test_supervision(3),
    )
    .expect("supervision config should be valid");
    let handle = runtime.handle();
    assert!(handle.start(CoreChannel::Stable).await.is_ok());

    state.health_failure.store(2, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle_tasks().await;
    assert!(matches!(
      handle.current_state().as_ref(),
      CoreState::Failed { message } if message.contains("automatic restart 1/3")
    ));

    state.health_failure.store(0, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle_tasks().await;
    assert!(matches!(
      handle.current_state().as_ref(),
      CoreState::Running {
        channel: CoreChannel::Stable,
        ..
      }
    ));

    assert!(handle.stop().await.is_ok());
    let starts_before = state
      .calls
      .lock()
      .expect("call log lock should be available")
      .iter()
      .filter(|call| matches!(call, Call::Start(_)))
      .count();
    state.health_failure.store(2, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(10)).await;
    settle_tasks().await;
    let starts_after = state
      .calls
      .lock()
      .expect("call log lock should be available")
      .iter()
      .filter(|call| matches!(call, Call::Start(_)))
      .count();
    assert_eq!(starts_before, 2);
    assert_eq!(starts_after, starts_before);
    assert!(runtime.shutdown().await.is_ok());
  }

  #[tokio::test(start_paused = true)]
  async fn exhausts_bounded_backoff_and_allows_manual_recovery() {
    let state = Arc::new(FakeState::default());
    let runtime = CoreRuntime::spawn_with_config(
      &tokio::runtime::Handle::current(),
      FakeController::new(Arc::clone(&state), Duration::ZERO),
      test_supervision(2),
    )
    .expect("supervision config should be valid");
    let handle = runtime.handle();
    assert!(handle.start(CoreChannel::Alpha).await.is_ok());

    state.health_failure.store(2, Ordering::SeqCst);
    state.fail_start.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle_tasks().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle_tasks().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle_tasks().await;
    assert!(matches!(
      handle.current_state().as_ref(),
      CoreState::Failed { message }
        if message.contains("automatic restart exhausted after 2 attempts")
    ));

    let starts = state
      .calls
      .lock()
      .expect("call log lock should be available")
      .iter()
      .filter(|call| matches!(call, Call::Start(_)))
      .count();
    assert_eq!(starts, 3);

    state.fail_start.store(false, Ordering::SeqCst);
    state.health_failure.store(0, Ordering::SeqCst);
    assert!(handle.start(CoreChannel::Alpha).await.is_ok());
    assert!(matches!(
      handle.current_state().as_ref(),
      CoreState::Running { .. }
    ));
    assert!(runtime.shutdown().await.is_ok());
  }
}
