use std::{
  collections::BTreeMap,
  fs,
  path::{Path, PathBuf},
  process::Stdio,
  time::Duration,
};

use async_trait::async_trait;
use tokio::{process::Command, time::timeout};

use crate::{
  Error, MihomoConfig, Result,
  store::{
    RollbackJournal, atomic_write, create_staging_file, read_bytes_if_exists,
    recover_pending_transactions, remove_file,
  },
};

const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

#[async_trait]
pub trait RuntimeValidator: Send + Sync {
  async fn validate(&self, staging_path: &Path) -> Result<()>;
}

#[async_trait]
pub trait RuntimeActivator: Send + Sync {
  async fn reload(&self, runtime_path: &Path) -> Result<()>;
  async fn restart(&self, runtime_path: &Path) -> Result<()>;
}

#[derive(Clone, Debug)]
pub struct CommandRuntimeValidator {
  binary: PathBuf,
  data_directory: PathBuf,
  timeout: Duration,
}

impl CommandRuntimeValidator {
  #[must_use]
  pub fn new(binary: impl Into<PathBuf>, data_directory: impl Into<PathBuf>) -> Self {
    Self {
      binary: binary.into(),
      data_directory: data_directory.into(),
      timeout: Duration::from_secs(15),
    }
  }

  #[must_use]
  pub const fn with_timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }

  pub fn binary(&self) -> &Path {
    &self.binary
  }

  pub fn data_directory(&self) -> &Path {
    &self.data_directory
  }
}

#[async_trait]
impl RuntimeValidator for CommandRuntimeValidator {
  async fn validate(&self, staging_path: &Path) -> Result<()> {
    let mut command = Command::new(&self.binary);
    command
      .args(["-t", "-d"])
      .arg(&self.data_directory)
      .arg("-f")
      .arg(staging_path)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true);
    let output = timeout(self.timeout, command.output())
      .await
      .map_err(|_| {
        Error::RuntimeValidation(format!("validator timed out after {:?}", self.timeout))
      })?
      .map_err(|source| Error::io("run Mihomo validator", &self.binary, source))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let has_fatal_output = ["FATA", "fatal", "Parse config error", "level=fatal"]
      .iter()
      .any(|keyword| stdout.contains(keyword) || stderr.contains(keyword));
    if output.status.success() && !has_fatal_output {
      return Ok(());
    }

    let diagnostic = if stderr.trim().is_empty() {
      stdout.as_ref()
    } else {
      stderr.as_ref()
    };
    let diagnostic = truncate_diagnostic(diagnostic);
    let message = if diagnostic.trim().is_empty() {
      output.status.code().map_or_else(
        || "validator was terminated".to_string(),
        |code| format!("validator exited with status {code}"),
      )
    } else {
      diagnostic
    };
    Err(Error::RuntimeValidation(message))
  }
}

fn truncate_diagnostic(diagnostic: &str) -> String {
  if diagnostic.len() <= MAX_DIAGNOSTIC_BYTES {
    return diagnostic.trim().to_string();
  }
  let mut boundary = MAX_DIAGNOSTIC_BYTES;
  while !diagnostic.is_char_boundary(boundary) {
    boundary = boundary.saturating_sub(1);
  }
  format!("{}\n… output truncated", diagnostic[..boundary].trim())
}

#[derive(Clone, Debug)]
pub struct RuntimeStore {
  runtime_path: PathBuf,
}

impl RuntimeStore {
  pub fn open(runtime_path: impl Into<PathBuf>) -> Result<Self> {
    let runtime_path = runtime_path.into();
    let root = runtime_path.parent().ok_or_else(|| {
      Error::InvalidConfiguration(format!(
        "{} has no parent directory",
        runtime_path.display()
      ))
    })?;
    recover_pending_transactions(root)?;
    Ok(Self { runtime_path })
  }

  pub fn path(&self) -> &Path {
    &self.runtime_path
  }

  pub fn initialize_if_missing(&self, config: &MihomoConfig) -> Result<bool> {
    if read_bytes_if_exists(&self.runtime_path)?.is_some() {
      return Ok(false);
    }
    atomic_write(&self.runtime_path, config.to_yaml()?.as_bytes())?;
    Ok(true)
  }

