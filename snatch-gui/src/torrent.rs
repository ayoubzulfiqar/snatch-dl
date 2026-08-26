//! BitTorrent engine, backed by [`librqbit`].
//!
//! aria2 can technically download a torrent, but it has no DHT worth the name,
//! no peer exchange and no way to bias piece selection for playback. librqbit
//! is a maintained pure-Rust engine that has all three, so torrents get their
//! own session here rather than being pushed through aria2.
//!
//! Two things are worth knowing before reading on:
//!
//! * **Proxying.** librqbit's outbound connector speaks `socks5://` only — an
//!   HTTP proxy cannot carry BitTorrent. [`crate::network`] enforces that, and
//!   the proxy is fixed for the whole session because librqbit resolves it once
//!   when the session is built.
//! * **Sequential download.** librqbit has no "sequential" switch. It
//!   prioritises the pieces a *reader* needs: an open [`librqbit::FileStream`]
//!   contributes a 32 MiB window ahead of its current position to the piece
//!   queue. Holding a stream still would therefore only prioritise the first
//!   32 MiB, so [`TorrentEngine::set_sequential`] runs a pump that keeps
//!   advancing the read position, dragging that window through the file in
//!   order. That is what makes the file playable while it downloads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ConnectionOptions, ManagedTorrent, Session,
    SessionOptions, TorrentStatsState,
};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::network::{Engine, ProxyManager};

/// How often the engine publishes a fresh picture of every torrent.
const POLL_INTERVAL: Duration = Duration::from_millis(700);
/// Read size for the sequential pump. Large enough to be cheap, small enough
/// that the priority window advances smoothly.
const PUMP_CHUNK: usize = 1024 * 1024;

/// Coarse state, mapped from librqbit's own so the UI never sees engine types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentPhase {
    /// Fetching metadata from a magnet link, or re-checking files on disk.
    Initializing,
    Downloading,
    Seeding,
    Paused,
    Error,
}

impl TorrentPhase {
    pub fn label(self) -> &'static str {
        match self {
            TorrentPhase::Initializing => "Preparing",
            TorrentPhase::Downloading => "Downloading",
            TorrentPhase::Seeding => "Seeding",
            TorrentPhase::Paused => "Paused",
            TorrentPhase::Error => "Failed",
        }
    }
}

/// Peer counts, split the way librqbit tracks them.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeerBreakdown {
    pub live: u32,
    pub tcp: u32,
    pub utp: u32,
    pub socks: u32,
    pub queued: u32,
    pub connecting: u32,
    pub seen: u32,
    pub dead: u32,
}

impl PeerBreakdown {
    /// One-line description for the torrent row.
    pub fn summary(&self) -> String {
        let mut kinds = Vec::new();
        if self.tcp > 0 {
            kinds.push(format!("{} TCP", self.tcp));
        }
        if self.utp > 0 {
            kinds.push(format!("{} uTP", self.utp));
        }
        if self.socks > 0 {
            kinds.push(format!("{} SOCKS", self.socks));
        }

        let mut summary = format!("{} peers", self.live);
        if !kinds.is_empty() {
            summary.push_str(&format!(" ({})", kinds.join(", ")));
        }
        if self.connecting > 0 {
            summary.push_str(&format!(", {} connecting", self.connecting));
        }
        if self.queued > 0 {
            summary.push_str(&format!(", {} queued", self.queued));
        }
        summary.push_str(&format!(", {} known", self.seen));
        if self.dead > 0 {
            summary.push_str(&format!(", {} dead", self.dead));
        }
        summary
    }
}

#[derive(Debug, Clone)]
pub struct TorrentFile {
    pub index: usize,
    pub name: String,
    pub length: u64,
    pub downloaded: u64,
}

impl TorrentFile {
    pub fn fraction(&self) -> f64 {
        if self.length == 0 {
            return 0.0;
        }
        (self.downloaded as f64 / self.length as f64).clamp(0.0, 1.0)
    }
}

/// Everything the UI needs to draw one torrent.
#[derive(Debug, Clone)]
pub struct TorrentSnapshot {
    pub id: usize,
    pub info_hash: String,
    pub name: String,
    pub phase: TorrentPhase,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    pub download_bps: u64,
    pub upload_bps: u64,
    pub eta: Option<Duration>,
    pub peers: PeerBreakdown,
    pub error: Option<String>,
    pub output_folder: PathBuf,
    pub files: Vec<TorrentFile>,
    /// True while a sequential pump is running for this torrent.
    pub sequential: bool,
    pub finished: bool,
}

