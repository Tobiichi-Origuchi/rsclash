use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_yaml_ng::{Mapping, Value};

use crate::{Error, Result};

pub type ExtraFields = BTreeMap<String, Value>;

pub fn from_yaml<T>(source: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_yaml_ng::from_str(source).map_err(Error::DecodeYaml)
}

pub fn to_yaml<T>(value: &T) -> Result<String>
where
    T: Serialize,
{
    serde_yaml_ng::to_string(value).map_err(Error::EncodeYaml)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MihomoConfig(Mapping);

impl MihomoConfig {
    pub fn new(mapping: Mapping) -> Self {
        Self(mapping)
    }

    pub fn parse(source: &str) -> Result<Self> {
        from_yaml(source)
    }

    pub fn mapping(&self) -> &Mapping {
        &self.0
    }

    pub fn mapping_mut(&mut self) -> &mut Mapping {
        &mut self.0
    }

    pub fn into_mapping(self) -> Mapping {
        self.0
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.0.insert(Value::String(key.into()), value)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().filter_map(Value::as_str)
    }

    pub fn to_yaml(&self) -> Result<String> {
        to_yaml(self)
    }
}

impl From<Mapping> for MihomoConfig {
    fn from(mapping: Mapping) -> Self {
        Self::new(mapping)
    }
}

impl From<MihomoConfig> for Mapping {
    fn from(config: MihomoConfig) -> Self {
        config.into_mapping()
    }
}

pub type ClashOverrides = MihomoConfig;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct VergeConfig {
    pub app_log_level: Option<String>,
    pub app_log_max_size: Option<u64>,
    pub app_log_max_count: Option<usize>,
    pub language: Option<String>,
    pub theme_mode: Option<String>,
    pub tray_event: Option<String>,
    pub start_page: Option<String>,
    pub startup_script: Option<String>,
    pub traffic_graph: Option<bool>,
    pub enable_memory_usage: Option<bool>,
    pub enable_group_icon: Option<bool>,
    pub pause_render_traffic_stats_on_blur: Option<bool>,
    pub common_tray_icon: Option<bool>,
    pub menu_icon: Option<String>,
    pub menu_order: Option<Vec<String>>,
    pub collapse_navbar: Option<bool>,
    pub enable_tun_mode: Option<bool>,
    pub enable_auto_launch: Option<bool>,
    pub enable_silent_start: Option<bool>,
    pub enable_system_proxy: Option<bool>,
    pub enable_proxy_guard: Option<bool>,
    pub enable_dns_settings: Option<bool>,
    pub system_proxy_bypass: Option<String>,
    pub proxy_guard_duration: Option<u64>,
    pub proxy_auto_config: Option<bool>,
    pub pac_file_content: Option<String>,
    pub proxy_host: Option<String>,
    pub theme_setting: Option<VergeTheme>,
    pub clash_core: Option<String>,
    pub hotkeys: Option<Vec<String>>,
    pub enable_global_hotkey: Option<bool>,
    pub home_cards: Option<serde_json::Value>,
    pub auto_close_connection: Option<bool>,
    pub auto_check_update: Option<bool>,
    pub default_latency_test: Option<String>,
    pub default_latency_timeout: Option<i16>,
    pub enable_auto_delay_detection: Option<bool>,
    pub auto_delay_detection_interval_minutes: Option<u64>,
    pub enable_builtin_enhanced: Option<bool>,
    pub proxy_layout_column: Option<u8>,
    pub test_list: Option<Vec<VergeTestItem>>,
    pub auto_log_clean: Option<i32>,
    pub enable_auto_backup_schedule: Option<bool>,
    pub auto_backup_interval_hours: Option<u64>,
    pub auto_backup_on_change: Option<bool>,
    pub verge_redir_port: Option<u16>,
    pub verge_redir_enabled: Option<bool>,
    pub verge_tproxy_port: Option<u16>,
    pub verge_tproxy_enabled: Option<bool>,
    pub verge_mixed_port: Option<u16>,
    pub verge_socks_port: Option<u16>,
    pub verge_socks_enabled: Option<bool>,
    pub verge_port: Option<u16>,
    pub verge_http_enabled: Option<bool>,
    pub tray_proxy_groups_display_mode: Option<String>,
    pub tray_inline_outbound_modes: Option<bool>,
    pub enable_auto_light_weight_mode: Option<bool>,
    pub auto_light_weight_minutes: Option<u64>,
    pub enable_hover_jump_navigator: Option<bool>,
    pub hover_jump_navigator_delay: Option<u64>,
    pub enable_external_controller: Option<bool>,
    #[serde(flatten)]
    pub unknown: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VergeTestItem {
    pub uid: Option<String>,
    pub name: Option<String>,
    pub icon: Option<String>,
    pub url: Option<String>,
    #[serde(flatten)]
    pub unknown: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VergeTheme {
    pub primary_color: Option<String>,
    pub secondary_color: Option<String>,
    pub primary_text: Option<String>,
    pub secondary_text: Option<String>,
    pub info_color: Option<String>,
    pub error_color: Option<String>,
    pub warning_color: Option<String>,
    pub success_color: Option<String>,
    pub font_family: Option<String>,
    pub css_injection: Option<String>,
    #[serde(flatten)]
    pub unknown: ExtraFields,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileCatalog {
    pub current: Option<String>,
    pub items: Option<Vec<ProfileItem>>,
    #[serde(flatten)]
    pub unknown: ExtraFields,
}

impl ProfileCatalog {
    pub fn items(&self) -> &[ProfileItem] {
        self.items.as_deref().unwrap_or_default()
    }

    pub fn items_mut(&mut self) -> &mut Vec<ProfileItem> {
        self.items.get_or_insert_with(Vec::new)
    }

    pub fn get(&self, uid: &str) -> Option<&ProfileItem> {
        self.items()
            .iter()
            .find(|item| item.uid.as_deref() == Some(uid))
    }

    pub fn current_item(&self) -> Option<&ProfileItem> {
        self.current.as_deref().and_then(|uid| self.get(uid))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileItem {
    pub uid: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<ProfileKind>,
    pub name: Option<String>,
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Vec<ProfileSelection>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<SubscriptionInfo>,
    pub updated: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option: Option<ProfileOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(skip)]
    pub file_data: Option<String>,
    #[serde(flatten)]
    pub unknown: ExtraFields,
}

impl ProfileItem {
    pub fn require_uid(&self) -> Result<&str> {
        self.uid
            .as_deref()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| Error::InvalidConfiguration("profile UID is missing".to_string()))
    }

    pub fn require_file(&self) -> Result<&str> {
        self.file
            .as_deref()
            .filter(|file| !file.is_empty())
            .ok_or_else(|| Error::InvalidConfiguration("profile file is missing".to_string()))
    }

    pub fn is_source(&self) -> bool {
        matches!(self.kind, Some(ProfileKind::Remote | ProfileKind::Local))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileKind {
    Remote,
    Local,
    Script,
    Merge,
    Rules,
    Proxies,
    Groups,
    Unknown(String),
}

impl ProfileKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Remote => "remote",
            Self::Local => "local",
            Self::Script => "script",
            Self::Merge => "merge",
            Self::Rules => "rules",
            Self::Proxies => "proxies",
            Self::Groups => "groups",
            Self::Unknown(value) => value,
        }
    }
}

impl Serialize for ProfileKind {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProfileKind {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "remote" => Self::Remote,
            "local" => Self::Local,
            "script" => Self::Script,
            "merge" => Self::Merge,
            "rules" => Self::Rules,
            "proxies" => Self::Proxies,
            "groups" => Self::Groups,
            _ => Self::Unknown(value),
        })
    }
}

impl fmt::Display for ProfileKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileSelection {
    pub name: Option<String>,
    pub now: Option<String>,
    #[serde(flatten)]
    pub unknown: ExtraFields,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionInfo {
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    pub expire: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_interval: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub danger_accept_invalid_certs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_auto_update: Option<bool>,
    pub merge: Option<String>,
    pub script: Option<String>,
    pub rules: Option<String>,
    pub proxies: Option<String>,
    pub groups: Option<String>,
    #[serde(flatten)]
    pub unknown: ExtraFields,
}

impl ProfileOptions {
    pub fn overlay(base: Option<&Self>, patch: Option<&Self>) -> Option<Self> {
        match (base, patch) {
            (None, None) => None,
            (Some(value), None) | (None, Some(value)) => Some(value.clone()),
            (Some(base), Some(patch)) => {
                let mut result = base.clone();
                result.user_agent = patch.user_agent.clone().or(result.user_agent);
                result.with_proxy = patch.with_proxy.or(result.with_proxy);
                result.self_proxy = patch.self_proxy.or(result.self_proxy);
                result.update_interval = patch.update_interval.or(result.update_interval);
                result.timeout_seconds = patch.timeout_seconds.or(result.timeout_seconds);
                result.danger_accept_invalid_certs = patch
                    .danger_accept_invalid_certs
                    .or(result.danger_accept_invalid_certs);
                result.allow_auto_update = patch.allow_auto_update.or(result.allow_auto_update);
                result.merge = patch.merge.clone().or(result.merge);
                result.script = patch.script.clone().or(result.script);
                result.rules = patch.rules.clone().or(result.rules);
                result.proxies = patch.proxies.clone().or(result.proxies);
                result.groups = patch.groups.clone().or(result.groups);
                result.unknown.extend(patch.unknown.clone());
                Some(result)
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuntimeConfig {
    pub config: Option<MihomoConfig>,
    pub source_keys: BTreeSet<String>,
    pub script_logs: BTreeMap<String, Vec<ScriptLog>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptLog {
    pub level: String,
    pub message: String,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use serde_yaml_ng::Value;

    use super::{
        MihomoConfig, ProfileCatalog, ProfileKind, ProfileOptions, VergeConfig, from_yaml, to_yaml,
    };

    #[test]
    fn verge_round_trip_preserves_unknown_fields() {
        let source = r#"
theme_mode: system
enable_tun_mode: true
future_setting:
  nested: 42
"#;
        let mut config: VergeConfig = from_yaml(source).expect("verge config should parse");
        config.theme_mode = Some("dark".to_string());
        let encoded = to_yaml(&config).expect("verge config should serialize");
        let round_trip: VergeConfig = from_yaml(&encoded).expect("verge config should parse again");

        assert_eq!(round_trip.theme_mode.as_deref(), Some("dark"));
        assert_eq!(
            round_trip
                .unknown
                .get("future_setting")
                .and_then(|value| value.get("nested"))
                .and_then(Value::as_u64),
            Some(42)
        );
    }

    #[test]
    fn profiles_preserve_unknown_types_and_nested_options() {
        let source = r#"
current: future
future_catalog_key: keep
items:
  - uid: future
    type: future-profile
    file: future.yaml
    option:
      timeout_seconds: 15
      future_option: keep
    future_item: keep
"#;
        let catalog: ProfileCatalog = from_yaml(source).expect("profiles should parse");
        let current = catalog
            .current_item()
            .expect("current profile should resolve");

        assert_eq!(
            current.kind,
            Some(ProfileKind::Unknown("future-profile".to_string()))
        );
        assert_eq!(
            current.unknown.get("future_item").and_then(Value::as_str),
            Some("keep")
        );
        assert_eq!(
            current
                .option
                .as_ref()
                .and_then(|option| option.unknown.get("future_option"))
                .and_then(Value::as_str),
            Some("keep")
        );
        let encoded = to_yaml(&catalog).expect("profiles should serialize");
        assert!(encoded.contains("future_catalog_key"));
    }

    #[test]
    fn mihomo_mapping_round_trip_keeps_arbitrary_schema() {
        let source = r#"
mixed-port: 7890
future:
  - one
  - two
"#;
        let mut config = MihomoConfig::parse(source).expect("Mihomo config should parse");
        config.insert("mode", Value::String("rule".to_string()));
        let round_trip =
            MihomoConfig::parse(&config.to_yaml().expect("Mihomo config should serialize"))
                .expect("Mihomo config should parse again");

        assert_eq!(
            round_trip.get("mixed-port").and_then(Value::as_u64),
            Some(7890)
        );
        assert_eq!(
            round_trip
                .get("future")
                .and_then(Value::as_sequence)
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn profile_option_overlay_is_right_biased_and_preserves_extensions() {
        let mut base = ProfileOptions {
            user_agent: Some("base".to_string()),
            timeout_seconds: Some(10),
            ..ProfileOptions::default()
        };
        base.unknown
            .insert("base_key".to_string(), Value::String("base".to_string()));
        let mut patch = ProfileOptions {
            user_agent: Some("patch".to_string()),
            ..ProfileOptions::default()
        };
        patch
            .unknown
            .insert("patch_key".to_string(), Value::String("patch".to_string()));

        let merged = ProfileOptions::overlay(Some(&base), Some(&patch))
            .expect("overlay should produce options");
        assert_eq!(merged.user_agent.as_deref(), Some("patch"));
        assert_eq!(merged.timeout_seconds, Some(10));
        assert!(merged.unknown.contains_key("base_key"));
        assert!(merged.unknown.contains_key("patch_key"));
    }
}
