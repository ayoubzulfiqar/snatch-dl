//! The Torrents page.
//!
//! Each torrent is an expander: collapsed it is a progress bar and a peer
//! count, expanded it shows the per-file breakdown and the transport split
//! (TCP / uTP / SOCKS), which is the detail that actually tells you whether a
//! swarm is healthy or whether your proxy is doing anything.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;

use super::format::{
    boxed_list, boxed_list_page, caption_label, control_button, human_bytes, human_duration,
};
use super::{PageSummary, Ui};
use crate::torrent::{TorrentPhase, TorrentSnapshot};
use crate::{adw, gtk};
use gtk::glib;

const PAGE_EMPTY: &str = "empty";
const PAGE_LIST: &str = "list";
const PAGE_BROKEN: &str = "broken";

pub struct TorrentsPage {
    root: gtk::Box,
    stack: gtk::Stack,
    list: gtk::ListBox,
    broken: adw::StatusPage,
    rows: RefCell<HashMap<usize, Rc<TorrentRow>>>,
    summary: RefCell<PageSummary>,
}

impl TorrentsPage {
    pub fn new() -> Self {
        let list = boxed_list();
        let scroller = boxed_list_page(&list);

        let empty = adw::StatusPage::builder()
            .icon_name("network-transmit-receive-symbolic")
            .title("No Torrents")
            .description(
                "Drop a magnet link in with the + button, or click one in your browser.\n\
                 DHT and peer exchange are on, so magnets work without trackers.",
            )
            .vexpand(true)
            .build();

        let broken = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("BitTorrent Unavailable")
            .vexpand(true)
            .build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&empty, Some(PAGE_EMPTY));
        stack.add_named(&scroller, Some(PAGE_LIST));
        stack.add_named(&broken, Some(PAGE_BROKEN));
        stack.set_visible_child_name(PAGE_EMPTY);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&stack);

        Self {
            root,
            stack,
            list,
            broken,
            rows: RefCell::new(HashMap::new()),
            summary: RefCell::new(PageSummary::default()),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn summary(&self) -> PageSummary {
        *self.summary.borrow()
    }

    /// Show why the session never came up, instead of an empty list.
    pub fn set_unavailable(&self, reason: &str) {
        self.broken.set_description(Some(reason));
        self.stack.set_visible_child_name(PAGE_BROKEN);
    }

    /// Returns the number of torrents actively transferring.
    pub fn apply(&self, ui: &Rc<Ui>, torrents: &[TorrentSnapshot]) -> usize {
        // A live snapshot means the session is up, so leave the error page.
        if self.stack.visible_child_name().as_deref() == Some(PAGE_BROKEN) {
            self.stack.set_visible_child_name(PAGE_EMPTY);
        }

        let mut seen = HashSet::with_capacity(torrents.len());
        let mut summary = PageSummary {
            total: torrents.len(),
            ..PageSummary::default()
        };

        {
            let mut rows = self.rows.borrow_mut();
            for torrent in torrents {
                seen.insert(torrent.id);
                let row = rows.entry(torrent.id).or_insert_with(|| {
                    let row = TorrentRow::new(torrent.id, ui);
                    self.list.append(&row.root);
                    row
                });
                row.update(torrent);

                if matches!(
                    torrent.phase,
                    TorrentPhase::Downloading | TorrentPhase::Seeding
                ) {
                    summary.active += 1;
                    summary.speed += torrent.download_bps;
                }
            }

            rows.retain(|id, row| {
                let keep = seen.contains(id);
                if !keep {
                    self.list.remove(&row.root);
                }
                keep
            });
        }

        self.stack.set_visible_child_name(if torrents.is_empty() {
            PAGE_EMPTY
        } else {
            PAGE_LIST
        });
        *self.summary.borrow_mut() = summary;
        summary.active
    }
}

#[derive(Default)]
struct RowState {
    paused: bool,
    sequential: bool,
    name: String,
    folder: String,
    /// Index of the largest file: the one a viewer actually wants streamed.
    /// Recomputed on every snapshot, since a magnet has no files until its
    /// metadata arrives.
    stream_file: usize,
}

struct TorrentRow {
    root: adw::ExpanderRow,
    progress: gtk::ProgressBar,
    detail: gtk::Label,
    peers: gtk::Label,
    files: gtk::Box,
    toggle: gtk::Button,
    sequential: gtk::ToggleButton,
    state: Rc<RefCell<RowState>>,
    /// Rendered file rows, rebuilt only when the file list actually changes.
    file_rows: RefCell<Vec<(gtk::Label, gtk::ProgressBar)>>,
}

