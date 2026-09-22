//! Terminal video: `ffmpeg` decodes a small rendition of the stream into raw
//! RGB frames sized to the panel. They are drawn either as coloured ASCII, or
//! as a real picture through the kitty graphics protocol (by the application).
//!
//! Events go back through an `mpsc::Sender` of the caller's own type, which
//! only has to be buildable from this crate's events.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;


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

    pub fn start<E: From<VideoEvent> + Send + 'static>(&mut self, url: &str, target: Target, tx: Sender<E>) {
        self.stop();
        if target.cols < 8 || target.rows < 4 {
            return;
        }
        self.started = Some((url.to_string(), target));
        let session = Session {
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
        };
        let (stop, child) = (session.stop.clone(), session.child.clone());
        let frame = self.frame.clone();
        let decoded = self.decoded.clone();
        let url = url.to_string();
        let fps = self.fps;
        let background = self.background;
        self.session = Some(session);

        thread::spawn(move || {
            match decode(&url, target, fps, background, &stop, &child, &frame, &decoded) {
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
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn decode(
    url: &str,
    target: Target,
    fps: u32,
    [r, g, b]: [u8; 3],
    stop: &AtomicBool,
    child: &Mutex<Option<Child>>,
    frame: &Mutex<Option<Frame>>,
    decoded: &AtomicU64,
) -> Result<(), String> {
    let (width, height) = (target.width as usize, target.height as usize);
    // Fit the picture inside the panel and pad the rest, so every frame has
    // exactly the same size and the aspect ratio survives.
    let filter = format!(
        "fps={fps},scale={width}:{height}:force_original_aspect_ratio=decrease,\
         pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=0x{r:02x}{g:02x}{b:02x}"
    );
    let mut ffmpeg = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "4"])
        // Without this ffmpeg races through whatever the server hands it, so
        // the picture arrives in bursts: a second of fast-forward, then a
        // freeze. Reading at the native rate keeps it steady and near live.
        .args(["-re", "-i", url, "-an", "-vf", &filter])
        .args(["-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start ffmpeg for video: {e}"))?;
    let mut output = ffmpeg.stdout.take().ok_or("ffmpeg stdout unavailable")?;
    *child.lock().unwrap() = Some(ffmpeg);

    let mut buffer = vec![0u8; width * height * 3];
    let mut seq = 0;
    while !stop.load(Ordering::SeqCst) {
        if let Err(e) = output.read_exact(&mut buffer) {
            return Err(format!("video stream ended: {e}"));
        }
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
