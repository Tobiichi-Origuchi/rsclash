mod fonts;
#[cfg(all(feature = "tray", target_os = "linux"))]
mod tray;

use std::{error::Error, time::Duration};

use eframe::egui;
use rsclash_app::{BackendHandle, WakeHandle};
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
    renderer: selected_renderer(),
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
      let backend = BackendHandle::spawn(runtime.handle(), wake);
      let client = backend.client();

      #[cfg(all(feature = "tray", target_os = "linux"))]
      let tray = match tray::TrayHandle::new(client.clone()) {
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
      tray.shutdown();
    }

    if let Some(backend) = self.backend.take()
      && let Err(error) = self.runtime.block_on(backend.shutdown())
    {
      error!(%error, "application coordinator did not shut down cleanly");
    }
  }
}

fn selected_renderer() -> eframe::Renderer {
  #[cfg(all(feature = "renderer-glow", feature = "renderer-wgpu"))]
  if std::env::var_os("RSCLASH_RENDERER").as_deref() == Some(std::ffi::OsStr::new("wgpu")) {
    return eframe::Renderer::Wgpu;
  }

  #[cfg(feature = "renderer-glow")]
  {
    eframe::Renderer::Glow
  }

  #[cfg(all(not(feature = "renderer-glow"), feature = "renderer-wgpu"))]
  {
    eframe::Renderer::Wgpu
  }

  #[cfg(not(any(feature = "renderer-glow", feature = "renderer-wgpu")))]
  compile_error!("enable either the `renderer-glow` or `renderer-wgpu` feature");
}

fn init_tracing() {
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rsclash=info"));
  let _ = tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_target(false)
    .compact()
    .try_init();
}
