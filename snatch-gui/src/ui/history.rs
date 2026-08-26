//! The History page: everything that has finished, and what to do with it.
//!
//! Two destructive actions live here and they are deliberately kept distinct,
//! because conflating them is how people lose files:
//!
//! * **Remove from history** forgets the record. The file stays.
//! * **Delete file** erases the file from disk. The record stays, greyed out.
//!
//! Selection mode turns the whole list into checkboxes so either can be done
//! to many rows at once, with one confirmation naming exactly what will
//! happen.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;

use super::Ui;
use super::format::{boxed_list, boxed_list_page, human_bytes};
use crate::db::{HistoryEntry, JobState};
use crate::{adw, gtk};
use gtk::glib;

const PAGE_EMPTY: &str = "empty";
const PAGE_LIST: &str = "list";
/// How many entries the page shows. Older ones stay in the database.
const LIMIT: u32 = 500;

pub struct HistoryPage {
    root: gtk::Box,
    stack: gtk::Stack,
    list: gtk::ListBox,
    /// The action bar shown only while selecting.
    selection_bar: gtk::Revealer,
    select_toggle: gtk::ToggleButton,
    selected_label: gtk::Label,
    /// Row id -> its checkbox and the entry it stands for.
    rows: RefCell<HashMap<i64, (gtk::CheckButton, HistoryEntry)>>,
    selecting: Cell<bool>,
}

impl HistoryPage {
    pub fn new() -> Self {
        let list = boxed_list();
        let scroller = boxed_list_page(&list);

        let empty = adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No History Yet")
            .description("Finished downloads are listed here, with where they went.")
            .vexpand(true)
            .build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        stack.add_named(&empty, Some(PAGE_EMPTY));
        stack.add_named(&scroller, Some(PAGE_LIST));
        stack.set_visible_child_name(PAGE_EMPTY);

        let selected_label = gtk::Label::builder()
            .label("0 selected")
            .css_classes(["snatch-detail"])
            .hexpand(true)
            .xalign(0.0)
            .build();

        let select_toggle = gtk::ToggleButton::builder()
            .icon_name("selection-mode-symbolic")
            .tooltip_text("Select several")
            .build();

        let bar = gtk::Box::builder()
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        bar.append(&selected_label);

        let selection_bar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .child(&bar)
            .reveal_child(false)
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&stack);
        root.append(&selection_bar);

        Self {
            root,
            stack,
            list,
            selection_bar,
            select_toggle,
            selected_label,
            rows: RefCell::new(HashMap::new()),
            selecting: Cell::new(false),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// The toggle that belongs in the header bar.
    pub fn select_toggle(&self) -> &gtk::ToggleButton {
        &self.select_toggle
    }

    /// Build the selection action bar and wire the mode toggle.
    pub fn wire(&self, ui: &Rc<Ui>) {
        let Some(bar) = self.selection_bar.child().and_downcast::<gtk::Box>() else {
            return;
        };

        let select_all = gtk::Button::builder()
            .label("All")
            .css_classes(["flat"])
            .build();
        let open = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Open the containing folder")
            .css_classes(["flat"])
            .build();
        let forget = gtk::Button::builder()
            .label("Remove")
            .tooltip_text("Forget these entries; the files stay on disk")
            .css_classes(["flat"])
            .build();
        let delete = gtk::Button::builder()
            .label("Delete Files")
            .tooltip_text("Erase these files from disk")
            .css_classes(["destructive-action"])
            .build();

        bar.append(&select_all);
        bar.append(&open);
        bar.append(&forget);
        bar.append(&delete);

        select_all.connect_clicked({
            let weak = Rc::downgrade(ui);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let page = &ui.history;
                // Toggle: all on, or all off if they already are.
                let all_on = page
                    .rows
                    .borrow()
                    .values()
                    .all(|(check, _)| check.is_active());
                for (check, _) in page.rows.borrow().values() {
                    check.set_active(!all_on);
                }
                page.refresh_selection_count();
            }
        });

        open.connect_clicked({
            let weak = Rc::downgrade(ui);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let folders: Vec<PathBuf> = ui
                    .history
                    .selected()
                    .into_iter()
                    .filter_map(|entry| entry.folder())
                    .collect();
                match folders.first() {
                    // Opening one window per selected file would carpet the
                    // desktop; the folder is what the user wants anyway.
                    Some(folder) => ui.reveal(folder),
                    None => ui.toast("Those entries have no folder on record"),
                }
            }
        });

