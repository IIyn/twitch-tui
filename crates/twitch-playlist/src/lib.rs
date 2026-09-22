//! Resolving a live channel into HLS renditions.

use twitch_core::http::{self, url_encode};
use twitch_core::json::quote;
use twitch_core::{Api, CLIENT_ID};

/// Resolves the HLS renditions of a live channel: the audio-only one for
/// playback, and every one carrying video for the picture.
pub fn stream_urls(api: &Api, login: &str) -> Result<StreamUrls, String> {
    let query = "query($l: String!) { streamPlaybackAccessToken(channelName: $l, params: \
                 { platform: \"web\", playerBackend: \"mediaplayer\", playerType: \"site\" }) \
                 { value signature } }";
    let data = api.gql(query, &format!("{{\"l\":{}}}", quote(login)))?;
    let token = data.get("streamPlaybackAccessToken");
    let value = token.get("value").as_str().ok_or("no playback token (channel offline?)")?;
    let sig = token.get("signature").str_or("");

    let url = format!(
        "https://usher.ttvnw.net/api/channel/hls/{}.m3u8?client_id={CLIENT_ID}&token={}&sig={}\
         &allow_source=true&allow_audio_only=true&fast_bread=true&player=twitchweb&p={}",
        url_encode(login),
        url_encode(value),
        url_encode(sig),
        std::process::id(),
    );
    let resp = http::request(&url, &[], None)?;
    if resp.status == 404 {
        return Err(format!("{login} is offline"));
    }
    if resp.status != 200 {
        return Err(format!("playlist request failed (HTTP {})", resp.status));
    }
    let audio = pick_audio_variant(&resp.body).ok_or("no playable variant in playlist")?;
    Ok(StreamUrls { audio, video: video_renditions(&resp.body) })
}

#[derive(Clone, Debug)]
pub struct StreamUrls {
    pub audio: String,
    /// Smallest first; empty when the playlist only carries audio.
    pub video: Vec<Rendition>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    /// As Twitch names it, e.g. `720p60`.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
    pub url: String,
}

/// Picture quality asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    /// The cheapest rendition that still has more detail than a terminal
    /// grid can show (a 160p feed looks mushy once converted).
    Auto,
    /// The tallest rendition at most this tall.
    MaxHeight(u32),
}

const AUTO_MIN_HEIGHT: u32 = 480;

impl Quality {
    pub fn parse(value: &str) -> Option<Quality> {
        match value {
            "auto" => Some(Quality::Auto),
            "source" | "best" => Some(Quality::MaxHeight(u32::MAX)),
            _ => value.trim_end_matches('p').parse().ok().map(Quality::MaxHeight),
        }
    }

    pub fn pick(self, renditions: &[Rendition]) -> Option<&Rendition> {
        let cheapest_at = |height: u32| {
            renditions.iter().filter(|r| r.height == height).min_by_key(|r| r.bandwidth)
        };
        let tallest_up_to = |max: u32| renditions.iter().map(|r| r.height).filter(|h| *h <= max).max();
        let height = match self {
            Quality::Auto => renditions.iter().map(|r| r.height).filter(|h| *h >= AUTO_MIN_HEIGHT).min(),
            Quality::MaxHeight(max) => tallest_up_to(max),
        };
        // Nothing fits: take whatever comes closest.
        let height = height.or_else(|| match self {
            Quality::Auto => tallest_up_to(u32::MAX),
            Quality::MaxHeight(_) => renditions.iter().map(|r| r.height).min(),
        })?;
        cheapest_at(height)
    }
}

fn video_renditions(master: &str) -> Vec<Rendition> {
    let lines: Vec<&str> = master.lines().map(str::trim).collect();
    let mut renditions = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else { continue };
        if attrs.contains("VIDEO=\"audio_only\"") {
            continue;
        }
        let Some(uri) = lines[i + 1..].iter().find(|l| !l.is_empty() && !l.starts_with('#')) else {
            continue;
        };
        let attribute = |name: &str| {
            attrs
                .split(',')
                .find_map(|kv| kv.trim().strip_prefix(name))
                .map(|v| v.trim_matches('"').to_string())
        };
        let (width, height) = attribute("RESOLUTION=")
            .and_then(|r| r.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
            .unwrap_or((0, 0));
        let bandwidth = attribute("BANDWIDTH=").and_then(|b| b.parse().ok()).unwrap_or(u64::MAX);
        let fps = attribute("FRAME-RATE=").and_then(|f| f.parse::<f32>().ok()).map(|f| f.round() as u32);
        // The source rendition is called `chunked`, which tells nothing.
        let name = match attribute("VIDEO=") {
            Some(name) if name != "chunked" => name,
            _ => format!("{height}p{} source", fps.filter(|f| *f != 30).map(|f| f.to_string()).unwrap_or_default()),
        };
        renditions.push(Rendition { name, width, height, bandwidth, url: uri.to_string() });
    }
    renditions.sort_by_key(|r| (r.height, r.bandwidth));
    renditions
}

