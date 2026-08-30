"use strict";

/**
 * Snatch - the button that appears on a video.
 *
 * Hover any video on any page and a small "Download with Snatch" pill appears
 * in its corner. Click it and Snatch reports what resolutions the page offers;
 * pick one and it downloads.
 *
 * Nothing is asked of Snatch until the pill is clicked. Probing on sight would
 * launch the whole download manager for every page that happens to contain a
 * video, so the pill is drawn from what is already in the DOM and the question
 * is only asked once the user has shown interest.
 *
 * Three details decide whether this works at all:
 *
 *   1. Videos are found with `elementsFromPoint`, not with a selector or a
 *      MutationObserver. Every serious player buries its `<video>` under a
 *      stack of overlays, so `event.target` is never the video and
 *      `closest("video")` never matches. A hit test looks through the stack.
 *      It also costs nothing on pages without video and needs no rescanning
 *      when a single-page site swaps its player out.
 *   2. The listeners are on `document` in the capture phase. Players call
 *      `stopPropagation` freely, and a bubbling listener never runs.
 *   3. The UI lives in a shadow root. Page CSS is hostile - `* { position:
 *      static !important }` is a real thing people write - and a shadow root
 *      is the only way to be sure of what is drawn.
 */

(() => {
  const api = globalThis.browser ?? globalThis.chrome;
  if (!api || !api.runtime || !api.runtime.sendMessage) {
    return;
  }
  // Injecting twice would leave two pills fighting over the same corner.
  if (globalThis.__snatchOverlayLoaded) {
    return;
  }
  globalThis.__snatchOverlayLoaded = true;

  const IS_FIREFOX =
    typeof globalThis.browser !== "undefined" &&
    typeof globalThis.browser.runtime.getBrowserInfo === "function";

  /** Below this a "video" is an advert, an emoji or a tracking pixel. */
  const MIN_WIDTH = 240;
  const MIN_HEIGHT = 140;
  /** A hit test per pointer move would be wasteful; eight a second is plenty. */
  const POINTER_INTERVAL_MS = 120;
  /** Grace period so the pointer can travel from the video to the pill. */
  const HIDE_DELAY_MS = 400;
  /** How long "Sent to Snatch" stays up before the panel closes itself. */
  const DONE_MS = 1500;

  /** What to say when this script has outlived its extension. */
  const UPDATED = "Snatch was updated. Reload this page to use the button here.";

  /** The two ways Chromium words the same thing. */
  const DEAD_CONTEXT = /Extension context invalidated|Receiving end does not exist|message port closed/i;

  /** The browser found the add-on but not the program behind it. */
  const NO_HOST = /native messaging host|not registered|host is unreachable|could not execute/i;

  /**
   * The app is older than this add-on.
   *
   * They are installed separately, so updating one is not updating the other.
   * An older app fails inside its own parser and answers with something like
   * "unknown variant `formats`, expected one of `download`, `magnet` …", which
   * is true and tells the reader nothing. Newer apps say so themselves; this
   * catches the ones that cannot.
   */
  const OUTDATED = /unknown variant|unknown field|not a valid Snatch download request|older than the browser add-on/i;

  let enabled = true;
  /** Set once this script belongs to an extension that is no longer there. */
  let orphaned = false;
  let host = null;
  let root = null;
  let pill = null;
  let panel = null;
  let panelBody = null;
  let panelTitle = null;
  let panelStatus = null;

  let video = null;
  let optionsBox = null;
  let startField = null;
  let minutesField = null;
  /** Videos seen firing `encrypted`, which is EME starting up. */
  const encryptedVideos = new WeakSet();
  const watchedVideos = new WeakSet();
  let pillVisible = false;
  let panelOpen = false;
  let hideTimer = 0;
  let frame = 0;
  let lastHitTest = 0;

  // -------------------------------------------------------------------------
  // Talking to the background worker
  // -------------------------------------------------------------------------

  /**
   * Whether this script can still reach the extension it came from.
   *
   * Reloading or updating an extension does not touch the content scripts
   * already running in open tabs. They keep their DOM listeners and their
   * drawn UI, and every call they make into the extension fails from then on
   * with "Extension context invalidated" or "Receiving end does not exist" --
   * which is exactly what a page open across an update reports.
   *
   * `runtime.id` is the cheapest way to tell: it goes undefined the moment the
   * context dies, and reading it never throws.
   */
  function alive() {
    try {
      return Boolean(api.runtime && api.runtime.id);
    } catch (error) {
      return false;
    }
  }

  /**
   * Take this script's UI out of the page and stop doing anything.
   *
   * The replacement injected by the new extension draws its own, and two
   * pills in one corner -- one of them dead -- is worse than none.
   */
  function teardown() {
    orphaned = true;
    enabled = false;
    panelOpen = false;
    pillVisible = false;
    if (hideTimer) {
      clearTimeout(hideTimer);
      hideTimer = 0;
    }
    if (host && host.parentNode) {
      host.parentNode.removeChild(host);
    }
    host = null;
    root = null;
    pill = null;
    panel = null;
  }

  /**
   * Send one request and always resolve, never reject: every caller here is
   * rendering the answer into the panel, and an exception would leave the
   * panel stuck on "Asking Snatch".
   */
  function send(message) {
    if (!alive()) {
      return Promise.resolve({ ok: false, error: UPDATED, updated: true });
    }
    if (IS_FIREFOX) {
      // Firefox's runtime API is promise-only and rejects a callback argument.
      return globalThis.browser.runtime
        .sendMessage(message)
        .then((reply) => reply || { ok: false, error: "Snatch sent no reply" })
        .catch((error) => {
          const message = describe(error);
          return DEAD_CONTEXT.test(message)
            ? { ok: false, error: UPDATED, updated: true }
            : { ok: false, error: message };
        });
    }
    return new Promise((resolve) => {
      try {
        api.runtime.sendMessage(message, (reply) => {
          const failure = api.runtime.lastError;
          if (failure) {
            const message = failure.message || "Snatch is unreachable";
            resolve(
              DEAD_CONTEXT.test(message)
                ? { ok: false, error: UPDATED, updated: true }
                : { ok: false, error: message }
            );
            return;
          }
          resolve(reply || { ok: false, error: "Snatch sent no reply" });
        });
      } catch (error) {
        resolve({ ok: false, error: describe(error) });
      }
    });
  }

  function describe(error) {
    return String((error && error.message) || error || "something went wrong");
  }

  // -------------------------------------------------------------------------
  // Formatting
  // -------------------------------------------------------------------------

  /**
   * Binary units, matching the Snatch window exactly. A panel that said
   * "281 MB" for the file the app then lists as "268 MiB" reads as a bug.
   */
  function humanBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes <= 0) {
      return "";
    }
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let value = bytes;
    let unit = 0;
    while (value >= 1024 && unit + 1 < units.length) {
      value /= 1024;
      unit += 1;
    }
    if (unit === 0) {
      return `${value} B`;
    }
    return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
  }

  function humanDuration(seconds) {
    if (!Number.isFinite(seconds) || seconds <= 0) {
      return "";
    }
    const total = Math.round(seconds);
    const hours = Math.floor(total / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    const rest = total % 60;
    const pad = (value) => String(value).padStart(2, "0");
    return hours > 0
      ? `${hours}:${pad(minutes)}:${pad(rest)}`
      : `${minutes}:${pad(rest)}`;
  }

  /** The right-hand column of a row: "268 MiB · mp4". */
  function detailOf(format) {
    const parts = [];
    const size = humanBytes(format.size);
    if (size) {
      // The size was worked out from the bitrate, so it is not a promise.
      parts.push(format.estimated ? `~${size}` : size);
    }
    if (format.ext) {
      parts.push(format.ext);
    }
    return parts.join(" · ");
  }

  // -------------------------------------------------------------------------
  // The video under the pointer
  // -------------------------------------------------------------------------

  function isWorthwhile(candidate) {
    if (!candidate || !candidate.isConnected) {
      return false;
    }
    const rect = candidate.getBoundingClientRect();
    return rect.width >= MIN_WIDTH && rect.height >= MIN_HEIGHT;
  }

  /**
   * Look through everything painted at this point for a video.
   *
   * `elementsFromPoint` returns the whole hit-test stack rather than just the
   * topmost element, which is what makes this work on a real player: the video
   * sits under the controls, the gradient, the click-catcher and the captions.
   */
  function videoAt(x, y) {
    const stack = document.elementsFromPoint(x, y);
    // The pointer is on our own pill or panel: keep whatever it belongs to.
    if (host && stack.includes(host)) {
      return video;
    }
    for (const element of stack) {
      if (element instanceof HTMLVideoElement) {
        return element;
      }
    }
    return null;
  }

  /**
   * Whether this video is playing DRM-protected content.
   *
   * Two signals, because either alone can be missed. `mediaKeys` is set once
   * by the player and stays set, so it survives arriving late; the `encrypted`
   * event fires when protected content starts and catches players that set
   * their keys after we looked.
   *
   * This is only used to explain, never to work around. What travels over the
   * wire for DRM is the encrypted bytes; the key goes to a module inside the
   * browser that never hands it back, so there is nothing to fetch. Saying so
   * plainly beats a download that fails for reasons nobody can see.
   */
  function isProtected(candidate) {
    if (!candidate) {
      return false;
    }
    if (encryptedVideos.has(candidate)) {
      return true;
    }
    try {
      return candidate.mediaKeys != null;
    } catch (error) {
      return false;
    }
  }

  function watchForEncryption(candidate) {
    if (!candidate || watchedVideos.has(candidate)) {
      return;
    }
    watchedVideos.add(candidate);
    try {
      candidate.addEventListener("encrypted", () => encryptedVideos.add(candidate), {
        once: true,
        passive: true
      });
    } catch (error) {
      // An element that will not take a listener tells us nothing either way.
    }
  }

  /** A plain file we could hand over even when yt-dlp cannot read the page. */
  function directSource(candidate) {
    if (!candidate) {
      return null;
    }
    const sources = [candidate.currentSrc, candidate.getAttribute("src")];
    for (const source of candidate.querySelectorAll("source[src]")) {
      sources.push(source.src);
    }
    for (const source of sources) {
      // blob: and data: are assembled in the page and cannot be refetched.
      if (typeof source === "string" && /^https?:\/\//i.test(source)) {
        return source;
      }
    }
    return null;
  }

  // -------------------------------------------------------------------------
  // The UI
  // -------------------------------------------------------------------------

  const STYLE = `
:host { all: initial; }
.layer {
  position: fixed;
  inset: 0;
  /* The layer covers the page, so it must never swallow a click. Only the
     pill and the panel take pointer events back. */
  pointer-events: none;
  z-index: 2147483647;
  font: 13px/1.35 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
}
.pill, .panel {
  position: fixed;
  pointer-events: auto;
  box-sizing: border-box;
  color: #ffffff;
  background: rgba(28, 28, 32, 0.94);
  border: 1px solid rgba(255, 255, 255, 0.16);
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45);
}
.pill {
  display: none;
  align-items: center;
  gap: 7px;
  padding: 6px 12px 6px 7px;
  border-radius: 999px;
  cursor: pointer;
  font-weight: 500;
  white-space: nowrap;
  user-select: none;
}
.pill:hover { background: #3584e4; border-color: #3584e4; }
.pill .dismiss {
  flex: 0 0 auto;
  margin-left: 3px;
  padding: 1px 3px 3px;
  border: 0;
  border-radius: 5px;
  background: transparent;
  color: rgba(255, 255, 255, 0.55);
  font: 15px/1 system-ui, sans-serif;
  cursor: pointer;
}
.pill .dismiss:hover { color: #ffffff; background: rgba(0, 0, 0, 0.4); }
.pill .mark {
  width: 18px;
  height: 18px;
  border-radius: 4px;
  display: block;
  flex: 0 0 auto;
  /* The logo is a dark tile, and so is the pill behind it. Without this ring
     the mark dissolves into the background at rest and only reappears on
     hover, when the pill turns blue. */
  box-shadow: 0 0 0 1px rgba(255, 255, 255, 0.24);
}
.panel {
  display: none;
  width: 300px;
  max-height: 60vh;
  border-radius: 12px;
  overflow: hidden;
  flex-direction: column;
}
.panel.open { display: flex; }
.head {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 11px 12px;
  border-bottom: 1px solid rgba(255, 255, 255, 0.1);
}
.title {
  flex: 1 1 auto;
  font-weight: 600;
  overflow: hidden;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  word-break: break-word;
}
.close {
  flex: 0 0 auto;
  cursor: pointer;
  border: 0;
  background: transparent;
  color: rgba(255, 255, 255, 0.6);
  font: 16px/1 system-ui, sans-serif;
  padding: 2px 4px;
  border-radius: 4px;
}
.close:hover { color: #ffffff; background: rgba(255, 255, 255, 0.12); }
.body { overflow-y: auto; padding: 5px; }
.row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  width: 100%;
  padding: 8px 9px;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.row:hover { background: #3584e4; }
.row .label { font-weight: 500; }
.row .detail { color: rgba(255, 255, 255, 0.62); font-size: 12px; white-space: nowrap; }
.row:hover .detail { color: rgba(255, 255, 255, 0.85); }
.note {
  padding: 12px 11px;
  color: rgba(255, 255, 255, 0.72);
  word-break: break-word;
}
.status {
  padding: 9px 12px;
  border-top: 1px solid rgba(255, 255, 255, 0.1);
  color: rgba(255, 255, 255, 0.7);
  font-size: 12px;
  word-break: break-word;
}
.options {
  display: none;
  border-top: 1px solid rgba(255, 255, 255, 0.1);
  padding: 8px 11px 10px;
}
.options.shown { display: block; }
.more {
  border: 0;
  background: transparent;
  color: rgba(255, 255, 255, 0.66);
  font: inherit;
  font-size: 12px;
  padding: 2px 0;
  cursor: pointer;
}
.more:hover { color: #ffffff; }
.fields { display: none; padding-top: 8px; }
.fields.shown { display: block; }
.field {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 3px 0;
  font-size: 12px;
  color: rgba(255, 255, 255, 0.78);
}
.field input {
  width: 108px;
  box-sizing: border-box;
  padding: 3px 6px;
  border-radius: 5px;
  border: 1px solid rgba(255, 255, 255, 0.2);
  background: rgba(0, 0, 0, 0.35);
  color: #ffffff;
  font: inherit;
  font-size: 12px;
}
.field input:focus { outline: 1px solid #3584e4; border-color: #3584e4; }
.status.bad { color: #ff9a92; }
.status.good { color: #8ff0a4; }
`;

  /** The logo, drawn inline, for when the packaged PNG cannot be loaded. */
  function fallbackMark() {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("class", "mark");
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", "M12 3v10.6l3.3-3.3 1.4 1.4L12 17.4l-4.7-4.7 1.4-1.4 3.3 3.3V3h2zM5 19h14v2H5v-2z");
    path.setAttribute("fill", "currentColor");
    svg.appendChild(path);
    return svg;
  }

  function build() {
    if (host) {
      return;
    }
    host = document.createElement("div");
    // A page could style any tag name it knows; this one it does not.
    host.setAttribute("data-snatch-overlay", "");
    // `closed` keeps page scripts from reaching in through `.shadowRoot`.
    root = host.attachShadow({ mode: "closed" });

    const style = document.createElement("style");
    style.textContent = STYLE;
    root.appendChild(style);

    const layer = document.createElement("div");
    layer.className = "layer";

    pill = document.createElement("div");
    pill.className = "pill";
    pill.setAttribute("role", "button");
    pill.setAttribute("tabindex", "0");

    const mark = document.createElement("img");
    mark.className = "mark";
    mark.alt = "";
    // A page with a strict image policy can still refuse an extension URL, and
    // a broken image icon in the corner of every video is worse than no logo.
    mark.addEventListener("error", () => mark.replaceWith(fallbackMark()), { once: true });
    try {
      mark.src = api.runtime.getURL("icons/icon-32.png");
    } catch (error) {
      mark.replaceWith(fallbackMark());
    }
    pill.appendChild(mark);

    const caption = document.createElement("span");
    caption.textContent = "Download with Snatch";
    pill.appendChild(caption);

    const dismiss = document.createElement("button");
    dismiss.className = "dismiss";
    dismiss.textContent = "×";
    dismiss.title = "Hide the button on this site";
    dismiss.setAttribute("aria-label", "Hide the button on this site");
    dismiss.addEventListener("click", (event) => {
      // Without this the click reaches the pill behind it and opens the panel
      // the user was trying to get rid of.
      event.preventDefault();
      event.stopPropagation();
      enabled = false;
      hide(true);
      void send({ type: "hide-site" });
    });
    pill.appendChild(dismiss);

    panel = document.createElement("div");
    panel.className = "panel";

    const head = document.createElement("div");
    head.className = "head";
    panelTitle = document.createElement("div");
    panelTitle.className = "title";
    const close = document.createElement("button");
    close.className = "close";
    close.textContent = "×";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", closePanel);
    head.append(panelTitle, close);

    panelBody = document.createElement("div");
    panelBody.className = "body";

    // Only shown when there is a recording to schedule. A quality from yt-dlp
    // is a download, and a download does not have a running time.
    optionsBox = document.createElement("div");
    optionsBox.className = "options";
    const more = document.createElement("button");
    more.className = "more";
    more.textContent = "Recording options ▾";
    const fields = document.createElement("div");
    fields.className = "fields";

    const startRow = document.createElement("label");
    startRow.className = "field";
    startRow.append(document.createTextNode("Start at"));
    startField = document.createElement("input");
    startField.type = "time";
    startRow.appendChild(startField);

    const minutesRow = document.createElement("label");
    minutesRow.className = "field";
    minutesRow.append(document.createTextNode("Record for"));
    minutesField = document.createElement("input");
    minutesField.type = "number";
    minutesField.min = "1";
    minutesField.placeholder = "minutes";
    minutesRow.appendChild(minutesField);

    fields.append(startRow, minutesRow);
    more.addEventListener("click", () => {
        const shown = fields.classList.toggle("shown");
        more.textContent = shown ? "Recording options ▴" : "Recording options ▾";
    });
    optionsBox.append(more, fields);

    panelStatus = document.createElement("div");
    panelStatus.className = "status";

    panel.append(head, panelBody, optionsBox, panelStatus);
    layer.append(pill, panel);
    root.appendChild(layer);

    // The player must not see any of this. Without it, a click on the pill
    // pauses YouTube, and a double click sends it fullscreen.
    for (const type of ["click", "dblclick", "mousedown", "mouseup", "pointerdown", "pointerup"]) {
      layer.addEventListener(type, (event) => event.stopPropagation());
    }
    pill.addEventListener("click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      togglePanel();
    });
    pill.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        togglePanel();
      }
    });

    attach();
  }

  /**
   * Put the overlay inside whatever is fullscreen, or back in the document.
   *
   * A fullscreen element is painted above everything else in the page, so a
   * fixed-position node outside it is simply not visible however high its
   * z-index is. Following the fullscreen element is the only way the pill
   * survives someone maximising the player.
   */
  function attach() {
    if (!host) {
      return;
    }
    const parent = document.fullscreenElement || document.documentElement;
    if (host.parentNode !== parent) {
      parent.appendChild(host);
    }
  }

  // -------------------------------------------------------------------------
  // Placement
  // -------------------------------------------------------------------------

  function place() {
    if (!video || !video.isConnected) {
      hide(true);
      return;
    }
    const rect = video.getBoundingClientRect();
    if (rect.width < MIN_WIDTH || rect.height < MIN_HEIGHT) {
      hide(true);
      return;
    }

    const width = pill.offsetWidth || 170;
    const height = pill.offsetHeight || 32;
    const margin = 12;
    // Anchored to the video's top right, then kept inside the viewport so a
    // player scrolled half off the screen still shows its pill.
    const left = clamp(rect.right - width - margin, margin, window.innerWidth - width - margin);
    const top = clamp(rect.top + margin, margin, window.innerHeight - height - margin);
    pill.style.left = `${Math.round(left)}px`;
    pill.style.top = `${Math.round(top)}px`;

    if (panelOpen) {
      const panelWidth = panel.offsetWidth || 300;
      const panelHeight = panel.offsetHeight || 200;
      const panelLeft = clamp(left + width - panelWidth, margin, window.innerWidth - panelWidth - margin);
      let panelTop = top + height + 8;
      // Not enough room below: flip it above the pill rather than off-screen.
      if (panelTop + panelHeight > window.innerHeight - margin) {
        panelTop = Math.max(margin, top - panelHeight - 8);
      }
      panel.style.left = `${Math.round(panelLeft)}px`;
      panel.style.top = `${Math.round(panelTop)}px`;
    }
  }

  function clamp(value, low, high) {
    return Math.min(Math.max(value, low), Math.max(low, high));
  }

  /** Track the video while anything is on screen; stop dead when it is not. */
  function loop() {
    if (!pillVisible && !panelOpen) {
      frame = 0;
      return;
    }
    place();
    frame = requestAnimationFrame(loop);
  }

  function show(candidate) {
    if (hideTimer) {
      clearTimeout(hideTimer);
      hideTimer = 0;
    }
    build();
    attach();
    watchForEncryption(candidate);
    if (candidate !== video) {
      // The pointer moved to a different video: the open list belongs to the
      // old one and would download the wrong thing.
      if (panelOpen) {
        closePanel();
      }
      video = candidate;
    }
    pill.style.display = "inline-flex";
    pillVisible = true;
    place();
    if (!frame) {
      frame = requestAnimationFrame(loop);
    }
  }

  function scheduleHide() {
    if (panelOpen || hideTimer) {
      return;
    }
    hideTimer = setTimeout(() => {
      hideTimer = 0;
      hide(false);
    }, HIDE_DELAY_MS);
  }

  function hide(immediate) {
    if (panelOpen && !immediate) {
      return;
    }
    if (hideTimer) {
      clearTimeout(hideTimer);
      hideTimer = 0;
    }
    pillVisible = false;
    if (pill) {
      pill.style.display = "none";
    }
    if (panelOpen) {
      closePanel();
    }
  }

  // -------------------------------------------------------------------------
  // The panel
  // -------------------------------------------------------------------------

  function togglePanel() {
    if (panelOpen) {
      closePanel();
    } else {
      openPanel();
    }
  }

  function closePanel() {
    panelOpen = false;
    if (panel) {
      panel.classList.remove("open");
    }
  }

  function status(text, tone) {
    panelStatus.className = tone ? `status ${tone}` : "status";
    panelStatus.textContent = text;
  }

  function clear(node) {
    while (node.firstChild) {
      node.removeChild(node.firstChild);
    }
  }

  function note(text) {
    clear(panelBody);
    const message = document.createElement("div");
    message.className = "note";
    message.textContent = text;
    panelBody.appendChild(message);
  }

  function addRow(label, detail, onPick) {
    const row = document.createElement("button");
    row.className = "row";
    const left = document.createElement("span");
    left.className = "label";
    // Every string here comes from the page or the site's own metadata.
    // `textContent` is what keeps it a string rather than markup.
    left.textContent = label;
    const right = document.createElement("span");
    right.className = "detail";
    right.textContent = detail;
    row.append(left, right);
    row.addEventListener("click", onPick);
    panelBody.appendChild(row);
    return row;
  }

  function openPanel() {
    panelOpen = true;
    panel.classList.add("open");
    panelTitle.textContent = document.title || location.hostname;
    note("Reading the page…");
    optionsBox.classList.remove("shown");
    status("Snatch is looking at what this page offers");
    place();
    if (!frame) {
      frame = requestAnimationFrame(loop);
    }

    const target = location.href;
    const fallback = directSource(video);

    // The address this player is using, straight from the DOM. The background
    // worker watches for these on the wire too, but only sees them the first
    // time: a file already in the browser's cache is fetched without a request
    // for anything to observe. Reading it from the element always works.
    send({ type: "formats", url: target, source: fallback }).then((reply) => {
      // The pointer moved on, or the user closed it, while we were asking.
      if (!panelOpen) {
        return;
      }
      if (reply.ok && Array.isArray(reply.formats) && reply.formats.length > 0) {
        renderFormats(reply, target);
        return;
      }
      renderFallback(reply.error, fallback, target, reply.updated === true);
    });
  }

  function renderFormats(reply, target) {
    clear(panelBody);
    let recordable = false;
    const heading = [reply.title, humanDuration(reply.duration)]
      .filter(Boolean)
      .join("  ·  ");
    if (heading) {
      panelTitle.textContent = heading;
    }
    for (const format of reply.formats) {
      if (!format || typeof format.id !== "string") {
        continue;
      }
      // Each row names the engine that takes it. A manifest is recorded by
      // ffmpeg, a plain file is fetched by the downloader in sixteen pieces,
      // and a selector goes to yt-dlp.
      const address = typeof format.url === "string" ? format.url : "";
      if (format.source === "stream" && address) {
        recordable = true;
        addRow(String(format.label || "Stream"), detailOf(format), () =>
          pick(
            "stream",
            {
              url: address,
              title: document.title,
              height: format.height,
              ...recordingOptions()
            },
            format.label
          )
        );
        continue;
      }
      if (format.source === "file" && address) {
        addRow(String(format.label || "Video file"), detailOf(format), () =>
          pick("direct", { url: address, referer: target }, format.label)
        );
        continue;
      }
      addRow(String(format.label || "Download"), detailOf(format), () =>
        pick("video", { url: target, format_id: format.id }, format.label)
      );
    }
    // Scheduling belongs to a recording. Offering it beside a yt-dlp format
    // would promise something that row cannot do.
    optionsBox.classList.toggle("shown", recordable);
    status(recordable ? "Pick a quality, or set when and how long" : "Pick a quality");
  }

  /**
   * yt-dlp could not read the page. A plain `<video src>` is still worth
   * offering: it is the ordinary hand-off Snatch does for any other link.
   */
  function renderFallback(error, fallback, target, updated) {
    clear(panelBody);
    optionsBox.classList.remove("shown");

    // Nothing on this page can work until it is reloaded, so say that and
    // nothing else. Offering a download that cannot start, or advice about
    // pressing play, only wastes the reader's time.
    if (updated) {
      note(UPDATED);
      status("Reload the page", "bad");
      return;
    }

    // Checked before anything else about the page: nothing on any site will
    // work until the app is updated, so it is the whole answer.
    if (OUTDATED.test(String(error || ""))) {
      note(
        "The Snatch app on this computer is older than this add-on, so it does " +
          "not understand what the button is asking for. Update Snatch, then " +
          "restart your browser."
      );
      status("Snatch is out of date", "bad");
      return;
    }

    // The add-on is fine and the program behind it is not there. Advice about
    // pressing play would send the reader looking in the wrong place.
    if (NO_HOST.test(String(error || ""))) {
      note(
        "Snatch itself is not answering. Install the Snatch app, then restart " +
          "your browser. The add-on on its own cannot download anything."
      );
      status("Snatch is not installed", "bad");
      return;
    }

    // Detected, and explained rather than attempted. The encrypted bytes are
    // all that crosses the wire; the key goes to a module inside the browser
    // that never gives it back.
    if (isProtected(video)) {
      note(
        "This video is locked with DRM. The key never leaves your browser, " +
          "so no download tool can save it — not Snatch, not any other."
      );
      status("Locked by the site", "bad");
      return;
    }
    if (fallback) {
      addRow("Download the video file", "direct", () =>
        pick("direct", { url: fallback, referer: target }, "the file")
      );
      status(error || "No quality list for this page");
      return;
    }
    addRow("Let Snatch try anyway", "best quality", () =>
      pick("video", { url: target }, "the video")
    );
    // Streams are only noticed once the player asks for them, so a panel
    // opened before anything has started has nothing to go on yet.
    note(
      (error || "Snatch could not list the qualities on this page.") +
        " If the video has not started, press play and try again."
    );
    status("");
  }

  /**
   * The time the user typed, as a Unix timestamp.
   *
   * A time input gives an hour and a minute with no date. Today is meant, and
   * tomorrow if today's has already gone -- which is what somebody setting
   * "02:00" at midnight means.
   */
  function scheduledFor(value) {
    const parts = /^(\d{1,2}):(\d{2})$/.exec(String(value || "").trim());
    if (!parts) {
      return null;
    }
    const hours = Number(parts[1]);
    const minutes = Number(parts[2]);
    // `setHours(99, 99)` does not fail, it rolls over into some other day.
    // A recording that starts four days late is worse than one refused.
    if (hours > 23 || minutes > 59) {
      return null;
    }
    const when = new Date();
    when.setHours(hours, minutes, 0, 0);
    if (when.getTime() <= Date.now()) {
      when.setDate(when.getDate() + 1);
    }
    return Math.round(when.getTime() / 1000);
  }

  /** Whatever was filled in, for a row that records rather than downloads. */
  function recordingOptions() {
    const options = {};
    const at = scheduledFor(startField && startField.value);
    if (at) {
      options.start_at = at;
    }
    const minutes = Number.parseInt((minutesField && minutesField.value) || "", 10);
    // A month is longer than any broadcast, and Snatch refuses more anyway;
    // stopping here means saying so in the panel rather than after a round
    // trip. A number too big for the field is simply not a length.
    if (Number.isFinite(minutes) && minutes > 0 && minutes <= 31 * 24 * 60) {
      options.record_seconds = minutes * 60;
    }
    return options;
  }

  function pick(type, request, what) {
    status("Sending to Snatch…");
    send({ type: type, ...request }).then((reply) => {
      if (!panelOpen) {
        return;
      }
      if (reply.updated === true) {
        status(UPDATED, "bad");
        return;
      }
      if (reply.ok) {
        status(`Sent ${what || "it"} to Snatch`, "good");
        setTimeout(() => {
          closePanel();
          hide(true);
        }, DONE_MS);
        return;
      }
      status(reply.error || "Snatch would not take it", "bad");
    });
  }

  // -------------------------------------------------------------------------
  // Listeners
  // -------------------------------------------------------------------------

  // Capture, because players stop pointer events from bubbling.
  document.addEventListener(
    "pointermove",
    (event) => {
      if (orphaned) {
        return;
      }
      const now = Date.now();
      if (now - lastHitTest < POINTER_INTERVAL_MS) {
        return;
      }
      lastHitTest = now;
      // Checked here because this is the first thing that happens on any page:
      // an extension replaced under a tab leaves this script running with a
      // pill that can no longer do anything, so it takes it away.
      if (!alive()) {
        teardown();
        return;
      }
      if (!enabled) {
        return;
      }

      const found = videoAt(event.clientX, event.clientY);
      if (found && isWorthwhile(found)) {
        show(found);
      } else {
        scheduleHide();
      }
    },
    { capture: true, passive: true }
  );

  document.addEventListener(
    "pointerdown",
    (event) => {
      // A click anywhere but our own UI closes the list.
      if (panelOpen && host && !event.composedPath().includes(host)) {
        closePanel();
      }
    },
    { capture: true, passive: true }
  );

  document.addEventListener(
    "keydown",
    (event) => {
      if (event.key === "Escape" && panelOpen) {
        closePanel();
      }
    },
    { capture: true }
  );

  document.addEventListener("fullscreenchange", () => {
    attach();
    place();
  });

  window.addEventListener("pagehide", () => hide(true));

  api.runtime.onMessage.addListener((message) => {
    if (!message || message.type !== "snatch-overlay") {
      return;
    }
    // This page knows its own host, so it is told which sites are hidden and
    // decides for itself. The alternative is the extension reading every
    // tab's address, which Firefox charges a browsing-history permission for.
    const blocked =
      Array.isArray(message.hidden) &&
      message.hidden.includes(location.hostname.toLowerCase());
    enabled = message.enabled !== false && !blocked;
    if (!enabled) {
      hide(true);
    }
  });

  // The toolbar button may already be paused when this page loads.
  send({ type: "state" }).then((reply) => {
    if (reply && reply.ok) {
      enabled = reply.enabled !== false;
      if (!enabled) {
        hide(true);
      }
    }
  });
})();
