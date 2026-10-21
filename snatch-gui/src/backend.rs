//! The engines the UI drives, bundled into one cheap-to-clone handle.
//!
//! Every field is either `Arc` or already cheap to clone, so a page can keep a
//! copy without thinking about lifetimes. The one job this type really does is
//! [`Backend::offload`]: it moves work off the GLib main loop onto the tokio
//! runtime and hands the result back, which is the only safe way for a GTK
//! callback to touch any of these engines.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::aria2::Aria2Client;
use crate::db::Database;
use crate::gallery::GalleryEngine;
use crate::network::ProxyManager;
use crate::processor::MediaQueue;
use crate::torrent::TorrentEngine;
use crate::wget::WgetEngine;
use crate::ytdlp::VideoEngine;

/// How long a published-digest lookup may hold a download up.
const CHECKSUM_LOOKUP: std::time::Duration = std::time::Duration::from_secs(8);

#[derive(Clone)]
pub struct Backend {
    pub aria2: Aria2Client,
    /// `None` when the BitTorrent session could not be started; the Torrents
    /// page shows the reason instead of pretending to work.
    pub torrents: Option<Arc<TorrentEngine>>,
    pub gallery: Arc<GalleryEngine>,
    pub video: Arc<VideoEngine>,
    pub wget: Arc<WgetEngine>,
    pub proxies: Arc<ProxyManager>,
    pub media: Arc<MediaQueue>,
    pub archives: Arc<crate::archive::ArchiveQueue>,
    pub mirrors: Arc<crate::mirror::MirrorEngine>,
    pub db: Database,
    pub download_dir: PathBuf,
    /// Where Snatch installs tools for itself.
    pub managed_bin_dir: PathBuf,
    /// Live settings, shared with every engine that reads them.
    pub settings: Arc<std::sync::RwLock<crate::settings::Settings>>,
    settings_path: PathBuf,
    /// Handed to each new scrape so its progress reaches the UI.
    pub gallery_events: tokio::sync::mpsc::Sender<crate::gallery::GalleryEvent>,
    /// Handed to each new extraction so its progress reaches the UI.
    pub video_events: tokio::sync::mpsc::Sender<crate::ytdlp::VideoEvent>,
    /// Handed to each wget download so its progress reaches the UI.
    pub wget_events: tokio::sync::mpsc::Sender<crate::wget::WgetEvent>,
    /// Handed to each crawl so its progress reaches the UI.
    pub mirror_events: tokio::sync::mpsc::Sender<crate::mirror::MirrorEvent>,
    handle: tokio::runtime::Handle,
}

