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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

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

/// What ffprobe could work out about a stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamInfo {
    /// `None` for a live stream, which has no end to measure to.
    pub duration: Option<Duration>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    /// Which stream of the input the numbers above describe.
    ///
    /// An HLS master playlist lists every rendition, so ffprobe reports a
    /// video stream per quality. ffmpeg left to itself takes the first, which
    /// is usually the smallest -- the row would promise 1080p and record
    /// 480p. These say which one it must actually take.
    pub video_index: Option<u32>,
    pub audio_index: Option<u32>,
}

impl StreamInfo {
    /// A stream with no duration is one that has not finished happening.
    pub fn is_live(&self) -> bool {
        self.duration.is_none()
    }

    pub fn has_video(&self) -> bool {
        self.video_codec.is_some()
    }

    /// What the picker shows: `1080p live`, `720p · 42:10`, `Audio stream`.
    pub fn label(&self) -> String {
        let quality = match (self.height, self.has_video()) {
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
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    /// ffprobe prints this as a string, and omits it for a live stream.
    #[serde(default)]
    duration: Option<String>,
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
        .arg("-show_streams");
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

    let mut info = StreamInfo {
        duration,
        ..StreamInfo::default()
    };
    for (position, stream) in raw.streams.iter().enumerate() {
        // ffprobe reports the input's own index; fall back to the position in
        // the list, which is the same thing for every stream seen so far.
        let index = stream.index.unwrap_or(position as u32);
        match stream.codec_type.as_deref() {
            Some("video") => {
                // Keep the tallest: a manifest carries one per rendition.
                if stream.height > info.height || info.video_codec.is_none() {
                    info.height = stream.height.or(info.height);
                    info.video_codec = stream.codec_name.clone();
                    info.video_index = Some(index);
                }
            }
            Some("audio") if info.audio_codec.is_none() => {
                info.audio_codec = stream.codec_name.clone();
                info.audio_index = Some(index);
            }
            _ => {}
        }
    }
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

/// Record `url` until it ends, or until the job is cancelled.
///
/// Cancelling aborts the task, which kills ffmpeg, which is why a live
/// recording is written to Matroska: the file has to survive being stopped
/// mid-write, and that is the whole point of the container.
pub async fn record(
    job_id: i64,
    url: &str,
    hint: Option<&str>,
    directory: &Path,
    headers: &Headers,
    proxies: &ProxyManager,
    events: &mpsc::Sender<VideoEvent>,
) -> Result<PathBuf> {
    validate_url(url)?;

    // Probing first is what decides the container, and it turns the row in the
    // window from "stream" into "1080p live" before a byte is written.
    let info = probe(url, headers).await?;
    let name = output_name(hint, url);
    let _ = events
        .send(VideoEvent::Title {
            job_id,
            title: format!("{name} ({})", info.label()),
        })
        .await;

    std::fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let output = unique_path(directory, &name, info.container());

    let binary = ffmpeg_binary();
    let mut command = Command::new(&binary);
    command
        .arg("-hide_banner")
        // Never let ffmpeg try to read the terminal; it is not attached to one.
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error");
    apply_headers(&mut command, headers);
    command.arg("-i").arg(url);
    // Take the rendition the row promised, not whichever ffmpeg would have
    // picked. Without this a master playlist records its smallest variant.
    if let Some(index) = info.video_index {
        command.arg("-map").arg(format!("0:{index}"));
    }
    if let Some(index) = info.audio_index {
        command.arg("-map").arg(format!("0:{index}"));
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
        .stdin(Stdio::null())
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
            line = progress_lines.next_line(), if stdout_open => match line {
                Ok(Some(line)) => {
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
                        }
                        Field::Speed(_) | Field::Done | Field::Ignored => {}
                    }
                    let _ = events
                        .send(VideoEvent::Progress { job_id, progress: current.clone() })
                        .await;
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
        assert_eq!(info.height, Some(1080));
        assert_eq!(info.video_index, Some(1));
        assert_eq!(info.audio_index, Some(2));
    }

    #[test]
    fn a_stream_list_without_indices_falls_back_to_its_order() {
        let info = probe_of(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264","height":720},
                           {"codec_type":"audio","codec_name":"aac"}],
                "format":{}}"#,
        );
        assert_eq!(info.video_index, Some(0));
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
