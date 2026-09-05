"use strict";

/**
 * Snatch - browser side.
 *
 * Two things happen here:
 *
 *   1. `webRequest` observes traffic and remembers metadata that the downloads
 *      API does not expose: the exact Referer and User-Agent that were sent,
 *      the Content-Disposition filename, the Content-Type and the size.
 *   2. `downloads.onCreated` performs the actual hand-off: the browser download
 *      is cancelled and erased, and the URL - with cookies, referer and user
 *      agent attached - is pushed to the native host `com.snatch.dl.nmh`.
 *   3. `webRequest` also watches for the HLS and DASH manifests a page's
 *      player fetches, and remembers them per tab. They are the fallback for
 *      the sites yt-dlp has never heard of: it cannot read the page, but the
 *      player still had to ask for a manifest, and ffmpeg can read one of
 *      those without knowing anything about the site.
 *   4. `content.js` draws a button on every video and asks, through here, what
 *      resolutions a page offers. That question and the download that follows
 *      are the only things a content script may ask for: it has no native
 *      messaging of its own, so everything it wants goes through `onMessage`
 *      below, where the URL is checked before Snatch ever sees it.
 *
 * The cancel deliberately lives in `downloads.onCreated` rather than in
 * `webRequest.onBeforeRequest`: Manifest V3 removed blocking webRequest in
 * Chromium, so `onBeforeRequest` can observe but not stop a request. By the
 * time a download item exists the browser has already committed to saving the
 * response, which is exactly the moment it is safe to take over.
 */

const api = globalThis.browser ?? globalThis.chrome;

const HOST_NAME = "com.snatch.dl.nmh";
const IS_FIREFOX =
  typeof globalThis.browser !== "undefined" &&
  typeof globalThis.browser.runtime.getBrowserInfo === "function";

const MENU_DOWNLOAD = "snatch-download-with";
const MENU_SCRAPE = "snatch-scrape-page";
const MENU_VIDEO = "snatch-extract-video";
const MENU_SNIFF = "snatch-sniff-page";
const MENU_OVERLAY = "snatch-video-overlay";
const HINT_TTL_MS = 90000;
const HINT_LIMIT = 256;
const BADGE_MS = 4000;

/** Control characters that must never reach a filename. */
const CONTROL_CHARACTERS = /[\u0000-\u001F\u007F]/g;

/** File types worth watching for on the wire. */
const DOWNLOAD_EXTENSIONS = new Set([
  "7z", "aac", "ai", "apk", "appimage", "avi", "bin", "bz2", "cab", "crx",
  "deb", "dmg", "doc", "docx", "epub", "exe", "flac", "flv", "gz", "img",
  "iso", "jar", "m4a", "m4v", "mkv", "mobi", "mov", "mp3", "mp4", "mpg",
  "msi", "odp", "ods", "odt", "ogg", "ogv", "opus", "pdf", "pkg", "ppt",
  "pptx", "psd", "rar", "rpm", "run", "sh", "sig", "snap", "sql", "tar",
  "tgz", "torrent", "ts", "txz", "vdi", "vmdk", "wav", "webm", "whl", "wma",
  "wmv", "xls", "xlsx", "xz", "zip", "zst"
]);

/**
 * Playlist and manifest types, by file extension.
 *
 * Smooth Streaming (.ism) and Adobe HDS (.f4m) are deliberately absent:
 * ffmpeg has no demuxer for either, so noticing one would only spend a
 * candidate slot on something that can never be recorded.
 */
const MANIFEST_EXTENSIONS = new Set(["m3u8", "m3u", "mpd"]);

/** ...and by what the server says they are, for the ones with no extension. */
const MANIFEST_TYPES = new Set([
  "application/vnd.apple.mpegurl",
  "application/x-mpegurl",
  "audio/mpegurl",
  "audio/x-mpegurl",
  "application/dash+xml",
  "video/vnd.mpeg.dash.mpd"
]);

/**
 * Whole media files, for the sites that just serve one.
 *
 * Everything ffmpeg can open and aria2 can fetch, not only the handful the
 * modern web uses: an archive of lecture recordings is .wmv, a fan site is
 * .rmvb, a podcast back catalogue is .oga, and a downloader that only knows
 * .mp4 walks past all of them. Nothing here is offered without ffprobe
 * opening it first, so a wrong guess costs a second, not a bad file.
 */
const MEDIA_EXTENSIONS = new Set([
  // Video containers.
  "mp4", "webm", "m4v", "mov", "mkv", "avi", "flv", "3gp", "3g2", "ogv",
  "ts", "mts", "m2ts", "mpg", "mpeg", "m2v", "mpv", "wmv", "asf", "f4v",
  "vob", "divx", "rm", "rmvb", "mxf", "y4m", "qt", "amv",
  // Audio containers.
  "m4a", "mp3", "aac", "flac", "wav", "ogg", "opus", "m4b", "m4r",
  "wma", "weba", "oga", "spx", "aiff", "aif", "aifc", "caf", "ape",
  "wv", "mka", "dsf", "dff", "amr", "ac3", "eac3", "dts", "mp2", "mpa",
  "au", "ra", "tta", "shn", "voc", "w64", "gsm"
]);

/**
 * Pieces of a stream, whatever their size. A four-second fragment is not a
 * download, and a page playing one produces hundreds of them.
 */
const FRAGMENT_EXTENSIONS = new Set(["m4s", "cmfv", "cmfa", "fmp4"]);

/**
 * MPEG-TS is both: the container a whole broadcast is served in, and the
 * container each four-second piece of an HLS stream is served in. Only the
 * size tells them apart, so one of these counts as a file to offer only when
 * the server says how big it is and it is far too big to be a fragment.
 */
const SEGMENT_OR_WHOLE = new Set(["ts", "mts", "m2ts"]);
const WHOLE_FILE_BYTES = 20 * 1024 * 1024;

/**
 * Request headers worth copying onto Snatch's own request.
 *
 * Not a guess at what a site wants: these are read off the request the page's
 * player actually made, and handed to ffmpeg or yt-dlp so it makes the same
 * one. `Origin` and `Referer` are what a CDN checks to see the request came
 * from the site; `Authorization` and the `X-` headers are where players put
 * their session tokens. Without them a manifest that plays perfectly in the
 * tab comes back 403 to everything else, which is the single most common
 * reason a download fails on a site that works.
 *
 * Everything else is left out on purpose. `Accept-Encoding` would promise a
 * compression the recorder never asked for, `Host` and `Content-Length`
 * belong to whoever opens the connection, and the rest carry no access.
 */
