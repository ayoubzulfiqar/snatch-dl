//! A browser window inside Snatch, for the pages nothing else can see into.
//!
//! The add-on can only report what the browser told it about. A page that
//! builds its manifest in JavaScript, signs each request with a token it
//! works out on the fly, or hands the player an address that never appears in
//! the HTML, is invisible from outside. So this opens the page *here*, lets
//! its own code run, and watches every request that code makes -- address,
//! headers and the server's answer -- which is the same information the
//! player had.
//!
//! Nothing is intercepted or altered. WebKit fetches the page exactly as it
//! would anyway and this reads the traffic going past, which is why a site
//! that works in a browser works here: it *is* a browser.
//!
//! **What this does not do is DRM.** A page using Encrypted Media Extensions
//! hands its key to a content decryption module, and there is no module here
//! to hand it to -- so such a page will not play, and if it did the decrypted
//! frames would never come back to us. What this solves is the other kind of
//! difficulty: manifests that only exist once the page has run.
//!
//! It is a page in the window rather than a window of its own, so browsing
//! sits beside the downloads it produces: pick something here and it appears
//! on the Downloads page without anything having to be dismissed first. The
//! page and its engine are built once and kept, so going away and coming
//! back does not reload the site or lose what has been found.
//!
//! Leaving the page does not stop anything. A download is handed to the
//! engines the moment it is picked, and they outlive the page the same way
//! they outlive the dialog that started them.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use webkit6::prelude::*;

use super::Ui;
use super::format::elide;
use crate::types::DownloadRequest;
use crate::{adw, gtk};
use gtk::glib;

/// How many finds to keep. A page that streams makes hundreds of requests and
/// the interesting ones are few; this is far above any real page and stops a
/// hostile one growing the list without end.
const MAX_FINDS: usize = 60;

/// What the counter says before anything has been played.
const FOUND_NOTHING: &str = "Nothing found yet";

/// What the server says a playlist is.
const MANIFEST_TYPES: [&str; 6] = [
    "application/vnd.apple.mpegurl",
    "application/x-mpegurl",
    "audio/mpegurl",
    "audio/x-mpegurl",
    "application/dash+xml",
    "video/vnd.mpeg.dash.mpd",
];

/// Headers that belong to the connection rather than to the request, and a
/// couple that would be wrong to replay.
///
/// The rest are copied as they were sent. `DownloadRequest::extra_headers`
/// sanitises again on the way out, so this is the first of two passes rather
/// than the only one.
const SKIP_HEADERS: [&str; 10] = [
    "host",
    "connection",
    "keep-alive",
    "content-length",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "accept-encoding",
    "range",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// A playlist, which ffmpeg records.
    Stream,
    /// A whole file, which the downloader fetches.
    File,
}

#[derive(Debug, Clone)]
struct Hit {
    url: String,
    kind: Found,
    label: String,
    headers: BTreeMap<String, String>,
    /// The page it was found on, which is what a CDN checks the referer for.
    page: String,
}

/// What a tab has found, and which of it has been queued, shared with the
/// dialog that lists it.
type Finds = (Rc<RefCell<Vec<Hit>>>, Rc<RefCell<Vec<String>>>);

/// One tab: a browser, and what that page has played.
///
/// Finds are kept per tab because they belong to a page. A stream found on
/// one channel should not be offered while looking at another.
struct Tab {
    view: webkit6::WebView,
    hits: Rc<RefCell<Vec<Hit>>>,
    queued: Rc<RefCell<Vec<String>>>,
    /// Video this tab's player assembled in JavaScript. See `mse.rs`.
    mse: Rc<RefCell<super::mse::MseCapture>>,
    /// The tab's own content manager, kept so it -- and the ad-block filter
    /// and capture handler on it -- is freed when the tab closes rather than
    /// living for the session.
    content: webkit6::UserContentManager,
}

/// The Browse page: tabs of real browsers, and what each one found.
pub struct BrowserPage {
    root: gtk::Box,
    tabs: adw::TabView,
    address: gtk::Entry,
    back: gtk::Button,
    forward: gtk::Button,
    /// Says how many things the tab on show has played, and opens the list.
    ///
    /// A counter rather than a list down the side: the page is what the
    /// reader is looking at, and a list that is empty most of the time
    /// should not take a fifth of the height to say so.
    found: gtk::Button,
    /// How many popups this session has refused, so blocking is visible
    /// rather than something that might or might not be happening.
    blocked: gtk::Label,
    popups_blocked: Cell<u32>,
    /// Captures being written now, keyed by the id given to the Downloads row
    /// that shows each one. A capture lives here, not in the browser toolbar:
    /// it is a job like any other, so it belongs on the Downloads page, where
    /// it stays whatever happens to the tab that started it.
    captures: RefCell<HashMap<u64, ActiveCapture>>,
    /// The next capture id to hand out. Global, so two tabs' captures never
    /// collide the way their per-page stream ids would.
    next_capture_id: Cell<u64>,
    /// The ad-block rules, compiled once and shared. Each tab has its own
    /// content manager -- so that a script message can be told which tab sent
    /// it -- and the one compiled filter is added to every one of them.
    blocklist: Rc<RefCell<Option<webkit6::UserContentFilter>>>,
    /// Shared by every tab, and carrying the memory-pressure settings that
    /// keep a heavy page from taking the machine down. See `browser_context`.
    context: webkit6::WebContext,
    open_tabs: RefCell<Vec<Tab>>,
    /// For queuing. Set by `attach`, once the rest of the window exists.
    ui: RefCell<Option<Weak<Ui>>>,
}

/// The page-world script that captures in-page video. See `mse.rs`.
const MSE_HOOK: &str = include_str!("mse-hook.js");

/// A capture in progress, shown as a Downloads task.
///
/// It holds an `Rc` to its tab's capture engine, so the engine -- and the temp
/// files under it -- outlives the tab: closing the tab drops the tab's own
/// handle, but this one keeps the bytes alive until they are saved.
struct ActiveCapture {
    /// The engine of the tab this capture came from.
    mse: Rc<RefCell<super::mse::MseCapture>>,
    /// The hook's MediaSource id within that tab.
    ms: i64,
    /// The filename to save under, taken from the page title when armed.
    name: String,
}

