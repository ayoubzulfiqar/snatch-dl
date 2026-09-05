//! The Downloads page: aria2 transfers plus the ffmpeg jobs they feed.
//!
//! Two lists live here. The upper one shows conversions in flight, because a
//! post-process job belongs next to the download that produced it rather than
//! on a page of its own. The lower one is the aria2 queue.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;

use super::format::{
    boxed_list, boxed_list_page, caption_label, control_button, heading_label, human_bytes,
    human_duration, plain_row, row_body,
};
use super::{PageSummary, Ui};
use crate::aria2::{DownloadStatus, QueueMove};
use crate::db::{JobState, NewHistoryEntry, Origin};
use crate::processor::{MediaAction, MediaEvent, MediaJob, format_duration};
use crate::wget::WgetEvent;
use crate::ytdlp::VideoEvent;
use crate::{adw, gtk};
use gtk::{gio, glib};

const PAGE_EMPTY: &str = "empty";
const PAGE_LIST: &str = "list";

/// Extensions we offer post-processing for. Anything else gets no menu.
const MEDIA_EXTENSIONS: [&str; 14] = [
    "mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", "ts", "mpg", "mpeg", "m4a", "aac",
    "opus",
];

fn is_media(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| MEDIA_EXTENSIONS.contains(&extension.as_str()))
}

pub struct DownloadsPage {
    /// GIDs already written to history, so a repeated poll does not re-record.
    recorded: RefCell<HashSet<String>>,
    graph: super::graph::Bandwidth,
    root: gtk::Box,
    stack: gtk::Stack,
    list: gtk::ListBox,
    jobs_list: gtk::ListBox,
    jobs_frame: gtk::Box,
    rows: RefCell<HashMap<String, Rc<Row>>>,
    jobs: RefCell<HashMap<i64, Rc<JobRow>>>,
    /// Extractions, keyed separately: their ids come from the archive
    /// queue's own counter and would collide with the database ids the
    /// media and video jobs use.
    archive_jobs: RefCell<HashMap<i64, Rc<JobRow>>>,
    /// Crawls, keyed separately for the same reason.
    mirror_jobs: RefCell<HashMap<i64, Rc<JobRow>>>,
    /// Recordings the user asked to have converted once they finish. The
    /// conversion cannot start any earlier: ffmpeg is still writing the file.
    convert_when_done: RefCell<HashSet<i64>>,
    summary: RefCell<PageSummary>,
}

