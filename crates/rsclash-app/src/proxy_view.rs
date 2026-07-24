use std::collections::{BTreeMap, BTreeSet};

use rsclash_domain::{
  ProxyCapabilities, ProxyGroupView, ProxyMemberSnapshot, ProxyMemberUnresolvedReason,
  ProxyNodeSnapshot, ProxyNodeSource, ProxyProviderView, ProxyViewOrderSource,
  ProxyViewProviderState, ProxyViewV1,
};
use rsclash_mihomo::models::{Proxies, Proxy, ProxyProviders};

pub(crate) struct ProxyViewInput {
  pub runtime_group_order: Vec<String>,
  pub proxies: Proxies,
  pub providers: Option<ProxyProviders>,
}

pub(crate) struct ProxyViewBuilder;

struct MemberResolver<'a> {
  group_names: BTreeSet<String>,
  core_node_ids: &'a BTreeMap<String, String>,
  provider_candidates: &'a BTreeMap<String, Vec<String>>,
  provider_available: bool,
}

impl MemberResolver<'_> {
  fn resolve(&self, name: String) -> ProxyMemberSnapshot {
    if self.group_names.contains(&name) {
      ProxyMemberSnapshot::Group { name }
    } else if let Some(record_id) = self.core_node_ids.get(&name) {
      ProxyMemberSnapshot::Node {
        name,
        record_id: record_id.clone(),
      }
    } else if !self.provider_available {
      ProxyMemberSnapshot::Unresolved {
        name,
        reason: ProxyMemberUnresolvedReason::ProviderUnavailable,
      }
    } else {
      match self.provider_candidates.get(&name).map(Vec::as_slice) {
        Some([record_id]) => ProxyMemberSnapshot::Node {
          name,
          record_id: record_id.clone(),
        },
        None => ProxyMemberSnapshot::Unresolved {
          name,
          reason: ProxyMemberUnresolvedReason::Missing,
        },
        Some(_) => ProxyMemberSnapshot::Unresolved {
          name,
          reason: ProxyMemberUnresolvedReason::Ambiguous,
        },
      }
    }
  }
}

impl ProxyViewBuilder {
  pub(crate) fn build(input: ProxyViewInput) -> ProxyViewV1 {
    let ProxyViewInput {
      runtime_group_order,
      proxies,
      providers,
    } = input;
    let provider_state = if providers.is_some() {
      ProxyViewProviderState::Ready
    } else {
      ProxyViewProviderState::Unavailable
    };
    let (mut core_groups, core_nodes) = partition_core(proxies);
    let (mut records, core_node_ids) = build_core_records(core_nodes);
    let (providers, provider_candidates) = build_provider_records(providers, &mut records);
    let resolver = MemberResolver {
      group_names: core_groups.keys().cloned().collect(),
      core_node_ids: &core_node_ids,
      provider_candidates: &provider_candidates,
      provider_available: provider_state == ProxyViewProviderState::Ready,
    };

    let global = core_groups
      .remove("GLOBAL")
      .map(|proxy| build_group("GLOBAL".to_string(), proxy, &resolver));
    let (groups, order_source) = build_ordered_groups(core_groups, &runtime_group_order, &resolver);
    let direct = core_node_ids.get("DIRECT").cloned();
    let standalone = build_standalone(&core_node_ids);

    ProxyViewV1 {
      schema_version: 1,
      order_source,
      provider_state,
      global,
      direct,
      groups,
      records,
      standalone,
      providers,
    }
  }
}

fn partition_core(proxies: Proxies) -> (BTreeMap<String, Proxy>, BTreeMap<String, Proxy>) {
  let mut groups = BTreeMap::new();
  let mut nodes = BTreeMap::new();
  for (name, proxy) in proxies.proxies {
    if proxy.all.is_some() {
      groups.insert(name, proxy);
    } else {
      nodes.insert(name, proxy);
    }
  }
  (groups, nodes)
}

