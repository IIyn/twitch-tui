//! Application state and behaviour.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use audio::{Audio, AudioEvent};
use term::Key;
use term::line_edit::LineEdit;
use twitch_auth::Me;
use twitch_channels::{Channel, ChannelDetails, Follows};
use twitch_chat::{Chat, ChatEvent, ChatMessage, MsgKind};
use twitch_core::Api;
use twitch_playlist::{Quality, Rendition, StreamUrls};
use video::{Target, Video, VideoEvent};

use crate::Event;
use crate::config::{Config, TokenSource, VideoOutput};
use crate::viz::{VizStyle, Visualizer};

const CHAT_HISTORY: usize = 600;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(450);
const DETAILS_REFRESH: Duration = Duration::from_secs(60);
/// Often enough to catch streams as they start; `r` refreshes on demand.
const FOLLOWING_REFRESH: Duration = Duration::from_secs(60);

pub enum ApiEvent {
    Me(Result<Me, String>),
    Following(Result<Follows, String>),
    Search(u64, Result<Vec<Channel>, String>),
    Details(String, Result<ChannelDetails, String>),
    StreamUrls(u64, Result<StreamUrls, String>),
    Follow(String, bool, Result<(), String>),
    /// A background request died unexpectedly.
    Crashed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Following,
    Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Chat,
}

