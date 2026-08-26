//! Image-board and social-media scraping, driven by `gallery-dl`.
//!
//! `gallery-dl` is a subprocess, not a library, so this module is really a
//! protocol adapter. The format below was captured from gallery-dl 1.32.9
//! rather than inferred, because the two streams carry different halves of the
//! information and both are needed:
//!
//! | stream | line                              | meaning                     |
//! |--------|-----------------------------------|-----------------------------|
//! | stdout | `/abs/path/file.png`              | file was downloaded         |
//! | stdout | `# /abs/path/file.png`            | file already existed        |
//! | stderr | `[3/12] https://…`                | index **and batch total**   |
//! | stderr | `[download][error] Failed to …`   | one file failed             |
//! | stderr | `[section][warning] …`            | non-fatal problem           |
//!
//! Only stderr knows how many files there are, and only stdout knows where they
//! landed, so both are read concurrently and merged.
//!
//! Note that gallery-dl is not packaged by every distribution (Fedora, for
//! one, does not ship it). Upstream moved to Codeberg and publishes a
//! standalone `gallery-dl.bin` for Linux; `install.sh` points at that.

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

/// The binary we invoke. Overridable for testing and for unusual installs.
fn gallery_dl_binary() -> String {
    std::env::var("SNATCH_GALLERY_DL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "gallery-dl".to_owned())
}

/// Knobs the UI exposes for a scrape.
#[derive(Debug, Clone)]
pub struct GalleryConfig {
    /// Base directory. gallery-dl appends its own `site/author/...` structure
    /// underneath, which is where the automatic organisation comes from.
    pub destination: PathBuf,
    /// Write gallery metadata next to the files.
    pub write_info_json: bool,
    /// Re-download files that already exist.
    pub overwrite: bool,
    /// `a-b` style index range, passed through to `--range`.
    pub range: Option<String>,
    /// Extra arguments, appended verbatim before the URL.
    pub extra_args: Vec<String>,
}

impl GalleryConfig {
    pub fn new(destination: PathBuf) -> Self {
        Self {
            destination,
            write_info_json: true,
            overwrite: false,
            range: None,
            extra_args: Vec::new(),
        }
    }
}

/// What the UI is told while a batch runs.
#[derive(Debug, Clone)]
pub enum GalleryEvent {
    Started {
        batch_id: i64,
        url: String,
    },
    /// gallery-dl revealed how many files the batch contains.
    Total {
        batch_id: i64,
        total: u64,
    },
    File {
        batch_id: i64,
        path: PathBuf,
        skipped: bool,
    },
    Warning {
        batch_id: i64,
        message: String,
    },
    Finished {
        batch_id: i64,
        state: JobState,
        error: Option<String>,
    },
}

/// One parsed line of gallery-dl output.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Downloaded(PathBuf),
    Skipped(PathBuf),
    /// `[3/12] url` — the index and the total.
    Progress {
        index: u64,
        total: u64,
    },
    Failed(String),
    Warning(String),
    Ignored,
}

/// Parse one line of gallery-dl **stdout**: a path, optionally `# `-prefixed.
fn parse_stdout(line: &str) -> Line {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() {
        return Line::Ignored;
    }

    // "# " marks a file that was already on disk.
    if let Some(rest) = trimmed.strip_prefix("# ") {
        let path = rest.trim();
        return if path.is_empty() {
            Line::Ignored
        } else {
            Line::Skipped(PathBuf::from(path))
        };
    }

    // Anything else on stdout is a written path. Guard against a stray log line
    // by requiring something that looks like a path rather than a `[tag]`.
    if trimmed.starts_with('[') {
        return Line::Ignored;
    }
    Line::Downloaded(PathBuf::from(trimmed))
}

/// Parse one line of gallery-dl **stderr**: progress counters and diagnostics.
fn parse_stderr(line: &str) -> Line {
    let trimmed = line.trim_end_matches(['\r', '\n']).trim();
    if trimmed.is_empty() {
        return Line::Ignored;
    }

    // `[3/12] https://example.com/image.png`
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let inside = &rest[..end];
        if let Some((index, total)) = inside.split_once('/')
            && let (Ok(index), Ok(total)) = (index.trim().parse(), total.trim().parse::<u64>())
            && total > 0
        {
            return Line::Progress { index, total };
        }
    }

    // `[downloader.http][warning] '404 …' for '…'` / `[download][error] …`
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.contains("[error]") {
        return Line::Failed(strip_tags(trimmed));
    }
    if lowered.contains("[warning]") {
        return Line::Warning(strip_tags(trimmed));
    }
    Line::Ignored
}