impl TorrentRow {
    fn new(id: usize, ui: &Rc<Ui>) -> Rc<Self> {
        let state = Rc::new(RefCell::new(RowState::default()));

        let progress = gtk::ProgressBar::builder().hexpand(true).build();
        let detail = caption_label(false);
        detail.add_css_class("snatch-detail");
        let peers = caption_label(false);
        peers.add_css_class("snatch-peers");

        let toggle = control_button("media-playback-pause-symbolic", "Pause");
        let open = control_button("folder-open-symbolic", "Show in file manager");
        let remove = control_button("user-trash-symbolic", "Remove");

        // Sequential mode is a toggle because it is a persistent mode, not an
        // action: librqbit keeps prioritising the read head until it is off.
        let sequential = gtk::ToggleButton::builder()
            .icon_name("media-playlist-consecutive-symbolic")
            .tooltip_text("Download in order, so the file can be played while it arrives")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();

        let controls = gtk::Box::builder().spacing(6).build();
        controls.append(&sequential);
        controls.append(&toggle);
        controls.append(&open);
        controls.append(&remove);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .hexpand(true)
            .build();
        body.append(&progress);
        body.append(&detail);
        body.append(&peers);

        let header = gtk::Box::builder().spacing(12).build();
        header.append(&body);
        header.append(&controls);

        let root = adw::ExpanderRow::builder().build();
        root.add_css_class("snatch-row");
        root.add_suffix(&header);

        let files = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        let files_row = gtk::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .child(&files)
            .build();
        root.add_row(&files_row);

        toggle.connect_clicked({
            let backend = ui.backend().clone();
            let weak = Rc::downgrade(ui);
            let state = Rc::clone(&state);
            move |button| {
                let resume = state.borrow().paused;
                button.set_sensitive(false);
                let button = button.clone();
                let backend = backend.clone();
                let weak = weak.clone();

                glib::spawn_future_local(async move {
                    let result = match backend.torrents() {
                        Ok(engine) => {
                            backend
                                .offload(async move {
                                    if resume {
                                        engine.resume(id).await
                                    } else {
                                        engine.pause(id).await
                                    }
                                })
                                .await
                        }
                        Err(error) => Err(error),
                    };
                    button.set_sensitive(true);
                    if let Err(error) = result
                        && let Some(ui) = weak.upgrade()
                    {
                        ui.toast(&format!("{error:#}"));
                    }
                });
            }
        });

        sequential.connect_toggled({
            let backend = ui.backend().clone();
            let weak = Rc::downgrade(ui);
            let state = Rc::clone(&state);
            move |button| {
                let wanted = button.is_active();
                // Ignore the toggle we set ourselves while refreshing.
                if wanted == state.borrow().sequential {
                    return;
                }
                let Some(ui) = weak.upgrade() else { return };
                let file_index = state.borrow().stream_file;
                match backend.torrents() {
                    Ok(engine) => match engine.set_sequential(id, file_index, wanted) {
                        Ok(()) => ui.toast(if wanted {
                            "Downloading in order for playback"
                        } else {
                            "Back to fastest-first order"
                        }),
                        Err(error) => {
                            button.set_active(!wanted);
                            ui.toast(&format!("{error:#}"));
                        }
                    },
                    Err(error) => {
                        button.set_active(!wanted);
                        ui.toast(&format!("{error:#}"));
                    }
                }
            }
        });

        open.connect_clicked({
            let weak = Rc::downgrade(ui);
            let state = Rc::clone(&state);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let folder = state.borrow().folder.clone();
                if folder.is_empty() {
                    ui.toast("This torrent has no folder yet");
                } else {
                    ui.reveal(std::path::Path::new(&folder));
                }
            }
        });

