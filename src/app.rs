//! Application state and behaviour: logging in and the channel lists. The
//! player and the chat, Linux only, live in `player`.

#[cfg(target_os = "linux")]
pub mod player;

use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use term::Key;
use term::line_edit::LineEdit;
use twitch_auth::Me;
use twitch_channels::Channel;
use twitch_core::Api;
use twitch_core::helix::Helix;

use crate::Event;
use crate::config::{Config, TokenSource};
#[cfg(target_os = "linux")]
use player::{Player, PlayerApi};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(450);
/// Often enough to catch streams as they start; `r` refreshes on demand.
const FOLLOWING_REFRESH: Duration = Duration::from_secs(60);

pub enum ApiEvent {
    Me(Result<Me, String>),
    Following(Result<Vec<Channel>, String>),
    Search(u64, Result<Vec<Channel>, String>),
    #[cfg(target_os = "linux")]
    Player(PlayerApi),
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
    #[cfg(target_os = "linux")]
    Chat,
}

/// Which text field currently receives keystrokes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Typing {
    #[cfg(target_os = "linux")]
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
pub enum Auth {
    Anonymous,
    Checking,
    LoggedIn(Me),
    Failed(String),
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
    pub filter: LineEdit,
    pub search: LineEdit,
    pub results: Vec<Channel>,
    pub search_state: Load,
    search_seq: u64,
    search_due: Option<Instant>,
    pub selected: [usize; 2],

    pub toast: Option<Toast>,
    pub show_help: bool,
    pub quit: bool,
    /// Only the channel lists: no playback nor chat. Always on outside Linux.
    pub channels_only: bool,
    last_following: Instant,

    #[cfg(target_os = "linux")]
    pub player: Player,
}

