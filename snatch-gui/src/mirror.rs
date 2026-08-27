//! Fetching a whole section of a site, not just one file from it.
//!
//! "Every PDF two levels deep from this documentation page" is a thing people
//! genuinely want and no single-file download manager can express. Wget2 is
//! already vendored as an alternative HTTP engine, and recursion is the one
//! thing it does that aria2 cannot do at all, so this drives it directly.
//!
//! Two runs, and the first is what makes this safe to offer:
//!
//! * **Preview.** `--spider` crawls without writing anything, printing an
//!   `Enqueue` line per discovered URL. Those are filtered here the same way
//!   the real run will filter them, so the list shown is what will actually be
//!   kept — not what will merely be visited.
//! * **Fetch.** The same crawl without `--spider`, printing `Saving '<path>'`
//!   once per file written. Counting those is the progress.
//!
//! Wget2's own filtering happens at save time, not at discovery: HTML pages
//! are always fetched, because they are how the crawler finds anything, and
//! then discarded if the filter excludes them. The preview says so rather than
//! pretending a filtered crawl touches fewer pages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::network::{Engine, ProxyManager};
use crate::settings::Settings;

/// Stop a preview crawl that will not end.
const PREVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Never enumerate more than this in a preview.
const PREVIEW_LIMIT: usize = 5_000;

/// What to fetch, and how far to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorConfig {
    pub url: String,
    /// Link depth. 0 means "no limit", which is what wget means by it too.
    pub depth: u32,
    /// Keep only these extensions. Empty means keep everything.
    pub accept: Vec<String>,
    /// Never keep these extensions.
    pub reject: Vec<String>,
    /// Stay on the host the crawl started from.
    pub same_host: bool,
    /// Never walk up above the starting directory.
    pub no_parent: bool,
    /// Also fetch the images and stylesheets a page needs to render.
    pub page_requisites: bool,
    /// Rewrite links so the copy browses offline.
    pub convert_links: bool,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            depth: 2,
            accept: Vec::new(),
            reject: Vec::new(),
            // Both on by default: a crawl that wanders onto other hosts or up
            // to the site root is how someone accidentally downloads the
            // internet.
            same_host: true,
            no_parent: true,
            page_requisites: false,
            convert_links: false,
        }
    }
}

impl MirrorConfig {
    /// Would the real run keep this URL?
    ///
    /// Mirrors wget2's own rule: the accept list, when given, is a whitelist;
    /// the reject list always wins. A URL with no extension is treated as a
    /// page, which is why a filtered crawl still keeps directory indexes.
    pub fn keeps(&self, url: &str) -> bool {
        let extension = extension_of(url);
        if let Some(extension) = &extension
            && self
                .reject
                .iter()
                .any(|reject| reject.eq_ignore_ascii_case(extension))
        {
            return false;
        }
        if self.accept.is_empty() {
            return true;
        }
        match extension {
            Some(extension) => self
                .accept
                .iter()
                .any(|accept| accept.eq_ignore_ascii_case(&extension)),
            None => false,
        }
    }

    /// The arguments both runs share.
    fn common_args(&self) -> Vec<String> {
        let mut args = vec![
            "--recursive".to_owned(),
            format!("--level={}", self.depth),
            "--progress=none".to_owned(),
            // Without this a crawl re-fetches what it already has every time.
            "--no-clobber".to_owned(),
        ];
        if self.no_parent {
            args.push("--no-parent".to_owned());
        }
        if self.same_host {
            // The default already refuses other hosts; saying so explicitly
            // means a future default change cannot widen a crawl silently.
            args.push("--span-hosts=off".to_owned());
        }
        if self.page_requisites {
            args.push("--page-requisites".to_owned());
        }
        if !self.accept.is_empty() {
            args.push(format!("--accept={}", self.accept.join(",")));
        }
        if !self.reject.is_empty() {
            args.push(format!("--reject={}", self.reject.join(",")));
        }
        args
    }

    pub fn validate(&self) -> Result<()> {
        let url = self.url.trim();
        if url.is_empty() {
            bail!("enter a page to start from");
        }
        let parsed = url::Url::parse(url).context("that is not a valid URL")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("only http and https pages can be crawled");
        }
        if self.depth > 20 {
            bail!("a depth above 20 will crawl far more than you mean it to");
        }
        Ok(())
    }

    /// The host a crawl starts from, used to name its folder.
    pub fn host(&self) -> Option<String> {
        url::Url::parse(self.url.trim())
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
    }
}

