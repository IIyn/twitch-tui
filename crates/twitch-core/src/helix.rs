//! Twitch's public API (Helix) and the OAuth session it runs on.
//!
//! The session comes from this app's own registration on dev.twitch.tv, a
//! public client: it has no secret, so the client id can ship in the binary.

use std::sync::Mutex;

use crate::http::{self, url_encode};
use crate::json::{self, Json};

pub const CLIENT_ID: &str = "iznuo8cjlt3uqkkpd4pa21fwkrocny";
const HELIX_URL: &str = "https://api.twitch.tv/helix";
pub const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
/// Renew the access token this many seconds before it expires.
const EXPIRY_MARGIN: i64 = 120;

/// A logged-in account's tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub access_token: String,
    /// Single use: renewing hands out a new one and voids this one.
    pub refresh_token: String,
    /// Unix time the access token stops working.
    pub expires_at: i64,
    pub user_id: String,
    pub login: String,
}

pub struct Helix {
    session: Mutex<Session>,
    /// Told about every renewed session, so it can be saved: the refresh
    /// token it replaces no longer works.
    on_renew: Box<dyn Fn(&Session) + Send + Sync>,
}

impl Helix {
    pub fn new(session: Session, on_renew: impl Fn(&Session) + Send + Sync + 'static) -> Helix {
        Helix { session: Mutex::new(session), on_renew: Box::new(on_renew) }
    }

    pub fn user_id(&self) -> String {
        self.session.lock().unwrap().user_id.clone()
    }

    /// A working access token, renewed first when about to expire.
    pub fn access_token(&self) -> Result<String, String> {
        let mut session = self.session.lock().unwrap();
        if session.expires_at - crate::now_secs() < EXPIRY_MARGIN {
            self.renew(&mut session)?;
        }
        Ok(session.access_token.clone())
    }

    fn renew(&self, session: &mut Session) -> Result<(), String> {
        *session = refresh(session)?;
        (self.on_renew)(session);
        Ok(())
    }

    /// `GET /helix{path}`, renewing the token once if Twitch turns it down.
    pub fn get(&self, path: &str) -> Result<Json, String> {
        let token = self.access_token()?;
        let mut resp = self.send(path, &token)?;
        if resp.status == 401 {
            let token = {
                let mut session = self.session.lock().unwrap();
                // Another thread may have renewed it in the meantime.
                if session.access_token == token {
                    self.renew(&mut session)?;
                }
                session.access_token.clone()
            };
            resp = self.send(path, &token)?;
        }
        let doc = json::parse(&resp.body).map_err(|e| format!("bad response (HTTP {}): {e}", resp.status))?;
        if resp.status != 200 {
            return Err(doc.get("message").str_or("unknown Twitch API error").to_string());
        }
        Ok(doc)
    }

    fn send(&self, path: &str, token: &str) -> Result<http::Response, String> {
        let auth = format!("Bearer {token}");
        let headers = [("Client-Id", CLIENT_ID), ("Authorization", auth.as_str())];
        http::request(&format!("{HELIX_URL}{path}"), &headers, None)
    }

    /// Every item of a paginated list. `path` must already carry a query string.
    pub fn get_all(&self, path: &str) -> Result<Vec<Json>, String> {
        let mut items = Vec::new();
        let mut cursor = String::new();
        loop {
            let page = if cursor.is_empty() { path.to_string() } else { format!("{path}&after={}", url_encode(&cursor)) };
            let doc = self.get(&page)?;
            items.extend(doc.get("data").as_array().iter().cloned());
            match doc.at(&["pagination", "cursor"]).as_str() {
                Some(next) if !next.is_empty() && next != cursor => cursor = next.to_string(),
                _ => return Ok(items),
            }
        }
    }
}

/// POSTs a form to Twitch's OAuth server. A refusal comes back as its
/// `message`, e.g. `authorization_pending` while a device login is waiting.
pub fn oauth_post(url: &str, form: &str) -> Result<Json, String> {
    let headers = [("Content-Type", "application/x-www-form-urlencoded")];
    let resp = http::request(url, &headers, Some(form))?;
    let doc = json::parse(&resp.body).map_err(|e| format!("bad response (HTTP {}): {e}", resp.status))?;
    if resp.status != 200 {
        return Err(doc.get("message").str_or("unknown OAuth error").to_string());
    }
    Ok(doc)
}

/// Builds a session out of a token response.
pub fn session_from(doc: &Json, user_id: &str, login: &str) -> Result<Session, String> {
    let access_token = doc.get("access_token").as_str().ok_or("no access token in Twitch's answer")?;
    Ok(Session {
        access_token: access_token.to_string(),
        refresh_token: doc.get("refresh_token").str_or("").to_string(),
        expires_at: crate::now_secs() + doc.get("expires_in").as_u64().unwrap_or(0) as i64,
        user_id: user_id.to_string(),
        login: login.to_string(),
    })
}

/// Trades the refresh token for a new pair of tokens.
pub fn refresh(session: &Session) -> Result<Session, String> {
    let form = format!(
        "client_id={CLIENT_ID}&grant_type=refresh_token&refresh_token={}",
        url_encode(&session.refresh_token)
    );
    let doc = oauth_post(TOKEN_URL, &form)
        .map_err(|e| format!("session expired ({e}): run twitch-tui --login again"))?;
    session_from(&doc, &session.user_id, &session.login)
}