/// A web context that tells WebKit to manage memory rather than let a page
/// grow until the machine is out of it.
///
/// A heavy stream site can allocate without end. Left alone, WebKit's process
/// grows until the operating system kills something -- sometimes the whole
/// session. These settings make it shed caches and run the collector as it
/// climbs, and, past a ceiling, kill just its own content process -- which is
/// recoverable, because `watch_crashes` reloads the tab, and which protects
/// everything else running on the machine. The thresholds are gentle and the
/// poll slow, because an aggressive limit with a fast poll makes WebKit spend
/// its time collecting instead of rendering.
fn browser_context() -> webkit6::WebContext {
    let mut memory = webkit6::MemoryPressureSettings::new();
    // Per content process, in MB. Generous: most pages sit far below it, and
    // it exists to catch the one that runs away.
    memory.set_memory_limit(3072);
    // The three thresholds must stay in ascending order, and each setter
    // checks the value against the neighbours already stored -- so they are
    // set highest first, or the defaults reject the ones below them. Kill the
    // content process past the ceiling before it can take the machine with
    // it; that kill is a crash `watch_crashes` catches and reloads from.
    memory.set_kill_threshold(0.85);
    memory.set_strict_threshold(0.65);
    memory.set_conservative_threshold(0.5);
    memory.set_poll_interval(30.0);
    webkit6::WebContext::builder()
        .memory_pressure_settings(&memory)
        .build()
}

impl BrowserPage {
    pub fn new() -> Rc<Self> {
        let tabs = adw::TabView::new();
        tabs.set_vexpand(true);
        let tab_bar = adw::TabBar::builder().view(&tabs).autohide(false).build();
        let new_tab = gtk::Button::from_icon_name("tab-new-symbolic");
        new_tab.set_tooltip_text(Some("New tab"));
        tab_bar.set_end_action_widget(Some(&new_tab));

        let address = gtk::Entry::builder()
            .placeholder_text("Type a web address and press Enter")
            .input_purpose(gtk::InputPurpose::Url)
            .hexpand(true)
            .build();

        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.set_tooltip_text(Some("Back"));
        let forward = gtk::Button::from_icon_name("go-next-symbolic");
        forward.set_tooltip_text(Some("Forward"));
        let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
        reload.set_tooltip_text(Some("Reload"));

        let found = gtk::Button::builder()
            .label(FOUND_NOTHING)
            .sensitive(false)
            .tooltip_text("What this page has played so far")
            .build();

        let blocked = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .visible(false)
            .build();

        let bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        bar.append(&back);
        bar.append(&forward);
        bar.append(&reload);
        bar.append(&address);
        bar.append(&blocked);
        bar.append(&found);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&bar);
        root.append(&tab_bar);
        root.append(&tabs);

        let page = Rc::new(Self {
            root,
            tabs,
            address,
            back,
            forward,
            found,
            blocked,
            popups_blocked: Cell::new(0),
            captures: RefCell::new(HashMap::new()),
            next_capture_id: Cell::new(0),
            blocklist: Rc::new(RefCell::new(None)),
            context: browser_context(),
            open_tabs: RefCell::new(Vec::new()),
            ui: RefCell::new(None),
        });

        page.compile_blocklist();
        page.wire(&reload, &new_tab);
        // There is always a tab to type into.
        page.open_tab(None, None);
        page
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Start queuing, now that there is a `Ui` to queue through.
    ///
    /// Separate from `new` because the page is built while `Ui` is still
    /// being assembled.
    pub fn attach(&self, ui: &Rc<Ui>) {
        *self.ui.borrow_mut() = Some(Rc::downgrade(ui));
    }

    /// Compile the ad-block rules once, then add them to every tab.
    ///
    /// WebKit compiles a rule list into a matcher and caches it on disk, so
    /// this costs something the first time and next to nothing after. It is
    /// asynchronous: a tab that opens before it finishes gets the filter the
    /// moment it is ready, and the first page it loads is simply unfiltered,
    /// which is better than holding the window up for it.
    fn compile_blocklist(self: &Rc<Self>) {
        let Some(cache) =
            dirs::cache_dir().map(|dir| dir.join("snatch-dl").join("content-filters"))
        else {
            return;
        };
        if let Err(error) = std::fs::create_dir_all(&cache) {
            log::warn!(
                "browse: no ad blocking, cannot create {}: {error}",
                cache.display()
            );
            return;
        }
        let store = webkit6::UserContentFilterStore::new(&cache.to_string_lossy());
        let rules = glib::Bytes::from_owned(super::blocklist::rules_json().into_bytes());
        let weak = Rc::downgrade(self);
        store.save(
            "snatch-blocklist",
            &rules,
            None::<&gtk::gio::Cancellable>,
            move |result| match result {
                Ok(filter) => {
                    let Some(page) = weak.upgrade() else { return };
                    // Every tab that already exists, and every one to come.
                    for tab in page.open_tabs.borrow().iter() {
                        tab.content.add_filter(&filter);
                    }
                    *page.blocklist.borrow_mut() = Some(filter);
                    log::info!(
                        "browse: ad blocking on ({} networks)",
                        super::blocklist::blocked_domains().count()
                    );
                }
                Err(error) => log::warn!("browse: ad blocking unavailable: {error}"),
            },
        );
    }

