//! An alternative HTTP engine built on Wget2.
//!
//! This is not a fallback for a missing aria2 so much as a second opinion.
//! Wget2 is genuinely multithreaded — `--max-threads` with `--chunk-size`
//! fetches a file in parallel ranges the way aria2 does — and some servers
//! behave better with its request pattern than with aria2's.
//!
//! **Progress is measured from the file on disk, not from wget's output.**
//! Wget2 draws an ANSI progress bar with cursor-save, cursor-up and
//! erase-to-end sequences interleaved mid-line; parsing it is fragile and
//! breaks whenever the bar layout changes. The size of the output file is
//! unambiguous, costs one `stat` per tick, and is correct even when wget is
//! writing several chunks at once. The total comes from a `HEAD` beforehand.
//!
//! Classic wget 1.x has none of the threading flags, so the engine detects
//! which is installed and degrades to a single stream rather than passing
//! arguments that would abort the download.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::network::{Engine, ProxyManager};
use crate::settings::Settings;
use crate::types::DownloadRequest;

/// How often the on-disk size is sampled.
const POLL: Duration = Duration::from_millis(500);

/// The host a `.netrc` entry keys on: no userinfo, no port.
fn host_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    parsed.host_str().map(str::to_ascii_lowercase)
}

/// A short-lived `.netrc` holding one machine's credentials.
///
/// wget takes `--user` and `--password` on the command line, where any user on
/// the machine can read them out of `ps`. A netrc file readable only by its
/// owner does not have that problem, so the password goes there instead and
/// the file is removed as soon as the download ends.
/// Check the finished file against the digest the request carried, if any.
///
/// An unreadable digest is reported rather than ignored. Unlike the aria2
/// path — where a bad option would make aria2 reject the add and lose the
/// download outright — nothing is at stake here but the message, and silently
/// skipping a check the user asked for would be worse than saying so.
async fn verify_if_requested(request: &DownloadRequest, path: &Path) -> Result<()> {
    let Some(text) = request.checksum.as_deref().map(str::trim) else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }
    let Some(expected) = crate::checksum::parse(text) else {
        bail!("'{text}' is not a checksum in any recognised form");
    };
    crate::checksum::verify_file(path, &expected).await
}

#[derive(Debug)]
struct NetrcFile(PathBuf);

/// One netrc token, always quoted.
///
/// Wget2's parser takes a double-quoted value with `\` and `"` backslash
/// escaped — verified against wget2 2.2.1 rather than assumed. Quoting
/// unconditionally is what makes a password containing a space work at all:
/// unquoted, the parser stops at the space and the rest becomes stray tokens
/// that a crafted value could turn into a second `machine` entry.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        if character == '\\' || character == '"' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

impl NetrcFile {
    fn create(job_id: i64, host: &str, user: &str, password: &str) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let path = crate::paths::data_dir()?.join(format!("netrc-{job_id}"));
        // Stale file from a previous run that died before its cleanup.
        let _ = std::fs::remove_file(&path);
        // A line break would end the entry and let the rest of the value be
        // read as a second `machine` block for a host we never intended.
        // Nothing legitimate needs one, so refuse rather than mangle.
        if [host, user, password]
            .iter()
            .any(|value| value.contains(['\r', '\n']))
        {
            bail!("credentials cannot contain a line break");
        }

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        writeln!(
            file,
            "machine {} login {} password {}",
            quote(host),
            quote(user),
            quote(password)
        )
        .context("could not write the credentials file")?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for NetrcFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn wget_binary() -> String {
    std::env::var("SNATCH_WGET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "wget".to_owned())
}

/// Which wget is installed. They take different flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    /// Wget2: supports `--max-threads` and `--chunk-size`.
    Wget2,
    /// Classic wget 1.x: single stream only.
    Legacy,
}

/// Ask wget which it is. `None` when it is not installed.
pub async fn detect() -> Option<Flavour> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(wget_binary())
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    let banner = String::from_utf8_lossy(&output.stdout);
    let first = banner.lines().next()?;
    // "GNU Wget2 2.2.1 - multithreaded metalink/file/website downloader"
    Some(if first.contains("Wget2") {
        Flavour::Wget2
    } else {
        Flavour::Legacy
    })
}

