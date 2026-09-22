//! Terminal handling without external crates: raw mode through libc FFI,
//! a double-buffered cell grid with diffed truecolor output, and key parsing.

pub mod line_edit;

use std::io::{self, Write};
use std::os::raw::{c_int, c_ulong};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------- raw mode

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

#[repr(C)]
#[derive(Default)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
struct SigAction {
    handler: usize,
    mask: [u64; 16],
    flags: c_int,
    restorer: usize,
}

unsafe extern "C" {
    fn sigaction(signal: c_int, action: *const SigAction, old: *mut SigAction) -> c_int;
    fn tcgetattr(fd: c_int, termios: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *const Termios) -> c_int;
    fn cfmakeraw(termios: *mut Termios);
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

const TCSANOW: c_int = 0;
const TIOCGWINSZ: c_ulong = 0x5413;
const VTIME: usize = 5;
const VMIN: usize = 6;

static ORIGINAL: Mutex<Option<Termios>> = Mutex::new(None);
static QUIT: AtomicBool = AtomicBool::new(false);

const SIGHUP: c_int = 1;
const SIGTERM: c_int = 15;

extern "C" fn on_signal(_signal: c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

/// Asks for a clean exit when the session goes away (`kill`, window closed),
/// so the terminal is restored and the decoders are stopped.
pub fn catch_termination() {
    let action = SigAction {
        handler: on_signal as extern "C" fn(c_int) as usize,
        mask: [0; 16],
        flags: 0,
        restorer: 0,
    };
    for signal in [SIGHUP, SIGTERM] {
        unsafe { sigaction(signal, &action, std::ptr::null_mut()) };
    }
}

/// True once a termination signal has been received.
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}

const ENTER_SEQ: &str = "\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[H\x1b[2J";
const LEAVE_SEQ: &str = "\x1b[0m\x1b[?2004l\x1b[?25h\x1b[?1049l";

pub fn enter() -> io::Result<()> {
    let mut termios = unsafe { std::mem::zeroed::<Termios>() };
    if unsafe { tcgetattr(0, &mut termios) } != 0 {
        return Err(io::Error::other("stdin is not a terminal"));
    }
    *ORIGINAL.lock().unwrap() = Some(termios);
    let mut raw = termios;
    unsafe { cfmakeraw(&mut raw) };
    // Reads return after 100ms even without input so a lone Esc is detected.
    raw.c_cc[VMIN] = 0;
    raw.c_cc[VTIME] = 1;
    unsafe { tcsetattr(0, TCSANOW, &raw) };

    let mut out = io::stdout();
    out.write_all(ENTER_SEQ.as_bytes())?;
    out.flush()
}

pub fn leave() {
    if let Some(original) = ORIGINAL.lock().map(|mut o| o.take()).ok().flatten() {
        unsafe { tcsetattr(0, TCSANOW, &original) };
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE_SEQ.as_bytes());
        let _ = out.flush();
    }
}