fn build_core_records(
  core_nodes: BTreeMap<String, Proxy>,
) -> (
  BTreeMap<String, ProxyNodeSnapshot>,
  BTreeMap<String, String>,
) {
  let mut records = BTreeMap::new();
  let mut ids = BTreeMap::new();
  for (index, (name, proxy)) in core_nodes.into_iter().enumerate() {
    let record_id = format!("c:{index}");
    ids.insert(name.clone(), record_id.clone());
    records.insert(
      record_id.clone(),
      build_node(
        record_id,
        name.clone(),
        proxy,
        ProxyNodeSource::Core { proxy_name: name },
      ),
    );
  }
  (records, ids)
}

fn build_provider_records(
  providers: Option<ProxyProviders>,
  records: &mut BTreeMap<String, ProxyNodeSnapshot>,
) -> (Vec<ProxyProviderView>, BTreeMap<String, Vec<String>>) {
  let providers = providers
    .map(|providers| providers.providers.into_iter().collect::<BTreeMap<_, _>>())
    .unwrap_or_default();
  let mut views = Vec::new();
  let mut candidates = BTreeMap::<String, Vec<String>>::new();

  for (provider_name, provider) in providers {
    let provider_index = views.len();
    let mut proxy_record_ids = Vec::new();
    for (member_index, proxy) in provider.proxies.into_iter().enumerate() {
      let record_id = format!("p:{provider_index}:{member_index}");
      let proxy_name = proxy.name.clone();
      candidates
        .entry(proxy_name.clone())
        .or_default()
        .push(record_id.clone());
      records.insert(
        record_id.clone(),
        build_node(
          record_id.clone(),
          proxy_name.clone(),
          proxy,
          ProxyNodeSource::Provider {
            provider_name: provider_name.clone(),
            proxy_name,
          },
        ),
      );
      proxy_record_ids.push(record_id);
    }
    views.push(ProxyProviderView {
      name: provider_name,
      kind: provider.kind,
      vehicle_type: provider.vehicle_type,
      updated_at: provider.updated_at,
      test_url: (!provider.test_url.is_empty()).then_some(provider.test_url),
      proxy_record_ids,
    });
  }
  (views, candidates)
}

fn build_group(name: String, proxy: Proxy, resolver: &MemberResolver<'_>) -> ProxyGroupView {
  let delay_ms = latest_delay(&proxy);
  let capabilities = capabilities(&proxy);
  ProxyGroupView {
    name,
    kind: proxy.kind,
    alive: proxy.alive,
    selected: proxy.now,
    fixed: proxy.fixed,
    hidden: proxy.hidden.unwrap_or(false),
    icon: proxy.icon,
    test_url: proxy.test_url,
    delay_ms,
    capabilities,
    members: proxy
      .all
      .unwrap_or_default()
      .into_iter()
      .map(|name| resolver.resolve(name))
      .collect(),
  }
}

fn build_node(
  record_id: String,
  name: String,
  proxy: Proxy,
  source: ProxyNodeSource,
) -> ProxyNodeSnapshot {
  ProxyNodeSnapshot {
    record_id,
    name,
    kind: proxy.kind.clone(),
    alive: proxy.alive,
    delay_ms: latest_delay(&proxy),
    hidden: proxy.hidden.unwrap_or(false),
    icon: proxy.icon.clone(),
    test_url: proxy.test_url.clone(),
    interface: (!proxy.interface.is_empty()).then(|| proxy.interface.clone()),
    dialer_proxy: (!proxy.dialer_proxy.is_empty()).then(|| proxy.dialer_proxy.clone()),
    capabilities: capabilities(&proxy),
    source: Some(source),
  }
}

const fn capabilities(proxy: &Proxy) -> ProxyCapabilities {
  ProxyCapabilities {
    udp: proxy.udp,
    uot: proxy.uot,
    xudp: proxy.xudp,
    tfo: proxy.tfo,
    mptcp: proxy.mptcp,
    smux: proxy.smux,
  }
}

fn latest_delay(proxy: &Proxy) -> Option<u32> {
  proxy
    .history
    .last()
    .map(|history| history.delay)
    .filter(|delay| *delay > 0)
}

