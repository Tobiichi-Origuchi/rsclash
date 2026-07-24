use std::{
  env,
  fs::{self, OpenOptions},
  io::Write as _,
  os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
  path::{Path, PathBuf},
  process::Command,
  sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;

use crate::{AppDirectory, DesktopIntegration, Error, Result};

const DESKTOP_FILE_NAME: &str = "io.github.rsclash.desktop";

#[derive(Clone, Debug)]
pub struct LinuxDesktopPaths {
  pub config: PathBuf,
  pub data: PathBuf,
  pub logs: PathBuf,
  pub core: PathBuf,
}

#[derive(Clone, Debug)]
pub struct LinuxDesktopIntegration {
  executable: PathBuf,
  autostart_file: PathBuf,
  applications_file: PathBuf,
  paths: LinuxDesktopPaths,
}

impl LinuxDesktopIntegration {
  pub fn discover(paths: LinuxDesktopPaths) -> Result<Self> {
    let home = env::var_os("HOME")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from)
      .ok_or_else(|| Error::Platform("HOME is not set".to_string()))?;
    let config_home = absolute_xdg("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let data_home = absolute_xdg("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
    let executable = env::current_exe()
      .map_err(|error| Error::Platform(format!("resolve current executable: {error}")))?;
    Ok(Self {
      executable,
      autostart_file: config_home.join("autostart").join(DESKTOP_FILE_NAME),
      applications_file: data_home.join("applications").join(DESKTOP_FILE_NAME),
      paths,
    })
  }

  #[cfg(test)]
  const fn with_layout(
    executable: PathBuf,
    autostart_file: PathBuf,
    applications_file: PathBuf,
    paths: LinuxDesktopPaths,
  ) -> Self {
    Self {
      executable,
      autostart_file,
      applications_file,
      paths,
    }
  }

  fn desktop_entry(&self, silent: bool) -> String {
    let argument = if silent { " --silent" } else { "" };
    format!(
      "[Desktop Entry]\n\
       Type=Application\n\
       Name=rsclash\n\
       Comment=Native Mihomo desktop client\n\
       Exec={}{} %U\n\
       Icon=io.github.rsclash\n\
       Terminal=false\n\
       Categories=Network;\n\
       MimeType=x-scheme-handler/rsclash;x-scheme-handler/clash;x-scheme-handler/clash-verge;\n\
       StartupNotify=true\n",
      desktop_exec_quote(&self.executable),
      argument
    )
  }

  fn directory(&self, directory: AppDirectory) -> &Path {
    match directory {
      AppDirectory::Configuration => &self.paths.config,
      AppDirectory::Data => &self.paths.data,
      AppDirectory::Logs => &self.paths.logs,
      AppDirectory::Core => &self.paths.core,
    }
  }
}

#[async_trait]
impl DesktopIntegration for LinuxDesktopIntegration {
  async fn autostart_enabled(&self) -> Result<bool> {
    let path = self.autostart_file.clone();
    tokio::task::spawn_blocking(move || regular_file_exists(&path))
      .await
      .map_err(|error| Error::Platform(format!("autostart inspection task failed: {error}")))?
  }

  async fn set_autostart(&self, enabled: bool, silent: bool) -> Result<()> {
    let path = self.autostart_file.clone();
    let contents = self.desktop_entry(silent);
    tokio::task::spawn_blocking(move || {
      if enabled {
        atomic_write(&path, contents.as_bytes(), 0o600)
      } else {
        remove_regular_file(&path)
      }
    })
    .await
    .map_err(|error| Error::Platform(format!("autostart update task failed: {error}")))?
  }

  async fn register_deep_links(&self) -> Result<()> {
    let path = self.applications_file.clone();
    let contents = self.desktop_entry(false);
    tokio::task::spawn_blocking(move || {
      atomic_write(&path, contents.as_bytes(), 0o644)?;
      for scheme in [
        "x-scheme-handler/rsclash",
        "x-scheme-handler/clash",
        "x-scheme-handler/clash-verge",
      ] {
        run_command("xdg-mime", ["default", DESKTOP_FILE_NAME, scheme])?;
      }
      Ok(())
    })
    .await
    .map_err(|error| Error::Platform(format!("deep-link registration task failed: {error}")))?
  }

  async fn open_directory(&self, directory: AppDirectory) -> Result<()> {
    let path = self.directory(directory).to_path_buf();
    tokio::task::spawn_blocking(move || {
      fs::create_dir_all(&path).map_err(|error| io_error("create directory", &path, error))?;
      run_command_os("xdg-open", [path.as_os_str()])
    })
    .await
    .map_err(|error| Error::Platform(format!("directory opener task failed: {error}")))?
  }

  async fn notify(&self, title: &str, body: &str) -> Result<()> {
    let title = title.to_string();
    let body = body.to_string();
    tokio::task::spawn_blocking(move || run_command("notify-send", [title.as_str(), body.as_str()]))
      .await
      .map_err(|error| Error::Platform(format!("notification task failed: {error}")))?
  }

  async fn run_startup_script(&self, script: &str) -> Result<()> {
    if script.trim().is_empty() {
      return Ok(());
    }
    if script.len() > 64 * 1024 {
      return Err(Error::Platform(
        "startup script exceeds the 64 KiB limit".to_string(),
      ));
    }
    let script = script.to_string();
    tokio::task::spawn_blocking(move || run_command("sh", ["-c", script.as_str()]))
      .await
      .map_err(|error| Error::Platform(format!("startup script task failed: {error}")))?
  }
}

fn absolute_xdg(name: &str) -> Option<PathBuf> {
  env::var_os(name)
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .filter(|path| path.is_absolute())
}

fn desktop_exec_quote(path: &Path) -> String {
  let value = path
    .to_string_lossy()
    .replace('\\', "\\\\")
    .replace('"', "\\\"");
  format!("\"{value}\"")
}

fn regular_file_exists(path: &Path) -> Result<bool> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Platform(format!(
      "desktop integration file is a symbolic link: {}",
      path.display()
    ))),
    Ok(metadata) => Ok(metadata.is_file()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(io_error("inspect", path, error)),
  }
}

