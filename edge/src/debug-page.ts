/**
 * `/debug` — the session inspector: a static, dependency-pinned HTML shell for
 * attaching read-only to live SessionRoom DOs (workspace + chat docs) from a
 * browser. Built for one job: watching streaming output arrive and measuring
 * where the time goes (WS RTT to the DO's runtime, %LOR frame cadence against
 * the host's 120ms commit clock, entry createdAt→arrival deltas).
 *
 * Design constraints, deliberate:
 * - The shell is served UNauthenticated (like any login page): it contains no
 *   data and no secrets. Every data call requires the same bearer the native
 *   clients present; the token is pasted into the page and kept in
 *   localStorage, sent via `Authorization` on fetches and `?token=` on room
 *   sockets (the transport the edge already defines for WebSockets, where
 *   headers are impossible in browsers).
 * - loro-crdt (wasm) and loro-protocol load from jsdelivr, PINNED to the exact
 *   versions in edge/package.json (interop with the DO is version-sensitive).
 *   Serving them from the Worker would double-embed the ~3MB wasm the bundle
 *   already carries for the DO; a CDN pin is the right cost for a debug
 *   surface. Bump these alongside package.json.
 * - View-only by construction: the page never publishes %LOR or %EPH updates,
 *   so it can never corrupt a room and never appears as a participant.
 *   (Sending messages is a planned follow-up and will ride the same socket.)
 */

/** Keep in lockstep with package.json — the DO speaks these exact versions. */
const LORO_CRDT_VERSION = "1.13.7";
const LORO_PROTOCOL_VERSION = "0.3.0";

const CDN = "https://cdn.jsdelivr.net";

export const DEBUG_PAGE_CSP = [
  "default-src 'none'",
  // 'unsafe-inline' carries the app itself; wasm-unsafe-eval lets Chrome
  // compile the loro wasm module under CSP.
  `script-src 'unsafe-inline' 'wasm-unsafe-eval' ${CDN}`,
  // 'self' covers same-origin wss:// in CSP3 (fetches + room sockets).
  `connect-src 'self' ${CDN}`,
  "style-src 'unsafe-inline'",
  "img-src 'self' data:",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-ancestors 'none'"
].join("; ");

export const debugPageResponse = (): Response =>
  new Response(DEBUG_PAGE_HTML, {
    headers: {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      "x-robots-tag": "noindex, nofollow",
      "content-security-policy": DEBUG_PAGE_CSP
    }
  });