impl DownloadsPage {
    pub fn new() -> Self {
        let list = boxed_list();
        let jobs_list = boxed_list();

        let jobs_heading = gtk::Label::builder()
            .xalign(0.0)
            .label("Tasks")
            .css_classes(["snatch-section-heading"])
            .build();

        let jobs_frame = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_bottom(12)
            .visible(false)
            .css_classes(["snatch-tasks"])
            .build();
        jobs_frame.append(&jobs_heading);
        jobs_frame.append(&jobs_list);

        let graph = super::graph::Bandwidth::new();

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .build();
        column.append(graph.widget());
        column.append(&jobs_frame);
        column.append(&list);

        let scroller = boxed_list_page(&column);

        let empty = adw::StatusPage::builder()
            .icon_name("folder-download-symbolic")
            .title("No Downloads Yet")
            .description(
                "Links you click in your browser land here.\n\
                 You can also paste one with the + button.",
            )
            .vexpand(true)
            .build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&empty, Some(PAGE_EMPTY));
        stack.add_named(&scroller, Some(PAGE_LIST));
        stack.set_visible_child_name(PAGE_EMPTY);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&stack);

        Self {
            root,
            stack,
            list,
            jobs_list,
            jobs_frame,
            recorded: RefCell::new(HashSet::new()),
            graph,
            rows: RefCell::new(HashMap::new()),
            jobs: RefCell::new(HashMap::new()),
            archive_jobs: RefCell::new(HashMap::new()),
            mirror_jobs: RefCell::new(HashMap::new()),
            convert_when_done: RefCell::new(HashSet::new()),
            summary: RefCell::new(PageSummary::default()),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn summary(&self) -> PageSummary {
        *self.summary.borrow()
    }

    /// Reconcile the visible rows with what aria2 reports. aria2 is the single
    /// source of truth; the UI holds no download state of its own.
    pub fn apply(&self, ui: &Rc<Ui>, downloads: &[DownloadStatus]) -> PageSummary {
        let mut seen = HashSet::with_capacity(downloads.len());
        let mut summary = PageSummary {
            total: downloads.len(),
            ..PageSummary::default()
        };

        {
            let mut rows = self.rows.borrow_mut();

            for download in downloads {
                seen.insert(download.gid.as_str());
                let row = rows.entry(download.gid.clone()).or_insert_with(|| {
                    let row = Row::new(&download.gid, ui);
                    self.list.append(&row.root);
                    row
                });
                row.update(download, ui.scheduled_minute_for(&download.gid));
                self.record_if_finished(ui, download);

                if download.is_active() {
                    summary.active += 1;
                    summary.speed += download.download_speed;
                }
            }

            rows.retain(|gid, row| {
                let keep = seen.contains(gid.as_str());
                if !keep {
                    self.list.remove(&row.root);
                }
                keep
            });
        }

        // One sample per poll keeps the graph in step with the numbers above
        // it. Upload is always zero here: this page is aria2's HTTP queue, and
        // seeding lives on the Torrents page.
        self.graph.push(summary.speed, 0);

        self.refresh_page();
        *self.summary.borrow_mut() = summary;
        summary
    }

    /// Write a download to history the first time it is seen finished.
    ///
    /// The poll returns the same completed entry until the user clears it, and
    /// the database ignores a duplicate engine id anyway, but skipping the
    /// round trip keeps a finished queue from issuing an insert twice a second.
    fn record_if_finished(&self, ui: &Rc<Ui>, download: &DownloadStatus) {
        if !download.is_finished() {
            return;
        }
        if !self.recorded.borrow_mut().insert(download.gid.clone()) {
            return;
        }

        if download.is_complete() {
            ui.notify(
                "Download finished",
                &format!("{} is ready", download.display_name()),
            );
            if let Some(path) = download.path() {
                ui.extract_if_archive(PathBuf::from(path));
            }
        }

        let entry = NewHistoryEntry {
            engine_id: format!("aria2:{}", download.gid),
            url: download.source_uri().unwrap_or_default().to_owned(),
            filename: download.display_name(),
            path: download.path().map(PathBuf::from),
            size: download.total_length,
            origin: Origin::Aria2,
            state: if download.is_complete() {
                JobState::Complete
            } else if download.is_error() {
                JobState::Failed
            } else {
                JobState::Cancelled
            },
            error: download.error_message.clone(),
        };

        let backend = ui.backend().clone();
        backend.clone().spawn(async move {
            if let Err(error) = backend.db.record_download(entry).await {
                log::warn!("could not record a download in history: {error:#}");
            }
        });
    }

    /// Reflect one ffmpeg event in the Processing list.
    pub fn handle_media(&self, ui: &Rc<Ui>, event: MediaEvent) {
        match event {
            MediaEvent::Started { job_id, label } => {
                let mut jobs = self.jobs.borrow_mut();
                let row = jobs.entry(job_id).or_insert_with(|| {
                    let row = JobRow::new(label);
                    row.on_cancel(ui, job_id, false);
                    self.jobs_list.append(&row.root);
                    row
                });
                row.set_label(label);
                drop(jobs);
                self.refresh_page();
            }
            MediaEvent::Progress { job_id, progress } => {
                if let Some(row) = self.jobs.borrow().get(&job_id) {
                    row.update(&progress);
                }
            }
            MediaEvent::Finished { job_id, output } => {
                self.drop_job(job_id);
                ui.toast(&format!(
                    "Finished {}",
                    output
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| output.display().to_string())
                ));
            }
            MediaEvent::Failed { job_id, error } => {
                self.drop_job(job_id);
                ui.toast(&format!("Conversion failed: {error}"));
            }
        }
        self.refresh_page();
    }

    /// Reflect one extraction in the task list.
    ///
    /// Unpacking is the same shape as a conversion — a subprocess turning one
    /// file into others — so it reuses the row, but not the cancel button:
    /// stopping an extraction half way leaves a directory of partial files
    /// that looks like a successful one.
    pub fn handle_archive(&self, event: &crate::archive::ArchiveEvent) {
        use crate::archive::ArchiveEvent;
        match event {
            ArchiveEvent::Started { job_id, name, .. } => {
                let mut jobs = self.archive_jobs.borrow_mut();
                let row = jobs.entry(*job_id).or_insert_with(|| {
                    let row = JobRow::new("Unpacking");
                    row.hide_cancel();
                    self.jobs_list.append(&row.root);
                    row
                });
                row.set_subtitle(name);
                drop(jobs);
                self.refresh_page();
            }
            ArchiveEvent::Progress { job_id, percent } => {
                if let Some(row) = self.archive_jobs.borrow().get(job_id) {
                    row.set_percent(*percent);
                }
            }
            ArchiveEvent::NeedsPassword { job_id, .. }
            | ArchiveEvent::Finished { job_id, .. }
            | ArchiveEvent::Failed { job_id, .. } => {
                if let Some(row) = self.archive_jobs.borrow_mut().remove(job_id) {
                    self.jobs_list.remove(&row.root);
                }
                self.refresh_page();
            }
        }
    }

    /// Reflect one crawl in the task list.
    pub fn handle_mirror(&self, ui: &Rc<Ui>, event: &crate::mirror::MirrorEvent) {
        use crate::mirror::MirrorEvent;
        match event {
            MirrorEvent::Started { job_id, host } => {
                let mut jobs = self.mirror_jobs.borrow_mut();
                let row = jobs.entry(*job_id).or_insert_with(|| {
                    let row = JobRow::new("Grabbing site");
                    row.on_cancel_mirror(ui, *job_id);
                    self.jobs_list.append(&row.root);
                    row
                });
                row.set_subtitle(host);
                drop(jobs);
                self.refresh_page();
            }
            MirrorEvent::Progress {
                job_id,
                saved,
                discovered,
                current,
            } => {
                if let Some(row) = self.mirror_jobs.borrow().get(job_id) {
                    // A crawl never knows its total until it ends, so the bar
                    // pulses and the count carries the real information.
                    row.pulse();
                    row.set_subtitle(&format!("{saved} of {discovered} found — {current}"));
                }
            }
            MirrorEvent::Finished { job_id, .. } | MirrorEvent::Failed { job_id, .. } => {
                if let Some(row) = self.mirror_jobs.borrow_mut().remove(job_id) {
                    self.jobs_list.remove(&row.root);
                }
                self.refresh_page();
            }
        }
    }

    /// Reflect one yt-dlp event in the task list. Video jobs share the row
    /// widget with conversions: both are "a subprocess producing a file".
    pub fn handle_video(&self, ui: &Rc<Ui>, event: VideoEvent) {
        match event {
            VideoEvent::Started {
                job_id,
                url,
                recording,
            } => {
                let mut jobs = self.jobs.borrow_mut();
                let row = jobs.entry(job_id).or_insert_with(|| {
                    let row = JobRow::new(if recording {
                        "Recording stream"
                    } else {
                        "Extracting video"
                    });
                    if recording {
                        row.on_stop_recording(ui, job_id);
                    } else {
                        row.on_cancel(ui, job_id, true);
                    }
                    self.jobs_list.append(&row.root);
                    row
                });
                row.set_subtitle(&url);
                drop(jobs);
                self.refresh_page();
            }
            VideoEvent::Title { job_id, title } => {
                if let Some(row) = self.jobs.borrow().get(&job_id) {
                    let recording = ui.backend().video.is_recording(job_id);
                    row.set_label(&format!(
                        "{} {title}",
                        if recording { "Recording" } else { "Extracting" }
                    ));
                }
            }
            VideoEvent::Progress { job_id, progress } => {
                if let Some(row) = self.jobs.borrow().get(&job_id) {
                    row.update_video(&progress);
                }
            }
            VideoEvent::Finished { job_id, output } => {
                self.drop_job(job_id);
                let convert = self.convert_when_done.borrow_mut().remove(&job_id);
                match output {
                    Some(path) => {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        // The recording is closed and playable now; this only
                        // repackages it, and it could not have run any sooner.
                        if convert {
                            convert_to_mp4(ui, path);
                            ui.toast(&format!("Saved {name}, converting to MP4"));
                        } else {
                            ui.toast(&format!("Saved {name}"));
                        }
                    }
                    None => ui.toast("Extraction finished"),
                }
            }
            VideoEvent::Failed { job_id, error } => {
                self.drop_job(job_id);
                self.convert_when_done.borrow_mut().remove(&job_id);
                ui.toast(&format!("Extraction failed: {error}"));
            }
        }
    }

    /// Reflect one wget event. aria2 downloads are reconciled from snapshots,
    /// but wget has no daemon to poll, so its rows are event-driven and live
    /// in the task list alongside conversions.
    pub fn handle_wget(&self, ui: &Rc<Ui>, event: WgetEvent) {
        // ffmpeg job ids are database row ids and always positive, so wget
        // rows are keyed negatively and the two can share one map.
        let key = |job_id: i64| -(job_id.saturating_abs() + 1);

        match event {
            WgetEvent::Started {
                job_id,
                name,
                total,
            } => {
                let mut jobs = self.jobs.borrow_mut();
                let row = jobs.entry(key(job_id)).or_insert_with(|| {
                    let row = JobRow::new(&format!("Downloading {name}"));
                    row.on_cancel_wget(ui, job_id);
                    self.jobs_list.append(&row.root);
                    row
                });
                row.set_label(&format!("Downloading {name}"));
                // Show the size before the first byte lands.
                row.update_bytes(0, total, 0);
                drop(jobs);
                self.refresh_page();
            }
            WgetEvent::Progress {
                job_id,
                downloaded,
                total,
                bytes_per_second,
            } => {
                if let Some(row) = self.jobs.borrow().get(&key(job_id)) {
                    row.update_bytes(downloaded, total, bytes_per_second);
                }
            }
            WgetEvent::Finished { job_id, path } => {
                self.drop_job(key(job_id));
                ui.toast(&format!(
                    "Saved {}",
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                ));
            }
            WgetEvent::Failed { job_id, error } => {
                self.drop_job(key(job_id));
                ui.toast(&format!("Download failed: {error}"));
            }
        }
    }

    /// How many task rows the list is holding, across every engine that
    /// shares it: conversions and video jobs, unpacking, and site grabs.
    fn task_count(&self) -> usize {
        self.jobs.borrow().len()
            + self.archive_jobs.borrow().len()
            + self.mirror_jobs.borrow().len()
    }

    /// Show the empty page only when there is genuinely nothing here.
    ///
    /// The page holds two lists: aria2's downloads and a task list shared by
    /// every other engine. Deciding on the downloads alone hid the task list
    /// the moment aria2 had nothing -- which is most of the time, because a
    /// recording, an extraction or a conversion is not an aria2 download. The
    /// row was appended, the job ran, and the next poll swapped the page out
    /// from under it, so what the user saw was nothing at all.
    fn refresh_page(&self) {
        let tasks = self.task_count();
        self.jobs_frame.set_visible(tasks > 0);
        self.stack
            .set_visible_child_name(if self.rows.borrow().is_empty() && tasks == 0 {
                PAGE_EMPTY
            } else {
                PAGE_LIST
            });
    }

    fn drop_job(&self, job_id: i64) {
        if let Some(row) = self.jobs.borrow_mut().remove(&job_id) {
            self.jobs_list.remove(&row.root);
        }
        self.refresh_page();
    }
}

