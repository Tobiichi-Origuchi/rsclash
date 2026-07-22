//! Profile persistence and deterministic Mihomo configuration generation.

pub mod enhance;
mod error;
mod model;
mod store;
mod transaction;

pub use enhance::{
    ApplicationLayer, EnhancementInput, EnhancementPipeline, ListenerPolicy, ManualLayer,
    ScriptExecutor, ScriptLayer, ScriptOutput, SequenceEdit, SequenceLayers, TargetPlatform,
    apply_deep_merge, apply_sequence_edit, cleanup_proxy_groups, lowercase_mapping, sort_top_level,
};
pub use error::{Error, Result};
pub use model::{
    ClashOverrides, ExtraFields, MihomoConfig, ProfileCatalog, ProfileItem, ProfileKind,
    ProfileOptions, ProfileSelection, RuntimeConfig, ScriptLog, SubscriptionInfo, VergeConfig,
    VergeTestItem, VergeTheme, from_yaml, to_yaml,
};
pub use store::{ConfigPaths, ProfileStore};
pub use transaction::{Draft, DraftState, ProfileTransaction};
