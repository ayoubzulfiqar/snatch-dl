//! aria2 orchestration: child-process supervision plus a JSON-RPC client.
//!
//! Snatch never implements HTTP transfers itself. `aria2c` is spawned as a
//! private child bound to loopback, and every operation goes through its
//! JSON-RPC endpoint over `wreq`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_channel::Sender;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::time::sleep;

use crate::network::{Engine, ProxyEndpoint, ProxyManager};
use crate::types::{DownloadRequest, UiEvent, name_from_url};

/// The assignment slot the proxy table is consulted with for a new download.
///
/// A download has no GID until aria2 has accepted it, so the only thing that
/// can decide its route at add time is this key, or the default proxy that
/// `resolve` falls back to. Per-download routing afterwards goes through
/// [`Aria2Client::route`], keyed by GID.
pub const PROXY_KEY: &str = "download";

/// How long to wait for aria2 to report a download paused before changing an
/// option on it. `pause` returns before the state machine has moved.
const PAUSE_SETTLE: Duration = Duration::from_secs(3);

/// Only ask aria2 for the fields the UI actually renders; `tellStopped` on a
/// long history is otherwise needlessly chatty.
const STATUS_KEYS: [&str; 10] = [
    "gid",
    "status",
    "totalLength",
    "completedLength",
    "downloadSpeed",
    "connections",
    "errorCode",
    "errorMessage",
    "dir",
    "files",
];

/// A browser-ish default: plenty of servers reject aria2's own user agent.
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

const READY_ATTEMPTS: u32 = 80;
const READY_INTERVAL: Duration = Duration::from_millis(250);

/// A step in the waiting queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMove {
    Up,
    Down,
    Top,
    Bottom,
}

/// Where aria2 writes files and keeps its resumable session.
#[derive(Debug, Clone)]
pub struct Aria2Config {
    pub download_dir: PathBuf,
    pub session_file: PathBuf,
    /// Tuning that can only be applied when aria2 starts.
    pub spawn_args: Vec<String>,
}

/// A cloneable handle to the aria2 RPC endpoint.
///
/// GTK callbacks reach it through [`crate::backend::Backend::offload`], which
/// is what moves the call onto the tokio runtime.
#[derive(Clone)]
pub struct Aria2Client {
    http: wreq::Client,
    endpoint: String,
    token: String,
    /// The loopback port the daemon is told to listen on.
    ///
    /// Picked at startup rather than fixed at 6800. A fixed port with a
    /// fixed secret meant any local process could drive the daemon -- and,
    /// through the per-download `dir` option, write anywhere this user can
    /// -- and two users on one machine silently shared one daemon.
    port: u16,
    /// The RPC secret, fresh for every launch, for the same reason.
    secret: String,
    download_dir: PathBuf,
    /// Where a new download's route is looked up.
    proxies: Arc<ProxyManager>,
    /// Per-download options, re-read on every add so a settings change
    /// affects the next download without a restart.
    options: Arc<RwLock<Vec<(String, String)>>>,
    /// Whether to file downloads into per-type subfolders.
    categorise: Arc<std::sync::atomic::AtomicBool>,
}

impl Aria2Client {
    pub fn new(
        download_dir: PathBuf,
        options: Arc<RwLock<Vec<(String, String)>>>,
        proxies: Arc<ProxyManager>,
    ) -> Result<Self> {
        let http = wreq::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(3))
            .pool_idle_timeout(Duration::from_secs(90))
            // The endpoint is loopback; a system-wide proxy must never apply.
            .no_proxy()
            .build()
            .context("could not build the HTTP client used for aria2 RPC")?;

        let port = free_loopback_port().context("could not find a port for aria2's RPC")?;
        let secret = random_secret();