/// Drop gallery-dl's leading `[tag][level]` markers for display.
fn strip_tags(line: &str) -> String {
    let mut rest = line;
    while rest.starts_with('[') {
        match rest.find(']') {
            Some(end) => rest = rest[end + 1..].trim_start(),
            None => break,
        }
    }
    if rest.is_empty() {
        line.to_owned()
    } else {
        rest.to_owned()
    }
}

/// Runs and tracks `gallery-dl` batches.
pub struct GalleryEngine {
    db: Database,
    jobs: Mutex<HashMap<i64, JoinHandle<()>>>,
}

impl GalleryEngine {
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

    #[allow(dead_code, reason = "queried by socket clients; the UI tracks rows")]
    pub fn is_running(&self, batch_id: i64) -> bool {
        self.jobs()
            .get(&batch_id)
            .is_some_and(|task| !task.is_finished())
    }

    pub fn running_count(&self) -> usize {
        self.jobs().values().filter(|t| !t.is_finished()).count()
    }

    /// Abort a batch. `kill_on_drop` turns the abort into a killed subprocess.
    pub fn cancel(&self, batch_id: i64) {
        if let Some(task) = self.jobs().remove(&batch_id) {
            task.abort();
            log::info!("cancelled scrape batch {batch_id}");
        }
    }

    /// Queue a scrape and return its database id immediately.
    ///
    /// The batch runs as a detached task; progress arrives on `events`.
    pub async fn start(
        self: &Arc<Self>,
        url: String,
        config: GalleryConfig,
        proxies: Arc<ProxyManager>,
        events: mpsc::Sender<GalleryEvent>,
    ) -> Result<i64> {
        let url = url.trim().to_owned();
        validate_url(&url)?;

        let batch_id = self
            .db
            .create_batch(url.clone(), config.destination.clone())
            .await
            .context("could not open a scrape batch")?;

        let _ = events
            .send(GalleryEvent::Started {
                batch_id,
                url: url.clone(),
            })
            .await;

        let engine = Arc::clone(self);
        let task = tokio::spawn(async move {
            let outcome = engine
                .run(batch_id, &url, &config, proxies.as_ref(), &events)
                .await;

            let (state, error) = match outcome {
                Ok(()) => (JobState::Complete, None),
                Err(error) => {
                    log::warn!("scrape batch {batch_id} failed: {error:#}");
                    (JobState::Failed, Some(format!("{error:#}")))
                }
            };

            if let Err(error) = engine.db.finish_batch(batch_id, state, error.clone()).await {
                log::warn!("could not close scrape batch {batch_id}: {error:#}");
            }
            let _ = events
                .send(GalleryEvent::Finished {
                    batch_id,
                    state,
                    error,
                })
                .await;
            engine.jobs().remove(&batch_id);
        });

        self.jobs().insert(batch_id, task);
        Ok(batch_id)
    }

    async fn run(
        &self,
        batch_id: i64,
        url: &str,
        config: &GalleryConfig,
        proxies: &ProxyManager,
        events: &mpsc::Sender<GalleryEvent>,
    ) -> Result<()> {
        std::fs::create_dir_all(&config.destination)
            .with_context(|| format!("could not create {}", config.destination.display()))?;

        let binary = gallery_dl_binary();
        let mut command = Command::new(&binary);
        command
            .arg("--destination")
            .arg(&config.destination)
            // Colours would corrupt the path lines we parse. gallery-dl already
            // suppresses them for a pipe, but a user config can force them on.
            .arg("--no-colors");

        if config.write_info_json {
            command.arg("--write-info-json");
        }
        if config.overwrite {
            command.arg("--no-skip");
        }
        if let Some(range) = &config.range {
            command.arg("--range").arg(range);
        }

        // gallery-dl takes a single --proxy for every protocol.
        let task_key = format!("gallery:{batch_id}");
        if let Some(proxy) = proxies
            .resolve_for(&task_key, Engine::Subprocess)
            .context("the scrape cannot use the configured proxy")?
        {
            log::info!(
                "scrape batch {batch_id} routed through {}",
                proxy.redacted()
            );
            command.arg("--proxy").arg(proxy.url());
        }

        for extra in &config.extra_args {
            command.arg(extra);
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
                bail!(
                    "{binary} was not found in PATH. gallery-dl is not packaged by every \
                     distribution; install the standalone binary from \
                     https://codeberg.org/mikf/gallery-dl/releases"
                );
            }
            Err(error) => return Err(error).context("could not start gallery-dl"),
        };

