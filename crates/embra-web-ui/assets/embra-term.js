// xterm.js ↔ /ws/terminal glue, kept out of Rust on purpose.
//
// Server protocol:
//   server→client: binary = raw PTY bytes; text {"t":"role",role,owner}
//   client→server: binary = keystrokes; text {"t":resize|input|key|takeover}
//
// Write-arbitration is server-authoritative; this also gates locally
// (observer keystrokes/resizes are not sent) for snappy UX.
(function () {
  let term = null, fit = null, ws = null;
  let writable = false;
  let roleCb = null;
  let lastRole = "observer", lastOwner = "none";
  let reconnectTimer = null;

  function wsUrl() {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    return `${proto}://${location.host}/ws/terminal`;
  }

  // Screen pixel size for the PTY winsize (media wave): the console
  // derives its cell size from xpixel/cols and ypixel/rows to size sixel
  // images. `.xterm-screen` is laid out at exactly cols×cellW by
  // rows×cellH, so its box is the cleanest public-API source; the private
  // render-service dimensions are the fallback. 0 = unknown → the console
  // falls back to halfblocks rather than guessing a cell size.
  function screenPixels() {
    try {
      const el = term && term.element && term.element.querySelector(".xterm-screen");
      if (el) {
        const r = el.getBoundingClientRect();
        if (r.width > 0 && r.height > 0) return [Math.round(r.width), Math.round(r.height)];
      }
      const d = term && term._core && term._core._renderService && term._core._renderService.dimensions;
      if (d && d.css && d.css.cell) {
        return [Math.round(d.css.cell.width * term.cols), Math.round(d.css.cell.height * term.rows)];
      }
    } catch (e) {}
    return [0, 0];
  }

  function sendResize() {
    if (writable && ws && ws.readyState === 1 && term) {
      const [xpixel, ypixel] = screenPixels();
      ws.send(JSON.stringify({ t: "resize", cols: term.cols, rows: term.rows, xpixel, ypixel }));
    }
  }

  function connect() {
    ws = new WebSocket(wsUrl());
    ws.binaryType = "arraybuffer";
    ws.onopen = () => { try { fit.fit(); } catch (e) {} sendResize(); };
    ws.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer) {
        term.write(new Uint8Array(e.data));
        return;
      }
      let msg;
      try { msg = JSON.parse(e.data); } catch (_) { return; }
      if (msg && msg.t === "role") {
        lastRole = msg.role; lastOwner = msg.owner;
        writable = msg.role === "writer";
        document.body.classList.toggle("is-observer", !writable);
        // First chance to push our geometry: sendResize() is gated on
        // `writable`, which is still false on ws.onopen / initial
        // ResizeObserver. Without this the PTY stays at portable-pty's
        // 80x24 default until a manual browser resize. Fires on initial
        // connect, reconnect, and takeover (resync per writer change).
        if (writable) { try { fit.fit(); } catch (e) {} sendResize(); }
        if (roleCb) roleCb(lastRole, lastOwner);
      }
    };
    ws.onclose = () => {
      term.write("\r\n\x1b[2m[embra-web] connection lost — reconnecting…\x1b[0m\r\n");
      if (!reconnectTimer) reconnectTimer = setTimeout(() => {
        reconnectTimer = null; connect();
      }, 2000);
    };
  }

  window.embraTermInit = function (elId) {
    if (term) return; // once
    // fontSize is pulled from the shell's --term-fs CSS variable so
    // app.css is the single source of truth for the font-size
    // hierarchy. Falls back to 15 (xterm.js's own default) if the
    // variable can't be parsed.
    const termFs = parseInt(
      getComputedStyle(document.documentElement).getPropertyValue("--term-fs"),
      10
    ) || 15;
    term = new Terminal({
      fontFamily: "'JetBrains Mono','Fira Code',ui-monospace,monospace",
      // scrollback: 0 — the TUI is full-screen ratatui drawn on the
      // normal buffer (no alt-screen, for QEMU serial parity) and owns
      // every cell: it re-diffs every ~200ms and repaints in full on
      // attach and resize. Any scrollback would just be stale snapshots
      // the user could wheel away into.
      fontSize: termFs, cursorBlink: true, scrollback: 0,
      // Warm brand bg + amber (flame) cursor to blend with the shell.
      // The 16 ANSI colors are intentionally left at xterm defaults so
      // the real ratatui TUI's own colors render true (parity-safe).
      theme: { background: "#0c0907", foreground: "#ece2d8",
               cursor: "#ff7a1a", cursorAccent: "#0c0907",
               selectionBackground: "#3a2a18" },
    });
    fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    // Media wave: in-terminal images. embra-console (EMBRA_TUI_GRAPHICS=
    // sixel on this PTY) renders its media pane as sixel through
    // ratatui-image; this addon paints it. Cell geometry travels in the
    // resize frames (screenPixels), not via the addon's size reports.
    // Loaded BEFORE open() (addon README order), try/catch so a missing/
    // incompatible addon degrades to "text card only" rather than a
    // blank terminal.
    try {
      term.loadAddon(new ImageAddon.ImageAddon({
        enableSizeReports: true,
        sixelSupport: true,
        sixelScrolling: true,
        sixelPaletteLimit: 4096,
        iipSupport: true,
        storageLimit: 32,
        showPlaceholder: false,
      }));
    } catch (e) { console.warn("embra-web: image addon unavailable", e); }
    term.open(document.getElementById(elId));
    // Grid-locked glyph rendering. xterm's default DOM renderer places
    // glyphs by their natural font advance, so on long/wrapped rows the
    // characters drift off the integer cell grid and visually bunch/
    // overlap (worse at non-100% browser zoom or fractional DPR). The
    // Canvas renderer paints every cell at col*cellWidth. Loaded AFTER
    // term.open() (xterm requires renderer addons to attach post-open),
    // wrapped in try/catch so a missing/incompatible addon degrades to
    // the DOM renderer rather than a blank terminal.
    try { term.loadAddon(new CanvasAddon.CanvasAddon()); } catch (e) {}
    try { fit.fit(); } catch (e) {}
    term.onData((d) => {
      if (writable && ws && ws.readyState === 1) ws.send(new TextEncoder().encode(d));
    });
    const ro = new ResizeObserver(() => { try { fit.fit(); } catch (e) {} sendResize(); });
    ro.observe(document.getElementById(elId));
    window.addEventListener("resize", () => { try { fit.fit(); } catch (e) {} sendResize(); });
    // Media wave: drop an image on the terminal, or paste image data,
    // to attach it — upload to the store, then type `/attach <id>` into
    // the console for the operator (the brain stages it for the next
    // message). Plain-text pastes are left to xterm.
    const host = document.getElementById(elId);
    host.addEventListener("dragover", (e) => { e.preventDefault(); });
    host.addEventListener("drop", (e) => {
      e.preventDefault();
      const files = e.dataTransfer && e.dataTransfer.files;
      if (files && files.length) window.embraUploadFiles(files);
    });
    document.addEventListener("paste", (e) => {
      const files = e.clipboardData && e.clipboardData.files;
      if (files && files.length && [...files].some((f) => f.type.startsWith("image/"))) {
        e.preventDefault();
        window.embraUploadFiles(files);
      }
    });
    connect();
  };

  // Upload one or more images to the MEDIA store and inject `/attach <id>`
  // per success. Errors surface as a terminal line (bracketed, dim) so the
  // operator sees them where they are looking.
  window.embraUploadFiles = async function (files) {
    for (const f of Array.from(files)) {
      if (!f.type.startsWith("image/") && f.type !== "") continue;
      try {
        const resp = await fetch("/api/media", {
          method: "PUT",
          headers: {
            "Content-Type": f.type || "application/octet-stream",
            "X-Embra-Name": encodeURIComponent(f.name),
          },
          body: f,
        });
        const body = await resp.json().catch(() => ({}));
        if (!resp.ok || !body.id) {
          const msg = body.error || ("upload rejected (" + resp.status + ")");
          if (term) term.write("\r\n\x1b[2m[embra-web] attach failed: " + msg + "\x1b[0m\r\n");
          continue;
        }
        if (writable) window.embraTermInject("/attach " + body.id + "\r");
        else if (term) term.write("\r\n\x1b[2m[embra-web] uploaded " + body.id + " — take control to attach it\x1b[0m\r\n");
      } catch (err) {
        if (term) term.write("\r\n\x1b[2m[embra-web] attach failed: " + err + "\x1b[0m\r\n");
      }
    }
  };

  window.embraTermInject = function (text) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify({ t: "input", data: text }));
  };
  window.embraTermKey = function (code) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify({ t: "key", code }));
  };
  window.embraTermTakeover = function () {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify({ t: "takeover" }));
  };
  window.embraTermFocus = function () {
    if (term) try { term.focus(); } catch (e) {}
  };
  window.embraTermSetRoleCb = function (cb) {
    roleCb = cb;
    cb(lastRole, lastOwner); // prime
  };
})();
