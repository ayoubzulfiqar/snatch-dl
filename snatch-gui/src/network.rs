//! Proxy routing.
//!
//! Snatch drives four transports and they do **not** agree on what a proxy is:
//!
//! | engine                    | HTTP proxy | SOCKS5 proxy |
//! |---------------------------|------------|--------------|
//! | aria2 (`--all-proxy`)     | yes        | **no**       |
//! | librqbit (torrents)       | **no**     | yes          |
//! | reqwest (our own traffic) | yes        | yes          |
//! | yt-dlp / gallery-dl       | yes        | yes          |
//!
//! aria2 genuinely has no SOCKS support (it is absent from `aria2c --help=#all`
//! entirely), and librqbit's outbound connector speaks only `socks5://`. So a
//! proxy is not a single global setting: it is chosen per download, and an
//! endpoint that cannot serve the engine a task needs is rejected up front
//! rather than silently ignored — a leaked direct connection is exactly the
//! failure a proxy user cannot tolerate.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Where a proxy sits in the pipeline. Used to reject impossible pairings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    /// Plain HTTP/FTP downloads handled by aria2.
    Aria2,
    /// BitTorrent transfers handled by librqbit.
    Torrent,
    /// Requests Snatch itself makes with reqwest.
    Http,
    /// External helpers (`yt-dlp`, `gallery-dl`) that take a `--proxy` flag.
    Subprocess,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Aria2 => "downloads",
            Engine::Torrent => "torrents",
            Engine::Http => "internal requests",
            Engine::Subprocess => "scrapers",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyScheme {
    Http,
    Https,
    Socks5,
}

impl ProxyScheme {
    fn as_str(self) -> &'static str {
        match self {
            ProxyScheme::Http => "http",
            ProxyScheme::Https => "https",
            ProxyScheme::Socks5 => "socks5",
        }
    }

    fn parse(scheme: &str) -> Result<Self> {
        Ok(match scheme.to_ascii_lowercase().as_str() {
            "http" => ProxyScheme::Http,
            "https" => ProxyScheme::Https,
            "socks5" | "socks5h" | "socks" => ProxyScheme::Socks5,
            other => bail!("unsupported proxy scheme '{other}' (use http, https or socks5)"),
        })
    }

    /// Which engines can actually use this kind of proxy.
    pub fn engines(self) -> &'static [Engine] {
        match self {
            // aria2 speaks HTTP proxies; librqbit does not.
            ProxyScheme::Http | ProxyScheme::Https => {
                &[Engine::Aria2, Engine::Http, Engine::Subprocess]
            }
            // librqbit speaks SOCKS5; aria2 has no SOCKS support whatsoever.
            ProxyScheme::Socks5 => &[Engine::Torrent, Engine::Http, Engine::Subprocess],
        }
    }
}

impl fmt::Display for ProxyScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One configured proxy server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyEndpoint {
    /// Stable, user-visible name. Also the key used by assignments.
    pub label: String,
    pub scheme: ProxyScheme,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl ProxyEndpoint {
    /// Parse `scheme://[user[:pass]@]host:port`.
    pub fn parse(label: impl Into<String>, raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("the proxy URL is empty");
        }
        let parsed =
            url::Url::parse(raw).with_context(|| format!("'{raw}' is not a valid proxy URL"))?;

        let scheme = ProxyScheme::parse(parsed.scheme())?;
        let host = parsed
            .host_str()
            .context("the proxy URL has no host")?
            .to_owned();
        let port = parsed
            .port_or_known_default()
            .context("the proxy URL has no port and the scheme has no default")?;

        let username = match parsed.username() {
            "" => None,
            name => Some(
                percent_decode(name).with_context(|| "the proxy username is not valid UTF-8")?,
            ),
        };
        let password = match parsed.password() {
            None | Some("") => None,
            Some(secret) => {
                Some(percent_decode(secret).context("the proxy password is not valid UTF-8")?)
            }
        };

        let label = label.into();
        let label = if label.trim().is_empty() {
            format!("{scheme}://{host}:{port}")
        } else {
            label.trim().to_owned()
        };

        Ok(Self {
            label,
            scheme,
            host,
            port,
            username,
            password,
        })
    }

    /// The URL to hand to an engine, credentials included.
    pub fn url(&self) -> String {
        match (&self.username, &self.password) {
            (Some(user), Some(secret)) => format!(
                "{}://{}:{}@{}:{}",
                self.scheme,
                percent_encode(user),
                percent_encode(secret),
                self.host,
                self.port
            ),
            (Some(user), None) => format!(
                "{}://{}@{}:{}",
                self.scheme,
                percent_encode(user),
                self.host,
                self.port
            ),
            _ => format!("{}://{}:{}", self.scheme, self.host, self.port),
        }
    }

