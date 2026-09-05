//! Snatch Native Messaging Host.
//!
//! The browser speaks Chrome's native-messaging wire format on stdin/stdout:
//! a 4-byte little-endian length prefix followed by exactly that many bytes of
//! UTF-8 JSON. This process translates each message into a line-delimited JSON
//! request on the Snatch GUI's Unix domain socket and relays the answer back.
//!
//! It is deliberately tiny and stateless: the browser owns its lifetime, so the
//! only durable state lives in the GUI on the other side of the socket.
//!
//! Both directions are forwarded **losslessly**. An earlier version
//! deserialised the request into a local struct and re-serialised that, which
//! silently dropped any field the host did not know about — when the extension
//! gained a `kind` field for magnets and scrapes, every scrape arrived at the
//! GUI as a plain download of the page's HTML. The reply had the same flaw and
//! the same fix: it is a `serde_json::Value` on the way back too, so when the
//! GUI began answering a format listing with the list, this binary did not
//! need to learn what a format was.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Instant, sleep, timeout};

/// Chrome refuses to send more than 64 MiB; anything larger is a protocol error.
const MAX_INCOMING_BYTES: u32 = 64 * 1024 * 1024;
/// Chrome refuses to *receive* more than 1 MiB. Our replies are a few hundred bytes.
const MAX_OUTGOING_BYTES: usize = 1024 * 1024;

const APP_DIR: &str = "snatch-dl";
const SOCKET_NAME: &str = "snatch.sock";
const GUI_BIN: &str = "snatch-gui";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long we wait for a freshly launched GUI to open its socket.
const GUI_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long we wait for the GUI to answer once connected.
///
/// This has to outlast the slowest question the GUI can be asked, which is
/// listing what a page offers. That walks a chain, and every step of it has
/// its own cap: yt-dlp 30s, then streamlink 20s, then ffprobe 25s on a
/// playlist. Nothing takes all three -- the first answer wins -- but a page
/// where everything refuses does, and 75 seconds of that is a page that is
/// working, not a page that has hung.
///
/// Giving up first is worse than waiting: the reader gets "Snatch is not
/// answering" for a broadcast the app was seconds away from offering to
/// record. Every step is separately bounded, so the GUI cannot hold this
/// open indefinitely no matter what a site does.
const GUI_REPLY_TIMEOUT: Duration = Duration::from_secs(90);

/// The minimum a hand-off must contain. Everything else is passed through
/// untouched, so this host never needs to know the full schema.
const REQUIRED_FIELD: &str = "url";

/// The one thing every reply must carry, in both directions. The extension
/// checks it before reading anything else.
const OK_FIELD: &str = "ok";

