use std::{
  env,
  ffi::OsStr,
  fs::{self, File, OpenOptions},
  io::{Read as _, Write as _},
  os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
  path::{Path, PathBuf},
  process::Command,
};

use crate::{DEFAULT_SERVICE_SOCKET, Error, InstalledServiceConfig, Result, ServiceBinaries};

const SERVICE_NAME: &str = "rsclash.service";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InstallIdentity {
  pub uid: u32,
  pub gid: u32,
}

impl InstallIdentity {
  pub fn from_elevation_environment(config_root: &Path) -> Result<Self> {
    if rustix::process::geteuid().as_raw() != 0 {
      return Err(Error::InvalidInstallation(
        "the installer must run as root through pkexec or sudo".to_string(),
      ));
    }
    let uid = environment_id("PKEXEC_UID").or_else(|| environment_id("SUDO_UID"));
    let gid = environment_id("PKEXEC_GID")
      .or_else(|| environment_id("SUDO_GID"))
      .or_else(|| {
        let metadata = fs::metadata(config_root).ok()?;
        (Some(metadata.uid()) == uid).then_some(metadata.gid())
      });
    match (uid, gid) {
      (Some(uid), Some(gid)) => Ok(Self { uid, gid }),
      _ => Err(Error::InvalidInstallation(
        "cannot identify the invoking user from PKEXEC_UID/PKEXEC_GID or SUDO_UID/SUDO_GID"
          .to_string(),
      )),
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallRequest {
  pub service_binary: PathBuf,
  pub stable_core: PathBuf,
  pub alpha_core: Option<PathBuf>,
  pub config_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallLayout {
  pub install_directory: PathBuf,
  pub configuration_directory: PathBuf,
  pub unit_directory: PathBuf,
}

impl InstallLayout {
  pub fn system() -> Self {
    Self {
      install_directory: PathBuf::from("/usr/lib/rsclash"),
      configuration_directory: PathBuf::from("/etc/rsclash"),
      unit_directory: PathBuf::from("/etc/systemd/system"),
    }
  }

  pub fn under(root: &Path) -> Self {
    Self {
      install_directory: root.join("usr/lib/rsclash"),
      configuration_directory: root.join("etc/rsclash"),
      unit_directory: root.join("etc/systemd/system"),
    }
  }

  pub fn service_binary(&self) -> PathBuf {
    self.install_directory.join("rsclash-service")
  }

  pub fn stable_core(&self) -> PathBuf {
    self.install_directory.join("mihomo")
  }

  pub fn alpha_core(&self) -> PathBuf {
    self.install_directory.join("mihomo-alpha")
  }

  pub fn service_config(&self) -> PathBuf {
    self.configuration_directory.join("service.json")
  }

  pub fn systemd_unit(&self) -> PathBuf {
    self.unit_directory.join(SERVICE_NAME)
  }
}

#[derive(Debug)]
pub struct SystemServiceInstaller {
  layout: InstallLayout,
}

impl SystemServiceInstaller {
  pub const fn new(layout: InstallLayout) -> Self {
    Self { layout }
  }

  pub fn install_files(
    &self,
    request: &InstallRequest,
    identity: InstallIdentity,
  ) -> Result<InstalledServiceConfig> {
    validate_config_root(&request.config_root, identity.uid)?;
    validate_source_binary(&request.service_binary, identity.uid)?;
    validate_source_binary(&request.stable_core, identity.uid)?;
    if let Some(alpha) = &request.alpha_core {
      validate_source_binary(alpha, identity.uid)?;
    }

    ensure_directory(&self.layout.install_directory, 0o755)?;
    ensure_directory(&self.layout.configuration_directory, 0o755)?;
    ensure_directory(&self.layout.unit_directory, 0o755)?;
    atomic_copy(
      &request.service_binary,
      &self.layout.service_binary(),
      identity.uid,
      0o755,
    )?;
    atomic_copy(
      &request.stable_core,
      &self.layout.stable_core(),
      identity.uid,
      0o755,
    )?;
    let installed_alpha = if let Some(alpha) = &request.alpha_core {
      atomic_copy(alpha, &self.layout.alpha_core(), identity.uid, 0o755)?;
      Some(self.layout.alpha_core())
    } else {
      remove_regular_file_if_present(&self.layout.alpha_core())?;
      None
    };

    let config = InstalledServiceConfig {
      allowed_uid: identity.uid,
      service_socket: PathBuf::from(DEFAULT_SERVICE_SOCKET),
      controller_runtime_directory: PathBuf::from("/run/rsclash/core"),
      data_directory: request.config_root.clone(),
      runtime_config: request.config_root.join("runtime.yaml"),
      binaries: ServiceBinaries {
        stable: self.layout.stable_core(),
        alpha: installed_alpha,
      },
    };
    let config_bytes = serde_json::to_vec_pretty(&config).map_err(Error::Encode)?;
    atomic_write(&self.layout.service_config(), &config_bytes, 0o644)?;
    let unit = render_systemd_unit(&self.layout, &request.config_root, identity);
    atomic_write(&self.layout.systemd_unit(), unit.as_bytes(), 0o644)?;
    Ok(config)
  }

  pub fn enable_and_start(&self) -> Result<()> {
    run_systemctl(["daemon-reload"])?;
    run_systemctl(["enable", "--now", SERVICE_NAME])
  }

  pub fn install(
    &self,
    request: &InstallRequest,
    identity: InstallIdentity,
  ) -> Result<InstalledServiceConfig> {
    let config = self.install_files(request, identity)?;
    self.enable_and_start()?;
    Ok(config)
  }
}

fn environment_id(name: &str) -> Option<u32> {
  env::var(name).ok()?.parse().ok()
}

fn validate_config_root(path: &Path, owner_uid: u32) -> Result<()> {
  if !path.is_absolute() {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  let metadata = fs::symlink_metadata(path)?;
  if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != owner_uid {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  let runtime = path.join("runtime.yaml");
  let runtime_metadata = fs::symlink_metadata(&runtime)?;
  if runtime_metadata.file_type().is_symlink()
    || !runtime_metadata.is_file()
    || runtime_metadata.uid() != owner_uid
  {
    return Err(Error::UnsafePath(runtime));
  }
  Ok(())
}

fn validate_source_binary(path: &Path, owner_uid: u32) -> Result<()> {
  if !path.is_absolute() {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  let metadata = fs::symlink_metadata(path)?;
  let mode = metadata.permissions().mode();
  if metadata.file_type().is_symlink()
    || !metadata.is_file()
    || metadata.uid() != owner_uid
    || mode & 0o022 != 0
    || mode & 0o111 == 0
  {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  Ok(())
}

fn ensure_directory(path: &Path, mode: u32) -> Result<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {},
    Ok(_) => return Err(Error::UnsafePath(path.to_path_buf())),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)?,
    Err(error) => return Err(error.into()),
  }
  fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
  Ok(())
}

fn open_source(path: &Path, owner_uid: u32) -> Result<File> {
  let nofollow = i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).map_err(|_| {
    Error::InvalidInstallation("O_NOFOLLOW does not fit the platform flag type".to_string())
  })?;
  let file = OpenOptions::new()
    .read(true)
    .custom_flags(nofollow)
    .open(path)?;
  let metadata = file.metadata()?;
  let mode = metadata.permissions().mode();
  if !metadata.is_file() || metadata.uid() != owner_uid || mode & 0o022 != 0 || mode & 0o111 == 0 {
    return Err(Error::UnsafePath(path.to_path_buf()));
  }
  Ok(file)
}

fn atomic_copy(source: &Path, destination: &Path, owner_uid: u32, mode: u32) -> Result<()> {
  let mut source = open_source(source, owner_uid)?;
  let mut bytes = Vec::new();
  source.read_to_end(&mut bytes)?;
  atomic_write(destination, &bytes, mode)
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
  reject_symlink(path)?;
  let parent = path
    .parent()
    .ok_or_else(|| Error::UnsafePath(path.to_path_buf()))?;
  let temporary = parent.join(format!(
    ".{}.{}.tmp",
    path
      .file_name()
      .and_then(OsStr::to_str)
      .unwrap_or("rsclash"),
    std::process::id()
  ));
  reject_existing_path(&temporary)?;
  let mut file = OpenOptions::new()
    .create_new(true)
    .write(true)
    .mode(mode)
    .open(&temporary)?;
  let result = (|| -> Result<()> {
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(&temporary);
  }
  result
}

fn reject_symlink(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::UnsafePath(path.to_path_buf())),
    Ok(_) => Ok(()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error.into()),
  }
}

fn reject_existing_path(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path) {
    Ok(_) => Err(Error::UnsafePath(path.to_path_buf())),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error.into()),
  }
}

fn remove_regular_file_if_present(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
      fs::remove_file(path).map_err(Into::into)
    },
    Ok(_) => Err(Error::UnsafePath(path.to_path_buf())),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error.into()),
  }
}

