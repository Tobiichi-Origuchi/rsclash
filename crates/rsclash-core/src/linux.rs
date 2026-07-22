use std::{
  collections::VecDeque,
  fmt, io,
  os::unix::fs::FileTypeExt as _,
  path::{Path, PathBuf},
  process::{ExitStatus, Stdio},
  sync::{Arc, Mutex, MutexGuard},
  time::Duration,
};

use async_trait::async_trait;
use rsclash_domain::{CoreChannel, CoreRunMode};
use rsclash_mihomo::{
  ControllerConfig, ControllerEndpoint, MihomoApi as _, MihomoClient, models::VersionInfo,
};
use rustix::{
  io::Errno,
  process::{Pid, Signal, kill_process_group},
};
use tokio::{
  fs,
  io::{AsyncRead, AsyncReadExt as _},
  process::{Child, Command},
  task::JoinHandle,
  time::{sleep, timeout},
};
use tracing::{debug, info, warn};

use crate::{ControllerError, LifecycleController, RunningCore};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_LOG_BYTES: usize = 256 * 1024;
const DEFAULT_LOG_ENTRIES: usize = 1_024;
const OUTPUT_CHUNK_BYTES: usize = 4 * 1024;
const OUTPUT_JOIN_TIMEOUT: Duration = Duration::from_secs(1);
const CONTROLLER_SOCKET_NAME: &str = "controller.sock";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreBinaries {
  stable: PathBuf,
  alpha: Option<PathBuf>,
}

impl CoreBinaries {
  pub fn new(stable: impl Into<PathBuf>) -> Self {
    Self {
      stable: stable.into(),
      alpha: None,
    }
  }

  pub fn with_alpha(mut self, alpha: impl Into<PathBuf>) -> Self {
    self.alpha = Some(alpha.into());
    self
  }

  pub fn for_channel(&self, channel: CoreChannel) -> Option<&Path> {
    match channel {
      CoreChannel::Stable => Some(&self.stable),
      CoreChannel::Alpha => self.alpha.as_deref(),
    }
  }
}

#[derive(Clone, Debug)]
pub struct LinuxSidecarConfig {
  pub binaries: CoreBinaries,
  pub data_directory: PathBuf,
  pub config_path: PathBuf,
  pub runtime_directory: PathBuf,
  pub startup_timeout: Duration,
  pub probe_interval: Duration,
  pub probe_timeout: Duration,
  pub shutdown_timeout: Duration,
  pub log_bytes: usize,
  pub log_entries: usize,
}

impl LinuxSidecarConfig {
  pub fn new(
    binaries: CoreBinaries,
    data_directory: impl Into<PathBuf>,
    config_path: impl Into<PathBuf>,
    runtime_directory: impl Into<PathBuf>,
  ) -> Self {
    Self {
      binaries,
      data_directory: data_directory.into(),
      config_path: config_path.into(),
      runtime_directory: runtime_directory.into(),
      startup_timeout: DEFAULT_STARTUP_TIMEOUT,
      probe_interval: DEFAULT_PROBE_INTERVAL,
      probe_timeout: DEFAULT_PROBE_TIMEOUT,
      shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
      log_bytes: DEFAULT_LOG_BYTES,
      log_entries: DEFAULT_LOG_ENTRIES,
    }
  }

