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
use std::collections::BTreeMap;
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
    /// Shared by every tab, and carrying the ad-block rules -- so a filter
    /// compiled once applies everywhere, including to a tab opened later.
    content: webkit6::UserContentManager,
    open_tabs: RefCell<Vec<Tab>>,
    /// For queuing. Set by `attach`, once the rest of the window exists.
    ui: RefCell<Option<Weak<Ui>>>,
}

impl BrowserPage {
    pub fn new() -> Rc<Self> {
        let content = webkit6::UserContentManager::new();
        install_blocklist(&content);

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
            content,
            open_tabs: RefCell::new(Vec::new()),
            ui: RefCell::new(None),
        });

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
                    page.open_tabs
                        .borrow_mut()
                        .retain(|tab| tab.view.upcast_ref::<gtk::Widget>() != &child);
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
                let Some((hits, queued)) = page.current_view().and_then(|v| page.tab_state(&v))
                else {
                    return;
                };
                present_finds(&ui, button, &hits, &queued);
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
        let view = match opener {
            Some(opener) => webkit6::WebView::builder().related_view(opener).build(),
            None => webkit6::WebView::builder()
                .user_content_manager(&self.content)
                .build(),
        };
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
        });

        let tab = self.tabs.append(&view);
        tab.set_title("New tab");

        self.watch_media(&view, &hits);
        self.watch_tab(&view, &tab);
        self.guard_popups(&view);

        self.tabs.set_selected_page(&tab);
        if let Some(url) = url {
            view.load_uri(&normalise(url));
        }
        view
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
        let total = self
            .tab_state(&view)
            .map(|(hits, _)| hits.borrow().len())
            .unwrap_or(0);
        self.show_count(total);
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
fn install_blocklist(content: &webkit6::UserContentManager) {
    let Some(cache) = dirs::cache_dir().map(|dir| dir.join("snatch-dl").join("content-filters"))
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
    let content = content.clone();
    store.save(
        "snatch-blocklist",
        &rules,
        None::<&gtk::gio::Cancellable>,
        move |result| match result {
            Ok(filter) => {
                content.add_filter(&filter);
                log::info!(
                    "browse: ad blocking on ({} networks)",
                    super::blocklist::blocked_domains().count()
                );
            }
            Err(error) => log::warn!("browse: ad blocking unavailable: {error}"),
        },
    );
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
    ui: &Rc<Ui>,
    anchor: &gtk::Button,
    hits: &Rc<RefCell<Vec<Hit>>>,
    queued: &Rc<RefCell<Vec<String>>>,
) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

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
                "Everything this page has played. Recording keeps going after \
             this window is closed.",
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
