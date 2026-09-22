//! Drawing.

use std::sync::atomic::Ordering;

use crate::app::{App, Auth, ChatState, Focus, Level, Load, Playback, Tab, Typing};
use term::{Rect, Rgb, Screen, Style, char_width, str_width, truncate};
use twitch_channels::Channel;
use twitch_core::now_secs;
use twitch_chat::{Badge, ChatMessage, MsgKind};
use video::{SAMPLES_X, SAMPLES_Y};

use crate::theme::{self, *};
use crate::viz::VizStyle;

use crate::util;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub struct Layout {
    header: Rect,
    sidebar: Option<Rect>,
    now: Option<Rect>,
    viz: Option<Rect>,
    chat: Option<Rect>,
    footer: Rect,
}

impl Layout {
    pub fn compute(w: u16, h: u16, app: &App) -> Layout {
        let body = Rect::new(0, 1, w, h.saturating_sub(2));
        if app.zoom {
            return Layout {
                header: Rect::new(0, 0, w, 1),
                sidebar: None,
                now: None,
                viz: Some(body),
                chat: None,
                footer: Rect::new(0, h.saturating_sub(1), w, 1),
            };
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

        let (mut now, mut viz, mut chat) = (None, None, None);
        if let Some(r) = right {
            let now_h = 6.min(r.h);
            let rest = r.h - now_h;
            // The ASCII picture deserves the room; the meters do not.
            let viz_h = if app.viz.style == VizStyle::Video {
                (rest * 7 / 10).max(rest.saturating_sub(10)).min(rest)
            } else if rest < 14 {
                rest.min(4)
            } else {
                (rest * 3 / 10).clamp(7, 16)
            };
            now = Some(Rect::new(r.x, r.y, r.w, now_h));
            viz = Some(Rect::new(r.x, r.y + now_h, r.w, viz_h));
            chat = Some(Rect::new(r.x, r.y + now_h + viz_h, r.w, rest - viz_h));
        }
        Layout {
            header: Rect::new(0, 0, w, 1),
            sidebar,
            now,
            viz,
            chat,
            footer: Rect::new(0, h.saturating_sub(1), w, 1),
        }
    }

    /// Drawing area of the visualizer, in cells.
    pub fn viz_size(&self) -> (u16, u16) {
        self.viz_inner().map(|r| (r.w, r.h)).unwrap_or((0, 0))
    }

    fn viz_inner(&self) -> Option<Rect> {
        self.viz.map(|r| Rect::new(r.x + 2, r.y + 1, r.w.saturating_sub(4), r.h.saturating_sub(2)))
    }

    /// Where the real picture goes, when it is shown at all.
    pub fn picture(&self, app: &App, (w, h): (u16, u16)) -> Option<Rect> {
        let shown = app.graphics && app.viz.style == VizStyle::Video && !app.show_help && w >= 30 && h >= 10;
        self.viz_inner().filter(|r| shown && r.w > 0 && r.h > 0)
    }
}

pub fn draw(s: &mut Screen, app: &App, frame: u64) {
    let (w, h) = (s.w, s.h);
    s.fill(Rect::new(0, 0, w, h), theme::base());
    if w < 30 || h < 10 {
        s.print(1, 1, "Terminal too small", theme::base().fg(MUTED), w.saturating_sub(2));
        return;
    }
    let layout = Layout::compute(w, h, app);
    draw_header(s, app, layout.header);
    if let Some(r) = layout.sidebar {
        draw_sidebar(s, app, r, frame);
    }
    if let Some(r) = layout.now {
        draw_now_playing(s, app, r, frame);
    }
    if let Some(r) = layout.viz {
        draw_viz(s, app, r, frame);
    }
    if let Some(r) = layout.chat {
        draw_chat(s, app, r);
    }
    draw_footer(s, app, layout.footer);
    if app.show_help {
        draw_help(s, app);
    }
}

// ---------------------------------------------------------------- widgets

/// Draws a rounded panel and returns its inner area.
fn panel(s: &mut Screen, r: Rect, title: &[(&str, Style)], right: &[(&str, Style)], focused: bool) -> Rect {
    if r.w < 2 || r.h < 2 {
        return Rect::new(r.x, r.y, 0, 0);
    }
    s.fill(r, theme::panel());
    let border = if focused { ACCENT } else { BORDER };
    let bs = theme::panel().fg(border);
    let (x0, y0, x1, y1) = (r.x, r.y, r.right() - 1, r.bottom() - 1);
    for x in x0 + 1..x1 {
        s.set(x, y0, '─', bs);
        s.set(x, y1, '─', bs);
    }
    for y in y0 + 1..y1 {
        s.set(x0, y, '│', bs);
        s.set(x1, y, '│', bs);
    }
    s.set(x0, y0, '╭', bs);
    s.set(x1, y0, '╮', bs);
    s.set(x0, y1, '╰', bs);
    s.set(x1, y1, '╯', bs);

    let max = r.w.saturating_sub(4);
    let mut x = x0 + 2;
    if !title.is_empty() && max > 2 {
        x += s.print(x, y0, " ", bs, 1);
        for (text, style) in title {
            x += s.print(x, y0, text, *style, (x0 + 2 + max).saturating_sub(x));
        }
        s.print(x, y0, " ", bs, 1);
    }
    let right_w: u16 = right.iter().map(|(t, _)| str_width(t) as u16).sum::<u16>() + 2;
    if !right.is_empty() && right_w + 6 < r.w.saturating_sub(x - x0) {
        let mut rx = x1 - 1 - right_w;
        rx += s.print(rx, y0, " ", bs, 1);
        for (text, style) in right {
            rx += s.print(rx, y0, text, *style, r.w);
        }
        s.print(rx, y0, " ", bs, 1);
    }
    Rect::new(r.x + 2, r.y + 1, r.w.saturating_sub(4), r.h.saturating_sub(2))
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

fn right_align(s: &mut Screen, r: Rect, y: u16, text: &str, style: Style) -> u16 {
    let w = str_width(text) as u16;
    let x = r.right().saturating_sub(w).max(r.x);
    s.print(x, y, text, style, r.right() - x);
    x
}

fn spinner(frame: u64) -> char {
    SPINNER[(frame / 3) as usize % SPINNER.len()]
}

// ---------------------------------------------------------------- header & footer

fn draw_header(s: &mut Screen, app: &App, r: Rect) {
    let bar = Style::new(TEXT, Rgb(24, 20, 36));
    s.fill(r, bar);
    let logo = " ♫ twitch-tui ";
    let n = logo.chars().count().max(2) as f32;
    let mut x = r.x + 1;
    for (i, c) in logo.chars().enumerate() {
        let st = bar.fg(Rgb(255, 255, 255)).bg(theme::gradient(i as f32 / (n - 1.0) * 0.7)).bold();
        x += s.print(x, r.y, &c.to_string(), st, 2);
    }
    let logo_end = x;

    let (dot, text, color) = match &app.auth {
        Auth::LoggedIn(me) => ("●", me.display_name.clone(), GREEN),
        Auth::Checking => ("◌", "logging in…".to_string(), YELLOW),
        Auth::Anonymous => ("○", "anonymous · ? to log in".to_string(), MUTED),
        Auth::Failed(_) => ("✕", "login failed · ? for help".to_string(), LIVE),
    };
    let label = format!("{dot} {text} ");
    let x = right_align(s, r, r.y, &label, bar.fg(color));
    s.print(x, r.y, dot, bar.fg(color).bold(), 1);

    let tagline = "  audio-only twitch";
    if logo_end + str_width(tagline) as u16 + 2 < x {
        s.print(logo_end, r.y, tagline, bar.fg(FAINT).italic(), x - logo_end);
    }
}

fn draw_footer(s: &mut Screen, app: &App, r: Rect) {
    let bar = Style::new(MUTED, BG);
    s.fill(r, bar);

    let hints: &[(&str, &str)] = match (app.typing, app.focus) {
        (Some(Typing::Chat), _) => &[("⏎", "send"), ("↑↓", "history"), ("esc", "done"), ("/me", "action")],
        (Some(Typing::Search), _) => &[("⏎", "search"), ("↓", "results"), ("esc", "done")],
        (Some(Typing::Filter), _) => &[("⏎", "done"), ("esc", "done"), ("", "type to filter")],
        (None, Focus::Sidebar) => &[
            ("↑↓", "move"),
            ("⏎", "play"),
            ("/", "filter"),
            ("s", "search"),
            ("tab", "chat"),
            ("space", "pause"),
            ("f", "follow"),
            ("+-", "volume"),
            ("v", "visualizer"),
            ("?", "help"),
            ("q", "quit"),
        ],
        (None, Focus::Chat) => &[
            ("i", "write"),
            ("↑↓", "scroll"),
            ("G", "latest"),
            ("tab", "channels"),
            ("space", "pause"),
            ("f", "follow"),
            ("+-", "volume"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };

    let toast_w = app.toast.as_ref().map(|t| (str_width(&t.text) + 4) as u16).unwrap_or(0);
    let limit = r.right().saturating_sub(toast_w + 1);
    let mut x = r.x + 1;
    for (key, desc) in hints {
        let need = (str_width(key) + str_width(desc) + 3) as u16;
        if x + need > limit {
            break;
        }
        if !key.is_empty() {
            x += s.print(x, r.y, key, bar.fg(ACCENT_SOFT).bold(), limit - x);
            x += 1;
        }
        x += s.print(x, r.y, desc, bar.fg(FAINT), limit - x);
        x += 2;
    }

    if let Some(toast) = &app.toast {
        let (fg, bg) = match toast.level {
            Level::Info => (TEXT, HIGHLIGHT),
            Level::Success => (Rgb(10, 30, 20), GREEN),
            Level::Error => (Rgb(255, 255, 255), Rgb(170, 30, 50)),
        };
        let text = truncate(&toast.text, r.w.saturating_sub(4) as usize);
        let label = format!(" {text} ");
        let w = str_width(&label) as u16;
        let x = r.right().saturating_sub(w + 1);
        s.print(x, r.y, &label, Style::new(fg, bg).bold(), w);
    }
}

// ---------------------------------------------------------------- sidebar

fn draw_sidebar(s: &mut Screen, app: &App, r: Rect, frame: u64) {
    let focused = app.focus == Focus::Sidebar;
    let title: &[(&str, Style)] = &[("Channels", theme::panel().fg(if focused { TEXT } else { MUTED }).bold())];
    let inner = panel(s, r, title, &[], focused);
    if inner.h < 3 || inner.w < 8 {
        return;
    }

    // Tabs
    let live = app.following.iter().filter(|c| c.stream.is_some()).count();
    let follow_label = match (&app.following_state, app.following.is_empty()) {
        (Load::Loading, false) => format!(" Following {} {live}● ", spinner(frame)),
        (_, false) => format!(" Following {live}● "),
        _ => " Following ".to_string(),
    };
    let tabs = [(Tab::Following, follow_label, "1"), (Tab::Search, " Search ".to_string(), "2")];
    let mut x = inner.x;
    for (tab, label, _) in &tabs {
        let active = app.tab == *tab;
        let st = if active {
            Style::new(Rgb(255, 255, 255), ACCENT).bold()
        } else {
            theme::panel().fg(MUTED)
        };
        x += s.print(x, inner.y, label, st, inner.right().saturating_sub(x));
        x += 1;
    }
    let mut y = inner.y + 2;

    // Search / filter field
    let field = match app.tab {
        Tab::Search => Some((&app.search, Typing::Search, "Search channels…", '⌕')),
        Tab::Following if !app.filter.is_empty() || app.typing == Some(Typing::Filter) => {
            Some((&app.filter, Typing::Filter, "Filter…", '⧩'))
        }
        _ => None,
    };
    if let Some((edit, kind, placeholder, icon)) = field {
        let active = app.typing == Some(kind);
        let field_bg = if active { HIGHLIGHT } else { Rgb(28, 27, 36) };
        let fr = Rect::new(inner.x, y, inner.w, 1);
        s.fill(fr, Style::new(TEXT, field_bg));
        s.print(fr.x + 1, y, &icon.to_string(), Style::new(ACCENT_SOFT, field_bg), 1);
        let tw = fr.w.saturating_sub(4) as usize;
        if edit.is_empty() && !active {
            s.print(fr.x + 3, y, placeholder, Style::new(FAINT, field_bg).italic(), tw as u16);
        } else {
            let (shown, cursor) = edit.view(tw);
            s.print(fr.x + 3, y, &shown, Style::new(TEXT, field_bg), tw as u16);
            if active {
                s.cursor = Some((fr.x + 3 + cursor as u16, y));
            }
        }
        y += 2;
    }

    let list = Rect::new(inner.x, y, inner.w, inner.bottom().saturating_sub(y));
    let channels = app.visible();
    let state = if app.tab == Tab::Following { &app.following_state } else { &app.search_state };

    let empty_msg: Option<(String, Rgb)> = match (app.tab, state) {
        (_, Load::Loading) if channels.is_empty() => Some((format!("{} Loading…", spinner(frame)), MUTED)),
        (_, Load::Failed(e)) if channels.is_empty() => Some((format!("✕ {e}"), LIVE)),
        (Tab::Following, _) if app.me().is_none() => {
            Some(("Log in to see the channels you follow. Press ? for setup.".into(), MUTED))
        }
        (Tab::Following, _) if channels.is_empty() && !app.filter.is_empty() => {
            Some(("No channel matches the filter.".into(), MUTED))
        }
        (Tab::Following, _) if channels.is_empty() => Some(("You don't follow anyone yet.".into(), MUTED)),
        (Tab::Search, Load::Idle) => Some(("Press s or / and type a channel name.".into(), MUTED)),
        (Tab::Search, _) if channels.is_empty() => Some(("No channels found.".into(), MUTED)),
        _ => None,
    };
    if let Some((msg, color)) = empty_msg {
        for (i, line) in wrap_plain(&msg, list.w as usize).iter().enumerate() {
            s.print(list.x, list.y + i as u16, line, theme::panel().fg(color).italic(), list.w);
        }
        return;
    }

    // Twitch hands third-party clients only the newest and oldest follows.
    let note = app.tab == Tab::Following && app.following_capped && list.h >= 6;
    let list = if note { Rect::new(list.x, list.y, list.w, list.h - 2) } else { list };
    if note {
        let text = format!(
            "Twitch shares only {} of your follows — s to search",
            twitch_channels::FOLLOWS_LIMIT * 2
        );
        for (i, line) in wrap_plain(&text, list.w as usize).iter().take(2).enumerate() {
            s.print(list.x, list.bottom() + i as u16, line, theme::panel().fg(FAINT).italic(), list.w);
        }
    }

    let per_page = (list.h / 2).max(1) as usize;
    let selected = app.selected_index();
    let offset = selected.saturating_sub(per_page - 1);
    let playing = app.current.as_ref().map(|c| c.channel.login.as_str());
    for (i, channel) in channels.iter().enumerate().skip(offset).take(per_page) {
        let y = list.y + ((i - offset) * 2) as u16;
        draw_channel_item(s, channel, Rect::new(list.x, y, list.w, 2), i == selected, focused, playing);
    }
    if channels.len() > per_page {
        // Scrollbar
        let track = list.h;
        let thumb_h = ((per_page as f32 / channels.len() as f32) * track as f32).ceil().max(1.0) as u16;
        let max_off = channels.len() - per_page;
        let thumb_y = ((offset as f32 / max_off.max(1) as f32) * (track - thumb_h) as f32).round() as u16;
        let sx = inner.right() + 1;
        for ty in 0..track {
            let on = ty >= thumb_y && ty < thumb_y + thumb_h;
            let st = theme::panel().fg(if on { ACCENT } else { BORDER });
            s.set(sx, list.y + ty, if on { '┃' } else { '│' }, st);
        }
    }
}

fn draw_channel_item(s: &mut Screen, c: &Channel, r: Rect, selected: bool, focused: bool, playing: Option<&str>) {
    let bg = match (selected, focused) {
        (true, true) => HIGHLIGHT,
        (true, false) => Rgb(30, 28, 40),
        _ => PANEL,
    };
    let base = Style::new(TEXT, bg);
    s.fill(Rect::new(r.x.saturating_sub(1), r.y, r.w + 1, 2), base);
    if selected {
        s.set(r.x.saturating_sub(1), r.y, '▌', base.fg(ACCENT));
        s.set(r.x.saturating_sub(1), r.y + 1, '▌', base.fg(ACCENT));
    }
    let is_playing = playing == Some(c.login.as_str());
    let (dot, dot_color) = match (&c.stream, is_playing) {
        (_, true) => ('♫', ACCENT_SOFT),
        (Some(_), _) => ('●', LIVE),
        (None, _) => ('○', FAINT),
    };
    s.print(r.x, r.y, &dot.to_string(), base.fg(dot_color), 1);

    let right = c.stream.as_ref().map(|st| util::compact(st.viewers)).unwrap_or_default();
    let uptime = c
        .stream
        .as_ref()
        .and_then(|st| util::parse_iso8601(&st.started_at))
        .map(|t| format!("◷ {}  ", util::short_duration(now_secs() - t)))
        .unwrap_or_default();
    let right_w = str_width(&right) + str_width(&uptime);
    let name_w = r.w.saturating_sub(right_w as u16 + 3);
    let name_style = if c.stream.is_some() { base.bold() } else { base.fg(MUTED) };
    s.print(r.x + 2, r.y, &truncate(&c.display_name, name_w as usize), name_style, name_w);
    if !right.is_empty() {
        let x = right_align(s, r, r.y, &right, base.fg(PINK));
        let ux = x.saturating_sub(str_width(&uptime) as u16).max(r.x);
        s.print(ux, r.y, &uptime, base.fg(MUTED), x - ux);
    }

    let detail_w = r.w.saturating_sub(2) as usize;
    match &c.stream {
        Some(st) => {
            let game = if st.game.is_empty() { "Live".to_string() } else { st.game.clone() };
            let game = truncate(&game, detail_w);
            let gw = s.print(r.x + 2, r.y + 1, &game, base.fg(ACCENT_SOFT), detail_w as u16);
            let rest = detail_w.saturating_sub(gw as usize + 3);
            if rest > 3 && !st.title.is_empty() {
                s.print(r.x + 2 + gw, r.y + 1, " · ", base.fg(FAINT), 3);
                s.print(r.x + 5 + gw, r.y + 1, &truncate(&st.title, rest), base.fg(MUTED), rest as u16);
            }
        }
        None => {
            s.print(r.x + 2, r.y + 1, "offline", base.fg(FAINT).italic(), detail_w as u16);
        }
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

    let Some(details) = &app.current else {
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
    let follow = match (details.following, app.follow_pending) {
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
        let (text, color) = match &app.playback {
            Playback::Idle => ("■ Stopped".to_string(), MUTED),
            Playback::Resolving => (format!("{} Resolving stream…", spinner(frame)), YELLOW),
            Playback::Buffering => (format!("{} Buffering…", spinner(frame)), YELLOW),
            Playback::Playing(since) => (
                format!(
                    "▶ Playing  {}  · {} via {}",
                    util::duration(since.elapsed().as_secs() as i64),
                    if app.audio_from_video { "sound synced to video" } else { "audio only" },
                    app.audio.backend()
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
    let vol = app.audio.volume.load(Ordering::Relaxed);
    let muted = app.audio.muted.load(Ordering::Relaxed);
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
    let drawn = app.video.with_frame(|picture| {
        // The real picture is laid over the panel after the cells are drawn.
        if app.graphics {
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
    let (text, color) = match (&app.playback, app.video_available()) {
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
    let name = app.viz.style.name();
    let name = if app.viz.style == VizStyle::Video && !app.graphics { "ascii video" } else { name };
    let quality = app.rendition().map(|r| r.name.as_str()).unwrap_or("");
    let rate = format!("{name} · {quality} · {:.0} fps", app.video_fps);
    let name = if app.viz.style == VizStyle::Video && app.video_fps > 0.0 { rate.as_str() } else { name };
    let title: &[(&str, Style)] = &[("♫ ", theme::panel().fg(PINK)), (name, theme::panel().fg(MUTED).bold())];
    let right: &[(&str, Style)] = if app.zoom {
        &[("z", theme::panel().fg(ACCENT_SOFT).bold()), (" exit zoom", theme::panel().fg(FAINT))]
    } else if app.viz.style == VizStyle::Video {
        &[("z", theme::panel().fg(ACCENT_SOFT).bold()), (" zoom", theme::panel().fg(FAINT))]
    } else {
        &[("v", theme::panel().fg(ACCENT_SOFT).bold()), (" style", theme::panel().fg(FAINT))]
    };
    let inner = panel(s, r, title, right, false);
    if app.viz.style == VizStyle::Video {
        draw_ascii_video(s, app, inner, frame);
        return;
    }
    let active = matches!(app.playback, Playback::Playing(_));
    app.viz.draw(s, inner, active);
}

// ---------------------------------------------------------------- chat

fn draw_chat(s: &mut Screen, app: &App, r: Rect) {
    let focused = app.focus == Focus::Chat;
    let channel = app.current.as_ref().map(|c| format!("#{}", c.channel.login)).unwrap_or_default();
    let title_style = theme::panel().fg(if focused { TEXT } else { MUTED }).bold();
    let title: &[(&str, Style)] = &[("Chat ", title_style), (&channel, theme::panel().fg(ACCENT_SOFT))];
    let (status, color) = match &app.chat_state {
        ChatState::Connecting => ("connecting…", YELLOW),
        ChatState::Connected if app.current.is_some() => ("joining…", YELLOW),
        ChatState::Connected => ("connected", GREEN),
        ChatState::Joined => ("live", GREEN),
        ChatState::Disconnected(_) => ("reconnecting…", LIVE),
    };
    let read_only = app.chat.as_ref().is_some_and(|c| !c.can_send);
    let status = if read_only && matches!(app.chat_state, ChatState::Joined) { "read-only" } else { status };
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
    if app.chat_input.is_empty() && !typing {
        let hint = match (app.current.is_some(), read_only) {
            (false, _) => "Join a channel to chat",
            (true, true) => "Read-only: add your token to chat (press ?)",
            (true, false) => "Press i to write a message",
        };
        s.print(inner.x + 2, y, hint, theme::panel().fg(FAINT).italic(), field_w as u16);
    } else {
        let (shown, cursor) = app.chat_input.view(field_w);
        s.print(inner.x + 2, y, &shown, theme::panel(), field_w as u16);
        if typing {
            s.cursor = Some((inner.x + 2 + cursor as u16, y));
        }
    }
}

type Line = Vec<(String, Style)>;

fn draw_messages(s: &mut Screen, app: &App, r: Rect) {
    if r.h == 0 || r.w < 10 {
        return;
    }
    let total = app.messages.len();
    let end = total.saturating_sub(app.chat_scroll);
    let mut lines: Vec<(Line, Rgb)> = Vec::new();
    for msg in app.messages.range(..end).rev() {
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

    if app.chat_scroll > 0 {
        let label = format!(" ↓ {} newer · G to jump ", app.chat_scroll);
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

/// Word-wraps styled segments; continuation lines are indented.
fn wrap(segs: &[(String, Style)], width: usize, indent: usize) -> Vec<Line> {
    let indent = indent.min(width / 3);
    let mut lines: Vec<Line> = vec![Vec::new()];
    let mut col = 0;

    fn push(line: &mut Line, text: &str, style: Style) {
        match line.last_mut() {
            Some((t, s)) if *s == style => t.push_str(text),
            _ => line.push((text.to_string(), style)),
        }
    }

    for (text, style) in segs {
        // Tokens: runs of spaces or runs of non-spaces.
        let mut tokens: Vec<String> = Vec::new();
        for c in text.chars() {
            match tokens.last_mut() {
                Some(t) if (t.ends_with(' ')) == (c == ' ') => t.push(c),
                _ => tokens.push(c.to_string()),
            }
        }
        for token in tokens {
            let tw = str_width(&token);
            let is_space = token.starts_with(' ');
            if col + tw <= width {
                push(lines.last_mut().unwrap(), &token, *style);
                col += tw;
                continue;
            }
            if is_space {
                // Drop spaces at a line break.
                lines.push(vec![(" ".repeat(indent), *style)]);
                col = indent;
                continue;
            }
            if tw <= width - indent && col > indent {
                lines.push(vec![(" ".repeat(indent), *style)]);
                col = indent;
                push(lines.last_mut().unwrap(), &token, *style);
                col += tw;
                continue;
            }
            // Hard-break a long word.
            for c in token.chars() {
                let cw = char_width(c);
                if col + cw > width {
                    lines.push(vec![(" ".repeat(indent), *style)]);
                    col = indent;
                }
                push(lines.last_mut().unwrap(), &c.to_string(), *style);
                col += cw;
            }
        }
    }
    lines
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let segs = vec![(text.to_string(), theme::panel())];
    wrap(&segs, width.max(1), 0)
        .into_iter()
        .map(|line| line.into_iter().map(|(t, _)| t).collect())
        .collect()
}

// ---------------------------------------------------------------- help

fn draw_help(s: &mut Screen, app: &App) {
    let w = 72.min(s.w.saturating_sub(4));
    let h = 28.min(s.h.saturating_sub(2));
    let r = Rect::new((s.w - w) / 2, (s.h - h) / 2, w, h);
    let title: &[(&str, Style)] = &[("Help", theme::panel().fg(TEXT).bold())];
    let right: &[(&str, Style)] = &[("any key to close", theme::panel().fg(FAINT))];
    let inner = panel(s, r, title, right, true);
    let base = theme::panel();

    let keys: &[(&str, &str)] = &[
        ("tab", "switch channels / chat"),
        ("1  2", "following / search tab"),
        ("↑↓ jk", "move / scroll chat"),
        ("⏎", "play selected channel"),
        ("/", "filter following / search"),
        ("s", "search Twitch channels"),
        ("i", "write in chat"),
        ("space", "pause / resume audio"),
        ("+  -", "volume up / down"),
        ("m", "mute"),
        ("v", "spectrum / mirror / scope / video"),
        ("a", "video: real picture / ASCII"),
        ("c", "video quality"),
        ("z", "zoom the picture to the window"),
        ("f", "follow / unfollow channel"),
        ("o", "open channel in browser"),
        ("r", "refresh"),
        ("q", "quit"),
    ];
    let col_w = inner.w / 2;
    let rows = keys.len().div_ceil(2);
    for (i, (key, desc)) in keys.iter().enumerate() {
        let x = inner.x + if i < rows { 0 } else { col_w };
        let y = inner.y + 1 + (i % rows) as u16;
        if y >= inner.bottom() {
            continue;
        }
        s.print(x, y, key, base.fg(ACCENT_SOFT).bold(), 7);
        s.print(x + 8, y, desc, base.fg(MUTED), col_w.saturating_sub(9));
    }

    let mut y = inner.y + rows as u16 + 2;
    let logged_in = match &app.auth {
        Auth::LoggedIn(me) => format!("Logged in as {} — {}.", me.display_name, app.token_source.label()),
        Auth::Checking => "Logging in…".to_string(),
        Auth::Failed(e) => format!("Login failed ({}): {e}", app.token_source.label()),
        Auth::Anonymous => format!("Not logged in: {}.", app.token_source.label()),
    };
    let setup: Vec<(String, Style)> = vec![
        ("Logging in".into(), base.fg(TEXT).bold()),
        (String::new(), base),
        (logged_in, base.fg(if app.me().is_some() { GREEN } else { YELLOW })),
        (String::new(), base),
        (
            "The app logs in by itself with the Twitch cookie of your Firefox profile: just stay \
             logged in on twitch.tv in Firefox and restart the app. LibreWolf, Zen, Floorp and \
             Waterfox work too."
                .into(),
            base.fg(MUTED),
        ),
        (String::new(), base),
        (
            "You can also set a token by hand, or turn the cookie lookup off, in:".into(),
            base.fg(MUTED),
        ),
        (format!("  {}", crate::config::path().display()), base.fg(YELLOW)),
        (String::new(), base),
        ("What Twitch does not allow".into(), base.fg(TEXT).bold()),
        (String::new(), base),
        (
            format!(
                "Outside its own site Twitch shares only your {} newest and {0} oldest follows, \
                 and refuses follow/unfollow: press o to do that in the browser.",
                twitch_channels::FOLLOWS_LIMIT
            ),
            base.fg(MUTED),
        ),
    ];
    for (text, style) in setup {
        for line in wrap_plain(&text, inner.w as usize) {
            if y >= inner.bottom() {
                return;
            }
            s.print(inner.x, y, &line, style, inner.w);
            y += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words_with_indent() {
        let st = theme::panel();
        let lines = wrap(&[("aaaa bbbb cccc".to_string(), st)], 10, 2);
        let text: Vec<String> = lines.iter().map(|l| l.iter().map(|(t, _)| t.as_str()).collect()).collect();
        assert_eq!(text, vec!["aaaa bbbb ", "  cccc"]);
    }

    #[test]
    fn hard_breaks_long_words() {
        let st = theme::panel();
        let lines = wrap(&[("abcdefghijkl".to_string(), st)], 5, 0);
        assert_eq!(lines.len(), 3);
    }
}
