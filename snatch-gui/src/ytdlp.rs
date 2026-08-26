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

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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

        if let Some(selector) = config.quality.format_selector() {
            command.arg("-f").arg(selector);
        }
        if config.quality == VideoQuality::AudioOnly {
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
