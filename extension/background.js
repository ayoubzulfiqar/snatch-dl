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

/** Playlist and manifest types, by file extension. */
const MANIFEST_EXTENSIONS = new Set(["m3u8", "m3u", "mpd", "ism", "f4m"]);

/** ...and by what the server says they are, for the ones with no extension. */
const MANIFEST_TYPES = new Set([
  "application/vnd.apple.mpegurl",
  "application/x-mpegurl",
  "audio/mpegurl",
  "audio/x-mpegurl",
  "application/dash+xml",
  "video/vnd.mpeg.dash.mpd"
]);

/** Whole media files, for the sites that just serve one. */
const MEDIA_EXTENSIONS = new Set([
  "mp4", "webm", "m4v", "mov", "mkv", "avi", "flv", "3gp", "ogv",
  "m4a", "mp3", "aac", "flac", "wav", "ogg", "opus", "m4b"
]);

/**
 * Pieces of a stream, never a file to offer on their own.
 *
 * A four-second fragment is not a download, and a page playing one produces
 * hundreds of them.
 */
const FRAGMENT_EXTENSIONS = new Set(["m4s", "cmfv", "cmfa", "fmp4", "ts"]);

/** Most manifests worth remembering for one tab. */
const STREAM_LIMIT = 24;
const FILE_LIMIT = 8;
const STREAM_TTL_MS = 15 * 60 * 1000;

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
    const wanted = globallyOn && !hidden.includes(hostOf(tab.url));
    try {
      const sent = api.tabs.sendMessage(tab.id, {
        type: "snatch-overlay",
        enabled: wanted
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

function remember(store, tabId, url, limit) {
  if (typeof tabId !== "number" || tabId < 0 || !isHijackable(url)) {
    return;
  }
  const now = Date.now();
  const kept = (store.get(tabId) ?? []).filter(
    (entry) => entry.url !== url && now - entry.at < STREAM_TTL_MS
  );
  kept.push({ url: url, at: now });
  // Newest last, oldest dropped: a page that re-fetches one playlist forever
  // must not push the others out.
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
function rememberFile(tabId, url) {
  if ((streams.get(tabId) ?? []).length > 0) {
    return;
  }
  const extension = extensionOf(url);
  if (FRAGMENT_EXTENSIONS.has(extension) || !MEDIA_EXTENSIONS.has(extension)) {
    return;
  }
  remember(files, tabId, url, FILE_LIMIT);
}

/**
 * The manifests worth offering for this tab, freshest first.
 *
 * Freshest first because the last thing a player asked for is the thing it is
 * playing now. Only a handful are ever inspected on the Snatch side.
 */
function recent(store, tabId) {
  if (typeof tabId !== "number") {
    return [];
  }
  const now = Date.now();
  const kept = (store.get(tabId) ?? []).filter((entry) => now - entry.at < STREAM_TTL_MS);
  store.set(tabId, kept);
  return kept.map((entry) => entry.url).reverse();
}

function recentStreams(tabId) {
  return recent(streams, tabId);
}

function recentFiles(tabId) {
  return recent(files, tabId);
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
    { url: url, filename: nameFromUrl(url), referer: referer },
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
      const observed = recentStreams(tabId);
      // What the page is playing now comes first: it is the one the user is
      // looking at, and it is the only one still available once the browser
      // has the file cached.
      const loaded = recentFiles(tabId);
      if (isHijackable(message.source) && !loaded.includes(message.source)) {
        loaded.unshift(message.source);
      }
      if (observed.length > 0) {
        payload.streams = observed;
      }
      if (loaded.length > 0) {
        payload.files = loaded;
      }
      if (observed.length > 0 || loaded.length > 0) {
        // A manifest is very often refused without the request looking like
        // the player's, so the same three go with it.
        payload.referer = url;
        const agent = globalThis.navigator && globalThis.navigator.userAgent;
        if (agent) {
          payload.user_agent = agent;
        }
        const cookies = await cookieHeader(url);
        if (cookies) {
          payload.cookies = cookies;
        }
      }
      const reply = await sendNative(payload);
      return {
        ok: true,
        title: reply.title,
        duration: reply.duration,
        formats: Array.isArray(reply.formats) ? reply.formats : []
      };
    }

    case "stream":
      // A manifest ffmpeg records. The cookies are read for the manifest's own
      // address, not the page's, because that is the request ffmpeg makes.
      await handOff({
        url: requireUrl(message.url),
        kind: "stream",
        filename: sanitiseName(message.title),
        referer: page,
        height: message.height,
        recordSeconds: message.record_seconds,
        startAt: message.start_at
      });
      flashBadge("ok", "#3584e4");
      return { ok: true };

    case "video": {
      const payload = { url: requireUrl(message.url), kind: "video" };
      if (typeof message.format_id === "string" && message.format_id) {
        payload.format_id = message.format_id;
      }
      await sendNative(payload);
      flashBadge("ok", "#3584e4");
      return { ok: true };
    }

    case "direct":
      // A plain file: the ordinary hand-off, with cookies and referer, so a
      // link that only works while signed in still works.
      await handOff({
        url: requireUrl(message.url),
        filename: nameFromUrl(message.url),
        referer: typeof message.referer === "string" ? message.referer : undefined
      });
      flashBadge("ok", "#3584e4");
      return { ok: true };

    default:
      throw new Error("unknown request");
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

api.webRequest.onSendHeaders.addListener(
  (details) => {
    if (!looksLikeDownload(details.url)) {
      return;
    }
    rememberHint(details.url, {
      referer: headerValue(details.requestHeaders, "referer"),
      userAgent: headerValue(details.requestHeaders, "user-agent")
    });
  },
  WEB_REQUEST_FILTER,
  ["requestHeaders"]
);

api.webRequest.onHeadersReceived.addListener(
  (details) => {
    // Plenty of manifests have no extension to recognise -- a signed CDN URL
    // ending in a token, say -- so the server's own answer is checked too.
    const declared = headerValue(details.responseHeaders, "content-type");
    if (declared && MANIFEST_TYPES.has(declared.split(";")[0].trim().toLowerCase())) {
      rememberStream(details.tabId, details.url);
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
});

api.runtime.onStartup.addListener(() => {
  void installContextMenu();
  void refreshAction();
});

// A service worker is often revived by an event rather than by startup.
void refreshAction();
