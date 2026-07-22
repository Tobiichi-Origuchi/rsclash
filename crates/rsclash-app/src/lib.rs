//! Asynchronous application coordinator and UI-facing client.

mod change;
mod runtime;

use std::{fmt, sync::Arc, time::Duration};

use rsclash_domain::{
  AppEvent, AppSnapshot, AppStatus, CommandError, CommandOutput, CommandResult, UiCommand,
};
use thiserror::Error;
use tokio::{
  runtime::Handle,
  sync::{broadcast, mpsc, oneshot, watch},
  task::JoinHandle,
  time::timeout,
};
use tracing::{debug, info};

pub use change::{
  ChangeAction, ChangeReceipt, CompensationFailure, PreparedChange, SideEffectError,
  SideEffectTransaction,
};
pub use runtime::MihomoRuntimeActivator;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 64;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct WakeHandle(Arc<dyn Fn() + Send + Sync>);

impl WakeHandle {
  pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
    Self(Arc::new(wake))
  }

  fn wake(&self) {
    (self.0)();
  }
}

impl Default for WakeHandle {
  fn default() -> Self {
    Self::new(|| {})
  }
}

impl fmt::Debug for WakeHandle {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("WakeHandle(..)")
  }
}

struct CommandEnvelope {
  command: UiCommand,
  reply: Option<oneshot::Sender<CommandResult>>,
}

#[derive(Clone)]
pub struct AppClient {
  command_tx: mpsc::Sender<CommandEnvelope>,
  snapshot_rx: watch::Receiver<Arc<AppSnapshot>>,
  event_tx: broadcast::Sender<AppEvent>,
}

impl AppClient {
  pub fn current_snapshot(&self) -> Arc<AppSnapshot> {
    self.snapshot_rx.borrow().clone()
  }

  pub fn take_snapshot_if_changed(&mut self) -> Option<Arc<AppSnapshot>> {
    match self.snapshot_rx.has_changed() {
      Ok(true) => Some(self.snapshot_rx.borrow_and_update().clone()),
      Ok(false) | Err(_) => None,
    }
  }

  pub async fn changed(&mut self) -> Result<Arc<AppSnapshot>, ClientError> {
    self
      .snapshot_rx
      .changed()
      .await
      .map_err(|_| ClientError::CoordinatorClosed)?;
    Ok(self.snapshot_rx.borrow_and_update().clone())
  }

  pub fn subscribe_events(&self) -> broadcast::Receiver<AppEvent> {
    self.event_tx.subscribe()
  }

  pub fn try_command(&self, command: UiCommand) -> Result<(), ClientError> {
    self
      .command_tx
      .try_send(CommandEnvelope {
        command,
        reply: None,
      })
      .map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => ClientError::CommandQueueFull,
        mpsc::error::TrySendError::Closed(_) => ClientError::CoordinatorClosed,
      })
  }

  pub async fn request(&self, command: UiCommand) -> Result<CommandOutput, ClientError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    self
      .command_tx
      .send(CommandEnvelope {
        command,
        reply: Some(reply_tx),
      })
      .await
      .map_err(|_| ClientError::CoordinatorClosed)?;

    reply_rx
      .await
      .map_err(|_| ClientError::ResponseDropped)?
      .map_err(ClientError::CommandRejected)
  }
}

impl fmt::Debug for AppClient {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("AppClient")
      .field("snapshot", &self.current_snapshot())
      .finish_non_exhaustive()
  }
}

#[derive(Debug, Error)]
pub enum ClientError {
  #[error("the application command queue is full")]
  CommandQueueFull,
  #[error("the application coordinator is closed")]
  CoordinatorClosed,
  #[error("the application coordinator dropped a command response")]
  ResponseDropped,
  #[error(transparent)]
  CommandRejected(CommandError),
}

#[derive(Debug, Error)]
pub enum BackendError {
  #[error("the application coordinator did not stop within the timeout")]
  ShutdownTimedOut,
  #[error("the application coordinator task failed: {0}")]
  TaskFailed(String),
}

pub struct BackendHandle {
  client: AppClient,
  coordinator: Option<JoinHandle<()>>,
}

impl BackendHandle {
  pub fn spawn(runtime: &Handle, wake: WakeHandle) -> Self {
    let initial_snapshot = Arc::new(AppSnapshot::default());
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (snapshot_tx, snapshot_rx) = watch::channel(initial_snapshot.clone());
    let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);

    let coordinator = Coordinator {
      snapshot: (*initial_snapshot).clone(),
      command_rx,
      snapshot_tx,
      event_tx: event_tx.clone(),
      wake,
    };
    let coordinator = runtime.spawn(coordinator.run());

    Self {
      client: AppClient {
        command_tx,
        snapshot_rx,
        event_tx,
      },
      coordinator: Some(coordinator),
    }
  }

  pub fn client(&self) -> AppClient {
    self.client.clone()
  }

  pub async fn shutdown(mut self) -> Result<(), BackendError> {
    if let Some(mut coordinator) = self.coordinator.take() {
      if !coordinator.is_finished() {
        let _ = self.client.request(UiCommand::Shutdown).await;
      }

      match timeout(SHUTDOWN_TIMEOUT, &mut coordinator).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(BackendError::TaskFailed(error.to_string())),
        Err(_) => {
          coordinator.abort();
          let _ = coordinator.await;
          Err(BackendError::ShutdownTimedOut)
        },
      }
    } else {
      Ok(())
    }
  }
}

