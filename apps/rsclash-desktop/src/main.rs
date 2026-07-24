mod fonts;
#[cfg(target_os = "linux")]
mod global_shortcuts;
#[cfg(target_os = "linux")]
mod linux_bootstrap;
#[cfg(target_os = "linux")]
mod logging;
#[cfg(target_os = "linux")]
mod single_instance;
#[cfg(all(feature = "tray", target_os = "linux"))]
mod tray;

use std::{error::Error, time::Duration};

use eframe::egui;
use rsclash_app::{AppClient, BackendHandle, WakeHandle};
use rsclash_domain::{RemoteProfileOptions, UiCommand};
use rsclash_ui::RsClashUi;
use tokio::runtime::{Builder, Runtime};
use tracing::{error, info};
#[cfg(not(target_os = "linux"))]
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn Error>> {
  #[cfg(target_os = "linux")]
  let launch_request = single_instance::LaunchRequest::from_environment();
  #[cfg(target_os = "linux")]
  let mut primary_instance =
    match single_instance::PrimaryInstance::acquire(&launch_request).map_err(io_error)? {
      single_instance::Instance::Primary(instance) => Some(instance),
      single_instance::Instance::Forwarded => return Ok(()),
    };

  init_tracing();

  let runtime = Builder::new_multi_thread()
    .worker_threads(2)
    .thread_name("rsclash-async")
    .enable_all()
    .build()?;

  let native_options = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
      .with_title("rsclash")
      .with_app_id("io.github.rsclash")
      .with_inner_size([1080.0, 720.0])
      .with_min_inner_size([760.0, 520.0]),
    renderer: eframe::Renderer::Glow,
    persist_window: true,
    run_and_return: true,
    ..Default::default()
  };

  info!("starting native egui shell");
  eframe::run_native(
    "rsclash",
    native_options,
    Box::new(move |creation| {
      fonts::install_system_cjk_font(&creation.egui_ctx);

      let repaint_context = creation.egui_ctx.clone();
      let wake = WakeHandle::new(move || repaint_context.request_repaint());
      let backend = create_backend(&runtime, wake);
      let client = backend.client();
      #[cfg(target_os = "linux")]
      dispatch_launch_request(&client, launch_request.clone());
      #[cfg(not(target_os = "linux"))]
      queue_initial_imports(&client);
      #[cfg(target_os = "linux")]
      let instance = primary_instance
        .take()
        .map(|instance| instance.listen(client.clone(), dispatch_launch_request))
        .transpose()
        .map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
      #[cfg(target_os = "linux")]
      let global_shortcuts =
        global_shortcuts::GlobalShortcutsHandle::spawn(runtime.handle(), client.clone());

      #[cfg(all(feature = "tray", target_os = "linux"))]
      let tray = match tray::TrayHandle::new(client.clone(), runtime.handle()) {
        Ok(tray) => Some(tray),
        Err(error) => {
          error!(%error, "failed to initialize the system tray; close-to-tray is disabled");
          None
        },
      };
      #[cfg(all(feature = "tray", target_os = "linux"))]
      let close_to_tray = tray.is_some();
      #[cfg(not(all(feature = "tray", target_os = "linux")))]
      let close_to_tray = false;

      let ui = RsClashUi::new(&creation.egui_ctx, client, close_to_tray);
      Ok(Box::new(DesktopApp {
        ui,
        runtime,
        backend: Some(backend),
        #[cfg(all(feature = "tray", target_os = "linux"))]
        tray,
        #[cfg(target_os = "linux")]
        _instance: instance,
        #[cfg(target_os = "linux")]
        _global_shortcuts: global_shortcuts,
      }))
    }),
  )?;

  Ok(())
}

#[cfg(target_os = "linux")]
fn create_backend(runtime: &Runtime, wake: WakeHandle) -> BackendHandle {
  let bootstrap = match linux_bootstrap::create_core_runtime(runtime.handle()) {
    Ok(bootstrap) => bootstrap,
    Err(error) => {
      error!(%error, "failed to configure the Mihomo sidecar");
      return BackendHandle::spawn(runtime.handle(), wake);
    },
  };
  match runtime.block_on(bootstrap.audit_startup()) {
    Ok(rsclash_platform::RecoveryOutcome::Restored) => {
      let _ = runtime.block_on(bootstrap.desktop.notify(
        "rsclash 已恢复系统设置",
        "检测到上次异常退出，系统代理已安全恢复。",
      ));
    },
    Ok(rsclash_platform::RecoveryOutcome::ExternalChangePreserved) => {
      info!("preserved a system proxy change made outside rsclash");
    },
    Ok(rsclash_platform::RecoveryOutcome::NothingPending) => {},
    Err(error) => {
      error!(%error, "failed to audit pending system state recovery");
    },
  }
  let initial_settings = bootstrap.initial_settings.clone();
  let desktop = std::sync::Arc::clone(&bootstrap.desktop);
  let backend = BackendHandle::spawn_with_linux_integrations(
    runtime.handle(),
    wake,
    bootstrap.core_runtime,
    bootstrap.system_proxy,
    bootstrap.mihomo_access,
    bootstrap.profile_access,
    bootstrap.settings_access,
  );
  apply_initial_settings(runtime, &backend, desktop.as_ref(), &initial_settings);
  backend
}