// The page script avoids backticks and ${…} so this outer template stays inert.
const DEBUG_PAGE_HTML = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>comet session inspector</title>
<style>
  :root {
    --bg: #0c0c0d; --panel: #131315; --panel2: #1a1a1d; --line: #26262a;
    --fg: #d6d6d9; --dim: #7c7c84; --faint: #4a4a52;
    --ok: #6fd18b; --warn: #e8c268; --bad: #e07a6b; --acc: #8fb8e8;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; background: var(--bg); color: var(--fg);
    font: 13px/1.45 ui-monospace, "Geist Mono", SFMono-Regular, Menlo, monospace;
  }
  header {
    display: flex; gap: 10px; align-items: center; flex-wrap: wrap;
    padding: 10px 14px; border-bottom: 1px solid var(--line);
    position: sticky; top: 0; background: var(--bg); z-index: 5;
  }
  header .brand { font-weight: 700; letter-spacing: .04em; }
  header .brand .dim { color: var(--dim); font-weight: 400; }
  input, button, select {
    background: var(--panel2); color: var(--fg); border: 1px solid var(--line);
    border-radius: 5px; padding: 5px 8px; font: inherit;
  }
  input:focus { outline: 1px solid var(--acc); }
  button { cursor: pointer; }
  button:hover { border-color: var(--dim); }
  button.primary { border-color: var(--acc); color: var(--acc); }
  #who { color: var(--dim); }
  #who .ok { color: var(--ok); }
  #who .bad { color: var(--bad); }
  main { display: grid; grid-template-columns: 340px 1fr; gap: 0; min-height: calc(100vh - 54px); }
  #left { border-right: 1px solid var(--line); padding: 10px; overflow-y: auto; max-height: calc(100vh - 54px); position: sticky; top: 54px; }
  #right { padding: 10px; display: grid; gap: 10px; align-content: start;
           grid-template-columns: repeat(auto-fit, minmax(430px, 1fr)); }
  h2 { font-size: 11px; text-transform: uppercase; letter-spacing: .09em; color: var(--dim); margin: 14px 0 6px; }
  .card { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; }
  .row { display: flex; gap: 8px; align-items: baseline; padding: 6px 8px; border-radius: 5px; }
  .row:hover { background: var(--panel2); }
  .row .title { flex: 1 1 auto; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .row .meta { color: var(--dim); font-size: 11px; white-space: nowrap; }
  .chip { font-size: 10px; padding: 1px 6px; border-radius: 8px; border: 1px solid var(--line); color: var(--dim); }
  .chip.working { color: var(--warn); border-color: var(--warn); }
  .chip.online { color: var(--ok); border-color: var(--ok); }
  .chip.streaming { color: var(--acc); border-color: var(--acc); }
  .chip.aborted { color: var(--bad); border-color: var(--bad); }
  .sess-head { display: flex; gap: 8px; align-items: center; padding: 8px 10px; border-bottom: 1px solid var(--line); }
  .sess-head .id { font-weight: 700; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .sess-head .state { font-size: 11px; color: var(--dim); margin-left: auto; white-space: nowrap; }
  .latency { display: grid; grid-template-columns: auto 1fr; gap: 3px 10px; padding: 8px 10px; border-bottom: 1px solid var(--line); font-size: 11px; }
  .latency .k { color: var(--dim); white-space: nowrap; }
  .latency .v { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }
  .latency canvas { background: var(--panel2); border-radius: 3px; }
  .num { color: var(--fg); } .num b { color: var(--acc); font-weight: 600; }
  .transcript { max-height: 46vh; overflow-y: auto; padding: 6px 10px; display: grid; gap: 6px; }
  .msg { border-left: 2px solid var(--line); padding: 2px 0 2px 8px; }
  .msg.user { border-left-color: var(--acc); }
  .msg.assistant { border-left-color: var(--faint); }
  .msg .head { color: var(--dim); font-size: 10px; display: flex; gap: 8px; }
  .msg pre { margin: 2px 0; white-space: pre-wrap; word-break: break-word; color: var(--fg); font: inherit; }
  .msg .tool { color: var(--warn); font-size: 11px; }
  .msg .err { color: var(--bad); }
  .foot { padding: 6px 10px; color: var(--faint); font-size: 10px; display: flex; gap: 12px; flex-wrap: wrap; }
  #log { color: var(--faint); font-size: 11px; padding: 8px; white-space: pre-wrap; }
  .empty { color: var(--faint); padding: 10px; }
  a { color: var(--acc); }
</style>
</head>
<body>
<header>
  <span class="brand">comet <span class="dim">/ session inspector</span></span>
  <input id="token" type="password" size="28" placeholder="bearer token (sc_rc_… | dev user)">
  <button id="connect" class="primary">connect</button>
  <span id="who">not connected</span>
</header>
<main>
  <div id="left">
    <h2>attach by id</h2>
    <div style="display:flex; gap:6px; flex-wrap:wrap;">
      <input id="attach-id" size="20" placeholder="sessionId">
      <input id="attach-dep" size="14" placeholder="deploymentId (opt)">
      <button id="attach-btn">attach</button>
    </div>
    <h2>devices</h2>
    <div id="devices" class="card"><div class="empty">workspace not joined</div></div>
    <h2>sessions</h2>
    <div id="chats" class="card"><div class="empty">workspace not joined</div></div>
    <h2>workspace room</h2>
    <div id="ws-room"></div>
    <div id="log"></div>
  </div>
  <div id="right"></div>
</main>
<script type="module">
"use strict";

// ── pinned modules (keep in lockstep with edge/package.json) ───────────────
const LORO_URL = "${CDN}/npm/loro-crdt@${LORO_CRDT_VERSION}/web/index.js";
const PROTO_URL = "${CDN}/npm/loro-protocol@${LORO_PROTOCOL_VERSION}/dist/index.js";

const logEl = document.getElementById("log");
const log = (s) => {
  logEl.textContent = (new Date()).toISOString().slice(11, 19) + " " + s + "\\n" + logEl.textContent.slice(0, 4000);
};

// Dynamic import (not static) on purpose: the specifiers are cross-origin CDN
// URLs in an unbundled inline script, and a load failure must surface in the
// page's own log element instead of killing the module before it can report.
let loro, proto;
try {
  loro = await import(LORO_URL);
  await loro.default();
  proto = await import(PROTO_URL);
} catch (e) {
  log("failed to load loro modules from CDN: " + e);
  throw e;
}
const { LoroDoc, EphemeralStore } = loro;
const { encode, decode, MessageType, CrdtType } = proto;

// ── helpers ────────────────────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const wsBase = location.origin.replace(/^http/, "ws");
const deviceId = "debug-" + Math.random().toString(36).slice(2, 10);
const fmtMs = (ms) => ms == null ? "–" : (ms < 1000 ? Math.round(ms) + "ms" : (ms / 1000).toFixed(1) + "s");
const fmtAge = (t) => {
  if (!t) return "–";
  const d = Date.now() - t;
  if (d < 60000) return Math.max(0, Math.round(d / 1000)) + "s ago";
  if (d < 3600000) return Math.round(d / 60000) + "m ago";
  if (d < 86400000) return Math.round(d / 3600000) + "h ago";
  return Math.round(d / 86400000) + "d ago";
};
const fmtBytes = (n) => n < 1024 ? n + "B" : n < 1048576 ? (n / 1024).toFixed(1) + "KB" : (n / 1048576).toFixed(1) + "MB";
const quantile = (arr, q) => {
  if (arr.length === 0) return null;
  const s = [...arr].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(q * s.length))];
};
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