        Ok(Self {
            http,
            endpoint: format!("http://127.0.0.1:{port}/jsonrpc"),
            token: format!("token:{secret}"),
            port,
            secret,
            download_dir,
            proxies,
            options,
            categorise: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    /// Turn per-type subfolders on or off for later downloads.
    pub fn set_categorise(&self, on: bool) {
        self.categorise
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Replace the per-download options used by later `addUri` calls.
    pub fn set_download_options(&self, options: Vec<(String, String)>) {
        *self
            .options
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = options;
    }

    /// Apply the handful of options aria2 accepts while running.
    pub async fn change_global_options(&self, options: Vec<(String, String)>) -> Result<()> {
        let map: Map<String, Value> = options
            .into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect();
        self.call::<Value>("aria2.changeGlobalOption", vec![Value::Object(map)])
            .await
            .map(drop)
    }

    /// Issue one JSON-RPC call, injecting the secret token as the first param.
    async fn call<T: DeserializeOwned>(&self, method: &str, params: Vec<Value>) -> Result<T> {
        let mut full_params = Vec::with_capacity(params.len() + 1);
        full_params.push(Value::String(self.token.clone()));
        full_params.extend(params);

        let body = json!({
            "jsonrpc": "2.0",
            "id": "snatch",
            "method": method,
            "params": full_params,
        });

        let response = match self.http.post(&self.endpoint).json(&body).send().await {
            Ok(response) => response,
            // Keep the common "daemon is down" case short: it is surfaced in
            // toasts and in the reply the browser extension shows the user.
            Err(error) if error.is_connect() => {
                bail!(
                    "{method}: aria2 is not listening on 127.0.0.1:{}",
                    self.port
                )
            }
            Err(error) => {
                return Err(error).with_context(|| format!("{method}: the aria2 RPC call failed"));
            }
        };

        let status = response.status();
        let payload: Value = response.json().await.with_context(|| {
            format!("{method}: aria2 sent a malformed response (HTTP {status})")
        })?;

        if let Some(error) = payload.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("{method}: aria2 returned error {code}: {message}");
        }

        let result = payload
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method}: the response carried no result"))?;

        serde_json::from_value(result)
            .with_context(|| format!("{method}: the result had an unexpected shape"))
    }

    /// Cheap liveness probe; doubles as the readiness check on startup.
    pub async fn version(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct VersionReply {
            version: String,
        }
        Ok(self
            .call::<VersionReply>("aria2.getVersion", vec![])
            .await?
            .version)
    }

    /// Queue a download and return its aria2 GID.
    pub async fn add_uri(&self, request: &DownloadRequest) -> Result<String> {
        request.validate()?;

        let mut options = Map::new();
        let chosen_name = request.sanitized_filename();

        // Filing by type happens here rather than after the fact: aria2 writes
        // straight into the destination, so moving the file afterwards would
        // mean copying gigabytes for no reason.
        let mut directory = self.download_dir.clone();
        if self.categorise.load(std::sync::atomic::Ordering::Relaxed) {
            let basis = chosen_name
                .clone()
                .or_else(|| crate::types::name_from_url(&request.url))
                .unwrap_or_default();
            if let Some(category) = crate::settings::category_for(&basis) {
                directory = directory.join(category);
            }
        }
        options.insert(
            "dir".to_owned(),
            json!(directory.to_string_lossy().into_owned()),
        );

        // Segmenting and per-download limits come from settings.
        for (key, value) in self
            .options
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
        {
            options.insert(key.clone(), json!(value));
        }

        if let Some(name) = chosen_name {
            options.insert("out".to_owned(), json!(name));
        }
        if let Some(referer) = clean_header_value(request.referer.as_deref()) {
            options.insert("referer".to_owned(), json!(referer));
        }
        options.insert(
            "user-agent".to_owned(),
            json!(
                clean_header_value(request.user_agent.as_deref())
                    .unwrap_or_else(|| DEFAULT_USER_AGENT.to_owned())
            ),
        );

        // Cookies ride as a real header so aria2 does not need a cookie jar,
        // and the rest of what the page's player sent rides beside them. A
        // link behind a login or a signed CDN is refused without both.
        let mut headers: Vec<String> = Vec::new();
        if let Some(cookies) = clean_header_value(request.cookies.as_deref()) {
            headers.push(format!("Cookie: {cookies}"));
        }
        for (name, value) in request.extra_headers() {
            let Some(value) = clean_header_value(Some(&value)) else {
                continue;
            };
            headers.push(format!("{name}: {value}"));
        }
        if !headers.is_empty() {
            options.insert("header".to_owned(), json!(headers));
        }

        // Credentials go through the RPC options rather than aria2's command
        // line, so a password never appears in `ps` output. aria2 keys them by
        // protocol, so the scheme decides which pair to set.
        if let Some((user, password)) = request.credentials() {
            let (user_key, password_key) = if request
                .url
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("ftp")
            {
                ("ftp-user", "ftp-passwd")
            } else {
                ("http-user", "http-passwd")
            };
            options.insert(user_key.to_owned(), json!(user));
            options.insert(password_key.to_owned(), json!(password));
        }

        // aria2 hashes the finished file itself and fails the download if it
        // does not match, so this is the whole of verification for the
        // default engine. An unparseable digest is dropped rather than passed
        // through: aria2 would reject the whole add and the user would lose
        // the download over a typo in an optional field.
        match request.checksum.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => match crate::checksum::parse(text) {
                Some(checksum) => {
                    options.insert("checksum".to_owned(), json!(checksum.aria2_value()));
                }
                None => log::warn!("ignoring '{text}': not a checksum in any recognised form"),
            },
            _ => {}
        }

        // A download with its own start time joins the queue paused, so it
        // holds its place and its settings without using bandwidth until the
        // scheduler unpauses it.
        if request
            .start_at
            .is_some_and(|start_at| start_at > crate::settings::now_unix())
        {
            options.insert("pause".to_owned(), json!("true"));
        }

        // The route. An unusable pairing -- a SOCKS proxy, which aria2 cannot
        // speak -- is an error rather than a silent direct connection: the
        // whole promise of the proxy table is that nothing goes out behind
        // the user's back. This used to be missing entirely, so every aria2
        // transfer went direct whatever the table said.
        if let Some(proxy) = self
            .proxies
            .resolve_for(PROXY_KEY, Engine::Aria2)
            .context("the download cannot use the configured proxy")?
        {
            options.insert("all-proxy".to_owned(), json!(proxy.url()));
        }

        // Every source for the same file. aria2 spreads its connections
        // across the mirrors and fails over between them, so one slow host
        // does not decide the speed.
        self.call(
            "aria2.addUri",
            vec![json!(request.sources()), Value::Object(options)],
        )
        .await
    }