  async fn prepare(
    &self,
    config: &MihomoConfig,
    validator: &dyn RuntimeValidator,
  ) -> Result<PreparedRuntime> {
    let content = config.to_yaml()?.into_bytes();
    let previous = read_bytes_if_exists(&self.runtime_path)?;
    let staging_path = create_staging_file(&self.runtime_path, &content)?;
    let prepared = PreparedRuntime {
      runtime_path: self.runtime_path.clone(),
      staging_path,
      content,
      previous,
      committed: false,
    };
    validator.validate(prepared.staging_path()).await?;
    Ok(prepared)
  }

  fn create_journal(&self, prepared: &PreparedRuntime) -> Result<RollbackJournal> {
    let root = self.runtime_path.parent().ok_or_else(|| {
      Error::InvalidConfiguration(format!(
        "{} has no parent directory",
        self.runtime_path.display()
      ))
    })?;
    let snapshots = BTreeMap::from([(self.runtime_path.clone(), prepared.previous.clone())]);
    RollbackJournal::create(root, &snapshots)
  }
}

struct PreparedRuntime {
  runtime_path: PathBuf,
  staging_path: PathBuf,
  content: Vec<u8>,
  previous: Option<Vec<u8>>,
  committed: bool,
}

impl PreparedRuntime {
  fn staging_path(&self) -> &Path {
    &self.staging_path
  }

  fn commit_file(&mut self) -> Result<()> {
    atomic_write(&self.runtime_path, &self.content)?;
    remove_file(&self.staging_path)?;
    self.committed = true;
    Ok(())
  }

  fn compensate_file(&self) -> Result<()> {
    match &self.previous {
      Some(previous) => atomic_write(&self.runtime_path, previous),
      None => remove_file(&self.runtime_path),
    }
  }

  const fn had_previous(&self) -> bool {
    self.previous.is_some()
  }
}

