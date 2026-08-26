//! Post-download media processing, driven by `ffmpeg`.
//!
//! ffmpeg's `-progress pipe:1` emits repeating key=value blocks:
//!
//! ```text
//! frame=75
//! fps=0.00
//! bitrate= 242.2kbits/s
//! total_size=90813
//! out_time_us=3000000
//! out_time_ms=3000000
//! out_time=00:00:03.000000
//! speed=33.1x
//! progress=continue
//! ```
//!
//! **`out_time_ms` is microseconds, not milliseconds.** That block was captured
//! from a file whose duration is exactly 3.000000 s, and `out_time_ms` reads
//! `3000000` — identical to `out_time_us`. The name is a long-standing ffmpeg
//! misnomer kept for compatibility. Reading it as milliseconds would make every
//! progress bar reach 100% a thousand times too early, so this module treats
//! both fields as microseconds and prefers `out_time_us`.
//!
//! The total duration comes from `ffprobe`, because ffmpeg itself only reports
//! elapsed output time, never the target.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

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

/// What to do with a finished download.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaAction {
    /// Strip the video track and encode audio to MP3.
    ExtractAudio { bitrate_kbps: u32 },
    /// Remux/transcode to MP4 (H.264 + AAC), copying streams when compatible.
    ConvertToMp4,
    /// Cut a section without re-encoding.
    Trim {
        start: Duration,
        end: Option<Duration>,
    },
    /// Combine a video-only and an audio-only file into one container.
    Mux { audio: PathBuf },
}

impl MediaAction {
    /// Stable identifier stored in the database.
    pub fn slug(&self) -> &'static str {
        match self {
            MediaAction::ExtractAudio { .. } => "extract-audio",
            MediaAction::ConvertToMp4 => "convert-mp4",
            MediaAction::Trim { .. } => "trim",
            MediaAction::Mux { .. } => "mux",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            MediaAction::ExtractAudio { .. } => "Extracting audio",
            MediaAction::ConvertToMp4 => "Converting to MP4",
            MediaAction::Trim { .. } => "Trimming",
            MediaAction::Mux { .. } => "Muxing",
        }
    }

    /// The extension the output should carry.
    pub fn output_extension(&self) -> &'static str {
        match self {
            MediaAction::ExtractAudio { .. } => "mp3",
            MediaAction::ConvertToMp4 | MediaAction::Mux { .. } => "mp4",
            // A trim keeps the source container; the caller overrides this.
            MediaAction::Trim { .. } => "",
        }
    }
}

/// One unit of work for ffmpeg.
#[derive(Debug, Clone)]
pub struct MediaJob {
    pub input: PathBuf,
    pub output: PathBuf,
    pub action: MediaAction,
    /// Overwrite the output if it exists.
    pub overwrite: bool,
}

impl MediaJob {
    /// Build a job whose output sits beside the input with a suitable name.
    pub fn beside_input(input: PathBuf, action: MediaAction) -> Self {
        let extension = match action.output_extension() {
            "" => input
                .extension()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| "mkv".to_owned()),
            other => other.to_owned(),
        };

        let stem = input
            .file_stem()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_owned());

        let suffix = match &action {
            MediaAction::ExtractAudio { .. } => "audio",
            MediaAction::ConvertToMp4 => "mp4",
            MediaAction::Trim { .. } => "trimmed",
            MediaAction::Mux { .. } => "muxed",
        };

        let directory = input.parent().unwrap_or(Path::new(".")).to_path_buf();
        let output = directory.join(format!("{stem}.{suffix}.{extension}"));
        Self {
            input,
            output,
            action,
            overwrite: false,
        }
    }
}

/// Live progress for one job.
#[derive(Debug, Clone)]
pub struct MediaProgress {
    /// Output position produced so far.
    pub elapsed: Duration,
    /// Total duration of the source, when ffprobe could determine it.
    pub total: Option<Duration>,
    /// Encoding rate, e.g. `33.1` for `speed=33.1x`.
    pub speed: Option<f64>,
    pub output_bytes: u64,
}