/// The lowercase extension of a URL's last path segment.
fn extension_of(url: &str) -> Option<String> {
    let path = url::Url::parse(url)
        .ok()
        .map(|parsed| parsed.path().to_owned())
        .unwrap_or_else(|| url.to_owned());
    let name = path.rsplit('/').next()?;
    let (_, extension) = name.rsplit_once('.')?;
    (!extension.is_empty() && extension.len() <= 10 && extension.chars().all(char::is_alphanumeric))
        .then(|| extension.to_ascii_lowercase())
}

/// What a crawl would fetch.
#[derive(Debug, Default, Clone)]
pub struct Preview {
    /// URLs the real run would keep.
    pub kept: Vec<String>,
    /// Pages that will be fetched to find links but discarded by the filter.
    pub traversed_only: usize,
    /// True when the crawl was cut short rather than finishing.
    pub truncated: bool,
}

/// Crawl without writing anything, to show what a real run would fetch.
pub async fn preview(
    config: &MirrorConfig,
    settings: &Settings,
    proxies: &ProxyManager,
) -> Result<Preview> {
    config.validate()?;

    let mut command = Command::new(crate::wget::wget_binary());
    command
        .arg("--spider")
        .args(config.common_args())
        .arg("--user-agent")
        .arg(&settings.download.user_agent);
    if !settings.download.check_certificate {
        command.arg("--no-check-certificate");
    }
    apply_proxy(&mut command, proxies, "mirror-preview")?;
    command
        .arg("--")
        .arg(config.url.trim())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = spawn(command)?;
    let stdout = child.stdout.take().context("wget produced no stdout")?;
    let stderr = child.stderr.take().context("wget produced no stderr")?;

    // Both streams, because the two wgets disagree about which one they
    // narrate on: Wget2 writes everything to stdout and leaves stderr empty,
    // classic wget does the opposite. One reader each, feeding one list.
    let collected = Arc::new(Mutex::new(Vec::<String>::new()));
    let readers = tokio::spawn({
        let out = Arc::clone(&collected);
        let err = Arc::clone(&collected);
        async move {
            tokio::join!(harvest_enqueued(stdout, out), harvest_enqueued(stderr, err));
        }
    });

    // A crawl of a large site can run for a very long time; a preview must
    // not. Whatever was discovered by the deadline is still a useful answer.
    let truncated = match tokio::time::timeout(PREVIEW_TIMEOUT, child.wait()).await {
        Ok(_) => false,
        Err(_) => {
            let _ = child.start_kill();
            true
        }
    };
    let _ = readers.await;

    let mut discovered = collected
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    // The starting page is never enqueued by itself.
    discovered.insert(0, config.url.trim().to_owned());
    discovered.sort();
    discovered.dedup();

    let truncated = truncated || discovered.len() >= PREVIEW_LIMIT;
    let total = discovered.len();
    let kept: Vec<String> = discovered
        .into_iter()
        .filter(|url| config.keeps(url))
        .collect();

    Ok(Preview {
        traversed_only: total.saturating_sub(kept.len()),
        kept,
        truncated,
    })
}

/// Collect the URLs a crawl announces, from one of its output streams.
async fn harvest_enqueued<R>(stream: R, collected: Arc<Mutex<Vec<String>>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = line.trim().strip_prefix("Enqueue ") {
            let mut held = collected
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if held.len() < PREVIEW_LIMIT {
                held.push(url.trim().to_owned());
            }
        }
    }
}

/// Everything a stream reader needs to report what it sees.
#[derive(Clone)]
struct CrawlProgress {
    job_id: i64,
    saved: Arc<std::sync::atomic::AtomicU64>,
    discovered: Arc<std::sync::atomic::AtomicU64>,
    diagnostics: Arc<Mutex<Vec<String>>>,
    events: mpsc::Sender<MirrorEvent>,
}

/// Turn one of wget's output streams into progress events.
async fn narrate<R>(stream: R, progress: CrawlProgress)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use std::sync::atomic::Ordering;

    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.starts_with("Enqueue ") {
            progress.discovered.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // `Saving 'path'` is printed once per file written, which is the only
        // unambiguous "one more done" in the output.
        if let Some(path) = line
            .strip_prefix("Saving '")
            .and_then(|rest| rest.strip_suffix('\''))
        {
            let done = progress.saved.fetch_add(1, Ordering::Relaxed) + 1;
            let current = Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_owned());
            let _ = progress
                .events
                .send(MirrorEvent::Progress {
                    job_id: progress.job_id,
                    saved: done,
                    discovered: progress.discovered.load(Ordering::Relaxed),
                    current,
                })
                .await;
            continue;
        }
        if line.contains("ERROR") || line.starts_with("Failed") {
            log::debug!(target: "mirror", "{line}");
            let mut held = progress
                .diagnostics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if held.len() < 20 {
                held.push(line.to_owned());
            }
        }
    }
}

