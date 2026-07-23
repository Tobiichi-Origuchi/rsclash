use std::{env, path::PathBuf, sync::Arc};

use rsclash_app::{MihomoAccess, ProfileAccess};
use rsclash_config::{CommandRuntimeValidator, RuntimeValidator, initialize_default_runtime};
use rsclash_core::{
  CoreBinaries, CoreRuntime, LinuxSidecarConfig, LinuxSidecarController, PreferredController,
};
use rsclash_mihomo::{ControllerConfig, ControllerEndpoint, MihomoApi, MihomoClient};
use rsclash_platform::{
  LinuxSystemProxyBackend, RecoveryOutcome, RecoveryReason, SystemProxyBackend, SystemProxyService,
  SystemStateRecovery as _,
};
use rsclash_service::{
  DEFAULT_CONTROLLER_SOCKET, DEFAULT_SERVICE_SOCKET, LinuxServiceController, ServiceClient,
};
use tokio::runtime::Handle;

pub(crate) fn create_core_runtime(runtime: &Handle) -> Result<LinuxBootstrap, String> {
  let home = home_directory()?;
  let config_root = xdg_directory("XDG_CONFIG_HOME")
    .unwrap_or_else(|| home.join(".config"))
    .join("rsclash");
  let data_root = xdg_directory("XDG_DATA_HOME")
    .unwrap_or_else(|| home.join(".local/share"))
    .join("rsclash");
  let runtime_root = xdg_directory("XDG_RUNTIME_DIR")
    .map_or_else(|| config_root.join("run"), |root| root.join("rsclash"));

  let mut binaries = CoreBinaries::new(resolve_stable_binary(&data_root));
  if let Some(alpha) = resolve_alpha_binary(&data_root) {
    binaries = binaries.with_alpha(alpha);
  }
  create_core_runtime_for_layout(
    runtime,
    BootstrapLayout {
      config_root,
      runtime_root,
      binaries,
      use_service: true,
    },
  )
}

struct BootstrapLayout {
  config_root: PathBuf,
  runtime_root: PathBuf,
  binaries: CoreBinaries,
  use_service: bool,
}

pub(crate) struct LinuxBootstrap {
  pub core_runtime: CoreRuntime,
  pub mihomo_access: MihomoAccess,
  pub profile_access: ProfileAccess,
  pub system_proxy: Arc<SystemProxyService>,
}

impl LinuxBootstrap {
  pub(crate) async fn audit_startup(&self) -> rsclash_platform::Result<RecoveryOutcome> {
    self
      .system_proxy
      .restore_pending(RecoveryReason::StartupAudit)
      .await
  }
}

fn create_core_runtime_for_layout(
  runtime: &Handle,
  layout: BootstrapLayout,
) -> Result<LinuxBootstrap, String> {
  let _ = rustls::crypto::ring::default_provider().install_default();
  let store = initialize_default_runtime(&layout.config_root)
    .map_err(|error| format!("initialize the Mihomo runtime configuration: {error}"))?;
  let validator_binary = if std::path::Path::new(rsclash_service::DEFAULT_INSTALLED_CORE).is_file()
  {
    PathBuf::from(rsclash_service::DEFAULT_INSTALLED_CORE)
  } else {
    layout
      .binaries
      .for_channel(rsclash_domain::CoreChannel::Stable)
      .ok_or_else(|| "the stable Mihomo validator binary is missing".to_string())?
      .to_path_buf()
  };

  let sidecar_config = LinuxSidecarConfig::new(
    layout.binaries,
    &store.paths().root,
    &store.paths().runtime_config,
    layout.runtime_root,
  );
  let sidecar_socket = sidecar_config.socket_path();
  let sidecar = LinuxSidecarController::new(sidecar_config);
  let controller = if layout.use_service {
    let service = LinuxServiceController::new(ServiceClient::new(DEFAULT_SERVICE_SOCKET));
    PreferredController::new(sidecar).with_service(service)
  } else {
    PreferredController::new(sidecar)
  };
  let mihomo_access = MihomoAccess::new(
    local_mihomo_client(sidecar_socket)?,
    local_mihomo_client(DEFAULT_CONTROLLER_SOCKET)?,
  );
  let validator: Arc<dyn RuntimeValidator> = Arc::new(CommandRuntimeValidator::new(
    validator_binary,
    &store.paths().root,
  ));
  let system_proxy_backend: Arc<dyn SystemProxyBackend> = Arc::new(LinuxSystemProxyBackend::new());
  let profile_access = ProfileAccess::new(store.clone(), validator)?
    .with_system_proxy_backend(Arc::clone(&system_proxy_backend));
  let system_proxy = Arc::new(SystemProxyService::new(
    store.paths().root.join("system-recovery.json"),
    system_proxy_backend,
  ));
  Ok(LinuxBootstrap {
    core_runtime: CoreRuntime::spawn(runtime, controller),
    mihomo_access,
    profile_access,
    system_proxy,
  })
}

