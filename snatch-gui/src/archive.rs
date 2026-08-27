//! Unpacking archives once they finish downloading.
//!
//! Bulk downloads arrive as archives, often split across volumes, and having
//! to go and unpack them by hand undoes most of the point of automating the
//! download in the first place.
//!
//! Three things make this more than "spawn a tool":
//!
//! * **Volumes.** `part1.rar`, `.7z.001`, `.r00` — a set is only extractable
//!   once every part has landed, and parts finish in whatever order the
//!   connections happen to complete. Every finished part triggers an attempt;
//!   an attempt that reports a missing volume is silently a no-op, so the set
//!   unpacks exactly once, when the last part arrives.
//! * **Passwords.** An encrypted archive cannot be unpacked unattended, so the
//!   job stops and asks rather than failing.
//! * **Where the files go.** Always into a directory named after the archive.
//!   Dumping several hundred loose files into someone's download folder is a
//!   worse outcome than one extra level of nesting.
//!
//! Extraction is deliberately conservative about what it recognises. A bare
//! `.gz`, `.xz` or `.zst` is left alone: those are frequently the artifact
//! somebody actually wanted, and `.tar.gz` already covers the archive case.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// What an archive needs to be opened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    SevenZip,
    Zip,
    Rar,
    Tar,
}

impl Format {
    fn label(self) -> &'static str {
        match self {
            Self::SevenZip => "7z",
            Self::Zip => "zip",
            Self::Rar => "rar",
            Self::Tar => "tar",
        }
    }
}

/// A recognised archive, with every volume that belongs to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The volume to hand the tool: extraction always starts at the first.
    pub first: PathBuf,
    /// Every file in the set, including `first`.
    pub parts: Vec<PathBuf>,
    /// Name for the directory the contents go into.
    pub stem: String,
    pub format: Format,
}

impl Archive {
    /// Where the contents will be written.
    pub fn destination(&self) -> PathBuf {
        let parent = self
            .first
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        unique_dir(&parent, &self.stem)
    }
}

/// Suffixes that are an archive on their own.
const SINGLE: [(&str, Format); 8] = [
    (".tar.gz", Format::Tar),
    (".tar.bz2", Format::Tar),
    (".tar.xz", Format::Tar),
    (".tar.zst", Format::Tar),
    (".tgz", Format::Tar),
    (".tar", Format::Tar),
    (".7z", Format::SevenZip),
    (".zip", Format::Zip),
];

/// Recognise an archive, and gather its sibling volumes if it has any.
///
/// Returns `None` for anything that is not an archive, and for a volume that
/// is not the first one — extraction always starts at the first, so a set is
/// described once no matter which part triggered the look.
pub fn identify(path: &Path) -> Option<Archive> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let parent = path.parent().unwrap_or(Path::new("."));

    // `archive.7z.001`, `archive.zip.002` — a numbered volume set.
    if let Some((base, number)) = lower.rsplit_once('.')
        && number.len() >= 2
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && let Some(format) = SINGLE
            .iter()
            .find_map(|(suffix, format)| base.ends_with(suffix).then_some(*format))
    {
        // Only the first volume describes the set.
        if number.parse::<u32>().ok()? != 1 {
            return None;
        }
        let prefix = &name[..name.len() - number.len()];
        let parts = siblings(parent, |candidate| {
            let Some(rest) = candidate.strip_prefix(prefix) else {
                return false;
            };
            !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
        });
        return Some(Archive {
            first: path.to_path_buf(),
            parts,
            stem: strip_known_suffix(&name[..name.len() - number.len() - 1]),
            format,
        });
    }

    // `archive.part1.rar`, `archive.part01.rar`.
    if let Some(base) = lower.strip_suffix(".rar")
        && let Some((stem, part)) = base.rsplit_once(".part")
        && !part.is_empty()
        && part.bytes().all(|byte| byte.is_ascii_digit())
    {
        if part.parse::<u32>().ok()? != 1 {
            return None;
        }
        let prefix = format!("{stem}.part");
        let parts = siblings(parent, |candidate| {
            let lower = candidate.to_ascii_lowercase();
            let Some(rest) = lower.strip_prefix(&prefix) else {
                return false;
            };
            rest.strip_suffix(".rar").is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
        });
        return Some(Archive {
            first: path.to_path_buf(),
            parts,
            stem: stem.to_owned(),
            format: Format::Rar,
        });
    }

    // `archive.r00`, `archive.r01` — a continuation of `archive.rar`, never
    // the entry point.
    if let Some((_, rest)) = lower.rsplit_once(".r")
        && rest.len() == 2
        && rest.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    // A plain archive. `.rar` may still head an old-style `.r00` set.
    if lower.ends_with(".rar") {
        let stem = &name[..name.len() - 4];
        let prefix = stem.to_ascii_lowercase();
        let parts = siblings(parent, |candidate| {
            let lower = candidate.to_ascii_lowercase();
            let Some(rest) = lower.strip_prefix(&prefix) else {
                return false;
            };
            rest == ".rar"
                || (rest.len() == 4
                    && rest.starts_with(".r")
                    && rest[2..].bytes().all(|byte| byte.is_ascii_digit()))
        });
        return Some(Archive {
            first: path.to_path_buf(),
            parts,
            stem: stem.to_owned(),
            format: Format::Rar,
        });
    }

    let format = SINGLE
        .iter()
        .find_map(|(suffix, format)| lower.ends_with(suffix).then_some(*format))?;
    Some(Archive {
        first: path.to_path_buf(),
        parts: vec![path.to_path_buf()],
        stem: strip_known_suffix(name),
        format,
    })
}

