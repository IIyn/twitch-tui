//! Raw mode, window size and termination signals through libc. The
//! structure layouts and constants differ between Linux and macOS.

use std::io;
use std::os::raw::{c_int, c_ulong};
use std::sync::Mutex;
use std::sync::atomic::Ordering;

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: c_ulong,
    c_oflag: c_ulong,
    c_cflag: c_ulong,
    c_lflag: c_ulong,
    c_cc: [u8; 20],
    c_ispeed: c_ulong,
    c_ospeed: c_ulong,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SigAction {
    handler: usize,
    mask: [u64; 16],
    flags: c_int,
    restorer: usize,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct SigAction {
    handler: usize,
    mask: u32,
    flags: c_int,
}

#[repr(C)]
#[derive(Default)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

unsafe extern "C" {
    fn sigaction(signal: c_int, action: *const SigAction, old: *mut SigAction) -> c_int;
    fn tcgetattr(fd: c_int, termios: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *const Termios) -> c_int;
    fn cfmakeraw(termios: *mut Termios);
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

const TCSANOW: c_int = 0;
#[cfg(target_os = "linux")]
const TIOCGWINSZ: c_ulong = 0x5413;
#[cfg(target_os = "linux")]
const VTIME: usize = 5;
#[cfg(target_os = "linux")]
const VMIN: usize = 6;
#[cfg(target_os = "macos")]
const TIOCGWINSZ: c_ulong = 0x4008_7468;
#[cfg(target_os = "macos")]
const VMIN: usize = 16;
#[cfg(target_os = "macos")]
const VTIME: usize = 17;

const SIGHUP: c_int = 1;
const SIGTERM: c_int = 15;

static ORIGINAL: Mutex<Option<Termios>> = Mutex::new(None);

extern "C" fn on_signal(_signal: c_int) {
    super::QUIT.store(true, Ordering::SeqCst);
}

pub fn catch_termination() {
    let mut action: SigAction = unsafe { std::mem::zeroed() };
    action.handler = on_signal as extern "C" fn(c_int) as usize;
    for signal in [SIGHUP, SIGTERM] {
        unsafe { sigaction(signal, &action, std::ptr::null_mut()) };
    }
}

pub fn enter_raw() -> io::Result<()> {
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
    Ok(())
}

/// Restores the terminal. False when raw mode was not on.
pub fn leave_raw() -> bool {
    match ORIGINAL.lock().map(|mut o| o.take()).ok().flatten() {
        Some(original) => {
            unsafe { tcsetattr(0, TCSANOW, &original) };
            true
        }
        None => false,
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