/// Progress of a running crawl.
#[derive(Debug)]
pub enum MirrorEvent {
    Started {
        job_id: i64,
        host: String,
    },
    Progress {
        job_id: i64,
        saved: u64,
        discovered: u64,
        /// The file most recently written.
        current: String,
    },
    Finished {
        job_id: i64,
        destination: PathBuf,
        saved: u64,
    },
    Failed {
        job_id: i64,
        error: String,
    },
}

/// Runs recursive crawls, one task each.
pub struct MirrorEngine {
    root: PathBuf,
    jobs: Mutex<HashMap<i64, JoinHandle<()>>>,
    next_id: std::sync::atomic::AtomicI64,
}

impl MirrorEngine {
    /// Crawls are filed under `Sites` so they never mix with plain downloads.
    pub fn new(download_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root: download_dir.join("Sites"),
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
        self.jobs().len()
    }

    pub fn cancel(&self, job_id: i64) -> bool {
        match self.jobs().remove(&job_id) {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }

    pub fn start(
        self: &Arc<Self>,
        config: MirrorConfig,
        settings: Settings,
        proxies: Arc<ProxyManager>,
        events: mpsc::Sender<MirrorEvent>,
    ) -> Result<i64> {
        config.validate()?;
        let job_id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let engine = Arc::clone(self);
        let task = tokio::spawn(async move {
            match engine
                .run(job_id, &config, &settings, &proxies, &events)
                .await
            {
                Ok((destination, saved)) => {
                    let _ = events
                        .send(MirrorEvent::Finished {
                            job_id,
                            destination,
                            saved,
                        })
                        .await;
                }
                Err(error) => {
                    log::warn!("crawl {job_id} failed: {error:#}");
                    let _ = events
                        .send(MirrorEvent::Failed {
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
        config: &MirrorConfig,
        settings: &Settings,
        proxies: &ProxyManager,
        events: &mpsc::Sender<MirrorEvent>,
    ) -> Result<(PathBuf, u64)> {
        let host = config.host().unwrap_or_else(|| "site".to_owned());
        // wget writes `<prefix>/<host>/...` itself, so the prefix stops one
        // level short of where the files land.
        let destination = self.root.join(&host);
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("could not create {}", self.root.display()))?;

        let _ = events.send(MirrorEvent::Started { job_id, host }).await;

        let mut command = Command::new(crate::wget::wget_binary());
        command
            .args(config.common_args())
            .arg("--directory-prefix")
            .arg(&self.root)
            .arg("--user-agent")
            .arg(&settings.download.user_agent)
            .arg("--tries")
            .arg(settings.download.retries.max(1).to_string());
        if !settings.download.check_certificate {
            command.arg("--no-check-certificate");
        }
        if settings.download.max_per_download_kib > 0 {
            command
                .arg("--limit-rate")
                .arg(format!("{}k", settings.download.max_per_download_kib));
        }
        if config.convert_links {
            command.arg("--convert-links");
        }
        apply_proxy(&mut command, proxies, &format!("mirror:{job_id}"))?;
        command
            .arg("--")
            .arg(config.url.trim())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = spawn(command)?;
        let stdout = child.stdout.take().context("wget produced no stdout")?;
        let stderr = child.stderr.take().context("wget produced no stderr")?;

        let saved = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let diagnostics = Arc::new(Mutex::new(Vec::<String>::new()));
        // Wget2 narrates on stdout and leaves stderr empty; classic wget does
        // the opposite. Reading only one of them is how a crawl reports
        // nothing at all, so both get their own reader over the same handler.
        // Selecting between them instead would end the loop the moment the
        // empty one hit EOF, which is immediately.
        let discovered = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reader = tokio::spawn({
            let progress = CrawlProgress {
                job_id,
                saved: Arc::clone(&saved),
                discovered: Arc::clone(&discovered),
                diagnostics: Arc::clone(&diagnostics),
                events: events.clone(),
            };
            let other = progress.clone();
            async move {
                tokio::join!(narrate(stdout, progress), narrate(stderr, other));
            }
        });

        let status = child.wait().await.context("waiting for wget failed")?;
        let _ = reader.await;
        let saved = saved.load(std::sync::atomic::Ordering::Relaxed);

        // wget exits non-zero when *any* URL failed, which on a crawl of a
        // real site is almost guaranteed — a single 404 must not throw away a
        // thousand good files.
        if !status.success() && saved == 0 {
            let said = diagnostics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .join("; ");
            let detail = if said.is_empty() {
                format!("wget exited with {status}")
            } else {
                said
            };
            bail!("nothing could be fetched: {detail}");
        }

        Ok((destination, saved))
    }
}

fn spawn(mut command: Command) -> Result<tokio::process::Child> {
    match command.spawn() {
        Ok(child) => Ok(child),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("wget was not found in PATH; install the 'wget2' package")
        }
        Err(error) => Err(error).context("could not start wget"),
    }
}

/// wget reads proxies from the environment, so this is how one is applied.
fn apply_proxy(command: &mut Command, proxies: &ProxyManager, key: &str) -> Result<()> {
    let proxy = proxies
        .resolve_for(key, Engine::Http)
        .context("the crawl cannot use the configured proxy")?;
    if let Some(proxy) = proxy {
        command.env("http_proxy", proxy.url());
        command.env("https_proxy", proxy.url());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(accept: &[&str], reject: &[&str]) -> MirrorConfig {
        MirrorConfig {
            url: "https://example.com/docs/".to_owned(),
            accept: accept.iter().map(|value| (*value).to_owned()).collect(),
            reject: reject.iter().map(|value| (*value).to_owned()).collect(),
            ..MirrorConfig::default()
        }
    }

    #[test]
    fn an_extension_is_read_from_the_path_not_the_query() {
        assert_eq!(
            extension_of("https://x.test/a/b.pdf").as_deref(),
            Some("pdf")
        );
        assert_eq!(
            extension_of("https://x.test/a/b.PDF?v=2").as_deref(),
            Some("pdf")
        );
        // A directory index has no extension, and neither does a bare path.
        assert_eq!(extension_of("https://x.test/docs/"), None);
        assert_eq!(extension_of("https://x.test/docs"), None);
        // A dotted hostname must not become an extension.
        assert_eq!(extension_of("https://x.test/"), None);
    }

    #[test]
    fn with_no_filter_everything_is_kept() {
        let config = config(&[], &[]);
        assert!(config.keeps("https://x.test/a.pdf"));
        assert!(config.keeps("https://x.test/a.html"));
        assert!(config.keeps("https://x.test/docs/"));
    }

    #[test]
    fn an_accept_list_is_a_whitelist() {
        let config = config(&["pdf", "epub"], &[]);
        assert!(config.keeps("https://x.test/a.pdf"));
        assert!(config.keeps("https://x.test/a.EPUB"));
        assert!(!config.keeps("https://x.test/a.html"));
        // A page still has to be crawled to find the PDFs, but it is not
        // something the user asked to keep.
        assert!(!config.keeps("https://x.test/docs/"));
    }

    #[test]
    fn a_reject_list_wins_over_accept() {
        let config = config(&["pdf", "zip"], &["zip"]);
        assert!(config.keeps("https://x.test/a.pdf"));
        assert!(!config.keeps("https://x.test/a.zip"));
    }

    #[test]
    fn the_arguments_match_what_was_asked_for() {
        let mut config = config(&["pdf"], &["exe"]);
        config.depth = 3;
        let args = config.common_args().join(" ");
        assert!(args.contains("--recursive"), "{args}");
        assert!(args.contains("--level=3"), "{args}");
        assert!(args.contains("--accept=pdf"), "{args}");
        assert!(args.contains("--reject=exe"), "{args}");
        // Both guards are on by default and must appear.
        assert!(args.contains("--no-parent"), "{args}");
        assert!(args.contains("--span-hosts=off"), "{args}");

        config.no_parent = false;
        config.same_host = false;
        let args = config.common_args().join(" ");
        assert!(!args.contains("--no-parent"), "{args}");
        assert!(!args.contains("--span-hosts"), "{args}");
    }

    #[test]
    fn a_crawl_needs_a_real_web_url() {
        let mut config = MirrorConfig::default();
        assert!(config.validate().is_err(), "an empty URL must be refused");

        config.url = "ftp://example.com/pub/".to_owned();
        assert!(config.validate().is_err(), "ftp cannot be crawled");

        config.url = "not a url".to_owned();
        assert!(config.validate().is_err());

        config.url = "https://example.com/docs/".to_owned();
        assert!(config.validate().is_ok());

        // A depth nobody means to ask for.
        config.depth = 50;
        assert!(config.validate().is_err());
    }

    /// A tiny linked site, so the crawl has something real to walk.
    async fn serve_site() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const ROUTES: [(&str, &str, &str); 5] = [
            (
                "/",
                "text/html",
                r#"<html><body><a href="docs/a.html">a</a> <a href="notes.txt">n</a> <a href="big.pdf">p</a></body></html>"#,
            ),
            (
                "/docs/a.html",
                "text/html",
                r#"<html><body><a href="deep.pdf">deep</a></body></html>"#,
            ),
            ("/notes.txt", "text/plain", "notes\n"),
            ("/big.pdf", "application/pdf", "%PDF-1.4 fake\n"),
            ("/docs/deep.pdf", "application/pdf", "%PDF-1.4 deeper\n"),
        ];

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 2048];
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/");
                    let response = match ROUTES.iter().find(|(route, _, _)| *route == path) {
                        Some((_, mime, body)) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        None => {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_owned()
                        }
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        (base, handle)
    }

    fn test_settings() -> Settings {
        Settings::default()
    }

    fn test_proxies() -> Arc<ProxyManager> {
        let path = std::env::temp_dir().join("snatch-mirror-proxies.json");
        let _ = std::fs::remove_file(&path);
        Arc::new(ProxyManager::load(path))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_preview_lists_what_the_filter_would_keep() {
        if crate::wget::detect().await.is_none() {
            eprintln!("no wget installed; skipping");
            return;
        }
        let (base, server) = serve_site().await;

        // Unfiltered: everything discovered is kept.
        let all = MirrorConfig {
            url: format!("{base}/"),
            depth: 3,
            ..MirrorConfig::default()
        };
        let unfiltered = preview(&all, &test_settings(), &test_proxies())
            .await
            .expect("preview runs");
        assert!(
            unfiltered.kept.iter().any(|url| url.ends_with("/big.pdf")),
            "{:?}",
            unfiltered.kept
        );
        assert!(
            unfiltered
                .kept
                .iter()
                .any(|url| url.ends_with("/notes.txt"))
        );

        // Filtered to PDFs: the text file and the pages drop out of the list,
        // but the pages still have to be crawled to find the PDFs.
        let pdfs = MirrorConfig {
            accept: vec!["pdf".to_owned()],
            ..all.clone()
        };
        let only_pdfs = preview(&pdfs, &test_settings(), &test_proxies())
            .await
            .expect("preview runs");
        assert!(
            only_pdfs.kept.iter().all(|url| url.ends_with(".pdf")),
            "{:?}",
            only_pdfs.kept
        );
        assert!(
            only_pdfs
                .kept
                .iter()
                .any(|url| url.ends_with("/docs/deep.pdf")),
            "a PDF two levels down must be found: {:?}",
            only_pdfs.kept
        );
        assert!(
            only_pdfs.traversed_only > 0,
            "the pages walked to find them should be reported"
        );

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_filtered_crawl_writes_only_what_was_asked_for() {
        if crate::wget::detect().await.is_none() {
            eprintln!("no wget installed; skipping");
            return;
        }
        let (base, server) = serve_site().await;
        let root = std::env::temp_dir().join("snatch-mirror-crawl");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");

        let config = MirrorConfig {
            url: format!("{base}/"),
            depth: 3,
            accept: vec!["pdf".to_owned()],
            ..MirrorConfig::default()
        };

        let (tx, mut rx) = mpsc::channel(256);
        let engine = MirrorEngine::new(root.clone());
        engine
            .start(config, test_settings(), test_proxies(), tx)
            .expect("the crawl starts");

        let mut saved = 0;
        let mut finished = None;
        while let Some(event) = rx.recv().await {
            match event {
                MirrorEvent::Progress { saved: done, .. } => saved = done,
                MirrorEvent::Finished { destination, .. } => {
                    finished = Some(destination);
                    break;
                }
                MirrorEvent::Failed { error, .. } => panic!("crawl failed: {error}"),
                MirrorEvent::Started { .. } => {}
            }
        }
        let destination = finished.expect("the crawl reported Finished");

        let written: Vec<String> = walk(&destination)
            .into_iter()
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect();
        assert!(written.contains(&"big.pdf".to_owned()), "{written:?}");
        assert!(written.contains(&"deep.pdf".to_owned()), "{written:?}");
        // The filter is what stops the crawl keeping the pages it walked.
        assert!(!written.contains(&"notes.txt".to_owned()), "{written:?}");
        assert!(saved >= 2, "progress should have counted the files");

        let _ = std::fs::remove_dir_all(&root);
        server.abort();
    }

    fn walk(directory: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(directory) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    #[test]
    fn the_folder_is_named_after_the_host() {
        let config = MirrorConfig {
            url: "https://Docs.Example.com/a/b.html".to_owned(),
            ..MirrorConfig::default()
        };
        assert_eq!(config.host().as_deref(), Some("docs.example.com"));
    }
}
