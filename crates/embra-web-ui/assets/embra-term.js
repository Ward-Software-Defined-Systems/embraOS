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
      term.write(`\r\n\x1b[90m[embra-web] connection lost — reconnecting in ${secs} s…\x1b[0m\r\n`);
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
    cur.innerHTML = '<svg width="18" height="26" viewBox="0 0 18 26"><path d="M1 1 L1 20 L6 15.5 L9.5 24 L12.5 22.5 L9 14 L16 14 Z" style="fill:var(--txt);stroke:var(--bg)" stroke-width="1.5" stroke-linejoin="round"/></svg>';
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
    kioskSheet = embraKioskSheet();
    // The launcher shows no dialog for a page's alert: the sheet says it.
    window.alert = function (message) { kioskSheet.open({ title: String(message) }); };
    // The launcher has no file picker and the display no files of its own:
    // a picker the page asks for is answered with where files come from.
    document.addEventListener("click", (e) => {
      if (e.target && e.target.matches && e.target.matches("input[type=file]")) {
        e.preventDefault(); e.stopImmediatePropagation();
        embraKioskNeedsBrowser("attach");
      }
    }, true);
    embraKioskMenu();
    embraKioskSelects();
  }

  // One panel over a dimmed page, for what the display console has to say:
  // this console's URL (the menu's last entry), an action that needs a
  // browser on another machine, and the page's alerts. It holds the
  // keyboard while it is up (Escape and Enter close it, nothing is typed
  // into the console or the form behind it) and gives the focus back to
  // where it was. The URL carries the access token: embra-web gives it to
  // the machine's own browser and to no other (`/api/console-url`), and it
  // is on the page only while the sheet is up.
  let kioskSheet = null;
  function embraKioskSheet() {
    const bg = document.createElement("div");
    bg.id = "embra-kiosk-sheet";
    bg.className = "kiosk-sheet-bg";
    const box = document.createElement("div");
    box.className = "kiosk-sheet";
    box.setAttribute("role", "dialog");
    bg.appendChild(box);
    document.body.appendChild(bg);
    let back = null;
    const el = (tag, cls, text) => {
      const n = document.createElement(tag);
      n.className = cls;
      if (text != null) n.textContent = text;
      return n;
    };
    const isOpen = () => bg.style.display === "flex";
    const close = () => {
      bg.style.display = "none";
      box.textContent = "";
      const to = back;
      back = null;
      if (to && to.isConnected && to !== document.body) {
        try { to.focus(); return; } catch (e) {}
      }
      if (term) try { term.focus(); } catch (e) {}
    };
    // { title, text: the paragraphs under it, urls: this console's URL after them }
    const open = async (what) => {
      if (!isOpen()) back = document.activeElement;
      box.textContent = "";
      box.appendChild(el("b", "ks-title", what.title));
      for (const line of (what.text || [])) box.appendChild(el("div", "ks-text", line));
      const rows = what.urls ? el("div", "ks-body", null) : null;
      if (rows) {
        box.appendChild(rows);
        box.appendChild(el("div", "ks-note", "The URL carries the console\u2019s access token: whoever has it has the console, until /web-token rotate makes a new one. The browser asks about the certificate once (embraOS-CA)."));
      }
      const bye = el("button", "ks-close", "Close");
      bye.type = "button";
      bye.addEventListener("click", close);
      box.appendChild(bye);
      bg.style.display = "flex";
      if (!rows) return;
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
    bg.addEventListener("mousedown", (e) => { if (e.target === bg) close(); });
    window.addEventListener("keydown", (e) => {
      if (!isOpen()) return;
      e.preventDefault(); e.stopImmediatePropagation();
      if (e.key === "Escape" || e.key === "Enter") close();
    }, true);
    return { open, close, isOpen };
  }

  // What the display console cannot finish and a browser on another machine
  // can: the machine's display has no file picker, no files of its own and
  // nowhere to save one. The sheet names the action, says what to do there,
  // and shows this console's URL, so that nothing the operator presses on
  // the display does nothing without a word.
  const KIOSK_NEEDS_BROWSER = {
    attach: ["Attach a file from a browser on another machine",
      "The display console has no file picker and no files of its own. Open this console in a browser on another machine and attach the file there: the Attach button, or a drop or a paste on its terminal.",
      "A file that is already in the workspace is attached from here: /attach and its path."],
    download: ["Download a file from a browser on another machine",
      "The display console has nowhere to save a file. Open this console in a browser on another machine and click the same row there."],
    backup: ["Upload a backup from a browser on another machine",
      "The display console has no file picker and no files of its own. Open this console in a browser on another machine and upload the backup there: the /backup form, action upload."],
    model: ["Upload a model's file from a browser on another machine",
      "The display console has no file picker and no files of its own. Open this console in a browser on another machine and upload the model's files there, one at a time: the /native form, action import."],
  };
  function embraKioskNeedsBrowser(kind, route) {
    const [title, ...text] = KIOSK_NEEDS_BROWSER[kind];
    if (route) text.push("The file: " + route);
    if (kioskSheet) kioskSheet.open({ title, text, urls: true });
  }

  // The display console's dropdown lists. The launcher draws the list of a
  // <select> on its Wayland platform alone: on the machine's display a press
  // on the box opened nothing, and the engine, which took its list for open
  // from then on, no longer wrote the choice into the box while the arrow
  // keys went on changing it. So the engine's own answer to that press (and
  // to Space and Enter on the box) never runs here, and the page draws the
  // list under the box, for the browser's own element: a pick sets its
  // value and fires the events a browser's pick fires. While the list is up
  // it holds the keyboard: the arrows, Home, End and a first letter move the
  // mark, Enter and Space pick, Escape closes and stops there (the form
  // stays, and in the console Esc is /stop). A remote browser keeps its own
  // list and runs none of this.
  function embraKioskSelects() {
    const list = document.createElement("div");
    list.id = "embra-kiosk-select";
    list.className = "kiosk-select";
    list.setAttribute("role", "listbox");
    document.body.appendChild(list);
    let owner = null, active = -1;
    const isOpen = () => list.style.display === "block";
    // A box with a list that drops down: not a list box, which the engine
    // draws inside the page.
    const plain = (el) => (el && el.matches && el.matches("select") && !el.multiple && el.size <= 1 ? el : null);
    const usable = (i) => !!owner && i >= 0 && i < owner.options.length && !owner.options[i].disabled;
    const mark = (i) => {
      active = i;
      Array.from(list.children).forEach((row, n) => row.classList.toggle("active", n === i));
      if (list.children[i]) list.children[i].scrollIntoView({ block: "nearest" });
    };
    // The next choice that can be picked, from `from` in steps of `by`.
    const step = (from, by) => {
      for (let i = from + by; owner && i >= 0 && i < owner.options.length; i += by) {
        if (usable(i)) return i;
      }
      return usable(from) ? from : active;
    };
    const close = () => {
      list.style.display = "none";
      list.textContent = "";
      owner = null; active = -1;
    };
    const open = (sel) => {
      close();
      owner = sel;
      Array.from(sel.options).forEach((o, i) => {
        const row = document.createElement("div");
        row.className = o.disabled ? "ks-opt off" : "ks-opt";
        row.setAttribute("role", "option");
        row.dataset.i = String(i);
        row.textContent = o.label;
        list.appendChild(row);
      });
      const r = sel.getBoundingClientRect();
      list.style.minWidth = r.width + "px";
      list.style.display = "block";
      // Under the box, or over it where the display ends first.
      const h = list.offsetHeight, w = list.offsetWidth;
      const y = r.bottom + 2 + h <= window.innerHeight ? r.bottom + 2 : Math.max(0, r.top - 2 - h);
      const x = Math.max(0, Math.min(r.left, window.innerWidth - w - 4));
      list.style.transform = "translate(" + x + "px," + y + "px)";
      mark(sel.selectedIndex);
    };
    const pick = (i) => {
      const sel = owner;
      close();
      if (!sel || !sel.isConnected) return;
      if (i >= 0 && i < sel.options.length && !sel.options[i].disabled && sel.selectedIndex !== i) {
        sel.selectedIndex = i;
        // The shell's fields are bound to these, as to a browser's own pick.
        sel.dispatchEvent(new Event("input", { bubbles: true }));
        sel.dispatchEvent(new Event("change", { bubbles: true }));
      }
      try { sel.focus(); } catch (e) {}
    };
    const rowOf = (e) => {
      const row = e.target && e.target.closest ? e.target.closest(".ks-opt") : null;
      return row && !row.classList.contains("off") ? Number(row.dataset.i) : -1;
    };

    // A press outside an open list closes it and does nothing else, as with
    // a browser's own list: the click that follows is not the page's (the
    // form behind the list stays).
    let dismissing = false;
    document.addEventListener("mousedown", (e) => {
      dismissing = false;
      if (isOpen() && e.button === 0 && list.contains(e.target)) {
        e.preventDefault(); // the box keeps the focus; the click picks
        return;
      }
      const was = owner;
      if (isOpen()) close();
      const sel = e.button === 0 && e.target && e.target.closest ? plain(e.target.closest("select")) : null;
      if (!sel) {
        if (was && e.button === 0) { dismissing = true; e.preventDefault(); }
        return;
      }
      e.preventDefault(); // the engine would ask the launcher for the list
      if (sel.disabled || sel === was) return;
      try { sel.focus(); } catch (err) {}
      open(sel);
    }, true);
    document.addEventListener("click", (e) => {
      if (!dismissing) return;
      dismissing = false;
      e.preventDefault(); e.stopImmediatePropagation();
    }, true);
    list.addEventListener("mousemove", (e) => {
      const i = rowOf(e);
      if (i >= 0 && i !== active) mark(i);
    });
    list.addEventListener("click", (e) => {
      const i = rowOf(e);
      if (i >= 0) pick(i);
    });
    window.addEventListener("keydown", (e) => {
      if (isOpen()) {
        if (!owner || !owner.isConnected || e.key === "Tab") { close(); return; }
        e.preventDefault(); e.stopImmediatePropagation();
        const k = e.key, n = owner.options.length;
        if (k === "Escape") close();
        else if (k === "Enter" || k === " ") pick(active);
        else if (k === "ArrowDown") mark(step(active, 1));
        else if (k === "ArrowUp") mark(step(active, -1));
        else if (k === "Home") mark(step(-1, 1));
        else if (k === "End") mark(step(n, -1));
        else if (k.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) {
          // A first letter: the next choice that starts with it, around the list.
          for (let d = 1; d <= n; d++) {
            const i = (Math.max(active, 0) + d) % n;
            if (usable(i) && owner.options[i].label.trim().toLowerCase().startsWith(k.toLowerCase())) { mark(i); break; }
          }
        }
        return;
      }
      const sel = plain(e.target);
      if (!sel || sel.disabled) return;
      if (e.key === " " || (e.altKey && (e.key === "ArrowDown" || e.key === "ArrowUp"))) {
        // A browser opens the list on these.
        e.preventDefault(); e.stopImmediatePropagation();
        open(sel);
      } else if (e.key === "Enter") {
        // The form's own answer to Enter stands; the engine's, its list, does not.
        e.preventDefault();
      }
    }, true);
    window.addEventListener("resize", () => { if (isOpen()) close(); });
    window.addEventListener("wheel", (e) => { if (isOpen() && !list.contains(e.target)) close(); }, { passive: true, capture: true });
    document.addEventListener("focusin", (e) => { if (isOpen() && e.target !== owner) close(); }, true);
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

    // This console's URL with the access token, for a browser on another
    // machine: the boot banner printed it once and the display console
    // covers that copy. The sheet shows it (`embraKioskSheet`).
    const sheetOpen = () => kioskSheet.isOpen();
    const showUrl = () => kioskSheet.open({ title: "This console from another machine", urls: true });

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
    // size, and Escape, which stops here (in the console Esc is /stop). The
    // sheet's keys are its own.
    document.addEventListener("mousedown", (e) => {
      if (open() && !menu.contains(e.target)) close();
    }, true);
    window.addEventListener("wheel", () => { if (open()) close(); }, { passive: true, capture: true });
    window.addEventListener("resize", () => { if (open()) close(); });
    window.addEventListener("keydown", (e) => {
      if (open() && e.key === "Escape") {
        e.preventDefault(); e.stopImmediatePropagation();
        close();
      }
    }, true);
  }

  // The terminal's colours, read from the theme's tokens on the page's root
  // (app.css: one block a theme). A token that is missing leaves xterm's
  // own colour in its place.
  const EMBRA_ANSI = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue", "brightMagenta", "brightCyan", "brightWhite"];
  function embraToken(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }
  // The activity strip draws on a canvas and asks for its colours here.
  window.embraThemeToken = embraToken;
  function embraTermTheme() {
    const theme = {};
    const put = (key, token) => { const v = embraToken(token); if (v) theme[key] = v; };
    put("background", "--bg"); put("foreground", "--txt");
    put("cursor", "--accent"); put("cursorAccent", "--bg");
    put("selectionBackground", "--term-selection");
    EMBRA_ANSI.forEach((key, i) => put(key, "--ansi-" + i));
    return theme;
  }
  function embraTermFloor() {
    const floor = parseFloat(embraToken("--term-floor"));
    return floor >= 1 && floor <= 21 ? floor : 4.5;
  }
  function embraLessMotion() {
    return !!(window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches);
  }
  // The page calls this when the theme on its root has changed: the
  // terminal takes the new colours where it stands, with nothing typed.
  window.embraTermApplyTheme = function () {
    if (!term) return;
    term.options.theme = embraTermTheme();
    term.options.minimumContrastRatio = embraTermFloor();
  };

  window.embraTermInit = function (elId) {
    if (term) return; // once
    // The display console (cog on the machine's own display, `?kiosk=1`):
    // the launcher draws no pointer on a display without a cursor plane,
    // so the page draws one, and the list of a dropdown with it; a download
    // has nowhere to go there and a file nowhere to come from, which the
    // page says with this console's URL; the page never navigates away; the
    // cursor does not blink (every blink is a repaint through software
    // rendering).
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
      // The cursor blinks neither on the display console nor for a reader
      // who asked the system for less motion.
      fontSize: termFs, cursorBlink: !kiosk && !embraLessMotion(), scrollback: 0,
      // The colours are the theme's (app.css), and so is the contrast every
      // character is held to against its own cell: a console program names
      // sixteen colours and knows nothing of the ground they land on (a dim
      // grey that is a bar's ground on one line is another line's text).
      theme: embraTermTheme(), minimumContrastRatio: embraTermFloor(),
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
    // On the display console a file has nowhere to go: the row the operator
    // pressed (`asked`) is answered on the sheet, with this console's URL;
    // an offer that arrives by itself is a line, which takes no key away.
    const embraDownload = (url, asked) => {
      if (kiosk) {
        if (asked) embraKioskNeedsBrowser("download", url);
        else term.write("\r\n\x1b[90m[embra-web] a file is downloaded from a browser on another machine (the right mouse button shows this console's URL): " + url + "\x1b[0m\r\n");
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
      // A download row: a file, a media item, a backup, or the CA
      // certificate a browser imports once (`/api/ca.crt`, from `/network`).
      const LINK = /\/api\/(?:(?:files|media|backups)\/[^\s]+|ca\.crt\b)/g;
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
              activate: (_e, u) => embraDownload(u, true),
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
  const TEXT_EXT = [".md", ".markdown", ".txt", ".csv", ".json", ".jsonl", ".yaml", ".yml", ".toml", ".pem", ".crt"];
  function embraAcceptsFile(f) {
    const name = f.name.toLowerCase();
    return f.type.startsWith("image/") || f.type.startsWith("text/") ||
      f.type === "application/json" || f.type === "" || TEXT_EXT.some((e) => name.endsWith(e));
  }

  // What an uploaded certificate file asks for: the `/ca import` of the
  // file where it landed, which shows the certificate and writes nothing.
  // Null for any other file. The rule of `app::certificate_import_command`.
  function embraCertificateCommand(name, path) {
    const lower = name.toLowerCase();
    if (!(lower.endsWith(".pem") || lower.endsWith(".crt")) || !path) return null;
    return "/ca import " + path.replace(/^\/embra\/workspace\//, "");
  }

  // Upload one or more files and inject `/attach <id|path>` per success:
  // an image answers with a media id, a text file with its uploads/ path.
  // A certificate file is not attached: its `/ca import` is typed into the
  // input and no Enter is sent. At a setup question that is a list of
  // choices, Enter answers the list whatever was typed, so the command
  // waits in the input for the operator, and for a question that takes
  // text. Errors surface as a terminal line (bracketed, dim) so the
  // operator sees them where they are looking.
  window.embraUploadFiles = async function (files) {
    for (const f of Array.from(files)) {
      if (!embraAcceptsFile(f)) {
        if (term) term.write("\r\n\x1b[90m[embra-web] attach skipped: " + f.name + " is not an image or a text file\x1b[0m\r\n");
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
          let msg = body.error || ("upload rejected (" + resp.status + ")");
          // A certificate in DER is not text, and the upload takes text.
          if (embraCertificateCommand(f.name, "x") && /not UTF-8 text/.test(msg)) {
            msg += ": a certificate in DER is not text. Convert it (openssl x509 -inform der -in <file> -out <file>.pem), or give /ca import a URL";
          }
          if (term) term.write("\r\n\x1b[90m[embra-web] attach failed: " + msg + "\x1b[0m\r\n");
          continue;
        }
        const certificate = embraCertificateCommand(f.name, body.path);
        if (certificate) {
          if (writable) window.embraTermInject(certificate);
          else if (term) term.write("\r\n\x1b[90m[embra-web] uploaded " + handle + " — take control and run " + certificate + "\x1b[0m\r\n");
          continue;
        }
        if (writable) window.embraTermInject("/attach " + handle + "\r");
        else if (term) term.write("\r\n\x1b[90m[embra-web] uploaded " + handle + " — take control to attach it\x1b[0m\r\n");
      } catch (err) {
        if (term) term.write("\r\n\x1b[90m[embra-web] attach failed: " + err + "\x1b[0m\r\n");
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
      open(name, what) {
        title.textContent = (what || "Backup upload") + ": " + name;
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
      embraKioskNeedsBrowser("backup");
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

  // One of a native model's two files (experimental; `PUT /api/models/<id>`,
  // embra-web's models.rs), for a machine that cannot reach the catalogue's
  // source: the model's file, or what it reads images through. The shell's
  // /native form picks the file; the machine's catalogue says which model's
  // which file it is, by its size (no two of the catalogue's files have
  // one), or else by a name one file alone has, and the same card shows the
  // progress and the answer. One upload at a time, a backup's included.
  window.embraModelUploadAllowed = function () {
    if (window.embraKiosk) {
      embraKioskNeedsBrowser("model");
      return false;
    }
    return !backupXhr;
  };

  window.embraUploadModel = async function (file) {
    if (backupXhr || !file) return;
    const card = embraBackupCard();
    card.open(file.name, "Model upload (experimental)");
    card.action("Close", () => {});
    card.say("Asking the machine which model this file is\u2026");
    let models = [];
    try {
      const resp = await fetch("/api/models", { cache: "no-store" });
      models = ((await resp.json()) || {}).models || [];
    } catch (err) {
      card.end("Not uploaded: the machine's catalogue could not be read (" + err + ")", false);
      return;
    }
    const files = [];
    for (const m of models) {
      files.push({ id: m.id, images: false, file: m.file, bytes: m.bytes });
      if (m.images) files.push({ id: m.id, images: true, file: m.images.file, bytes: m.images.bytes });
    }
    // The size first: the machine decides by it. A name that one file alone
    // has finds a file of another size, which the machine then refuses with
    // the sizes it takes.
    const named = files.filter((f) => f.file === file.name);
    const model = files.find((f) => f.bytes === file.size) || (named.length === 1 ? named[0] : null);
    if (!model) {
      card.end("Not uploaded: " + file.name + " (" + file.size + " bytes) is no file of the catalogue. The catalogue: " +
        files.map((f) => f.id + (f.images ? ", its image file: " : ": ") + f.file + " (" + f.bytes + " bytes)").join("; ") + ".", false);
      return;
    }
    const route = "/api/models/" + encodeURIComponent(model.id);
    card.open(model.id + (model.images ? ", its image file" : ""), "Model upload (experimental)");
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
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.upload.onprogress = (e) => { if (e.lengthComputable) card.progress(e.loaded, e.total); };
    xhr.upload.onload = () => {
      sent = true;
      card.say("Sent " + embraMb(file.size) + "; the machine is finishing its check against the catalogue\u2026");
    };
    xhr.onload = () => {
      const body = xhr.response || {};
      if (xhr.status === 201) {
        over(model.images
          ? "Checked against the catalogue and installed: the image file of " + model.id + ". With it the model reads images; when that model is loaded, the model server starts again with the file, and a call that runs on it is cut."
          : "Checked against the catalogue and installed: " + model.id + ". Load it with /native use " + model.id + ".", true);
      } else {
        over("Not uploaded: " + (body.error || ("the machine answered " + xhr.status)), false);
      }
    };
    xhr.onerror = async () => {
      // No answer came. The machine is asked again, so that a refusal it
      // sent into the closing connection is not lost.
      const now = await embraBackupAsk(route, file.size);
      over("The connection ended before the machine's answer. " +
        (sent ? "The file had been sent whole: /native shows whether it was installed." : "Nothing was kept.") +
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
