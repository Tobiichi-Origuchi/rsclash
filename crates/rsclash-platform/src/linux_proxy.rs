use std::{
  env,
  process::{Command, Output},
  sync::Arc,
};

use async_trait::async_trait;

use crate::{
  Error, PendingSystemRecovery, Result, SystemProxyBackend, SystemProxySnapshot,
  SystemRecoveryBackend,
};

const ROOT_SCHEMA: &str = "org.gnome.system.proxy";
const HTTP_SCHEMA: &str = "org.gnome.system.proxy.http";
const HTTPS_SCHEMA: &str = "org.gnome.system.proxy.https";
const SOCKS_SCHEMA: &str = "org.gnome.system.proxy.socks";

trait SettingsRunner: Send + Sync {
  fn get(&self, schema: &str, key: &str) -> Result<String>;
  fn set(&self, schema: &str, key: &str, value: &str) -> Result<()>;
}

struct ProcessSettingsRunner;

impl ProcessSettingsRunner {
  fn command() -> Command {
    let mut command = Command::new("gsettings");
    if env::var_os("APPIMAGE").is_some() {
      command.env_remove("LD_LIBRARY_PATH");
    }
    command
  }

  fn check(operation: &str, output: Output) -> Result<String> {
    if output.status.success() {
      String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| Error::Platform(format!("gsettings returned invalid UTF-8 for {operation}")))
    } else {
      let detail = String::from_utf8_lossy(&output.stderr);
      Err(Error::Platform(format!(
        "gsettings failed to {operation}: {}",
        detail.trim()
      )))
    }
  }
}

impl SettingsRunner for ProcessSettingsRunner {
  fn get(&self, schema: &str, key: &str) -> Result<String> {
    let output = Self::command()
      .args(["get", schema, key])
      .output()
      .map_err(|source| Error::Platform(format!("start gsettings: {source}")))?;
    Self::check(&format!("read {schema} {key}"), output)
  }

  fn set(&self, schema: &str, key: &str, value: &str) -> Result<()> {
    let output = Self::command()
      .args(["set", schema, key, value])
      .output()
      .map_err(|source| Error::Platform(format!("start gsettings: {source}")))?;
    Self::check(&format!("write {schema} {key}"), output).map(|_| ())
  }
}

#[derive(Clone)]
pub struct LinuxSystemProxyBackend {
  runner: Arc<dyn SettingsRunner>,
}

impl LinuxSystemProxyBackend {
  pub fn new() -> Self {
    Self {
      runner: Arc::new(ProcessSettingsRunner),
    }
  }

  fn current_sync(&self) -> Result<SystemProxySnapshot> {
    let mode = parse_variant_string(&self.runner.get(ROOT_SCHEMA, "mode")?)?;
    Ok(SystemProxySnapshot {
      enabled: mode == "manual",
      backend: Some(self.name().to_string()),
      mode: Some(mode),
      http_proxy: self.read_endpoint(HTTP_SCHEMA)?,
      https_proxy: self.read_endpoint(HTTPS_SCHEMA)?,
      socks_proxy: self.read_endpoint(SOCKS_SCHEMA)?,
      bypass: parse_variant_string_list(&self.runner.get(ROOT_SCHEMA, "ignore-hosts")?)?,
      auto_config_url: Some(parse_variant_string(
        &self.runner.get(ROOT_SCHEMA, "autoconfig-url")?,
      )?),
    })
  }

  fn apply_sync(&self, snapshot: &SystemProxySnapshot) -> Result<()> {
    if snapshot
      .backend
      .as_deref()
      .is_some_and(|backend| backend != self.name())
    {
      return Err(Error::Unsupported(format!(
        "cannot restore {} system proxy settings with the {} backend",
        snapshot.backend.as_deref().unwrap_or("unknown"),
        self.name()
      )));
    }
    self.write_endpoint(HTTP_SCHEMA, snapshot.http_proxy.as_deref())?;
    self.write_endpoint(HTTPS_SCHEMA, snapshot.https_proxy.as_deref())?;
    self.write_endpoint(SOCKS_SCHEMA, snapshot.socks_proxy.as_deref())?;
    self.runner.set(
      ROOT_SCHEMA,
      "ignore-hosts",
      &render_variant_string_list(&snapshot.bypass),
    )?;
    if let Some(url) = snapshot.auto_config_url.as_deref() {
      self
        .runner
        .set(ROOT_SCHEMA, "autoconfig-url", &quote_variant_string(url))?;
    }
    let mode = snapshot
      .mode
      .as_deref()
      .unwrap_or(if snapshot.enabled { "manual" } else { "none" });
    if !matches!(mode, "none" | "manual" | "auto") {
      return Err(Error::Platform(format!(
        "unsupported GSettings proxy mode {mode}"
      )));
    }
    self
      .runner
      .set(ROOT_SCHEMA, "mode", &quote_variant_string(mode))
  }

