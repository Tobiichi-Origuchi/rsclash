use std::{
  env,
  fs::{self, File, OpenOptions},
  io::{self, Write as _},
  os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use rsclash_config::ProfileStore;
use rsclash_domain::AppSettings;
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt as _};

const ACTIVE_LOG: &str = "rsclash.log";

pub(crate) fn init() {
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rsclash=info"));
  let file_writer = RotatingLogWriter::discover();
  let initialized = match file_writer {
    Ok(writer) => tracing_subscriber::fmt()
      .with_env_filter(filter)
      .with_target(false)
      .with_ansi(false)
      .compact()
      .with_writer(io::stderr.and(writer))
      .try_init(),
    Err(error) => {
      diagnostic(format_args!("file logging is unavailable: {error}"));
      tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init()
    },
  };
  if let Err(error) = initialized {
    diagnostic(format_args!("tracing initialization failed: {error}"));
  }
}

fn diagnostic(message: impl std::fmt::Display) {
  let _ = writeln!(io::stderr().lock(), "rsclash: {message}");
}

#[derive(Clone)]
struct RotatingLogWriter {
  state: Arc<Mutex<LogState>>,
}

impl RotatingLogWriter {
  fn discover() -> io::Result<Self> {
    let home = env::var_os("HOME")
      .map(PathBuf::from)
      .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let config_root = env::var_os("XDG_CONFIG_HOME")
      .map(PathBuf::from)
      .unwrap_or_else(|| home.join(".config"))
      .join("rsclash");
    let settings = ProfileStore::open(&config_root)
      .and_then(|store| store.load_application_settings())
      .unwrap_or_else(|error| {
        diagnostic(format_args!("using default log limits: {error}"));
        AppSettings::default()
      });
    Self::open(
      config_root.join("logs"),
      &settings,
      Some(config_root.join("settings.yaml")),
    )
  }

  fn open(
    directory: PathBuf,
    settings: &AppSettings,
    settings_path: Option<PathBuf>,
  ) -> io::Result<Self> {
    create_private_directory(&directory)?;
    let (max_bytes, max_count, retention) = log_limits(settings);
    let active = directory.join(ACTIVE_LOG);
    reject_symlink(&active)?;
    let file = open_private_append(&active)?;
    let bytes = file.metadata()?.len();
    let state = LogState {
      directory,
      active,
      file,
      bytes,
      max_bytes,
      max_count,
      retention,
      sequence: 0,
      settings_path,
      last_settings_check: UNIX_EPOCH,
    };
    state.prune()?;
    Ok(Self {
      state: Arc::new(Mutex::new(state)),
    })
  }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for RotatingLogWriter {
  type Writer = LogWriterGuard;

  fn make_writer(&'writer self) -> Self::Writer {
    LogWriterGuard {
      state: Arc::clone(&self.state),
    }
  }
}

struct LogWriterGuard {
  state: Arc<Mutex<LogState>>,
}

impl io::Write for LogWriterGuard {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    self
      .state
      .lock()
      .map_err(|_| io::Error::other("log writer lock is poisoned"))?
      .write(buffer)
  }

  fn flush(&mut self) -> io::Result<()> {
    self
      .state
      .lock()
      .map_err(|_| io::Error::other("log writer lock is poisoned"))?
      .file
      .flush()
  }
}

struct LogState {
  directory: PathBuf,
  active: PathBuf,
  file: File,
  bytes: u64,
  max_bytes: u64,
  max_count: usize,
  retention: Duration,
  sequence: u64,
  settings_path: Option<PathBuf>,
  last_settings_check: SystemTime,
}

