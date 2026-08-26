# Snatch

A download manager for Linux that behaves the way IDM users expect, built out
of engines that are already good at their jobs rather than a homegrown
downloader.

GTK4 / libadwaita front-end, a browser extension that captures downloads before
the browser starts them, and four purpose-built engines behind one window.

![Downloads](docs/downloads.png)

---

## What it does

| Job | Engine | Why not something else |
|---|---|---|
| HTTP / FTP downloads | **aria2** (JSON-RPC) | 16 segmented connections, resume, and a battle-tested retry policy. Writing another HTTP downloader would be strictly worse. |
| Torrents and magnets | **librqbit** (in-process) | DHT, peer exchange, uTP and sequential streaming. aria2's BitTorrent support has none of that. |
| Site video | **yt-dlp** | Resolves DASH/HLS manifests, picks formats, muxes audio and video. |
| Image galleries | **gallery-dl** | Hundreds of site-specific extractors, organised output. |
| Conversion / trimming | **ffmpeg** | Post-process without leaving the app. |

Everything runs off the UI thread. GTK owns the widgets, a Tokio runtime owns
every socket and subprocess, and the two meet through a single event channel —
so the window keeps repainting while four engines are working.

---

## Use cases

**Grab a big file without babysitting it.**
Click a download in Firefox or Chrome. The extension cancels the browser's
transfer and hands the URL — with cookies, referer and user agent — to aria2,
which fetches it in 16 parallel segments and resumes if the connection drops.
Files behind a login work because the session cookies travel with the request.

**Stream a torrent while it downloads.**
Add a magnet, open the row and press the sequential button. Snatch drives an
open read head through the file so pieces arrive in playback order, and you can
open it in a player before it finishes. The peer line shows the real swarm —
`7 peers (2 TCP, 5 uTP), 121 connecting, 571 known` — so you can tell a dead
torrent from a slow one.

**Pull a video, keep only the audio.**
`Ctrl+D`, paste the watch page, choose *Audio only (MP3)*. yt-dlp resolves the
formats and ffmpeg extracts the track. Or download the video first and
right-click it → **Extract Audio**.

**Archive an artist's gallery.**
Right-click any page → **Scrape This Page with Snatch**. gallery-dl walks the
site with its own extractor — following pagination and reaching originals
behind thumbnails, which a DOM scrape cannot — and files everything under
`Downloads/Snatch Galleries/<site>/<author>/`. The Scraper tab shows live
counts and thumbnails.

