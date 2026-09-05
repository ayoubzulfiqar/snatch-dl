//! Live streams, for the sites yt-dlp is weakest on.
//!
//! yt-dlp is built around archival: given a page, work out the file and fetch
//! it. streamlink is built around the opposite job — given a page, work out
//! what is being broadcast *right now* and hand back somewhere to read it
//! from. On Twitch, Kick, chzzk, SOOP and a hundred others that difference
//! shows: streamlink resolves a channel that yt-dlp either misreads or
//! refuses, and it keeps working across the ad breaks and mid-roll splices
//! that make a live channel awkward.
//!
//! Snatch uses it as a **resolver only**. `streamlink --json` reports the
//! qualities a page is broadcasting and a playlist address for each, and
//! those go into the same picker rows and the same ffmpeg recording as every
//! other stream. Nothing here downloads anything: the code that records is
//! the code that already records, which is what keeps the stop button, the
//! scheduling and the container choice working the same way for these.
//!
//! Absent, it costs nothing — the whole pass is skipped and the reader is
//! never told about a tool they do not have.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::network::{Engine, ProxyManager};

/// Long enough for a plugin that has to sign in or follow a redirect chain,
/// short enough that a wrong guess does not hold up the picker.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);

/// streamlink names one entry per quality plus these aliases for the ends of
/// the list. They point at a quality that is already listed, so offering them
/// would put the same stream in the picker three times.
const ALIASES: [&str; 4] = ["best", "worst", "best-unfiltered", "worst-unfiltered"];

pub fn binary() -> String {
    std::env::var("SNATCH_STREAMLINK")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "streamlink".to_owned())
}

/// One quality a page is broadcasting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broadcast {
    /// streamlink's own name for it: `1080p60`, `720p`, `audio_only`.
    pub name: String,
    /// A playlist ffmpeg can open.
    pub url: String,
    /// Pixel height, when the name carries one.
    pub height: Option<u32>,
    /// True when the name says there is no picture.
    pub audio_only: bool,
}

/// What streamlink found on a page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// The site plugin that matched, for the log.
    pub plugin: Option<String>,
    pub title: Option<String>,
    /// Best first.
    pub streams: Vec<Broadcast>,
}

// --- what `--json` prints ---------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawOutput {
    #[serde(default)]
    plugin: Option<String>,
    #[serde(default)]
    metadata: RawMetadata,
    #[serde(default)]
    streams: std::collections::BTreeMap<String, RawStream>,
    /// Present instead of `streams` when the plugin refused.
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMetadata {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    /// Absent for a `muxed-stream`, which names its parts in `substreams`
    /// instead. Those have no single address to record, so they are skipped
    /// and the site's other qualities are offered.
    #[serde(default)]
    url: Option<String>,
}

/// Read a height out of streamlink's name for a quality.
///
/// `1080p60` is 1080, `720p` is 720, `audio_only` has none, and `source` --
/// which is what Twitch calls the broadcaster's own feed -- has none either
/// even though it is the best one there is. A row without a height still
/// sorts and still records; it just cannot be matched against "give me 720p".
fn height_of(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    // `1080p60` -> 1080, but `2` from something like `2ch` is not a height.
    name[digits.len()..]
        .starts_with('p')
        .then(|| digits.parse().ok())
        .flatten()
        .filter(|height: &u32| (16..=8640).contains(height))
}

impl Resolved {
    fn from_raw(raw: RawOutput) -> Result<Self> {
        if let Some(error) = raw.error {
            bail!("streamlink could not read that page: {error}");
        }

        // Read before the map below takes ownership of it.
        let title = raw.title_of();

        let mut streams: Vec<Broadcast> = raw
            .streams
            .into_iter()
            .filter(|(name, _)| !ALIASES.iter().any(|alias| name.eq_ignore_ascii_case(alias)))
            .filter_map(|(name, stream)| {
                let url = stream.url?;
                if crate::stream::validate_url(&url).is_err() {
                    return None;
                }
                Some(Broadcast {
                    height: height_of(&name),
                    audio_only: name.to_ascii_lowercase().contains("audio"),
                    name,
                    url,
                })
            })
            .collect();

        // Tallest first, then the ones with no height (`source` is usually
        // the best thing on offer), then sound-only last.
        streams.sort_by(|a, b| {
            // `false` sorts before `true`, which puts anything with a picture
            // ahead of anything without one.
            a.audio_only
                .cmp(&b.audio_only)
                .then(b.height.cmp(&a.height))
                .then(a.name.cmp(&b.name))
        });

        Ok(Self {
            plugin: raw.plugin,
            title,
            streams,
        })
    }
}

