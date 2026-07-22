//! Tauri-free access to the Mihomo controller API.

mod api;
mod client;
mod endpoint;
mod error;
pub mod models;

pub use api::MihomoApi;
pub use client::MihomoClient;
pub use endpoint::{ControllerConfig, ControllerEndpoint, ControllerSecret};
pub use error::{Error, Result};
