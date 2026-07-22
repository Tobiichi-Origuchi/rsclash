#![cfg(target_os = "linux")]
#![allow(
  clippy::expect_used,
  reason = "integration tests use expect for clear failures"
)]

use std::{
  fs,
  os::unix::fs::PermissionsExt as _,
  path::PathBuf,
  sync::atomic::{AtomicU64, Ordering},
  time::Duration,
};

use rsclash_core::{CoreBinaries, CoreRuntime, LinuxSidecarConfig, LinuxSidecarController};
use rsclash_domain::{CoreChannel, CoreRunMode, CoreState};

const CONFIG: &str = include_str!("../../rsclash-mihomo/tests/fixtures/minimal-config.yaml");

#[tokio::test]
#[ignore = "requires a pinned Mihomo binary through RSCLASH_MIHOMO_BIN"]
async fn pinned_mihomo_runs_through_the_lifecycle_coordinator() {
  let binary = std::env::var_os("RSCLASH_MIHOMO_BIN")
    .expect("RSCLASH_MIHOMO_BIN must point to the pinned Mihomo binary");
  let directory = TestDirectory::new();
  fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
    .expect("test directory permissions should be restricted");
  let data_directory = directory.path().join("data");
  fs::create_dir_all(&data_directory).expect("data directory should be created");
  let config_path = data_directory.join("config.yaml");
  fs::write(&config_path, CONFIG).expect("test config should be written");
  let runtime_directory = directory.path().join("runtime");
  let mut config = LinuxSidecarConfig::new(
    CoreBinaries::new(&binary).with_alpha(&binary),
    &data_directory,
    &config_path,
    &runtime_directory,
  );
  config.startup_timeout = Duration::from_secs(10);
  let controller = LinuxSidecarController::new(config);
  let runtime = CoreRuntime::spawn(&tokio::runtime::Handle::current(), controller);
  let handle = runtime.handle();

  let running = handle
    .start(CoreChannel::Stable)
    .await
    .expect("Mihomo should start");
  assert!(matches!(
    running,
    CoreState::Running {
      mode: CoreRunMode::Sidecar,
      channel: CoreChannel::Stable,
      version: Some(version),
    } if !version.is_empty()
  ));
  handle.reload().await.expect("Mihomo should reload");
  assert!(handle.restart(CoreChannel::Alpha).await.is_ok());
  assert_eq!(handle.stop().await.ok(), Some(CoreState::Stopped));
  assert!(!runtime_directory.join("controller.sock").exists());
  assert!(runtime.shutdown().await.is_ok());
}

struct TestDirectory(PathBuf);

impl TestDirectory {
  fn new() -> Self {
    static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
      "rsclash-core-real-mihomo-{}-{}",
      std::process::id(),
      NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).expect("test directory should be created");
    Self(path)
  }

  fn path(&self) -> &std::path::Path {
    &self.0
  }
}

impl Drop for TestDirectory {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}