    /// Install the in-page capture hook on a tab's content manager and route
    /// what it sends into that tab's capture.
    fn wire_capture(
        self: &Rc<Self>,
        content: &webkit6::UserContentManager,
        mse: &Rc<RefCell<super::mse::MseCapture>>,
    ) {
        content.register_script_message_handler("snatchMse", None);

        let weak = Rc::downgrade(self);
        let mse = Rc::clone(mse);
        content.connect_script_message_received(Some("snatchMse"), move |_cm, value| {
            let Some(page) = weak.upgrade() else { return };
            let update = mse.borrow_mut().handle(&value.to_str());
            if let super::mse::Update::Present { ms, bytes: 0 } = update {
                log::info!("browse: in-page video detected (ms {ms})");
            }
            // A growing armed stream feeds the task that shows it on the
            // Downloads page, wherever its tab is -- a background tab's capture
            // must keep counting up too.
            if let super::mse::Update::Present { ms, bytes } = update
                && let Some(id) = page.capture_id_for(&mse, ms)
                && let Some(ui) = page.ui.borrow().as_ref().and_then(Weak::upgrade)
            {
                ui.downloads.capture_progress(id, bytes);
            }
            // A stream appearing or growing may change the counter, but only
            // for the tab on show -- a background tab must not repaint it.
            if let super::mse::Update::Present { .. } | super::mse::Update::Ended { .. } = update
                && page
                    .current_capture()
                    .is_some_and(|current| Rc::ptr_eq(&current, &mse))
            {
                page.sync_toolbar();
            }
        });

        let script = webkit6::UserScript::new(
            MSE_HOOK,
            webkit6::UserContentInjectedFrames::AllFrames,
            webkit6::UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        content.add_script(&script);
    }

    /// The capture belonging to the tab on show.
    fn current_capture(&self) -> Option<Rc<RefCell<super::mse::MseCapture>>> {
        let view = self.current_view()?;
        self.open_tabs
            .borrow()
            .iter()
            .find(|tab| tab.view == view)
            .map(|tab| Rc::clone(&tab.mse))
    }

    /// Start capturing a MediaSource, and show it as a task on the Downloads
    /// page. Returns the id of the task, which is how it is stopped later.
    fn register_capture(
        self: &Rc<Self>,
        mse: &Rc<RefCell<super::mse::MseCapture>>,
        ms: i64,
        name: String,
    ) -> Option<u64> {
        let ui = self.ui.borrow().as_ref().and_then(Weak::upgrade)?;
        let id = self.next_capture_id.get();
        self.next_capture_id.set(id + 1);
        self.captures.borrow_mut().insert(
            id,
            ActiveCapture {
                mse: Rc::clone(mse),
                ms,
                name: name.clone(),
            },
        );
        ui.downloads.capture_started(&ui, id, &name);
        Some(id)
    }

    /// The task id capturing a given MediaSource, if one is.
    fn capture_id_for(&self, mse: &Rc<RefCell<super::mse::MseCapture>>, ms: i64) -> Option<u64> {
        self.captures
            .borrow()
            .iter()
            .find(|(_, capture)| capture.ms == ms && Rc::ptr_eq(&capture.mse, mse))
            .map(|(id, _)| *id)
    }

    /// Stop a capture and mux what it took into a file.
    ///
    /// Called from the Downloads row's stop button, and whenever a capture's
    /// tab goes -- navigates, crashes or closes. `detach` takes the bytes out
    /// of the tab's engine into a plan that owns them, so the save finishes
    /// even though the engine is about to be reset or dropped.
    pub fn save_capture(self: &Rc<Self>, id: u64) {
        let Some(ui) = self.ui.borrow().as_ref().and_then(Weak::upgrade) else {
            return;
        };
        let Some(capture) = self.captures.borrow_mut().remove(&id) else {
            return;
        };
        let name = capture.name.clone();
        let plan = capture.mse.borrow_mut().detach(capture.ms);
        // The engine handle can go now: `detach` moved the files out from
        // under it.
        drop(capture);

        let Some(plan) = plan else {
            ui.downloads
                .capture_finished(&ui, id, Err("nothing was captured".to_owned()));
            return;
        };

        glib::spawn_future_local(async move {
            let dest = crate::ytdlp::destination_for(&ui.backend().download_dir);
            let result = ui
                .backend()
                .offload(async move { plan.mux(&dest, &name).await })
                .await
                .map_err(|error| format!("{error:#}"));
            ui.downloads.capture_finished(&ui, id, result);
        });
    }

    /// Save every capture that belongs to a tab's engine, because the tab is
    /// going. The page's JavaScript stops the instant the tab does, so a
    /// capture cannot go on -- but what it took is finished and saved rather
    /// than lost.
    fn finalize_tab_captures(self: &Rc<Self>, mse: &Rc<RefCell<super::mse::MseCapture>>) {
        let ids: Vec<u64> = self
            .captures
            .borrow()
            .iter()
            .filter(|(_, capture)| Rc::ptr_eq(&capture.mse, mse))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.save_capture(id);
        }
    }

    /// Go to an address in the tab on show, or just focus the bar.
    pub fn open(&self, url: Option<&str>) {
        match url.map(str::trim).filter(|url| !url.is_empty()) {
            Some(url) => {
                self.address.set_text(url);
                if let Some(view) = self.current_view() {
                    view.load_uri(&normalise(url));
                }
            }
            None => {
                self.address.grab_focus();
            }
        }
    }

    /// The browser in the tab on show.
    fn current_view(&self) -> Option<webkit6::WebView> {
        let selected = self.tabs.selected_page()?.child();
        self.open_tabs
            .borrow()
            .iter()
            .find(|tab| tab.view.upcast_ref::<gtk::Widget>() == &selected)
            .map(|tab| tab.view.clone())
    }

    /// The finds that belong to a browser.
    fn tab_state(&self, view: &webkit6::WebView) -> Option<Finds> {
        self.open_tabs
            .borrow()
            .iter()
            .find(|tab| &tab.view == view)
            .map(|tab| (Rc::clone(&tab.hits), Rc::clone(&tab.queued)))
    }

    /// Connect the toolbar and the tab strip. Called once, from `new`.
    fn wire(self: &Rc<Self>, reload: &gtk::Button, new_tab: &gtk::Button) {
        let weak = Rc::downgrade(self);
        self.address.connect_activate(move |entry| {
            let Some(page) = weak.upgrade() else { return };
            let typed = entry.text().trim().to_owned();
            if let (false, Some(view)) = (typed.is_empty(), page.current_view()) {
                view.load_uri(&normalise(&typed));
            }
        });

        for (button, go) in [
            (&self.back, Go::Back),
            (&self.forward, Go::Forward),
            (reload, Go::Reload),
        ] {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                let Some(view) = weak.upgrade().and_then(|page| page.current_view()) else {
                    return;
                };
                match go {
                    Go::Back => view.go_back(),
                    Go::Forward => view.go_forward(),
                    Go::Reload => view.reload(),
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            new_tab.connect_clicked(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.open_tab(None, None);
                    page.address.grab_focus();
                }
            });
        }

