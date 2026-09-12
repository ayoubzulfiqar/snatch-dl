// Capture the video a page assembles in JavaScript.
//
// Some players never let a whole file cross the network. They fetch segments
// -- sometimes decrypting or reassembling them in the page first -- and hand
// the pieces to a MediaSource through appendBuffer. From outside, the network
// carries only unusable fragments; the playable bytes exist only here, inside
// the page, as they are appended.
//
// So this wraps the three MediaSource calls that matter and copies the bytes
// as they go past. It is injected into the page's own world at document-start,
// before the page has a chance to hold a reference to the originals, and it
// changes nothing it sees: every wrapped call runs the real one and returns
// exactly what it returned.
//
// It does NOT defeat DRM. Encrypted Media Extensions never append clear bytes
// -- the decrypt happens in a module downstream of appendBuffer -- so what is
// copied here is the ciphertext, which is useless. This is for the far more
// common player that assembles clear media in JavaScript.
//
// Bytes leave through window.webkit.messageHandlers, which takes a string, so
// each chunk is base64. The 33% overhead is nothing against being certain the
// bytes arrive unmangled: a "binary string" would be re-encoded as UTF-8 on
// the way through and corrupted.
(function () {
  "use strict";
  // The page can be injected into more than once -- subframes, a world reused
  // across a same-document navigation. Wrap the prototypes once.
  if (window.__snatchMseInstalled) {
    return;
  }
  window.__snatchMseInstalled = true;

  var CHANNEL = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.snatchMse;
  if (!CHANNEL || typeof window.MediaSource === "undefined") {
    return;
  }

  var nextMs = 1;
  var nextSb = 1;
  // MediaSource id -> { armed, buffers: [sbId...] }
  var sources = new Map();
  // SourceBuffer id -> {
  //   ms, mime, seq,
  //   init:   [Uint8Array]  -- the init segment, never evicted,
  //   ring:   [Uint8Array]  -- media held before arming, oldest evicted,
  //   held:   bytes in ring,
  //   total:  bytes ever appended,
  // }
  var buffers = new Map();

  // How much media to hold, across the WHOLE page, before a capture is armed.
  //
  // A budget for everything, not per track. A video-heavy feed opens many
  // MediaSources at once, each with a video and an audio track, and a
  // per-track allowance times all of them is hundreds of megabytes to a
  // gigabyte held in the page -- which is what was crashing the web process
  // on exactly those sites. This caps the total the page can ever hold: the
  // oldest media anywhere is dropped once it is reached. Init segments are
  // kept apart and never counted, because media without one cannot be played.
  var RING_BYTES = 32 * 1024 * 1024;
  // Media chunks held for capture, page-wide: the SourceBuffer id of each, in
  // arrival order, so the globally-oldest can be found to evict.
  var heldOrder = [];
  var heldTotal = 0;

  // A hard ceiling on how many tracks are followed at once. A pathological
  // page cannot make the maps grow without end.
  var MAX_TRACKS = 256;

  // Drop the oldest media anywhere until the page is back under budget. The
  // init segment is never here, so it is never dropped.
  function evict() {
    while (heldTotal > RING_BYTES && heldOrder.length > 0) {
      var sb = heldOrder.shift();
      var s = buffers.get(sb);
      if (s && s.ring.length > 0) {
        heldTotal -= s.ring[0].length;
        s.held -= s.ring[0].length;
        s.ring.shift();
      }
    }
  }

  function post(message) {
    try {
      CHANNEL.postMessage(JSON.stringify(message));
    } catch (error) {
      // A dead channel must never break the page.
    }
  }

  // A fast base64 that does not overflow the stack on a multi-megabyte chunk:
  // String.fromCharCode.apply throws above a few hundred thousand arguments,
  // so the binary string is built a window at a time.
  function base64(bytes) {
    var binary = "";
    var window = 0x8000;
    for (var i = 0; i < bytes.length; i += window) {
      binary += String.fromCharCode.apply(null, bytes.subarray(i, Math.min(i + window, bytes.length)));
    }
    return btoa(binary);
  }

  // Whichever shape appendBuffer was handed, as bytes we own. Copied rather
  // than referenced: the caller is free to reuse or detach its buffer the
  // instant appendBuffer returns.
  function toBytes(data) {
    if (data instanceof ArrayBuffer) {
      return new Uint8Array(data.slice(0));
    }
    if (ArrayBuffer.isView(data)) {
      return new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
    }
    return null;
  }

  // The four-character box type at the head of an ISO-BMFF chunk, or "" for
  // anything too short. Used only to tell an init segment from media.
  function boxType(bytes) {
    if (bytes.length < 8) {
      return "";
    }
    return String.fromCharCode(bytes[4], bytes[5], bytes[6], bytes[7]);
  }

  // Whether a chunk is (part of) an initialisation segment rather than media.
  //
  // An init segment describes the tracks and must lead the file, or the media
  // that follows it cannot be decoded. fMP4 puts it in ftyp/styp/moov boxes;
  // WebM in an EBML header. Media is moof/mdat, or a WebM Cluster. Knowing
  // which lets capture that is armed part way through still start with the
  // init segment it kept, so the file it writes is playable.
  function isInit(bytes) {
    var box = boxType(bytes);
    if (box === "ftyp" || box === "styp" || box === "moov") {
      return true;
    }
    if (box === "moof" || box === "mdat" || box === "sidx") {
      return false;
    }
    // WebM: an EBML header (0x1A45DFA3) is init; a Cluster (0x1F43B675) is
    // media. Anything else unrecognised is treated as media, so retention
    // stops rather than growing without bound.
    if (bytes.length >= 4) {
      if (bytes[0] === 0x1a && bytes[1] === 0x45 && bytes[2] === 0xdf && bytes[3] === 0xa3) {
        return true;
      }
    }
    return false;
  }

  // Keep an init segment small even if the classifier is fooled. A real one
  // is a few kilobytes; this ceiling only ever trips on a stream we misread,
  // and it is kept low because init segments are held outside the page-wide
  // budget -- one per track, up to MAX_TRACKS of them -- so an over-generous
  // ceiling times many tracks would be its own way to grow without bound.
  var MAX_INIT_BYTES = 512 * 1024;

  function onAppend(sbId, data) {
    var state = buffers.get(sbId);
    if (!state) {
      return;
    }
    var bytes = toBytes(data);
    if (!bytes || bytes.length === 0) {
      return;
    }
    state.total += bytes.length;
    var source = sources.get(state.ms);

    // Armed: every append goes straight out.
    if (source && source.armed) {
      post({ t: "data", sb: sbId, seq: state.seq++, b64: base64(bytes) });
      return;
    }

    // Not armed: hold it, so arming later still captures the lead-in. The
    // init segment is kept apart and never evicted -- media without it cannot
    // be decoded. Media joins the page-wide ring, and the oldest anywhere is
    // dropped once the whole page is over budget.
    if (isInit(bytes) && state.ring.length === 0) {
      if (state.initBytes + bytes.length <= MAX_INIT_BYTES) {
        state.init.push(bytes);
        state.initBytes += bytes.length;
      }
    } else {
      state.ring.push(bytes);
      state.held += bytes.length;
      heldTotal += bytes.length;
      heldOrder.push(sbId);
      evict();
    }
    post({ t: "grow", ms: state.ms, sb: sbId, bytes: state.initBytes + state.held });
  }

  var addSourceBuffer = window.MediaSource.prototype.addSourceBuffer;
  window.MediaSource.prototype.addSourceBuffer = function (mime) {
    var sb = addSourceBuffer.apply(this, arguments);
    // A page cannot make us track without limit.
    if (buffers.size >= MAX_TRACKS) {
      return sb;
    }
    try {
      if (this.__snatchId === undefined) {
        this.__snatchId = nextMs++;
        sources.set(this.__snatchId, { armed: false, buffers: [] });
        post({ t: "open", ms: this.__snatchId });
      }
      var sbId = nextSb++;
      sb.__snatchId = sbId;
      sb.__snatchMs = this.__snatchId;
      buffers.set(sbId, {
        ms: this.__snatchId,
        mime: String(mime || ""),
        init: [],
        initBytes: 0,
        ring: [],
        held: 0,
        total: 0,
        seq: 0,
      });
      sources.get(this.__snatchId).buffers.push(sbId);
      post({ t: "track", ms: this.__snatchId, sb: sbId, mime: String(mime || "") });
    } catch (error) {
      // Never let bookkeeping break the page's own player.
    }
    return sb;
  };

  var appendBuffer = window.SourceBuffer.prototype.appendBuffer;
  window.SourceBuffer.prototype.appendBuffer = function (data) {
    try {
      if (this.__snatchId !== undefined) {
        onAppend(this.__snatchId, data);
      }
    } catch (error) {
      // Copying the bytes must never stop them being appended.
    }
    return appendBuffer.apply(this, arguments);
  };

  var endOfStream = window.MediaSource.prototype.endOfStream;
  window.MediaSource.prototype.endOfStream = function () {
    try {
      if (this.__snatchId !== undefined) {
        post({ t: "end", ms: this.__snatchId });
      }
    } catch (error) {
      // ignore
    }
    return endOfStream.apply(this, arguments);
  };

  // Called from the Rust side when the reader chooses to capture a stream.
  // Emits the init segment it kept, then lets every later append through.
  window.__snatchArm = function (msId) {
    var source = sources.get(msId);
    if (!source || source.armed) {
      return false;
    }
    source.armed = true;
    source.buffers.forEach(function (sbId) {
      var state = buffers.get(sbId);
      if (!state) {
        return;
      }
      // The init segment first, then the media held since it opened, in the
      // order it arrived -- so what is written begins with a decodable head.
      state.init.concat(state.ring).forEach(function (part) {
        post({ t: "data", sb: sbId, seq: state.seq++, b64: base64(part) });
      });
      // Give the page-wide ring back what this track was holding.
      heldTotal -= state.held;
      heldOrder = heldOrder.filter(function (id) {
        return id !== sbId;
      });
      state.init = [];
      state.ring = [];
      state.held = 0;
      state.initBytes = 0;
    });
    return true;
  };

  post({ t: "ready" });
})();
