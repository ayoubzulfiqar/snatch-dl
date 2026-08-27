//! Snatch — a download manager for Linux.
//!
//! Startup order matters and is deliberate:
//!
//! 1. Build the tokio runtime. It owns every socket, child process and HTTP
//!    call; the GLib main loop owns every widget. The two meet only through
//!    [`backend::Backend::offload`] and an `async_channel` of [`UiEvent`]s.
//! 2. Register on the session bus. A second launch hands off to the running
//!    instance and exits, so the browser can fire `snatch-gui` at will.
//! 3. Bind the IPC socket *before* the UI, so a duplicate instance fails loudly
//!    instead of silently stealing the path `snatch-nmh` writes to.
//! 4. Start the engines — aria2, librqbit, gallery-dl, ffmpeg — all off the UI
//!    thread, and funnel their event streams into one channel the UI drains.
//!
//! A failure to start the BitTorrent session is not fatal: the rest of the
//! application works and the Torrents page explains itself.

mod aria2;
mod backend;
mod batch;
mod checksum;
mod curl;
mod db;
mod deps;
mod gallery;
mod ipc;
mod network;
mod paths;
mod processor;
mod settings;
mod sniff;
mod torrent;
mod types;
mod ui;
mod wget;
mod ytdlp;

pub use gtk4 as gtk;
pub use libadwaita as adw;

use std::sync::Arc;

use anyhow::{Context, Result};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::backend::Backend;
use crate::types::UiEvent;