const COPIED_HEADERS = new Set([
  "origin",
  "referer",
  "authorization",
  "user-agent",
  "cookie",
  "x-requested-with",
  "x-forwarded-for",
  "x-csrf-token",
  "x-api-key",
  "x-auth-token",
  "x-access-token",
  "x-playback-session-id"
]);

/** Any of the site's own headers, which is where the awkward ones live. */
const COPIED_PREFIX = /^x-/i;

/** The headers observed per media URL. Bounded like `hints`, and for the same reason. */
const mediaHeaders = new Map();
const HEADER_URL_LIMIT = 128;

/**
 * The same, kept per origin as well.
 *
 * A manifest is not always recognisable from its address -- a signed CDN URL
 * ending in a token is a common shape -- so the exact one being asked about
 * may never have had its headers recorded under its own name. Every media
 * request to one host carries the same access headers, though, so the host's
 * are the right answer for any address on it.
 */
const originHeaders = new Map();
const HEADER_ORIGIN_LIMIT = 32;

/** Most manifests worth remembering for one tab. */
const STREAM_LIMIT = 24;
const FILE_LIMIT = 8;
const STREAM_TTL_MS = 15 * 60 * 1000;

/**
 * How far before a video started playing to still count a request as its own.
 *
 * A player fetches its manifest a moment before the element reports that it
 * is loading, so the window has to open slightly earlier than the event.
 */
const PLAYBACK_GRACE_MS = 15000;

const WEB_REQUEST_FILTER = { urls: ["http://*/*", "https://*/*"] };
const WEB_REQUEST_TYPES = [
  "main_frame", "sub_frame", "xmlhttprequest", "object", "media", "other"
];

/**
 * Per-URL metadata gathered from webRequest.
 *
 * In Chromium the service worker can be evicted between events, which loses
 * this map. That only degrades the hand-off (we fall back to the URL for the
 * filename); it never breaks it, and the eviction window is far larger than
 * the milliseconds between a response arriving and its download item existing.
 */
const hints = new Map();

/** URLs we deliberately let the browser handle once (hand-off fallback). */
const passThrough = new Set();

/**
 * Whole media files each tab has loaded, newest last. See `rememberFile`.
 */
const files = new Map();

/**
 * Manifests each tab's player has fetched, newest last.
 *
 * Like `hints` this is lost if Chromium evicts the service worker, and for the
 * same reason it rarely matters: a live playlist is re-fetched every few
 * seconds, which is exactly the traffic that keeps the worker awake. A page
 * quiet enough for the worker to be evicted is one with nothing playing.
 */
const streams = new Map();

/** Hostnames the user has told the button to leave alone. */
let hiddenSitesCache = null;

/** Cached copy of the enabled flag; `null` until first read. */
let enabledCache = null;

/** Cached copy of the video-button flag; `null` until first read. */
let overlayCache = null;

// ---------------------------------------------------------------------------
// Enabled flag
// ---------------------------------------------------------------------------

async function isEnabled() {
  if (enabledCache !== null) {
    return enabledCache;
  }
  try {
    const stored = await api.storage.local.get({ enabled: true });
    enabledCache = stored.enabled !== false;
  } catch (error) {
    console.warn("Snatch: could not read settings, defaulting to enabled", error);
    enabledCache = true;
  }
  return enabledCache;
}

async function setEnabled(value) {
  enabledCache = Boolean(value);
  try {
    await api.storage.local.set({ enabled: enabledCache });
  } catch (error) {
    console.warn("Snatch: could not persist settings", error);
  }
  await refreshAction();
}

/**
 * Whether the button on videos is wanted.
 *
 * Separate from `enabled` because they answer different questions: pausing
 * Snatch stops it taking downloads, while this only stops it drawing on pages.
 * Someone who finds the pill intrusive should not have to give up capture.
 */
async function isOverlayEnabled() {
  if (overlayCache !== null) {
    return overlayCache;
  }
  try {
    const stored = await api.storage.local.get({ overlay: true });
    overlayCache = stored.overlay !== false;
  } catch (error) {
    console.warn("Snatch: could not read the video button setting", error);
    overlayCache = true;
  }
  return overlayCache;
}

async function setOverlayEnabled(value) {
  overlayCache = Boolean(value);
  try {
    await api.storage.local.set({ overlay: overlayCache });
    // Turning it back on means "show it again", including on the sites it was
    // dismissed on one at a time. Without this the toggle would look broken:
    // ticked, and still nothing on the site you last hid it from.
    if (overlayCache) {
      hiddenSitesCache = [];
      await api.storage.local.set({ hiddenSites: [] });
    }
  } catch (error) {
    console.warn("Snatch: could not persist the video button setting", error);
  }
  await broadcastOverlay();
}

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch (error) {
    return "";
  }
}

/** Sites the user dismissed the button on, with the × on the pill. */
async function hiddenSites() {
  if (hiddenSitesCache !== null) {
    return hiddenSitesCache;
  }
  try {
    const stored = await api.storage.local.get({ hiddenSites: [] });
    hiddenSitesCache = Array.isArray(stored.hiddenSites) ? stored.hiddenSites : [];
  } catch (error) {
    console.warn("Snatch: could not read the hidden sites", error);
    hiddenSitesCache = [];
  }
  return hiddenSitesCache;
}

async function hideSite(url) {
  const host = hostOf(url);
  if (!host) {
    return false;
  }
  const sites = await hiddenSites();
  if (!sites.includes(host)) {
    hiddenSitesCache = sites.concat(host);
    try {
      await api.storage.local.set({ hiddenSites: hiddenSitesCache });
    } catch (error) {
      console.warn("Snatch: could not persist the hidden sites", error);
    }
  }
  return true;
}

/** Whether the button belongs on this particular page. */
async function isOverlayWanted(pageUrl) {
  if (!(await isEnabled()) || !(await isOverlayEnabled())) {
    return false;
  }
  const host = hostOf(pageUrl);
  return host === "" || !(await hiddenSites()).includes(host);
}

/**
 * Tell every open page whether to draw the button.
 *
 * Most tabs have no content script - a settings page, a PDF, a tab that was
 * open before the extension was installed - so a rejection here is the normal
 * case and not worth reporting.
 */
