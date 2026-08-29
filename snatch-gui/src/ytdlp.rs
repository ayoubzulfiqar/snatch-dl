//! Site video extraction, driven by `yt-dlp`.
//!
//! aria2 can fetch a URL; it cannot work out that a watch page contains a
//! DASH manifest, pick a format, and mux the result. yt-dlp does all of that,
//! so a "video" job is a supervised subprocess rather than a transfer.
//!
//! Progress comes from `--progress-template`, which emits one line per tick in
//! a layout we choose. Captured from yt-dlp 2026.08.19:
//!
//! ```text
//! SNATCH|downloading|130048|429790|NA|13983665.66|0|NA|NA
//! SNATCH|finished|429790|429790|NA|21963155.52|NA|NA|NA
//! ```
//!
//! Every field can be the literal string `NA` — including `total_bytes` on a
//! live stream and `speed` on the very first tick — so nothing may be parsed
//! without a fallback. The destination path arrives separately, on yt-dlp's
//! ordinary stdout (`[download] Destination: …`, or `[Merger] Merging formats
//! into "…"` once audio and video are combined), because the progress template
//! has no placeholder that survives a merge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::db::{Database, JobState};
use crate::network::{Engine, ProxyManager};

/// Marker that separates our progress lines from yt-dlp's own logging.
const SENTINEL: &str = "SNATCH|";

const PROGRESS_TEMPLATE: &str = concat!(
    "SNATCH|%(progress.status)s|%(progress.downloaded_bytes)s|%(progress.total_bytes)s",
    "|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s",
    "|%(progress.fragment_index)s|%(progress.fragment_count)s"
);

fn yt_dlp_binary() -> String {
    std::env::var("SNATCH_YT_DLP")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "yt-dlp".to_owned())
}

/// What quality to ask yt-dlp for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoQuality {
    /// Best video plus best audio, merged.
    #[default]
    Best,
    /// Cap the height, useful on a metered link.
    UpTo1080,
    UpTo720,
    /// Audio only, extracted to MP3.
    AudioOnly,
}

impl VideoQuality {
    pub fn label(self) -> &'static str {
        match self {
            VideoQuality::Best => "Best available",
            VideoQuality::UpTo1080 => "Up to 1080p",
            VideoQuality::UpTo720 => "Up to 720p",
            VideoQuality::AudioOnly => "Audio only (MP3)",
        }
    }

    pub fn all() -> [VideoQuality; 4] {
        [
            VideoQuality::Best,
            VideoQuality::UpTo1080,
            VideoQuality::UpTo720,
            VideoQuality::AudioOnly,
        ]
    }

    fn format_selector(self) -> Option<&'static str> {
        match self {
            VideoQuality::Best => Some("bestvideo*+bestaudio/best"),
            VideoQuality::UpTo1080 => Some("bestvideo[height<=1080]+bestaudio/best[height<=1080]"),
            VideoQuality::UpTo720 => Some("bestvideo[height<=720]+bestaudio/best[height<=720]"),
            // -x picks the format; an explicit selector would fight it.
            VideoQuality::AudioOnly => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub destination: PathBuf,
    pub quality: VideoQuality,
    /// An exact `-f` selector, chosen from a [`probe`] rather than from the
    /// coarse [`VideoQuality`] list. Overrides `quality` when set: this is how
    /// the resolution the user picked in the browser reaches yt-dlp.
    pub format: Option<String>,
    /// Write chapters, thumbnail and metadata into the container.
    pub embed_metadata: bool,
    /// Fetch subtitles when the site offers them.
    pub subtitles: bool,
    /// Download a whole playlist rather than just the linked item.
    pub playlist: bool,
}

impl VideoConfig {
    pub fn new(destination: PathBuf) -> Self {
        Self {
            destination,
            quality: VideoQuality::default(),
            format: None,
            embed_metadata: true,
            subtitles: false,
            playlist: false,
        }
    }
}

/// Live progress for one extraction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoProgress {
    pub downloaded: u64,
    /// `None` for a live stream or an unknown-length fragment feed.
    pub total: Option<u64>,
    pub speed: Option<f64>,
    pub eta_seconds: Option<u64>,
    /// Fragment m of n, for DASH/HLS.
    pub fragment: Option<(u64, u64)>,
}

impl VideoProgress {
    pub fn fraction(&self) -> Option<f64> {
        // Byte counts are authoritative when present.
        if let Some(total) = self.total.filter(|total| *total > 0) {
            return Some((self.downloaded as f64 / total as f64).clamp(0.0, 1.0));
        }
        // A fragmented stream has no byte total until the last fragment.
        if let Some((index, count)) = self.fragment.filter(|(_, count)| *count > 0) {
            return Some((index as f64 / count as f64).clamp(0.0, 1.0));
        }
        None
    }
}

#[derive(Debug, Clone)]
pub enum VideoEvent {
    Started {
        job_id: i64,
        url: String,
    },
    /// yt-dlp resolved a title, which is a better row label than the URL.
    Title {
        job_id: i64,
        title: String,
    },
    Progress {
        job_id: i64,
        progress: VideoProgress,
    },
    Finished {
        job_id: i64,
        output: Option<PathBuf>,
    },
    Failed {
        job_id: i64,
        error: String,
    },
}