/// Depth of each engine's event channel. Generous: a scrape of a large gallery
/// can emit hundreds of file events in a burst, and dropping them would leave
/// the counters wrong.
const EVENT_QUEUE: usize = 1024;

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    match run() {
        Ok(code) => code,
        Err(error) => {
            log::error!("{error:#}");
            eprintln!("snatch-gui: {error:#}");
            glib::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<glib::ExitCode> {
    // Four workers: aria2 RPC is idle-ish, but librqbit, gallery-dl and ffmpeg
    // all want to make progress at the same time.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("snatch-io")
        .enable_all()
        .build()
        .context("could not start the async runtime")?;

    let app = adw::Application::builder()
        .application_id(paths::APP_ID)
        .build();

    match app.register(gio::Cancellable::NONE) {
        Ok(()) if app.is_remote() => {
            // Snatch is already running. Let GApplication forward the
            // activation over D-Bus and unregister us on the way out; calling
            // `activate()` directly would leave the bus name dangling.
            log::info!(
                "another instance owns {}; raising its window",
                paths::APP_ID
            );
            return Ok(app.run_with_args::<&str>(&[]));
        }
        Ok(()) => {}
        Err(error) => {
            log::warn!("session bus unavailable ({error}); single-instance handling is degraded");
        }
    }

    // Put Snatch-managed tools on PATH before anything spawns a subprocess,
    // so every engine finds a self-installed yt-dlp or gallery-dl without the
    // user having to touch their shell configuration.
    if let Ok(managed) = paths::managed_bin_dir() {
        prepend_to_path(&managed);
    }

    // Settings are read before anything is configured by them.
    let settings_path = paths::settings_file()?;
    let settings = Arc::new(std::sync::RwLock::new(settings::Settings::load(
        &settings_path,
    )));
    let current = settings
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();

    let download_dir = match current.download_dir_override() {
        Some(chosen) => {
            std::fs::create_dir_all(&chosen)
                .with_context(|| format!("could not create {}", chosen.display()))?;
            chosen
        }
        None => paths::download_dir()?,
    };
    let socket_path = paths::socket_path()?;
    let aria2_config = aria2::Aria2Config {
        download_dir: download_dir.clone(),
        session_file: paths::session_file()?,
        spawn_args: current.aria2_spawn_args(),
    };

    let (events_tx, events_rx) = async_channel::unbounded::<UiEvent>();

    // Bind before anything else can race us for the path.
    let listener = runtime.block_on(ipc::bind(&socket_path))?;

    let proxies = Arc::new(network::ProxyManager::load(paths::proxy_file()?));
    let database = runtime.block_on(db::Database::open(paths::database_file()?))?;

    // Anything left "running" by a crash is not running now.
    match runtime.block_on(database.reconcile_orphans()) {
        Ok(0) => {}
        Ok(count) => log::info!("marked {count} interrupted job(s) as failed"),
        Err(error) => log::warn!("could not reconcile interrupted jobs: {error:#}"),
    }

    let aria2_options = Arc::new(std::sync::RwLock::new(current.aria2_download_options()));
    let aria2_client = aria2::Aria2Client::new(download_dir.clone(), Arc::clone(&aria2_options))?;
    aria2_client.set_categorise(current.download.categorise);

    // The BitTorrent session binds sockets and reads resume data, so it is
    // started up front — but a failure only disables the Torrents page.
    let torrent_engine = match runtime.block_on(torrent::TorrentEngine::start(
        download_dir.clone(),
        paths::torrent_state_dir()?,
        proxies.as_ref(),
    )) {
        Ok(engine) => Some(engine),
        Err(error) => {
            log::warn!("BitTorrent session unavailable: {error:#}");
            let _ = events_tx.send_blocking(UiEvent::TorrentsUnavailable(format!("{error:#}")));
            None
        }
    };

    let (gallery_tx, gallery_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
    let (media_tx, media_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
    let (torrent_tx, torrent_rx) = tokio::sync::mpsc::channel(64);
    let (video_tx, video_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
    let (wget_tx, wget_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);

    let gallery_engine = gallery::GalleryEngine::new(database.clone());
    let video_engine = ytdlp::VideoEngine::new(database.clone());
    let wget_engine = wget::WgetEngine::new(download_dir.clone());
    let media_queue = processor::MediaQueue::new(database.clone(), media_tx);

    let backend = Backend::new(
        aria2_client.clone(),
        torrent_engine.clone(),
        gallery_engine,
        video_engine,
        wget_engine,
        Arc::clone(&proxies),
        media_queue,
        database,
        download_dir,
        paths::managed_bin_dir()?,
        Arc::clone(&settings),
        settings_path,
        gallery_tx,
        video_tx,
        wget_tx,
        runtime.handle().clone(),
    );

    runtime.spawn(ipc::serve(listener, backend.clone(), events_tx.clone()));
    runtime.spawn(aria2::supervise(
        aria2_client.clone(),
        aria2_config,
        events_tx.clone(),
    ));
    runtime.spawn(aria2::poll_snapshots(
        aria2_client.clone(),
        events_tx.clone(),
    ));
    if let Some(engine) = torrent_engine.clone() {
        runtime.spawn(engine.watch(torrent_tx));
    }

    // Every engine speaks its own channel; fold them into the one the UI reads
    // so the GLib side has a single `recv` loop and events stay ordered per
    // engine.
    runtime.spawn(forward(torrent_rx, events_tx.clone(), UiEvent::Torrents));
    runtime.spawn(forward(gallery_rx, events_tx.clone(), UiEvent::Gallery));
    runtime.spawn(forward(media_rx, events_tx.clone(), UiEvent::Media));
    runtime.spawn(forward(video_rx, events_tx.clone(), UiEvent::Video));
    runtime.spawn(forward(wget_rx, events_tx.clone(), UiEvent::Wget));
    runtime.spawn(watch_for_shutdown(events_tx));

    app.connect_activate({
        let backend = backend.clone();
        move |app| ui::build(app, backend.clone(), events_rx.clone())
    });

    // GTK must not try to parse our argv; Snatch takes no command line options.
    let code = app.run_with_args::<&str>(&[]);

    // The window is gone. Stop the torrent session first so it flushes resume
    // data, then persist and stop aria2.
    runtime.block_on(async {
        if let Some(engine) = &torrent_engine {
            engine.shutdown().await;
        }
        if let Err(error) = aria2_client.save_session().await {
            log::debug!("could not save the aria2 session: {error:#}");
        }
        if let Err(error) = aria2_client.shutdown().await {
            log::debug!("could not shut aria2 down cleanly: {error:#}");
        }
    });

    if let Err(error) = std::fs::remove_file(&socket_path) {
        log::debug!("could not remove {}: {error}", socket_path.display());
    }

    Ok(code)
}

/// Pump one engine's channel into the shared UI channel.
async fn forward<T, F>(
    mut source: tokio::sync::mpsc::Receiver<T>,
    sink: async_channel::Sender<UiEvent>,
    wrap: F,
) where
    F: Fn(T) -> UiEvent + Send + 'static,
    T: Send + 'static,
{
    while let Some(item) = source.recv().await {
        if sink.send(wrap(item)).await.is_err() {
            return; // the UI is gone
        }
    }
}

/// Turn Ctrl+C and a session logout into a normal GApplication quit, so the
/// session is saved and the socket removed instead of left behind as a stale
/// file. Signals are handled on the tokio side and delivered to the GLib main
/// loop as an ordinary [`UiEvent`].
async fn watch_for_shutdown(events: async_channel::Sender<UiEvent>) {
    use tokio::signal::unix::{SignalKind, signal};

    let (mut interrupt, mut terminate) = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
        _ => {
            log::warn!("could not install signal handlers; Ctrl+C will not shut down cleanly");
            return;
        }
    };

    let name = tokio::select! {
        _ = interrupt.recv() => "SIGINT",
        _ = terminate.recv() => "SIGTERM",
    };

    log::info!("received {name}; shutting down");
    let _ = events.send(UiEvent::Quit).await;
}

/// Prepend a directory to this process's `PATH`.
///
/// Child processes inherit it, which is the whole point: the engines look up
/// `yt-dlp` and `gallery-dl` by name.
fn prepend_to_path(directory: &std::path::Path) {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut entries = vec![directory.to_path_buf()];
    entries.extend(std::env::split_paths(&existing));
    match std::env::join_paths(entries) {
        Ok(joined) => {
            // SAFETY: called from main before any thread that reads PATH is
            // spawned; the tokio runtime and GTK have not started yet.
            unsafe { std::env::set_var("PATH", joined) };
        }
        Err(error) => log::warn!(
            "could not extend PATH with {}: {error}",
            directory.display()
        ),
    }
}
