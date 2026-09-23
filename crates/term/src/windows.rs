//! Raw mode, window size and termination through the Windows console API.
//! The console is switched to virtual terminal mode both ways, so it reads
//! and writes the same escape sequences as a Unix terminal.

use std::ffi::c_void;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

type Handle = *mut c_void;

#[repr(C)]
#[derive(Default)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
#[derive(Default)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

#[repr(C)]
#[derive(Default)]
struct ScreenBufferInfo {
    size: Coord,
    cursor: Coord,
    attributes: u16,
    window: SmallRect,
    max_window: Coord,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(which: u32) -> Handle;
    fn GetConsoleMode(console: Handle, mode: *mut u32) -> i32;
    fn SetConsoleMode(console: Handle, mode: u32) -> i32;
    fn GetConsoleCP() -> u32;
    fn SetConsoleCP(page: u32) -> i32;
    fn GetConsoleOutputCP() -> u32;
    fn SetConsoleOutputCP(page: u32) -> i32;
    fn GetConsoleScreenBufferInfo(console: Handle, info: *mut ScreenBufferInfo) -> i32;
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const UTF8: u32 = 65001;

/// Input: keys arrive as VT sequences, Ctrl+C included, and quick edit is
/// off so a click does not freeze the program.
const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
const RAW_INPUT: u32 = ENABLE_EXTENDED_FLAGS | ENABLE_VIRTUAL_TERMINAL_INPUT;
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
const RAW_OUTPUT: u32 = ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;

const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;

/// Modes and code pages to put back on exit.
struct Saved {
    input: u32,
    output: u32,
    input_cp: u32,
    output_cp: u32,
}

static ORIGINAL: Mutex<Option<Saved>> = Mutex::new(None);

fn handle(which: u32) -> Handle {
    unsafe { GetStdHandle(which) }
}

unsafe extern "system" fn on_close(event: u32) -> i32 {
    match event {
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            super::QUIT.store(true, Ordering::SeqCst);
            // Windows ends the process once this returns: leave the main
            // loop a moment to restore the console and stop its children.
            std::thread::sleep(std::time::Duration::from_millis(500));
            1
        }
        _ => 0,
    }
}

pub fn catch_termination() {
    unsafe { SetConsoleCtrlHandler(Some(on_close), 1) };
}

pub fn enter_raw() -> io::Result<()> {
    let (input, output) = (handle(STD_INPUT_HANDLE), handle(STD_OUTPUT_HANDLE));
    let (mut input_mode, mut output_mode) = (0, 0);
    if unsafe { GetConsoleMode(input, &mut input_mode) } == 0 {
        return Err(io::Error::other("stdin is not a console"));
    }
    if unsafe { GetConsoleMode(output, &mut output_mode) } == 0 {
        return Err(io::Error::other("stdout is not a console"));
    }
    let saved = Saved {
        input: input_mode,
        output: output_mode,
        input_cp: unsafe { GetConsoleCP() },
        output_cp: unsafe { GetConsoleOutputCP() },
    };
    if unsafe { SetConsoleMode(output, RAW_OUTPUT) } == 0 {
        return Err(io::Error::other("this console does not understand escape sequences (Windows 10 or later needed)"));
    }
    unsafe {
        SetConsoleMode(input, RAW_INPUT);
        SetConsoleCP(UTF8);
        SetConsoleOutputCP(UTF8);
    }
    *ORIGINAL.lock().unwrap() = Some(saved);
    Ok(())
}

/// Restores the console. False when raw mode was not on.
pub fn leave_raw() -> bool {
    let Some(saved) = ORIGINAL.lock().map(|mut o| o.take()).ok().flatten() else { return false };
    unsafe {
        SetConsoleMode(handle(STD_INPUT_HANDLE), saved.input);
        SetConsoleMode(handle(STD_OUTPUT_HANDLE), saved.output);
        SetConsoleCP(saved.input_cp);
        SetConsoleOutputCP(saved.output_cp);
    }
    true
}

pub fn size() -> (u16, u16) {
    let mut info = ScreenBufferInfo::default();
    if unsafe { GetConsoleScreenBufferInfo(handle(STD_OUTPUT_HANDLE), &mut info) } == 0 {
        return (80, 24);
    }
    let w = info.window.right - info.window.left + 1;
    let h = info.window.bottom - info.window.top + 1;
    if w > 0 && h > 0 { (w as u16, h as u16) } else { (80, 24) }
}

/// The console never reports its cell size in pixels.
pub fn cell_pixels() -> Option<(u16, u16)> {
    None
}
