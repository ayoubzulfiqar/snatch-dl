<div align="center">

<img src="docs/icon.png" width="128" alt="Snatch">

# Snatch

</div>

A download manager for Linux that behaves the way IDM users expect, built out
of engines that are already good at their jobs rather than a homegrown
downloader.

GTK4 / libadwaita front-end, a browser extension that captures downloads before
the browser starts them, and four purpose-built engines behind one window.

![Downloads](docs/downloads.png)

Point it at a page and pick what you want:

![Media sniffer](docs/sniffer.png)

Everything that finished, where it went, and what to do with it:

![History](docs/history.png)

Every engine option is configurable:

![Settings](docs/settings.png)

---

## What it does

| Job | Engine | Why not something else |
|---|---|---|
| HTTP / FTP downloads | **aria2** (JSON-RPC) | 16 segmented connections, resume, and a battle-tested retry policy. Writing another HTTP downloader would be strictly worse. |
| Torrents and magnets | **librqbit** (in-process) | DHT, peer exchange, uTP and sequential streaming. aria2's BitTorrent support has none of that. |
| Site video | **yt-dlp** | Resolves DASH/HLS manifests, picks formats, muxes audio and video. |
| Image galleries | **gallery-dl** | Hundreds of site-specific extractors, organised output. |
| Conversion / trimming | **ffmpeg** | Post-process without leaving the app. |
| Finding media on a page | built-in **sniffer** | Reads the DOM *and* asks yt-dlp, then lets you pick. |
| A second HTTP engine | **Wget2** | Also multithreaded; some servers prefer its request pattern. |
| Downloads behind a login | **cURL import** | Paste "Copy as cURL" and the cookies, referer and user agent come with it. |
| Keeping track | **history** | What finished, where it went, and the file itself. |
| Downloading overnight | **scheduler** | A daily window, then suspend or shut down when done. |

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

**Point Snatch at a page and take what you want.**
`Ctrl+F`, paste a URL — or right-click the page → **Find All Media on This
Page**. Snatch fetches the document, walks it for images, video, audio,
documents, archives and subtitles (including `srcset` candidates, lazy-loaded
`data-src`, Open Graph metadata and CSS backgrounds), *and* asks yt-dlp in case
the real media is behind a DASH/HLS manifest that appears nowhere in the HTML.
Each link is probed with a `HEAD` for its true type and size, so a download
link with no extension is still classified correctly. Results are grouped by
kind with per-group select-all; tick what you want and it queues.

**Download something that needs your login.**
In the browser's network inspector, right-click the request → **Copy as cURL**,
then paste the whole thing into Snatch's Add box. The URL, cookies, referer and
user agent are read straight out of it, so a file behind a session works
without you reconstructing any of that. Chrome, Firefox and the Windows `cmd`
dialects all parse.

**Pull one file from several mirrors.**
Paste several URLs, tick *Treat multiple URLs as mirrors of one file*, and
aria2 spreads its connections across all of them and fails over automatically.
Untick it and each line becomes its own download instead.

**Find something you downloaded last week.**
The History page lists what finished, how big it was, when, and the folder it
went into. Each row opens that folder, deletes the file, or downloads it again.
Turn on selection mode to act on many at once — and *Remove* and *Delete Files*
are deliberately separate buttons, because forgetting a record and erasing a
file are not the same thing.

**Stop rummaging through one enormous Downloads folder.**
Files are sorted as they arrive into `Video`, `Music`, `Images`, `Documents`,
`Compressed` and `Programs` beneath your download folder. Anything Snatch does
not recognise is left in the root rather than filed under a guess. Turn it off
in Settings if you would rather have one flat folder.

**Download overnight and go to bed.**
Set a window in Settings — say 01:00 to 08:00, which wraps past midnight —
and Snatch pauses everything outside it. Pick *Suspend* or *Shut the computer
down* for when the queue empties; you get a minute to cancel, and it goes
through logind so it needs no root.

**Reorder the queue.**
Waiting downloads carry up and down buttons, and the row menu can send one
straight to the top or bottom. Active downloads have no position to change, so
the buttons are hidden rather than doing nothing.

**Send one download through a different proxy.**
The row menu has *Route Through a Proxy…*. A SOCKS entry is labelled as
unusable for downloads, because aria2 cannot use one.

**Let it catch links you copy.**
Turn on clipboard watching and copying a file link anywhere offers it as a
toast with a Download button — a toast, not a dialog, so it never steals focus.
Only links that name a file are offered; an ordinary page link is not.

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

### Let Snatch install the dependencies

```bash
./install.sh --with-deps
```

That installs `aria2` and `ffmpeg` through your package manager (you will be
prompted for sudo, by your package manager, not by Snatch), and fetches the
standalone `yt-dlp` and `gallery-dl` binaries from their official releases —
each verified against the project's published SHA-256 sums before it is made
executable. No `pip`, and no root for the standalone pair.

You can also do it later from inside the app: **Menu → Dependencies…** lists
every tool, what breaks without it, and offers an **Install** button for the
two that need no root. For `aria2` and `ffmpeg` it shows the exact command for
your distribution with a copy button — Snatch never runs `sudo` itself, because
a download manager that asks for your password is one you should not trust.

Self-installed tools go to `~/.local/share/snatch-dl/bin`, not `~/.local/bin`,
so uninstalling Snatch cannot remove a tool you rely on elsewhere. Snatch puts
that directory first on its own `PATH` at startup.