        // The toolbar always describes the tab on show.
        {
            let weak = Rc::downgrade(self);
            self.tabs.connect_selected_page_notify(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.sync_toolbar();
                }
            });
        }

        // Forget a closed tab's browser and finds, and never leave the page
        // with no tab at all.
        {
            let weak = Rc::downgrade(self);
            self.tabs.connect_close_page(move |tabs, closing| {
                if let Some(page) = weak.upgrade() {
                    let child = closing.child();
                    // Take the tab out, but note the engine it was holding.
                    let mut removed = None;
                    page.open_tabs.borrow_mut().retain(|tab| {
                        let keep = tab.view.upcast_ref::<gtk::Widget>() != &child;
                        if !keep {
                            removed = Some(Rc::clone(&tab.mse));
                        }
                        keep
                    });
                    // A capture must not die with its tab. The page's
                    // JavaScript is gone, so it cannot go on capturing -- but
                    // what it took is saved rather than lost, and the task on
                    // the Downloads page finishes cleanly instead of hanging.
                    if let Some(mse) = removed {
                        let had = page
                            .captures
                            .borrow()
                            .values()
                            .any(|capture| Rc::ptr_eq(&capture.mse, &mse));
                        page.finalize_tab_captures(&mse);
                        if had && let Some(ui) = page.ui.borrow().as_ref().and_then(Weak::upgrade) {
                            ui.toast("Tab closed — saving what was captured");
                        }
                    }
                    // Closed last: open a fresh one once this finishes.
                    if tabs.n_pages() <= 1 {
                        let weak = Rc::downgrade(&page);
                        glib::idle_add_local_once(move || {
                            if let Some(page) = weak.upgrade()
                                && page.tabs.n_pages() == 0
                            {
                                page.open_tab(None, None);
                            }
                        });
                    }
                }
                tabs.close_page_finish(closing, true);
                glib::Propagation::Stop
            });
        }

        {
            let weak = Rc::downgrade(self);
            self.found.connect_clicked(move |button| {
                let Some(page) = weak.upgrade() else { return };
                let Some(ui) = page.ui.borrow().as_ref().and_then(Weak::upgrade) else {
                    return;
                };
                let Some(view) = page.current_view() else {
                    return;
                };
                let Some((hits, queued)) = page.tab_state(&view) else {
                    return;
                };
                let Some(mse) = page.current_capture() else {
                    return;
                };
                present_finds(&page, &ui, button, &view, &hits, &queued, &mse);
            });
        }
    }

    /// Open a tab, optionally at an address, and show it.
    ///
    /// `opener` is the browser a popup came from. WebKit requires a popup to
    /// be *related* to the page that opened it -- the same web process, the
    /// same session -- or it will not hand the popup its content.
    fn open_tab(
        self: &Rc<Self>,
        url: Option<&str>,
        opener: Option<&webkit6::WebView>,
    ) -> webkit6::WebView {
        // Each tab has its own content manager, so a script message can be
        // attributed to the tab it came from: the received signal names no
        // WebView, and the hook's stream ids restart at one per page, so a
        // shared manager could not tell two tabs' captures apart.
        let content = webkit6::UserContentManager::new();
        let mse = Rc::new(RefCell::new(super::mse::MseCapture::new(capture_dir())));
        self.wire_capture(&content, &mse);
        if let Some(filter) = self.blocklist.borrow().as_ref() {
            content.add_filter(filter);
        }

        let mut builder = webkit6::WebView::builder()
            .user_content_manager(&content)
            // The shared context carries the memory-pressure settings.
            .web_context(&self.context);
        // A popup must be related to the page that opened it, or WebKit will
        // not hand it its content.
        if let Some(opener) = opener {
            builder = builder.related_view(opener);
        }
        let view = builder.build();
        view.set_vexpand(true);

        // Send every popup through `guard_popups`, including the ones WebKit
        // would otherwise drop by itself. Left at its default, WebKit refuses
        // a `window.open` with no click behind it before the `create` signal
        // is ever emitted -- which blocks it, but silently, so a page that
        // tried six popunders read "0 popups blocked". One policy in one
        // place, tested, decides all of them; the outcome is the same, and
        // the count is true. Anything it does let through opens as a tab,
        // never as a stray window.
        if let Some(settings) = WebViewExt::settings(&view) {
            settings.set_javascript_can_open_windows_automatically(true);
        }

        let hits: Rc<RefCell<Vec<Hit>>> = Rc::new(RefCell::new(Vec::new()));
        let queued: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        self.open_tabs.borrow_mut().push(Tab {
            view: view.clone(),
            hits: Rc::clone(&hits),
            queued,
            mse: Rc::clone(&mse),
            content,
        });

        let tab = self.tabs.append(&view);
        tab.set_title("New tab");

        self.watch_media(&view, &hits);
        self.watch_navigation(&view, &hits, &mse);
        self.watch_crashes(&view, &tab, &hits, &mse);
        self.watch_tab(&view, &tab);
        self.guard_popups(&view);

        self.tabs.set_selected_page(&tab);
        if let Some(url) = url {
            view.load_uri(&normalise(url));
        }
        view
    }

    /// Forget a tab's finds when it navigates to a new page.
    ///
    /// What a page offered belongs to that page. Without this, moving from a
    /// listing full of clips to the one you picked kept all the listing's
    /// finds alongside the new page's -- so the count only ever grew, and most
    /// of it was stale. The in-page capture is reset for the same reason, and
    /// because the next page's stream ids restart at one and would otherwise
    /// land on top of the last page's.
    fn watch_navigation(
        self: &Rc<Self>,
        view: &webkit6::WebView,
        hits: &Rc<RefCell<Vec<Hit>>>,
        mse: &Rc<RefCell<super::mse::MseCapture>>,
    ) {
        let weak = Rc::downgrade(self);
        let hits = Rc::clone(hits);
        let mse = Rc::clone(mse);
        let watched = view.clone();
        view.connect_load_changed(move |_, event| {
            if event != webkit6::LoadEvent::Started {
                return;
            }
            let had = hits.borrow().len() + mse.borrow().sources().len();
            hits.borrow_mut().clear();
            // Save any capture on this tab before its engine is wiped: the old
            // page is gone, so the capture cannot continue, but it is finished
            // and saved rather than dropped.
            let page = weak.upgrade();
            if let Some(page) = &page {
                page.finalize_tab_captures(&mse);
            }
            mse.borrow_mut().reset();
            if had > 0 {
                log::info!("browse: navigated; cleared {had} find(s) from the last page");
            }
            if let Some(page) = page
                && page.current_view().as_ref() == Some(&watched)
            {
                page.sync_toolbar();
            }
        });
    }

    /// Bring a tab back when its web process dies.
    ///
    /// WebKit runs each page in its own process, and a heavy stream site can
    /// take that process down -- most often by running it out of memory. The
    /// page turns into WebKit's "encountered an error" screen and the tab goes
    /// blank, with no way back but retyping the address. This catches that,
    /// drops the dead page's finds, and reloads once.
    ///
    /// Reloading a page that crashes on load would loop forever, so it is
    /// tried only while the page has managed at least one clean load since the
    /// last run of crashes. A page that keeps dying without ever finishing --
    /// three times over -- is left on its error screen instead of fought,
    /// however long apart the crashes are. A page that loads, works, and
    /// crashes later starts with a clean slate and is reloaded again.
    fn watch_crashes(
        self: &Rc<Self>,
        view: &webkit6::WebView,
        tab: &adw::TabPage,
        hits: &Rc<RefCell<Vec<Hit>>>,
        mse: &Rc<RefCell<super::mse::MseCapture>>,
    ) {
        // Crashes in a row with no successful load between them.
        let in_a_row: Rc<Cell<u32>> = Rc::new(Cell::new(0));

        // A clean load means the page is healthy again; forget past crashes.
        {
            let in_a_row = Rc::clone(&in_a_row);
            view.connect_load_changed(move |_, event| {
                if event == webkit6::LoadEvent::Finished {
                    in_a_row.set(0);
                }
            });
        }

        let weak = Rc::downgrade(self);
        let hits = Rc::clone(hits);
        let mse = Rc::clone(mse);
        let tab = tab.clone();
        let watched = view.clone();
        view.connect_web_process_terminated(move |view, reason| {
            let why = match reason {
                webkit6::WebProcessTerminationReason::ExceededMemoryLimit => "ran out of memory",
                webkit6::WebProcessTerminationReason::Crashed => "crashed",
                _ => "stopped",
            };
            // The page is gone; so is anything it had offered.
            hits.borrow_mut().clear();
            // Whatever a capture took before the crash is on disk already;
            // save it rather than letting the reset throw it away.
            let page = weak.upgrade();
            if let Some(page) = &page {
                page.finalize_tab_captures(&mse);
            }
            mse.borrow_mut().reset();

            let count = in_a_row.get() + 1;
            in_a_row.set(count);
            if count > 3 {
                log::warn!("browse: the page {why} {count} times over; leaving the error page up");
                tab.set_title("Page keeps crashing");
            } else {
                log::warn!("browse: the page {why}; reloading it");
                tab.set_title("Reloading…");
                view.reload();
            }
            if let Some(page) = page
                && page.current_view().as_ref() == Some(&watched)
            {
                page.sync_toolbar();
            }
        });
    }

    /// Keep a tab's title, spinner and the toolbar in step with its page.
    fn watch_tab(self: &Rc<Self>, view: &webkit6::WebView, tab: &adw::TabPage) {
        {
            let tab = tab.clone();
            view.connect_title_notify(move |view| {
                let title = view.title().map(|t| t.to_string()).unwrap_or_default();
                tab.set_title(if title.trim().is_empty() {
                    "Untitled"
                } else {
                    &title
                });
            });
        }
        {
            let tab = tab.clone();
            view.connect_is_loading_notify(move |view| tab.set_loading(view.is_loading()));
        }
        // A page that calls `window.close()` closes its own tab.
        {
            let weak = Rc::downgrade(self);
            let tab = tab.clone();
            view.connect_close(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.tabs.close_page(&tab);
                }
            });
        }
        for signal in [Refresh::Uri, Refresh::Load] {
            let weak = Rc::downgrade(self);
            let watched = view.clone();
            let refresh = move || {
                let Some(page) = weak.upgrade() else { return };
                // Only the tab on show writes the toolbar.
                if page.current_view().as_ref() == Some(&watched) {
                    page.sync_toolbar();
                }
            };
            match signal {
                Refresh::Uri => {
                    view.connect_uri_notify(move |_| refresh());
                }
                Refresh::Load => {
                    view.connect_load_changed(move |_, _| refresh());
                }
            }
        }
    }

    /// Refuse the popups nobody asked for, and open the rest as tabs.
    fn guard_popups(self: &Rc<Self>, view: &webkit6::WebView) {
        let weak = Rc::downgrade(self);
        view.connect_create(move |opener, action| {
            let page = weak.upgrade()?;
            let allowed = allow_popup(action.is_user_gesture(), action.navigation_type());
            if !allowed {
                let refused = page.popups_blocked.get() + 1;
                page.popups_blocked.set(refused);
                page.blocked.set_label(&blocked_label(refused));
                page.blocked.set_visible(true);
                log::info!(
                    "browse: refused a popup to {}",
                    action
                        .request()
                        .and_then(|request| request.uri())
                        .map(|uri| uri.to_string())
                        .unwrap_or_default()
                );
                return None;
            }
            let tab = page.open_tab(None, Some(opener));
            Some(tab.upcast())
        });
    }

    /// Make the toolbar describe the tab on show.
    fn sync_toolbar(&self) {
        let Some(view) = self.current_view() else {
            return;
        };
        self.address
            .set_text(&view.uri().map(|uri| uri.to_string()).unwrap_or_default());
        self.back.set_sensitive(view.can_go_back());
        self.forward.set_sensitive(view.can_go_forward());

        let network = self
            .tab_state(&view)
            .map(|(hits, _)| hits.borrow().len())
            .unwrap_or(0);
        // The counter offers what can still be started. A stream already being
        // captured has left the picker for the Downloads page, so it is not
        // counted here.
        let waiting = self
            .current_capture()
            .map(|mse| mse.borrow().unarmed_sources().len())
            .unwrap_or(0);
        self.show_count(network + waiting);
    }

    fn show_count(&self, total: usize) {
        self.found.set_label(&count_label(total));
        self.found.set_sensitive(total > 0);
        if total > 0 {
            self.found.add_css_class("suggested-action");
        } else {
            self.found.remove_css_class("suggested-action");
        }
    }

    /// Watch what a tab fetches and remember anything worth downloading.
    fn watch_media(self: &Rc<Self>, view: &webkit6::WebView, hits: &Rc<RefCell<Vec<Hit>>>) {
        let weak = Rc::downgrade(self);
        let hits = Rc::clone(hits);
        let watched = view.clone();

        view.connect_resource_load_started(move |view, resource, request| {
            // Every request that actually went out. A request the content
            // blocker stopped never gets here, which makes this the plainest
            // way to see what was blocked: `RUST_LOG=snatch_gui::ui::browser=trace`.
            log::trace!(
                "browse: requesting {}",
                request.uri().map(|uri| uri.to_string()).unwrap_or_default()
            );
            // The page the request belongs to, for the referer a CDN checks.
            let page_url = view.uri().map(|uri| uri.to_string()).unwrap_or_default();
            let headers = copy_headers(request.http_headers().as_ref());

            let weak = weak.clone();
            let hits = Rc::clone(&hits);
            let watched = watched.clone();

            // Waiting for the answer rather than acting on the request means
            // the server's own content type decides what this is. Plenty of
            // media addresses carry no extension to guess from -- a signed
            // CDN URL ending in a token is the usual shape -- and the answer
            // is the only thing that knows.
            //
            // The answer is read off the resource rather than taken from the
            // `sent-request` signal. That signal's second argument is the
            // *redirected* response, which C leaves null for the ordinary
            // request that was not redirected -- and the binding dereferences
            // it without checking, inside a callback that cannot unwind. The
            // result is not an error a caller could handle: the process
            // aborts, on the first page that loads.
            resource.connect_response_notify(move |resource| {
                let Some(response) = resource.response() else {
                    return;
                };
                if !(200..400).contains(&response.status_code()) {
                    return;
                }
                let Some(url) = resource.uri().map(|uri| uri.to_string()) else {
                    return;
                };
                let mime = response
                    .mime_type()
                    .map(|mime| mime.to_string())
                    .unwrap_or_default();
                let Some(hit) =
                    classify(&url, &mime, response.content_length(), &page_url, &headers)
                else {
                    return;
                };

                let total = {
                    let mut hits = hits.borrow_mut();
                    // A live playlist is re-fetched every few seconds, so
                    // without this the same broadcast is counted forever.
                    if hits.iter().any(|known| known.url == hit.url) || hits.len() >= MAX_FINDS {
                        return;
                    }
                    log::info!(
                        "browse: found {} {} ({} header(s) copied)",
                        match hit.kind {
                            Found::Stream => "stream",
                            Found::File => "file",
                        },
                        hit.url,
                        hit.headers.len()
                    );
                    hits.push(hit);
                    hits.len()
                };

                // A background tab finding something must not repaint the
                // counter of the tab on show.
                if let Some(page) = weak.upgrade()
                    && page.current_view().as_ref() == Some(&watched)
                {
                    page.show_count(total);
                }
            });
        });
    }
}