/// A conversion in progress.
struct JobRow {
    root: gtk::ListBoxRow,
    title: gtk::Label,
    detail: gtk::Label,
    progress: gtk::ProgressBar,
    /// Only a recording can be paused, so this stays hidden for every other
    /// kind of job rather than sitting there doing nothing.
    pause: gtk::Button,
    cancel: gtk::Button,
}

impl JobRow {
    fn new(label: &str) -> Rc<Self> {
        let title = heading_label();
        title.set_text(label);
        title.add_css_class("snatch-title");
        let detail = caption_label(false);
        detail.add_css_class("snatch-detail");
        let progress = gtk::ProgressBar::builder().hexpand(true).build();
        let pause = control_button("media-playback-pause-symbolic", "Pause this recording");
        pause.set_visible(false);
        let cancel = control_button("process-stop-symbolic", "Stop this job");

        let heading = gtk::Box::builder().spacing(12).build();
        heading.append(&title);
        heading.append(&pause);
        heading.append(&cancel);

        let body = row_body();
        body.append(&heading);
        body.append(&progress);
        body.append(&detail);

        let root = plain_row(&body);
        root.add_css_class("snatch-row");

        Rc::new(Self {
            root,
            title,
            detail,
            progress,
            pause,
            cancel,
        })
    }

    /// Stop a wget download.
    fn on_cancel_wget(&self, ui: &Rc<Ui>, job_id: i64) {
        let weak = Rc::downgrade(ui);
        self.cancel.connect_clicked(move |button| {
            let Some(ui) = weak.upgrade() else { return };
            ui.backend().wget.cancel(job_id);
            button.set_sensitive(false);
            ui.toast("Stopping the download");
        });
    }

