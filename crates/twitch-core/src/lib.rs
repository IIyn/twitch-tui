//! What every Twitch feature shares: the HTTP and JSON plumbing, the GraphQL
//! client used by the twitch.tv website and the public API client.

pub mod gql;
pub mod helix;
pub mod http;
pub mod json;

pub use gql::{Api, CLIENT_ID};

/// Creates a new file that only its owner can read. On Windows the user's
/// profile directories already keep other accounts out.
pub fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
