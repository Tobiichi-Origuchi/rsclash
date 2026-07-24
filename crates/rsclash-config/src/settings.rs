use std::collections::BTreeSet;

use rsclash_domain::{AppSettings, DnsEnhancedMode, StreamLogLevel, TunStack};
use serde_yaml_ng::{Mapping, Value};

use crate::{Error, MihomoConfig, Result};

pub fn validate_application_settings(settings: &AppSettings) -> Result<()> {
  let ports = [
    Some(settings.ports.mixed),
    settings.ports.socks,
    settings.ports.http,
    settings.ports.redir,
    settings.ports.tproxy,
  ]
  .into_iter()
  .flatten()
  .collect::<Vec<_>>();
  if ports.contains(&0) {
    return invalid("proxy listener ports must be non-zero");
  }
  if ports.iter().copied().collect::<BTreeSet<_>>().len() != ports.len() {
    return invalid("proxy listener ports must be unique");
  }
  if !(100..=60_000).contains(&settings.refresh_interval_ms) {
    return invalid("refresh interval must be between 100 and 60000 milliseconds");
  }
  if !(1..=6).contains(&settings.proxy_layout_columns) {
    return invalid("proxy layout columns must be between 1 and 6");
  }
  if !(100..=120_000).contains(&settings.latency_timeout_ms) {
    return invalid("latency timeout must be between 100 and 120000 milliseconds");
  }
  if !(1..=1_024).contains(&settings.app_log_max_size_mib)
    || !(1..=100).contains(&settings.app_log_max_count)
    || !(1..=365).contains(&settings.app_log_retention_days)
  {
    return invalid("application log limits exceed the supported bounds");
  }
  if settings.startup_script.len() > 64 * 1024 {
    return invalid("startup script exceeds the 64 KiB limit");
  }
  if settings
    .system_proxy_bypass
    .iter()
    .any(|entry| entry.trim().is_empty() || entry.contains(['\n', '\r']))
  {
    return invalid("system proxy bypass entries must be non-empty single-line values");
  }
  if let Some(url) = settings.pac_url.as_deref()
    && !valid_http_url(url)
  {
    return invalid("PAC URL must use HTTP or HTTPS");
  }
  if !valid_http_url(&settings.latency_test_url) {
    return invalid("latency test URL must use HTTP or HTTPS");
  }
  validate_known_unique_values(
    &settings.home_cards,
    &["profile", "proxy", "network", "traffic"],
    "home cards",
  )?;
  validate_known_unique_values(
    &settings.connection_columns,
    &["destination", "traffic", "process", "rule", "chains"],
    "connection columns",
  )?;
  if settings
    .network_interface
    .as_deref()
    .is_some_and(|interface| interface.contains(['\n', '\r']))
  {
    return invalid("network interface must be a single-line value");
  }
  if settings.controller.enabled {
    validate_socket_address(&settings.controller.address, "external controller address")?;
    let controller = settings
      .controller
      .address
      .parse::<std::net::SocketAddr>()
      .map_err(|_| {
        Error::InvalidConfiguration(
          "external controller address must be an IP socket address".to_string(),
        )
      })?;
    if !controller.ip().is_loopback() && settings.controller.secret.expose().is_empty() {
      return invalid("a non-loopback external controller requires a secret");
    }
    if settings.controller.allowed_origins.iter().any(|origin| {
      origin != "*" && !origin.starts_with("http://") && !origin.starts_with("https://")
    }) {
      return invalid("CORS origins must be *, HTTP, or HTTPS origins");
    }
  }
  if settings.dns.enabled {
    validate_socket_address(&settings.dns.listen, "DNS listen address")?;
    if settings.dns.nameservers.is_empty() {
      return invalid("DNS requires at least one nameserver");
    }
    if settings
      .dns
      .nameservers
      .iter()
      .chain(&settings.dns.default_nameservers)
      .chain(&settings.dns.fallback)
      .any(|server| server.trim().is_empty() || server.contains(['\n', '\r']))
    {
      return invalid("DNS server entries must be non-empty single-line values");
    }
  }
  for tunnel in &settings.tunnels {
    if tunnel.network.is_empty()
      || tunnel
        .network
        .iter()
        .any(|network| !matches!(network.as_str(), "tcp" | "udp"))
      || tunnel.address.trim().is_empty()
      || tunnel.target.trim().is_empty()
    {
      return invalid("tunnels require tcp/udp, a listen address, and a target");
    }
  }
  Ok(())
}

