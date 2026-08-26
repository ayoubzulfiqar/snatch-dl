//! The Media Scraper page: `gallery-dl` batches and the files they produce.
//!
//! A batch can be hundreds of files, so the row shows aggregate progress and
//! an expandable grid of the most recent arrivals rather than one row per
//! image. Thumbnails are loaded lazily and only for images that are actually
//! on disk — a scrape of a thousand files must not turn into a thousand
//! decoded pixbufs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;

use super::Ui;
use super::format::{boxed_list, boxed_list_page, caption_label, control_button};
use crate::db::{GalleryBatch, GalleryFile, JobState};
use crate::gallery::{GalleryConfig, GalleryEvent, destination_for};
use crate::types::DownloadRequest;
use crate::{adw, gtk};
use gtk::{gdk, glib};

const PAGE_EMPTY: &str = "empty";
const PAGE_LIST: &str = "list";
/// How many recent files each batch keeps on screen.
const PREVIEW_LIMIT: usize = 24;
const THUMBNAIL_PX: i32 = 96;

const IMAGE_EXTENSIONS: [&str; 8] = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "avif", "jxl"];

fn is_image(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension.as_str()))
}

pub struct ScraperPage {
    root: gtk::Box,
    stack: gtk::Stack,
    list: gtk::ListBox,
    rows: RefCell<HashMap<i64, Rc<BatchRow>>>,
}

impl ScraperPage {
    pub fn new() -> Self {
        let list = boxed_list();
        let scroller = boxed_list_page(&list);

        let empty = adw::StatusPage::builder()
            .icon_name("image-x-generic-symbolic")
            .title("No Scrapes Yet")
            .description(
                "Give Snatch a gallery, board or profile page and gallery-dl will pull \
                 the whole set, filed by site and author.",
            )
            .vexpand(true)
            .build();

        let start = gtk::Button::builder()
            .label("Scrape a Page…")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .action_name("win.scrape")
            .build();
        empty.set_child(Some(&start));

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
            rows: RefCell::new(HashMap::new()),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Install the page's own action once the `Ui` exists.
    pub fn wire(&self, ui: &Rc<Ui>) {
        let action = gtk::gio::SimpleAction::new("scrape", None);
        let weak = Rc::downgrade(ui);
        action.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                present_scrape_dialog(&ui);
            }
        });
        ui.window().add_action(&action);
    }

    /// Render batches recovered from the database at startup.
    pub fn load(&self, ui: &Rc<Ui>, batches: &[GalleryBatch], files: &[GalleryFile]) {
        for batch in batches {
            let row = self.row_for(ui, batch.id, &batch.url);
            row.apply_record(batch);
        }
        // Files arrive newest-first per batch; replay them so a restored batch
        // shows the same thumbnails it had before the restart.
        let rows = self.rows.borrow();
        for file in files.iter().rev() {
            if let Some(row) = rows.get(&file.batch_id) {
                row.restore_file(&file.path, file.skipped);
            }
        }
        drop(rows);
        self.refresh_page();
    }

    /// Queue a new scrape.
    pub fn start(&self, ui: &Rc<Ui>, url: String) {
        let backend = ui.backend().clone();
        let destination = destination_for(&backend.download_dir.join("Snatch Galleries"), &url);
        let config = GalleryConfig::new(destination);

        let weak = Rc::downgrade(ui);
        glib::spawn_future_local(async move {
            let engine = backend.gallery.clone();
            let proxies = backend.proxies.clone();
            let events = backend.gallery_events.clone();
            let url_for_task = url.clone();

            let result = backend
                .offload(async move { engine.start(url_for_task, config, proxies, events).await })
                .await;

            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(id) => log::info!("started scrape batch {id} for {url}"),
                Err(error) => ui.toast(&format!("Could not start the scrape: {error:#}")),
            }
        });
    }

    pub fn handle(&self, ui: &Rc<Ui>, event: GalleryEvent) {
        match event {
            GalleryEvent::Started { batch_id, url } => {
                let row = self.row_for(ui, batch_id, &url);
                row.set_state(JobState::Running, None);
            }
            GalleryEvent::Total { batch_id, total } => {
                if let Some(row) = self.rows.borrow().get(&batch_id) {
                    row.set_total(total);
                }
            }
            GalleryEvent::File {
                batch_id,
                path,
                skipped,
            } => {
                if let Some(row) = self.rows.borrow().get(&batch_id) {
                    row.add_file(&path, skipped);
                }
            }
            GalleryEvent::Warning { batch_id, message } => {
                if let Some(row) = self.rows.borrow().get(&batch_id) {
                    row.note_warning(&message);
                }
            }
            GalleryEvent::Finished {
                batch_id,
                state,
                error,
            } => {
                if let Some(row) = self.rows.borrow().get(&batch_id) {
                    row.set_state(state, error.as_deref());
                }
                if state == JobState::Complete {
                    ui.toast("Scrape finished");
                }
            }
        }
        self.refresh_page();
    }

    fn row_for(&self, ui: &Rc<Ui>, batch_id: i64, url: &str) -> Rc<BatchRow> {
        let mut rows = self.rows.borrow_mut();
        let row = rows.entry(batch_id).or_insert_with(|| {
            let row = BatchRow::new(batch_id, url, ui);
            // Newest first: a running scrape belongs at the top.
            self.list.prepend(&row.root);
            row
        });
        Rc::clone(row)
    }

    fn refresh_page(&self) {
        self.stack
            .set_visible_child_name(if self.rows.borrow().is_empty() {
                PAGE_EMPTY
            } else {
                PAGE_LIST
            });
    }
}

