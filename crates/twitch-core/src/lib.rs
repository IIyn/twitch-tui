//! What every Twitch feature shares: the HTTP and JSON plumbing and the
//! GraphQL client used by the twitch.tv website.

pub mod gql;
pub mod http;
pub mod json;

pub use gql::{Api, CLIENT_ID};

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
