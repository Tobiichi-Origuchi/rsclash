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

#[cfg(target_os = "linux")]
mod linux_desktop;
#[cfg(target_os = "linux")]
mod linux_proxy;
#[cfg(target_os = "linux")]
pub use linux_desktop::{LinuxDesktopIntegration, LinuxDesktopPaths};
#[cfg(target_os = "linux")]
pub use linux_proxy::LinuxSystemProxyBackend;

const RECOVERY_VERSION: u8 = 2;
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
  ExternalChangePreserved,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemProxySnapshot {
  pub enabled: bool,
  pub backend: Option<String>,
  pub mode: Option<String>,
  pub http_proxy: Option<String>,
  pub https_proxy: Option<String>,
  pub socks_proxy: Option<String>,
  pub bypass: Vec<String>,
  pub auto_config_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingSystemRecovery {
  pub version: u8,
  pub system_proxy: Option<SystemProxySnapshot>,
  pub system_proxy_target: Option<SystemProxySnapshot>,
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
      system_proxy_target: None,
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
  #[error("platform operation failed: {0}")]
  Platform(String),
  #[error("failed to {action} {path}: {source}")]
  Io {
    action: &'static str,
    path: PathBuf,
    #[source]
    source: io::Error,
  },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppDirectory {
  Configuration,
  Data,
  Logs,
  Core,
}

#[async_trait]
pub trait DesktopIntegration: Send + Sync + 'static {
  async fn autostart_enabled(&self) -> Result<bool>;
  async fn set_autostart(&self, enabled: bool, silent: bool) -> Result<()>;
  async fn register_deep_links(&self) -> Result<()>;
  async fn open_directory(&self, directory: AppDirectory) -> Result<()>;
  async fn notify(&self, title: &str, body: &str) -> Result<()>;
  async fn run_startup_script(&self, script: &str) -> Result<()>;
}

#[async_trait]
pub trait SystemRecoveryBackend: Send + Sync + 'static {
  async fn restore(&self, pending: &PendingSystemRecovery) -> Result<()>;
}

#[async_trait]
pub trait SystemProxyBackend: SystemRecoveryBackend {
  fn name(&self) -> &'static str;
  async fn current(&self) -> Result<SystemProxySnapshot>;
  async fn apply(&self, snapshot: &SystemProxySnapshot) -> Result<()>;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemProxyStatus {
  pub backend: String,
  pub enabled_by_app: bool,
  pub applied: bool,
}

pub struct SystemProxyService {
  recovery: RecoveryManager,
  backend: Arc<dyn SystemProxyBackend>,
  target: Mutex<Option<SystemProxySnapshot>>,
  gate: Mutex<()>,
}

impl SystemProxyService {
  pub fn new(recovery_path: impl Into<PathBuf>, backend: Arc<dyn SystemProxyBackend>) -> Self {
    let recovery_backend: Arc<dyn SystemRecoveryBackend> =
      Arc::<dyn SystemProxyBackend>::clone(&backend);
    Self {
      recovery: RecoveryManager::new(recovery_path, recovery_backend),
      backend,
      target: Mutex::new(None),
      gate: Mutex::new(()),
    }
  }

  pub async fn status(&self) -> Result<SystemProxyStatus> {
    let current = self.backend.current().await?;
    let target = self.target.lock().await;
    Ok(SystemProxyStatus {
      backend: self.backend.name().to_string(),
      enabled_by_app: target.is_some(),
      applied: target.as_ref().is_some_and(|target| target == &current),
    })
  }

  pub async fn enable(&self, host: &str, port: u16, bypass: Vec<String>) -> Result<()> {
    if host.is_empty() || port == 0 {
      return Err(Error::Platform(
        "system proxy host and port must be valid".to_string(),
      ));
    }
    let endpoint = format_endpoint(host, port);
    self
      .enable_target(|original| SystemProxySnapshot {
        enabled: true,
        backend: Some(self.backend.name().to_string()),
        mode: Some("manual".to_string()),
        http_proxy: Some(endpoint.clone()),
        https_proxy: Some(endpoint.clone()),
        socks_proxy: Some(endpoint),
        bypass,
        auto_config_url: original.auto_config_url.clone(),
      })
      .await
  }

