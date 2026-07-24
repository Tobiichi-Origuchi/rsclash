use std::{path::PathBuf, process::Stdio, sync::Arc};

use rsclash_config::{
  MihomoConfig, ProfileStore, RuntimeActivator, RuntimeDeployer, RuntimeStore, RuntimeValidator,
  apply_application_settings, validate_application_settings,
};
use rsclash_domain::{
  AppSettings, ApplicationDirectory, ApplicationPathsView, ServiceIntegrationView, SettingsSnapshot,
};
use rsclash_platform::{AppDirectory, DesktopIntegration};
use rsclash_service::ServiceClient;
use tokio::{process::Command, sync::mpsc};

#[derive(Clone, Debug)]
pub struct ServiceInstallAccess {
  pub setup_binary: PathBuf,
  pub installed_config: PathBuf,
  pub stable_core: PathBuf,
  pub alpha_core: Option<PathBuf>,
  pub config_root: PathBuf,
  pub service_socket: PathBuf,
}

#[derive(Clone)]
pub struct SettingsAccess {
  store: ProfileStore,
  validator: Arc<dyn RuntimeValidator>,
  desktop: Arc<dyn DesktopIntegration>,
  service: ServiceInstallAccess,
  paths: ApplicationPathsView,
}

impl SettingsAccess {
  pub fn new(
    store: ProfileStore,
    validator: Arc<dyn RuntimeValidator>,
    desktop: Arc<dyn DesktopIntegration>,
    service: ServiceInstallAccess,
    paths: ApplicationPathsView,
  ) -> Self {
    Self {
      store,
      validator,
      desktop,
      service,
      paths,
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) enum SettingsBridgeCommand {
  Refresh,
  Apply(Box<AppSettings>),
  PersistSystemProxy(bool),
  InstallService,
  UninstallService,
  RegisterDeepLinks,
  OpenDirectory(ApplicationDirectory),
  OpenWebUi,
}

pub(crate) enum SettingsBridgeEvent {
  Snapshot(Box<SettingsSnapshot>),
  CommandFailed(String),
}

struct SettingsWorker {
  access: SettingsAccess,
  activator: Arc<dyn RuntimeActivator>,
  snapshot: SettingsSnapshot,
  event_tx: mpsc::Sender<SettingsBridgeEvent>,
}

impl SettingsWorker {
  fn new(
    access: SettingsAccess,
    activator: Arc<dyn RuntimeActivator>,
    event_tx: mpsc::Sender<SettingsBridgeEvent>,
  ) -> Self {
    Self {
      snapshot: SettingsSnapshot {
        paths: access.paths.clone(),
        ..SettingsSnapshot::default()
      },
      access,
      activator,
      event_tx,
    }
  }

  async fn run(mut self, mut command_rx: mpsc::Receiver<SettingsBridgeCommand>) {
    self.refresh().await;
    while let Some(command) = command_rx.recv().await {
      self.handle_command(command).await;
    }
  }

  async fn handle_command(&mut self, command: SettingsBridgeCommand) {
    match command {
      SettingsBridgeCommand::Refresh => self.refresh().await,
      SettingsBridgeCommand::Apply(settings) => self.apply(*settings).await,
      SettingsBridgeCommand::PersistSystemProxy(enabled) => {
        self.persist_system_proxy(enabled).await;
      },
      SettingsBridgeCommand::InstallService => self.change_service(false).await,
      SettingsBridgeCommand::UninstallService => self.change_service(true).await,
      SettingsBridgeCommand::RegisterDeepLinks => self.register_deep_links().await,
      SettingsBridgeCommand::OpenDirectory(directory) => self.open_directory(directory).await,
      SettingsBridgeCommand::OpenWebUi => {
        if let Err(error) = open_web_ui(&self.snapshot.value).await {
          self.fail(error).await;
        }
      },
    }
  }

  async fn apply(&mut self, settings: AppSettings) {
    self.set_busy(true).await;
    match SettingsChangePlan::new(&self.access, Arc::clone(&self.activator), settings.clone())
      .execute()
      .await
    {
      Ok(summary) => {
        self.snapshot.value = settings;
        self.snapshot.last_applied = Some(summary);
        self.refresh().await;
      },
      Err(error) => self.fail(error).await,
    }
  }

  async fn persist_system_proxy(&mut self, enabled: bool) {
    let mut settings = match self.access.store.load_application_settings() {
      Ok(settings) => settings,
      Err(error) => {
        self.fail(error.to_string()).await;
        return;
      },
    };
    settings.system_proxy_enabled = enabled;
    match self.access.store.save_application_settings(&settings) {
      Ok(()) => {
        self.snapshot.value = settings;
        self.publish().await;
      },
      Err(error) => self.fail(error.to_string()).await,
    }
  }

  async fn change_service(&mut self, uninstall: bool) {
    self.set_busy(true).await;
    match run_service_setup(&self.access.service, uninstall).await {
      Ok(()) => {
        let message = if uninstall {
          "特权服务已卸载".to_string()
        } else {
          "特权服务已安装并启动".to_string()
        };
        self.snapshot.last_applied = Some(message.clone());
        let _ = self
          .access
          .desktop
          .notify("rsclash 特权服务", &message)
          .await;
        self.refresh().await;
      },
      Err(error) => self.fail(error).await,
    }
  }

  async fn register_deep_links(&mut self) {
    self.set_busy(true).await;
    match self.access.desktop.register_deep_links().await {
      Ok(()) => {
        self.snapshot.last_applied = Some("深链协议已注册".to_string());
        self.refresh().await;
      },
      Err(error) => self.fail(error.to_string()).await,
    }
  }

  async fn open_directory(&mut self, directory: ApplicationDirectory) {
    let directory = match directory {
      ApplicationDirectory::Configuration => AppDirectory::Configuration,
      ApplicationDirectory::Data => AppDirectory::Data,
      ApplicationDirectory::Logs => AppDirectory::Logs,
      ApplicationDirectory::Core => AppDirectory::Core,
    };
    if let Err(error) = self.access.desktop.open_directory(directory).await {
      self.fail(error.to_string()).await;
    }
  }

  async fn refresh(&mut self) {
    match self.load_snapshot().await {
      Ok(snapshot) => self.snapshot = snapshot,
      Err(error) => {
        self.snapshot.busy = false;
        self.fail(error).await;
        return;
      },
    }
    self.publish().await;
  }

  async fn load_snapshot(&self) -> Result<SettingsSnapshot, String> {
    let value = self
      .access
      .store
      .load_application_settings()
      .map_err(|error| error.to_string())?;
    let autostart_enabled = self
      .access
      .desktop
      .autostart_enabled()
      .await
      .map_err(|error| error.to_string())?;
    let service = service_status(&self.access.service).await;
    Ok(SettingsSnapshot {
      value,
      busy: false,
      autostart_enabled,
      service,
      paths: self.access.paths.clone(),
      last_applied: self.snapshot.last_applied.clone(),
    })
  }

  async fn set_busy(&mut self, busy: bool) {
    self.snapshot.busy = busy;
    self.publish().await;
  }

  async fn fail(&mut self, error: String) {
    self.snapshot.busy = false;
    self.publish().await;
    let _ = self
      .event_tx
      .send(SettingsBridgeEvent::CommandFailed(error))
      .await;
  }

  async fn publish(&self) {
    let _ = self
      .event_tx
      .send(SettingsBridgeEvent::Snapshot(Box::new(
        self.snapshot.clone(),
      )))
      .await;
  }
}

pub(crate) struct SettingsChangePlan<'a> {
  access: &'a SettingsAccess,
  activator: Arc<dyn RuntimeActivator>,
  next: AppSettings,
}

impl<'a> SettingsChangePlan<'a> {
  fn new(
    access: &'a SettingsAccess,
    activator: Arc<dyn RuntimeActivator>,
    next: AppSettings,
  ) -> Self {
    Self {
      access,
      activator,
      next,
    }
  }

