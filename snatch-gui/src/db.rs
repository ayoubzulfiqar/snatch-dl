//! Persistent job history, backed by SQLite.
//!
//! `rusqlite` is built with the `bundled` feature, so SQLite is compiled from
//! source into the binary and there is no `libsqlite3-dev` to chase across
//! distributions.
//!
//! Every public method is `async` and runs the actual query on
//! [`tokio::task::spawn_blocking`]. SQLite calls block, and blocking a tokio
//! worker while a torrent session and three subprocesses are live is exactly
//! how a UI ends up stuttering.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Lifecycle of a scrape batch or a media job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Complete,
    Failed,
    Cancelled,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Complete => "complete",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "complete" => JobState::Complete,
            "failed" => JobState::Failed,
            "cancelled" => JobState::Cancelled,
            _ => JobState::Running,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            JobState::Running => "Running",
            JobState::Complete => "Completed",
            JobState::Failed => "Failed",
            JobState::Cancelled => "Cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, JobState::Running)
    }
}

/// One `gallery-dl` run. Mirrors the `gallery_batches` row, so some columns
/// are carried for callers other than the scraper page.
#[allow(dead_code, reason = "faithful mirror of the table row")]
#[derive(Debug, Clone)]
pub struct GalleryBatch {
    pub id: i64,
    pub url: String,
    pub destination: PathBuf,
    pub state: JobState,
    /// Total files gallery-dl announced via its `[n/m]` counter, if known.
    pub total: u64,
    pub downloaded: u64,
    pub skipped: u64,
    pub failed: u64,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[allow(dead_code, reason = "helpers for callers other than the scraper page")]
impl GalleryBatch {
    /// Progress in 0..=1, or `None` while the total is still unknown.
    pub fn fraction(&self) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        let done = self.downloaded + self.skipped + self.failed;
        Some((done as f64 / self.total as f64).clamp(0.0, 1.0))
    }

    pub fn handled(&self) -> u64 {
        self.downloaded + self.skipped + self.failed
    }
}

/// One file produced by a batch.
#[allow(dead_code, reason = "faithful mirror of the table row")]
#[derive(Debug, Clone)]
pub struct GalleryFile {
    pub id: i64,
    pub batch_id: i64,
    pub path: PathBuf,
    pub skipped: bool,
}