  pub fn socket_path(&self) -> PathBuf {
    self.runtime_directory.join(CONTROLLER_SOCKET_NAME)
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreOutputStream {
  Stdout,
  Stderr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreLogEntry {
  pub sequence: u64,
  pub stream: CoreOutputStream,
  pub text: String,
}

#[derive(Clone)]
pub struct CoreLogStore {
  inner: Arc<Mutex<CoreLogBuffer>>,
}

impl CoreLogStore {
  pub fn new(max_bytes: usize, max_entries: usize) -> Self {
    Self {
      inner: Arc::new(Mutex::new(CoreLogBuffer {
        entries: VecDeque::new(),
        total_bytes: 0,
        max_bytes,
        max_entries,
        next_sequence: 0,
      })),
    }
  }

  pub fn snapshot(&self) -> Vec<CoreLogEntry> {
    self.lock().entries.iter().cloned().collect()
  }

  pub fn clear(&self) {
    let mut buffer = self.lock();
    buffer.entries.clear();
    buffer.total_bytes = 0;
  }

  fn push(&self, stream: CoreOutputStream, bytes: &[u8]) {
    if bytes.is_empty() {
      return;
    }
    self
      .lock()
      .push(stream, String::from_utf8_lossy(bytes).into_owned());
  }

  fn recent_text(&self) -> String {
    self
      .lock()
      .entries
      .iter()
      .rev()
      .take(8)
      .rev()
      .map(|entry| entry.text.trim_end())
      .filter(|text| !text.is_empty())
      .collect::<Vec<_>>()
      .join("\n")
  }

  fn lock(&self) -> MutexGuard<'_, CoreLogBuffer> {
    self
      .inner
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
  }
}

impl fmt::Debug for CoreLogStore {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let buffer = self.lock();
    formatter
      .debug_struct("CoreLogStore")
      .field("entries", &buffer.entries.len())
      .field("total_bytes", &buffer.total_bytes)
      .field("max_bytes", &buffer.max_bytes)
      .field("max_entries", &buffer.max_entries)
      .finish()
  }
}

struct CoreLogBuffer {
  entries: VecDeque<CoreLogEntry>,
  total_bytes: usize,
  max_bytes: usize,
  max_entries: usize,
  next_sequence: u64,
}

impl CoreLogBuffer {
  fn push(&mut self, stream: CoreOutputStream, text: String) {
    if self.max_bytes == 0 || self.max_entries == 0 {
      return;
    }

    let text = retain_text_tail(text, self.max_bytes);
    let entry_bytes = text.len();
    while self.entries.len() >= self.max_entries
      || self.total_bytes.saturating_add(entry_bytes) > self.max_bytes
    {
      let Some(removed) = self.entries.pop_front() else {
        break;
      };
      self.total_bytes = self.total_bytes.saturating_sub(removed.text.len());
    }

    let entry = CoreLogEntry {
      sequence: self.next_sequence,
      stream,
      text,
    };
    self.next_sequence = self.next_sequence.saturating_add(1);
    self.total_bytes = self.total_bytes.saturating_add(entry_bytes);
    self.entries.push_back(entry);
  }
}

fn retain_text_tail(mut text: String, max_bytes: usize) -> String {
  if text.len() <= max_bytes {
    return text;
  }

  let mut boundary = text.len().saturating_sub(max_bytes);
  while !text.is_char_boundary(boundary) {
    boundary = boundary.saturating_add(1);
  }
  text.drain(..boundary);
  text
}

pub struct LinuxSidecarController {
  config: LinuxSidecarConfig,
  logs: CoreLogStore,
  sidecar: Option<SidecarProcess>,
}

impl LinuxSidecarController {
  pub fn new(config: LinuxSidecarConfig) -> Self {
    let logs = CoreLogStore::new(config.log_bytes, config.log_entries);
    Self {
      config,
      logs,
      sidecar: None,
    }
  }

  pub fn logs(&self) -> CoreLogStore {
    self.logs.clone()
  }

