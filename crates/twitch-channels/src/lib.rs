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

/// Twitch refuses free-form queries on follows ("service error"), so the
/// website's whitelisted operation is used instead.
const CHANNEL_FOLLOWS: (&str, &str) =
    ("ChannelFollows", "eecf815273d3d949e5cf0085cc5084cd8a1b5b7b6f7990cf43cb0beadf546907");

/// How many followed channels Twitch hands over per request.
pub const FOLLOWS_LIMIT: usize = 100;

pub struct Follows {
    pub channels: Vec<Channel>,
    /// False when the user follows more channels than Twitch lets us see.
    pub complete: bool,
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

/// Channels the user follows, with live status.
///
/// Twitch only answers this through one whitelisted operation, which caps
/// each answer at `FOLLOWS_LIMIT` and ignores pagination: free-form follow
/// queries are refused without the website's integrity header. Asking for
/// the newest and the oldest follows covers up to twice that many.
pub fn followed(api: &Api) -> Result<Follows, String> {
    api.require_login()?;
    let newest = follows_page(api, "DESC")?;
    let oldest = follows_page(api, "ASC").unwrap_or_default();
    let complete = newest.len() < FOLLOWS_LIMIT
        || oldest.iter().any(|o| newest.iter().any(|n| n.login == o.login));
    let mut channels = newest;
    for channel in oldest {
        if !channels.iter().any(|c| c.login == channel.login) {
            channels.push(channel);
        }
    }

    // Live status is not part of that operation.
    let logins: Vec<String> = channels.iter().map(|c| c.login.clone()).collect();
    if let Ok(live) = live_status(api, &logins) {
        for channel in channels.iter_mut() {
            if let Some(found) = live.iter().find(|l| l.login == channel.login) {
                channel.stream = found.stream.clone();
            }
        }
    }
    Ok(Follows { channels, complete })
}

fn follows_page(api: &Api, order: &str) -> Result<Vec<Channel>, String> {
    let vars = format!("{{\"limit\":{FOLLOWS_LIMIT},\"order\":{}}}", quote(order));
    let data = api.gql_persisted(CHANNEL_FOLLOWS, &vars)?;
    let follows = data.at(&["user", "follows"]);
    if follows.is_null() {
        return Err("Twitch refused the followed channels request".into());
    }
    Ok(follows.get("edges").as_array().iter().filter_map(|e| parse_channel(e.get("node"))).collect())
}

/// Looks up who is live, a hundred channels per request.
pub fn live_status(api: &Api, logins: &[String]) -> Result<Vec<Channel>, String> {
    let query = format!("query($l: [String!]) {{ users(logins: $l) {{ {CHANNEL_FIELDS} }} }}");
    let mut channels = Vec::new();
    for chunk in logins.chunks(100) {
        let list: Vec<String> = chunk.iter().map(|l| quote(l)).collect();
        let data = api.gql(&query, &format!("{{\"l\":[{}]}}", list.join(",")))?;
        channels.extend(data.get("users").as_array().iter().filter_map(parse_channel));
    }
    Ok(channels)
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
    let self_field = if api.token().is_some() { "self { follower { followedAt } }" } else { "" };
    let query = format!(
        "query($l: String!) {{ user(login: $l) {{ {CHANNEL_FIELDS} description \
         followers {{ totalCount }} {self_field} }} }}"
    );
    let data = api.gql(&query, &format!("{{\"l\":{}}}", quote(login)))?;
    let user = data.get("user");
    let channel = parse_channel(user).ok_or_else(|| format!("channel '{login}' not found"))?;
    Ok(ChannelDetails {
        channel,
        description: user.get("description").str_or("").trim().to_string(),
        followers: user.at(&["followers", "totalCount"]).as_u64().unwrap_or(0),
        following: api.token().map(|_| !user.at(&["self", "follower"]).is_null()),
    })
}

/// Follows or unfollows a channel. Twitch guards these mutations with an
/// integrity check, so on failure we fetch an integrity token and retry.
pub fn set_follow(api: &Api, channel_id: &str, follow: bool) -> Result<(), String> {
    api.require_login()?;
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
