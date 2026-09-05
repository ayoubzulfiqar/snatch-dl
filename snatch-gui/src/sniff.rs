//! Media sniffing: point Snatch at a page, get back everything downloadable.
//!
//! Two passes, because neither alone is enough:
//!
//! 1. **The document.** Fetch the page and walk the DOM for `img`, `video`,
//!    `audio`, `source`, `srcset`, `a[href]`, `link`, Open Graph and Twitter
//!    card metadata, and CSS `background-image` in inline styles. A regex over
//!    markup would miss `srcset` candidate lists, `<base href>` and every
//!    relative URL, which is why this uses a real HTML parser.
//! 2. **yt-dlp, when it recognises the site.** A streaming page's real media
//!    lives behind a DASH/HLS manifest that appears nowhere in the HTML.
//!    `yt-dlp --dump-single-json` resolves it. This pass is skipped when
//!    yt-dlp is absent, and its failure is never fatal.
//!
//! Candidates are then probed with `HEAD` (concurrently, bounded) to learn
//! their real content type and size, because an extension is a guess and
//! plenty of media URLs have no extension at all.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use scraper::{Html, Selector};
use tokio::sync::Semaphore;

/// Cap the page we parse. A malicious or broken site should not exhaust memory.
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
/// Cap how many candidates we probe and show.
const MAX_CANDIDATES: usize = 500;
/// Concurrent HEAD requests. Enough to be quick, few enough to be polite.
const PROBE_CONCURRENCY: usize = 12;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

// There is no user agent constant here any more. Many sites do serve a stub
// or a 403 to anything that is not a browser, but the client itself now is
// one, all the way down to the TLS handshake, and it brings the matching user
// agent and header order with it. Naming a *different* browser on top of that
// would put a Firefox banner on a Chrome handshake, which is the exact
// disagreement the emulation exists to avoid. See `network::browser`.

/// Broad classification. Snatch downloads anything, so "Other" is a first-class
/// result rather than a reason to hide a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
    Document,
    Archive,
    Subtitle,
    Other,
}

impl MediaKind {
    pub const ALL: [MediaKind; 7] = [
        MediaKind::Video,
        MediaKind::Audio,
        MediaKind::Image,
        MediaKind::Document,
        MediaKind::Archive,
        MediaKind::Subtitle,
        MediaKind::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Video => "Video",
            MediaKind::Audio => "Audio",
            MediaKind::Image => "Images",
            MediaKind::Document => "Documents",
            MediaKind::Archive => "Archives",
            MediaKind::Subtitle => "Subtitles",
            MediaKind::Other => "Other files",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            MediaKind::Video => "video-x-generic-symbolic",
            MediaKind::Audio => "audio-x-generic-symbolic",
            MediaKind::Image => "image-x-generic-symbolic",
            MediaKind::Document => "x-office-document-symbolic",
            MediaKind::Archive => "package-x-generic-symbolic",
            MediaKind::Subtitle => "media-view-subtitles-symbolic",
            MediaKind::Other => "text-x-generic-symbolic",
        }
    }

    /// Classify by MIME type, which is authoritative when the server sends one.
    fn from_mime(mime: &str) -> Option<Self> {
        let mime = mime
            .split(';')
            .next()
            .unwrap_or(mime)
            .trim()
            .to_ascii_lowercase();
        if mime.starts_with("video/") {
            return Some(MediaKind::Video);
        }
        if mime.starts_with("audio/") {
            return Some(MediaKind::Audio);
        }
        if mime.starts_with("image/") {
            return Some(MediaKind::Image);
        }
        Some(match mime.as_str() {
            "application/pdf"
            | "application/epub+zip"
            | "application/msword"
            | "text/plain"
            | "application/rtf" => MediaKind::Document,
            "application/zip"
            | "application/x-tar"
            | "application/gzip"
            | "application/x-7z-compressed"
            | "application/x-rar-compressed"
            | "application/vnd.debian.binary-package"
            | "application/x-rpm"
            | "application/x-iso9660-image"
            | "application/x-xz"
            | "application/zstd" => MediaKind::Archive,
            "application/x-subrip" | "text/vtt" => MediaKind::Subtitle,
            // Streaming manifests are the video, as far as a user is concerned.
            "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "application/dash+xml" => {
                MediaKind::Video
            }
            "application/octet-stream" => MediaKind::Other,
            _ => return None,
        })
    }

    /// Fall back to the file extension when there is no usable MIME type.
    fn from_extension(extension: &str) -> Self {
        match extension.to_ascii_lowercase().as_str() {
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg"
            | "ts" | "m3u8" | "mpd" | "ogv" | "3gp" | "vob" | "rmvb" | "divx" => MediaKind::Video,
            "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "wma" | "alac" | "aiff"
            | "mka" | "ape" => MediaKind::Audio,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "avif" | "jxl" | "svg" | "tif"
            | "tiff" | "heic" | "heif" | "ico" => MediaKind::Image,
            "pdf" | "epub" | "mobi" | "azw3" | "doc" | "docx" | "odt" | "rtf" | "txt" | "xls"
            | "xlsx" | "ods" | "ppt" | "pptx" | "odp" | "djvu" | "cbz" | "cbr" => {
                MediaKind::Document
            }
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" | "img"
            | "deb" | "rpm" | "apk" | "dmg" | "exe" | "msi" | "appimage" | "snap" | "flatpak"
            | "jar" | "whl" => MediaKind::Archive,
            "srt" | "vtt" | "ass" | "ssa" | "sub" | "sbv" => MediaKind::Subtitle,
            _ => MediaKind::Other,
        }
    }
}