  async fn start_sidecar(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
    if self.sidecar.is_some() {
      return Err(ControllerError::new("a sidecar process is already owned"));
    }
    validate_durations(&self.config)?;
    let binary = self
      .config
      .binaries
      .for_channel(channel)
      .ok_or_else(|| ControllerError::new("the alpha core binary is not configured"))?;

    prepare_private_directory(&self.config.data_directory).await?;
    validate_runtime_config(&self.config.data_directory, &self.config.config_path).await?;
    prepare_private_directory(&self.config.runtime_directory).await?;
    let socket_path = self.config.socket_path();
    remove_stale_socket(&socket_path).await?;
    self.logs.clear();

    let client = MihomoClient::new(
      ControllerConfig::local(ControllerEndpoint::unix_socket(&socket_path))
        .with_request_timeout(self.config.probe_timeout)
        .with_max_safe_retries(0),
    )
    .map_err(|error| ControllerError::new(format!("build the Mihomo client: {error}")))?;
    let mut sidecar = SidecarProcess::spawn(
      binary,
      &self.config.data_directory,
      &self.config.config_path,
      socket_path,
      client,
      self.logs.clone(),
    )?;

    let ready = timeout(
      self.config.startup_timeout,
      wait_until_ready(&mut sidecar, self.config.probe_interval),
    )
    .await;
    let version = match ready {
      Ok(Ok(version)) => version,
      Ok(Err(error)) => {
        return Err(cleanup_failed_start(sidecar, error, &self.config, &self.logs).await);
      },
      Err(_) => {
        let error = ControllerError::new(format!(
          "Mihomo did not become ready within {:?}",
          self.config.startup_timeout
        ));
        return Err(cleanup_failed_start(sidecar, error, &self.config, &self.logs).await);
      },
    };

    if let Err(error) = secure_socket(sidecar.socket_path()).await {
      return Err(cleanup_failed_start(sidecar, error, &self.config, &self.logs).await);
    }
    info!(pid = ?sidecar.process_group(), ?channel, "Mihomo sidecar is ready");
    self.sidecar = Some(sidecar);
    Ok(RunningCore::new(
      CoreRunMode::Sidecar,
      Some(version.version),
    ))
  }

  async fn stop_sidecar(&mut self) -> Result<(), ControllerError> {
    let Some(mut sidecar) = self.sidecar.take() else {
      return Ok(());
    };
    let pid = sidecar.process_group();
    let result = sidecar.terminate(self.config.shutdown_timeout).await;
    if result.is_ok() {
      info!(?pid, "Mihomo sidecar stopped");
    }
    result
  }

  async fn reload_sidecar(&mut self) -> Result<RunningCore, ControllerError> {
    let sidecar = self
      .sidecar
      .as_mut()
      .ok_or_else(|| ControllerError::new("no sidecar process is running"))?;
    if let Some(status) = sidecar.try_wait()? {
      return Err(process_exit_error(status, &self.logs));
    }

    let path = self
      .config
      .config_path
      .to_str()
      .ok_or_else(|| ControllerError::new("the runtime configuration path is not valid UTF-8"))?;
    sidecar
      .client
      .reload_config(path, false)
      .await
      .map_err(|error| ControllerError::new(format!("reload Mihomo configuration: {error}")))?;
    let version = sidecar
      .client
      .health_check()
      .await
      .map_err(|error| ControllerError::new(format!("check Mihomo after reload: {error}")))?;
    Ok(RunningCore::new(
      CoreRunMode::Sidecar,
      Some(version.version),
    ))
  }
}

#[async_trait]
impl LifecycleController for LinuxSidecarController {
  async fn start(&mut self, channel: CoreChannel) -> Result<RunningCore, ControllerError> {
    self.start_sidecar(channel).await
  }

  async fn stop(&mut self) -> Result<(), ControllerError> {
    self.stop_sidecar().await
  }

