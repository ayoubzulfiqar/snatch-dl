//! The proxy settings dialog.
//!
//! The list makes the engine matrix visible, because it is the thing that
//! surprises people: a SOCKS5 endpoint cannot serve aria2 and an HTTP endpoint
//! cannot serve BitTorrent. Each row states which engines it can carry, and
//! the default-proxy selector refuses a choice that would strand one of them.

use std::rc::Rc;

use adw::prelude::*;

use super::Ui;
use crate::network::{Engine, ProxyEndpoint, ProxyHealth};
use crate::{adw, gtk};
use gtk::glib;

pub fn present(ui: &Rc<Ui>) {
    let page = adw::PreferencesPage::builder()
        .title("Proxies")
        .icon_name("network-workgroup-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Proxy Servers")
        .description(
            "aria2 can only use HTTP proxies and BitTorrent can only use SOCKS5. \
             Snatch refuses a pairing it cannot honour rather than connecting directly.",
        )
        .build();

    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add a proxy")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    group.set_header_suffix(Some(&add));

    let test_all = gtk::Button::builder()
        .label("Test All")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();

    let actions = adw::PreferencesGroup::new();
    let torrent_note = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption", "dim-label"])
        .label(
            "The BitTorrent session fixes its proxy when it starts, so changing \
             the torrent proxy takes effect the next time Snatch launches.",
        )
        .build();
    actions.add(&torrent_note);
    actions.set_header_suffix(Some(&test_all));

    page.add(&group);
    page.add(&actions);

    let dialog = adw::PreferencesDialog::builder()
        .title("Proxy Settings")
        .content_width(620)
        .content_height(560)
        .build();
    dialog.add(&page);

    // Rebuild the whole group after any mutation: the list is a handful of
    // rows, so diffing it would be more code than it is worth.
    let rebuild: Rc<dyn Fn()> = {
        let group = group.clone();
        let ui = Rc::downgrade(ui);
        Rc::new(move || {
            let Some(ui) = ui.upgrade() else { return };
            refill(&group, &ui);
        })
    };
    rebuild();

    add.connect_clicked({
        let ui = Rc::downgrade(ui);
        let rebuild = Rc::clone(&rebuild);
        move |_| {
            if let Some(ui) = ui.upgrade() {
                present_add(&ui, Rc::clone(&rebuild));
            }
        }
    });

    test_all.connect_clicked({
        let ui = Rc::downgrade(ui);
        let rebuild = Rc::clone(&rebuild);
        move |button| {
            let Some(ui) = ui.upgrade() else { return };
            button.set_sensitive(false);

            let backend = ui.backend().clone();
            let button = button.clone();
            let rebuild = Rc::clone(&rebuild);
            let weak = Rc::downgrade(&ui);

            glib::spawn_future_local(async move {
                let proxies = backend.proxies.clone();
                let result = backend
                    .offload(async move { Ok(proxies.probe_all().await) })
                    .await;
                button.set_sensitive(true);
                rebuild();

                if let (Ok(results), Some(ui)) = (result, weak.upgrade()) {
                    let healthy = results
                        .iter()
                        .filter(|(_, health)| health.is_healthy())
                        .count();
                    ui.toast(&format!("{healthy} of {} proxies reachable", results.len()));
                }
            });
        }
    });

    dialog.present(Some(ui.window()));
}

fn refill(group: &adw::PreferencesGroup, ui: &Rc<Ui>) {
    // PreferencesGroup has no "clear", so remove what we added last time.
    let mut stale = Vec::new();
    let mut child = group.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(row) = widget.clone().downcast::<adw::ActionRow>() {
            stale.push(row);
        }
    }
    for row in stale {
        group.remove(&row);
    }

    let proxies = ui.backend().proxies.list();
    let default = ui.backend().proxies.default_label();

    if proxies.is_empty() {
        let empty = adw::ActionRow::builder()
            .title("No proxies configured")
            .subtitle("Everything connects directly.")
            .build();
        group.add(&empty);
        return;
    }

    for (proxy, health) in proxies {
        group.add(&build_row(ui, &proxy, health.as_ref(), default.as_deref()));
    }
}