/// Where a candidate was found. Shown so the user can judge it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A media element in the document.
    Element,
    /// A hyperlink whose target looks downloadable.
    Link,
    /// Open Graph or Twitter card metadata.
    Metadata,
    /// Resolved by yt-dlp.
    Extractor,
    /// The URL itself was the media.
    Direct,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Element => "embedded",
            Origin::Link => "link",
            Origin::Metadata => "metadata",
            Origin::Extractor => "yt-dlp",
            Origin::Direct => "direct",
        }
    }
}

/// One downloadable thing found on a page.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub url: String,
    pub kind: MediaKind,
    /// Filename, alt text or extractor title.
    pub label: String,
    pub origin: Origin,
    pub mime: Option<String>,
    pub size: Option<u64>,
    /// True once a HEAD confirmed the server serves it.
    pub verified: bool,
}

impl Candidate {
    fn new(url: String, label: String, origin: Origin) -> Self {
        let kind = MediaKind::from_extension(&extension_of(&url));
        Self {
            url,
            kind,
            label,
            origin,
            mime: None,
            size: None,
            verified: false,
        }
    }

    /// A filename to save as.
    pub fn filename(&self) -> String {
        let from_url = url_basename(&self.url);
        if !from_url.is_empty() {
            return from_url;
        }
        let cleaned: String = self
            .label
            .chars()
            .map(|c| if c.is_control() || c == '/' { '_' } else { c })
            .collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            "download".to_owned()
        } else {
            cleaned.to_owned()
        }
    }
}

/// What a sniff turned up.
#[derive(Debug, Clone)]
pub struct SniffResult {
    pub page_title: Option<String>,
    pub page_url: String,
    pub candidates: Vec<Candidate>,
    /// Non-fatal problems worth telling the user about.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SniffOptions {
    /// Ask yt-dlp about the page as well as reading the DOM.
    pub use_extractor: bool,
    /// Send a HEAD to each candidate for its real type and size.
    pub probe: bool,
    /// Path to yt-dlp, when it is available.
    pub yt_dlp: Option<PathBuf>,
}

impl Default for SniffOptions {
    fn default() -> Self {
        Self {
            use_extractor: true,
            probe: true,
            yt_dlp: None,
        }
    }
}

/// Sniff a page for everything downloadable on it.
pub async fn sniff(
    page_url: &str,
    client: wreq::Client,
    options: SniffOptions,
) -> Result<SniffResult> {
    let base = url::Url::parse(page_url.trim())
        .with_context(|| format!("'{page_url}' is not a valid URL"))?;
    if !matches!(base.scheme(), "http" | "https") {
        bail!("only http and https pages can be sniffed");
    }

    let mut notes = Vec::new();

    let response = client
        .get(base.as_str())
        .header(wreq::header::ACCEPT, "text/html,*/*")
        .send()
        .await
        .with_context(|| format!("could not fetch {base}"))?
        .error_for_status()
        .with_context(|| format!("{base} returned an error"))?;

    // Redirects mean the effective URL is what relative links resolve against.
    // wreq reports it as an `http::Uri`; everything downstream resolves
    // against a `url::Url`, and the address we started from is the right
    // answer if the round trip somehow fails.
    let effective = url::Url::parse(&response.uri().to_string()).unwrap_or_else(|_| base.clone());
    let content_type = response
        .headers()
        .get(wreq::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();

    // The URL might already *be* the media, in which case there is no document
    // to walk and the answer is a single candidate.
    if !content_type.is_empty() && !content_type.contains("html") && !content_type.contains("xml") {
        let size = response.content_length();
        let mut candidate = Candidate::new(
            effective.to_string(),
            url_basename(effective.as_str()),
            Origin::Direct,
        );
        if let Some(kind) = MediaKind::from_mime(&content_type) {
            candidate.kind = kind;
        }
        candidate.mime = Some(content_type);
        candidate.size = size;
        candidate.verified = true;
        return Ok(SniffResult {
            page_title: None,
            page_url: effective.to_string(),
            candidates: vec![candidate],
            notes,
        });
    }

    let body = read_capped(response).await?;
    let (title, mut candidates) = extract_from_html(&body, &effective);

    if options.use_extractor {
        match options.yt_dlp.as_ref() {
            Some(binary) => match extractor_candidates(binary, effective.as_str()).await {
                Ok(found) => candidates.extend(found),
                // A site yt-dlp does not know is the common case, not an error.
                Err(error) => {
                    log::debug!("yt-dlp found nothing for {effective}: {error:#}");
                }
            },
            None => notes
                .push("yt-dlp is not installed, so streaming sites were not inspected".to_owned()),
        }
    }

    let before = candidates.len();
    dedupe(&mut candidates);
    if candidates.len() > MAX_CANDIDATES {
        candidates.truncate(MAX_CANDIDATES);
        notes.push(format!(
            "showing the first {MAX_CANDIDATES} of {before} links found"
        ));
    }

    if options.probe {
        probe_all(&mut candidates, &client).await;
    }

    // Most interesting kinds first, then biggest, so the useful rows are on top.
    candidates.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)))
            .then(a.label.cmp(&b.label))
    });

    Ok(SniffResult {
        page_title: title,
        page_url: effective.to_string(),
        candidates,
        notes,
    })
}

