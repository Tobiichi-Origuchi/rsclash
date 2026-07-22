//! Profile persistence and deterministic Mihomo configuration generation.

mod error;
mod model;

pub use error::{Error, Result};
pub use model::{
    ClashOverrides, ExtraFields, MihomoConfig, ProfileCatalog, ProfileItem, ProfileKind,
    ProfileOptions, ProfileSelection, RuntimeConfig, ScriptLog, SubscriptionInfo, VergeConfig,
    VergeTestItem, VergeTheme, from_yaml, to_yaml,
};
