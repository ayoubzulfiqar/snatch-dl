//! Types shared between the IPC layer, the aria2 client and the UI.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::aria2::DownloadStatus;
use crate::gallery::GalleryEvent;
use crate::processor::MediaEvent;
use crate::torrent::TorrentSnapshot;

/// Schemes we are willing to hand to aria2. Anything else (`file:`, `data:`,
/// `javascript:` …) is rejected: this payload arrives from a web page.
const ALLOWED_SCHEMES: [&str; 4] = ["http", "https", "ftp", "ftps"];

/// What the browser (or a socket client) is asking Snatch to do.
///
/// Absent on the wire means [`JobKind::Download`], so a payload written for the
/// first release of the extension still parses unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobKind {
    /// A direct HTTP/FTP link for aria2.
    #[default]
    Download,
    /// A `magnet:` URI for the BitTorrent engine.
    Magnet,
    /// A page to hand to gallery-dl.
    Scrape,
    /// A watch page to hand to yt-dlp.
    Video,
    /// A page to scan for media, presenting a picker rather than downloading.
    Sniff,
    /// A question, not a job: what resolutions does this page offer? The
    /// answer comes back in the reply, and nothing is queued. This is what the
    /// button on a video asks before the user has picked anything.
    Formats,
    /// A stream address for ffmpeg to record, for the pages yt-dlp cannot
    /// read. Its scheme may be `rtmp:` or `rtsp:` as well as `http:`.
    Stream,
}

impl JobKind {
    pub fn label(self) -> &'static str {
        match self {
            JobKind::Download => "download",
            JobKind::Magnet => "torrent",
            JobKind::Scrape => "scrape",
            JobKind::Video => "video",
            JobKind::Sniff => "sniff",
            JobKind::Formats => "format listing",
            JobKind::Stream => "stream",
        }
    }
}

/// A download hand-off, as produced by the browser extension and forwarded by
/// `snatch-nmh`. Must stay wire-compatible with the struct of the same name in
/// the `snatch-nmh` crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    /// Which engine should take this. Defaults to [`JobKind::Download`].
    #[serde(default)]
    pub kind: JobKind,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// HTTP Basic or FTP username for a link that asks for one.
    ///
    /// Deliberately never persisted: it lives on the request only long enough
    /// to reach the engine, so nothing writes a password to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// A digest to check the finished file against, in any form a download
    /// page might print it. Parsed by [`crate::checksum::parse`] at the point
    /// of use, so the wire format stays whatever the sender had to hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// The exact yt-dlp format the user picked from the list a
    /// [`JobKind::Formats`] request returned. Ignored by every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_id: Option<String>,
    /// Which quality to record, named by height. Stream jobs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Stop the recording after this long. Stream jobs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_seconds: Option<u64>,
    /// Begin this far into the stream. Stream jobs only, and ignored for a
    /// live one, which has no beginning to measure from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_seconds: Option<u64>,
    /// Unix time this job should start at. A download is added paused until
    /// then, so it holds its place in the queue without using bandwidth; a
    /// recording waits as a visible job that can be cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Manifest addresses the browser watched this page's player fetch.
    ///
    /// Only consulted when yt-dlp cannot read the page. They are what makes
    /// the long tail work: a site yt-dlp has never heard of still has a player
    /// asking for an HLS or DASH manifest, and ffmpeg can read one of those
    /// without knowing anything about the site.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<String>,
    /// Plain media files the browser watched this page load, for the same
    /// reason as `streams` and consulted at the same time. A site yt-dlp has
    /// never heard of very often just serves an `.mp4`, and that wants the
    /// ordinary downloader rather than ffmpeg.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Additional sources for the *same* file. aria2 spreads its connections
    /// across every mirror and fails over between them, so a slow or flaky
    /// primary does not decide the speed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
}