    /// The status string aria2 reports for one download.
    async fn status_of(&self, gid: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Status {
            #[serde(default)]
            status: String,
        }
        Ok(self
            .call::<Status>("aria2.tellStatus", vec![json!(gid), json!(["status"])])
            .await?
            .status)
    }

    /// Send one download through `proxy` from now on, or directly for `None`.
    ///
    /// aria2 only lets a proxy be changed on a download that is waiting or
    /// paused, so a running one is paused around the change and resumed
    /// afterwards, which reopens its connections through the new route. A
    /// download that has already finished has nothing left to route.
    pub async fn route(&self, gid: &str, proxy: Option<&ProxyEndpoint>) -> Result<()> {
        if let Some(proxy) = proxy
            && !proxy.supports(Engine::Aria2)
        {
            bail!(
                "proxy '{}' is {} and aria2 can only use HTTP proxies",
                proxy.label,
                proxy.scheme
            );
        }

        let status = self.status_of(gid).await?;
        if matches!(status.as_str(), "complete" | "error" | "removed") {
            bail!("the download has already finished");
        }
        let was_active = status == "active";
        if was_active {
            self.pause(gid).await?;
            // The state moves on the next pass of aria2's loop; changing an
            // option before then is refused.
            let deadline = tokio::time::Instant::now() + PAUSE_SETTLE;
            while self.status_of(gid).await? == "active" {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        }

        let mut options = Map::new();
        options.insert(
            "all-proxy".to_owned(),
            json!(proxy.map(ProxyEndpoint::url).unwrap_or_default()),
        );
        let changed = self
            .call::<Value>(
                "aria2.changeOption",
                vec![json!(gid), Value::Object(options)],
            )
            .await
            .map(drop);

        if was_active && let Err(error) = self.unpause(gid).await {
            log::warn!("could not resume {gid} after rerouting it: {error:#}");
        }
        changed
    }

    /// Everything aria2 knows about, in one round trip per bucket.
    pub async fn snapshot(&self) -> Result<Vec<DownloadStatus>> {
        let keys = json!(STATUS_KEYS);
        let (active, waiting, stopped) = tokio::try_join!(
            self.call::<Vec<DownloadStatus>>("aria2.tellActive", vec![keys.clone()]),
            self.call::<Vec<DownloadStatus>>(
                "aria2.tellWaiting",
                vec![json!(0), json!(512), keys.clone()]
            ),
            self.call::<Vec<DownloadStatus>>(
                "aria2.tellStopped",
                vec![json!(0), json!(512), keys.clone()]
            ),
        )?;

        let mut all = active;
        all.extend(waiting);
        all.extend(stopped);
        Ok(all)
    }

    pub async fn pause(&self, gid: &str) -> Result<()> {
        self.call::<Value>("aria2.pause", vec![json!(gid)])
            .await
            .map(drop)
    }

    pub async fn unpause(&self, gid: &str) -> Result<()> {
        self.call::<Value>("aria2.unpause", vec![json!(gid)])
            .await
            .map(drop)
    }

    /// Move a waiting download within the queue.
    ///
    /// aria2 only orders *waiting* downloads: an active one is already
    /// running and a finished one has no position, so the call is a no-op
    /// there rather than an error worth surfacing.
    pub async fn move_in_queue(&self, gid: &str, movement: QueueMove) -> Result<()> {
        let (position, how) = match movement {
            QueueMove::Up => (-1, "POS_CUR"),
            QueueMove::Down => (1, "POS_CUR"),
            QueueMove::Top => (0, "POS_SET"),
            QueueMove::Bottom => (0, "POS_END"),
        };
        self.call::<Value>(
            "aria2.changePosition",
            vec![json!(gid), json!(position), json!(how)],
        )
        .await
        .map(drop)
    }

    pub async fn pause_all(&self) -> Result<()> {
        self.call::<Value>("aria2.pauseAll", vec![]).await.map(drop)
    }

    pub async fn unpause_all(&self) -> Result<()> {
        self.call::<Value>("aria2.unpauseAll", vec![])
            .await
            .map(drop)
    }

    /// Drop a download from aria2 entirely.
    ///
    /// A GID is either live (`remove` applies) or already stopped
    /// (`removeDownloadResult` applies), so we try both and only fail if
    /// neither worked.
    pub async fn remove(&self, gid: &str) -> Result<()> {
        let live = self
            .call::<Value>("aria2.forceRemove", vec![json!(gid)])
            .await;
        let purged = self
            .call::<Value>("aria2.removeDownloadResult", vec![json!(gid)])
            .await;

        match (live, purged) {
            (Ok(_), _) | (_, Ok(_)) => Ok(()),
            (Err(live_error), Err(purge_error)) => Err(anyhow!(
                "could not remove {gid}: {live_error}; {purge_error}"
            )),
        }
    }

    /// Forget every finished/failed entry, leaving the files on disk.
    pub async fn purge_finished(&self) -> Result<()> {
        self.call::<Value>("aria2.purgeDownloadResult", vec![])
            .await
            .map(drop)
    }

    pub async fn save_session(&self) -> Result<()> {
        self.call::<Value>("aria2.saveSession", vec![])
            .await
            .map(drop)
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.call::<Value>("aria2.shutdown", vec![]).await.map(drop)
    }
}

/// Strip CR/LF so a hostile page cannot smuggle extra headers into aria2.
fn clean_header_value(value: Option<&str>) -> Option<String> {
    let value = value?;
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_owned())
    }
}