/// Picks the audio_only rendition from a master playlist, falling back to the
/// lowest bandwidth variant (ffmpeg drops the video anyway).
fn pick_audio_variant(master: &str) -> Option<String> {
    let lines: Vec<&str> = master.lines().map(str::trim).collect();
    let mut best: Option<(u64, &str)> = None;
    for (i, line) in lines.iter().enumerate() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else { continue };
        let Some(uri) = lines[i + 1..].iter().find(|l| !l.is_empty() && !l.starts_with('#')) else {
            continue;
        };
        if attrs.contains("VIDEO=\"audio_only\"") {
            return Some(uri.to_string());
        }
        let bandwidth = attrs
            .split(',')
            .find_map(|kv| kv.strip_prefix("BANDWIDTH="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(u64::MAX);
        if best.is_none_or(|(b, _)| bandwidth < b) {
            best = Some((bandwidth, uri));
        }
    }
    best.map(|(_, uri)| uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_audio_only() {
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=6000000,VIDEO=\"chunked\"\nhttp://src\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=160000,CODECS=\"mp4a.40.2\",VIDEO=\"audio_only\"\nhttp://audio\n";
        assert_eq!(pick_audio_variant(m).as_deref(), Some("http://audio"));
    }

    fn pick(quality: Quality, master: &str) -> Option<String> {
        quality.pick(&video_renditions(master)).map(|r| r.url.clone())
    }

    const FULL: &str = "#EXT-X-STREAM-INF:BANDWIDTH=7946000,RESOLUTION=1920x1080,FRAME-RATE=60.000,VIDEO=\"chunked\"\nhttp://src\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=3422000,RESOLUTION=1280x720,VIDEO=\"720p60\"\nhttp://720p60\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=2373000,RESOLUTION=1280x720,VIDEO=\"720p30\"\nhttp://720p30\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=1427000,RESOLUTION=852x480,VIDEO=\"480p30\"\nhttp://mid\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=230000,RESOLUTION=284x160,VIDEO=\"160p30\"\nhttp://small\n\
                        #EXT-X-STREAM-INF:BANDWIDTH=160000,VIDEO=\"audio_only\"\nhttp://audio\n";

    #[test]
    fn picks_detailed_enough_video() {
        assert_eq!(pick(Quality::Auto, FULL).as_deref(), Some("http://mid"));
        assert_eq!(pick_audio_variant(FULL).as_deref(), Some("http://audio"));
    }

    #[test]
    fn falls_back_to_tallest_small_video() {
        let m = "#EXT-X-STREAM-INF:BANDWIDTH=230000,RESOLUTION=284x160,VIDEO=\"160p30\"\nhttp://small\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=630000,RESOLUTION=640x360,VIDEO=\"360p30\"\nhttp://mid\n";
        assert_eq!(pick(Quality::Auto, m).as_deref(), Some("http://mid"));
    }

    #[test]
    fn picks_asked_quality() {
        assert_eq!(pick(Quality::MaxHeight(720), FULL).as_deref(), Some("http://720p30"));
        assert_eq!(pick(Quality::MaxHeight(600), FULL).as_deref(), Some("http://mid"));
        assert_eq!(pick(Quality::MaxHeight(u32::MAX), FULL).as_deref(), Some("http://src"));
        assert_eq!(pick(Quality::MaxHeight(100), FULL).as_deref(), Some("http://small"));
        assert_eq!(Quality::parse("720p"), Some(Quality::MaxHeight(720)));
        assert_eq!(video_renditions(FULL).last().map(|r| r.name.as_str()), Some("1080p60 source"));
    }

    #[test]
    fn falls_back_to_lowest_bandwidth() {
        let m = "#EXT-X-STREAM-INF:BANDWIDTH=900\nhttp://b\n#EXT-X-STREAM-INF:BANDWIDTH=300\nhttp://a\n";
        assert_eq!(pick_audio_variant(m).as_deref(), Some("http://a"));
    }
}