  fn read_endpoint(&self, schema: &str) -> Result<Option<String>> {
    let host = parse_variant_string(&self.runner.get(schema, "host")?)?;
    let port = self
      .runner
      .get(schema, "port")?
      .parse::<u16>()
      .map_err(|_| Error::Platform(format!("invalid GSettings proxy port for {schema}")))?;
    if host.is_empty() || port == 0 {
      Ok(None)
    } else {
      Ok(Some(super::format_endpoint(&host, port)))
    }
  }

  fn write_endpoint(&self, schema: &str, endpoint: Option<&str>) -> Result<()> {
    let (host, port) = endpoint.map_or_else(|| Ok((String::new(), 0)), parse_endpoint)?;
    self
      .runner
      .set(schema, "host", &quote_variant_string(&host))?;
    self.runner.set(schema, "port", &port.to_string())
  }

  #[cfg(test)]
  fn with_runner(runner: Arc<dyn SettingsRunner>) -> Self {
    Self { runner }
  }
}

impl Default for LinuxSystemProxyBackend {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl SystemProxyBackend for LinuxSystemProxyBackend {
  fn name(&self) -> &'static str {
    "gsettings"
  }

  async fn current(&self) -> Result<SystemProxySnapshot> {
    let backend = self.clone();
    tokio::task::spawn_blocking(move || backend.current_sync())
      .await
      .map_err(|error| Error::Platform(format!("system proxy read task failed: {error}")))?
  }

  async fn apply(&self, snapshot: &SystemProxySnapshot) -> Result<()> {
    let backend = self.clone();
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || backend.apply_sync(&snapshot))
      .await
      .map_err(|error| Error::Platform(format!("system proxy write task failed: {error}")))?
  }
}

#[async_trait]
impl SystemRecoveryBackend for LinuxSystemProxyBackend {
  async fn restore(&self, pending: &PendingSystemRecovery) -> Result<()> {
    if pending.tun_enabled_by_app {
      return Err(Error::Unsupported(
        "Linux TUN recovery is not implemented by the system proxy backend".to_string(),
      ));
    }
    if let Some(snapshot) = &pending.system_proxy {
      if let Some(target) = pending.system_proxy_target.as_ref()
        && self.current().await? != *target
      {
        return Ok(());
      }
      self.apply(snapshot).await?;
    }
    Ok(())
  }
}

fn parse_endpoint(endpoint: &str) -> Result<(String, u16)> {
  let (host, port) = if let Some(rest) = endpoint.strip_prefix('[') {
    let (host, port) = rest
      .split_once("]:")
      .ok_or_else(|| Error::Platform(format!("invalid proxy endpoint {endpoint}")))?;
    (host, port)
  } else {
    endpoint
      .rsplit_once(':')
      .ok_or_else(|| Error::Platform(format!("invalid proxy endpoint {endpoint}")))?
  };
  let port = port
    .parse::<u16>()
    .map_err(|_| Error::Platform(format!("invalid proxy endpoint port in {endpoint}")))?;
  if host.is_empty() || port == 0 {
    return Err(Error::Platform(format!(
      "proxy endpoint must contain a host and non-zero port: {endpoint}"
    )));
  }
  Ok((host.to_string(), port))
}

fn parse_variant_string(value: &str) -> Result<String> {
  let value = value.trim();
  if value.len() < 2 || !value.starts_with('\'') || !value.ends_with('\'') {
    return Err(Error::Platform(format!(
      "invalid GVariant string returned by gsettings: {value}"
    )));
  }
  unescape_variant_string(&value[1..value.len() - 1])
}