/// Every file in `directory` whose name the predicate accepts, sorted.
fn siblings(directory: &Path, accept: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str()
            && accept(name)
        {
            found.insert(entry.path());
        }
    }
    found.into_iter().collect()
}

/// Drop a recognised archive suffix to get a directory name.
fn strip_known_suffix(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for (suffix, _) in SINGLE {
        if lower.ends_with(suffix) {
            return name[..name.len() - suffix.len()].to_owned();
        }
    }
    name.to_owned()
}

/// A directory name inside `parent` that is not taken yet.
fn unique_dir(parent: &Path, stem: &str) -> PathBuf {
    let stem = if stem.trim().is_empty() {
        "extracted"
    } else {
        stem
    };
    let first = parent.join(stem);
    if !first.exists() {
        return first;
    }
    for suffix in 1..1000 {
        let candidate = parent.join(format!("{stem}.{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// What the extractor reports back.
#[derive(Debug)]
pub enum ArchiveEvent {
    Started {
        job_id: i64,
        name: String,
        parts: usize,
    },
    Progress {
        job_id: i64,
        percent: u8,
    },
    /// The archive is encrypted. Nothing else happens until a password is
    /// supplied and the job resubmitted.
    NeedsPassword {
        job_id: i64,
        name: String,
    },
    Finished {
        job_id: i64,
        destination: PathBuf,
        removed_parts: usize,
    },
    Failed {
        job_id: i64,
        error: String,
    },
}

/// Which program to drive, chosen from what is actually installed.
fn extractor_for(format: Format) -> Option<&'static str> {
    // 7-Zip handles everything here except RAR, whose codec is non-free and
    // is left out of most distribution builds -- so RAR gets its own search
    // rather than being assumed to work.
    let candidates: &[&str] = match format {
        Format::Rar => &["unar", "unrar", "7z", "7zz", "7za"],
        _ => &["7z", "7zz", "7za", "bsdtar"],
    };
    candidates
        .iter()
        .copied()
        .find(|tool| which(tool).is_some())
}

fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(tool))
        .find(|candidate| candidate.is_file())
}

/// One archive waiting to be unpacked.
#[derive(Debug, Clone)]
pub struct ArchiveJob {
    pub archive: Archive,
    pub password: Option<String>,
    /// Remove the volumes once the contents are safely out.
    pub delete_after: bool,
}

/// Serial extractor. One archive at a time, because unpacking is disk-bound
/// and running several at once makes all of them slower.
pub struct ArchiveQueue {
    events: mpsc::Sender<ArchiveEvent>,
    turn: tokio::sync::Mutex<()>,
    next_id: std::sync::atomic::AtomicI64,
    running: std::sync::atomic::AtomicUsize,
}

impl ArchiveQueue {
    pub fn new(events: mpsc::Sender<ArchiveEvent>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            events,
            turn: tokio::sync::Mutex::new(()),
            next_id: std::sync::atomic::AtomicI64::new(1),
            running: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Jobs waiting for or holding the extractor.
    pub fn outstanding(&self) -> usize {
        self.running.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Unpack one archive. Returns when it has finished, so spawn it.
    pub async fn submit(self: std::sync::Arc<Self>, job: ArchiveJob, job_id: i64) {
        self.running
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _slot = ReleaseOnDrop(std::sync::Arc::clone(&self));
        let _turn = self.turn.lock().await;

        let name = job
            .archive
            .first
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.archive.stem.clone());

        let _ = self
            .events
            .send(ArchiveEvent::Started {
                job_id,
                name: name.clone(),
                parts: job.archive.parts.len().max(1),
            })
            .await;

        match self.run(&job, job_id).await {
            Ok(destination) => {
                let removed = if job.delete_after {
                    remove_parts(&job.archive)
                } else {
                    0
                };
                let _ = self
                    .events
                    .send(ArchiveEvent::Finished {
                        job_id,
                        destination,
                        removed_parts: removed,
                    })
                    .await;
            }
            Err(Failure::NeedsPassword) => {
                let _ = self
                    .events
                    .send(ArchiveEvent::NeedsPassword { job_id, name })
                    .await;
            }
            // A set that is still missing volumes is not a failure: the part
            // that completes it will trigger another attempt.
            Err(Failure::Incomplete) => {
                log::debug!("{name} is not complete yet; waiting for more volumes");
            }
            Err(Failure::Fatal(error)) => {
                let _ = self
                    .events
                    .send(ArchiveEvent::Failed {
                        job_id,
                        error: format!("{error:#}"),
                    })
                    .await;
            }
        }
    }

    pub fn next_job_id(&self) -> i64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    async fn run(&self, job: &ArchiveJob, job_id: i64) -> std::result::Result<PathBuf, Failure> {
        let format = job.archive.format;
        let tool = extractor_for(format).ok_or_else(|| {
            Failure::Fatal(anyhow::anyhow!(
                "no tool installed that can open a {} archive; install {}",
                format.label(),
                if format == Format::Rar {
                    "unar"
                } else {
                    "p7zip"
                }
            ))
        })?;

        let destination = job.archive.destination();
        std::fs::create_dir_all(&destination)
            .with_context(|| format!("could not create {}", destination.display()))
            .map_err(Failure::Fatal)?;

        let mut command = Command::new(tool);
        if tool == "unar" {
            command
                .arg("-quiet")
                .arg("-no-directory")
                .arg("-output-directory")
                .arg(&destination);
            match &job.password {
                Some(password) => {
                    command.arg("-password").arg(password);
                }
                // Without this unar stops for input and never returns.
                None => {
                    command.arg("-password").arg("");
                }
            }
            command.arg("--").arg(&job.archive.first);
        } else if tool == "unrar" {
            command.arg("x").arg("-y");
            match &job.password {
                Some(password) => {
                    command.arg(format!("-p{password}"));
                }
                None => {
                    command.arg("-p-");
                }
            }
            command.arg(&job.archive.first).arg(&destination);
        } else if tool == "bsdtar" {
            command
                .arg("-x")
                .arg("-f")
                .arg(&job.archive.first)
                .arg("-C")
                .arg(&destination);
        } else {
            // 7-Zip. `x` keeps the stored directory structure; `-y` answers
            // the overwrite prompts that would otherwise hang the job.
            //
            // Path traversal is 7-Zip's own responsibility and it does handle
            // it: an entry named `../../x` or `/tmp/x` is written inside the
            // output directory, verified against 7-Zip 26.02. Extracting into
            // a fresh directory per archive is the second line of defence.
            command
                .arg("x")
                .arg(&job.archive.first)
                .arg(format!("-o{}", destination.display()))
                .arg("-y")
                .arg("-bsp1")
                .arg("-bso0");
            // An empty password makes 7-Zip fail instead of blocking on a
            // prompt that has no terminal to read from.
            command.arg(format!("-p{}", job.password.clone().unwrap_or_default()));
        }

        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Failure::Fatal(anyhow::anyhow!(
                    "{tool} disappeared from PATH between the check and the run"
                )));
            }
            Err(error) => {
                return Err(Failure::Fatal(
                    anyhow::Error::new(error).context(format!("could not start {tool}")),
                ));
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let diagnostics = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        // Progress rides on stdout, terminated with carriage returns rather
        // than newlines, so the reader splits on both.
        let progress = tokio::spawn({
            let events = self.events.clone();
            let diagnostics = std::sync::Arc::clone(&diagnostics);
            async move {
                let Some(stdout) = stdout else { return };
                let mut reader = BufReader::new(stdout);
                let mut chunk = Vec::new();
                let mut last = 0_u8;
                loop {
                    chunk.clear();
                    // `\r` is the separator during progress, `\n` between
                    // messages; reading to either keeps both usable.
                    let read = read_until_either(&mut reader, &mut chunk).await;
                    if read == 0 {
                        return;
                    }
                    let line = String::from_utf8_lossy(&chunk).trim().to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    record(&diagnostics, &line);
                    if let Some(percent) = parse_percent(&line)
                        && percent != last
                    {
                        last = percent;
                        let _ = events
                            .send(ArchiveEvent::Progress { job_id, percent })
                            .await;
                    }
                }
            }
        });

        let errors = tokio::spawn({
            let diagnostics = std::sync::Arc::clone(&diagnostics);
            async move {
                let Some(stderr) = stderr else { return };
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    record(&diagnostics, line.trim());
                }
            }
        });

        let status = child
            .wait()
            .await
            .context("waiting for the extractor failed")
            .map_err(Failure::Fatal)?;
        let _ = progress.await;
        let _ = errors.await;

        let said = diagnostics
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .join("\n");

        if status.success() {
            return Ok(destination);
        }

        // Nothing useful was written, so do not leave an empty directory.
        let _ = std::fs::remove_dir(&destination);

        let lower = said.to_ascii_lowercase();
        if lower.contains("wrong password")
            || lower.contains("password is incorrect")
            || lower.contains("cannot open encrypted")
        {
            return Err(Failure::NeedsPassword);
        }
        if lower.contains("missing volume") || lower.contains("cannot find volume") {
            return Err(Failure::Incomplete);
        }

        let detail = said.lines().last().unwrap_or("no output").to_owned();
        Err(Failure::Fatal(anyhow::anyhow!(
            "{tool} could not unpack it: {detail}"
        )))
    }
}