    /// Stop a live recording, keeping what has been written.
    ///
    /// A recording is not a download that failed halfway: everything already
    /// on disk is watchable, and stopping is the only way one ever ends. So
    /// the button asks what to do with the file rather than throwing it away.
    fn on_stop_recording(&self, ui: &Rc<Ui>, job_id: i64) {
        let weak = Rc::downgrade(ui);
        self.cancel.connect_clicked(move |button| {
            let Some(ui) = weak.upgrade() else { return };
            present_stop_recording(&ui, job_id, button.clone());
        });

        // Pausing stops capturing and starts again where the button says so.
        // What was missed while paused is missing from the result, which for a
        // broadcast is the only thing pausing can mean.
        self.pause.set_visible(true);
        let weak = Rc::downgrade(ui);
        let paused = std::cell::Cell::new(false);
        self.pause.connect_clicked(move |button| {
            let Some(ui) = weak.upgrade() else { return };
            let wanted = !paused.get();
            if !ui.backend().video.set_paused(job_id, wanted) {
                ui.toast("That recording has already finished");
                return;
            }
            paused.set(wanted);
            button.set_icon_name(if wanted {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
            button.set_tooltip_text(Some(if wanted {
                "Carry on recording"
            } else {
                "Pause this recording"
            }));
            ui.toast(if wanted {
                "Paused — what happens now will not be recorded"
            } else {
                "Recording again"
            });
        });
    }

    /// Wire the stop button to whichever engine owns this job.
    fn on_cancel(&self, ui: &Rc<Ui>, job_id: i64, video: bool) {
        let weak = Rc::downgrade(ui);
        self.cancel.connect_clicked(move |button| {
            let Some(ui) = weak.upgrade() else { return };
            if video {
                ui.backend().video.cancel(job_id);
            } else {
                // ffmpeg jobs are queued serially and are not individually
                // abortable yet; say so rather than doing nothing.
                ui.toast("A conversion cannot be stopped once it has started");
                return;
            }
            button.set_sensitive(false);
            ui.toast("Stopping");
        });
    }

    fn set_label(&self, label: &str) {
        self.title.set_text(label);
    }

    fn set_subtitle(&self, text: &str) {
        self.detail.set_text(text);
    }

    /// A crawl has no total until it finishes, so the bar pulses.
    fn pulse(&self) {
        self.progress.pulse();
    }

    /// Stop a crawl. Unlike extraction, a partial crawl is just fewer files.
    fn on_cancel_mirror(&self, ui: &Rc<Ui>, job_id: i64) {
        let backend = ui.backend().clone();
        self.cancel.connect_clicked(move |_| {
            backend.mirrors.cancel(job_id);
        });
    }

    /// Extraction has no safe midpoint to stop at, so it offers no cancel.
    fn hide_cancel(&self) {
        self.cancel.set_visible(false);
    }

    /// Whole-percent progress, which is all an extractor reports.
    fn set_percent(&self, percent: u8) {
        self.progress
            .set_fraction(f64::from(percent.min(100)) / 100.0);
    }

    /// Byte-counted progress, used by the wget engine.
    fn update_bytes(&self, downloaded: u64, total: Option<u64>, bytes_per_second: u64) {
        match total.filter(|total| *total > 0) {
            Some(total) => self
                .progress
                .set_fraction((downloaded as f64 / total as f64).clamp(0.0, 1.0)),
            // The server refused a HEAD or gave no length.
            None => self.progress.pulse(),
        }

        let mut parts = vec![match total {
            Some(total) => format!("{} of {}", human_bytes(downloaded), human_bytes(total)),
            None => format!("{} downloaded", human_bytes(downloaded)),
        }];
        if bytes_per_second > 0 {
            parts.push(format!("{}/s", human_bytes(bytes_per_second)));
            if let Some(total) = total.filter(|total| *total > downloaded) {
                parts.push(format!(
                    "{} left",
                    human_duration((total - downloaded) / bytes_per_second.max(1))
                ));
            }
        }
        self.detail.set_text(&parts.join(" · "));
    }

    fn update_video(&self, progress: &crate::ytdlp::VideoProgress) {
        match progress.fraction() {
            Some(fraction) => self.progress.set_fraction(fraction),
            // A live stream has no total; show motion, not a false 0%.
            None => self.progress.pulse(),
        }

        let mut parts = Vec::new();
        parts.push(match progress.total {
            Some(total) => format!(
                "{} of {}",
                human_bytes(progress.downloaded),
                human_bytes(total)
            ),
            None => format!("{} downloaded", human_bytes(progress.downloaded)),
        });
        if let Some((index, count)) = progress.fragment {
            parts.push(format!("fragment {index}/{count}"));
        }
        if let Some(speed) = progress.speed {
            parts.push(format!("{}/s", human_bytes(speed as u64)));
        }
        if let Some(eta) = progress.eta_seconds.filter(|eta| *eta > 0) {
            parts.push(format!("{} left", human_duration(eta)));
        }
        self.detail.set_text(&parts.join(" · "));
    }

    fn update(&self, progress: &crate::processor::MediaProgress) {
        match progress.fraction() {
            Some(fraction) => self.progress.set_fraction(fraction),
            // ffprobe could not read a duration: show motion, not a false 0%.
            None => self.progress.pulse(),
        }

        let mut parts = vec![format_duration(progress.elapsed)];
        if let Some(total) = progress.total {
            parts.push(format!("of {}", format_duration(total)));
        }
        if let Some(speed) = progress.speed {
            parts.push(format!("{speed:.1}x"));
        }
        if let Some(eta) = progress.eta() {
            parts.push(format!("{} left", human_duration(eta.as_secs())));
        }
        if progress.output_bytes > 0 {
            parts.push(human_bytes(progress.output_bytes));
        }
        self.detail.set_text(&parts.join(" · "));
    }
}

/// The mutable bits a row's button callbacks need to read at click time.
#[derive(Default)]
struct RowState {
    paused: bool,
    finished: bool,
    complete: bool,
    name: String,
    path: Option<String>,
    folder: Option<String>,
}

/// One download: title, progress bar, live detail line and controls.
struct Row {
    root: gtk::ListBoxRow,
    title: gtk::Label,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    detail: gtk::Label,
    toggle: gtk::Button,
    up: gtk::Button,
    down: gtk::Button,
    open: gtk::Button,
    process: gtk::MenuButton,
    state: Rc<RefCell<RowState>>,
}

impl Row {
    fn new(gid: &str, ui: &Rc<Ui>) -> Rc<Self> {
        let state = Rc::new(RefCell::new(RowState::default()));

        let title = heading_label();
        title.add_css_class("snatch-title");
        let status = caption_label(true);
        status.add_css_class("snatch-status");
        let progress = gtk::ProgressBar::builder().hexpand(true).build();
        let detail = caption_label(false);
        detail.add_css_class("snatch-detail");

        // Queue order only means anything for a download that is waiting, so
        // these appear and disappear with that state.
        let up = control_button("go-up-symbolic", "Move up the queue");
        let down = control_button("go-down-symbolic", "Move down the queue");
        up.set_visible(false);
        down.set_visible(false);

        let toggle = control_button("media-playback-pause-symbolic", "Pause");
        // Shown from the start: knowing where a download is going is useful
        // while it runs, not only once it has finished.
        let open = control_button("folder-open-symbolic", "Open the destination folder");
        let remove = control_button("user-trash-symbolic", "Remove");

        // Actions for one download: conversions once it is a media file, and
        // proxy routing at any time.
        let process = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("More actions for this download")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .visible(false)
            .build();
        process.set_menu_model(Some(&post_process_menu(gid)));

        let top = gtk::Box::builder().spacing(12).build();
        top.append(&title);
        top.append(&status);

        let bottom = gtk::Box::builder().spacing(6).build();
        bottom.append(&detail);
        bottom.append(&up);
        bottom.append(&down);
        bottom.append(&toggle);
        bottom.append(&process);
        bottom.append(&open);
        bottom.append(&remove);

        let body = row_body();
        body.append(&top);
        body.append(&progress);
        body.append(&bottom);
        let root = plain_row(&body);
        root.add_css_class("snatch-row");

        // A right-click anywhere on the row opens the same menu.
        let gesture = gtk::GestureClick::builder()
            .button(gdk_button_secondary())
            .build();
        gesture.connect_pressed({
            let process = process.clone();
            move |_, _, _, _| {
                if process.is_visible() {
                    process.popup();
                }
            }
        });
        root.add_controller(gesture);

        toggle.connect_clicked({
            let backend = ui.backend().clone();
            let weak = Rc::downgrade(ui);
            let state = Rc::clone(&state);
            let gid = gid.to_owned();
            move |button| {
                let resume = state.borrow().paused;
                button.set_sensitive(false);

                let button = button.clone();
                let backend = backend.clone();
                let weak = weak.clone();
                let gid = gid.clone();

                glib::spawn_future_local(async move {
                    let client = backend.aria2.clone();
                    let result = backend
                        .offload(async move {
                            if resume {
                                client.unpause(&gid).await
                            } else {
                                client.pause(&gid).await
                            }
                        })
                        .await;

                    button.set_sensitive(true);
                    if let Err(error) = result
                        && let Some(ui) = weak.upgrade()
                    {
                        ui.toast(&format!("{error:#}"));
                    }
                });
            }
        });

        open.connect_clicked({
            let weak = Rc::downgrade(ui);
            let state = Rc::clone(&state);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let target = {
                    let state = state.borrow();
                    state.path.clone().or_else(|| state.folder.clone())
                };
                match target {
                    Some(target) => ui.reveal(Path::new(&target)),
                    None => ui.toast("aria2 has not chosen a location yet"),
                }
            }
        });

        remove.connect_clicked({
            let weak = Rc::downgrade(ui);
            let state = Rc::clone(&state);
            let gid = gid.to_owned();
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let (finished, name, path) = {
                    let state = state.borrow();
                    (state.finished, state.name.clone(), state.path.clone())
                };
                confirm_remove(&ui, &gid, &name, finished, path);
            }
        });

        // Each row installs its own actions, scoped by GID so two rows never
        // collide in the window's action map.
        install_row_actions(ui, gid, &state);

        for (button, movement) in [(&up, QueueMove::Up), (&down, QueueMove::Down)] {
            button.connect_clicked({
                let backend = ui.backend().clone();
                let weak = Rc::downgrade(ui);
                let gid = gid.to_owned();
                move |_| {
                    let backend = backend.clone();
                    let weak = weak.clone();
                    let gid = gid.clone();
                    glib::spawn_future_local(async move {
                        let client = backend.aria2.clone();
                        let result = backend
                            .offload(async move { client.move_in_queue(&gid, movement).await })
                            .await;
                        if let Err(error) = result
                            && let Some(ui) = weak.upgrade()
                        {
                            ui.toast(&format!("{error:#}"));
                        }
                    });
                }
            });
        }

        Rc::new(Self {
            root,
            title,
            status,
            progress,
            detail,
            toggle,
            up,
            down,
            open,
            process,
            state,
        })
    }

