//! Recording a stream with `ffmpeg`, for the pages yt-dlp cannot read.
//!
//! yt-dlp knows about eighteen hundred sites, so this is the long tail: a
//! player fetching an HLS or DASH manifest that yt-dlp cannot find from the
//! page alone. The browser watches that request go past and hands the manifest
//! address here, where ffmpeg reads it directly.
//!
//! ffmpeg is the right tool for it. It speaks HLS, DASH, RTMP, RTSP and MMS,
//! it follows a live playlist as it grows, and it handles the AES-128 that
//! ordinary HLS is encrypted with - that key is served to any client that
//! asks for it, which is how every player in the world gets one.
//!
//! It does not handle DRM, and neither does anything else here. Widevine,
//! PlayReady and FairPlay keep the key inside a module the player never hands
//! out, so there is nothing for ffmpeg to be given. yt-dlp reports those
//! streams as `has_drm` and [`crate::ytdlp`] drops them from the list, so a
//! quality that cannot work is never offered in the first place.
//!
//! Two decisions below are not obvious:
//!
//!   * **A live recording is written to Matroska, not MP4.** An MP4 keeps its
//!     index in a `moov` atom written when the file is closed. Stop a live
//!     recording - which is the only way one ever ends - and there is no
//!     `moov`, so the file will not open. Matroska writes as it goes and stays
//!     playable however it is interrupted.
//!   * **Streams are copied, never re-encoded.** `-c copy` moves the packets
//!     across untouched, so recording a 4K stream costs no CPU and loses no
//!     quality. Re-encoding would do both.
//!
//! Stopping a live recording is the normal way one ends, so it is a first
//! class operation rather than a kill. ffmpeg is given a pipe for stdin and
//! sent `q`, which is its own "stop cleanly" key: it finishes the packet it is
//! on, writes the trailer, and exits successfully. Killing it instead leaves a
//! file with no trailer -- survivable in Matroska, and not in MP4.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout};

use crate::network::{Engine, ProxyManager};
use crate::processor::{Field, parse_progress_line};
use crate::ytdlp::{VideoEvent, VideoProgress};

/// How long ffprobe may spend inspecting one stream.
///
/// Several are inspected at once when a panel opens, and the whole answer has
/// to reach the browser inside the thirty seconds `snatch-nmh` allows, so this
/// is the budget for all of them together rather than for each in turn.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many observed addresses are worth inspecting. A busy page fetches
/// dozens; the player's own manifest is always among the first few.
const MAX_CANDIDATES: usize = 4;

/// Protocols ffmpeg can be pointed at. Anything else is either a local path
/// (`file:`, and a bare path) or something a web page must not be able to
/// reach, and this address arrives from a browser.
const ALLOWED_SCHEMES: [&str; 7] = ["http", "https", "rtmp", "rtmps", "rtsp", "rtsps", "mms"];

fn ffmpeg_binary() -> String {
    std::env::var("SNATCH_FFMPEG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
}

fn ffprobe_binary() -> String {
    std::env::var("SNATCH_FFPROBE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "ffprobe".to_owned())
}

/// What the request that a page's player made looks like, so ffmpeg can make
/// the same one. A manifest is very often refused without them.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
}

impl Headers {
    /// One `-headers` value: CRLF-separated lines, as ffmpeg wants them.
    fn as_field(&self) -> Option<String> {
        let mut lines = String::new();
        if let Some(referer) = clean(self.referer.as_deref()) {
            lines.push_str(&format!("Referer: {referer}\r\n"));
        }
        if let Some(cookies) = clean(self.cookies.as_deref()) {
            lines.push_str(&format!("Cookie: {cookies}\r\n"));
        }
        if lines.is_empty() { None } else { Some(lines) }
    }

    fn agent(&self) -> Option<String> {
        clean(self.user_agent.as_deref()).map(str::to_owned)
    }
}

/// Reject a value that would break out of its header line.
///
/// These arrive from a web page. A newline in one would let a page append
/// headers of its own to the request ffmpeg makes.
fn clean(value: Option<&str>) -> Option<&str> {
    let value = value.map(str::trim).filter(|value| !value.is_empty())?;
    if value.contains(['\r', '\n', '\0']) || value.len() > 4096 {
        return None;
    }
    Some(value)
}

/// One recording to make: what to read, when, how much of it, and where to
/// put the result.
#[derive(Debug, Clone)]
pub struct Recording {
    pub url: String,
    /// What to call the file. The page's title, when the browser knew one.
    pub name_hint: Option<String>,
    pub directory: PathBuf,
    pub headers: Headers,
    /// Which quality to take, named by height.
    ///
    /// Deliberately not the stream index: a live master playlist can be
    /// rewritten between the moment the picker listed it and the moment the
    /// recording starts, and an index that has shifted silently records the
    /// wrong quality. A height either still exists or it does not.
    pub height: Option<u32>,
    /// Unix time to begin at. Until then the job sits waiting and can be
    /// cancelled like any other.
    pub start_at: Option<i64>,
    /// How far into the stream to begin. Ignored for a live one, which has no
    /// beginning to measure from.
    pub skip: Option<Duration>,
    /// Stop after this much has been recorded.
    pub limit: Option<Duration>,
}

impl Recording {
    /// A recording that starts now, takes the best quality, and runs until it
    /// is stopped.
    pub fn new(url: String, directory: PathBuf) -> Self {
        Self {
            url,
            name_hint: None,
            directory,
            headers: Headers::default(),
            height: None,
            start_at: None,
            skip: None,
            limit: None,
        }
    }
}

/// One quality an address offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rendition {
    /// The input stream index ffmpeg has to be told to take.
    pub index: u32,
    pub height: Option<u32>,
    /// The audio that belongs with this quality.
    ///
    /// Each variant of a master playlist carries its own sound. Pairing a
    /// quality with another variant's audio works, and makes ffmpeg fetch that
    /// whole variant as well -- so recording 240p would quietly download the
    /// 720p segments too, for their soundtrack.
    pub audio_index: Option<u32>,
}