async function broadcastOverlay() {
  const globallyOn = (await isEnabled()) && (await isOverlayEnabled());
  const hidden = globallyOn ? await hiddenSites() : [];
  let tabs = [];
  try {
    tabs = await api.tabs.query({});
  } catch (error) {
    return;
  }
  for (const tab of tabs) {
    if (typeof tab.id !== "number") {
      continue;
    }
    // The list goes to the page and the page decides, rather than this
    // reading `tab.url` to decide for it. Firefox hands out a tab's address
    // only with the "tabs" permission, which asks the reader for their
    // browsing history -- a lot to charge for something the page already
    // knows about itself.
    try {
      const sent = api.tabs.sendMessage(tab.id, {
        type: "snatch-overlay",
        enabled: globallyOn,
        hidden: hidden
      });
      if (sent && typeof sent.catch === "function") {
        sent.catch(() => {});
      }
    } catch (error) {
      // No content script in that tab.
    }
  }
}

async function refreshAction() {
  const enabled = await isEnabled();
  try {
    await api.action.setTitle({
      title: enabled
        ? "Snatch is capturing downloads - click to pause"
        : "Snatch is paused - click to capture downloads"
    });
    await api.action.setBadgeText({ text: enabled ? "" : "off" });
    await api.action.setBadgeBackgroundColor({ color: "#77767b" });
  } catch (error) {
    console.warn("Snatch: could not update the toolbar button", error);
  }
}

function flashBadge(text, color) {
  try {
    api.action.setBadgeText({ text: text });
    api.action.setBadgeBackgroundColor({ color: color });
    setTimeout(() => void refreshAction(), BADGE_MS);
  } catch (error) {
    console.warn("Snatch: could not flash the toolbar button", error);
  }
}

// ---------------------------------------------------------------------------
// Stream cache
// ---------------------------------------------------------------------------

function isManifestUrl(url) {
  try {
    const path = new URL(url).pathname;
    const dot = path.lastIndexOf(".");
    if (dot < 0 || dot === path.length - 1) {
      return false;
    }
    return MANIFEST_EXTENSIONS.has(path.slice(dot + 1).toLowerCase());
  } catch (error) {
    return false;
  }
}

/**
 * Keep the headers one media request carried.
 *
 * Recorded against the URL rather than the tab: a page can be playing two
 * things at once, and the token that opens one of them is no use for the
 * other.
 */
function rememberHeaders(url, requestHeaders) {
  if (!url || !Array.isArray(requestHeaders)) {
    return;
  }
  const kept = {};
  let any = false;
  for (const header of requestHeaders) {
    const name = String(header.name || "").toLowerCase();
    if (!COPIED_HEADERS.has(name) && !COPIED_PREFIX.test(name)) {
      continue;
    }
    const value = header.value;
    if (typeof value !== "string" || value === "" || value.length > 4096) {
      continue;
    }
    kept[name] = value;
    any = true;
  }
  if (!any) {
    return;
  }
  store(mediaHeaders, url, kept, HEADER_URL_LIMIT);
  const origin = originOf(url);
  if (origin) {
    store(originHeaders, origin, kept, HEADER_ORIGIN_LIMIT);
  }
}

/** Insert at the end and prune the front, so the map is oldest-first. */
function store(map, key, value, limit) {
  map.delete(key);
  map.set(key, value);
  while (map.size > limit) {
    map.delete(map.keys().next().value);
  }
}

function originOf(url) {
  try {
    return new URL(url).origin;
  } catch (error) {
    return "";
  }
}

/**
 * The headers to send with these addresses, merged newest-wins.
 *
 * Merged rather than picked one at a time because the whole set goes on one
 * request: Snatch asks for the master playlist, and ffmpeg follows it to the
 * renditions and segments itself, all with the headers it was given.
 */
function headersFor(urls) {
  const merged = {};
  for (const url of urls) {
    // This exact address if it was seen, and otherwise whatever else on the
    // same host was: the access headers are the host's, not the file's.
    const seen = mediaHeaders.get(url) ?? originHeaders.get(originOf(url));
    if (!seen) {
      continue;
    }
    for (const [name, value] of Object.entries(seen)) {
      merged[name] = value;
    }
  }
  return merged;
}

function remember(store, tabId, url, limit) {
  if (typeof tabId !== "number" || tabId < 0 || !isHijackable(url)) {
    return;
  }
  const now = Date.now();
  const kept = (store.get(tabId) ?? []).filter((entry) => now - entry.at < STREAM_TTL_MS);
  const seen = kept.find((entry) => entry.url === url);
  if (seen) {
    // Counted where it stands rather than moved to the end. How often a
    // manifest is asked for is the signal below, and reordering would throw
    // away the order they were first seen in.
    seen.at = now;
    seen.hits += 1;
  } else {
    kept.push({ url: url, at: now, hits: 1 });
  }
  store.set(tabId, kept.slice(-limit));
}

function rememberStream(tabId, url) {
  remember(streams, tabId, url, STREAM_LIMIT);
}

function extensionOf(url) {
  try {
    const path = new URL(url).pathname;
    const dot = path.lastIndexOf(".");
    if (dot < 0 || dot === path.length - 1) {
      return "";
    }
    return path.slice(dot + 1).toLowerCase();
  } catch (error) {
    return "";
  }
}

/**
 * Remember a whole media file this tab loaded.
 *
 * Skipped entirely once the tab has shown a manifest. A page that streams
 * fetches its video as hundreds of fragments, and every one of them looks like
 * a media file; the manifest is already the better answer for that page, so
 * there is nothing to gain by listing its pieces.
 */
/**
 * Query parameters a player adds to ask for one slice of a file.
 *
 * `range` is the slice; `rn` and `rbuf` are the bookkeeping beside it. None
 * of them name a different file, so dropping them turns "the first half
 * megabyte of the film" back into "the film".
 */
const SLICE_PARAMETERS = new Set(["range", "rn", "rbuf"]);

/**
 * The whole file an address is one slice of.
 *
 * A page that streams asks for its video a slice at a time, so the same file
 * goes past a hundred times under a hundred addresses. Trimming the slice off
 * makes them one address again -- which is both the right thing to download
 * and the difference between remembering one file and filling the list with
 * a hundred copies of it.
 *
 * Rebuilt by hand rather than through URLSearchParams, which re-encodes every
 * other parameter on the way out and invalidates a CDN's signature.
 */