/// One file belonging to a download. aria2 reports one entry per file; for
/// plain HTTP downloads there is exactly one.
#[derive(Debug, Clone, Deserialize)]
pub struct Aria2File {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub uris: Vec<Aria2Uri>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Aria2Uri {
    #[serde(default)]
    pub uri: String,
}

/// A single download as reported by `aria2.tell*`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStatus {
    pub gid: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, deserialize_with = "de_loose_u64")]
    pub total_length: u64,
    #[serde(default, deserialize_with = "de_loose_u64")]
    pub completed_length: u64,
    #[serde(default, deserialize_with = "de_loose_u64")]
    pub download_speed: u64,
    #[serde(default, deserialize_with = "de_loose_u64")]
    pub connections: u64,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub files: Vec<Aria2File>,
}

impl DownloadStatus {
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }

    pub fn is_paused(&self) -> bool {
        self.status == "paused"
    }

    pub fn is_waiting(&self) -> bool {
        self.status == "waiting"
    }

    pub fn is_complete(&self) -> bool {
        self.status == "complete"
    }

    pub fn is_error(&self) -> bool {
        self.status == "error"
    }

    /// True once aria2 will not make further progress on its own.
    pub fn is_finished(&self) -> bool {
        matches!(self.status.as_str(), "complete" | "error" | "removed")
    }

    pub fn fraction(&self) -> f64 {
        if self.is_complete() {
            return 1.0;
        }
        if self.total_length == 0 {
            return 0.0;
        }
        (self.completed_length as f64 / self.total_length as f64).clamp(0.0, 1.0)
    }

    /// The destination path, if aria2 has decided on one.
    pub fn path(&self) -> Option<&str> {
        self.files
            .first()
            .map(|file| file.path.as_str())
            .filter(|path| !path.is_empty())
    }

    /// The directory aria2 will write into, even before a path exists.
    pub fn folder(&self) -> Option<&str> {
        self.dir.as_deref().filter(|dir| !dir.is_empty())
    }

    pub fn source_uri(&self) -> Option<&str> {
        self.files
            .iter()
            .flat_map(|file| file.uris.iter())
            .map(|uri| uri.uri.as_str())
            .find(|uri| !uri.is_empty())
    }

    /// Best available label: destination basename, then source URL, then GID.
    pub fn display_name(&self) -> String {
        if let Some(path) = self.path()
            && let Some(name) = path.rsplit('/').next().filter(|name| !name.is_empty())
        {
            return name.to_owned();
        }
        if let Some(uri) = self.source_uri()
            && let Some(name) = name_from_url(uri)
        {
            return name;
        }
        format!("Download {}", self.gid)
    }

    /// Seconds remaining at the current rate, when that is meaningful.
    pub fn eta_seconds(&self) -> Option<u64> {
        if !self.is_active() || self.download_speed == 0 || self.total_length == 0 {
            return None;
        }
        let remaining = self.total_length.saturating_sub(self.completed_length);
        if remaining == 0 {
            return None;
        }
        Some(remaining / self.download_speed.max(1))
    }
}