  async fn reload(&mut self) -> Result<RunningCore, ControllerError> {
    self.reload_sidecar().await
  }
}

fn validate_durations(config: &LinuxSidecarConfig) -> Result<(), ControllerError> {
  if config.startup_timeout.is_zero() {
    return Err(ControllerError::new("the startup timeout must not be zero"));
  }
  if config.probe_interval.is_zero() {
    return Err(ControllerError::new(
      "the ready probe interval must not be zero",
    ));
  }
  if config.probe_timeout.is_zero() {
    return Err(ControllerError::new(
      "the ready probe timeout must not be zero",
    ));
  }
  Ok(())
}

async fn prepare_private_directory(path: &Path) -> Result<(), ControllerError> {
  use std::os::unix::fs::PermissionsExt as _;

  fs::create_dir_all(path)
    .await
    .map_err(|error| io_error("create private directory", path, error))?;
  let metadata = fs::symlink_metadata(path)
    .await
    .map_err(|error| io_error("inspect private directory", path, error))?;
  if metadata.file_type().is_symlink() || !metadata.is_dir() {
    return Err(ControllerError::new(format!(
      "the private runtime path is not a directory: {}",
      path.display()
    )));
  }
  fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    .await
    .map_err(|error| io_error("restrict private directory", path, error))
}

async fn validate_runtime_config(
  data_directory: &Path,
  config_path: &Path,
) -> Result<(), ControllerError> {
  let metadata = fs::symlink_metadata(config_path)
    .await
    .map_err(|error| io_error("inspect runtime configuration", config_path, error))?;
  if metadata.file_type().is_symlink() || !metadata.is_file() {
    return Err(ControllerError::new(format!(
      "the runtime configuration is not a regular file: {}",
      config_path.display()
    )));
  }

  let canonical_data = fs::canonicalize(data_directory)
    .await
    .map_err(|error| io_error("resolve data directory", data_directory, error))?;
  let canonical_config = fs::canonicalize(config_path)
    .await
    .map_err(|error| io_error("resolve runtime configuration", config_path, error))?;
  if !canonical_config.starts_with(&canonical_data) {
    return Err(ControllerError::new(format!(
      "the runtime configuration must be inside the Mihomo data directory: {}",
      config_path.display()
    )));
  }
  Ok(())
}

async fn remove_stale_socket(path: &Path) -> Result<(), ControllerError> {
  match fs::symlink_metadata(path).await {
    Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)
      .await
      .map_err(|error| io_error("remove stale controller socket", path, error)),
    Ok(_) => Err(ControllerError::new(format!(
      "refusing to replace a non-socket controller path: {}",
      path.display()
    ))),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(io_error("inspect controller socket", path, error)),
  }
}

async fn secure_socket(path: &Path) -> Result<(), ControllerError> {
  use std::os::unix::fs::PermissionsExt as _;

  let metadata = fs::symlink_metadata(path)
    .await
    .map_err(|error| io_error("inspect controller socket", path, error))?;
  if !metadata.file_type().is_socket() {
    return Err(ControllerError::new(format!(
      "the controller path is not a Unix socket: {}",
      path.display()
    )));
  }
  fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    .await
    .map_err(|error| io_error("restrict controller socket", path, error))
}

fn io_error(operation: &str, path: &Path, error: io::Error) -> ControllerError {
  ControllerError::new(format!("{operation} at {}: {error}", path.display()))
}

struct SidecarProcess {
  child: Child,
  process_group: Option<Pid>,
  client: MihomoClient,
  socket_path: PathBuf,
  output_tasks: Vec<JoinHandle<io::Result<()>>>,
}

impl SidecarProcess {
  fn spawn(
    binary: &Path,
    data_directory: &Path,
    config_path: &Path,
    socket_path: PathBuf,
    client: MihomoClient,
    logs: CoreLogStore,
  ) -> Result<Self, ControllerError> {
    let mut command = Command::new(binary);
    command
      .arg("-d")
      .arg(data_directory)
      .arg("-f")
      .arg(config_path)
      .arg("-ext-ctl-unix")
      .arg(&socket_path)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .process_group(0);
    let mut child = command.spawn().map_err(|error| {
      ControllerError::new(format!("spawn Mihomo at {}: {error}", binary.display()))
    })?;
    let raw_pid = child
      .id()
      .and_then(|pid| i32::try_from(pid).ok())
      .and_then(Pid::from_raw)
      .ok_or_else(|| ControllerError::new("Mihomo did not expose a valid process ID"))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| ControllerError::new("Mihomo stdout was not captured"))?;
    let stderr = child
      .stderr
      .take()
      .ok_or_else(|| ControllerError::new("Mihomo stderr was not captured"))?;
    let output_tasks = vec![
      tokio::spawn(capture_output(
        stdout,
        CoreOutputStream::Stdout,
        logs.clone(),
      )),
      tokio::spawn(capture_output(stderr, CoreOutputStream::Stderr, logs)),
    ];
    debug!(pid = %raw_pid, binary = %binary.display(), "spawned Mihomo sidecar");

