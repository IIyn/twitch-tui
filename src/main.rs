mod app;
mod config;
mod graphics;
mod theme;
mod ui;
mod util;
mod viz;

use std::io::Read;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use app::{ApiEvent, App};
use audio::AudioEvent;
use term::{Key, KeyParser, Screen};
use twitch_chat::ChatEvent;
use video::VideoEvent;

const FRAME: Duration = Duration::from_millis(33);

pub enum Event {
    Key(Key),
    Api(ApiEvent),
    Chat(ChatEvent),
    Audio(AudioEvent),
    Video(VideoEvent),
}

impl From<ChatEvent> for Event {
    fn from(e: ChatEvent) -> Event {
        Event::Chat(e)
    }
}

impl From<AudioEvent> for Event {
    fn from(e: AudioEvent) -> Event {
        Event::Audio(e)
    }
}

impl From<VideoEvent> for Event {
    fn from(e: VideoEvent) -> Event {
        Event::Video(e)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", config::usage());
        return;
    }
    let config = match config::load(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("twitch-tui: {e}");
            std::process::exit(2);
        }
    };
    if args.iter().any(|a| a == "--login") {
        std::process::exit(login());
    }
    if args.iter().any(|a| a == "--logout") {
        std::process::exit(logout(&config));
    }
    if args.iter().any(|a| a == "--check-login") {
        check_login(&config);
        return;
    }
    let required: &[&str] = if config.channels_only { &["curl"] } else { &["curl", "ffmpeg"] };
    let missing: Vec<&str> = required.iter().copied().filter(|b| !in_path(b)).collect();
    if !missing.is_empty() {
        eprintln!("twitch-tui: missing required programs: {}", missing.join(", "));
        std::process::exit(1);
    }

    if let Err(e) = term::enter() {
        eprintln!("twitch-tui: {e}");
        std::process::exit(1);
    }
    // Only the main thread owns the terminal: a worker panicking must not
    // drop the interface out of the alternate screen.
    let main_thread = std::thread::current().id();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == main_thread {
            term::leave();
            default_hook(info);
        }
    }));

    term::catch_termination();
    run(&config);
    term::leave();
}

/// Logs in to a Twitch account with the device flow and saves the session.
fn login() -> i32 {
    let result = twitch_auth::session::login(|code| {
        println!("To log in, open this page and approve the code {}:", code.user_code);
        println!("  {}", code.verification_uri);
        let graphical = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some());
        if graphical {
            let _ = std::process::Command::new("xdg-open")
                .arg(&code.verification_uri)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        println!("Waiting for approval…");
    });
    let saved = result.and_then(|session| twitch_auth::session::save(&session).map(|_| session));
    match saved {
        Ok(session) => {
            println!("Logged in as {}.", session.login);
            0
        }
        Err(e) => {
            eprintln!("twitch-tui: login failed: {e}");
            1
        }
    }
}

fn logout(config: &config::Config) -> i32 {
    let Some(session) = &config.session else {
        println!("Not logged in.");
        return 0;
    };
    match twitch_auth::session::logout(session) {
        Ok(()) => {
            println!("Logged out of {}.", session.login);
            0
        }
        Err(e) => {
            // The session is forgotten here even when Twitch could not be told.
            eprintln!("twitch-tui: logged out, but Twitch did not revoke the token: {e}");
            1
        }
    }
}

/// Prints which account the session opens and where the website token comes
/// from. Tokens themselves are never printed.
fn check_login(config: &config::Config) {
    match &config.session {
        Some(session) => {
            let api = twitch_core::Api::new(None).with_helix(twitch_core::helix::Helix::new(
                session.clone(),
                |s| {
                    let _ = twitch_auth::session::save(s);
                },
            ));
            match twitch_auth::me(&api) {
                Ok(me) => println!("account: logged in as {} (id {})", me.display_name, me.id),
                Err(e) => println!("account: rejected by Twitch: {e}"),
            }
        }
        None => println!("account: not logged in (run twitch-tui --login)"),
    }
    println!("website token: {}", config.token_source.label());
    let Some(token) = &config.token else { return };
    match twitch_core::Api::new(Some(token.clone())).gql("query { currentUser { login } }", "{}") {
        Ok(data) => match data.at(&["currentUser", "login"]).as_str() {
            Some(login) => println!("website token: works, belongs to {login}"),
            None => println!("website token: invalid or expired"),
        },
        Err(e) => println!("website token: rejected by Twitch: {e}"),
    }
}

fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn run(config: &config::Config) {
    let (tx, rx) = mpsc::channel();

    let input_tx = tx.clone();
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut parser = KeyParser::default();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => {
                    for key in parser.feed(&buf[..n]) {
                        if input_tx.send(Event::Key(key)).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    });

    let mut app = App::new(config, tx);
    let mut screen = Screen::new();
    let mut graphics = graphics::Graphics::default();
    let mut out = std::io::stdout();
    let mut frame: u64 = 0;
    let mut last = Instant::now();

    while !app.quit && !term::quit_requested() {
        let deadline = last + FRAME;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline - now) {
                Ok(event) => {
                    app.handle(event);
                    if app.quit || term::quit_requested() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        let now = Instant::now();
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;
        frame += 1;

        let (w, h) = term::size();
        let layout = ui::Layout::compute(w, h, &app);
        app.tick(dt, layout.viz_size());
        screen.begin(w, h, theme::base());
        ui::draw(&mut screen, &app, frame);
        if screen.flush(&mut out).is_err() {
            screen.invalidate();
            graphics.forget();
        }
        let shown = match layout.picture(&app, (w, h)) {
            Some(r) => graphics.show(&mut out, &app.video, r),
            None => graphics.hide(&mut out),
        };
        if shown.is_err() {
            graphics.forget();
        }
    }
    app.audio.stop();
}