/// Why an extraction stopped.
enum Failure {
    NeedsPassword,
    /// A volume set that has not all arrived yet.
    Incomplete,
    Fatal(anyhow::Error),
}

fn record(diagnostics: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, line: &str) {
    if line.is_empty() {
        return;
    }
    log::debug!(target: "archive", "{line}");
    let mut held = diagnostics
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if held.len() < 40 {
        held.push(line.to_owned());
    }
}

/// Read up to the next `\r` or `\n`, whichever comes first.
async fn read_until_either<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> usize {
    use tokio::io::AsyncReadExt;
    let mut total = 0;
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte).await {
            Ok(0) => return total,
            Ok(_) => {
                total += 1;
                if byte[0] == b'\r' || byte[0] == b'\n' {
                    return total;
                }
                out.push(byte[0]);
                // A pathological line must not grow without bound.
                if out.len() > 4096 {
                    return total;
                }
            }
            Err(_) => return total,
        }
    }
}

/// Pull `42%` out of a progress line.
fn parse_percent(line: &str) -> Option<u8> {
    let index = line.find('%')?;
    let digits: String = line[..index]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse::<u8>().ok().filter(|value| *value <= 100)
}

/// Delete the volumes, returning how many went.
fn remove_parts(archive: &Archive) -> usize {
    let mut removed = 0;
    for part in &archive.parts {
        match std::fs::remove_file(part) {
            Ok(()) => removed += 1,
            Err(error) => log::warn!("could not remove {}: {error}", part.display()),
        }
    }
    removed
}