    /// The same URL with the password masked, for the UI and the log.
    pub fn redacted(&self) -> String {
        match &self.username {
            Some(user) => format!("{}://{user}:***@{}:{}", self.scheme, self.host, self.port),
            None => format!("{}://{}:{}", self.scheme, self.host, self.port),
        }
    }

    pub fn supports(&self, engine: Engine) -> bool {
        self.scheme.engines().contains(&engine)
    }

    fn socket_addr(&self) -> String {
        // Bracket IPv6 literals so the string round-trips through a resolver.
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// The result of the most recent reachability probe.
#[derive(Debug, Clone)]
pub struct ProxyHealth {
    /// Round-trip time of the TCP handshake to the proxy itself.
    pub connect: Option<Duration>,
    /// Round-trip time of a real request forwarded through the proxy.
    pub end_to_end: Option<Duration>,
    pub error: Option<String>,
}

impl ProxyHealth {
    pub fn is_healthy(&self) -> bool {
        self.error.is_none() && self.end_to_end.is_some()
    }

    /// One-line summary for the proxy list.
    pub fn summary(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        match (self.connect, self.end_to_end) {
            (Some(connect), Some(total)) => format!(
                "{} ms to connect, {} ms end to end",
                connect.as_millis(),
                total.as_millis()
            ),
            (Some(connect), None) => format!("{} ms to connect", connect.as_millis()),
            _ => "not tested".to_owned(),
        }
    }
}

/// A small, tolerant URL-escape for credentials embedded in a proxy URL.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|text| u8::from_str_radix(text, 16).ok());
            match hex {
                Some(decoded) => {
                    out.push(decoded);
                    index += 3;
                    continue;
                }
                None => bail!("malformed percent-escape in the proxy URL"),
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).context("proxy credentials are not valid UTF-8")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    proxies: Vec<ProxyEndpoint>,
    /// Label of the proxy used when a task has no explicit assignment.
    #[serde(default)]
    default_proxy: Option<String>,
    /// Task key (aria2 GID, torrent info hash, scraper job id) -> proxy label.
    #[serde(default)]
    assignments: HashMap<String, String>,
}

#[derive(Default)]
struct State {
    persisted: PersistedState,
    health: HashMap<String, ProxyHealth>,
}

/// The proxy table plus per-task routing decisions.
///
/// Locks are `std` and never held across an `.await`, so both the GLib main
/// loop and tokio workers can read the table without an executor round trip.
pub struct ProxyManager {
    state: RwLock<State>,
    path: PathBuf,
    probe_url: String,
}

/// A 204-with-no-body endpoint: the smallest honest "did this proxy work" check.
const DEFAULT_PROBE_URL: &str = "https://www.gstatic.com/generate_204";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

impl ProxyManager {
    /// Load the proxy table, tolerating a missing or corrupt file.
    pub fn load(path: PathBuf) -> Self {
        let persisted = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedState>(&bytes) {
                Ok(state) => state,
                Err(error) => {
                    log::warn!("ignoring malformed {}: {error}", path.display());
                    PersistedState::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(error) => {
                log::warn!("could not read {}: {error}", path.display());
                PersistedState::default()
            }
        };

        Self {
            state: RwLock::new(State {
                persisted,
                health: HashMap::new(),
            }),
            path,
            probe_url: std::env::var("SNATCH_PROXY_PROBE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_PROBE_URL.to_owned()),
        }
    }

    /// `std::sync` lock poisoning cannot happen with `panic = "abort"`, but in a
    /// debug build we would rather carry on with the data than abort.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn list(&self) -> Vec<(ProxyEndpoint, Option<ProxyHealth>)> {
        let state = self.read();
        state
            .persisted
            .proxies
            .iter()
            .map(|proxy| (proxy.clone(), state.health.get(&proxy.label).cloned()))
            .collect()
    }

    #[allow(dead_code, reason = "lookup helper for per-task assignment")]
    pub fn get(&self, label: &str) -> Option<ProxyEndpoint> {
        self.read()
            .persisted
            .proxies
            .iter()
            .find(|proxy| proxy.label == label)
            .cloned()
    }

    pub fn default_label(&self) -> Option<String> {
        self.read().persisted.default_proxy.clone()
    }

    /// Add or replace a proxy, keyed by label.
    pub fn upsert(&self, endpoint: ProxyEndpoint) -> Result<()> {
        {
            let mut state = self.write();
            match state
                .persisted
                .proxies
                .iter_mut()
                .find(|proxy| proxy.label == endpoint.label)
            {
                Some(existing) => *existing = endpoint,
                None => state.persisted.proxies.push(endpoint),
            }
        }
        self.persist()
    }

    pub fn remove(&self, label: &str) -> Result<()> {
        {
            let mut state = self.write();
            state.persisted.proxies.retain(|proxy| proxy.label != label);
            state.health.remove(label);
            if state.persisted.default_proxy.as_deref() == Some(label) {
                state.persisted.default_proxy = None;
            }
            state
                .persisted
                .assignments
                .retain(|_, assigned| assigned != label);
        }
        self.persist()
    }

    /// Choose the proxy used when a task has no explicit assignment.
    pub fn set_default(&self, label: Option<&str>) -> Result<()> {
        {
            let mut state = self.write();
            match label {
                Some(label) => {
                    if !state.persisted.proxies.iter().any(|p| p.label == label) {
                        bail!("there is no proxy called '{label}'");
                    }
                    state.persisted.default_proxy = Some(label.to_owned());
                }
                None => state.persisted.default_proxy = None,
            }
        }
        self.persist()
    }

    /// Pin one task (aria2 GID, torrent info hash, scraper job id) to a proxy.
    ///
    /// Persisted and honoured by `resolve_for`; the dialog currently exposes
    /// only the session-wide default, so assignments come from the socket.
    #[allow(
        dead_code,
        reason = "per-task routing is set over the socket, not the dialog"
    )]
    pub fn assign(&self, task_key: &str, label: Option<&str>) -> Result<()> {
        {
            let mut state = self.write();
            match label {
                Some(label) => {
                    if !state.persisted.proxies.iter().any(|p| p.label == label) {
                        bail!("there is no proxy called '{label}'");
                    }
                    state
                        .persisted
                        .assignments
                        .insert(task_key.to_owned(), label.to_owned());
                }
                None => {
                    state.persisted.assignments.remove(task_key);
                }
            }
        }
        self.persist()
    }

