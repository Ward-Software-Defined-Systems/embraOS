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

  function sendResize() {
    if (writable && ws && ws.readyState === 1 && term) {
      ws.send(JSON.stringify({ t: "resize", cols: term.cols, rows: term.rows }));
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
    term = new Terminal({
      fontFamily: "'JetBrains Mono','Fira Code',ui-monospace,monospace",
      fontSize: 14, cursorBlink: true, scrollback: 4000,
      // Warm brand bg + amber (flame) cursor to blend with the shell.
      // The 16 ANSI colors are intentionally left at xterm defaults so
      // the real ratatui TUI's own colors render true (parity-safe).
      theme: { background: "#0c0907", foreground: "#ece2d8",
               cursor: "#ff7a1a", cursorAccent: "#0c0907",
               selectionBackground: "#3a2a18" },
    });
    fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(document.getElementById(elId));
    try { fit.fit(); } catch (e) {}
    term.onData((d) => {
      if (writable && ws && ws.readyState === 1) ws.send(new TextEncoder().encode(d));
    });
    const ro = new ResizeObserver(() => { try { fit.fit(); } catch (e) {} sendResize(); });
    ro.observe(document.getElementById(elId));
    window.addEventListener("resize", () => { try { fit.fit(); } catch (e) {} sendResize(); });
    connect();
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
  window.embraTermSetRoleCb = function (cb) {
    roleCb = cb;
    cb(lastRole, lastOwner); // prime
  };
})();
