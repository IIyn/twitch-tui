//! Logging in to a Twitch account with the device flow (the user types a
//! short code on twitch.tv/activate), and keeping the session on disk.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use twitch_core::helix::{self, CLIENT_ID, Session, TOKEN_URL, oauth_post};
use twitch_core::http::{self, url_encode};
use twitch_core::json;

const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const REVOKE_URL: &str = "https://id.twitch.tv/oauth2/revoke";
/// Followed channels, then reading and writing in chat.
const SCOPES: &str = "user:read:follows chat:read chat:edit";

/// What the user needs to approve the login.
pub struct DeviceCode {
    pub user_code: String,
    /// Already carries the code, so opening it is enough.
    pub verification_uri: String,
}

/// Runs the device flow: `show` tells the user where to go, then this waits
/// until they approve (or refuse, or let the code expire).
pub fn login(show: impl FnOnce(&DeviceCode)) -> Result<Session, String> {
    let scopes = url_encode(SCOPES);
    let doc = oauth_post(DEVICE_URL, &format!("client_id={CLIENT_ID}&scopes={scopes}"))?;
    let device_code = doc.get("device_code").as_str().ok_or("no device code in Twitch's answer")?;
    let code = DeviceCode {
        user_code: doc.get("user_code").str_or("").to_string(),
        verification_uri: doc.get("verification_uri").str_or("https://www.twitch.tv/activate").to_string(),
    };
    let mut interval = doc.get("interval").as_u64().unwrap_or(5).max(1);
    let deadline = Instant::now() + Duration::from_secs(doc.get("expires_in").as_u64().unwrap_or(1800));
    show(&code);

    let form = format!(
        "client_id={CLIENT_ID}&scopes={scopes}&device_code={}&grant_type={}",
        url_encode(device_code),
        url_encode("urn:ietf:params:oauth:grant-type:device_code"),
    );
    let tokens = loop {
        std::thread::sleep(Duration::from_secs(interval));
        if Instant::now() >= deadline {
            return Err("the code expired before it was approved".into());
        }
        match oauth_post(TOKEN_URL, &form) {
            Ok(doc) => break doc,
            Err(e) if e == "authorization_pending" => {}
            Err(e) if e == "slow_down" => interval += 5,
            Err(e) if e == "invalid device code" => return Err("the login was refused or expired".into()),
            Err(e) => return Err(e),
        }
    };

    let token = tokens.get("access_token").str_or("");
    let (user_id, login) = validate(token)?;
    helix::session_from(&tokens, &user_id, &login)
}

/// The account an access token belongs to: `(user id, login)`.
pub fn validate(token: &str) -> Result<(String, String), String> {
    let auth = format!("OAuth {token}");
    let resp = http::request(VALIDATE_URL, &[("Authorization", auth.as_str())], None)?;
    let doc = json::parse(&resp.body).map_err(|e| format!("bad response (HTTP {}): {e}", resp.status))?;
    match (doc.get("user_id").as_str(), doc.get("login").as_str()) {
        (Some(id), Some(login)) if resp.status == 200 => Ok((id.to_string(), login.to_string())),
        _ => Err(doc.get("message").str_or("token rejected by Twitch").to_string()),
    }
}

/// Revokes the session's tokens on Twitch's side and forgets it here.
pub fn logout(session: &Session) -> Result<(), String> {
    let form = format!("client_id={CLIENT_ID}&token={}", url_encode(&session.access_token));
    let revoked = oauth_post(REVOKE_URL, &form).map(|_| ());
    delete()?;
    revoked
}

pub fn path() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")
    } else {
        var("XDG_STATE_HOME").or_else(|| var("HOME").map(|h| h.join(".local/state")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("twitch-tui").join("session")
}

/// The saved session, if any. A damaged file counts as none.
pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()).ok()?;
    let field = |key: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| v.trim().to_string())
    };
    let session = Session {
        access_token: field("access_token")?,
        refresh_token: field("refresh_token")?,
        expires_at: field("expires_at")?.parse().ok()?,
        user_id: field("user_id")?,
        login: field("login")?,
    };
    (!session.access_token.is_empty()).then_some(session)
}

/// Writes the session, readable by the user only. The file is replaced in
/// one step so a crash never leaves half of it.
pub fn save(session: &Session) -> Result<(), String> {
    use std::io::Write;

    let path = path();
    let dir = path.parent().ok_or("no directory for the session file")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let text = format!(
        "access_token = {}\nrefresh_token = {}\nexpires_at = {}\nuser_id = {}\nlogin = {}\n",
        session.access_token, session.refresh_token, session.expires_at, session.user_id, session.login
    );
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = twitch_core::create_private(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    file.write_all(text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub fn delete() -> Result<(), String> {
    match std::fs::remove_file(path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("cannot remove the session: {e}")),
        _ => Ok(()),
    }
}