impl LogState {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    self.refresh_limits()?;
    if self.bytes > 0 && self.bytes.saturating_add(buffer.len() as u64) > self.max_bytes {
      self.rotate()?;
    }
    let written = self.file.write(buffer)?;
    self.bytes = self.bytes.saturating_add(written as u64);
    Ok(written)
  }

  fn refresh_limits(&mut self) -> io::Result<()> {
    let now = SystemTime::now();
    if now
      .duration_since(self.last_settings_check)
      .is_ok_and(|elapsed| elapsed < Duration::from_secs(1))
    {
      return Ok(());
    }
    self.last_settings_check = now;
    let Some(path) = self.settings_path.as_ref() else {
      return Ok(());
    };
    match fs::symlink_metadata(path) {
      Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => return Ok(()),
      Ok(_) => {},
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
      Err(error) => return Err(error),
    }
    let source = fs::read_to_string(path)?;
    let Ok(settings) = rsclash_config::from_yaml::<AppSettings>(&source) else {
      return Ok(());
    };
    (self.max_bytes, self.max_count, self.retention) = log_limits(&settings);
    self.prune()
  }

  fn rotate(&mut self) -> io::Result<()> {
    self.file.flush()?;
    let timestamp = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_secs();
    let archived = loop {
      let path = self
        .directory
        .join(format!("rsclash-{timestamp}-{}.log", self.sequence));
      self.sequence = self.sequence.saturating_add(1);
      if !path.exists() {
        break path;
      }
    };
    fs::rename(&self.active, archived)?;
    self.file = open_private_append(&self.active)?;
    self.bytes = 0;
    self.prune()
  }

  fn prune(&self) -> io::Result<()> {
    let now = SystemTime::now();
    let mut archives = fs::read_dir(&self.directory)?
      .filter_map(Result::ok)
      .filter_map(|entry| {
        let name = entry.file_name();
        let name = name.to_str()?;
        (name.starts_with("rsclash-") && name.ends_with(".log")).then_some(entry.path())
      })
      .filter_map(|path| {
        let metadata = fs::symlink_metadata(&path).ok()?;
        metadata
          .file_type()
          .is_file()
          .then_some((path, metadata.modified().unwrap_or(UNIX_EPOCH)))
      })
      .collect::<Vec<_>>();
    for (path, modified) in &archives {
      if now
        .duration_since(*modified)
        .is_ok_and(|age| age > self.retention)
      {
        fs::remove_file(path)?;
      }
    }
    archives.retain(|(path, _)| path.exists());
    archives.sort_by_key(|(_, modified)| *modified);
    let archive_limit = self.max_count.saturating_sub(1);
    let excess = archives.len().saturating_sub(archive_limit);
    for (path, _) in archives.into_iter().take(excess) {
      fs::remove_file(path)?;
    }
    Ok(())
  }
}

fn log_limits(settings: &AppSettings) -> (u64, usize, Duration) {
  (
    settings
      .app_log_max_size_mib
      .clamp(1, 1_024)
      .saturating_mul(1024 * 1024),
    settings.app_log_max_count.clamp(1, 100),
    Duration::from_secs(
      u64::from(settings.app_log_retention_days.clamp(1, 365)).saturating_mul(24 * 60 * 60),
    ),
  )
}

fn create_private_directory(path: &Path) -> io::Result<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => {
      return Err(io::Error::other(format!(
        "log directory is a symbolic link: {}",
        path.display()
      )));
    },
    Ok(metadata) if !metadata.is_dir() => {
      return Err(io::Error::other(format!(
        "log path is not a directory: {}",
        path.display()
      )));
    },
    Ok(_) => {},
    Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
    Err(error) => return Err(error),
  }
  fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn reject_symlink(path: &Path) -> io::Result<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::other(format!(
      "log file is a symbolic link: {}",
      path.display()
    ))),
    Ok(_) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error),
  }
}

fn open_private_append(path: &Path) -> io::Result<File> {
  let file = OpenOptions::new()
    .create(true)
    .append(true)
    .mode(0o600)
    .open(path)?;
  file.set_permissions(fs::Permissions::from_mode(0o600))?;
  Ok(file)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    io::Write as _,
    sync::atomic::{AtomicU64, Ordering},
  };

  use super::{AppSettings, RotatingLogWriter};

  static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

  #[test]
  fn rotates_and_bounds_application_logs() {
    let directory = std::env::temp_dir().join(format!(
      "rsclash-log-test-{}-{}",
      std::process::id(),
      NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    let settings = AppSettings {
      app_log_max_size_mib: 1,
      app_log_max_count: 2,
      ..AppSettings::default()
    };
    let writer =
      RotatingLogWriter::open(directory.clone(), &settings, None).expect("log writer should open");
    let mut writer = super::LogWriterGuard {
      state: writer.state,
    };
    for _ in 0..3 {
      writer
        .write_all(&vec![b'x'; 800 * 1024])
        .expect("log write should succeed");
    }

    let logs = std::fs::read_dir(&directory)
      .expect("log directory should be readable")
      .count();
    assert_eq!(logs, 2);
    std::fs::remove_dir_all(directory).expect("temporary logs should be removable");
  }
}