/// One parsed line of yt-dlp output.
#[derive(Debug, PartialEq)]
enum Line {
    Progress(VideoProgress),
    /// yt-dlp finished writing a file.
    Complete,
    Destination(PathBuf),
    Title(String),
    Error(String),
    Ignored,
}

/// `NA` is yt-dlp's "not available"; treat it and an empty field as absent.
fn field(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() || value == "NA" || value == "None" {
        None
    } else {
        Some(value)
    }
}

fn parse_line(line: &str) -> Line {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Line::Ignored;
    }

    if let Some(rest) = trimmed.strip_prefix(SENTINEL) {
        let parts: Vec<&str> = rest.split('|').collect();
        if parts.is_empty() {
            return Line::Ignored;
        }
        if field(parts[0]) == Some("finished") {
            return Line::Complete;
        }

        let get = |index: usize| parts.get(index).copied().and_then(field);
        let fragment = match (
            get(6).and_then(|value| value.parse::<u64>().ok()),
            get(7).and_then(|value| value.parse::<u64>().ok()),
        ) {
            (Some(index), Some(count)) => Some((index, count)),
            _ => None,
        };

        return Line::Progress(VideoProgress {
            downloaded: get(1).and_then(|value| value.parse().ok()).unwrap_or(0),
            // `total_bytes` first, then the estimate a fragmented feed gives.
            total: get(2)
                .and_then(|value| value.parse().ok())
                .or_else(|| get(3).and_then(|value| value.parse::<f64>().ok().map(|v| v as u64)))
                .filter(|total| *total > 0),
            speed: get(4)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|s| *s > 0.0),
            eta_seconds: get(5).and_then(|value| value.parse().ok()),
            fragment,
        });
    }

    // `[Merger] Merging formats into "/path/file.mkv"` is the final name when
    // separate audio and video streams were combined.
    if let Some(rest) = trimmed.strip_prefix("[Merger] Merging formats into ") {
        let path = rest.trim().trim_matches('"');
        if !path.is_empty() {
            return Line::Destination(PathBuf::from(path));
        }
    }
    if let Some(rest) = trimmed.strip_prefix("[ExtractAudio] Destination: ") {
        return Line::Destination(PathBuf::from(rest.trim()));
    }
    if let Some(rest) = trimmed.strip_prefix("[download] Destination: ") {
        return Line::Destination(PathBuf::from(rest.trim()));
    }

    // `[info] <title>: Downloading 1 format(s): mp4`
    if let Some(rest) = trimmed.strip_prefix("[info] ")
        && let Some((title, tail)) = rest.rsplit_once(": Downloading ")
        && tail.contains("format")
        && !title.is_empty()
    {
        return Line::Title(title.to_owned());
    }

    if trimmed.starts_with("ERROR:") {
        return Line::Error(trimmed.trim_start_matches("ERROR:").trim().to_owned());
    }

    Line::Ignored
}

/// Runs and tracks `yt-dlp` extractions.
pub struct VideoEngine {
    db: Database,
    jobs: Mutex<HashMap<i64, JoinHandle<()>>>,
}

impl VideoEngine {
    pub fn new(db: Database) -> Arc<Self> {
        Arc::new(Self {
            db,
            jobs: Mutex::new(HashMap::new()),
        })
    }