function wholeFile(url) {
  const mark = url.indexOf("?");
  if (mark < 0) {
    return url;
  }
  const hash = url.indexOf("#", mark);
  const head = url.slice(0, mark);
  const query = url.slice(mark + 1, hash < 0 ? undefined : hash);
  const tail = hash < 0 ? "" : url.slice(hash);
  const kept = query.split("&").filter((pair) => {
    if (!pair) {
      return false;
    }
    const equals = pair.indexOf("=");
    const name = (equals < 0 ? pair : pair.slice(0, equals)).toLowerCase();
    return !SLICE_PARAMETERS.has(name);
  });
  return head + (kept.length > 0 ? "?" + kept.join("&") : "") + tail;
}

/**
 * True when the address names one piece of a live stream.
 *
 * A sequence number has no whole file behind it to ask for instead, so there
 * is nothing to trim and nothing to offer. Digits only: `sq` is an ordinary
 * word, and plenty of sites use it for a search query.
 */
function isSequenced(url) {
  const mark = url.indexOf("?");
  if (mark < 0) {
    return false;
  }
  const hash = url.indexOf("#", mark);
  return url
    .slice(mark + 1, hash < 0 ? undefined : hash)
    .split("&")
    .some((pair) => /^sq=\d+$/i.test(pair));
}

function rememberFile(tabId, url, knownWhole) {
  if ((streams.get(tabId) ?? []).length > 0) {
    return;
  }
  const extension = extensionOf(url);
  if (!MEDIA_EXTENSIONS.has(extension) || FRAGMENT_EXTENSIONS.has(extension)) {
    return;
  }
  if (SEGMENT_OR_WHOLE.has(extension) && !knownWhole) {
    return;
  }
  if (isSequenced(url)) {
    return;
  }
  remember(files, tabId, wholeFile(url), FILE_LIMIT);
}

/**
 * The manifests worth offering for this tab, freshest first.
 *
 * Freshest first because the last thing a player asked for is the thing it is
 * playing now. Only a handful are ever inspected on the Snatch side.
 */
/**
 * @param since When the video being asked about started loading. Everything
 *   older belonged to a different one.
 */
function recent(store, tabId, since) {
  if (typeof tabId !== "number") {
    return [];
  }
  const now = Date.now();
  const kept = (store.get(tabId) ?? []).filter((entry) => now - entry.at < STREAM_TTL_MS);
  store.set(tabId, kept);

  // A feed is one tab with fifty videos in it, and this map holds all of
  // them. Offering the whole tab's worth is how the panel ends up listing
  // qualities that belong to a clip three posts further down -- so it is
  // narrowed to what was fetched once the video under the pointer started
  // loading, which is when its own playlist was asked for.
  //
  // Never narrowed to nothing, though. A video the browser served from cache
  // made no request to see, and a guess from the same tab beats no answer.
  let scoped = kept;
  if (Number.isFinite(since) && since > 0) {
    const narrowed = kept.filter((entry) => entry.at >= since - PLAYBACK_GRACE_MS);
    if (narrowed.length > 0) {
      scoped = narrowed;
    }
  }
  // Fewest fetches first, then newest.
  //
  // A master playlist is fetched once and lists every quality. The media
  // playlist inside it is polled every few seconds and holds exactly one. So
  // newest-first put the polled ones at the top and, on a player that
  // switches bitrate, crowded the master out of the handful that get looked
  // at -- costing the quality list on precisely the streams that have one.
  return scoped
    .slice()
    .sort((a, b) => a.hits - b.hits || b.at - a.at)
    .map((entry) => entry.url);
}

function recentStreams(tabId, since) {
  return recent(streams, tabId, since);
}

function recentFiles(tabId, since) {
  return recent(files, tabId, since);
}

// ---------------------------------------------------------------------------
// Hint cache
// ---------------------------------------------------------------------------

function rememberHint(url, patch) {
  if (!url) {
    return;
  }
  const entry = hints.get(url) ?? {};
  for (const [key, value] of Object.entries(patch)) {
    if (value !== undefined && value !== null && value !== "") {
      entry[key] = value;
    }
  }
  entry.at = Date.now();
  hints.set(url, entry);

  if (hints.size > HINT_LIMIT) {
    pruneHints();
  }
}

function takeHint(url) {
  if (!url) {
    return null;
  }
  const hint = hints.get(url);
  if (!hint) {
    return null;
  }
  hints.delete(url);
  return Date.now() - hint.at > HINT_TTL_MS ? null : hint;
}

/** Drop expired entries, then oldest-first until the map is back to half size. */
function pruneHints() {
  const now = Date.now();
  for (const [url, hint] of hints) {
    if (now - hint.at > HINT_TTL_MS || hints.size > HINT_LIMIT / 2) {
      hints.delete(url);
    }
    if (hints.size <= HINT_LIMIT / 2) {
      break;
    }
  }
}

// ---------------------------------------------------------------------------
// URL and header helpers
// ---------------------------------------------------------------------------

function headerValue(headers, name) {
  const wanted = name.toLowerCase();
  for (const header of headers ?? []) {
    if (header.name.toLowerCase() === wanted) {
      return header.value;
    }
  }
  return undefined;
}

function looksLikeDownload(url) {
  try {
    const path = new URL(url).pathname;
    const dot = path.lastIndexOf(".");
    if (dot < 0 || dot === path.length - 1) {
      return false;
    }
    return DOWNLOAD_EXTENSIONS.has(path.slice(dot + 1).toLowerCase());
  } catch (error) {
    return false;
  }
}

function isMagnet(url) {
  return typeof url === "string" && url.trim().toLowerCase().startsWith("magnet:");
}

/** Only schemes aria2 can actually fetch; never blob:, data: or filesystem:. */
function isHijackable(url) {
  if (typeof url !== "string") {
    return false;
  }
  const colon = url.indexOf(":");
  if (colon < 1) {
    return false;
  }
  const scheme = url.slice(0, colon).toLowerCase();
  return scheme === "http" || scheme === "https" || scheme === "ftp" || scheme === "ftps";
}

function sanitiseName(name) {
  if (!name) {
    return undefined;
  }
  const cleaned = String(name)
    .trim()
    .replace(/[\\/]+/g, "_")
    .replace(CONTROL_CHARACTERS, "")
    .trim();
  return cleaned.length > 0 ? cleaned.slice(0, 200) : undefined;
}

function nameFromUrl(url) {
  try {
    const last = new URL(url).pathname.split("/").filter(Boolean).pop();
    return last ? sanitiseName(decodeURIComponent(last)) : undefined;
  } catch (error) {
    return undefined;
  }
}

