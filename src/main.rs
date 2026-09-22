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
    if args.iter().any(|a| a == "--check-login") {
        check_login(&config);
        return;
    }
    let missing: Vec<&str> = ["curl", "ffmpeg"].into_iter().filter(|b| !in_path(b)).collect();
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

/// Prints where the session token comes from and which account it opens.
/// The token itself is never printed.
fn check_login(config: &config::Config) {
    println!("source: {}", config.token_source.label());
    let Some(token) = &config.token else {
        println!("result: not logged in");
        return;
    };
    println!("token:  found, {} characters", token.chars().count());
    match twitch_auth::me(&twitch_core::Api::new(Some(token.clone()))) {
        Ok(me) => println!("result: logged in as {} (id {})", me.display_name, me.id),
        Err(e) => println!("result: rejected by Twitch: {e}"),
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
