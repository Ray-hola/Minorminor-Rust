pub mod ai;
pub mod cleanup;
pub mod discovery;
pub mod files;
pub mod index;
pub mod mole;
pub mod scan;
pub mod server;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub fn fail(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub mod advice;
pub mod candidates;
pub mod cloud;
pub mod database;
pub mod discovery_extra;
pub mod measure;
pub mod project_info;
pub mod reports;
pub mod review;

pub mod access;
pub mod storage;

pub mod compact;
pub mod fs_compression;
pub mod snapshot;
pub mod user_state;

pub mod bulk;

pub mod overlay;

pub mod directory;
pub mod read_recovery;