#[derive(Debug, Clone)]
pub enum WgetEvent {
    Started {
        job_id: i64,
        name: String,
        total: Option<u64>,
    },
    Progress {
        job_id: i64,
        downloaded: u64,
        total: Option<u64>,
        bytes_per_second: u64,
    },
    Finished {
        job_id: i64,
        path: PathBuf,
    },
    Failed {
        job_id: i64,
        error: String,
    },
}

/// Runs and tracks wget downloads.
pub struct WgetEngine {
    download_dir: PathBuf,
    jobs: Mutex<HashMap<i64, JoinHandle<()>>>,
    next_id: std::sync::atomic::AtomicI64,
}

impl WgetEngine {
    pub fn new(download_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            download_dir,
            jobs: Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicI64::new(1),
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
            log::info!("cancelled wget job {job_id}");
        }
    }

    /// Queue a download and return its id immediately.
    pub fn start(
        self: &Arc<Self>,
        request: DownloadRequest,
        settings: Settings,
        proxies: Arc<ProxyManager>,
        events: mpsc::Sender<WgetEvent>,
    ) -> Result<i64> {
        request.validate()?;
        let job_id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let engine = Arc::clone(self);
        let task = tokio::spawn(async move {
            match engine
                .run(job_id, &request, &settings, proxies.as_ref(), &events)
                .await
            {
                Ok(path) => {
                    // wget has no equivalent of aria2's --checksum, so the
                    // file is hashed here instead. A mismatch is a failure,
                    // not a warning: the point of asking for a digest is that
                    // the wrong bytes are worse than no bytes.
                    match verify_if_requested(&request, &path).await {
                        Ok(()) => {
                            let _ = events.send(WgetEvent::Finished { job_id, path }).await;
                        }
                        Err(error) => {
                            log::warn!("wget job {job_id} failed verification: {error:#}");
                            let _ = std::fs::remove_file(&path);
                            let _ = events
                                .send(WgetEvent::Failed {
                                    job_id,
                                    error: format!("{error:#}"),
                                })
                                .await;
                        }
                    }
                }
                Err(error) => {
                    log::warn!("wget job {job_id} failed: {error:#}");
                    let _ = events
                        .send(WgetEvent::Failed {
                            job_id,
                            error: format!("{error:#}"),
                        })
                        .await;
                }
            }
            engine.jobs().remove(&job_id);
        });

        self.jobs().insert(job_id, task);
        Ok(job_id)
    }

    async fn run(
        &self,
        job_id: i64,
        request: &DownloadRequest,
        settings: &Settings,
        proxies: &ProxyManager,
        events: &mpsc::Sender<WgetEvent>,
    ) -> Result<PathBuf> {
        let flavour = detect()
            .await
            .context("wget was not found in PATH; install the 'wget' package")?;

        std::fs::create_dir_all(&self.download_dir)
            .with_context(|| format!("could not create {}", self.download_dir.display()))?;

        let name = request
            .sanitized_filename()
            .or_else(|| crate::types::name_from_url(&request.url))
            .unwrap_or_else(|| "download".to_owned());
        let target = unique_path(&self.download_dir, &name);

        // wget cannot report a total for a file it has not started, and the
        // on-disk size means nothing without one, so ask the server first.
        let task_key = format!("wget:{job_id}");
        let proxy = proxies
            .resolve_for(&task_key, Engine::Http)
            .context("the download cannot use the configured proxy")?;
        let total = head_length(&request.url, proxies, proxy.as_ref()).await;

        let _ = events
            .send(WgetEvent::Started {
                job_id,
                name: name.clone(),
                total,
            })
            .await;

        let mut command = Command::new(wget_binary());
        command
            .arg("--continue")
            // The bar is unparseable, and we do not parse it.
            .arg("--progress=none")
            .arg("--tries")
            .arg(settings.download.retries.max(1).to_string())
            .arg("--user-agent")
            .arg(&settings.download.user_agent)
            .arg("-O")
            .arg(&target);

        if flavour == Flavour::Wget2 {
            // The whole reason to offer wget as an engine.
            command
                .arg("--max-threads")
                .arg(settings.download.split.clamp(1, 64).to_string())
                .arg(format!(
                    "--chunk-size={}M",
                    settings.download.min_split_mib.max(1)
                ));
        } else {
            log::info!("classic wget detected; downloading in a single stream");
        }

        if !settings.download.check_certificate {
            command.arg("--no-check-certificate");
        }
        if settings.download.max_per_download_kib > 0 {
            command
                .arg("--limit-rate")
                .arg(format!("{}k", settings.download.max_per_download_kib));
        }
        if let Some(referer) = request
            .referer
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            command.arg("--referer").arg(sanitise_header(referer));
        }
        if let Some(cookies) = request
            .cookies
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            command
                .arg("--header")
                .arg(format!("Cookie: {}", sanitise_header(cookies)));
        }
        if let Some(proxy) = &proxy {
            // wget reads proxies from the environment.
            command.env("http_proxy", proxy.url());
            command.env("https_proxy", proxy.url());
        }

        // Bound, not discarded: dropping it deletes the file, so it has to
        // outlive the child process.
        let _netrc = match request.credentials() {
            Some((user, password)) => match host_from_url(&request.url) {
                Some(host) if flavour == Flavour::Wget2 => {
                    let file = NetrcFile::create(job_id, &host, &user, &password)?;
                    command.arg("--netrc").arg("--netrc-file").arg(file.path());
                    Some(file)
                }
                Some(_) => {
                    // Classic wget has no --netrc-file, so there is nowhere to
                    // put the password except argv. Say so rather than doing
                    // it silently.
                    log::warn!(
                        "classic wget cannot read a private credentials file; \
                         the password will be visible in the process list"
                    );
                    command
                        .arg("--user")
                        .arg(&user)
                        .arg("--password")
                        .arg(&password);
                    None
                }
                None => bail!("the URL has no host to authenticate against"),
            },
            None => None,
        };

        command
            .arg("--")
            .arg(request.url.trim())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!("wget was not found in PATH; install the 'wget' package");
            }
            Err(error) => return Err(error).context("could not start wget"),
        };

        // Collect stderr so a failure can say why, without parsing it for progress.
        let stderr = child.stderr.take().context("wget produced no stderr")?;
        let diagnostics = Arc::new(Mutex::new(Vec::<String>::new()));
        tokio::spawn({
            let diagnostics = Arc::clone(&diagnostics);
            async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    log::debug!(target: "wget", "{line}");
                    let mut held = diagnostics
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    if held.len() < 20 {
                        held.push(line);
                    }
                }
            }
        });

        // Watch the file grow rather than wget's terminal output.
        let mut ticker = tokio::time::interval(POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = (Instant::now(), 0u64);

        let status = loop {
            tokio::select! {
                finished = child.wait() => {
                    break finished.context("could not wait for wget to exit")?;
                }
                _ = ticker.tick() => {
                    let downloaded = tokio::fs::metadata(&target)
                        .await
                        .map(|meta| meta.len())
                        .unwrap_or(0);
                    let now = Instant::now();
                    let elapsed = now.duration_since(last.0).as_secs_f64();
                    let speed = if elapsed > 0.0 {
                        ((downloaded.saturating_sub(last.1)) as f64 / elapsed) as u64
                    } else {
                        0
                    };
                    last = (now, downloaded);
                    let _ = events
                        .send(WgetEvent::Progress { job_id, downloaded, total, bytes_per_second: speed })
                        .await;
                }
            }
        };

        if !status.success() {
            let code = status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "a signal".to_owned());
            let detail = {
                let held = diagnostics
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                held.join("; ")
            };
            // A partial file left behind looks like a finished download.
            if let Err(error) = tokio::fs::remove_file(&target).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("could not clean up {}: {error}", target.display());
            }
            if detail.is_empty() {
                bail!("wget exited with {code}");
            }
            bail!("wget exited with {code}: {detail}");
        }

        Ok(target)
    }
}

