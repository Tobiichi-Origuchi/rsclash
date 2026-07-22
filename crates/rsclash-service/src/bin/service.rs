use std::error::Error;

#[cfg(target_os = "linux")]
use rsclash_core::{CoreRuntime, LinuxSidecarController};
#[cfg(target_os = "linux")]
use rsclash_service::{
  CoreServiceHandler, DEFAULT_INSTALLED_CONFIG, InstalledServiceConfig, ServiceServer,
};
#[cfg(target_os = "linux")]
use std::{env, path::PathBuf, sync::Arc};
#[cfg(target_os = "linux")]
use tokio::runtime::Handle;
#[cfg(target_os = "linux")]
use tracing::{error, info};
#[cfg(target_os = "linux")]
use tracing_subscriber::EnvFilter;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn Error>> {
  init_tracing();
  let config_path = config_path()?;
  let config = InstalledServiceConfig::load_installed(&config_path)?;
  config.validate_running_uid()?;

  let controller = LinuxSidecarController::new(config.sidecar_config());
  let core_runtime = CoreRuntime::spawn(&Handle::current(), controller);
  let handler = Arc::new(CoreServiceHandler::new(core_runtime.handle()));
  let server = ServiceServer::new(&config.service_socket, config.allowed_uid, handler);
  info!(socket = %config.service_socket.display(), "starting rsclash service");
  let server_result = server.run_until(shutdown_signal()).await;
  let core_result = core_runtime.shutdown().await;
  if let Err(error) = &server_result {
    error!(%error, "service IPC stopped with an error");
  }
  if let Err(error) = &core_result {
    error!(%error, "service core runtime stopped with an error");
  }
  server_result?;
  core_result?;
  Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn Error>> {
  Err("rsclash-service is not implemented on this platform".into())
}

#[cfg(target_os = "linux")]
fn config_path() -> Result<PathBuf, String> {
  let mut arguments = env::args_os().skip(1);
  match (arguments.next(), arguments.next(), arguments.next()) {
    (None, None, None) => Ok(PathBuf::from(DEFAULT_INSTALLED_CONFIG)),
    (Some(flag), Some(path), None) if flag == "--config" => Ok(PathBuf::from(path)),
    _ => Err("usage: rsclash-service [--config PATH]".to_string()),
  }
}

#[cfg(target_os = "linux")]
fn init_tracing() {
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
  let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(target_os = "linux")]
async fn shutdown_signal() {
  use tokio::signal::unix::{SignalKind, signal};

  let interrupt = signal(SignalKind::interrupt());
  let terminate = signal(SignalKind::terminate());
  let (Ok(mut interrupt), Ok(mut terminate)) = (interrupt, terminate) else {
    std::future::pending::<()>().await;
    return;
  };
  tokio::select! {
    _ = interrupt.recv() => {},
    _ = terminate.recv() => {},
  }
}