/// Counters kept per batch so the row can render without re-querying SQLite.
#[derive(Default)]
struct Counters {
    total: u64,
    downloaded: u64,
    skipped: u64,
    failed: u64,
    destination: Option<PathBuf>,
    running: bool,
}

struct BatchRow {
    root: adw::ExpanderRow,
    progress: gtk::ProgressBar,
    detail: gtk::Label,
    grid: gtk::FlowBox,
    cancel: gtk::Button,
    counters: RefCell<Counters>,
    previews: RefCell<usize>,
}

impl BatchRow {
    fn new(batch_id: i64, url: &str, ui: &Rc<Ui>) -> Rc<Self> {
        let progress = gtk::ProgressBar::builder().hexpand(true).build();
        let detail = caption_label(false);
        detail.add_css_class("snatch-detail");

        let cancel = control_button("process-stop-symbolic", "Stop this scrape");
        let open = control_button("folder-open-symbolic", "Open the destination folder");

        let controls = gtk::Box::builder().spacing(6).build();
        controls.append(&cancel);
        controls.append(&open);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .hexpand(true)
            .build();
        body.append(&progress);
        body.append(&detail);

        let header = gtk::Box::builder().spacing(12).build();
        header.append(&body);
        header.append(&controls);

        let root = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(url))
            .css_classes(["snatch-row"])
            .build();
        root.add_suffix(&header);

        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .row_spacing(6)
            .column_spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .min_children_per_line(2)
            .max_children_per_line(8)
            .build();
        let grid_row = gtk::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .child(&grid)
            .build();
        root.add_row(&grid_row);

        let counters = RefCell::new(Counters::default());

        cancel.connect_clicked({
            let weak = Rc::downgrade(ui);
            move |button| {
                let Some(ui) = weak.upgrade() else { return };
                ui.backend().gallery.cancel(batch_id);
                button.set_sensitive(false);
                ui.toast("Stopping the scrape");
            }
        });

        open.connect_clicked({
            let weak = Rc::downgrade(ui);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let backend = ui.backend().clone();
                let inner = Rc::downgrade(&ui);
                glib::spawn_future_local(async move {
                    let db = backend.db.clone();
                    let found = backend
                        .offload(async move { db.batch(batch_id).await })
                        .await;
                    let Some(ui) = inner.upgrade() else { return };
                    match found {
                        Ok(Some(batch)) => ui.reveal(&batch.destination),
                        Ok(None) => ui.toast("That batch is no longer in the database"),
                        Err(error) => ui.toast(&format!("{error:#}")),
                    }
                });
            }
        });

        Rc::new(Self {
            root,
            progress,
            detail,
            grid,
            cancel,
            counters,
            previews: RefCell::new(0),
        })
    }

    fn apply_record(&self, batch: &GalleryBatch) {
        {
            let mut counters = self.counters.borrow_mut();
            counters.total = batch.total;
            counters.downloaded = batch.downloaded;
            counters.skipped = batch.skipped;
            counters.failed = batch.failed;
            counters.destination = Some(batch.destination.clone());
            counters.running = !batch.state.is_terminal();
        }
        self.set_state(batch.state, batch.error.as_deref());
        self.render();
    }

    fn set_total(&self, total: u64) {
        self.counters.borrow_mut().total = total;
        self.render();
    }

    fn add_file(&self, path: &Path, skipped: bool) {
        {
            let mut counters = self.counters.borrow_mut();
            if skipped {
                counters.skipped += 1;
            } else {
                counters.downloaded += 1;
            }
        }

        // Only the newest handful get a thumbnail; the rest are just counted.
        let mut shown = self.previews.borrow_mut();
        if !skipped && *shown < PREVIEW_LIMIT && is_image(path) {
            *shown += 1;
            self.grid.insert(&thumbnail(path), 0);
        }
        drop(shown);

        self.render();
    }

    /// Add a thumbnail for a file already counted in the batch record.
    fn restore_file(&self, path: &Path, skipped: bool) {
        let mut shown = self.previews.borrow_mut();
        if !skipped && *shown < PREVIEW_LIMIT && is_image(path) && path.exists() {
            *shown += 1;
            self.grid.insert(&thumbnail(path), 0);
        }
    }

    fn note_warning(&self, message: &str) {
        self.counters.borrow_mut().failed += 1;
        self.root.set_subtitle(&glib::markup_escape_text(message));
        self.render();
    }

    fn set_state(&self, state: JobState, error: Option<&str>) {
        self.counters.borrow_mut().running = !state.is_terminal();
        self.cancel.set_visible(!state.is_terminal());

        let subtitle = match (state, error) {
            (JobState::Failed, Some(error)) => format!("Failed: {error}"),
            (state, _) => state.label().to_owned(),
        };
        self.root.set_subtitle(&glib::markup_escape_text(&subtitle));
        self.render();
    }

    fn render(&self) {
        let counters = self.counters.borrow();
        let handled = counters.downloaded + counters.skipped + counters.failed;

        if counters.total > 0 {
            self.progress
                .set_fraction((handled as f64 / counters.total as f64).clamp(0.0, 1.0));
        } else if counters.running {
            // gallery-dl has not announced a total yet.
            self.progress.pulse();
        } else {
            self.progress.set_fraction(1.0);
        }

        let mut parts = Vec::new();
        parts.push(match counters.total {
            0 => format!("{handled} files"),
            total => format!("{handled} of {total} files"),
        });
        if counters.downloaded > 0 {
            parts.push(format!("{} new", counters.downloaded));
        }
        if counters.skipped > 0 {
            parts.push(format!("{} already had", counters.skipped));
        }
        if counters.failed > 0 {
            parts.push(format!("{} failed", counters.failed));
        }
        self.detail.set_text(&parts.join(" · "));
    }
}