let token = localStorage.getItem("comet-debug-token") || "";
let identity = null;

const authedFetch = (path) =>
  fetch(path, { headers: { authorization: "Bearer " + token } });

// ── room client (read-only loro-protocol peer) ─────────────────────────────
// Mirrors crates/sync/src/room.rs: join %LOR with our VV (empty ⇒ snapshot
// backfill), join %EPH, import DocUpdate frames, reassemble fragments,
// reconnect with backoff carrying the VV forward. Never sends updates.
class Room {
  constructor(opts) {
    this.path = opts.path;           // e.g. /session/abc/ws
    this.roomId = opts.roomId;       // protocol-frame room id (chatId / ws4/…)
    this.query = opts.query || "";   // extra query (deploymentId)
    this.onChange = opts.onChange;   // throttled by caller
    this.onState = opts.onState;
    this.doc = new LoroDoc();
    this.eph = new EphemeralStore(60000);
    this.state = "connecting";
    this.joined = { lor: false, eph: false };
    this.closeInfo = "";
    this.fragments = new Map();      // batchId → {parts, received, total}
    // metrics
    this.rtt = [];                   // {t, ms}
    this.frames = [];                // {t, dt, bytes, crdt, kind}
    this.e2e = [];                   // {t, ms} entry createdAt → arrival
    this.knownEntries = new Set();
    this.expectBackfill = true;
    this.lastLorFrameAt = null;
    this.joinSentAt = null;
    this.joinRtt = null;
    this.pingSentAt = null;
    this.closed = false;
    this.backoff = 1000;
    this.dial();
    this.pingTimer = setInterval(() => this.ping(), 2000);
  }

  url() {
    return wsBase + this.path + "?token=" + encodeURIComponent(token) +
      "&device=" + deviceId + this.query;
  }