fn local_mihomo_client(socket: impl Into<PathBuf>) -> Result<Arc<dyn MihomoApi>, String> {
  let config = ControllerConfig::local(ControllerEndpoint::unix_socket(socket))
    .with_request_timeout(std::time::Duration::from_secs(2));
  MihomoClient::new(config)
    .map(|client| Arc::new(client) as Arc<dyn MihomoApi>)
    .map_err(|error| format!("configure the Mihomo controller client: {error}"))
}

fn home_directory() -> Result<PathBuf, String> {
  env::var_os("HOME")
    .filter(|home| !home.is_empty())
    .map(PathBuf::from)
    .ok_or_else(|| "HOME is not set; cannot resolve XDG fallback directories".to_string())
}

fn xdg_directory(name: &str) -> Option<PathBuf> {
  env::var_os(name)
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .filter(|path| path.is_absolute())
}

fn resolve_stable_binary(data_root: &std::path::Path) -> PathBuf {
  env::var_os("RSCLASH_MIHOMO_BIN")
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
    .or_else(|| first_existing_binary(data_root, "mihomo"))
    .unwrap_or_else(|| PathBuf::from("mihomo"))
}

fn resolve_alpha_binary(data_root: &std::path::Path) -> Option<PathBuf> {
  env::var_os("RSCLASH_MIHOMO_ALPHA_BIN")
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
    .or_else(|| first_existing_binary(data_root, "mihomo-alpha"))
}

fn first_existing_binary(data_root: &std::path::Path, name: &str) -> Option<PathBuf> {
  let adjacent = env::current_exe()
    .ok()
    .and_then(|executable| executable.parent().map(|parent| parent.join(name)));
  [adjacent, Some(data_root.join("bin").join(name))]
    .into_iter()
    .flatten()
    .find(|path| path.is_file())
}

