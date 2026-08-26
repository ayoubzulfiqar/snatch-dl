//! The libadwaita front-end.
//!
//! Every widget lives on the GLib main loop and nothing here ever blocks:
//! engine calls are shipped to the tokio runtime with [`Backend::offload`] and
//! awaited from inside `glib::spawn_future_local`, so the window keeps
//! repainting while RPC, IPC and three subprocesses are in flight.
//!
//! The window is an [`adw::ViewStack`] of three pages — Downloads, Torrents and
//! Scraper — each in its own module. Pages own their widgets and row maps but
//! hold no back-reference to [`Ui`]; instead their methods take `&Rc<Ui>`, which
//! keeps the ownership graph a tree and avoids an `Rc` cycle that would leak
//! every row.

mod deps;
mod downloads;
mod format;
mod proxy;
mod scraper;
mod settings;
mod sniff;
mod torrents;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;

use crate::backend::Backend;
use crate::types::{DownloadRequest, JobKind, UiEvent};
use crate::{adw, gtk};
use gtk::{gdk, gio, glib};

const WINDOW_WIDTH: i32 = 900;
const WINDOW_HEIGHT: i32 = 640;

pub const PAGE_DOWNLOADS: &str = "downloads";
pub const PAGE_TORRENTS: &str = "torrents";
pub const PAGE_SCRAPER: &str = "scraper";
pub const PAGE_SETTINGS: &str = "settings";

/// Construct the window on first activation; raise it on every later one.
pub fn build(app: &adw::Application, backend: Backend, events: async_channel::Receiver<UiEvent>) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    install_stylesheet();

    let ui = Ui::new(app, backend);
    ui.window.present();
    ui.load_history();

    // The task owns the only strong reference to `Ui`: it is the application
    // state and lives exactly as long as the event channel.
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            ui.handle(event);
        }
    });
}