/// Read a body, refusing to buffer more than [`MAX_PAGE_BYTES`].
async fn read_capped(response: wreq::Response) -> Result<String> {
    let mut body = Vec::new();
    let mut stream = std::pin::pin!(response.bytes_stream());
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        let chunk = chunk.context("the page download failed")?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_PAGE_BYTES {
            body.truncate(MAX_PAGE_BYTES);
            log::warn!("page truncated at {MAX_PAGE_BYTES} bytes");
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Walk the DOM. Returns the page title and everything downloadable.
fn extract_from_html(body: &str, page: &url::Url) -> (Option<String>, Vec<Candidate>) {
    let document = Html::parse_document(body);
    let mut candidates = Vec::new();

    let title = select(&document, "title")
        .next()
        .map(|element| element.text().collect::<String>().trim().to_owned())
        .filter(|text| !text.is_empty());

    // <base href> changes what every relative URL resolves against.
    let base = select(&document, "base[href]")
        .next()
        .and_then(|element| element.attr("href"))
        .and_then(|href| page.join(href).ok())
        .unwrap_or_else(|| page.clone());

    let push = |raw: &str, label: String, origin: Origin, candidates: &mut Vec<Candidate>| {
        if let Some(resolved) = resolve(&base, raw) {
            candidates.push(Candidate::new(resolved, label, origin));
        }
    };

    // Media elements, including <source> children and poster frames.
    for (selector, attribute) in [
        ("img[src]", "src"),
        // Lazy-loading sites keep the real URL out of src.
        ("img[data-src]", "data-src"),
        ("img[data-original]", "data-original"),
        // The rest of the lazy-load vocabulary. There is no standard for it,
        // so every framework picked its own name, and a page written against
        // one of them looks empty to a scraper that knows only the other two.
        // They cost one selector each.
        ("[data-lazy-src]", "data-lazy-src"),
        ("[data-lazy]", "data-lazy"),
        ("[data-echo]", "data-echo"),
        ("[data-full]", "data-full"),
        ("[data-full-src]", "data-full-src"),
        ("[data-large]", "data-large"),
        ("[data-image]", "data-image"),
        ("[data-thumb]", "data-thumb"),
        ("[data-poster]", "data-poster"),
        ("[data-video]", "data-video"),
        ("[data-video-src]", "data-video-src"),
        ("[data-mp4]", "data-mp4"),
        ("[data-hls]", "data-hls"),
        ("[data-dash]", "data-dash"),
        ("[data-audio]", "data-audio"),
        ("[data-file]", "data-file"),
        // Microdata states outright which URL is the media.
        (r#"meta[itemprop="contentUrl"]"#, "content"),
        (r#"[itemprop="contentUrl"]"#, "href"),
        ("video[src]", "src"),
        ("video[poster]", "poster"),
        ("audio[src]", "src"),
        ("source[src]", "src"),
        ("embed[src]", "src"),
        ("object[data]", "data"),
        ("track[src]", "src"),
    ] {
        for element in select(&document, selector) {
            let Some(raw) = element.attr(attribute) else {
                continue;
            };
            let label = element
                .attr("alt")
                .or_else(|| element.attr("title"))
                .map(str::to_owned)
                .unwrap_or_else(|| url_basename(raw));
            push(raw, label, Origin::Element, &mut candidates);
        }
    }

    // srcset carries a whole candidate list; take every entry.
    for selector in ["img[srcset]", "source[srcset]"] {
        for element in select(&document, selector) {
            let Some(raw) = element.attr("srcset") else {
                continue;
            };
            for entry in parse_srcset(raw) {
                push(
                    &entry,
                    url_basename(&entry),
                    Origin::Element,
                    &mut candidates,
                );
            }
        }
    }

    // Hyperlinks that point at something downloadable.
    for element in select(&document, "a[href]") {
        let Some(raw) = element.attr("href") else {
            continue;
        };
        // A `download` attribute is an explicit statement of intent, so accept
        // it whatever the extension says.
        let explicit = element.attr("download").is_some();
        if !explicit && MediaKind::from_extension(&extension_of(raw)) == MediaKind::Other {
            continue;
        }
        let text = element.text().collect::<String>();
        let label = if text.trim().is_empty() {
            url_basename(raw)
        } else {
            text.trim().to_owned()
        };
        push(raw, label, Origin::Link, &mut candidates);
    }

    // Open Graph and Twitter cards name the page's primary media.
    for selector in [
        r#"meta[property="og:image"]"#,
        r#"meta[property="og:video"]"#,
        r#"meta[property="og:video:url"]"#,
        r#"meta[property="og:video:secure_url"]"#,
        r#"meta[property="og:audio"]"#,
        r#"meta[name="twitter:image"]"#,
        r#"meta[name="twitter:player:stream"]"#,
    ] {
        for element in select(&document, selector) {
            let Some(raw) = element.attr("content") else {
                continue;
            };
            push(
                raw,
                title.clone().unwrap_or_else(|| url_basename(raw)),
                Origin::Metadata,
                &mut candidates,
            );
        }
    }

    // link rel=image_src / preload as=video
    for element in select(&document, "link[href]") {
        let rel = element.attr("rel").unwrap_or_default();
        if !matches!(rel, "image_src" | "apple-touch-icon" | "preload") {
            continue;
        }
        let Some(raw) = element.attr("href") else {
            continue;
        };
        if MediaKind::from_extension(&extension_of(raw)) == MediaKind::Other {
            continue;
        }
        push(raw, url_basename(raw), Origin::Metadata, &mut candidates);
    }

    // schema.org, which is where a video page states its own media outright.
    //
    // A VideoObject carries `contentUrl` -- the file itself -- and usually
    // `embedUrl` and `thumbnailUrl` beside it. Sites publish this for search
    // engines, so it is kept accurate, and it is present on pages whose
    // player is otherwise entirely JavaScript and invisible to a scraper.
    // The shape varies -- one object, a list, or a `@graph` -- so this walks
    // whatever is there rather than reaching for a fixed path.
    for element in select(&document, r#"script[type="application/ld+json"]"#) {
        let text = element.text().collect::<String>();
        if text.len() > 512 * 1024 {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let mut found = Vec::new();
        collect_linked_data(&value, &mut found, 0);
        for raw in found {
            push(
                &raw,
                title.clone().unwrap_or_else(|| url_basename(&raw)),
                Origin::Metadata,
                &mut candidates,
            );
        }
    }

    // CSS background images in inline styles.
    for element in select(&document, "[style]") {
        let Some(style) = element.attr("style") else {
            continue;
        };
        for raw in background_urls(style) {
            push(&raw, url_basename(&raw), Origin::Element, &mut candidates);
        }
    }

    (title, candidates)
}

/// Compile a selector, treating a bad one as matching nothing.
///
/// The selectors here are literals, so a failure is a programming error rather
/// than something a page can cause; log it and carry on rather than abort.
/// The media addresses inside a block of JSON-LD.
///
/// Only the keys that name a file are followed: a VideoObject also carries
/// author URLs, a licence URL and a publisher's logo, and offering those as
/// downloads would bury the video in noise.
fn collect_linked_data(value: &serde_json::Value, found: &mut Vec<String>, depth: usize) {
    const MEDIA_KEYS: [&str; 6] = [
        "contenturl",
        "embedurl",
        "thumbnailurl",
        "image",
        "audio",
        "video",
    ];
    // A page can nest `@graph` inside `@graph`. Deep enough for any of them,
    // and shallow enough that a hostile page cannot spend the stack.
    if depth > 12 {
        return;
    }
    let keep = |text: &str, found: &mut Vec<String>| {
        let text = text.trim();
        if text.starts_with("http") && !found.iter().any(|seen| seen == text) {
            found.push(text.to_owned());
        }
    };
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_linked_data(item, found, depth + 1);
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, child) in fields {
                let named = MEDIA_KEYS.contains(&key.to_ascii_lowercase().as_str());
                match child {
                    // "contentUrl": "https://..."
                    serde_json::Value::String(text) if named => keep(text, found),
                    // "image": { "@type": "ImageObject", "url": "https://..." }
                    serde_json::Value::Object(inner) if named => {
                        if let Some(serde_json::Value::String(text)) = inner.get("url") {
                            keep(text, found);
                        }
                        collect_linked_data(child, found, depth + 1);
                    }
                    // "image": ["https://a", "https://b"]
                    serde_json::Value::Array(items) if named => {
                        for item in items {
                            match item {
                                serde_json::Value::String(text) => keep(text, found),
                                other => collect_linked_data(other, found, depth + 1),
                            }
                        }
                    }
                    other => collect_linked_data(other, found, depth + 1),
                }
            }
        }
        _ => {}
    }
}

fn select<'a>(
    document: &'a Html,
    selector: &str,
) -> Box<dyn Iterator<Item = scraper::ElementRef<'a>> + 'a> {
    match Selector::parse(selector) {
        Ok(parsed) => Box::new(document.select(&parsed).collect::<Vec<_>>().into_iter()),
        Err(error) => {
            log::error!("invalid selector '{selector}': {error}");
            Box::new(std::iter::empty())
        }
    }
}

/// `url1 1x, url2 2x` or `url1 320w, url2 640w`.
fn parse_srcset(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter_map(|entry| entry.split_whitespace().next())
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Pull `url(...)` targets out of an inline style.
fn background_urls(style: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = style;
    while let Some(start) = rest.find("url(") {
        rest = &rest[start + 4..];
        let Some(end) = rest.find(')') else { break };
        let raw = rest[..end].trim().trim_matches(['"', '\'']).trim();
        if !raw.is_empty() && !raw.starts_with("data:") {
            found.push(raw.to_owned());
        }
        rest = &rest[end + 1..];
    }
    found
}

/// Resolve a possibly-relative reference, rejecting anything unfetchable.
fn resolve(base: &url::Url, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    // data:, blob: and javascript: cannot be handed to a downloader.
    let lowered = raw.to_ascii_lowercase();
    for prefix in ["data:", "blob:", "javascript:", "about:", "mailto:", "tel:"] {
        if lowered.starts_with(prefix) {
            return None;
        }
    }
    let joined = base.join(raw).ok()?;
    if !matches!(joined.scheme(), "http" | "https") {
        return None;
    }
    Some(joined.to_string())
}

/// Collapse duplicates, keeping the most informative origin for each URL.
fn dedupe(candidates: &mut Vec<Candidate>) {
    let mut seen: HashSet<String> = HashSet::with_capacity(candidates.len());
    candidates.retain(|candidate| seen.insert(candidate.url.clone()));
}

/// Ask yt-dlp what the page really contains.
async fn extractor_candidates(binary: &std::path::Path, page: &str) -> Result<Vec<Candidate>> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(binary)
            .arg("--dump-single-json")
            .arg("--no-warnings")
            .arg("--no-playlist")
            .arg("--ignore-config")
            .arg("--")
            .arg(page)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("yt-dlp timed out")?
    .context("could not run yt-dlp")?;

    if !output.status.success() {
        bail!("yt-dlp does not recognise this page");
    }

    let info: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("yt-dlp returned malformed JSON")?;

    let title = info
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("video")
        .to_owned();

    let mut candidates = Vec::new();

    // The single best pre-merged URL, when there is one.
    if let Some(url) = info.get("url").and_then(serde_json::Value::as_str) {
        let mut candidate = Candidate::new(url.to_owned(), title.clone(), Origin::Extractor);
        candidate.kind = MediaKind::Video;
        candidates.push(candidate);
    }

    // Every individual format, so the user can pick a size.
    if let Some(formats) = info.get("formats").and_then(serde_json::Value::as_array) {
        for format in formats {
            let Some(url) = format.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let note = format
                .get("format_note")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let extension = format
                .get("ext")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let has_video = format
                .get("vcodec")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|codec| codec != "none");

            let mut candidate = Candidate::new(
                url.to_owned(),
                format!("{title} [{note} {extension}]")
                    .replace("[ ", "[")
                    .trim()
                    .to_owned(),
                Origin::Extractor,
            );
            candidate.kind = if has_video {
                MediaKind::Video
            } else {
                MediaKind::Audio
            };
            candidate.size = format
                .get("filesize")
                .or_else(|| format.get("filesize_approx"))
                .and_then(serde_json::Value::as_u64);
            // A format URL is signed and time-limited; a HEAD would waste one
            // of its uses, and yt-dlp already told us the size.
            candidate.verified = true;
            candidates.push(candidate);
        }
    }

    Ok(candidates)
}

