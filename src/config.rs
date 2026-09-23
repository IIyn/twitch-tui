//! Configuration file (`key = value` lines) and command line.

use std::path::PathBuf;

use twitch_auth::{cookies, session};
use twitch_core::helix::Session;
#[cfg(target_os = "linux")]
use twitch_playlist::Quality;

#[cfg(target_os = "linux")]
use crate::viz::VizStyle;

/// Where the website token came from, for display in the interface.
#[derive(Clone, PartialEq, Eq)]
pub enum TokenSource {
    Config,
    Environment,
    Firefox { browser: String, profile: String },
    /// No token: the reason is shown in the help screen.
    None(String),
}

impl TokenSource {
    pub fn label(&self) -> String {
        match self {
            TokenSource::Config => "token from the config file".into(),
            TokenSource::Environment => "token from TWITCH_TOKEN".into(),
            TokenSource::Firefox { browser, profile } => format!("{browser} cookie ({profile})"),
            TokenSource::None(reason) => reason.clone(),
        }
    }
}

/// How the picture is drawn.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoOutput {
    /// Real picture when the terminal supports it, else ASCII.
    Auto,
    Ascii,
    /// Real picture, even when the terminal was not recognised.
    Graphics,
}

pub struct Config {
    /// The Twitch account, from `--login`.
    pub session: Option<Session>,
    /// The twitch.tv website's own token: optional, it only brings the
    /// account's subscriber perks to playback and lets follow be attempted.
    pub token: Option<String>,
    pub token_source: TokenSource,
    /// Frames per second for the picture.
    #[cfg(target_os = "linux")]
    pub video_fps: u32,
    #[cfg(target_os = "linux")]
    pub video_output: VideoOutput,
    #[cfg(target_os = "linux")]
    pub video_quality: Quality,
    #[cfg(target_os = "linux")]
    pub volume: u32,
    #[cfg(target_os = "linux")]
    pub visualizer: VizStyle,
    pub autoplay: Option<String>,
    /// Only the channel lists: no playback nor chat.
    pub channels_only: bool,
}

const LOGIN_TEMPLATE: &str = "\
# twitch-tui configuration
#
# Log in to your Twitch account (followed channels, chat) by running
# `twitch-tui --login` once.
#
# Besides that, the app reads the twitch.tv session cookie of your Firefox
# profile when it finds one. It is optional: it brings your subscriber
# perks (no ads, subscriber-only streams) to playback, and lets the app
# try to follow/unfollow channels.
#
# Set this to false to never read the browser cookies.
# firefox_login = true
#
# Only look in this profile instead of searching for one.
# firefox_profile = ~/.mozilla/firefox/xxxxxxxx.default-release
#
# Token to use instead of the browser cookie (the `auth-token` cookie value).
# Keep this file private.
# token = your_auth_token
";

/// Settings of the player, which only exists on Linux.
#[cfg(target_os = "linux")]
const PLAYER_TEMPLATE: &str = "
# Initial volume, 0-150.
volume = 80

# Frames per second of the picture, 5 to 60. Lower it on a slow terminal
# or over SSH: each ASCII frame repaints the whole panel.
video_fps = 30

# How the picture is drawn: auto (real picture in kitty or Ghostty, ASCII
# elsewhere), ascii, or graphics (force the kitty graphics protocol).
# The `a` key switches between the two while running.
video_output = auto

# Rendition of the stream to decode: auto (480p, plenty for a terminal), a
# height such as 360p, 720p or 1080p (the tallest one up to it), or source.
# Taller costs more CPU and bandwidth. The `c` key cycles through them.
video_quality = auto

# Visualizer style: spectrum, mirror, scope or video (the stream picture).
visualizer = spectrum
";

pub fn path() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = if cfg!(windows) {
        var("APPDATA")
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("twitch-tui").join("config")
}

/// Writes the commented template on first run, readable by the user only.
fn create_template(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = twitch_core::create_private(path) {
        use std::io::Write;
        let _ = file.write_all(LOGIN_TEMPLATE.as_bytes());
        #[cfg(target_os = "linux")]
        let _ = file.write_all(PLAYER_TEMPLATE.as_bytes());
    }
}

