//! Platform recovery journal and shutdown restoration boundary.

use std::{
  fs::{self, OpenOptions},
  io::{self, Write as _},
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

const RECOVERY_VERSION: u8 = 1;
const TEMP_PREFIX: &str = ".rsclash-recovery-";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReason {
  StartupAudit,
  CleanShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryOutcome {
  NothingPending,
  Restored,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemProxySnapshot {
  pub enabled: bool,
  pub http_proxy: Option<String>,
  pub https_proxy: Option<String>,
  pub socks_proxy: Option<String>,
  pub bypass: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingSystemRecovery {
  pub version: u8,
  pub system_proxy: Option<SystemProxySnapshot>,
  pub tun_enabled_by_app: bool,
}

impl PendingSystemRecovery {
  pub const fn is_empty(&self) -> bool {
    self.system_proxy.is_none() && !self.tun_enabled_by_app
  }
}

impl Default for PendingSystemRecovery {
  fn default() -> Self {
    Self {
      version: RECOVERY_VERSION,
      system_proxy: None,
      tun_enabled_by_app: false,
    }
  }
}

#[derive(Debug, Error)]
pub enum Error {
  #[error("failed to decode the system recovery journal: {0}")]
  Decode(#[source] serde_json::Error),
  #[error("failed to encode the system recovery journal: {0}")]
  Encode(#[source] serde_json::Error),
  #[error("unsupported system recovery: {0}")]
  Unsupported(String),
  #[error("invalid recovery journal: {0}")]
  InvalidJournal(String),
  #[error("failed to {action} {path}: {source}")]
  Io {
    action: &'static str,
    path: PathBuf,
    #[source]
    source: io::Error,
  },
}

pub type Result<T> = std::result::Result<T, Error>;

#[async_trait]
pub trait SystemRecoveryBackend: Send + Sync + 'static {
  async fn restore(&self, pending: &PendingSystemRecovery) -> Result<()>;
}

#[async_trait]
pub trait SystemStateRecovery: Send + Sync + 'static {
  async fn restore_pending(&self, reason: RecoveryReason) -> Result<RecoveryOutcome>;
}

#[derive(Debug)]
pub struct UnavailableRecoveryBackend {
  reason: String,
}

impl UnavailableRecoveryBackend {
  pub fn new(reason: impl Into<String>) -> Self {
    Self {
      reason: reason.into(),
    }
  }
}

#[async_trait]
impl SystemRecoveryBackend for UnavailableRecoveryBackend {
  async fn restore(&self, _pending: &PendingSystemRecovery) -> Result<()> {
    Err(Error::Unsupported(self.reason.clone()))
  }
}

pub struct RecoveryManager {
  store: RecoveryStore,
  backend: Arc<dyn SystemRecoveryBackend>,
  gate: Mutex<()>,
}

impl RecoveryManager {
  pub fn new(path: impl Into<PathBuf>, backend: Arc<dyn SystemRecoveryBackend>) -> Self {
    Self {
      store: RecoveryStore::new(path),
      backend,
      gate: Mutex::new(()),
    }
  }

  pub async fn mark_pending(&self, pending: &PendingSystemRecovery) -> Result<()> {
    let _guard = self.gate.lock().await;
    if pending.is_empty() {
      self.store.clear()
    } else {
      self.store.save(pending)
    }
  }

  pub async fn pending(&self) -> Result<Option<PendingSystemRecovery>> {
    let _guard = self.gate.lock().await;
    self.store.load()
  }

  async fn restore(&self) -> Result<RecoveryOutcome> {
    let _guard = self.gate.lock().await;
    let Some(pending) = self.store.load()? else {
      return Ok(RecoveryOutcome::NothingPending);
    };
    if pending.is_empty() {
      self.store.clear()?;
      return Ok(RecoveryOutcome::NothingPending);
    }
    self.backend.restore(&pending).await?;
    self.store.clear()?;
    Ok(RecoveryOutcome::Restored)
  }
}

#[async_trait]
impl SystemStateRecovery for RecoveryManager {
  async fn restore_pending(&self, _reason: RecoveryReason) -> Result<RecoveryOutcome> {
    self.restore().await
  }
}

#[derive(Debug)]
struct RecoveryStore {
  path: PathBuf,
}

impl RecoveryStore {
  fn new(path: impl Into<PathBuf>) -> Self {
    Self { path: path.into() }
  }

  fn load(&self) -> Result<Option<PendingSystemRecovery>> {
    reject_symlink(&self.path)?;
    let bytes = match fs::read(&self.path) {
      Ok(bytes) => bytes,
      Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
      Err(source) => return Err(io_error("read", &self.path, source)),
    };
    serde_json::from_slice(&bytes)
      .map(Some)
      .map_err(Error::Decode)
  }

  fn save(&self, pending: &PendingSystemRecovery) -> Result<()> {
    let content = serde_json::to_vec_pretty(pending).map_err(Error::Encode)?;
    atomic_write(&self.path, &content)
  }

  fn clear(&self) -> Result<()> {
    reject_symlink(&self.path)?;
    match fs::remove_file(&self.path) {
      Ok(()) => sync_parent(&self.path),
      Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(source) => Err(io_error("remove", &self.path, source)),
    }
  }
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
  let parent = path
    .parent()
    .ok_or_else(|| Error::InvalidJournal(format!("journal has no parent: {}", path.display())))?;
  fs::create_dir_all(parent).map_err(|source| io_error("create directory", parent, source))?;
  restrict_directory(parent)?;
  reject_symlink(path)?;
  let temporary = temporary_path(path);
  let mut file = OpenOptions::new()
    .create_new(true)
    .write(true)
    .open(&temporary)
    .map_err(|source| io_error("create temporary journal", &temporary, source))?;
  restrict_file(&file, &temporary)?;
  if let Err(source) = file.write_all(content).and_then(|()| file.sync_all()) {
    let _ = fs::remove_file(&temporary);
    return Err(io_error("write temporary journal", &temporary, source));
  }
  drop(file);
  if let Err(source) = fs::rename(&temporary, path) {
    let _ = fs::remove_file(&temporary);
    return Err(io_error("replace recovery journal", path, source));
  }
  sync_parent(path)
}

fn reject_symlink(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::InvalidJournal(format!(
      "recovery journal must not be a symbolic link: {}",
      path.display()
    ))),
    Ok(_) => Ok(()),
    Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(source) => Err(io_error("inspect", path, source)),
  }
}

fn sync_parent(path: &Path) -> Result<()> {
  let parent = path
    .parent()
    .ok_or_else(|| Error::InvalidJournal(format!("journal has no parent: {}", path.display())))?;
  let directory =
    fs::File::open(parent).map_err(|source| io_error("open recovery directory", parent, source))?;
  directory
    .sync_all()
    .map_err(|source| io_error("sync recovery directory", parent, source))
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
  use std::os::unix::fs::PermissionsExt as _;

  fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    .map_err(|source| io_error("restrict directory", path, source))
}

#[cfg(not(unix))]
const fn restrict_directory(_path: &Path) -> Result<()> {
  Ok(())
}

#[cfg(unix)]
fn restrict_file(file: &fs::File, path: &Path) -> Result<()> {
  use std::os::unix::fs::PermissionsExt as _;

  file
    .set_permissions(fs::Permissions::from_mode(0o600))
    .map_err(|source| io_error("restrict temporary journal", path, source))
}

#[cfg(not(unix))]
const fn restrict_file(_file: &fs::File, _path: &Path) -> Result<()> {
  Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
  static NEXT_ID: AtomicU64 = AtomicU64::new(0);
  let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
  let name = format!("{TEMP_PREFIX}{}-{id}.tmp", std::process::id());
  match path.parent() {
    Some(parent) => parent.join(name),
    None => PathBuf::from(name),
  }
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> Error {
  Error::Io {
    action,
    path: path.to_path_buf(),
    source,
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    path::PathBuf,
    sync::{
      Arc,
      atomic::{AtomicBool, AtomicUsize, Ordering},
    },
  };

  use async_trait::async_trait;

  use super::{
    Error, PendingSystemRecovery, RecoveryManager, RecoveryOutcome, RecoveryReason, Result,
    SystemRecoveryBackend, SystemStateRecovery as _, UnavailableRecoveryBackend,
  };

  #[derive(Default)]
  struct FakeBackend {
    calls: AtomicUsize,
    fail: AtomicBool,
  }

  #[async_trait]
  impl SystemRecoveryBackend for FakeBackend {
    async fn restore(&self, _pending: &PendingSystemRecovery) -> Result<()> {
      self.calls.fetch_add(1, Ordering::SeqCst);
      if self.fail.load(Ordering::SeqCst) {
        Err(Error::Unsupported("planned failure".to_string()))
      } else {
        Ok(())
      }
    }
  }

  #[tokio::test]
  async fn successful_audit_restores_and_clears_the_marker() {
    let directory = TestDirectory::new();
    let backend = Arc::new(FakeBackend::default());
    let backend_for_manager: Arc<dyn SystemRecoveryBackend> = Arc::<FakeBackend>::clone(&backend);
    let manager = RecoveryManager::new(directory.path.join("recovery.json"), backend_for_manager);
    let pending = PendingSystemRecovery {
      tun_enabled_by_app: true,
      ..PendingSystemRecovery::default()
    };
    manager
      .mark_pending(&pending)
      .await
      .expect("pending state should persist");

    assert_eq!(
      manager
        .restore_pending(RecoveryReason::StartupAudit)
        .await
        .expect("startup audit should restore"),
      RecoveryOutcome::Restored
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(manager.pending().await.ok(), Some(None));
  }

  #[tokio::test]
  async fn failed_recovery_keeps_the_marker_for_the_next_launch() {
    let directory = TestDirectory::new();
    let backend = Arc::new(FakeBackend::default());
    backend.fail.store(true, Ordering::SeqCst);
    let manager = RecoveryManager::new(directory.path.join("recovery.json"), backend);
    let pending = PendingSystemRecovery {
      tun_enabled_by_app: true,
      ..PendingSystemRecovery::default()
    };
    manager
      .mark_pending(&pending)
      .await
      .expect("pending state should persist");

    assert!(
      manager
        .restore_pending(RecoveryReason::CleanShutdown)
        .await
        .is_err()
    );
    assert_eq!(manager.pending().await.ok(), Some(Some(pending)));
  }

  #[tokio::test]
  async fn unavailable_backend_never_silently_clears_pending_state() {
    let directory = TestDirectory::new();
    let manager = RecoveryManager::new(
      directory.path.join("recovery.json"),
      Arc::new(UnavailableRecoveryBackend::new("not implemented")),
    );
    manager
      .mark_pending(&PendingSystemRecovery {
        tun_enabled_by_app: true,
        ..PendingSystemRecovery::default()
      })
      .await
      .expect("pending state should persist");

    assert!(
      manager
        .restore_pending(RecoveryReason::StartupAudit)
        .await
        .is_err()
    );
    assert!(matches!(manager.pending().await, Ok(Some(_))));
  }

  struct TestDirectory {
    path: PathBuf,
  }

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-platform-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&path).expect("test directory should be created");
      Self { path }
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.path);
    }
  }
}