impl Backend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        aria2: Aria2Client,
        torrents: Option<Arc<TorrentEngine>>,
        gallery: Arc<GalleryEngine>,
        video: Arc<VideoEngine>,
        wget: Arc<WgetEngine>,
        proxies: Arc<ProxyManager>,
        media: Arc<MediaQueue>,
        archives: Arc<crate::archive::ArchiveQueue>,
        mirrors: Arc<crate::mirror::MirrorEngine>,
        db: Database,
        download_dir: PathBuf,
        managed_bin_dir: PathBuf,
        settings: Arc<std::sync::RwLock<crate::settings::Settings>>,
        settings_path: PathBuf,
        gallery_events: tokio::sync::mpsc::Sender<crate::gallery::GalleryEvent>,
        video_events: tokio::sync::mpsc::Sender<crate::ytdlp::VideoEvent>,
        wget_events: tokio::sync::mpsc::Sender<crate::wget::WgetEvent>,
        mirror_events: tokio::sync::mpsc::Sender<crate::mirror::MirrorEvent>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            aria2,
            torrents,
            gallery,
            video,
            wget,
            proxies,
            media,
            archives,
            mirrors,
            db,
            download_dir,
            managed_bin_dir,
            settings,
            settings_path,
            gallery_events,
            video_events,
            wget_events,
            mirror_events,
            handle,
        }
    }

    /// Run a future on the tokio runtime and await it from the GLib main loop.
    pub async fn offload<T, F>(&self, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.handle.spawn(async move {
            let _ = tx.send(future.await);
        });
        rx.await.context("the background task was cancelled")?
    }

    /// Spawn a detached background task on the tokio runtime.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn(future);
    }

    /// A snapshot of the current settings.
    pub fn settings(&self) -> crate::settings::Settings {
        self.settings
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Persist new settings and apply everything that can change live.
    ///
    /// Returns the settings that need a restart to take effect, so the UI can
    /// say so rather than leaving the user wondering.
    pub async fn apply_settings(
        &self,
        next: crate::settings::Settings,
    ) -> Result<Vec<&'static str>> {
        let next = next.clamped();
        let previous = self.settings();

        // A folder that cannot be used is refused here, with the reason,
        // rather than saved and discovered at the next launch.
        if let Some(chosen) = next.download_dir_override() {
            crate::settings::prepare_download_dir(&chosen).with_context(|| {
                format!("the download folder {} cannot be used", chosen.display())
            })?;
        }

        {
            let mut guard = self
                .settings
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            *guard = next.clone();
        }
        next.save(&self.settings_path)
            .context("could not save settings")?;

        // Per-download options apply to the next download immediately.
        self.aria2
            .set_download_options(next.aria2_download_options());

        // The short list aria2 accepts while running.
        if let Err(error) = self
            .aria2
            .change_global_options(next.aria2_live_options())
            .await
        {
            log::warn!("could not apply live aria2 options: {error:#}");
        }

        // The upload cap is the one torrent setting librqbit changes live.
        if previous.torrent.max_upload_kib != next.torrent.max_upload_kib
            && let Some(engine) = &self.torrents
        {
            engine.set_upload_limit_kib(next.torrent.max_upload_kib);
        }

        // Anything read only at spawn time needs aria2 or the session
        // restarted; say which rather than silently doing nothing.
        let mut restart_needed = Vec::new();
        if previous.download.allocation != next.download.allocation {
            restart_needed.push("file allocation");
        }
        if previous.download.auto_save_interval != next.download.auto_save_interval {
            restart_needed.push("resume-data writing");
        }
        if previous.download.retries != next.download.retries
            || previous.download.retry_wait_seconds != next.download.retry_wait_seconds
        {
            restart_needed.push("retry policy");
        }
        if previous.download.check_certificate != next.download.check_certificate {
            restart_needed.push("certificate checking");
        }
        // `--user-agent` is a spawn-time option too, and the one most likely
        // to be changed to get past a site that refuses the default.
        if previous.download.user_agent != next.download.user_agent {
            restart_needed.push("the user agent");
        }
        if previous.torrent.enable_dht != next.torrent.enable_dht
            || previous.torrent.accept_incoming != next.torrent.accept_incoming
            || previous.torrent.max_peers != next.torrent.max_peers
            || previous.torrent.seed_ratio != next.torrent.seed_ratio
        {
            restart_needed.push("BitTorrent networking");
        }
        if previous.interface.download_dir != next.interface.download_dir {
            restart_needed.push("download folder");
        }
        Ok(restart_needed)
    }

    /// Save settings without touching any engine.
    ///
    /// Used for incidental state such as the last-visited page, which must not
    /// re-apply aria2 options or claim a restart is needed.
    #[allow(dead_code, reason = "kept for callers that already hold a full copy")]
    pub async fn persist_only(&self, next: crate::settings::Settings) -> Result<()> {
        {
            let mut guard = self
                .settings
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            *guard = next.clone();
        }
        next.save(&self.settings_path)
            .context("could not save settings")
    }

    /// Change one thing about the settings and save, atomically.
    ///
    /// The read, the change and the write all happen under the lock, so two
    /// incidental updates in flight at once -- the page switch and the
    /// drawer toggle fire together when the sidebar is collapsed -- cannot
    /// overwrite each other's field. Nothing is written when the change
    /// turned out to be a no-op.
    pub fn update_settings(
        &self,
        change: impl FnOnce(&mut crate::settings::Settings),
    ) -> Result<()> {
        let mut guard = self
            .settings
            .write()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut next = guard.clone();
        change(&mut next);
        if next == *guard {
            return Ok(());
        }
        next.save(&self.settings_path)
            .context("could not save settings")?;
        *guard = next;
        Ok(())
    }

    /// Look for a digest published beside a file, before it is queued.
    ///
    /// One place for both entry points -- the window and the browser socket
    /// -- so "Verify downloads" means the same thing whichever way a file
    /// arrives. A miss is `None`, never an error: most downloads have no
    /// published digest, and a server that never answers must not hold up
    /// the download it was only ever going to annotate, which is what the
    /// cap is for.
    pub async fn discover_checksum(
        &self,
        url: &str,
        filename: &str,
    ) -> Option<(crate::checksum::Checksum, String)> {
        let proxy = self
            .proxies
            .resolve_for("checksum", crate::network::Engine::Http)
            .unwrap_or(None);
        let client = match self.proxies.client(proxy.as_ref()) {
            Ok(client) => client,
            Err(error) => {
                log::debug!("no checksum lookup: {error:#}");
                return None;
            }
        };
        tokio::time::timeout(
            CHECKSUM_LOOKUP,
            crate::checksum::discover(&client, url, filename),
        )
        .await
        .ok()
        .flatten()
    }

    /// Whether a download should be checked against a published digest:
    /// the setting is on and nothing supplied one already.
    pub fn wants_checksum_lookup(&self, request: &crate::types::DownloadRequest) -> bool {
        request.checksum.is_none() && self.settings().download.verify_downloads
    }

    /// The torrent engine, or a readable error when it never started.
    pub fn torrents(&self) -> Result<Arc<TorrentEngine>> {
        self.torrents
            .clone()
            .context("the BitTorrent session is not running")
    }
}