impl Drop for BackendHandle {
  fn drop(&mut self) {
    if let Some(coordinator) = self.coordinator.take() {
      let _ = self.client.try_command(UiCommand::Shutdown);
      coordinator.abort();
    }
  }
}

struct Coordinator {
  snapshot: AppSnapshot,
  command_rx: mpsc::Receiver<CommandEnvelope>,
  snapshot_tx: watch::Sender<Arc<AppSnapshot>>,
  event_tx: broadcast::Sender<AppEvent>,
  wake: WakeHandle,
}

impl Coordinator {
  async fn run(mut self) {
    self.snapshot.status = AppStatus::Ready;
    self.publish_snapshot();
    self.emit(AppEvent::BackendReady);
    info!("application coordinator is ready");

    while let Some(envelope) = self.command_rx.recv().await {
      let (result, should_stop) = self.handle_command(envelope.command);
      if let Some(reply) = envelope.reply {
        let _ = reply.send(result);
      }
      if should_stop {
        break;
      }
    }

    debug!("application coordinator stopped");
  }

  fn handle_command(&mut self, command: UiCommand) -> (CommandResult, bool) {
    if self.snapshot.status == AppStatus::ShuttingDown && command != UiCommand::Shutdown {
      return (Err(CommandError::ShuttingDown), false);
    }

    match command {
      UiCommand::Ping => (Ok(CommandOutput::Pong), false),
      UiCommand::Navigate(page) => {
        self.snapshot.page = page;
        self.publish_snapshot();
        self.emit(AppEvent::NavigationChanged(page));
        (Ok(CommandOutput::Accepted), false)
      },
      UiCommand::SetTheme(theme) => {
        self.snapshot.theme = theme;
        self.publish_snapshot();
        self.emit(AppEvent::ThemeChanged(theme));
        (Ok(CommandOutput::Accepted), false)
      },
      UiCommand::SetWindowVisible(visible) => {
        self.set_window_visible(visible);
        (Ok(CommandOutput::Accepted), false)
      },
      UiCommand::ToggleWindow => {
        self.set_window_visible(!self.snapshot.window_visible);
        (Ok(CommandOutput::Accepted), false)
      },
      UiCommand::ClearError => {
        self.snapshot.last_error = None;
        self.publish_snapshot();
        (Ok(CommandOutput::Accepted), false)
      },
      UiCommand::Shutdown => {
        if self.snapshot.status != AppStatus::ShuttingDown {
          self.snapshot.status = AppStatus::ShuttingDown;
          self.publish_snapshot();
          self.emit(AppEvent::ShuttingDown);
        }
        (Ok(CommandOutput::ShutdownAccepted), true)
      },
    }
  }

  fn set_window_visible(&mut self, visible: bool) {
    if self.snapshot.window_visible != visible {
      self.snapshot.window_visible = visible;
      self.publish_snapshot();
      self.emit(AppEvent::WindowVisibilityChanged(visible));
    }
  }

  fn publish_snapshot(&mut self) {
    self.snapshot.revision = self.snapshot.revision.saturating_add(1);
    self
      .snapshot_tx
      .send_replace(Arc::new(self.snapshot.clone()));
    self.wake.wake();
  }

  fn emit(&self, event: AppEvent) {
    let _ = self.event_tx.send(event);
    self.wake.wake();
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use rsclash_domain::{AppStatus, CommandOutput, Page, ThemeMode, UiCommand};
  use tokio::time::timeout;

  use super::{BackendHandle, WakeHandle};

  async fn wait_for_snapshot(
    client: &mut super::AppClient,
    predicate: impl Fn(&rsclash_domain::AppSnapshot) -> bool,
  ) {
    let result = timeout(Duration::from_secs(1), async {
      loop {
        let snapshot = client.changed().await.map_err(|error| error.to_string())?;
        if predicate(&snapshot) {
          return Ok::<(), String>(());
        }
      }
    })
    .await;

    assert!(matches!(result, Ok(Ok(()))));
  }

  #[tokio::test]
  async fn coordinator_processes_commands_and_publishes_snapshots() {
    let backend = BackendHandle::spawn(&tokio::runtime::Handle::current(), WakeHandle::default());
    let mut client = backend.client();

    wait_for_snapshot(&mut client, |snapshot| snapshot.status == AppStatus::Ready).await;
    assert_eq!(
      client.request(UiCommand::Ping).await.ok(),
      Some(CommandOutput::Pong)
    );

    assert!(
      client
        .try_command(UiCommand::Navigate(Page::Proxies))
        .is_ok()
    );
    wait_for_snapshot(&mut client, |snapshot| snapshot.page == Page::Proxies).await;

    assert!(
      client
        .try_command(UiCommand::SetTheme(ThemeMode::Dark))
        .is_ok()
    );
    wait_for_snapshot(&mut client, |snapshot| snapshot.theme == ThemeMode::Dark).await;

    assert!(client.try_command(UiCommand::ToggleWindow).is_ok());
    wait_for_snapshot(&mut client, |snapshot| !snapshot.window_visible).await;

    assert!(backend.shutdown().await.is_ok());
  }

  #[tokio::test]
  async fn shutdown_is_idempotent_at_protocol_level() {
    let backend = BackendHandle::spawn(&tokio::runtime::Handle::current(), WakeHandle::default());
    let client = backend.client();

    assert_eq!(
      client.request(UiCommand::Shutdown).await.ok(),
      Some(CommandOutput::ShutdownAccepted)
    );
    assert!(backend.shutdown().await.is_ok());
  }
}