/// A queued or finished ffmpeg job.
#[allow(dead_code, reason = "returned by recent_media_jobs")]
#[derive(Debug, Clone)]
pub struct MediaJobRecord {
    pub id: i64,
    pub input: PathBuf,
    pub output: PathBuf,
    pub action: String,
    pub state: JobState,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// A handle to the job database.
#[derive(Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open (creating if needed) and migrate the database.
    pub async fn open(path: PathBuf) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::open_blocking(&path))
            .await
            .context("the database task panicked")?
    }

    fn open_blocking(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }

        let connection =
            Connection::open(path).with_context(|| format!("could not open {}", path.display()))?;

        // WAL keeps readers from blocking the writer, which matters because the
        // UI polls while scrapers write.
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("could not enable WAL mode")?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .context("could not set the synchronous pragma")?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .context("could not enable foreign keys")?;

        connection
            .execute_batch(SCHEMA)
            .context("could not apply the database schema")?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Run a closure against the connection on the blocking pool.
    async fn with<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let guard = connection
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            work(&guard)
        })
        .await
        .context("the database task panicked")?
    }

    // -- gallery batches ---------------------------------------------------

    pub async fn create_batch(&self, url: String, destination: PathBuf) -> Result<i64> {
        let now = unix_now();
        self.with(move |connection| {
            connection
                .execute(
                    "INSERT INTO gallery_batches (url, destination, state, started_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        url,
                        destination.to_string_lossy(),
                        JobState::Running.as_str(),
                        now
                    ],
                )
                .context("could not record the scrape batch")?;
            Ok(connection.last_insert_rowid())
        })
        .await
    }

    /// Record the batch total once gallery-dl reveals it.
    pub async fn set_batch_total(&self, batch_id: i64, total: u64) -> Result<()> {
        self.with(move |connection| {
            connection
                .execute(
                    "UPDATE gallery_batches SET total = ?2 WHERE id = ?1",
                    params![batch_id, total as i64],
                )
                .context("could not update the batch total")?;
            Ok(())
        })
        .await
    }

    /// Record one file and bump the matching counter in a single transaction.
    pub async fn record_file(&self, batch_id: i64, path: PathBuf, skipped: bool) -> Result<()> {
        let now = unix_now();
        self.with(move |connection| {
            let column = if skipped { "skipped" } else { "downloaded" };
            connection
                .execute(
                    "INSERT INTO gallery_files (batch_id, path, skipped, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![batch_id, path.to_string_lossy(), skipped as i64, now],
                )
                .context("could not record the scraped file")?;
            connection
                .execute(
                    &format!("UPDATE gallery_batches SET {column} = {column} + 1 WHERE id = ?1"),
                    params![batch_id],
                )
                .context("could not bump the batch counter")?;
            Ok(())
        })
        .await
    }

    pub async fn record_failure(&self, batch_id: i64) -> Result<()> {
        self.with(move |connection| {
            connection
                .execute(
                    "UPDATE gallery_batches SET failed = failed + 1 WHERE id = ?1",
                    params![batch_id],
                )
                .context("could not record the failed file")?;
            Ok(())
        })
        .await
    }

    pub async fn finish_batch(
        &self,
        batch_id: i64,
        state: JobState,
        error: Option<String>,
    ) -> Result<()> {
        let now = unix_now();
        self.with(move |connection| {
            connection
                .execute(
                    "UPDATE gallery_batches
                     SET state = ?2, error = ?3, finished_at = ?4
                     WHERE id = ?1",
                    params![batch_id, state.as_str(), error, now],
                )
                .context("could not close the scrape batch")?;
            Ok(())
        })
        .await
    }

    pub async fn batch(&self, batch_id: i64) -> Result<Option<GalleryBatch>> {
        self.with(move |connection| {
            connection
                .query_row(
                    "SELECT id, url, destination, state, total, downloaded, skipped, failed,
                            error, started_at, finished_at
                     FROM gallery_batches WHERE id = ?1",
                    params![batch_id],
                    map_batch,
                )
                .optional()
                .context("could not read the scrape batch")
        })
        .await
    }

    /// Most recent batches first.
    pub async fn recent_batches(&self, limit: u32) -> Result<Vec<GalleryBatch>> {
        self.with(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, url, destination, state, total, downloaded, skipped, failed,
                            error, started_at, finished_at
                     FROM gallery_batches
                     ORDER BY started_at DESC, id DESC
                     LIMIT ?1",
                )
                .context("could not prepare the batch query")?;
            let rows = statement
                .query_map(params![limit], map_batch)
                .context("could not read scrape batches")?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("could not decode scrape batches")
        })
        .await
    }

    /// The files a batch produced, newest first.
    pub async fn batch_files(&self, batch_id: i64, limit: u32) -> Result<Vec<GalleryFile>> {
        self.with(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, batch_id, path, skipped FROM gallery_files
                     WHERE batch_id = ?1 ORDER BY id DESC LIMIT ?2",
                )
                .context("could not prepare the file query")?;
            let rows = statement
                .query_map(params![batch_id, limit], |row| {
                    Ok(GalleryFile {
                        id: row.get(0)?,
                        batch_id: row.get(1)?,
                        path: PathBuf::from(row.get::<_, String>(2)?),
                        skipped: row.get::<_, i64>(3)? != 0,
                    })
                })
                .context("could not read scraped files")?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("could not decode scraped files")
        })
        .await
    }

    /// Mark batches left `running` by a crash as failed. Call once at startup.
    pub async fn reconcile_orphans(&self) -> Result<usize> {
        let now = unix_now();
        self.with(move |connection| {
            let batches = connection
                .execute(
                    "UPDATE gallery_batches
                     SET state = 'failed',
                         error = COALESCE(error, 'Snatch exited while this batch was running'),
                         finished_at = ?1
                     WHERE state = 'running'",
                    params![now],
                )
                .context("could not reconcile interrupted batches")?;
            let jobs = connection
                .execute(
                    "UPDATE media_jobs
                     SET state = 'failed',
                         error = COALESCE(error, 'Snatch exited while this job was running'),
                         finished_at = ?1
                     WHERE state = 'running'",
                    params![now],
                )
                .context("could not reconcile interrupted media jobs")?;
            Ok(batches + jobs)
        })
        .await
    }

    // -- media jobs --------------------------------------------------------

    pub async fn create_media_job(
        &self,
        input: PathBuf,
        output: PathBuf,
        action: String,
    ) -> Result<i64> {
        let now = unix_now();
        self.with(move |connection| {
            connection
                .execute(
                    "INSERT INTO media_jobs (input, output, action, state, started_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        input.to_string_lossy(),
                        output.to_string_lossy(),
                        action,
                        JobState::Running.as_str(),
                        now
                    ],
                )
                .context("could not record the media job")?;
            Ok(connection.last_insert_rowid())
        })
        .await
    }

    pub async fn finish_media_job(
        &self,
        job_id: i64,
        state: JobState,
        error: Option<String>,
    ) -> Result<()> {
        let now = unix_now();
        self.with(move |connection| {
            connection
                .execute(
                    "UPDATE media_jobs SET state = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
                    params![job_id, state.as_str(), error, now],
                )
                .context("could not close the media job")?;
            Ok(())
        })
        .await
    }

    #[allow(dead_code, reason = "history query for a future Jobs view")]
    pub async fn recent_media_jobs(&self, limit: u32) -> Result<Vec<MediaJobRecord>> {
        self.with(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, input, output, action, state, error, started_at, finished_at
                     FROM media_jobs ORDER BY started_at DESC, id DESC LIMIT ?1",
                )
                .context("could not prepare the media job query")?;
            let rows = statement
                .query_map(params![limit], |row| {
                    Ok(MediaJobRecord {
                        id: row.get(0)?,
                        input: PathBuf::from(row.get::<_, String>(1)?),
                        output: PathBuf::from(row.get::<_, String>(2)?),
                        action: row.get(3)?,
                        state: JobState::parse(&row.get::<_, String>(4)?),
                        error: row.get(5)?,
                        started_at: row.get(6)?,
                        finished_at: row.get(7)?,
                    })
                })
                .context("could not read media jobs")?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("could not decode media jobs")
        })
        .await
    }
}

