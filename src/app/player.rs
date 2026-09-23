//! The player and the chat: playback, picture, visualizer, following the
//! current channel. Linux only.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use audio::{Audio, AudioEvent};
use term::Key;
use term::line_edit::LineEdit;
use twitch_channels::{Channel, ChannelDetails};
use twitch_chat::{Chat, ChatEvent, ChatMessage, MsgKind, TokenFn};
use twitch_playlist::{Quality, Rendition, StreamUrls};
use video::{Target, Video, VideoEvent};

use super::{ApiEvent, App, Focus, Level, Typing};
use crate::Event;
use crate::config::{Config, VideoOutput};
use crate::viz::{VizStyle, Visualizer};

const CHAT_HISTORY: usize = 600;
const DETAILS_REFRESH: Duration = Duration::from_secs(60);

/// Answers of the requests the player makes.
pub enum PlayerApi {
    Details(String, Result<ChannelDetails, String>),
    StreamUrls(u64, Result<StreamUrls, String>),
    Follow(String, bool, Result<(), String>),
}

/// What the chat connection and the decoders report.
pub enum PlayerEvent {
    Chat(ChatEvent),
    Audio(AudioEvent),
    Video(VideoEvent),
}

impl From<ChatEvent> for Event {
    fn from(e: ChatEvent) -> Event {
        Event::Player(PlayerEvent::Chat(e))
    }
}

impl From<AudioEvent> for Event {
    fn from(e: AudioEvent) -> Event {
        Event::Player(PlayerEvent::Audio(e))
    }
}

impl From<VideoEvent> for Event {
    fn from(e: VideoEvent) -> Event {
        Event::Player(PlayerEvent::Video(e))
    }
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
pub enum ChatState {
    Connecting,
    Connected,
    Joined,
    Disconnected(String),
}

pub struct Player {
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
    /// The ASCII picture fills the window.
    pub zoom: bool,
    /// Style to come back to when leaving the zoom.
    unzoom_style: VizStyle,

    last_details: Instant,
}

impl Player {
    pub fn new(config: &Config) -> Player {
        let mut viz = Visualizer::new();
        viz.style = config.visualizer;
        let mut video = Video::new();
        video.fps = config.video_fps;
        let bg = crate::theme::panel().bg;
        video.background = [bg.0, bg.1, bg.2];
        let graphics_ok = config.video_output == VideoOutput::Graphics || crate::graphics::supported();
        Player {
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
            video,
            renditions: Vec::new(),
            video_quality: config.video_quality,
            video_started: Instant::now(),
            video_fps: 0.0,
            video_counted: (0, Instant::now()),
            graphics_ok,
            graphics: graphics_ok && config.video_output != VideoOutput::Ascii,
            viz,
            zoom: false,
            unzoom_style: VizStyle::Bars,
            last_details: Instant::now(),
        }
    }

    pub fn current_login(&self) -> Option<String> {
        self.current.as_ref().map(|c| c.channel.login.clone())
    }

    /// Whether the current stream offers a rendition with video.
    pub fn video_available(&self) -> bool {
        !self.renditions.is_empty()
    }

    /// Rendition the picture is decoded from.
    pub fn rendition(&self) -> Option<&Rendition> {
        self.video_quality.pick(&self.renditions)
    }

    fn change_volume(&mut self, delta: i32) {
        let v = (self.audio.volume.load(Ordering::Relaxed) as i32 + delta).clamp(0, 150) as u32;
        self.audio.volume.store(v, Ordering::Relaxed);
        self.audio.muted.store(false, Ordering::Relaxed);
    }

    fn leave_zoom(&mut self) {
        self.zoom = false;
        self.viz.style = self.unzoom_style;
    }

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
}

impl App {
    pub(super) fn start_chat(&mut self, login: Option<String>) {
        if self.channels_only {
            return;
        }
        let api = self.api.clone();
        let token = (login.is_some() && api.logged_in())
            .then(|| Box::new(move || api.helix()?.access_token()) as TokenFn);
        let chat = Chat::start(login, token, self.tx.clone());
        if let Some(current) = &self.player.current {
            chat.join(&current.channel.login);
        }
        self.player.chat = Some(chat);
    }

    /// The account is known: chat as it, and see whether it follows the
    /// current channel.
    pub(super) fn player_logged_in(&mut self) {
        let login = self.me().map(|me| me.login.clone());
        self.start_chat(login);
        self.refresh_details();
    }

    pub(super) fn refresh_details(&mut self) {
        if let Some(login) = self.player.current_login() {
            self.player.last_details = Instant::now();
            self.spawn(move |api| ApiEvent::Player(PlayerApi::Details(login.clone(), twitch_channels::channel(api, &login))));
        }
    }

