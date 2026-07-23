use serde_yaml_ng::{Mapping, Value};

const COMPATIBILITY_DEFAULTS: &[NativeTransform] = &[
  NativeTransform::GuardLegacyScriptMode,
  NativeTransform::NormalizeHysteriaAlpn,
  NativeTransform::MigrateLegacyWebSocketOptions,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeTransform {
  GuardLegacyScriptMode,
  NormalizeHysteriaAlpn,
  MigrateLegacyWebSocketOptions,
  EnableProxyUdp,
}

impl NativeTransform {
  #[must_use]
  pub const fn compatibility_defaults() -> &'static [Self] {
    COMPATIBILITY_DEFAULTS
  }

  pub fn apply(self, config: &mut Mapping) {
    match self {
      Self::GuardLegacyScriptMode => guard_legacy_script_mode(config),
      Self::NormalizeHysteriaAlpn => normalize_hysteria_alpn(config),
      Self::MigrateLegacyWebSocketOptions => migrate_legacy_websocket_options(config),
      Self::EnableProxyUdp => enable_proxy_udp(config),
    }
  }
}

fn guard_legacy_script_mode(config: &mut Mapping) {
  let is_legacy_script_mode = config
    .get("mode")
    .and_then(Value::as_str)
    .is_some_and(|mode| mode.eq_ignore_ascii_case("script"));
  if is_legacy_script_mode {
    config.insert("mode".into(), Value::String("rule".to_string()));
  }
}

fn normalize_hysteria_alpn(config: &mut Mapping) {
  for proxy in proxy_mappings(config) {
    let is_hysteria = proxy
      .get("type")
      .and_then(Value::as_str)
      .is_some_and(|kind| kind.eq_ignore_ascii_case("hysteria"));
    if !is_hysteria {
      continue;
    }
    let Some(alpn) = proxy
      .get("alpn")
      .and_then(Value::as_str)
      .map(str::to_string)
    else {
      continue;
    };
    proxy.insert("alpn".into(), Value::Sequence(vec![Value::String(alpn)]));
  }
}

fn migrate_legacy_websocket_options(config: &mut Mapping) {
  for proxy in proxy_mappings(config) {
    let is_websocket = proxy
      .get("network")
      .and_then(Value::as_str)
      .is_some_and(|network| network.eq_ignore_ascii_case("ws"));
    if !is_websocket || (!proxy.contains_key("ws-path") && !proxy.contains_key("ws-headers")) {
      continue;
    }

    let mut options = proxy
      .get("ws-opts")
      .and_then(Value::as_mapping)
      .cloned()
      .unwrap_or_default();
    if let Some(path) = proxy.remove("ws-path") {
      options.insert("path".into(), path);
    }
    if let Some(headers) = proxy.remove("ws-headers") {
      options.insert("headers".into(), headers);
    }
    proxy.insert("ws-opts".into(), Value::Mapping(options));
  }
}

fn enable_proxy_udp(config: &mut Mapping) {
  for proxy in proxy_mappings(config) {
    let is_named_proxy = proxy
      .get("name")
      .and_then(Value::as_str)
      .is_some_and(|name| !name.trim().is_empty());
    if is_named_proxy {
      proxy.insert("udp".into(), Value::Bool(true));
    }
  }
}

fn proxy_mappings(config: &mut Mapping) -> impl Iterator<Item = &mut Mapping> {
  config
    .get_mut("proxies")
    .and_then(Value::as_sequence_mut)
    .into_iter()
    .flatten()
    .filter_map(Value::as_mapping_mut)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use serde_yaml_ng::{Mapping, Value};

  use super::NativeTransform;

  fn mapping(source: &str) -> Mapping {
    serde_yaml_ng::from_str(source).expect("test YAML should parse")
  }

  #[test]
  fn compatibility_defaults_match_known_cvr_fixes() {
    let mut config = mapping(
      r"
mode: script
proxies:
  - {name: hy, type: hysteria, alpn: h3}
  - {name: ws, type: vmess, network: ws, ws-path: /legacy, ws-headers: {Host: example.test}}
  - {name: modern, type: vmess, network: ws, ws-opts: {path: /modern}}
",
    );

    for transform in NativeTransform::compatibility_defaults() {
      transform.apply(&mut config);
    }

    assert_eq!(config.get("mode").and_then(Value::as_str), Some("rule"));
    let proxies = config
      .get("proxies")
      .and_then(Value::as_sequence)
      .expect("proxies should remain a sequence");
    let hysteria = proxies[0]
      .as_mapping()
      .expect("proxy should remain a mapping");
    assert_eq!(
      hysteria
        .get("alpn")
        .and_then(Value::as_sequence)
        .and_then(|alpn| alpn.first())
        .and_then(Value::as_str),
      Some("h3")
    );
    let websocket = proxies[1]
      .as_mapping()
      .expect("proxy should remain a mapping");
    let options = websocket
      .get("ws-opts")
      .and_then(Value::as_mapping)
      .expect("legacy fields should become ws-opts");
    assert_eq!(options.get("path").and_then(Value::as_str), Some("/legacy"));
    assert_eq!(
      options
        .get("headers")
        .and_then(Value::as_mapping)
        .and_then(|headers| headers.get("Host"))
        .and_then(Value::as_str),
      Some("example.test")
    );
    assert!(!websocket.contains_key("ws-path"));
    assert!(!websocket.contains_key("ws-headers"));
    assert_eq!(
      proxies[2]
        .as_mapping()
        .and_then(|proxy| proxy.get("ws-opts"))
        .and_then(Value::as_mapping)
        .and_then(|options| options.get("path"))
        .and_then(Value::as_str),
      Some("/modern")
    );
  }

  #[test]
  fn udp_transform_is_available_but_not_a_compatibility_default() {
    assert!(!NativeTransform::compatibility_defaults().contains(&NativeTransform::EnableProxyUdp));
    let mut config = mapping("proxies: [{name: node, type: ss}, {type: ss}]");

    NativeTransform::EnableProxyUdp.apply(&mut config);

    let proxies = config
      .get("proxies")
      .and_then(Value::as_sequence)
      .expect("proxies should remain a sequence");
    assert_eq!(
      proxies[0]
        .as_mapping()
        .and_then(|proxy| proxy.get("udp"))
        .and_then(Value::as_bool),
      Some(true)
    );
    assert!(
      proxies[1]
        .as_mapping()
        .is_some_and(|proxy| !proxy.contains_key("udp"))
    );
  }
}
