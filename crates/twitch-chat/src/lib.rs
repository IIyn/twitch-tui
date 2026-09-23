//! Twitch chat over IRC (plain TCP on port 6667).
//!
//! Events go back through an `mpsc::Sender` of the caller's own type, which
//! only has to be buildable from this crate's events.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use twitch_core::now_secs;


const HOST: &str = "irc.chat.twitch.tv:6667";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    Normal,
    Action,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Broadcaster,
    Moderator,
    Vip,
    Subscriber,
    Staff,
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub kind: MsgKind,
    pub author: String,
    pub color: Option<[u8; 3]>,
    pub badges: Vec<Badge>,
    /// Text split into (fragment, is_emote) spans.
    pub spans: Vec<(String, bool)>,
    pub timestamp: i64,
    pub mention: bool,
    pub author_login: String,
    pub id: String,
    pub deleted: bool,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> ChatMessage {
        ChatMessage {
            kind: MsgKind::System,
            author: String::new(),
            color: None,
            badges: Vec::new(),
            spans: vec![(text.into(), false)],
            timestamp: now_secs(),
            mention: false,
            author_login: String::new(),
            id: String::new(),
            deleted: false,
        }
    }
}

pub enum ChatEvent {
    Connected,
    Disconnected(String),
    Joined(String),
    Message(String, ChatMessage),
    /// A user's messages were cleared (timeout/ban), or all when `None`.
    Clear(String, Option<String>),
    DeleteMessage(String),
    Identity { display_name: String, color: Option<[u8; 3]> },
}

struct Shared {
    writer: Option<TcpStream>,
    channel: Option<String>,
}

/// Hands out a working access token for logging in to chat.
pub type TokenFn = Box<dyn Fn() -> Result<String, String> + Send>;

pub struct Chat {
    shared: Arc<Mutex<Shared>>,
    pub nick: String,
    pub can_send: bool,
}

impl Chat {
    /// Starts the connection thread. Without a token the connection is
    /// anonymous (read only). The token is asked for on every connection,
    /// so an expired one can be renewed in between.
    pub fn start<E>(login: Option<String>, token: Option<TokenFn>, tx: Sender<E>) -> Chat
    where
        E: From<ChatEvent> + Send + 'static,
    {
        let (nick, token) = match (login, token) {
            (Some(login), Some(token)) => (login.to_lowercase(), Some(token)),
            _ => (format!("justinfan{}", 10_000 + std::process::id() % 80_000), None),
        };
        let shared = Arc::new(Mutex::new(Shared { writer: None, channel: None }));
        let chat = Chat { shared: shared.clone(), nick: nick.clone(), can_send: token.is_some() };

        thread::spawn(move || {
            let mut backoff = 1;
            loop {
                let pass = token.as_ref().map(|t| t()).transpose();
                match pass.and_then(|pass| session(&nick, pass.as_deref(), &shared, &tx)) {
                    Ok(()) => backoff = 1,
                    Err(e) => {
                        let _ = tx.send(ChatEvent::Disconnected(e).into());
                    }
                }
                shared.lock().unwrap().writer = None;
                thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(30);
            }
        });
        chat
    }

    fn send_raw(&self, line: &str) -> Result<(), String> {
        let mut shared = self.shared.lock().unwrap();
        let writer = shared.writer.as_mut().ok_or("chat not connected")?;
        writer
            .write_all(format!("{line}\r\n").as_bytes())
            .map_err(|e| format!("chat write failed: {e}"))
    }

    pub fn join(&self, channel: &str) {
        let channel = channel.to_lowercase();
        let old = self.shared.lock().unwrap().channel.replace(channel.clone());
        if let Some(old) = old.filter(|o| *o != channel) {
            let _ = self.send_raw(&format!("PART #{old}"));
        }
        let _ = self.send_raw(&format!("JOIN #{channel}"));
    }

    pub fn send(&self, text: &str) -> Result<(), String> {
        if !self.can_send {
            return Err("log in to send messages (see ? for help)".into());
        }
        let channel = self.shared.lock().unwrap().channel.clone().ok_or("no channel joined")?;
        let text = text.replace(['\r', '\n'], " ");
        let payload = match text.strip_prefix("/me ") {
            Some(action) => format!("\x01ACTION {action}\x01"),
            None => text,
        };
        self.send_raw(&format!("PRIVMSG #{channel} :{payload}"))
    }
}