fn render_systemd_unit(
  layout: &InstallLayout,
  config_root: &Path,
  identity: InstallIdentity,
) -> String {
  format!(
    "[Unit]\n\
Description=rsclash privileged network service\n\
Wants=network-online.target\n\
After=network-online.target\n\n\
[Service]\n\
Type=simple\n\
ExecStart={} --config {}\n\
User={}\n\
Group={}\n\
UMask=0077\n\
RuntimeDirectory=rsclash\n\
RuntimeDirectoryMode=0700\n\
Restart=on-failure\n\
RestartSec=2s\n\
TimeoutStopSec=20s\n\
KillMode=control-group\n\
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE\n\
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE\n\
NoNewPrivileges=true\n\
PrivateTmp=true\n\
ProtectSystem=strict\n\
ProtectHome=read-only\n\
ReadWritePaths={}\n\
ProtectHostname=true\n\
ProtectClock=true\n\
ProtectKernelTunables=true\n\
ProtectKernelModules=true\n\
ProtectControlGroups=true\n\
RestrictNamespaces=true\n\
RestrictRealtime=true\n\
RestrictSUIDSGID=true\n\
LockPersonality=true\n\
SystemCallArchitectures=native\n\
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK\n\
DeviceAllow=/dev/net/tun rw\n\n\
[Install]\n\
WantedBy=multi-user.target\n",
    systemd_quote(&layout.service_binary()),
    systemd_quote(&layout.service_config()),
    identity.uid,
    identity.gid,
    systemd_quote(config_root),
  )
}