    pub(super) fn player_request_crashed(&mut self, reason: String) {
        if matches!(self.player.playback, Playback::Resolving) {
            self.player.playback = Playback::Failed(reason);
        }
        self.player.follow_pending = false;
    }

    /// Steps through auto, then every height on offer, smallest first.
    fn cycle_quality(&mut self) {
        let player = &mut self.player;
        let mut heights: Vec<u32> = player.renditions.iter().map(|r| r.height).collect();
        heights.dedup();
        if heights.is_empty() {
            self.notify("No video to pick a quality for", Level::Info);
            return;
        }
        player.video_quality = match player.video_quality {
            Quality::Auto => Quality::MaxHeight(heights[0]),
            Quality::MaxHeight(current) => match heights.iter().find(|h| **h > current) {
                Some(h) => Quality::MaxHeight(*h),
                None => Quality::Auto,
            },
        };
        let name = player.rendition().map(|r| r.name.clone()).unwrap_or_default();
        let label = if player.video_quality == Quality::Auto { format!("auto ({name})") } else { name };
        self.notify(format!("Video quality: {label}"), Level::Info);
    }

    // ------------------------------------------------------------ playback

    pub fn play(&mut self, login: &str) {
        let login = login.trim().trim_start_matches('#').to_lowercase();
        if login.is_empty() {
            return;
        }
        let known = self
            .following
            .iter()
            .chain(self.results.iter())
            .find(|c| c.login == login)
            .cloned();
        let player = &mut self.player;
        player.audio.stop();
        player.video.stop();
        player.audio_from_video = false;
        player.audio_url = None;
        player.renditions.clear();
        player.play_id += 1;
        let id = player.play_id;

        let is_switch = player.current.as_ref().is_none_or(|c| c.channel.login != login);
        if is_switch {
            player.current = Some(ChannelDetails {
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
            player.messages.clear();
            player.chat_scroll = 0;
            player.messages.push_back(ChatMessage::system(format!("Joining #{login}…")));
            if let Some(chat) = &player.chat {
                chat.join(&login);
            }
            self.refresh_details();
        }

        self.player.playback = Playback::Resolving;
        self.spawn(move |api| ApiEvent::Player(PlayerApi::StreamUrls(id, twitch_playlist::stream_urls(api, &login))));
    }

    fn toggle_playback(&mut self) {
        let player = &mut self.player;
        if player.audio.is_active() || matches!(player.playback, Playback::Resolving) {
            player.play_id += 1;
            player.audio.stop();
            player.playback = Playback::Paused;
        } else if let Some(login) = player.current_login() {
            self.play(&login);
        } else {
            self.notify("Pick a channel first (Enter on the list)", Level::Info);
        }
    }

    fn toggle_follow(&mut self) {
        if self.me().is_none() {
            self.notify("Log in to follow channels (press ? for setup)", Level::Error);
            return;
        }
        let Some(current) = &self.player.current else {
            self.notify("Nothing is playing", Level::Info);
            return;
        };
        let (Some(following), false) = (current.following, current.channel.id.is_empty()) else {
            self.notify("Channel info still loading…", Level::Info);
            return;
        };
        if self.player.follow_pending {
            return;
        }
        let id = current.channel.id.clone();
        let name = current.channel.display_name.clone();
        self.player.follow_pending = true;
        self.spawn(move |api| {
            ApiEvent::Player(PlayerApi::Follow(name, !following, twitch_channels::set_follow(api, &id, !following)))
        });
    }

    // ------------------------------------------------------------ chat

    fn send_chat(&mut self) {
        let player = &self.player;
        let text = player.chat_input.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(chat) = &player.chat else { return };
        match chat.send(&text) {
            Ok(()) => {
                let name = player
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
                msg.color = player.my_color;
                let player = &mut self.player;
                player.push_message(msg);
                player.chat_scroll = 0;
                player.history.push(text);
                player.history_pos = None;
                player.chat_input.clear();
            }
            Err(e) => self.notify(e, Level::Error),
        }
    }

    // ------------------------------------------------------------ events

    pub(super) fn on_player_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Chat(e) => self.on_chat(e),
            PlayerEvent::Audio(e) => self.on_audio(e),
            PlayerEvent::Video(VideoEvent::Stopped(reason)) => {
                self.back_to_audio_only();
                // Retry later rather than switching the sound back and forth.
                self.player.video_started = Instant::now() + Duration::from_secs(3);
                if self.player.viz.style == VizStyle::Video {
                    self.notify(format!("Video stopped: {reason}"), Level::Error);
                }
            }
        }
    }