  dial() {
    if (this.closed) return;
    this.setState("connecting");
    let ws;
    try { ws = new WebSocket(this.url()); } catch (e) { this.scheduleRedial("dial failed: " + e); return; }
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.backoff = 1000;
      this.joinSentAt = performance.now();
      // First %LOR batch after a (re)join is VV backfill, not live latency.
      this.expectBackfill = true;
      // Empty doc ⇒ ZERO-LENGTH version bytes (the Rust client's contract):
      // the DO snapshot-backfills on empty version but takes the update-export
      // path for any decodable VV — and an update export "from empty" on a
      // shallow-trimmed room silently omits the trimmed history.
      const vv = this.doc.version();
      let version;
      try {
        version = vv.toJSON().size === 0 ? new Uint8Array(0) : vv.encode();
      } finally { if (vv.free) vv.free(); }
      ws.send(encode({ type: MessageType.JoinRequest, crdt: CrdtType.Loro, roomId: this.roomId, auth: new Uint8Array(0), version }));
      ws.send(encode({ type: MessageType.JoinRequest, crdt: CrdtType.LoroEphemeralStore, roomId: this.roomId, auth: new Uint8Array(0), version: new Uint8Array(0) }));
    };
    ws.onmessage = (ev) => this.onMessage(ev);
    ws.onclose = (ev) => {
      this.joined = { lor: false, eph: false };
      this.fragments.clear();
      this.pingSentAt = null;
      this.scheduleRedial("closed " + ev.code + (ev.reason ? " " + ev.reason : ""));
    };
    ws.onerror = () => {};
  }

  scheduleRedial(why) {
    if (this.closed) return;
    this.closeInfo = why;
    this.setState("reconnecting");
    setTimeout(() => this.dial(), this.backoff);
    this.backoff = Math.min(this.backoff * 2, 30000);
  }

  setState(s) { this.state = s; if (this.onState) this.onState(); }

  ping() {
    if (!this.ws || this.ws.readyState !== 1 || this.pingSentAt != null) return;
    this.pingSentAt = performance.now();
    this.ws.send("ping");
  }

  onMessage(ev) {
    const now = performance.now();
    if (typeof ev.data === "string") {
      if (ev.data === "pong" && this.pingSentAt != null) {
        this.push(this.rtt, { t: Date.now(), ms: now - this.pingSentAt }, 200);
        this.pingSentAt = null;
        if (this.onState) this.onState();
      }
      return;
    }
    const bytes = new Uint8Array(ev.data);
    let m;
    try { m = decode(bytes); } catch (e) { log(this.roomId + ": undecodable frame (" + bytes.length + "B): " + e); return; }
    const dt = m.crdt === CrdtType.Loro && this.lastLorFrameAt != null ? now - this.lastLorFrameAt : null;
    if (m.type === MessageType.DocUpdate || m.type === MessageType.DocUpdateFragment) {
      this.push(this.frames, { t: Date.now(), dt, bytes: bytes.length, crdt: m.crdt, kind: m.type }, 400);
      if (m.crdt === CrdtType.Loro) this.lastLorFrameAt = now;
    }
    switch (m.type) {
      case MessageType.JoinResponseOk:
        if (m.crdt === CrdtType.Loro) { this.joined.lor = true; this.joinRtt = now - this.joinSentAt; this.setState("joined"); }
        if (m.crdt === CrdtType.LoroEphemeralStore) this.joined.eph = true;
        break;
      case MessageType.JoinError:
        this.closeInfo = "join error " + m.code + ": " + m.message;
        this.setState("join-error");
        log(this.roomId + " join error: " + m.message);
        break;
      case MessageType.DocUpdate:
        this.applyUpdates(m.crdt, m.updates);
        break;
      case MessageType.DocUpdateFragmentHeader:
        this.fragments.set(m.batchId, { crdt: m.crdt, parts: new Array(Number(m.fragmentCount)).fill(null), received: 0, total: Number(m.totalSizeBytes) });
        break;
      case MessageType.DocUpdateFragment: {
        const b = this.fragments.get(m.batchId);
        if (!b) break;
        b.parts[Number(m.index)] = m.fragment;
        b.received++;
        if (b.received === b.parts.length) {
          this.fragments.delete(m.batchId);
          const total = new Uint8Array(b.total);
          let off = 0;
          for (const p of b.parts) { total.set(p, off); off += p.length; }
          this.applyUpdates(b.crdt, [total]);
        }
        break;
      }
      case MessageType.RoomError:
        this.closeInfo = "room error " + m.code + ": " + m.message;
        this.setState("room-error");
        break;
      default: break; // Ack / Leave — not ours
    }
  }

  applyUpdates(crdt, updates) {
    if (crdt === CrdtType.Loro) {
      for (const u of updates) {
        if (u.length === 0) continue;
        try { this.doc.import(u); } catch (e) { log(this.roomId + ": import failed: " + e); }
      }
      this.sampleE2E();
    } else if (crdt === CrdtType.LoroEphemeralStore) {
      for (const u of updates) {
        if (u.length === 0) continue;
        try { this.eph.apply(u); } catch (e) { log(this.roomId + ": eph apply failed: " + e); }
      }
    }
    if (this.onChange) this.onChange();
  }

  // Entry createdAt → arrival. Host-clock based, so skew-sensitive; still the
  // only end-to-end number available without a protocol change, and exact
  // when both clocks are NTP-disciplined.
  sampleE2E() {
    const json = this.doc.toJSON();
    const messages = json && json.messages;
    if (!Array.isArray(messages)) return;
    const now = Date.now();
    const backfill = this.expectBackfill;
    this.expectBackfill = false;
    for (const m of messages) {
      if (!m || typeof m.id !== "string" || this.knownEntries.has(m.id)) continue;
      this.knownEntries.add(m.id);
      if (backfill) continue; // join backfill entries are historic, not latency
      if (typeof m.createdAt === "number" && now - m.createdAt < 120000) {
        this.push(this.e2e, { t: now, ms: Math.max(0, now - m.createdAt) }, 100);
      }
    }
  }

  push(arr, item, cap) { arr.push(item); if (arr.length > cap) arr.splice(0, arr.length - cap); }

  close() {
    this.closed = true;
    clearInterval(this.pingTimer);
    if (this.ws) { try { this.ws.close(1000); } catch (e) {} }
    if (this.doc.free) this.doc.free();
    if (this.eph.free) this.eph.free();
  }
}