impl MediaProgress {
    pub fn fraction(&self) -> Option<f64> {
        let total = self.total?;
        if total.is_zero() {
            return None;
        }
        Some((self.elapsed.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0))
    }

    /// Wall-clock time left, derived from the encoding speed.
    pub fn eta(&self) -> Option<Duration> {
        let total = self.total?;
        let speed = self.speed.filter(|value| *value > 0.0)?;
        let remaining = total.checked_sub(self.elapsed)?;
        Some(Duration::from_secs_f64(remaining.as_secs_f64() / speed))
    }
}

#[derive(Debug, Clone)]
pub enum MediaEvent {
    Started {
        job_id: i64,
        label: &'static str,
    },
    Progress {
        job_id: i64,
        progress: MediaProgress,
    },
    Finished {
        job_id: i64,
        output: PathBuf,
    },
    Failed {
        job_id: i64,
        error: String,
    },
}

/// One `key=value` pair from ffmpeg's progress stream.
#[derive(Debug, PartialEq)]
enum Field {
    /// Output position, always in microseconds regardless of the key name.
    OutTimeMicros(u64),
    Speed(f64),
    TotalSize(u64),
    Done,
    Ignored,
}

fn parse_progress_line(line: &str) -> Field {
    let Some((key, value)) = line.split_once('=') else {
        return Field::Ignored;
    };
    let key = key.trim();
    let value = value.trim();

    // ffmpeg emits N/A before the first frame is written.
    if value.is_empty() || value.eq_ignore_ascii_case("N/A") {
        return Field::Ignored;
    }

    match key {
        // Both keys carry microseconds; see the module docs.
        "out_time_us" | "out_time_ms" => value
            .parse::<i64>()
            .ok()
            .filter(|micros| *micros >= 0)
            .map(|micros| Field::OutTimeMicros(micros as u64))
            .unwrap_or(Field::Ignored),
        "speed" => value
            .trim_end_matches('x')
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|speed| speed.is_finite() && *speed > 0.0)
            .map(Field::Speed)
            .unwrap_or(Field::Ignored),
        "total_size" => value
            .parse::<u64>()
            .map(Field::TotalSize)
            .unwrap_or(Field::Ignored),
        "progress" if value == "end" => Field::Done,
        _ => Field::Ignored,
    }
}

/// Ask ffprobe how long the source is. `None` when it cannot tell.
pub async fn probe_duration(path: &Path) -> Result<Option<Duration>> {
    let output = Command::new(ffprobe_binary())
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=nw=1:nk=1")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("could not run {}", ffprobe_binary()))?;

    if !output.status.success() {
        // A missing duration is not fatal: the job still runs, just without a
        // determinate progress bar.
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "ffprobe could not read {}: {}",
            path.display(),
            stderr.trim()
        );
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(Duration::from_secs_f64))
}

