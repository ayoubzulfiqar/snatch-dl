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
const MENU_MAGNET = "snatch-open-magnet";
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

/** Cached copy of the enabled flag; `null` until first read. */
let enabledCache = null;

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
// Listeners
// ---------------------------------------------------------------------------

api.webRequest.onBeforeRequest.addListener(
  (details) => {
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

// A magnet navigation never becomes a download item, so it has to be caught
// on the request itself. Chromium cannot cancel it in MV3, but Snatch still
// receives the link and the external-handler prompt can simply be dismissed.
api.webRequest.onBeforeRequest.addListener(
  (details) => {
    if (!isMagnet(details.url)) {
      return;
    }
    isEnabled()
      .then((enabled) => {
        if (enabled) {
          return hijackDirect(details.url, details.originUrl || details.initiator);
        }
      })
      .catch((error) => console.error("Snatch: magnet capture failed", error));
  },
  { urls: ["magnet:*"], types: ["main_frame", "sub_frame"] }
);

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
    case MENU_MAGNET:
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
  if (area === "local" && "enabled" in changes) {
    enabledCache = changes.enabled.newValue !== false;
    void refreshAction();
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
    api.contextMenus.create({
      id: MENU_MAGNET,
      title: "Send Magnet to Snatch",
      contexts: ["link"],
      targetUrlPatterns: ["magnet:*"]
    });
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