/** RFC 6266: prefer `filename*=UTF-8''...`, fall back to plain `filename=`. */
function filenameFromDisposition(disposition) {
  if (!disposition) {
    return undefined;
  }

  const extended = /filename\*\s*=\s*([^;]+)/i.exec(disposition);
  if (extended) {
    const raw = extended[1].trim();
    const parts = raw.split("'");
    const encoded = parts.length >= 3 ? parts.slice(2).join("'") : raw;
    try {
      return sanitiseName(decodeURIComponent(encoded));
    } catch (error) {
      // Malformed percent-encoding; fall through to the plain form below.
    }
  }

  const quoted = /filename\s*=\s*"([^"]*)"/i.exec(disposition);
  if (quoted) {
    return sanitiseName(quoted[1]);
  }

  const bare = /filename\s*=\s*([^;]+)/i.exec(disposition);
  return bare ? sanitiseName(bare[1]) : undefined;
}

function pickFilename(item, hint, url) {
  if (item && item.filename) {
    const name = sanitiseName(item.filename.split(/[\\/]/).pop());
    if (name) {
      return name;
    }
  }
  return hint.filename ?? nameFromUrl(url);
}

// ---------------------------------------------------------------------------
// Native messaging
// ---------------------------------------------------------------------------

function checkReply(reply) {
  if (!reply || reply.ok !== true) {
    throw new Error((reply && reply.error) || "Snatch rejected the download");
  }
  return reply;
}

function sendNative(message) {
  // Firefox returns a promise and rejects a third callback argument.
  if (IS_FIREFOX) {
    return globalThis.browser.runtime
      .sendNativeMessage(HOST_NAME, message)
      .then(checkReply);
  }

  return new Promise((resolve, reject) => {
    try {
      api.runtime.sendNativeMessage(HOST_NAME, message, (reply) => {
        const failure = api.runtime.lastError;
        if (failure) {
          reject(new Error(failure.message || "the native host is unreachable"));
          return;
        }
        try {
          resolve(checkReply(reply));
        } catch (error) {
          reject(error);
        }
      });
    } catch (error) {
      reject(error);
    }
  });
}

async function cookieHeader(url) {
  try {
    const query = { url: url };
    if (IS_FIREFOX) {
      // Return cookies regardless of first-party isolation.
      query.firstPartyDomain = null;
    }
    const cookies = await api.cookies.getAll(query);
    return cookies
      .filter((cookie) => cookie.name)
      .map((cookie) => cookie.name + "=" + cookie.value)
      .join("; ");
  } catch (error) {
    console.warn("Snatch: could not read cookies for", url, error);
    return "";
  }
}

