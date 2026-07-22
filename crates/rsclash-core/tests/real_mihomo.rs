#![cfg(target_os = "linux")]
#![allow(
  clippy::expect_used,
  reason = "integration tests use expect for clear failures"
)]

use std::{
  fs,
  os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
  path::{Path, PathBuf},
  sync::atomic::{AtomicU64, Ordering},
  time::Duration,
};

use rsclash_core::{
  CoreBinaries, CoreHandle, CoreRuntime, LinuxSidecarConfig, LinuxSidecarController,
  SupervisionConfig,
};
use rsclash_domain::{CoreChannel, CoreRunMode, CoreState};
use rustix::process::{Pid, Signal, kill_process};

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
  let runtime = CoreRuntime::spawn_with_config(
    &tokio::runtime::Handle::current(),
    controller,
    SupervisionConfig {
      health_interval: Duration::from_millis(50),
      max_consecutive_health_failures: 1,
      initial_restart_delay: Duration::from_millis(200),
      max_restart_delay: Duration::from_secs(1),
      max_restart_attempts: 3,
      stable_reset_after: Duration::from_secs(5),
    },
  )
  .expect("supervision config should be valid");
  let mut handle = runtime.handle();

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
  let socket_path = runtime_directory.join("controller.sock");
  let pid = find_process_with_argument(&socket_path)
    .expect("the Mihomo process should contain the private socket argument");
  kill_process(pid, Signal::KILL).expect("the Mihomo process should be killable");
  wait_for_state(&mut handle, |state| {
    matches!(state, CoreState::Failed { .. })
  })
  .await;
  wait_for_state(&mut handle, |state| {
    matches!(
      state,
      CoreState::Running {
        channel: CoreChannel::Stable,
        ..
      }
    )
  })
  .await;
  assert!(handle.restart(CoreChannel::Alpha).await.is_ok());
  assert_eq!(handle.stop().await.ok(), Some(CoreState::Stopped));
  assert!(!socket_path.exists());
  assert!(runtime.shutdown().await.is_ok());
}

async fn wait_for_state(
  handle: &mut CoreHandle,
  predicate: impl Fn(&CoreState) -> bool + Send + Sync,
) {
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      let state = handle.current_state();
      if predicate(&state) {
        return;
      }
      handle
        .changed()
        .await
        .expect("the lifecycle state channel should remain open");
    }
  })
  .await
  .expect("the expected lifecycle state should arrive before the timeout");
}

fn find_process_with_argument(argument: &Path) -> Option<Pid> {
  let argument = argument.as_os_str().as_bytes();
  fs::read_dir("/proc")
    .ok()?
    .filter_map(Result::ok)
    .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
    .filter_map(Pid::from_raw)
    .find(|pid| {
      fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|command_line| {
        command_line
          .split(|byte| *byte == 0)
          .any(|item| item == argument)
      })
    })
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

  fn path(&self) -> &Path {
    &self.0
  }
}

impl Drop for TestDirectory {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}