/// A reply this host generated itself, because the GUI never got the chance.
fn failure(error: impl Into<String>) -> Value {
    json!({ "ok": false, "error": error.into() })
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("snatch-nmh: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(pump()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // stderr from a native messaging host lands in the browser's log.
            eprintln!("snatch-nmh: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Read messages until the browser closes stdin.
async fn pump() -> Result<()> {
    let socket = socket_path()?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    loop {
        let Some(raw) = read_message(&mut stdin).await? else {
            return Ok(()); // clean EOF: the browser is done with us
        };

        let reply = match dispatch(&socket, &raw).await {
            Ok(reply) => reply,
            Err(error) => {
                eprintln!("snatch-nmh: hand-off failed: {error:#}");
                failure(format!("{error:#}"))
            }
        };

        write_message(&mut stdout, &reply).await?;
    }
}

/// Parse one native message and hand it to the GUI.
async fn dispatch(socket: &Path, raw: &[u8]) -> Result<Value> {
    // A `Value` round-trips every field, known or not.
    let request: Value = serde_json::from_slice(raw).context("payload is not valid JSON")?;

    let Some(object) = request.as_object() else {
        bail!("the payload is not a JSON object");
    };
    match object.get(REQUIRED_FIELD).and_then(Value::as_str) {
        Some(url) if !url.trim().is_empty() => {}
        Some(_) => bail!("request carries an empty url"),
        None => bail!("request has no url"),
    }

    // Compact encoding guarantees a single line, which the socket framing needs.
    let mut line = serde_json::to_string(&request).context("could not re-encode the request")?;
    if line.contains('\n') {
        bail!("the encoded request contains a line break");
    }
    line.push('\n');

    let stream = connect_or_launch_gui(socket).await?;
    let (read_half, mut write_half) = stream.into_split();

    write_half
        .write_all(line.as_bytes())
        .await
        .context("could not write the request to the Snatch GUI")?;
    write_half
        .flush()
        .await
        .context("could not flush the request to the Snatch GUI")?;

    let mut answer = String::new();
    let mut reader = BufReader::new(read_half.take(MAX_OUTGOING_BYTES as u64));
    let read = timeout(GUI_REPLY_TIMEOUT, reader.read_line(&mut answer))
        .await
        .context("the Snatch GUI did not answer in time")?
        .context("could not read the reply from the Snatch GUI")?;

    if read == 0 || answer.trim().is_empty() {
        bail!("the Snatch GUI closed the connection without answering");
    }

    // A `Value` round-trips whatever the GUI chose to include. Only the one
    // field the extension relies on is checked here; everything beside it is
    // carried through untouched.
    let reply: Value =
        serde_json::from_str(answer.trim()).context("the Snatch GUI sent a malformed reply")?;
    match reply.get(OK_FIELD) {
        Some(Value::Bool(_)) => Ok(reply),
        Some(_) => bail!("the Snatch GUI sent a reply whose '{OK_FIELD}' was not true or false"),
        None => bail!("the Snatch GUI sent a reply with no '{OK_FIELD}' field"),
    }
}

/// Connect to the GUI, starting it on demand if it is not running yet.
async fn connect_or_launch_gui(socket: &Path) -> Result<UnixStream> {
    if let Ok(stream) = connect(socket).await {
        return Ok(stream);
    }

    launch_gui().context("the Snatch GUI is not running and could not be started")?;

    let deadline = Instant::now() + GUI_STARTUP_TIMEOUT;
    loop {
        sleep(Duration::from_millis(250)).await;
        match connect(socket).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() >= deadline => {
                return Err(error)
                    .context("the Snatch GUI was started but never opened its socket");
            }
            Err(_) => continue,
        }
    }
}

async fn connect(socket: &Path) -> Result<UnixStream> {
    timeout(CONNECT_TIMEOUT, UnixStream::connect(socket))
        .await
        .with_context(|| format!("timed out connecting to {}", socket.display()))?
        .with_context(|| format!("could not connect to {}", socket.display()))
}

/// Spawn the GUI detached from this process so it survives the browser closing us.
///
/// The spawned child is reaped by a background task. Without that it becomes a
/// zombie the moment the user quits the GUI: a browser holding a long-lived
/// native-messaging Port keeps this host alive for the whole session, and an
/// unreaped child would sit in the process table until the browser closed.
fn launch_gui() -> Result<()> {
    let binary = gui_binary();
    let mut child = tokio::process::Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A new process group so a terminal signal aimed at the browser does
        // not take the download manager with it.
        .process_group(0)
        // Explicitly not kill_on_drop: the GUI must outlive this host.
        .kill_on_drop(false)
        .spawn()
        .with_context(|| format!("could not execute {}", binary.display()))?;

    tokio::spawn(async move {
        match child.wait().await {
            // Nothing to do with the status; the wait itself is the point.
            Ok(_) => {}
            Err(error) => eprintln!("snatch-nmh: could not reap the GUI: {error}"),
        }
    });

    Ok(())
}

/// Prefer a GUI sitting next to this binary, then `$PATH`.
fn gui_binary() -> PathBuf {
    if let Some(override_path) = std::env::var_os("SNATCH_GUI_BIN")
        && !override_path.is_empty()
    {
        return PathBuf::from(override_path);
    }

    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
    {
        let sibling = dir.join(GUI_BIN);
        if sibling.is_file() {
            return sibling;
        }
    }

    PathBuf::from(GUI_BIN)
}

fn socket_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .context("neither $XDG_DATA_HOME nor $HOME is set")?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Ok(base.join(APP_DIR).join(SOCKET_NAME))
}

/// Read one native message. Returns `None` on a clean EOF at a message boundary.
async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; 4];
    if !read_exact_or_eof(reader, &mut prefix).await? {
        return Ok(None);
    }

    let length = u32::from_le_bytes(prefix);
    if length == 0 {
        bail!("the browser announced a zero-length message");
    }
    if length > MAX_INCOMING_BYTES {
        bail!("the browser announced a {length} byte message, which exceeds the 64 MiB limit");
    }

    let mut body = vec![0u8; length as usize];
    reader
        .read_exact(&mut body)
        .await
        .context("the native message body was truncated")?;
    Ok(Some(body))
}

/// Fill `buf` completely. `Ok(false)` means EOF arrived before the first byte.
async fn read_exact_or_eof<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let read = reader
            .read(&mut buf[filled..])
            .await
            .context("could not read from stdin")?;
        if read == 0 {
            if filled == 0 {
                return Ok(false);
            }
            bail!("stdin ended in the middle of the 4-byte length prefix");
        }
        filled += read;
    }
    Ok(true)
}

/// Write one native message: 4-byte little-endian length prefix, then the JSON body.
async fn write_message<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value).context("could not encode the reply")?;
    if body.len() > MAX_OUTGOING_BYTES {
        bail!(
            "reply is {} bytes, which exceeds the 1 MiB browser limit",
            body.len()
        );
    }
    let length = u32::try_from(body.len()).context("reply length does not fit in u32")?;

    writer
        .write_all(&length.to_le_bytes())
        .await
        .context("could not write the reply length prefix")?;
    writer
        .write_all(&body)
        .await
        .context("could not write the reply body")?;
    writer.flush().await.context("could not flush stdout")?;
    Ok(())
}