fn parse_variant_string_list(value: &str) -> Result<Vec<String>> {
  let value = value.trim();
  let Some(inner) = value
    .strip_prefix('[')
    .and_then(|value| value.strip_suffix(']'))
  else {
    return Err(Error::Platform(format!(
      "invalid GVariant string list returned by gsettings: {value}"
    )));
  };
  let mut result = Vec::new();
  let mut chars = inner.chars().peekable();
  loop {
    while chars
      .peek()
      .is_some_and(|value| value.is_whitespace() || *value == ',')
    {
      let _ = chars.next();
    }
    let Some(quote) = chars.next() else {
      break;
    };
    if quote != '\'' {
      return Err(Error::Platform(
        "GVariant string list contains a non-string value".to_string(),
      ));
    }
    let mut escaped = false;
    let mut item = String::new();
    loop {
      let Some(character) = chars.next() else {
        return Err(Error::Platform(
          "GVariant string list has an unterminated value".to_string(),
        ));
      };
      if escaped {
        item.push(character);
        escaped = false;
      } else if character == '\\' {
        escaped = true;
      } else if character == '\'' {
        break;
      } else {
        item.push(character);
      }
    }
    result.push(item);
  }
  Ok(result)
}

fn unescape_variant_string(value: &str) -> Result<String> {
  let mut result = String::with_capacity(value.len());
  let mut escaped = false;
  for character in value.chars() {
    if escaped {
      result.push(character);
      escaped = false;
    } else if character == '\\' {
      escaped = true;
    } else {
      result.push(character);
    }
  }
  if escaped {
    Err(Error::Platform(
      "GVariant string ends with an incomplete escape".to_string(),
    ))
  } else {
    Ok(result)
  }
}