    /// The proxy a task should use, if any: its own assignment, else the default.
    pub fn resolve(&self, task_key: &str) -> Option<ProxyEndpoint> {
        let state = self.read();
        let label = state
            .persisted
            .assignments
            .get(task_key)
            .or(state.persisted.default_proxy.as_ref())?;
        state
            .persisted
            .proxies
            .iter()
            .find(|proxy| &proxy.label == label)
            .cloned()
    }

    /// The proxy for a task, but only if it can actually serve `engine`.
    ///
    /// Returns `Err` rather than `None` when a proxy is configured but unusable,
    /// so the caller fails loudly instead of quietly connecting directly.
    pub fn resolve_for(&self, task_key: &str, engine: Engine) -> Result<Option<ProxyEndpoint>> {
        let Some(proxy) = self.resolve(task_key) else {
            return Ok(None);
        };
        if !proxy.supports(engine) {
            bail!(
                "proxy '{}' is {} and cannot be used for {}; \
                 aria2 supports only HTTP proxies and BitTorrent only SOCKS5",
                proxy.label,
                proxy.scheme,
                engine.label()
            );
        }
        Ok(Some(proxy))
    }

    /// Build a reqwest client that routes through `endpoint` (or directly).
    pub fn client(&self, endpoint: Option<&ProxyEndpoint>) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("Snatch/", env!("CARGO_PKG_VERSION")));

        builder = match endpoint {
            Some(proxy) => builder.proxy(
                reqwest::Proxy::all(proxy.url())
                    .with_context(|| format!("'{}' is not a usable proxy", proxy.redacted()))?,
            ),
            None => builder.no_proxy(),
        };

        builder.build().context("could not build the HTTP client")
    }

    /// Measure a proxy: TCP handshake first, then a real request through it.
    pub async fn probe(&self, endpoint: &ProxyEndpoint) -> ProxyHealth {
        let address = endpoint.socket_addr();

        let started = Instant::now();
        let connect = match timeout(CONNECT_TIMEOUT, TcpStream::connect(&address)).await {
            Ok(Ok(_stream)) => Some(started.elapsed()),
            Ok(Err(error)) => {
                return self.record(
                    endpoint,
                    ProxyHealth {
                        connect: None,
                        end_to_end: None,
                        error: Some(format!("cannot reach {address}: {error}")),
                    },
                );
            }
            Err(_) => {
                return self.record(
                    endpoint,
                    ProxyHealth {
                        connect: None,
                        end_to_end: None,
                        error: Some(format!(
                            "{address} did not answer within {}s",
                            CONNECT_TIMEOUT.as_secs()
                        )),
                    },
                );
            }
        };

        let client = match self.client(Some(endpoint)) {
            Ok(client) => client,
            Err(error) => {
                return self.record(
                    endpoint,
                    ProxyHealth {
                        connect,
                        end_to_end: None,
                        error: Some(format!("{error:#}")),
                    },
                );
            }
        };

        let started = Instant::now();
        let health = match client.get(&self.probe_url).send().await {
            Ok(response) if response.status().is_success() || response.status().as_u16() == 204 => {
                ProxyHealth {
                    connect,
                    end_to_end: Some(started.elapsed()),
                    error: None,
                }
            }
            Ok(response) => ProxyHealth {
                connect,
                end_to_end: None,
                error: Some(format!("the proxy answered HTTP {}", response.status())),
            },
            Err(error) => ProxyHealth {
                connect,
                end_to_end: None,
                // reqwest's chained source is noise in a list row.
                error: Some(format!("request through the proxy failed: {error}")),
            },
        };

        self.record(endpoint, health)
    }

    /// Probe every configured proxy concurrently.
    pub async fn probe_all(&self) -> Vec<(String, ProxyHealth)> {
        let proxies: Vec<ProxyEndpoint> = self.read().persisted.proxies.to_vec();

        let mut results = Vec::with_capacity(proxies.len());
        // Sequential on purpose: a handful of endpoints, and a burst of
        // simultaneous connects is exactly what makes flaky proxies look dead.
        for proxy in proxies {
            let health = self.probe(&proxy).await;
            results.push((proxy.label.clone(), health));
        }
        results
    }

    fn record(&self, endpoint: &ProxyEndpoint, health: ProxyHealth) -> ProxyHealth {
        self.write()
            .health
            .insert(endpoint.label.clone(), health.clone());
        health
    }

    fn persist(&self) -> Result<()> {
        let snapshot = {
            let state = self.read();
            serde_json::to_vec_pretty(&state.persisted)
                .context("could not encode the proxy table")?
        };
        write_atomically(&self.path, &snapshot)
    }
}