impl App {
    pub fn new(config: &Config, tx: Sender<Event>) -> App {
        let mut api = Api::new(config.token.clone());
        if let Some(session) = config.session.clone() {
            // A renewed refresh token voids the saved one: keep the file current.
            api = api.with_helix(Helix::new(session, |s| {
                let _ = twitch_auth::session::save(s);
            }));
        }
        let mut app = App {
            tx,
            auth: if api.logged_in() { Auth::Checking } else { Auth::Anonymous },
            token_source: config.token_source.clone(),
            api,
            tab: Tab::Following,
            focus: Focus::Sidebar,
            typing: None,
            following: Vec::new(),
            following_state: Load::Idle,
            filter: LineEdit::default(),
            search: LineEdit::default(),
            results: Vec::new(),
            search_state: Load::Idle,
            search_seq: 0,
            search_due: None,
            selected: [0, 0],
            toast: None,
            show_help: false,
            quit: false,
            channels_only: config.channels_only,
            last_following: Instant::now(),
            #[cfg(target_os = "linux")]
            player: Player::new(config),
        };

        if app.api.logged_in() {
            app.following_state = Load::Loading;
            app.spawn(|api| ApiEvent::Me(twitch_auth::me(api)));
        } else {
            // Anonymous: read-only chat, start on the search tab.
            app.tab = Tab::Search;
            #[cfg(target_os = "linux")]
            app.start_chat(None);
        }
        if let Some(channel) = &config.autoplay {
            app.activate(channel);
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

    pub fn notify(&mut self, text: impl Into<String>, level: Level) {
        let secs = if level == Level::Error { 6 } else { 3 };
        self.toast = Some(Toast { text: text.into(), level, until: Instant::now() + Duration::from_secs(secs) });
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

    fn selected_login(&self) -> Option<String> {
        self.visible().get(self.selected_index()).map(|c| c.login.clone())
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
            ApiEvent::Following(twitch_channels::followed(api).map(|mut channels| {
                sort_channels(&mut channels);
                channels
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

    /// Enter on a channel: play it, or in channels-only mode open it in the
    /// browser.
    fn activate(&mut self, login: &str) {
        #[cfg(target_os = "linux")]
        if !self.channels_only {
            self.play(login);
            return;
        }
        self.open_in_browser(login);
    }

    /// The playing channel, or in channels-only mode the selected one.
    fn open_current_in_browser(&mut self) {
        #[cfg(target_os = "linux")]
        let login = if self.channels_only { self.selected_login() } else { self.player.current_login() };
        #[cfg(not(target_os = "linux"))]
        let login = self.selected_login();
        if let Some(login) = login {
            self.open_in_browser(&login);
        }
    }

    fn open_in_browser(&mut self, login: &str) {
        let url = format!("https://www.twitch.tv/{login}");
        match crate::util::open_url(&url) {
            Ok(()) => self.notify(format!("Opened {url}"), Level::Info),
            Err(e) => self.notify(format!("Cannot open the browser: {e}"), Level::Error),
        }
    }

    // ------------------------------------------------------------ events

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.on_key(key),
            Event::Api(e) => self.on_api(e),
            #[cfg(target_os = "linux")]
            Event::Player(e) => self.on_player_event(e),
        }
    }

    fn on_api(&mut self, event: ApiEvent) {
        match event {
            ApiEvent::Me(Ok(me)) => {
                self.notify(format!("Logged in as {}", me.display_name), Level::Success);
                self.auth = Auth::LoggedIn(me);
                #[cfg(target_os = "linux")]
                self.player_logged_in();
                self.refresh_following();
            }
            ApiEvent::Me(Err(e)) => {
                self.notify(format!("Login failed: {e}"), Level::Error);
                self.auth = Auth::Failed(e.clone());
                self.following_state = Load::Failed(e);
                self.api = self.api.without_helix();
                self.tab = Tab::Search;
                #[cfg(target_os = "linux")]
                self.start_chat(None);
            }
            ApiEvent::Following(Ok(channels)) => {
                // The order changes between refreshes: keep the cursor on its channel.
                let slot = Tab::Following as usize;
                let kept = self.visible_following().get(self.selected[slot]).map(|c| c.login.clone());
                self.following = channels;
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
            #[cfg(target_os = "linux")]
            ApiEvent::Player(e) => self.on_player_api(e),
            ApiEvent::Crashed(reason) => {
                self.notify(format!("Request failed: {reason}"), Level::Error);
                if self.auth == Auth::Checking {
                    self.auth = Auth::Failed(reason.clone());
                }
                if self.following_state == Load::Loading {
                    self.following_state = Load::Failed(reason.clone());
                }
                #[cfg(target_os = "linux")]
                self.player_request_crashed(reason);
            }
        }
    }

    /// Timers of the lists. The player has its own, `tick_player`.
    pub fn tick(&mut self) {
        if self.toast.as_ref().is_some_and(|t| Instant::now() >= t.until) {
            self.toast = None;
        }
        if self.search_due.is_some_and(|due| Instant::now() >= due) {
            self.run_search();
        }
        if self.me().is_some() && self.last_following.elapsed() >= FOLLOWING_REFRESH {
            self.refresh_following();
        }
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
        #[cfg(target_os = "linux")]
        if !self.channels_only && self.on_player_key(&key) {
            return;
        }

        match key {
            Key::Char('q') => self.quit = true,
            Key::Char('?') => self.show_help = true,
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
            Key::Char('o') => self.open_current_in_browser(),
            Key::Char('r') => {
                self.refresh_following();
                #[cfg(target_os = "linux")]
                self.refresh_details();
                self.notify("Refreshing…", Level::Info);
            }
            Key::Esc if !self.filter.is_empty() => self.filter.clear(),
            _ => match self.focus {
                Focus::Sidebar => self.on_sidebar_key(key),
                #[cfg(target_os = "linux")]
                Focus::Chat => self.on_chat_key(key),
            },
        }
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
            Key::Enter => match self.selected_login() {
                Some(login) => self.activate(&login),
                None if self.tab == Tab::Search => self.typing = Some(Typing::Search),
                None => {}
            },
            _ => {}
        }
    }

    fn on_typing_key(&mut self, typing: Typing, key: Key) {
        match (typing, &key) {
            (_, Key::Esc) => self.typing = None,
            #[cfg(target_os = "linux")]
            (Typing::Chat, _) => self.on_chat_typing_key(key),
            (Typing::Search, Key::Enter) => {
                self.run_search();
                self.typing = None;
            }
            (Typing::Filter, Key::Enter) => self.typing = None,
            (Typing::Search | Typing::Filter, Key::Up | Key::Down) => {
                self.typing = None;
                self.on_sidebar_key(key);
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
