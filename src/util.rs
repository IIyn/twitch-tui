//! Formatting and time helpers.

// Local time, for the chat's timestamps.
#[cfg(target_os = "linux")]
use std::os::raw::{c_char, c_int, c_long};

#[cfg(target_os = "linux")]
#[repr(C)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: c_long,
    tm_zone: *const c_char,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
}

/// `HH:MM` in the local timezone.
#[cfg(target_os = "linux")]
pub fn local_hm(ts: i64) -> String {
    let mut tm = unsafe { std::mem::zeroed::<Tm>() };
    if unsafe { localtime_r(&ts, &mut tm) }.is_null() {
        return "--:--".into();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut command = if cfg!(windows) {
        // Rather than through cmd, which would split the URL on `&`.
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    command.arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}

/// Parses `YYYY-MM-DDTHH:MM:SS(.fff)Z` into a unix timestamp.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil, Howard Hinnant's algorithm.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// 950, 12.3k, 1.2M
pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => trim_decimal(n as f64 / 1_000.0, "k"),
        _ => trim_decimal(n as f64 / 1_000_000.0, "M"),
    }
}

fn trim_decimal(v: f64, suffix: &str) -> String {
    if v >= 100.0 {
        format!("{v:.0}{suffix}")
    } else {
        format!("{:.1}{suffix}", (v * 10.0).floor() / 10.0).replace(".0", "")
    }
}

/// 12,345
#[cfg(target_os = "linux")]
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 3:04:05 or 4:05
#[cfg(target_os = "linux")]
pub fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// `12m`, `1h05`: compact enough for a list.
pub fn short_duration(secs: i64) -> String {
    let mins = secs.max(0) / 60;
    if mins < 60 { format!("{mins}m") } else { format!("{}h{:02}", mins / 60, mins % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(12_345), "12.3k");
        assert_eq!(compact(1_661_107), "1.6M");
        assert_eq!(compact(2_000), "2k");
        assert_eq!(short_duration(59), "0m");
        assert_eq!(short_duration(12 * 60 + 5), "12m");
        assert_eq!(short_duration(3600 + 5 * 60), "1h05");
        assert_eq!(parse_iso8601("2023-11-14T22:13:20Z"), Some(1_700_000_000));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn formats_player_values() {
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(duration(3 * 3600 + 4 * 60 + 5), "3:04:05");
    }
}