/// What ffprobe could work out about a stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamInfo {
    /// `None` for a live stream, which has no end to measure to.
    pub duration: Option<Duration>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    /// Every video quality the address offers, tallest first.
    ///
    /// A master playlist lists one per quality, so this is usually several.
    /// ffmpeg left to itself takes the first stream, which is usually the
    /// smallest, so the one that is wanted has to be named with `-map`.
    pub renditions: Vec<Rendition>,
    pub audio_index: Option<u32>,
    /// Bytes, when the address is one file rather than a playlist.
    pub size: Option<u64>,
}

impl StreamInfo {
    /// A stream with no duration is one that has not finished happening.
    pub fn is_live(&self) -> bool {
        self.duration.is_none()
    }

    pub fn has_video(&self) -> bool {
        !self.renditions.is_empty()
    }

    /// The rendition to record, given the height the user picked.
    ///
    /// Falls back to the tallest that does not exceed what was asked for, and
    /// then to the tallest of all: a live playlist can drop a quality between
    /// the picker listing it and the recording starting, and recording
    /// something is better than refusing.
    pub fn choose(&self, wanted: Option<u32>) -> Option<Rendition> {
        if let Some(wanted) = wanted {
            if let Some(exact) = self
                .renditions
                .iter()
                .find(|rendition| rendition.height == Some(wanted))
            {
                return Some(*exact);
            }
            if let Some(under) = self
                .renditions
                .iter()
                .filter(|rendition| rendition.height.is_some_and(|height| height <= wanted))
                .max_by_key(|rendition| rendition.height)
            {
                return Some(*under);
            }
        }
        self.renditions.first().copied()
    }

    /// What one quality of this address is called: `1080p live`, `720p · 3:45`.
    pub fn label_for(&self, rendition: &Rendition) -> String {
        let quality = match (rendition.height, self.has_video()) {
            (Some(height), _) => format!("{height}p"),
            (None, true) => "Stream".to_owned(),
            (None, false) => "Audio stream".to_owned(),
        };
        if self.is_live() {
            return format!("{quality} live");
        }
        match self.duration {
            Some(duration) => format!(
                "{quality} · {}",
                crate::processor::format_duration(duration)
            ),
            None => quality,
        }
    }

    /// What the picker shows for the best quality on offer.
    pub fn label(&self) -> String {
        match self.renditions.first() {
            Some(best) => self.label_for(best),
            None => self.label_for(&Rendition {
                index: 0,
                height: None,
                audio_index: None,
            }),
        }
    }

    /// Matroska for a live recording; see the module docs.
    pub fn container(&self) -> &'static str {
        if self.is_live() { "mkv" } else { "mp4" }
    }
}

// ---------------------------------------------------------------------------
// ffprobe
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawProbe {
    #[serde(default)]
    format: Option<RawFormat>,
    #[serde(default)]
    streams: Vec<RawStream>,
    /// One per variant of a master playlist, each listing the streams that
    /// belong together. Absent for a plain file.
    #[serde(default)]
    programs: Vec<RawProgram>,
}

#[derive(Debug, Deserialize)]
struct RawProgram {
    #[serde(default)]
    streams: Vec<RawStream>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    /// ffprobe prints this as a string, and omits it for a live stream.
    #[serde(default)]
    duration: Option<String>,
    /// Bytes, also as a string. Meaningful for a plain file; for a playlist
    /// it is the size of the playlist itself, which is not worth showing.
    #[serde(default)]
    size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    codec_type: Option<String>,
    #[serde(default)]
    codec_name: Option<String>,
    #[serde(default)]
    height: Option<u32>,
}

pub fn validate_url(url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        bail!("the stream address is empty");
    }
    if url.len() > 8192 {
        bail!("the stream address is unreasonably long");
    }
    if url.contains(['\r', '\n', '\0']) {
        bail!("the stream address contains a line break");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("the stream address has no scheme");
    };
    let scheme = scheme.to_ascii_lowercase();
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        bail!("unsupported stream scheme '{scheme}'");
    }
    if rest.is_empty() {
        bail!("the stream address has no host");
    }
    Ok(())
}