#[cfg(test)]
#[allow(
  clippy::expect_used,
  reason = "integration tests use expect for clear failures"
)]
mod tests {
  use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
  };

  use rsclash_app::{BackendHandle, WakeHandle};
  use rsclash_core::CoreBinaries;
  use rsclash_domain::{CoreChannel, CoreState, UiCommand};
  use rsclash_platform::RecoveryOutcome;
  use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
  };

  use super::{BootstrapLayout, create_core_runtime_for_layout};

  #[tokio::test]
  #[ignore = "requires a pinned Mihomo binary through RSCLASH_MIHOMO_BIN"]
  async fn empty_layout_generates_config_and_starts_through_the_app_bridge() {
    let binary = std::env::var_os("RSCLASH_MIHOMO_BIN")
      .expect("RSCLASH_MIHOMO_BIN must point to the pinned Mihomo binary");
    let directory = TestDirectory::new();
    let config_root = directory.path().join("config");
    let bootstrap = create_core_runtime_for_layout(
      &tokio::runtime::Handle::current(),
      BootstrapLayout {
        config_root: config_root.clone(),
        runtime_root: directory.path().join("runtime"),
        binaries: CoreBinaries::new(binary),
        use_service: false,
      },
    )
    .expect("empty layout should initialize");
    assert_eq!(
      bootstrap
        .audit_startup()
        .await
        .expect("empty recovery audit should succeed"),
      RecoveryOutcome::NothingPending
    );
    let runtime_path = config_root.join("runtime.yaml");
    let generated = fs::read_to_string(&runtime_path).expect("runtime config should be generated");
    assert!(generated.contains("mixed-port: 17897"));
    assert!(generated.contains("device: rsclash"));
    let mixed_port = available_port();
    fs::write(
      &runtime_path,
      generated.replace("mixed-port: 17897", &format!("mixed-port: {mixed_port}")),
    )
    .expect("integration port should be replaced");

    let backend = BackendHandle::spawn_with_core_integrations(
      &tokio::runtime::Handle::current(),
      WakeHandle::default(),
      bootstrap.core_runtime,
      bootstrap.system_proxy,
      bootstrap.mihomo_access,
      bootstrap.profile_access,
    );
    let mut client = backend.client();
    client
      .request(UiCommand::StartCore(CoreChannel::Stable))
      .await
      .expect("startup command should be accepted");
    tokio::time::timeout(Duration::from_secs(10), async {
      loop {
        if matches!(client.current_snapshot().core, CoreState::Running { .. }) {
          return;
        }
        client
          .changed()
          .await
          .expect("application snapshot channel should remain open");
      }
    })
    .await
    .expect("Mihomo should become ready before the timeout");

    let profile_path = directory.path().join("daily.yaml");
    fs::write(
      &profile_path,
      "mode: global\nproxies: []\nproxy-groups:\n- name: GLOBAL\n  type: select\n  proxies:\n  - DIRECT\nrules:\n- DOMAIN,example.com,DIRECT\n- MATCH,GLOBAL\n",
    )
    .expect("the profile fixture should be written");
    client
      .request(UiCommand::ImportLocalProfile {
        name: "Daily".to_string(),
        path: profile_path.display().to_string(),
      })
      .await
      .expect("profile import should be accepted");
    let uid = tokio::time::timeout(Duration::from_secs(5), async {
      loop {
        if let Some(profile) = client.current_snapshot().profiles.items.first() {
          return profile.uid.clone();
        }
        client
          .changed()
          .await
          .expect("application snapshot channel should remain open");
      }
    })
    .await
    .expect("profile import should finish before the timeout");
    client
      .try_command(UiCommand::ActivateProfile { uid })
      .expect("profile activation should be accepted");
    tokio::time::timeout(Duration::from_secs(10), async {
      loop {
        if client.current_snapshot().profiles.current().is_some() {
          return;
        }
        client
          .changed()
          .await
          .expect("application snapshot channel should remain open");
      }
    })
    .await
    .expect("profile activation should finish before the timeout");
    let activated = fs::read_to_string(&runtime_path).expect("runtime config should be readable");
    assert!(activated.contains(&format!("mixed-port: {mixed_port}")));
    assert!(activated.contains("DOMAIN,example.com,DIRECT"));
    assert_proxy_connection(mixed_port).await;
    assert!(backend.shutdown().await.is_ok());
  }

  async fn assert_proxy_connection(mixed_port: u16) {
    let origin = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("the local origin should bind");
    let origin_address = origin
      .local_addr()
      .expect("the local origin should have an address");
    let server = tokio::spawn(async move {
      let (mut stream, _) = origin
        .accept()
        .await
        .expect("Mihomo should reach the origin");
      let mut request = [0_u8; 2_048];
      let read = stream
        .read(&mut request)
        .await
        .expect("the proxied request should be readable");
      assert!(
        String::from_utf8_lossy(&request[..read]).contains("/rsclash-ready"),
        "the origin should receive the requested path"
      );
      stream
        .write_all(
          b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\nrsclash-proxy-ok",
        )
        .await
        .expect("the origin response should send");
    });

    let mut proxy = tokio::time::timeout(Duration::from_secs(5), async {
      loop {
        match TcpStream::connect(("127.0.0.1", mixed_port)).await {
          Ok(stream) => return stream,
          Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
      }
    })
    .await
    .expect("the mixed listener should accept connections");
    let request = format!(
      "GET http://{origin_address}/rsclash-ready HTTP/1.1\r\nHost: {origin_address}\r\nConnection: close\r\n\r\n"
    );
    proxy
      .write_all(request.as_bytes())
      .await
      .expect("the proxy request should send");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), proxy.read_to_end(&mut response))
      .await
      .expect("the proxy response should arrive")
      .expect("the proxy response should be readable");
    assert!(
      String::from_utf8_lossy(&response).contains("rsclash-proxy-ok"),
      "the request should complete through Mihomo's mixed listener"
    );
    server.await.expect("the local origin should finish");
  }

  fn available_port() -> u16 {
    let listener =
      std::net::TcpListener::bind("127.0.0.1:0").expect("an integration port should be available");
    listener
      .local_addr()
      .expect("the integration listener should have an address")
      .port()
  }

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-desktop-bootstrap-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&path).expect("test directory should be created");
      Self(path)
    }

    fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.0);
    }
  }
}