/// Which text field currently receives keystrokes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Typing {
    Chat,
    Search,
    Filter,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Load {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum Playback {
    Idle,
    Resolving,
    Buffering,
    Playing(Instant),
    Paused,
    Offline,
    Failed(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    Anonymous,
    Checking,
    LoggedIn(Me),
    Failed(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum ChatState {
    Connecting,
    Connected,
    Joined,
    Disconnected(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Error,
}

pub struct Toast {
    pub text: String,
    pub level: Level,
    pub until: Instant,
}

/// Live channels first, the most recently started on top, then offline ones
/// alphabetically.
fn sort_channels(channels: &mut [Channel]) {
    let started = |c: &Channel| {
        c.stream.as_ref().map(|s| crate::util::parse_iso8601(&s.started_at).unwrap_or(0))
    };
    channels.sort_by(|a, b| started(b).cmp(&started(a)).then_with(|| a.login.cmp(&b.login)));
}

pub struct App {
    tx: Sender<Event>,
    api: Api,
    pub auth: Auth,
    pub token_source: TokenSource,

    pub tab: Tab,
    pub focus: Focus,
    pub typing: Option<Typing>,

    pub following: Vec<Channel>,
    pub following_state: Load,
    /// True when Twitch withheld some follows, so the interface can say so.
    pub following_capped: bool,
    pub filter: LineEdit,
    pub search: LineEdit,
    pub results: Vec<Channel>,
    pub search_state: Load,
    search_seq: u64,
    search_due: Option<Instant>,
    pub selected: [usize; 2],

    pub current: Option<ChannelDetails>,
    pub playback: Playback,
    play_id: u64,
    pub follow_pending: bool,

    pub chat: Option<Chat>,
    pub chat_state: ChatState,
    pub messages: VecDeque<ChatMessage>,
    /// Number of messages hidden below the bottom of the chat view.
    pub chat_scroll: usize,
    pub chat_input: LineEdit,
    history: Vec<String>,
    history_pos: Option<usize>,
    my_name: Option<String>,
    my_color: Option<[u8; 3]>,

    pub audio: Audio,
    /// Audio-only rendition of the current stream.
    audio_url: Option<String>,
    /// The sound comes from the video decoder rather than the audio-only
    /// rendition, so it matches the picture.
    pub audio_from_video: bool,
    pub video: Video,
    /// Renditions carrying video for the current channel, smallest first.
    renditions: Vec<Rendition>,
    pub video_quality: Quality,
    video_started: Instant,
    /// Decoded frames per second, smoothed, for the panel title.
    pub video_fps: f32,
    video_counted: (u64, Instant),
    /// The terminal can show a real picture.
    graphics_ok: bool,
    /// The picture is drawn with the graphics protocol instead of ASCII.
    pub graphics: bool,
    pub viz: Visualizer,
    pub toast: Option<Toast>,
    pub show_help: bool,
    /// The ASCII picture fills the window.
    pub zoom: bool,
    /// Style to come back to when leaving the zoom.
    unzoom_style: VizStyle,
    pub quit: bool,

    last_details: Instant,
    last_following: Instant,
    pending_autoplay: Option<String>,
}

impl App {
    pub fn new(config: &Config, tx: Sender<Event>) -> App {
        let api = Api::new(config.token.clone());
        let mut viz = Visualizer::new();
        viz.style = config.visualizer;
        let mut app = App {
            tx,
            auth: if api.token().is_some() { Auth::Checking } else { Auth::Anonymous },
            token_source: config.token_source.clone(),
            api,
            tab: Tab::Following,
            focus: Focus::Sidebar,
            typing: None,
            following: Vec::new(),
            following_state: Load::Idle,
            following_capped: false,
            filter: LineEdit::default(),
            search: LineEdit::default(),
            results: Vec::new(),
            search_state: Load::Idle,
            search_seq: 0,
            search_due: None,
            selected: [0, 0],
            current: None,
            playback: Playback::Idle,
            play_id: 0,
            follow_pending: false,
            chat: None,
            chat_state: ChatState::Connecting,
            messages: VecDeque::new(),
            chat_scroll: 0,
            chat_input: LineEdit::default(),
            history: Vec::new(),
            history_pos: None,
            my_name: None,
            my_color: None,
            audio: Audio::new(config.volume),
            audio_url: None,
            audio_from_video: false,
            video: {
                let mut video = Video::new();
                video.fps = config.video_fps;
                let bg = crate::theme::panel().bg;
                video.background = [bg.0, bg.1, bg.2];
                video
            },
            renditions: Vec::new(),
            video_quality: config.video_quality,
            video_started: Instant::now(),
            video_fps: 0.0,
            video_counted: (0, Instant::now()),
            graphics_ok: config.video_output == VideoOutput::Graphics || crate::graphics::supported(),
            graphics: false,
            viz,
            toast: None,
            show_help: false,
            zoom: false,
            unzoom_style: VizStyle::Bars,
            quit: false,
            last_details: Instant::now(),
            last_following: Instant::now(),
            pending_autoplay: config.autoplay.clone(),
        };
        app.graphics = app.graphics_ok && config.video_output != VideoOutput::Ascii;

        if app.api.token().is_some() {
            app.following_state = Load::Loading;
            app.spawn(|api| ApiEvent::Me(twitch_auth::me(api)));
        } else {
            // Anonymous: read-only chat, start on the search tab.
            app.tab = Tab::Search;
            app.start_chat(None);
        }
        if let Some(channel) = app.pending_autoplay.take() {
            app.play(&channel);
        }
        app
    }

    /// Runs an API call on its own thread. A panic there is reported instead
    /// of leaving the interface waiting for an answer that never comes.
    fn spawn(&self, job: impl FnOnce(&Api) -> ApiEvent + Send + 'static) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&api)));
            let event = outcome.unwrap_or_else(|payload| {
                let reason = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown error".into());
                ApiEvent::Crashed(reason)
            });
            let _ = tx.send(Event::Api(event));
        });
    }

    fn start_chat(&mut self, login: Option<String>) {
        let token = login.as_ref().and(self.api.token().map(String::from));
        let chat = Chat::start(login, token, self.tx.clone());
        if let Some(current) = &self.current {
            chat.join(&current.channel.login);
        }
        self.chat = Some(chat);
    }

    pub fn notify(&mut self, text: impl Into<String>, level: Level) {
        let secs = if level == Level::Error { 6 } else { 3 };
        self.toast = Some(Toast { text: text.into(), level, until: Instant::now() + Duration::from_secs(secs) });
    }

    /// Whether the current stream offers a rendition with video.
    pub fn video_available(&self) -> bool {
        !self.renditions.is_empty()
    }

    /// Rendition the picture is decoded from.
    pub fn rendition(&self) -> Option<&Rendition> {
        self.video_quality.pick(&self.renditions)
    }

    /// Steps through auto, then every height on offer, smallest first.
    fn cycle_quality(&mut self) {
        let mut heights: Vec<u32> = self.renditions.iter().map(|r| r.height).collect();
        heights.dedup();
        if heights.is_empty() {
            self.notify("No video to pick a quality for", Level::Info);
            return;
        }
        self.video_quality = match self.video_quality {
            Quality::Auto => Quality::MaxHeight(heights[0]),
            Quality::MaxHeight(current) => match heights.iter().find(|h| **h > current) {
                Some(h) => Quality::MaxHeight(*h),
                None => Quality::Auto,
            },
        };
        let name = self.rendition().map(|r| r.name.clone()).unwrap_or_default();
        let label = if self.video_quality == Quality::Auto { format!("auto ({name})") } else { name };
        self.notify(format!("Video quality: {label}"), Level::Info);
    }

    pub fn me(&self) -> Option<&Me> {
        match &self.auth {
            Auth::LoggedIn(me) => Some(me),
            _ => None,
        }
    }

    // ------------------------------------------------------------ lists

    pub fn visible(&self) -> Vec<&Channel> {
        match self.tab {
            Tab::Following => self.visible_following(),
            Tab::Search => self.results.iter().collect(),
        }
    }

    fn visible_following(&self) -> Vec<&Channel> {
        let needle = self.filter.text().to_lowercase();
        self.following
            .iter()
            .filter(|c| {
                needle.is_empty()
                    || c.login.contains(&needle)
                    || c.display_name.to_lowercase().contains(&needle)
                    || c.stream.as_ref().is_some_and(|s| s.game.to_lowercase().contains(&needle))
            })
            .collect()
    }

    fn tab_index(&self) -> usize {
        self.tab as usize
    }

    pub fn selected_index(&self) -> usize {
        self.selected[self.tab_index()].min(self.visible().len().saturating_sub(1))
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        let i = self.selected_index() as isize + delta;
        self.selected[self.tab_index()] = i.clamp(0, len as isize - 1) as usize;
    }

    fn refresh_following(&mut self) {
        if self.me().is_none() {
            return;
        }
        self.last_following = Instant::now();
        if self.following.is_empty() {
            self.following_state = Load::Loading;
        }
        self.spawn(|api| {
            ApiEvent::Following(twitch_channels::followed(api).map(|mut follows| {
                sort_channels(&mut follows.channels);
                follows
            }))
        });
    }

    fn run_search(&mut self) {
        self.search_due = None;
        let query = self.search.text().trim().to_string();
        if query.is_empty() {
            self.results.clear();
            self.search_state = Load::Idle;
            return;
        }
        self.search_seq += 1;
        let seq = self.search_seq;
        self.search_state = Load::Loading;
        self.spawn(move |api| ApiEvent::Search(seq, twitch_channels::search(api, &query)));
    }

    // ------------------------------------------------------------ playback

    pub fn play(&mut self, login: &str) {
        let login = login.trim().trim_start_matches('#').to_lowercase();
        if login.is_empty() {
            return;
        }
        self.audio.stop();
        self.video.stop();
        self.audio_from_video = false;
        self.audio_url = None;
        self.renditions.clear();
        self.play_id += 1;
        let id = self.play_id;

        let known = self
            .following
            .iter()
            .chain(self.results.iter())
            .find(|c| c.login == login)
            .cloned();
        let is_switch = self.current.as_ref().is_none_or(|c| c.channel.login != login);
        if is_switch {
            self.current = Some(ChannelDetails {
                channel: known.unwrap_or_else(|| Channel {
                    id: String::new(),
                    login: login.clone(),
                    display_name: login.clone(),
                    stream: None,
                }),
                description: String::new(),
                followers: 0,
                following: None,
            });
            self.messages.clear();
            self.chat_scroll = 0;
            self.messages.push_back(ChatMessage::system(format!("Joining #{login}…")));
            if let Some(chat) = &self.chat {
                chat.join(&login);
            }
            let l = login.clone();
            self.spawn(move |api| ApiEvent::Details(l.clone(), twitch_channels::channel(api, &l)));
            self.last_details = Instant::now();
        }

        self.playback = Playback::Resolving;
        self.spawn(move |api| ApiEvent::StreamUrls(id, twitch_playlist::stream_urls(api, &login)));
    }

    fn toggle_playback(&mut self) {
        if self.audio.is_active() || matches!(self.playback, Playback::Resolving) {
            self.play_id += 1;
            self.audio.stop();
            self.playback = Playback::Paused;
        } else if let Some(login) = self.current.as_ref().map(|c| c.channel.login.clone()) {
            self.play(&login);
        } else {
            self.notify("Pick a channel first (Enter on the list)", Level::Info);
        }
    }

    fn change_volume(&mut self, delta: i32) {
        let v = (self.audio.volume.load(Ordering::Relaxed) as i32 + delta).clamp(0, 150) as u32;
        self.audio.volume.store(v, Ordering::Relaxed);
        self.audio.muted.store(false, Ordering::Relaxed);
    }

    fn toggle_follow(&mut self) {
        if self.me().is_none() {
            self.notify("Log in to follow channels (press ? for setup)", Level::Error);
            return;
        }
        let Some(current) = &self.current else {
            self.notify("Nothing is playing", Level::Info);
            return;
        };
        let (Some(following), false) = (current.following, current.channel.id.is_empty()) else {
            self.notify("Channel info still loading…", Level::Info);
            return;
        };
        if self.follow_pending {
            return;
        }
        self.follow_pending = true;
        let id = current.channel.id.clone();
        let name = current.channel.display_name.clone();
        self.spawn(move |api| ApiEvent::Follow(name, !following, twitch_channels::set_follow(api, &id, !following)));
    }

    fn open_in_browser(&mut self) {
        if let Some(c) = &self.current {
            let url = format!("https://www.twitch.tv/{}", c.channel.login);
            let spawned = std::process::Command::new("xdg-open")
                .arg(&url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Ok(_) => self.notify(format!("Opened {url}"), Level::Info),
                Err(e) => self.notify(format!("xdg-open failed: {e}"), Level::Error),
            }
        }
    }

    // ------------------------------------------------------------ chat

    fn push_message(&mut self, msg: ChatMessage) {
        self.messages.push_back(msg);
        if self.chat_scroll > 0 {
            self.chat_scroll += 1;
        }
        while self.messages.len() > CHAT_HISTORY {
            self.messages.pop_front();
            self.chat_scroll = self.chat_scroll.min(self.messages.len().saturating_sub(1));
        }
    }

    fn send_chat(&mut self) {
        let text = self.chat_input.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(chat) = &self.chat else { return };
        match chat.send(&text) {
            Ok(()) => {
                let name = self
                    .my_name
                    .clone()
                    .or_else(|| self.me().map(|m| m.display_name.clone()))
                    .unwrap_or_else(|| chat.nick.clone());
                let (kind, body) = match text.strip_prefix("/me ") {
                    Some(action) => (MsgKind::Action, action.to_string()),
                    None => (MsgKind::Normal, text.clone()),
                };
                let mut msg = ChatMessage::system(body);
                msg.kind = kind;
                msg.author_login = chat.nick.clone();
                msg.author = name;
                msg.color = self.my_color;
                self.push_message(msg);
                self.chat_scroll = 0;
                self.history.push(text);
                self.history_pos = None;
                self.chat_input.clear();
            }
            Err(e) => self.notify(e, Level::Error),
        }
    }

    fn recall_history(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        let pos = match (self.history_pos, older) {
            (None, true) => Some(self.history.len() - 1),
            (None, false) => None,
            (Some(p), true) => Some(p.saturating_sub(1)),
            (Some(p), false) if p + 1 < self.history.len() => Some(p + 1),
            (Some(_), false) => None,
        };
        self.history_pos = pos;
        match pos {
            Some(p) => self.chat_input.set(&self.history[p].clone()),
            None => self.chat_input.clear(),
        }
    }

    fn scroll_chat(&mut self, delta: isize) {
        let max = self.messages.len().saturating_sub(1) as isize;
        self.chat_scroll = (self.chat_scroll as isize + delta).clamp(0, max) as usize;
    }

    // ------------------------------------------------------------ events

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.on_key(key),
            Event::Api(e) => self.on_api(e),
            Event::Chat(e) => self.on_chat(e),
            Event::Audio(e) => self.on_audio(e),
            Event::Video(VideoEvent::Stopped(reason)) => {
                self.back_to_audio_only();
                // Retry later rather than switching the sound back and forth.
                self.video_started = Instant::now() + Duration::from_secs(3);
                if self.viz.style == VizStyle::Video {
                    self.notify(format!("Video stopped: {reason}"), Level::Error);
                }
            }
        }
    }

    fn on_api(&mut self, event: ApiEvent) {
        match event {
            ApiEvent::Me(Ok(me)) => {
                let source = self.token_source.label();
                self.notify(format!("Logged in as {} · {source}", me.display_name), Level::Success);
                let login = me.login.clone();
                self.auth = Auth::LoggedIn(me);
                self.start_chat(Some(login));
                self.refresh_following();
                if let Some(c) = &self.current {
                    let l = c.channel.login.clone();
                    self.spawn(move |api| ApiEvent::Details(l.clone(), twitch_channels::channel(api, &l)));
                }
            }
            ApiEvent::Me(Err(e)) => {
                let hint = match self.token_source {
                    TokenSource::Firefox { .. } => " — log in again on twitch.tv in Firefox",
                    _ => "",
                };
                self.notify(format!("Login failed: {e}{hint}"), Level::Error);
                self.auth = Auth::Failed(e.clone());
                self.following_state = Load::Failed(e);
                self.api = Api::new(None);
                self.tab = Tab::Search;
                self.start_chat(None);
            }
            ApiEvent::Following(Ok(follows)) => {
                self.following_capped = !follows.complete;
                // The order changes between refreshes: keep the cursor on its channel.
                let slot = Tab::Following as usize;
                let kept = self.visible_following().get(self.selected[slot]).map(|c| c.login.clone());
                self.following = follows.channels;
                if let Some(i) = kept.and_then(|l| self.visible_following().iter().position(|c| c.login == l)) {
                    self.selected[slot] = i;
                }
                self.following_state = Load::Ready;
            }
            ApiEvent::Following(Err(e)) => {
                if self.following.is_empty() {
                    self.following_state = Load::Failed(e.clone());
                }
                self.notify(format!("Could not load followed channels: {e}"), Level::Error);
            }
            ApiEvent::Search(seq, result) if seq == self.search_seq => match result {
                Ok(list) => {
                    self.results = list;
                    self.selected[Tab::Search as usize] = 0;
                    self.search_state = Load::Ready;
                }
                Err(e) => self.search_state = Load::Failed(e),
            },
            ApiEvent::Search(..) => {}
            ApiEvent::Details(login, result) => {
                let Some(current) = &mut self.current else { return };
                if current.channel.login != login {
                    return;
                }
                match result {
                    Ok(details) => *current = details,
                    Err(e) => self.notify(e, Level::Error),
                }
            }
            ApiEvent::StreamUrls(id, _) if id != self.play_id => {}
            ApiEvent::StreamUrls(id, Ok(urls)) => {
                self.playback = Playback::Buffering;
                self.renditions = urls.video;
                self.audio_url = Some(urls.audio.clone());
                self.audio.play(urls.audio, id, self.tx.clone());
            }
            ApiEvent::StreamUrls(_, Err(e)) => {
                if e.contains("offline") {
                    self.playback = Playback::Offline;
                } else {
                    self.notify(e.clone(), Level::Error);
                    self.playback = Playback::Failed(e);
                }
            }
            ApiEvent::Crashed(reason) => {
                self.notify(format!("Request failed: {reason}"), Level::Error);
                if self.auth == Auth::Checking {
                    self.auth = Auth::Failed(reason.clone());
                }
                if self.following_state == Load::Loading {
                    self.following_state = Load::Failed(reason.clone());
                }
                if matches!(self.playback, Playback::Resolving) {
                    self.playback = Playback::Failed(reason);
                }
                self.follow_pending = false;
            }
            ApiEvent::Follow(name, follow, result) => {
                self.follow_pending = false;
                match result {
                    Ok(()) => {
                        if let Some(c) = self.current.as_mut().filter(|c| c.channel.display_name == name) {
                            c.following = Some(follow);
                        }
                        let text = if follow { format!("♥ Followed {name}") } else { format!("Unfollowed {name}") };
                        self.notify(text, Level::Success);
                        self.refresh_following();
                    }
                    Err(e) => self.notify(e, Level::Error),
                }
            }
        }
    }

    fn on_chat(&mut self, event: ChatEvent) {
        let current = self.current.as_ref().map(|c| c.channel.login.clone());
        match event {
            ChatEvent::Connected => self.chat_state = ChatState::Connected,
            ChatEvent::Disconnected(e) => {
                self.chat_state = ChatState::Disconnected(e.clone());
                if current.is_some() {
                    self.push_message(ChatMessage::system(format!("Disconnected: {e}. Reconnecting…")));
                }
            }
            ChatEvent::Joined(channel) => {
                if Some(&channel) == current.as_ref() {
                    self.chat_state = ChatState::Joined;
                    self.push_message(ChatMessage::system(format!("Welcome to #{channel}'s chat")));
                }
            }
            ChatEvent::Message(channel, msg) => {
                if Some(&channel) == current.as_ref() || (channel.is_empty() && current.is_some()) {
                    self.push_message(msg);
                }
            }
            ChatEvent::Clear(channel, target) => {
                if Some(&channel) != current.as_ref() {
                    return;
                }
                match target {
                    Some(user) => {
                        for m in self.messages.iter_mut().filter(|m| m.author_login == user) {
                            m.deleted = true;
                        }
                    }
                    None => {
                        self.messages.clear();
                        self.chat_scroll = 0;
                        self.push_message(ChatMessage::system("Chat was cleared by a moderator"));
                    }
                }
            }
            ChatEvent::DeleteMessage(id) => {
                if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
                    m.deleted = true;
                }
            }
            ChatEvent::Identity { display_name, color } => {
                self.my_name = Some(display_name);
                if color.is_some() {
                    self.my_color = color;
                }
            }
        }
    }

    fn on_audio(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Playing(id) if id == self.play_id => {
                // Keep counting from the first start when the source switches.
                if !matches!(self.playback, Playback::Playing(_)) {
                    self.playback = Playback::Playing(Instant::now());
                }
            }
            // The video decoder died, taking the sound with it: its own event
            // reports why, the stream goes on from the audio-only rendition.
            AudioEvent::Stopped(id, _) if id == self.play_id && self.audio_from_video => self.back_to_audio_only(),
            AudioEvent::Stopped(id, err) if id == self.play_id => {
                self.playback = match err {
                    Some(e) if e != "stream ended" => {
                        self.notify(format!("Playback stopped: {e}"), Level::Error);
                        Playback::Failed(e)
                    }
                    _ => Playback::Offline,
                };
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, dt: f32, viz: (u16, u16)) {
        if self.toast.as_ref().is_some_and(|t| Instant::now() >= t.until) {
            self.toast = None;
        }
        if self.search_due.is_some_and(|due| Instant::now() >= due) {
            self.run_search();
        }
        if let Some(c) = self.current.as_ref().filter(|_| self.last_details.elapsed() >= DETAILS_REFRESH) {
            let l = c.channel.login.clone();
            self.last_details = Instant::now();
            self.spawn(move |api| ApiEvent::Details(l.clone(), twitch_channels::channel(api, &l)));
        }
        if self.me().is_some() && self.last_following.elapsed() >= FOLLOWING_REFRESH {
            self.refresh_following();
        }

        self.tick_video(viz);

        let samples: Vec<f32> = match self.audio.tap.lock() {
            Ok(tap) if matches!(self.playback, Playback::Playing(_)) => tap.samples.iter().copied().collect(),
            _ => Vec::new(),
        };
        let count = self.viz.bar_count(viz.0);
        self.viz.update(&samples, count, dt);
    }

    /// Starts, stops and resizes the picture decoder to follow the panel.
    fn tick_video(&mut self, (cols, rows): (u16, u16)) {

        let wanted = self.viz.style == VizStyle::Video
            && matches!(self.playback, Playback::Playing(_) | Playback::Buffering);
        if !wanted {
            self.back_to_audio_only();
            return;
        }
        // Measure the rate actually achieved, over a short window.
        let elapsed = self.video_counted.1.elapsed().as_secs_f32();
        if elapsed >= 0.5 {
            let decoded = self.video.decoded();
            let rate = decoded.saturating_sub(self.video_counted.0) as f32 / elapsed;
            self.video_fps = if self.video_fps == 0.0 { rate } else { self.video_fps * 0.6 + rate * 0.4 };
            self.video_counted = (decoded, Instant::now());
        }

        let Some(rendition) = self.rendition() else { return };
        let target = match term::cell_pixels() {
            Some(cell) if self.graphics => Target::pixels(cols, rows, cell, (rendition.width, rendition.height)),
            _ => Target::ascii(cols, rows),
        };
        let url = rendition.url.clone();
        let restart = !self.video.is_running() || self.video.needs_restart(&url, target);
        // Debounced, so dragging the window does not restart ffmpeg per frame.
        if restart && self.video_started.elapsed() >= Duration::from_millis(500) {
            self.video_started = Instant::now();
            // Stopped first, so the old decoder's end is not taken for the
            // stream's.
            self.audio.stop();
            match self.video.start(&url, target, self.tx.clone()) {
                Some(feed) => {
                    self.audio.play_from(feed.pcm, feed.heard, self.play_id, self.tx.clone());
                    self.audio_from_video = true;
                }
                None => self.back_to_audio_only(),
            }
        }
    }

    /// Stops the picture and, if the sound came with it, plays the audio-only
    /// rendition again.
    fn back_to_audio_only(&mut self) {
        let was_feeding = std::mem::take(&mut self.audio_from_video);
        if was_feeding && matches!(self.playback, Playback::Playing(_) | Playback::Buffering)
            && let Some(url) = self.audio_url.clone()
        {
            // Before stopping the video, so its end is not reported as the
            // stream's.
            self.audio.play(url, self.play_id, self.tx.clone());
        }
        self.video.stop();
    }

    // ------------------------------------------------------------ keys

    fn on_key(&mut self, key: Key) {
        if key == Key::Ctrl('c') {
            self.quit = true;
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        if let Some(typing) = self.typing {
            self.on_typing_key(typing, key);
            return;
        }

        // Chat and the lists are hidden while zoomed, so reaching for them
        // leaves the zoom first.
        if self.zoom && matches!(key, Key::Char('i') | Key::Tab) {
            self.leave_zoom();
            self.focus = Focus::Chat;
            self.typing = Some(Typing::Chat);
            return;
        }

        match key {
            Key::Char('q') => self.quit = true,
            Key::Char('?') => self.show_help = true,
            Key::Tab | Key::BackTab => {
                self.focus = if self.focus == Focus::Sidebar { Focus::Chat } else { Focus::Sidebar };
            }
            Key::Char('1') => {
                self.tab = Tab::Following;
                self.focus = Focus::Sidebar;
            }
            Key::Char('2') => {
                self.tab = Tab::Search;
                self.focus = Focus::Sidebar;
            }
            Key::Char('s') => {
                self.tab = Tab::Search;
                self.focus = Focus::Sidebar;
                self.typing = Some(Typing::Search);
            }
            Key::Char('/') => {
                self.focus = Focus::Sidebar;
                self.typing = Some(if self.tab == Tab::Following { Typing::Filter } else { Typing::Search });
            }
            Key::Char('i') => {
                self.focus = Focus::Chat;
                self.typing = Some(Typing::Chat);
            }
            Key::Char('f') => self.toggle_follow(),
            Key::Char(' ') | Key::Char('p') => self.toggle_playback(),
            Key::Char('+') | Key::Char('=') => self.change_volume(5),
            Key::Char('-') | Key::Char('_') => self.change_volume(-5),
            Key::Char('m') => {
                let muted = !self.audio.muted.load(Ordering::Relaxed);
                self.audio.muted.store(muted, Ordering::Relaxed);
            }
            Key::Char('v') => {
                self.viz.style = self.viz.style.next();
                self.zoom = self.zoom && self.viz.style == VizStyle::Video;
            }
            // One key for the big picture, whatever the current style.
            Key::Char('z') => {
                self.zoom = !self.zoom;
                if self.zoom {
                    self.unzoom_style = self.viz.style;
                    self.viz.style = VizStyle::Video;
                } else {
                    self.viz.style = self.unzoom_style;
                }
            }
            Key::Esc if self.zoom => self.leave_zoom(),
            Key::Char('c') => self.cycle_quality(),
            Key::Char('a') if self.graphics_ok => self.graphics = !self.graphics,
            Key::Char('a') => {
                self.notify("This terminal cannot show a real picture (kitty or Ghostty needed)", Level::Info)
            }
            Key::Char('o') => self.open_in_browser(),
            Key::Char('r') => {
                self.refresh_following();
                if let Some(c) = &self.current {
                    let l = c.channel.login.clone();
                    self.spawn(move |api| ApiEvent::Details(l.clone(), twitch_channels::channel(api, &l)));
                }
                self.notify("Refreshing…", Level::Info);
            }
            Key::Esc if !self.filter.is_empty() => self.filter.clear(),
            _ => match self.focus {
                Focus::Sidebar => self.on_sidebar_key(key),
                Focus::Chat => self.on_chat_key(key),
            },
        }
    }

    fn leave_zoom(&mut self) {
        self.zoom = false;
        self.viz.style = self.unzoom_style;
    }

    fn on_sidebar_key(&mut self, key: Key) {
        match key {
            Key::Up | Key::Char('k') => self.move_selection(-1),
            Key::Down | Key::Char('j') => self.move_selection(1),
            Key::PageUp | Key::Ctrl('u') => self.move_selection(-8),
            Key::PageDown | Key::Ctrl('d') => self.move_selection(8),
            Key::Home | Key::Char('g') => self.move_selection(isize::MIN / 2),
            Key::End | Key::Char('G') => self.move_selection(isize::MAX / 2),
            Key::Left | Key::Char('h') | Key::Right | Key::Char('l') => {
                self.tab = if self.tab == Tab::Following { Tab::Search } else { Tab::Following };
            }
            Key::Enter => {
                let login = self.visible().get(self.selected_index()).map(|c| c.login.clone());
                match login {
                    Some(login) => self.play(&login),
                    None if self.tab == Tab::Search => self.typing = Some(Typing::Search),
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn on_chat_key(&mut self, key: Key) {
        match key {
            Key::Up | Key::Char('k') => self.scroll_chat(1),
            Key::Down | Key::Char('j') => self.scroll_chat(-1),
            Key::PageUp | Key::Ctrl('u') => self.scroll_chat(10),
            Key::PageDown | Key::Ctrl('d') => self.scroll_chat(-10),
            Key::Home | Key::Char('g') => self.scroll_chat(isize::MAX / 2),
            Key::End | Key::Char('G') => self.chat_scroll = 0,
            Key::Enter => self.typing = Some(Typing::Chat),
            _ => {}
        }
    }

    fn on_typing_key(&mut self, typing: Typing, key: Key) {
        match (typing, &key) {
            (_, Key::Esc) => self.typing = None,
            (Typing::Chat, Key::Enter) => self.send_chat(),
            (Typing::Chat, Key::Up) => self.recall_history(true),
            (Typing::Chat, Key::Down) => self.recall_history(false),
            (Typing::Chat, Key::PageUp) => self.scroll_chat(10),
            (Typing::Chat, Key::PageDown) => self.scroll_chat(-10),
            (Typing::Search, Key::Enter) => {
                self.run_search();
                self.typing = None;
            }
            (Typing::Filter, Key::Enter) => self.typing = None,
            (Typing::Search | Typing::Filter, Key::Up | Key::Down) => {
                self.typing = None;
                self.on_sidebar_key(key);
            }
            (Typing::Chat, _) => {
                self.chat_input.handle(&key);
            }
            (Typing::Search, _) => {
                if self.search.handle(&key) {
                    self.search_due = Some(Instant::now() + SEARCH_DEBOUNCE);
                }
            }
            (Typing::Filter, _) => {
                if self.filter.handle(&key) {
                    self.selected[Tab::Following as usize] = 0;
                }
            }
        }
    }
}