fn quote_variant_string(value: &str) -> String {
  format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn render_variant_string_list(values: &[String]) -> String {
  format!(
    "[{}]",
    values
      .iter()
      .map(|value| quote_variant_string(value))
      .collect::<Vec<_>>()
      .join(", ")
  )
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    collections::BTreeMap,
    env,
    sync::{Arc, Mutex},
  };

  use super::{
    HTTP_SCHEMA, HTTPS_SCHEMA, LinuxSystemProxyBackend, ROOT_SCHEMA, SOCKS_SCHEMA, SettingsRunner,
    parse_endpoint, parse_variant_string_list, render_variant_string_list,
  };
  use crate::{Result, SystemProxyBackend as _, SystemProxyService, SystemProxySnapshot};

  #[derive(Default)]
  struct FakeSettings {
    values: Mutex<BTreeMap<(String, String), String>>,
  }

  impl FakeSettings {
    fn seeded() -> Self {
      let values = BTreeMap::from([
        (
          (ROOT_SCHEMA.to_string(), "mode".to_string()),
          "'auto'".to_string(),
        ),
        (
          (ROOT_SCHEMA.to_string(), "ignore-hosts".to_string()),
          "['localhost', '127.0.0.0/8']".to_string(),
        ),
        (
          (ROOT_SCHEMA.to_string(), "autoconfig-url".to_string()),
          "'https://proxy.example/proxy.pac'".to_string(),
        ),
        (
          (HTTP_SCHEMA.to_string(), "host".to_string()),
          "'old.example'".to_string(),
        ),
        (
          (HTTP_SCHEMA.to_string(), "port".to_string()),
          "3128".to_string(),
        ),
        (
          (HTTPS_SCHEMA.to_string(), "host".to_string()),
          "''".to_string(),
        ),
        (
          (HTTPS_SCHEMA.to_string(), "port".to_string()),
          "0".to_string(),
        ),
        (
          (SOCKS_SCHEMA.to_string(), "host".to_string()),
          "''".to_string(),
        ),
        (
          (SOCKS_SCHEMA.to_string(), "port".to_string()),
          "0".to_string(),
        ),
      ]);
      Self {
        values: Mutex::new(values),
      }
    }
  }

  impl SettingsRunner for FakeSettings {
    fn get(&self, schema: &str, key: &str) -> Result<String> {
      self
        .values
        .lock()
        .expect("settings lock should open")
        .get(&(schema.to_string(), key.to_string()))
        .cloned()
        .ok_or_else(|| crate::Error::Platform(format!("missing fake value {schema} {key}")))
    }

    fn set(&self, schema: &str, key: &str, value: &str) -> Result<()> {
      self
        .values
        .lock()
        .expect("settings lock should open")
        .insert((schema.to_string(), key.to_string()), value.to_string());
      Ok(())
    }
  }

  #[test]
  fn gvariant_lists_round_trip_escaped_values() {
    let values = vec![
      "localhost".to_string(),
      "host's-name".to_string(),
      r"folder\name".to_string(),
    ];
    assert_eq!(
      parse_variant_string_list(&render_variant_string_list(&values)).ok(),
      Some(values)
    );
  }

  #[test]
  fn endpoints_support_hostnames_and_ipv6() {
    assert_eq!(
      parse_endpoint("proxy.example:7890").ok(),
      Some(("proxy.example".to_string(), 7890))
    );
    assert_eq!(
      parse_endpoint("[::1]:7890").ok(),
      Some(("::1".to_string(), 7890))
    );
  }

  #[tokio::test]
  async fn reads_and_restores_every_gsettings_proxy_field() {
    let settings = Arc::new(FakeSettings::seeded());
    let runner: Arc<dyn SettingsRunner> = Arc::<FakeSettings>::clone(&settings);
    let backend = LinuxSystemProxyBackend::with_runner(runner);
    let original = backend.current().await.expect("settings should read");
    assert_eq!(original.mode.as_deref(), Some("auto"));
    assert_eq!(original.http_proxy.as_deref(), Some("old.example:3128"));
    assert_eq!(
      original.auto_config_url.as_deref(),
      Some("https://proxy.example/proxy.pac")
    );

    backend
      .apply(&SystemProxySnapshot {
        enabled: true,
        backend: Some("gsettings".to_string()),
        mode: Some("manual".to_string()),
        http_proxy: Some("127.0.0.1:17897".to_string()),
        https_proxy: Some("127.0.0.1:17897".to_string()),
        socks_proxy: Some("127.0.0.1:17897".to_string()),
        bypass: vec!["localhost".to_string()],
        auto_config_url: None,
      })
      .await
      .expect("manual settings should apply");
    assert_eq!(
      settings.get(ROOT_SCHEMA, "mode").ok().as_deref(),
      Some("'manual'")
    );

    backend
      .apply(&original)
      .await
      .expect("original settings should restore");
    assert_eq!(
      backend.current().await.expect("settings should read"),
      original
    );
  }

  #[tokio::test]
  #[ignore = "modifies the desktop proxy; requires RSCLASH_SYSTEM_PROXY_SMOKE_RECOVERY"]
  async fn real_gsettings_round_trip_restores_the_original_state() {
    let recovery_path = env::var_os("RSCLASH_SYSTEM_PROXY_SMOKE_RECOVERY")
      .expect("the explicit recovery journal path should be provided");
    let backend = Arc::new(LinuxSystemProxyBackend::new());
    let original = backend
      .current()
      .await
      .expect("the original desktop proxy should be readable");
    let service = SystemProxyService::new(
      recovery_path,
      Arc::<LinuxSystemProxyBackend>::clone(&backend),
    );

    let enable_result = service
      .enable(
        "127.0.0.1",
        17_897,
        vec![
          "localhost".to_string(),
          "127.0.0.0/8".to_string(),
          "::1".to_string(),
        ],
      )
      .await;
    let status_result = service.status().await;
    let mut restore_result = service.disable().await;
    let mut final_result = backend.current().await;
    if restore_result.is_err()
      || !final_result
        .as_ref()
        .is_ok_and(|current| current == &original)
    {
      backend
        .apply(&original)
        .await
        .expect("the emergency proxy restore should succeed");
      restore_result = service.disable().await;
      final_result = backend.current().await;
    }

    enable_result.expect("the real system proxy should enable");
    let status = status_result.expect("the enabled system proxy should be readable");
    assert!(status.enabled_by_app);
    assert!(status.applied);
    restore_result.expect("the original system proxy should restore");
    assert_eq!(
      final_result.expect("the restored system proxy should be readable"),
      original
    );
  }
}
