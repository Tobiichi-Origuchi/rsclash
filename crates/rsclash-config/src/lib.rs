//! Profile persistence and deterministic Mihomo configuration generation.

mod defaults;
mod deployment;
pub mod enhance;
mod error;
mod import;
mod model;
mod store;
mod transaction;

pub use defaults::{DEFAULT_RUNTIME_CONFIG, initialize_default_runtime};
pub use deployment::{
  ActivationMode, CommandRuntimeValidator, DeploymentOutcome, RuntimeActivator, RuntimeDeployer,
  RuntimeStore, RuntimeValidator,
};
pub use enhance::{
  ApplicationLayer, EnhancementInput, EnhancementPipeline, ListenerPolicy, ManualLayer,
  NativeTransform, SequenceEdit, SequenceLayers, TargetPlatform, apply_deep_merge,
  apply_sequence_edit, cleanup_proxy_groups, extract_control_plane, lowercase_mapping,
  sort_top_level,
};
pub use error::{Error, Result};
pub use import::{CvrImportOutcome, CvrImportReport, CvrImporter};
pub use model::{
  ClashOverrides, ExtraFields, MihomoConfig, ProfileCatalog, ProfileItem, ProfileKind,
  ProfileOptions, ProfileSelection, RuntimeConfig, SubscriptionInfo, VergeConfig, VergeTestItem,
  VergeTheme, from_yaml, to_yaml,
};
pub use store::{ConfigPaths, ProfileStore};
pub use transaction::{Draft, DraftState, ProfileTransaction};