fn remove_regular_file(path: &Path) -> Result<()> {
  if !regular_file_exists(path)? {
    return Ok(());
  }
  fs::remove_file(path).map_err(|error| io_error("remove", path, error))
}

fn atomic_write(path: &Path, content: &[u8], mode: u32) -> Result<()> {
  let parent = path
    .parent()
    .ok_or_else(|| Error::Platform(format!("path has no parent: {}", path.display())))?;
  fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;
  let temporary = temporary_path(path);
  let mut file = OpenOptions::new()
    .create_new(true)
    .write(true)
    .mode(mode)
    .open(&temporary)
    .map_err(|error| io_error("create temporary file", &temporary, error))?;
  if let Err(error) = file.write_all(content).and_then(|()| file.sync_all()) {
    let _ = fs::remove_file(&temporary);
    return Err(io_error("write temporary file", &temporary, error));
  }
  drop(file);
  fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
    .map_err(|error| io_error("set permissions", &temporary, error))?;
  fs::rename(&temporary, path).map_err(|error| io_error("replace", path, error))?;
  Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
  static NEXT_ID: AtomicU64 = AtomicU64::new(0);
  let name = format!(
    ".rsclash-desktop-{}-{}.tmp",
    std::process::id(),
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
  );
  match path.parent() {
    Some(parent) => parent.join(name),
    None => PathBuf::from(name),
  }
}

fn run_command<const N: usize>(program: &str, arguments: [&str; N]) -> Result<()> {
  let status = Command::new(program)
    .args(arguments)
    .status()
    .map_err(|error| Error::Platform(format!("start {program}: {error}")))?;
  if status.success() {
    Ok(())
  } else {
    Err(Error::Platform(format!("{program} exited with {status}")))
  }
}

fn run_command_os<const N: usize>(program: &str, arguments: [&std::ffi::OsStr; N]) -> Result<()> {
  let status = Command::new(program)
    .args(arguments)
    .status()
    .map_err(|error| Error::Platform(format!("start {program}: {error}")))?;
  if status.success() {
    Ok(())
  } else {
    Err(Error::Platform(format!("{program} exited with {status}")))
  }
}

fn io_error(action: &'static str, path: &Path, source: std::io::Error) -> Error {
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
    sync::atomic::{AtomicU64, Ordering},
  };

  use crate::DesktopIntegration as _;

  use super::{LinuxDesktopIntegration, LinuxDesktopPaths};

  #[tokio::test]
  async fn autostart_round_trip_uses_a_private_desktop_file() {
    let directory = TestDirectory::new();
    let executable = directory.path.join("rsclash");
    fs::write(&executable, b"binary").expect("executable should be created");
    let autostart = directory.path.join("config/autostart/rsclash.desktop");
    let integration = LinuxDesktopIntegration::with_layout(
      executable.clone(),
      autostart.clone(),
      directory.path.join("applications/rsclash.desktop"),
      LinuxDesktopPaths {
        config: directory.path.join("app-config"),
        data: directory.path.join("app-data"),
        logs: directory.path.join("logs"),
        core: directory.path.join("core"),
      },
    );

    assert!(!integration.autostart_enabled().await.unwrap_or(true));
    integration
      .set_autostart(true, true)
      .await
      .expect("autostart should enable");
    let contents = fs::read_to_string(&autostart).expect("desktop file should read");
    assert!(contents.contains("--silent"));
    assert!(contents.contains(&format!("Exec=\"{}\"", executable.display())));
    let mode = std::os::unix::fs::PermissionsExt::mode(
      &fs::metadata(&autostart)
        .expect("metadata should exist")
        .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    integration
      .set_autostart(false, false)
      .await
      .expect("autostart should disable");
    assert!(!autostart.exists());
  }

  struct TestDirectory {
    path: PathBuf,
  }

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-linux-desktop-{}-{}",
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
