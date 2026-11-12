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
  // Reconnect pacing (#30): 1 s after a loss, doubling to 10 s while the
  // socket keeps closing at once; a socket that stayed open for
  // RECONNECT_STABLE_MS was working, so the next wait is 1 s again.
  const RECONNECT_BASE_MS = 1000, RECONNECT_MAX_MS = 10000, RECONNECT_STABLE_MS = 5000;
  let reconnectDelay = RECONNECT_BASE_MS, openedAt = 0;

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
    ws.onopen = () => { openedAt = Date.now(); try { fit.fit(); } catch (e) {} sendResize(); };
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
      if (openedAt && Date.now() - openedAt >= RECONNECT_STABLE_MS) reconnectDelay = RECONNECT_BASE_MS;
      openedAt = 0;
      const secs = Math.round(reconnectDelay / 1000);
      term.write(`\r\n\x1b[2m[embra-web] connection lost — reconnecting in ${secs} s…\x1b[0m\r\n`);
      if (!reconnectTimer) reconnectTimer = setTimeout(() => {
        reconnectTimer = null; connect();
      }, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX_MS);
    };
  }

  // The display console's pointer and fences (see embraTermInit).
  function embraKioskSetup() {
    const cur = document.createElement("div");
    cur.id = "embra-kiosk-cursor";
    cur.style.cssText = "position:fixed;left:0;top:0;width:18px;height:26px;pointer-events:none;z-index:2147483647;display:none;";
    cur.innerHTML = '<svg width="18" height="26" viewBox="0 0 18 26"><path d="M1 1 L1 20 L6 15.5 L9.5 24 L12.5 22.5 L9 14 L16 14 Z" fill="#ece2d8" stroke="#0c0907" stroke-width="1.5" stroke-linejoin="round"/></svg>';
    document.body.appendChild(cur);
    window.addEventListener("pointermove", (e) => {
      cur.style.transform = "translate(" + e.clientX + "px," + e.clientY + "px)";
      cur.style.display = "block";
    }, { passive: true });
    // The page is the whole screen: no new windows, no leaving the origin.
    window.open = function () { return null; };
    document.addEventListener("click", (e) => {
      const a = e.target && e.target.closest ? e.target.closest("a[href]") : null;
      if (a && a.origin !== window.location.origin) e.preventDefault();
    }, true);
    embraKioskMenu();
  }

  // The display console's own menu, on the right mouse button. The launcher
  // draws no menus on the machine's display, so the page draws this one, as
  // it draws the pointer. Copy takes the terminal's selection, or the page's
  // where the terminal has none. Paste hands the text to xterm's paste path
  // (one bracketed paste: the console reads it as a paste, never as typed
  // lines) or puts it into the text field that was clicked. The clipboard is
  // the page's own and lives as long as the page does: the display has no
  // other, and nothing leaves the machine through it.
  let kioskClip = "";
  function embraKioskMenu() {
    const menu = document.createElement("div");
    menu.id = "embra-kiosk-menu";
    menu.className = "kiosk-menu";
    menu.setAttribute("role", "menu");
    const item = (act, label) => {
      const b = document.createElement("button");
      b.type = "button"; b.setAttribute("role", "menuitem");
      b.dataset.act = act; b.textContent = label;
      menu.appendChild(b);
      return b;
    };
    const copy = item("copy", "Copy"), paste = item("paste", "Paste");
    menu.appendChild(document.createElement("hr"));
    item("url", "This console\u2019s URL\u2026");
    document.body.appendChild(menu);

    // This console's URL with the boot's token, for a browser on another
    // machine. The boot banner printed it once and the display console
    // covers that copy; embra-web gives it to the machine's own browser and
    // to no other (`/api/console-url`). It is on the page only while the
    // sheet is up.
    const sheet = document.createElement("div");
    sheet.id = "embra-kiosk-url";
    sheet.className = "kiosk-sheet-bg";
    const box = document.createElement("div");
    box.className = "kiosk-sheet";
    box.setAttribute("role", "dialog");
    sheet.appendChild(box);
    document.body.appendChild(sheet);
    const sheetOpen = () => sheet.style.display === "flex";
    const closeSheet = () => {
      sheet.style.display = "none";
      box.textContent = "";
      if (term) try { term.focus(); } catch (e) {}
    };
    const el = (tag, cls, text) => {
      const n = document.createElement(tag);
      n.className = cls;
      if (text != null) n.textContent = text;
      return n;
    };
    const showUrl = async () => {
      box.textContent = "";
      box.appendChild(el("b", "ks-title", "This console from another machine"));
      const rows = el("div", "ks-body", null);
      box.appendChild(rows);
      box.appendChild(el("div", "ks-note", "The URL carries this boot\u2019s access token: whoever has it has the console, until the next boot. The browser asks about the certificate once (embraOS-CA)."));
      const bye = el("button", "ks-close", "Close");
      bye.type = "button";
      bye.addEventListener("click", closeSheet);
      box.appendChild(bye);
      sheet.style.display = "flex";
      try {
        const resp = await fetch("/api/console-url", { cache: "no-store" });
        if (!resp.ok) throw new Error("the web console answered " + resp.status);
        const data = await resp.json();
        for (const row of (data.urls || [])) {
          rows.appendChild(el("code", "ks-url", row.url));
          rows.appendChild(el("div", "ks-where", row.where));
        }
      } catch (err) {
        rows.appendChild(el("div", "ks-where", "The URL could not be read: " + (err && err.message ? err.message : err)));
      }
    };
    sheet.addEventListener("mousedown", (e) => { if (e.target === sheet) closeSheet(); });

    // What the menu was opened on: the text selected then, and the page's
    // text field under the pointer (xterm's own textarea is the terminal).
    let picked = "", field = null;
    const open = () => menu.style.display === "block";
    const close = () => { menu.style.display = "none"; picked = ""; field = null; };
    const textField = (el) => {
      if (!el || !el.matches || el.closest(".xterm")) return null;
      if (el.matches("textarea")) return el;
      // The input types that have a caret to paste at.
      return el.matches("input") && /^(text|search|password|url|tel|)$/.test(el.getAttribute("type") || "") ? el : null;
    };
    const selected = () => {
      // A masked field gives nothing to copy, as in any browser.
      if (field && field.type === "password") return "";
      if (field && field.selectionEnd > field.selectionStart) {
        return field.value.substring(field.selectionStart, field.selectionEnd);
      }
      if (term && term.hasSelection()) return term.getSelection();
      const s = window.getSelection ? window.getSelection() : null;
      return s ? s.toString() : "";
    };

    document.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      if (sheetOpen()) return;
      field = textField(e.target);
      picked = selected();
      copy.disabled = picked === "";
      // An observer types nothing into the terminal; a page field is its own.
      paste.disabled = kioskClip === "" || !(field || writable);
      menu.style.display = "block";
      const x = Math.max(0, Math.min(e.clientX, window.innerWidth - menu.offsetWidth - 4));
      const y = Math.max(0, Math.min(e.clientY, window.innerHeight - menu.offsetHeight - 4));
      menu.style.transform = "translate(" + x + "px," + y + "px)";
    }, true);

    // A press on the menu keeps the focus and the selection where they are.
    menu.addEventListener("mousedown", (e) => { e.preventDefault(); });
    menu.addEventListener("click", (e) => {
      const b = e.target && e.target.closest ? e.target.closest("button[data-act]") : null;
      if (!b || b.disabled) return;
      const text = picked, into = field;
      close();
      if (b.dataset.act === "url") {
        showUrl();
      } else if (b.dataset.act === "copy") {
        kioskClip = text;
      } else if (b.dataset.act === "paste") {
        if (into && into.isConnected) {
          // The shell's fields are bound to their `input` event.
          try {
            into.focus();
            into.setRangeText(kioskClip, into.selectionStart, into.selectionEnd, "end");
            into.dispatchEvent(new Event("input", { bubbles: true }));
          } catch (err) {}
        } else if (writable && term) {
          term.paste(kioskClip);
          term.focus();
        }
      }
    });

    // Anything else closes the menu: a press elsewhere, the wheel, a new
    // size, and Escape, which stops here (in the console Esc is /stop).
    document.addEventListener("mousedown", (e) => {
      if (open() && !menu.contains(e.target)) close();
    }, true);
    window.addEventListener("wheel", () => { if (open()) close(); }, { passive: true, capture: true });
    window.addEventListener("resize", () => { if (open()) close(); });
    window.addEventListener("keydown", (e) => {
      if (open() && e.key === "Escape") {
        e.preventDefault(); e.stopImmediatePropagation();
        close();
      } else if (sheetOpen()) {
        // The sheet holds the keyboard: Escape closes it, nothing is typed
        // into the console behind it.
        e.preventDefault(); e.stopImmediatePropagation();
        if (e.key === "Escape") closeSheet();
      }
    }, true);
  }

  window.embraTermInit = function (elId) {
    if (term) return; // once
    // The display console (cog on the machine's own display, `?kiosk=1`):
    // the launcher draws no pointer on a display without a cursor plane,
    // so the page draws one; a download has nowhere to go there; the page
    // never navigates away; the cursor does not blink (every blink is a
    // repaint through software rendering).
    const kiosk = /(?:^\?|&)kiosk=1(?:&|$)/.test(window.location.search);
    window.embraKiosk = kiosk;
    if (kiosk) embraKioskSetup();
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
      fontSize: termFs, cursorBlink: !kiosk, scrollback: 0,
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
    // On the display console nobody clicks first: the terminal takes the
    // keyboard as soon as it is open (a remote browser focuses on its own).
    if (kiosk) setTimeout(() => { try { term.focus(); } catch (e) {} }, 0);
    // Grid-locked glyph rendering. xterm's default DOM renderer places
    // glyphs by their natural font advance, so on long/wrapped rows the
    // characters drift off the integer cell grid and visually bunch/
    // overlap (worse at non-100% browser zoom or fractional DPR). The
    // Canvas renderer paints every cell at col*cellWidth. Loaded AFTER
    // term.open() (xterm requires renderer addons to attach post-open),
    // wrapped in try/catch so a missing/incompatible addon degrades to
    // the DOM renderer rather than a blank terminal.
    try { term.loadAddon(new CanvasAddon.CanvasAddon()); } catch (e) {}
    // Download links. The console prints `web: /api/files/<path>` (a file
    // card), `web: /api/media/<id>` (an image card) and
    // `web: /api/backups/<name>` (`/backup list`) on a row of its own; the
    // provider underlines the route and a click fetches it as a download —
    // same origin, so the `download` attribute is honoured.
    // Both routes are ASCII (percent-encoded), so column equals index.
    const embraDownload = (url) => {
      if (kiosk) {
        term.write("\r\n\x1b[2m[embra-web] downloads are for a remote browser: " + url + "\x1b[0m\r\n");
        return;
      }
      const a = document.createElement("a");
      a.href = url; a.download = ""; a.style.display = "none";
      document.body.appendChild(a); a.click(); a.remove();
    };
    // A live offer (file_offer, /download) starts its download by itself:
    // the console announces it once with a private OSC that never reaches
    // the screen (`ESC ] 8771 ; /api/files/<path> BEL`); the writer tab
    // fetches it, observers keep the clickable row below.
    try {
      term.parser.registerOscHandler(8771, (data) => {
        if (writable && /^\/api\/files\//.test(data)) embraDownload(data);
        return true;
      });
    } catch (e) { console.warn("embra-web: download handler unavailable", e); }
    try {
      const LINK = /\/api\/(files|media|backups)\/[^\s]+/g;
      term.registerLinkProvider({
        provideLinks(y, cb) {
          const line = term.buffer.active.getLine(y - 1);
          if (!line) { cb(undefined); return; }
          const text = line.translateToString(true);
          const links = [];
          let m;
          LINK.lastIndex = 0;
          while ((m = LINK.exec(text)) !== null) {
            const url = m[0];
            links.push({
              text: url,
              range: { start: { x: m.index + 1, y }, end: { x: m.index + url.length, y } },
              activate: (_e, u) => embraDownload(u),
            });
          }
          cb(links.length ? links : undefined);
        },
      });
    } catch (e) { console.warn("embra-web: link provider unavailable", e); }
    // A tool call's marker row (`⚙ #17  14:02:11  read_file  0.8s`, `!` on
    // an error) links to the call in the sidebar's Tools tab: the number
    // is the brain's per-boot call number, the same the tool feed carries.
    // Observers click too — nothing is typed.
    try {
      const MARKER = /[⚙!] #(\d+)\b/g;
      term.registerLinkProvider({
        provideLinks(y, cb) {
          const line = term.buffer.active.getLine(y - 1);
          if (!line) { cb(undefined); return; }
          const text = line.translateToString(true);
          const links = [];
          let m;
          MARKER.lastIndex = 0;
          while ((m = MARKER.exec(text)) !== null) {
            const seq = Number(m[1]);
            // Columns are code units; the marker's glyph is one cell.
            const start = Array.from(text.slice(0, m.index)).length + 1;
            links.push({
              text: m[0],
              range: { start: { x: start, y }, end: { x: start + Array.from(m[0]).length - 1, y } },
              activate: () => window.dispatchEvent(new CustomEvent("embra-tool-open", { detail: { seq } })),
            });
          }
          cb(links.length ? links : undefined);
        },
      });
    } catch (e) { console.warn("embra-web: marker link provider unavailable", e); }
    try { fit.fit(); } catch (e) {}
    term.onData((d) => {
      if (writable && ws && ws.readyState === 1) ws.send(new TextEncoder().encode(d));
    });
    const ro = new ResizeObserver(() => { try { fit.fit(); } catch (e) {} sendResize(); });
    ro.observe(document.getElementById(elId));
    window.addEventListener("resize", () => { try { fit.fit(); } catch (e) {} sendResize(); });
    // Media wave: drop a file on the terminal, or paste one, to attach
    // it — upload it, then type `/attach <id|path>` into the console for
    // the operator (the brain stages it for the next message). An image
    // gets a media id, a text file lands in uploads/ and is named by its
    // path. Plain-text pastes are left to xterm.
    const host = document.getElementById(elId);
    host.addEventListener("dragover", (e) => { e.preventDefault(); });
    host.addEventListener("drop", (e) => {
      e.preventDefault();
      const files = e.dataTransfer && e.dataTransfer.files;
      if (files && files.length) window.embraUploadFiles(files);
    });
    document.addEventListener("paste", (e) => {
      const files = e.clipboardData && e.clipboardData.files;
      if (files && files.length) {
        e.preventDefault();
        window.embraUploadFiles(files);
      }
    });
    connect();
  };

  // What the picker, a drop or a paste may upload: an image, a text file
  // by type or by extension, or a file the browser cannot type (the brain
  // then decides). Anything else is said so and skipped.
  const TEXT_EXT = [".md", ".markdown", ".txt", ".csv", ".json", ".jsonl", ".yaml", ".yml", ".toml"];
  function embraAcceptsFile(f) {
    const name = f.name.toLowerCase();
    return f.type.startsWith("image/") || f.type.startsWith("text/") ||
      f.type === "application/json" || f.type === "" || TEXT_EXT.some((e) => name.endsWith(e));
  }

  // Upload one or more files and inject `/attach <id|path>` per success:
  // an image answers with a media id, a text file with its uploads/ path.
  // Errors surface as a terminal line (bracketed, dim) so the operator
  // sees them where they are looking.
  window.embraUploadFiles = async function (files) {
    for (const f of Array.from(files)) {
      if (!embraAcceptsFile(f)) {
        if (term) term.write("\r\n\x1b[2m[embra-web] attach skipped: " + f.name + " is not an image or a text file\x1b[0m\r\n");
        continue;
      }
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
        const handle = body.id || body.path;
        if (!resp.ok || !handle) {
          const msg = body.error || ("upload rejected (" + resp.status + ")");
          if (term) term.write("\r\n\x1b[2m[embra-web] attach failed: " + msg + "\x1b[0m\r\n");
          continue;
        }
        if (writable) window.embraTermInject("/attach " + handle + "\r");
        else if (term) term.write("\r\n\x1b[2m[embra-web] uploaded " + handle + " — take control to attach it\x1b[0m\r\n");
      } catch (err) {
        if (term) term.write("\r\n\x1b[2m[embra-web] attach failed: " + err + "\x1b[0m\r\n");
      }
    }
  };

  // A backup's way back (`PUT /api/backups/<name>`, embra-web's backups.rs).
  // The shell's /backup form picks the file and names it; this streams it to
  // the machine and shows the progress and the answer on a card of the
  // page's own, beside the terminal and never in it: an upload runs for
  // minutes, the console stays usable meanwhile, and nothing is typed into
  // it. The machine is asked first whether the name and the size pass
  // (`…/upload-check`): over HTTP/1.1 a refusal sent while the body is still
  // on its way may never reach the page.
  let backupXhr = null, backupCard = null;
  const embraMb = (n) => (n / 1048576).toFixed(n < 10485760 ? 1 : 0) + " MB";

  function embraBackupCard() {
    if (backupCard) return backupCard;
    const el = (cls, tag) => { const e = document.createElement(tag || "div"); e.className = cls; return e; };
    const box = el("backup-card"), title = el("bc-title"), line = el("bc-line");
    const bar = el("bc-bar"), fill = el("bc-fill"), btn = el("bc-btn", "button");
    btn.type = "button";
    bar.appendChild(fill);
    box.appendChild(title); box.appendChild(line); box.appendChild(bar); box.appendChild(btn);
    document.body.appendChild(box);
    let act = null;
    btn.addEventListener("click", () => { if (act) act(); });
    backupCard = {
      open(name) {
        title.textContent = "Backup upload: " + name;
        box.classList.remove("ok", "bad");
        bar.style.display = "block"; fill.style.width = "0%";
        box.style.display = "flex";
      },
      say(text) { line.textContent = text; },
      progress(done, total) {
        fill.style.width = (total ? Math.floor(done * 100 / total) : 0) + "%";
        line.textContent = "Uploading: " + embraMb(done) + " of " + embraMb(total);
      },
      action(label, fn) { btn.textContent = label; act = fn; },
      end(text, ok) {
        line.textContent = text;
        bar.style.display = "none";
        box.classList.add(ok ? "ok" : "bad");
        btn.textContent = "Close";
        act = () => { box.style.display = "none"; };
      },
    };
    return backupCard;
  }

  // What the machine says of an upload of that size under that name:
  // { ok, status, error }.
  async function embraBackupAsk(route, bytes) {
    try {
      const resp = await fetch(route + "/upload-check?bytes=" + bytes, { cache: "no-store" });
      const body = await resp.json().catch(() => ({}));
      return { ok: resp.ok, status: resp.status, error: body.error || ("the machine answered " + resp.status) };
    } catch (err) {
      return { ok: false, status: 0, error: "the machine could not be asked (" + err + ")" };
    }
  }

  window.embraBackupUploadAllowed = function () {
    if (window.embraKiosk) {
      if (term) term.write("\r\n\x1b[2m[embra-web] a backup is uploaded from a remote browser: its /backup form, action upload\x1b[0m\r\n");
      return false;
    }
    return !backupXhr;
  };

  window.embraUploadBackup = async function (file, name) {
    if (backupXhr || !file) return;
    const card = embraBackupCard();
    const route = "/api/backups/" + encodeURIComponent(name);
    card.open(name);
    card.action("Close", () => {});
    card.say("Asking the machine whether " + embraMb(file.size) + " fits\u2026");
    const first = await embraBackupAsk(route, file.size);
    if (!first.ok) { card.end("Not uploaded: " + first.error, false); return; }
    const xhr = new XMLHttpRequest();
    backupXhr = xhr;
    let sent = false;
    // Leaving the page ends the upload: the browser asks first.
    const leave = (e) => { e.preventDefault(); e.returnValue = ""; };
    const over = (text, ok) => {
      backupXhr = null;
      window.removeEventListener("beforeunload", leave);
      card.end(text, ok);
    };
    xhr.open("PUT", route);
    xhr.responseType = "json";
    xhr.setRequestHeader("Content-Type", "application/gzip");
    xhr.upload.onprogress = (e) => { if (e.lengthComputable) card.progress(e.loaded, e.total); };
    xhr.upload.onload = () => {
      sent = true;
      card.say("Sent " + embraMb(file.size) + "; the machine is checking the archive\u2026");
    };
    xhr.onload = () => {
      const body = xhr.response || {};
      if (xhr.status === 201 && body.record) {
        const r = body.record;
        over("Checked and listed: " + r.files + " files, " + embraMb(r.bytes_uncompressed) + " unpacked" +
          (r.data_engine ? ", a " + r.data_engine + " database" : "") +
          ". Restore it with /backup restore " + name +
          " and /reboot now, or from the boot menu: Backup and restore, then Restore STATE and DATA.", true);
      } else {
        over("Not uploaded: " + (body.error || ("the machine answered " + xhr.status)), false);
      }
    };
    xhr.onerror = async () => {
      // No answer came. The machine is asked again, so that a refusal it
      // sent into the closing connection is not lost.
      const now = await embraBackupAsk(route, file.size);
      over("The connection ended before the machine's answer. " +
        (sent ? "The file had been sent whole: /backup list shows whether it was taken in." : "Nothing was kept.") +
        (now.ok ? "" : " Asked again, the machine says: " + now.error), false);
    };
    xhr.onabort = () => over("Upload cancelled. Nothing was kept.", false);
    card.action("Cancel", () => xhr.abort());
    window.addEventListener("beforeunload", leave);
    xhr.send(file);
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