        remove.connect_clicked({
            let weak = Rc::downgrade(ui);
            let state = Rc::clone(&state);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let name = state.borrow().name.clone();
                confirm_remove(&ui, id, &name);
            }
        });

        Rc::new(Self {
            root,
            progress,
            detail,
            peers,
            files,
            toggle,
            sequential,
            state,
            file_rows: RefCell::new(Vec::new()),
        })
    }

    fn update(&self, torrent: &TorrentSnapshot) {
        self.root
            .set_title(&glib::markup_escape_text(&torrent.name));
        self.root.set_subtitle(&glib::markup_escape_text(&format!(
            "{} · {}",
            torrent.phase.label(),
            // Short form of the info hash: enough to identify the torrent
            // without pushing the phase off a narrow row.
            &torrent.info_hash[..torrent.info_hash.len().min(12)]
        )));

        {
            let mut state = self.state.borrow_mut();
            state.paused = torrent.phase == TorrentPhase::Paused;
            state.sequential = torrent.sequential;
            state.name = torrent.name.clone();
            state.folder = torrent.output_folder.to_string_lossy().into_owned();
            // Stream the biggest file: in a season pack or a movie-plus-extras
            // torrent, file 0 is often a sample or a subtitle.
            state.stream_file = torrent
                .files
                .iter()
                .max_by_key(|file| file.length)
                .map(|file| file.index)
                .unwrap_or(0);
        }

        self.progress.set_fraction(torrent.fraction());
        self.root.set_css_classes(&[
            "snatch-row",
            match torrent.phase {
                TorrentPhase::Seeding => "done",
                TorrentPhase::Error => "failed",
                TorrentPhase::Paused => "paused",
                _ => "",
            },
        ]);

        // `set_active` re-enters the toggled handler, which is why the handler
        // compares against the state we just stored.
        if self.sequential.is_active() != torrent.sequential {
            self.sequential.set_active(torrent.sequential);
        }

        let running = !matches!(torrent.phase, TorrentPhase::Error);
        self.toggle.set_visible(running);
        let (icon, tooltip) = if torrent.phase == TorrentPhase::Paused {
            ("media-playback-start-symbolic", "Resume")
        } else {
            ("media-playback-pause-symbolic", "Pause")
        };
        self.toggle.set_icon_name(icon);
        self.toggle.set_tooltip_text(Some(tooltip));

        self.detail.set_text(&detail_line(torrent));
        self.peers.set_text(&torrent.peers.summary());

        self.sync_files(torrent);
    }

    /// Rebuild the file list only when its shape changes; a torrent with 400
    /// files must not re-create 400 widgets twice a second.
    fn sync_files(&self, torrent: &TorrentSnapshot) {
        let mut rows = self.file_rows.borrow_mut();

        if rows.len() != torrent.files.len() {
            while let Some(child) = self.files.first_child() {
                self.files.remove(&child);
            }
            rows.clear();

            for file in &torrent.files {
                let label = caption_label(false);
                label.set_text(&file.name);
                let bar = gtk::ProgressBar::builder().hexpand(true).build();
                self.files.append(&label);
                self.files.append(&bar);
                rows.push((label, bar));
            }
        }

        for ((label, bar), file) in rows.iter().zip(&torrent.files) {
            bar.set_fraction(file.fraction());
            label.set_text(&format!(
                "{} · {} of {}",
                file.name,
                human_bytes(file.downloaded),
                human_bytes(file.length)
            ));
        }
    }
}

fn detail_line(torrent: &TorrentSnapshot) -> String {
    if let Some(error) = &torrent.error {
        return error.clone();
    }

    let mut parts = vec![format!(
        "{} of {}",
        human_bytes(torrent.progress_bytes),
        human_bytes(torrent.total_bytes)
    )];

    if torrent.download_bps > 0 {
        parts.push(format!("↓ {}/s", human_bytes(torrent.download_bps)));
    }
    if torrent.upload_bps > 0 {
        parts.push(format!("↑ {}/s", human_bytes(torrent.upload_bps)));
    }
    if let Some(eta) = torrent.eta {
        parts.push(format!("{} left", human_duration(eta.as_secs())));
    }
    if let Some(ratio) = torrent.ratio() {
        parts.push(format!("ratio {ratio:.2}"));
    }
    if torrent.sequential {
        parts.push("in order".to_owned());
    }

    parts.join(" · ")
}

/// Removing a torrent can also delete gigabytes, so the choice is explicit.
fn confirm_remove(ui: &Rc<Ui>, id: usize, name: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("Remove Torrent?")
        .body(format!("“{name}” will be removed from the session."))
        .build();
    dialog.add_responses(&[
        ("keep", "Cancel"),
        ("list", "Remove, Keep Files"),
        ("files", "Remove and Delete Files"),
    ]);
    dialog.set_response_appearance("files", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    dialog.set_close_response("keep");

    let weak = Rc::downgrade(ui);
    dialog.connect_response(None, move |_, response| {
        let delete_files = match response {
            "list" => false,
            "files" => true,
            _ => return,
        };
        let Some(ui) = weak.upgrade() else { return };
        let backend = ui.backend().clone();
        let inner = Rc::downgrade(&ui);

        glib::spawn_future_local(async move {
            let result = match backend.torrents() {
                Ok(engine) => {
                    backend
                        .offload(async move { engine.remove(id, delete_files).await })
                        .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result
                && let Some(ui) = inner.upgrade()
            {
                ui.toast(&format!("{error:#}"));
            }
        });
    });

    dialog.present(Some(ui.window()));
}