impl TorrentSnapshot {
    pub fn fraction(&self) -> f64 {
        if self.finished {
            return 1.0;
        }
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.progress_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }

    /// Share ratio, or `None` before anything has been downloaded.
    pub fn ratio(&self) -> Option<f64> {
        if self.progress_bytes == 0 {
            return None;
        }
        Some(self.uploaded_bytes as f64 / self.progress_bytes as f64)
    }
}

/// A running sequential pump.
struct Pump {
    file_index: usize,
    task: JoinHandle<()>,
}

impl Drop for Pump {
    fn drop(&mut self) {
        // Dropping the FileStream inside the task is what deregisters the
        // priority window, so aborting is enough.
        self.task.abort();
    }
}

/// The torrent session plus Snatch's own bookkeeping.
pub struct TorrentEngine {
    session: Arc<Session>,
    download_dir: PathBuf,
    /// torrent id -> active sequential pump.
    pumps: RwLock<HashMap<usize, Pump>>,
    /// The proxy the session was built with, for display.
    proxy_label: Option<String>,
}

impl TorrentEngine {
    /// Build the session.
    ///
    /// The proxy is resolved once, here: librqbit fixes its connector when the
    /// session is created, so changing proxies needs a restart. `proxies` is
    /// consulted with the reserved key `"torrent"`, which the proxy dialog uses
    /// as the session-wide assignment slot.
    pub async fn start(
        download_dir: PathBuf,
        state_dir: PathBuf,
        proxies: &ProxyManager,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&download_dir)
            .with_context(|| format!("could not create {}", download_dir.display()))?;
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("could not create {}", state_dir.display()))?;

        // An unusable pairing is an error, not a silent direct connection.
        let proxy = proxies
            .resolve_for("torrent", Engine::Torrent)
            .context("the torrent session cannot use the configured proxy")?;
        if let Some(proxy) = &proxy {
            log::info!("routing torrent traffic through {}", proxy.redacted());
        }

        let options = SessionOptions {
            // DHT and peer exchange are the whole point of using a real engine.
            dht: Some(librqbit::DhtSessionConfig::default()),
            fastresume: true,
            persistence: Some(librqbit::SessionPersistenceConfig::Json {
                folder: Some(state_dir),
            }),
            // Without a listener librqbit only makes outgoing connections:
            // no peer can reach us, which halves the usable swarm and makes
            // seeding impossible. A port of 0 lets the OS pick.
            //
            // A SOCKS5 proxy carries outgoing connections only, so binding a
            // public listener would leak the real address; skip it when the
            // session is proxied.
            listen: (proxy.is_none()).then(|| librqbit::ListenerOptions {
                mode: librqbit::ListenerMode::TcpAndUtp,
                enable_upnp_port_forwarding: true,
                ..Default::default()
            }),
            connect: Some(ConnectionOptions {
                proxy_url: proxy.as_ref().map(|proxy| proxy.url()),
                ..Default::default()
            }),
            client_name_and_version: Some(concat!("Snatch ", env!("CARGO_PKG_VERSION")).to_owned()),
            ..Default::default()
        };

        let session = Session::new_with_opts(download_dir.clone(), options)
            .await
            .context("could not start the BitTorrent session")?;

        log::info!(
            "BitTorrent session ready (DHT enabled, listening on {:?})",
            session.listen_addr()
        );

        Ok(Arc::new(Self {
            session,
            download_dir,
            pumps: RwLock::new(HashMap::new()),
            proxy_label: proxy.map(|proxy| proxy.label),
        }))
    }

    #[allow(dead_code, reason = "the UI uses Backend::download_dir")]
    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    pub fn proxy_label(&self) -> Option<&str> {
        self.proxy_label.as_deref()
    }

    fn pumps(&self) -> std::sync::RwLockReadGuard<'_, HashMap<usize, Pump>> {
        self.pumps
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn pumps_mut(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<usize, Pump>> {
        self.pumps
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Add a `magnet:` link. Metadata is fetched from DHT/peers in the
    /// background, so this returns as soon as the torrent is registered.
    pub async fn add_magnet(&self, magnet: &str) -> Result<usize> {
        let magnet = magnet.trim();
        if !magnet.starts_with("magnet:") {
            bail!("'{}' is not a magnet link", truncate(magnet, 60));
        }
        // Parse before handing over so a bad link fails with our message.
        librqbit::Magnet::parse(magnet).context("could not parse the magnet link")?;
        self.add(AddTorrent::from_url(magnet.to_owned())).await
    }

    /// Add a `.torrent` file from disk.
    pub async fn add_torrent_file(&self, path: &Path) -> Result<usize> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("could not read {}", path.display()))?;
        if bytes.is_empty() {
            bail!("{} is empty", path.display());
        }
        self.add(AddTorrent::from_bytes(bytes)).await
    }

    /// Add a torrent from raw `.torrent` bytes (as posted over IPC).
    ///
    /// Reachable over the socket rather than the UI, which uses a file chooser.
    #[allow(dead_code, reason = "engine API used by socket clients, not the UI")]
    pub async fn add_torrent_bytes(&self, bytes: Vec<u8>) -> Result<usize> {
        if bytes.is_empty() {
            bail!("the torrent file is empty");
        }
        self.add(AddTorrent::from_bytes(bytes)).await
    }

    async fn add(&self, add: AddTorrent<'_>) -> Result<usize> {
        let options = AddTorrentOptions {
            // Resuming an existing download requires writing over the files
            // that are already there.
            overwrite: true,
            ..Default::default()
        };

        let response = self
            .session
            .add_torrent(add, Some(options))
            .await
            .context("the BitTorrent session rejected the torrent")?;

        match response {
            AddTorrentResponse::Added(id, handle) => {
                log::info!("added torrent {id} ({})", handle.info_hash().as_string());
                Ok(id)
            }
            AddTorrentResponse::AlreadyManaged(id, handle) => {
                log::info!(
                    "torrent {id} ({}) is already in the session",
                    handle.info_hash().as_string()
                );
                Ok(id)
            }
            // We never pass `list_only`, so this branch means librqbit changed
            // behaviour rather than that the caller asked for a listing.
            AddTorrentResponse::ListOnly(_) => {
                bail!("the session returned a file listing instead of adding the torrent")
            }
        }
    }

    fn handle(&self, id: usize) -> Result<Arc<ManagedTorrent>> {
        self.session
            .get(id.into())
            .with_context(|| format!("there is no torrent with id {id}"))
    }

    pub async fn pause(&self, id: usize) -> Result<()> {
        let handle = self.handle(id)?;
        // A paused torrent has no live pieces, so a pump would spin on errors.
        self.stop_sequential(id);
        self.session
            .pause(&handle)
            .await
            .with_context(|| format!("could not pause torrent {id}"))
    }

    pub async fn resume(&self, id: usize) -> Result<()> {
        let handle = self.handle(id)?;
        self.session
            .unpause(&handle)
            .await
            .with_context(|| format!("could not resume torrent {id}"))
    }

    /// Remove a torrent, optionally deleting what it has written.
    pub async fn remove(&self, id: usize, delete_files: bool) -> Result<()> {
        self.stop_sequential(id);
        self.session
            .delete(id.into(), delete_files)
            .await
            .with_context(|| format!("could not remove torrent {id}"))
    }

    /// Detailed peer counts for one torrent.
    ///
    /// The page renders the counts carried in each snapshot; this is the
    /// on-demand equivalent for a socket client.
    #[allow(dead_code, reason = "on-demand query for socket clients")]
    pub fn peers(&self, id: usize) -> Result<PeerBreakdown> {
        let handle = self.handle(id)?;
        Ok(peer_breakdown(&handle.stats()))
    }

    /// True if a sequential pump is running for this torrent.
    #[allow(dead_code, reason = "the UI reads the flag from the snapshot instead")]
    pub fn is_sequential(&self, id: usize) -> bool {
        self.pumps().contains_key(&id)
    }

    /// Turn sequential (streaming-friendly) download on or off.
    ///
    /// See the module docs: this drives an open [`librqbit::FileStream`]
    /// forward so librqbit's 32 MiB priority window walks the file in order.
    pub fn set_sequential(
        self: &Arc<Self>,
        id: usize,
        file_index: usize,
        enabled: bool,
    ) -> Result<()> {
        if !enabled {
            self.stop_sequential(id);
            return Ok(());
        }

        if let Some(existing) = self.pumps().get(&id)
            && existing.file_index == file_index
        {
            return Ok(()); // already pumping that file
        }

        let handle = self.handle(id)?;
        let file_count = handle.metadata.load().as_ref().map(|m| m.file_infos.len());
        if let Some(count) = file_count
            && file_index >= count
        {
            bail!("torrent {id} has {count} files, so there is no file {file_index}");
        }

        let engine = Arc::clone(self);
        let task = tokio::spawn(async move {
            if let Err(error) = pump_sequentially(handle, file_index).await {
                log::warn!("sequential download for torrent {id} stopped: {error:#}");
            }
            // Clear our own slot so the UI stops claiming it is sequential.
            engine.pumps_mut().remove(&id);
        });

        // Replacing an entry drops the old Pump, which aborts its task.
        self.pumps_mut().insert(id, Pump { file_index, task });
        log::info!("sequential download enabled for torrent {id}, file {file_index}");
        Ok(())
    }

    fn stop_sequential(&self, id: usize) {
        if self.pumps_mut().remove(&id).is_some() {
            log::info!("sequential download disabled for torrent {id}");
        }
    }

    /// A picture of every torrent in the session.
    pub fn snapshot(&self) -> Vec<TorrentSnapshot> {
        let sequential: Vec<usize> = self.pumps().keys().copied().collect();
        // `with_torrents` takes an `Fn`, so accumulate through a cell.
        let collected = std::cell::RefCell::new(Vec::new());

        self.session.with_torrents(|torrents| {
            let mut collected = collected.borrow_mut();
            for (id, handle) in torrents {
                collected.push(describe(id, handle, sequential.contains(&id)));
            }
        });

        let mut out = collected.into_inner();
        out.sort_by_key(|snapshot| snapshot.id);
        out
    }

    /// Publish a snapshot on `POLL_INTERVAL` until the receiver goes away.
    pub async fn watch(self: Arc<Self>, updates: mpsc::Sender<Vec<TorrentSnapshot>>) {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            if updates.send(self.snapshot()).await.is_err() {
                log::debug!("torrent watcher stopping: the UI is gone");
                return;
            }
        }
    }

    /// Stop the session, flushing persistence.
    pub async fn shutdown(&self) {
        {
            // Abort every pump before the session goes away.
            let mut pumps = self.pumps_mut();
            pumps.clear();
        }
        self.session.stop().await;
    }
}

