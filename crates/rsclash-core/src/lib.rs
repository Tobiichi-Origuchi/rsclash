//! Serialized Mihomo lifecycle coordination.

use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use rsclash_domain::{CoreChannel, CoreRunMode, CoreState};
use thiserror::Error;
use tokio::{
  runtime::Handle,
  sync::{mpsc, oneshot, watch},
  task::JoinHandle,
  time::timeout,
};

const COMMAND_CAPACITY: usize = 16;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
  message: String,
}

impl ControllerError {
  pub fn new(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
    }
  }
}

#[async_trait]
pub trait LifecycleController: Send + 'static {
  async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError>;

  async fn stop(&mut self) -> Result<(), ControllerError>;

  async fn reload(&mut self) -> Result<RunningCore, ControllerError>;
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
    let initial_state = Arc::new(CoreState::Stopped);
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (state_tx, state_rx) = watch::channel(Arc::clone(&initial_state));
    let coordinator = Coordinator {
      controller,
      state: (*initial_state).clone(),
      active_channel: None,
      command_rx,
      state_tx,
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
}

impl<C> Coordinator<C>
where
  C: LifecycleController,
{
  async fn run(mut self) {
    while let Some(envelope) = self.command_rx.recv().await {
      let should_stop = matches!(envelope.command, LifecycleCommand::Shutdown);
      let result = self.handle_command(envelope.command).await;
      let _ = envelope.reply.send(result);
      if should_stop {
        return;
      }
    }

    let _ = self.stop(LifecycleOperation::Shutdown).await;
  }

  async fn handle_command(
    &mut self,
    command: LifecycleCommand,
  ) -> Result<CoreState, LifecycleError> {
    match command {
      LifecycleCommand::Start(channel) => self.start(channel, LifecycleOperation::Start).await,
      LifecycleCommand::Stop => self.stop(LifecycleOperation::Stop).await,
      LifecycleCommand::Restart(channel) => self.restart(channel).await,
      LifecycleCommand::Reload => self.reload().await,
      LifecycleCommand::Shutdown => self.stop(LifecycleOperation::Shutdown).await,
    }
  }

  async fn start(
    &mut self,
    channel: CoreChannel,
    operation: LifecycleOperation,
  ) -> Result<CoreState, LifecycleError> {
    match &self.state {
      CoreState::Stopped | CoreState::Failed { .. } => {},
      CoreState::Running {
        channel: active_channel,
        ..
      } if *active_channel == channel => return Ok(self.state.clone()),
      state => return Err(Self::invalid_transition(operation, state)),
    }

    self.publish(CoreState::Starting);
    match self.controller.start(channel).await {
      Ok(running) => {
        self.active_channel = Some(channel);
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

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    sync::{
      Arc, Mutex,
      atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
  };

  use async_trait::async_trait;
  use rsclash_domain::{CoreChannel, CoreRunMode, CoreState};
  use tokio::time::{sleep, timeout};

  use super::{
    ControllerError, CoreRuntime, LifecycleController, LifecycleError, LifecycleOperation,
    RunningCore,
  };

  #[derive(Clone, Debug, Eq, PartialEq)]
  enum Call {
    Start(CoreChannel),
    Stop,
    Reload,
  }

  #[derive(Default)]
  struct FakeState {
    calls: Mutex<Vec<Call>>,
    fail_next_start: AtomicBool,
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
      if self.state.fail_next_start.swap(false, Ordering::SeqCst) {
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
}
