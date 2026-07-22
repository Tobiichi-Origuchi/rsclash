use std::path::Path;

use crate::{MihomoConfig, ProfileStore, Result, RuntimeStore};

pub const DEFAULT_RUNTIME_CONFIG: &str = r"mixed-port: 17897
allow-lan: false
mode: rule
log-level: info
ipv6: false
tun:
  enable: false
  stack: mixed
  device: rsclash
  auto-route: true
  auto-redirect: false
  auto-detect-interface: true
  strict-route: false
  dns-hijack:
    - any:53
proxies: []
proxy-groups:
  - name: GLOBAL
    type: select
    proxies:
      - DIRECT
      - REJECT
rules:
  - MATCH,GLOBAL
";

pub fn initialize_default_runtime(config_root: &Path) -> Result<ProfileStore> {
  let store = ProfileStore::open(config_root)?;
  let runtime_store = RuntimeStore::open(&store.paths().runtime_config)?;
  let config = MihomoConfig::parse(DEFAULT_RUNTIME_CONFIG)?;
  runtime_store.initialize_if_missing(&config)?;
  Ok(store)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
  };

  use super::{DEFAULT_RUNTIME_CONFIG, initialize_default_runtime};
  use crate::MihomoConfig;

  #[test]
  fn default_tun_is_disabled_and_uses_the_rsclash_device() {
    let config =
      MihomoConfig::parse(DEFAULT_RUNTIME_CONFIG).expect("the default runtime config should parse");
    let tun = config
      .mapping()
      .get("tun")
      .and_then(serde_yaml_ng::Value::as_mapping)
      .expect("the default TUN mapping should exist");
    assert_eq!(
      tun.get("enable").and_then(serde_yaml_ng::Value::as_bool),
      Some(false)
    );
    assert_eq!(
      tun.get("device").and_then(serde_yaml_ng::Value::as_str),
      Some("rsclash")
    );
    assert_eq!(
      tun
        .get("auto-route")
        .and_then(serde_yaml_ng::Value::as_bool),
      Some(true)
    );
  }

  #[test]
  fn initialization_preserves_an_existing_runtime() {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
      "rsclash-default-config-test-{}-{}",
      std::process::id(),
      NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let store = initialize_default_runtime(&root).expect("default runtime should initialize");
    fs::write(&store.paths().runtime_config, b"mixed-port: 1\n")
      .expect("runtime should be replaced for the test");
    initialize_default_runtime(&root).expect("existing runtime should remain valid");
    assert_eq!(
      fs::read_to_string(&store.paths().runtime_config)
        .ok()
        .as_deref(),
      Some("mixed-port: 1\n")
    );
    let _ = fs::remove_dir_all(root);
  }
}