impl Drop for PreparedRuntime {
  fn drop(&mut self) {
    if !self.committed {
      let _ignored = fs::remove_file(&self.staging_path);
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationMode {
  Reload,
  Restart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentOutcome {
  pub activation: ActivationMode,
  pub runtime_path: PathBuf,
}

pub struct RuntimeDeployer<'a> {
  store: &'a RuntimeStore,
  validator: &'a dyn RuntimeValidator,
  activator: &'a dyn RuntimeActivator,
}

impl<'a> RuntimeDeployer<'a> {
  #[must_use]
  pub const fn new(
    store: &'a RuntimeStore,
    validator: &'a dyn RuntimeValidator,
    activator: &'a dyn RuntimeActivator,
  ) -> Self {
    Self {
      store,
      validator,
      activator,
    }
  }

  pub async fn deploy(&self, config: &MihomoConfig) -> Result<DeploymentOutcome> {
    let mut prepared = self.store.prepare(config, self.validator).await?;
    let journal = self.store.create_journal(&prepared)?;
    if let Err(commit_error) = prepared.commit_file() {
      return self.handle_commit_failure(&prepared, &journal, commit_error);
    }

    match self.activator.reload(self.store.path()).await {
      Ok(()) => {
        self
          .finish_success(&prepared, &journal, ActivationMode::Reload)
          .await
      },
      Err(reload_error) => match self.activator.restart(self.store.path()).await {
        Ok(()) => {
          self
            .finish_success(&prepared, &journal, ActivationMode::Restart)
            .await
        },
        Err(restart_error) => {
          let activation_error =
            format!("reload failed: {reload_error}; controlled restart failed: {restart_error}");
          self.compensate(&prepared, &activation_error).await?;
          if let Err(compensation_error) = journal.complete() {
            return Err(Error::DeploymentCompensation {
              activation_error,
              compensation_error: compensation_error.to_string(),
            });
          }
          Err(Error::RuntimeActivation(activation_error))
        },
      },
    }
  }

  async fn finish_success(
    &self,
    prepared: &PreparedRuntime,
    journal: &RollbackJournal,
    activation: ActivationMode,
  ) -> Result<DeploymentOutcome> {
    match journal.complete() {
      Ok(()) => Ok(self.outcome(activation)),
      Err(error) => {
        let activation_error = format!("failed to finalize runtime transaction: {error}");
        self.compensate(prepared, &activation_error).await?;
        if let Err(compensation_error) = journal.complete() {
          return Err(Error::DeploymentCompensation {
            activation_error,
            compensation_error: compensation_error.to_string(),
          });
        }
        Err(Error::RuntimeActivation(activation_error))
      },
    }
  }

  fn handle_commit_failure(
    &self,
    prepared: &PreparedRuntime,
    journal: &RollbackJournal,
    commit_error: Error,
  ) -> Result<DeploymentOutcome> {
    if let Err(compensation_error) = prepared.compensate_file() {
      return Err(Error::DeploymentCompensation {
        activation_error: commit_error.to_string(),
        compensation_error: compensation_error.to_string(),
      });
    }
    if let Err(compensation_error) = journal.complete() {
      return Err(Error::DeploymentCompensation {
        activation_error: commit_error.to_string(),
        compensation_error: compensation_error.to_string(),
      });
    }
    Err(commit_error)
  }

  fn outcome(&self, activation: ActivationMode) -> DeploymentOutcome {
    DeploymentOutcome {
      activation,
      runtime_path: self.store.path().to_path_buf(),
    }
  }

  async fn compensate(&self, prepared: &PreparedRuntime, activation_error: &str) -> Result<()> {
    if let Err(error) = prepared.compensate_file() {
      return Err(Error::DeploymentCompensation {
        activation_error: activation_error.to_string(),
        compensation_error: error.to_string(),
      });
    }
    if !prepared.had_previous() {
      return Ok(());
    }

    if let Err(reload_error) = self.activator.reload(self.store.path()).await
      && let Err(restart_error) = self.activator.restart(self.store.path()).await
    {
      return Err(Error::DeploymentCompensation {
        activation_error: activation_error.to_string(),
        compensation_error: format!(
          "old config reload failed: {reload_error}; old config restart failed: {restart_error}"
        ),
      });
    }
    Ok(())
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
  };

  use async_trait::async_trait;

  use crate::{Error, MihomoConfig, Result};

  use super::{
    ActivationMode, CommandRuntimeValidator, RuntimeActivator, RuntimeDeployer, RuntimeStore,
    RuntimeValidator,
  };

  struct AcceptValidator;

  #[async_trait]
  impl RuntimeValidator for AcceptValidator {
    async fn validate(&self, staging_path: &Path) -> Result<()> {
      let content = fs::read_to_string(staging_path)
        .map_err(|source| Error::io("read test staging file", staging_path, source))?;
      if content.contains("mode: rule") {
        Ok(())
      } else {
        Err(Error::RuntimeValidation("missing mode".to_string()))
      }
    }
  }

  struct RejectValidator;

  #[async_trait]
  impl RuntimeValidator for RejectValidator {
    async fn validate(&self, _staging_path: &Path) -> Result<()> {
      Err(Error::RuntimeValidation("rejected".to_string()))
    }
  }

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  enum Call {
    Reload,
    Restart,
  }

  #[derive(Default)]
  struct FakeActivator {
    calls: Arc<Mutex<Vec<Call>>>,
    failures: Arc<Mutex<Vec<bool>>>,
  }

  impl FakeActivator {
    fn with_failures(failures: Vec<bool>) -> Self {
      Self {
        calls: Arc::default(),
        failures: Arc::new(Mutex::new(failures)),
      }
    }

    fn calls(&self) -> Vec<Call> {
      self.calls.lock().expect("calls lock should open").clone()
    }

    fn call(&self, call: Call) -> Result<()> {
      self
        .calls
        .lock()
        .expect("calls lock should open")
        .push(call);
      let fail = if self
        .failures
        .lock()
        .expect("failures lock should open")
        .is_empty()
      {
        false
      } else {
        self
          .failures
          .lock()
          .expect("failures lock should open")
          .remove(0)
      };
      if fail {
        Err(Error::RuntimeActivation(format!("{call:?} failed")))
      } else {
        Ok(())
      }
    }
  }

  #[async_trait]
  impl RuntimeActivator for FakeActivator {
    async fn reload(&self, _runtime_path: &Path) -> Result<()> {
      self.call(Call::Reload)
    }

    async fn restart(&self, _runtime_path: &Path) -> Result<()> {
      self.call(Call::Restart)
    }
  }

  #[tokio::test]
  async fn validates_staging_before_commit_and_reloads() {
    let directory = TestDirectory::new();
    let runtime_path = directory.path.join("runtime.yaml");
    let store = RuntimeStore::open(&runtime_path).expect("store should open");
    let activator = FakeActivator::default();
    let output = RuntimeDeployer::new(&store, &AcceptValidator, &activator)
      .deploy(&config("mode: rule"))
      .await
      .expect("deployment should succeed");

    assert_eq!(output.activation, ActivationMode::Reload);
    assert_eq!(activator.calls(), vec![Call::Reload]);
    assert_eq!(
      fs::read_to_string(&runtime_path).expect("runtime should be readable"),
      "mode: rule\n"
    );
    assert_no_staging_files(&directory.path);
  }

  #[test]
  fn initializes_an_empty_runtime_without_replacing_existing_content() {
    let directory = TestDirectory::new();
    let runtime_path = directory.path.join("runtime.yaml");
    let store = RuntimeStore::open(&runtime_path).expect("store should open");

    assert!(
      store
        .initialize_if_missing(&config("mode: rule"))
        .expect("empty runtime should initialize")
    );
    assert!(
      !store
        .initialize_if_missing(&config("mode: global"))
        .expect("existing runtime should remain unchanged")
    );
    assert_eq!(
      fs::read_to_string(runtime_path).expect("runtime should be readable"),
      "mode: rule\n"
    );
  }

  #[tokio::test]
  async fn validation_failure_never_replaces_runtime() {
    let directory = TestDirectory::new();
    let runtime_path = directory.path.join("runtime.yaml");
    fs::write(&runtime_path, "mode: old\n").expect("old runtime should write");
    let store = RuntimeStore::open(&runtime_path).expect("store should open");
    let activator = FakeActivator::default();
    let result = RuntimeDeployer::new(&store, &RejectValidator, &activator)
      .deploy(&config("mode: rule"))
      .await;

    assert!(matches!(result, Err(Error::RuntimeValidation(_))));
    assert_eq!(
      fs::read_to_string(&runtime_path).expect("runtime should be readable"),
      "mode: old\n"
    );
    assert!(activator.calls().is_empty());
    assert_no_staging_files(&directory.path);
  }

  #[tokio::test]
  async fn reload_failure_uses_one_controlled_restart() {
    let directory = TestDirectory::new();
    let runtime_path = directory.path.join("runtime.yaml");
    let store = RuntimeStore::open(&runtime_path).expect("store should open");
    let activator = FakeActivator::with_failures(vec![true, false]);
    let output = RuntimeDeployer::new(&store, &AcceptValidator, &activator)
      .deploy(&config("mode: rule"))
      .await
      .expect("restart should recover deployment");

    assert_eq!(output.activation, ActivationMode::Restart);
    assert_eq!(activator.calls(), vec![Call::Reload, Call::Restart]);
  }

  #[tokio::test]
  async fn activation_failure_restores_and_reloads_old_runtime() {
    let directory = TestDirectory::new();
    let runtime_path = directory.path.join("runtime.yaml");
    fs::write(&runtime_path, "mode: old\n").expect("old runtime should write");
    let store = RuntimeStore::open(&runtime_path).expect("store should open");
    let activator = FakeActivator::with_failures(vec![true, true, false]);
    let result = RuntimeDeployer::new(&store, &AcceptValidator, &activator)
      .deploy(&config("mode: rule"))
      .await;

    assert!(matches!(result, Err(Error::RuntimeActivation(_))));
    assert_eq!(
      fs::read_to_string(&runtime_path).expect("runtime should be readable"),
      "mode: old\n"
    );
    assert_eq!(
      activator.calls(),
      vec![Call::Reload, Call::Restart, Call::Reload]
    );
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn command_validator_uses_process_exit_status() {
    let directory = TestDirectory::new();
    let staging = directory.path.join("staging.yaml");
    fs::write(&staging, "mode: rule\n").expect("staging should write");

    assert!(
      CommandRuntimeValidator::new("/bin/true", &directory.path)
        .validate(&staging)
        .await
        .is_ok()
    );
    assert!(
      CommandRuntimeValidator::new("/bin/false", &directory.path)
        .validate(&staging)
        .await
        .is_err()
    );
  }

  fn config(source: &str) -> MihomoConfig {
    MihomoConfig::parse(source).expect("test config should parse")
  }

  fn assert_no_staging_files(directory: &Path) {
    assert!(
      fs::read_dir(directory)
        .expect("directory should be readable")
        .all(|entry| !entry
          .expect("entry should be readable")
          .file_name()
          .to_string_lossy()
          .ends_with(".tmp"))
    );
  }

  struct TestDirectory {
    path: PathBuf,
  }

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
      let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      let path = std::env::temp_dir().join(format!("rsclash-runtime-{}-{id}", std::process::id()));
      fs::create_dir_all(&path).expect("test directory should be created");
      Self { path }
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ignored = fs::remove_dir_all(&self.path);
    }
  }
}
