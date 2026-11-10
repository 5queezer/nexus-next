//! Nexus domain storage: SQLite repository, blob store, timestamps and the export importer.

pub mod auth;
pub mod blob;
pub mod db;
pub mod error;
pub mod import;
pub mod repo;
pub mod time;

pub use blob::BlobStore;
pub use db::Db;
pub use error::{Error, Result};
pub use time::Timestamp;

static ENV_LABEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Names this server as something other than production, such as `Staging`. Every page then shows
/// the label. Call it once at start: the first value stays.
pub fn set_env_label(label: String) {
    let _ = ENV_LABEL.set(label);
}

/// `None` in production. Plain text, for the pages to escape.
pub fn env_label() -> Option<&'static str> {
    ENV_LABEL.get().map(String::as_str)
}
