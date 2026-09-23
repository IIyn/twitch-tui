//! The player's panels: now playing, the visualizer or the picture, and the
//! chat.

use std::sync::atomic::Ordering;

use term::{Rect, Rgb, Screen, Style, str_width, truncate};
use twitch_chat::{Badge, ChatMessage, MsgKind};
use twitch_core::now_secs;
use video::{SAMPLES_X, SAMPLES_Y};

use super::{Layout, Line, panel, right_align, spinner, wrap};
use crate::app::player::{ChatState, Playback};
use crate::app::{App, Focus, Typing};
use crate::theme::{self, *};
use crate::util;
use crate::viz::VizStyle;

/// Where the player's panels go; none of them when they do not fit.
#[derive(Default)]
pub struct Areas {
    now: Option<Rect>,
    viz: Option<Rect>,
    chat: Option<Rect>,
}

/// Shares the body between the channel list and the player's panels.
pub fn split(body: Rect, app: &App) -> (Option<Rect>, Areas) {
    let w = body.w;
    if app.player.zoom {
        return (None, Areas { viz: Some(body), ..Areas::default() });
    }
    let sidebar_w = match w {
        90.. => 36,
        64.. => 28,
        _ => 0,
    };
    let (sidebar, right) = if sidebar_w == 0 {
        if app.focus == Focus::Sidebar {
            (Some(body), None)
        } else {
            (None, Some(body))
        }
    } else {
        (
            Some(Rect::new(0, body.y, sidebar_w, body.h)),
            Some(Rect::new(sidebar_w, body.y, w - sidebar_w, body.h)),
        )
    };

    let mut areas = Areas::default();
    if let Some(r) = right {
        let now_h = 6.min(r.h);
        let rest = r.h - now_h;
        // The ASCII picture deserves the room; the meters do not.
        let viz_h = if app.player.viz.style == VizStyle::Video {
            (rest * 7 / 10).max(rest.saturating_sub(10)).min(rest)
        } else if rest < 14 {
            rest.min(4)
        } else {
            (rest * 3 / 10).clamp(7, 16)
        };
        areas.now = Some(Rect::new(r.x, r.y, r.w, now_h));
        areas.viz = Some(Rect::new(r.x, r.y + now_h, r.w, viz_h));
        areas.chat = Some(Rect::new(r.x, r.y + now_h + viz_h, r.w, rest - viz_h));
    }
    (sidebar, areas)
}

impl Layout {
    /// Drawing area of the visualizer, in cells.
    pub fn viz_size(&self) -> (u16, u16) {
        self.viz_inner().map(|r| (r.w, r.h)).unwrap_or((0, 0))
    }

    fn viz_inner(&self) -> Option<Rect> {
        self.player.viz.map(|r| Rect::new(r.x + 2, r.y + 1, r.w.saturating_sub(4), r.h.saturating_sub(2)))
    }

    /// Where the real picture goes, when it is shown at all.
    pub fn picture(&self, app: &App, (w, h): (u16, u16)) -> Option<Rect> {
        let player = &app.player;
        let shown = player.graphics && player.viz.style == VizStyle::Video && !app.show_help && w >= 30 && h >= 10;
        self.viz_inner().filter(|r| shown && r.w > 0 && r.h > 0)
    }
}

pub fn draw(s: &mut Screen, app: &App, areas: &Areas, frame: u64) {
    if let Some(r) = areas.now {
        draw_now_playing(s, app, r, frame);
    }
    if let Some(r) = areas.viz {
        draw_viz(s, app, r, frame);
    }
    if let Some(r) = areas.chat {
        draw_chat(s, app, r);
    }
}

// ---------------------------------------------------------------- now playing

