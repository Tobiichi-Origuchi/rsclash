mod fonts;
#[cfg(target_os = "linux")]
mod linux_bootstrap;
#[cfg(all(feature = "tray", target_os = "linux"))]
mod tray;

use std::{error::Error, time::Duration};

use eframe::egui;
use rsclash_app::{BackendHandle, WakeHandle};
#[cfg(target_os = "linux")]
use rsclash_domain::{CoreChannel, UiCommand};
use rsclash_ui::RsClashUi;
use tokio::runtime::{Builder, Runtime};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn Error>> {
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
  if let Err(error) = runtime.block_on(bootstrap.audit_startup()) {
    error!(%error, "failed to audit pending system state recovery");
  }
  let backend = BackendHandle::spawn_with_core_integrations(
    runtime.handle(),
    wake,
    bootstrap.core_runtime,
    bootstrap.system_recovery,
    bootstrap.mihomo_access,
    bootstrap.profile_access,
  );
  if let Err(error) = backend
    .client()
    .try_command(UiCommand::StartCore(CoreChannel::Stable))
  {
    error!(%error, "failed to queue Mihomo startup");
  }
  backend
}

#[cfg(not(target_os = "linux"))]
fn create_backend(runtime: &Runtime, wake: WakeHandle) -> BackendHandle {
  BackendHandle::spawn(runtime.handle(), wake)
}

struct DesktopApp {
  ui: RsClashUi,
  runtime: Runtime,
  backend: Option<BackendHandle>,
  #[cfg(all(feature = "tray", target_os = "linux"))]
  tray: Option<tray::TrayHandle>,
}

impl eframe::App for DesktopApp {
  fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
    self.ui.logic(context);
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
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rsclash=info"));
  let _ = tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_target(false)
    .compact()
    .try_init();
}