    fn jobs(&self) -> std::sync::MutexGuard<'_, HashMap<i64, JoinHandle<()>>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn running_count(&self) -> usize {
        self.jobs()
            .values()
            .filter(|task| !task.is_finished())
            .count()
    }

    /// Abort a job. `kill_on_drop` turns the abort into a killed subprocess.
    pub fn cancel(&self, job_id: i64) {
        if let Some(task) = self.jobs().remove(&job_id) {
            task.abort();
            log::info!("cancelled video job {job_id}");
        }
    }

    /// Queue an extraction and return its database id immediately.
    pub async fn start(
        self: &Arc<Self>,
        url: String,
        config: VideoConfig,
        proxies: Arc<ProxyManager>,
        events: mpsc::Sender<VideoEvent>,
    ) -> Result<i64> {
        let url = url.trim().to_owned();
        validate_url(&url)?;

        let job_id = self
            .db
            .create_media_job(
                PathBuf::from(&url),
                config.destination.clone(),
                "fetch-video".to_owned(),
            )
            .await
            .context("could not record the video job")?;

        let _ = events
            .send(VideoEvent::Started {
                job_id,
                url: url.clone(),
            })
            .await;

        let engine = Arc::clone(self);
        let task = tokio::spawn(async move {
            let outcome = engine
                .run(job_id, &url, &config, proxies.as_ref(), &events)
                .await;

            let (state, error) = match outcome {
                Ok(output) => {
                    let _ = events.send(VideoEvent::Finished { job_id, output }).await;
                    (JobState::Complete, None)
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    log::warn!("video job {job_id} failed: {message}");
                    let _ = events
                        .send(VideoEvent::Failed {
                            job_id,
                            error: message.clone(),
                        })
                        .await;
                    (JobState::Failed, Some(message))
                }
            };

            if let Err(error) = engine.db.finish_media_job(job_id, state, error).await {
                log::warn!("could not close video job {job_id}: {error:#}");
            }
            engine.jobs().remove(&job_id);
        });

        self.jobs().insert(job_id, task);
        Ok(job_id)
    }

    async fn run(
        &self,
        job_id: i64,
        url: &str,
        config: &VideoConfig,
        proxies: &ProxyManager,
        events: &mpsc::Sender<VideoEvent>,
    ) -> Result<Option<PathBuf>> {
        std::fs::create_dir_all(&config.destination)
            .with_context(|| format!("could not create {}", config.destination.display()))?;

        let binary = yt_dlp_binary();
        let mut command = Command::new(&binary);
        command
            // One progress line per tick instead of a redrawn carriage return.
            .arg("--newline")
            .arg("--no-colors")
            .arg("--progress")
            .arg("--progress-template")
            .arg(PROGRESS_TEMPLATE)
            // Never read the user's config: Snatch's behaviour must be its own.
            .arg("--ignore-config")
            .arg("--no-simulate")
            .arg("-o")
            .arg(config.destination.join("%(title)s [%(id)s].%(ext)s"));

        // An exact selector wins: the user picked that resolution by name, so
        // a quality band must not quietly widen it back out.
        match config.format.as_deref() {
            Some(selector) => {
                validate_format(selector)?;
                command.arg("-f").arg(selector);
            }
            None => {
                if let Some(selector) = config.quality.format_selector() {
                    command.arg("-f").arg(selector);
                }
            }
        }
        if config.format.is_none() && config.quality == VideoQuality::AudioOnly {
            command
                .arg("-x")
                .arg("--audio-format")
                .arg("mp3")
                .arg("--audio-quality")
                .arg("0");
        }
        if config.embed_metadata {
            command.arg("--embed-metadata").arg("--embed-chapters");
        }
        if config.subtitles {
            command
                .arg("--write-subs")
                .arg("--write-auto-subs")
                .arg("--sub-langs")
                .arg("en.*,-live_chat")
                .arg("--embed-subs");
        }
        command.arg(if config.playlist {
            "--yes-playlist"
        } else {
            "--no-playlist"
        });

        let task_key = format!("video:{job_id}");
        if let Some(proxy) = proxies
            .resolve_for(&task_key, Engine::Subprocess)
            .context("the video job cannot use the configured proxy")?
        {
            log::info!("video job {job_id} routed through {}", proxy.redacted());
            command.arg("--proxy").arg(proxy.url());
        }

        // `--` keeps a URL that begins with a dash from being read as a flag.
        command.arg("--").arg(url);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!("{binary} was not found in PATH; install the 'yt-dlp' package");
            }
            Err(error) => return Err(error).context("could not start yt-dlp"),
        };

        let stdout = child.stdout.take().context("yt-dlp produced no stdout")?;
        let stderr = child.stderr.take().context("yt-dlp produced no stderr")?;
        let mut out_lines = BufReader::new(stdout).lines();
        let mut err_lines = BufReader::new(stderr).lines();

        let mut destination: Option<PathBuf> = None;
        let mut last_error: Option<String> = None;
        let (mut stdout_open, mut stderr_open) = (true, true);

        // Both pipes must drain concurrently or yt-dlp blocks once one fills.
        while stdout_open || stderr_open {
            tokio::select! {
                line = out_lines.next_line(), if stdout_open => match line {
                    Ok(Some(line)) => {
                        self.consume(job_id, &line, events, &mut destination, &mut last_error).await;
                    }
                    Ok(None) => stdout_open = false,
                    Err(error) => {
                        log::warn!("video job {job_id}: could not read stdout: {error}");
                        stdout_open = false;
                    }
                },
                line = err_lines.next_line(), if stderr_open => match line {
                    Ok(Some(line)) => {
                        self.consume(job_id, &line, events, &mut destination, &mut last_error).await;
                    }
                    Ok(None) => stderr_open = false,
                    Err(_) => stderr_open = false,
                },
            }
        }

        let status = child
            .wait()
            .await
            .context("could not wait for yt-dlp to exit")?;

        if !status.success() {
            let code = status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "a signal".to_owned());
            match last_error {
                Some(message) => bail!("yt-dlp exited with {code}: {message}"),
                None => bail!("yt-dlp exited with {code}"),
            }
        }

        Ok(destination)
    }

    async fn consume(
        &self,
        job_id: i64,
        line: &str,
        events: &mpsc::Sender<VideoEvent>,
        destination: &mut Option<PathBuf>,
        last_error: &mut Option<String>,
    ) {
        match parse_line(line) {
            Line::Progress(progress) => {
                let _ = events.send(VideoEvent::Progress { job_id, progress }).await;
            }
            Line::Complete => {}
            Line::Destination(path) => {
                // A later line wins: a merge renames the per-stream file.
                *destination = Some(path);
            }
            Line::Title(title) => {
                let _ = events.send(VideoEvent::Title { job_id, title }).await;
            }
            Line::Error(message) => {
                log::warn!(target: "yt-dlp", "{message}");
                *last_error = Some(message);
            }
            Line::Ignored => {}
        }
    }
}

fn validate_url(url: &str) -> Result<()> {
    if url.is_empty() {
        bail!("the video URL is empty");
    }
    if url.contains(['\r', '\n']) {
        bail!("the video URL contains a line break");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("the video URL has no scheme");
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        bail!("unsupported video URL scheme '{scheme}'");
    }
    if rest.is_empty() {
        bail!("the video URL has no host");
    }
    Ok(())
}

/// Where extractions land: `<base>/Snatch Video`.
pub fn destination_for(base: &Path) -> PathBuf {
    base.join("Snatch Video")
}

