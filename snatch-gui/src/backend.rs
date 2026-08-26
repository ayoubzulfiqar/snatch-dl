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
        db: Database,
        download_dir: PathBuf,
        managed_bin_dir: PathBuf,
        settings: Arc<std::sync::RwLock<crate::settings::Settings>>,
        settings_path: PathBuf,
        gallery_events: tokio::sync::mpsc::Sender<crate::gallery::GalleryEvent>,
        video_events: tokio::sync::mpsc::Sender<crate::ytdlp::VideoEvent>,
        wget_events: tokio::sync::mpsc::Sender<crate::wget::WgetEvent>,
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
            db,
            download_dir,
            managed_bin_dir,
            settings,
            settings_path,
            gallery_events,
            video_events,
            wget_events,
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
        if previous.torrent.enable_dht != next.torrent.enable_dht
            || previous.torrent.accept_incoming != next.torrent.accept_incoming
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

    /// The torrent engine, or a readable error when it never started.
    pub fn torrents(&self) -> Result<Arc<TorrentEngine>> {
        self.torrents
            .clone()
            .context("the BitTorrent session is not running")
    }
}
