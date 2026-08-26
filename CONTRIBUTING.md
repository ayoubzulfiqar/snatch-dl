# Contributing

## Getting set up

```bash
sudo dnf install aria2 ffmpeg yt-dlp gtk4-devel libadwaita-devel   # Fedora
# sudo apt install aria2 ffmpeg yt-dlp libgtk-4-dev libadwaita-1-dev  # Debian
./install.sh --fetch-gallery-dl
cargo build --release
```

## Before opening a pull request

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --check extension/background.js
bash -n install.sh
```

CI runs exactly these. `clippy` is `-D warnings`, so a new warning fails the
build.

## House rules

**No `.unwrap()` or `.expect()` in a production path.** Return `Result` and add
context with `anyhow::Context`. `unwrap_or`, `unwrap_or_else` and
`unwrap_or_default` are fine — they cannot panic. The test modules may use
`expect` with a message that reads as a sentence.

**Never block the GTK main loop.** Widget code runs on the GLib main loop;
every socket, subprocess and HTTP call belongs to the Tokio runtime. Cross the
boundary with `Backend::offload` inside `glib::spawn_future_local`, never with
`block_on`.

**Parsers are written against captured output, not guesses.** If you add or
change one, run the real tool, capture its output, and put those literal lines
in the test as fixtures. Three formats in this codebase have quirks that
invented fixtures would miss:

- ffmpeg's `out_time_ms` is microseconds despite the name.
- gallery-dl puts file paths on stdout and its `[n/m]` counter on stderr.
- yt-dlp emits the literal string `NA` for any unavailable field.

**Subprocesses must drain both pipes concurrently.** Reading stdout to
completion before touching stderr deadlocks as soon as the stderr pipe buffer
fills. Use `tokio::select!` over both, as the existing engines do.

**Anything reaching an engine from a web page is untrusted.** URLs are checked
against a scheme allowlist, filenames are reduced to a basename, and header
values have control characters stripped. Keep it that way.

## Layout

| Path | What lives there |
|---|---|
| `snatch-gui/src/*.rs` | One module per engine |
| `snatch-gui/src/ui/` | One module per page, plus shared helpers |
| `snatch-nmh/` | Native messaging host — forwards payloads losslessly |
| `extension/` | WebExtension **source**; the staged dirs are generated |
| `packaging/` | PKGBUILD and the desktop entry used by distro packages |

`extension-chromium/` and `extension-firefox/` are build output. Edit
`extension/` and re-run `./install.sh`.