/// Which way a navigation button goes.
#[derive(Clone, Copy)]
enum Go {
    Back,
    Forward,
    Reload,
}

/// Which change to a page should repaint the toolbar.
#[derive(Clone, Copy)]
enum Refresh {
    Uri,
    Load,
}

/// Whether a page may open a new window.
///
/// Two conditions, and a popunder fails the second even when it passes the
/// first:
///
/// * **Somebody asked.** A popup opened with no click behind it -- on load,
///   on a timer -- is an ad, every time.
/// * **They clicked a link.** Popunder networks hijack the first click
///   anywhere on the page, so they *do* arrive with a user gesture. But that
///   click was on a video player or an invisible overlay, and the window is
///   opened from script -- which WebKit reports as `Other`. A genuine "open
///   in a new tab" is a click on an `<a target="_blank">`, and arrives as
///   `LinkClicked`.
///
/// What slips through -- a script that builds a link and clicks it for you
/// -- is what the content blocker is for: the networks that do it are on the
/// list, so their scripts never load.
fn allow_popup(user_gesture: bool, kind: webkit6::NavigationType) -> bool {
    user_gesture && kind == webkit6::NavigationType::LinkClicked
}

/// "3 popups blocked", and "1 popup blocked".
fn blocked_label(refused: u32) -> String {
    match refused {
        1 => "1 popup blocked".to_owned(),
        many => format!("{many} popups blocked"),
    }
}

