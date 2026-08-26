//! The dependency dialog: what is installed, what is missing, and what to do.
//!
//! Two classes of missing tool get two different treatments. yt-dlp and
//! gallery-dl publish standalone checksummed binaries, so Snatch offers an
//! **Install** button. aria2 and FFmpeg need root, so Snatch shows the exact
//! command for the detected distribution with a copy button, and never runs it
//! — a download manager that asks for your password is a download manager you
//! should not trust.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::Ui;
use super::format::human_bytes;
use crate::deps::{Distro, InstallProgress, Tool, ToolStatus};
use crate::{adw, gtk};
use gtk::{gdk, glib};

/// Show the dialog and survey the system.
pub fn present(ui: &Rc<Ui>) {
    let group = adw::PreferencesGroup::builder()
        .title("External Tools")
        .description(
            "Snatch drives these programs. Torrents need none of them — \
             the BitTorrent engine is built in.",
        )
        .build();

    let page = adw::PreferencesPage::builder()
        .title("Dependencies")
        .icon_name("system-run-symbolic")
        .build();
    page.add(&group);

    let dialog = adw::PreferencesDialog::builder()
        .title("Dependencies")
        .content_width(660)
        .content_height(560)
        .build();
    dialog.add(&page);

    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    refresh(ui, &group, &rows);
    dialog.present(Some(ui.window()));
}

/// Re-survey and rebuild the list.
fn refresh(ui: &Rc<Ui>, group: &adw::PreferencesGroup, rows: &Rc<RefCell<Vec<adw::ActionRow>>>) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }

    let weak = Rc::downgrade(ui);
    let group = group.clone();
    let rows = Rc::clone(rows);
    let backend = ui.backend().clone();

    glib::spawn_future_local(async move {
        let managed = backend.managed_bin_dir.clone();
        let surveyed = backend
            .offload(async move { Ok(crate::deps::survey(&managed).await) })
            .await;

        let Some(ui) = weak.upgrade() else { return };
        let statuses = match surveyed {
            Ok(statuses) => statuses,
            Err(error) => {
                ui.toast(&format!("{error:#}"));
                return;
            }
        };

        let distro = Distro::detect();
        for status in statuses {
            let row = build_row(&ui, &status, distro, &group, &rows);
            group.add(&row);
            rows.borrow_mut().push(row);
        }
    });
}

fn build_row(
    ui: &Rc<Ui>,
    status: &ToolStatus,
    distro: Distro,
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
) -> adw::ActionRow {
    let subtitle = match (&status.path, &status.version) {
        (Some(path), Some(version)) => format!(
            "{version} · {}{}",
            path.display(),
            if status.managed {
                " · installed by Snatch"
            } else {
                ""
            }
        ),
        (Some(path), None) => path.display().to_string(),
        (None, _) => format!("Not installed — needed for {}", status.tool.purpose()),
    };

    let row = adw::ActionRow::builder()
        .title(status.tool.title())
        .subtitle(glib::markup_escape_text(&subtitle))
        .build();

    let icon = if status.present() {
        let image = gtk::Image::from_icon_name("emblem-ok-symbolic");
        image.add_css_class("success");
        image
    } else if status.tool.required() {
        let image = gtk::Image::from_icon_name("dialog-error-symbolic");
        image.add_css_class("error");
        image
    } else {
        let image = gtk::Image::from_icon_name("dialog-warning-symbolic");
        image.add_css_class("warning");
        image
    };
    row.add_prefix(&icon);

    if status.present() {
        return row;
    }

    if status.tool.self_installable() {
        row.add_suffix(&install_button(ui, status.tool, group, rows));
    } else if let Some(command) = status.manual_command(distro) {
        row.add_suffix(&copy_button(ui, &command));
        row.set_subtitle(&glib::markup_escape_text(&format!("{subtitle}\n{command}")));
    }

    row
}

/// Download, verify and install a standalone tool.
fn install_button(
    ui: &Rc<Ui>,
    tool: Tool,
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
) -> gtk::Box {
    let progress = gtk::ProgressBar::builder()
        .width_request(140)
        .valign(gtk::Align::Center)
        .visible(false)
        .show_text(true)
        .build();

    let button = gtk::Button::builder()
        .label("Install")
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .build();

    let container = gtk::Box::builder().spacing(8).build();
    container.append(&progress);
    container.append(&button);

    button.connect_clicked({
        let ui = Rc::downgrade(ui);
        let progress = progress.clone();
        let group = group.clone();
        let rows = Rc::clone(rows);

        move |button| {
            let Some(ui) = ui.upgrade() else { return };
            button.set_sensitive(false);
            button.set_label("Installing…");
            progress.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_text(Some("starting"));

            // Progress arrives from a tokio task; hop it back to the GLib loop
            // through a channel rather than touching widgets off-thread.
            let (tx, rx) = async_channel::unbounded::<InstallProgress>();
            let backend = ui.backend().clone();
            let managed = backend.managed_bin_dir.clone();

            glib::spawn_future_local({
                let progress = progress.clone();
                async move {
                    while let Ok(update) = rx.recv().await {
                        match update {
                            InstallProgress::Resolving => {
                                progress.set_text(Some("finding release"));
                                progress.pulse();
                            }
                            InstallProgress::Downloading { received, total } => match total {
                                Some(total) if total > 0 => {
                                    progress.set_fraction(
                                        (received as f64 / total as f64).clamp(0.0, 1.0),
                                    );
                                    progress.set_text(Some(&format!(
                                        "{} of {}",
                                        human_bytes(received),
                                        human_bytes(total)
                                    )));
                                }
                                _ => {
                                    progress.pulse();
                                    progress.set_text(Some(&human_bytes(received)));
                                }
                            },
                            InstallProgress::Verifying => {
                                progress.set_fraction(1.0);
                                progress.set_text(Some("verifying"));
                            }
                            InstallProgress::Installed(path) => {
                                progress.set_fraction(1.0);
                                progress.set_text(Some(
                                    path.file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| "done".to_owned())
                                        .as_str(),
                                ));
                            }
                        }
                    }
                }
            });

            let button = button.clone();
            let progress = progress.clone();
            let group = group.clone();
            let rows = Rc::clone(&rows);
            let weak = Rc::downgrade(&ui);

            glib::spawn_future_local(async move {
                let outcome = backend
                    .offload(async move {
                        crate::deps::install(tool, &managed, move |update| {
                            // An unbounded send only fails once the UI is gone.
                            let _ = tx.send_blocking(update);
                        })
                        .await
                    })
                    .await;

                progress.set_visible(false);
                button.set_sensitive(true);
                button.set_label("Install");

                let Some(ui) = weak.upgrade() else { return };
                match outcome {
                    Ok(path) => {
                        ui.toast(&format!("Installed {} to {}", tool.title(), path.display()));
                        refresh(&ui, &group, &rows);
                    }
                    Err(error) => {
                        ui.toast(&format!("Could not install {}: {error:#}", tool.title()))
                    }
                }
            });
        }
    });

    container
}

/// Put an install command on the clipboard for the user to run themselves.
fn copy_button(ui: &Rc<Ui>, command: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy the install command")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();

    button.connect_clicked({
        let ui = Rc::downgrade(ui);
        let command = command.to_owned();
        move |_| {
            let Some(ui) = ui.upgrade() else { return };
            match gdk::Display::default() {
                Some(display) => {
                    display.clipboard().set_text(&command);
                    ui.toast("Command copied — run it in a terminal");
                }
                None => ui.toast(&command),
            }
        }
    });

    button
}
