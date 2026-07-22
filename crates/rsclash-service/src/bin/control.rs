use std::error::Error;

#[cfg(target_os = "linux")]
use std::{
  env,
  io::{self, Write as _},
  path::{Path, PathBuf},
  time::Duration,
};

#[cfg(target_os = "linux")]
use async_trait::async_trait;
#[cfg(target_os = "linux")]
use rsclash_config::{
  CommandRuntimeValidator, MihomoConfig, RuntimeActivator, RuntimeDeployer, RuntimeStore,
};
#[cfg(target_os = "linux")]
use rsclash_domain::{CoreChannel, CoreState};
#[cfg(target_os = "linux")]
use rsclash_service::{
  DEFAULT_INSTALLED_CORE, DEFAULT_SERVICE_SOCKET, ServiceClient, ServiceCommand,
};
#[cfg(target_os = "linux")]
use serde_yaml_ng::{Mapping, Value};

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
  let command = env::args().nth(1).ok_or_else(usage)?;
  if env::args().nth(2).is_some() {
    return Err(usage().into());
  }
  let client = ServiceClient::new(DEFAULT_SERVICE_SOCKET);
  let output = match command.as_str() {
    "ping" => serde_json::json!({ "service_version": client.ping().await? }),
    "status" => serde_json::to_value(client.status().await?)?,
    "start-stable" => serde_json::to_value(
      client
        .command(ServiceCommand::StartCore {
          channel: CoreChannel::Stable,
        })
        .await?,
    )?,
    "start-alpha" => serde_json::to_value(
      client
        .command(ServiceCommand::StartCore {
          channel: CoreChannel::Alpha,
        })
        .await?,
    )?,
    "reload" => serde_json::to_value(client.command(ServiceCommand::ReloadCore).await?)?,
    "stop" => serde_json::to_value(client.command(ServiceCommand::StopCore).await?)?,
    "tun-smoke" => run_tun_smoke(&client).await?,
    _ => return Err(usage().into()),
  };
  let stdout = io::stdout();
  let mut stdout = stdout.lock();
  serde_json::to_writer_pretty(&mut stdout, &output)?;
  writeln!(stdout)?;
  Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn Error>> {
  Err("rsclash-service-control is not implemented on this platform".into())
}

#[cfg(target_os = "linux")]
fn usage() -> String {
  "usage: rsclash-service-control <ping|status|start-stable|start-alpha|reload|stop|tun-smoke>"
    .to_string()
}

#[cfg(target_os = "linux")]
async fn run_tun_smoke(client: &ServiceClient) -> Result<serde_json::Value, Box<dyn Error>> {
  let status = client.status().await?;
  if status.core != CoreState::Stopped {
    return Err("TUN smoke test requires the service core to be stopped".into());
  }
  let config_root = default_config_root()?;
  let runtime_path = config_root.join("runtime.yaml");
  let original_source = std::fs::read_to_string(&runtime_path)?;
  let original = MihomoConfig::parse(&original_source)?;
  let smoke = smoke_config(original.clone());
  let store = RuntimeStore::open(&runtime_path)?;
  let validator = CommandRuntimeValidator::new(DEFAULT_INSTALLED_CORE, &config_root);
  let activator = NoopActivator;
  let deployer = RuntimeDeployer::new(&store, &validator, &activator);
  deployer.deploy(&smoke).await?;

  let test_result = run_tun_smoke_inner(client).await;
  let stop_result = client.command(ServiceCommand::StopCore).await;
  let restore_result = deployer.deploy(&original).await;
  test_result.map_err(|error| -> Box<dyn Error> { error.into() })?;
  stop_result?;
  restore_result?;
  Ok(serde_json::json!({
    "device": "rsclash",
    "auto_route": false,
    "created_and_removed": true,
  }))
}

#[cfg(target_os = "linux")]
async fn run_tun_smoke_inner(client: &ServiceClient) -> Result<(), String> {
  let state = client
    .command(ServiceCommand::StartCore {
      channel: CoreChannel::Stable,
    })
    .await
    .map_err(|error| error.to_string())?;
  if !matches!(state, CoreState::Running { .. }) {
    return Err(format!(
      "service returned an unexpected start state: {state:?}"
    ));
  }
  wait_for_interface(true).await?;
  client
    .command(ServiceCommand::StopCore)
    .await
    .map_err(|error| error.to_string())?;
  wait_for_interface(false).await
}

#[cfg(target_os = "linux")]
fn smoke_config(mut config: MihomoConfig) -> MihomoConfig {
  let mut tun = config
    .get("tun")
    .and_then(Value::as_mapping)
    .cloned()
    .unwrap_or_else(Mapping::new);
  tun.insert("enable".into(), Value::Bool(true));
  tun.insert("device".into(), Value::String("rsclash".to_string()));
  tun.insert("auto-route".into(), Value::Bool(false));
  tun.insert("auto-redirect".into(), Value::Bool(false));
  tun.insert("strict-route".into(), Value::Bool(false));
  tun.insert("dns-hijack".into(), Value::Sequence(Vec::new()));
  config.insert("tun", Value::Mapping(tun));
  config
}

#[cfg(target_os = "linux")]
async fn wait_for_interface(expected: bool) -> Result<(), String> {
  let path = Path::new("/sys/class/net/rsclash");
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      if path.exists() == expected {
        return;
      }
      tokio::time::sleep(Duration::from_millis(25)).await;
    }
  })
  .await
  .map_err(|_| {
    format!(
      "rsclash TUN interface did not become {} before the timeout",
      if expected { "present" } else { "absent" }
    )
  })?;
  Ok(())
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
    .ok_or_else(|| "HOME is not set".to_string())
}

#[cfg(target_os = "linux")]
struct NoopActivator;

#[cfg(target_os = "linux")]
#[async_trait]
impl RuntimeActivator for NoopActivator {
  async fn reload(&self, _runtime_path: &Path) -> rsclash_config::Result<()> {
    Ok(())
  }

  async fn restart(&self, _runtime_path: &Path) -> rsclash_config::Result<()> {
    Ok(())
  }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use serde_yaml_ng::Value;

  use super::smoke_config;
  use rsclash_config::MihomoConfig;

  #[test]
  fn smoke_config_enables_only_an_isolated_tun_device() {
    let original = MihomoConfig::parse(
      "mixed-port: 17897\ntun:\n  enable: false\n  auto-route: true\ncustom: preserved\n",
    )
    .expect("the source config should parse");
    let smoke = smoke_config(original);
    let tun = smoke
      .get("tun")
      .and_then(Value::as_mapping)
      .expect("the TUN mapping should exist");

    assert_eq!(tun.get("enable").and_then(Value::as_bool), Some(true));
    assert_eq!(tun.get("device").and_then(Value::as_str), Some("rsclash"));
    assert_eq!(tun.get("auto-route").and_then(Value::as_bool), Some(false));
    assert_eq!(
      tun.get("auto-redirect").and_then(Value::as_bool),
      Some(false)
    );
    assert!(
      tun
        .get("dns-hijack")
        .and_then(Value::as_sequence)
        .is_some_and(Vec::is_empty)
    );
    assert_eq!(
      smoke.get("custom").and_then(Value::as_str),
      Some("preserved")
    );
    assert_eq!(
      smoke.get("mixed-port").and_then(Value::as_u64),
      Some(17_897)
    );
  }
}
