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
//! Closing the window does not stop anything. A download is handed to the
//! engines the moment it is picked, and they outlive this window the same way
//! they outlive the dialog that started them.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

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

/// Open the browser, optionally at a starting address.
pub fn present(ui: &Rc<Ui>, start: Option<String>) {
    let view = webkit6::WebView::new();
    view.set_vexpand(true);

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

    let finds = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let finds_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(150)
        .max_content_height(260)
        .propagate_natural_height(true)
        .child(&finds)
        .build();

    let heading = gtk::Label::builder()
        .xalign(0.0)
        .label("Nothing found yet — play the video and it will appear here")
        .css_classes(["snatch-section-heading"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .build();

    let found_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_bottom(6)
        .build();
    found_box.append(&heading);
    found_box.append(&finds_scroller);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    column.append(&bar);
    column.append(&view);
    column.append(&found_box);

    let window = adw::Window::builder()
        .title("Browse")
        .default_width(1100)
        .default_height(800)
        .content(&column)
        .build();
    window.set_transient_for(Some(&ui.window));

    // Every address seen so far, so the same manifest fetched every few
    // seconds -- which is what a live playlist is -- is listed once.
    let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    watch(ui, &view, &finds, &heading, &seen);

    {
        let view = view.clone();
        address.connect_activate(move |entry| {
            let typed = entry.text().trim().to_owned();
            if typed.is_empty() {
                return;
            }
            view.load_uri(&normalise(&typed));
        });
    }
    {
        let view = view.clone();
        back.connect_clicked(move |_| view.go_back());
    }
    {
        let view = view.clone();
        forward.connect_clicked(move |_| view.go_forward());
    }
    {
        let view = view.clone();
        reload.connect_clicked(move |_| view.reload());
    }
    // Keep the address bar showing where the page actually went, which is not
    // always where it was sent -- a redirect, or a link the page followed.
    {
        let address = address.clone();
        view.connect_uri_notify(move |view| {
            if let Some(uri) = view.uri() {
                address.set_text(&uri);
            }
        });
    }

    if let Some(start) = start.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        address.set_text(start);
        view.load_uri(&normalise(start));
    }

    window.present();
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

/// Watch what the page fetches and list anything worth downloading.
fn watch(
    ui: &Rc<Ui>,
    view: &webkit6::WebView,
    finds: &gtk::ListBox,
    heading: &gtk::Label,
    seen: &Rc<RefCell<Vec<String>>>,
) {
    let ui = Rc::clone(ui);
    let finds = finds.clone();
    let heading = heading.clone();
    let seen = Rc::clone(seen);

    view.connect_resource_load_started(move |view, resource, request| {
        // The page the request belongs to, for the referer a CDN checks.
        let page = view.uri().map(|uri| uri.to_string()).unwrap_or_default();
        let headers = copy_headers(request.http_headers().as_ref());

        let ui = Rc::clone(&ui);
        let finds = finds.clone();
        let heading = heading.clone();
        let seen = Rc::clone(&seen);

        // Waiting for the answer rather than acting on the request means the
        // server's own content type decides what this is. Plenty of media
        // addresses carry no extension to guess from -- a signed CDN URL
        // ending in a token is the usual shape -- and the answer is the only
        // thing that knows.
        //
        // The answer is read off the resource rather than taken from the
        // `sent-request` signal. That signal's second argument is the
        // *redirected* response, which C leaves null for the ordinary request
        // that was not redirected -- and the binding dereferences it without
        // checking, inside a callback that cannot unwind. The result is not
        // an error a caller could handle: the process aborts, on the first
        // page that loads. Asked for this way it is an `Option`, and the
        // notification arrives as soon as the headers do rather than waiting
        // for a body that, on a live stream, never ends.
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
            let Some(hit) = classify(&url, &mime, response.content_length(), &page, &headers)
            else {
                return;
            };

            {
                let mut seen = seen.borrow_mut();
                if seen.iter().any(|known| known == &hit.url) || seen.len() >= MAX_FINDS {
                    return;
                }
                seen.push(hit.url.clone());
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
            heading.set_label("Found on this page");
            finds.append(&row(&ui, &hit));
        });
    });
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
fn row(ui: &Rc<Ui>, hit: &Hit) -> gtk::ListBoxRow {
    let title = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .label(&hit.label)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();

    let action = gtk::Button::builder()
        .label(match hit.kind {
            Found::Stream => "Record",
            Found::File => "Download",
        })
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .build();

    {
        let ui = Rc::clone(ui);
        let hit = hit.clone();
        action.connect_clicked(move |button| {
            ui.enqueue(request_for(&hit));
            // Queued once. The engines own it from here, so this window can
            // be closed and the download carries on.
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