fn draw_now_playing(s: &mut Screen, app: &App, r: Rect, frame: u64) {
    let title: &[(&str, Style)] = &[("Now playing", theme::panel().fg(MUTED).bold())];
    let inner = panel(s, r, title, &[], false);
    if inner.h == 0 {
        return;
    }
    let base = theme::panel();

    let Some(details) = &app.player.current else {
        s.print(inner.x, inner.y, "Nothing playing", base.bold(), inner.w);
        if inner.h > 1 {
            let hint = "Pick a channel on the left and press Enter, or search with s.";
            s.print(inner.x, inner.y + 1, &truncate(hint, inner.w as usize), base.fg(MUTED).italic(), inner.w);
        }
        return;
    };
    let c = &details.channel;

    // Line 1: badge, name, follow state · viewers, uptime
    let mut x = inner.x;
    if c.stream.is_some() {
        x += s.print(x, inner.y, " LIVE ", Style::new(Rgb(255, 255, 255), LIVE).bold(), inner.w);
    } else {
        x += s.print(x, inner.y, " OFFLINE ", Style::new(TEXT, BORDER).bold(), inner.w);
    }
    x += 1;
    x += s.print(x, inner.y, &c.display_name, base.bold(), inner.right().saturating_sub(x));
    x += 2;
    let follow = match (details.following, app.player.follow_pending) {
        (_, true) => Some((format!("{} updating", spinner(frame)), MUTED)),
        (Some(true), _) => Some(("♥ following".to_string(), PINK)),
        (Some(false), _) => Some(("♡ follow (f)".to_string(), MUTED)),
        (None, _) => None,
    };
    if let Some((text, color)) = follow {
        x += s.print(x, inner.y, &text, base.fg(color), inner.right().saturating_sub(x));
    }
    if let Some(st) = &c.stream {
        let uptime = util::parse_iso8601(&st.started_at)
            .map(|t| format!("  ◷ {}", util::duration(now_secs() - t)))
            .unwrap_or_default();
        let stats = format!("◉ {} viewers{uptime}", util::grouped(st.viewers));
        if x + str_width(&stats) as u16 + 2 < inner.right() {
            right_align(s, inner, inner.y, &stats, base.fg(PINK));
        }
    }

    // Line 2: title
    if inner.h > 1 {
        let title = c
            .stream
            .as_ref()
            .map(|st| st.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| details.description.clone());
        s.print(inner.x, inner.y + 1, &truncate(&title, inner.w as usize), base, inner.w);
    }

    // Line 3: game · followers
    if inner.h > 2 {
        let mut segs = Vec::new();
        if let Some(st) = c.stream.as_ref().filter(|st| !st.game.is_empty()) {
            segs.push((st.game.clone(), base.fg(ACCENT_SOFT)));
            segs.push(("  ·  ".to_string(), base.fg(FAINT)));
        }
        if details.followers > 0 {
            segs.push((format!("{} followers", util::compact(details.followers)), base.fg(MUTED)));
            segs.push(("  ·  ".to_string(), base.fg(FAINT)));
        }
        segs.push((format!("twitch.tv/{}", c.login), base.fg(FAINT)));
        segments(s, inner.x, inner.y + 2, &segs, inner.right());
    }

    // Line 4: playback status and volume
    if inner.h > 3 {
        let y = inner.y + 3;
        let (text, color) = match &app.player.playback {
            Playback::Idle => ("■ Stopped".to_string(), MUTED),
            Playback::Resolving => (format!("{} Resolving stream…", spinner(frame)), YELLOW),
            Playback::Buffering => (format!("{} Buffering…", spinner(frame)), YELLOW),
            Playback::Playing(since) => (
                format!(
                    "▶ Playing  {}  · {} via {}",
                    util::duration(since.elapsed().as_secs() as i64),
                    if app.player.audio_from_video { "sound synced to video" } else { "audio only" },
                    app.player.audio.backend()
                ),
                GREEN,
            ),
            Playback::Paused => ("⏸ Paused, press space to resume".to_string(), MUTED),
            Playback::Offline => ("● Offline, chat only".to_string(), MUTED),
            Playback::Failed(e) => (format!("✕ {e}"), LIVE),
        };
        let tw = inner.w.saturating_sub(VOLUME_W + 2);
        s.print(inner.x, y, &truncate(&text, tw as usize), base.fg(color), tw);
        draw_volume(s, app, inner.right().saturating_sub(VOLUME_W), y);
    }
}

const VOLUME_W: u16 = 19;

