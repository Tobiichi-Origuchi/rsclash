use std::error::Error;

#[cfg(target_os = "linux")]
use std::{env, ffi::OsString, path::PathBuf, process::Command};

#[cfg(target_os = "linux")]
use rsclash_config::initialize_default_runtime;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn Error>> {
  let arguments = parse_arguments()?;
  initialize_default_runtime(&arguments.config_root)?;
  let status = Command::new("pkexec")
    .arg(&arguments.installer)
    .arg("--service-binary")
    .arg(&arguments.service_binary)
    .arg("--stable-core")
    .arg(&arguments.stable_core)
    .args(
      arguments
        .alpha_core
        .as_ref()
        .map(|path| [OsString::from("--alpha-core"), path.as_os_str().to_owned()])
        .into_iter()
        .flatten(),
    )
    .arg("--config-root")
    .arg(&arguments.config_root)
    .status()?;
  if status.success() {
    Ok(())
  } else {
    Err(format!("elevated service installer exited with {status}").into())
  }
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn Error>> {
  Err("rsclash-service-setup is not implemented on this platform".into())
}

#[cfg(target_os = "linux")]
struct SetupArguments {
  installer: PathBuf,
  service_binary: PathBuf,
  stable_core: PathBuf,
  alpha_core: Option<PathBuf>,
  config_root: PathBuf,
}

#[cfg(target_os = "linux")]
fn parse_arguments() -> Result<SetupArguments, String> {
  let executable = env::current_exe().map_err(|error| error.to_string())?;
  let sibling = |name: &str| executable.with_file_name(name);
  let mut installer = None;
  let mut service_binary = None;
  let mut stable_core = None;
  let mut alpha_core = None;
  let mut config_root = None;
  let mut arguments = env::args_os().skip(1);
  while let Some(flag) = arguments.next() {
    let value = arguments
      .next()
      .ok_or_else(|| format!("missing value for {}", flag.to_string_lossy()))?;
    match flag.to_str() {
      Some("--installer") => set_once(&mut installer, value, "--installer")?,
      Some("--service-binary") => set_once(&mut service_binary, value, "--service-binary")?,
      Some("--stable-core") => set_once(&mut stable_core, value, "--stable-core")?,
      Some("--alpha-core") => set_once(&mut alpha_core, value, "--alpha-core")?,
      Some("--config-root") => set_once(&mut config_root, value, "--config-root")?,
      _ => return Err(format!("unknown argument: {}", flag.to_string_lossy())),
    }
  }
  Ok(SetupArguments {
    installer: installer.unwrap_or_else(|| sibling("rsclash-service-install")),
    service_binary: service_binary.unwrap_or_else(|| sibling("rsclash-service")),
    stable_core: stable_core.ok_or_else(usage)?,
    alpha_core,
    config_root: config_root.map_or_else(default_config_root, Ok)?,
  })
}

#[cfg(target_os = "linux")]
fn default_config_root() -> Result<PathBuf, String> {
  if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
    let path = PathBuf::from(path);
    if path.is_absolute() {
      return Ok(path.join("rsclash"));
    }
  }
  env::var_os("HOME")
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
    .map(|path| path.join(".config/rsclash"))
    .ok_or_else(|| "HOME is not set; pass --config-root".to_string())
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
  "usage: rsclash-service-setup --stable-core PATH [--alpha-core PATH] [--config-root PATH] [--installer PATH] [--service-binary PATH]".to_string()
}