/// Write via a temporary file and rename, so a crash cannot truncate the table.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("the proxy table has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;

    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_socks_url() {
        let proxy = ProxyEndpoint::parse("home", "socks5://127.0.0.1:1080")
            .expect("a plain socks5 URL should parse");
        assert_eq!(proxy.scheme, ProxyScheme::Socks5);
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, 1080);
        assert_eq!(proxy.url(), "socks5://127.0.0.1:1080");
    }

    #[test]
    fn round_trips_credentials_that_need_escaping() {
        let proxy = ProxyEndpoint::parse("paid", "http://user:p%40ss%3Aword@example.com:8080")
            .expect("an escaped credential should parse");
        assert_eq!(proxy.username.as_deref(), Some("user"));
        assert_eq!(proxy.password.as_deref(), Some("p@ss:word"));
        // The reserved characters must be escaped again on the way out, or the
        // URL would be reparsed with the wrong host.
        assert_eq!(proxy.url(), "http://user:p%40ss%3Aword@example.com:8080");
    }

    #[test]
    fn never_prints_the_password() {
        let proxy = ProxyEndpoint::parse("paid", "socks5://bob:hunter2@example.com:1080")
            .expect("credentials should parse");
        let shown = proxy.redacted();
        assert!(
            !shown.contains("hunter2"),
            "redacted form leaked the password: {shown}"
        );
        assert!(shown.contains("bob"));
    }

    #[test]
    fn engine_support_matches_reality() {
        let socks = ProxyEndpoint::parse("s", "socks5://127.0.0.1:1080").expect("parses");
        let http = ProxyEndpoint::parse("h", "http://127.0.0.1:8080").expect("parses");

        // aria2 has no SOCKS support; librqbit has no HTTP-proxy support.
        assert!(!socks.supports(Engine::Aria2));
        assert!(socks.supports(Engine::Torrent));
        assert!(http.supports(Engine::Aria2));
        assert!(!http.supports(Engine::Torrent));
    }

    #[test]
    fn rejects_unusable_pairings_loudly() {
        let directory = std::env::temp_dir().join("snatch-proxy-test-reject.json");
        let _ = std::fs::remove_file(&directory);
        let manager = ProxyManager::load(directory.clone());
        manager
            .upsert(ProxyEndpoint::parse("s", "socks5://127.0.0.1:1080").expect("parses"))
            .expect("upsert works");
        manager.set_default(Some("s")).expect("default works");

        // A SOCKS proxy for an aria2 task must error, not fall back to direct.
        assert!(manager.resolve_for("gid-1", Engine::Aria2).is_err());
        assert!(manager.resolve_for("gid-1", Engine::Torrent).is_ok());
        let _ = std::fs::remove_file(&directory);
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        assert!(ProxyEndpoint::parse("x", "ftp://127.0.0.1:21").is_err());
        assert!(ProxyEndpoint::parse("x", "not a url").is_err());
    }
}