  pub async fn enable_pac(&self, url: &str, bypass: Vec<String>) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
      return Err(Error::Platform(
        "PAC URL must use HTTP or HTTPS".to_string(),
      ));
    }
    self
      .enable_target(|original| SystemProxySnapshot {
        enabled: true,
        backend: Some(self.backend.name().to_string()),
        mode: Some("auto".to_string()),
        http_proxy: original.http_proxy.clone(),
        https_proxy: original.https_proxy.clone(),
        socks_proxy: original.socks_proxy.clone(),
        bypass,
        auto_config_url: Some(url.to_string()),
      })
      .await
  }

  async fn enable_target(
    &self,
    target: impl FnOnce(&SystemProxySnapshot) -> SystemProxySnapshot,
  ) -> Result<()> {
    let _guard = self.gate.lock().await;
    if self.recovery.pending().await?.is_some() {
      return Err(Error::InvalidJournal(
        "pending system state must be restored before enabling the proxy".to_string(),
      ));
    }
    let original = self.backend.current().await?;
    let target = target(&original);
    self
      .recovery
      .mark_pending(&PendingSystemRecovery {
        system_proxy: Some(original.clone()),
        system_proxy_target: Some(target.clone()),
        ..PendingSystemRecovery::default()
      })
      .await?;
    if let Err(apply_error) = self.backend.apply(&target).await {
      return match self.backend.apply(&original).await {
        Ok(()) => {
          self
            .recovery
            .mark_pending(&PendingSystemRecovery::default())
            .await?;
          Err(apply_error)
        },
        Err(restore_error) => Err(Error::Platform(format!(
          "{apply_error}; restoring the previous system proxy also failed: {restore_error}"
        ))),
      };
    }
    *self.target.lock().await = Some(target);
    Ok(())
  }

  pub async fn disable(&self) -> Result<RecoveryOutcome> {
    let _guard = self.gate.lock().await;
    let outcome = self.restore_if_owned(RecoveryReason::CleanShutdown).await?;
    *self.target.lock().await = None;
    Ok(outcome)
  }

  pub async fn pending(&self) -> Result<Option<PendingSystemRecovery>> {
    self.recovery.pending().await
  }

  async fn restore_if_owned(&self, reason: RecoveryReason) -> Result<RecoveryOutcome> {
    let Some(pending) = self.recovery.pending().await? else {
      return Ok(RecoveryOutcome::NothingPending);
    };
    if let Some(target) = pending.system_proxy_target.as_ref()
      && self.backend.current().await? != *target
    {
      self.recovery.clear_pending().await?;
      return Ok(RecoveryOutcome::ExternalChangePreserved);
    }
    self.recovery.restore_pending(reason).await
  }
}

#[async_trait]
impl SystemStateRecovery for SystemProxyService {
  async fn restore_pending(&self, reason: RecoveryReason) -> Result<RecoveryOutcome> {
    let _guard = self.gate.lock().await;
    let outcome = self.restore_if_owned(reason).await?;
    *self.target.lock().await = None;
    Ok(outcome)
  }
}

