//! The Unix domain socket server that `snatch-nmh` talks to.
//!
//! Protocol: one JSON object per line in, one JSON object per line out. Line
//! framing keeps the host trivial and makes the socket usable from a shell:
//!
//! ```text
//! printf '{"url":"https://example.com/f.iso"}\n' | nc -U ~/.local/share/snatch-dl/snatch.sock
//! ```

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_channel::Sender;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{sleep, timeout};

use crate::backend::Backend;
use crate::gallery::{GalleryConfig, destination_for};
use crate::types::{DownloadRequest, IpcResponse, JobKind, UiEvent};

/// A hand-off is a URL plus a cookie header; 256 KiB is generous.
const MAX_REQUEST_BYTES: u64 = 256 * 1024;
/// A client that cannot complete a request in this long is broken or hostile.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(45);

/// Bind the socket, clearing a stale file left behind by a crash.
///
/// This doubles as Snatch's cross-instance lock: if something is already
/// *listening*, we refuse to start rather than steal the path.
pub async fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        match timeout(Duration::from_secs(2), UnixStream::connect(path)).await {
            Ok(Ok(_)) => bail!(
                "another Snatch instance is already listening on {}",
                path.display()
            ),
            _ => {
                log::info!("clearing stale socket at {}", path.display());
                std::fs::remove_file(path)
                    .with_context(|| format!("could not remove stale socket {}", path.display()))?;
            }
        }
    }

    let listener =
        UnixListener::bind(path).with_context(|| format!("could not bind {}", path.display()))?;

    // Owner-only: the socket queues downloads on this user's behalf.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict permissions on {}", path.display()))?;

    log::info!("listening on {}", path.display());
    Ok(listener)
}

/// Accept hand-offs forever. Never returns under normal operation.
pub async fn serve(listener: UnixListener, backend: Backend, events: Sender<UiEvent>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let backend = backend.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let outcome = timeout(CLIENT_TIMEOUT, handle(stream, backend, events))
                        .await
                        .unwrap_or_else(|_| Err(anyhow!("the client timed out")));
                    if let Err(error) = outcome {
                        log::warn!("IPC client rejected: {error:#}");
                    }
                });
            }
            Err(error) => {
                log::error!("could not accept an IPC connection: {error}");
                sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn handle(stream: UnixStream, backend: Backend, events: Sender<UiEvent>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half.take(MAX_REQUEST_BYTES));

    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .await
        .context("could not read the IPC request")?;

    if read == 0 {
        // A bare connect-and-close: that is how `bind` probes for a live peer.
        return Ok(());
    }

    let outcome = accept_request(&line, &backend, &events).await;
    let response = match &outcome {
        Ok(gid) => IpcResponse::accepted(gid.clone()),
        Err(error) => IpcResponse::rejected(format!("{error:#}")),
    };

    let mut bytes = serde_json::to_vec(&response).context("could not encode the IPC reply")?;
    bytes.push(b'\n');
    write_half
        .write_all(&bytes)
        .await
        .context("could not send the IPC reply")?;
    write_half
        .flush()
        .await
        .context("could not flush the IPC reply")?;

    outcome.map(drop)
}

/// Validate, hand to the right engine, and tell the UI about it.
///
/// The returned id is engine-specific — an aria2 GID, a torrent id or a scrape
/// batch id — and is echoed to the browser so the extension can report it.
async fn accept_request(line: &str, backend: &Backend, events: &Sender<UiEvent>) -> Result<String> {
    let request: DownloadRequest = serde_json::from_str(line.trim())
        .context("the request was not a valid Snatch download request")?;
    request.validate()?;

    let name = request.display_name();
    let kind = request.inferred_kind();

    // A sniff has nothing to queue: it opens the picker in the window.
    if kind == JobKind::Sniff {
        events
            .send(UiEvent::SniffRequested {
                url: request.url.clone(),
            })
            .await
            .ok()
            .context("the window is not available to show the picker")?;
        return Ok("sniff".to_owned());
    }

    let id = match kind {
        // Honour the configured engine here too: this is the path every
        // browser hand-off takes, so routing only the UI through the setting
        // would leave it looking broken.
        JobKind::Download => match backend.settings().download.engine {
            crate::settings::HttpEngine::Wget => backend
                .wget
                .clone()
                .start(
                    request.clone(),
                    backend.settings(),
                    backend.proxies.clone(),
                    backend.wget_events.clone(),
                )?
                .to_string(),
            crate::settings::HttpEngine::Aria2 => {
                let gid = backend.aria2.add_uri(&request).await?;
                // add_uri adds a scheduled download paused. Without the
                // matching database row nothing would ever start it, so this
                // path has to record it exactly as the UI does.
                if let Some(start_at) = request
                    .start_at
                    .filter(|start_at| *start_at > crate::settings::now_unix())
                    && let Err(error) = backend
                        .db
                        .schedule_start(gid.clone(), request.display_name(), start_at)
                        .await
                {
                    log::warn!("could not record a scheduled start: {error:#}");
                }
                gid
            }
        },
        JobKind::Magnet => {
            let engine = backend.torrents()?;
            engine.add_magnet(&request.url).await?.to_string()
        }
        JobKind::Video => {
            let destination = crate::ytdlp::destination_for(&backend.download_dir);
            let config = crate::ytdlp::VideoConfig::new(destination);
            backend
                .video
                .clone()
                .start(
                    request.url.clone(),
                    config,
                    backend.proxies.clone(),
                    backend.video_events.clone(),
                )
                .await?
                .to_string()
        }
        // The early return above handles this. Returning an error rather
        // than `unreachable!` keeps the no-panic rule intact even if the
        // guard above is ever refactored away.
        JobKind::Sniff => bail!("a sniff has nothing to queue"),
        JobKind::Scrape => {
            let base = backend.download_dir.join("Snatch Galleries");
            let config = GalleryConfig::new(destination_for(&base, &request.url));
            backend
                .gallery
                .clone()
                .start(
                    request.url.clone(),
                    config,
                    backend.proxies.clone(),
                    backend.gallery_events.clone(),
                )
                .await?
                .to_string()
        }
    };

    log::info!("queued {} '{name}' as {id}", kind.label());

    // A closed channel just means the window went away first.
    let _ = events.send(UiEvent::Added { name, kind }).await;
    Ok(id)
}
