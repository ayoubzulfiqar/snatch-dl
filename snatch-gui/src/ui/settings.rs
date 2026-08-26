//! The Settings page.
//!
//! A page rather than a dialog, because it is reached from the sidebar like
//! everything else and because there is far too much here for a dialog.
//!
//! Each row edits a field on a working copy; **Apply** writes the whole thing
//! at once. That matters because several values interact — raising `split`
//! above `max-connection-per-server` does nothing, for instance — and applying
//! them one keystroke at a time would produce nonsense intermediate states.
//!
//! Where a setting cannot take effect immediately the row says so, and Apply
//! reports exactly which changes are waiting on a restart.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::Ui;
use crate::settings::{Allocation, HttpEngine, Settings, WhenFinished};
use crate::{adw, gtk};
use gtk::glib;

/// Build the Settings page.
pub struct SettingsPage {
    root: gtk::Box,
    /// The working copy being edited; committed by Apply.
    draft: Rc<RefCell<Settings>>,
    /// Set while loading values in, so change handlers do not fight the load.
    loading: Rc<std::cell::Cell<bool>>,
}

impl SettingsPage {
    pub fn new() -> Self {
        Self {
            root: gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .build(),
            draft: Rc::new(RefCell::new(Settings::default())),
            loading: Rc::new(std::cell::Cell::new(false)),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Populate the page. Called once, after `Ui` exists.
    pub fn build(&self, ui: &Rc<Ui>) {
        *self.draft.borrow_mut() = ui.backend().settings();

        let page = adw::PreferencesPage::new();
        self.add_download_group(&page, ui);
        self.add_torrent_group(&page, ui);
        self.add_media_group(&page, ui);
        self.add_schedule_group(&page, ui);
        self.add_interface_group(&page, ui);
        self.add_actions_group(&page, ui);

        self.root.append(&page);
    }

    fn add_download_group(&self, page: &adw::PreferencesPage, ui: &Rc<Ui>) {
        let draft = self.draft.borrow().clone();
        let group = adw::PreferencesGroup::builder()
            .title("Downloads")
            .description(
                "Segmenting is what makes a download manager faster than a browser: \
                 the file is fetched in parallel pieces and reassembled.",
            )
            .build();

        let engines: Vec<&str> = HttpEngine::ALL.iter().map(|e| e.label()).collect();
        let engine = adw::ComboRow::builder()
            .title("Download engine")
            .subtitle("Applies to new downloads")
            .model(&gtk::StringList::new(&engines))
            .selected(
                HttpEngine::ALL
                    .iter()
                    .position(|e| *e == draft.download.engine)
                    .unwrap_or(0) as u32,
            )
            .build();
        engine.connect_selected_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let chosen = HttpEngine::ALL
                    .get(row.selected() as usize)
                    .copied()
                    .unwrap_or_default();
                this.edit(&ui, |settings| settings.download.engine = chosen);
            }
        });
        group.add(&engine);

        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Segments per download",
                subtitle: "How many pieces to split a file into. 16 is the practical maximum.",
                min: 1.0,
                max: 64.0,
                value: draft.download.split as f64,
                setter: |settings, value| settings.download.split = value as u32,
            },
        ));
        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Connections per server",
                subtitle: "aria2 refuses more than 16, and many servers refuse fewer.",
                min: 1.0,
                max: 16.0,
                value: draft.download.connections_per_server as f64,
                setter: |settings, value| settings.download.connections_per_server = value as u32,
            },
        ));
        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Minimum split size (MiB)",
                subtitle: "Files smaller than this are not split.",
                min: 1.0,
                max: 1024.0,
                value: draft.download.min_split_mib as f64,
                setter: |settings, value| settings.download.min_split_mib = value as u32,
            },
        ));
        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Simultaneous downloads",
                subtitle: "Applies immediately.",
                min: 1.0,
                max: 50.0,
                value: draft.download.concurrent_downloads as f64,
                setter: |settings, value| settings.download.concurrent_downloads = value as u32,
            },
        ));
        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Overall speed limit (KiB/s)",
                subtitle: "0 means unlimited. Applies immediately.",
                min: 0.0,
                max: 1_000_000.0,
                value: draft.download.max_overall_down_kib as f64,
                setter: |settings, value| settings.download.max_overall_down_kib = value as u64,
            },
        ));
        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Per-download speed limit (KiB/s)",
                subtitle: "0 means unlimited. Applies to new downloads.",
                min: 0.0,
                max: 1_000_000.0,
                value: draft.download.max_per_download_kib as f64,
                setter: |settings, value| settings.download.max_per_download_kib = value as u64,
            },
        ));
        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Retries",
                subtitle: "Attempts before a download is marked failed. Needs a restart.",
                min: 0.0,
                max: 60.0,
                value: draft.download.retries as f64,
                setter: |settings, value| settings.download.retries = value as u32,
            },
        ));

        let allocations: Vec<&str> = Allocation::ALL.iter().map(|a| a.label()).collect();
        let allocation = adw::ComboRow::builder()
            .title("Disk allocation")
            .subtitle("Needs a restart")
            .model(&gtk::StringList::new(&allocations))
            .selected(
                Allocation::ALL
                    .iter()
                    .position(|a| *a == draft.download.allocation)
                    .unwrap_or(1) as u32,
            )
            .build();
        allocation.connect_selected_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let chosen = Allocation::ALL
                    .get(row.selected() as usize)
                    .copied()
                    .unwrap_or_default();
                this.edit(&ui, |settings| settings.download.allocation = chosen);
            }
        });
        group.add(&allocation);

        let categorise = adw::SwitchRow::builder()
            .title("Sort downloads into folders by type")
            .subtitle(
                "Video, Music, Images, Documents, Compressed and Programs \
                 subfolders under your download folder. Applies to new downloads.",
            )
            .active(draft.download.categorise)
            .build();
        categorise.connect_active_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let on = row.is_active();
                this.edit(&ui, |settings| settings.download.categorise = on);
            }
        });
        group.add(&categorise);

        // This is the ".aria2 file" question, phrased as what it actually costs.
        let resume = adw::SwitchRow::builder()
            .title("Write resume data while downloading")
            .subtitle(
                "Creates a temporary .aria2 file next to each download so it can \
                 resume after a crash or power loss. Turning this off removes those \
                 files but loses crash resume. Needs a restart.",
            )
            .active(draft.download.auto_save_interval > 0)
            .build();
        resume.connect_active_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let on = row.is_active();
                this.edit(&ui, |settings| {
                    settings.download.auto_save_interval = if on { 60 } else { 0 };
                });
            }
        });
        group.add(&resume);

        let certificates = adw::SwitchRow::builder()
            .title("Verify TLS certificates")
            .subtitle("Turning this off exposes downloads to interception. Needs a restart.")
            .active(draft.download.check_certificate)
            .build();
        certificates.connect_active_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let on = row.is_active();
                this.edit(&ui, |settings| settings.download.check_certificate = on);
            }
        });
        group.add(&certificates);

        let agent = adw::EntryRow::builder()
            .title("User agent")
            .text(&draft.download.user_agent)
            .build();
        agent.connect_changed({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let value = row.text().to_string();
                this.edit(&ui, |settings| settings.download.user_agent = value);
            }
        });
        group.add(&agent);

        page.add(&group);
    }

    fn add_torrent_group(&self, page: &adw::PreferencesPage, ui: &Rc<Ui>) {
        let draft = self.draft.borrow().clone();
        let group = adw::PreferencesGroup::builder()
            .title("Torrents")
            .description("The BitTorrent engine is built in; nothing needs installing.")
            .build();

        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Upload limit (KiB/s)",
                subtitle: "0 means unlimited. Applies immediately.",
                min: 0.0,
                max: 1_000_000.0,
                value: draft.torrent.max_upload_kib as f64,
                setter: |settings, value| settings.torrent.max_upload_kib = value as u64,
            },
        ));
        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Maximum peers per torrent",
                subtitle: "Needs a restart.",
                min: 1.0,
                max: 1000.0,
                value: draft.torrent.max_peers as f64,
                setter: |settings, value| settings.torrent.max_peers = value as u32,
            },
        ));

        let dht = adw::SwitchRow::builder()
            .title("Distributed hash table")
            .subtitle("Finds peers without a tracker. Magnets need it. Restart to change.")
            .active(draft.torrent.enable_dht)
            .build();
        dht.connect_active_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let on = row.is_active();
                this.edit(&ui, |settings| settings.torrent.enable_dht = on);
            }
        });
        group.add(&dht);

        let incoming = adw::SwitchRow::builder()
            .title("Accept incoming peers")
            .subtitle(
                "Doubles the reachable swarm and lets you seed. Disabled automatically \
                 while a SOCKS5 proxy is set, because a listener would reveal your \
                 real address. Restart to change.",
            )
            .active(draft.torrent.accept_incoming)
            .build();
        incoming.connect_active_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let on = row.is_active();
                this.edit(&ui, |settings| settings.torrent.accept_incoming = on);
            }
        });
        group.add(&incoming);

        page.add(&group);
    }

    fn add_media_group(&self, page: &adw::PreferencesPage, ui: &Rc<Ui>) {
        let draft = self.draft.borrow().clone();
        let group = adw::PreferencesGroup::builder()
            .title("Media and Scraping")
            .build();

        group.add(&self.spin_row(
            ui,
            Numeric {
                title: "Extracted audio bitrate (kbps)",
                subtitle: "Used by Extract Audio and by audio-only video jobs.",
                min: 64.0,
                max: 320.0,
                value: draft.media.audio_bitrate_kbps as f64,
                setter: |settings, value| settings.media.audio_bitrate_kbps = value as u32,
            },
        ));

        for (title, subtitle, initial, setter) in [
            (
                "Embed metadata and chapters",
                "Write titles, chapters and thumbnails into extracted video.",
                draft.media.embed_metadata,
                0usize,
            ),
            (
                "Download subtitles",
                "Fetch and embed subtitles when the site offers them.",
                draft.media.write_subtitles,
                1,
            ),
            (
                "Write gallery metadata",
                "Save an info.json beside scraped galleries.",
                draft.media.gallery_metadata,
                2,
            ),
            (
                "Re-download existing gallery files",
                "Off means gallery-dl skips files already on disk.",
                draft.media.gallery_overwrite,
                3,
            ),
        ] {
            let row = adw::SwitchRow::builder()
                .title(title)
                .subtitle(subtitle)
                .active(initial)
                .build();
            row.connect_active_notify({
                let this = self.clone_handles();
                let ui = Rc::downgrade(ui);
                move |row| {
                    let Some(ui) = ui.upgrade() else { return };
                    let on = row.is_active();
                    this.edit(&ui, |settings| match setter {
                        0 => settings.media.embed_metadata = on,
                        1 => settings.media.write_subtitles = on,
                        2 => settings.media.gallery_metadata = on,
                        _ => settings.media.gallery_overwrite = on,
                    });
                }
            });
            group.add(&row);
        }

        page.add(&group);
    }

    fn add_schedule_group(&self, page: &adw::PreferencesPage, ui: &Rc<Ui>) {
        let draft = self.draft.borrow().clone();
        let group = adw::PreferencesGroup::builder()
            .title("Schedule")
            .description(
                "Restrict downloading to a window each day. A window whose end is \
                 before its start runs overnight.",
            )
            .build();

        let enabled = adw::SwitchRow::builder()
            .title("Only download during a set window")
            .active(draft.schedule.enabled)
            .build();
        enabled.connect_active_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let on = row.is_active();
                this.edit(&ui, |settings| settings.schedule.enabled = on);
            }
        });
        group.add(&enabled);

        for (title, initial, is_start) in [
            ("Start at (HH:MM)", draft.schedule.start.clone(), true),
            ("Stop at (HH:MM)", draft.schedule.stop.clone(), false),
        ] {
            let row = adw::EntryRow::builder().title(title).text(&initial).build();
            row.connect_changed({
                let this = self.clone_handles();
                let ui = Rc::downgrade(ui);
                move |row| {
                    let Some(ui) = ui.upgrade() else { return };
                    let value = row.text().to_string();
                    // Mark a bad time as the user types rather than silently
                    // repairing it on save.
                    let valid = crate::settings::parse_hhmm(&value).is_some();
                    if valid {
                        row.remove_css_class("error");
                    } else {
                        row.add_css_class("error");
                    }
                    this.edit(&ui, |settings| {
                        if is_start {
                            settings.schedule.start = value.clone();
                        } else {
                            settings.schedule.stop = value.clone();
                        }
                    });
                }
            });
            group.add(&row);
        }

        let finishes: Vec<&str> = WhenFinished::ALL.iter().map(|w| w.label()).collect();
        let when_done = adw::ComboRow::builder()
            .title("When everything finishes")
            .subtitle("Suspend and shut down go through logind and can be cancelled.")
            .model(&gtk::StringList::new(&finishes))
            .selected(
                WhenFinished::ALL
                    .iter()
                    .position(|w| *w == draft.interface.when_finished)
                    .unwrap_or(0) as u32,
            )
            .build();
        when_done.connect_selected_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let chosen = WhenFinished::ALL
                    .get(row.selected() as usize)
                    .copied()
                    .unwrap_or_default();
                this.edit(&ui, |settings| settings.interface.when_finished = chosen);
            }
        });
        group.add(&when_done);

        page.add(&group);
    }

    fn add_interface_group(&self, page: &adw::PreferencesPage, ui: &Rc<Ui>) {
        let draft = self.draft.borrow().clone();
        let group = adw::PreferencesGroup::builder().title("Interface").build();

        let folder = adw::EntryRow::builder()
            .title("Download folder — blank uses your XDG Downloads")
            .text(&draft.interface.download_dir)
            .build();
        let browse = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Choose a folder")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        browse.connect_clicked({
            let folder = folder.clone();
            let ui = Rc::downgrade(ui);
            move |_| {
                let Some(ui) = ui.upgrade() else { return };
                let chooser = gtk::FileDialog::builder()
                    .title("Choose a download folder")
                    .modal(true)
                    .build();
                let folder = folder.clone();
                chooser.select_folder(
                    Some(ui.window()),
                    gtk::gio::Cancellable::NONE,
                    move |result| {
                        if let Ok(file) = result
                            && let Some(path) = file.path()
                        {
                            folder.set_text(&path.to_string_lossy());
                        }
                    },
                );
            }
        });
        folder.add_suffix(&browse);
        folder.connect_changed({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let value = row.text().to_string();
                this.edit(&ui, |settings| settings.interface.download_dir = value);
            }
        });
        group.add(&folder);

        for (title, subtitle, initial, which) in [
            (
                "Raise the window on capture",
                "Bring Snatch forward when the browser hands over a download.",
                draft.interface.raise_on_capture,
                0usize,
            ),
            (
                "Notify when a download finishes",
                "Send a desktop notification as each download completes.",
                draft.interface.notify_on_finish,
                2usize,
            ),
            (
                "Watch the clipboard for links",
                "Offer to download a file link copied anywhere on the desktop.",
                draft.interface.watch_clipboard,
                3usize,
            ),
            (
                "Confirm before cancelling",
                "Ask before discarding a download in progress.",
                draft.interface.confirm_cancel,
                1,
            ),
        ] {
            let row = adw::SwitchRow::builder()
                .title(title)
                .subtitle(subtitle)
                .active(initial)
                .build();
            row.connect_active_notify({
                let this = self.clone_handles();
                let ui = Rc::downgrade(ui);
                move |row| {
                    let Some(ui) = ui.upgrade() else { return };
                    let on = row.is_active();
                    this.edit(&ui, |settings| match which {
                        0 => settings.interface.raise_on_capture = on,
                        2 => settings.interface.notify_on_finish = on,
                        3 => settings.interface.watch_clipboard = on,
                        _ => settings.interface.confirm_cancel = on,
                    });
                }
            });
            group.add(&row);
        }

        page.add(&group);
    }

    fn add_actions_group(&self, page: &adw::PreferencesPage, ui: &Rc<Ui>) {
        let group = adw::PreferencesGroup::new();

        let apply = gtk::Button::builder()
            .label("Apply")
            .css_classes(["suggested-action", "pill"])
            .halign(gtk::Align::Center)
            .build();
        let reset = gtk::Button::builder()
            .label("Reset to defaults")
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .build();

        let buttons = gtk::Box::builder()
            .spacing(12)
            .halign(gtk::Align::Center)
            .margin_top(12)
            .build();
        buttons.append(&reset);
        buttons.append(&apply);

        apply.connect_clicked({
            let draft = Rc::clone(&self.draft);
            let ui = Rc::downgrade(ui);
            move |button| {
                let Some(ui) = ui.upgrade() else { return };
                button.set_sensitive(false);

                let next = draft.borrow().clone();
                let backend = ui.backend().clone();
                let button = button.clone();
                let weak = Rc::downgrade(&ui);

                glib::spawn_future_local(async move {
                    let outcome = backend.apply_settings(next).await;
                    button.set_sensitive(true);
                    let Some(ui) = weak.upgrade() else { return };
                    match outcome {
                        Ok(restart) if restart.is_empty() => {
                            ui.clear_settings_dirty();
                            ui.toast("Settings applied");
                        }
                        Ok(restart) => {
                            ui.clear_settings_dirty();
                            // Naming the specific settings beats a generic
                            // "restart required" the user cannot act on.
                            ui.toast(&format!(
                                "Applied. {} take effect after restarting Snatch.",
                                restart.join(", ")
                            ));
                        }
                        Err(error) => ui.toast(&format!("{error:#}")),
                    }
                });
            }
        });

        reset.connect_clicked({
            let ui = Rc::downgrade(ui);
            move |_| {
                let Some(ui) = ui.upgrade() else { return };
                let dialog = adw::AlertDialog::builder()
                    .heading("Reset Settings?")
                    .body("Every setting returns to its default.")
                    .build();
                dialog.add_responses(&[("keep", "Cancel"), ("reset", "Reset")]);
                dialog.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
                dialog.set_close_response("keep");

                let weak = Rc::downgrade(&ui);
                dialog.connect_response(None, move |_, response| {
                    if response != "reset" {
                        return;
                    }
                    let Some(ui) = weak.upgrade() else { return };
                    let backend = ui.backend().clone();
                    let inner = Rc::downgrade(&ui);
                    glib::spawn_future_local(async move {
                        let outcome = backend.apply_settings(Settings::default()).await;
                        let Some(ui) = inner.upgrade() else { return };
                        match outcome {
                            Ok(_) => ui.toast("Settings reset — restart Snatch to apply them all"),
                            Err(error) => ui.toast(&format!("{error:#}")),
                        }
                    });
                });
                dialog.present(Some(ui.window()));
            }
        });

        group.add(&buttons);
        page.add(&group);
    }

    /// A labelled spin button bound to one numeric field.
    fn spin_row(&self, ui: &Rc<Ui>, spec: Numeric) -> adw::SpinRow {
        let Numeric {
            title,
            subtitle,
            min,
            max,
            value,
            setter,
        } = spec;

        let row = adw::SpinRow::builder()
            .title(title)
            .subtitle(subtitle)
            .adjustment(&gtk::Adjustment::new(value, min, max, 1.0, 10.0, 0.0))
            .value(value)
            .build();

        row.connect_value_notify({
            let this = self.clone_handles();
            let ui = Rc::downgrade(ui);
            move |row| {
                let Some(ui) = ui.upgrade() else { return };
                let value = row.value();
                this.edit(&ui, |settings| setter(settings, value));
            }
        });
        row
    }

    /// The shared state a change handler needs, without cloning widgets.
    fn clone_handles(&self) -> Handles {
        Handles {
            draft: Rc::clone(&self.draft),
            loading: Rc::clone(&self.loading),
        }
    }
}

/// One numeric setting: what to show and where to store it.
struct Numeric {
    title: &'static str,
    subtitle: &'static str,
    min: f64,
    max: f64,
    value: f64,
    setter: fn(&mut Settings, f64),
}

/// The pieces of [`SettingsPage`] a signal handler captures.
#[derive(Clone)]
struct Handles {
    draft: Rc<RefCell<Settings>>,
    loading: Rc<std::cell::Cell<bool>>,
}

impl Handles {
    fn edit(&self, ui: &Rc<Ui>, change: impl FnOnce(&mut Settings)) {
        if self.loading.get() {
            return;
        }
        change(&mut self.draft.borrow_mut());
        ui.mark_settings_dirty();
    }
}
