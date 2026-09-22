//! Terminal video: `ffmpeg` decodes a small rendition of the stream into raw
//! RGB frames sized to the panel. They are drawn either as coloured ASCII, or
//! as a real picture through the kitty graphics protocol (by the application).
//!
//! The same ffmpeg also decodes the sound, on a second pipe handed to the
//! audio player: both come from one rendition and one timeline, and each
//! frame is released only once the sound has reached it, so they stay in sync.
//!
//! Events go back through an `mpsc::Sender` of the caller's own type, which
//! only has to be buildable from this crate's events.

use std::io::{self, PipeReader, Read};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}
const F_SETFD: c_int = 2;
const F_SETPIPE_SZ: c_int = 1031;
/// Room for decoded frames waiting for the sound to catch up, so ffmpeg can
/// keep feeding the audio meanwhile.
const FRAME_PIPE: c_int = 1 << 20;
/// A frame waits for the sound only while the sound moves: if it stalls this
/// long, ffmpeg may be blocked on us and the frame goes out anyway.
const STALL: Duration = Duration::from_millis(150);

/// Frames per second when the config does not say otherwise.
pub const DEFAULT_FPS: u32 = 30;
/// Samples per cell: two across, four down, so the renderer can see the
/// structure inside a character instead of one average colour.
pub const SAMPLES_X: u16 = 2;
pub const SAMPLES_Y: u16 = 4;

/// What the decoder produces: the picture fitted, with padding, into
/// `width` x `height` pixels that stand for `cols` x `rows` cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Target {
    pub cols: u16,
    pub rows: u16,
    pub width: u32,
    pub height: u32,
}

impl Target {
    /// `SAMPLES_X` x `SAMPLES_Y` samples per cell, for the ASCII picture.
    pub fn ascii(cols: u16, rows: u16) -> Target {
        Target { cols, rows, width: (cols * SAMPLES_X) as u32, height: (rows * SAMPLES_Y) as u32 }
    }

    /// The panel's real size in pixels, capped at the size of the source
    /// (`max`): the terminal scales it up on the GPU, so going beyond would
    /// only cost memory.
    pub fn pixels(cols: u16, rows: u16, (cell_w, cell_h): (u16, u16), max: (u32, u32)) -> Target {
        let (w, h) = (cols as f32 * cell_w as f32, rows as f32 * cell_h as f32);
        let (max_w, max_h) = (max.0.max(64) as f32, max.1.max(64) as f32);
        let scale = (max_w / w).min(max_h / h).min(1.0);
        // Even sizes keep ffmpeg's scaler happy.
        let even = |v: f32| ((v * scale) as u32 & !1).max(2);
        Target { cols, rows, width: even(w), height: even(h) }
    }
}

/// Sent when the decoder stops on its own, with the reason.
pub enum VideoEvent {
    Stopped(String),
}

/// The sound of a running decoder, for the audio player.
pub struct Feed {
    /// PCM in the `audio::PCM_ARGS` format.
    pub pcm: PipeReader,
    /// Where the player stores the stream time being heard, in microseconds,
    /// which paces the pictures.
    pub heard: Arc<AtomicU64>,
}

pub struct Frame {
    pub target: Target,
    /// Number of the frame since the decoder started, to spot new ones.
    pub seq: u64,
    /// `target.width` x `target.height` RGB pixels, top to bottom.
    pub pixels: Vec<u8>,
}

impl Frame {
    /// One pixel of the picture.
    pub fn sample(&self, x: u16, y: u16) -> [u8; 3] {
        let index = (y as usize * self.target.width as usize + x as usize) * 3;
        match self.pixels.get(index..index + 3) {
            Some(p) => [p[0], p[1], p[2]],
            None => [0, 0, 0],
        }
    }
}

pub struct Video {
    frame: Arc<Mutex<Option<Frame>>>,
    /// Frames decoded since start, to report the rate actually achieved.
    decoded: Arc<AtomicU64>,
    session: Option<Session>,
    /// Rendition and output the running decoder was started for.
    started: Option<(String, Target)>,
    pub fps: u32,
    /// Color of the bars padding the picture to the panel's shape.
    pub background: [u8; 3],
}

struct Session {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Session {
    fn kill(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Default for Video {
    fn default() -> Video {
        Video::new()
    }
}

impl Video {
    pub fn new() -> Video {
        Video {
            frame: Arc::new(Mutex::new(None)),
            decoded: Arc::new(AtomicU64::new(0)),
            session: None,
            started: None,
            fps: DEFAULT_FPS,
            background: [0, 0, 0],
        }
    }

    pub fn decoded(&self) -> u64 {
        self.decoded.load(Ordering::Relaxed)
    }

    pub fn is_running(&self) -> bool {
        self.session.is_some()
    }

    /// True when the decoder runs a rendition or an output no longer wanted.
    pub fn needs_restart(&self, url: &str, target: Target) -> bool {
        self.is_running() && self.started.as_ref().is_none_or(|(u, t)| u != url || *t != target)
    }

    pub fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            session.kill();
        }
        if let Ok(mut frame) = self.frame.lock() {
            *frame = None;
        }
    }

