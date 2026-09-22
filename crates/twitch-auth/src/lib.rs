//! Logging in: the session token lifted from Firefox's cookies, and the
//! account it opens.

pub mod cookies;
mod sqlite;

use twitch_core::Api;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Me {
    pub id: String,
    pub login: String,
    pub display_name: String,
}

/// The account the token opens.
pub fn me(api: &Api) -> Result<Me, String> {
    api.require_login()?;
    let data = api.gql("query { currentUser { id login displayName } }", "{}")?;
    let user = data.get("currentUser");
    let login = user.get("login").as_str().ok_or("token is invalid or expired")?;
    Ok(Me {
        id: user.get("id").str_or("").to_string(),
        login: login.to_string(),
        display_name: user.get("displayName").str_or(login).to_string(),
    })
}