        forget.connect_clicked({
            let weak = Rc::downgrade(ui);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                ui.history.confirm_forget(&ui);
            }
        });

        delete.connect_clicked({
            let weak = Rc::downgrade(ui);
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                ui.history.confirm_delete_files(&ui);
            }
        });

        self.select_toggle.connect_toggled({
            let weak = Rc::downgrade(ui);
            move |button| {
                let Some(ui) = weak.upgrade() else { return };
                ui.history.set_selecting(button.is_active());
            }
        });
    }

    fn set_selecting(&self, on: bool) {
        self.selecting.set(on);
        self.selection_bar.set_reveal_child(on);
        for (check, _) in self.rows.borrow().values() {
            check.set_visible(on);
            if !on {
                check.set_active(false);
            }
        }
        self.refresh_selection_count();
    }

    fn refresh_selection_count(&self) {
        let count = self
            .rows
            .borrow()
            .values()
            .filter(|(check, _)| check.is_active())
            .count();
        self.selected_label.set_text(&format!("{count} selected"));
    }

    /// The entries currently ticked, or the whole selection mode being off,
    /// nothing.
    fn selected(&self) -> Vec<HistoryEntry> {
        self.rows
            .borrow()
            .values()
            .filter(|(check, _)| check.is_active())
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// Reload from the database.
    pub fn reload(&self, ui: &Rc<Ui>) {
        let weak = Rc::downgrade(ui);
        let backend = ui.backend().clone();
        glib::spawn_future_local(async move {
            let db = backend.db.clone();
            let loaded = backend
                .offload(async move { db.history(LIMIT).await })
                .await;
            let Some(ui) = weak.upgrade() else { return };
            match loaded {
                Ok(entries) => ui.history.fill(&ui, entries),
                Err(error) => ui.toast(&format!("Could not read the history: {error:#}")),
            }
        });
    }

    fn fill(&self, ui: &Rc<Ui>, entries: Vec<HistoryEntry>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();

        let selecting = self.selecting.get();
        for entry in entries {
            let check = gtk::CheckButton::builder()
                .valign(gtk::Align::Center)
                .visible(selecting)
                .build();
            check.connect_toggled({
                let weak = Rc::downgrade(ui);
                move |_| {
                    if let Some(ui) = weak.upgrade() {
                        ui.history.refresh_selection_count();
                    }
                }
            });

            let row = build_row(ui, &entry, &check);
            self.list.append(&row);
            self.rows.borrow_mut().insert(entry.id, (check, entry));
        }

        let empty = self.rows.borrow().is_empty();
        self.stack
            .set_visible_child_name(if empty { PAGE_EMPTY } else { PAGE_LIST });
        self.refresh_selection_count();
    }

    fn confirm_forget(&self, ui: &Rc<Ui>) {
        let chosen = self.selected();
        if chosen.is_empty() {
            ui.toast("Nothing selected");
            return;
        }

        let ids: Vec<i64> = chosen.iter().map(|entry| entry.id).collect();
        let backend = ui.backend().clone();
        let weak = Rc::downgrade(ui);
        glib::spawn_future_local(async move {
            let db = backend.db.clone();
            let removed = backend
                .offload(async move { db.forget_downloads(ids).await })
                .await;
            let Some(ui) = weak.upgrade() else { return };
            match removed {
                Ok(count) => {
                    ui.toast(&format!("Removed {count} from history; files kept"));
                    ui.history.reload(&ui);
                }
                Err(error) => ui.toast(&format!("{error:#}")),
            }
        });
    }

    fn confirm_delete_files(&self, ui: &Rc<Ui>) {
        let chosen: Vec<HistoryEntry> = self
            .selected()
            .into_iter()
            .filter(|entry| entry.exists())
            .collect();

        if chosen.is_empty() {
            ui.toast("None of those files are still on disk");
            return;
        }

        let total: u64 = chosen.iter().map(|entry| entry.size).sum();
        let body = if chosen.len() == 1 {
            format!(
                "“{}” will be erased from disk. This cannot be undone.",
                chosen[0].filename
            )
        } else {
            format!(
                "{} files ({}) will be erased from disk. This cannot be undone.",
                chosen.len(),
                human_bytes(total)
            )
        };

        let dialog = adw::AlertDialog::builder()
            .heading("Delete Files?")
            .body(body)
            .build();
        dialog.add_responses(&[
            ("keep", "Cancel"),
            ("delete", "Delete Files"),
            ("both", "Delete and Forget"),
        ]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("both", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("keep");

        let weak = Rc::downgrade(ui);
        dialog.connect_response(None, move |_, response| {
            let forget_too = match response {
                "delete" => false,
                "both" => true,
                _ => return,
            };
            let Some(ui) = weak.upgrade() else { return };
            delete_files(&ui, chosen.clone(), forget_too);
        });

        dialog.present(Some(ui.window()));
    }
}

/// Erase the files, then optionally forget the records.
fn delete_files(ui: &Rc<Ui>, entries: Vec<HistoryEntry>, forget_too: bool) {
    let backend = ui.backend().clone();
    let weak = Rc::downgrade(ui);

    glib::spawn_future_local(async move {
        let db = backend.db.clone();
        let outcome = backend
            .offload(async move {
                let mut deleted = 0usize;
                let mut failures = Vec::new();
                let mut ids = Vec::new();

                for entry in entries {
                    let Some(path) = entry.path.clone() else {
                        continue;
                    };
                    match tokio::fs::remove_file(&path).await {
                        Ok(()) => {
                            deleted += 1;
                            ids.push(entry.id);
                            // aria2 leaves a control file beside an
                            // interrupted download; it is useless once the
                            // payload is gone.
                            let control = PathBuf::from(format!("{}.aria2", path.display()));
                            let _ = tokio::fs::remove_file(control).await;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            ids.push(entry.id);
                        }
                        Err(error) => {
                            failures.push(format!("{}: {error}", entry.filename));
                        }
                    }
                }

                if forget_too && !ids.is_empty() {
                    db.forget_downloads(ids).await?;
                }
                Ok((deleted, failures))
            })
            .await;

        let Some(ui) = weak.upgrade() else { return };
        match outcome {
            Ok((deleted, failures)) if failures.is_empty() => {
                ui.toast(&format!("Deleted {deleted} file(s)"));
                ui.history.reload(&ui);
            }
            Ok((deleted, failures)) => {
                ui.toast(&format!(
                    "Deleted {deleted}; {} could not be removed: {}",
                    failures.len(),
                    failures.join(", ")
                ));
                ui.history.reload(&ui);
            }
            Err(error) => ui.toast(&format!("{error:#}")),
        }
    });
}

fn build_row(ui: &Rc<Ui>, entry: &HistoryEntry, check: &gtk::CheckButton) -> adw::ActionRow {
    let present = entry.exists();

    let mut facts = vec![entry.origin.label().to_owned()];
    if entry.size > 0 {
        facts.push(human_bytes(entry.size));
    }
    facts.push(when(entry.finished_at));
    match (&entry.state, present) {
        (JobState::Failed, _) => {
            facts.push(entry.error.clone().unwrap_or_else(|| "failed".to_owned()))
        }
        // Saying the file is gone is more useful than showing a dead path.
        (_, false) => facts.push("file no longer on disk".to_owned()),
        _ => {}
    }
    if let Some(folder) = entry.folder() {
        facts.push(folder.to_string_lossy().into_owned());
    }

    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&entry.filename))
        .subtitle(glib::markup_escape_text(&facts.join(" · ")))
        .tooltip_text(&entry.url)
        .build();
    row.add_prefix(check);

    let icon = gtk::Image::from_icon_name(match entry.state {
        JobState::Complete if present => "emblem-ok-symbolic",
        JobState::Complete => "image-missing-symbolic",
        JobState::Failed => "dialog-error-symbolic",
        _ => "process-stop-symbolic",
    });
    if entry.state == JobState::Failed {
        icon.add_css_class("error");
    } else if present {
        icon.add_css_class("success");
    } else {
        icon.add_css_class("dim-label");
    }
    row.add_prefix(&icon);

    // The folder button is the one thing every row wants, present or not:
    // even for a deleted file the directory is usually still interesting.
    if let Some(folder) = entry.folder() {
        let open = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(format!("Open {}", folder.display()))
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        open.connect_clicked({
            let weak = Rc::downgrade(ui);
            let path = entry.path.clone().unwrap_or(folder);
            move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.reveal(&path);
                }
            }
        });
        row.add_suffix(&open);
    }

    if present {
        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete this file from disk")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        delete.connect_clicked({
            let weak = Rc::downgrade(ui);
            let entry = entry.clone();
            move |_| {
                let Some(ui) = weak.upgrade() else { return };
                confirm_single_delete(&ui, entry.clone());
            }
        });
        row.add_suffix(&delete);
    }

    let again = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Download again")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    again.connect_clicked({
        let weak = Rc::downgrade(ui);
        let url = entry.url.clone();
        move |_| {
            let Some(ui) = weak.upgrade() else { return };
            ui.enqueue(crate::types::DownloadRequest::from_url(url.clone()));
        }
    });
    row.add_suffix(&again);

    row
}