    /// Reads the latest frame, if one has been decoded yet.
    pub fn with_frame<T>(&self, f: impl FnOnce(&Frame) -> T) -> Option<T> {
        self.frame.lock().ok()?.as_ref().map(f)
    }

    /// Starts decoding `url`, returning its sound, which must be played for
    /// the pictures to move on.
    pub fn start<E: From<VideoEvent> + Send + 'static>(&mut self, url: &str, target: Target, tx: Sender<E>) -> Option<Feed> {
        self.stop();
        if target.cols < 8 || target.rows < 4 {
            return None;
        }
        self.started = Some((url.to_string(), target));
        let (ffmpeg, output, pcm) = match spawn(url, target, self.fps, self.background) {
            Ok(spawned) => spawned,
            Err(e) => {
                let _ = tx.send(VideoEvent::Stopped(e).into());
                return None;
            }
        };
        let session = Session {
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(Some(ffmpeg))),
        };
        let (stop, child) = (session.stop.clone(), session.child.clone());
        let heard = Arc::new(AtomicU64::new(0));
        let frame = self.frame.clone();
        let decoded = self.decoded.clone();
        let fps = self.fps;
        self.session = Some(session);

        let clock = heard.clone();
        thread::spawn(move || {
            match decode(output, target, fps, &clock, &stop, &frame, &decoded) {
                Err(e) if !stop.load(Ordering::SeqCst) => {
                    let _ = tx.send(VideoEvent::Stopped(e).into());
                }
                _ => {}
            }
            if let Ok(mut guard) = child.lock()
                && let Some(child) = guard.as_mut()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        });
        Some(Feed { pcm, heard })
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts ffmpeg with the frames on its stdout and the sound on a pipe of
/// its own.
fn spawn(url: &str, target: Target, fps: u32, [r, g, b]: [u8; 3]) -> Result<(Child, ChildStdout, PipeReader), String> {
    let (width, height) = (target.width, target.height);
    // Fit the picture inside the panel and pad the rest, so every frame has
    // exactly the same size and the aspect ratio survives. Both outputs are
    // made to start at time zero of the input (padding with repeated frames
    // or silence) so frame `n` is due when the sound reaches `n / fps`.
    let filter = format!(
        "fps=fps={fps}:start_time=0,scale={width}:{height}:force_original_aspect_ratio=decrease,\
         pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=0x{r:02x}{g:02x}{b:02x}"
    );
    let (pcm, pcm_writer) = io::pipe().map_err(|e| format!("cannot create audio pipe: {e}"))?;
    let pcm_fd = pcm_writer.as_raw_fd();
    let mut command = Command::new("ffmpeg");
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "4"])
        .args(["-i", url])
        .args(["-map", "0:v:0", "-vf", &filter, "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
        .args(["-map", "0:a:0", "-af", "aresample=async=1:first_pts=0"])
        .args(audio::PCM_ARGS)
        .arg(format!("pipe:{pcm_fd}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The pipe is created close-on-exec; let this one child inherit it.
    unsafe {
        command.pre_exec(move || match fcntl(pcm_fd, F_SETFD, 0) {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let mut ffmpeg = command.spawn().map_err(|e| format!("cannot start ffmpeg for video: {e}"))?;
    // Only ffmpeg may hold the writing end, so its exit reads as the end.
    drop(pcm_writer);
    let output = ffmpeg.stdout.take().ok_or("ffmpeg stdout unavailable")?;
    unsafe { fcntl(output.as_raw_fd(), F_SETPIPE_SZ, FRAME_PIPE) };
    Ok((ffmpeg, output, pcm))
}

fn decode(
    mut output: ChildStdout,
    target: Target,
    fps: u32,
    heard: &AtomicU64,
    stop: &AtomicBool,
    frame: &Mutex<Option<Frame>>,
    decoded: &AtomicU64,
) -> Result<(), String> {
    let mut buffer = vec![0u8; target.width as usize * target.height as usize * 3];
    let mut seq = 0;
    while !stop.load(Ordering::SeqCst) {
        if let Err(e) = output.read_exact(&mut buffer) {
            return Err(format!("video stream ended: {e}"));
        }
        wait_for_sound(seq * 1_000_000 / fps as u64, heard, stop);
        seq += 1;
        if let Ok(mut frame) = frame.lock() {
            // Reuse the previous frame's allocation.
            let pixels = match frame.take() {
                Some(mut old) if old.pixels.len() == buffer.len() => {
                    old.pixels.copy_from_slice(&buffer);
                    old.pixels
                }
                _ => buffer.clone(),
            };
            *frame = Some(Frame { target, seq, pixels });
        }
        decoded.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// Blocks until the sound being heard reaches `due` (microseconds), unless
/// the sound stops moving.
fn wait_for_sound(due: u64, heard: &AtomicU64, stop: &AtomicBool) {
    let mut last = heard.load(Ordering::Relaxed);
    let mut moved = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let now = heard.load(Ordering::Relaxed);
        if now >= due {
            return;
        }
        if now != last {
            (last, moved) = (now, Instant::now());
        } else if moved.elapsed() >= STALL {
            return;
        }
        thread::sleep(Duration::from_micros((due - now).min(5_000)));
    }
}