pub fn size() -> (u16, u16) {
    let mut ws = Winsize::default();
    if unsafe { ioctl(1, TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// Size of one cell in pixels, when the terminal reports it.
pub fn cell_pixels() -> Option<(u16, u16)> {
    let mut ws = Winsize::default();
    let ok = unsafe { ioctl(1, TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_col > 0 && ws.ws_row > 0 && ws.ws_xpixel > 0 && ws.ws_ypixel > 0)
        .then(|| (ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

// ---------------------------------------------------------------- colors & cells

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn lerp(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(mix(self.0, other.0), mix(self.1, other.1), mix(self.2, other.2))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Style {
    pub fn new(fg: Rgb, bg: Rgb) -> Style {
        Style { fg, bg, bold: false, italic: false, underline: false }
    }
    pub fn fg(mut self, fg: Rgb) -> Style {
        self.fg = fg;
        self
    }
    pub fn bg(mut self, bg: Rgb) -> Style {
        self.bg = bg;
        self
    }
    pub fn bold(mut self) -> Style {
        self.bold = true;
        self
    }
    pub fn italic(mut self) -> Style {
        self.italic = true;
        self
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Cell {
    ch: char,
    style: Style,
}

/// Marks the right half of a double-width character.
const WIDE_TAIL: char = '\0';

/// Approximate terminal column width of a character (0, 1 or 2).
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    let zero = matches!(cp,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200B..=0x200F | 0x2028..=0x202E
        | 0x2060..=0x206F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
        | 0xFEFF | 0xE0000..=0xE01EF | 0x1F3FB..=0x1F3FF);
    if zero {
        return 0;
    }
    let wide = matches!(cp,
        0x1100..=0x115F | 0x231A..=0x231B | 0x2329..=0x232A | 0x23E9..=0x23EC
        | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653
        | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5
        | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E
        | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF
        | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x2E80..=0x303E | 0x3041..=0x33FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F004 | 0x1F0CF | 0x1F18E
        | 0x1F191..=0x1F19A | 0x1F200..=0x1F2FF | 0x1F300..=0x1F64F
        | 0x1F680..=0x1F6FF | 0x1F7E0..=0x1F7EB | 0x1F90C..=0x1F9FF
        | 0x1FA70..=0x1FAFF | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Truncates `s` to `max` columns, adding an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------- screen

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(self) -> u16 {
        self.x + self.w
    }
    pub fn bottom(self) -> u16 {
        self.y + self.h
    }
}

pub struct Screen {
    pub w: u16,
    pub h: u16,
    cells: Vec<Cell>,
    prev: Vec<Cell>,
    force_redraw: bool,
    pub cursor: Option<(u16, u16)>,
}

impl Default for Screen {
    fn default() -> Screen {
        Screen::new()
    }
}

impl Screen {
    pub fn new() -> Screen {
        Screen { w: 0, h: 0, cells: Vec::new(), prev: Vec::new(), force_redraw: true, cursor: None }
    }

    /// Resizes if needed and clears the back buffer with `style`.
    pub fn begin(&mut self, w: u16, h: u16, style: Style) {
        if w != self.w || h != self.h {
            self.w = w;
            self.h = h;
            self.prev = Vec::new();
            self.force_redraw = true;
        }
        let blank = Cell { ch: ' ', style };
        self.cells.clear();
        self.cells.resize(w as usize * h as usize, blank);
        self.cursor = None;
    }

    pub fn fill(&mut self, r: Rect, style: Style) {
        for y in r.y..r.bottom().min(self.h) {
            for x in r.x..r.right().min(self.w) {
                self.cells[y as usize * self.w as usize + x as usize] = Cell { ch: ' ', style };
            }
        }
    }

    pub fn set(&mut self, x: u16, y: u16, ch: char, style: Style) {
        if x < self.w && y < self.h {
            self.cells[y as usize * self.w as usize + x as usize] = Cell { ch, style };
        }
    }

    /// Sets only the foreground/char of a cell, keeping its background.
    pub fn put(&mut self, x: u16, y: u16, ch: char, fg: Rgb) {
        if x < self.w && y < self.h {
            let cell = &mut self.cells[y as usize * self.w as usize + x as usize];
            cell.ch = ch;
            cell.style.fg = fg;
        }
    }

    /// Prints `text` clipped to `max` columns. Returns the columns used.
    pub fn print(&mut self, x: u16, y: u16, text: &str, style: Style, max: u16) -> u16 {
        let limit = (x as usize + max as usize).min(self.w as usize);
        let mut cx = x as usize;
        for c in text.chars() {
            let cw = char_width(c);
            if cw == 0 {
                continue;
            }
            if cx + cw > limit {
                break;
            }
            self.set(cx as u16, y, c, style);
            if cw == 2 {
                self.set(cx as u16 + 1, y, WIDE_TAIL, style);
            }
            cx += cw;
        }
        (cx - x as usize) as u16
    }

    /// Writes the difference with the previous frame to the terminal.
    pub fn flush(&mut self, out: &mut impl Write) -> io::Result<()> {
        let mut buf = String::with_capacity(16 * 1024);
        buf.push_str("\x1b[?25l");
        if self.force_redraw {
            buf.push_str("\x1b[0m\x1b[2J");
        }
        let mut last_style: Option<Style> = None;
        let mut cursor_at: Option<(usize, usize)> = None;
        let w = self.w as usize;

        for y in 0..self.h as usize {
            for x in 0..w {
                let i = y * w + x;
                let cell = self.cells[i];
                if !self.force_redraw && self.prev.get(i) == Some(&cell) {
                    continue;
                }
                if cell.ch == WIDE_TAIL {
                    // Drawn along with its head; if the head didn't change
                    // there is nothing to do.
                    continue;
                }
                if cursor_at != Some((x, y)) {
                    buf.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));
                }
                if last_style != Some(cell.style) {
                    push_sgr(&mut buf, cell.style, last_style);
                    last_style = Some(cell.style);
                }
                buf.push(cell.ch);
                // After a wide glyph the terminal's idea of the cursor might
                // differ from ours, so always reposition explicitly.
                cursor_at = if char_width(cell.ch) == 2 { None } else { Some((x + 1, y)) };
            }
        }
        if let Some((x, y)) = self.cursor {
            buf.push_str(&format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1));
        }
        out.write_all(buf.as_bytes())?;
        out.flush()?;
        std::mem::swap(&mut self.prev, &mut self.cells);
        self.force_redraw = false;
        Ok(())
    }

    pub fn invalidate(&mut self) {
        self.force_redraw = true;
    }
}

/// Writes the escape sequence moving from `previous` to `style`. Changing
/// only the foreground is by far the most common case (the ASCII picture
/// recolours every cell), and is worth the shorter sequence.
fn push_sgr(buf: &mut String, style: Style, previous: Option<Style>) {
    if let Some(previous) = previous
        && (Style { fg: style.fg, ..previous }) == style
    {
        buf.push_str(&format!("\x1b[38;2;{};{};{}m", style.fg.0, style.fg.1, style.fg.2));
        return;
    }
    buf.push_str(&format!(
        "\x1b[0;38;2;{};{};{};48;2;{};{};{}",
        style.fg.0, style.fg.1, style.fg.2, style.bg.0, style.bg.1, style.bg.2
    ));
    if style.bold {
        buf.push_str(";1");
    }
    if style.italic {
        buf.push_str(";3");
    }
    if style.underline {
        buf.push_str(";4");
    }
    buf.push('m');
}

// ---------------------------------------------------------------- input

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Paste(String),
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Incremental decoder from raw terminal bytes to keys.
#[derive(Default)]
pub struct KeyParser {
    pending: Vec<u8>,
    paste: Option<Vec<u8>>,
}

const PASTE_END: &[u8] = b"\x1b[201~";

impl KeyParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut i = 0;
        let data = std::mem::take(&mut self.pending);

        while i < data.len() {
            if let Some(paste) = self.paste.as_mut() {
                let rest = &data[i..];
                if let Some(pos) = rest.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
                    paste.extend_from_slice(&rest[..pos]);
                    let text = String::from_utf8_lossy(paste).into_owned();
                    keys.push(Key::Paste(text));
                    self.paste = None;
                    i += pos + PASTE_END.len();
                    continue;
                }
                paste.extend_from_slice(rest);
                break;
            }

            let b = data[i];
            match b {
                0x1b => {
                    let (key, used) = parse_escape(&data[i..]);
                    if key == Some(Key::Paste(String::new())) {
                        self.paste = Some(Vec::new());
                    } else if let Some(k) = key {
                        keys.push(k);
                    }
                    i += used;
                }
                b'\r' | b'\n' => {
                    keys.push(Key::Enter);
                    i += 1;
                }
                0x7f | 0x08 => {
                    keys.push(Key::Backspace);
                    i += 1;
                }
                b'\t' => {
                    keys.push(Key::Tab);
                    i += 1;
                }
                0x00 => {
                    keys.push(Key::Ctrl(' '));
                    i += 1;
                }
                0x01..=0x1a => {
                    keys.push(Key::Ctrl((b - 1 + b'a') as char));
                    i += 1;
                }
                _ => {
                    let len = utf8_len(b);
                    if i + len > data.len() {
                        // Incomplete multi-byte sequence: wait for more input.
                        self.pending = data[i..].to_vec();
                        return keys;
                    }
                    if let Ok(s) = std::str::from_utf8(&data[i..i + len]) {
                        keys.extend(s.chars().map(Key::Char));
                    }
                    i += len;
                }
            }
        }
        keys
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// Parses an escape sequence at the start of `data`. Returns the key (an
/// empty `Paste` marks the start of a bracketed paste) and bytes consumed.
fn parse_escape(data: &[u8]) -> (Option<Key>, usize) {
    match data.get(1) {
        None => (Some(Key::Esc), 1),
        Some(b'[') => {
            let mut end = 2;
            while end < data.len() && !(0x40..=0x7e).contains(&data[end]) {
                end += 1;
            }
            if end >= data.len() {
                return (None, data.len());
            }
            let params = std::str::from_utf8(&data[2..end]).unwrap_or("");
            let first: u32 = params.split(';').next().and_then(|p| p.parse().ok()).unwrap_or(0);
            let key = match data[end] {
                b'A' => Some(Key::Up),
                b'B' => Some(Key::Down),
                b'C' => Some(Key::Right),
                b'D' => Some(Key::Left),
                b'H' => Some(Key::Home),
                b'F' => Some(Key::End),
                b'Z' => Some(Key::BackTab),
                b'~' => match first {
                    1 | 7 => Some(Key::Home),
                    4 | 8 => Some(Key::End),
                    3 => Some(Key::Delete),
                    5 => Some(Key::PageUp),
                    6 => Some(Key::PageDown),
                    200 => Some(Key::Paste(String::new())),
                    _ => None,
                },
                _ => None,
            };
            (key, end + 1)
        }
        Some(b'O') => {
            let key = match data.get(2) {
                Some(b'A') => Some(Key::Up),
                Some(b'B') => Some(Key::Down),
                Some(b'C') => Some(Key::Right),
                Some(b'D') => Some(Key::Left),
                Some(b'H') => Some(Key::Home),
                Some(b'F') => Some(Key::End),
                _ => None,
            };
            (key, 3.min(data.len()))
        }
        Some(0x1b) => (Some(Key::Esc), 1),
        Some(&b) if b.is_ascii_graphic() || b == b' ' => (Some(Key::Alt(b as char)), 2),
        Some(0x7f) => (Some(Key::Alt('\x7f')), 2),
        Some(_) => (Some(Key::Esc), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys() {
        let mut p = KeyParser::default();
        assert_eq!(
            p.feed(b"a\x1b[A\x1b[5~\r\x7f\x03\x1b"),
            vec![
                Key::Char('a'),
                Key::Up,
                Key::PageUp,
                Key::Enter,
                Key::Backspace,
                Key::Ctrl('c'),
                Key::Esc
            ]
        );
    }

    #[test]
    fn parses_split_utf8_and_paste() {
        let mut p = KeyParser::default();
        let bytes = "é".as_bytes();
        assert!(p.feed(&bytes[..1]).is_empty());
        assert_eq!(p.feed(&bytes[1..]), vec![Key::Char('é')]);
        assert_eq!(p.feed(b"\x1b[200~hi\nthere"), vec![]);
        assert_eq!(p.feed(b"!\x1b[201~x"), vec![Key::Paste("hi\nthere!".into()), Key::Char('x')]);
    }

    #[test]
    fn widths() {
        assert_eq!(str_width("abc"), 3);
        assert_eq!(str_width("日本"), 4);
        assert_eq!(truncate("hello world", 6), "hello…");
    }
}
