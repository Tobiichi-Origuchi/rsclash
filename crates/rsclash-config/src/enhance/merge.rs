use serde_yaml_ng::{Mapping, Value};

use super::lowercase_mapping;

pub fn apply_deep_merge(config: &mut Mapping, patch: &Mapping) {
  let mut config_value = Value::Mapping(std::mem::take(config));
  deep_merge(&mut config_value, Value::Mapping(lowercase_mapping(patch)));
  *config = config_value.as_mapping().cloned().unwrap_or_default();
}

fn deep_merge(target: &mut Value, patch: Value) {
  match (target, patch) {
    (Value::Mapping(target), Value::Mapping(patch)) => {
      for (key, value) in patch {
        if let Some(current) = target.get_mut(&key) {
          deep_merge(current, value);
        } else {
          target.insert(key, value);
        }
      }
    },
    (target, patch) => *target = patch,
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use serde_yaml_ng::{Mapping, Value};

  use super::apply_deep_merge;

  fn mapping(source: &str) -> Mapping {
    serde_yaml_ng::from_str(source).expect("test YAML should parse")
  }

  #[test]
  fn merge_recurses_into_mappings_and_replaces_other_values() {
    let mut config = mapping(
      r"
mixed-port: 7890
dns:
  enable: true
  nameserver: [system]
rules: [original]
",
    );
    let patch = mapping(
      r"
MIXED-PORT: 7891
DNS:
  ipv6: true
rules: [replacement]
",
    );

    apply_deep_merge(&mut config, &patch);

    assert_eq!(config.get("mixed-port").and_then(Value::as_u64), Some(7891));
    let dns = config
      .get("dns")
      .and_then(Value::as_mapping)
      .expect("DNS should remain a mapping");
    assert_eq!(dns.get("enable").and_then(Value::as_bool), Some(true));
    assert_eq!(dns.get("ipv6").and_then(Value::as_bool), Some(true));
    assert_eq!(
      config
        .get("rules")
        .and_then(Value::as_sequence)
        .and_then(|rules| rules.first())
        .and_then(Value::as_str),
      Some("replacement")
    );
  }
}