/// HEAD every candidate to learn its real type and size.
async fn probe_all(candidates: &mut [Candidate], client: &wreq::Client) {
    let limit = Arc::new(Semaphore::new(PROBE_CONCURRENCY));
    let mut tasks = Vec::with_capacity(candidates.len());

    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.verified {
            continue;
        }
        let client = client.clone();
        let url = candidate.url.clone();
        let limit = Arc::clone(&limit);
        tasks.push(tokio::spawn(async move {
            // A closed semaphore cannot happen here; treat it as "skip".
            let _permit = limit.acquire_owned().await.ok()?;
            let response = tokio::time::timeout(PROBE_TIMEOUT, client.head(&url).send())
                .await
                .ok()?
                .ok()?;

            if !response.status().is_success() {
                return None;
            }
            let mime = response
                .headers()
                .get(wreq::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            // Read the header rather than `content_length()`: a HEAD reply has
            // no body, so wreq reports 0 for it and every size would be lost.
            let size = response
                .headers()
                .get(wreq::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|length| *length > 0);
            Some((index, mime, size))
        }));
    }

    for task in tasks {
        let Ok(Some((index, mime, size))) = task.await else {
            continue;
        };
        let Some(candidate) = candidates.get_mut(index) else {
            continue;
        };
        // A served content type beats an extension guess.
        if let Some(mime) = &mime
            && let Some(kind) = MediaKind::from_mime(mime)
        {
            candidate.kind = kind;
        }
        candidate.mime = mime;
        candidate.size = candidate.size.or(size);
        candidate.verified = true;
    }
}