    pub(super) fn on_player_api(&mut self, event: PlayerApi) {
        match event {
            PlayerApi::Details(login, result) => {
                let Some(current) = &mut self.player.current else { return };
                if current.channel.login != login {
                    return;
                }
                match result {
                    Ok(details) => *current = details,
                    Err(e) => self.notify(e, Level::Error),
                }
            }
            PlayerApi::StreamUrls(id, _) if id != self.player.play_id => {}
            PlayerApi::StreamUrls(id, Ok(urls)) => {
                let player = &mut self.player;
                player.playback = Playback::Buffering;
                player.renditions = urls.video;
                player.audio_url = Some(urls.audio.clone());
                player.audio.play(urls.audio, id, self.tx.clone());
            }
            PlayerApi::StreamUrls(_, Err(e)) => {
                if e.contains("offline") {
                    self.player.playback = Playback::Offline;
                } else {
                    self.notify(e.clone(), Level::Error);
                    self.player.playback = Playback::Failed(e);
                }
            }
            PlayerApi::Follow(name, follow, result) => {
                self.player.follow_pending = false;
                match result {
                    Ok(()) => {
                        if let Some(c) = self.player.current.as_mut().filter(|c| c.channel.display_name == name) {
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
        let player = &mut self.player;
        let current = player.current_login();
        match event {
            ChatEvent::Connected => player.chat_state = ChatState::Connected,
            ChatEvent::Disconnected(e) => {
                player.chat_state = ChatState::Disconnected(e.clone());
                if current.is_some() {
                    player.push_message(ChatMessage::system(format!("Disconnected: {e}. Reconnecting…")));
                }
            }
            ChatEvent::Joined(channel) => {
                if Some(&channel) == current.as_ref() {
                    player.chat_state = ChatState::Joined;
                    player.push_message(ChatMessage::system(format!("Welcome to #{channel}'s chat")));
                }
            }
            ChatEvent::Message(channel, msg) => {
                if Some(&channel) == current.as_ref() || (channel.is_empty() && current.is_some()) {
                    player.push_message(msg);
                }
            }
            ChatEvent::Clear(channel, target) => {
                if Some(&channel) != current.as_ref() {
                    return;
                }
                match target {
                    Some(user) => {
                        for m in player.messages.iter_mut().filter(|m| m.author_login == user) {
                            m.deleted = true;
                        }
                    }
                    None => {
                        player.messages.clear();
                        player.chat_scroll = 0;
                        player.push_message(ChatMessage::system("Chat was cleared by a moderator"));
                    }
                }
            }
            ChatEvent::DeleteMessage(id) => {
                if let Some(m) = player.messages.iter_mut().find(|m| m.id == id) {
                    m.deleted = true;
                }
            }
            ChatEvent::Identity { display_name, color } => {
                player.my_name = Some(display_name);
                if color.is_some() {
                    player.my_color = color;
                }
            }
        }
    }

    fn on_audio(&mut self, event: AudioEvent) {
        let play_id = self.player.play_id;
        match event {
            AudioEvent::Playing(id) if id == play_id => {
                // Keep counting from the first start when the source switches.
                if !matches!(self.player.playback, Playback::Playing(_)) {
                    self.player.playback = Playback::Playing(Instant::now());
                }
            }
            // The video decoder died, taking the sound with it: its own event
            // reports why, the stream goes on from the audio-only rendition.
            AudioEvent::Stopped(id, _) if id == play_id && self.player.audio_from_video => self.back_to_audio_only(),
            AudioEvent::Stopped(id, err) if id == play_id => {
                self.player.playback = match err {
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

    /// Timers of the player and the visualizer, sized `viz` in cells.
    pub fn tick_player(&mut self, dt: f32, viz: (u16, u16)) {
        if self.player.current.is_some() && self.player.last_details.elapsed() >= DETAILS_REFRESH {
            self.refresh_details();
        }

        self.tick_video(viz);

        let player = &mut self.player;
        let samples: Vec<f32> = match player.audio.tap.lock() {
            Ok(tap) if matches!(player.playback, Playback::Playing(_)) => tap.samples.iter().copied().collect(),
            _ => Vec::new(),
        };
        let count = player.viz.bar_count(viz.0);
        player.viz.update(&samples, count, dt);
    }

    /// Starts, stops and resizes the picture decoder to follow the panel.
    fn tick_video(&mut self, (cols, rows): (u16, u16)) {
        let player = &mut self.player;
        let wanted = player.viz.style == VizStyle::Video
            && matches!(player.playback, Playback::Playing(_) | Playback::Buffering);
        if !wanted {
            self.back_to_audio_only();
            return;
        }
        // Measure the rate actually achieved, over a short window.
        let elapsed = player.video_counted.1.elapsed().as_secs_f32();
        if elapsed >= 0.5 {
            let decoded = player.video.decoded();
            let rate = decoded.saturating_sub(player.video_counted.0) as f32 / elapsed;
            player.video_fps = if player.video_fps == 0.0 { rate } else { player.video_fps * 0.6 + rate * 0.4 };
            player.video_counted = (decoded, Instant::now());
        }

        let Some(rendition) = player.rendition() else { return };
        let target = match term::cell_pixels() {
            Some(cell) if player.graphics => Target::pixels(cols, rows, cell, (rendition.width, rendition.height)),
            _ => Target::ascii(cols, rows),
        };
        let url = rendition.url.clone();
        let restart = !player.video.is_running() || player.video.needs_restart(&url, target);
        // Debounced, so dragging the window does not restart ffmpeg per frame.
        if restart && player.video_started.elapsed() >= Duration::from_millis(500) {
            player.video_started = Instant::now();
            // Stopped first, so the old decoder's end is not taken for the
            // stream's.
            player.audio.stop();
            match player.video.start(&url, target, self.tx.clone()) {
                Some(feed) => {
                    player.audio.play_from(feed.pcm, feed.heard, player.play_id, self.tx.clone());
                    player.audio_from_video = true;
                }
                None => self.back_to_audio_only(),
            }
        }
    }

    /// Stops the picture and, if the sound came with it, plays the audio-only
    /// rendition again.
    fn back_to_audio_only(&mut self) {
        let player = &mut self.player;
        let was_feeding = std::mem::take(&mut player.audio_from_video);
        if was_feeding && matches!(player.playback, Playback::Playing(_) | Playback::Buffering)
            && let Some(url) = player.audio_url.clone()
        {
            // Before stopping the video, so its end is not reported as the
            // stream's.
            player.audio.play(url, player.play_id, self.tx.clone());
        }
        player.video.stop();
    }

    // ------------------------------------------------------------ keys

    /// Keys of the player and the chat. False when the key is not one.
    pub(super) fn on_player_key(&mut self, key: &Key) -> bool {
        // Chat and the lists are hidden while zoomed, so reaching for them
        // leaves the zoom first.
        if self.player.zoom && matches!(key, Key::Char('i') | Key::Tab) {
            self.player.leave_zoom();
            self.focus = Focus::Chat;
            self.typing = Some(Typing::Chat);
            return true;
        }

        let player = &mut self.player;
        match key {
            Key::Tab | Key::BackTab => {
                self.focus = if self.focus == Focus::Sidebar { Focus::Chat } else { Focus::Sidebar };
            }
            Key::Char('i') => {
                self.focus = Focus::Chat;
                self.typing = Some(Typing::Chat);
            }
            Key::Char('f') => self.toggle_follow(),
            Key::Char(' ') | Key::Char('p') => self.toggle_playback(),
            Key::Char('+') | Key::Char('=') => player.change_volume(5),
            Key::Char('-') | Key::Char('_') => player.change_volume(-5),
            Key::Char('m') => {
                let muted = !player.audio.muted.load(Ordering::Relaxed);
                player.audio.muted.store(muted, Ordering::Relaxed);
            }
            Key::Char('v') => {
                player.viz.style = player.viz.style.next();
                player.zoom = player.zoom && player.viz.style == VizStyle::Video;
            }
            // One key for the big picture, whatever the current style.
            Key::Char('z') => {
                player.zoom = !player.zoom;
                if player.zoom {
                    player.unzoom_style = player.viz.style;
                    player.viz.style = VizStyle::Video;
                } else {
                    player.viz.style = player.unzoom_style;
                }
            }
            Key::Esc if player.zoom => player.leave_zoom(),
            Key::Char('c') => self.cycle_quality(),
            Key::Char('a') if player.graphics_ok => player.graphics = !player.graphics,
            Key::Char('a') => {
                self.notify("This terminal cannot show a real picture (kitty or Ghostty needed)", Level::Info)
            }
            _ => return false,
        }
        true
    }

    pub(super) fn on_chat_key(&mut self, key: Key) {
        let player = &mut self.player;
        match key {
            Key::Up | Key::Char('k') => player.scroll_chat(1),
            Key::Down | Key::Char('j') => player.scroll_chat(-1),
            Key::PageUp | Key::Ctrl('u') => player.scroll_chat(10),
            Key::PageDown | Key::Ctrl('d') => player.scroll_chat(-10),
            Key::Home | Key::Char('g') => player.scroll_chat(isize::MAX / 2),
            Key::End | Key::Char('G') => player.chat_scroll = 0,
            Key::Enter => self.typing = Some(Typing::Chat),
            _ => {}
        }
    }

    /// Keys while writing in chat, Esc aside.
    pub(super) fn on_chat_typing_key(&mut self, key: Key) {
        let player = &mut self.player;
        match key {
            Key::Enter => self.send_chat(),
            Key::Up => player.recall_history(true),
            Key::Down => player.recall_history(false),
            Key::PageUp => player.scroll_chat(10),
            Key::PageDown => player.scroll_chat(-10),
            _ => {
                player.chat_input.handle(&key);
            }
        }
    }
}
