//! The "sniff a page" dialog.
//!
//! Give it a URL, it lists everything downloadable on the page, and the user
//! ticks what they want. The list is grouped by kind with per-group select-all,
//! because a gallery page routinely yields two hundred images and nobody wants
//! to tick those individually.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::Ui;
use super::format::{elide, human_bytes};
use crate::sniff::{Candidate, MediaKind, SniffOptions, SniffResult};
use crate::types::DownloadRequest;
use crate::{adw, gtk};
use gtk::{gdk, glib};

const PAGE_FORM: &str = "form";
const PAGE_BUSY: &str = "busy";
const PAGE_RESULTS: &str = "results";

/// Open the dialog, optionally pre-filled with a URL.
pub fn present(ui: &Rc<Ui>, prefill: Option<String>) {
    let entry = gtk::Entry::builder()
        .placeholder_text("https://example.com/page-with-media")
        .input_purpose(gtk::InputPurpose::Url)
        .activates_default(true)
        .hexpand(true)
        .build();

    let use_extractor = gtk::CheckButton::builder()
        .label("Also ask yt-dlp (finds streams the page does not link directly)")
        .active(true)
        .build();
    let probe = gtk::CheckButton::builder()
        .label("Check each link for its real type and size")
        .active(true)
        .build();

    let form = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    form.append(
        &gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .label(
                "Snatch reads the page and lists every image, video, audio file, \
             document and archive it can reach — then you pick.",
            )
            .css_classes(["snatch-hint"])
            .build(),
    );
    form.append(&entry);
    form.append(&use_extractor);
    form.append(&probe);

    // gtk::Spinner rather than adw::Spinner: the latter needs libadwaita 1.6
    // and Snatch targets 1.5 so it builds on Ubuntu 24.04.
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .width_request(32)
        .height_request(32)
        .build();
    let busy_label = gtk::Label::builder()
        .label("Reading the page…")
        .css_classes(["snatch-hint"])
        .build();
    let busy = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .vexpand(true)
        .build();
    busy.append(&spinner);
    busy.append(&busy_label);

    let results_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let results_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&results_box)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    stack.add_named(&form, Some(PAGE_FORM));
    stack.add_named(&busy, Some(PAGE_BUSY));
    stack.add_named(&results_scroller, Some(PAGE_RESULTS));
    stack.set_visible_child_name(PAGE_FORM);

    let sniff_button = gtk::Button::builder()
        .label("Sniff")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let download_button = gtk::Button::builder()
        .label("Download")
        .css_classes(["suggested-action"])
        .visible(false)
        .build();

    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new("Sniff a Page", ""))
        .build();
    header.pack_end(&sniff_button);
    header.pack_end(&download_button);

    let toolbar = adw::ToolbarView::builder().content(&stack).build();
    toolbar.add_top_bar(&header);

    let dialog = adw::Dialog::builder()
        .title("Sniff a Page")
        .content_width(760)
        .content_height(620)
        .child(&toolbar)
        .build();

    // Every checkbox the results page creates, so Download can read them.
    let selection: Rc<RefCell<Vec<(gtk::CheckButton, Candidate)>>> =
        Rc::new(RefCell::new(Vec::new()));

    entry.connect_changed({
        let sniff_button = sniff_button.clone();
        move |entry| sniff_button.set_sensitive(!entry.text().trim().is_empty())
    });

    let autostart = prefill.is_some();
    if let Some(url) = prefill {
        entry.set_text(url.trim());
    } else {
        // Offer the clipboard, the way the add dialog does.
        glib::spawn_future_local({
            let entry = entry.clone();
            async move {
                let Some(display) = gdk::Display::default() else {
                    return;
                };
                if let Ok(Some(text)) = display.clipboard().read_text_future().await {
                    let text = text.trim();
                    if text.starts_with("http://") || text.starts_with("https://") {
                        entry.set_text(text);
                    }
                }
            }
        });
    }

    sniff_button.connect_clicked({
        let ui = Rc::downgrade(ui);
        let entry = entry.clone();
        let stack = stack.clone();
        let results_box = results_box.clone();
        let selection = Rc::clone(&selection);
        let sniff_button = sniff_button.clone();
        let download_button = download_button.clone();
        let busy_label = busy_label.clone();
        let use_extractor = use_extractor.clone();
        let probe = probe.clone();
        let header = header.clone();

        move |_| {
            let Some(ui) = ui.upgrade() else { return };
            let url = entry.text().trim().to_owned();
            if url.is_empty() {
                return;
            }

            stack.set_visible_child_name(PAGE_BUSY);
            busy_label.set_text("Reading the page…");
            sniff_button.set_sensitive(false);

            let backend = ui.backend().clone();
            let options = SniffOptions {
                use_extractor: use_extractor.is_active(),
                probe: probe.is_active(),
                yt_dlp: crate::deps::resolve(crate::deps::Tool::YtDlp, &backend.managed_bin_dir),
            };

            let stack = stack.clone();
            let results_box = results_box.clone();
            let selection = Rc::clone(&selection);
            let sniff_button = sniff_button.clone();
            let download_button = download_button.clone();
            let header = header.clone();
            let weak = Rc::downgrade(&ui);

            glib::spawn_future_local(async move {
                let proxies = backend.proxies.clone();
                let target = url.clone();
                let outcome = backend
                    .offload(async move {
                        // Sniffing is Snatch's own traffic, so it honours the
                        // proxy assigned to internal requests.
                        let proxy = proxies
                            .resolve_for("sniff", crate::network::Engine::Http)
                            .unwrap_or(None);
                        let client = proxies.client(proxy.as_ref())?;
                        crate::sniff::sniff(&target, client, options).await
                    })
                    .await;

                sniff_button.set_sensitive(true);
                let Some(ui) = weak.upgrade() else { return };

                match outcome {
                    Ok(result) => {
                        let count = result.candidates.len();
                        if count == 0 {
                            ui.toast("Nothing downloadable found on that page");
                            stack.set_visible_child_name(PAGE_FORM);
                            return;
                        }
                        let heading = result
                            .page_title
                            .clone()
                            .unwrap_or_else(|| host_of(&result.page_url));
                        header.set_title_widget(Some(&adw::WindowTitle::new(
                            &elide(&heading, 60),
                            &format!("{count} found on {}", host_of(&result.page_url)),
                        )));
                        fill_results(&results_box, &selection, &result);
                        stack.set_visible_child_name(PAGE_RESULTS);
                        sniff_button.set_visible(false);
                        download_button.set_visible(true);
                    }
                    Err(error) => {
                        ui.toast(&format!("{error:#}"));
                        stack.set_visible_child_name(PAGE_FORM);
                    }
                }
            });
        }
    });

    download_button.connect_clicked({
        let ui = Rc::downgrade(ui);
        let selection = Rc::clone(&selection);
        let dialog = dialog.clone();
        move |_| {
            let Some(ui) = ui.upgrade() else { return };
            let chosen: Vec<Candidate> = selection
                .borrow()
                .iter()
                .filter(|(check, _)| check.is_active())
                .map(|(_, candidate)| candidate.clone())
                .collect();

            if chosen.is_empty() {
                ui.toast("Nothing selected");
                return;
            }

            for candidate in &chosen {
                let mut request = DownloadRequest::from_url(candidate.url.clone());
                request.filename = Some(candidate.filename());
                request.mime = candidate.mime.clone();
                request.size = candidate.size;
                ui.enqueue_quiet(request);
            }
            ui.toast(&format!("Queued {} file(s)", chosen.len()));
            dialog.close();
        }
    });

    dialog.present(Some(ui.window()));

    // A URL handed over from the browser or the socket was already chosen by
    // the user; making them press Sniff as well is pure friction.
    if autostart {
        sniff_button.emit_clicked();
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

/// Build one group per media kind, each with its own select-all.
fn fill_results(
    container: &gtk::Box,
    selection: &Rc<RefCell<Vec<(gtk::CheckButton, Candidate)>>>,
    result: &SniffResult,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    selection.borrow_mut().clear();

    for note in &result.notes {
        container.append(
            &gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .label(note)
                .css_classes(["snatch-hint"])
                .build(),
        );
    }

    for kind in MediaKind::ALL {
        let members: Vec<&Candidate> = result
            .candidates
            .iter()
            .filter(|candidate| candidate.kind == kind)
            .collect();
        if members.is_empty() {
            continue;
        }

        // Video and audio are almost always what the user came for.
        let default_on = matches!(kind, MediaKind::Video | MediaKind::Audio);

        let toggle_all = gtk::CheckButton::builder()
            .active(default_on)
            .tooltip_text("Select every item in this group")
            .valign(gtk::Align::Center)
            .build();

        let group = adw::PreferencesGroup::builder()
            .title(format!("{} ({})", kind.label(), members.len()))
            .header_suffix(&toggle_all)
            .build();

        let mut checks = Vec::with_capacity(members.len());
        for candidate in members {
            let check = gtk::CheckButton::builder()
                .active(default_on)
                .valign(gtk::Align::Center)
                .build();

            let mut facts = vec![candidate.origin.label().to_owned()];
            if let Some(size) = candidate.size {
                facts.push(human_bytes(size));
            }
            if let Some(mime) = &candidate.mime {
                facts.push(mime.split(';').next().unwrap_or(mime).to_owned());
            }
            if !candidate.verified {
                facts.push("unverified".to_owned());
            }

            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&candidate.filename()))
                .subtitle(glib::markup_escape_text(&facts.join(" · ")))
                .tooltip_text(&candidate.url)
                .activatable_widget(&check)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(kind.icon()));
            row.add_suffix(&check);
            group.add(&row);

            checks.push(check.clone());
            selection.borrow_mut().push((check, (*candidate).clone()));
        }

        toggle_all.connect_toggled(move |button| {
            let wanted = button.is_active();
            for check in &checks {
                check.set_active(wanted);
            }
        });

        container.append(&group);
    }
}