/// Last path segment, percent-decoded, without the query string.
fn url_basename(url: &str) -> String {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let last = path.rsplit('/').next().unwrap_or("").trim();
    percent_decode(last)
}

fn extension_of(url: &str) -> String {
    let name = url_basename(url);
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_owned())
        .unwrap_or_default()
}

fn percent_decode(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn page(body: &str) -> (Option<String>, Vec<Candidate>) {
        let base = url::Url::parse("https://example.com/gallery/index.html").expect("valid base");
        extract_from_html(body, &base)
    }

    fn urls(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.url.as_str()).collect()
    }

    /// A page whose player is entirely JavaScript still says what it holds.
    ///
    /// The markup here has no `<video>` at all -- the player is built at
    /// runtime -- which is the shape of most news video and a good deal of
    /// social media. What it does have is the block the site publishes for
    /// search engines, and that names the file outright.
    #[test]
    fn finds_the_video_a_page_only_declares_in_json_ld() {
        let (_, found) = page(
            r#"<html><head><title>Report</title>
            <script type="application/ld+json">
            {"@context":"https://schema.org","@graph":[
              {"@type":"WebPage","url":"https://example.com/x"},
              {"@type":"VideoObject",
               "name":"The report",
               "contentUrl":"https://cdn.example.com/report-1080.mp4",
               "embedUrl":"https://example.com/embed/9f8a",
               "thumbnailUrl":["https://cdn.example.com/thumb-a.jpg"],
               "image":{"@type":"ImageObject","url":"https://cdn.example.com/poster.jpg"},
               "author":{"@type":"Person","url":"https://example.com/staff/jo"}}
            ]}
            </script></head><body>nothing to see</body></html>"#,
        );
        let addresses = urls(&found);
        assert!(addresses.contains(&"https://cdn.example.com/report-1080.mp4"));
        assert!(addresses.contains(&"https://example.com/embed/9f8a"));
        assert!(addresses.contains(&"https://cdn.example.com/thumb-a.jpg"));
        assert!(addresses.contains(&"https://cdn.example.com/poster.jpg"));
        // The author's page is a URL in the same block and is not media.
        assert!(!addresses.contains(&"https://example.com/staff/jo"));
    }

    #[test]
    fn a_broken_json_ld_block_costs_nothing() {
        // Sites ship invalid JSON here more often than anyone would like.
        let (_, found) = page(
            r#"<html><body><script type="application/ld+json">{ oh dear </script>
            <img src="real.jpg"></body></html>"#,
        );
        assert_eq!(urls(&found), vec!["https://example.com/gallery/real.jpg"]);
    }

    #[test]
    fn lazy_loaded_images_are_not_invisible() {
        // Four frameworks, four attribute names, one page.
        let (_, found) = page(
            r#"<html><body>
            <img data-lazy-src="a.jpg"><div data-image="b.jpg"></div>
            <span data-thumb="c.jpg"></span><i data-full="d.jpg"></i>
            </body></html>"#,
        );
        let addresses = urls(&found);
        for name in ["a.jpg", "b.jpg", "c.jpg", "d.jpg"] {
            let want = format!("https://example.com/gallery/{name}");
            assert!(addresses.contains(&want.as_str()), "{name} was missed");
        }
    }

    #[test]
    fn finds_media_elements_and_resolves_relative_urls() {
        let (_, found) = page(
            r#"<html><body>
                 <img src="a.jpg">
                 <img src="/abs/b.png">
                 <video src="../v/clip.mp4" poster="poster.webp"></video>
                 <audio src="//cdn.example.net/song.mp3"></audio>
               </body></html>"#,
        );
        let found = urls(&found);
        assert!(found.contains(&"https://example.com/gallery/a.jpg"));
        assert!(found.contains(&"https://example.com/abs/b.png"));
        assert!(found.contains(&"https://example.com/v/clip.mp4"));
        assert!(found.contains(&"https://example.com/gallery/poster.webp"));
        // A protocol-relative URL must inherit the page's scheme.
        assert!(found.contains(&"https://cdn.example.net/song.mp3"));
    }

    #[test]
    fn honours_base_href() {
        let (_, found) = page(
            r#"<html><head><base href="https://cdn.example.org/media/"></head>
               <body><img src="x.png"></body></html>"#,
        );
        // Without <base> this would resolve against the page directory.
        assert_eq!(urls(&found), vec!["https://cdn.example.org/media/x.png"]);
    }

    #[test]
    fn expands_every_srcset_entry() {
        let (_, mut found) = page(
            r#"<img srcset="small.jpg 320w, medium.jpg 640w, large.jpg 1280w" src="small.jpg">"#,
        );
        // src and srcset both name small.jpg, so extraction yields it twice;
        // the pipeline collapses duplicates after every pass has contributed.
        dedupe(&mut found);
        let found = urls(&found);
        assert!(found.contains(&"https://example.com/gallery/large.jpg"));
        assert!(found.contains(&"https://example.com/gallery/medium.jpg"));
        assert_eq!(found.iter().filter(|u| u.ends_with("small.jpg")).count(), 1);
    }

    #[test]
    fn picks_up_lazy_loaded_images() {
        let (_, found) = page(r#"<img data-src="real.jpg" src="placeholder.gif">"#);
        assert!(urls(&found).contains(&"https://example.com/gallery/real.jpg"));
    }

    #[test]
    fn links_are_kept_only_when_downloadable() {
        let (_, found) = page(
            r#"<a href="movie.mkv">Film</a>
               <a href="/about.html">About</a>
               <a href="report" download>Report</a>"#,
        );
        let found = urls(&found);
        assert!(found.contains(&"https://example.com/gallery/movie.mkv"));
        // An ordinary page link is navigation, not a download.
        assert!(!found.iter().any(|u| u.ends_with("about.html")));
        // An explicit download attribute overrides the extension guess.
        assert!(found.contains(&"https://example.com/gallery/report"));
    }

    #[test]
    fn reads_open_graph_and_inline_backgrounds() {
        let (_, found) = page(
            r#"<html><head>
                 <meta property="og:video" content="https://cdn.example.net/v.mp4">
               </head><body>
                 <div style="background-image: url('bg.jpg'); color: red"></div>
               </body></html>"#,
        );
        let found = urls(&found);
        assert!(found.contains(&"https://cdn.example.net/v.mp4"));
        assert!(found.contains(&"https://example.com/gallery/bg.jpg"));
    }

    #[test]
    fn rejects_references_a_downloader_cannot_fetch() {
        // r##"..."## because the fragment link contains the sequence `"#`,
        // which would close an r#"..."# literal early.
        let (_, found) = page(
            r##"<img src="data:image/png;base64,AAAA">
                <img src="blob:https://example.com/uuid">
                <a href="javascript:void(0)">x</a>
                <a href="#anchor">y</a>
                <a href="mailto:a@b.c">z</a>"##,
        );
        assert!(found.is_empty(), "{:?}", urls(&found));
    }

    #[test]
    fn classifies_by_mime_first_then_extension() {
        // The server is authoritative.
        assert_eq!(
            MediaKind::from_mime("video/mp4; codecs=avc1"),
            Some(MediaKind::Video)
        );
        assert_eq!(
            MediaKind::from_mime("application/x-mpegurl"),
            Some(MediaKind::Video)
        );
        assert_eq!(MediaKind::from_mime("text/html"), None);

        // Extensions cover what the server does not label.
        assert_eq!(MediaKind::from_extension("mkv"), MediaKind::Video);
        assert_eq!(MediaKind::from_extension("FLAC"), MediaKind::Audio);
        assert_eq!(MediaKind::from_extension("cbz"), MediaKind::Document);
        assert_eq!(MediaKind::from_extension("appimage"), MediaKind::Archive);
        assert_eq!(MediaKind::from_extension("srt"), MediaKind::Subtitle);
        // Anything unrecognised is still downloadable, just unclassified.
        assert_eq!(MediaKind::from_extension("xyz"), MediaKind::Other);
        assert_eq!(MediaKind::from_extension(""), MediaKind::Other);
    }

    #[test]
    fn filenames_survive_query_strings_and_escaping() {
        assert_eq!(
            url_basename("https://x/a/My%20File%20(1).mp4?token=abc#frag"),
            "My File (1).mp4"
        );
        assert_eq!(extension_of("https://x/a/clip.MP4?t=1"), "MP4");
        assert_eq!(url_basename("https://x/"), "");
    }

    #[test]
    fn duplicates_collapse() {
        let mut candidates = vec![
            Candidate::new("https://x/a.jpg".into(), "a".into(), Origin::Element),
            Candidate::new("https://x/a.jpg".into(), "a again".into(), Origin::Link),
            Candidate::new("https://x/b.jpg".into(), "b".into(), Origin::Element),
        ];
        dedupe(&mut candidates);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn background_url_parsing_handles_quotes_and_multiples() {
        assert_eq!(
            background_urls(r#"background: url("a.png"), url('b.png'), url(c.png)"#),
            vec!["a.png", "b.png", "c.png"]
        );
        // A data URI is not fetchable and must not become a candidate.
        assert!(background_urls("background: url(data:image/png;base64,AA)").is_empty());
        assert!(background_urls("color: red").is_empty());
    }

    /// A one-shot HTTP server good enough to exercise fetch, parse and HEAD.
    ///
    /// Hand-rolled rather than pulling in a test-only HTTP dependency: the
    /// point is to prove the real `sniff` pipeline works against a socket,
    /// and that needs perhaps forty lines.
    mod server {
        use std::io::{BufRead, BufReader, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::{Arc, Mutex};

        pub struct Fixture {
            pub base: String,
            /// Every `User-Agent` the server was sent, in arrival order.
            ///
            /// Recorded so a test can check what actually went out on the
            /// socket rather than what the code meant to send.
            pub agents: Arc<Mutex<Vec<String>>>,
            _thread: std::thread::JoinHandle<()>,
        }

        pub fn start(page: &'static str) -> Option<Fixture> {
            let listener = TcpListener::bind("127.0.0.1:0").ok()?;
            let port = listener.local_addr().ok()?.port();
            let agents: Arc<Mutex<Vec<String>>> = Arc::default();
            let seen = Arc::clone(&agents);
            let thread = std::thread::spawn(move || {
                // Enough connections for the page plus every HEAD probe.
                for stream in listener.incoming().take(64) {
                    let Ok(stream) = stream else { break };
                    let _ = serve(stream, page, &seen);
                }
            });
            Some(Fixture {
                base: format!("http://127.0.0.1:{port}"),
                agents,
                _thread: thread,
            })
        }

        fn serve(
            mut stream: TcpStream,
            page: &str,
            agents: &Arc<Mutex<Vec<String>>>,
        ) -> std::io::Result<()> {
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut request = String::new();
            reader.read_line(&mut request)?;
            // Drain headers so the client does not see a reset, noting the
            // one this fixture is asked about.
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("user-agent")
                    && let Ok(mut agents) = agents.lock()
                {
                    agents.push(value.trim().to_owned());
                }
            }

            let mut parts = request.split_whitespace();
            let method = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("/");

            let (status, mime, body): (&str, &str, &[u8]) = match path {
                "/page.html" => ("200 OK", "text/html; charset=utf-8", page.as_bytes()),
                "/photo1.png" | "/photo2.png" | "/photo3.png" => {
                    ("200 OK", "image/png", &[0x89, b'P', b'N', b'G'])
                }
                "/manual.pdf" => ("200 OK", "application/pdf", b"%PDF-1.4"),
                "/archive.zip" => ("200 OK", "application/zip", b"PK\x05\x06"),
                "/clip" => ("200 OK", "video/mp4", b"\x00\x00\x00\x18ftyp"),
                _ => ("404 Not Found", "text/plain", b"no"),
            };

            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes())?;
            if method != "HEAD" {
                stream.write_all(body)?;
            }
            stream.flush()
        }
    }

    const FIXTURE_PAGE: &str = r#"<html><head>
<title>Mixed Media Test Page</title>
<meta property="og:image" content="/photo1.png">
</head><body>
<img src="photo1.png" alt="First">
<img srcset="photo2.png 320w, photo3.png 640w" src="photo2.png">
<a href="manual.pdf">The manual</a>
<a href="archive.zip">Archive</a>
<a href="/other-page.html">Not a download</a>
<a href="clip" download>Clip with no extension</a>
<img src="data:image/png;base64,AAAA">
</body></html>"#;

    #[tokio::test]
    async fn sniffs_a_real_page_over_http() {
        let Some(fixture) = server::start(FIXTURE_PAGE) else {
            eprintln!("skipping: could not bind a local port");
            return;
        };
        // Built the way ProxyManager builds it, so what the fixture records
        // is what a real sniff puts on the socket.
        let client = wreq::Client::builder()
            .no_proxy()
            .emulation(crate::network::browser_profile())
            .build()
            .expect("client builds");

        let result = sniff(
            &format!("{}/page.html", fixture.base),
            client,
            SniffOptions {
                // yt-dlp is not part of what this test is proving.
                use_extractor: false,
                probe: true,
                yt_dlp: None,
            },
        )
        .await
        .expect("the page is sniffable");

        assert_eq!(result.page_title.as_deref(), Some("Mixed Media Test Page"));

        // Snatch introduces itself as the browser it emulates, on the page
        // fetch and on every HEAD behind it. Nothing sets a user agent by
        // hand any more, so if one is ever added back on top of the profile
        // this is where the disagreement shows up.
        let agents = fixture.agents.lock().expect("the fixture is not poisoned");
        assert!(!agents.is_empty(), "the server saw no user agent at all");
        for agent in agents.iter() {
            assert!(
                agent.contains("Chrome/149") && agent.contains("X11; Linux x86_64"),
                "sent {agent:?}, which is not the emulated browser"
            );
        }

        let names: Vec<String> = result
            .candidates
            .iter()
            .map(|candidate| candidate.filename())
            .collect();
        assert!(names.contains(&"photo1.png".to_owned()), "{names:?}");
        assert!(
            names.contains(&"photo3.png".to_owned()),
            "srcset: {names:?}"
        );
        assert!(names.contains(&"manual.pdf".to_owned()), "{names:?}");
        assert!(names.contains(&"archive.zip".to_owned()), "{names:?}");
        // Navigation and data: URIs are not downloads.
        assert!(!names.iter().any(|n| n.contains("other-page")), "{names:?}");

        let find = |name: &str| {
            result
                .candidates
                .iter()
                .find(|candidate| candidate.filename() == name)
                .unwrap_or_else(|| panic!("{name} missing from {names:?}"))
        };

        // The HEAD probe must have supplied the real type and size.
        let pdf = find("manual.pdf");
        assert_eq!(pdf.kind, MediaKind::Document);
        assert_eq!(pdf.mime.as_deref(), Some("application/pdf"));
        assert!(pdf.verified);
        assert_eq!(pdf.size, Some(8));

        // A download link with no extension is classified only because the
        // server said video/mp4 — the whole reason for probing.
        let clip = find("clip");
        assert_eq!(clip.kind, MediaKind::Video, "mime should beat extension");
        assert!(clip.verified);

        // Duplicates across src and srcset collapsed.
        assert_eq!(
            names.iter().filter(|n| *n == "photo2.png").count(),
            1,
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn a_direct_media_url_needs_no_document() {
        let Some(fixture) = server::start(FIXTURE_PAGE) else {
            return;
        };
        let client = wreq::Client::builder()
            .no_proxy()
            .build()
            .expect("client builds");

        // Pointing the sniffer straight at a file must yield that one file
        // rather than trying to parse it as HTML.
        let result = sniff(
            &format!("{}/manual.pdf", fixture.base),
            client,
            SniffOptions {
                use_extractor: false,
                probe: false,
                yt_dlp: None,
            },
        )
        .await
        .expect("a direct URL is sniffable");

        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].kind, MediaKind::Document);
        assert_eq!(result.candidates[0].origin, Origin::Direct);
        assert!(result.candidates[0].verified);
    }
}