/// Snatch's stylesheet, layered above the theme so a user override still wins.
fn install_stylesheet() {
    let Some(display) = gdk::Display::default() else {
        log::warn!("no display; skipping the stylesheet");
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

pub struct Ui {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    stack: adw::ViewStack,
    split: adw::NavigationSplitView,
    sidebar_list: gtk::ListBox,
    /// Page name paired with its sidebar count badge.
    sidebar_rows: Vec<(String, gtk::Label)>,
    backend: Backend,
    downloads: downloads::DownloadsPage,
    torrents: torrents::TorrentsPage,
    scraper: scraper::ScraperPage,
    settings_page: settings::SettingsPage,
    /// Set once if the torrent session failed, so the page can explain itself.
    torrent_error: RefCell<Option<String>>,
    /// True while the Settings page holds unapplied edits.
    settings_dirty: std::cell::Cell<bool>,
}

impl Ui {
    fn new(app: &adw::Application, backend: Backend) -> Rc<Self> {
        let title = adw::WindowTitle::new("Snatch", "No downloads");

        let downloads = downloads::DownloadsPage::new();
        let torrents = torrents::TorrentsPage::new();
        let scraper = scraper::ScraperPage::new();

        let settings_page = settings::SettingsPage::new();

        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(
            downloads.widget(),
            Some(PAGE_DOWNLOADS),
            "Downloads",
            "folder-download-symbolic",
        );
        stack.add_titled_with_icon(
            torrents.widget(),
            Some(PAGE_TORRENTS),
            "Torrents",
            "network-transmit-receive-symbolic",
        );
        stack.add_titled_with_icon(
            scraper.widget(),
            Some(PAGE_SCRAPER),
            "Scraper",
            "image-x-generic-symbolic",
        );
        stack.add_titled_with_icon(
            settings_page.widget(),
            Some(PAGE_SETTINGS),
            "Settings",
            "preferences-system-symbolic",
        );

        // The sidebar. A list rather than a ViewSwitcher: it has room for a
        // description per entry and for a live count, and it is where a
        // desktop user looks for navigation once an app has more than three
        // places to be.
        let sidebar_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();

        let mut sidebar_rows = Vec::new();
        for (name, title, subtitle, icon) in [
            (
                PAGE_DOWNLOADS,
                "Downloads",
                "Files, videos and conversions",
                "folder-download-symbolic",
            ),
            (
                PAGE_TORRENTS,
                "Torrents",
                "Magnets, peers and seeding",
                "network-transmit-receive-symbolic",
            ),
            (
                PAGE_SCRAPER,
                "Scraper",
                "Whole galleries, filed by site",
                "image-x-generic-symbolic",
            ),
            (
                PAGE_SETTINGS,
                "Settings",
                "Speed, segmenting and engines",
                "preferences-system-symbolic",
            ),
        ] {
            let badge = gtk::Label::builder()
                .css_classes(["snatch-badge"])
                .visible(false)
                .build();
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            row.add_suffix(&badge);
            // The page name travels with the row so selection can find it.
            row.set_widget_name(name);
            sidebar_list.append(&row);
            sidebar_rows.push((name.to_owned(), badge));
        }

        let sidebar_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&sidebar_list)
            .build();

        // gtk::Button takes either a label or an icon, not both; ButtonContent
        // is the widget that shows the pair.
        let quick_add = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("list-add-symbolic")
                    .label("Add")
                    .halign(gtk::Align::Center)
                    .build(),
            )
            .css_classes(["suggested-action"])
            .tooltip_text("Add a download, magnet, gallery or video (Ctrl+N)")
            .action_name("win.add")
            .build();
        let sniff_button = gtk::Button::builder()
            .icon_name("edit-find-symbolic")
            .tooltip_text("Find all media on a page (Ctrl+F)")
            .action_name("win.sniff")
            .build();
        let quick_bar = gtk::Box::builder()
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        quick_add.set_hexpand(true);
        quick_bar.append(&quick_add);
        quick_bar.append(&sniff_button);

        let sidebar_toolbar = adw::ToolbarView::builder()
            .content(&sidebar_scroller)
            .build();
        sidebar_toolbar.add_top_bar(
            &adw::HeaderBar::builder()
                .title_widget(&adw::WindowTitle::new("Snatch", ""))
                .show_end_title_buttons(false)
                .build(),
        );
        sidebar_toolbar.add_bottom_bar(&quick_bar);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));
        let banner = adw::Banner::builder().revealed(false).build();

        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .tooltip_text("Main menu")
                .menu_model(&main_menu())
                .primary(true)
                .build(),
        );

        let content_toolbar = adw::ToolbarView::builder().content(&toasts).build();
        content_toolbar.add_top_bar(&header);
        content_toolbar.add_top_bar(&banner);

        let split = adw::NavigationSplitView::builder()
            .sidebar(
                &adw::NavigationPage::builder()
                    .title("Snatch")
                    .child(&sidebar_toolbar)
                    .build(),
            )
            .content(
                &adw::NavigationPage::builder()
                    .title("Downloads")
                    .child(&content_toolbar)
                    .build(),
            )
            .min_sidebar_width(240.0)
            .max_sidebar_width(300.0)
            .build();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Snatch")
            .default_width(WINDOW_WIDTH)
            .default_height(WINDOW_HEIGHT)
            .width_request(360)
            .height_request(400)
            .content(&split)
            .build();

        // On a narrow window the split view shows one pane at a time.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            700.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);

        let ui = Rc::new(Self {
            window,
            title,
            banner,
            toasts,
            stack,
            split,
            sidebar_list,
            sidebar_rows,
            backend,
            downloads,
            torrents,
            scraper,
            settings_page,
            torrent_error: RefCell::new(None),
            settings_dirty: std::cell::Cell::new(false),
        });
        ui.install_actions(app);
        ui.scraper.wire(&ui);
        ui.settings_page.build(&ui);
        ui.wire_sidebar();

        // Reopen where the user left off.
        let last = ui.backend.settings().interface.last_page;
        let start = [PAGE_DOWNLOADS, PAGE_TORRENTS, PAGE_SCRAPER, PAGE_SETTINGS]
            .into_iter()
            .find(|page| *page == last)
            .unwrap_or(PAGE_DOWNLOADS);
        ui.select_page(start);
        ui
    }

    /// Selecting a sidebar row shows its page.
    fn wire_sidebar(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.sidebar_list.connect_row_selected(move |_, row| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(row) = row else { return };
            let name = row.widget_name();
            if name.is_empty() {
                return;
            }
            ui.stack.set_visible_child_name(&name);
            ui.remember_page(&name);
            // Keep the content pane's own title in step, which is what the
            // collapsed layout shows as a back-navigable page name.
            if let Some(page) = ui.split.content() {
                page.set_title(&pretty_page_name(&name));
            }
            // On a narrow window, choosing a destination should reveal it.
            if ui.split.is_collapsed() {
                ui.split.set_show_content(true);
            }
        });
    }

    /// Show a page and move the sidebar selection with it.
    pub fn select_page(self: &Rc<Self>, name: &str) {
        let mut index = 0;
        while let Some(row) = self.sidebar_list.row_at_index(index) {
            if row.widget_name() == name {
                self.sidebar_list.select_row(Some(&row));
                return;
            }
            index += 1;
        }
        // No matching row: still switch the stack rather than doing nothing.
        self.stack.set_visible_child_name(name);
    }

    /// Put a count next to a sidebar entry, or hide it at zero.
    fn set_badge(&self, page: &str, count: usize) {
        for (name, badge) in &self.sidebar_rows {
            if name == page {
                badge.set_visible(count > 0);
                badge.set_text(&count.to_string());
            }
        }
        if let Some(child) = self.stack.child_by_name(page) {
            let stack_page = self.stack.page(&child);
            stack_page.set_badge_number(count as u32);
            stack_page.set_needs_attention(count > 0);
        }
    }

    /// Record the page for the next launch, without disturbing unapplied edits.
    fn remember_page(&self, name: &str) {
        let backend = self.backend.clone();
        let name = name.to_owned();
        backend.clone().spawn(async move {
            let mut settings = backend.settings();
            if settings.interface.last_page == name {
                return;
            }
            settings.interface.last_page = name;
            if let Err(error) = backend.persist_only(settings).await {
                log::debug!("could not remember the page: {error:#}");
            }
        });
    }

    pub fn mark_settings_dirty(&self) {
        if !self.settings_dirty.replace(true) {
            for (name, badge) in &self.sidebar_rows {
                if name == PAGE_SETTINGS {
                    badge.set_visible(true);
                    badge.set_text("•");
                }
            }
        }
    }

    pub fn clear_settings_dirty(&self) {
        if self.settings_dirty.replace(false) {
            for (name, badge) in &self.sidebar_rows {
                if name == PAGE_SETTINGS {
                    badge.set_visible(false);
                }
            }
        }
    }

    fn install_actions(self: &Rc<Self>, app: &adw::Application) {
        self.add_action("add", |ui| ui.present_add_dialog());
        self.add_action("add-torrent-file", |ui| ui.present_torrent_chooser());
        self.add_action("pause-all", |ui| {
            ui.spawn_aria2("Paused all downloads", |client| async move {
                client.pause_all().await
            })
        });
        self.add_action("resume-all", |ui| {
            ui.spawn_aria2("Resumed all downloads", |client| async move {
                client.unpause_all().await
            })
        });
        self.add_action("clear-finished", |ui| {
            ui.spawn_aria2("Cleared finished downloads", |client| async move {
                client.purge_finished().await
            })
        });
        self.add_action("open-folder", |ui| {
            let folder = ui.backend.download_dir.clone();
            ui.reveal(&folder);
        });
        self.add_action("proxies", proxy::present);
        self.add_action("extract-video", |ui| ui.present_video_dialog());
        self.add_action("sniff", |ui| sniff::present(ui, None));
        self.add_action("dependencies", deps::present);
        self.add_action("show-settings", |ui| ui.select_page(PAGE_SETTINGS));
        self.add_action("shortcuts", |ui| ui.present_shortcuts());
        self.add_action("about", |ui| ui.present_about());

        app.set_accels_for_action("win.add", &["<Primary>n"]);
        app.set_accels_for_action("win.pause-all", &["<Primary>p"]);
        app.set_accels_for_action("win.extract-video", &["<Primary>d"]);
        app.set_accels_for_action("win.sniff", &["<Primary>f"]);
        app.set_accels_for_action("win.show-settings", &["<Primary>comma"]);
        app.set_accels_for_action("win.shortcuts", &["<Primary>question"]);
        app.set_accels_for_action("window.close", &["<Primary>w"]);
    }

    fn add_action<F: Fn(&Rc<Self>) + 'static>(self: &Rc<Self>, name: &str, callback: F) {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                callback(&ui);
            }
        });
        self.window.add_action(&action);
    }

    /// Show what previous runs left behind before any live event arrives.
    fn load_history(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let backend = self.backend.clone();
        glib::spawn_future_local(async move {
            let db = backend.db.clone();
            let loaded = backend
                .offload(async move {
                    let batches = db.recent_batches(50).await?;
                    // Only the newest batches are worth thumbnailing on start.
                    let mut files = Vec::new();
                    for batch in batches.iter().take(10) {
                        files.extend(db.batch_files(batch.id, 24).await?);
                    }
                    Ok((batches, files))
                })
                .await;
            let Some(ui) = weak.upgrade() else { return };
            match loaded {
                Ok((batches, files)) => ui.scraper.load(&ui, &batches, &files),
                Err(error) => log::warn!("could not load scrape history: {error:#}"),
            }
        });
    }

    fn handle(self: &Rc<Self>, event: UiEvent) {
        match event {
            UiEvent::Added { name, kind } => {
                self.toast(&format!("Added {name}"));
                // Show the page the job actually landed on: a magnet caught in
                // the browser is confusing if the window opens on Downloads.
                self.select_page(match kind {
                    JobKind::Magnet => PAGE_TORRENTS,
                    JobKind::Scrape => PAGE_SCRAPER,
                    // A yt-dlp extraction produces a file, so it belongs with
                    // the downloads rather than on a page of its own.
                    JobKind::Download | JobKind::Video | JobKind::Sniff => PAGE_DOWNLOADS,
                });
                self.raise_if_hidden();
            }
            UiEvent::Snapshot(list) => {
                let summary = self.downloads.apply(self, &list);
                self.set_badge(PAGE_DOWNLOADS, summary.active);
                self.refresh_title();
            }
            UiEvent::Aria2Up(version) => {
                log::info!("aria2 {version} is online");
                self.banner.set_revealed(false);
            }
            UiEvent::Aria2Down(reason) => {
                log::warn!("aria2 unavailable: {reason}");
                self.banner.set_title(&reason);
                self.banner.set_revealed(true);
            }
            UiEvent::Torrents(list) => {
                let active = self.torrents.apply(self, &list);
                self.set_badge(PAGE_TORRENTS, active);
                self.refresh_title();
            }
            UiEvent::TorrentsUnavailable(reason) => {
                log::warn!("BitTorrent session unavailable: {reason}");
                *self.torrent_error.borrow_mut() = Some(reason.clone());
                self.torrents.set_unavailable(&reason);
            }
            UiEvent::Gallery(event) => {
                self.scraper.handle(self, event);
                self.set_badge(PAGE_SCRAPER, self.backend.gallery.running_count());
            }
            UiEvent::Media(event) => self.downloads.handle_media(self, event),
            UiEvent::SniffRequested { url } => {
                self.raise_if_hidden();
                sniff::present(self, Some(url));
            }
            UiEvent::Video(event) => {
                self.downloads.handle_video(self, event);
                self.refresh_title();
            }
            UiEvent::Wget(event) => {
                self.downloads.handle_wget(self, event);
                self.refresh_title();
            }
            UiEvent::Quit => {
                if let Some(app) = self.window.application() {
                    app.quit();
                } else {
                    self.window.close();
                }
            }
        }
    }

    /// One subtitle covering every engine, so the count is visible on any page.
    fn refresh_title(&self) {
        let downloads = self.downloads.summary();
        let torrents = self.torrents.summary();

        let mut parts = Vec::new();
        if downloads.total > 0 {
            parts.push(format!(
                "{} of {} downloading · {}/s",
                downloads.active,
                downloads.total,
                format::human_bytes(downloads.speed)
            ));
        }
        if torrents.total > 0 {
            parts.push(format!(
                "{} of {} seeding or leeching · {}/s",
                torrents.active,
                torrents.total,
                format::human_bytes(torrents.speed)
            ));
        }
        let scrapes = self.backend.gallery.running_count();
        if scrapes > 0 {
            parts.push(format!("{scrapes} scraping"));
        }
        let videos = self.backend.video.running_count();
        if videos > 0 {
            parts.push(format!("{videos} extracting"));
        }
        let wget_jobs = self.backend.wget.running_count();
        if wget_jobs > 0 {
            parts.push(format!("{wget_jobs} fetching"));
        }
        let encodes = self.backend.media.outstanding();
        if encodes > 0 {
            parts.push(format!("{encodes} converting"));
        }

        self.title.set_subtitle(&if parts.is_empty() {
            "Idle".to_owned()
        } else {
            parts.join(" · ")
        });
    }

    fn raise_if_hidden(&self) {
        if !self.window.is_visible() {
            self.window.present();
        }
    }

    /// Route a request to whichever engine owns it.
    pub fn enqueue(self: &Rc<Self>, request: DownloadRequest) {
        if let Err(error) = request.validate() {
            self.toast(&format!("{error:#}"));
            return;
        }

        match request.inferred_kind() {
            JobKind::Magnet => self.add_magnet(request.url),
            JobKind::Scrape => self.scraper.start(self, request.url),
            JobKind::Video => self.add_video(request.url),
            JobKind::Sniff => sniff::present(self, Some(request.url)),
            JobKind::Download => self.add_download(request),
        }
    }

    /// Queue a download without its own toast, for bulk adds.
    pub fn enqueue_quiet(self: &Rc<Self>, request: DownloadRequest) {
        if let Err(error) = request.validate() {
            log::warn!("skipping {}: {error:#}", request.url);
            return;
        }
        let backend = self.backend.clone();
        glib::spawn_future_local(async move {
            let client = backend.aria2.clone();
            if let Err(error) = backend
                .offload(async move { client.add_uri(&request).await })
                .await
            {
                log::warn!("could not queue a sniffed file: {error:#}");
            }
        });
    }

    fn add_download(self: &Rc<Self>, request: DownloadRequest) {
        // The configured engine decides who fetches it.
        if self.backend.settings().download.engine == crate::settings::HttpEngine::Wget {
            return self.add_wget_download(request);
        }

        let name = request.display_name();
        let weak = Rc::downgrade(self);
        let backend = self.backend.clone();

        glib::spawn_future_local(async move {
            let client = backend.aria2.clone();
            let result = backend
                .offload(async move { client.add_uri(&request).await })
                .await;
            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(gid) => {
                    log::info!("queued '{name}' as gid {gid}");
                    ui.toast(&format!("Added {name}"));
                    ui.select_page(PAGE_DOWNLOADS);
                }
                Err(error) => ui.toast(&format!("Could not add {name}: {error:#}")),
            }
        });
    }

    /// Hand a watch page to yt-dlp with default options.
    pub fn add_video(self: &Rc<Self>, url: String) {
        let config = crate::ytdlp::VideoConfig::new(crate::ytdlp::destination_for(
            &self.backend.download_dir,
        ));
        self.start_video(url, config);
    }

    fn start_video(self: &Rc<Self>, url: String, config: crate::ytdlp::VideoConfig) {
        let weak = Rc::downgrade(self);
        let backend = self.backend.clone();

        glib::spawn_future_local(async move {
            let engine = backend.video.clone();
            let proxies = backend.proxies.clone();
            let events = backend.video_events.clone();

            let result = backend
                .offload(async move { engine.start(url, config, proxies, events).await })
                .await;

            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(_) => ui.stack.set_visible_child_name(PAGE_DOWNLOADS),
                Err(error) => ui.toast(&format!("Could not start the extraction: {error:#}")),
            }
        });
    }

    /// Hand a plain download to Wget2 instead of aria2.
    fn add_wget_download(self: &Rc<Self>, request: DownloadRequest) {
        let name = request.display_name();
        let backend = self.backend.clone();
        let settings = backend.settings();
        let outcome = backend.wget.clone().start(
            request,
            settings,
            backend.proxies.clone(),
            backend.wget_events.clone(),
        );
        match outcome {
            Ok(_) => {
                self.toast(&format!("Added {name}"));
                self.select_page(PAGE_DOWNLOADS);
            }
            Err(error) => self.toast(&format!("Could not add {name}: {error:#}")),
        }
    }

    pub fn add_magnet(self: &Rc<Self>, magnet: String) {
        let weak = Rc::downgrade(self);
        let backend = self.backend.clone();

        glib::spawn_future_local(async move {
            let engine = match backend.torrents() {
                Ok(engine) => engine,
                Err(error) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.toast(&format!("{error:#}"));
                    }
                    return;
                }
            };
            let result = backend
                .offload(async move { engine.add_magnet(&magnet).await })
                .await;
            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(id) => {
                    ui.toast("Added torrent");
                    ui.select_page(PAGE_TORRENTS);
                    log::info!("added torrent {id}");
                }
                Err(error) => ui.toast(&format!("Could not add the torrent: {error:#}")),
            }
        });
    }

    fn present_add_dialog(self: &Rc<Self>) {
        let entry = gtk::Entry::builder()
            .placeholder_text("https://example.com/file.iso, magnet:?xt=…, or a gallery page")
            .input_purpose(gtk::InputPurpose::Url)
            .activates_default(true)
            .hexpand(true)
            .build();

        // The kind is inferred from the URL, but the user can override it.
        let kinds = gtk::DropDown::from_strings(&[
            "Detect automatically",
            "Direct download",
            "Torrent (magnet)",
            "Scrape gallery",
            "Extract video (yt-dlp)",
            "Find all media on the page",
        ]);
        kinds.set_selected(0);

        let fields = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        fields.append(&entry);
        fields.append(&kinds);

        let dialog = adw::AlertDialog::builder()
            .heading("Add to Snatch")
            .body(
                "Direct links go to aria2 with up to 16 connections, magnets to the \
                 BitTorrent engine, and gallery pages to gallery-dl.",
            )
            .extra_child(&fields)
            .build();
        dialog.add_responses(&[("close", "Cancel"), ("add", "Add")]);
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("add", false);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("close");

        entry.connect_changed({
            let dialog = dialog.clone();
            move |entry| dialog.set_response_enabled("add", !entry.text().trim().is_empty())
        });

        // Pre-fill from the clipboard when it already holds something usable.
        glib::spawn_future_local({
            let entry = entry.clone();
            async move {
                let Some(display) = gdk::Display::default() else {
                    return;
                };
                if let Ok(Some(text)) = display.clipboard().read_text_future().await {
                    let text = text.trim();
                    if text.starts_with("http://")
                        || text.starts_with("https://")
                        || text.starts_with("magnet:")
                    {
                        entry.set_text(text);
                    }
                }
            }
        });

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "add" {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            let url = entry.text().trim().to_owned();
            if url.is_empty() {
                return;
            }
            let request = match kinds.selected() {
                1 => DownloadRequest::from_url(url),
                2 => DownloadRequest::magnet(url),
                3 => DownloadRequest::scrape(url),
                4 => DownloadRequest::video(url),
                5 => DownloadRequest::sniff(url),
                // 0: let `inferred_kind` decide from the scheme.
                _ => DownloadRequest::from_url(url),
            };
            ui.enqueue(request);
        });

        dialog.present(Some(&self.window));
    }

    /// Pick a `.torrent` from disk and hand it to the engine.
    fn present_torrent_chooser(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Torrent files"));
        filter.add_pattern("*.torrent");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);

        let chooser = gtk::FileDialog::builder()
            .title("Add a torrent file")
            .filters(&filters)
            .modal(true)
            .build();

        let weak = Rc::downgrade(self);
        chooser.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Some(ui) = weak.upgrade() else { return };
            let path = match result {
                Ok(file) => match file.path() {
                    Some(path) => path,
                    None => {
                        ui.toast("That file has no local path");
                        return;
                    }
                },
                // Dismissing the chooser is not an error worth reporting.
                Err(error) if error.matches(gtk::DialogError::Dismissed) => return,
                Err(error) => {
                    ui.toast(&format!("Could not choose a file: {error}"));
                    return;
                }
            };

            let backend = ui.backend().clone();
            let inner = Rc::downgrade(&ui);
            glib::spawn_future_local(async move {
                let result = match backend.torrents() {
                    Ok(engine) => {
                        backend
                            .offload(async move { engine.add_torrent_file(&path).await })
                            .await
                    }
                    Err(error) => Err(error),
                };
                let Some(ui) = inner.upgrade() else { return };
                match result {
                    Ok(_) => {
                        ui.toast("Added torrent");
                        ui.select_page(PAGE_TORRENTS);
                    }
                    Err(error) => ui.toast(&format!("Could not add the torrent: {error:#}")),
                }
            });
        });
    }

    /// A video extraction with the options yt-dlp actually needs.
    fn present_video_dialog(self: &Rc<Self>) {
        let url = adw::EntryRow::builder()
            .title("Video or playlist URL")
            .build();

        let qualities: Vec<&str> = crate::ytdlp::VideoQuality::all()
            .iter()
            .map(|quality| quality.label())
            .collect();
        let quality = adw::ComboRow::builder()
            .title("Quality")
            .model(&gtk::StringList::new(&qualities))
            .build();

        let subtitles = adw::SwitchRow::builder()
            .title("Subtitles")
            .subtitle("Fetch and embed English subtitles when offered")
            .active(false)
            .build();
        let playlist = adw::SwitchRow::builder()
            .title("Whole playlist")
            .subtitle("Download every item, not just the linked one")
            .active(false)
            .build();

        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        fields.append(&url);
        fields.append(&quality);
        fields.append(&subtitles);
        fields.append(&playlist);

        let dialog = adw::AlertDialog::builder()
            .heading("Extract Video")
            .body("yt-dlp resolves the real media behind a watch page and muxes the result.")
            .extra_child(&fields)
            .build();
        dialog.add_responses(&[("close", "Cancel"), ("go", "Extract")]);
        dialog.set_response_appearance("go", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("go"));
        dialog.set_close_response("close");

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "go" {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            let target = url.text().trim().to_owned();
            if target.is_empty() {
                ui.toast("Enter a URL");
                return;
            }

            let choices = crate::ytdlp::VideoQuality::all();
            let mut config = crate::ytdlp::VideoConfig::new(crate::ytdlp::destination_for(
                &ui.backend.download_dir,
            ));
            config.quality = choices
                .get(quality.selected() as usize)
                .copied()
                .unwrap_or_default();
            config.subtitles = subtitles.is_active();
            config.playlist = playlist.is_active();
            ui.start_video(target, config);
        });

        dialog.present(Some(&self.window));
    }

    fn present_shortcuts(self: &Rc<Self>) {
        let text = [
            ("Ctrl+N", "Add a download, magnet or gallery"),
            ("Ctrl+D", "Extract a video with yt-dlp"),
            ("Ctrl+F", "Sniff a page for media"),
            ("Ctrl+P", "Pause every download"),
            ("Ctrl+Comma", "Settings"),
            ("Ctrl+W", "Close the window"),
            ("Ctrl+?", "This list"),
        ];

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (keys, what) in text {
            list.append(&adw::ActionRow::builder().title(what).subtitle(keys).build());
        }

        let dialog = adw::AlertDialog::builder()
            .heading("Keyboard Shortcuts")
            .extra_child(&list)
            .build();
        dialog.add_responses(&[("close", "Close")]);
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        dialog.present(Some(&self.window));
    }

    fn present_about(self: &Rc<Self>) {
        let torrent_line = match self.backend.torrents.as_ref() {
            Some(engine) => match engine.proxy_label() {
                Some(label) => format!("BitTorrent via librqbit, proxied through {label}"),
                None => "BitTorrent via librqbit (DHT and peer exchange enabled)".to_owned(),
            },
            None => "BitTorrent unavailable".to_owned(),
        };

        adw::AboutDialog::builder()
            .application_name("Snatch")
            .application_icon("folder-download-symbolic")
            .developer_name("Snatch contributors")
            .version(env!("CARGO_PKG_VERSION"))
            .comments(format!(
                "A download manager for Linux.\n\n\
                 HTTP and FTP via aria2.\n{torrent_line}.\n\
                 Galleries via gallery-dl, post-processing via ffmpeg."
            ))
            .license_type(gtk::License::Gpl30)
            .website("https://codeberg.org/snatch-dl/snatch")
            .build()
            .present(Some(&self.window));
    }

    /// Run an aria2 call off the GLib loop and report the outcome as a toast.
    fn spawn_aria2<F, Fut>(self: &Rc<Self>, success: &'static str, make: F)
    where
        F: FnOnce(crate::aria2::Aria2Client) -> Fut + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let weak = Rc::downgrade(self);
        let backend = self.backend.clone();

        glib::spawn_future_local(async move {
            let result = backend.offload(make(backend.aria2.clone())).await;
            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(()) => ui.toast(success),
                Err(error) => ui.toast(&format!("{error:#}")),
            }
        });
    }

    /// Open a path (or its parent directory) in the user's file manager.
    pub fn reveal(&self, path: &Path) {
        let target = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let uri = gio::File::for_path(target).uri();
        if let Err(error) = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
        {
            self.toast(&format!("Could not open {}: {error}", target.display()));
        }
    }

    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn window(&self) -> &adw::ApplicationWindow {
        &self.window
    }
}

