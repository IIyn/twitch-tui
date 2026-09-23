//! Drawing: the header, the channel lists, the footer and the help. The
//! player's panels, Linux only, are drawn by `player`.

#[cfg(target_os = "linux")]
mod player;

use crate::app::{App, Auth, Focus, Level, Load, Tab, Typing};
use term::{Rect, Rgb, Screen, Style, char_width, str_width, truncate};
use twitch_channels::Channel;
use twitch_core::now_secs;

use crate::theme::{self, *};

use crate::util;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub struct Layout {
    header: Rect,
    sidebar: Option<Rect>,
    #[cfg(target_os = "linux")]
    player: player::Areas,
    footer: Rect,
}

impl Layout {
    // Outside Linux there is only the list to lay out.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables, unused_mut))]
    pub fn compute(w: u16, h: u16, app: &App) -> Layout {
        let body = Rect::new(0, 1, w, h.saturating_sub(2));
        let mut layout = Layout {
            header: Rect::new(0, 0, w, 1),
            sidebar: Some(body),
            #[cfg(target_os = "linux")]
            player: player::Areas::default(),
            footer: Rect::new(0, h.saturating_sub(1), w, 1),
        };
        #[cfg(target_os = "linux")]
        if !app.channels_only {
            (layout.sidebar, layout.player) = player::split(body, app);
        }
        layout
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
    #[cfg(target_os = "linux")]
    player::draw(s, app, &layout.player, frame);
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

    let tagline = if app.channels_only { "  channels only" } else { "  audio-only twitch" };
    if logo_end + str_width(tagline) as u16 + 2 < x {
        s.print(logo_end, r.y, tagline, bar.fg(FAINT).italic(), x - logo_end);
    }
}

fn draw_footer(s: &mut Screen, app: &App, r: Rect) {
    let bar = Style::new(MUTED, BG);
    s.fill(r, bar);

    let hints: &[(&str, &str)] = match (app.typing, app.focus) {
        (None, _) if app.channels_only => &[
            ("↑↓", "move"),
            ("⏎", "open in browser"),
            ("/", "filter"),
            ("s", "search"),
            ("1 2", "tabs"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
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

    let per_page = (list.h / 2).max(1) as usize;
    let selected = app.selected_index();
    let offset = selected.saturating_sub(per_page - 1);
    #[cfg(target_os = "linux")]
    let playing = app.player.current.as_ref().map(|c| c.channel.login.as_str());
    #[cfg(not(target_os = "linux"))]
    let playing = None;
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

type Line = Vec<(String, Style)>;

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

    let keys: &[(&str, &str)] = if app.channels_only {
        &[
            ("1  2", "following / search tab"),
            ("↑↓ jk", "move"),
            ("⏎  o", "open channel in browser"),
            ("/", "filter following / search"),
            ("s", "search Twitch channels"),
            ("r", "refresh"),
            ("q", "quit"),
        ]
    } else {
        &[
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
        ]
    };
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
        Auth::LoggedIn(me) => format!("Logged in as {}.", me.display_name),
        Auth::Checking => "Logging in…".to_string(),
        Auth::Failed(e) => format!("Login failed: {e}"),
        Auth::Anonymous => "Not logged in.".to_string(),
    };
    let setup: Vec<(String, Style)> = vec![
        ("Logging in".into(), base.fg(TEXT).bold()),
        (String::new(), base),
        (logged_in, base.fg(if app.me().is_some() { GREEN } else { YELLOW })),
        (String::new(), base),
        (
            "Quit and run `twitch-tui --login`, then approve the code it shows on twitch.tv. \
             This gives your followed channels and lets you write in chat."
                .into(),
            base.fg(MUTED),
        ),
        (String::new(), base),
        (format!("Website token: {}.", app.token_source.label()), base.fg(MUTED)),
        (
            "Optional: read from your Firefox profile (LibreWolf, Zen, Floorp and Waterfox work \
             too), it brings your subscriber perks to playback. It can be set by hand, or its \
             lookup turned off, in:"
                .into(),
            base.fg(MUTED),
        ),
        (format!("  {}", crate::config::path().display()), base.fg(YELLOW)),
        (String::new(), base),
        ("What Twitch does not allow".into(), base.fg(TEXT).bold()),
        (String::new(), base),
        (
            "Following and unfollowing is reserved to Twitch's own site: press o to do it in the \
             browser."
                .into(),
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
