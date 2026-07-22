//! Tauri-free access to the Mihomo controller API.

mod api;
mod client;
mod endpoint;
mod error;
mod fake;
pub mod models;
mod stream;

pub use api::{MihomoApi, MihomoStream};
pub use client::MihomoClient;
pub use endpoint::{ControllerConfig, ControllerEndpoint, ControllerSecret};
pub use error::{Error, Result};
pub use fake::{FakeMihomoApi, FakeMihomoState, MihomoCall};