/// Ask ffprobe what is at this address, without recording any of it.
pub async fn probe(url: &str, headers: &Headers) -> Result<StreamInfo> {
    validate_url(url)?;

    let binary = ffprobe_binary();
    let mut command = Command::new(&binary);
    command
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        // Which streams belong to which variant, so a quality is recorded
        // with its own sound rather than another variant's.
        .arg("-show_programs");
    apply_headers(&mut command, headers);
    command
        .arg("-i")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{binary} was not found in PATH; install the 'ffmpeg' package");
        }
        Err(error) => return Err(error).context("could not start ffprobe"),
    };

    let output = timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .context("ffprobe took too long to read the stream")?
        .context("could not read ffprobe's answer")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().map(str::trim).rfind(|line| !line.is_empty());
        match reason {
            Some(reason) => bail!("ffprobe could not read that stream: {reason}"),
            None => bail!("ffprobe could not read that stream"),
        }
    }

    let raw: RawProbe = serde_json::from_slice(&output.stdout)
        .context("ffprobe printed something that was not a stream description")?;
    Ok(distil(&raw))
}

fn distil(raw: &RawProbe) -> StreamInfo {
    let duration = raw
        .format
        .as_ref()
        .and_then(|format| format.duration.as_deref())
        .and_then(|value| value.trim().parse::<f64>().ok())
        // A live HLS playlist reports "N/A", and some report 0.
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(Duration::from_secs_f64);

    let size = raw
        .format
        .as_ref()
        .and_then(|format| format.size.as_deref())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|bytes| *bytes > 0);

    let mut info = StreamInfo {
        duration,
        size,
        ..StreamInfo::default()
    };
    for (position, stream) in raw.streams.iter().enumerate() {
        // ffprobe reports the input's own index; fall back to the position in
        // the list, which is the same thing for every stream seen so far.
        let index = stream.index.unwrap_or(position as u32);
        match stream.codec_type.as_deref() {
            Some("video") => {
                if info.video_codec.is_none() {
                    info.video_codec = stream.codec_name.clone();
                }
                info.renditions.push(Rendition {
                    index,
                    height: stream.height,
                    audio_index: None,
                });
            }
            Some("audio") if info.audio_codec.is_none() => {
                info.audio_codec = stream.codec_name.clone();
                info.audio_index = Some(index);
            }
            _ => {}
        }
    }

    // Pair each quality with the sound from its own variant.
    for program in &raw.programs {
        let audio = program
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("audio"))
            .and_then(|stream| stream.index);
        for stream in &program.streams {
            if stream.codec_type.as_deref() != Some("video") {
                continue;
            }
            if let Some(rendition) = info
                .renditions
                .iter_mut()
                .find(|rendition| Some(rendition.index) == stream.index)
            {
                rendition.audio_index = audio;
            }
        }
    }
    // Tallest first, so `first()` is the best on offer everywhere else.
    info.renditions
        .sort_by_key(|rendition| std::cmp::Reverse(rendition.height));
    info
}