**Route one download through a proxy without proxying everything.**
Add a SOCKS5 or HTTP proxy in **Proxy Settings**, test its latency, and set it
as the default or pin it to a single job. Snatch knows which engines can use
which kind and refuses an impossible pairing instead of silently connecting
direct — see [Proxies](#proxies).

**Trim a clip without re-encoding.**
Right-click a finished video → **Trim…**, give a start and end. Streams are
copied, so it is instant and lossless.

---

## Install

### Dependencies

| Tool | Required | Fedora | Debian / Ubuntu | Arch |
|---|---|---|---|---|
| `aria2` | **yes** | `sudo dnf install aria2` | `sudo apt install aria2` | `sudo pacman -S aria2` |
| `ffmpeg` | for post-processing | `sudo dnf install ffmpeg` | `sudo apt install ffmpeg` | `sudo pacman -S ffmpeg` |
| `yt-dlp` | for video extraction | `sudo dnf install yt-dlp` | `sudo apt install yt-dlp` | `sudo pacman -S yt-dlp` |
| `gallery-dl` | for the scraper | *not packaged* — see below | *not packaged* | `sudo pacman -S gallery-dl` |

Torrents need nothing installed: librqbit is compiled in.

Snatch needs **GTK 4.12+ and libadwaita 1.5+** — Fedora 39+, Ubuntu 24.04+,
Debian 13+, or any rolling distribution. Older releases cannot build it.

gallery-dl is not in most distribution repositories. It moved to
[Codeberg](https://codeberg.org/mikf/gallery-dl) and publishes a standalone
Linux binary, which the installer can fetch and verify for you:

```bash
./install.sh --fetch-gallery-dl
```

That downloads `gallery-dl.bin` from the latest release, checks it against the
published `SHA256SUMS`, and installs it to `~/.local/bin` — no `pip`, no root.

### Build and install

```bash
git clone https://github.com/ayoubzulfiqar/snatch-dl.git
cd snatch-dl
./install.sh
```

Everything is per-user; nothing is written outside `$HOME` and no step needs
`sudo`. The installer:

- builds the workspace in release mode,
- installs `snatch-gui` and `snatch-nmh` to `~/.local/bin`,
- stages the browser extension into `extension-chromium/` and
  `extension-firefox/`,
- registers the native messaging host for every browser it finds,
- installs a desktop entry.

`./install.sh --uninstall` reverses all of it.

### Load the extension

**Chromium / Chrome** — `chrome://extensions` → enable *Developer mode* →
*Load unpacked* → select the **`extension-chromium/` folder**.

The installer generates an RSA key and pins the extension ID, so the ID stays
the same across reloads and the native messaging manifest keeps matching it.

**Firefox** — `about:debugging#/runtime/this-firefox` → *Load Temporary Add-on*
→ select **`extension-firefox/manifest.json`**.

> Firefox clears temporary add-ons on restart. To keep it, sign the extension
> or use Developer Edition with `xpinstall.signatures.required=false`.

The two directories are **not interchangeable**: Manifest V3 in Chromium
requires `background.service_worker` and rejects `background.scripts`, while
Firefox has no service-worker background and needs `background.scripts`. Both
are generated from `extension/manifest.base.json`.

---

## Using it

| Shortcut | Action |
|---|---|
| `Ctrl+N` | Add a download, magnet or gallery (kind is auto-detected) |
| `Ctrl+D` | Extract a video with yt-dlp |
| `Ctrl+P` | Pause everything |
| `Ctrl+,` | Proxy settings |
| `Ctrl+?` | Shortcuts |

From the browser, right-click gives you **Download with Snatch**, **Send Magnet
to Snatch**, **Extract Video with Snatch** and **Scrape This Page with Snatch**.
Clicking the toolbar button pauses and resumes capture.

### Command line

The GUI listens on a Unix socket, so anything can queue a job:

```bash
SOCK=~/.local/share/snatch-dl/snatch.sock

# A direct download
printf '{"url":"https://example.com/file.iso"}\n' | nc -U "$SOCK"

# A magnet (kind is inferred from the scheme)
printf '{"url":"magnet:?xt=urn:btih:..."}\n' | nc -U "$SOCK"

# A video, explicitly
printf '{"kind":"video","url":"https://example.com/watch?v=..."}\n' | nc -U "$SOCK"

# A gallery
printf '{"kind":"scrape","url":"https://example.com/user/gallery"}\n' | nc -U "$SOCK"
```

The reply is one line of JSON: `{"ok":true,"gid":"..."}` or
`{"ok":false,"error":"..."}`. Snatch starts automatically if it is not running.

---

## Proxies

The four engines do **not** agree on what a proxy is, and Snatch does not
pretend otherwise:

| Engine | HTTP proxy | SOCKS5 |
|---|---|---|
| aria2 (downloads) | yes | **no** — aria2 has no SOCKS support at all |
| librqbit (torrents) | **no** | yes |
| yt-dlp / gallery-dl | yes | yes |
| Snatch's own requests | yes | yes |

Assigning a SOCKS5 proxy to an aria2 download is refused with an explanation
rather than silently falling back to a direct connection — a leaked direct
connection is the one failure a proxy user cannot tolerate.

Two consequences worth knowing:

- The torrent session fixes its proxy when it starts, so changing it takes
  effect on the next launch.
- When a SOCKS5 proxy is configured, the inbound peer listener is **disabled**,
  because a public listener would advertise your real address past the proxy.
  You will connect out but not receive incoming peers.

---

## Where things live

```
~/.local/bin/snatch-gui                     the application
~/.local/bin/snatch-nmh                     native messaging host
~/.local/share/snatch-dl/snatch.sock        IPC socket (0600)
~/.local/share/snatch-dl/snatch.sqlite      job history
~/.local/share/snatch-dl/aria2.session      resumable queue
~/.local/share/snatch-dl/torrents/          torrent resume data
~/.local/share/snatch-dl/proxies.json       proxy table
~/.local/share/snatch-dl/chromium-extension-key.pem   pins the extension ID
```

Downloads go to your XDG download directory; galleries to
`Snatch Galleries/<site>/` and videos to `Snatch Video/` beneath it.

---

## Architecture

```
  browser extension  ──native messaging──▶  snatch-nmh
                                                 │  JSON line over a Unix socket
                                                 ▼
                     ┌──────────────────  snatch-gui  ──────────────────┐
                     │                                                   │
   GLib main loop ───┤  UI: ViewStack of Downloads / Torrents / Scraper  │
   (widgets only)    │                        ▲                          │
                     │                 UiEvent channel                   │
   Tokio runtime ────┤                        │                          │
   (all I/O)         │  aria2 RPC · librqbit · yt-dlp · gallery-dl ·      │
                     │  ffmpeg · SQLite · proxy router                    │
                     └───────────────────────────────────────────────────┘
```

`snatch-nmh` forwards the browser's payload **losslessly** — it parses to a
generic JSON value rather than a typed struct, so a field the host does not
know about still reaches the GUI. An earlier version round-tripped through a
struct and silently ate the `kind` field, turning every scrape into a plain
download of the page's HTML.

| Module | Responsibility |
|---|---|
| `aria2.rs` | Spawns and supervises `aria2c`, JSON-RPC client |
| `torrent.rs` | librqbit session, magnets, sequential streaming |
| `ytdlp.rs` | yt-dlp subprocess, progress-template parsing |
| `gallery.rs` | gallery-dl subprocess, two-stream output merge |
| `processor.rs` | ffmpeg jobs and the serial encode queue |
| `network.rs` | Proxy table, engine matrix, latency probes |
| `db.rs` | SQLite history (WAL), crash reconciliation |
| `ipc.rs` | Unix socket server, job routing |
| `ui/` | One module per page |

### Notes for anyone touching the parsers

Three things in here are counter-intuitive and are all covered by tests:

- **ffmpeg's `out_time_ms` is microseconds, not milliseconds.** Reading it as
  milliseconds puts every progress bar at 100% instantly.
- **gallery-dl splits its output across both streams.** stdout carries file
  paths (`# ` prefix means "already had it"); only stderr carries the `[3/12]`
  counter that gives you a batch total.
- **librqbit has no "sequential" switch.** An open `FileStream` prioritises a
  32 MiB window ahead of its read position, so sequential mode is a pump that
  keeps advancing that position.

---

## Development

```bash
cargo build --release
cargo test --workspace       # 54 tests
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

The test suite runs without a display and without a network. Tests that need a
real binary (the ffmpeg end-to-end encode) skip themselves when it is absent.

Parser tests use literal output captured from the real tools — aria2 1.37.0,
ffmpeg 8.1.2, gallery-dl 1.32.9, yt-dlp 2026.08.19 — rather than invented
fixtures, because every one of those formats has a quirk that invented
fixtures would miss.

---

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

Snatch drives aria2, ffmpeg, yt-dlp and gallery-dl as separate programs and
links librqbit as a library; each carries its own license.
