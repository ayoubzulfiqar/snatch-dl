# Contributing

Thanks for wanting to help.

## Read this first

**Snatch does not take pull requests.** It has one maintainer. Pull requests
are closed unread, so please do not spend your time on one.

**Open an issue instead.** That is the way in, and it works well:

- **Found a bug?** [Report it.](https://github.com/ayoubzulfiqar/snatch-dl/issues/new/choose)
  Say what happened and how to make it happen again. That is usually enough.
- **Want a feature?** [Ask for it.](https://github.com/ayoubzulfiqar/snatch-dl/issues/new/choose)
  Describe the problem you are stuck on, not just the fix you had in mind.
- **Already fixed it?** Open an issue and paste your fix into it. It will get
  used, and you will be credited.

**Found a security hole?** Do not open an issue. Read [SECURITY.md](SECURITY.md).

## A good bug report

The forms ask for these. Here is why each one matters:

| We ask | Why |
|---|---|
| The steps | We cannot fix what we cannot repeat. |
| Your Snatch version | The bug may already be gone. |
| Your Linux | Some bugs only happen on one. |
| What it printed | Run `snatch-gui` in a terminal and copy the output. |

## Want to fork it?

Ask first. Snatch is source-available, not open source. You may read it,
build it, run it and change your own copy freely. Publishing a fork needs
written permission.

Email [contact@ayoubzulfiqar.com](mailto:contact@ayoubzulfiqar.com) and say what
you want to do. The answer is usually yes. Asking costs you one email.

If you get permission, you must credit the author where people will see it,
and pick your own name for it. The details are in [LICENSE](LICENSE).

## Building it yourself

Install what you need:

```bash
# Fedora
sudo dnf install aria2 ffmpeg yt-dlp gtk4-devel libadwaita-devel p7zip wget2

# Debian or Ubuntu
sudo apt install aria2 ffmpeg yt-dlp libgtk-4-dev libadwaita-1-dev p7zip-full wget2
```

Then build:

```bash
git clone https://github.com/ayoubzulfiqar/snatch-dl.git
cd snatch-dl
cargo build --release
```

Run `./install.sh` to install your build for yourself.

Check your work with the same four commands CI runs:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --check extension/background.js extension/content.js
```

Clippy fails on any warning. CI uses the newest stable Rust, so run
`rustup update stable` if yours is old.

## How the code works

If you are reading the source, these are the things that will confuse you.
Each one is here because getting it wrong caused a real bug.

### Never use `.unwrap()` in real code

Also no `.expect()`, no `panic!`, no `unreachable!`.

Return a `Result`. Add a note with `anyhow::Context` so the error says what
went wrong.

`unwrap_or`, `unwrap_or_else` and `unwrap_or_default` are fine. They cannot
crash. Tests may use `.expect()`.

### Never block the window

Widgets live on the GTK main loop. Every socket, subprocess and web request
lives on the Tokio runtime.

Cross between them with `Backend::offload` inside `glib::spawn_future_local`.
Never use `block_on`. Block the main loop and the window freezes.

### Parsers are written against real output

Nobody guesses what a tool prints. You run it, copy what it printed, and put
those exact lines in the test.

Five things look wrong but are not:

- ffmpeg's `out_time_ms` is **microseconds**, not milliseconds. Read it wrong
  and every progress bar jumps to 100%.
- gallery-dl splits its output. File paths go to stdout. The `[3/12]` counter
  goes to stderr.
- yt-dlp prints the word `NA` for any field it does not have.
- A `HEAD` reply has no body, so `content_length()` returns 0. Read the
  `Content-Length` header yourself.
- **Wget2 prints everything to stdout and nothing to stderr. Classic wget does
  the opposite.** Read both. Reading one meant every failed download reported
  "no output" instead of the reason.

### Subprocesses read both pipes at once

Read stdout to the end before touching stderr and the program hangs. The other
pipe fills up and the program stops, waiting for space.

Use `tokio::select!` over both, or one reader task each.

### The button on a video is a hit test, not a search

Nothing scans the page for `<video>`. Every serious player buries its video
under a stack of overlays, so `event.target` is never the video and
`closest("video")` never matches.

`document.elementsFromPoint` looks through the whole stack instead. It costs
nothing on a page with no video, and needs no rescan when a single-page site
swaps its player out.

Those listeners are on `document` and use **capture**. Players call
`stopPropagation` freely, and a bubbling listener never runs.

The overlay is drawn in a closed shadow root. Page CSS is hostile — `* {
position: static !important }` is a real thing people write.

### Nothing from a web page is trusted

URLs are checked against a list of allowed schemes. Filenames are cut to the
last part, so `../../etc/passwd` cannot escape. Control characters are stripped
out of headers.

## Where things are

| Path | What is in it |
|---|---|
| `snatch-gui/src/*.rs` | One file per engine |
| `snatch-gui/src/ui/` | One file per page |
| `snatch-nmh/` | The bridge between the browser and the app |
| `extension/` | The browser add-on. This is the source. |
| `packaging/` | Files the Linux packages need |
| `get.sh` | The one-line installer |

`extension-firefox/` is built for you. Do not edit it. Edit `extension/` and
run `./install.sh`.

## Be kind

Read [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
