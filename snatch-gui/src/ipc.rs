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
        Ok(response) => response.clone(),
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
/// The reply carries an engine-specific id — an aria2 GID, a torrent id or a
/// scrape batch id — which is echoed to the browser so the extension can
/// report it. A format listing answers with the list instead, having queued
/// nothing.
async fn accept_request(
    line: &str,
    backend: &Backend,
    events: &Sender<UiEvent>,
) -> Result<IpcResponse> {
    let request: DownloadRequest = serde_json::from_str(line.trim())
        .context("the request was not a valid Snatch download request")?;
    request.validate()?;

    let name = request.display_name();
    let kind = request.inferred_kind();

    // A listing is a question, not a job. Answer it and queue nothing: the
    // button on a video asks this before the user has picked a resolution, so
    // it must not put anything in the window.
    if kind == JobKind::Formats {
        match crate::ytdlp::probe(&request.url, backend.proxies.as_ref()).await {
            Ok(probe) => {
                log::info!("listed {} formats for {name}", probe.formats.len());
                return Ok(IpcResponse::listing(probe));
            }
            // yt-dlp does not know this site. The browser watched what the
            // page loaded, so there is still a manifest to record or a file to
            // fetch -- neither of which needs anything to know the site.
            Err(error) if !request.streams.is_empty() || !request.files.is_empty() => {
                log::info!("yt-dlp could not read {name} ({error:#}); trying what the page loaded");
                let probe = stream_listing(&request).await;
                if probe.formats.is_empty() {
                    return Err(error);
                }
                log::info!(
                    "found {} playable address(es) on {name}",
                    probe.formats.len()
                );
                return Ok(IpcResponse::listing(probe));
            }
            Err(error) => return Err(error),
        }
    }

    // A sniff has nothing to queue either: it opens the picker in the window.
    if kind == JobKind::Sniff {
        events
            .send(UiEvent::SniffRequested {
                url: request.url.clone(),
            })
            .await
            .ok()
            .context("the window is not available to show the picker")?;
        return Ok(IpcResponse::accepted("sniff".to_owned()));
    }

    let id = match kind {
        JobKind::Stream => backend
            .video
            .clone()
            .start_stream(
                crate::stream::Recording::from_request(
                    &request,
                    crate::ytdlp::destination_for(&backend.download_dir),
                ),
                backend.proxies.clone(),
                backend.video_events.clone(),
            )
            .await?
            .to_string(),
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
            let mut config = crate::ytdlp::VideoConfig::new(destination);
            // The resolution the user picked in the browser. Without one,
            // yt-dlp is left to choose, which is what every other entry point
            // does.
            config.format = request.format_id.clone();
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
        // The early returns above handle these. Returning an error rather
        // than `unreachable!` keeps the no-panic rule intact even if the
        // guards above are ever refactored away.
        JobKind::Sniff => bail!("a sniff has nothing to queue"),
        JobKind::Formats => bail!("a format listing has nothing to queue"),
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
    Ok(IpcResponse::accepted(id))
}

/// The request headers a page's player would have sent. A manifest is very
/// often refused without them.
fn stream_headers(request: &DownloadRequest) -> crate::stream::Headers {
    crate::stream::Headers {
        referer: request.referer.clone(),
        user_agent: request.user_agent.clone(),
        cookies: request.cookies.clone(),
    }
}

/// Describe everything the browser watched this page load, as picker rows.
///
/// Two kinds arrive together and are told apart by what they are, not by
/// where they came from: a playlist is recorded with ffmpeg, and a plain file
/// is fetched by the downloader, which is far quicker at it and can resume.
async fn stream_listing(request: &DownloadRequest) -> crate::ytdlp::MediaProbe {
    use crate::ytdlp::FormatSource;

    let headers = stream_headers(request);
    let (streams, files) = futures::future::join(
        crate::stream::describe_all(&request.streams, &headers),
        crate::stream::describe_all(&request.files, &headers),
    )
    .await;

    // One row per quality, not per address: a master playlist is a list of
    // qualities, and offering only its best would throw the rest away.
    let mut rows: Vec<crate::ytdlp::MediaFormat> = Vec::new();
    for (source, url, info) in streams
        .into_iter()
        .map(|(url, info)| (FormatSource::Stream, url, info))
        .chain(
            files
                .into_iter()
                .map(|(url, info)| (FormatSource::File, url, info)),
        )
    {
        // A file is downloaded as it is, so it keeps its own extension; only a
        // recording gets to choose a container.
        let ext = match source {
            FormatSource::File => {
                crate::stream::extension_of(&url).unwrap_or_else(|| info.container().to_owned())
            }
            _ => info.container().to_owned(),
        };
        let qualities: Vec<Option<crate::stream::Rendition>> = match source {
            // Every quality of a playlist is separately recordable.
            FormatSource::Stream if !info.renditions.is_empty() => {
                info.renditions.iter().copied().map(Some).collect()
            }
            // A file is one thing however many streams ffprobe found in it.
            _ => vec![info.renditions.first().copied()],
        };
        for rendition in qualities {
            rows.push(crate::ytdlp::MediaFormat {
                id: String::new(),
                label: match &rendition {
                    Some(rendition) => info.label_for(rendition),
                    None => info.label(),
                },
                ext: ext.clone(),
                // Only a file has a size worth showing. ffprobe reports the
                // size of a *playlist* as the playlist's own few hundred
                // bytes, which would put "385 B" beside a two-hour stream.
                size: match source {
                    FormatSource::File => info.size,
                    _ => None,
                },
                estimated: false,
                height: rendition.and_then(|rendition| rendition.height),
                audio_only: !info.has_video(),
                source,
                url: Some(url.clone()),
            });
        }
    }

    // Best first, and only one row per description. A player fetches its
    // master playlist and then the rendition inside it, which come back as
    // two rows saying exactly the same thing.
    rows.sort_by_key(|row| std::cmp::Reverse(row.height));
    let mut seen: Vec<String> = Vec::new();
    rows.retain(|row| {
        if seen.contains(&row.label) {
            return false;
        }
        seen.push(row.label.clone());
        true
    });

    crate::ytdlp::MediaProbe {
        title: request.filename.clone(),
        duration: None,
        formats: rows,
    }
}