fn draw_volume(s: &mut Screen, app: &App, x: u16, y: u16) {
    let base = theme::panel();
    let vol = app.player.audio.volume.load(Ordering::Relaxed);
    let muted = app.player.audio.muted.load(Ordering::Relaxed);
    let mut cx = x;
    cx += s.print(cx, y, "vol ", base.fg(MUTED), 4);
    const SLOTS: u32 = 10;
    let filled = (vol * SLOTS).div_ceil(100).min(SLOTS);
    for i in 0..SLOTS {
        let color = if muted {
            FAINT
        } else if i < filled {
            theme::gradient(i as f32 / SLOTS as f32)
        } else {
            BORDER
        };
        cx += s.print(cx, y, if i < filled { "▮" } else { "▯" }, base.fg(color), 1);
    }
    let label = if muted { " mute".to_string() } else { format!(" {vol:>3}%") };
    s.print(cx, y, &label, base.fg(if muted { LIVE } else { TEXT }), 5);
}

// ---------------------------------------------------------------- visualizer

/// Brightness ramp, darkest first, with roughly even ink coverage. Denser
/// ramps only add characters too close in weight to tell apart.
const ASCII_RAMP: &[u8] = b" .:-=+*#%@";

fn luma(pixel: [u8; 3]) -> f32 {
    0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32 + 0.114 * pixel[2] as f32
}

/// Draws the decoded picture as coloured ASCII, one character per cell.
///
/// Every cell averages a 2x4 block of samples, which keeps the tones smooth;
/// detail comes from the number of cells, hence the zoom (`z`).
fn draw_ascii_video(s: &mut Screen, app: &App, r: Rect, frame: u64) {
    let drawn = app.player.video.with_frame(|picture| {
        // The real picture is laid over the panel after the cells are drawn.
        if app.player.graphics {
            return true;
        }
        let (cols, rows) = (picture.target.cols, picture.target.rows);
        // A frame from before a switch to ASCII has no samples to spare.
        if picture.target != video::Target::ascii(cols, rows) {
            return false;
        }
        let cols = cols.min(r.w);
        let rows = rows.min(r.h);
        let x0 = r.x + (r.w - cols) / 2;
        let y0 = r.y + (r.h - rows) / 2;

        // A few extreme pixels must not set the scale, so the range comes
        // from the 5th and 95th percentile of a histogram.
        let mut histogram = [0u32; 64];
        for y in 0..picture.target.height as u16 {
            for x in 0..picture.target.width as u16 {
                let bin = (luma(picture.sample(x, y)) / 4.0) as usize;
                histogram[bin.min(63)] += 1;
            }
        }
        let total: u32 = histogram.iter().sum();
        let percentile = |want: u32| -> f32 {
            let mut seen = 0;
            for (bin, count) in histogram.iter().enumerate() {
                seen += count;
                if seen >= want {
                    return bin as f32 * 4.0;
                }
            }
            255.0
        };
        let low = percentile(total / 20);
        let span = (percentile(total - total / 20) - low).max(24.0);

        for row in 0..rows {
            for col in 0..cols {
                let mut sum = [0u32; 3];
                for dy in 0..SAMPLES_Y {
                    for dx in 0..SAMPLES_X {
                        let pixel = picture.sample(col * SAMPLES_X + dx, row * SAMPLES_Y + dy);
                        for (channel, value) in sum.iter_mut().zip(pixel) {
                            *channel += value as u32;
                        }
                    }
                }
                let samples = (SAMPLES_X * SAMPLES_Y) as u32;
                let color =
                    Rgb((sum[0] / samples) as u8, (sum[1] / samples) as u8, (sum[2] / samples) as u8);
                // The gamma lifts the mid tones, which dark scenes are full of.
                let level =
                    ((luma([color.0, color.1, color.2]) - low) / span).clamp(0.0, 1.0).powf(0.75);
                let step = (level * (ASCII_RAMP.len() - 1) as f32).round() as usize;
                let ch = ASCII_RAMP[step.min(ASCII_RAMP.len() - 1)] as char;
                // Lift the colour so dark areas keep their hue.
                s.put(x0 + col, y0 + row, ch, color.lerp(Rgb(255, 255, 255), 0.25));
            }
        }
        true
    });
    if drawn.is_some() {
        return;
    }
    let (text, color) = match (&app.player.playback, app.player.video_available()) {
        (Playback::Playing(_) | Playback::Buffering, true) => {
            (format!("{} Starting the picture…", spinner(frame)), MUTED)
        }
        (Playback::Playing(_) | Playback::Buffering, false) => {
            ("This stream offers no video rendition".to_string(), MUTED)
        }
        _ => ("Play a channel to see the picture".to_string(), FAINT),
    };
    let text = truncate(&text, r.w as usize);
    let x = r.x + (r.w.saturating_sub(str_width(&text) as u16)) / 2;
    s.print(x, r.y + r.h / 2, &text, theme::panel().fg(color).italic(), r.w);
}

