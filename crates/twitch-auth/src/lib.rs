//! Logging in: the Twitch account session (device flow), the website token
//! lifted from Firefox's cookies, and the account the session opens.

pub mod cookies;
pub mod session;
mod sqlite;

use twitch_core::Api;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Me {
    pub id: String,
    pub login: String,
    pub display_name: String,
}

/// The account the session opens.
pub fn me(api: &Api) -> Result<Me, String> {
    let doc = api.helix()?.get("/users")?;
    let user = doc.get("data").as_array().first().ok_or("Twitch did not say who is logged in")?;
    let login = user.get("login").as_str().ok_or("Twitch did not say who is logged in")?;
    Ok(Me {
        id: user.get("id").str_or("").to_string(),
        login: login.to_string(),
        display_name: user.get("display_name").str_or(login).to_string(),
    })
}