// ---------------------------------------------------------------------------
// Format probing
//
// The browser overlay asks "what resolutions does this page have?" before the
// user picks one. That question is `yt-dlp -J`, whose answer is a few hundred
// kilobytes describing every stream the site offers -- 53 of them for an
// ordinary YouTube video. Handing that list to a menu would be useless, so it
// is distilled here into one entry per resolution.
//
// The choosing rules below are not taste. Each one comes from what the real
// listing looks like (captured from yt-dlp 2026.08.19, and pinned in the
// tests at the bottom of this file):
//
//   * The highest bitrate at a resolution is usually an HLS fragment feed with
//     no size, no resume and hundreds of pieces. The progressive stream beside
//     it is the one a download manager wants.
//   * YouTube's storyboards are formats too: `mhtml`, 27 to 180 pixels tall.
//     Left in, the menu offers "27p".
//   * `-drc` audio is the same track with the loudness squashed. It sits right
//     next to the real one and sorts identically.
// ---------------------------------------------------------------------------

/// How long a probe may take before it is given up on.
///
/// `snatch-nmh` allows the GUI 30 seconds to answer and the browser allows the
/// host its own budget on top. A probe that outlives either turns into a
/// mystery timeout in the page, so it is cut short here, where the reason can
/// still be reported.
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// A listing larger than this is not a video page. Real ones are ~150 KiB.
const MAX_PROBE_BYTES: usize = 32 * 1024 * 1024;

/// Most resolutions a menu can be asked to show.
const MAX_FORMATS: usize = 12;

/// One entry in the quality list, ready for the browser to render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaFormat {
    /// The `-f` selector that downloads this, e.g. `299+bestaudio[ext=m4a]/299`.
    pub id: String,
    /// What the user reads: `1080p60`, or `Audio only`.
    pub label: String,
    /// The container it should land in.
    pub ext: String,
    /// Video and audio together. `None` when the site reports neither a size
    /// nor a bitrate to work one out from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `true` when `size` was calculated from the bitrate rather than reported,
    /// so the menu can show it as approximate instead of promising a figure.
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub audio_only: bool,
}

/// What one page offers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaProbe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Seconds, when the site says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    pub formats: Vec<MediaFormat>,
}

/// One stream as yt-dlp describes it.
///
/// Every number is read as `f64`. yt-dlp mixes integer and floating point for
/// the same field between formats in a single listing -- `fps` came back as
/// both `60` and `60.0` -- and an `f64` accepts either, where a `u64` would
/// fail the whole parse on the first float.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawFormat {
    #[serde(default)]
    format_id: Option<String>,
    #[serde(default)]
    ext: Option<String>,
    #[serde(default)]
    height: Option<f64>,
    #[serde(default)]
    fps: Option<f64>,
    #[serde(default)]
    vcodec: Option<String>,
    #[serde(default)]
    acodec: Option<String>,
    #[serde(default)]
    filesize: Option<f64>,
    #[serde(default)]
    filesize_approx: Option<f64>,
    /// Total bitrate, kbit/s.
    #[serde(default)]
    tbr: Option<f64>,
    /// Audio bitrate, kbit/s.
    #[serde(default)]
    abr: Option<f64>,
    #[serde(default)]
    format_note: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    has_drm: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawInfo {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    formats: Vec<RawFormat>,
    /// A direct media URL has no `formats` array: the top level describes the
    /// single stream itself, in exactly the same fields.
    #[serde(flatten)]
    single: RawFormat,
}

impl RawFormat {
    fn has_video(&self) -> bool {
        matches!(self.vcodec.as_deref(), Some(codec) if codec != "none")
    }

    fn has_audio(&self) -> bool {
        matches!(self.acodec.as_deref(), Some(codec) if codec != "none")
    }

    /// Anything yt-dlp can hand over as one file. A fragment feed has no size
    /// up front and cannot be resumed, so it loses to a progressive stream at
    /// the same resolution -- but it is still kept, because some sites (live
    /// streams, most news video) publish nothing else.
    fn is_progressive(&self) -> bool {
        matches!(
            self.protocol.as_deref().unwrap_or("https"),
            "https" | "http"
        )
    }

    /// Storyboards are contact sheets of thumbnails, listed as formats with a
    /// height. Without this the menu offers to download "27p".
    fn is_storyboard(&self) -> bool {
        self.ext.as_deref() == Some("mhtml")
            || self.protocol.as_deref() == Some("mhtml")
            || self.format_note.as_deref() == Some("storyboard")
    }

    fn is_usable(&self) -> bool {
        self.format_id.is_some()
            && self.has_drm != Some(true)
            && !self.is_storyboard()
            && (self.has_video() || self.has_audio())
    }

    fn height(&self) -> Option<u32> {
        self.height
            .filter(|height| *height >= 1.0 && height.is_finite())
            .map(|height| height as u32)
    }

    /// Bytes, and whether that had to be worked out rather than read.
    fn size(&self, duration: Option<f64>) -> (Option<u64>, bool) {
        if let Some(bytes) = self.filesize.filter(|size| *size > 0.0) {
            return (Some(bytes as u64), false);
        }
        if let Some(bytes) = self.filesize_approx.filter(|size| *size > 0.0) {
            return (Some(bytes as u64), true);
        }
        // kbit/s over seconds: /8 for bytes, *1000 because it is kilobits.
        match (self.tbr.filter(|rate| *rate > 0.0), duration) {
            (Some(rate), Some(seconds)) if seconds > 0.0 => {
                (Some((rate * seconds * 125.0) as u64), true)
            }
            _ => (None, false),
        }
    }
}