// ── latency rendering ──────────────────────────────────────────────────────
const sparkline = (canvas, samples, pick, budgetMs) => {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 140, h = canvas.clientHeight || 26;
  if (canvas.width !== w * dpr) { canvas.width = w * dpr; canvas.height = h * dpr; }
  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  if (samples.length === 0) return;
  const vals = samples.map(pick);
  const max = Math.max(budgetMs || 0, ...vals) * 1.15;
  if (budgetMs) {
    ctx.strokeStyle = "#3a3a42";
    ctx.setLineDash([2, 3]);
    const y = h - (budgetMs / max) * h;
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
    ctx.setLineDash([]);
  }
  ctx.strokeStyle = "#8fb8e8";
  ctx.beginPath();
  const n = vals.length;
  for (let i = 0; i < n; i++) {
    const x = (i / Math.max(1, n - 1)) * (w - 2) + 1;
    const y = h - Math.min(1, vals[i] / max) * (h - 2) - 1;
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  }
  ctx.stroke();
};

// Frame timeline: last 30s, one bar per %LOR/%EPH frame, height = log(bytes).
const timeline = (canvas, frames) => {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 280, h = canvas.clientHeight || 26;
  if (canvas.width !== w * dpr) { canvas.width = w * dpr; canvas.height = h * dpr; }
  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const now = Date.now(), span = 30000;
  for (const f of frames) {
    const age = now - f.t;
    if (age > span) continue;
    const x = w - (age / span) * w;
    const hh = Math.max(2, Math.min(h, Math.log2(2 + f.bytes) * 2.2));
    ctx.fillStyle = f.crdt === "%LOR" ? "#8fb8e8" : "#5b8f6e";
    ctx.fillRect(x - 1, h - hh, 2, hh);
  }
};

// ── session cards ──────────────────────────────────────────────────────────
const cards = new Map(); // key → {room, el, canvases, renderScheduled}
window.__cards = cards;  // console/debugging escape hatch

// Trailing-edge throttle on a plain timer — rAF starves in background tabs,
// and an inspector left in one must keep rendering (100ms is plenty).
const scheduleRender = (card) => {
  if (card.renderScheduled) return;
  card.renderScheduled = true;
  setTimeout(() => { card.renderScheduled = false; renderCard(card); }, 100);
};