fn draw_viz(s: &mut Screen, app: &App, r: Rect, frame: u64) {
    let name = app.player.viz.style.name();
    let name = if app.player.viz.style == VizStyle::Video && !app.player.graphics { "ascii video" } else { name };
    let quality = app.player.rendition().map(|r| r.name.as_str()).unwrap_or("");
    let rate = format!("{name} · {quality} · {:.0} fps", app.player.video_fps);
    let name = if app.player.viz.style == VizStyle::Video && app.player.video_fps > 0.0 { rate.as_str() } else { name };
    let title: &[(&str, Style)] = &[("♫ ", theme::panel().fg(PINK)), (name, theme::panel().fg(MUTED).bold())];
    let right: &[(&str, Style)] = if app.player.zoom {
        &[("z", theme::panel().fg(ACCENT_SOFT).bold()), (" exit zoom", theme::panel().fg(FAINT))]
    } else if app.player.viz.style == VizStyle::Video {
        &[("z", theme::panel().fg(ACCENT_SOFT).bold()), (" zoom", theme::panel().fg(FAINT))]
    } else {
        &[("v", theme::panel().fg(ACCENT_SOFT).bold()), (" style", theme::panel().fg(FAINT))]
    };
    let inner = panel(s, r, title, right, false);
    if app.player.viz.style == VizStyle::Video {
        draw_ascii_video(s, app, inner, frame);
        return;
    }
    let active = matches!(app.player.playback, Playback::Playing(_));
    app.player.viz.draw(s, inner, active);
}

// ---------------------------------------------------------------- chat

fn draw_chat(s: &mut Screen, app: &App, r: Rect) {
    let focused = app.focus == Focus::Chat;
    let channel = app.player.current.as_ref().map(|c| format!("#{}", c.channel.login)).unwrap_or_default();
    let title_style = theme::panel().fg(if focused { TEXT } else { MUTED }).bold();
    let title: &[(&str, Style)] = &[("Chat ", title_style), (&channel, theme::panel().fg(ACCENT_SOFT))];
    let (status, color) = match &app.player.chat_state {
        ChatState::Connecting => ("connecting…", YELLOW),
        ChatState::Connected if app.player.current.is_some() => ("joining…", YELLOW),
        ChatState::Connected => ("connected", GREEN),
        ChatState::Joined => ("live", GREEN),
        ChatState::Disconnected(_) => ("reconnecting…", LIVE),
    };
    let read_only = app.player.chat.as_ref().is_some_and(|c| !c.can_send);
    let status = if read_only && matches!(app.player.chat_state, ChatState::Joined) { "read-only" } else { status };
    let dot = theme::panel().fg(color);
    let right: &[(&str, Style)] = &[("● ", dot), (status, theme::panel().fg(MUTED))];
    let inner = panel(s, r, title, right, focused);
    if inner.h < 3 {
        return;
    }

    let msg_area = Rect::new(inner.x, inner.y, inner.w, inner.h - 2);
    draw_messages(s, app, msg_area);

    // Separator and input
    let sep_y = inner.bottom() - 2;
    for x in inner.x..inner.right() {
        s.set(x, sep_y, '─', theme::panel().fg(Rgb(36, 35, 46)));
    }
    let y = inner.bottom() - 1;
    let typing = app.typing == Some(Typing::Chat);
    let prompt_style = theme::panel().fg(if typing { PINK } else { FAINT }).bold();
    s.print(inner.x, y, "›", prompt_style, 1);
    let field_w = inner.w.saturating_sub(2) as usize;
    if app.player.chat_input.is_empty() && !typing {
        let hint = match (app.player.current.is_some(), read_only) {
            (false, _) => "Join a channel to chat",
            (true, true) => "Read-only: add your token to chat (press ?)",
            (true, false) => "Press i to write a message",
        };
        s.print(inner.x + 2, y, hint, theme::panel().fg(FAINT).italic(), field_w as u16);
    } else {
        let (shown, cursor) = app.player.chat_input.view(field_w);
        s.print(inner.x + 2, y, &shown, theme::panel(), field_w as u16);
        if typing {
            s.cursor = Some((inner.x + 2 + cursor as u16, y));
        }
    }
}