/// Human-readable name for a page id.
fn pretty_page_name(name: &str) -> String {
    match name {
        PAGE_DOWNLOADS => "Downloads",
        PAGE_TORRENTS => "Torrents",
        PAGE_SCRAPER => "Scraper",
        PAGE_SETTINGS => "Settings",
        other => other,
    }
    .to_owned()
}

fn main_menu() -> gio::Menu {
    let sources = gio::Menu::new();
    sources.append(Some("Add Torrent File…"), Some("win.add-torrent-file"));
    sources.append(Some("Sniff a Page…"), Some("win.sniff"));
    sources.append(Some("Extract Video…"), Some("win.extract-video"));
    sources.append(Some("Scrape a Page…"), Some("win.scrape"));

    let transfers = gio::Menu::new();
    transfers.append(Some("Pause All"), Some("win.pause-all"));
    transfers.append(Some("Resume All"), Some("win.resume-all"));
    transfers.append(Some("Clear Finished"), Some("win.clear-finished"));

    let settings = gio::Menu::new();
    settings.append(Some("Settings"), Some("win.show-settings"));
    settings.append(Some("Dependencies…"), Some("win.dependencies"));
    settings.append(Some("Proxy Settings…"), Some("win.proxies"));
    settings.append(Some("Open Download Folder"), Some("win.open-folder"));

    let other = gio::Menu::new();
    other.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    other.append(Some("About Snatch"), Some("win.about"));

    let menu = gio::Menu::new();
    menu.append_section(None, &sources);
    menu.append_section(None, &transfers);
    menu.append_section(None, &settings);
    menu.append_section(None, &other);
    menu
}

/// Aggregate numbers a page contributes to the window subtitle.
#[derive(Debug, Clone, Copy, Default)]
pub struct PageSummary {
    pub total: usize,
    pub active: usize,
    pub speed: u64,
}
