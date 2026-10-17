//! Core library for the Pawchive downloader: API client, file model, classification,
//! naming, and the download engine. It has no GUI dependencies.

pub mod api;
pub mod classify;
pub mod control;
pub mod cookies;
pub mod creator;
pub mod engine;
pub mod error;
pub mod gate;
pub mod http;
pub mod links;
pub mod model;
pub mod names;
pub mod plan;
pub mod probe;
pub mod session;
pub mod settings;

pub use error::{Error, Result};

/// Crate version, used in the user agent (`pawchive-downloader/<version>`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