/// Build the ffmpeg argument list for a job.
///
/// Kept separate from execution so it can be asserted in tests without
/// spawning anything.
fn build_args(job: &MediaJob) -> Vec<String> {
    let mut args: Vec<String> = vec![
        // Never wait on a terminal; we give ffmpeg a null stdin anyway.
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        // Machine-readable progress on stdout, leaving stderr for real errors.
        "-progress".into(),
        "pipe:1".into(),
    ];

    args.push(if job.overwrite {
        "-y".into()
    } else {
        "-n".into()
    });

    // A trim seeks before the input so ffmpeg can skip without decoding.
    if let MediaAction::Trim { start, .. } = &job.action
        && !start.is_zero()
    {
        args.push("-ss".into());
        args.push(format!("{:.3}", start.as_secs_f64()));
    }

    args.push("-i".into());
    args.push(job.input.to_string_lossy().into_owned());

    match &job.action {
        MediaAction::ExtractAudio { bitrate_kbps } => {
            args.push("-vn".into());
            args.push("-map".into());
            args.push("0:a:0".into());
            args.push("-c:a".into());
            args.push("libmp3lame".into());
            args.push("-b:a".into());
            args.push(format!("{bitrate_kbps}k"));
        }
        MediaAction::ConvertToMp4 => {
            args.push("-c:v".into());
            args.push("libx264".into());
            args.push("-preset".into());
            args.push("veryfast".into());
            args.push("-crf".into());
            args.push("21".into());
            args.push("-c:a".into());
            args.push("aac".into());
            args.push("-b:a".into());
            args.push("192k".into());
            // Let a player start before the whole file has arrived.
            args.push("-movflags".into());
            args.push("+faststart".into());
        }
        MediaAction::Trim { start, end } => {
            if let Some(end) = end {
                let duration = end.saturating_sub(*start);
                args.push("-t".into());
                args.push(format!("{:.3}", duration.as_secs_f64()));
            }
            // Stream copy: a trim should be instant and lossless.
            args.push("-c".into());
            args.push("copy".into());
            args.push("-avoid_negative_ts".into());
            args.push("make_zero".into());
        }
        MediaAction::Mux { audio } => {
            args.push("-i".into());
            args.push(audio.to_string_lossy().into_owned());
            args.push("-map".into());
            args.push("0:v:0".into());
            args.push("-map".into());
            args.push("1:a:0".into());
            args.push("-c".into());
            args.push("copy".into());
            args.push("-shortest".into());
        }
    }

    args.push(job.output.to_string_lossy().into_owned());
    args
}