impl DownloadRequest {
    /// Every source for this file: the primary followed by any mirrors,
    /// trimmed and deduplicated.
    pub fn sources(&self) -> Vec<String> {
        let mut sources = vec![self.url.trim().to_owned()];
        for mirror in &self.mirrors {
            let mirror = mirror.trim();
            if !mirror.is_empty() && !sources.iter().any(|existing| existing == mirror) {
                sources.push(mirror.to_owned());
            }
        }
        sources
    }

    /// The username and password to authenticate with, if one was given.
    ///
    /// A username is required, a password is not: anonymous FTP and a few
    /// HTTP endpoints take an empty one, and refusing that would block a
    /// working case for no reason.
    pub fn credentials(&self) -> Option<(String, String)> {
        let user = self.username.as_deref().map(str::trim).unwrap_or_default();
        if user.is_empty() {
            return None;
        }
        let password = self.password.clone().unwrap_or_default();
        Some((user.to_owned(), password))
    }

    /// A magnet link, for the torrent engine.
    pub fn magnet(url: impl Into<String>) -> Self {
        Self {
            kind: JobKind::Magnet,
            ..Self::from_url(url)
        }
    }

    /// A page for gallery-dl to scrape.
    pub fn scrape(url: impl Into<String>) -> Self {
        Self {
            kind: JobKind::Scrape,
            ..Self::from_url(url)
        }
    }

    /// A watch page for yt-dlp to extract.
    pub fn video(url: impl Into<String>) -> Self {
        Self {
            kind: JobKind::Video,
            ..Self::from_url(url)
        }
    }

    /// A stream address for ffmpeg to record.
    pub fn stream(url: impl Into<String>) -> Self {
        Self {
            kind: JobKind::Stream,
            ..Self::from_url(url)
        }
    }

    /// A page to scan for media.
    pub fn sniff(url: impl Into<String>) -> Self {
        Self {
            kind: JobKind::Sniff,
            ..Self::from_url(url)
        }
    }

    /// Infer the kind from the URL when the sender did not say.
    pub fn inferred_kind(&self) -> JobKind {
        if self.kind == JobKind::Download && self.url.trim_start().starts_with("magnet:") {
            return JobKind::Magnet;
        }
        self.kind
    }

    /// A bare request, as produced by the "Add download" dialog.
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            kind: JobKind::Download,
            url: url.into(),
            filename: None,
            cookies: None,
            referer: None,
            user_agent: None,
            username: None,
            password: None,
            checksum: None,
            format_id: None,
            height: None,
            record_seconds: None,
            skip_seconds: None,
            start_at: None,
            mime: None,
            size: None,
            streams: Vec::new(),
            files: Vec::new(),
            mirrors: Vec::new(),
        }
    }

    /// Reject anything the chosen engine should never be asked to fetch.
    pub fn validate(&self) -> Result<()> {
        let url = self.url.trim();
        if url.is_empty() {
            bail!("the URL is empty");
        }
        if self.inferred_kind() == JobKind::Magnet {
            // A magnet has no `://`, so it is checked on its own terms.
            if !url.starts_with("magnet:") {
                bail!("a torrent job needs a magnet: link");
            }
            if url.len() > 8192 || url.contains(['\r', '\n']) {
                bail!("the magnet link is malformed");
            }
            return Ok(());
        }
        if self.inferred_kind() == JobKind::Stream {
            // rtmp: and rtsp: are streams too, so the http-only rule below
            // would refuse addresses ffmpeg reads perfectly well.
            return crate::stream::validate_url(url);
        }
        if url.len() > 8192 {
            bail!("the URL is unreasonably long ({} bytes)", url.len());
        }
        if url.contains(['\r', '\n']) {
            bail!("the URL contains a line break");
        }

        let Some((scheme, rest)) = url.split_once("://") else {
            bail!(
                "the URL has no scheme (expected one of {})",
                ALLOWED_SCHEMES.join(", ")
            );
        };
        let scheme = scheme.to_ascii_lowercase();
        if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
            bail!("unsupported URL scheme '{scheme}'");
        }
        if rest.is_empty() {
            bail!("the URL has no host");
        }
        if let Some(format) = self.format_id.as_deref() {
            crate::ytdlp::validate_format(format)?;
        }
        Ok(())
    }

    /// The `out` option for aria2: a plain basename with no path components,
    /// control characters or leading dots.
    pub fn sanitized_filename(&self) -> Option<String> {
        let raw = self.filename.as_deref()?.trim();
        // Take the last path component so "../../.bashrc" can never escape.
        let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
        let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
        let cleaned = cleaned.trim().trim_start_matches('.').trim();
        if cleaned.is_empty() {
            return None;
        }
        Some(clamp_bytes(cleaned, 200))
    }

    /// A human-readable label, used for toasts before an engine reports a path.
    pub fn display_name(&self) -> String {
        if let Some(name) = self.sanitized_filename() {
            return name;
        }
        // A magnet has no path to take a basename from; its `dn` parameter is
        // the display name the standard provides for exactly this purpose.
        // Without one, show the whole link — `name_from_url` would reduce it
        // to the bare string "magnet:", which identifies nothing.
        if self.inferred_kind() == JobKind::Magnet {
            return magnet_display_name(&self.url)
                .unwrap_or_else(|| clamp_bytes(self.url.trim(), 200));
        }
        name_from_url(&self.url).unwrap_or_else(|| self.url.trim().to_owned())
    }
}