/// Read a file inside the torrent from start to finish, discarding the bytes.
///
/// The discard is the point: each read blocks until the piece under the read
/// head has arrived, and advancing the head moves librqbit's priority window,
/// so pieces are requested in file order instead of rarest-first. The cost is
/// re-reading each byte once from the local cache, which is far cheaper than
/// the network transfer it is pacing.
async fn pump_sequentially(handle: Arc<ManagedTorrent>, file_index: usize) -> Result<()> {
    handle
        .wait_until_initialized()
        .await
        .context("the torrent never finished initialising")?;

    let mut stream = handle
        .stream(file_index)
        .await
        .with_context(|| format!("could not open file {file_index} for streaming"))?;

    let total = stream.len();
    let mut buffer = vec![0u8; PUMP_CHUNK];
    let mut read_total: u64 = 0;

    while read_total < total {
        let read = stream
            .read(&mut buffer)
            .await
            .context("sequential read failed")?;
        if read == 0 {
            break; // end of file
        }
        read_total += read as u64;
    }

    log::info!("sequential pass over file {file_index} finished ({read_total} bytes)");
    Ok(())
}

fn describe(id: usize, handle: &Arc<ManagedTorrent>, sequential: bool) -> TorrentSnapshot {
    let stats = handle.stats();

    let phase = match (&stats.state, stats.finished) {
        (TorrentStatsState::Error, _) => TorrentPhase::Error,
        (TorrentStatsState::Paused, _) => TorrentPhase::Paused,
        (TorrentStatsState::Initializing { .. }, _) => TorrentPhase::Initializing,
        (TorrentStatsState::Live, true) => TorrentPhase::Seeding,
        (TorrentStatsState::Live, false) => TorrentPhase::Downloading,
    };

    // librqbit exposes its own ETA only as a preformatted string, so compute
    // one from bytes and speed to match how the rest of the UI formats time.
    let (download_bps, upload_bps) = match &stats.live {
        Some(live) => (
            mbps_to_bytes(live.download_speed.mbps),
            mbps_to_bytes(live.upload_speed.mbps),
        ),
        None => (0, 0),
    };
    let eta = eta_for(
        stats.total_bytes,
        stats.progress_bytes,
        download_bps,
        stats.finished,
    );

    let files = handle
        .metadata
        .load()
        .as_ref()
        .map(|metadata| {
            metadata
                .file_infos
                .iter()
                .enumerate()
                .map(|(index, info)| TorrentFile {
                    index,
                    name: info.relative_filename.to_string_lossy().into_owned(),
                    length: info.len,
                    downloaded: stats.file_progress.get(index).copied().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();

    TorrentSnapshot {
        id,
        info_hash: handle.info_hash().as_string(),
        name: handle.name().unwrap_or_else(|| format!("Torrent {id}")),
        phase,
        progress_bytes: stats.progress_bytes,
        total_bytes: stats.total_bytes,
        uploaded_bytes: stats.uploaded_bytes,
        download_bps,
        upload_bps,
        eta,
        peers: peer_breakdown(&stats),
        error: stats.error.clone(),
        output_folder: handle.output_folder().to_path_buf(),
        files,
        sequential,
        finished: stats.finished,
    }
}

fn peer_breakdown(stats: &librqbit::TorrentStats) -> PeerBreakdown {
    let Some(live) = &stats.live else {
        return PeerBreakdown::default();
    };
    let peers = &live.snapshot.peer_stats;
    PeerBreakdown {
        live: peers.live,
        tcp: peers.live_tcp,
        utp: peers.live_utp,
        socks: peers.live_socks,
        queued: peers.queued,
        connecting: peers.connecting,
        seen: peers.seen,
        dead: peers.dead,
    }
}

/// Seconds remaining at the current rate, when that is meaningful.
fn eta_for(total: u64, progress: u64, bytes_per_second: u64, finished: bool) -> Option<Duration> {
    if finished || bytes_per_second == 0 || total == 0 {
        return None;
    }
    let remaining = total.saturating_sub(progress);
    if remaining == 0 {
        return None;
    }
    Some(Duration::from_secs(remaining / bytes_per_second.max(1)))
}

/// librqbit reports speed in megabits per second; the UI works in bytes.
fn mbps_to_bytes(mbps: f64) -> u64 {
    if !mbps.is_finite() || mbps <= 0.0 {
        return 0;
    }
    (mbps * 1_000_000.0 / 8.0) as u64
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let kept: String = value.chars().take(max).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_librqbit_speed_units() {
        // 8 Mbit/s is 1 MB/s.
        assert_eq!(mbps_to_bytes(8.0), 1_000_000);
        assert_eq!(mbps_to_bytes(0.0), 0);
        assert_eq!(mbps_to_bytes(-1.0), 0);
        assert_eq!(mbps_to_bytes(f64::NAN), 0);
    }

    #[test]
    fn peer_summary_mentions_every_transport() {
        let peers = PeerBreakdown {
            live: 12,
            tcp: 8,
            utp: 3,
            socks: 1,
            connecting: 4,
            seen: 90,
            ..Default::default()
        };
        let summary = peers.summary();
        assert!(summary.contains("12 peers"), "{summary}");
        assert!(summary.contains("8 TCP"), "{summary}");
        assert!(summary.contains("3 uTP"), "{summary}");
        assert!(summary.contains("1 SOCKS"), "{summary}");
        assert!(summary.contains("4 connecting"), "{summary}");
        assert!(summary.contains("90 known"), "{summary}");
    }

    #[test]
    fn progress_is_clamped_and_finished_wins() {
        let mut snapshot = TorrentSnapshot {
            id: 1,
            info_hash: "abc".into(),
            name: "t".into(),
            phase: TorrentPhase::Downloading,
            progress_bytes: 50,
            total_bytes: 100,
            uploaded_bytes: 25,
            download_bps: 0,
            upload_bps: 0,
            eta: None,
            peers: PeerBreakdown::default(),
            error: None,
            output_folder: PathBuf::from("/tmp"),
            files: Vec::new(),
            sequential: false,
            finished: false,
        };
        assert!((snapshot.fraction() - 0.5).abs() < f64::EPSILON);
        assert!((snapshot.ratio().unwrap_or_default() - 0.5).abs() < f64::EPSILON);

        // A finished torrent reads as complete even if the byte counts lag.
        snapshot.finished = true;
        assert!((snapshot.fraction() - 1.0).abs() < f64::EPSILON);

        // No division by zero before anything is known.
        snapshot.finished = false;
        snapshot.total_bytes = 0;
        snapshot.progress_bytes = 0;
        assert_eq!(snapshot.fraction(), 0.0);
        assert!(snapshot.ratio().is_none());
    }
}
