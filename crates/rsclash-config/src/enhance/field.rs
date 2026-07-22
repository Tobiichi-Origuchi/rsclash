use std::collections::BTreeSet;

use serde_yaml_ng::{Mapping, Value};

const LEADING_FIELDS: &[&str] = &[
    "mode",
    "redir-port",
    "tproxy-port",
    "mixed-port",
    "socks-port",
    "port",
    "allow-lan",
    "log-level",
    "ipv6",
    "external-controller",
    "external-controller-unix",
    "external-controller-pipe",
    "secret",
    "unified-delay",
];

const TRAILING_FIELDS: &[&str] = &[
    "proxies",
    "proxy-providers",
    "proxy-groups",
    "rule-providers",
    "rules",
];

const BUILTIN_POLICIES: &[&str] = &["DIRECT", "REJECT", "REJECT-DROP", "PASS"];

pub fn lowercase_mapping(mapping: &Mapping) -> Mapping {
    mapping
        .iter()
        .filter_map(|(key, value)| {
            key.as_str()
                .map(|key| (Value::String(key.to_ascii_lowercase()), value.clone()))
        })
        .collect()
}

pub fn sort_top_level(config: &mut Mapping) {
    let mut remaining = std::mem::take(config);
    for field in LEADING_FIELDS {
        move_field(&mut remaining, config, field);
    }

    let trailing = TRAILING_FIELDS
        .iter()
        .filter_map(|field| remaining.shift_remove(*field).map(|value| (*field, value)))
        .collect::<Vec<_>>();
    config.extend(remaining);
    for (field, value) in trailing {
        config.insert(field.into(), value);
    }
}

fn move_field(source: &mut Mapping, destination: &mut Mapping, field: &str) {
    if let Some(value) = source.shift_remove(field) {
        destination.insert(field.into(), value);
    }
}

pub fn cleanup_proxy_groups(config: &mut Mapping) {
    let proxy_names = collect_sequence_names(config.get("proxies"));
    let group_names = collect_sequence_names(config.get("proxy-groups"));
    let provider_names = config
        .get("proxy-providers")
        .and_then(Value::as_mapping)
        .map(|providers| {
            providers
                .keys()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let allowed = proxy_names
        .into_iter()
        .chain(group_names)
        .chain(provider_names.iter().cloned())
        .chain(BUILTIN_POLICIES.iter().map(|name| (*name).to_string()))
        .collect::<BTreeSet<_>>();

    let Some(groups) = config
        .get_mut("proxy-groups")
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };
    for group in groups {
        let Some(group) = group.as_mapping_mut() else {
            continue;
        };
        let has_valid_provider = retain_valid_providers(group, &provider_names);
        if let Some(proxies) = group.get_mut("proxies").and_then(Value::as_sequence_mut) {
            proxies.retain(|proxy| {
                proxy
                    .as_str()
                    .is_none_or(|name| allowed.contains(name) || has_valid_provider)
            });
        }
    }
}

fn collect_sequence_names(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|item| match item {
            Value::Mapping(mapping) => mapping.get("name").and_then(Value::as_str),
            Value::String(name) => Some(name),
            _ => None,
        })
        .map(str::to_string)
        .collect()
}

fn retain_valid_providers(group: &mut Mapping, providers: &BTreeSet<String>) -> bool {
    let Some(uses) = group.get_mut("use").and_then(Value::as_sequence_mut) else {
        return false;
    };
    uses.retain(|provider| {
        provider
            .as_str()
            .is_some_and(|name| providers.contains(name))
    });
    !uses.is_empty()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use serde_yaml_ng::{Mapping, Value};

    use super::{cleanup_proxy_groups, lowercase_mapping, sort_top_level};

    fn mapping(source: &str) -> Mapping {
        serde_yaml_ng::from_str(source).expect("test YAML should parse")
    }

    #[test]
    fn lowercases_only_top_level_string_keys() {
        let mapping = lowercase_mapping(&mapping("DNS: {IPv6: true}\nMode: rule"));
        assert!(mapping.contains_key("dns"));
        assert!(mapping.contains_key("mode"));
        assert_eq!(
            mapping
                .get("dns")
                .and_then(Value::as_mapping)
                .and_then(|dns| dns.get("IPv6"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn sorts_control_fields_first_and_runtime_lists_last() {
        let mut mapping =
            mapping("rules: []\ncustom: true\nmode: rule\nproxies: []\nsecret: value\ndns: {}");
        sort_top_level(&mut mapping);

        let keys = mapping.keys().filter_map(Value::as_str).collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec!["mode", "secret", "custom", "dns", "proxies", "rules"]
        );
    }

    #[test]
    fn removes_invalid_group_references_and_keeps_valid_providers() {
        let mut mapping = mapping(
            r#"
proxies: [{name: node, type: ss}]
proxy-providers: {remote: {type: http}}
proxy-groups:
  - name: group
    type: select
    proxies: [node, missing, DIRECT]
  - name: provider-group
    type: select
    use: [remote, missing-provider]
    proxies: [dynamic-node]
"#,
        );
        cleanup_proxy_groups(&mut mapping);

        let groups = mapping
            .get("proxy-groups")
            .and_then(Value::as_sequence)
            .expect("proxy groups should be a sequence");
        let first = groups[0]
            .as_mapping()
            .and_then(|group| group.get("proxies"))
            .and_then(Value::as_sequence)
            .expect("group proxies should be a sequence");
        assert_eq!(
            first.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
            vec!["node", "DIRECT"]
        );
        let second = groups[1].as_mapping().expect("group should be a mapping");
        assert_eq!(
            second
                .get("use")
                .and_then(Value::as_sequence)
                .expect("providers should be a sequence")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            vec!["remote"]
        );
        assert_eq!(
            second
                .get("proxies")
                .and_then(Value::as_sequence)
                .expect("proxies should be a sequence")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            vec!["dynamic-node"]
        );
    }
}