/** Build the wire payload and push it to the native host. */
async function handOff(details) {
  const message = { url: details.url };
  if (details.kind) {
    message.kind = details.kind;
  }

  // A magnet has no origin to read cookies for, and no headers to forward.
  if (details.kind === "magnet") {
    return sendNative(message);
  }

  const cookies = await cookieHeader(details.url);
  if (cookies) {
    message.cookies = cookies;
  }
  if (details.filename) {
    message.filename = details.filename;
  }
  if (details.referer) {
    message.referer = details.referer;
  }

  const agent = details.userAgent || (globalThis.navigator && globalThis.navigator.userAgent);
  if (agent) {
    message.user_agent = agent;
  }
  if (details.mime) {
    message.mime = details.mime;
  }
  if (Number.isFinite(details.size) && details.size > 0) {
    message.size = details.size;
  }
  // Recording options. Each is checked here rather than trusted: they come
  // from fields in a page, and reach a subprocess on the other side.
  if (Number.isFinite(details.height) && details.height > 0) {
    message.height = Math.round(details.height);
  }
  if (Number.isFinite(details.recordSeconds) && details.recordSeconds > 0) {
    message.record_seconds = Math.round(details.recordSeconds);
  }
  if (Number.isFinite(details.startAt) && details.startAt > 0) {
    message.start_at = Math.round(details.startAt);
  }
  if (Number.isFinite(details.skipSeconds) && details.skipSeconds > 0) {
    message.skip_seconds = Math.round(details.skipSeconds);
  }
  // What the page's player sent, minus the three that have fields above.
  if (details.headers && typeof details.headers === "object") {
    const extra = { ...details.headers };
    delete extra.referer;
    delete extra["user-agent"];
    if (!message.cookies && extra.cookie) {
      message.cookies = extra.cookie;
    }
    delete extra.cookie;
    if (Object.keys(extra).length > 0) {
      message.headers = extra;
    }
  }

  return sendNative(message);
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/** Take over a download the browser has just started. */
async function hijack(item) {
  if (!(await isEnabled())) {
    return;
  }

  const url = item.finalUrl || item.url || "";
  if (!isHijackable(url) || item.state !== "in_progress") {
    return;
  }
  if (passThrough.delete(url)) {
    return; // we handed this one back to the browser on purpose
  }

  try {
    await api.downloads.cancel(item.id);
  } catch (error) {
    // The download finished or vanished before we got here. Leave it alone
    // rather than risk fetching the same file twice.
    console.warn("Snatch: could not cancel the browser download", error);
    return;
  }
  try {
    await api.downloads.erase({ id: item.id });
  } catch (error) {
    console.warn("Snatch: could not erase the cancelled download", error);
  }

  const hint = takeHint(url) || takeHint(item.url) || {};
  const filename = pickFilename(item, hint, url);

  try {
    await handOff({
      url: url,
      filename: filename,
      referer: item.referrer || hint.referer,
      userAgent: hint.userAgent,
      // The headers the browser's own request carried. A link behind a login
      // is refused without them, and that is exactly the download somebody
      // most wants a manager for.
      headers: headersFor([url, item.url]),
      mime: item.mime || hint.mime,
      size: item.totalBytes > 0 ? item.totalBytes : hint.size
    });
    console.info("Snatch: handed off", filename || url);
    flashBadge("ok", "#3584e4");
  } catch (error) {
    console.error("Snatch: hand-off failed", error);
    flashBadge("!", "#e01b24");
    // Never silently lose a download: give it back to the browser.
    passThrough.add(url);
    try {
      await api.downloads.download({ url: url });
    } catch (fallbackError) {
      passThrough.delete(url);
      console.error("Snatch: the browser fallback also failed", fallbackError);
    }
  }
}

/** Hand off a URL the user picked from the context menu. */
async function hijackDirect(url, referer) {
  if (isMagnet(url)) {
    return sendToSnatch({ url: url.trim(), kind: "magnet" }, "torrent");
  }
  if (!isHijackable(url)) {
    console.warn("Snatch: cannot download", url);
    flashBadge("!", "#e01b24");
    return;
  }
  return sendToSnatch(
    {
      url: url,
      filename: nameFromUrl(url),
      referer: referer,
      // A "download this link" on a page behind a login is refused without
      // the headers the page's own requests carry.
      headers: headersFor([url])
    },
    "download"
  );
}

/** Send one job and report the outcome on the toolbar button. */
async function sendToSnatch(details, what) {
  try {
    await handOff(details);
    console.info("Snatch: sent", what, details.url);
    flashBadge("ok", "#3584e4");
    return true;
  } catch (error) {
    console.error("Snatch: could not send the " + what, error);
    flashBadge("!", "#e01b24");
    return false;
  }
}

/**
 * Collect every media link on the page and hand the page itself to gallery-dl.
 *
 * The count is only used to tell the user what was found: gallery-dl re-walks
 * the page with its own site-specific extractor, which finds far more than a
 * DOM scrape can (paginated galleries, API-backed feeds, originals behind
 * thumbnails). Sending the URL rather than the scraped list is what makes the
 * per-site organisation work.
 */
async function scrapePage(tab) {
  if (!tab || !isHijackable(tab.url)) {
    flashBadge("!", "#e01b24");
    console.warn("Snatch: cannot scrape", tab && tab.url);
    return;
  }

  let found = 0;
  try {
    const results = await api.scripting.executeScript({
      target: { tabId: tab.id },
      func: countMediaLinks
    });
    if (Array.isArray(results) && results.length > 0 && results[0]) {
      found = results[0].result || 0;
    }
  } catch (error) {
    // Scripting can be blocked on privileged pages; the scrape still works.
    console.warn("Snatch: could not count media on the page", error);
  }

  const sent = await sendToSnatch({ url: tab.url, kind: "scrape" }, "scrape");
  if (sent && found > 0) {
    console.info("Snatch: page advertises " + found + " media links");
  }
}

/**
 * Runs in the page. Counts distinct image/video/audio sources plus links that
 * point at a media file. Must be self-contained: it is injected, not imported.
 */
function countMediaLinks() {
  const seen = new Set();
  const pattern =
    /\.(jpg|jpeg|png|gif|webp|bmp|avif|jxl|mp4|webm|mkv|mov|m4v|mp3|m4a|flac|wav|ogg|opus)(\?|#|$)/i;

  for (const element of document.querySelectorAll("img[src], source[src], video[src], audio[src]")) {
    const value = element.currentSrc || element.src;
    if (value && !value.startsWith("data:") && !value.startsWith("blob:")) {
      seen.add(value);
    }
  }
  for (const anchor of document.querySelectorAll("a[href]")) {
    if (pattern.test(anchor.href)) {
      seen.add(anchor.href);
    }
  }
  return seen.size;
}

// ---------------------------------------------------------------------------
// Requests from the button on a video
// ---------------------------------------------------------------------------

/**
 * Answer one request from `content.js`.
 *
 * A content script runs in a web page, so nothing it sends is trusted. Every
 * URL is checked here against the same rule the context menu uses before it
 * can reach the native host.
 */
async function handleContentMessage(message, sender) {
  if (!message || typeof message !== "object") {
    throw new Error("empty request");
  }
  const tabId = sender && sender.tab ? sender.tab.id : undefined;
  // The frame's own address, which for an embedded player is the embed rather
  // than the page around it. That is the one whose player made the requests.
  const page = (sender && sender.url) || (sender && sender.tab && sender.tab.url);

  switch (message.type) {
    case "state":
      return { ok: true, enabled: await isOverlayWanted(page) };

    case "hide-site":
      // The × on the pill. Hiding the whole site rather than this one page is
      // what people mean by it, and it is undone from the toolbar menu.
      if (!(await hideSite(page))) {
        return { ok: false, error: "that page has no site to hide" };
      }
      await broadcastOverlay();
      return { ok: true };

    case "formats": {
      // The listing is a question. It queues nothing, so it is safe to ask
      // whenever the user opens the panel.
      const url = requireUrl(message.url);
      const payload = { url: url, kind: "formats" };
      // What the page's player fetched, for the sites yt-dlp cannot read.
      // Snatch only looks at these if yt-dlp comes back with nothing.
      const observed = recentStreams(tabId, message.since);
      // What the page is playing now comes first: it is the one the user is
      // looking at, and it is the only one still available once the browser
      // has the file cached.
      const loaded = recentFiles(tabId, message.since);
      if (isHijackable(message.source) && !loaded.includes(message.source)) {
        loaded.unshift(message.source);
      }
      if (observed.length > 0) {
        payload.streams = observed;
      }
      if (loaded.length > 0) {
        payload.files = loaded;
      }
      // Attached every time, not only when something was observed. yt-dlp
      // needs them just as much: a members-only video, a signed page or a
      // site that checks the referer refuses the probe without them, and
      // that refusal is what the reader sees as "no qualities found".
      await attachAccess(payload, url, observed.concat(loaded));
      const reply = await sendNative(payload);
      return {
        ok: true,
        title: reply.title,
        duration: reply.duration,
        live: reply.live === true,
        formats: Array.isArray(reply.formats) ? reply.formats : []
      };
    }

    case "stream": {
      // A manifest ffmpeg records. The cookies are read for the manifest's own
      // address, not the page's, because that is the request ffmpeg makes.
      const target = requireUrl(message.url);
      await handOff({
        url: target,
        kind: "stream",
        filename: sanitiseName(message.title),
        referer: page,
        // What the player sent for this exact manifest. ffmpeg has to make
        // the same request or the CDN refuses it, and refuses every segment
        // behind it too.
        headers: headersFor([target]),
        height: message.height,
        recordSeconds: message.record_seconds,
        skipSeconds: message.skip_seconds,
        startAt: message.start_at
      });
      flashBadge("ok", "#3584e4");
      return { ok: true };
    }

    case "video": {
      const target = requireUrl(message.url);
      const payload = { url: target, kind: "video" };
      if (typeof message.format_id === "string" && message.format_id) {
        payload.format_id = message.format_id;
      }
      // The same access the listing was made with. Without it yt-dlp probes
      // the page as a stranger, finds the formats, and is then refused when
      // it goes to fetch one -- which reads as a download that failed for no
      // reason.
      const observed = recentStreams(tabId, message.since);
      const loaded = recentFiles(tabId, message.since);
      if (observed.length > 0) {
        payload.streams = observed;
      }
      if (loaded.length > 0) {
        payload.files = loaded;
      }
      await attachAccess(payload, target, observed.concat(loaded));
      await sendNative(payload);
      flashBadge("ok", "#3584e4");
      return { ok: true };
    }

    case "direct": {
      // A plain file: the ordinary hand-off, with cookies and referer, so a
      // link that only works while signed in still works.
      const target = requireUrl(message.url);
      await handOff({
        url: target,
        filename: nameFromUrl(target),
        referer: typeof message.referer === "string" ? message.referer : undefined,
        headers: headersFor([target])
      });
      flashBadge("ok", "#3584e4");
      return { ok: true };
    }

    default:
      throw new Error("unknown request");
  }
}

/**
 * Put everything that gets a request accepted onto a payload.
 *
 * Three of these have fields of their own because every engine takes them as
 * a named option; the rest ride along in `headers`, which is where a site's
 * own access token ends up. The page's address is the referer rather than the
 * media's: that is the request a player makes, and a CDN that checks looks
 * for the site it is embedded on.
 */
async function attachAccess(payload, pageUrl, mediaUrls) {
  payload.referer = pageUrl;
  const agent = globalThis.navigator && globalThis.navigator.userAgent;
  if (agent) {
    payload.user_agent = agent;
  }
  // Read through the cookie store rather than off the wire: that reaches the
  // httpOnly session cookies a request listener is never shown, and those are
  // the ones a signed-in page depends on.
  const cookies = await cookieHeader(pageUrl);
  if (cookies) {
    payload.cookies = cookies;
  }
  const observed = headersFor(mediaUrls || []);
  // Already sent above, and a duplicate is how a request ends up with two
  // Referers and is refused by a server that reads the second one.
  delete observed.referer;
  delete observed["user-agent"];
  if (!cookies && observed.cookie) {
    payload.cookies = observed.cookie;
  }
  delete observed.cookie;
  if (Object.keys(observed).length > 0) {
    payload.headers = observed;
  }
}

function requireUrl(url) {
  if (!isHijackable(url)) {
    throw new Error("Snatch cannot download that address");
  }
  return url;
}

// ---------------------------------------------------------------------------
// Listeners
// ---------------------------------------------------------------------------

api.runtime.onMessage.addListener((message, sender, sendResponse) => {
  handleContentMessage(message, sender)
    .then(sendResponse)
    .catch((error) => {
      console.warn("Snatch: could not answer the page", error);
      sendResponse({ ok: false, error: String((error && error.message) || error) });
    });
  // Keeps the channel open for the async answer above. Without it the page
  // sees the port close before the native host has replied.
  return true;
});

api.webRequest.onBeforeRequest.addListener(
  (details) => {
    // A new page in this tab: the old page's manifests belong to it, not this
    // one, and offering them here would record the wrong programme.
    if (details.type === "main_frame") {
      streams.delete(details.tabId);
      files.delete(details.tabId);
    }
    if (isManifestUrl(details.url)) {
      rememberStream(details.tabId, details.url);
    } else if (details.type === "media") {
      rememberFile(details.tabId, details.url);
    }
    if (looksLikeDownload(details.url)) {
      rememberHint(details.url, {
        referer: details.originUrl || details.initiator,
        tabId: details.tabId
      });
    }
  },
  { urls: WEB_REQUEST_FILTER.urls, types: WEB_REQUEST_TYPES }
);

/**
 * Watch the headers going out.
 *
 * `extraHeaders` matters more than it looks. Chromium hides Cookie, Referer
 * and Accept-Language from a plain `requestHeaders` listener, so without it
 * the two headers that decide whether a CDN accepts the request are exactly
 * the two that never arrive -- and the download fails on a site that plays
 * fine in the tab. Firefox has no such rule and accepts the flag anyway; the
 * fallback is there for anything that does not.
 */
function watchOutgoingHeaders() {
  const listener = (details) => {
    // Anything the page fetched to play, plus anything that looks like a
    // file. A page's XHR is included because that is how a player asks for a
    // manifest, and a signed manifest URL is not recognisable as one.
    const watched =
      details.type === "media" ||
      details.type === "xmlhttprequest" ||
      details.type === "object" ||
      isManifestUrl(details.url) ||
      looksLikeDownload(details.url);
    if (!watched) {
      return;
    }
    rememberHeaders(details.url, details.requestHeaders);
    if (looksLikeDownload(details.url)) {
      rememberHint(details.url, {
        referer: headerValue(details.requestHeaders, "referer"),
        userAgent: headerValue(details.requestHeaders, "user-agent")
      });
    }
  };

  try {
    api.webRequest.onSendHeaders.addListener(listener, WEB_REQUEST_FILTER, [
      "requestHeaders",
      "extraHeaders"
    ]);
  } catch (error) {
    console.warn("Snatch: extraHeaders is unavailable here", error);
    api.webRequest.onSendHeaders.addListener(listener, WEB_REQUEST_FILTER, ["requestHeaders"]);
  }
}

watchOutgoingHeaders();

api.webRequest.onHeadersReceived.addListener(
  (details) => {
    // Plenty of manifests have no extension to recognise -- a signed CDN URL
    // ending in a token, say -- so the server's own answer is checked too.
    const declared = headerValue(details.responseHeaders, "content-type");
    if (declared && MANIFEST_TYPES.has(declared.split(";")[0].trim().toLowerCase())) {
      rememberStream(details.tabId, details.url);
    }

    // A whole broadcast served as one MPEG-TS file looks exactly like a piece
    // of an HLS stream until the server says how big it is.
    const declaredLength = Number.parseInt(
      headerValue(details.responseHeaders, "content-length") ?? "",
      10
    );
    if (Number.isFinite(declaredLength) && declaredLength >= WHOLE_FILE_BYTES) {
      rememberFile(details.tabId, details.url, true);
    }

    const disposition = headerValue(details.responseHeaders, "content-disposition");
    const isAttachment = typeof disposition === "string" && /^\s*attachment/i.test(disposition);
    if (!isAttachment && !looksLikeDownload(details.url)) {
      return;
    }
    const contentType = headerValue(details.responseHeaders, "content-type");
    const contentLength = headerValue(details.responseHeaders, "content-length");
    rememberHint(details.url, {
      filename: filenameFromDisposition(disposition),
      mime: contentType ? contentType.split(";")[0].trim() : undefined,
      size: contentLength ? Number.parseInt(contentLength, 10) : undefined
    });
  },
  WEB_REQUEST_FILTER,
  ["responseHeaders"]
);

// There is deliberately no webRequest listener for magnet links.
//
// An earlier version filtered on `magnet:*`, which browsers reject: a match
// pattern must be <scheme>://<host>/<path>, and `magnet:` has no host. Chrome
// reported "'magnet:*' is not a valid URL pattern." on every load.
//
// Widening the filter would not have helped either. webRequest only ever sees
// http, https, ws, wss, ftp and file. A magnet link is handed to an external
// protocol handler and never becomes a web request at all, so no filter can
// catch one. Magnets are sent to Snatch from the context menu instead.

api.tabs.onRemoved.addListener((tabId) => {
  streams.delete(tabId);
  files.delete(tabId);
});

api.downloads.onCreated.addListener((item) => {
  hijack(item).catch((error) => console.error("Snatch: capture failed", error));
});

api.action.onClicked.addListener(() => {
  isEnabled()
    .then((enabled) => setEnabled(!enabled))
    .catch((error) => console.error("Snatch: could not toggle capture", error));
});

api.contextMenus.onClicked.addListener((info, tab) => {
  switch (info.menuItemId) {
    case MENU_OVERLAY:
      setOverlayEnabled(info.checked === true).catch((error) =>
        console.error("Snatch: could not change the video button setting", error)
      );
      return;
    case MENU_SCRAPE:
      scrapePage(tab).catch((error) =>
        console.error("Snatch: page scrape failed", error)
      );
      return;
    case MENU_SNIFF: {
      const target = (tab && tab.url) || info.pageUrl;
      if (!isHijackable(target)) {
        flashBadge("!", "#e01b24");
        return;
      }
      // The GUI does the sniffing and shows the picker; the extension only
      // hands over the address, so cookies and login state stay in Snatch's
      // own request rather than being copied around.
      sendToSnatch({ url: target, kind: "sniff" }, "sniff").catch((error) =>
        console.error("Snatch: sniff hand-off failed", error)
      );
      return;
    }
    case MENU_VIDEO: {
      const target = info.linkUrl || (tab && tab.url);
      if (!isHijackable(target)) {
        flashBadge("!", "#e01b24");
        return;
      }
      sendToSnatch({ url: target, kind: "video" }, "video").catch((error) =>
        console.error("Snatch: video hand-off failed", error)
      );
      return;
    }
    case MENU_DOWNLOAD: {
      const url = info.linkUrl || info.srcUrl || (info.selectionText || "").trim();
      hijackDirect(url, info.pageUrl).catch((error) =>
        console.error("Snatch: context-menu capture failed", error)
      );
      return;
    }
    default:
  }
});

api.storage.onChanged.addListener((changes, area) => {
  if (area !== "local") {
    return;
  }
  if ("enabled" in changes) {
    enabledCache = changes.enabled.newValue !== false;
    void refreshAction();
    // Pausing Snatch takes the button off every page too.
    void broadcastOverlay();
  }
  if ("overlay" in changes) {
    overlayCache = changes.overlay.newValue !== false;
    void broadcastOverlay();
  }
  if ("hiddenSites" in changes) {
    hiddenSitesCache = Array.isArray(changes.hiddenSites.newValue)
      ? changes.hiddenSites.newValue
      : [];
  }
});

async function installContextMenu() {
  try {
    await api.contextMenus.removeAll();
  } catch (error) {
    console.warn("Snatch: could not clear old context menu entries", error);
  }
  try {
    api.contextMenus.create({
      id: MENU_DOWNLOAD,
      title: "Download with Snatch",
      contexts: ["link", "image", "video", "audio", "selection"]
    });
    // There is no separate magnet entry. It would need
    // `targetUrlPatterns: ["magnet:*"]` to appear only on magnet links, and a
    // match pattern must be <scheme>://<host>/<path>, so that one can never
    // match anything -- the entry was registered but never shown. Without the
    // pattern it would appear on every link instead, which is worse.
    //
    // "Download with Snatch" already covers it: it has no pattern, so it does
    // appear on a magnet link, and hijackDirect sends anything starting with
    // `magnet:` to the torrent engine rather than the downloader.
    api.contextMenus.create({
      id: MENU_VIDEO,
      title: "Extract Video with Snatch",
      contexts: ["page", "link", "video"]
    });
    api.contextMenus.create({
      id: MENU_SNIFF,
      title: "Find All Media on This Page",
      contexts: ["page", "frame", "image", "video", "audio"]
    });
    api.contextMenus.create({
      id: MENU_SCRAPE,
      title: "Scrape This Page with Snatch",
      contexts: ["page", "frame", "image"]
    });
  } catch (error) {
    console.error("Snatch: could not create the context menu entries", error);
  }

  // Kept in its own try: the "action" context is newer than the rest, and a
  // browser that rejects it must not take the download entries down with it.
  try {
    api.contextMenus.create({
      id: MENU_OVERLAY,
      title: "Show the button on videos",
      type: "checkbox",
      checked: await isOverlayEnabled(),
      contexts: ["action"]
    });
  } catch (error) {
    console.warn("Snatch: could not add the video button toggle", error);
  }
}

api.runtime.onInstalled.addListener(() => {
  void installContextMenu();
  void refreshAction();
  void armOpenTabs();
});

/**
 * Put a working content script back into tabs that were already open.
 *
 * Installing or updating an extension does not touch the content scripts
 * already running: they keep their listeners and their drawn UI, and every
 * call they make into the extension fails from then on. Every tab open at the
 * moment of an update is left with a button that cannot do anything, which
 * looks like the extension is broken everywhere at once.
 *
 * Injecting again gives each of those tabs a live script without the reader
 * having to reload anything. The stale one notices it has been replaced and
 * removes its own UI.
 */
async function armOpenTabs() {
  let tabs = [];
  try {
    // Deliberately unfiltered: matching on `url` needs the "tabs" permission
    // in Firefox. Injecting into a tab that will not have it simply fails,
    // which is caught below and is the normal case anyway.
    tabs = await api.tabs.query({});
  } catch (error) {
    console.warn("Snatch: could not list the open tabs", error);
    return;
  }
  for (const tab of tabs) {
    if (typeof tab.id !== "number") {
      continue;
    }
    try {
      await api.scripting.executeScript({
        target: { tabId: tab.id, allFrames: true },
        files: ["content.js"]
      });
    } catch (error) {
      // Perfectly normal: the Web Store, a PDF, a page that was closing, or
      // any of the pages an extension is never allowed to touch.
    }
  }
}

api.runtime.onStartup.addListener(() => {
  void installContextMenu();
  void refreshAction();
});

// A service worker is often revived by an event rather than by startup.
void refreshAction();