  async fn execute(self) -> Result<String, String> {
    validate_application_settings(&self.next).map_err(|error| error.to_string())?;
    let previous = self
      .access
      .store
      .load_application_settings()
      .map_err(|error| error.to_string())?;
    let runtime_path = &self.access.store.paths().runtime_config;
    let source = tokio::fs::read_to_string(runtime_path)
      .await
      .map_err(|error| format!("read runtime configuration: {error}"))?;
    let mut runtime = MihomoConfig::parse(&source).map_err(|error| error.to_string())?;
    apply_application_settings(&mut runtime, &self.next).map_err(|error| error.to_string())?;
    let runtime_store = RuntimeStore::open(runtime_path).map_err(|error| error.to_string())?;

    self
      .access
      .desktop
      .set_autostart(self.next.auto_launch, self.next.silent_start)
      .await
      .map_err(|error| error.to_string())?;
    if let Err(error) = self.access.store.save_application_settings(&self.next) {
      let _ = self
        .access
        .desktop
        .set_autostart(previous.auto_launch, previous.silent_start)
        .await;
      return Err(error.to_string());
    }
    let deployer = RuntimeDeployer::new(
      &runtime_store,
      self.access.validator.as_ref(),
      self.activator.as_ref(),
    );
    if let Err(error) = deployer.deploy(&runtime).await {
      let settings_restore = self.access.store.save_application_settings(&previous);
      let autostart_restore = self
        .access
        .desktop
        .set_autostart(previous.auto_launch, previous.silent_start)
        .await;
      let compensation = [
        settings_restore.err().map(|error| error.to_string()),
        autostart_restore.err().map(|error| error.to_string()),
      ]
      .into_iter()
      .flatten()
      .collect::<Vec<_>>();
      return if compensation.is_empty() {
        Err(error.to_string())
      } else {
        Err(format!(
          "{error}; settings compensation also failed: {}",
          compensation.join("; ")
        ))
      };
    }
    Ok("设置已验证、保存并应用到 Mihomo".to_string())
  }
}

async fn service_status(access: &ServiceInstallAccess) -> ServiceIntegrationView {
  let installed = access.installed_config.is_file();
  let client = ServiceClient::new(&access.service_socket);
  match client.ping().await {
    Ok(version) => ServiceIntegrationView {
      installed,
      reachable: true,
      version: Some(version),
      ..ServiceIntegrationView::default()
    },
    Err(error) => ServiceIntegrationView {
      installed,
      reachable: false,
      detail: installed.then(|| error.to_string()),
      ..ServiceIntegrationView::default()
    },
  }
}

async fn run_service_setup(access: &ServiceInstallAccess, uninstall: bool) -> Result<(), String> {
  let mut command = Command::new(&access.setup_binary);
  command
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  if uninstall {
    command.arg("--uninstall");
  } else {
    if !access.stable_core.is_absolute() {
      return Err("安装服务需要可验证的 Mihomo 绝对路径".to_string());
    }
    command
      .arg("--stable-core")
      .arg(&access.stable_core)
      .arg("--config-root")
      .arg(&access.config_root);
    if let Some(alpha) = access.alpha_core.as_ref() {
      command.arg("--alpha-core").arg(alpha);
    }
  }
  let output = command
    .output()
    .await
    .map_err(|error| format!("启动特权服务设置程序：{error}"))?;
  if output.status.success() {
    Ok(())
  } else {
    let detail = String::from_utf8_lossy(&output.stderr);
    Err(format!(
      "特权服务设置程序退出状态为 {}：{}",
      output.status,
      detail.trim()
    ))
  }
}

async fn open_web_ui(settings: &AppSettings) -> Result<(), String> {
  if !settings.controller.enabled {
    return Err("请先启用外部控制器".to_string());
  }
  let mut address = settings
    .controller
    .address
    .parse::<std::net::SocketAddr>()
    .map_err(|_| "外部控制器必须使用 IP:端口".to_string())?;
  if address.ip().is_unspecified() {
    address.set_ip(match address.ip() {
      std::net::IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
      std::net::IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
    });
  }
  let url = format!("http://{address}/ui/");
  let status = Command::new("xdg-open")
    .arg(url)
    .status()
    .await
    .map_err(|error| format!("打开外部 Web UI：{error}"))?;
  if status.success() {
    Ok(())
  } else {
    Err(format!("xdg-open 退出状态为 {status}"))
  }
}

pub(crate) async fn run_settings_worker(
  access: SettingsAccess,
  activator: Arc<dyn RuntimeActivator>,
  command_rx: mpsc::Receiver<SettingsBridgeCommand>,
  event_tx: mpsc::Sender<SettingsBridgeEvent>,
) {
  SettingsWorker::new(access, activator, event_tx)
    .run(command_rx)
    .await;
}
