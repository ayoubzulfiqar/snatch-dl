//! Capturing the video a page assembles in JavaScript.
//!
//! The page-side half of this lives in `mse-hook.js`, injected into every
//! tab. It wraps the `MediaSource` calls a player uses to feed video it has
//! fetched -- and often decrypted or stitched together -- inside the page,
//! where the network never carries a whole file. The hook copies each
//! appended chunk out as base64 through a script message; this is what
//! receives them.
//!
//! Bytes are written straight to a temp file per track as they arrive, never
//! held in memory: a live capture can outlast any amount of it. When the
//! reader saves, the tracks -- video in one file, audio in another, which is
//! how a player usually feeds them -- are muxed together with ffmpeg into one
//! file. A capture that is never saved is thrown away when its tab goes.
//!
//! This does not defeat DRM. Encrypted Media Extensions decrypt downstream of
//! `appendBuffer`, so the bytes seen here are ciphertext. It is for the far
//! commoner player that assembles clear media itself.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;
use tokio::process::Command;

/// One message from the page hook. The `t` field says which.
#[derive(Debug, Deserialize)]
#[serde(tag = "t")]
enum Message {
    /// The hook installed itself.
    #[serde(rename = "ready")]
    Ready,
    /// A MediaSource opened.
    #[serde(rename = "open")]
    Open { ms: i64 },
    /// A track was added to a MediaSource, with its MIME type.
    #[serde(rename = "track")]
    Track { ms: i64, sb: i64, mime: String },
    /// A chunk of a track, once capture is armed.
    #[serde(rename = "data")]
    Data { sb: i64, b64: String },
    /// A track grew while unarmed -- how much is held, for the counter.
    #[serde(rename = "grow")]
    Grow { ms: i64, bytes: u64 },
    /// A MediaSource ended.
    #[serde(rename = "end")]
    End { ms: i64 },
}

/// What a received message means for the page's UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// A capturable stream appeared or grew; total bytes seen for it so far.
    Present { ms: i64, bytes: u64 },
    /// The stream signalled its own end.
    Ended { ms: i64 },
    /// Nothing the UI needs to act on.
    Quiet,
}

/// One track being written to disk.
struct Track {
    /// Opened on the first chunk, so a track that is announced but never fed
    /// leaves no empty file behind.
    file: Option<File>,
    path: PathBuf,
    mime: String,
    bytes: u64,
}

/// One MediaSource: its tracks, and whether the reader has armed it.
struct Source {
    tracks: Vec<i64>,
    armed: bool,
    /// Bytes the hook is holding for it before it is armed.
    held: u64,
    /// While paused, chunks that arrive are dropped rather than written, so the
    /// saved file skips whatever played meanwhile -- the same as pausing a
    /// recording. The page goes on sending; only this side stops keeping.
    paused: bool,
}

/// Every in-page capture for one tab.
///
/// Keyed by the ids the hook assigns, which restart at one per page -- so a
/// capture belongs to the tab whose message handler owns this, and ids never
/// cross between tabs because each tab has an engine of its own.
pub struct MseCapture {
    dir: PathBuf,
    sources: BTreeMap<i64, Source>,
    tracks: BTreeMap<i64, Track>,
}

