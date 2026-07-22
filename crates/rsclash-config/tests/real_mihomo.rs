#![cfg(unix)]

use std::{
  fs,
  path::{Path, PathBuf},
  sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use rsclash_config::{
  ActivationMode, CommandRuntimeValidator, MihomoConfig, Result, RuntimeActivator, RuntimeDeployer,
  RuntimeStore,
};

const CONFIG: &str = include_str!("fixtures/minimal-runtime.yaml");

struct ReloadOnlyActivator;

#[async_trait]
impl RuntimeActivator for ReloadOnlyActivator {
  async fn reload(&self, runtime_path: &Path) -> Result<()> {
    if runtime_path.exists() {
      Ok(())
    } else {
      Err(rsclash_config::Error::RuntimeActivation(
        "runtime file is missing".to_string(),
      ))
    }
  }

  async fn restart(&self, _runtime_path: &Path) -> Result<()> {
    Err(rsclash_config::Error::RuntimeActivation(
      "restart should not be needed".to_string(),
    ))
  }
}

#[tokio::test]
#[ignore = "requires a pinned Mihomo binary through RSCLASH_MIHOMO_BIN"]
#[allow(clippy::expect_used)]
async fn pinned_mihomo_validates_staging_before_runtime_commit() {
  let binary = std::env::var_os("RSCLASH_MIHOMO_BIN")
    .expect("RSCLASH_MIHOMO_BIN must point to the pinned Mihomo binary");
  let directory = TestDirectory::new();
  let runtime_path = directory.path.join("runtime.yaml");
  let store = RuntimeStore::open(&runtime_path).expect("runtime store should open");
  let validator = CommandRuntimeValidator::new(binary, &directory.path);
  let config = MihomoConfig::parse(CONFIG).expect("fixture should parse");

  let outcome = RuntimeDeployer::new(&store, &validator, &ReloadOnlyActivator)
    .deploy(&config)
    .await
    .expect("real Mihomo should validate the staged runtime");

  assert_eq!(outcome.activation, ActivationMode::Reload);
  assert!(runtime_path.exists());
  assert!(
    fs::read_dir(&directory.path)
      .expect("test directory should be readable")
      .all(|entry| !entry
        .expect("directory entry should be readable")
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp"))
  );
}

struct TestDirectory {
  path: PathBuf,
}

impl TestDirectory {
  #[allow(clippy::expect_used)]
  fn new() -> Self {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
      "rsclash-real-validation-{}-{}",
      std::process::id(),
      NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).expect("test directory should be created");
    Self { path }
  }
}

impl Drop for TestDirectory {
  fn drop(&mut self) {
    let _ignored = fs::remove_dir_all(&self.path);
  }
}
