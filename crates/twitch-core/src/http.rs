//! HTTPS through the system `curl` binary (the standard library has no TLS).
//! The request is passed as a curl config file on stdin so that tokens never
//! show up in the process list.

use std::io::Write;
use std::process::{Command, Stdio};

pub struct Response {
    pub status: u16,
    pub body: String,
}

fn config_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn request(url: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<Response, String> {
    let mut config = format!("url = {}\n", config_quote(url));
    for (name, value) in headers {
        config.push_str(&format!("header = {}\n", config_quote(&format!("{name}: {value}"))));
    }
    if let Some(body) = body {
        config.push_str(&format!("data-binary = {}\n", config_quote(body)));
    }

    let mut child = Command::new("curl")
        .args(["-sS", "--compressed", "--max-time", "15", "-w", "\n%{http_code}", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;

    child
        .stdin
        .take()
        .ok_or("curl stdin unavailable")?
        .write_all(config.as_bytes())
        .map_err(|e| format!("curl write failed: {e}"))?;

    let output = child.wait_with_output().map_err(|e| format!("curl failed: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(err.trim().trim_start_matches("curl: ").to_string());
    }

    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    Ok(Response {
        status: code.trim().parse().unwrap_or(0),
        body: body.to_string(),
    })
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