/// Lower sorts better. H.264 plays on every phone, television and old laptop,
/// and hardware-decodes almost everywhere; AV1 is the smallest of the rest;
/// VP9 is the fallback. A download is kept and played later on some unknown
/// device, so this leans on compatibility rather than on file size.
fn video_codec_rank(codec: &str) -> u8 {
    let codec = codec.to_ascii_lowercase();
    if codec.starts_with("avc1") || codec.starts_with("h264") {
        0
    } else if codec.starts_with("av01") || codec.starts_with("av1") {
        1
    } else if codec.starts_with("vp9") || codec.starts_with("vp09") {
        2
    } else {
        3
    }
}

/// Lower sorts better. m4a plays anywhere; opus in a webm container does not.
fn audio_ext_rank(ext: &str) -> u8 {
    match ext {
        "m4a" | "mp4" => 0,
        "mp3" => 1,
        "opus" | "webm" | "ogg" => 2,
        _ => 3,
    }
}

/// yt-dlp's `-drc` variants are the same track with the dynamic range
/// squashed. They sit beside the real one and score identically, so without
/// this the "Audio only" entry is a coin toss between them.
fn is_drc(format: &RawFormat) -> bool {
    format
        .format_id
        .as_deref()
        .is_some_and(|id| id.ends_with("-drc"))
        || format
            .format_note
            .as_deref()
            .is_some_and(|note| note.contains("DRC"))
}

/// `1080p60`. The frame rate is only worth showing when it is not the usual
/// 24-30, which is the same rule the sites themselves use.
fn video_label(height: u32, fps: Option<f64>) -> String {
    match fps.map(|fps| fps.round() as u32) {
        Some(rate) if rate >= 50 => format!("{height}p{rate}"),
        _ => format!("{height}p"),
    }
}

/// The selector that downloads `format`, merging in audio when it needs some.
///
/// The `[ext=…]` preference keeps the container the menu promised: pairing an
/// mp4 video with an opus track makes yt-dlp fall back to Matroska, so a row
/// reading "mp4" would produce an .mkv. Both fallbacks stay in the chain, so a
/// site with only one audio track still works.
fn selector_for(format: &RawFormat, id: &str) -> String {
    if format.has_audio() {
        return id.to_owned();
    }
    match format.ext.as_deref() {
        Some("mp4") => format!("{id}+bestaudio[ext=m4a]/{id}+bestaudio/{id}"),
        Some("webm") => format!("{id}+bestaudio[ext=webm]/{id}+bestaudio/{id}"),
        _ => format!("{id}+bestaudio/{id}"),
    }
}

/// Turn a full listing into one entry per resolution, best first.
fn distil(info: &RawInfo) -> MediaProbe {
    let duration = info.duration.filter(|seconds| *seconds > 0.0);

    // A direct media URL describes itself at the top level with no `formats`.
    let owned;
    let formats: &[RawFormat] = if info.formats.is_empty() {
        owned = [info.single.clone()];
        &owned
    } else {
        &info.formats
    };
    let usable: Vec<&RawFormat> = formats.iter().filter(|f| f.is_usable()).collect();

    // Best audio: progressive first, then the real track over its squashed
    // twin, then the container that plays everywhere, then the highest rate.
    let best_audio = usable
        .iter()
        .filter(|f| f.has_audio() && !f.has_video())
        .min_by(|a, b| {
            let key = |f: &RawFormat| {
                (
                    u8::from(!f.is_progressive()),
                    u8::from(is_drc(f)),
                    audio_ext_rank(f.ext.as_deref().unwrap_or_default()),
                    -f.abr.or(f.tbr).unwrap_or_default(),
                )
            };
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied();
    let (audio_size, audio_estimated) = best_audio
        .map(|audio| audio.size(duration))
        .unwrap_or((None, false));

    // One winner per resolution.
    let mut by_height: HashMap<u32, &RawFormat> = HashMap::new();
    for format in usable.iter().filter(|f| f.has_video()) {
        let Some(height) = format.height() else {
            continue;
        };
        let key = |f: &RawFormat| {
            (
                // A single resumable file beats a fragment feed every time.
                u8::from(!f.is_progressive()),
                // No merge, no ffmpeg, nothing to go wrong.
                u8::from(!f.has_audio()),
                video_codec_rank(f.vcodec.as_deref().unwrap_or_default()),
                -f.tbr.unwrap_or_default(),
            )
        };
        by_height
            .entry(height)
            .and_modify(|current| {
                if key(format) < key(current) {
                    *current = format;
                }
            })
            .or_insert(format);
    }

    let mut heights: Vec<u32> = by_height.keys().copied().collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));

    let mut entries = Vec::new();
    for height in heights.into_iter().take(MAX_FORMATS) {
        let Some(format) = by_height.get(&height) else {
            continue;
        };
        let Some(id) = format.format_id.as_deref() else {
            continue;
        };
        let (video_size, video_estimated) = format.size(duration);
        // A merge downloads both streams, so the figure has to cover both.
        let (size, estimated) = if format.has_audio() {
            (video_size, video_estimated)
        } else {
            match (video_size, audio_size) {
                (Some(video), Some(audio)) => {
                    (Some(video + audio), video_estimated || audio_estimated)
                }
                (video, _) => (video, video_estimated),
            }
        };
        entries.push(MediaFormat {
            id: selector_for(format, id),
            label: video_label(height, format.fps),
            ext: format.ext.clone().unwrap_or_else(|| "mp4".to_owned()),
            size,
            estimated,
            height: Some(height),
            audio_only: false,
        });
    }

    // Some sites report no height at all. Rather than an empty menu, offer the
    // one thing that always works.
    if entries.is_empty() && usable.iter().any(|f| f.has_video()) {
        entries.push(MediaFormat {
            id: "bestvideo*+bestaudio/best".to_owned(),
            label: "Best available".to_owned(),
            ext: "mp4".to_owned(),
            size: None,
            estimated: false,
            height: None,
            audio_only: false,
        });
    }

    if let Some(audio) = best_audio
        && let Some(id) = audio.format_id.as_deref()
    {
        entries.push(MediaFormat {
            id: id.to_owned(),
            label: "Audio only".to_owned(),
            ext: audio.ext.clone().unwrap_or_else(|| "m4a".to_owned()),
            size: audio_size,
            estimated: audio_estimated,
            height: None,
            audio_only: true,
        });
    }

    MediaProbe {
        title: info
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(|title| title.chars().take(200).collect()),
        duration: duration.map(|seconds| seconds as u64),
        formats: entries,
    }
}