/// Pull `dn=` (display name) out of a magnet link, percent/plus decoded.
pub fn magnet_display_name(magnet: &str) -> Option<String> {
    let query = magnet.split_once('?')?.1;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "dn" || value.is_empty() {
            continue;
        }
        // `+` is a space in a query string; everything else is percent-encoded.
        let decoded = percent_decode_lossy(&value.replace('+', " "));
        let trimmed = decoded.trim();
        if !trimmed.is_empty() {
            return Some(clamp_bytes(trimmed, 200));
        }
    }
    None
}

/// Best-effort percent decoding; invalid escapes are kept verbatim.
fn percent_decode_lossy(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Derive a filename from the path component of a URL.
pub fn name_from_url(url: &str) -> Option<String> {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let last = path.rsplit('/').next()?.trim();
    if last.is_empty() {
        None
    } else {
        Some(clamp_bytes(last, 200))
    }
}

/// Truncate to at most `max` bytes without splitting a UTF-8 character.
fn clamp_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// The line-delimited JSON answer sent back over the Unix socket. Must stay
/// wire-compatible with `Reply` in the `snatch-nmh` crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The page's title, when a [`JobKind::Formats`] request asked for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Its length in seconds, so the picker can show which video it read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    /// What the page offers, best first. Empty for every other kind, and
    /// omitted from the wire entirely so an ordinary hand-off is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<crate::ytdlp::MediaFormat>,
}

impl IpcResponse {
    pub fn accepted(gid: String) -> Self {
        Self {
            ok: true,
            gid: Some(gid),
            error: None,
            title: None,
            duration: None,
            formats: Vec::new(),
        }
    }

    pub fn rejected(error: String) -> Self {
        Self {
            ok: false,
            gid: None,
            error: Some(error),
            title: None,
            duration: None,
            formats: Vec::new(),
        }
    }

    /// The answer to "what resolutions does this page have?".
    pub fn listing(probe: crate::ytdlp::MediaProbe) -> Self {
        Self {
            ok: true,
            gid: None,
            error: None,
            title: probe.title,
            duration: probe.duration,
            formats: probe.formats,
        }
    }
}