impl RawOutput {
    fn title_of(&self) -> Option<String> {
        self.metadata
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
    }
}

/// Ask streamlink what a page is broadcasting.
///
/// Errors when streamlink is missing, does not recognise the page, or finds
/// nothing live on it — all of which are ordinary and none of which are worth
/// showing anyone. The caller logs and carries on down the chain.
pub async fn resolve(
    url: &str,
    headers: &crate::stream::Headers,
    proxies: &ProxyManager,
) -> Result<Resolved> {
    let url = url.trim();
    crate::stream::validate_url(url)?;

    let binary = binary();
    let mut command = Command::new(&binary);
    command
        .arg("--json")
        // One go. The picker is waiting, and a channel that is not live now
        // will not become live inside the next few seconds.
        .arg("--retry-max")
        .arg("0")
        // Never read the user's streamlink config: it can set a player, an
        // output file or an ad-hoc plugin directory, none of which belong in
        // a listing that is only supposed to answer a question.
        .arg("--no-config");

    if let Some(referer) = clean(headers.referer.as_deref()) {
        command
            .arg("--http-header")
            .arg(format!("Referer={referer}"));
    }
    if let Some(agent) = clean(headers.user_agent.as_deref()) {
        command
            .arg("--http-header")
            .arg(format!("User-Agent={agent}"));
    }
    if let Some(cookies) = clean(headers.cookies.as_deref()) {
        command
            .arg("--http-header")
            .arg(format!("Cookie={cookies}"));
    }
    for (name, value) in &headers.extra {
        let (Some(name), Some(value)) = (clean(Some(name)), clean(Some(value))) else {
            continue;
        };
        command.arg("--http-header").arg(format!("{name}={value}"));
    }

    if let Some(proxy) = proxies
        .resolve_for("streamlink", Engine::Subprocess)
        .context("streamlink cannot use the configured proxy")?
    {
        command.arg("--http-proxy").arg(proxy.url());
    }

    command
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{binary} is not installed");
        }
        Err(error) => return Err(error).context("could not start streamlink"),
    };

    let output = timeout(RESOLVE_TIMEOUT, child.wait_with_output())
        .await
        .context("streamlink took too long to answer")?
        .context("could not read streamlink's answer")?;

    // A page it does not recognise exits non-zero and still prints the JSON
    // saying so, which is the more useful message of the two.
    let parsed: RawOutput = serde_json::from_slice(&output.stdout).map_err(|_| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("streamlink printed nothing readable");
        anyhow::anyhow!("{}", reason.trim())
    })?;

    let resolved = Resolved::from_raw(parsed)?;
    if resolved.streams.is_empty() {
        bail!("streamlink found nothing being broadcast there");
    }
    Ok(resolved)
}