fn map_batch(row: &rusqlite::Row<'_>) -> rusqlite::Result<GalleryBatch> {
    Ok(GalleryBatch {
        id: row.get(0)?,
        url: row.get(1)?,
        destination: PathBuf::from(row.get::<_, String>(2)?),
        state: JobState::parse(&row.get::<_, String>(3)?),
        total: row.get::<_, i64>(4)?.max(0) as u64,
        downloaded: row.get::<_, i64>(5)?.max(0) as u64,
        skipped: row.get::<_, i64>(6)?.max(0) as u64,
        failed: row.get::<_, i64>(7)?.max(0) as u64,
        error: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS gallery_batches (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    url         TEXT    NOT NULL,
    destination TEXT    NOT NULL,
    state       TEXT    NOT NULL,
    total       INTEGER NOT NULL DEFAULT 0,
    downloaded  INTEGER NOT NULL DEFAULT 0,
    skipped     INTEGER NOT NULL DEFAULT 0,
    failed      INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE IF NOT EXISTS gallery_files (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id   INTEGER NOT NULL REFERENCES gallery_batches(id) ON DELETE CASCADE,
    path       TEXT    NOT NULL,
    skipped    INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS gallery_files_batch ON gallery_files (batch_id, id DESC);

CREATE TABLE IF NOT EXISTS media_jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    input       TEXT    NOT NULL,
    output      TEXT    NOT NULL,
    action      TEXT    NOT NULL,
    state       TEXT    NOT NULL,
    error       TEXT,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE INDEX IF NOT EXISTS media_jobs_recent ON media_jobs (started_at DESC, id DESC);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    async fn scratch(name: &str) -> Database {
        let path = std::env::temp_dir().join(format!("snatch-db-test-{name}.sqlite"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        Database::open(path).await.expect("the database opens")
    }

    #[tokio::test]
    async fn counts_downloads_skips_and_failures_separately() {
        let db = scratch("counts").await;
        let batch = db
            .create_batch(
                "https://example.com/gallery".into(),
                PathBuf::from("/tmp/x"),
            )
            .await
            .expect("batch is created");

        db.set_batch_total(batch, 4).await.expect("total is set");
        db.record_file(batch, PathBuf::from("/tmp/x/a.png"), false)
            .await
            .expect("file recorded");
        db.record_file(batch, PathBuf::from("/tmp/x/b.png"), false)
            .await
            .expect("file recorded");
        db.record_file(batch, PathBuf::from("/tmp/x/c.png"), true)
            .await
            .expect("skip recorded");
        db.record_failure(batch).await.expect("failure recorded");

        let stored = db
            .batch(batch)
            .await
            .expect("batch reads")
            .expect("batch exists");
        assert_eq!(stored.downloaded, 2);
        assert_eq!(stored.skipped, 1);
        assert_eq!(stored.failed, 1);
        assert_eq!(stored.handled(), 4);
        assert_eq!(stored.fraction(), Some(1.0));
        assert_eq!(stored.state, JobState::Running);

        let files = db.batch_files(batch, 10).await.expect("files read");
        assert_eq!(
            files.len(),
            3,
            "only real files are rows; failures are counters"
        );
    }

    #[tokio::test]
    async fn progress_is_unknown_until_the_total_arrives() {
        let db = scratch("unknown-total").await;
        let batch = db
            .create_batch("https://example.com/g".into(), PathBuf::from("/tmp/y"))
            .await
            .expect("batch is created");
        let stored = db.batch(batch).await.expect("reads").expect("exists");
        assert_eq!(
            stored.fraction(),
            None,
            "an unknown total must not render as 0% of nothing"
        );
    }

    #[tokio::test]
    async fn interrupted_work_is_reconciled_on_restart() {
        let db = scratch("orphans").await;
        let batch = db
            .create_batch("https://example.com/g".into(), PathBuf::from("/tmp/z"))
            .await
            .expect("batch is created");
        db.create_media_job(
            PathBuf::from("/tmp/in.mkv"),
            PathBuf::from("/tmp/out.mp3"),
            "extract-audio".into(),
        )
        .await
        .expect("job is created");

        let repaired = db.reconcile_orphans().await.expect("reconcile runs");
        assert_eq!(repaired, 2, "both the batch and the job were left running");

        let stored = db.batch(batch).await.expect("reads").expect("exists");
        assert_eq!(stored.state, JobState::Failed);
        assert!(stored.error.is_some());
        assert!(stored.finished_at.is_some());

        // A second pass has nothing left to do.
        assert_eq!(db.reconcile_orphans().await.expect("reconcile runs"), 0);
    }

    #[tokio::test]
    async fn finishing_a_batch_is_recorded() {
        let db = scratch("finish").await;
        let batch = db
            .create_batch("https://example.com/g".into(), PathBuf::from("/tmp/w"))
            .await
            .expect("batch is created");
        db.finish_batch(batch, JobState::Complete, None)
            .await
            .expect("batch closes");

        let stored = db.batch(batch).await.expect("reads").expect("exists");
        assert_eq!(stored.state, JobState::Complete);
        assert!(stored.state.is_terminal());
        assert!(stored.finished_at.is_some());

        let recent = db.recent_batches(10).await.expect("recent reads");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, batch);
    }
}