/// Compile the ad-block rules and give them to every tab.
///
/// WebKit compiles a rule list into a matcher and caches it on disk, so this
/// costs something once and next to nothing after. It is asynchronous: the
/// first page may load before it is ready, and that page is simply not
/// filtered, which is better than holding the window up for it.
/// A fresh temp directory for one tab's in-page captures.
fn capture_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("snatch-mse-{}-{n}", std::process::id()))
}

/// "3 found", and "1 found" rather than "1 founds".
fn count_label(total: usize) -> String {
    match total {
        0 => FOUND_NOTHING.to_owned(),
        1 => "1 found".to_owned(),
        many => format!("{many} found"),
    }
}

/// Show what the page has played, and let one be taken.
///
/// A dialog rather than a permanent list: it is empty most of the time, and
/// the page is what the reader came to look at.
fn present_finds(
    page: &Rc<BrowserPage>,
    ui: &Rc<Ui>,
    anchor: &gtk::Button,
    view: &webkit6::WebView,
    hits: &Rc<RefCell<Vec<Hit>>>,
    queued: &Rc<RefCell<Vec<String>>>,
    mse: &Rc<RefCell<super::mse::MseCapture>>,
) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // Video the page assembled in JavaScript comes first: it is the thing
    // that nothing else could get, which is the reason to be here.
    for ms in mse.borrow().sources() {
        list.append(&mse_row(page, ui, view, mse, ms));
    }

    // Newest first: on a page that has been browsed for a while, the thing
    // just played is the thing being asked for.
    for hit in hits.borrow().iter().rev() {
        list.append(&row(ui, hit, queued));
    }

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .child(&list)
        .build();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(
        &gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .label(
                "Everything this page has played. A capture you start appears \
             on the Downloads page, where you stop and save it.",
            )
            .css_classes(["dim-label"])
            .build(),
    );
    body.append(&scroller);

    // A header bar, because that is what carries the close button. Without
    // one an AdwDialog is a box with no way out of it but the Escape key,
    // which is not a way out anybody can see.
    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new("Found on this page", ""))
        .build();
    let toolbar = adw::ToolbarView::builder().content(&body).build();
    toolbar.add_top_bar(&header);

    let dialog = adw::Dialog::builder()
        .title("Found on this page")
        .content_width(620)
        .content_height(480)
        .child(&toolbar)
        .build();
    dialog.present(Some(anchor));
}