fn build_row(
    ui: &Rc<Ui>,
    proxy: &ProxyEndpoint,
    health: Option<&ProxyHealth>,
    default: Option<&str>,
) -> adw::ActionRow {
    let engines: Vec<&str> = [
        Engine::Aria2,
        Engine::Torrent,
        Engine::Subprocess,
        Engine::Http,
    ]
    .into_iter()
    .filter(|engine| proxy.supports(*engine))
    .map(Engine::label)
    .collect();

    let status = health
        .map(ProxyHealth::summary)
        .unwrap_or_else(|| "not tested".to_owned());

    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&proxy.label))
        // `redacted()` never contains the password.
        .subtitle(glib::markup_escape_text(&format!(
            "{} · carries {} · {status}",
            proxy.redacted(),
            engines.join(", ")
        )))
        .build();

    if let Some(health) = health {
        let icon = gtk::Image::from_icon_name(if health.is_healthy() {
            "emblem-ok-symbolic"
        } else {
            "dialog-warning-symbolic"
        });
        icon.add_css_class(if health.is_healthy() {
            "success"
        } else {
            "error"
        });
        row.add_prefix(&icon);
    }

    let is_default = default == Some(proxy.label.as_str());
    let default_toggle = gtk::CheckButton::builder()
        .active(is_default)
        .tooltip_text("Use this proxy for new jobs")
        .valign(gtk::Align::Center)
        .build();

    let test = gtk::Button::builder()
        .icon_name("network-transmit-receive-symbolic")
        .tooltip_text("Test this proxy")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();

    let delete = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove this proxy")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();

    row.add_suffix(&default_toggle);
    row.add_suffix(&test);
    row.add_suffix(&delete);

    default_toggle.connect_toggled({
        let ui = Rc::downgrade(ui);
        let label = proxy.label.clone();
        move |button| {
            let Some(ui) = ui.upgrade() else { return };
            let wanted = button.is_active();
            let choice = if wanted { Some(label.as_str()) } else { None };
            if let Err(error) = ui.backend().proxies.set_default(choice) {
                ui.toast(&format!("{error:#}"));
                button.set_active(!wanted);
            } else if wanted {
                ui.toast(&format!("New jobs will use {label}"));
            }
        }
    });

    test.connect_clicked({
        let ui = Rc::downgrade(ui);
        let proxy = proxy.clone();
        move |button| {
            let Some(ui) = ui.upgrade() else { return };
            button.set_sensitive(false);

            let backend = ui.backend().clone();
            let button = button.clone();
            let weak = Rc::downgrade(&ui);
            let probe = proxy.clone();

            glib::spawn_future_local(async move {
                let proxies = backend.proxies.clone();
                let health = backend
                    .offload(async move { Ok(proxies.probe(&probe).await) })
                    .await;
                button.set_sensitive(true);
                let Some(ui) = weak.upgrade() else { return };
                match health {
                    Ok(health) => ui.toast(&health.summary()),
                    Err(error) => ui.toast(&format!("{error:#}")),
                }
            });
        }
    });

    delete.connect_clicked({
        let ui = Rc::downgrade(ui);
        let label = proxy.label.clone();
        let row = row.clone();
        move |_| {
            let Some(ui) = ui.upgrade() else { return };
            match ui.backend().proxies.remove(&label) {
                Ok(()) => {
                    row.set_sensitive(false);
                    ui.toast(&format!("Removed {label}"));
                }
                Err(error) => ui.toast(&format!("{error:#}")),
            }
        }
    });

    row
}

fn present_add(ui: &Rc<Ui>, rebuild: Rc<dyn Fn()>) {
    let name = adw::EntryRow::builder().title("Name").build();
    let url = adw::EntryRow::builder()
        .title("URL — socks5://host:1080 or http://user:pass@host:8080")
        .build();

    let fields = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    fields.append(&name);
    fields.append(&url);

    let dialog = adw::AlertDialog::builder()
        .heading("Add Proxy")
        .body("SOCKS5 works for torrents and scrapers; HTTP works for downloads.")
        .extra_child(&fields)
        .build();
    dialog.add_responses(&[("close", "Cancel"), ("save", "Add")]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("close");

    let weak = Rc::downgrade(ui);
    dialog.connect_response(None, move |_, response| {
        if response != "save" {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };

        let raw = url.text().trim().to_owned();
        if raw.is_empty() {
            ui.toast("Enter a proxy URL");
            return;
        }

        match ProxyEndpoint::parse(name.text().to_string(), &raw) {
            Ok(endpoint) => match ui.backend().proxies.upsert(endpoint.clone()) {
                Ok(()) => {
                    ui.toast(&format!("Added {}", endpoint.label));
                    rebuild();
                }
                Err(error) => ui.toast(&format!("{error:#}")),
            },
            Err(error) => ui.toast(&format!("{error:#}")),
        }
    });

    dialog.present(Some(ui.window()));
}