fn session<E: From<ChatEvent>>(
    nick: &str,
    pass: Option<&str>,
    shared: &Mutex<Shared>,
    tx: &Sender<E>,
) -> Result<(), String> {
    let stream = TcpStream::connect(HOST).map_err(|e| format!("chat connection failed: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(400))).ok();
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;

    let mut hello = String::from("CAP REQ :twitch.tv/tags twitch.tv/commands\r\n");
    if let Some(pass) = pass {
        hello.push_str(&format!("PASS oauth:{pass}\r\n"));
    }
    hello.push_str(&format!("NICK {nick}\r\n"));
    {
        // Hold the lock so a concurrent join() can't slip in between.
        let mut guard = shared.lock().unwrap();
        if let Some(channel) = &guard.channel {
            hello.push_str(&format!("JOIN #{channel}\r\n"));
        }
        writer.write_all(hello.as_bytes()).map_err(|e| e.to_string())?;
        guard.writer = Some(writer);
    }

    let reader = BufReader::new(stream);
    for line in reader.split(b'\n') {
        let line = line.map_err(|e| format!("chat connection lost: {e}"))?;
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end_matches('\r');
        let Some(msg) = parse_line(line) else { continue };
        match msg.command.as_str() {
            "PING" => {
                let mut shared = shared.lock().unwrap();
                if let Some(w) = shared.writer.as_mut() {
                    let _ = w.write_all(format!("PONG :{}\r\n", msg.trailing).as_bytes());
                }
            }
            "RECONNECT" => return Err("server asked to reconnect".into()),
            "001" => {
                let _ = tx.send(ChatEvent::Connected.into());
            }
            "JOIN" => {
                if msg.prefix_nick() == nick {
                    let _ = tx.send(ChatEvent::Joined(msg.channel()).into());
                }
            }
            "PRIVMSG" => {
                let _ = tx.send(ChatEvent::Message(msg.channel(), to_chat_message(&msg, nick)).into());
            }
            "USERNOTICE" => {
                let system = msg.tag("system-msg").to_string();
                if !system.is_empty() {
                    let _ = tx.send(ChatEvent::Message(msg.channel(), ChatMessage::system(system)).into());
                }
                if !msg.trailing.is_empty() {
                    let _ = tx.send(ChatEvent::Message(msg.channel(), to_chat_message(&msg, nick)).into());
                }
            }
            "NOTICE" => {
                let text = msg.trailing.clone();
                if text.contains("Login authentication failed") || text.contains("Improperly formatted auth") {
                    return Err(format!("chat login failed: {text}"));
                }
                let _ = tx.send(ChatEvent::Message(msg.channel(), ChatMessage::system(text)).into());
            }
            "CLEARCHAT" => {
                let target = (!msg.trailing.is_empty()).then(|| msg.trailing.to_lowercase());
                let _ = tx.send(ChatEvent::Clear(msg.channel(), target).into());
            }
            "CLEARMSG" => {
                let _ = tx.send(ChatEvent::DeleteMessage(msg.tag("target-msg-id").to_string()).into());
            }
            "USERSTATE" | "GLOBALUSERSTATE" => {
                let display_name = msg.tag("display-name").to_string();
                if !display_name.is_empty() {
                    let _ = tx.send(
                        ChatEvent::Identity { display_name, color: hex_color(msg.tag("color")) }.into(),
                    );
                }
            }
            _ => {}
        }
    }
    Err("chat connection closed".into())
}

struct IrcMessage {
    tags: HashMap<String, String>,
    prefix: String,
    command: String,
    params: Vec<String>,
    trailing: String,
}

impl IrcMessage {
    fn tag(&self, key: &str) -> &str {
        self.tags.get(key).map(String::as_str).unwrap_or("")
    }
    fn prefix_nick(&self) -> &str {
        self.prefix.split('!').next().unwrap_or("")
    }
    fn channel(&self) -> String {
        self.params
            .iter()
            .find_map(|p| p.strip_prefix('#'))
            .unwrap_or("")
            .to_string()
    }
}

fn unescape_tag(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some(':') => out.push(';'),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn parse_line(line: &str) -> Option<IrcMessage> {
    let mut rest = line;
    let mut tags = HashMap::new();
    if let Some(stripped) = rest.strip_prefix('@') {
        let (raw, after) = stripped.split_once(' ')?;
        for pair in raw.split(';') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            tags.insert(k.to_string(), unescape_tag(v));
        }
        rest = after;
    }
    let mut prefix = String::new();
    if let Some(stripped) = rest.strip_prefix(':') {
        let (p, after) = stripped.split_once(' ').unwrap_or((stripped, ""));
        prefix = p.to_string();
        rest = after;
    }
    let (head, trailing) = match rest.split_once(" :") {
        Some((h, t)) => (h, t.to_string()),
        None => (rest, String::new()),
    };
    let mut parts = head.split_whitespace();
    let command = parts.next()?.to_string();
    Some(IrcMessage { tags, prefix, command, params: parts.map(String::from).collect(), trailing })
}

fn to_chat_message(msg: &IrcMessage, own_nick: &str) -> ChatMessage {
    let mut text = msg.trailing.clone();
    let mut kind = MsgKind::Normal;
    if let Some(action) = text.strip_prefix("\x01ACTION ").map(|t| t.trim_end_matches('\x01').to_string()) {
        text = action;
        kind = MsgKind::Action;
    }
    let login = msg.prefix_nick().to_string();
    let author = match msg.tag("display-name") {
        "" => login.clone(),
        name => name.to_string(),
    };
    let badges = msg
        .tag("badges")
        .split(',')
        .filter_map(|b| match b.split('/').next()? {
            "broadcaster" => Some(Badge::Broadcaster),
            "moderator" => Some(Badge::Moderator),
            "vip" => Some(Badge::Vip),
            "subscriber" | "founder" => Some(Badge::Subscriber),
            "staff" | "admin" | "global_mod" => Some(Badge::Staff),
            _ => None,
        })
        .collect();
    let timestamp = msg.tag("tmi-sent-ts").parse::<i64>().map(|ms| ms / 1000).unwrap_or_else(|_| now_secs());
    let lower = text.to_lowercase();
    let mention = !own_nick.starts_with("justinfan") && lower.contains(&own_nick.to_lowercase());
    ChatMessage {
        kind,
        spans: split_emotes(&text, msg.tag("emotes")),
        color: hex_color(msg.tag("color")),
        author,
        badges,
        timestamp,
        mention,
        author_login: login,
        id: msg.tag("id").to_string(),
        deleted: false,
    }
}

/// Splits text into plain and emote spans using the `emotes` tag
/// (`id:start-end,start-end/id:...`, positions in code points).
fn split_emotes(text: &str, tag: &str) -> Vec<(String, bool)> {
    let mut ranges: Vec<(usize, usize)> = tag
        .split('/')
        .filter_map(|e| e.split_once(':'))
        .flat_map(|(_, positions)| positions.split(','))
        .filter_map(|r| {
            let (a, b) = r.split_once('-')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .collect();
    if ranges.is_empty() {
        return vec![(text.to_string(), false)];
    }
    ranges.sort();
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut pos = 0;
    for (start, end) in ranges {
        if start < pos || end >= chars.len() {
            continue;
        }
        if start > pos {
            spans.push((chars[pos..start].iter().collect(), false));
        }
        spans.push((chars[start..=end].iter().collect(), true));
        pos = end + 1;
    }
    if pos < chars.len() {
        spans.push((chars[pos..].iter().collect(), false));
    }
    spans
}

/// `#rrggbb`, as Twitch sends user colors.
fn hex_color(s: &str) -> Option<[u8; 3]> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_privmsg() {
        let line = "@badges=moderator/1,subscriber/6;color=#FF0000;display-name=Foo\\sBar;emotes=25:6-10;tmi-sent-ts=1700000000000 \
                    :foo!foo@foo.tmi.twitch.tv PRIVMSG #chan :hello Kappa hi";
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.command, "PRIVMSG");
        assert_eq!(msg.channel(), "chan");
        let chat = to_chat_message(&msg, "me");
        assert_eq!(chat.author, "Foo Bar");
        assert_eq!(chat.badges, vec![Badge::Moderator, Badge::Subscriber]);
        assert_eq!(chat.spans[1], ("Kappa".to_string(), true));
        assert_eq!(chat.timestamp, 1_700_000_000);
    }

    #[test]
    fn parses_action() {
        let msg = parse_line(":a!a@a PRIVMSG #c :\x01ACTION waves\x01").unwrap();
        let chat = to_chat_message(&msg, "me");
        assert_eq!(chat.kind, MsgKind::Action);
        assert_eq!(chat.spans[0].0, "waves");
    }
}