/// Give a typed word a scheme, so "example.com" goes somewhere.
fn normalise(typed: &str) -> String {
    if typed.contains("://") {
        return typed.to_owned();
    }
    // A word with a dot in it is a host; anything else is a search.
    if typed.contains(' ') || !typed.contains('.') {
        return format!(
            "https://duckduckgo.com/?q={}",
            glib::Uri::escape_string(typed, None, false)
        );
    }
    format!("https://{typed}")
}

/// Decide whether an address is worth offering, and as what.
fn classify(
    url: &str,
    mime: &str,
    length: u64,
    page: &str,
    headers: &BTreeMap<String, String>,
) -> Option<Hit> {
    if crate::stream::validate_url(url).is_err() {
        return None;
    }
    // A page that streams fetches its video as hundreds of pieces, and every
    // one of them is a small valid video. The manifest is the better answer
    // for those and arrives on the same page.
    if crate::stream::is_fragment(url) {
        return None;
    }

    let mime = mime.split(';').next().unwrap_or(mime).trim().to_lowercase();
    let extension = crate::stream::extension_of(url);

    let manifest = MANIFEST_TYPES.contains(&mime.as_str())
        || matches!(extension.as_deref(), Some("m3u8" | "m3u" | "mpd"));
    let media = mime.starts_with("video/") || mime.starts_with("audio/");
    if !manifest && !media {
        return None;
    }

    // A byte range is the same file asked for a piece at a time; trimming
    // that off is what makes it the file again.
    let url = crate::stream::whole_file(url).unwrap_or_else(|| url.to_owned());

    let name = url
        .split(['?', '#'])
        .next()
        .unwrap_or(&url)
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("stream")
        .to_owned();

    let label = if manifest {
        format!("{} — stream", elide(&name, 48))
    } else if length > 0 {
        format!(
            "{} — {}",
            elide(&name, 40),
            super::format::human_bytes(length)
        )
    } else {
        elide(&name, 48)
    };

    Some(Hit {
        url,
        kind: if manifest { Found::Stream } else { Found::File },
        label,
        headers: headers.clone(),
        page: page.to_owned(),
    })
}

/// Copy the headers the page's own request carried.
fn copy_headers(headers: Option<&webkit6::soup::MessageHeaders>) -> BTreeMap<String, String> {
    let mut copied = BTreeMap::new();
    let Some(headers) = headers else {
        return copied;
    };
    headers.foreach(|name, value| {
        let lowered = name.to_ascii_lowercase();
        if SKIP_HEADERS.contains(&lowered.as_str()) {
            return;
        }
        if value.is_empty() || value.len() > 4096 {
            return;
        }
        copied.insert(name.to_owned(), value.to_owned());
    });
    copied
}

/// One row, with the button that queues it.
/// A row for video the page is assembling in JavaScript.
///
/// Capture is two steps on purpose. The first arms it: from then on every
/// piece the player appends is kept, and for a live stream that is what you
/// want -- capture now, save when you have enough. The second saves what has
/// been kept so far into one file. A row opened before arming offers Capture;
/// after, once there are bytes, it offers Save.
fn mse_row(
    page: &Rc<BrowserPage>,
    ui: &Rc<Ui>,
    view: &webkit6::WebView,
    mse: &Rc<RefCell<super::mse::MseCapture>>,
    ms: i64,
) -> gtk::ListBoxRow {
    let armed = mse.borrow().is_armed(ms);

    let title = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .label("In-page video")
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();

    let action = gtk::Button::builder()
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .build();

    if armed {
        // Already being captured: it is a task on the Downloads page now, with
        // its size climbing and a button to stop and save, so here it is only
        // a note.
        action.set_label("Capturing…");
        action.set_sensitive(false);
    } else {
        action.set_label("Capture");
        let page = Rc::clone(page);
        let ui = Rc::clone(ui);
        let view = view.clone();
        let mse = Rc::clone(mse);
        action.connect_clicked(move |button| {
            mse.borrow_mut().arm(ms);
            // Tell the page to send the pieces it has been holding, and
            // everything from here on.
            view.evaluate_javascript(
                &format!("window.__snatchArm({ms})"),
                None,
                None,
                gtk::gio::Cancellable::NONE,
                |_| {},
            );
            // Show it as a task on the Downloads page, and take the reader
            // there so the capture they just started is in front of them.
            let started = page
                .register_capture(&mse, ms, capture_name(&view))
                .is_some();
            if let Some(dialog) = button
                .ancestor(adw::Dialog::static_type())
                .and_downcast::<adw::Dialog>()
            {
                dialog.close();
            }
            if started {
                ui.select_page(super::PAGE_DOWNLOADS);
                ui.toast("Capturing — stop and save it from the Downloads page");
            }
        });
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&title);
    content.append(&action);

    gtk::ListBoxRow::builder()
        .child(&content)
        .activatable(false)
        .build()
}

/// A filename for a capture, from the page's title.
fn capture_name(view: &webkit6::WebView) -> String {
    let title = view.title().map(|t| t.to_string()).unwrap_or_default();
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "capture".to_owned()
    } else {
        cleaned.chars().take(120).collect()
    }
}

fn row(ui: &Rc<Ui>, hit: &Hit, queued: &Rc<RefCell<Vec<String>>>) -> gtk::ListBoxRow {
    let already = queued.borrow().iter().any(|url| url == &hit.url);
    let title = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .label(&hit.label)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();

    let action = gtk::Button::builder()
        .label(if already {
            "Queued"
        } else {
            match hit.kind {
                Found::Stream => "Record",
                Found::File => "Download",
            }
        })
        .sensitive(!already)
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .build();

    {
        let ui = Rc::clone(ui);
        let hit = hit.clone();
        let queued = Rc::clone(queued);
        action.connect_clicked(move |button| {
            ui.enqueue(request_for(&hit));
            queued.borrow_mut().push(hit.url.clone());
            // Queued once. The engines own it from here, so the dialog and
            // the page can both be left and the download carries on.
            button.set_sensitive(false);
            button.set_label("Queued");
        });
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&title);
    content.append(&action);

    gtk::ListBoxRow::builder()
        .child(&content)
        .activatable(false)
        .build()
}