fn build_ordered_groups(
  mut core_groups: BTreeMap<String, Proxy>,
  runtime_group_order: &[String],
  resolver: &MemberResolver<'_>,
) -> (Vec<ProxyGroupView>, ProxyViewOrderSource) {
  let mut selected = BTreeSet::new();
  let mut names = Vec::new();
  for name in runtime_group_order {
    if core_groups.contains_key(name) && selected.insert(name.clone()) {
      names.push(name.clone());
    }
  }
  let order_source = if names.is_empty() {
    ProxyViewOrderSource::Fallback
  } else {
    ProxyViewOrderSource::Runtime
  };
  names.extend(
    core_groups
      .keys()
      .filter(|name| !selected.contains(*name))
      .cloned(),
  );
  let groups = names
    .into_iter()
    .filter_map(|name| {
      core_groups
        .remove(&name)
        .map(|proxy| build_group(name, proxy, resolver))
    })
    .collect();
  (groups, order_source)
}

fn build_standalone(core_node_ids: &BTreeMap<String, String>) -> Vec<String> {
  let mut standalone = ["DIRECT", "REJECT"]
    .into_iter()
    .filter_map(|name| core_node_ids.get(name).cloned())
    .collect::<Vec<_>>();
  standalone.extend(
    core_node_ids
      .iter()
      .filter(|(name, _)| name.as_str() != "DIRECT" && name.as_str() != "REJECT")
      .map(|(_, record_id)| record_id.clone()),
  );
  standalone
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::collections::HashMap;

  use rsclash_domain::{ProxyMemberSnapshot, ProxyMemberUnresolvedReason, ProxyViewOrderSource};
  use rsclash_mihomo::models::{Proxies, Proxy, ProxyProvider, ProxyProviders};

  use super::{ProxyViewBuilder, ProxyViewInput};

  fn node(name: &str) -> Proxy {
    Proxy {
      name: name.to_string(),
      kind: "Shadowsocks".to_string(),
      alive: true,
      udp: true,
      ..Proxy::default()
    }
  }

  fn group(members: &[&str]) -> Proxy {
    Proxy {
      kind: "Selector".to_string(),
      alive: true,
      all: Some(members.iter().map(|name| (*name).to_string()).collect()),
      ..Proxy::default()
    }
  }

  #[test]
  fn records_and_runtime_order_are_deterministic() {
    let entries = [
      ("Zulu", group(&["node-b"])),
      ("Alpha", group(&["node-a"])),
      ("node-b", node("ignored")),
      ("node-a", node("ignored")),
      ("DIRECT", node("ignored")),
    ];
    let build = |reverse| {
      let mut proxies = HashMap::new();
      if reverse {
        proxies.extend(
          entries
            .clone()
            .into_iter()
            .rev()
            .map(|(name, proxy)| (name.to_string(), proxy)),
        );
      } else {
        proxies.extend(
          entries
            .clone()
            .into_iter()
            .map(|(name, proxy)| (name.to_string(), proxy)),
        );
      }
      ProxyViewBuilder::build(ProxyViewInput {
        runtime_group_order: vec!["Zulu".to_string(), "Alpha".to_string()],
        proxies: Proxies {
          proxies,
          ..Proxies::default()
        },
        providers: None,
      })
    };
    let first = build(false);
    let second = build(true);

    assert_eq!(first, second);
    assert_eq!(first.order_source, ProxyViewOrderSource::Runtime);
    assert_eq!(
      first
        .groups
        .iter()
        .map(|group| group.name.as_str())
        .collect::<Vec<_>>(),
      ["Zulu", "Alpha"]
    );
  }

  #[test]
  fn duplicate_provider_members_remain_ambiguous() {
    let providers = ProxyProviders {
      providers: HashMap::from([
        (
          "a".to_string(),
          ProxyProvider {
            proxies: vec![node("same")],
            ..ProxyProvider::default()
          },
        ),
        (
          "b".to_string(),
          ProxyProvider {
            proxies: vec![node("same")],
            ..ProxyProvider::default()
          },
        ),
      ]),
      ..ProxyProviders::default()
    };
    let view = ProxyViewBuilder::build(ProxyViewInput {
      runtime_group_order: Vec::new(),
      proxies: Proxies {
        proxies: HashMap::from([("Group".to_string(), group(&["same"]))]),
        ..Proxies::default()
      },
      providers: Some(providers),
    });

    assert!(matches!(
      view.groups[0].members[0],
      ProxyMemberSnapshot::Unresolved {
        reason: ProxyMemberUnresolvedReason::Ambiguous,
        ..
      }
    ));
  }
}