pub fn apply_application_settings(
  runtime: &mut MihomoConfig,
  settings: &AppSettings,
) -> Result<()> {
  validate_application_settings(settings)?;
  let mapping = runtime.mapping_mut();
  insert(mapping, "mixed-port", settings.ports.mixed);
  set_optional_port(mapping, "socks-port", settings.ports.socks);
  set_optional_port(mapping, "port", settings.ports.http);
  set_optional_port(mapping, "redir-port", settings.ports.redir);
  set_optional_port(mapping, "tproxy-port", settings.ports.tproxy);
  insert(mapping, "allow-lan", settings.allow_lan);
  insert(mapping, "ipv6", settings.ipv6);
  insert(mapping, "unified-delay", settings.unified_delay);
  insert(
    mapping,
    "log-level",
    match settings.mihomo_log_level {
      StreamLogLevel::Debug => "debug",
      StreamLogLevel::Info => "info",
      StreamLogLevel::Warning => "warning",
      StreamLogLevel::Error => "error",
      StreamLogLevel::Silent => "silent",
    },
  );

  if settings.controller.enabled {
    insert(
      mapping,
      "external-controller",
      settings.controller.address.clone(),
    );
    insert(
      mapping,
      "secret",
      settings.controller.secret.expose().to_string(),
    );
    let mut cors = Mapping::new();
    insert(
      &mut cors,
      "allow-private-network",
      settings.controller.allow_private_network,
    );
    insert(
      &mut cors,
      "allow-origins",
      settings.controller.allowed_origins.clone(),
    );
    mapping.insert("external-controller-cors".into(), Value::Mapping(cors));
  } else {
    mapping.remove("external-controller");
    mapping.remove("secret");
    mapping.remove("external-controller-cors");
  }

  let mut tun = mapping
    .get("tun")
    .and_then(Value::as_mapping)
    .cloned()
    .unwrap_or_default();
  insert(&mut tun, "enable", settings.tun_enabled);
  insert(
    &mut tun,
    "stack",
    match settings.tun_stack {
      TunStack::System => "system",
      TunStack::Gvisor => "gvisor",
      TunStack::Mixed => "mixed",
    },
  );
  insert(&mut tun, "device", "rsclash");
  mapping.insert("tun".into(), Value::Mapping(tun));

  if let Some(interface) = settings
    .network_interface
    .as_deref()
    .map(str::trim)
    .filter(|value| !value.is_empty())
  {
    insert(mapping, "interface-name", interface);
  } else {
    mapping.remove("interface-name");
  }

  let mut dns = Mapping::new();
  insert(&mut dns, "enable", settings.dns.enabled);
  insert(&mut dns, "listen", settings.dns.listen.clone());
  insert(&mut dns, "ipv6", settings.dns.ipv6);
  insert(
    &mut dns,
    "enhanced-mode",
    match settings.dns.enhanced_mode {
      DnsEnhancedMode::Normal => "normal",
      DnsEnhancedMode::RedirHost => "redir-host",
      DnsEnhancedMode::FakeIp => "fake-ip",
    },
  );
  insert(
    &mut dns,
    "fake-ip-range",
    settings.dns.fake_ip_range.clone(),
  );
  insert(
    &mut dns,
    "default-nameserver",
    settings.dns.default_nameservers.clone(),
  );
  insert(&mut dns, "nameserver", settings.dns.nameservers.clone());
  if !settings.dns.fallback.is_empty() {
    insert(&mut dns, "fallback", settings.dns.fallback.clone());
  }
  mapping.insert("dns".into(), Value::Mapping(dns));

  if settings.tunnels.is_empty() {
    mapping.remove("tunnels");
  } else {
    let tunnels = serde_yaml_ng::to_value(&settings.tunnels).map_err(Error::EncodeYaml)?;
    mapping.insert("tunnels".into(), tunnels);
  }
  Ok(())
}