/// Turn a find into the request the engines take.
fn request_for(hit: &Hit) -> DownloadRequest {
    let mut request = match hit.kind {
        Found::Stream => DownloadRequest::stream(hit.url.clone()),
        Found::File => DownloadRequest::from_url(hit.url.clone()),
    };

    // Three of these have fields of their own because every engine takes them
    // as a named option; the rest travel together.
    let mut extra = hit.headers.clone();
    for (name, slot) in [
        ("cookie", &mut request.cookies),
        ("user-agent", &mut request.user_agent),
        ("referer", &mut request.referer),
    ] {
        if let Some(key) = extra
            .keys()
            .find(|key| key.eq_ignore_ascii_case(name))
            .cloned()
            && let Some(value) = extra.remove(&key)
        {
            *slot = Some(value);
        }
    }
    // The page itself is the referer when the request did not carry one,
    // which is the usual case for the very first fetch a player makes.
    if request.referer.is_none() && !hit.page.is_empty() {
        request.referer = Some(hit.page.clone());
    }
    request.headers = extra;
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    use webkit6::NavigationType;

    /// A popup opened with nobody clicking is an ad, every time.
    #[test]
    fn a_popup_nobody_asked_for_is_refused() {
        for kind in [
            NavigationType::LinkClicked,
            NavigationType::Other,
            NavigationType::FormSubmitted,
        ] {
            assert!(!allow_popup(false, kind), "{kind:?} with no gesture");
        }
    }

    /// The popunder trick: it hijacks the first click anywhere on the page,
    /// so it *has* a user gesture -- but the window comes from script, not
    /// from a link, and that is what gives it away.
    #[test]
    fn a_popunder_riding_a_real_click_is_still_refused() {
        assert!(!allow_popup(true, NavigationType::Other));
        assert!(!allow_popup(true, NavigationType::FormSubmitted));
    }

    /// A genuine "open in a new tab" is a click on a link, and gets a tab.
    #[test]
    fn a_link_the_reader_clicked_opens_as_a_tab() {
        assert!(allow_popup(true, NavigationType::LinkClicked));
    }

    #[test]
    fn the_blocked_count_reads_properly() {
        assert_eq!(blocked_label(1), "1 popup blocked");
        assert_eq!(blocked_label(4), "4 popups blocked");
    }

    #[test]
    fn the_counter_says_how_many_and_says_one_properly() {
        assert_eq!(count_label(0), FOUND_NOTHING);
        assert_eq!(count_label(1), "1 found");
        assert_eq!(count_label(7), "7 found");
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn a_playlist_is_recorded_and_a_file_is_downloaded() {
        let manifest = classify(
            "https://c.example/live/master.m3u8",
            "application/vnd.apple.mpegurl",
            0,
            "https://site.example/watch",
            &BTreeMap::new(),
        )
        .expect("a manifest is worth offering");
        assert_eq!(manifest.kind, Found::Stream);

        let file = classify(
            "https://c.example/clip",
            "video/mp4",
            5_000_000,
            "https://site.example/watch",
            &BTreeMap::new(),
        )
        .expect("a media answer is worth offering");
        assert_eq!(file.kind, Found::File);
        // No extension to guess from, so the server's answer is what decided
        // it -- which is the whole reason for waiting for the response.
        assert!(file.label.contains("MiB"), "{}", file.label);
    }

    #[test]
    fn the_pieces_of_a_stream_are_not_offered() {
        // A live page fetches hundreds of these and each one is a small valid
        // video. Listing them would bury the manifest.
        for (url, mime) in [
            ("https://c.example/seg-00042.m4s", "video/iso.segment"),
            ("https://c.example/videoplayback?id=1&sq=137", "video/mp4"),
        ] {
            assert!(
                classify(url, mime, 1000, "https://site.example/", &BTreeMap::new()).is_none(),
                "{url} should be skipped"
            );
        }
    }

    #[test]
    fn a_slice_of_a_file_is_offered_as_the_whole_file() {
        let hit = classify(
            "https://c.example/film.mp4?range=0-524287&rn=3",
            "video/mp4",
            524_288,
            "https://site.example/",
            &BTreeMap::new(),
        )
        .expect("still worth offering");
        assert_eq!(hit.url, "https://c.example/film.mp4");
    }

    #[test]
    fn everything_that_is_not_media_is_ignored() {
        for (url, mime) in [
            ("https://site.example/app.js", "application/javascript"),
            ("https://site.example/style.css", "text/css"),
            ("https://site.example/page", "text/html"),
            ("https://site.example/logo.png", "image/png"),
        ] {
            assert!(classify(url, mime, 0, "https://site.example/", &BTreeMap::new()).is_none());
        }
    }

    #[test]
    fn the_players_own_request_is_what_gets_replayed() {
        let hit = classify(
            "https://c.example/live/master.m3u8",
            "application/x-mpegurl",
            0,
            "https://site.example/watch",
            &headers(&[
                ("Cookie", "session=abc"),
                ("User-Agent", "Mozilla/5.0"),
                ("Origin", "https://site.example"),
                ("X-Playback-Token", "let-me-in"),
            ]),
        )
        .expect("a manifest");

        let request = request_for(&hit);
        assert_eq!(request.kind, crate::types::JobKind::Stream);
        assert_eq!(request.cookies.as_deref(), Some("session=abc"));
        assert_eq!(request.user_agent.as_deref(), Some("Mozilla/5.0"));
        // Nothing sent one, so the page it was found on stands in.
        assert_eq!(
            request.referer.as_deref(),
            Some("https://site.example/watch")
        );
        // The rest travel together, and the named three are not repeated.
        assert_eq!(
            request.headers.get("X-Playback-Token").map(String::as_str),
            Some("let-me-in")
        );
        assert_eq!(
            request.headers.get("Origin").map(String::as_str),
            Some("https://site.example")
        );
        assert!(
            !request
                .headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("cookie"))
        );
    }

    #[test]
    fn a_typed_word_is_given_a_scheme_or_a_search() {
        assert_eq!(normalise("https://example.com/x"), "https://example.com/x");
        assert_eq!(normalise("example.com"), "https://example.com");
        assert!(normalise("live football").starts_with("https://duckduckgo.com/?q="));
        assert!(normalise("weather").starts_with("https://duckduckgo.com/?q="));
    }
}
