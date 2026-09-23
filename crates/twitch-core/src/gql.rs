//! Twitch GraphQL API (the one used by the twitch.tv website).

use std::sync::Arc;

use crate::helix::Helix;
use crate::http;
use crate::json::{self, Json, quote};

pub const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_URL: &str = "https://gql.twitch.tv/gql";

/// Both ways into Twitch: the website's GraphQL API, anonymous or with the
/// website's session token, and the public API with the account's session.
#[derive(Clone)]
pub struct Api {
    token: Option<String>,
    device_id: String,
    helix: Option<Arc<Helix>>,
}

impl Api {
    pub fn new(token: Option<String>) -> Self {
        let token = token
            .map(|t| t.trim().trim_start_matches("oauth:").to_string())
            .filter(|t| !t.is_empty());
        Api { token, device_id: random_hex(16), helix: None }
    }

    pub fn with_helix(mut self, helix: Helix) -> Self {
        self.helix = Some(Arc::new(helix));
        self
    }

    /// The same, logged out of the account (the website token stays).
    pub fn without_helix(&self) -> Self {
        Api { helix: None, ..self.clone() }
    }

    /// The account's public API client, if logged in.
    pub fn helix(&self) -> Result<&Helix, String> {
        self.helix.as_deref().ok_or_else(|| "not logged in: run twitch-tui --login".to_string())
    }

    pub fn logged_in(&self) -> bool {
        self.helix.is_some()
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn headers<'a>(&'a self, auth: &'a str, integrity: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        let mut headers = vec![
            ("Client-Id", CLIENT_ID),
            ("Content-Type", "application/json"),
            ("X-Device-Id", self.device_id.as_str()),
        ];
        if self.token.is_some() {
            headers.push(("Authorization", auth));
        }
        if let Some(token) = integrity {
            headers.push(("Client-Integrity", token));
        }
        headers
    }

    pub fn gql_raw(&self, query: &str, variables: &str, integrity: Option<&str>) -> Result<Json, GqlError> {
        let body = format!("{{\"query\":{},\"variables\":{}}}", quote(query), variables);
        self.post(&body, integrity)
    }

    /// Calls one of the website's persisted (whitelisted) operations.
    pub fn gql_persisted(&self, (name, sha): (&str, &str), variables: &str) -> Result<Json, String> {
        let body = format!(
            "{{\"operationName\":{},\"variables\":{},\"extensions\":             {{\"persistedQuery\":{{\"version\":1,\"sha256Hash\":{}}}}}}}",
            quote(name),
            variables,
            quote(sha)
        );
        self.post(&body, None).map_err(|e| e.to_string())
    }

    fn post(&self, body: &str, integrity: Option<&str>) -> Result<Json, GqlError> {
        let auth = format!("OAuth {}", self.token.as_deref().unwrap_or(""));
        let resp = http::request(GQL_URL, &self.headers(&auth, integrity), Some(body))
            .map_err(GqlError::Other)?;
        if resp.status == 401 {
            return Err(GqlError::Other("token rejected by Twitch (401), check your config".into()));
        }
        let doc = json::parse(&resp.body)
            .map_err(|e| GqlError::Other(format!("bad response (HTTP {}): {e}", resp.status)))?;
        if let Some(err) = doc.get("errors").as_array().first() {
            let code = err.at(&["extensions", "code"]).str_or("");
            if code == "IntegrityCheckFailed" {
                return Err(GqlError::Integrity);
            }
            return Err(GqlError::Other(err.get("message").str_or("unknown GraphQL error").to_string()));
        }
        if let Some(msg) = doc.get("message").as_str() {
            return Err(GqlError::Other(msg.to_string()));
        }
        Ok(doc.get("data").clone())
    }

    pub fn gql(&self, query: &str, variables: &str) -> Result<Json, String> {
        self.gql_raw(query, variables, None).map_err(|e| e.to_string())
    }

    pub fn integrity_token(&self) -> Result<String, String> {
        let auth = format!("OAuth {}", self.token.as_deref().unwrap_or(""));
        let resp = http::request("https://gql.twitch.tv/integrity", &self.headers(&auth, None), Some(""))?;
        let doc = json::parse(&resp.body).map_err(|_| integrity_hint())?;
        doc.get("token").as_str().map(String::from).ok_or_else(integrity_hint)
    }
}

/// Twitch guards its follow mutations with a browser-only integrity check,
/// so no third-party client can follow on the user's behalf.
fn integrity_hint() -> String {
    "Twitch blocks follow/unfollow outside its own site — press o to open the channel".into()
}

pub enum GqlError {
    Integrity,
    Other(String),
}

impl std::fmt::Display for GqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GqlError::Integrity => f.write_str(&integrity_hint()),
            GqlError::Other(msg) => f.write_str(msg),
        }
    }
}

pub fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    if ok.is_err() {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(97);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