/// Run one ffmpeg job, reporting progress until it finishes.
pub async fn execute_ffmpeg_job(
    job_id: i64,
    job: &MediaJob,
    events: &mpsc::Sender<MediaEvent>,
) -> Result<PathBuf> {
    if !job.input.is_file() {
        bail!("{} does not exist", job.input.display());
    }
    if let MediaAction::Mux { audio } = &job.action
        && !audio.is_file()
    {
        bail!("{} does not exist", audio.display());
    }
    if job.output == job.input {
        bail!("the output would overwrite the input");
    }
    if !job.overwrite && job.output.exists() {
        bail!("{} already exists", job.output.display());
    }
    if let Some(parent) = job.output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }

    // A trim's own bounds are more accurate than the source duration.
    let total = match &job.action {
        MediaAction::Trim {
            start,
            end: Some(end),
        } => Some(end.saturating_sub(*start)),
        _ => probe_duration(&job.input).await.unwrap_or(None),
    };

    let _ = events
        .send(MediaEvent::Started {
            job_id,
            label: job.action.label(),
        })
        .await;

    let binary = ffmpeg_binary();
    let mut child = match Command::new(&binary)
        .args(build_args(job))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
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

    let mut current = MediaProgress {
        elapsed: Duration::ZERO,
        total,
        speed: None,
        output_bytes: 0,
    };
    // ffmpeg at -loglevel error is quiet unless something is wrong, so
    // whatever lands on stderr is the explanation for a non-zero exit.
    let mut diagnostics: Vec<String> = Vec::new();
    let (mut stdout_open, mut stderr_open) = (true, true);

    while stdout_open || stderr_open {
        tokio::select! {
            line = progress_lines.next_line(), if stdout_open => match line {
                Ok(Some(line)) => {
                    let mut changed = false;
                    match parse_progress_line(&line) {
                        Field::OutTimeMicros(micros) => {
                            current.elapsed = Duration::from_micros(micros);
                            changed = true;
                        }
                        Field::Speed(speed) => current.speed = Some(speed),
                        Field::TotalSize(bytes) => current.output_bytes = bytes,
                        // A block ends with `progress=continue|end`; publish then.
                        Field::Done => changed = true,
                        Field::Ignored => {}
                    }
                    if changed {
                        let _ = events
                            .send(MediaEvent::Progress { job_id, progress: current.clone() })
                            .await;
                    }
                }
                Ok(None) => stdout_open = false,
                Err(error) => {
                    log::warn!("ffmpeg job {job_id}: could not read progress: {error}");
                    stdout_open = false;
                }
            },
            line = error_lines.next_line(), if stderr_open => match line {
                Ok(Some(line)) => {
                    let line = line.trim().to_owned();
                    if !line.is_empty() {
                        log::warn!(target: "ffmpeg", "{line}");
                        // Keep the tail; ffmpeg can be verbose when it fails.
                        if diagnostics.len() < 20 {
                            diagnostics.push(line);
                        }
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
        // A partial output file is worse than none: a half-written MP4 looks
        // like a real result to the user and to the file manager.
        if let Err(error) = tokio::fs::remove_file(&job.output).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("could not clean up {}: {error}", job.output.display());
        }
        let detail = if diagnostics.is_empty() {
            String::new()
        } else {
            format!(": {}", diagnostics.join("; "))
        };
        bail!("ffmpeg exited with {code}{detail}");
    }

    let _ = events
        .send(MediaEvent::Finished {
            job_id,
            output: job.output.clone(),
        })
        .await;
    Ok(job.output.clone())
}

/// Human-readable `H:MM:SS`, used by the conversion row.
pub fn format_duration(value: Duration) -> String {
    let seconds = value.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

impl fmt::Display for MediaAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A serial queue of ffmpeg jobs.
///
/// Serial on purpose: ffmpeg already saturates the available cores, so running
/// several encodes at once makes every one of them slower and the progress
/// bars less honest.
pub struct MediaQueue {
    db: crate::db::Database,
    events: mpsc::Sender<MediaEvent>,
    pending: tokio::sync::Mutex<()>,
    running: std::sync::atomic::AtomicUsize,
}

impl MediaQueue {
    pub fn new(db: crate::db::Database, events: mpsc::Sender<MediaEvent>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            db,
            events,
            pending: tokio::sync::Mutex::new(()),
            running: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Jobs waiting for or holding the encoder slot.
    pub fn outstanding(&self) -> usize {
        self.running.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Queue a job. Returns once it has finished, so callers should spawn it.
    pub async fn submit(self: std::sync::Arc<Self>, job: MediaJob) -> Result<PathBuf> {
        self.running
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Released when this guard drops, even on an early return.
        let _slot = ReleaseOnDrop(std::sync::Arc::clone(&self));

        let job_id = self
            .db
            .create_media_job(
                job.input.clone(),
                job.output.clone(),
                job.action.slug().to_owned(),
            )
            .await
            .context("could not record the media job")?;

        // One encode at a time.
        let _turn = self.pending.lock().await;

        match execute_ffmpeg_job(job_id, &job, &self.events).await {
            Ok(output) => {
                if let Err(error) = self
                    .db
                    .finish_media_job(job_id, crate::db::JobState::Complete, None)
                    .await
                {
                    log::warn!("could not close media job {job_id}: {error:#}");
                }
                Ok(output)
            }
            Err(error) => {
                let message = format!("{error:#}");
                if let Err(error) = self
                    .db
                    .finish_media_job(job_id, crate::db::JobState::Failed, Some(message.clone()))
                    .await
                {
                    log::warn!("could not close media job {job_id}: {error:#}");
                }
                let _ = self
                    .events
                    .send(MediaEvent::Failed {
                        job_id,
                        error: message,
                    })
                    .await;
                Err(error)
            }
        }
    }
}

struct ReleaseOnDrop(std::sync::Arc<MediaQueue>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0
            .running
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_time_ms_is_microseconds_not_milliseconds() {
        // Captured from ffmpeg 8.1.2 encoding a file of exactly 3.000000 s.
        // If this were milliseconds the position would be 50 minutes.
        assert_eq!(
            parse_progress_line("out_time_ms=3000000"),
            Field::OutTimeMicros(3_000_000)
        );
        assert_eq!(
            parse_progress_line("out_time_us=3000000"),
            Field::OutTimeMicros(3_000_000)
        );

        let progress = MediaProgress {
            elapsed: Duration::from_micros(3_000_000),
            total: Some(Duration::from_secs(3)),
            speed: None,
            output_bytes: 0,
        };
        assert_eq!(progress.fraction(), Some(1.0));
        assert_eq!(progress.elapsed.as_secs(), 3);
    }

    #[test]
    fn parses_the_rest_of_a_real_progress_block() {
        assert_eq!(parse_progress_line("speed=33.1x"), Field::Speed(33.1));
        assert_eq!(
            parse_progress_line("total_size=90813"),
            Field::TotalSize(90813)
        );
        assert_eq!(parse_progress_line("progress=end"), Field::Done);
        assert_eq!(parse_progress_line("progress=continue"), Field::Ignored);
        assert_eq!(parse_progress_line("frame=75"), Field::Ignored);
        assert_eq!(parse_progress_line("bitrate= 242.2kbits/s"), Field::Ignored);
    }

    #[test]
    fn tolerates_the_values_ffmpeg_emits_before_the_first_frame() {
        assert_eq!(parse_progress_line("out_time_us=N/A"), Field::Ignored);
        assert_eq!(parse_progress_line("speed=N/A"), Field::Ignored);
        assert_eq!(parse_progress_line("speed=   0x"), Field::Ignored);
        assert_eq!(parse_progress_line("out_time_us=-1"), Field::Ignored);
        assert_eq!(parse_progress_line("garbage"), Field::Ignored);
        assert_eq!(parse_progress_line("out_time_us="), Field::Ignored);
    }

    #[test]
    fn progress_without_a_known_duration_is_indeterminate() {
        let progress = MediaProgress {
            elapsed: Duration::from_secs(5),
            total: None,
            speed: Some(2.0),
            output_bytes: 10,
        };
        assert_eq!(progress.fraction(), None);
        assert_eq!(progress.eta(), None);
    }

    #[test]
    fn eta_accounts_for_encoding_speed() {
        let progress = MediaProgress {
            elapsed: Duration::from_secs(10),
            total: Some(Duration::from_secs(70)),
            speed: Some(2.0),
            output_bytes: 0,
        };
        // 60 s of material left at 2x realtime is 30 s of waiting.
        assert_eq!(progress.eta(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn extract_audio_builds_a_video_free_mp3_command() {
        let job = MediaJob::beside_input(
            PathBuf::from("/m/Clip.mkv"),
            MediaAction::ExtractAudio { bitrate_kbps: 192 },
        );
        assert_eq!(job.output, PathBuf::from("/m/Clip.audio.mp3"));

        let args = build_args(&job);
        assert!(args.windows(2).any(|w| w == ["-progress", "pipe:1"]));
        assert!(args.contains(&"-vn".to_owned()));
        assert!(args.windows(2).any(|w| w == ["-c:a", "libmp3lame"]));
        assert!(args.windows(2).any(|w| w == ["-b:a", "192k"]));
        // Without -n an existing file would be silently clobbered.
        assert!(args.contains(&"-n".to_owned()));
        assert_eq!(args.last().map(String::as_str), Some("/m/Clip.audio.mp3"));
    }

    #[test]
    fn trim_seeks_before_the_input_and_copies_streams() {
        let job = MediaJob::beside_input(
            PathBuf::from("/m/Clip.mp4"),
            MediaAction::Trim {
                start: Duration::from_secs(30),
                end: Some(Duration::from_secs(90)),
            },
        );
        // A trim keeps the source container.
        assert_eq!(job.output, PathBuf::from("/m/Clip.trimmed.mp4"));

        let args = build_args(&job);
        let seek = args
            .iter()
            .position(|a| a == "-ss")
            .expect("-ss is present");
        let input = args.iter().position(|a| a == "-i").expect("-i is present");
        assert!(seek < input, "-ss must precede -i so ffmpeg can fast-seek");
        assert_eq!(args[seek + 1], "30.000");
        // 90 s end minus 30 s start is a 60 s duration, not an end timestamp.
        let t = args.iter().position(|a| a == "-t").expect("-t is present");
        assert_eq!(args[t + 1], "60.000");
        assert!(args.windows(2).any(|w| w == ["-c", "copy"]));
    }

    #[test]
    fn mux_maps_one_stream_from_each_input() {
        let job = MediaJob {
            input: PathBuf::from("/m/video.mp4"),
            output: PathBuf::from("/m/joined.mp4"),
            action: MediaAction::Mux {
                audio: PathBuf::from("/m/audio.m4a"),
            },
            overwrite: true,
        };
        let args = build_args(&job);
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 2);
        assert!(args.windows(2).any(|w| w == ["-map", "0:v:0"]));
        assert!(args.windows(2).any(|w| w == ["-map", "1:a:0"]));
        assert!(args.contains(&"-y".to_owned()), "overwrite was requested");
    }

    #[test]
    fn convert_to_mp4_enables_faststart() {
        let job = MediaJob::beside_input(PathBuf::from("/m/a.webm"), MediaAction::ConvertToMp4);
        assert_eq!(job.output, PathBuf::from("/m/a.mp4.mp4"));
        let args = build_args(&job);
        assert!(args.windows(2).any(|w| w == ["-movflags", "+faststart"]));
    }

    /// End-to-end against the real binary: generate a clip, extract its audio,
    /// and check that the progress stream actually tracked it. Skipped when
    /// ffmpeg is absent so the suite stays runnable anywhere.
    #[tokio::test]
    async fn drives_a_real_ffmpeg_job_to_completion() {
        if which("ffmpeg").is_none() || which("ffprobe").is_none() {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        }

        let dir = std::env::temp_dir().join("snatch-ffmpeg-e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let source = dir.join("clip.mp4");

        let generated = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=4",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&source)
            .output()
            .await
            .expect("ffmpeg runs");
        assert!(generated.status.success(), "could not build the test clip");

        // ffprobe must agree the clip is ~4 s, or the fraction check is vacuous.
        let duration = probe_duration(&source)
            .await
            .expect("probe succeeds")
            .expect("the clip has a duration");
        assert!(
            (duration.as_secs_f64() - 4.0).abs() < 0.5,
            "unexpected clip duration: {duration:?}"
        );

        let job = MediaJob::beside_input(
            source.clone(),
            MediaAction::ExtractAudio { bitrate_kbps: 128 },
        );
        let (tx, mut rx) = mpsc::channel(256);
        let output = execute_ffmpeg_job(1, &job, &tx)
            .await
            .expect("the job succeeds");
        drop(tx);

        let mut fractions = Vec::new();
        let mut finished = false;
        while let Some(event) = rx.recv().await {
            match event {
                MediaEvent::Progress { progress, .. } => {
                    if let Some(fraction) = progress.fraction() {
                        fractions.push(fraction);
                    }
                }
                MediaEvent::Finished { .. } => finished = true,
                _ => {}
            }
        }

        assert!(finished, "no Finished event was emitted");
        assert!(output.is_file(), "{} was not written", output.display());
        assert!(
            std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0) > 0,
            "the extracted audio is empty"
        );
        assert!(
            !fractions.is_empty(),
            "no determinate progress was reported"
        );
        // The decisive check: had out_time_ms been read as milliseconds, the
        // very first sample would already exceed 1.0 and be clamped.
        assert!(
            fractions.iter().all(|f| (0.0..=1.0).contains(f)),
            "progress left the 0..1 range: {fractions:?}"
        );
        assert!(
            fractions.last().copied().unwrap_or(0.0) > 0.9,
            "progress never approached completion: {fractions:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn which(binary: &str) -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(binary))
                .find(|candidate| candidate.is_file())
        })
    }

    #[test]
    fn durations_render_with_hours_only_when_needed() {
        assert_eq!(format_duration(Duration::from_secs(59)), "0:59");
        assert_eq!(format_duration(Duration::from_secs(600)), "10:00");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1:01:01");
    }
}
