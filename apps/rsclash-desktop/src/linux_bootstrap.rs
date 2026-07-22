use std::{env, path::PathBuf, sync::Arc};

use rsclash_config::{MihomoConfig, ProfileStore, RuntimeStore};
use rsclash_core::{
  CoreBinaries, CoreRuntime, LinuxSidecarConfig, LinuxSidecarController, PreferredController,
};
use rsclash_platform::{
  RecoveryManager, RecoveryOutcome, RecoveryReason, SystemStateRecovery as _,
  UnavailableRecoveryBackend,
};
use rsclash_service::{DEFAULT_SERVICE_SOCKET, LinuxServiceController, ServiceClient};
use tokio::runtime::Handle;

const DEFAULT_RUNTIME_CONFIG: &str = r"mixed-port: 7897
allow-lan: false
mode: rule
log-level: info
ipv6: false
proxies: []
proxy-groups:
  - name: GLOBAL
    type: select
    proxies:
      - DIRECT
      - REJECT
rules:
  - MATCH,GLOBAL
";

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
    },
  )
}

struct BootstrapLayout {
  config_root: PathBuf,
  runtime_root: PathBuf,
  binaries: CoreBinaries,
}

pub(crate) struct LinuxBootstrap {
  pub core_runtime: CoreRuntime,
  pub system_recovery: Arc<RecoveryManager>,
}

impl LinuxBootstrap {
  pub(crate) async fn audit_startup(&self) -> rsclash_platform::Result<RecoveryOutcome> {
    self
      .system_recovery
      .restore_pending(RecoveryReason::StartupAudit)
      .await
  }
}

fn create_core_runtime_for_layout(
  runtime: &Handle,
  layout: BootstrapLayout,
) -> Result<LinuxBootstrap, String> {
  let store = ProfileStore::open(&layout.config_root)
    .map_err(|error| format!("open the rsclash configuration directory: {error}"))?;
  let runtime_store = RuntimeStore::open(&store.paths().runtime_config)
    .map_err(|error| format!("open the Mihomo runtime configuration: {error}"))?;
  let config = MihomoConfig::parse(DEFAULT_RUNTIME_CONFIG)
    .map_err(|error| format!("parse the built-in runtime configuration: {error}"))?;
  runtime_store
    .initialize_if_missing(&config)
    .map_err(|error| format!("initialize the Mihomo runtime configuration: {error}"))?;

  let sidecar = LinuxSidecarController::new(LinuxSidecarConfig::new(
    layout.binaries,
    &store.paths().root,
    &store.paths().runtime_config,
    layout.runtime_root,
  ));
  let service = LinuxServiceController::new(
    ServiceClient::new(DEFAULT_SERVICE_SOCKET).with_timeout(std::time::Duration::from_millis(250)),
  );
  let controller = PreferredController::new(sidecar).with_service(service);
  let system_recovery = Arc::new(RecoveryManager::new(
    store.paths().root.join("system-recovery.json"),
    Arc::new(UnavailableRecoveryBackend::new(
      "Linux system proxy and TUN recovery are not implemented yet",
    )),
  ));
  Ok(LinuxBootstrap {
    core_runtime: CoreRuntime::spawn(runtime, controller),
    system_recovery,
  })
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
    assert!(generated.contains("mixed-port: 7897"));
    fs::write(
      &runtime_path,
      generated.replace("mixed-port: 7897", "mixed-port: 0"),
    )
    .expect("integration port should be replaced");

    let backend = BackendHandle::spawn_with_core_and_recovery(
      &tokio::runtime::Handle::current(),
      WakeHandle::default(),
      bootstrap.core_runtime,
      bootstrap.system_recovery,
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
    assert!(backend.shutdown().await.is_ok());
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