    Ok(Self {
      child,
      process_group: Some(raw_pid),
      client,
      socket_path,
      output_tasks,
    })
  }

  const fn process_group(&self) -> Option<Pid> {
    self.process_group
  }

  fn socket_path(&self) -> &Path {
    &self.socket_path
  }

  fn try_wait(&mut self) -> Result<Option<ExitStatus>, ControllerError> {
    let status = self
      .child
      .try_wait()
      .map_err(|error| ControllerError::new(format!("read Mihomo process status: {error}")))?;
    if status.is_some() {
      self.process_group = None;
    }
    Ok(status)
  }

  async fn terminate(&mut self, grace: Duration) -> Result<(), ControllerError> {
    let process_result = if self.try_wait()?.is_none() {
      self.stop_process(grace).await
    } else {
      Ok(())
    };
    self.join_output_tasks().await;
    let socket_result = remove_stale_socket(&self.socket_path).await;
    process_result?;
    socket_result
  }

  async fn stop_process(&mut self, grace: Duration) -> Result<(), ControllerError> {
    if let Err(term_error) = self.signal(Signal::TERM) {
      let _ = self.signal(Signal::KILL);
      let _ = self.child.start_kill();
      let wait_result = self.wait().await;
      return match wait_result {
        Ok(_) => Err(term_error),
        Err(wait_error) => Err(ControllerError::new(format!(
          "{term_error}; forced cleanup also failed: {wait_error}"
        ))),
      };
    }

    match timeout(grace, self.wait()).await {
      Ok(Ok(_)) => return Ok(()),
      Ok(Err(error)) => return Err(error),
      Err(_) => {},
    }
    warn!(
      pid = ?self.process_group,
      "Mihomo did not exit after SIGTERM; sending SIGKILL"
    );
    self.signal(Signal::KILL)?;
    self.wait().await.map(|_| ())
  }

  fn signal(&self, signal: Signal) -> Result<(), ControllerError> {
    let Some(process_group) = self.process_group else {
      return Ok(());
    };
    match kill_process_group(process_group, signal) {
      Ok(()) | Err(Errno::SRCH) => Ok(()),
      Err(error) => Err(ControllerError::new(format!(
        "send {signal:?} to Mihomo process group {process_group}: {error}"
      ))),
    }
  }

  async fn wait(&mut self) -> Result<ExitStatus, ControllerError> {
    let status = self
      .child
      .wait()
      .await
      .map_err(|error| ControllerError::new(format!("wait for Mihomo to exit: {error}")))?;
    self.process_group = None;
    Ok(status)
  }

  async fn join_output_tasks(&mut self) {
    while let Some(task) = self.output_tasks.pop() {
      join_output_task(task).await;
    }
  }
}

async fn join_output_task(mut task: JoinHandle<io::Result<()>>) {
  match timeout(OUTPUT_JOIN_TIMEOUT, &mut task).await {
    Ok(Ok(Ok(()))) => {},
    Ok(Ok(Err(error))) => warn!(%error, "failed to capture Mihomo output"),
    Ok(Err(error)) => warn!(%error, "Mihomo output task failed"),
    Err(_) => {
      task.abort();
      let _ = task.await;
      warn!("timed out while draining Mihomo output");
    },
  }
}

impl Drop for SidecarProcess {
  fn drop(&mut self) {
    if let Some(process_group) = self.process_group.take() {
      let _ = kill_process_group(process_group, Signal::KILL);
      let _ = self.child.start_kill();
    }
    for task in self.output_tasks.drain(..) {
      task.abort();
    }
  }
}

async fn capture_output<R>(
  mut reader: R,
  stream: CoreOutputStream,
  logs: CoreLogStore,
) -> io::Result<()>
where
  R: AsyncRead + Send + Unpin + 'static,
{
  let mut buffer = [0_u8; OUTPUT_CHUNK_BYTES];
  loop {
    let read = reader.read(&mut buffer).await?;
    if read == 0 {
      return Ok(());
    }
    logs.push(stream, &buffer[..read]);
  }
}

