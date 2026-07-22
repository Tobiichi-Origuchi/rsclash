use std::{
  fs,
  os::unix::fs::{MetadataExt as _, PermissionsExt as _},
  path::{Path, PathBuf},
};

use rsclash_core::{CoreBinaries, LinuxSidecarConfig};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServiceBinaries {
  pub stable: PathBuf,
  pub alpha: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstalledServiceConfig {
  pub allowed_uid: u32,
  pub service_socket: PathBuf,
  pub controller_runtime_directory: PathBuf,
  pub data_directory: PathBuf,
  pub runtime_config: PathBuf,
  pub binaries: ServiceBinaries,
}

impl InstalledServiceConfig {
  pub fn load_installed(path: &Path) -> Result<Self> {
    validate_trusted_file(path, 0, false)?;
    let bytes = fs::read(path)?;
    let config: Self = serde_json::from_slice(&bytes).map_err(Error::Decode)?;
    config.validate_installed()?;
    Ok(config)
  }

  pub fn validate_installed(&self) -> Result<()> {
    validate_absolute_path(&self.service_socket)?;
    validate_absolute_path(&self.controller_runtime_directory)?;
    validate_absolute_path(&self.data_directory)?;
    validate_absolute_path(&self.runtime_config)?;
    validate_trusted_file(&self.binaries.stable, 0, true)?;
    if let Some(alpha) = &self.binaries.alpha {
      validate_trusted_file(alpha, 0, true)?;
    }
    validate_user_directory(&self.data_directory, self.allowed_uid)?;
    validate_user_file(&self.data_directory, &self.runtime_config, self.allowed_uid)
  }

  pub fn validate_running_uid(&self) -> Result<()> {
    let actual_uid = rustix::process::geteuid().as_raw();
    if actual_uid == self.allowed_uid {
      Ok(())
    } else {
      Err(Error::UnauthorizedPeer {
        expected: self.allowed_uid,
        actual: actual_uid,
      })
    }
  }

  pub fn sidecar_config(&self) -> LinuxSidecarConfig {
    let mut binaries = CoreBinaries::new(&self.binaries.stable);
    if let Some(alpha) = &self.binaries.alpha {
      binaries = binaries.with_alpha(alpha);
    }
    LinuxSidecarConfig::new(
      binaries,
      &self.data_directory,
      &self.runtime_config,
      &self.controller_runtime_directory,
    )
  }

  pub fn controller_socket(&self) -> PathBuf {
    self.sidecar_config().socket_path()
  }
}

fn validate_absolute_path(path: &Path) -> Result<()> {
  if path.is_absolute() {
    Ok(())
  } else {
    Err(Error::UnsafePath(path.to_path_buf()))
  }
}

fn validate_trusted_file(path: &Path, owner_uid: u32, executable: bool) -> Result<()> {
  validate_absolute_path(path)?;
  let metadata = fs::symlink_metadata(path)?;
  let mode = metadata.permissions().mode();
  let is_executable = !executable || mode & 0o111 != 0;
  if metadata.file_type().is_symlink()
    || !metadata.is_file()
    || metadata.uid() != owner_uid
    || mode & 0o022 != 0
    || !is_executable
  {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  Ok(())
}

fn validate_user_directory(path: &Path, owner_uid: u32) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != owner_uid {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  Ok(())
}

fn validate_user_file(root: &Path, path: &Path, owner_uid: u32) -> Result<()> {
  let root = root.canonicalize()?;
  let parent = path
    .parent()
    .ok_or_else(|| Error::UnsafePath(path.to_path_buf()))?
    .canonicalize()?;
  if !parent.starts_with(&root) {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  let metadata = fs::symlink_metadata(path)?;
  if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.uid() != owner_uid {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
  };

  use super::{validate_trusted_file, validate_user_file};

  #[test]
  fn rejects_writable_or_symlinked_installed_binaries() {
    let directory = TestDirectory::new();
    let binary = directory.path().join("mihomo");
    fs::write(&binary, b"binary").expect("binary should be written");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
      .expect("permissions should be set");
    let owner = fs::metadata(&binary).expect("metadata should exist").uid();
    assert!(validate_trusted_file(&binary, owner, true).is_ok());

    fs::set_permissions(&binary, fs::Permissions::from_mode(0o775))
      .expect("permissions should be changed");
    assert!(validate_trusted_file(&binary, owner, true).is_err());
    let link = directory.path().join("mihomo-link");
    std::os::unix::fs::symlink(&binary, &link).expect("symlink should be created");
    assert!(validate_trusted_file(&link, owner, true).is_err());
  }

  #[test]
  fn keeps_the_runtime_config_inside_the_user_root() {
    let directory = TestDirectory::new();
    let root = directory.path().join("data");
    let outside = directory.path().join("outside.yaml");
    fs::create_dir(&root).expect("data directory should be created");
    fs::write(root.join("runtime.yaml"), b"mode: rule").expect("runtime should be written");
    fs::write(&outside, b"mode: rule").expect("outside config should be written");
    let owner = fs::metadata(&root).expect("metadata should exist").uid();
    assert!(validate_user_file(&root, &root.join("runtime.yaml"), owner).is_ok());
    assert!(validate_user_file(&root, &outside, owner).is_err());
  }

  use std::os::unix::fs::MetadataExt as _;

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-service-config-test-{}-{}",
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
