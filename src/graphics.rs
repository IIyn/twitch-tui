//! Real picture through the kitty graphics protocol (kitty, Ghostty).
//!
//! Frames travel through shared memory: the terminal reads the pixels straight
//! from RAM and scales them on the GPU, so the tty only carries a short command
//! per frame. That makes it lighter than the ASCII picture, which repaints
//! every cell of the panel.

use std::io::{self, Write};

use term::Rect;
use video::Video;

/// Arbitrary, just unlikely to clash with another program's images.
const IMAGE_ID: u32 = 0x7474_7569;
/// Shared memory names in rotation, so a frame is never overwritten while the
/// terminal may still be reading it.
const SLOTS: u64 = 4;

/// Whether the terminal speaks the protocol with shared memory, which needs
/// it to run on this machine.
pub fn supported() -> bool {
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    if !var("SSH_CONNECTION").is_empty() || !var("SSH_TTY").is_empty() {
        return false;
    }
    var("TERM").contains("kitty")
        || !var("KITTY_WINDOW_ID").is_empty()
        || var("TERM").contains("ghostty")
        || var("TERM_PROGRAM").eq_ignore_ascii_case("ghostty")
}

#[derive(Default)]
pub struct Graphics {
    /// Where the picture is, and which frame it shows.
    shown: Option<(Rect, u64)>,
}

impl Graphics {
    /// Puts the latest frame over `r`, if it is not already there.
    pub fn show(&mut self, out: &mut impl Write, video: &Video, r: Rect) -> io::Result<()> {
        let command = video.with_frame(|frame| {
            if self.shown == Some((r, frame.seq)) {
                return Ok(None);
            }
            let slot = frame.seq % SLOTS;
            std::fs::write(shm_path(slot), &frame.pixels)?;
            // Centred like the ASCII picture while the decoder catches up
            // with a new panel size.
            let (cols, rows) = (frame.target.cols.min(r.w), frame.target.rows.min(r.h));
            let (x, y) = (r.x + (r.w - cols) / 2, r.y + (r.h - rows) / 2);
            let command = format!(
                "\x1b7\x1b[{};{}H\x1b_Ga=T,f=24,t=s,s={},v={},i={IMAGE_ID},p=1,c={cols},r={rows},C=1,q=2;{}\x1b\\\x1b8",
                y + 1,
                x + 1,
                frame.target.width,
                frame.target.height,
                base64(shm_name(slot).as_bytes()),
            );
            io::Result::Ok(Some((command, frame.seq)))
        });
        let command = match command {
            // No picture any more: the stream stopped.
            None => return self.hide(out),
            Some(command) => command?,
        };
        let Some((command, seq)) = command else { return Ok(()) };
        out.write_all(command.as_bytes())?;
        out.flush()?;
        self.shown = Some((r, seq));
        Ok(())
    }

    pub fn hide(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.shown.take().is_some() {
            out.write_all(format!("\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\").as_bytes())?;
            out.flush()?;
        }
        Ok(())
    }

    /// The screen was cleared, which removes the picture with it.
    pub fn forget(&mut self) {
        self.shown = None;
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        let _ = self.hide(&mut io::stdout());
        // The terminal deletes what it read; this catches what it did not.
        for slot in 0..SLOTS {
            let _ = std::fs::remove_file(shm_path(slot));
        }
    }
}

fn shm_name(slot: u64) -> String {
    format!("/twitch-tui-{}-{slot}", std::process::id())
}

/// Where `shm_open` puts `shm_name` on Linux.
fn shm_path(slot: u64) -> String {
    format!("/dev/shm{}", shm_name(slot))
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"/twitch-tui-1-0"), "L3R3aXRjaC10dWktMS0w");
    }
}
