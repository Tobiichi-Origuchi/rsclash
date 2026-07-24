//! Asynchronous application coordinator and UI-facing client.

mod change;
mod mihomo;
mod profiles;
mod proxy_view;
mod runtime;
mod settings;
mod system_proxy;

use std::{fmt, future::pending, sync::Arc, time::Duration};

use rsclash_core::{CoreHandle, CoreRuntime};
use rsclash_domain::{
  AppEvent, AppSnapshot, AppStatus, CommandError, CommandOutput, CommandResult, CoreChannel,
  CoreState, ErrorView, UiCommand,
};
use rsclash_platform::{RecoveryReason, SystemProxyService, SystemStateRecovery};
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
pub use mihomo::MihomoAccess;
pub use profiles::ProfileAccess;
pub use runtime::CoreRuntimeActivator;
pub use settings::{ServiceInstallAccess, SettingsAccess};
pub use system_proxy::SystemProxyAccess;

use mihomo::{MihomoBridgeCommand, MihomoBridgeEvent, run_mihomo_worker};
use profiles::{
  ProfileBridgeCommand, ProfileBridgeEvent, ProfileContentCommand, ProfileImportCommand,
  ProfileMutationCommand, ProfileQrCommand, ProfileUpdateCommand, run_profile_worker,
};
use settings::{SettingsBridgeCommand, SettingsBridgeEvent, run_settings_worker};
use system_proxy::{SystemProxyBridgeCommand, SystemProxyBridgeEvent, run_system_proxy_worker};

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 64;
const CORE_EVENT_CAPACITY: usize = 32;
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
  last_snapshot_revision: u64,
  event_tx: broadcast::Sender<AppEvent>,
}

pub struct AppEventReceiver {
  receiver: broadcast::Receiver<AppEvent>,
}

impl AppEventReceiver {
  pub fn try_recv(&mut self) -> Option<AppEvent> {
    loop {
      match self.receiver.try_recv() {
        Ok(event) => return Some(event),
        Err(broadcast::error::TryRecvError::Lagged(_)) => {},
        Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
          return None;
        },
      }
    }
  }
}

impl AppClient {
  pub fn current_snapshot(&self) -> Arc<AppSnapshot> {
    self.snapshot_rx.borrow().clone()
  }

  pub fn take_snapshot_if_changed(&mut self) -> Option<Arc<AppSnapshot>> {
    let has_changed = self
      .snapshot_rx
      .has_changed()
      .unwrap_or_else(|_| self.snapshot_rx.borrow().revision != self.last_snapshot_revision);
    if has_changed {
      let snapshot = self.snapshot_rx.borrow_and_update().clone();
      self.last_snapshot_revision = snapshot.revision;
      Some(snapshot)
    } else {
      None
    }
  }

  pub async fn changed(&mut self) -> Result<Arc<AppSnapshot>, ClientError> {
    self
      .snapshot_rx
      .changed()
      .await
      .map_err(|_| ClientError::CoordinatorClosed)?;
    let snapshot = self.snapshot_rx.borrow_and_update().clone();
    self.last_snapshot_revision = snapshot.revision;
    Ok(snapshot)
  }

  pub fn subscribe_events(&self) -> AppEventReceiver {
    AppEventReceiver {
      receiver: self.event_tx.subscribe(),
    }
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
  #[error("the core runtime did not shut down cleanly: {0}")]
  CoreShutdown(String),
  #[error("system state recovery failed during shutdown: {0}")]
  SystemRecovery(String),
}

pub struct BackendHandle {
  client: AppClient,
  coordinator: Option<JoinHandle<()>>,
  core_relay: Option<JoinHandle<()>>,
  core_worker: Option<JoinHandle<()>>,
  mihomo_worker: Option<JoinHandle<()>>,
  profile_worker: Option<JoinHandle<()>>,
  system_proxy_worker: Option<JoinHandle<()>>,
  settings_worker: Option<JoinHandle<()>>,
  core_runtime: Option<CoreRuntime>,
  system_recovery: Option<Arc<dyn SystemStateRecovery>>,
}

impl BackendHandle {
  pub fn spawn(runtime: &Handle, wake: WakeHandle) -> Self {
    Self::spawn_inner(runtime, wake, None, None, None, None, None, None)
  }

  pub fn spawn_with_core(runtime: &Handle, wake: WakeHandle, core_runtime: CoreRuntime) -> Self {
    Self::spawn_inner(
      runtime,
      wake,
      Some(core_runtime),
      None,
      None,
      None,
      None,
      None,
    )
  }

  pub fn spawn_with_core_and_mihomo(
    runtime: &Handle,
    wake: WakeHandle,
    core_runtime: CoreRuntime,
    mihomo_access: MihomoAccess,
  ) -> Self {
    Self::spawn_inner(
      runtime,
      wake,
      Some(core_runtime),
      None,
      Some(mihomo_access),
      None,
      None,
      None,
    )
  }

  pub fn spawn_with_core_and_recovery(
    runtime: &Handle,
    wake: WakeHandle,
    core_runtime: CoreRuntime,
    system_recovery: Arc<dyn SystemStateRecovery>,
  ) -> Self {
    Self::spawn_inner(
      runtime,
      wake,
      Some(core_runtime),
      Some(system_recovery),
      None,
      None,
      None,
      None,
    )
  }

  pub fn spawn_with_core_recovery_and_mihomo(
    runtime: &Handle,
    wake: WakeHandle,
    core_runtime: CoreRuntime,
    system_recovery: Arc<dyn SystemStateRecovery>,
    mihomo_access: MihomoAccess,
  ) -> Self {
    Self::spawn_inner(
      runtime,
      wake,
      Some(core_runtime),
      Some(system_recovery),
      Some(mihomo_access),
      None,
      None,
      None,
    )
  }

  pub fn spawn_with_core_integrations(
    runtime: &Handle,
    wake: WakeHandle,
    core_runtime: CoreRuntime,
    system_recovery: Arc<dyn SystemStateRecovery>,
    mihomo_access: MihomoAccess,
    profile_access: ProfileAccess,
  ) -> Self {
    Self::spawn_inner(
      runtime,
      wake,
      Some(core_runtime),
      Some(system_recovery),
      Some(mihomo_access),
      Some(profile_access),
      None,
      None,
    )
  }

  pub fn spawn_with_system_proxy_integrations(
    runtime: &Handle,
    wake: WakeHandle,
    core_runtime: CoreRuntime,
    system_proxy: Arc<SystemProxyService>,
    mihomo_access: MihomoAccess,
    profile_access: ProfileAccess,
  ) -> Self {
    let system_recovery: Arc<dyn SystemStateRecovery> =
      Arc::<SystemProxyService>::clone(&system_proxy);
    Self::spawn_inner(
      runtime,
      wake,
      Some(core_runtime),
      Some(system_recovery),
      Some(mihomo_access),
      Some(profile_access),
      Some(SystemProxyAccess::new(system_proxy)),
      None,
    )
  }

  pub fn spawn_with_linux_integrations(
    runtime: &Handle,
    wake: WakeHandle,
    core_runtime: CoreRuntime,
    system_proxy: Arc<SystemProxyService>,
    mihomo_access: MihomoAccess,
    profile_access: ProfileAccess,
    settings_access: SettingsAccess,
  ) -> Self {
    let system_recovery: Arc<dyn SystemStateRecovery> =
      Arc::<SystemProxyService>::clone(&system_proxy);
    Self::spawn_inner(
      runtime,
      wake,
      Some(core_runtime),
      Some(system_recovery),
      Some(mihomo_access),
      Some(profile_access),
      Some(SystemProxyAccess::new(system_proxy)),
      Some(settings_access),
    )
  }