/// Ask the server how big the file is, so progress has a denominator.
async fn head_length(
    url: &str,
    proxies: &ProxyManager,
    proxy: Option<&crate::network::ProxyEndpoint>,
) -> Option<u64> {
    let client = proxies.client(proxy).ok()?;
    let response = tokio::time::timeout(Duration::from_secs(15), client.head(url).send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    // A HEAD reply has no body, so the header is the only source of truth.
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|length| *length > 0)
}

/// Strip control characters so a hostile page cannot inject a header.
fn sanitise_header(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// Pick a name that does not overwrite an existing file.
///
/// aria2 does this itself with `--auto-file-renaming`; wget would happily
/// clobber, so Snatch has to.
fn unique_path(directory: &Path, name: &str) -> PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let (stem, extension) = match name.rsplit_once('.') {
        // A leading dot is part of the name, not an extension separator.
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };

    for index in 1..10_000 {
        let attempt = match extension {
            Some(extension) => format!("{stem}.{index}.{extension}"),
            None => format!("{stem}.{index}"),
        };
        let candidate = directory.join(attempt);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Astronomically unlikely; overwriting is still better than failing.
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_path_leaves_a_free_name_alone() {
        let directory = std::env::temp_dir().join("snatch-wget-unique-free");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        assert_eq!(
            unique_path(&directory, "video.mkv"),
            directory.join("video.mkv")
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn unique_path_never_clobbers() {
        let directory = std::env::temp_dir().join("snatch-wget-unique-taken");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        std::fs::write(directory.join("video.mkv"), b"a").expect("write");
        std::fs::write(directory.join("video.1.mkv"), b"a").expect("write");

        // wget has no --auto-file-renaming, so this is what stops it
        // overwriting a file the user already has.
        assert_eq!(
            unique_path(&directory, "video.mkv"),
            directory.join("video.2.mkv")
        );

        // A dotfile has no extension to split on.
        std::fs::write(directory.join(".env"), b"a").expect("write");
        assert_eq!(unique_path(&directory, ".env"), directory.join(".env.1"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn header_values_cannot_carry_a_line_break() {
        assert_eq!(sanitise_header("a=b\r\nX-Evil: 1"), "a=bX-Evil: 1");
        assert_eq!(sanitise_header("plain"), "plain");
    }

    #[test]
    fn netrc_keys_on_the_bare_host() {
        // Port and userinfo must not reach the netrc entry: it matches on
        // neither, so leaving them in would silently stop it applying.
        assert_eq!(
            host_from_url("https://files.example.com:8443/a/b.iso").as_deref(),
            Some("files.example.com")
        );
        assert_eq!(
            host_from_url("ftp://someone@ftp.example.com/pub/f").as_deref(),
            Some("ftp.example.com")
        );
        assert_eq!(
            host_from_url("https://EXAMPLE.com/f").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_from_url("not a url").as_deref(), None);
    }

    #[test]
    fn the_credentials_file_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;

        let path = {
            let netrc = NetrcFile::create(987_654, "example.com", "alice", "s3cret")
                .expect("the credentials file is created");
            let path = netrc.path().to_path_buf();

            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            // Nobody but the owner: this is the whole point of the file.
            assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);

            let body = std::fs::read_to_string(&path).expect("read");
            assert_eq!(
                body.trim(),
                r#"machine "example.com" login "alice" password "s3cret""#
            );
            path
        };

        // Dropping it takes the password off disk.
        assert!(!path.exists(), "the credentials file outlived its guard");
    }

    #[test]
    fn a_password_may_contain_spaces_quotes_and_backslashes() {
        // Quoting is what makes these work; the escapes match what wget2
        // 2.2.1 actually accepts.
        assert_eq!(quote("pa ss word"), r#""pa ss word""#);
        assert_eq!(quote(r#"pa"ss"#), r#""pa\"ss""#);
        assert_eq!(quote(r"pa\ss"), r#""pa\\ss""#);
        assert_eq!(quote("simple"), r#""simple""#);
    }

    #[test]
    fn a_crafted_password_cannot_forge_a_second_entry() {
        // Quoted and escaped, the whole value stays one token, so the
        // injected keywords are read as part of the password.
        let netrc = NetrcFile::create(
            987_655,
            "example.com",
            "alice",
            r#"x" machine evil.test login root password "hunter2"#,
        )
        .expect("created");
        let body = std::fs::read_to_string(netrc.path()).expect("read");
        // The quote that would have closed the value is escaped, so the
        // injected keywords stay inside the password and no second `machine`
        // entry exists.
        assert_eq!(
            body.trim(),
            r#"machine "example.com" login "alice" password "x\" machine evil.test login root password \"hunter2""#
        );
        assert_eq!(body.lines().count(), 1, "got: {body:?}");
    }

    #[test]
    fn a_line_break_in_a_credential_is_refused() {
        // The one thing quoting cannot contain.
        let error = NetrcFile::create(987_656, "example.com", "alice", "a\nmachine evil.test")
            .expect_err("a line break must be refused");
        assert!(error.to_string().contains("line break"), "got: {error}");
    }
}