/// A `-f` selector is one argv entry and never touches a shell, but this one
/// arrives from a web browser. Holding it to the characters a real selector is
/// made of means nothing else can be smuggled into yt-dlp's argument list.
pub fn validate_format(selector: &str) -> Result<()> {
    if selector.is_empty() {
        bail!("the format selector is empty");
    }
    if selector.len() > 200 {
        bail!("the format selector is unreasonably long");
    }
    if selector.starts_with('-') {
        bail!("a format selector may not start with a dash");
    }
    if let Some(bad) = selector
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && !"+-./_*[]<>=,~^:?".contains(*c))
    {
        bail!("the format selector contains '{bad}'");
    }
    Ok(())
}

/// Ask yt-dlp what a page offers, without downloading any of it.
pub async fn probe(url: &str, proxies: &ProxyManager) -> Result<MediaProbe> {
    let url = url.trim();
    validate_url(url)?;

    let binary = yt_dlp_binary();
    let mut command = Command::new(&binary);
    command
        .arg("-J")
        .arg("--no-playlist")
        .arg("--ignore-config")
        .arg("--no-warnings")
        .arg("--no-colors")
        .arg("--no-progress");

    if let Some(proxy) = proxies
        .resolve_for("video-probe", Engine::Subprocess)
        .context("the format probe cannot use the configured proxy")?
    {
        command.arg("--proxy").arg(proxy.url());
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
            bail!("{binary} was not found in PATH; install the 'yt-dlp' package");
        }
        Err(error) => return Err(error).context("could not start yt-dlp"),
    };

    // `wait_with_output` drains both pipes at once; reading one to the end
    // first would deadlock as soon as the other filled. Dropping the future on
    // a timeout kills the child, because the command was built with
    // `kill_on_drop`.
    let output = timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .context("yt-dlp took too long to list the formats")?
        .context("could not read yt-dlp's format listing")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .filter_map(|line| line.trim().strip_prefix("ERROR:"))
            .next_back()
            .map(str::trim);
        match reason {
            Some(reason) => bail!("yt-dlp could not read that page: {reason}"),
            None => bail!("yt-dlp could not read that page"),
        }
    }
    if output.stdout.len() > MAX_PROBE_BYTES {
        bail!("yt-dlp returned an unreasonably large listing");
    }

    let info: RawInfo = serde_json::from_slice(&output.stdout)
        .context("yt-dlp printed something that was not a format listing")?;
    let probe = distil(&info);
    if probe.formats.is_empty() {
        bail!("yt-dlp found nothing downloadable on that page");
    }
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures below are literal lines captured from yt-dlp 2026.08.19.

    #[test]
    fn reads_a_progress_tick() {
        let Line::Progress(progress) =
            parse_line("SNATCH|downloading|130048|429790|NA|13983665.664931936|0|NA|NA")
        else {
            panic!("expected a progress line");
        };
        assert_eq!(progress.downloaded, 130_048);
        assert_eq!(progress.total, Some(429_790));
        assert_eq!(progress.eta_seconds, Some(0));
        assert!(progress.speed.is_some_and(|speed| speed > 13_000_000.0));
        assert!((progress.fraction().unwrap_or_default() - 0.3026).abs() < 0.001);
    }

    #[test]
    fn the_first_tick_has_no_speed_or_eta() {
        // Every field can be NA; none may be parsed without a fallback.
        let Line::Progress(progress) = parse_line("SNATCH|downloading|1024|429790|NA|NA|NA|NA|NA")
        else {
            panic!("expected a progress line");
        };
        assert_eq!(progress.downloaded, 1024);
        assert_eq!(progress.speed, None);
        assert_eq!(progress.eta_seconds, None);
        assert_eq!(progress.fragment, None);
    }

    #[test]
    fn a_finished_tick_is_not_progress() {
        assert_eq!(
            parse_line("SNATCH|finished|429790|429790|NA|21963155.5|NA|NA|NA"),
            Line::Complete
        );
    }

    #[test]
    fn a_live_stream_falls_back_to_fragment_counts() {
        // No byte total, but 30 of 120 fragments is real progress.
        let Line::Progress(progress) = parse_line("SNATCH|downloading|900|NA|NA|1000.0|NA|30|120")
        else {
            panic!("expected a progress line");
        };
        assert_eq!(progress.total, None);
        assert_eq!(progress.fragment, Some((30, 120)));
        assert_eq!(progress.fraction(), Some(0.25));
    }

    #[test]
    fn an_estimate_stands_in_for_a_missing_total() {
        let Line::Progress(progress) =
            parse_line("SNATCH|downloading|500|NA|2000.5|100.0|10|NA|NA")
        else {
            panic!("expected a progress line");
        };
        assert_eq!(progress.total, Some(2000));
    }

    #[test]
    fn progress_is_indeterminate_when_nothing_is_known() {
        let Line::Progress(progress) = parse_line("SNATCH|downloading|500|NA|NA|NA|NA|NA|NA")
        else {
            panic!("expected a progress line");
        };
        assert_eq!(progress.fraction(), None);
    }

    #[test]
    fn destinations_are_read_from_plain_output() {
        assert_eq!(
            parse_line("[download] Destination: /tmp/ytp/out/clip.mp4"),
            Line::Destination(PathBuf::from("/tmp/ytp/out/clip.mp4"))
        );
        // A merge renames the file, so its line must win over the earlier one.
        assert_eq!(
            parse_line(r#"[Merger] Merging formats into "/tmp/out/video.mkv""#),
            Line::Destination(PathBuf::from("/tmp/out/video.mkv"))
        );
        assert_eq!(
            parse_line("[ExtractAudio] Destination: /tmp/out/song.mp3"),
            Line::Destination(PathBuf::from("/tmp/out/song.mp3"))
        );
    }

    #[test]
    fn titles_and_errors_are_recognised() {
        assert_eq!(
            parse_line("[info] clip: Downloading 1 format(s): mp4"),
            Line::Title("clip".to_owned())
        );
        assert_eq!(
            parse_line("ERROR: [generic] Unable to download webpage: HTTP 404"),
            Line::Error("[generic] Unable to download webpage: HTTP 404".to_owned())
        );
        assert_eq!(
            parse_line("[generic] Extracting URL: http://x/"),
            Line::Ignored
        );
        assert_eq!(parse_line(""), Line::Ignored);
    }

    #[test]
    fn quality_maps_to_a_format_selector() {
        assert!(
            VideoQuality::UpTo720
                .format_selector()
                .is_some_and(|selector| selector.contains("height<=720"))
        );
        // -x chooses the stream for audio-only; a selector would fight it.
        assert_eq!(VideoQuality::AudioOnly.format_selector(), None);
    }

    #[test]
    fn rejects_urls_yt_dlp_cannot_fetch() {
        assert!(validate_url("https://example.com/watch?v=1").is_ok());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("magnet:?xt=urn:btih:abc").is_err());
        assert!(validate_url("https://x/\nX: y").is_err());
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// A real `yt-dlp -J` listing, cut down to the formats that decide each
    /// rule below. Captured from yt-dlp 2026.08.19.
    const YOUTUBE: &str = include_str!("testdata/youtube-formats.json");

    fn probe_fixture() -> MediaProbe {
        let info: RawInfo = serde_json::from_str(YOUTUBE).expect("the captured listing must parse");
        distil(&info)
    }

    fn labels(probe: &MediaProbe) -> Vec<&str> {
        probe.formats.iter().map(|f| f.label.as_str()).collect()
    }

    fn entry<'a>(probe: &'a MediaProbe, label: &str) -> &'a MediaFormat {
        probe
            .formats
            .iter()
            .find(|f| f.label == label)
            .unwrap_or_else(|| panic!("no {label} entry in {:?}", labels(probe)))
    }

    #[test]
    fn one_entry_per_resolution_biggest_first() {
        let probe = probe_fixture();
        // Four heights and one audio track out of fifteen raw formats.
        assert_eq!(
            labels(&probe),
            ["2160p60", "1080p60", "720p60", "144p", "Audio only"]
        );
        assert_eq!(probe.duration, Some(635));
        assert!(probe.title.is_some_and(|t| t.starts_with("Big Buck Bunny")));
    }

    #[test]
    fn storyboards_never_reach_the_menu() {
        // `sb0` is a contact sheet of thumbnails listed as a 180-pixel format.
        // Left in, the menu offers to download "180p".
        let probe = probe_fixture();
        assert!(!labels(&probe).contains(&"180p"));
        assert!(probe.formats.iter().all(|f| f.ext != "mhtml"));
    }

    #[test]
    fn a_single_file_beats_a_fragment_feed_at_the_same_height() {
        let probe = probe_fixture();
        // 628 has by far the highest bitrate at 2160p, and is HLS: no size, no
        // resume, hundreds of pieces. 312 is the same trap at 1080p.
        assert!(entry(&probe, "2160p60").id.starts_with("401"));
        assert!(entry(&probe, "1080p60").id.starts_with("299"));
    }

    #[test]
    fn h264_wins_where_a_site_offers_it() {
        let probe = probe_fixture();
        // 299 is 257 MB of H.264; 399 is 124 MB of AV1 at the same resolution.
        // The smaller file loses on purpose: a download is played later, on
        // some unknown device.
        let best = entry(&probe, "1080p60");
        assert!(best.id.starts_with("299"), "got {}", best.id);
        assert_eq!(best.ext, "mp4");
        // No H.264 exists at 2160p, so AV1 takes it over VP9.
        assert!(entry(&probe, "2160p60").id.starts_with("401"));
    }

    #[test]
    fn video_only_formats_carry_audio_in_with_them() {
        let probe = probe_fixture();
        let best = entry(&probe, "1080p60");
        // Without the `[ext=m4a]` preference yt-dlp may pair the mp4 video with
        // an opus track and mux to Matroska, so the row would promise an .mp4
        // and produce an .mkv.
        assert_eq!(best.id, "299+bestaudio[ext=m4a]/299+bestaudio/299");
        // 257,619,653 video + 10,271,496 audio: the figure has to cover both
        // streams, because both get downloaded.
        assert_eq!(best.size, Some(267_891_149));
        assert!(!best.estimated);
        assert!(!best.audio_only);
    }

    #[test]
    fn the_squashed_drc_track_never_wins() {
        let probe = probe_fixture();
        let audio = entry(&probe, "Audio only");
        // `140-drc` is the same track with the loudness flattened. It has an
        // identical bitrate and container, so it ties on every other rule.
        assert_eq!(audio.id, "140");
        assert_eq!(audio.ext, "m4a");
        assert_eq!(audio.size, Some(10_271_496));
        assert!(audio.audio_only);
        assert_eq!(audio.height, None);
    }

    #[test]
    fn frame_rates_are_shown_only_when_they_are_unusual() {
        let probe = probe_fixture();
        // 60 is worth saying; 30 is not.
        assert!(labels(&probe).contains(&"720p60"));
        assert!(labels(&probe).contains(&"144p"));
    }

    #[test]
    fn both_integer_and_floating_point_numbers_parse() {
        // yt-dlp mixes the two for the same field inside one listing: `fps` is
        // `30` on format 160 and `60.0` on format 312. Reading either as an
        // integer fails the whole probe on the first float.
        let info: RawInfo = serde_json::from_str(YOUTUBE).expect("must parse");
        let fps: Vec<Option<f64>> = info.formats.iter().map(|f| f.fps).collect();
        assert!(fps.contains(&Some(30.0)) && fps.contains(&Some(60.0)));
    }

    #[test]
    fn a_direct_media_url_describes_itself() {
        // A plain .mp4 has no `formats` array: the top level is the format.
        let info: RawInfo = serde_json::from_str(
            r#"{"title":"clip","duration":12.5,"format_id":"mp4","ext":"mp4",
                "height":720,"fps":25,"vcodec":"h264","acodec":"aac",
                "filesize":1048576,"protocol":"https"}"#,
        )
        .expect("must parse");
        let probe = distil(&info);
        assert_eq!(labels(&probe), ["720p"]);
        // It already has audio, so nothing is merged into it.
        assert_eq!(probe.formats[0].id, "mp4");
        assert_eq!(probe.formats[0].size, Some(1_048_576));
    }

    #[test]
    fn a_missing_size_is_worked_out_from_the_bitrate() {
        let info: RawInfo = serde_json::from_str(
            r#"{"duration":100,"formats":[
                {"format_id":"a","ext":"mp4","height":480,"vcodec":"avc1",
                 "acodec":"aac","tbr":1000,"protocol":"https"}]}"#,
        )
        .expect("must parse");
        let probe = distil(&info);
        // 1000 kbit/s for 100 s is 12.5 MB, and the menu must say so is a guess.
        assert_eq!(probe.formats[0].size, Some(12_500_000));
        assert!(probe.formats[0].estimated);
    }

    #[test]
    fn a_page_with_nothing_playable_distils_to_nothing() {
        let info: RawInfo = serde_json::from_str(r#"{"formats":[]}"#).expect("must parse");
        assert!(distil(&info).formats.is_empty());
    }

    #[test]
    fn sites_that_report_no_height_still_get_an_entry() {
        let info: RawInfo = serde_json::from_str(
            r#"{"formats":[{"format_id":"x","ext":"mp4","vcodec":"h264",
                           "acodec":"aac","protocol":"https"}]}"#,
        )
        .expect("must parse");
        let probe = distil(&info);
        assert_eq!(labels(&probe), ["Best available"]);
        assert_eq!(probe.formats[0].id, "bestvideo*+bestaudio/best");
    }

    #[test]
    fn drm_protected_streams_are_not_offered() {
        let info: RawInfo = serde_json::from_str(
            r#"{"formats":[{"format_id":"drm","ext":"mp4","height":1080,
                            "vcodec":"avc1","acodec":"aac","protocol":"https",
                            "has_drm":true}]}"#,
        )
        .expect("must parse");
        assert!(distil(&info).formats.is_empty());
    }

    #[test]
    fn selectors_reject_anything_that_is_not_one() {
        // The shapes this module actually produces.
        assert!(validate_format("299+bestaudio[ext=m4a]/299+bestaudio/299").is_ok());
        assert!(validate_format("bestvideo*+bestaudio/best").is_ok());
        assert!(validate_format("140").is_ok());

        // The selector arrives from a browser and becomes an argv entry.
        assert!(validate_format("").is_err());
        assert!(validate_format("--exec=rm -rf ~").is_err());
        assert!(validate_format("140 --exec touch /tmp/x").is_err());
        assert!(validate_format("140;id").is_err());
        assert!(validate_format("140\nbest").is_err());
        assert!(validate_format(&"1".repeat(201)).is_err());
    }
}