const partHtml = (p) => {
  if (!p || typeof p !== "object") return "";
  if (p.kind === "text") {
    const text = p.text || "";
    // Streaming tails matter most; keep renders cheap on multi-MB entries.
    const clipped = text.length > 4000 ? "… (" + (text.length - 4000) + " earlier chars)\\n" + text.slice(-4000) : text;
    return "<pre>" + esc(clipped) + "</pre>";
  }
  if (p.kind === "tool") {
    const call = p.call || {};
    return '<div class="tool">⚒ ' + esc(call.name || "tool") + (p.isError ? ' <span class="err">error</span>' : "") + "</div>";
  }
  if (p.kind === "input") return '<div class="tool">? input requested' + (p.resolved ? " (resolved)" : "") + "</div>";
  if (p.kind === "error") return '<div class="err">' + esc(p.message || "error") + "</div>";
  return "";
};

const renderLatency = (card) => {
  const r = card.room;
  const rttVals = r.rtt.map((s) => s.ms);
  const last = rttVals.length ? rttVals[rttVals.length - 1] : null;
  const lorFrames = r.frames.filter((f) => f.crdt === "%LOR");
  const dts = lorFrames.map((f) => f.dt).filter((v) => v != null && v < 10000);
  const recent = lorFrames.filter((f) => Date.now() - f.t < 10000);
  const bytesPerSec = recent.reduce((a, f) => a + f.bytes, 0) / 10;
  const e2eVals = r.e2e.map((s) => s.ms);
  card.el.querySelector(".v-rtt .num").innerHTML =
    "<b>" + fmtMs(last) + "</b> p50 " + fmtMs(quantile(rttVals, 0.5)) + " · p95 " + fmtMs(quantile(rttVals, 0.95)) +
    (r.joinRtt != null ? " · join " + fmtMs(r.joinRtt) : "");
  card.el.querySelector(".v-frames .num").innerHTML =
    "<b>" + lorFrames.length + "</b> frames · Δ p50 " + fmtMs(quantile(dts, 0.5)) + " · " + fmtBytes(Math.round(bytesPerSec)) + "/s";
  card.el.querySelector(".v-e2e .num").innerHTML = e2eVals.length
    ? "<b>" + fmtMs(e2eVals[e2eVals.length - 1]) + "</b> p50 " + fmtMs(quantile(e2eVals, 0.5)) + " (" + e2eVals.length + " entries)"
    : "no new entries yet";
  sparkline(card.canvases.rtt, r.rtt, (s) => s.ms, null);
  timeline(card.canvases.frames, r.frames);
  sparkline(card.canvases.e2e, r.e2e, (s) => s.ms, 120);
  card.el.querySelector(".state").textContent =
    r.state + (r.state === "joined" ? "" : r.closeInfo ? " · " + r.closeInfo : "");
};

const renderCard = (card) => {
  renderLatency(card);
  const r = card.room;
  if (card.kind === "workspace") { renderWorkspace(card); return; }
  const json = r.doc.toJSON();
  const meta = json.meta || {};
  const raw = Array.isArray(json.messages) ? json.messages : [];
  // stitch continuations onto their roots (mirror of session-doc/messages.ts)
  const roots = new Map();
  const entries = [];
  for (const m of raw) {
    if (!m || typeof m !== "object") continue;
    if (m.continuationOf && roots.has(m.continuationOf)) {
      const root = roots.get(m.continuationOf);
      root.parts = [...(root.parts || []), ...(m.parts || [])];
      continue;
    }
    const copy = { ...m, parts: [...(m.parts || [])] };
    roots.set(copy.id, copy);
    entries.push(copy);
  }
  card.el.querySelector(".id").textContent = (meta.title ? meta.title + " — " : "") + card.sessionId;
  const box = card.el.querySelector(".transcript");
  const atBottom = box.scrollTop + box.clientHeight >= box.scrollHeight - 40;
  const shown = entries.slice(-80);
  box.innerHTML = shown.length === 0
    ? '<div class="empty">no messages in doc</div>'
    : shown.map((m) => {
        const status = m.status && m.status !== "complete" ? ' <span class="chip ' + esc(m.status) + '">' + esc(m.status) + "</span>" : "";
        return '<div class="msg ' + esc(m.role || "") + '"><div class="head"><span>' + esc(m.role || "?") +
          "</span><span>" + esc(m.deviceId || "") + "</span><span>" + fmtAge(m.createdAt) + "</span>" + status +
          "</div>" + (m.parts || []).map(partHtml).join("") + "</div>";
      }).join("");
  if (atBottom) box.scrollTop = box.scrollHeight;
  const presence = Object.keys(r.eph.getAllStates()).length;
  card.el.querySelector(".foot-live").textContent =
    "entries " + entries.length + (entries.length !== shown.length ? " (showing " + shown.length + ")" : "") +
    " · presence keys " + presence;
};

