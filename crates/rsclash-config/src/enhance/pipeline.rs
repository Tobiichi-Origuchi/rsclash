use std::{collections::BTreeSet, net::IpAddr};

use serde_yaml_ng::{Mapping, Value};

use crate::{MihomoConfig, Result, RuntimeConfig, ScriptLog};

use super::{
  SequenceEdit, apply_deep_merge, apply_sequence_edit, cleanup_proxy_groups, lowercase_mapping,
  sort_top_level,
};

const CONTROL_PLANE_FIELDS: &[&str] = &[
  "external-controller",
  "external-controller-unix",
  "external-controller-pipe",
  "external-controller-cors",
  "secret",
  "mixed-port",
  "socks-port",
  "port",
  "redir-port",
  "tproxy-port",
  "tun",
  "mode",
  "allow-lan",
  "log-level",
  "ipv6",
  "unified-delay",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetPlatform {
  Linux,
  MacOs,
  Windows,
}

impl TargetPlatform {
  #[must_use]
  pub const fn current() -> Self {
    #[cfg(target_os = "linux")]
    {
      Self::Linux
    }
    #[cfg(target_os = "macos")]
    {
      Self::MacOs
    }
    #[cfg(target_os = "windows")]
    {
      Self::Windows
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
      Self::Linux
    }
  }
}

impl Default for TargetPlatform {
  fn default() -> Self {
    Self::current()
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListenerPolicy {
  pub socks: bool,
  pub http: bool,
  pub redir: bool,
  pub tproxy: bool,
  pub external_controller: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScriptLayer {
  pub id: String,
  pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScriptOutput {
  pub config: Mapping,
  pub logs: Vec<ScriptLog>,
}

pub trait ScriptExecutor: Send + Sync {
  fn execute(
    &self,
    script: &ScriptLayer,
    config: &Mapping,
    profile_name: &str,
  ) -> Result<ScriptOutput>;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SequenceLayers {
  pub rules: SequenceEdit,
  pub proxies: SequenceEdit,
  pub groups: SequenceEdit,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ManualLayer {
  pub merge: Option<Mapping>,
  pub script: Option<ScriptLayer>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApplicationLayer {
  pub defaults: Mapping,
  pub listeners: ListenerPolicy,
  pub platform: TargetPlatform,
  pub enable_tun: bool,
  pub dns_settings: Option<Mapping>,
  pub builtin_scripts: Vec<ScriptLayer>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EnhancementInput {
  pub current: MihomoConfig,
  pub sequence: SequenceLayers,
  pub application: ApplicationLayer,
  pub global: ManualLayer,
  pub profile: ManualLayer,
  pub profile_name: String,
}

pub struct EnhancementPipeline<'a> {
  scripts: &'a dyn ScriptExecutor,
}

impl<'a> EnhancementPipeline<'a> {
  #[must_use]
  pub const fn new(scripts: &'a dyn ScriptExecutor) -> Self {
    Self { scripts }
  }

  #[must_use]
  pub fn enhance(&self, input: EnhancementInput) -> RuntimeConfig {
    let EnhancementInput {
      current,
      sequence,
      application,
      global,
      profile,
      profile_name,
    } = input;
    let mut config = current.into_mapping();

    apply_sequence_edit(&mut config, "rules", sequence.rules);
    apply_sequence_edit(&mut config, "proxies", sequence.proxies);
    apply_sequence_edit(&mut config, "proxy-groups", sequence.groups);
    let mut source_keys = top_level_keys(&config);

    apply_application_defaults(&mut config, &application);
    let mut script_logs = std::collections::BTreeMap::new();
    for script in &application.builtin_scripts {
      self.run_script(
        &mut config,
        script,
        &profile_name,
        &mut source_keys,
        &mut script_logs,
        false,
      );
    }
    apply_tun(&mut config, application.enable_tun);
    if let Some(settings) = &application.dns_settings {
      apply_dns_settings(&mut config, settings);
    }

    let control_plane = snapshot_control_plane(&config);
    let dns_ipv6 = application
      .dns_settings
      .as_ref()
      .and_then(|_| snapshot_dns_ipv6(&config));

    self.apply_manual_layer(
      &mut config,
      &global,
      &profile_name,
      &mut source_keys,
      &mut script_logs,
    );
    self.apply_manual_layer(
      &mut config,
      &profile,
      &profile_name,
      &mut source_keys,
      &mut script_logs,
    );

    restore_control_plane(&mut config, &control_plane);
    restore_dns_ipv6(&mut config, dns_ipv6);
    ensure_lan_bind_address(&mut config);
    cleanup_proxy_groups(&mut config);
    sort_top_level(&mut config);

    RuntimeConfig {
      config: Some(MihomoConfig::new(config)),
      source_keys,
      script_logs,
    }
  }

  fn apply_manual_layer(
    &self,
    config: &mut Mapping,
    layer: &ManualLayer,
    profile_name: &str,
    source_keys: &mut BTreeSet<String>,
    logs: &mut std::collections::BTreeMap<String, Vec<ScriptLog>>,
  ) {
    if let Some(patch) = &layer.merge {
      source_keys.extend(top_level_keys(&lowercase_mapping(patch)));
      apply_deep_merge(config, patch);
    }
    if let Some(script) = &layer.script {
      self.run_script(config, script, profile_name, source_keys, logs, true);
    }
  }

  fn run_script(
    &self,
    config: &mut Mapping,
    script: &ScriptLayer,
    profile_name: &str,
    source_keys: &mut BTreeSet<String>,
    logs: &mut std::collections::BTreeMap<String, Vec<ScriptLog>>,
    track_changes: bool,
  ) {
    match self.scripts.execute(script, config, profile_name) {
      Ok(output) => {
        if track_changes {
          source_keys.extend(changed_keys(config, &output.config));
        }
        *config = output.config;
        logs.insert(script.id.clone(), output.logs);
      },
      Err(error) => {
        logs.insert(
          script.id.clone(),
          vec![ScriptLog {
            level: "exception".to_string(),
            message: error.to_string(),
          }],
        );
      },
    }
  }
}

fn top_level_keys(config: &Mapping) -> BTreeSet<String> {
  config
    .keys()
    .filter_map(Value::as_str)
    .map(str::to_ascii_lowercase)
    .collect()
}

fn changed_keys(before: &Mapping, after: &Mapping) -> BTreeSet<String> {
  after
    .iter()
    .filter_map(|(key, value)| {
      (before.get(key) != Some(value))
        .then(|| key.as_str().map(str::to_ascii_lowercase))
        .flatten()
    })
    .collect()
}

fn apply_application_defaults(config: &mut Mapping, application: &ApplicationLayer) {
  let defaults = lowercase_mapping(&application.defaults);
  for (key, value) in defaults {
    let Some(field) = key.as_str() else {
      continue;
    };
    if field == "tun" {
      let mut tun = config
        .get("tun")
        .and_then(Value::as_mapping)
        .cloned()
        .unwrap_or_default();
      if let Value::Mapping(patch) = value {
        tun.extend(patch);
      }
      config.insert("tun".into(), Value::Mapping(tun));
      continue;
    }
    if listener_is_disabled(field, application) {
      config.remove(field);
      continue;
    }
    if field == "external-controller" && !application.listeners.external_controller {
      config.insert(key, Value::String(String::new()));
    } else {
      config.insert(key, value);
    }
  }
}

fn listener_is_disabled(field: &str, application: &ApplicationLayer) -> bool {
  match field {
    "socks-port" => !application.listeners.socks,
    "port" => !application.listeners.http,
    "redir-port" => application.platform == TargetPlatform::Windows || !application.listeners.redir,
    "tproxy-port" => application.platform != TargetPlatform::Linux || !application.listeners.tproxy,
    _ => false,
  }
}

fn apply_tun(config: &mut Mapping, enabled: bool) {
  let mut tun = config
    .get("tun")
    .and_then(Value::as_mapping)
    .cloned()
    .unwrap_or_default();
  tun.insert("enable".into(), Value::Bool(enabled));
  config.insert("tun".into(), Value::Mapping(tun));

  if !enabled {
    return;
  }
  let mut dns = config
    .get("dns")
    .and_then(Value::as_mapping)
    .cloned()
    .unwrap_or_default();
  let enhanced_mode = dns.get("enhanced-mode").and_then(Value::as_str);
  if enhanced_mode.is_none() || enhanced_mode == Some("fake-ip") {
    let ipv6 = config.get("ipv6").and_then(Value::as_bool).unwrap_or(false);
    dns.insert("enable".into(), Value::Bool(true));
    dns.insert("ipv6".into(), Value::Bool(ipv6));
    dns
      .entry("enhanced-mode".into())
      .or_insert_with(|| Value::String("fake-ip".to_string()));
    dns
      .entry("fake-ip-range".into())
      .or_insert_with(|| Value::String("198.18.0.1/16".to_string()));
    if ipv6 {
      dns
        .entry("fake-ip-range6".into())
        .or_insert_with(|| Value::String("fdfe:dcba:9876::1/64".to_string()));
    }
  }
  config.insert("dns".into(), Value::Mapping(dns));
}

fn apply_dns_settings(config: &mut Mapping, settings: &Mapping) {
  if let Some(hosts) = settings.get("hosts").filter(|value| value.is_mapping()) {
    config.insert("hosts".into(), hosts.clone());
  }

  let mut dns = settings
    .get("dns")
    .and_then(Value::as_mapping)
    .cloned()
    .unwrap_or_else(|| settings.clone());
  ensure_fake_ip_range6(&mut dns);
  config.insert("dns".into(), Value::Mapping(dns));
}

fn ensure_fake_ip_range6(dns: &mut Mapping) {
  let ipv6 = dns.get("ipv6").and_then(Value::as_bool).unwrap_or(false);
  let fake_ip = dns
    .get("enhanced-mode")
    .and_then(Value::as_str)
    .is_none_or(|mode| mode == "fake-ip");
  let range_missing = dns
    .get("fake-ip-range6")
    .and_then(Value::as_str)
    .is_none_or(|range| range.trim().is_empty());
  if ipv6 && fake_ip && range_missing {
    dns.insert(
      "fake-ip-range6".into(),
      Value::String("fdfe:dcba:9876::1/64".to_string()),
    );
  }
}

fn snapshot_control_plane(config: &Mapping) -> Mapping {
  CONTROL_PLANE_FIELDS
    .iter()
    .filter_map(|field| {
      config
        .get(*field)
        .cloned()
        .map(|value| (Value::String((*field).to_string()), value))
    })
    .collect()
}

fn restore_control_plane(config: &mut Mapping, snapshot: &Mapping) {
  for field in CONTROL_PLANE_FIELDS {
    if !snapshot.contains_key(*field) {
      config.remove(*field);
    }
  }
  config.extend(snapshot.clone());
}

fn snapshot_dns_ipv6(config: &Mapping) -> Option<Value> {
  config
    .get("dns")
    .and_then(Value::as_mapping)
    .and_then(|dns| dns.get("ipv6"))
    .cloned()
}

fn restore_dns_ipv6(config: &mut Mapping, ipv6: Option<Value>) {
  let Some(ipv6) = ipv6 else {
    return;
  };
  if let Some(dns) = config.get_mut("dns").and_then(Value::as_mapping_mut) {
    dns.insert("ipv6".into(), ipv6);
  }
}

fn ensure_lan_bind_address(config: &mut Mapping) {
  let allow_lan = config
    .get("allow-lan")
    .and_then(Value::as_bool)
    .unwrap_or(false);
  let loopback = config
    .get("bind-address")
    .and_then(Value::as_str)
    .is_some_and(is_loopback_address);
  if allow_lan && loopback {
    config.insert("bind-address".into(), Value::String("*".to_string()));
  }
}

fn is_loopback_address(address: &str) -> bool {
  let address = address.trim();
  let unbracketed = address
    .strip_prefix('[')
    .and_then(|value| value.strip_suffix(']'))
    .unwrap_or(address);
  unbracketed.eq_ignore_ascii_case("localhost")
    || unbracketed
      .parse::<IpAddr>()
      .is_ok_and(|ip| ip.is_loopback())
    || is_ipv4_shorthand_loopback(unbracketed)
}

fn is_ipv4_shorthand_loopback(address: &str) -> bool {
  let Ok(parts) = address
    .split('.')
    .map(str::parse::<u32>)
    .collect::<std::result::Result<Vec<_>, _>>()
  else {
    return false;
  };
  match parts.as_slice() {
    [first, rest] => *first == 127 && *rest <= 0x00ff_ffff,
    [first, second, rest] => *first == 127 && *second <= 0xff && *rest <= 0xffff,
    [first, second, third, fourth] => {
      *first == 127 && *second <= 0xff && *third <= 0xff && *fourth <= 0xff
    },
    _ => false,
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use serde_yaml_ng::{Mapping, Value};

  use crate::{Error, MihomoConfig, Result, ScriptLog};

  use super::{
    ApplicationLayer, EnhancementInput, EnhancementPipeline, ListenerPolicy, ManualLayer,
    ScriptExecutor, ScriptLayer, ScriptOutput, SequenceEdit, SequenceLayers, TargetPlatform,
    apply_tun,
  };

  struct TestScripts;

  impl ScriptExecutor for TestScripts {
    fn execute(
      &self,
      script: &ScriptLayer,
      config: &Mapping,
      _profile_name: &str,
    ) -> Result<ScriptOutput> {
      if script.source == "fail" {
        return Err(Error::ScriptExecution("expected failure".to_string()));
      }
      let mut output = config.clone();
      output.insert("winner".into(), Value::String(script.source.clone()));
      output.insert("mixed-port".into(), Value::Number(9999.into()));
      output
        .entry("dns".into())
        .or_insert_with(|| Value::Mapping(Mapping::new()));
      if let Some(dns) = output.get_mut("dns").and_then(Value::as_mapping_mut) {
        dns.insert("ipv6".into(), Value::Bool(false));
      }
      Ok(ScriptOutput {
        config: output,
        logs: vec![ScriptLog {
          level: "info".to_string(),
          message: script.id.clone(),
        }],
      })
    }
  }

  fn mapping(source: &str) -> Mapping {
    serde_yaml_ng::from_str(source).expect("test YAML should parse")
  }

  #[test]
  fn pipeline_enforces_order_and_restores_application_control_plane() {
    let input = EnhancementInput {
      current: MihomoConfig::new(mapping(
        r#"
mode: direct
mixed-port: 7000
allow-lan: false
bind-address: "127.1"
rules: [old, delete]
proxies: [{name: node, type: ss}]
proxy-groups: [{name: select, type: select, proxies: [node, missing]}]
"#,
      )),
      sequence: SequenceLayers {
        rules: SequenceEdit {
          prepend: serde_yaml_ng::from_str("[first]").expect("sequence should parse"),
          append: serde_yaml_ng::from_str("[last]").expect("sequence should parse"),
          delete: vec!["delete".to_string()],
        },
        ..SequenceLayers::default()
      },
      application: ApplicationLayer {
        defaults: mapping(
          r#"
mode: rule
mixed-port: 7890
allow-lan: true
external-controller: 127.0.0.1:9090
"#,
        ),
        listeners: ListenerPolicy {
          external_controller: true,
          ..ListenerPolicy::default()
        },
        platform: TargetPlatform::Linux,
        enable_tun: true,
        dns_settings: Some(mapping(
          "dns: {enable: true, ipv6: true, enhanced-mode: fake-ip}",
        )),
        builtin_scripts: Vec::new(),
      },
      global: ManualLayer {
        merge: Some(mapping("winner: global-merge\nmixed-port: 9000")),
        script: Some(ScriptLayer {
          id: "global-script".to_string(),
          source: "global-script".to_string(),
        }),
      },
      profile: ManualLayer {
        merge: Some(mapping("winner: profile-merge\nmixed-port: 9001")),
        script: Some(ScriptLayer {
          id: "profile-script".to_string(),
          source: "profile-script".to_string(),
        }),
      },
      profile_name: "Test".to_string(),
    };

    let runtime = EnhancementPipeline::new(&TestScripts).enhance(input);
    let config = runtime
      .config
      .as_ref()
      .expect("runtime should contain config")
      .mapping();

    assert_eq!(config.get("mode").and_then(Value::as_str), Some("rule"));
    assert_eq!(config.get("mixed-port").and_then(Value::as_u64), Some(7890));
    assert_eq!(
      config.get("winner").and_then(Value::as_str),
      Some("profile-script")
    );
    assert_eq!(
      config.get("bind-address").and_then(Value::as_str),
      Some("*")
    );
    assert_eq!(
      config
        .get("tun")
        .and_then(Value::as_mapping)
        .and_then(|tun| tun.get("enable"))
        .and_then(Value::as_bool),
      Some(true)
    );
    assert_eq!(
      config
        .get("dns")
        .and_then(Value::as_mapping)
        .and_then(|dns| dns.get("ipv6"))
        .and_then(Value::as_bool),
      Some(true)
    );
    let rules = config
      .get("rules")
      .and_then(Value::as_sequence)
      .expect("rules should remain a sequence");
    assert_eq!(
      rules.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
      vec!["first", "old", "last"]
    );
    let group_proxies = config
      .get("proxy-groups")
      .and_then(Value::as_sequence)
      .and_then(|groups| groups.first())
      .and_then(Value::as_mapping)
      .and_then(|group| group.get("proxies"))
      .and_then(Value::as_sequence)
      .expect("group proxies should remain a sequence");
    assert_eq!(
      group_proxies
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>(),
      vec!["node"]
    );
    assert!(runtime.source_keys.contains("winner"));
    assert!(runtime.script_logs.contains_key("global-script"));
    assert!(runtime.script_logs.contains_key("profile-script"));
  }

  #[test]
  fn disabled_and_unsupported_listeners_are_removed() {
    let input = EnhancementInput {
      current: MihomoConfig::new(mapping(
        "socks-port: 1\nport: 2\nredir-port: 3\ntproxy-port: 4",
      )),
      application: ApplicationLayer {
        defaults: mapping("socks-port: 7891\nport: 7892\nredir-port: 7893\ntproxy-port: 7894"),
        listeners: ListenerPolicy::default(),
        platform: TargetPlatform::MacOs,
        ..ApplicationLayer::default()
      },
      ..EnhancementInput::default()
    };

    let runtime = EnhancementPipeline::new(&TestScripts).enhance(input);
    let config = runtime.config.expect("runtime should contain config");
    for field in ["socks-port", "port", "redir-port", "tproxy-port"] {
      assert!(!config.mapping().contains_key(field));
    }
  }

  #[test]
  fn failed_script_keeps_previous_config_and_records_exception() {
    let input = EnhancementInput {
      current: MihomoConfig::new(mapping("mode: rule")),
      global: ManualLayer {
        script: Some(ScriptLayer {
          id: "failure".to_string(),
          source: "fail".to_string(),
        }),
        ..ManualLayer::default()
      },
      ..EnhancementInput::default()
    };

    let runtime = EnhancementPipeline::new(&TestScripts).enhance(input);
    assert_eq!(
      runtime
        .config
        .as_ref()
        .and_then(|config| config.get("mode"))
        .and_then(Value::as_str),
      Some("rule")
    );
    assert_eq!(runtime.script_logs["failure"][0].level, "exception");
  }

  #[test]
  fn tun_preserves_non_fake_ip_dns_mode() {
    let mut config = mapping("ipv6: true\ndns: {enhanced-mode: redir-host, enable: false}");
    apply_tun(&mut config, true);

    let dns = config
      .get("dns")
      .and_then(Value::as_mapping)
      .expect("DNS should be a mapping");
    assert_eq!(dns.get("enable").and_then(Value::as_bool), Some(false));
    assert!(!dns.contains_key("fake-ip-range"));
  }
}