/// aria2 encodes every integer as a JSON string. Be liberal and never fail the
/// whole snapshot over one odd field.
fn de_loose_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => text.trim().parse().unwrap_or(0),
        Value::Number(number) => number.as_u64().unwrap_or(0),
        _ => 0,
    })
}

/// Keep an aria2 daemon alive for as long as Snatch runs.
///
/// The loop is intentionally state-light: "is something answering on the RPC
/// port?" is the only question it asks. That makes it correct whether the
/// daemon is ours, was inherited from a previous crash, or was started by hand.
pub async fn supervise(client: Aria2Client, config: Aria2Config, events: Sender<UiEvent>) {
    let mut child: Option<Child> = None;
    let mut backoff = Duration::from_secs(2);

    loop {
        if let Ok(version) = client.version().await {
            backoff = Duration::from_secs(2);
            if events.send(UiEvent::Aria2Up(version)).await.is_err() {
                return; // the UI is gone; so are we
            }

            // Idle here until the endpoint stops answering.
            loop {
                sleep(Duration::from_secs(3)).await;
                if client.version().await.is_err() {
                    break;
                }
            }

            log::warn!(
                "aria2 stopped answering on port {}; restarting it",
                client.port
            );
            let _ = events
                .send(UiEvent::Aria2Down(
                    "aria2 stopped responding — restarting".to_owned(),
                ))
                .await;
            drop(child.take()); // kill_on_drop reaps a half-dead daemon
            continue;
        }

        match build_command(&config, &client).spawn() {
            Ok(mut spawned) => {
                if let Some(stderr) = spawned.stderr.take() {
                    tokio::spawn(forward_stderr(stderr));
                }
                // Dropping the previous slot kills any half-dead daemon.
                drop(child.take());

                match wait_until_ready(&client, &mut spawned).await {
                    Ok(_) => {
                        child = Some(spawned);
                        continue; // the top of the loop will publish Aria2Up
                    }
                    Err(error) => {
                        drop(spawned);
                        let _ = events
                            .send(UiEvent::Aria2Down(format!(
                                "aria2c started but its RPC never came up: {error:#}"
                            )))
                            .await;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = events
                    .send(UiEvent::Aria2Down(
                        "aria2c was not found in PATH — install the 'aria2' package".to_owned(),
                    ))
                    .await;
            }
            Err(error) => {
                let _ = events
                    .send(UiEvent::Aria2Down(format!(
                        "could not start aria2c: {error}"
                    )))
                    .await;
            }
        }

        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn build_command(config: &Aria2Config, client: &Aria2Client) -> Command {
    let mut command = Command::new("aria2c");
    command
        .arg("--enable-rpc")
        .arg("--rpc-listen-all=false")
        // NOTE: --rpc-listen-port, not --listen-port; the latter is the
        // BitTorrent peer port and would collide with the RPC socket.
        .arg(format!("--rpc-listen-port={}", client.port))
        // aria2 blanks the secret out of its own argv, so it is not left
        // readable in `ps` for the life of the daemon.
        .arg(format!("--rpc-secret={}", client.secret))
        .arg("--rpc-allow-origin-all=false")
        .arg(format!("--dir={}", config.download_dir.display()))
        // Resume support. The segmenting and limits are appended below from
        // settings, so they are configurable rather than baked in.
        .arg("--continue=true")
        .arg("--auto-file-renaming=true")
        .arg("--allow-overwrite=false")
        .arg("--conditional-get=true")
        // A .torrent file should be saved, not silently joined.
        .arg("--follow-torrent=false")
        .arg("--follow-metalink=false")
        // Persist the queue so a restart picks up where we left off.
        .arg(format!("--save-session={}", config.session_file.display()))
        .arg("--save-session-interval=20")
        // Deliberately NOT --force-save: it keeps a .aria2 control file next
        // to every finished file and re-saves completed downloads into the
        // session, so they would come back on the next launch.
        .arg("--summary-interval=0")
        .arg("--console-log-level=warn")
        // If Snatch dies without a clean shutdown, aria2 must not outlive it.
        .arg(format!("--stop-with-process={}", std::process::id()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    command.args(&config.spawn_args);

    // aria2 refuses to start if --input-file points at a missing file.
    if config
        .session_file
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
    {
        command.arg(format!("--input-file={}", config.session_file.display()));
    }

    command
}

/// Poll the RPC endpoint until it answers, or the daemon exits first.
///
/// An exit is reported straight away with its status. Before this the loop
/// polled for the full twenty seconds against a child that had already died
/// -- a port in use is enough -- and then reported that the RPC "never came
/// up", with the real reason only in the log.
async fn wait_until_ready(client: &Aria2Client, child: &mut Child) -> Result<String> {
    let mut last_error = None;
    for _ in 0..READY_ATTEMPTS {
        match client.version().await {
            Ok(version) => {
                log::info!("aria2 {version} is ready on port {}", client.port);
                return Ok(version);
            }
            Err(error) => last_error = Some(error),
        }
        tokio::select! {
            exited = child.wait() => {
                let status = exited.context("could not wait for aria2c")?;
                bail!("aria2c exited with {status} (its stderr is in the log)");
            }
            _ = sleep(READY_INTERVAL) => {}
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("aria2 did not become ready")))
}

/// A loopback port nothing is listening on right now.
///
/// Bound and released, so there is a window in which something else could
/// take it; aria2 then fails to start and `supervise` reports the exit.
fn free_loopback_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("could not bind a loopback port")?;
    Ok(listener.local_addr()?.port())
}

/// Thirty-two hex characters from the kernel's pool, or from the hash
/// seed when `/dev/urandom` cannot be read, which never happens on Linux.
fn random_secret() -> String {
    use std::io::Read;

    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !read {
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let word = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
                .to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn forward_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if !line.trim().is_empty() => log::warn!(target: "aria2c", "{line}"),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return,
        }
    }
}

/// Push a fresh snapshot at the UI, polling faster while transfers are running.
pub async fn poll_snapshots(client: Aria2Client, events: Sender<UiEvent>) {
    const BUSY: Duration = Duration::from_millis(500);
    const IDLE: Duration = Duration::from_millis(1500);
    const OFFLINE: Duration = Duration::from_secs(2);

    let mut interval = IDLE;
    loop {
        sleep(interval).await;
        match client.snapshot().await {
            Ok(downloads) => {
                interval = if downloads.iter().any(DownloadStatus::is_active) {
                    BUSY
                } else {
                    IDLE
                };
                if events.send(UiEvent::Snapshot(downloads)).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                log::debug!("snapshot failed: {error:#}");
                interval = OFFLINE;
            }
        }
    }
}
