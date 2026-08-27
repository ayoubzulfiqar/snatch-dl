# Contributing

Thanks for wanting to help. Here is how.

## Set up

Install what you need to build:

```bash
# Fedora
sudo dnf install aria2 ffmpeg yt-dlp gtk4-devel libadwaita-devel p7zip wget2

# Debian or Ubuntu
sudo apt install aria2 ffmpeg yt-dlp libgtk-4-dev libadwaita-1-dev p7zip-full wget2
```

Then build it:

```bash
git clone https://github.com/ayoubzulfiqar/snatch-dl.git
cd snatch-dl
cargo build --release
```

To install your build for yourself, run `./install.sh`.

## Before you open a pull request

Run these four. CI runs the same ones.

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --check extension/background.js
```

Clippy fails on any warning. So a new warning breaks the build.

CI uses the newest stable Rust. If yours is older, clippy can pass for you and
fail there. Run `rustup update stable` first.

## The rules

These are not style preferences. Each one is here because breaking it caused a
real bug.

### Never use `.unwrap()` in real code

Also no `.expect()`, no `panic!`, no `unreachable!`.

Return a `Result` instead. Add a note with `anyhow::Context` so the error says
what went wrong.

`unwrap_or`, `unwrap_or_else` and `unwrap_or_default` are fine. They cannot
crash.

Tests may use `.expect()`. Write the message as a sentence.

### Never block the window

Widgets live on the GTK main loop. Every socket, subprocess and web request
lives on the Tokio runtime.

To cross between them, use `Backend::offload` inside
`glib::spawn_future_local`. Never use `block_on`.

Block the main loop and the window freezes.

### Write parsers against real output

Do not guess what a tool prints. Run it. Copy what it printed. Put those exact
lines in the test.

Five things in here look wrong but are not:

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

### Read both pipes at once

When you run a subprocess, read stdout and stderr at the same time.

Read one to the end first and the program hangs. The other pipe fills up and
the program stops waiting for space.

Use `tokio::select!` over both, or one reader task each. The existing engines
show how.

### Trust nothing from a web page

A URL from a browser is not safe. Check it against the list of allowed
schemes. Cut filenames down to the last part. Strip control characters out of
headers.

Keep it that way.

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

## Found a security hole?

Do not open an issue. Read [SECURITY.md](SECURITY.md).