impl MseCapture {
    /// A capture area for one tab. The directory is created on first write.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            sources: BTreeMap::new(),
            tracks: BTreeMap::new(),
        }
    }

    /// Whether a MediaSource has been armed for capture.
    pub fn is_armed(&self, ms: i64) -> bool {
        self.sources.get(&ms).is_some_and(|source| source.armed)
    }

    /// Total bytes captured (armed) or held (not yet) for a MediaSource.
    pub fn bytes(&self, ms: i64) -> u64 {
        let Some(source) = self.sources.get(&ms) else {
            return 0;
        };
        if source.armed {
            source
                .tracks
                .iter()
                .filter_map(|sb| self.tracks.get(sb))
                .map(|track| track.bytes)
                .sum()
        } else {
            source.held
        }
    }

    /// Mark a MediaSource armed, so the hook's later `data` messages are kept.
    ///
    /// The caller separately tells the page to start sending them, by running
    /// `window.__snatchArm(ms)`. This only prepares the receiving side.
    pub fn arm(&mut self, ms: i64) {
        if let Some(source) = self.sources.get_mut(&ms) {
            source.armed = true;
        }
    }

    /// Pause or resume a MediaSource's capture. While paused, chunks that
    /// arrive are dropped rather than written. Returns whether the source was
    /// still there to pause.
    pub fn set_paused(&mut self, ms: i64, paused: bool) -> bool {
        match self.sources.get_mut(&ms) {
            Some(source) => {
                source.paused = paused;
                true
            }
            None => false,
        }
    }

    /// The streams still waiting to be captured, for the "found" counter.
    pub fn unarmed_sources(&self) -> Vec<i64> {
        self.sources
            .iter()
            .filter(|(_, source)| !source.armed)
            .map(|(ms, _)| *ms)
            .collect()
    }

    /// Forget everything, for when a tab navigates to a new page.
    ///
    /// The old page's MediaSources are gone with its JavaScript world, and its
    /// ids restart at one on the next page -- so without this a new page's
    /// stream would land on top of the old one's. The temp files go too; an
    /// ffmpeg mux already reading them keeps its own open handles, so a save
    /// in flight when the reader navigates still finishes (the inode outlives
    /// the name on Linux).
    pub fn reset(&mut self) {
        if self.dir.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        self.sources.clear();
        self.tracks.clear();
    }

    /// Take one message from the hook and act on it.
    ///
    /// Returns what, if anything, the UI should do about it. A malformed
    /// message, or a write that fails, is logged and swallowed: a capture
    /// going wrong must never disturb the page it came from.
    pub fn handle(&mut self, raw: &str) -> Update {
        let message: Message = match serde_json::from_str(raw) {
            Ok(message) => message,
            Err(error) => {
                log::debug!("mse: unreadable message ({error})");
                return Update::Quiet;
            }
        };
        match message {
            Message::Ready => Update::Quiet,
            Message::Open { ms } => {
                self.sources.entry(ms).or_insert_with(|| Source {
                    tracks: Vec::new(),
                    armed: false,
                    held: 0,
                    paused: false,
                });
                Update::Present { ms, bytes: 0 }
            }
            Message::Track { ms, sb, mime } => {
                self.sources.entry(ms).or_insert_with(|| Source {
                    tracks: Vec::new(),
                    armed: false,
                    held: 0,
                    paused: false,
                });
                if let Some(source) = self.sources.get_mut(&ms)
                    && !source.tracks.contains(&sb)
                {
                    source.tracks.push(sb);
                }
                self.tracks.entry(sb).or_insert_with(|| Track {
                    file: None,
                    path: PathBuf::new(),
                    mime,
                    bytes: 0,
                });
                Update::Quiet
            }
            Message::Grow { ms, bytes } => {
                if let Some(source) = self.sources.get_mut(&ms) {
                    source.held = bytes;
                }
                Update::Present { ms, bytes }
            }
            Message::Data { sb, b64 } => {
                let ms = self.source_of(sb);
                let paused = ms
                    .and_then(|ms| self.sources.get(&ms))
                    .is_some_and(|source| source.paused);
                // Paused: let the chunk go by unwritten, so the file skips what
                // played while paused instead of stitching a gap into it.
                if !paused {
                    self.write(sb, &b64);
                }
                match ms {
                    Some(ms) => Update::Present {
                        ms,
                        bytes: self.bytes(ms),
                    },
                    None => Update::Quiet,
                }
            }
            Message::End { ms } => Update::Ended { ms },
        }
    }

    /// The MediaSource a track belongs to.
    fn source_of(&self, sb: i64) -> Option<i64> {
        self.sources
            .iter()
            .find(|(_, source)| source.tracks.contains(&sb))
            .map(|(ms, _)| *ms)
    }

    /// Decode a chunk and append it to its track's file, opening it if needed.
    fn write(&mut self, sb: i64, b64: &str) {
        let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(bytes) => bytes,
            Err(error) => {
                log::debug!("mse: undecodable chunk for track {sb} ({error})");
                return;
            }
        };
        let Some(track) = self.tracks.get_mut(&sb) else {
            return;
        };
        // First chunk: give the track a real file now that there is something
        // to put in it.
        if track.file.is_none() {
            if let Err(error) = std::fs::create_dir_all(&self.dir) {
                log::warn!("mse: cannot create {}: {error}", self.dir.display());
                return;
            }
            let path = self.dir.join(format!("track-{sb}.bin"));
            match File::create(&path) {
                Ok(file) => {
                    track.file = Some(file);
                    track.path = path;
                }
                Err(error) => {
                    log::warn!("mse: cannot open a capture file: {error}");
                    return;
                }
            }
        }
        if let Some(file) = track.file.as_mut()
            && let Err(error) = file.write_all(&bytes)
        {
            log::warn!("mse: a capture write failed: {error}");
            return;
        }
        track.bytes += bytes.len() as u64;
    }

    /// The tracks of a MediaSource that actually have bytes on disk, in the
    /// order they were added.
    fn written_tracks(&self, ms: i64) -> Vec<&Track> {
        let Some(source) = self.sources.get(&ms) else {
            return Vec::new();
        };
        source
            .tracks
            .iter()
            .filter_map(|sb| self.tracks.get(sb))
            .filter(|track| track.bytes > 0 && !track.path.as_os_str().is_empty())
            .collect()
    }

    /// The container a set of tracks should be muxed into.
    ///
    /// WebM if every track is WebM; MP4 otherwise, which is where fragmented
    /// MP4 -- by far the common case -- belongs, and what plays everywhere.
    fn container_for(tracks: &[&Track]) -> &'static str {
        if tracks
            .iter()
            .all(|track| track.mime.to_ascii_lowercase().contains("webm"))
        {
            "webm"
        } else {
            "mp4"
        }
    }

    /// The MediaSources worth offering to capture, oldest first.
    pub fn sources(&self) -> Vec<i64> {
        self.sources.keys().copied().collect()
    }

    /// What it would take to save a MediaSource's capture.
    ///
    /// Everything the mux needs, owned, so the caller can run ffmpeg without
    /// holding a borrow of the capture across the await -- the tracks keep
    /// growing while it runs, and a partial live capture muxes fine because
    /// the files are always a valid prefix.
    ///
    /// The files stay owned by this engine: the plan reads them where they
    /// lie, so this is only safe while the engine is alive. Use `detach` for a
    /// capture that must outlive its tab.
    pub fn plan(&self, ms: i64) -> Option<SavePlan> {
        let tracks = self.written_tracks(ms);
        if tracks.is_empty() {
            return None;
        }
        Some(SavePlan {
            inputs: tracks.iter().map(|track| track.path.clone()).collect(),
            container: Self::container_for(&tracks),
            owned_dir: None,
        })
    }

    /// Stop following a MediaSource and take its captured tracks out into a
    /// self-contained plan.
    ///
    /// This is how a capture ends. Its files are moved to a directory the plan
    /// owns, apart from this engine's, so resetting or dropping the engine --
    /// which happens the instant a tab navigates, crashes or closes -- cannot
    /// delete a capture that is on its way to a file. The source is then
    /// forgotten, so any later chunk the page sends for it is ignored: that is
    /// what stops a live capture.
    ///
    /// `None` when nothing was captured, but the source is forgotten either
    /// way, so a stop always stops.
    pub fn detach(&mut self, ms: i64) -> Option<SavePlan> {
        // What there is to save, gathered before the source is forgotten. This
        // is the same plan a live save would make; `detach` then moves its
        // files somewhere the engine can no longer reach.
        let plan = self.plan(ms);
        // Forget the source and its tracks whether or not anything was
        // captured, so it stops being followed: a later chunk for a source
        // that is gone is ignored, which is what stops a live capture.
        if let Some(source) = self.sources.remove(&ms) {
            for sb in source.tracks {
                self.tracks.remove(&sb);
            }
        }
        let mut plan = plan?;

        // A directory the plan owns, so this engine resetting or dropping
        // leaves the files being saved alone. If even a temp directory cannot
        // be made, the plan keeps the originals -- a save that might race the
        // reset, which is far better than refusing to save at all.
        let owned = save_dir();
        if std::fs::create_dir_all(&owned).is_ok() {
            let mut moved = Vec::with_capacity(plan.inputs.len());
            for (index, path) in plan.inputs.iter().enumerate() {
                // A rename is free within a filesystem; a copy covers the rare
                // cross-device case; if both fail the original path is kept.
                let dest = owned.join(format!("track-{index}.bin"));
                let out = match std::fs::rename(path, &dest) {
                    Ok(()) => dest,
                    Err(_) => match std::fs::copy(path, &dest) {
                        Ok(_) => dest,
                        Err(_) => path.clone(),
                    },
                };
                moved.push(out);
            }
            plan.inputs = moved;
            plan.owned_dir = Some(owned);
        }
        Some(plan)
    }
}