fn draw_messages(s: &mut Screen, app: &App, r: Rect) {
    if r.h == 0 || r.w < 10 {
        return;
    }
    let total = app.player.messages.len();
    let end = total.saturating_sub(app.player.chat_scroll);
    let mut lines: Vec<(Line, Rgb)> = Vec::new();
    for msg in app.player.messages.range(..end).rev() {
        let bg = if msg.mention { Rgb(52, 26, 48) } else { PANEL };
        let wrapped = wrap(&message_segments(msg, bg), r.w as usize, 6);
        for line in wrapped.into_iter().rev() {
            lines.push((line, bg));
        }
        if lines.len() >= r.h as usize {
            break;
        }
    }

    let visible = lines.len().min(r.h as usize);
    let top = r.bottom() - visible as u16;
    for (i, (line, bg)) in lines.iter().take(visible).rev().enumerate() {
        let y = top + i as u16;
        if *bg != PANEL {
            s.fill(Rect::new(r.x, y, r.w, 1), theme::panel().bg(*bg));
            s.set(r.x.saturating_sub(1), y, '▎', theme::panel().fg(PINK));
        }
        segments(s, r.x, y, line, r.right());
    }

    if app.player.chat_scroll > 0 {
        let label = format!(" ↓ {} newer · G to jump ", app.player.chat_scroll);
        let w = str_width(&label) as u16;
        let x = r.x + r.w.saturating_sub(w) / 2;
        let y = r.bottom() - 1;
        s.fill(Rect::new(r.x.saturating_sub(1), y, r.w + 1, 1), theme::panel());
        s.print(x, y, &label, Style::new(Rgb(255, 255, 255), ACCENT).bold(), w);
    }
}

fn message_segments(msg: &ChatMessage, bg: Rgb) -> Line {
    let base = theme::panel().bg(bg);
    let mut segs: Line = vec![(format!("{} ", util::local_hm(msg.timestamp)), base.fg(FAINT))];
    if msg.kind == MsgKind::System {
        segs.push(("• ".into(), base.fg(ACCENT)));
        for (text, _) in &msg.spans {
            segs.push((text.clone(), base.fg(MUTED).italic()));
        }
        return segs;
    }
    for badge in &msg.badges {
        let (glyph, color) = match badge {
            Badge::Broadcaster => ("◉", LIVE),
            Badge::Moderator => ("⚔", GREEN),
            Badge::Vip => ("◆", PINK),
            Badge::Subscriber => ("★", ACCENT_SOFT),
            Badge::Staff => ("✓", CYAN),
        };
        segs.push((format!("{glyph} "), base.fg(color)));
    }
    let color = msg.color.map(|[r, g, b]| Rgb(r, g, b));
    let color = theme::readable(color.unwrap_or_else(|| theme::name_color(&msg.author_login)));
    segs.push((msg.author.clone(), base.fg(color).bold()));
    if msg.deleted {
        segs.push((": ".into(), base.fg(MUTED)));
        segs.push(("<message deleted>".into(), base.fg(FAINT).italic()));
        return segs;
    }
    let (sep, text_style) = match msg.kind {
        MsgKind::Action => (" ", base.fg(color).italic()),
        _ => (": ", base.fg(TEXT)),
    };
    segs.push((sep.into(), base.fg(MUTED)));
    for (text, emote) in &msg.spans {
        let st = if *emote { base.fg(YELLOW).bold() } else { text_style };
        segs.push((text.clone(), st));
    }
    segs
}

/// Prints styled segments in sequence, returns the end column.
fn segments(s: &mut Screen, x: u16, y: u16, segs: &[(String, Style)], max_x: u16) -> u16 {
    let mut cx = x;
    for (text, style) in segs {
        if cx >= max_x {
            break;
        }
        cx += s.print(cx, y, text, *style, max_x - cx);
    }
    cx
}
