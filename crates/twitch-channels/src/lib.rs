//! Channels: the ones the user follows, search, and channel pages.

use twitch_core::Api;
use twitch_core::gql::GqlError;
use twitch_core::json::{Json, quote};

#[derive(Clone, Debug)]
pub struct Stream {
    pub title: String,
    pub game: String,
    pub viewers: u64,
    pub started_at: String,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: String,
    pub login: String,
    pub display_name: String,
    pub stream: Option<Stream>,
}

#[derive(Clone, Debug)]
pub struct ChannelDetails {
    pub channel: Channel,
    pub description: String,
    pub followers: u64,
    /// `None` when logged out.
    pub following: Option<bool>,
}

const CHANNEL_FIELDS: &str =
    "id login displayName stream { title viewersCount createdAt game { displayName } }";

fn parse_channel(node: &Json) -> Option<Channel> {
    let login = node.get("login").as_str()?.to_string();
    let stream = node.get("stream");
    Some(Channel {
        id: node.get("id").str_or("").to_string(),
        display_name: node.get("displayName").str_or(&login).to_string(),
        login,
        stream: (!stream.is_null()).then(|| Stream {
            title: stream.get("title").str_or("").trim().to_string(),
            game: stream.at(&["game", "displayName"]).str_or("").to_string(),
            viewers: stream.get("viewersCount").as_u64().unwrap_or(0),
            started_at: stream.get("createdAt").str_or("").to_string(),
        }),
    })
}

/// Every channel the user follows, with live status.
pub fn followed(api: &Api) -> Result<Vec<Channel>, String> {
    let helix = api.helix()?;
    let user = helix.user_id();
    let follows = helix.get_all(&format!("/channels/followed?user_id={user}&first=100"))?;
    // The live ones only; a failure here still leaves the list usable.
    let live = helix.get_all(&format!("/streams/followed?user_id={user}&first=100")).unwrap_or_default();
    Ok(follows
        .iter()
        .filter_map(|f| {
            let id = f.get("broadcaster_id").as_str()?;
            let login = f.get("broadcaster_login").as_str()?.to_string();
            let stream = live.iter().find(|s| s.get("user_id").as_str() == Some(id));
            Some(Channel {
                id: id.to_string(),
                display_name: f.get("broadcaster_name").str_or(&login).to_string(),
                login,
                stream: stream.map(|s| Stream {
                    title: s.get("title").str_or("").trim().to_string(),
                    game: s.get("game_name").str_or("").to_string(),
                    viewers: s.get("viewer_count").as_u64().unwrap_or(0),
                    started_at: s.get("started_at").str_or("").to_string(),
                }),
            })
        })
        .collect())
}

pub fn search(api: &Api, text: &str) -> Result<Vec<Channel>, String> {
    let query = format!(
        "query($q: String!) {{ searchFor(userQuery: $q, platform: \"web\") {{ \
         channels {{ items {{ {CHANNEL_FIELDS} }} }} }} }}"
    );
    let data = api.gql(&query, &format!("{{\"q\":{}}}", quote(text)))?;
    Ok(data
        .at(&["searchFor", "channels", "items"])
        .as_array()
        .iter()
        .filter_map(parse_channel)
        .collect())
}

pub fn channel(api: &Api, login: &str) -> Result<ChannelDetails, String> {
    let query = format!(
        "query($l: String!) {{ user(login: $l) {{ {CHANNEL_FIELDS} description \
         followers {{ totalCount }} }} }}"
    );
    let data = api.gql(&query, &format!("{{\"l\":{}}}", quote(login)))?;
    let user = data.get("user");
    let channel = parse_channel(user).ok_or_else(|| format!("channel '{login}' not found"))?;
    let channel_id = channel.id.clone();
    Ok(ChannelDetails {
        channel,
        description: user.get("description").str_or("").trim().to_string(),
        followers: user.at(&["followers", "totalCount"]).as_u64().unwrap_or(0),
        following: is_following(api, &channel_id),
    })
}

/// `None` when logged out or when Twitch does not answer.
fn is_following(api: &Api, channel_id: &str) -> Option<bool> {
    let helix = api.helix().ok()?;
    if channel_id.is_empty() {
        return None;
    }
    let path = format!("/channels/followed?user_id={}&broadcaster_id={channel_id}", helix.user_id());
    let doc = helix.get(&path).ok()?;
    Some(!doc.get("data").as_array().is_empty())
}

/// Follows or unfollows a channel. The public API has no way to, so this
/// goes through the website's API, which needs the website token. Twitch
/// guards these mutations with an integrity check, so on failure we fetch an
/// integrity token and retry.
pub fn set_follow(api: &Api, channel_id: &str, follow: bool) -> Result<(), String> {
    if api.token().is_none() {
        return Err("Twitch only lets its own site follow channels — press o to open the channel".into());
    }
    let query = if follow {
        "mutation($id: ID!) { followUser(input: { targetID: $id, disableNotifications: false }) \
         { follow { user { id } } error { code } } }"
    } else {
        "mutation($id: ID!) { unfollowUser(input: { targetID: $id }) { follow { user { id } } } }"
    };
    let vars = format!("{{\"id\":{}}}", quote(channel_id));
    let data = match api.gql_raw(query, &vars, None) {
        Err(GqlError::Integrity) => {
            let token = api.integrity_token()?;
            api.gql_raw(query, &vars, Some(&token)).map_err(|e| e.to_string())?
        }
        other => other.map_err(|e| e.to_string())?,
    };
    if let Some(code) = data.at(&["followUser", "error", "code"]).as_str() {
        return Err(format!("follow refused by Twitch: {code}"));
    }
    Ok(())
}
