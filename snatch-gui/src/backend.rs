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
use crate::ytdlp::VideoEngine;

#[derive(Clone)]
pub struct Backend {
    pub aria2: Aria2Client,
    /// `None` when the BitTorrent session could not be started; the Torrents
    /// page shows the reason instead of pretending to work.
    pub torrents: Option<Arc<TorrentEngine>>,
    pub gallery: Arc<GalleryEngine>,
    pub video: Arc<VideoEngine>,
    pub proxies: Arc<ProxyManager>,
    pub media: Arc<MediaQueue>,
    pub db: Database,
    pub download_dir: PathBuf,
    /// Handed to each new scrape so its progress reaches the UI.
    pub gallery_events: tokio::sync::mpsc::Sender<crate::gallery::GalleryEvent>,
    /// Handed to each new extraction so its progress reaches the UI.
    pub video_events: tokio::sync::mpsc::Sender<crate::ytdlp::VideoEvent>,
    handle: tokio::runtime::Handle,
}

impl Backend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        aria2: Aria2Client,
        torrents: Option<Arc<TorrentEngine>>,
        gallery: Arc<GalleryEngine>,
        video: Arc<VideoEngine>,
        proxies: Arc<ProxyManager>,
        media: Arc<MediaQueue>,
        db: Database,
        download_dir: PathBuf,
        gallery_events: tokio::sync::mpsc::Sender<crate::gallery::GalleryEvent>,
        video_events: tokio::sync::mpsc::Sender<crate::ytdlp::VideoEvent>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            aria2,
            torrents,
            gallery,
            video,
            proxies,
            media,
            db,
            download_dir,
            gallery_events,
            video_events,
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

    /// The torrent engine, or a readable error when it never started.
    pub fn torrents(&self) -> Result<Arc<TorrentEngine>> {
        self.torrents
            .clone()
            .context("the BitTorrent session is not running")
    }
}