/// A thumbnail button that opens the file when clicked.
fn thumbnail(path: &Path) -> gtk::Widget {
    let picture = gtk::Picture::builder()
        .width_request(THUMBNAIL_PX)
        .height_request(THUMBNAIL_PX)
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .build();

    // Loading is best effort: a partially written file simply shows nothing.
    match gdk::Texture::from_filename(path) {
        Ok(texture) => picture.set_paintable(Some(&texture)),
        Err(error) => log::debug!("no thumbnail for {}: {error}", path.display()),
    }

    let button = gtk::Button::builder()
        .child(&picture)
        .tooltip_text(path.to_string_lossy())
        .css_classes(["flat", "snatch-thumb"])
        .build();

    let target = path.to_path_buf();
    button.connect_clicked(move |_| {
        let uri = gtk::gio::File::for_path(&target).uri();
        if let Err(error) =
            gtk::gio::AppInfo::launch_default_for_uri(&uri, gtk::gio::AppLaunchContext::NONE)
        {
            log::warn!("could not open {}: {error}", target.display());
        }
    });

    button.upcast()
}

fn present_scrape_dialog(ui: &Rc<Ui>) {
    let entry = gtk::Entry::builder()
        .placeholder_text("https://example.com/user/gallery")
        .input_purpose(gtk::InputPurpose::Url)
        .activates_default(true)
        .hexpand(true)
        .build();

    let dialog = adw::AlertDialog::builder()
        .heading("Scrape a Gallery")
        .body(
            "gallery-dl walks the page and files everything under \
             Downloads/Snatch Galleries, organised by site and author.",
        )
        .extra_child(&entry)
        .build();
    dialog.add_responses(&[("close", "Cancel"), ("scrape", "Scrape")]);
    dialog.set_response_appearance("scrape", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("scrape", false);
    dialog.set_default_response(Some("scrape"));
    dialog.set_close_response("close");

    entry.connect_changed({
        let dialog = dialog.clone();
        move |entry| dialog.set_response_enabled("scrape", !entry.text().trim().is_empty())
    });

    let weak = Rc::downgrade(ui);
    dialog.connect_response(None, move |_, response| {
        if response != "scrape" {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        let url = entry.text().trim().to_owned();
        if !url.is_empty() {
            ui.enqueue(DownloadRequest::scrape(url));
        }
    });

    dialog.present(Some(ui.window()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_images_get_a_thumbnail() {
        assert!(is_image(Path::new("/g/a.jpg")));
        assert!(is_image(Path::new("/g/A.PNG")));
        assert!(is_image(Path::new("/g/x.webp")));
        // A scraped video or metadata file must not be decoded as a pixbuf.
        assert!(!is_image(Path::new("/g/clip.mp4")));
        assert!(!is_image(Path::new("/g/info.json")));
        assert!(!is_image(Path::new("/g/plain")));
    }
}