/// A fresh directory a `SavePlan` owns, apart from any capture engine's.
fn save_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("snatch-save-{}-{n}", std::process::id()))
}

/// A capture ready to be muxed to a file. Owns its inputs, so it holds nothing
/// borrowed and can be moved onto the runtime that runs ffmpeg.
pub struct SavePlan {
    inputs: Vec<PathBuf>,
    container: &'static str,
    /// A directory this plan owns and must clean up, set when the files were
    /// moved out of a capture engine by `detach`. `None` when the files still
    /// belong to a live engine (from `plan`), which cleans up after itself.
    owned_dir: Option<PathBuf>,
}

impl SavePlan {
    /// Mux the captured tracks into one file under `dest`, named `stem`.
    ///
    /// A player usually feeds video and audio as separate tracks, so each is
    /// an input and the streams are copied into one container without
    /// re-encoding.
    pub async fn mux(self, dest: &Path, stem: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(dest)
            .with_context(|| format!("could not create {}", dest.display()))?;
        let output = crate::stream::unique_path(dest, stem, self.container);

        let binary = crate::stream::ffmpeg_binary();
        let mut command = Command::new(&binary);
        command
            .arg("-hide_banner")
            .arg("-nostdin")
            .arg("-loglevel")
            .arg("error");
        for input in &self.inputs {
            command.arg("-i").arg(input);
        }
        // Take a stream from each input, so a two-input mux keeps both.
        for index in 0..self.inputs.len() {
            command.arg("-map").arg(index.to_string());
        }
        command
            .arg("-c")
            .arg("copy")
            // fMP4 pieces concatenated by appending have baseline timestamps
            // that a plain copy can trip over; regenerating them is safe and
            // fixes the "non-monotonous DTS" a raw concat would hit.
            .arg("-fflags")
            .arg("+genpts");
        if self.container == "mp4" {
            command.arg("-movflags").arg("+faststart");
        }
        command
            .arg("-y")
            .arg(&output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let result = command
            .output()
            .await
            .with_context(|| format!("could not run {binary} to mux the capture"))?;
        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let reason = stderr
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .unwrap_or("ffmpeg gave no reason");
            bail!("could not mux the capture: {reason}");
        }
        Ok(output)
    }
}