fn systemd_quote(path: &Path) -> String {
  let value = path.to_string_lossy();
  let mut escaped = String::with_capacity(value.len() + 2);
  escaped.push('"');
  for character in value.chars() {
    match character {
      '\\' => escaped.push_str("\\\\"),
      '"' => escaped.push_str("\\\""),
      '\n' => escaped.push_str("\\n"),
      '\r' => escaped.push_str("\\r"),
      '\t' => escaped.push_str("\\t"),
      character => escaped.push(character),
    }
  }
  escaped.push('"');
  escaped
}

fn run_systemctl<const N: usize>(arguments: [&str; N]) -> Result<()> {
  let output = Command::new("systemctl").args(arguments).output()?;
  if output.status.success() {
    return Ok(());
  }
  let detail = if output.stderr.is_empty() {
    String::from_utf8_lossy(&output.stdout).into_owned()
  } else {
    String::from_utf8_lossy(&output.stderr).into_owned()
  };
  Err(Error::InstallCommand(format!(
    "systemctl exited with {}: {}",
    output.status,
    detail.trim()
  )))
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
  };

  use super::{
    InstallIdentity, InstallLayout, InstallRequest, SystemServiceInstaller, systemd_quote,
  };

  #[test]
  fn installs_fixed_files_and_a_hardened_user_service() {
    let directory = TestDirectory::new();
    let sources = directory.path().join("sources");
    let config_root = directory.path().join("user/config");
    fs::create_dir_all(&sources).expect("source directory should be created");
    fs::create_dir_all(&config_root).expect("config root should be created");
    fs::write(config_root.join("runtime.yaml"), b"mode: rule")
      .expect("runtime config should be written");
    let service = executable(&sources.join("rsclash-service"), b"service");
    let stable = executable(&sources.join("mihomo"), b"stable");
    let alpha = executable(&sources.join("mihomo-alpha"), b"alpha");
    let identity = InstallIdentity {
      uid: fs::metadata(&config_root)
        .expect("metadata should exist")
        .uid(),
      gid: fs::metadata(&config_root)
        .expect("metadata should exist")
        .gid(),
    };
    let layout = InstallLayout::under(&directory.path().join("root"));
    let installer = SystemServiceInstaller::new(layout.clone());
    let installed = installer
      .install_files(
        &InstallRequest {
          service_binary: service,
          stable_core: stable,
          alpha_core: Some(alpha),
          config_root: config_root.clone(),
        },
        identity,
      )
      .expect("installation files should be generated");

    assert_eq!(
      fs::read(layout.stable_core()).ok().as_deref(),
      Some(b"stable".as_slice())
    );
    assert_eq!(
      fs::read(layout.alpha_core()).ok().as_deref(),
      Some(b"alpha".as_slice())
    );
    assert_eq!(installed.allowed_uid, identity.uid);
    assert_eq!(installed.data_directory, config_root);
    let unit = fs::read_to_string(layout.systemd_unit()).expect("unit should be readable");
    assert!(unit.contains(&format!("User={}", identity.uid)));
    assert!(unit.contains("AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE"));
    assert!(unit.contains("NoNewPrivileges=true"));
    assert!(unit.contains("DeviceAllow=/dev/net/tun rw"));
    assert!(!unit.contains("sudo"));
    assert_eq!(
      fs::metadata(layout.service_binary())
        .expect("installed service should exist")
        .permissions()
        .mode()
        & 0o777,
      0o755
    );
  }

  #[test]
  fn refuses_symlinked_sources_and_destinations() {
    let directory = TestDirectory::new();
    let source = executable(&directory.path().join("source"), b"binary");
    let link = directory.path().join("source-link");
    std::os::unix::fs::symlink(&source, &link).expect("source link should be created");
    let config_root = directory.path().join("config");
    fs::create_dir(&config_root).expect("config root should be created");
    fs::write(config_root.join("runtime.yaml"), b"mode: rule")
      .expect("runtime config should be written");
    let metadata = fs::metadata(&config_root).expect("metadata should exist");
    let identity = InstallIdentity {
      uid: metadata.uid(),
      gid: metadata.gid(),
    };
    let layout = InstallLayout::under(&directory.path().join("root"));
    let installer = SystemServiceInstaller::new(layout);
    assert!(
      installer
        .install_files(
          &InstallRequest {
            service_binary: link,
            stable_core: source,
            alpha_core: None,
            config_root,
          },
          identity,
        )
        .is_err()
    );
  }

  #[test]
  fn quotes_systemd_paths_without_shell_interpolation() {
    assert_eq!(
      systemd_quote(Path::new("/home/a b/quote\"slash\\")),
      "\"/home/a b/quote\\\"slash\\\\\""
    );
  }

  #[test]
  fn generated_unit_passes_systemd_verification_when_available() {
    let directory = TestDirectory::new();
    let sources = directory.path().join("sources");
    let config_root = directory.path().join("user/config");
    fs::create_dir_all(&sources).expect("source directory should be created");
    fs::create_dir_all(&config_root).expect("config root should be created");
    fs::write(config_root.join("runtime.yaml"), b"mode: rule")
      .expect("runtime config should be written");
    let service = executable(&sources.join("rsclash-service"), b"service");
    let stable = executable(&sources.join("mihomo"), b"stable");
    let metadata = fs::metadata(&config_root).expect("metadata should exist");
    let identity = InstallIdentity {
      uid: metadata.uid(),
      gid: metadata.gid(),
    };
    let layout = InstallLayout::under(&directory.path().join("root"));
    SystemServiceInstaller::new(layout.clone())
      .install_files(
        &InstallRequest {
          service_binary: service,
          stable_core: stable,
          alpha_core: None,
          config_root,
        },
        identity,
      )
      .expect("installation files should be generated");

    let output = match std::process::Command::new("systemd-analyze")
      .arg("verify")
      .arg(layout.systemd_unit())
      .output()
    {
      Ok(output) => output,
      Err(_) => return,
    };
    assert!(
      output.status.success(),
      "systemd unit verification failed: {}",
      String::from_utf8_lossy(&output.stderr)
    );
  }

  fn executable(path: &Path, content: &[u8]) -> PathBuf {
    fs::write(path, content).expect("source should be written");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
      .expect("source should be executable");
    path.to_path_buf()
  }

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-service-install-test-{}-{}",
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