    fn update(&self, download: &DownloadStatus, scheduled: Option<u32>) {
        let name = download.display_name();
        self.title.set_text(&name);
        self.title.set_tooltip_text(download.path());

        {
            let mut state = self.state.borrow_mut();
            state.paused = download.is_paused();
            state.finished = download.is_finished();
            state.complete = download.is_complete();
            state.name = name;
            state.path = download.path().map(str::to_owned);
            state.folder = download.folder().map(str::to_owned);
        }

        self.progress.set_fraction(download.fraction());

        // The row class drives the progress-bar colour; the pill class drives
        // the label. Both are replaced wholesale so states never accumulate.
        let (label, pill, row_state): (&str, &[&str], &str) = if download.is_complete() {
            ("Completed", &["snatch-status", "done"], "done")
        } else if download.is_error() {
            ("Failed", &["snatch-status", "failed"], "failed")
        } else if download.is_paused() {
            ("Paused", &["snatch-status"], "paused")
        } else if download.is_waiting() {
            ("Queued", &["snatch-status"], "")
        } else if download.is_active() {
            ("Downloading", &["snatch-status", "active"], "")
        } else {
            ("Cancelled", &["snatch-status"], "")
        };
        // A download waiting for its own start time is paused as far as aria2
        // is concerned, but "Paused" alone reads as something the user did and
        // leaves them wondering why it never resumes.
        match scheduled.filter(|_| download.is_paused()) {
            Some(minute) => self.status.set_text(&format!(
                "Starts {}",
                crate::settings::format_local_hhmm(minute)
            )),
            None => self.status.set_text(label),
        }
        self.status.set_css_classes(pill);
        self.root.set_css_classes(&["snatch-row", row_state]);
        self.detail.set_text(&detail_line(download));

        // Only a queued download has a position to change.
        let queued = download.is_waiting();
        self.up.set_visible(queued);
        self.down.set_visible(queued);

        let running = !download.is_finished();
        self.toggle.set_visible(running);
        if running {
            let (icon, tooltip) = if download.is_paused() {
                ("media-playback-start-symbolic", "Resume")
            } else {
                ("media-playback-pause-symbolic", "Pause")
            };
            self.toggle.set_icon_name(icon);
            self.toggle.set_tooltip_text(Some(tooltip));
        }

        self.open
            .set_visible(download.path().is_some() || download.folder().is_some());
        self.open.set_tooltip_text(Some(if download.is_complete() {
            "Show the file in the file manager"
        } else {
            "Open the destination folder"
        }));

        // Offering "Extract audio" on a half-downloaded ZIP would just produce
        // an ffmpeg error, so the menu is gated on a finished media file.
        let processable =
            download.is_complete() && download.path().map(Path::new).is_some_and(is_media);
        self.process.set_visible(processable);
    }
}