fn format_endpoint(host: &str, port: u16) -> String {
  if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
    format!("[{host}]:{port}")
  } else {
    format!("{host}:{port}")
  }
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

  pub async fn clear_pending(&self) -> Result<()> {
    let _guard = self.gate.lock().await;
    self.store.clear()
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
      Arc, Mutex,
      atomic::{AtomicBool, AtomicUsize, Ordering},
    },
  };

  use async_trait::async_trait;

  use super::{
    Error, PendingSystemRecovery, RecoveryManager, RecoveryOutcome, RecoveryReason, Result,
    SystemProxyBackend, SystemProxyService, SystemProxySnapshot, SystemRecoveryBackend,
    SystemStateRecovery as _, UnavailableRecoveryBackend,
  };

  #[derive(Default)]
  struct FakeBackend {
    calls: AtomicUsize,
    fail: AtomicBool,
  }

  struct FakeProxyBackend {
    current: Mutex<SystemProxySnapshot>,
    fail_manual: AtomicBool,
  }

  impl FakeProxyBackend {
    fn new(current: SystemProxySnapshot) -> Self {
      Self {
        current: Mutex::new(current),
        fail_manual: AtomicBool::new(false),
      }
    }
  }

  #[async_trait]
  impl SystemRecoveryBackend for FakeProxyBackend {
    async fn restore(&self, pending: &PendingSystemRecovery) -> Result<()> {
      if let Some(snapshot) = &pending.system_proxy {
        self.apply(snapshot).await?;
      }
      Ok(())
    }
  }

  #[async_trait]
  impl SystemProxyBackend for FakeProxyBackend {
    fn name(&self) -> &'static str {
      "fake"
    }

    async fn current(&self) -> Result<SystemProxySnapshot> {
      Ok(self.current.lock().expect("proxy lock should open").clone())
    }

    async fn apply(&self, snapshot: &SystemProxySnapshot) -> Result<()> {
      if self.fail_manual.load(Ordering::SeqCst) && snapshot.mode.as_deref() == Some("manual") {
        return Err(Error::Platform("planned apply failure".to_string()));
      }
      *self.current.lock().expect("proxy lock should open") = snapshot.clone();
      Ok(())
    }
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

  #[tokio::test]
  async fn system_proxy_service_restores_the_exact_previous_state() {
    let directory = TestDirectory::new();
    let original = SystemProxySnapshot {
      enabled: false,
      backend: Some("fake".to_string()),
      mode: Some("auto".to_string()),
      auto_config_url: Some("https://example.test/proxy.pac".to_string()),
      bypass: vec!["localhost".to_string()],
      ..SystemProxySnapshot::default()
    };
    let backend = Arc::new(FakeProxyBackend::new(original.clone()));
    let service = SystemProxyService::new(
      directory.path.join("recovery.json"),
      Arc::<FakeProxyBackend>::clone(&backend),
    );

    service
      .enable("127.0.0.1", 17_897, vec!["localhost".to_string()])
      .await
      .expect("system proxy should enable");
    assert!(service.status().await.expect("status should read").applied);
    assert!(matches!(service.pending().await, Ok(Some(_))));

    assert_eq!(
      service.disable().await.ok(),
      Some(RecoveryOutcome::Restored)
    );
    assert_eq!(
      backend.current().await.expect("proxy should read"),
      original
    );
    assert!(matches!(service.pending().await, Ok(None)));
  }

  #[tokio::test]
  async fn system_proxy_service_preserves_external_changes() {
    let directory = TestDirectory::new();
    let original = SystemProxySnapshot {
      backend: Some("fake".to_string()),
      mode: Some("none".to_string()),
      ..SystemProxySnapshot::default()
    };
    let backend = Arc::new(FakeProxyBackend::new(original));
    let service = SystemProxyService::new(
      directory.path.join("recovery.json"),
      Arc::<FakeProxyBackend>::clone(&backend),
    );
    service
      .enable("127.0.0.1", 17_897, Vec::new())
      .await
      .expect("system proxy should enable");
    let external = SystemProxySnapshot {
      enabled: true,
      backend: Some("fake".to_string()),
      mode: Some("manual".to_string()),
      http_proxy: Some("external.example:8080".to_string()),
      ..SystemProxySnapshot::default()
    };
    backend
      .apply(&external)
      .await
      .expect("external settings should apply");

    assert_eq!(
      service.disable().await.ok(),
      Some(RecoveryOutcome::ExternalChangePreserved)
    );
    assert_eq!(
      backend.current().await.expect("proxy should read"),
      external
    );
    assert!(matches!(service.pending().await, Ok(None)));
  }

  #[tokio::test]
  async fn failed_enable_compensates_and_clears_the_journal() {
    let directory = TestDirectory::new();
    let original = SystemProxySnapshot {
      backend: Some("fake".to_string()),
      mode: Some("none".to_string()),
      ..SystemProxySnapshot::default()
    };
    let backend = Arc::new(FakeProxyBackend::new(original.clone()));
    backend.fail_manual.store(true, Ordering::SeqCst);
    let service = SystemProxyService::new(
      directory.path.join("recovery.json"),
      Arc::<FakeProxyBackend>::clone(&backend),
    );

    assert!(
      service
        .enable("127.0.0.1", 17_897, Vec::new())
        .await
        .is_err()
    );
    assert_eq!(
      backend.current().await.expect("proxy should read"),
      original
    );
    assert!(matches!(service.pending().await, Ok(None)));
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