#[cfg(target_os = "linux")]
fn apply_initial_settings(
  runtime: &Runtime,
  backend: &BackendHandle,
  desktop: &dyn rsclash_platform::DesktopIntegration,
  settings: &rsclash_domain::AppSettings,
) {
  if let Err(error) = runtime.block_on(desktop.run_startup_script(&settings.startup_script)) {
    error!(%error, "startup script failed");
    let _ = runtime.block_on(desktop.notify(
      "rsclash 启动脚本失败",
      "启动脚本没有成功完成，请在设置或应用日志中检查详细信息。",
    ));
  }
  let client = backend.client();
  if let Err(error) = client.try_command(UiCommand::Navigate(settings.start_page)) {
    error!(%error, "failed to apply the configured start page");
  }
  if let Err(error) = backend
    .client()
    .try_command(UiCommand::StartCore(settings.core_channel))
  {
    error!(%error, "failed to queue Mihomo startup");
  }
}

#[cfg(not(target_os = "linux"))]
fn create_backend(runtime: &Runtime, wake: WakeHandle) -> BackendHandle {
  BackendHandle::spawn(runtime.handle(), wake)
}

#[cfg(not(target_os = "linux"))]
fn queue_initial_imports(client: &AppClient) {
  for argument in std::env::args_os().skip(1) {
    queue_import_argument(client, argument);
  }
}

#[cfg(target_os = "linux")]
fn dispatch_launch_request(client: &AppClient, request: single_instance::LaunchRequest) {
  if request.show_window {
    if let Err(error) = client.try_command(UiCommand::SetWindowVisible(true)) {
      error!(%error, "failed to show the window for a launch request");
    }
  } else if let Err(error) = client.try_command(UiCommand::SetWindowVisible(false)) {
    error!(%error, "failed to apply silent launch");
  }
  for argument in request.arguments {
    queue_import_argument(client, argument);
  }
}

fn queue_import_argument(client: &AppClient, argument: std::ffi::OsString) {
  let value = argument.to_string_lossy();
  if value.starts_with("http://")
    || value.starts_with("https://")
    || value.starts_with("rsclash://")
    || value.starts_with("clash://")
    || value.starts_with("clash-verge://")
  {
    if let Err(error) = client.try_command(UiCommand::ImportRemoteProfile {
      name: String::new(),
      url: value.into_owned(),
      options: RemoteProfileOptions::default(),
    }) {
      error!(%error, "failed to queue command-line subscription import");
    }
    return;
  }
  let path = std::path::Path::new(value.as_ref());
  if matches!(
    path.extension().and_then(|extension| extension.to_str()),
    Some("yaml" | "yml")
  ) {
    let name = path
      .file_stem()
      .and_then(|name| name.to_str())
      .unwrap_or("Imported profile")
      .to_string();
    if let Err(error) = client.try_command(UiCommand::ImportLocalProfile {
      name,
      path: value.into_owned(),
    }) {
      error!(%error, "failed to queue command-line profile import");
    }
  }
}

#[cfg(target_os = "linux")]
fn io_error(error: String) -> std::io::Error {
  std::io::Error::other(error)
}

struct DesktopApp {
  ui: RsClashUi,
  runtime: Runtime,
  backend: Option<BackendHandle>,
  #[cfg(all(feature = "tray", target_os = "linux"))]
  tray: Option<tray::TrayHandle>,
  #[cfg(target_os = "linux")]
  _instance: Option<single_instance::InstanceHandle>,
  #[cfg(target_os = "linux")]
  _global_shortcuts: global_shortcuts::GlobalShortcutsHandle,
}

impl eframe::App for DesktopApp {
  fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
    self.ui.logic(context);
    #[cfg(all(feature = "tray", target_os = "linux"))]
    if let Some(tray) = self.tray.as_mut() {
      tray.sync(self.runtime.handle());
    }
  }

  fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    self.ui.ui(ui);
  }

  fn auto_save_interval(&self) -> Duration {
    Duration::from_secs(30)
  }
}

impl Drop for DesktopApp {
  fn drop(&mut self) {
    #[cfg(all(feature = "tray", target_os = "linux"))]
    if let Some(mut tray) = self.tray.take() {
      tray.shutdown(self.runtime.handle());
    }

    if let Some(backend) = self.backend.take()
      && let Err(error) = self.runtime.block_on(backend.shutdown())
    {
      error!(%error, "application coordinator did not shut down cleanly");
    }
  }
}

fn init_tracing() {
  #[cfg(target_os = "linux")]
  {
    logging::init();
  }
  #[cfg(not(target_os = "linux"))]
  {
    let filter =
      EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rsclash=info"));
    let _ = tracing_subscriber::fmt()
      .with_env_filter(filter)
      .with_target(false)
      .compact()
      .try_init();
  }
}