        let stdout = child
            .stdout
            .take()
            .context("gallery-dl produced no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("gallery-dl produced no stderr")?;
        let mut stdout = BufReader::new(stdout).lines();
        let mut stderr = BufReader::new(stderr).lines();

        let mut total_seen = 0u64;
        let mut last_error: Option<String> = None;
        let (mut stdout_open, mut stderr_open) = (true, true);

        // Both streams must be drained concurrently: gallery-dl blocks once a
        // pipe buffer fills, so reading them in sequence would deadlock on any
        // batch bigger than a page of output.
        while stdout_open || stderr_open {
            tokio::select! {
                line = stdout.next_line(), if stdout_open => match line {
                    Ok(Some(line)) => {
                        self.handle_stdout(batch_id, &line, events).await;
                    }
                    Ok(None) => stdout_open = false,
                    Err(error) => {
                        log::warn!("scrape {batch_id}: could not read stdout: {error}");
                        stdout_open = false;
                    }
                },
                line = stderr.next_line(), if stderr_open => match line {
                    Ok(Some(line)) => {
                        self.handle_stderr(batch_id, &line, events, &mut total_seen, &mut last_error)
                            .await;
                    }
                    Ok(None) => stderr_open = false,
                    Err(error) => {
                        log::warn!("scrape {batch_id}: could not read stderr: {error}");
                        stderr_open = false;
                    }
                },
            }
        }

        let status = child
            .wait()
            .await
            .context("could not wait for gallery-dl to exit")?;

        if !status.success() {
            let code = status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "a signal".to_owned());
            match last_error {
                Some(message) => bail!("gallery-dl exited with {code}: {message}"),
                None => bail!("gallery-dl exited with {code}"),
            }
        }
        Ok(())
    }

    async fn handle_stdout(&self, batch_id: i64, line: &str, events: &mpsc::Sender<GalleryEvent>) {
        let (path, skipped) = match parse_stdout(line) {
            Line::Downloaded(path) => (path, false),
            Line::Skipped(path) => (path, true),
            _ => return,
        };

        if let Err(error) = self.db.record_file(batch_id, path.clone(), skipped).await {
            log::warn!("could not record {} : {error:#}", path.display());
        }
        let _ = events
            .send(GalleryEvent::File {
                batch_id,
                path,
                skipped,
            })
            .await;
    }

    async fn handle_stderr(
        &self,
        batch_id: i64,
        line: &str,
        events: &mpsc::Sender<GalleryEvent>,
        total_seen: &mut u64,
        last_error: &mut Option<String>,
    ) {
        match parse_stderr(line) {
            Line::Progress { total, .. } => {
                // Only write when the total actually changes: a 500-file batch
                // would otherwise issue 500 identical UPDATEs.
                if total != *total_seen {
                    *total_seen = total;
                    if let Err(error) = self.db.set_batch_total(batch_id, total).await {
                        log::warn!("could not store the batch total: {error:#}");
                    }
                    let _ = events.send(GalleryEvent::Total { batch_id, total }).await;
                }
            }
            Line::Failed(message) => {
                if let Err(error) = self.db.record_failure(batch_id).await {
                    log::warn!("could not record a scrape failure: {error:#}");
                }
                *last_error = Some(message.clone());
                let _ = events
                    .send(GalleryEvent::Warning { batch_id, message })
                    .await;
            }
            Line::Warning(message) => {
                let _ = events
                    .send(GalleryEvent::Warning { batch_id, message })
                    .await;
            }
            _ => {}
        }
    }
}