async fn wait_until_ready(
  sidecar: &mut SidecarProcess,
  probe_interval: Duration,
) -> Result<VersionInfo, ControllerError> {
  loop {
    if let Some(status) = sidecar.try_wait()? {
      return Err(ControllerError::new(format!(
        "Mihomo exited before the controller became ready: {status}"
      )));
    }
    if let Ok(version) = sidecar.client.health_check().await {
      return Ok(version);
    }
    sleep(probe_interval).await;
  }
}

async fn cleanup_failed_start(
  mut sidecar: SidecarProcess,
  error: ControllerError,
  config: &LinuxSidecarConfig,
  logs: &CoreLogStore,
) -> ControllerError {
  let cleanup_error = sidecar.terminate(config.shutdown_timeout).await.err();
  let recent_output = logs.recent_text();
  let diagnostic = if recent_output.is_empty() {
    error.to_string()
  } else {
    format!("{error}; recent output:\n{recent_output}")
  };
  match cleanup_error {
    Some(cleanup_error) => ControllerError::new(format!(
      "{diagnostic}; cleanup also failed: {cleanup_error}"
    )),
    None => ControllerError::new(diagnostic),
  }
}

fn process_exit_error(status: ExitStatus, logs: &CoreLogStore) -> ControllerError {
  let recent = logs.recent_text();
  if recent.is_empty() {
    ControllerError::new(format!("Mihomo exited unexpectedly: {status}"))
  } else {
    ControllerError::new(format!(
      "Mihomo exited unexpectedly: {status}; recent output:\n{recent}"
    ))
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
  };

  use rsclash_domain::CoreChannel;

  use super::{
    CoreBinaries, CoreLogStore, CoreOutputStream, LifecycleController as _, LinuxSidecarConfig,
    LinuxSidecarController,
  };

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-core-test-{}-{}",
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

  #[test]
  fn output_log_is_bounded_by_bytes_and_entries() {
    let logs = CoreLogStore::new(8, 2);
    logs.push(CoreOutputStream::Stdout, b"first");
    logs.push(CoreOutputStream::Stderr, b"second");
    logs.push(CoreOutputStream::Stdout, b"abcdefghijk");

    let entries = logs.snapshot();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "defghijk");
    assert_eq!(entries[0].sequence, 2);
  }

  #[tokio::test]
  async fn failed_start_collects_output_and_reaps_the_process() {
    let directory = TestDirectory::new();
    let binary = directory.path().join("fake-mihomo");
    fs::write(
      &binary,
      "#!/bin/sh\nprintf 'captured stdout\\n'\nprintf 'captured stderr\\n' >&2\nexit 23\n",
    )
    .expect("fake core should be written");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
      .expect("fake core should be executable");
    let data_directory = directory.path().join("data");
    fs::create_dir_all(&data_directory).expect("data directory should be created");
    let config_path = data_directory.join("config.yaml");
    fs::write(&config_path, "mode: rule\n").expect("config should be written");
    let runtime_directory = directory.path().join("runtime");
    let mut config = LinuxSidecarConfig::new(
      CoreBinaries::new(&binary),
      &data_directory,
      &config_path,
      &runtime_directory,
    );
    config.startup_timeout = Duration::from_secs(1);
    config.probe_interval = Duration::from_millis(10);
    let mut controller = LinuxSidecarController::new(config);
    let logs = controller.logs();

    let error = controller
      .start(CoreChannel::Stable)
      .await
      .expect_err("the fake core should fail during startup");
    assert!(error.to_string().contains("exited before"));
    let output = logs.snapshot();
    assert!(
      output
        .iter()
        .any(|entry| entry.text.contains("captured stdout"))
    );
    assert!(
      output
        .iter()
        .any(|entry| entry.text.contains("captured stderr"))
    );
    assert_eq!(
      fs::metadata(&data_directory)
        .expect("data directory should exist")
        .permissions()
        .mode()
        & 0o777,
      0o700
    );
    assert_eq!(
      fs::metadata(&runtime_directory)
        .expect("runtime directory should exist")
        .permissions()
        .mode()
        & 0o777,
      0o700
    );
    assert!(controller.stop().await.is_ok());
  }
}