fn gdk_button_secondary() -> u32 {
    // GDK_BUTTON_SECONDARY
    3
}

fn post_process_menu(gid: &str) -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(
        Some("Extract Audio (MP3)"),
        Some(&format!("win.extract-audio::{gid}")),
    );
    menu.append(
        Some("Convert to MP4"),
        Some(&format!("win.convert-mp4::{gid}")),
    );
    menu.append(Some("Trim…"), Some(&format!("win.trim::{gid}")));
    menu.append(Some("Mux With Audio…"), Some(&format!("win.mux::{gid}")));
    menu
}

/// Install the three post-process actions for one row.
///
/// GTK actions are addressed by name plus a string parameter, so all rows share
/// three action names and pass their own GID as the parameter. Installing them
/// once per row would be wrong, so this is a no-op after the first row.
fn install_row_actions(ui: &Rc<Ui>, _gid: &str, _state: &Rc<RefCell<RowState>>) {
    let window = ui.window();
    if window.lookup_action("extract-audio").is_some() {
        return;
    }

    for (name, action) in [
        (
            "extract-audio",
            MediaAction::ExtractAudio { bitrate_kbps: 192 },
        ),
        ("convert-mp4", MediaAction::ConvertToMp4),
    ] {
        let simple = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(ui);
        let action = action.clone();
        simple.connect_activate(move |_, parameter| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(gid) = parameter.and_then(|value| value.str().map(str::to_owned)) else {
                return;
            };
            start_post_process(&ui, &gid, action.clone());
        });
        window.add_action(&simple);
    }

    let trim = gio::SimpleAction::new("trim", Some(glib::VariantTy::STRING));
    let weak = Rc::downgrade(ui);
    trim.connect_activate(move |_, parameter| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(gid) = parameter.and_then(|value| value.str().map(str::to_owned)) else {
            return;
        };
        present_trim_dialog(&ui, &gid);
    });
    window.add_action(&trim);

    for (name, movement) in [
        ("queue-top", QueueMove::Top),
        ("queue-bottom", QueueMove::Bottom),
    ] {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(ui);
        action.connect_activate(move |_, parameter| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(gid) = parameter.and_then(|value| value.str().map(str::to_owned)) else {
                return;
            };
            let backend = ui.backend().clone();
            let inner = Rc::downgrade(&ui);
            glib::spawn_future_local(async move {
                let client = backend.aria2.clone();
                let result = backend
                    .offload(async move { client.move_in_queue(&gid, movement).await })
                    .await;
                if let Err(error) = result
                    && let Some(ui) = inner.upgrade()
                {
                    ui.toast(&format!("{error:#}"));
                }
            });
        });
        window.add_action(&action);
    }

    let route = gio::SimpleAction::new("route", Some(glib::VariantTy::STRING));
    let weak = Rc::downgrade(ui);
    route.connect_activate(move |_, parameter| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(gid) = parameter.and_then(|value| value.str().map(str::to_owned)) else {
            return;
        };
        present_proxy_picker(&ui, &gid);
    });
    window.add_action(&route);

    let mux = gio::SimpleAction::new("mux", Some(glib::VariantTy::STRING));
    let weak = Rc::downgrade(ui);
    mux.connect_activate(move |_, parameter| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(gid) = parameter.and_then(|value| value.str().map(str::to_owned)) else {
            return;
        };
        choose_audio_and_mux(&ui, &gid);
    });
    window.add_action(&mux);
}