  #[allow(
    clippy::too_many_arguments,
    reason = "the constructor keeps each optional worker integration explicit"
  )]
  fn spawn_inner(
    runtime: &Handle,
    wake: WakeHandle,
    core_runtime: Option<CoreRuntime>,
    system_recovery: Option<Arc<dyn SystemStateRecovery>>,
    mihomo_access: Option<MihomoAccess>,
    profile_access: Option<ProfileAccess>,
    system_proxy_access: Option<SystemProxyAccess>,
    settings_access: Option<SettingsAccess>,
  ) -> Self {
    let core = core_runtime.as_ref().map(CoreRuntime::handle);
    let profile_core = core.clone();
    let settings_core = core.clone();
    let mut initial_snapshot = AppSnapshot::default();
    if let Some(core) = &core {
      initial_snapshot.core = core.current_state().as_ref().clone();
    }
    let initial_snapshot = Arc::new(initial_snapshot);
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (snapshot_tx, snapshot_rx) = watch::channel(Arc::clone(&initial_snapshot));
    let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
    let (core_command_tx, core_event_rx, core_relay, core_worker) = match core {
      Some(core) => {
        let (core_event_tx, core_event_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
        let (core_command_tx, core_command_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
        let relay_tx = core_event_tx.clone();
        let relay = runtime.spawn(relay_core_states(core.clone(), relay_tx));
        let worker = runtime.spawn(run_core_commands(core, core_command_rx, core_event_tx));
        (
          Some(core_command_tx),
          Some(core_event_rx),
          Some(relay),
          Some(worker),
        )
      },
      None => (None, None, None, None),
    };
    let (mihomo_command_tx, mihomo_event_rx, mihomo_worker) = match mihomo_access {
      Some(access) => {
        let (command_tx, command_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
        let _ = command_tx.try_send(MihomoBridgeCommand::CoreState(
          initial_snapshot.core.clone(),
        ));
        let worker = runtime.spawn(run_mihomo_worker(access, command_rx, event_tx));
        (Some(command_tx), Some(event_rx), Some(worker))
      },
      None => (None, None, None),
    };
    let (profile_command_tx, profile_event_rx, profile_worker) =
      match (profile_access, profile_core) {
        (Some(access), Some(core)) => {
          let (command_tx, command_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
          let (event_tx, event_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
          let activator: Arc<dyn rsclash_config::RuntimeActivator> =
            Arc::new(CoreRuntimeActivator::new(core));
          let worker = runtime.spawn(run_profile_worker(access, activator, command_rx, event_tx));
          (Some(command_tx), Some(event_rx), Some(worker))
        },
        _ => (None, None, None),
      };
    let (system_proxy_command_tx, system_proxy_event_rx, system_proxy_worker) =
      match system_proxy_access {
        Some(access) => {
          let (command_tx, command_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
          let (event_tx, event_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
          let worker = runtime.spawn(run_system_proxy_worker(access, command_rx, event_tx));
          (Some(command_tx), Some(event_rx), Some(worker))
        },
        None => (None, None, None),
      };
    let (settings_command_tx, settings_event_rx, settings_worker) =
      match (settings_access, settings_core) {
        (Some(access), Some(core)) => {
          let (command_tx, command_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
          let (event_tx, event_rx) = mpsc::channel(CORE_EVENT_CAPACITY);
          let activator: Arc<dyn rsclash_config::RuntimeActivator> =
            Arc::new(CoreRuntimeActivator::new(core));
          let worker = runtime.spawn(run_settings_worker(access, activator, command_rx, event_tx));
          (Some(command_tx), Some(event_rx), Some(worker))
        },
        _ => (None, None, None),
      };

    let coordinator = Coordinator {
      snapshot: (*initial_snapshot).clone(),
      command_rx,
      snapshot_tx,
      event_tx: event_tx.clone(),
      wake,
      core_command_tx,
      core_event_rx,
      mihomo_command_tx,
      mihomo_event_rx,
      profile_command_tx,
      profile_event_rx,
      system_proxy_command_tx,
      system_proxy_event_rx,
      settings_command_tx,
      settings_event_rx,
    };
    let coordinator = runtime.spawn(coordinator.run());

    Self {
      client: AppClient {
        command_tx,
        snapshot_rx,
        last_snapshot_revision: initial_snapshot.revision,
        event_tx,
      },
      coordinator: Some(coordinator),
      core_relay,
      core_worker,
      mihomo_worker,
      profile_worker,
      system_proxy_worker,
      settings_worker,
      core_runtime,
      system_recovery,
    }
  }

  pub fn client(&self) -> AppClient {
    self.client.clone()
  }

  pub async fn shutdown(mut self) -> Result<(), BackendError> {
    let coordinator_result = if let Some(mut coordinator) = self.coordinator.take() {
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
    };

    if let Some(core_relay) = self.core_relay.take() {
      core_relay.abort();
      let _ = core_relay.await;
    }
    if let Some(core_worker) = self.core_worker.take() {
      core_worker.abort();
      let _ = core_worker.await;
    }
    if let Some(mihomo_worker) = self.mihomo_worker.take() {
      mihomo_worker.abort();
      let _ = mihomo_worker.await;
    }
    if let Some(profile_worker) = self.profile_worker.take() {
      profile_worker.abort();
      let _ = profile_worker.await;
    }
    if let Some(system_proxy_worker) = self.system_proxy_worker.take() {
      system_proxy_worker.abort();
      let _ = system_proxy_worker.await;
    }
    if let Some(settings_worker) = self.settings_worker.take() {
      settings_worker.abort();
      let _ = settings_worker.await;
    }

    let recovery_result = if let Some(system_recovery) = self.system_recovery.take() {
      system_recovery
        .restore_pending(RecoveryReason::CleanShutdown)
        .await
        .map(|_| ())
        .map_err(|error| BackendError::SystemRecovery(error.to_string()))
    } else {
      Ok(())
    };

    let core_result = if let Some(core_runtime) = self.core_runtime.take() {
      core_runtime
        .shutdown()
        .await
        .map_err(|error| BackendError::CoreShutdown(error.to_string()))
    } else {
      Ok(())
    };
    coordinator_result?;
    recovery_result?;
    core_result
  }
}

impl Drop for BackendHandle {
  fn drop(&mut self) {
    if let Some(coordinator) = self.coordinator.take() {
      let _ = self.client.try_command(UiCommand::Shutdown);
      coordinator.abort();
    }
    if let Some(core_relay) = self.core_relay.take() {
      core_relay.abort();
    }
    if let Some(core_worker) = self.core_worker.take() {
      core_worker.abort();
    }
    if let Some(mihomo_worker) = self.mihomo_worker.take() {
      mihomo_worker.abort();
    }
    if let Some(profile_worker) = self.profile_worker.take() {
      profile_worker.abort();
    }
    if let Some(system_proxy_worker) = self.system_proxy_worker.take() {
      system_proxy_worker.abort();
    }
    if let Some(settings_worker) = self.settings_worker.take() {
      settings_worker.abort();
    }
    let _ = self.system_recovery.take();
    let _ = self.core_runtime.take();
  }
}

#[derive(Clone, Copy, Debug)]
enum CoreBridgeCommand {
  Start(CoreChannel),
  Stop,
  Restart(CoreChannel),
  Reload,
}

enum CoreBridgeEvent {
  State(CoreState),
  CommandFailed(String),
}

struct Coordinator {
  snapshot: AppSnapshot,
  command_rx: mpsc::Receiver<CommandEnvelope>,
  snapshot_tx: watch::Sender<Arc<AppSnapshot>>,
  event_tx: broadcast::Sender<AppEvent>,
  wake: WakeHandle,
  core_command_tx: Option<mpsc::Sender<CoreBridgeCommand>>,
  core_event_rx: Option<mpsc::Receiver<CoreBridgeEvent>>,
  mihomo_command_tx: Option<mpsc::Sender<MihomoBridgeCommand>>,
  mihomo_event_rx: Option<mpsc::Receiver<MihomoBridgeEvent>>,
  profile_command_tx: Option<mpsc::Sender<ProfileBridgeCommand>>,
  profile_event_rx: Option<mpsc::Receiver<ProfileBridgeEvent>>,
  system_proxy_command_tx: Option<mpsc::Sender<SystemProxyBridgeCommand>>,
  system_proxy_event_rx: Option<mpsc::Receiver<SystemProxyBridgeEvent>>,
  settings_command_tx: Option<mpsc::Sender<SettingsBridgeCommand>>,
  settings_event_rx: Option<mpsc::Receiver<SettingsBridgeEvent>>,
}

impl Coordinator {
  async fn run(mut self) {
    self.snapshot.status = AppStatus::Ready;
    self.publish_snapshot();
    self.emit(AppEvent::BackendReady);
    info!("application coordinator is ready");

    loop {
      match receive_coordinator_input(
        &mut self.command_rx,
        &mut self.core_event_rx,
        &mut self.mihomo_event_rx,
        &mut self.profile_event_rx,
        &mut self.system_proxy_event_rx,
        &mut self.settings_event_rx,
      )
      .await
      {
        CoordinatorInput::Command(envelope) => {
          let Some(envelope) = envelope else {
            break;
          };
          let (result, should_stop) = self.handle_command(envelope.command);
          if let Some(reply) = envelope.reply {
            let _ = reply.send(result);
          }
          if should_stop {
            break;
          }
        },
        CoordinatorInput::Core(core_event) => match core_event {
          Some(event) => self.handle_core_event(event),
          None => self.core_event_rx = None,
        },
        CoordinatorInput::Mihomo(mihomo_event) => match mihomo_event {
          Some(event) => self.handle_mihomo_event(event),
          None => self.mihomo_event_rx = None,
        },
        CoordinatorInput::Profile(profile_event) => match profile_event {
          Some(event) => self.handle_profile_event(event),
          None => self.profile_event_rx = None,
        },
        CoordinatorInput::SystemProxy(system_proxy_event) => match system_proxy_event {
          Some(event) => self.handle_system_proxy_event(event),
          None => self.system_proxy_event_rx = None,
        },
        CoordinatorInput::Settings(settings_event) => match settings_event {
          Some(event) => self.handle_settings_event(event),
          None => self.settings_event_rx = None,
        },
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
      UiCommand::StartCore(channel) => self.dispatch_core(CoreBridgeCommand::Start(channel)),
      UiCommand::StopCore => self.dispatch_core(CoreBridgeCommand::Stop),
      UiCommand::RestartCore(channel) => self.dispatch_core(CoreBridgeCommand::Restart(channel)),
      UiCommand::ReloadCore => self.dispatch_core(CoreBridgeCommand::Reload),
      UiCommand::RefreshMihomo => self.dispatch_mihomo(MihomoBridgeCommand::Refresh),
      UiCommand::SelectProxy { group, proxy } => {
        self.dispatch_mihomo(MihomoBridgeCommand::SelectProxy { group, proxy })
      },
      UiCommand::TestProxy { record_id } => {
        self.dispatch_mihomo(MihomoBridgeCommand::TestProxy { record_id })
      },
      UiCommand::TestProxyGroup { name } => {
        self.dispatch_mihomo(MihomoBridgeCommand::TestProxyGroup { name })
      },
      UiCommand::TestAllProxies => self.dispatch_mihomo(MihomoBridgeCommand::TestAllProxies),
      UiCommand::UpdateProxyProvider { name } => {
        self.dispatch_mihomo(MihomoBridgeCommand::UpdateProxyProvider { name })
      },
      UiCommand::UpdateAllProxyProviders => {
        self.dispatch_mihomo(MihomoBridgeCommand::UpdateAllProxyProviders)
      },
      UiCommand::HealthcheckProxyProvider { name } => {
        self.dispatch_mihomo(MihomoBridgeCommand::HealthcheckProxyProvider { name })
      },
      UiCommand::SetProxyChain { group, nodes } => {
        self.dispatch_profile(ProfileBridgeCommand::SetProxyChain { group, nodes })
      },
      UiCommand::UpdateRuleProvider { name } => {
        self.dispatch_mihomo(MihomoBridgeCommand::UpdateRuleProvider { name })
      },
      UiCommand::CloseConnection { id } => {
        self.dispatch_mihomo(MihomoBridgeCommand::CloseConnection { id })
      },
      UiCommand::CloseAllConnections => {
        self.dispatch_mihomo(MihomoBridgeCommand::CloseAllConnections)
      },
      UiCommand::ClearClosedConnections => {
        self.dispatch_mihomo(MihomoBridgeCommand::ClearClosedConnections)
      },
      UiCommand::SetConnectionsPaused(paused) => {
        self.dispatch_mihomo(MihomoBridgeCommand::SetConnectionsPaused(paused))
      },
      UiCommand::ClearLogs => self.dispatch_mihomo(MihomoBridgeCommand::ClearLogs),
      UiCommand::SetLogsPaused(paused) => {
        self.dispatch_mihomo(MihomoBridgeCommand::SetLogsPaused(paused))
      },
      UiCommand::SetLogLevel(level) => {
        self.dispatch_mihomo(MihomoBridgeCommand::SetLogLevel(level))
      },
      UiCommand::SetProxyMode(mode) => self.dispatch_mihomo(MihomoBridgeCommand::SetMode(mode)),
      UiCommand::RefreshProfiles => self.dispatch_profile(ProfileBridgeCommand::Refresh),
      UiCommand::ImportLocalProfile { name, path } => {
        self.dispatch_profile(ProfileBridgeCommand::Import(ProfileImportCommand::Local {
          name,
          path,
        }))
      },
      UiCommand::ImportRemoteProfile { name, url, options } => {
        self.dispatch_profile(ProfileBridgeCommand::Import(ProfileImportCommand::Remote {
          name,
          url,
          options,
        }))
      },
      UiCommand::ImportProfileQr {
        name,
        path,
        options,
      } => self.dispatch_profile(ProfileBridgeCommand::Import(ProfileImportCommand::Qr {
        name,
        path,
        options,
      })),
      UiCommand::RequestProfileQr { uid } => {
        self.dispatch_profile(ProfileBridgeCommand::Qr(ProfileQrCommand::Share { uid }))
      },
      UiCommand::ActivateProfile { uid } => {
        self.dispatch_profile(ProfileBridgeCommand::Activate { uid })
      },
      UiCommand::RenameProfile { uid, name } => self.dispatch_profile(
        ProfileBridgeCommand::Mutate(ProfileMutationCommand::Rename { uid, name }),
      ),
      UiCommand::DuplicateProfile { uid } => self.dispatch_profile(ProfileBridgeCommand::Mutate(
        ProfileMutationCommand::Duplicate { uid },
      )),
      UiCommand::DeleteProfiles { uids } => self.dispatch_profile(ProfileBridgeCommand::Mutate(
        ProfileMutationCommand::Delete { uids },
      )),
      UiCommand::ReorderProfile { uid, new_index } => self.dispatch_profile(
        ProfileBridgeCommand::Mutate(ProfileMutationCommand::Reorder { uid, new_index }),
      ),
      UiCommand::SetRemoteProfileOptions { uid, options } => self.dispatch_profile(
        ProfileBridgeCommand::Mutate(ProfileMutationCommand::SetRemoteOptions { uid, options }),
      ),
      UiCommand::LoadProfileContent { uid } => {
        self.dispatch_profile(ProfileBridgeCommand::Content(ProfileContentCommand::Load {
          uid,
        }))
      },
      UiCommand::SaveProfileContent { uid, content } => {
        self.dispatch_profile(ProfileBridgeCommand::Content(ProfileContentCommand::Save {
          uid,
          content,
        }))
      },
      UiCommand::UpdateProfile { uid } => {
        self.dispatch_profile(ProfileBridgeCommand::Update(ProfileUpdateCommand::One {
          uid,
        }))
      },
      UiCommand::UpdateAllProfiles => {
        self.dispatch_profile(ProfileBridgeCommand::Update(ProfileUpdateCommand::All))
      },
      UiCommand::RefreshSystemProxy => {
        self.dispatch_system_proxy(SystemProxyBridgeCommand::Refresh)
      },
      UiCommand::SetSystemProxy(enabled) => self.set_system_proxy(enabled),
      UiCommand::RefreshSettings => self.dispatch_settings(SettingsBridgeCommand::Refresh),
      UiCommand::ApplySettings(settings) => {
        let settings = *settings;
        if settings.tun_enabled
          && !matches!(
            self.snapshot.core,
            CoreState::Running {
              mode: rsclash_domain::CoreRunMode::Service,
              ..
            }
          )
        {
          return (
            Err(CommandError::InvalidState(
              "the privileged service must be the active core backend before enabling TUN"
                .to_string(),
            )),
            false,
          );
        }
        self.dispatch_settings(SettingsBridgeCommand::Apply(Box::new(settings)))
      },
      UiCommand::InstallService => self.dispatch_settings(SettingsBridgeCommand::InstallService),
      UiCommand::UninstallService => {
        self.dispatch_settings(SettingsBridgeCommand::UninstallService)
      },
      UiCommand::RegisterDeepLinks => {
        self.dispatch_settings(SettingsBridgeCommand::RegisterDeepLinks)
      },
      UiCommand::OpenDirectory(directory) => {
        self.dispatch_settings(SettingsBridgeCommand::OpenDirectory(directory))
      },
      UiCommand::OpenWebUi => self.dispatch_settings(SettingsBridgeCommand::OpenWebUi),
      UiCommand::Navigate(page) => {
        self.snapshot.page = page;
        self.update_mihomo_presentation();
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

  fn dispatch_core(&self, command: CoreBridgeCommand) -> (CommandResult, bool) {
    let Some(command_tx) = &self.core_command_tx else {
      return (
        Err(CommandError::InvalidState(
          "the Mihomo core runtime is not configured".to_string(),
        )),
        false,
      );
    };
    match command_tx.try_send(command) {
      Ok(()) => (Ok(CommandOutput::Accepted), false),
      Err(mpsc::error::TrySendError::Full(_)) => (
        Err(CommandError::InvalidState(
          "the Mihomo core command queue is full".to_string(),
        )),
        false,
      ),
      Err(mpsc::error::TrySendError::Closed(_)) => (
        Err(CommandError::InvalidState(
          "the Mihomo core command bridge is closed".to_string(),
        )),
        false,
      ),
    }
  }

  fn dispatch_mihomo(&self, command: MihomoBridgeCommand) -> (CommandResult, bool) {
    let Some(command_tx) = &self.mihomo_command_tx else {
      return (
        Err(CommandError::InvalidState(
          "the Mihomo controller bridge is not configured".to_string(),
        )),
        false,
      );
    };
    match command_tx.try_send(command) {
      Ok(()) => (Ok(CommandOutput::Accepted), false),
      Err(mpsc::error::TrySendError::Full(_)) => (
        Err(CommandError::InvalidState(
          "the Mihomo controller command queue is full".to_string(),
        )),
        false,
      ),
      Err(mpsc::error::TrySendError::Closed(_)) => (
        Err(CommandError::InvalidState(
          "the Mihomo controller bridge is closed".to_string(),
        )),
        false,
      ),
    }
  }

  fn dispatch_profile(&self, command: ProfileBridgeCommand) -> (CommandResult, bool) {
    let Some(command_tx) = &self.profile_command_tx else {
      return (
        Err(CommandError::InvalidState(
          "the profile runtime is not configured".to_string(),
        )),
        false,
      );
    };
    match command_tx.try_send(command) {
      Ok(()) => (Ok(CommandOutput::Accepted), false),
      Err(mpsc::error::TrySendError::Full(_)) => (
        Err(CommandError::InvalidState(
          "the profile command queue is full".to_string(),
        )),
        false,
      ),
      Err(mpsc::error::TrySendError::Closed(_)) => (
        Err(CommandError::InvalidState(
          "the profile bridge is closed".to_string(),
        )),
        false,
      ),
    }
  }

  fn dispatch_system_proxy(&self, command: SystemProxyBridgeCommand) -> (CommandResult, bool) {
    let Some(command_tx) = &self.system_proxy_command_tx else {
      return (
        Err(CommandError::InvalidState(
          "the system proxy backend is not configured".to_string(),
        )),
        false,
      );
    };
    match command_tx.try_send(command) {
      Ok(()) => (Ok(CommandOutput::Accepted), false),
      Err(mpsc::error::TrySendError::Full(_)) => (
        Err(CommandError::InvalidState(
          "the system proxy command queue is full".to_string(),
        )),
        false,
      ),
      Err(mpsc::error::TrySendError::Closed(_)) => (
        Err(CommandError::InvalidState(
          "the system proxy bridge is closed".to_string(),
        )),
        false,
      ),
    }
  }

  fn set_system_proxy(&self, enabled: bool) -> (CommandResult, bool) {
    if !enabled {
      return self.dispatch_system_proxy(SystemProxyBridgeCommand::SetEnabled {
        enabled: false,
        port: 0,
        bypass: Vec::new(),
        pac_url: None,
      });
    }
    if self.snapshot.settings.value.pac_url.is_some() {
      return self.dispatch_system_proxy(SystemProxyBridgeCommand::SetEnabled {
        enabled: true,
        port: 0,
        bypass: self.snapshot.settings.value.system_proxy_bypass.clone(),
        pac_url: self.snapshot.settings.value.pac_url.clone(),
      });
    }
    if !matches!(self.snapshot.core, CoreState::Running { .. }) {
      return (
        Err(CommandError::InvalidState(
          "Mihomo must be running before enabling the system proxy".to_string(),
        )),
        false,
      );
    }
    let Some(port) = self.snapshot.mihomo.mixed_port else {
      return (
        Err(CommandError::InvalidState(
          "Mihomo did not report a valid mixed proxy port".to_string(),
        )),
        false,
      );
    };
    self.dispatch_system_proxy(SystemProxyBridgeCommand::SetEnabled {
      enabled: true,
      port,
      bypass: self.snapshot.settings.value.system_proxy_bypass.clone(),
      pac_url: self.snapshot.settings.value.pac_url.clone(),
    })
  }

  fn dispatch_settings(&self, command: SettingsBridgeCommand) -> (CommandResult, bool) {
    let Some(command_tx) = &self.settings_command_tx else {
      return (
        Err(CommandError::InvalidState(
          "the settings backend is not configured".to_string(),
        )),
        false,
      );
    };
    match command_tx.try_send(command) {
      Ok(()) => (Ok(CommandOutput::Accepted), false),
      Err(mpsc::error::TrySendError::Full(_)) => (
        Err(CommandError::InvalidState(
          "the settings command queue is full".to_string(),
        )),
        false,
      ),
      Err(mpsc::error::TrySendError::Closed(_)) => (
        Err(CommandError::InvalidState(
          "the settings bridge is closed".to_string(),
        )),
        false,
      ),
    }
  }

  fn handle_core_event(&mut self, event: CoreBridgeEvent) {
    match event {
      CoreBridgeEvent::State(state) => {
        if let CoreState::Failed { message } = &state {
          self.snapshot.last_error = Some(ErrorView {
            title: "Mihomo core failed".to_string(),
            detail: message.clone(),
            retryable: true,
          });
        }
        if let Some(command_tx) = &self.mihomo_command_tx
          && let Err(error) = command_tx.try_send(MihomoBridgeCommand::CoreState(state.clone()))
        {
          self.snapshot.last_error = Some(ErrorView {
            title: "Mihomo controller state update failed".to_string(),
            detail: error.to_string(),
            retryable: true,
          });
        }
        if !matches!(state, CoreState::Running { .. })
          && self.snapshot.system_proxy.enabled
          && let Some(command_tx) = &self.system_proxy_command_tx
        {
          let _ = command_tx.try_send(SystemProxyBridgeCommand::SetEnabled {
            enabled: false,
            port: 0,
            bypass: Vec::new(),
            pac_url: None,
          });
        }
        self.snapshot.core = state.clone();
        self.publish_snapshot();
        self.emit(AppEvent::CoreStateChanged(state));
      },
      CoreBridgeEvent::CommandFailed(message) => {
        self.snapshot.last_error = Some(ErrorView {
          title: "Mihomo core command failed".to_string(),
          detail: message,
          retryable: true,
        });
        self.publish_snapshot();
      },
    }
  }

  fn handle_mihomo_event(&mut self, event: MihomoBridgeEvent) {
    match event {
      MihomoBridgeEvent::Snapshot(snapshot) => {
        self.snapshot.mihomo = *snapshot;
        self.ensure_desired_system_proxy();
        self.publish_snapshot();
        self.emit(AppEvent::MihomoStateChanged);
      },
      MihomoBridgeEvent::ProxySelected {
        group,
        proxy,
        previous,
      } => {
        if let Some(command_tx) = &self.profile_command_tx {
          let _ = command_tx.try_send(ProfileBridgeCommand::PersistSelection {
            group,
            proxy,
            previous,
          });
        }
      },
      MihomoBridgeEvent::CommandFailed(message) => {
        self.snapshot.last_error = Some(ErrorView {
          title: "Mihomo controller command failed".to_string(),
          detail: message,
          retryable: true,
        });
        self.publish_snapshot();
      },
    }
  }

  fn handle_profile_event(&mut self, event: ProfileBridgeEvent) {
    match event {
      ProfileBridgeEvent::Snapshot(snapshot) => {
        let active_changed = self.snapshot.profiles.current().map(|profile| &profile.uid)
          != snapshot.current().map(|profile| &profile.uid);
        self.snapshot.profiles = snapshot;
        self.publish_snapshot();
        self.emit(AppEvent::ProfilesChanged);
        if active_changed && let Some(command_tx) = &self.mihomo_command_tx {
          let _ = command_tx.try_send(MihomoBridgeCommand::Refresh);
        }
      },
      ProfileBridgeEvent::RuntimeChanged(sync) => {
        if let Some(command_tx) = &self.mihomo_command_tx {
          let _ = command_tx.try_send(MihomoBridgeCommand::SynchronizeProfile(sync));
          if self.snapshot.settings.value.auto_test {
            let _ = command_tx.try_send(MihomoBridgeCommand::TestAllProxies);
          }
        }
      },
      ProfileBridgeEvent::SelectionPersisted {
        previous,
        close_connections,
      } => {
        if close_connections
          && let (Some(command_tx), Some(proxy)) = (&self.mihomo_command_tx, previous)
        {
          let _ = command_tx.try_send(MihomoBridgeCommand::CloseConnectionsForProxy { proxy });
        }
      },
      ProfileBridgeEvent::ContentLoaded { uid, content } => {
        self.emit(AppEvent::ProfileContentLoaded { uid, content });
      },
      ProfileBridgeEvent::ContentSaved { uid } => {
        self.emit(AppEvent::ProfileContentSaved { uid });
      },
      ProfileBridgeEvent::QrReady(qr) => {
        self.emit(AppEvent::ProfileQrReady(qr));
      },
      ProfileBridgeEvent::ProxyChainChanged { group, nodes } => {
        if let Some(command_tx) = &self.mihomo_command_tx {
          let _ = command_tx.try_send(MihomoBridgeCommand::ProxyChainChanged { group, nodes });
        }
      },
      ProfileBridgeEvent::CommandFailed(message) => {
        self.snapshot.last_error = Some(ErrorView {
          title: "Profile operation failed".to_string(),
          detail: message,
          retryable: true,
        });
        self.publish_snapshot();
      },
    }
  }

  fn handle_system_proxy_event(&mut self, event: SystemProxyBridgeEvent) {
    match event {
      SystemProxyBridgeEvent::Snapshot(snapshot) => {
        self.snapshot.system_proxy = snapshot;
        self.publish_snapshot();
        self.emit(AppEvent::SystemProxyChanged);
      },
      SystemProxyBridgeEvent::EnabledChanged(enabled) => {
        let _ = self.dispatch_settings(SettingsBridgeCommand::PersistSystemProxy(enabled));
      },
      SystemProxyBridgeEvent::CommandFailed(message) => {
        self.snapshot.last_error = Some(ErrorView {
          title: "System proxy operation failed".to_string(),
          detail: message,
          retryable: true,
        });
        self.publish_snapshot();
      },
    }
  }

  fn handle_settings_event(&mut self, event: SettingsBridgeEvent) {
    match event {
      SettingsBridgeEvent::Snapshot(snapshot) => {
        let snapshot = *snapshot;
        let system_proxy_target_changed = self.snapshot.settings.value.system_proxy_bypass
          != snapshot.value.system_proxy_bypass
          || self.snapshot.settings.value.pac_url != snapshot.value.pac_url;
        let reapply_system_proxy =
          system_proxy_target_changed && self.snapshot.system_proxy.enabled;
        let theme_changed = self.snapshot.theme != snapshot.value.theme;
        self.snapshot.theme = snapshot.value.theme;
        self.snapshot.settings = snapshot;
        if let Some(command_tx) = &self.mihomo_command_tx {
          let _ = command_tx.try_send(MihomoBridgeCommand::SetStreamFlushInterval(
            self.snapshot.settings.value.refresh_interval_ms,
          ));
          let _ = command_tx.try_send(MihomoBridgeCommand::SetLogLevel(
            self.snapshot.settings.value.mihomo_log_level,
          ));
          let _ = command_tx.try_send(MihomoBridgeCommand::SetLatencyTest {
            url: self.snapshot.settings.value.latency_test_url.clone(),
            timeout_ms: self
              .snapshot
              .settings
              .value
              .latency_timeout_ms
              .try_into()
              .unwrap_or(u32::MAX),
          });
        }
        if reapply_system_proxy {
          let _ = self.set_system_proxy(false);
          let _ = self.set_system_proxy(true);
        }
        self.ensure_desired_system_proxy();
        self.publish_snapshot();
        self.emit(AppEvent::SettingsChanged);
        if theme_changed {
          self.emit(AppEvent::ThemeChanged(self.snapshot.theme));
        }
      },
      SettingsBridgeEvent::CommandFailed(message) => {
        self.snapshot.last_error = Some(ErrorView {
          title: "Settings operation failed".to_string(),
          detail: message,
          retryable: true,
        });
        self.publish_snapshot();
      },
    }
  }

  fn set_window_visible(&mut self, visible: bool) {
    if self.snapshot.window_visible != visible {
      self.snapshot.window_visible = visible;
      self.update_mihomo_presentation();
      self.publish_snapshot();
      self.emit(AppEvent::WindowVisibilityChanged(visible));
    }
  }

  fn ensure_desired_system_proxy(&self) {
    let settings = &self.snapshot.settings.value;
    if !settings.system_proxy_enabled
      || self.snapshot.system_proxy.enabled
      || self.snapshot.system_proxy.busy
      || !self.snapshot.system_proxy.available
    {
      return;
    }
    let ready = settings.pac_url.is_some()
      || (self.snapshot.mihomo.connection == rsclash_domain::MihomoConnection::Connected
        && self.snapshot.mihomo.mixed_port.is_some());
    if ready {
      let _ = self.set_system_proxy(true);
    }
  }

  fn update_mihomo_presentation(&self) {
    if let Some(command_tx) = &self.mihomo_command_tx {
      let _ = command_tx.try_send(MihomoBridgeCommand::SetPresentation {
        page: self.snapshot.page,
        visible: self.snapshot.window_visible,
      });
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

enum CoordinatorInput {
  Command(Option<CommandEnvelope>),
  Core(Option<CoreBridgeEvent>),
  Mihomo(Option<MihomoBridgeEvent>),
  Profile(Option<ProfileBridgeEvent>),
  SystemProxy(Option<SystemProxyBridgeEvent>),
  Settings(Option<SettingsBridgeEvent>),
}

async fn receive_coordinator_input(
  command_rx: &mut mpsc::Receiver<CommandEnvelope>,
  core_event_rx: &mut Option<mpsc::Receiver<CoreBridgeEvent>>,
  mihomo_event_rx: &mut Option<mpsc::Receiver<MihomoBridgeEvent>>,
  profile_event_rx: &mut Option<mpsc::Receiver<ProfileBridgeEvent>>,
  system_proxy_event_rx: &mut Option<mpsc::Receiver<SystemProxyBridgeEvent>>,
  settings_event_rx: &mut Option<mpsc::Receiver<SettingsBridgeEvent>>,
) -> CoordinatorInput {
  tokio::select! {
    biased;
    envelope = command_rx.recv() => CoordinatorInput::Command(envelope),
    event = receive_core_event(core_event_rx) => CoordinatorInput::Core(event),
    event = receive_mihomo_event(mihomo_event_rx) => CoordinatorInput::Mihomo(event),
    event = receive_profile_event(profile_event_rx) => CoordinatorInput::Profile(event),
    event = receive_system_proxy_event(system_proxy_event_rx) => {
      CoordinatorInput::SystemProxy(event)
    },
    event = receive_settings_event(settings_event_rx) => CoordinatorInput::Settings(event),
  }
}

async fn relay_core_states(mut core: CoreHandle, event_tx: mpsc::Sender<CoreBridgeEvent>) {
  while let Ok(state) = core.changed().await {
    if event_tx
      .send(CoreBridgeEvent::State(state.as_ref().clone()))
      .await
      .is_err()
    {
      return;
    }
  }
}

async fn run_core_commands(
  core: CoreHandle,
  mut command_rx: mpsc::Receiver<CoreBridgeCommand>,
  event_tx: mpsc::Sender<CoreBridgeEvent>,
) {
  while let Some(command) = command_rx.recv().await {
    let result = match command {
      CoreBridgeCommand::Start(channel) => core.start(channel).await,
      CoreBridgeCommand::Stop => core.stop().await,
      CoreBridgeCommand::Restart(channel) => core.restart(channel).await,
      CoreBridgeCommand::Reload => core.reload().await,
    };
    if let Err(error) = result
      && event_tx
        .send(CoreBridgeEvent::CommandFailed(error.to_string()))
        .await
        .is_err()
    {
      return;
    }
  }
}

async fn receive_core_event(
  receiver: &mut Option<mpsc::Receiver<CoreBridgeEvent>>,
) -> Option<CoreBridgeEvent> {
  if let Some(receiver) = receiver {
    receiver.recv().await
  } else {
    pending::<Option<CoreBridgeEvent>>().await
  }
}

async fn receive_mihomo_event(
  receiver: &mut Option<mpsc::Receiver<MihomoBridgeEvent>>,
) -> Option<MihomoBridgeEvent> {
  if let Some(receiver) = receiver {
    receiver.recv().await
  } else {
    pending::<Option<MihomoBridgeEvent>>().await
  }
}

async fn receive_profile_event(
  receiver: &mut Option<mpsc::Receiver<ProfileBridgeEvent>>,
) -> Option<ProfileBridgeEvent> {
  if let Some(receiver) = receiver {
    receiver.recv().await
  } else {
    pending::<Option<ProfileBridgeEvent>>().await
  }
}

async fn receive_system_proxy_event(
  receiver: &mut Option<mpsc::Receiver<SystemProxyBridgeEvent>>,
) -> Option<SystemProxyBridgeEvent> {
  if let Some(receiver) = receiver {
    receiver.recv().await
  } else {
    pending::<Option<SystemProxyBridgeEvent>>().await
  }
}

async fn receive_settings_event(
  receiver: &mut Option<mpsc::Receiver<SettingsBridgeEvent>>,
) -> Option<SettingsBridgeEvent> {
  if let Some(receiver) = receiver {
    receiver.recv().await
  } else {
    pending::<Option<SettingsBridgeEvent>>().await
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    collections::HashMap,
    fs,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
  };

  use async_trait::async_trait;
  use rsclash_config::{
    ProfileItem, ProfileKind, Result as ConfigResult, RuntimeValidator, initialize_default_runtime,
  };
  use rsclash_core::{ControllerError, CoreRuntime, LifecycleController, RunningCore};
  use rsclash_domain::{
    AppEvent, AppStatus, CommandOutput, CoreChannel, CoreRunMode, CoreState, MihomoConnection,
    Page, ProxyMode, ThemeMode, UiCommand,
  };
  use rsclash_mihomo::{
    FakeMihomoApi, FakeMihomoState, MihomoCall,
    models::{BaseConfig, Connections, DelayHistory, Groups, Proxies, Proxy, VersionInfo},
  };
  use rsclash_platform::{
    RecoveryOutcome, RecoveryReason, Result as RecoveryResult, SystemStateRecovery,
  };
  use serde_json::json;
  use tokio::{sync::broadcast, time::timeout};

  use super::{AppEventReceiver, BackendHandle, MihomoAccess, ProfileAccess, WakeHandle};

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  enum CoreCall {
    Start(CoreChannel),
    Stop,
    Reload,
  }

  struct BridgeController {
    calls: Arc<Mutex<Vec<CoreCall>>>,
  }

  struct OrderedController {
    order: Arc<Mutex<Vec<&'static str>>>,
  }

  #[async_trait]
  impl LifecycleController for OrderedController {
    async fn start(&mut self, _channel: CoreChannel) -> Result<RunningCore, ControllerError> {
      Ok(BridgeController::running())
    }

    async fn stop(&mut self) -> Result<(), ControllerError> {
      self
        .order
        .lock()
        .expect("shutdown order lock should be available")
        .push("core");
      Ok(())
    }

    async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
      Ok(BridgeController::running())
    }

    async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
      Ok(BridgeController::running())
    }
  }

  struct OrderedRecovery {
    order: Arc<Mutex<Vec<&'static str>>>,
  }

  struct AcceptValidator;

  #[async_trait]
  impl RuntimeValidator for AcceptValidator {
    async fn validate(&self, _staging_path: &std::path::Path) -> ConfigResult<()> {
      Ok(())
    }
  }

  #[async_trait]
  impl SystemStateRecovery for OrderedRecovery {
    async fn restore_pending(&self, _reason: RecoveryReason) -> RecoveryResult<RecoveryOutcome> {
      self
        .order
        .lock()
        .expect("shutdown order lock should be available")
        .push("recovery");
      Ok(RecoveryOutcome::NothingPending)
    }
  }

  impl BridgeController {
    fn record(&self, call: CoreCall) {
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
  impl LifecycleController for BridgeController {
    async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
      self.record(CoreCall::Start(channel));
      tokio::time::sleep(Duration::from_millis(50)).await;
      Ok(Self::running())
    }

    async fn stop(&mut self) -> Result<(), ControllerError> {
      self.record(CoreCall::Stop);
      Ok(())
    }

    async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
      self.record(CoreCall::Reload);
      Ok(Self::running())
    }

    async fn health_check(&mut self) -> Result<RunningCore, ControllerError> {
      Ok(Self::running())
    }
  }

  async fn wait_for_snapshot(
    client: &mut super::AppClient,
    predicate: impl Fn(&rsclash_domain::AppSnapshot) -> bool + Send + Sync,
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

  #[test]
  fn event_receiver_skips_lagged_events() {
    let (sender, _) = broadcast::channel(1);
    let mut events = AppEventReceiver {
      receiver: sender.subscribe(),
    };

    assert!(sender.send(AppEvent::BackendReady).is_ok());
    assert!(sender.send(AppEvent::ShuttingDown).is_ok());
    assert_eq!(events.try_recv(), Some(AppEvent::ShuttingDown));
    assert_eq!(events.try_recv(), None);
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
  async fn mihomo_bridge_publishes_dashboard_data_and_routes_mutations() {
    let node = Proxy {
      name: "Node A".to_string(),
      alive: true,
      history: vec![DelayHistory {
        delay: 42,
        ..DelayHistory::default()
      }],
      ..Proxy::default()
    };
    let group = Proxy {
      name: "GLOBAL".to_string(),
      kind: "Selector".to_string(),
      all: Some(vec![node.name.clone()]),
      now: Some(node.name.clone()),
      ..Proxy::default()
    };
    let fake = FakeMihomoApi::new(FakeMihomoState {
      version: VersionInfo {
        version: "1.20.0".to_string(),
        ..VersionInfo::default()
      },
      base_config: BaseConfig {
        mixed_port: 17_897,
        mode: "rule".to_string(),
        tun: json!({ "enable": true }),
        ..BaseConfig::default()
      },
      groups: Groups {
        proxies: vec![group],
        ..Groups::default()
      },
      proxies: Proxies {
        proxies: HashMap::from([(node.name.clone(), node)]),
        ..Proxies::default()
      },
      connections: Connections {
        upload_total: 1_024,
        download_total: 2_048,
        memory: 4_096,
        connections: Some(vec![Default::default()]),
        ..Connections::default()
      },
      ..FakeMihomoState::default()
    });
    let core = CoreRuntime::spawn(
      &tokio::runtime::Handle::current(),
      BridgeController {
        calls: Arc::new(Mutex::new(Vec::new())),
      },
    );
    let backend = BackendHandle::spawn_with_core_and_mihomo(
      &tokio::runtime::Handle::current(),
      WakeHandle::default(),
      core,
      MihomoAccess::same(Arc::new(fake.clone())),
    );
    let mut client = backend.client();

    client
      .request(UiCommand::StartCore(CoreChannel::Stable))
      .await
      .expect("core start should be accepted");
    wait_for_snapshot(&mut client, |snapshot| {
      snapshot.mihomo.connection == MihomoConnection::Connected
        && snapshot.mihomo.version.as_deref() == Some("1.20.0")
        && snapshot.mihomo.connection_count == 1
    })
    .await;
    assert_eq!(client.current_snapshot().mihomo.memory_bytes, 4_096);
    assert_eq!(client.current_snapshot().mihomo.mixed_port, Some(17_897));
    assert!(client.current_snapshot().mihomo.tun_enabled);
    assert_eq!(
      client.current_snapshot().mihomo.current_proxy(),
      Some("Node A")
    );
    assert_eq!(
      client.current_snapshot().mihomo.groups[0].options[0].delay_ms,
      Some(42)
    );

    client
      .request(UiCommand::SetProxyMode(ProxyMode::Global))
      .await
      .expect("mode change should be accepted");
    client
      .request(UiCommand::SelectProxy {
        group: "GLOBAL".to_string(),
        proxy: "Node A".to_string(),
      })
      .await
      .expect("proxy selection should be accepted");
    wait_for_snapshot(&mut client, |snapshot| {
      snapshot.mihomo.mode == ProxyMode::Global
    })
    .await;

    let calls = fake.calls().expect("fake calls should be available");
    assert!(
      calls
        .iter()
        .any(|call| matches!(call, MihomoCall::PatchBaseConfig(_)))
    );
    assert!(calls.iter().any(|call| matches!(call, MihomoCall::SelectProxy { group, proxy } if group == "GLOBAL" && proxy == "Node A")));
    assert!(backend.shutdown().await.is_ok());
  }

  #[tokio::test]
  async fn proxy_selection_is_persisted_and_cleans_up_the_previous_node() {
    let root = std::env::temp_dir().join(format!(
      "rsclash-app-selection-{}-{}",
      std::process::id(),
      SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
    ));
    let store = initialize_default_runtime(&root).expect("the default runtime should initialize");
    let uid = "selection-profile";
    let mut transaction = store.begin().expect("the profile transaction should begin");
    transaction
      .add_profile(
        ProfileItem {
          uid: Some(uid.to_string()),
          kind: Some(ProfileKind::Local),
          name: Some("Selection profile".to_string()),
          file: Some(format!("{uid}.yaml")),
          ..ProfileItem::default()
        },
        "mode: rule\nproxies: []\nproxy-groups: []\nrules: []\n",
      )
      .expect("the source profile should stage");
    transaction
      .edit_catalog(|catalog| catalog.current = Some(uid.to_string()))
      .expect("the current profile should stage");
    transaction
      .validate()
      .expect("the profile transaction should validate");
    transaction
      .commit()
      .expect("the profile transaction should commit");
    let node_a = Proxy {
      name: "Node A".to_string(),
      alive: true,
      ..Proxy::default()
    };
    let node_b = Proxy {
      name: "Node B".to_string(),
      alive: true,
      ..Proxy::default()
    };
    let group = Proxy {
      name: "GLOBAL".to_string(),
      kind: "Selector".to_string(),
      all: Some(vec![node_a.name.clone(), node_b.name.clone()]),
      now: Some(node_a.name.clone()),
      ..Proxy::default()
    };
    let fake = FakeMihomoApi::new(FakeMihomoState {
      groups: Groups {
        proxies: vec![group],
        ..Groups::default()
      },
      proxies: Proxies {
        proxies: HashMap::from([(node_a.name.clone(), node_a), (node_b.name.clone(), node_b)]),
        ..Proxies::default()
      },
      connections: Connections {
        connections: Some(vec![rsclash_mihomo::models::Connection {
          id: "old-node-connection".to_string(),
          chains: vec!["Node A".to_string()],
          ..rsclash_mihomo::models::Connection::default()
        }]),
        ..Connections::default()
      },
      ..FakeMihomoState::default()
    });
    let core = CoreRuntime::spawn(
      &tokio::runtime::Handle::current(),
      BridgeController {
        calls: Arc::new(Mutex::new(Vec::new())),
      },
    );
    let recovery: Arc<dyn SystemStateRecovery> = Arc::new(OrderedRecovery {
      order: Arc::new(Mutex::new(Vec::new())),
    });
    let profile_access = ProfileAccess::new(store.clone(), Arc::new(AcceptValidator))
      .expect("profile access should build");
    let backend = BackendHandle::spawn_with_core_integrations(
      &tokio::runtime::Handle::current(),
      WakeHandle::default(),
      core,
      recovery,
      MihomoAccess::same(Arc::new(fake.clone())),
      profile_access,
    );
    let mut client = backend.client();
    client
      .request(UiCommand::StartCore(CoreChannel::Stable))
      .await
      .expect("core start should be accepted");
    wait_for_snapshot(&mut client, |snapshot| {
      snapshot.mihomo.connection == MihomoConnection::Connected
        && snapshot.mihomo.current_proxy() == Some("Node A")
    })
    .await;
    client
      .request(UiCommand::SelectProxy {
        group: "GLOBAL".to_string(),
        proxy: "Node B".to_string(),
      })
      .await
      .expect("proxy selection should be accepted");

    timeout(Duration::from_secs(1), async {
      loop {
        let persisted = store
          .load_catalog()
          .expect("the profile catalog should load")
          .get(uid)
          .and_then(|item| item.selected.as_deref())
          .is_some_and(|selected| {
            selected.iter().any(|selection| {
              selection.name.as_deref() == Some("GLOBAL")
                && selection.now.as_deref() == Some("Node B")
            })
          });
        let closed = fake
          .calls()
          .expect("the fake calls should be available")
          .iter()
          .any(
            |call| matches!(call, MihomoCall::CloseConnection(id) if id == "old-node-connection"),
          );
        if persisted && closed {
          break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .expect("selection persistence and cleanup should complete");

    assert!(backend.shutdown().await.is_ok());
    fs::remove_dir_all(root).expect("the test directory should be removed");
  }

  #[tokio::test]
  async fn shutdown_is_idempotent_at_protocol_level() {
    let backend = BackendHandle::spawn(&tokio::runtime::Handle::current(), WakeHandle::default());
    let mut client = backend.client();

    assert_eq!(
      client.request(UiCommand::Shutdown).await.ok(),
      Some(CommandOutput::ShutdownAccepted)
    );
    assert!(backend.shutdown().await.is_ok());
    assert_eq!(
      client
        .take_snapshot_if_changed()
        .map(|snapshot| snapshot.status),
      Some(AppStatus::ShuttingDown)
    );
    assert!(client.take_snapshot_if_changed().is_none());
  }

  #[tokio::test]
  async fn core_bridge_preserves_command_order_without_blocking_ui_commands() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let core_runtime = CoreRuntime::spawn(
      &tokio::runtime::Handle::current(),
      BridgeController {
        calls: Arc::clone(&calls),
      },
    );
    let backend = BackendHandle::spawn_with_core(
      &tokio::runtime::Handle::current(),
      WakeHandle::default(),
      core_runtime,
    );
    let mut client = backend.client();
    wait_for_snapshot(&mut client, |snapshot| snapshot.status == AppStatus::Ready).await;

    assert_eq!(
      client
        .request(UiCommand::StartCore(CoreChannel::Stable))
        .await
        .ok(),
      Some(CommandOutput::Accepted)
    );
    assert_eq!(
      client.request(UiCommand::ReloadCore).await.ok(),
      Some(CommandOutput::Accepted)
    );
    assert_eq!(
      client
        .request(UiCommand::Navigate(Page::Proxies))
        .await
        .ok(),
      Some(CommandOutput::Accepted)
    );
    wait_for_snapshot(&mut client, |snapshot| snapshot.page == Page::Proxies).await;
    wait_for_snapshot(&mut client, |snapshot| {
      matches!(snapshot.core, CoreState::Running { .. })
    })
    .await;

    let ordered = timeout(Duration::from_secs(1), async {
      loop {
        let calls = calls
          .lock()
          .expect("call log lock should be available")
          .clone();
        if calls.len() >= 2 {
          return calls;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("core commands should finish before the timeout");
    assert_eq!(
      ordered,
      vec![CoreCall::Start(CoreChannel::Stable), CoreCall::Reload]
    );
    assert!(backend.shutdown().await.is_ok());
  }

  #[tokio::test]
  async fn shutdown_restores_system_state_before_stopping_the_core() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let core_runtime = CoreRuntime::spawn(
      &tokio::runtime::Handle::current(),
      OrderedController {
        order: Arc::clone(&order),
      },
    );
    core_runtime
      .handle()
      .start(CoreChannel::Stable)
      .await
      .expect("core should start");
    let backend = BackendHandle::spawn_with_core_and_recovery(
      &tokio::runtime::Handle::current(),
      WakeHandle::default(),
      core_runtime,
      Arc::new(OrderedRecovery {
        order: Arc::clone(&order),
      }),
    );

    assert!(backend.shutdown().await.is_ok());
    assert_eq!(
      *order
        .lock()
        .expect("shutdown order lock should be available"),
      vec!["recovery", "core"]
    );
  }
}