struct ReleaseOnDrop(std::sync::Arc<ArchiveQueue>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0
            .running
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("snatch-archive-{name}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        directory
    }

    fn touch(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, b"x").expect("write");
        path
    }

    #[test]
    fn plain_archives_are_recognised() {
        let directory = scratch("plain");
        for (name, format, stem) in [
            ("release.7z", Format::SevenZip, "release"),
            ("release.zip", Format::Zip, "release"),
            ("release.tar.gz", Format::Tar, "release"),
            ("release.tar.zst", Format::Tar, "release"),
            ("release.tgz", Format::Tar, "release"),
            ("release.rar", Format::Rar, "release"),
        ] {
            let path = touch(&directory, name);
            let found = identify(&path).unwrap_or_else(|| panic!("{name} should be recognised"));
            assert_eq!(found.format, format, "{name}");
            assert_eq!(found.stem, stem, "{name}");
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn things_that_are_not_archives_are_left_alone() {
        let directory = scratch("not-archives");
        for name in [
            "movie.mkv",
            "photo.jpg",
            "notes.txt",
            // Deliberately excluded: usually the artifact somebody wanted.
            "dump.sql.gz",
            "data.xz",
            "image.iso",
        ] {
            let path = touch(&directory, name);
            assert!(identify(&path).is_none(), "{name} should not be an archive");
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_numbered_volume_set_is_gathered_from_its_first_part() {
        let directory = scratch("volumes");
        let first = touch(&directory, "big.7z.001");
        touch(&directory, "big.7z.002");
        touch(&directory, "big.7z.003");
        touch(&directory, "unrelated.txt");

        let found = identify(&first).expect("the first volume describes the set");
        assert_eq!(found.format, Format::SevenZip);
        assert_eq!(found.stem, "big");
        assert_eq!(found.parts.len(), 3, "{:?}", found.parts);

        // Only the first volume is an entry point, so a set is never
        // extracted three times over.
        assert!(identify(&directory.join("big.7z.002")).is_none());
        assert!(identify(&directory.join("big.7z.003")).is_none());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_rar_part_set_is_gathered() {
        let directory = scratch("rar-parts");
        let first = touch(&directory, "movie.part1.rar");
        touch(&directory, "movie.part2.rar");
        touch(&directory, "movie.part3.rar");

        let found = identify(&first).expect("part1 describes the set");
        assert_eq!(found.format, Format::Rar);
        assert_eq!(found.stem, "movie");
        assert_eq!(found.parts.len(), 3);
        assert!(identify(&directory.join("movie.part2.rar")).is_none());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_old_style_rar_set_starts_at_the_rar_file() {
        let directory = scratch("rar-r00");
        let first = touch(&directory, "show.rar");
        touch(&directory, "show.r00");
        touch(&directory, "show.r01");

        let found = identify(&first).expect("the .rar heads the set");
        assert_eq!(found.parts.len(), 3, "{:?}", found.parts);
        // A continuation volume is never an entry point.
        assert!(identify(&directory.join("show.r00")).is_none());
        assert!(identify(&directory.join("show.r01")).is_none());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_destination_never_overwrites_an_existing_directory() {
        let directory = scratch("destination");
        let path = touch(&directory, "release.zip");
        let archive = identify(&path).expect("recognised");
        assert_eq!(archive.destination(), directory.join("release"));

        std::fs::create_dir_all(directory.join("release")).expect("mkdir");
        assert_eq!(archive.destination(), directory.join("release.1"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Build a real archive with whatever 7-Zip is installed.
    ///
    /// Returns `None` when there is none, so the suite still runs on a
    /// machine without it rather than failing for the wrong reason.
    fn build_archive(directory: &Path, name: &str, password: Option<&str>) -> Option<PathBuf> {
        let tool = ["7z", "7zz", "7za"]
            .into_iter()
            .find(|t| which(t).is_some())?;
        let payload = directory.join("payload");
        std::fs::create_dir_all(&payload).expect("payload dir");
        std::fs::write(payload.join("inside.txt"), b"unpacked me\n").expect("write");

        let archive = directory.join(name);
        let mut command = std::process::Command::new(tool);
        command.arg("a").arg("-bso0").arg("-bsp0").arg(&archive);
        if let Some(password) = password {
            command.arg(format!("-p{password}"));
        }
        command.arg(&payload);
        let status = command.status().expect("7z runs");
        assert!(status.success(), "could not build the test archive");
        let _ = std::fs::remove_dir_all(&payload);
        Some(archive)
    }

    async fn drain(mut rx: mpsc::Receiver<ArchiveEvent>) -> Vec<ArchiveEvent> {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            seen.push(event);
        }
        seen
    }

    #[tokio::test]
    async fn a_real_archive_is_unpacked() {
        let directory = scratch("extract-real");
        let Some(archive) = build_archive(&directory, "bundle.7z", None) else {
            eprintln!("no 7-Zip installed; skipping");
            return;
        };

        let found = identify(&archive).expect("recognised");
        let (tx, rx) = mpsc::channel(64);
        let queue = ArchiveQueue::new(tx);
        let job_id = queue.next_job_id();
        std::sync::Arc::clone(&queue)
            .submit(
                ArchiveJob {
                    archive: found.clone(),
                    password: None,
                    delete_after: false,
                },
                job_id,
            )
            .await;
        drop(queue);

        let events = drain(rx).await;
        let destination = events
            .iter()
            .find_map(|event| match event {
                ArchiveEvent::Finished { destination, .. } => Some(destination.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a Finished event, got {events:?}"));

        // The contents are under the archive's own directory, not loose.
        let unpacked = destination.join("payload").join("inside.txt");
        assert!(unpacked.exists(), "missing {}", unpacked.display());
        assert_eq!(
            std::fs::read_to_string(&unpacked).expect("read"),
            "unpacked me\n"
        );
        // delete_after was false, so the archive is still there.
        assert!(archive.exists(), "the archive should not have been removed");

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn an_encrypted_archive_asks_for_a_password_then_unpacks() {
        let directory = scratch("extract-encrypted");
        let Some(archive) = build_archive(&directory, "locked.7z", Some("hunter2")) else {
            eprintln!("no 7-Zip installed; skipping");
            return;
        };
        let found = identify(&archive).expect("recognised");

        // Without the password it must ask, not fail and not hang.
        let (tx, rx) = mpsc::channel(64);
        let queue = ArchiveQueue::new(tx);
        let job_id = queue.next_job_id();
        std::sync::Arc::clone(&queue)
            .submit(
                ArchiveJob {
                    archive: found.clone(),
                    password: None,
                    delete_after: false,
                },
                job_id,
            )
            .await;
        drop(queue);
        let events = drain(rx).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ArchiveEvent::NeedsPassword { .. })),
            "expected NeedsPassword, got {events:?}"
        );

        // With it, the same job goes through and takes the parts with it.
        let (tx, rx) = mpsc::channel(64);
        let queue = ArchiveQueue::new(tx);
        let job_id = queue.next_job_id();
        std::sync::Arc::clone(&queue)
            .submit(
                ArchiveJob {
                    archive: found,
                    password: Some("hunter2".to_owned()),
                    delete_after: true,
                },
                job_id,
            )
            .await;
        drop(queue);
        let events = drain(rx).await;
        let (destination, removed) = events
            .iter()
            .find_map(|event| match event {
                ArchiveEvent::Finished {
                    destination,
                    removed_parts,
                    ..
                } => Some((destination.clone(), *removed_parts)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected Finished, got {events:?}"));

        assert!(destination.join("payload").join("inside.txt").exists());
        assert_eq!(removed, 1);
        assert!(!archive.exists(), "delete_after should have removed it");

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn progress_percentages_are_read_from_the_line() {
        assert_eq!(parse_percent(" 42% 3 - src/file.bin"), Some(42));
        assert_eq!(parse_percent("  0M Scan           0%"), Some(0));
        assert_eq!(parse_percent("100%"), Some(100));
        assert_eq!(parse_percent("no percentage here"), None);
        // Not a percentage of anything sane.
        assert_eq!(parse_percent("999%"), None);
    }
}