/// Inspect every address the browser saw a player fetch, and keep the ones
/// ffprobe can actually read.
///
/// They are inspected at the same time, not one after another: four addresses
/// at ten seconds each would take longer in sequence than the browser is
/// willing to wait for the whole answer.
pub async fn describe_all(urls: &[String], headers: &Headers) -> Vec<(String, StreamInfo)> {
    let mut wanted: Vec<String> = Vec::new();
    for url in urls {
        let url = url.trim();
        if url.is_empty() || wanted.iter().any(|seen| seen == url) {
            continue;
        }
        if validate_url(url).is_err() {
            continue;
        }
        wanted.push(url.to_owned());
        if wanted.len() >= MAX_CANDIDATES {
            break;
        }
    }

    let probes = wanted.into_iter().map(|url| async move {
        match probe(&url, headers).await {
            Ok(info) => Some((url, info)),
            Err(error) => {
                log::debug!("ignoring {url}: {error:#}");
                None
            }
        }
    });
    futures::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

fn apply_headers(command: &mut Command, headers: &Headers) {
    if let Some(field) = headers.as_field() {
        command.arg("-headers").arg(field);
    }
    if let Some(agent) = headers.agent() {
        command.arg("-user_agent").arg(agent);
    }
}

/// Turn a page title into a filename, or fall back to the stream's host.
pub fn output_name(hint: Option<&str>, url: &str) -> String {
    if let Some(name) = hint
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .map(sanitize)
        .filter(|name| !name.is_empty())
    {
        return name;
    }
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("stream");
    let host = sanitize(host);
    if host.is_empty() {
        "stream".to_owned()
    } else {
        host
    }
}

/// The file's own extension, for a row that downloads it as it is rather than
/// remuxing it. `None` when the address does not end in a plausible one.
pub fn extension_of(url: &str) -> Option<String> {
    let path = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['?', '#'])
        .next()?;
    let last = path.rsplit('/').next()?;
    let extension = last.rsplit_once('.')?.1;
    if extension.is_empty()
        || extension.len() > 5
        || !extension.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

/// "3 minutes", "2 hours" -- enough for a row that is only counting down.
fn human_wait(wait: Duration) -> String {
    let seconds = wait.as_secs();
    if seconds < 90 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 90 {
        return format!("{minutes} min");
    }
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    cleaned
        .trim_start_matches('.')
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// A path that does not exist yet, so a second recording never overwrites the
/// first. ffmpeg is run with `-y`, which would otherwise do exactly that.
pub fn unique_path(directory: &Path, name: &str, extension: &str) -> PathBuf {
    let first = directory.join(format!("{name}.{extension}"));
    if !first.exists() {
        return first;
    }
    for index in 2..1000 {
        let candidate = directory.join(format!("{name} ({index}).{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    directory.join(format!(
        "{name} ({}).{extension}",
        crate::settings::now_unix()
    ))
}

/// Record `url` until it ends, or until `stop` fires.
///
/// `stop` is the graceful path and the one a live recording normally takes:
/// ffmpeg is asked to finish, so the file is closed properly and plays. The
/// task can still be aborted outright, which kills ffmpeg mid-write -- which
/// is the other reason a live recording goes to Matroska.
pub async fn record(
    job_id: i64,
    job: &Recording,
    proxies: &ProxyManager,
    stop: oneshot::Receiver<()>,
    events: &mpsc::Sender<VideoEvent>,
) -> Result<PathBuf> {
    let Recording {
        url,
        name_hint,
        directory,
        headers,
        height,
        start_at,
        skip,
        limit,
    } = job;
    let url = url.as_str();
    validate_url(url)?;

    let mut stop = stop;

    // A scheduled recording waits here, as a visible job that can be cancelled
    // like any other. Waiting before the probe is deliberate: a programme that
    // has not started yet has nothing to probe.
    if let Some(at) = *start_at {
        let now = crate::settings::now_unix();
        if at > now {
            let wait = Duration::from_secs((at - now) as u64);
            let name = output_name(name_hint.as_deref(), url);
            let _ = events
                .send(VideoEvent::Title {
                    job_id,
                    title: format!("{name} (waiting {})", human_wait(wait)),
                })
                .await;
            log::info!(
                "stream job {job_id} waits {}s before starting",
                wait.as_secs()
            );
            tokio::select! {
                _ = sleep(wait) => {}
                _ = &mut stop => bail!("stopped before the recording was due to start"),
            }
        }
    }

    // Probing decides the container and which quality to take, and turns the
    // row in the window from "stream" into "1080p live" before a byte lands.
    let info = probe(url, headers).await?;
    let chosen = info.choose(*height);
    let name = output_name(name_hint.as_deref(), url);
    let described = match &chosen {
        Some(rendition) => info.label_for(rendition),
        None => info.label(),
    };
    let _ = events
        .send(VideoEvent::Title {
            job_id,
            title: format!("{name} ({described})"),
        })
        .await;

    std::fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let output = unique_path(directory.as_path(), &name, info.container());

    let binary = ffmpeg_binary();
    let mut command = Command::new(&binary);
    // No `-nostdin`: ffmpeg is given a pipe of its own and read from it is how
    // it is asked to stop. It is not attached to a terminal, so there is
    // nothing for it to steal.
    command.arg("-hide_banner").arg("-loglevel").arg("error");
    apply_headers(&mut command, headers);
    // Seeking before the input is the fast kind: ffmpeg jumps to the nearest
    // keyframe rather than decoding its way there. With `-c copy` that is the
    // only kind available, and a keyframe is where a copied stream has to
    // start anyway. A live stream has no beginning to seek from, so it is
    // skipped there rather than failing.
    if let Some(skip) = skip.filter(|skip| !skip.is_zero() && !info.is_live()) {
        command.arg("-ss").arg(format!("{:.3}", skip.as_secs_f64()));
    }
    command.arg("-i").arg(url);
    // Take the rendition the row promised, not whichever ffmpeg would have
    // picked. Without this a master playlist records its smallest variant.
    if let Some(rendition) = &chosen {
        command.arg("-map").arg(format!("0:{}", rendition.index));
    }
    // The chosen quality's own sound, falling back to the input's only audio
    // when there are no variants to belong to.
    if let Some(index) = chosen
        .and_then(|rendition| rendition.audio_index)
        .or(info.audio_index)
    {
        command.arg("-map").arg(format!("0:{index}"));
    }
    if let Some(limit) = limit.filter(|limit| !limit.is_zero()) {
        command.arg("-t").arg(format!("{:.3}", limit.as_secs_f64()));
    }
    // Copy the packets across: no CPU cost, no quality lost.
    command.arg("-c").arg("copy");
    if info.container() == "mp4" {
        command.arg("-movflags").arg("+faststart");
    }
    command
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .arg("-y")
        .arg(&output);

    let task_key = format!("stream:{job_id}");
    if let Some(proxy) = proxies
        .resolve_for(&task_key, Engine::Subprocess)
        .context("the stream cannot use the configured proxy")?
    {
        log::info!("stream job {job_id} routed through {}", proxy.redacted());
        // ffmpeg has no proxy flag; its HTTP protocol reads these.
        command
            .env("http_proxy", proxy.url())
            .env("https_proxy", proxy.url());
    }

    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{binary} was not found in PATH; install the 'ffmpeg' package");
        }
        Err(error) => return Err(error).context("could not start ffmpeg"),
    };

    let mut keyboard = child.stdin.take();
    let mut stopping = false;
    let stdout = child.stdout.take().context("ffmpeg produced no stdout")?;
    let stderr = child.stderr.take().context("ffmpeg produced no stderr")?;
    let mut progress_lines = BufReader::new(stdout).lines();
    let mut error_lines = BufReader::new(stderr).lines();

    let mut current = VideoProgress::default();
    let mut last_error: Option<String> = None;
    let (mut stdout_open, mut stderr_open) = (true, true);
    // ffmpeg reports how far through the media it is and how many bytes it
    // has written, but never a byte total -- for a live stream there is no
    // such number. The rate is worked out from the byte counter instead.
    let mut last_sample: Option<(std::time::Instant, u64)> = None;

    // Both pipes drain at once. Reading one to the end first would block as
    // soon as the other filled, which for a long recording it certainly will.
    while stdout_open || stderr_open {
        tokio::select! {
            // A dropped sender means the engine is going away, and is treated
            // the same as an explicit stop: finish the file rather than lose it.
            _ = &mut stop, if !stopping => {
                stopping = true;
                log::info!("stream job {job_id}: asking ffmpeg to finish");
                if let Some(mut pipe) = keyboard.take() {
                    // ffmpeg's own key for "stop cleanly". Closing the pipe
                    // afterwards is a second signal for the same thing.
                    let _ = pipe.write_all(b"q").await;
                    let _ = pipe.flush().await;
                }
            },
            line = progress_lines.next_line(), if stdout_open => match line {
                Ok(Some(line)) => {
                    // ffmpeg writes a dozen lines per tick. Publishing each one
                    // sends twelve near-identical events, and a consumer that
                    // falls behind stops this loop reading -- which fills
                    // ffmpeg's stdout pipe and stalls the recording itself.
                    // So the fields accumulate and one event goes out per tick.
                    let mut changed = false;
                    match parse_progress_line(&line) {
                        Field::TotalSize(bytes) => {
                            let now = std::time::Instant::now();
                            if let Some((then, before)) = last_sample {
                                let seconds = now.duration_since(then).as_secs_f64();
                                if seconds >= 0.5 && bytes > before {
                                    current.speed = Some((bytes - before) as f64 / seconds);
                                    last_sample = Some((now, bytes));
                                }
                            } else {
                                last_sample = Some((now, bytes));
                            }
                            current.downloaded = bytes;
                        }
                        // How far into the media ffmpeg has got. `total` stays
                        // empty on purpose: it is a byte count everywhere else
                        // in the window, and there is no byte total to put in
                        // it. The row pulses and says what has arrived, which
                        // is the truth for a stream that has not ended yet.
                        Field::OutTimeMicros(micros) => {
                            current.eta_seconds = info
                                .duration
                                .and_then(|total| {
                                    total.as_micros().checked_sub(u128::from(micros))
                                })
                                .map(|left| (left / 1_000_000) as u64);
                            changed = true;
                        }
                        // A block ends with `progress=continue|end`.
                        Field::Done => changed = true,
                        Field::Speed(_) | Field::Ignored => {}
                    }
                    if changed {
                        let _ = events
                            .send(VideoEvent::Progress { job_id, progress: current.clone() })
                            .await;
                    }
                }
                Ok(None) => stdout_open = false,
                Err(error) => {
                    log::warn!("stream job {job_id}: could not read progress: {error}");
                    stdout_open = false;
                }
            },
            line = error_lines.next_line(), if stderr_open => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if !line.is_empty() {
                        log::warn!(target: "ffmpeg", "{line}");
                        last_error = Some(line.to_owned());
                    }
                }
                Ok(None) => stderr_open = false,
                Err(_) => stderr_open = false,
            },
        }
    }

    // Dropping the pipe if it was never used closes ffmpeg's stdin, which it
    // is content to ignore while it still has an input to read.
    drop(keyboard);

    let status = child
        .wait()
        .await
        .context("could not wait for ffmpeg to exit")?;
    if !status.success() {
        let code = status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "a signal".to_owned());
        match last_error {
            Some(message) => bail!("ffmpeg exited with {code}: {message}"),
            None => bail!("ffmpeg exited with {code}"),
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_of(json: &str) -> StreamInfo {
        distil(&serde_json::from_str(json).expect("must parse"))
    }

    #[test]
    fn a_live_playlist_has_no_duration_and_records_to_matroska() {
        // ffprobe omits `duration` entirely for a live HLS playlist.
        let info = probe_of(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264","height":1080},
                           {"codec_type":"audio","codec_name":"aac"}],
                "format":{}}"#,
        );
        assert!(info.is_live());
        assert_eq!(info.label(), "1080p live");
        // An MP4 written without a moov atom will not open, and stopping the
        // recording is the only way a live one ever ends.
        assert_eq!(info.container(), "mkv");
    }

    #[test]
    fn a_plain_file_reports_its_size() {
        let info = probe_of(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264","height":720}],
                "format":{"duration":"225.0","size":"47185920"}}"#,
        );
        assert_eq!(info.size, Some(47_185_920));
        assert_eq!(info.label(), "720p · 3:45");
    }

    #[test]
    fn a_files_own_extension_is_read_from_its_address() {
        assert_eq!(
            extension_of("https://cdn.example/a/clip.mp4").as_deref(),
            Some("mp4")
        );
        // A signed URL keeps its extension in front of the query.
        assert_eq!(
            extension_of("https://cdn.example/clip.webm?token=abc&x=1").as_deref(),
            Some("webm")
        );
        assert_eq!(extension_of("https://cdn.example/stream").as_deref(), None);
        // A dotted host with no path must not be read as an extension.
        assert_eq!(extension_of("https://cdn.example.com/").as_deref(), None);
    }

    #[test]
    fn a_finished_stream_keeps_its_duration_and_lands_in_mp4() {
        let info = probe_of(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264","height":720},
                           {"codec_type":"audio","codec_name":"aac"}],
                "format":{"duration":"2530.500000"}}"#,
        );
        assert!(!info.is_live());
        assert_eq!(info.duration, Some(Duration::from_secs_f64(2530.5)));
        assert_eq!(info.label(), "720p · 42:10");
        assert_eq!(info.container(), "mp4");
    }

    #[test]
    fn na_and_zero_durations_count_as_live() {
        // Both turn up in the wild; neither is a length.
        assert!(probe_of(r#"{"format":{"duration":"N/A"},"streams":[]}"#).is_live());
        assert!(probe_of(r#"{"format":{"duration":"0.000000"},"streams":[]}"#).is_live());
    }

    #[test]
    fn the_tallest_rendition_is_the_one_named_and_the_one_taken() {
        // A master playlist lists every quality. ffmpeg left to itself takes
        // the first, so the row would say 1080p and record 480p.
        let info = probe_of(
            r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","height":480},
                           {"index":1,"codec_type":"video","codec_name":"h264","height":1080},
                           {"index":2,"codec_type":"audio","codec_name":"aac"}],
                "format":{"duration":"60"}}"#,
        );
        // Tallest first, and every quality kept: each is separately
        // recordable, so offering only the best would throw the rest away.
        assert_eq!(
            info.renditions
                .iter()
                .map(|r| (r.height, r.index))
                .collect::<Vec<_>>(),
            [(Some(1080), 1), (Some(480), 0)]
        );
        assert_eq!(info.choose(None).map(|r| r.index), Some(1));
        assert_eq!(info.choose(Some(480)).map(|r| r.index), Some(0));
        // A quality that has gone falls back to the tallest beneath it.
        assert_eq!(info.choose(Some(720)).map(|r| r.index), Some(0));
        // ...and to the best of all when there is nothing beneath it.
        assert_eq!(info.choose(Some(144)).map(|r| r.index), Some(1));
        assert_eq!(info.audio_index, Some(2));
    }

    #[test]
    fn each_quality_is_paired_with_its_own_sound() {
        // Captured from a real three-variant master playlist. Pairing 240p
        // with program 0's audio would make ffmpeg fetch the 720p segments
        // too, purely for their soundtrack.
        let info = probe_of(
            r#"{"format":{"duration":"20.0"},
                "streams":[{"index":0,"codec_type":"video","codec_name":"h264","height":720},
                           {"index":1,"codec_type":"audio","codec_name":"aac"},
                           {"index":2,"codec_type":"video","codec_name":"h264","height":480},
                           {"index":3,"codec_type":"audio","codec_name":"aac"},
                           {"index":4,"codec_type":"video","codec_name":"h264","height":240},
                           {"index":5,"codec_type":"audio","codec_name":"aac"}],
                "programs":[
                  {"streams":[{"index":0,"codec_type":"video","height":720},
                              {"index":1,"codec_type":"audio"}]},
                  {"streams":[{"index":2,"codec_type":"video","height":480},
                              {"index":3,"codec_type":"audio"}]},
                  {"streams":[{"index":4,"codec_type":"video","height":240},
                              {"index":5,"codec_type":"audio"}]}]}"#,
        );
        assert_eq!(
            info.renditions
                .iter()
                .map(|r| (r.height, r.index, r.audio_index))
                .collect::<Vec<_>>(),
            [
                (Some(720), 0, Some(1)),
                (Some(480), 2, Some(3)),
                (Some(240), 4, Some(5)),
            ]
        );
        assert_eq!(info.choose(Some(240)).and_then(|r| r.audio_index), Some(5));
    }

    #[test]
    fn a_stream_list_without_indices_falls_back_to_its_order() {
        let info = probe_of(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264","height":720},
                           {"codec_type":"audio","codec_name":"aac"}],
                "format":{}}"#,
        );
        assert_eq!(info.renditions.first().map(|r| r.index), Some(0));
        assert_eq!(info.audio_index, Some(1));
    }

    #[test]
    fn an_audio_only_stream_says_so() {
        let info =
            probe_of(r#"{"streams":[{"codec_type":"audio","codec_name":"aac"}],"format":{}}"#);
        assert!(!info.has_video());
        assert_eq!(info.label(), "Audio stream live");
    }

    #[test]
    fn headers_are_built_the_way_ffmpeg_wants_them() {
        let headers = Headers {
            referer: Some("https://example.com/watch".to_owned()),
            cookies: Some("session=abc".to_owned()),
            user_agent: Some("Mozilla/5.0".to_owned()),
        };
        assert_eq!(
            headers.as_field().as_deref(),
            Some("Referer: https://example.com/watch\r\nCookie: session=abc\r\n")
        );
        assert_eq!(headers.agent().as_deref(), Some("Mozilla/5.0"));
        assert_eq!(Headers::default().as_field(), None);
    }

    #[test]
    fn a_header_can_never_carry_a_line_break() {
        // These arrive from a web page. A newline would let it append headers
        // of its own to the request ffmpeg makes.
        let headers = Headers {
            referer: Some("https://x/\r\nX-Evil: 1".to_owned()),
            cookies: Some("a=b\nc=d".to_owned()),
            user_agent: None,
        };
        assert_eq!(headers.as_field(), None);

        // A break at the end is only trailing whitespace: it is trimmed off,
        // and what is left cannot carry anything into the next header.
        let tidy = Headers {
            user_agent: Some("agent\r\n".to_owned()),
            ..Headers::default()
        };
        assert_eq!(tidy.agent().as_deref(), Some("agent"));
    }

    #[test]
    fn only_protocols_ffmpeg_should_be_pointed_at_are_allowed() {
        assert!(validate_url("https://example.com/live/index.m3u8").is_ok());
        assert!(validate_url("rtmp://example.com/live/key").is_ok());
        // A page must not be able to make Snatch read the disk.
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("concat:/etc/passwd").is_err());
        assert!(validate_url("/etc/passwd").is_err());
        assert!(validate_url("https://x/\nfoo").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn names_come_from_the_page_title_and_fall_back_to_the_host() {
        // The colon cannot go in a filename, and the run of spaces it leaves
        // behind is collapsed rather than kept.
        assert_eq!(
            output_name(Some("Match of the Day: Week 4"), "https://x/y.m3u8"),
            "Match of the Day Week 4"
        );
        // A title that is only punctuation leaves nothing usable behind.
        assert_eq!(
            output_name(Some("///"), "https://live.example.com/a.m3u8"),
            "live.example.com"
        );
        assert_eq!(
            output_name(None, "https://live.example.com/a.m3u8"),
            "live.example.com"
        );
        // A leading dot would make a hidden file.
        assert_eq!(output_name(Some(".hidden"), "https://x/y"), "hidden");
    }

    #[test]
    fn a_second_recording_never_overwrites_the_first() {
        let dir = std::env::temp_dir().join(format!("snatch-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let first = unique_path(&dir, "show", "mkv");
        assert_eq!(first.file_name().and_then(|n| n.to_str()), Some("show.mkv"));
        std::fs::write(&first, b"x").expect("write");
        let second = unique_path(&dir, "show", "mkv");
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("show (2).mkv")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    use std::sync::Arc;

    use tokio::net::TcpListener;

    fn which(binary: &str) -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(binary))
                .find(|candidate| candidate.is_file())
        })
    }

    /// Serve one file, slowly, for as long as anyone asks for it.
    ///
    /// The pace is the point: it keeps ffmpeg reading for several seconds so
    /// the recording can be stopped part-way through, which is the thing under
    /// test. Range headers are ignored and the whole body is sent, which
    /// ffmpeg is content with.
    async fn serve_slowly(path: PathBuf) -> u16 {
        serve(path, Duration::from_millis(60)).await
    }

    /// As above, at full speed, for the tests that are not about timing.
    async fn serve_fast(path: PathBuf) -> u16 {
        serve(path, Duration::ZERO).await
    }

    async fn serve(path: PathBuf, pace: Duration) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let body = std::fs::read(&path).expect("read the clip");
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    // Enough of the request line to know one arrived.
                    let mut scratch = [0u8; 2048];
                    let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut scratch).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\n\
                         Content-Length: {}\r\nAccept-Ranges: none\r\n\r\n",
                        body.len()
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    for chunk in body.chunks(32 * 1024) {
                        if socket.write_all(chunk).await.is_err() {
                            return;
                        }
                        if !pace.is_zero() {
                            tokio::time::sleep(pace).await;
                        }
                    }
                });
            }
        });
        port
    }

    /// Build a clip of `seconds`, keyframed once a second so a seek can land
    /// close to where it was asked to.
    async fn build_clip(dir: &Path, seconds: u32) -> PathBuf {
        std::fs::create_dir_all(dir).expect("scratch directory");
        let source = dir.join("clip.mp4");
        let video = format!("testsrc2=size=320x240:rate=25:duration={seconds}");
        let audio = format!("sine=frequency=440:duration={seconds}");
        let built = tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i"])
            .arg(&video)
            .args(["-f", "lavfi", "-i"])
            .arg(&audio)
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "25",
                "-c:a",
                "aac",
                "-movflags",
                "+faststart",
                "-shortest",
                "-y",
            ])
            .arg(&source)
            .output()
            .await
            .expect("ffmpeg runs");
        assert!(built.status.success(), "could not build the test clip");
        source
    }

    /// Six seconds asked for, starting five in, must give six seconds -- not
    /// the whole half minute.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn records_only_the_segment_asked_for() {
        if which("ffmpeg").is_none() || which("ffprobe").is_none() {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("snatch-seg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = build_clip(&dir, 30).await;
        let port = serve_fast(source).await;

        let (events, mut seen) = mpsc::channel(64);
        tokio::spawn(async move { while seen.recv().await.is_some() {} });
        let (_stopper, stop) = oneshot::channel();

        let job = Recording {
            name_hint: Some("segment".to_owned()),
            skip: Some(Duration::from_secs(5)),
            limit: Some(Duration::from_secs(6)),
            ..Recording::new(format!("http://127.0.0.1:{port}/clip.mp4"), dir.clone())
        };
        let proxies = ProxyManager::load(dir.join("proxies.json"));
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            record(2, &job, &proxies, stop, &events),
        )
        .await
        .expect("the segment ends on its own")
        .expect("the recording succeeded");

        let seconds = crate::processor::probe_duration(&output)
            .await
            .expect("ffprobe reads it")
            .expect("it has a duration")
            .as_secs_f64();
        // `-ss` seeks to the nearest keyframe, so where it starts is
        // approximate. How much it takes is not.
        assert!(
            (seconds - 6.0).abs() < 1.5,
            "expected about six seconds, got {seconds}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scheduled recording waits, says so, and leaves nothing behind if it
    /// is called off before its time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_scheduled_recording_waits_and_can_be_called_off() {
        if which("ffmpeg").is_none() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("snatch-sched-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");

        let (events, mut seen) = mpsc::channel(64);
        let (stopper, stop) = oneshot::channel();
        // An hour away, and an address nothing answers on: if the wait were
        // not honoured this would fail trying to reach it.
        let job = Recording {
            start_at: Some(crate::settings::now_unix() + 3600),
            ..Recording::new("http://127.0.0.1:1/never.m3u8".to_owned(), dir.clone())
        };
        let proxies = ProxyManager::load(dir.join("proxies.json"));
        let waiting = tokio::spawn(async move { record(3, &job, &proxies, stop, &events).await });

        let titled = tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("a title arrives promptly")
            .expect("the channel is open");
        match titled {
            VideoEvent::Title { title, .. } => {
                assert!(title.contains("waiting"), "unhelpful title: {title}");
            }
            other => panic!("expected a title, got {other:?}"),
        }

        stopper.send(()).expect("still waiting");
        let outcome = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("it gives up promptly once called off")
            .expect("the task did not panic");
        let error = format!(
            "{:#}",
            outcome.expect_err("a called-off wait is not a recording")
        );
        assert!(
            error.contains("before the recording"),
            "unclear reason: {error}"
        );
        assert!(
            std::fs::read_dir(&dir)
                .expect("the directory exists")
                .flatten()
                .all(|entry| entry.file_name() != "never.mkv"),
            "a called-off recording left a file behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The decisive test for stopping a recording.
    ///
    /// The output is an MP4, whose index lives in a `moov` atom written when
    /// the file is closed. Kill ffmpeg instead of asking it to stop and there
    /// is no `moov`, and ffprobe reports "moov atom not found" rather than a
    /// duration. So this passing *is* the proof that the file was finished
    /// properly rather than merely left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stopping_a_recording_leaves_a_file_that_plays() {
        if which("ffmpeg").is_none() || which("ffprobe").is_none() {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        }

        let dir = std::env::temp_dir().join(format!("snatch-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // `+faststart` puts the index at the front, so the probe that `record`
        // does first finishes quickly even though the body arrives slowly.
        let source = build_clip(&dir, 30).await;
        let port = serve_slowly(source).await;
        let (events, mut seen) = mpsc::channel(64);
        let (stopper, stop) = oneshot::channel();

        let job = Recording {
            name_hint: Some("stop test".to_owned()),
            ..Recording::new(format!("http://127.0.0.1:{port}/clip.mp4"), dir.clone())
        };
        let proxies = ProxyManager::load(dir.join("proxies.json"));

        // Drained concurrently, not collected at the end: a full channel
        // stops the recorder reading ffmpeg's output, which stalls ffmpeg.
        let progressed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watching = Arc::clone(&progressed);
        tokio::spawn(async move {
            while let Some(event) = seen.recv().await {
                if let VideoEvent::Progress { progress, .. } = event
                    && progress.downloaded > 0
                {
                    watching.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });

        let recording = tokio::spawn(async move { record(1, &job, &proxies, stop, &events).await });

        // Long enough that ffmpeg is well into the file and nowhere near its
        // end, so stopping is genuinely interrupting a recording in progress.
        tokio::time::sleep(Duration::from_secs(3)).await;
        stopper.send(()).expect("the recorder is still listening");

        let output = tokio::time::timeout(Duration::from_secs(30), recording)
            .await
            .expect("the recording ends promptly once asked")
            .expect("the task did not panic")
            .expect("the recording succeeded");

        assert!(output.is_file(), "no file at {}", output.display());
        assert_eq!(output.extension().and_then(|e| e.to_str()), Some("mp4"));

        // The whole point: a killed ffmpeg leaves an MP4 ffprobe cannot open,
        // so reading a duration back at all is the proof it was finished.
        let seconds = crate::processor::probe_duration(&output)
            .await
            .expect("ffprobe reads the stopped recording")
            .expect("the stopped recording has a duration")
            .as_secs_f64();
        assert!(
            seconds > 0.5 && seconds < 29.0,
            "expected part of the clip, got {seconds}s"
        );

        assert!(
            progressed.load(std::sync::atomic::Ordering::Relaxed),
            "no progress was reported"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