/// Only schemes gallery-dl can fetch, and never a local path.
fn validate_url(url: &str) -> Result<()> {
    if url.is_empty() {
        bail!("the gallery URL is empty");
    }
    if url.contains(['\r', '\n']) {
        bail!("the gallery URL contains a line break");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("the gallery URL has no scheme");
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        bail!("unsupported gallery URL scheme '{scheme}'");
    }
    if rest.is_empty() {
        bail!("the gallery URL has no host");
    }
    Ok(())
}

/// A destination for a batch: `<base>/<host>`, so two sites never interleave.
///
/// gallery-dl then builds its own `category/author/…` tree underneath.
pub fn destination_for(base: &Path, url: &str) -> PathBuf {
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_start_matches("www.");

    let safe: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if safe.is_empty() {
        base.to_path_buf()
    } else {
        base.join(safe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixtures below are literal lines captured from gallery-dl 1.32.9.

    #[test]
    fn reads_a_downloaded_path_from_stdout() {
        assert_eq!(
            parse_stdout("/tmp/gdl/out/directlink/127.0.0.1:8734__img1.png"),
            Line::Downloaded(PathBuf::from(
                "/tmp/gdl/out/directlink/127.0.0.1:8734__img1.png"
            ))
        );
    }

    #[test]
    fn reads_a_skipped_path_from_stdout() {
        assert_eq!(
            parse_stdout("# /tmp/gdl/out/directlink/127.0.0.1:8734__img2.png"),
            Line::Skipped(PathBuf::from(
                "/tmp/gdl/out/directlink/127.0.0.1:8734__img2.png"
            ))
        );
    }

    #[test]
    fn reads_the_batch_total_from_stderr() {
        assert_eq!(
            parse_stderr("[3/12] http://example.com/img3.png"),
            Line::Progress {
                index: 3,
                total: 12
            }
        );
        assert_eq!(
            parse_stderr("[1/3] http://127.0.0.1:8734/img1.png"),
            Line::Progress { index: 1, total: 3 }
        );
    }

    #[test]
    fn reads_failures_and_warnings_from_stderr() {
        assert_eq!(
            parse_stderr("[download][error] Failed to download 127.0.0.1:8734__missing.png"),
            Line::Failed("Failed to download 127.0.0.1:8734__missing.png".to_owned())
        );
        assert_eq!(
            parse_stderr(
                "[downloader.http][warning] '404 File not found' for 'http://x/missing.png'"
            ),
            Line::Warning("'404 File not found' for 'http://x/missing.png'".to_owned())
        );
    }

    #[test]
    fn a_log_line_on_stdout_is_not_mistaken_for_a_path() {
        // Defensive: a user config can route logging to stdout.
        assert_eq!(
            parse_stderr("[gallery-dl][debug] requests 2.34.2"),
            Line::Ignored
        );
        assert_eq!(
            parse_stdout("[download][error] Failed to download x"),
            Line::Ignored
        );
        assert_eq!(parse_stdout("   "), Line::Ignored);
        assert_eq!(parse_stdout("# "), Line::Ignored);
    }

    #[test]
    fn a_bracketed_non_counter_is_not_progress() {
        assert_eq!(
            parse_stderr("[extractor][info] something/other"),
            Line::Ignored
        );
        assert_eq!(parse_stderr("[0/0] http://x/"), Line::Ignored);
    }

    #[test]
    fn destinations_are_split_by_host_and_sanitised() {
        let base = Path::new("/home/u/Downloads");
        assert_eq!(
            destination_for(base, "https://www.example.com/gallery/1"),
            base.join("example.com")
        );
        assert_eq!(
            destination_for(base, "https://sub.site.org:8443/a"),
            base.join("sub.site.org_8443"),
            "a colon must not create a nested path component"
        );
    }

    #[test]
    fn rejects_urls_gallery_dl_cannot_fetch() {
        assert!(validate_url("https://example.com/a").is_ok());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("").is_err());
        assert!(validate_url("https://example.com/a\nX: y").is_err());
        assert!(validate_url("example.com").is_err());
    }
}