fn confirm_single_delete(ui: &Rc<Ui>, entry: HistoryEntry) {
    let dialog = adw::AlertDialog::builder()
        .heading("Delete File?")
        .body(format!(
            "“{}” will be erased from disk. This cannot be undone.",
            entry.filename
        ))
        .build();
    dialog.add_responses(&[
        ("keep", "Cancel"),
        ("delete", "Delete File"),
        ("both", "Delete and Forget"),
    ]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_response_appearance("both", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("keep");

    let weak = Rc::downgrade(ui);
    dialog.connect_response(None, move |_, response| {
        let forget_too = match response {
            "delete" => false,
            "both" => true,
            _ => return,
        };
        if let Some(ui) = weak.upgrade() {
            delete_files(&ui, vec![entry.clone()], forget_too);
        }
    });

    dialog.present(Some(ui.window()));
}

/// A coarse "how long ago", which is what a history list actually needs.
fn when(unix_seconds: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(unix_seconds);
    let delta = now.saturating_sub(unix_seconds).max(0);

    match delta {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", delta / 60),
        3600..=86_399 => format!("{} h ago", delta / 3600),
        86_400..=604_799 => format!("{} days ago", delta / 86_400),
        _ => format!("{} weeks ago", delta / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_read_naturally() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0);
        assert_eq!(when(now), "just now");
        assert_eq!(when(now - 120), "2 min ago");
        assert_eq!(when(now - 7200), "2 h ago");
        assert_eq!(when(now - 172_800), "2 days ago");
        assert_eq!(when(now - 1_209_600), "2 weeks ago");
        // A clock that jumped backwards must not produce a negative age.
        assert_eq!(when(now + 500), "just now");
    }
}