pub fn load(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        session: None,
        token: None,
        token_source: TokenSource::None(String::new()),
        #[cfg(target_os = "linux")]
        video_fps: video::DEFAULT_FPS,
        #[cfg(target_os = "linux")]
        video_output: VideoOutput::Auto,
        #[cfg(target_os = "linux")]
        video_quality: Quality::Auto,
        #[cfg(target_os = "linux")]
        volume: 80,
        #[cfg(target_os = "linux")]
        visualizer: VizStyle::Bars,
        autoplay: None,
        channels_only: false,
    };
    let mut firefox_login = true;
    let mut firefox_profile: Option<PathBuf> = None;
    let mut anonymous = false;

    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| format!("{}:{}: expected `key = value`", path.display(), n + 1))?;
                let value = value.trim().trim_matches('"');
                match key.trim() {
                    "token" => config.token = Some(value.to_string()).filter(|v| !v.is_empty()),
                    // Settings of the player, which does not exist here.
                    #[cfg(not(target_os = "linux"))]
                    "volume" | "visualizer" | "video_fps" | "video_output" | "video_quality" => {}
                    #[cfg(target_os = "linux")]
                    "volume" => config.volume = value.parse().map_err(|_| format!("invalid volume: {value}"))?,
                    #[cfg(target_os = "linux")]
                    "visualizer" => config.visualizer = parse_viz(value)?,
                    #[cfg(target_os = "linux")]
                    "video_fps" => {
                        let fps: u32 = value.parse().map_err(|_| format!("invalid video_fps: {value}"))?;
                        config.video_fps = fps.clamp(5, 60);
                    }
                    #[cfg(target_os = "linux")]
                    "video_output" => {
                        config.video_output = match value {
                            "auto" => VideoOutput::Auto,
                            "ascii" => VideoOutput::Ascii,
                            "graphics" | "kitty" => VideoOutput::Graphics,
                            _ => return Err(format!("unknown video_output `{value}` (auto, ascii, graphics)")),
                        }
                    }
                    #[cfg(target_os = "linux")]
                    "video_quality" => {
                        config.video_quality = Quality::parse(value).ok_or_else(|| {
                            format!("unknown video_quality `{value}` (auto, 360p, 720p, 1080p, source…)")
                        })?
                    }
                    "firefox_login" => firefox_login = value != "false",
                    "firefox_profile" => firefox_profile = Some(expand_home(value)),
                    other => return Err(format!("{}:{}: unknown key `{other}`", path.display(), n + 1)),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_template(&path),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    }

    if config.token.is_some() {
        config.token_source = TokenSource::Config;
    }
    if let Some(token) = std::env::var("TWITCH_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        config.token = Some(token);
        config.token_source = TokenSource::Environment;
    }

    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-v" | "--volume" => {
                let v = iter.next().ok_or("--volume needs a value")?;
                let volume: u32 = v.parse().map_err(|_| format!("invalid volume: {v}"))?;
                #[cfg(target_os = "linux")]
                {
                    config.volume = volume.min(150);
                }
                #[cfg(not(target_os = "linux"))]
                let _ = volume;
            }
            "--anonymous" => anonymous = true,
            "--channels-only" | "--compatibility" => config.channels_only = true,
            "--check-login" | "--login" | "--logout" => {}
            "--profile" => {
                let path = iter.next().ok_or("--profile needs a directory")?;
                firefox_profile = Some(expand_home(path));
            }
            a if a.starts_with('-') => return Err(format!("unknown option {a} (see --help)")),
            channel => {
                let channel = channel.rsplit('/').next().unwrap_or(channel);
                config.autoplay = Some(channel.to_string());
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        config.volume = config.volume.min(150);
    }
    // The player and the chat rely on Linux (audio players, shared memory,
    // pipes handed to ffmpeg): elsewhere only the channel lists run.
    if !cfg!(target_os = "linux") {
        config.channels_only = true;
    }
    if !anonymous {
        config.session = session::load();
    }

    // Without an explicit token, use the browser's Twitch cookie.
    if anonymous {
        config.token = None;
        config.token_source = TokenSource::None("started with --anonymous".into());
    } else if config.token.is_none() {
        if config.channels_only {
            // Only playback and following use it.
            config.token_source = TokenSource::None("not needed in channels-only mode".into());
        } else if firefox_login {
            match cookies::find_token(firefox_profile.as_deref()) {
                Ok(found) => {
                    config.token = Some(found.token);
                    config.token_source =
                        TokenSource::Firefox { browser: found.browser.to_string(), profile: found.profile };
                }
                Err(reason) => config.token_source = TokenSource::None(reason),
            }
        } else {
            config.token_source = TokenSource::None("firefox_login is off in the config file".into());
        }
    }
    Ok(config)
}

fn expand_home(value: &str) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(value),
    }
}

#[cfg(target_os = "linux")]
fn parse_viz(value: &str) -> Result<VizStyle, String> {
    match value {
        "spectrum" | "bars" => Ok(VizStyle::Bars),
        "mirror" => Ok(VizStyle::Mirror),
        "scope" | "wave" => Ok(VizStyle::Wave),
        "video" | "ascii" => Ok(VizStyle::Video),
        _ => Err(format!("unknown visualizer `{value}` (spectrum, mirror, scope, video)")),
    }
}

pub fn usage() -> String {
    format!(
        "twitch-tui: listen to Twitch streams (audio only) and chat from your terminal\n\n\
         USAGE:\n    twitch-tui [OPTIONS] [CHANNEL]\n\n\
         ARGS:\n    CHANNEL            channel login or URL to start playing\n\n\
         OPTIONS:\n    -v, --volume N     initial volume (0-150)\n        \
         --login        log in to your Twitch account and exit\n        \
         --logout       log out of your Twitch account and exit\n        \
         --profile DIR  Firefox profile to read the Twitch cookie from\n        \
         --anonymous    start logged out\n        \
         --channels-only  only the followed channels and search, no playback\n                       \
         nor chat (alias: --compatibility)\n        \
         --check-login  report the account and the website token, then exit\n    \
         -h, --help         show this help\n\n\
         LOGIN:\n    Run `twitch-tui --login` once and approve the code on twitch.tv.\n    \
         The Twitch cookie of your Firefox profile, when found, adds your\n    \
         subscriber perks to playback.\n\n\
         SESSION:\n    {}\n\n\
         CONFIG:\n    {}\n    TWITCH_TOKEN environment variable overrides the website token\n\n\
         REQUIRES:\n    curl, ffmpeg and one of pw-cat / pacat / aplay\n",
        session::path().display(),
        path().display()
    )
}
