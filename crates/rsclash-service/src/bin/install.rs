use std::error::Error;

#[cfg(target_os = "linux")]
use rsclash_service::{InstallIdentity, InstallLayout, InstallRequest, SystemServiceInstaller};
#[cfg(target_os = "linux")]
use std::{env, ffi::OsString, path::PathBuf};

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn Error>> {
  match parse_arguments()? {
    InstallerCommand::Install(request) => {
      let identity = InstallIdentity::from_elevation_environment(&request.config_root)?;
      SystemServiceInstaller::new(InstallLayout::system()).install(&request, identity)?;
    },
    InstallerCommand::Uninstall => {
      SystemServiceInstaller::new(InstallLayout::system()).uninstall()?;
    },
  }
  Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn Error>> {
  Err("rsclash-service-install is not implemented on this platform".into())
}

#[cfg(target_os = "linux")]
enum InstallerCommand {
  Install(InstallRequest),
  Uninstall,
}

#[cfg(target_os = "linux")]
fn parse_arguments() -> Result<InstallerCommand, String> {
  let mut service_binary = None;
  let mut stable_core = None;
  let mut alpha_core = None;
  let mut config_root = None;
  let mut uninstall = false;
  let mut arguments = env::args_os().skip(1);
  while let Some(flag) = arguments.next() {
    if flag == "--uninstall" {
      uninstall = true;
      continue;
    }
    let value = arguments
      .next()
      .ok_or_else(|| format!("missing value for {}", flag.to_string_lossy()))?;
    match flag.to_str() {
      Some("--service-binary") => set_once(&mut service_binary, value, "--service-binary")?,
      Some("--stable-core") => set_once(&mut stable_core, value, "--stable-core")?,
      Some("--alpha-core") => set_once(&mut alpha_core, value, "--alpha-core")?,
      Some("--config-root") => set_once(&mut config_root, value, "--config-root")?,
      _ => return Err(format!("unknown argument: {}", flag.to_string_lossy())),
    }
  }
  if uninstall {
    if service_binary.is_some()
      || stable_core.is_some()
      || alpha_core.is_some()
      || config_root.is_some()
    {
      return Err("--uninstall cannot be combined with install arguments".to_string());
    }
    return Ok(InstallerCommand::Uninstall);
  }
  Ok(InstallerCommand::Install(InstallRequest {
    service_binary: service_binary.ok_or_else(usage)?,
    stable_core: stable_core.ok_or_else(usage)?,
    alpha_core,
    config_root: config_root.ok_or_else(usage)?,
  }))
}

#[cfg(target_os = "linux")]
fn set_once(target: &mut Option<PathBuf>, value: OsString, flag: &str) -> Result<(), String> {
  if target.is_some() {
    return Err(format!("duplicate argument: {flag}"));
  }
  *target = Some(PathBuf::from(value));
  Ok(())
}

#[cfg(target_os = "linux")]
fn usage() -> String {
  "usage: rsclash-service-install (--service-binary PATH --stable-core PATH [--alpha-core PATH] \
   --config-root PATH | --uninstall)"
    .to_string()
}
