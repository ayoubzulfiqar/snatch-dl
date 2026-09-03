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
    //
    // yt-dlp failing is not the end of the answer, it is the start of the
    // second half of it. It knows a few thousand sites; the web has rather
    // more, and the ones it does not know still have a player that fetched
    // something ffmpeg can read. So a failure here is logged and worked
    // around, never reported, and the reader only ever sees an error when
    // there was genuinely nothing on the page to offer.
    if kind == JobKind::Formats {
        let headers = stream_headers(&request);
        let refused =
            match crate::ytdlp::probe(&request.url, &headers, backend.proxies.as_ref()).await {
                Ok(probe) if !probe.formats.is_empty() => {
                    log::info!("listed {} formats for {name}", probe.formats.len());
                    return Ok(IpcResponse::listing(probe));
                }
                Ok(_) => anyhow!("yt-dlp found nothing downloadable on that page"),
                Err(error) => error,
            };

        log::info!("yt-dlp could not read {name} ({refused:#}); trying what the page loaded");
        let probe = stream_listing(&request).await;
        if !probe.formats.is_empty() {
            log::info!(
                "found {} playable address(es) on {name}",
                probe.formats.len()
            );
            return Ok(IpcResponse::listing(probe));
        }
        return Err(refused);
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

    // Said plainly, and said early: the add-on and the app are installed
    // separately, so one can be newer than the other and the reader has no way
    // to know which. This is the only answer that helps.
    if kind == JobKind::Unknown {
        bail!(
            "this copy of Snatch is older than the browser add-on and does not know \
             how to do that yet; update Snatch"
        );
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
            // The request the page's player made. A members-only video, a
            // signed CDN or a site that checks `Origin` refuses anything else.
            config.headers = stream_headers(&request);
            // What to fall back to if yt-dlp cannot do it after all. The probe
            // succeeded or the user would not be looking at a format list, but
            // extraction happens later and a site can refuse in between.
            config.fallbacks = request
                .files
                .iter()
                .chain(request.streams.iter())
                .cloned()
                .collect();
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
        JobKind::Unknown => bail!("this version of Snatch does not know that request"),
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

/// Whether an address is a playlist, which only ffmpeg can follow.
///
/// A scheme ffmpeg alone speaks -- `rtmp:`, `rtsp:`, `srt:` -- counts too:
/// there is no file at the other end of one of those to download.
fn is_playlist(url: &str) -> bool {
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(scheme.as_str(), "http" | "https") {
        return true;
    }
    matches!(
        crate::stream::extension_of(url).as_deref(),
        Some("m3u8" | "m3u" | "mpd")
    )
}

/// The request headers a page's player would have sent. A manifest is very
/// often refused without them.
fn stream_headers(request: &DownloadRequest) -> crate::stream::Headers {
    crate::stream::Headers {
        referer: request.referer.clone(),
        user_agent: request.user_agent.clone(),
        cookies: request.cookies.clone(),
        extra: request.extra_headers(),
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

    // Nothing was watched loading. The address itself is still worth opening:
    // plenty of pages yt-dlp has never heard of *are* the media -- a bare
    // `.mp4`, an `.m3u8` pasted into the bar, a camera's RTSP address -- and
    // ffprobe answers that question definitively by opening it. A page that
    // is only a page fails here in a second and costs nothing.
    //
    // Which of the two lists it belongs in is decided by what it is: a
    // playlist has to be recorded, and anything else is a file the downloader
    // fetches in sixteen pieces and can resume. Sending an `.mp4` to ffmpeg
    // instead would work and be several times slower.
    let fallback;
    let (watched_streams, watched_files) = if request.streams.is_empty() && request.files.is_empty()
    {
        fallback = vec![request.url.clone()];
        if is_playlist(&request.url) {
            (&fallback[..], &[][..])
        } else {
            (&[][..], &fallback[..])
        }
    } else {
        (&request.streams[..], &request.files[..])
    };

    let (streams, files) = futures::future::join(
        crate::stream::describe_all(watched_streams, &headers),
        crate::stream::describe_all(watched_files, &headers),
    )
    .await;

    // One row per quality, not per address: a master playlist is a list of
    // qualities, and offering only its best would throw the rest away.
    let mut rows: Vec<crate::ytdlp::MediaFormat> = Vec::new();
    let mut live = false;
    for (source, url, info) in streams
        .into_iter()
        .map(|(url, info)| (FormatSource::Stream, url, info))
        .chain(
            files
                .into_iter()
                .map(|(url, info)| (FormatSource::File, url, info)),
        )
    {
        // Only a playlist can be live. A file has already finished being
        // whatever it is, however little ffprobe could measure about it.
        live = live || (source == FormatSource::Stream && info.is_live());
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
                label: match (&rendition, source) {
                    // A file is never live, however little ffprobe could
                    // measure about it.
                    (Some(rendition), FormatSource::File) => info.file_label_for(rendition),
                    (Some(rendition), _) => info.label_for(rendition),
                    (None, _) => info.label(),
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
    //
    // Kept per source, not per label: a plain file and a playlist of the same
    // programme describe themselves identically, and they are different
    // answers -- one is recorded and one is fetched in sixteen pieces.
    rows.sort_by_key(|row| std::cmp::Reverse(row.height));
    let mut seen: Vec<(String, crate::ytdlp::FormatSource)> = Vec::new();
    rows.retain(|row| {
        let key = (row.label.clone(), row.source);
        if seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });

    crate::ytdlp::MediaProbe {
        title: request.filename.clone(),
        duration: None,
        live,
        formats: rows,
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use crate::ytdlp::FormatSource;

    #[test]
    fn a_playlist_is_recorded_and_a_file_is_downloaded() {
        // Only ffmpeg can follow a playlist, and only it speaks these
        // schemes; everything else is a file the downloader fetches in
        // sixteen pieces and can resume, which is several times quicker.
        assert!(is_playlist("https://c.example/master.m3u8"));
        assert!(is_playlist("https://c.example/stream.mpd?token=abc"));
        assert!(is_playlist("rtmp://live.example/app/key"));
        assert!(is_playlist("rtsp://camera.local/stream1"));
        assert!(is_playlist("srt://live.example:9000"));

        assert!(!is_playlist("https://c.example/video.mp4"));
        assert!(!is_playlist("https://c.example/video.mp4?sig=xyz"));
        // No extension at all is a file until something says otherwise: a
        // signed CDN URL ending in a token is the common shape, and ffprobe
        // is what settles it.
        assert!(!is_playlist("https://c.example/media/9f8a7b6c"));
    }

    /// The whole point of the fallback, end to end.
    ///
    /// yt-dlp reports no codecs at all for a bare media URL -- checked
    /// against 2026.08.19 -- so a page that is simply an `.mp4` produces an
    /// empty listing and, before this, an error message. Opening the address
    /// answers the question properly, and the reader gets a row to click.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_page_yt_dlp_cannot_read_is_still_offered() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let dir = std::env::temp_dir().join(format!("snatch-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let clip = dir.join("clip.mp4");
        let built = tokio::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10:duration=1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                "-y",
            ])
            .arg(&clip)
            .output()
            .await;
        match built {
            Ok(output) if output.status.success() => {}
            _ => {
                eprintln!("skipping: ffmpeg is not available");
                std::fs::remove_dir_all(&dir).ok();
                return;
            }
        }

        let body = std::fs::read(&clip).expect("read the clip");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut scratch = [0u8; 4096];
                    let _ = socket.read(&mut scratch).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\n\
                         Content-Length: {}\r\nAccept-Ranges: bytes\r\n\
                         Connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        // Nothing observed: no manifests, no files. Exactly what arrives from
        // a page whose video was already in the browser's cache.
        let request = DownloadRequest::from_url(format!("http://127.0.0.1:{port}/clip.mp4"));
        assert!(request.streams.is_empty() && request.files.is_empty());

        let probe = stream_listing(&request).await;
        assert_eq!(probe.formats.len(), 1, "{:?}", probe.formats);
        let row = &probe.formats[0];
        // Fetched by the downloader, not recorded: it is a finished file.
        assert_eq!(row.source, FormatSource::File);
        assert_eq!(row.height, Some(240));
        assert_eq!(row.ext, "mp4");
        assert!(row.size.is_some(), "a file knows how big it is");
        assert!(!probe.live);

        std::fs::remove_dir_all(&dir).ok();
    }
}