fn validate_known_unique_values(values: &[String], supported: &[&str], label: &str) -> Result<()> {
  let mut seen = BTreeSet::new();
  if values
    .iter()
    .any(|value| !supported.contains(&value.as_str()) || !seen.insert(value))
  {
    return invalid(format!("{label} contain unsupported or duplicate values"));
  }
  Ok(())
}

fn set_optional_port(mapping: &mut Mapping, key: &str, value: Option<u16>) {
  if let Some(port) = value {
    insert(mapping, key, port);
  } else {
    mapping.remove(key);
  }
}

fn insert(mapping: &mut Mapping, key: &str, value: impl Into<Value>) {
  mapping.insert(key.into(), value.into());
}

fn validate_socket_address(value: &str, name: &str) -> Result<()> {
  let Some((host, port)) = value.rsplit_once(':') else {
    return invalid(format!("{name} must use host:port"));
  };
  if host.trim_matches(['[', ']']).is_empty()
    || port.parse::<u16>().ok().filter(|port| *port != 0).is_none()
  {
    return invalid(format!("{name} must use a valid host and non-zero port"));
  }
  Ok(())
}

fn valid_http_url(value: &str) -> bool {
  ["http://", "https://"]
    .into_iter()
    .any(|prefix| value.starts_with(prefix) && value.len() > prefix.len())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
  Err(Error::InvalidConfiguration(message.into()))
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use rsclash_domain::AppSettings;

  use crate::{MihomoConfig, apply_application_settings, validate_application_settings};

  #[test]
  fn rejects_duplicate_listener_ports() {
    let mut settings = AppSettings::default();
    settings.ports.http = Some(settings.ports.mixed);

    assert!(validate_application_settings(&settings).is_err());
  }

  #[test]
  fn rejects_insecure_or_unknown_application_preferences() {
    let mut settings = AppSettings::default();
    settings.controller.enabled = true;
    settings.controller.address = "0.0.0.0:9090".to_string();
    assert!(validate_application_settings(&settings).is_err());

    settings.controller.address = "127.0.0.1:9090".to_string();
    settings.home_cards.push("unknown".to_string());
    assert!(validate_application_settings(&settings).is_err());
  }

  #[test]
  fn applies_complete_native_settings_without_changing_tun_device() {
    let mut runtime =
      MihomoConfig::parse("mixed-port: 7890\ntun: {device: old}\nrules: [MATCH,DIRECT]\n")
        .expect("runtime should parse");
    let mut settings = AppSettings {
      allow_lan: true,
      ipv6: true,
      unified_delay: true,
      tun_enabled: true,
      ..AppSettings::default()
    };
    settings.ports.http = Some(17_898);
    settings.dns.enabled = true;

    apply_application_settings(&mut runtime, &settings).expect("settings should apply");

    assert_eq!(
      runtime.get("mixed-port").and_then(|value| value.as_u64()),
      Some(17_897)
    );
    assert_eq!(
      runtime.get("allow-lan").and_then(Value::as_bool),
      Some(true)
    );
    let tun = runtime
      .get("tun")
      .and_then(Value::as_mapping)
      .expect("TUN should exist");
    assert_eq!(tun.get("device").and_then(Value::as_str), Some("rsclash"));
    assert_eq!(tun.get("enable").and_then(Value::as_bool), Some(true));
  }

  use serde_yaml_ng::Value;
}