const latencyBlock =
  '<div class="latency">' +
  '<span class="k">ws rtt</span><span class="v v-rtt"><canvas width="140" height="26" style="width:140px;height:26px"></canvas><span class="num">–</span></span>' +
  '<span class="k">%LOR frames</span><span class="v v-frames"><canvas width="280" height="26" style="width:280px;height:26px"></canvas><span class="num">–</span></span>' +
  '<span class="k">entry e2e</span><span class="v v-e2e"><canvas width="140" height="26" style="width:140px;height:26px"></canvas><span class="num">–</span></span>' +
  "</div>";

const attachSession = (sessionId, deploymentId) => {
  const key = "s:" + sessionId + ":" + (deploymentId || "");
  if (cards.has(key)) { cards.get(key).el.scrollIntoView({ behavior: "smooth" }); return; }
  const el = document.createElement("div");
  el.className = "card";
  el.innerHTML =
    '<div class="sess-head"><span class="id">' + esc(sessionId) + '</span>' +
    (deploymentId ? '<span class="chip">dep ' + esc(deploymentId) + "</span>" : "") +
    '<span class="state">connecting</span><button class="stats">stats</button><button class="detach">detach</button></div>' +
    latencyBlock +
    '<div class="transcript"><div class="empty">syncing…</div></div>' +
    '<div class="foot"><span class="foot-live"></span><span class="stats-out"></span></div>';
  $("right").appendChild(el);
  const card = {
    kind: "session", sessionId, deploymentId, el,
    canvases: { rtt: el.querySelector(".v-rtt canvas"), frames: el.querySelector(".v-frames canvas"), e2e: el.querySelector(".v-e2e canvas") }
  };
  card.room = new Room({
    path: "/session/" + encodeURIComponent(sessionId) + "/ws",
    roomId: sessionId,
    query: deploymentId ? "&deploymentId=" + encodeURIComponent(deploymentId) : "",
    onChange: () => scheduleRender(card),
    onState: () => scheduleRender(card)
  });
  el.querySelector(".stats").onclick = async () => {
    const out = el.querySelector(".stats-out");
    out.textContent = "…";
    try {
      const res = await authedFetch("/stats/" + encodeURIComponent(sessionId) + (deploymentId ? "?deploymentId=" + encodeURIComponent(deploymentId) : ""));
      const body = await res.json();
      out.textContent = res.ok
        ? "replay " + body.lastReplayMs + "ms/" + body.lastReplayRows + "rows · log " + fmtBytes(body.updateLogBytes) + " · snap " + fmtBytes(body.snapshotBytes) + " · sockets " + body.connectedSockets
        : res.status + " " + JSON.stringify(body);
    } catch (err) { out.textContent = String(err); }
  };
  el.querySelector(".detach").onclick = () => {
    card.room.close();
    cards.delete(key);
    el.remove();
  };
  cards.set(key, card);
  log("attached " + sessionId + (deploymentId ? " (deployment " + deploymentId + ")" : ""));
};

// ── workspace (session discovery) ──────────────────────────────────────────
let wsCard = null;