/// Everything the background tasks push at the GLib main loop.
#[derive(Debug)]
pub enum UiEvent {
    /// A job was accepted by an engine (from the browser or the CLI socket).
    /// `kind` lets the window show the page that job landed on.
    Added { name: String, kind: JobKind },
    /// A page arrived that the user should pick media from.
    SniffRequested { url: String },
    /// A full picture of every download aria2 currently knows about.
    Snapshot(Vec<DownloadStatus>),
    /// The aria2 RPC endpoint is answering.
    Aria2Up(String),
    /// aria2 is unavailable; the string explains why.
    Aria2Down(String),
    /// A signal asked us to shut down.
    Quit,
    /// A fresh picture of every torrent in the session.
    Torrents(Vec<TorrentSnapshot>),
    /// The BitTorrent session could not be started.
    TorrentsUnavailable(String),
    /// Progress from a `gallery-dl` batch.
    Gallery(GalleryEvent),
    /// Progress from an `ffmpeg` job.
    Media(MediaEvent),
    /// Progress from a `yt-dlp` extraction.
    Video(crate::ytdlp::VideoEvent),
    /// Progress from a wget download.
    Wget(crate::wget::WgetEvent),
    /// An archive being unpacked after its download finished.
    Archive(crate::archive::ArchiveEvent),
    /// A recursive crawl of a site.
    Mirror(crate::mirror::MirrorEvent),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_names_come_from_the_dn_parameter() {
        let request = DownloadRequest::magnet(
            "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny",
        );
        // Without this the toast would read "Added magnet:".
        assert_eq!(request.display_name(), "Big Buck Bunny");
    }

    #[test]
    fn magnet_names_are_percent_decoded() {
        let request =
            DownloadRequest::magnet("magnet:?xt=urn:btih:abc&dn=Cosmos%20Laundromat%20%282015%29");
        assert_eq!(request.display_name(), "Cosmos Laundromat (2015)");
    }

    #[test]
    fn a_magnet_without_dn_falls_back_to_the_link() {
        let request = DownloadRequest::magnet("magnet:?xt=urn:btih:abc");
        assert_eq!(request.display_name(), "magnet:?xt=urn:btih:abc");
    }

    #[test]
    fn kind_is_inferred_from_a_magnet_scheme() {
        // The extension may omit `kind` entirely.
        let request = DownloadRequest::from_url("magnet:?xt=urn:btih:abc");
        assert_eq!(request.kind, JobKind::Download);
        assert_eq!(request.inferred_kind(), JobKind::Magnet);
        assert!(request.validate().is_ok());
    }

    #[test]
    fn magnets_are_validated_on_their_own_terms() {
        // A magnet has no "://", so the http path would have rejected it.
        assert!(
            DownloadRequest::magnet("magnet:?xt=urn:btih:abc")
                .validate()
                .is_ok()
        );
        assert!(
            DownloadRequest::magnet("https://example.com/x")
                .validate()
                .is_err()
        );
        assert!(
            DownloadRequest::magnet("magnet:?dn=a\r\nX: y")
                .validate()
                .is_err(),
            "a CRLF must never reach the engine"
        );
    }

    #[test]
    fn absent_kind_still_parses_from_an_older_extension() {
        let request: DownloadRequest =
            serde_json::from_str(r#"{"url":"https://example.com/a.zip"}"#)
                .expect("a payload without `kind` must still parse");
        assert_eq!(request.kind, JobKind::Download);
        assert_eq!(request.inferred_kind(), JobKind::Download);
    }
}

#[cfg(test)]
mod mirror_tests {
    use super::*;

    #[test]
    fn a_plain_request_has_one_source() {
        let request = DownloadRequest::from_url("https://a.example/f.iso");
        assert_eq!(request.sources(), vec!["https://a.example/f.iso"]);
    }

    #[test]
    fn mirrors_follow_the_primary_and_deduplicate() {
        let mut request = DownloadRequest::from_url("https://a.example/f.iso");
        request.mirrors = vec![
            "  https://b.example/f.iso ".to_owned(),
            // A repeat of the primary would make aria2 open two connections
            // to the same host thinking they were different mirrors.
            "https://a.example/f.iso".to_owned(),
            String::new(),
            "https://c.example/f.iso".to_owned(),
        ];
        assert_eq!(
            request.sources(),
            vec![
                "https://a.example/f.iso",
                "https://b.example/f.iso",
                "https://c.example/f.iso",
            ]
        );
    }
}
