//! Audio pipeline: `ffmpeg` decodes the HLS stream to raw PCM on its stdout,
//! we apply the volume, tap the samples for the visualizer and pipe them into
//! a system player (`pw-cat`, `pacat` or `aplay`).
//!
//! Events go back through an `mpsc::Sender` of the caller's own type, which
//! only has to be buildable from this crate's events.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

pub const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const TAP_LEN: usize = 8192;

unsafe extern "C" {
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}
const F_SETPIPE_SZ: c_int = 1031;

/// Events carry the id given to `Audio::play` so stale ones can be ignored.
pub enum AudioEvent {
    Playing(u64),
    Stopped(u64, Option<String>),
}

/// Sample tap shared with the visualizer (mono, pre-volume).
#[derive(Default)]
pub struct Tap {
    pub samples: VecDeque<f32>,
    pub level: f32,
}

pub struct Audio {
    pub volume: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    pub tap: Arc<Mutex<Tap>>,
    session: Option<Session>,
    backend: Option<&'static str>,
}

struct Session {
    stop: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<Child>>>,
}

impl Session {
    fn kill(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut children) = self.children.lock() {
            for child in children.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn player_command(backend: &str) -> Command {
    let rate = SAMPLE_RATE.to_string();
    let mut cmd = Command::new(backend);
    match backend {
        "pw-cat" => cmd.args(["--playback", "--raw", "--format", "s16", "--rate", &rate, "--channels", "2"])
            .args(["--latency", "60ms", "--media-role", "Music", "-"]),
        "pacat" => cmd.args(["--playback", "--format=s16le", &format!("--rate={rate}"), "--channels=2"])
            .args(["--latency-msec=60", "--client-name=twitch-tui"]),
        _ => cmd.args(["-q", "-f", "S16_LE", "-r", &rate, "-c", "2", "--buffer-time=120000", "-"]),
    };
    cmd
}

impl Audio {
    pub fn new(volume: u32) -> Audio {
        Audio {
            volume: Arc::new(AtomicU32::new(volume.min(150))),
            muted: Arc::new(AtomicBool::new(false)),
            tap: Arc::new(Mutex::new(Tap::default())),
            session: None,
            backend: ["pw-cat", "pacat", "aplay"].into_iter().find(|b| in_path(b)),
        }
    }

    pub fn backend(&self) -> &str {
        self.backend.unwrap_or("none")
    }

    pub fn is_active(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.stop.load(Ordering::SeqCst))
    }

    pub fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            session.kill();
        }
        if let Ok(mut tap) = self.tap.lock() {
            tap.samples.clear();
            tap.level = 0.0;
        }
    }

    pub fn play<E: From<AudioEvent> + Send + 'static>(&mut self, url: String, id: u64, tx: Sender<E>) {
        self.stop();
        let Some(backend) = self.backend else {
            let _ = tx.send(
                AudioEvent::Stopped(
                    id,
                    Some("no audio player found (install pipewire, pulseaudio-utils or alsa-utils)".into()),
                )
                .into(),
            );
            return;
        };
        let session = Session {
            stop: Arc::new(AtomicBool::new(false)),
            children: Arc::new(Mutex::new(Vec::new())),
        };
        let stop = session.stop.clone();
        let children = session.children.clone();
        let volume = self.volume.clone();
        let muted = self.muted.clone();
        let tap = self.tap.clone();
        self.session = Some(session);

        thread::spawn(move || {
            let result = run_pipeline(&url, id, backend, &stop, &children, &volume, &muted, &tap, &tx);
            let was_stopped = stop.swap(true, Ordering::SeqCst);
            if let Ok(mut children) = children.lock() {
                for child in children.iter_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            if !was_stopped {
                let _ = tx.send(AudioEvent::Stopped(id, result.err()).into());
            }
        });
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline<E: From<AudioEvent>>(
    url: &str,
    id: u64,
    backend: &str,
    stop: &AtomicBool,
    children: &Mutex<Vec<Child>>,
    volume: &AtomicU32,
    muted: &AtomicBool,
    tap: &Mutex<Tap>,
    tx: &Sender<E>,
) -> Result<(), String> {
    let mut decoder = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "4"])
        .args(["-i", url, "-vn", "-f", "s16le", "-ac", "2", "-ar", &SAMPLE_RATE.to_string(), "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
    let mut pcm = decoder.stdout.take().ok_or("ffmpeg stdout unavailable")?;
    let mut errors = decoder.stderr.take();
    children.lock().unwrap().push(decoder);

    // Collect ffmpeg's last error line for reporting.
    let last_error = Arc::new(Mutex::new(String::new()));
    if let Some(mut stderr) = errors.take() {
        let last_error = last_error.clone();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
                *last_error.lock().unwrap() = line.trim().to_string();
            }
        });
    }

    let mut player = player_command(backend)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {backend}: {e}"))?;
    let mut sink = player.stdin.take().ok_or("player stdin unavailable")?;
    // Keep the pipe small so the visualizer stays in sync with what we hear.
    unsafe { fcntl(sink.as_raw_fd(), F_SETPIPE_SZ, 8192 as c_int) };
    children.lock().unwrap().push(player);

    let mut buf = vec![0u8; 1024 * CHANNELS * 2];
    let mut filled = 0;
    let mut started = false;
    while !stop.load(Ordering::SeqCst) {
        let n = pcm.read(&mut buf[filled..]).map_err(|e| format!("decoder read failed: {e}"))?;
        if n == 0 {
            let err = last_error.lock().unwrap().clone();
            return Err(if err.is_empty() { "stream ended".into() } else { err });
        }
        filled += n;
        let usable = filled - filled % (CHANNELS * 2);
        if usable == 0 {
            continue;
        }

        let gain = if muted.load(Ordering::Relaxed) {
            0.0
        } else {
            let v = volume.load(Ordering::Relaxed) as f32 / 100.0;
            v * v
        };
        let mut sum_sq = 0.0f32;
        let mut mono = Vec::with_capacity(usable / 4);
        for frame in buf[..usable].as_chunks_mut::<4>().0 {
            let l = i16::from_le_bytes([frame[0], frame[1]]);
            let r = i16::from_le_bytes([frame[2], frame[3]]);
            let m = (l as f32 + r as f32) / 65536.0;
            sum_sq += m * m;
            mono.push(m);
            let scale = |s: i16| ((s as f32 * gain).clamp(-32768.0, 32767.0) as i16).to_le_bytes();
            frame[..2].copy_from_slice(&scale(l));
            frame[2..].copy_from_slice(&scale(r));
        }
        if let Ok(mut tap) = tap.lock() {
            tap.samples.extend(mono.iter());
            let excess = tap.samples.len().saturating_sub(TAP_LEN);
            tap.samples.drain(..excess);
            tap.level = (sum_sq / mono.len().max(1) as f32).sqrt();
        }

        sink.write_all(&buf[..usable]).map_err(|e| format!("{backend} stopped: {e}"))?;
        buf.copy_within(usable..filled, 0);
        filled -= usable;

        if !started {
            started = true;
            let _ = tx.send(AudioEvent::Playing(id).into());
        }
    }
    Ok(())
}