const renderWorkspace = (card) => {
  const json = card.room.doc.toJSON();
  const devices = json.devices || {};
  const chats = json.chats || {};
  const sessions = json.sessions || {};
  const presence = card.room.eph.getAllStates();
  const online = new Set();
  for (const k of Object.keys(presence)) {
    if (k.startsWith("presence/")) online.add(k.slice("presence/".length));
  }
  const devEl = $("devices");
  const devRows = Object.values(devices);
  devEl.innerHTML = devRows.length === 0 ? '<div class="empty">no devices</div>' :
    devRows.map((d) =>
      '<div class="row"><span class="title">' + esc(d.name || d.id) + "</span>" +
      (online.has(d.id) ? '<span class="chip online">online</span>' : '<span class="meta">' + fmtAge(d.lastSeenAt) + "</span>") +
      '<span class="meta">' + esc(d.platform || "") + "</span></div>"
    ).join("");
  const chatEl = $("chats");
  const rows = Object.values(chats)
    .filter((c) => c && !c.archived)
    .sort((a, b) => (b.lastMessageAt || b.createdAt || 0) - (a.lastMessageAt || a.createdAt || 0));
  chatEl.innerHTML = rows.length === 0 ? '<div class="empty">no sessions in workspace doc</div>' :
    rows.map((c) => {
      const s = sessions[c.id];
      const working = s && s.status === "working" && Date.now() - (s.updatedAt || 0) < 90000;
      const dev = devices[c.deviceId];
      return '<div class="row" data-chat="' + esc(c.id) + '" style="cursor:pointer" title="' + esc(c.id) + '">' +
        '<span class="title">' + esc(c.title || c.id) + "</span>" +
        (working ? '<span class="chip working">working</span>' : "") +
        '<span class="meta">' + esc(dev ? dev.name || dev.id : c.deviceId || "") + "</span>" +
        '<span class="meta">' + fmtAge(c.lastMessageAt || c.createdAt) + "</span></div>";
    }).join("");
  for (const row of chatEl.querySelectorAll("[data-chat]")) {
    row.onclick = () => attachSession(row.dataset.chat, $("attach-dep").value.trim() || undefined);
  }
};

const joinWorkspace = () => {
  if (wsCard) { wsCard.room.close(); wsCard.el.remove(); }
  const el = document.createElement("div");
  el.className = "card";
  el.innerHTML =
    '<div class="sess-head"><span class="id">ws4/' + esc(identity.projectScope) + '</span><span class="state">connecting</span></div>' +
    latencyBlock;
  $("ws-room").replaceChildren(el);
  wsCard = {
    kind: "workspace", el,
    canvases: { rtt: el.querySelector(".v-rtt canvas"), frames: el.querySelector(".v-frames canvas"), e2e: el.querySelector(".v-e2e canvas") }
  };
  wsCard.room = new Room({
    path: "/workspace/" + encodeURIComponent(identity.projectScope) + "/ws",
    roomId: "ws4/" + identity.projectScope,
    onChange: () => scheduleRender(wsCard),
    onState: () => scheduleRender(wsCard)
  });
};

// ── boot ───────────────────────────────────────────────────────────────────
const connect = async () => {
  token = $("token").value.trim();
  if (!token) { $("who").innerHTML = '<span class="bad">token required</span>'; return; }
  localStorage.setItem("comet-debug-token", token);
  $("who").textContent = "verifying…";
  let res;
  try { res = await authedFetch("/whoami"); } catch (e) { $("who").innerHTML = '<span class="bad">' + esc(String(e)) + "</span>"; return; }
  if (!res.ok) {
    $("who").innerHTML = '<span class="bad">auth failed (' + res.status + ")</span>";
    return;
  }
  identity = await res.json();
  $("who").innerHTML = '<span class="ok">' + esc(identity.userId) + "</span> @ " + esc(identity.projectScope) +
    " · " + esc(identity.environment) + " · " + esc(identity.credential);
  joinWorkspace();
};

$("connect").onclick = connect;
$("token").addEventListener("keydown", (e) => { if (e.key === "Enter") connect(); });
$("attach-btn").onclick = () => {
  const id = $("attach-id").value.trim();
  if (!id) return;
  if (!identity) { log("connect first"); return; }
  attachSession(id, $("attach-dep").value.trim() || undefined);
};
if (token) { $("token").value = token; connect(); }

// keep latency panels ticking even when no frames arrive
setInterval(() => {
  for (const card of cards.values()) renderLatency(card);
  if (wsCard) renderLatency(wsCard);
}, 1000);
</script>
</body>
</html>`;
