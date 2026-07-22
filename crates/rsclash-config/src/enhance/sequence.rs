use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Sequence, Value};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SequenceEdit {
  pub prepend: Sequence,
  pub append: Sequence,
  pub delete: Vec<String>,
}

pub fn apply_sequence_edit(config: &mut Mapping, field: &str, edit: SequenceEdit) {
  let SequenceEdit {
    prepend,
    append,
    delete,
  } = edit;
  let added_proxy_names = if field == "proxies" {
    collect_unique_names(prepend.iter().chain(&append))
  } else {
    Vec::new()
  };
  let deleted = delete.into_iter().collect::<BTreeSet<_>>();

  let mut updated = prepend;
  if let Some(Value::Sequence(existing)) = config.remove(field) {
    updated.extend(
      existing
        .into_iter()
        .filter(|item| !is_deleted(item, &deleted)),
    );
  }
  updated.extend(append);
  config.insert(field.into(), Value::Sequence(updated));

  if field == "proxies" {
    update_proxy_group_references(config, &added_proxy_names, &deleted);
  }
}

fn collect_unique_names<'a>(items: impl Iterator<Item = &'a Value>) -> Vec<String> {
  let mut seen = BTreeSet::new();
  items
    .filter_map(item_name)
    .filter(|name| seen.insert((*name).to_string()))
    .map(str::to_string)
    .collect()
}

fn item_name(item: &Value) -> Option<&str> {
  match item {
    Value::Mapping(mapping) => mapping.get("name").and_then(Value::as_str),
    Value::String(name) => Some(name),
    _ => None,
  }
}

fn is_deleted(item: &Value, deleted: &BTreeSet<String>) -> bool {
  item_name(item).is_some_and(|name| deleted.contains(name))
}

fn update_proxy_group_references(
  config: &mut Mapping,
  added_proxy_names: &[String],
  deleted: &BTreeSet<String>,
) {
  let Some(groups) = config
    .get_mut("proxy-groups")
    .and_then(Value::as_sequence_mut)
  else {
    return;
  };

  let mut inserted = false;
  for group in groups {
    let Some(group) = group.as_mapping_mut() else {
      continue;
    };
    let is_first_selector = !inserted && is_selector(group) && !added_proxy_names.is_empty();
    match group.get_mut("proxies") {
      Some(Value::Sequence(proxies)) => {
        proxies.retain(|proxy| proxy.as_str().is_none_or(|name| !deleted.contains(name)));
        if is_first_selector {
          prepend_unique_names(proxies, added_proxy_names);
          inserted = true;
        }
      },
      None if is_first_selector => {
        group.insert(
          "proxies".into(),
          Value::Sequence(
            added_proxy_names
              .iter()
              .cloned()
              .map(Value::String)
              .collect(),
          ),
        );
        inserted = true;
      },
      Some(_) | None => {},
    }
  }
}

fn is_selector(group: &Mapping) -> bool {
  group
    .get("type")
    .and_then(Value::as_str)
    .is_some_and(|kind| {
      kind.eq_ignore_ascii_case("select") || kind.eq_ignore_ascii_case("selector")
    })
}

fn prepend_unique_names(proxies: &mut Sequence, names: &[String]) {
  let mut seen = BTreeSet::new();
  let mut merged = Sequence::new();
  for name in names {
    if seen.insert(name.clone()) {
      merged.push(Value::String(name.clone()));
    }
  }
  for proxy in std::mem::take(proxies) {
    if proxy
      .as_str()
      .is_some_and(|name| !seen.insert(name.to_string()))
    {
      continue;
    }
    merged.push(proxy);
  }
  *proxies = merged;
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use serde_yaml_ng::{Mapping, Sequence, Value};

  use super::{SequenceEdit, apply_sequence_edit};

  fn mapping(source: &str) -> Mapping {
    serde_yaml_ng::from_str(source).expect("test YAML should parse")
  }

  fn sequence(source: &str) -> Sequence {
    serde_yaml_ng::from_str(source).expect("test YAML should parse")
  }

  #[test]
  fn edits_rules_in_prepend_existing_append_order() {
    let mut config = mapping("rules: [keep, remove]");
    apply_sequence_edit(
      &mut config,
      "rules",
      SequenceEdit {
        prepend: sequence("[first]"),
        append: sequence("[last]"),
        delete: vec!["remove".to_string()],
      },
    );

    let rules = config
      .get("rules")
      .and_then(Value::as_sequence)
      .expect("rules should be a sequence");
    assert_eq!(
      rules.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
      vec!["first", "keep", "last"]
    );
  }

  #[test]
  fn proxy_edit_updates_every_deleted_reference_and_first_selector() {
    let mut config = mapping(
      r#"
proxies:
  - {name: old, type: ss}
  - {name: keep, type: ss}
proxy-groups:
  - {name: automatic, type: url-test, proxies: [old, keep]}
  - {name: primary, type: select, proxies: [old, keep, added]}
  - {name: secondary, type: selector, proxies: [old, keep]}
"#,
    );
    apply_sequence_edit(
      &mut config,
      "proxies",
      SequenceEdit {
        prepend: sequence("[{name: added, type: ss}]"),
        append: sequence("[{name: later, type: ss}, {name: added, type: ss}]"),
        delete: vec!["old".to_string()],
      },
    );

    let groups = config
      .get("proxy-groups")
      .and_then(Value::as_sequence)
      .expect("proxy groups should remain a sequence");
    let names = |index: usize| {
      groups[index]
        .as_mapping()
        .and_then(|group| group.get("proxies"))
        .and_then(Value::as_sequence)
        .expect("group proxies should be a sequence")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
    };
    assert_eq!(names(0), vec!["keep"]);
    assert_eq!(names(1), vec!["added", "later", "keep"]);
    assert_eq!(names(2), vec!["keep"]);
  }

  #[test]
  fn non_sequence_field_is_replaced_without_touching_invalid_groups() {
    let mut config = mapping("proxies: invalid\nproxy-groups: invalid");
    apply_sequence_edit(&mut config, "proxies", SequenceEdit::default());

    assert!(config.get("proxies").is_some_and(Value::is_sequence));
    assert_eq!(
      config.get("proxy-groups").and_then(Value::as_str),
      Some("invalid")
    );
  }

  #[test]
  fn added_proxy_creates_list_on_first_selector_without_one() {
    let mut config = mapping(
      "proxies: []\nproxy-groups:\n  - {name: primary, type: select}\n  - {name: other, type: select}",
    );
    apply_sequence_edit(
      &mut config,
      "proxies",
      SequenceEdit {
        prepend: sequence("[{name: added, type: ss}]"),
        ..SequenceEdit::default()
      },
    );

    let groups = config
      .get("proxy-groups")
      .and_then(Value::as_sequence)
      .expect("proxy groups should remain a sequence");
    assert_eq!(
      groups[0]
        .as_mapping()
        .and_then(|group| group.get("proxies"))
        .and_then(Value::as_sequence)
        .and_then(|proxies| proxies.first())
        .and_then(Value::as_str),
      Some("added")
    );
    assert!(
      groups[1]
        .as_mapping()
        .is_some_and(|group| !group.contains_key("proxies"))
    );
  }
}