impl Drop for MseCapture {
    /// A capture that was never saved leaves nothing behind.
    fn drop(&mut self) {
        if self.dir.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

impl Drop for SavePlan {
    /// A detached plan owns its files, so it deletes them once the mux that
    /// read them is done -- which is when the plan is dropped. A plan from
    /// `plan` owns nothing and touches nothing here.
    fn drop(&mut self) {
        if let Some(dir) = &self.owned_dir
            && dir.exists()
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("snatch-mse-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_stream_is_present_once_it_opens_and_grows() {
        let mut capture = MseCapture::new(scratch("present"));
        assert_eq!(capture.handle(r#"{"t":"ready"}"#), Update::Quiet);
        assert_eq!(
            capture.handle(r#"{"t":"open","ms":1}"#),
            Update::Present { ms: 1, bytes: 0 }
        );
        assert!(capture.sources().contains(&1));
        assert!(!capture.is_armed(1));
        assert_eq!(
            capture.handle(r#"{"t":"grow","ms":1,"bytes":4096}"#),
            Update::Present { ms: 1, bytes: 4096 }
        );
        assert_eq!(capture.bytes(1), 4096);
    }

    #[test]
    fn armed_data_is_written_and_counted() {
        let dir = scratch("data");
        let mut capture = MseCapture::new(dir.clone());
        capture.handle(r#"{"t":"open","ms":1}"#);
        capture.handle(r#"{"t":"track","ms":1,"sb":1,"mime":"video/mp4"}"#);
        capture.arm(1);
        // "hello" base64.
        let update = capture.handle(r#"{"t":"data","sb":1,"b64":"aGVsbG8="}"#);
        assert_eq!(update, Update::Present { ms: 1, bytes: 5 });
        assert_eq!(capture.bytes(1), 5);
        let written = std::fs::read(dir.join("track-1.bin")).expect("the chunk was written");
        assert_eq!(written, b"hello");
    }

    #[test]
    fn a_malformed_message_is_ignored() {
        let mut capture = MseCapture::new(scratch("bad"));
        assert_eq!(capture.handle("not json"), Update::Quiet);
        assert_eq!(capture.handle(r#"{"t":"nonsense"}"#), Update::Quiet);
    }

    #[test]
    fn the_container_follows_the_tracks() {
        let webm = Track {
            file: None,
            path: PathBuf::from("x"),
            mime: "video/webm; codecs=\"vp9\"".to_owned(),
            bytes: 1,
        };
        let mp4 = Track {
            file: None,
            path: PathBuf::from("y"),
            mime: "video/mp4; codecs=\"avc1\"".to_owned(),
            bytes: 1,
        };
        assert_eq!(MseCapture::container_for(&[&webm]), "webm");
        assert_eq!(MseCapture::container_for(&[&mp4]), "mp4");
        // A mix goes to MP4, which carries both.
        assert_eq!(MseCapture::container_for(&[&webm, &mp4]), "mp4");
    }

    #[test]
    fn nothing_captured_yields_no_save_plan() {
        let capture = MseCapture::new(scratch("empty"));
        assert!(capture.plan(1).is_none());
    }

    /// The whole chain with a real muxer: receive a fragmented MP4 as base64
    /// chunks, write it, plan and mux, and check the result plays.
    #[test]
    fn a_captured_fragmented_mp4_muxes_to_a_playable_file() {
        // Build a small fragmented MP4 the way a page would feed one.
        let dir = scratch("mux");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("src.mp4");
        let built = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10:duration=1",
                "-c:v",
                "libx264",
                "-profile:v",
                "baseline",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
                "-y",
            ])
            .arg(&source)
            .output();
        match built {
            Ok(out) if out.status.success() => {}
            _ => {
                eprintln!("skipping: ffmpeg is not available");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        }
        let bytes = std::fs::read(&source).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let mut capture = MseCapture::new(dir.join("cap"));
        capture.handle(r#"{"t":"open","ms":1}"#);
        capture.handle(r#"{"t":"track","ms":1,"sb":1,"mime":"video/mp4"}"#);
        capture.arm(1);
        capture.handle(&format!(r#"{{"t":"data","sb":1,"b64":"{b64}"}}"#));
        assert!(capture.plan(1).is_some());

        let plan = capture.plan(1).expect("a plan once there is data");
        let dest = dir.join("out");
        let saved = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(plan.mux(&dest, "clip"))
            .expect("the capture muxes");
        assert_eq!(saved.extension().unwrap(), "mp4");

        // It decodes and reports a real duration.
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=nw=1:nk=1",
            ])
            .arg(&saved)
            .output()
            .unwrap();
        let duration: f64 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0);
        assert!(duration > 0.5, "muxed file has no duration: {duration}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_forgets_streams_and_files() {
        let dir = scratch("reset");
        let mut capture = MseCapture::new(dir.clone());
        capture.handle(r#"{"t":"open","ms":1}"#);
        capture.handle(r#"{"t":"track","ms":1,"sb":1,"mime":"video/mp4"}"#);
        capture.arm(1);
        capture.handle(r#"{"t":"data","sb":1,"b64":"aGVsbG8="}"#);
        assert!(capture.is_armed(1));
        assert!(dir.exists());

        capture.reset();
        assert!(capture.sources().is_empty());
        assert!(capture.unarmed_sources().is_empty());
        assert!(!dir.exists(), "the temp files should be gone");

        // A new page's stream starts clean on the same capture.
        capture.handle(r#"{"t":"open","ms":1}"#);
        assert_eq!(capture.sources(), vec![1]);
        assert!(!capture.is_armed(1));
    }

    #[test]
    fn a_detached_capture_survives_the_engine_resetting() {
        let dir = scratch("detach");
        let mut capture = MseCapture::new(dir.clone());
        capture.handle(r#"{"t":"open","ms":1}"#);
        capture.handle(r#"{"t":"track","ms":1,"sb":1,"mime":"video/mp4"}"#);
        capture.arm(1);
        capture.handle(r#"{"t":"data","sb":1,"b64":"aGVsbG8="}"#); // "hello"

        let plan = capture.detach(1).expect("a plan once there is data");
        // The source is forgotten, so the capture has stopped.
        assert!(capture.sources().is_empty());
        // A later chunk for the gone source is ignored, not written.
        assert_eq!(
            capture.handle(r#"{"t":"data","sb":1,"b64":"d29ybGQ="}"#),
            Update::Quiet
        );

        // Resetting and dropping the engine must not touch the detached files:
        // this is exactly what happens when the tab navigates or closes while a
        // save is running.
        let input = plan.inputs.first().cloned().expect("an input file");
        capture.reset();
        assert!(input.exists(), "the detached file survives a reset");
        assert_eq!(std::fs::read(&input).unwrap(), b"hello");
        drop(capture);
        assert!(
            input.exists(),
            "the detached file survives the engine dropping"
        );

        // Dropping the plan cleans up after itself.
        let owned = input.parent().unwrap().to_path_buf();
        drop(plan);
        assert!(!owned.exists(), "the plan deletes its own files when done");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_paused_capture_drops_chunks_until_it_resumes() {
        let dir = scratch("pause");
        let mut capture = MseCapture::new(dir.clone());
        capture.handle(r#"{"t":"open","ms":1}"#);
        capture.handle(r#"{"t":"track","ms":1,"sb":1,"mime":"video/mp4"}"#);
        capture.arm(1);
        capture.handle(r#"{"t":"data","sb":1,"b64":"aGVsbG8="}"#); // "hello"
        assert_eq!(capture.bytes(1), 5);

        // Paused: the chunk is dropped, so the count does not move.
        assert!(capture.set_paused(1, true));
        capture.handle(r#"{"t":"data","sb":1,"b64":"d29ybGQ="}"#); // "world"
        assert_eq!(capture.bytes(1), 5, "a paused capture keeps nothing new");

        // Resumed: chunks are kept again.
        assert!(capture.set_paused(1, false));
        capture.handle(r#"{"t":"data","sb":1,"b64":"ISEh"}"#); // "!!!"
        assert_eq!(capture.bytes(1), 8);
        // What is on disk is the kept chunks only, with the paused one missing.
        assert_eq!(std::fs::read(dir.join("track-1.bin")).unwrap(), b"hello!!!");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detaching_a_bare_source_stops_it_and_yields_no_plan() {
        let mut capture = MseCapture::new(scratch("detach-empty"));
        capture.handle(r#"{"t":"open","ms":1}"#);
        capture.handle(r#"{"t":"track","ms":1,"sb":1,"mime":"video/mp4"}"#);
        // Armed but nothing appended yet.
        capture.arm(1);
        assert!(capture.detach(1).is_none());
        assert!(
            capture.sources().is_empty(),
            "the source is still forgotten"
        );
    }

    #[test]
    fn a_dropped_capture_deletes_its_directory() {
        let dir = scratch("cleanup");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("track-1.bin"), b"data").unwrap();
        assert!(dir.exists());
        drop(MseCapture::new(dir.clone()));
        assert!(!dir.exists(), "the capture directory should be gone");
    }
}