gallery-dl in particular is not in most distribution repositories — it moved to
[Codeberg](https://codeberg.org/mikf/gallery-dl) and ships a standalone Linux
binary, which is what `--with-deps` and `--fetch-gallery-dl` retrieve.

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
- generates the Firefox build of the extension into `extension-firefox/`,
- registers the native messaging host for every browser it finds,
- installs a desktop entry.

`./install.sh --uninstall` reverses all of it.

### Load the extension

**Chromium / Chrome** — `chrome://extensions` → enable *Developer mode* →
*Load unpacked* → select the **`extension/` folder itself** (the folder, not a
file inside it).

`extension/` is the Chromium extension as committed, so this works from a bare
clone with no build step. Its manifest pins a public key, which fixes the ID at
`nlajonamjkdakodfojdlhbhlbcamjkik` — the same ID the installer writes into the
native messaging manifest, so the two keep matching across reloads.

**Firefox** — `about:debugging#/runtime/this-firefox` → *Load Temporary Add-on*
→ select **`extension-firefox/manifest.json`**, which `./install.sh` generates.

> Firefox clears temporary add-ons on restart. To keep it, sign the extension
> or use Developer Edition with `xpinstall.signatures.required=false`.

The two are **not interchangeable**, which is why Firefox needs a generated
copy at all: Manifest V3 in Chromium accepts only `background.service_worker`
and rejects `background.scripts`, while Firefox has no service-worker
background and needs `background.scripts`. The installer builds
`extension-firefox/` by swapping those members and dropping the Chromium key,
keeping everything else byte-identical to `extension/manifest.json`.

---

## Using it

| Shortcut | Action |
|---|---|
| `Ctrl+N` | Add a download, magnet or gallery (kind is auto-detected) |
| `Ctrl+D` | Extract a video with yt-dlp |
| `Ctrl+F` | Find all media on a page |
| `Ctrl+H` | History |
| `Ctrl+P` | Pause everything |
| `Ctrl+,` | Settings |
| `F9` | Show or hide the sidebar |
| `Ctrl+?` | Shortcuts |

From the browser, right-click gives you **Download with Snatch**, **Send Magnet
to Snatch**, **Extract Video with Snatch**, **Find All Media on This Page** and
**Scrape This Page with Snatch**.
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

# Open the media picker for a page
printf '{"kind":"sniff","url":"https://example.com/article"}\n' | nc -U "$SOCK"
```

The reply is one line of JSON: `{"ok":true,"gid":"..."}` or
`{"ok":false,"error":"..."}`. Snatch starts automatically if it is not running.

---

## Settings

Snatch is navigated from a sidebar drawer: **Downloads**, **Torrents**,
**Scraper**, **History** and **Settings**, each a real page rather than a dialog. Live
counts appear next to a destination, so activity on a page you are not looking
at is still visible.

The drawer is toggled with the button in the header or **F9**, and it stays
however you left it — it is never closed for you. On a narrow window it floats
over the content instead of pushing it aside, and picking a destination there
gets it out of the way. Snatch also reopens on whichever page you left it.

The download folder is set here too, with a folder chooser; leave it blank to
use your XDG Downloads directory.

Settings covers what the underlying engines expose — segments per download,
connections per server, minimum split size, simultaneous downloads, overall
and per-download speed caps, retries, disk allocation, TLS verification, user
agent, torrent upload cap and peer limit, DHT, audio bitrate, subtitles and
gallery behaviour.

Each row says **when** it takes effect, because the engines differ:

| When | Settings |
|---|---|
| Immediately | Simultaneous downloads, overall speed caps, torrent upload cap, schedule, clipboard watching |
| Next download | Segments, connections per server, per-download cap, engine choice |
| After a restart | Disk allocation, retries, TLS verification, resume-data writing, DHT, download folder |

Apply names the specific settings waiting on a restart rather than showing a
generic warning.

### Those `.aria2` files

While a download runs, aria2 writes a `something.aria2` control file beside
it. That file is what lets a download resume after a crash, a network drop or
a power cut, and aria2 deletes it the moment the download completes — if one
is left behind after a finished download, that is a bug, not by design.

If you would rather not see them at all, **Settings → Write resume data while
downloading** turns them off. You lose crash resume in exchange.

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
~/.local/share/snatch-dl/snatch.sqlite      download and job history
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
| `sniff.rs` | Page fetch, DOM walk, extractor pass, HEAD probing |
| `wget.rs` | Wget2 engine, progress measured from the file on disk |
| `settings.rs` | Persisted configuration and where each value applies |
| `curl.rs` | Parsing a browser's "Copy as cURL" into a request |
| `ui/history.rs` | Finished downloads, multi-select, folder and file actions |
| `ui/graph.rs` | The bandwidth sparkline, drawn with Cairo |
| `deps.rs` | Tool discovery and verified self-installation |
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
- **A `HEAD` response has no body**, so `reqwest::Response::content_length()`
  reports 0. The sniffer reads the `Content-Length` header directly; using the
  convenience method silently loses every size.
- **Wget2's progress bar is unparseable** — ANSI cursor-save, cursor-up and
  erase sequences interleaved mid-line. The wget engine measures progress by
  `stat`-ing the output file instead, with the total from a `HEAD`.
- **aria2 rejects the entire `changeGlobalOption` call** if any key in it is
  not globally changeable, so that list stays deliberately short.

---

## Development

```bash
cargo build --release
cargo test --workspace       # 105 tests
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