/// Pin one download to a proxy, or clear its assignment.
///
/// aria2 reads the proxy when a download is added, so an existing transfer
/// keeps the route it started with; the dialog says so rather than implying
/// the change is retroactive.
fn present_proxy_picker(ui: &Rc<Ui>, gid: &str) {
    let proxies = ui.backend().proxies.list();
    if proxies.is_empty() {
        ui.toast("No proxies configured — add one in Proxy Settings");
        return;
    }

    let mut labels = vec!["Direct connection".to_owned()];
    labels.extend(proxies.iter().map(|(proxy, _)| {
        if proxy.supports(crate::network::Engine::Aria2) {
            proxy.label.clone()
        } else {
            // aria2 has no SOCKS support, so naming the reason beats letting
            // the user pick something that will be refused.
            format!("{} (SOCKS — not usable for downloads)", proxy.label)
        }
    }));
    let names: Vec<&str> = labels.iter().map(String::as_str).collect();

    let current = ui.backend().proxies.resolve(gid);
    let selected = current
        .as_ref()
        .and_then(|proxy| proxies.iter().position(|(p, _)| p.label == proxy.label))
        .map(|index| index as u32 + 1)
        .unwrap_or(0);

    let chooser = gtk::DropDown::from_strings(&names);
    chooser.set_selected(selected);

    let dialog = adw::AlertDialog::builder()
        .heading("Route This Download")
        .body(
            "aria2 picks up a proxy when a download starts, so this applies from \
             the next time it is queued or resumed.",
        )
        .extra_child(&chooser)
        .build();
    dialog.add_responses(&[("close", "Cancel"), ("save", "Apply")]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("close");

    let weak = Rc::downgrade(ui);
    let gid = gid.to_owned();
    dialog.connect_response(None, move |_, response| {
        if response != "save" {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        let index = chooser.selected() as usize;
        let choice = if index == 0 {
            None
        } else {
            proxies.get(index - 1).map(|(proxy, _)| proxy.label.clone())
        };
        match ui.backend().proxies.assign(&gid, choice.as_deref()) {
            Ok(()) => {
                let message = match &choice {
                    Some(label) => format!("This download will use {label}"),
                    None => "This download will connect directly".to_owned(),
                };
                ui.toast(&message);
            }
            Err(error) => ui.toast(&format!("{error:#}")),
        }
    });

    dialog.present(Some(ui.window()));
}

/// Ask for an audio file, then queue a stream-copy mux with the video.
fn choose_audio_and_mux(ui: &Rc<Ui>, gid: &str) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Audio"));
    for pattern in [
        "*.m4a", "*.aac", "*.mp3", "*.opus", "*.ogg", "*.flac", "*.wav",
    ] {
        filter.add_pattern(pattern);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let chooser = gtk::FileDialog::builder()
        .title("Choose the audio track to mux in")
        .filters(&filters)
        .modal(true)
        .build();

    let weak = Rc::downgrade(ui);
    let gid = gid.to_owned();
    chooser.open(Some(ui.window()), gio::Cancellable::NONE, move |result| {
        let Some(ui) = weak.upgrade() else { return };
        match result {
            Ok(file) => match file.path() {
                Some(audio) => start_post_process(&ui, &gid, MediaAction::Mux { audio }),
                None => ui.toast("That file has no local path"),
            },
            // Dismissing the chooser is not an error worth reporting.
            Err(error) if error.matches(gtk::DialogError::Dismissed) => {}
            Err(error) => ui.toast(&format!("Could not choose a file: {error}")),
        }
    });
}

/// Look the download's path up from aria2, then queue the ffmpeg job.
fn start_post_process(ui: &Rc<Ui>, gid: &str, action: MediaAction) {
    let weak = Rc::downgrade(ui);
    let backend = ui.backend().clone();
    let gid = gid.to_owned();

    glib::spawn_future_local(async move {
        let client = backend.aria2.clone();
        let lookup = backend
            .offload(async move { client.snapshot().await })
            .await;

        let Some(ui) = weak.upgrade() else { return };
        let input = match lookup {
            Ok(list) => list
                .into_iter()
                .find(|download| download.gid == gid)
                .and_then(|download| download.path().map(PathBuf::from)),
            Err(error) => {
                ui.toast(&format!("{error:#}"));
                return;
            }
        };
        let Some(input) = input else {
            ui.toast("That download no longer has a file on disk");
            return;
        };

        let job = MediaJob::beside_input(input, action);
        let label = job.action.label();
        let queue = backend.media.clone();
        // Detached: the queue reports progress through MediaEvent.
        backend.spawn(async move {
            if let Err(error) = queue.submit(job).await {
                log::warn!("media job failed: {error:#}");
            }
        });
        ui.toast(&format!("{label}…"));
    });
}

/// Ask what to do with a recording the user wants to end.
fn present_stop_recording(ui: &Rc<Ui>, job_id: i64, button: gtk::Button) {
    let dialog = adw::AlertDialog::builder()
        .heading("Stop Recording?")
        .body(
            "Snatch will finish the file so it plays properly. Everything              recorded so far is kept.\n\nMatroska plays in VLC and mpv.              Convert it if you want a file that plays anywhere.",
        )
        .build();
    dialog.add_responses(&[
        ("keep", "Keep Recording"),
        ("save", "Stop and Save"),
        ("mp4", "Stop and Convert to MP4"),
    ]);
    dialog.set_response_appearance("mp4", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("mp4"));
    dialog.set_close_response("keep");

    let weak = Rc::downgrade(ui);
    dialog.connect_response(None, move |_, response| {
        let convert = match response {
            "save" => false,
            "mp4" => true,
            _ => return,
        };
        let Some(ui) = weak.upgrade() else { return };
        if convert {
            ui.downloads.convert_when_done.borrow_mut().insert(job_id);
        }
        // `stop` asks ffmpeg to finish and close the file. `cancel` is the
        // fallback for a job that turned out not to be a recording after all,
        // which can happen if it ended between the click and this answer.
        if !ui.backend().video.stop(job_id) {
            ui.backend().video.cancel(job_id);
        }
        button.set_sensitive(false);
        ui.toast("Finishing the recording");
    });

    dialog.present(Some(ui.window()));
}

/// Repackage a finished recording as MP4 without re-encoding it.
fn convert_to_mp4(ui: &Rc<Ui>, input: PathBuf) {
    let backend = ui.backend().clone();
    let job = MediaJob::beside_input(input, MediaAction::ConvertToMp4);
    let queue = backend.media.clone();
    // Detached: the queue reports its own progress through MediaEvent.
    backend.spawn(async move {
        if let Err(error) = queue.submit(job).await {
            log::warn!("could not convert the recording: {error:#}");
        }
    });
}

fn present_trim_dialog(ui: &Rc<Ui>, gid: &str) {
    let start = gtk::Entry::builder()
        .placeholder_text("Start, e.g. 0:30")
        .activates_default(true)
        .build();
    let end = gtk::Entry::builder()
        .placeholder_text("End, e.g. 2:15 (blank for the end of the file)")
        .activates_default(true)
        .build();

    let fields = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    fields.append(&start);
    fields.append(&end);

    let dialog = adw::AlertDialog::builder()
        .heading("Trim Video")
        .body("Streams are copied, so the cut is instant and lossless.")
        .extra_child(&fields)
        .build();
    dialog.add_responses(&[("close", "Cancel"), ("trim", "Trim")]);
    dialog.set_response_appearance("trim", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("trim"));
    dialog.set_close_response("close");

    let weak = Rc::downgrade(ui);
    let gid = gid.to_owned();
    dialog.connect_response(None, move |_, response| {
        if response != "trim" {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };

        let from = match parse_timecode(&start.text()) {
            Some(value) => value,
            None => {
                ui.toast("Could not read the start time; use mm:ss or hh:mm:ss");
                return;
            }
        };
        let to = if end.text().trim().is_empty() {
            None
        } else {
            match parse_timecode(&end.text()) {
                Some(value) => Some(value),
                None => {
                    ui.toast("Could not read the end time; use mm:ss or hh:mm:ss");
                    return;
                }
            }
        };
        if let Some(to) = to
            && to <= from
        {
            ui.toast("The end time must be after the start time");
            return;
        }

        start_post_process(
            &ui,
            &gid,
            MediaAction::Trim {
                start: from,
                end: to,
            },
        );
    });

    dialog.present(Some(ui.window()));
}

/// Accept `ss`, `mm:ss` or `hh:mm:ss`, with optional fractional seconds.
fn parse_timecode(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let mut seconds = 0f64;
    for part in value.split(':') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        let number: f64 = part.parse().ok()?;
        if number < 0.0 || !number.is_finite() {
            return None;
        }
        seconds = seconds * 60.0 + number;
    }
    if value.split(':').count() > 3 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

/// Ask before throwing away data the user is still waiting for.
fn confirm_remove(ui: &Rc<Ui>, gid: &str, name: &str, finished: bool, path: Option<String>) {
    if finished {
        // Nothing is in flight, so removing the row and deleting the file are
        // genuinely different things. Offer both rather than guessing.
        let has_file = path.as_deref().map(Path::new).is_some_and(Path::is_file);
        if !has_file {
            remove_download(ui, gid.to_owned(), None);
            return;
        }

        let dialog = adw::AlertDialog::builder()
            .heading("Remove Download?")
            .body(format!(
                "“{name}” has finished. Remove it from the list, or also erase \
                 the file from disk?"
            ))
            .build();
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            ("list", "Remove From List"),
            ("file", "Delete File"),
        ]);
        dialog.set_response_appearance("file", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("list"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(ui);
        let gid = gid.to_owned();
        dialog.connect_response(None, move |_, response| {
            let delete_file = match response {
                "list" => false,
                "file" => true,
                _ => return,
            };
            let Some(ui) = weak.upgrade() else { return };
            remove_download(
                &ui,
                gid.clone(),
                if delete_file { path.clone() } else { None },
            );
        });

        dialog.present(Some(ui.window()));
        return;
    }

    let dialog = adw::AlertDialog::builder()
        .heading("Cancel Download?")
        .body(format!(
            "“{name}” will be cancelled and the data downloaded so far will be deleted."
        ))
        .build();
    dialog.add_responses(&[("keep", "Keep Downloading"), ("drop", "Cancel Download")]);
    dialog.set_response_appearance("drop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    dialog.set_close_response("keep");

    let weak = Rc::downgrade(ui);
    let gid = gid.to_owned();
    dialog.connect_response(None, move |_, response| {
        if response != "drop" {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            remove_download(&ui, gid.clone(), path.clone());
        }
    });

    dialog.present(Some(ui.window()));
}

/// Drop a download from aria2, optionally deleting its partial payload.
fn remove_download(ui: &Rc<Ui>, gid: String, partial: Option<String>) {
    let weak = Rc::downgrade(ui);
    let backend = ui.backend().clone();

    glib::spawn_future_local(async move {
        let client = backend.aria2.clone();
        let result = backend
            .offload(async move {
                client.remove(&gid).await?;
                if let Some(path) = partial {
                    delete_partial(&path).await;
                }
                Ok(())
            })
            .await;

        if let Err(error) = result
            && let Some(ui) = weak.upgrade()
        {
            ui.toast(&format!("{error:#}"));
        }
    });
}

/// Delete a cancelled download's payload and aria2's control file.
async fn delete_partial(path: &str) {
    for candidate in [path.to_owned(), format!("{path}.aria2")] {
        match tokio::fs::remove_file(&candidate).await {
            Ok(()) => log::info!("deleted partial file {candidate}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("could not delete {candidate}: {error}"),
        }
    }
}

fn detail_line(download: &DownloadStatus) -> String {
    if download.is_error() {
        let code = download.error_code.as_deref().unwrap_or("?");
        return match download
            .error_message
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty())
        {
            Some(message) => format!("Error {code}: {}", crate::network::explain_failure(message)),
            None => format!("aria2 error {code}"),
        };
    }

    if download.is_complete() {
        return match download.path() {
            Some(path) => format!("{} · {path}", human_bytes(download.total_length)),
            None => human_bytes(download.total_length),
        };
    }

    let mut parts = vec![if download.total_length > 0 {
        format!(
            "{} of {}",
            human_bytes(download.completed_length),
            human_bytes(download.total_length)
        )
    } else {
        format!("{} downloaded", human_bytes(download.completed_length))
    }];

    if download.is_active() {
        parts.push(format!("{}/s", human_bytes(download.download_speed)));
        if download.connections > 0 {
            parts.push(format!("{} connections", download.connections));
        }
        if let Some(eta) = download.eta_seconds() {
            parts.push(format!("{} left", human_duration(eta)));
        }
    } else if download.is_paused() {
        parts.push("paused".to_owned());
    } else if download.is_waiting() {
        parts.push("waiting for a free slot".to_owned());
    }

    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecodes_accept_the_three_common_shapes() {
        assert_eq!(parse_timecode("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_timecode("1:30"), Some(Duration::from_secs(90)));
        assert_eq!(parse_timecode("1:01:01"), Some(Duration::from_secs(3661)));
        assert_eq!(
            parse_timecode(" 0:02.5 "),
            Some(Duration::from_secs_f64(2.5))
        );
    }

    #[test]
    fn timecodes_reject_nonsense() {
        assert_eq!(parse_timecode(""), None);
        assert_eq!(parse_timecode("abc"), None);
        assert_eq!(parse_timecode("1:"), None);
        assert_eq!(parse_timecode("-5"), None);
        assert_eq!(parse_timecode("1:2:3:4"), None);
    }

    #[test]
    fn only_media_files_offer_post_processing() {
        assert!(is_media(Path::new("/x/movie.mkv")));
        assert!(is_media(Path::new("/x/CLIP.MP4")));
        assert!(is_media(Path::new("/x/song.m4a")));
        // Offering "extract audio" on an archive would only produce an error.
        assert!(!is_media(Path::new("/x/archive.zip")));
        assert!(!is_media(Path::new("/x/no-extension")));
    }
}