/// An argument value safe to pass along. These arrive from a web page.
///
/// streamlink takes headers as one `name=value` argument, so a value carrying
/// a newline could otherwise introduce a second header.
fn clean(value: Option<&str>) -> Option<&str> {
    let value = value.map(str::trim).filter(|value| !value.is_empty())?;
    if value.contains(['\r', '\n', '\0']) || value.len() > 4096 || value.starts_with('-') {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `streamlink --json` actually prints, captured from 8.5.0.
    const REAL: &str = r#"{
      "plugin": "hls",
      "metadata": {"id": null, "author": null, "category": null, "title": "Test Feed"},
      "streams": {
        "1080p": {"type": "hls", "url": "https://c.example/1080.m3u8",
                  "headers": {"User-Agent": "Mozilla/5.0"},
                  "master": "https://c.example/master.m3u8"},
        "720p60": {"type": "hls", "url": "https://c.example/720.m3u8"},
        "audio_only": {"type": "hls", "url": "https://c.example/audio.m3u8"},
        "best": {"type": "hls", "url": "https://c.example/1080.m3u8"},
        "worst": {"type": "hls", "url": "https://c.example/720.m3u8"}
      }
    }"#;

    fn parse(json: &str) -> Result<Resolved> {
        Resolved::from_raw(serde_json::from_str(json).expect("the fixture is valid JSON"))
    }

    #[test]
    fn reads_the_qualities_a_page_is_broadcasting() {
        let resolved = parse(REAL).expect("the fixture resolves");
        assert_eq!(resolved.plugin.as_deref(), Some("hls"));
        assert_eq!(resolved.title.as_deref(), Some("Test Feed"));

        let names: Vec<&str> = resolved
            .streams
            .iter()
            .map(|stream| stream.name.as_str())
            .collect();
        // Tallest first, sound-only last, and `best`/`worst` gone: they point
        // at qualities already in the list, so keeping them would offer the
        // same broadcast three times.
        assert_eq!(names, vec!["1080p", "720p60", "audio_only"]);
        assert_eq!(resolved.streams[0].height, Some(1080));
        assert_eq!(resolved.streams[1].height, Some(720));
        assert!(resolved.streams[2].audio_only);
        assert_eq!(resolved.streams[2].height, None);
    }

    #[test]
    fn a_quality_with_no_single_address_is_skipped_not_offered() {
        // A `muxed-stream` names its parts in `substreams` and has no `url`.
        // ffmpeg cannot be pointed at it, so the row would fail on click.
        let resolved = parse(
            r#"{"plugin":"x","streams":{
                 "1080p":{"type":"muxed-stream","substreams":[{"type":"hls"}]},
                 "480p":{"type":"hls","url":"https://c.example/480.m3u8"}}}"#,
        )
        .expect("the rest still resolves");
        assert_eq!(resolved.streams.len(), 1);
        assert_eq!(resolved.streams[0].name, "480p");
    }

    #[test]
    fn a_plugin_that_refused_says_why() {
        let error = parse(r#"{"plugin":"twitch","error":"This channel is offline"}"#)
            .expect_err("an error field is a failure");
        assert!(format!("{error:#}").contains("This channel is offline"));
    }

    #[test]
    fn an_address_streamlink_invented_is_not_recorded() {
        // Whatever streamlink prints is still checked before it reaches
        // ffmpeg, the same as every other address Snatch is handed.
        let resolved = parse(
            r#"{"plugin":"x","streams":{
                 "1080p":{"type":"hls","url":"file:///etc/passwd"},
                 "720p":{"type":"hls","url":"https://c.example/720.m3u8"}}}"#,
        )
        .expect("the good one survives");
        assert_eq!(resolved.streams.len(), 1);
        assert_eq!(resolved.streams[0].name, "720p");
    }

    #[test]
    fn heights_come_from_the_name_and_only_when_they_are_one() {
        assert_eq!(height_of("1080p60"), Some(1080));
        assert_eq!(height_of("720p"), Some(720));
        assert_eq!(height_of("160p"), Some(160));
        // Twitch's name for the broadcaster's own feed. No height, still the
        // best thing on offer.
        assert_eq!(height_of("source"), None);
        assert_eq!(height_of("audio_only"), None);
        // Not a height: a channel count, and a number with nothing after it.
        assert_eq!(height_of("2ch"), None);
        assert_eq!(height_of("1080"), None);
    }

    #[test]
    fn a_header_value_that_could_smuggle_another_one_is_dropped() {
        assert_eq!(clean(Some(" token ")), Some("token"));
        assert_eq!(clean(Some("a\r\nX-Evil: b")), None);
        assert_eq!(clean(Some("-oops")), None);
        assert_eq!(clean(Some("")), None);
    }
}
