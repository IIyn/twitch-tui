//! Finds the Twitch session cookie in a local Firefox profile, so the app can
//! log in without asking the user to copy a token by hand.

use std::path::{Path, PathBuf};

use crate::sqlite::Database;

const COOKIE_NAME: &str = "auth-token";
const COOKIE_HOST: &str = "twitch.tv";

pub struct Found {
    pub token: String,
    /// Directory name of the profile the cookie came from.
    pub profile: String,
    pub browser: &'static str,
}

struct Candidate {
    token: String,
    /// `lastAccessed`, used to prefer the session in active use.
    seen: i64,
}

/// Profile roots of Firefox and its forks. Recent versions follow the XDG
/// layout (`~/.config/mozilla`), older ones use `~/.mozilla`.
fn roots() -> Vec<(&'static str, PathBuf)> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    vec![
        ("Firefox", config.join("mozilla/firefox")),
        ("Firefox", home.join(".mozilla/firefox")),
        ("Firefox", home.join("snap/firefox/common/.mozilla/firefox")),
        ("Firefox", home.join(".var/app/org.mozilla.firefox/.mozilla/firefox")),
        ("Firefox", home.join(".var/app/org.mozilla.firefox/config/mozilla/firefox")),
        ("LibreWolf", config.join("librewolf/librewolf")),
        ("LibreWolf", home.join(".librewolf")),
        ("LibreWolf", home.join(".var/app/io.gitlab.librewolf-community/.librewolf")),
        ("Zen", config.join("zen/zen")),
        ("Zen", home.join(".zen")),
        ("Zen", home.join(".var/app/app.zen_browser.zen/.zen")),
        ("Floorp", config.join("floorp/floorp")),
        ("Floorp", home.join(".floorp")),
        ("Floorp", home.join(".var/app/one.ablaze.floorp/.floorp")),
        ("Waterfox", home.join(".waterfox")),
    ]
}

/// Looks for the cookie in every profile of every supported browser and keeps
/// the most recently used one. `only` restricts the search to one profile.
pub fn find_token(only: Option<&Path>) -> Result<Found, String> {
    if let Some(dir) = only {
        let best = read_profile(dir)?.into_iter().max_by_key(|c| c.seen);
        return best
            .map(|c| Found { token: c.token, profile: profile_name(dir), browser: "Firefox" })
            .ok_or_else(|| format!("no Twitch cookie in {}", dir.display()));
    }

    let mut best: Option<(Candidate, String, &'static str)> = None;
    let (mut roots_found, mut searched) = (0, 0);
    for (browser, root) in roots() {
        if !root.is_dir() {
            continue;
        }
        roots_found += 1;
        for profile in profiles(&root) {
            searched += 1;
            let Ok(candidates) = read_profile(&profile) else { continue };
            for candidate in candidates {
                if best.as_ref().is_none_or(|(b, _, _)| candidate.seen > b.seen) {
                    best = Some((candidate, profile_name(&profile), browser));
                }
            }
        }
    }
    if let Some((candidate, profile, browser)) = best {
        return Ok(Found { token: candidate.token, profile, browser });
    }
    Err(match (roots_found, searched) {
        (0, _) => "no Firefox found in the usual places; use firefox_profile in the config".into(),
        (_, 0) => "Firefox found but none of its profiles has a cookie store yet".into(),
        (_, n) => format!("no Twitch cookie in {n} browser profile(s); log in on twitch.tv first"),
    })
}

fn profile_name(dir: &Path) -> String {
    dir.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string()
}

/// Profile directories of one browser, the default install's first.
fn profiles(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut preferred: Vec<PathBuf> = Vec::new();

    if let Ok(ini) = std::fs::read_to_string(root.join("profiles.ini")) {
        let mut section = String::new();
        let mut path: Option<String> = None;
        let mut relative = true;
        let mut default = false;
        // The sentinel flushes the last section.
        for line in ini.lines().chain(std::iter::once("[end]")) {
            let line = line.trim();
            if line.starts_with('[') {
                if let Some(value) = path.take() {
                    let dir = if relative { root.join(&value) } else { PathBuf::from(&value) };
                    if default { preferred.push(dir) } else { found.push(dir) }
                }
                section = line.trim_matches(['[', ']']).to_string();
                relative = true;
                default = false;
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match key {
                // [Install...] sections point at the profile actually in use.
                "Default" if section.starts_with("Install") => preferred.push(root.join(value)),
                "Default" if section.starts_with("Profile") => default = value == "1",
                "Path" if section.starts_with("Profile") => path = Some(value.to_string()),
                "IsRelative" => relative = value != "0",
                _ => {}
            }
        }
    }

    // Also take directories the ini does not mention.
    if let Ok(entries) = std::fs::read_dir(root) {
        found.extend(entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }

    preferred.extend(found);
    preferred.retain(|p| p.join("cookies.sqlite").is_file());
    let mut seen = Vec::new();
    preferred.retain(|p| {
        let new = !seen.contains(p);
        seen.push(p.clone());
        new
    });
    preferred
}

/// Unexpired Twitch `auth-token` cookies of one profile.
fn read_profile(profile: &Path) -> Result<Vec<Candidate>, String> {
    let path = profile.join("cookies.sqlite");
    // Firefox may be writing while we read: a torn read is worth retrying.
    let mut last_error = String::new();
    for _ in 0..3 {
        match collect(&path) {
            Ok(found) => return Ok(found),
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

fn collect(path: &Path) -> Result<Vec<Candidate>, String> {
    let db = Database::open(path)?;
    let table = db.table("moz_cookies")?;
    let now = twitch_core::now_secs();

    Ok(table
        .rows
        .iter()
        .filter(|row| table.get(row, "name").as_str() == Some(COOKIE_NAME))
        .filter(|row| {
            let host = table.get(row, "host").as_str().unwrap_or_default();
            let host = host.trim_start_matches('.');
            host == COOKIE_HOST || host.ends_with(&format!(".{COOKIE_HOST}"))
        })
        // Container tabs hold a different session, so skip them.
        .filter(|row| table.get(row, "originAttributes").as_str().unwrap_or_default().is_empty())
        .filter(|row| match table.get(row, "expiry").as_int() {
            // Depending on the Firefox version this is in seconds or milliseconds.
            Some(expiry) => {
                let expiry = if expiry > 100_000_000_000 { expiry / 1000 } else { expiry };
                expiry > now
            }
            None => true,
        })
        .filter_map(|row| {
            let token = table.get(row, "value").as_str()?.trim().to_string();
            (!token.is_empty()).then(|| Candidate {
                token,
                seen: table.get(row, "lastAccessed").as_int().unwrap_or(0),
            })
        })
        .collect())
}
