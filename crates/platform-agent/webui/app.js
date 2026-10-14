"use strict";

// Lanthorn Host: the node agent UI. Vanilla ES2020. No frameworks, no imports.
// Talks to the same-origin API. Polls every 5s, pausing while a mutating request is in flight.

const TOKEN_KEY = "agent_token";
const POLL_MS = 5000;
// Resource usage is read faster than the list: it is what someone watches
// while a server is busy. Two minutes of history at this rate.
const USAGE_MS = 2000;
const USAGE_HISTORY = 60;
// Below this much free space where game files live, the page warns before a
// download. A Source game's install is several GB; a GoldSrc one about one.
const LOW_DISK_BYTES = 10 * 1024 ** 3;
// The log view: how often it re-reads while open, and how much it may ask for.
const LOG_REFRESH_MS = 2000;
const LOG_LINE_CHOICES = [200, 1000, 2000];
// Kept in step with MAX_CRASH_RESTARTS_PER_HOUR in agent.rs.
const MAX_CRASH_RESTARTS = 5;
const PLAYERS_REFRESH_MS = 5000;
// The Installed page re-reads this often while it is open, like /usage.
const INSTALLED_MS = 2000;

// The map-name alphabet, kept in step with `validate_map_name` in
// crates/game-bridge/src/console.rs. The node is the authority — this only
// saves a round trip and gives a better sentence than a 400 would.
const MAP_NAME_RE = /^[A-Za-z0-9_.\/-]+$/;

const state = {
  token: null,
  capacity: null,        // {max_instances, running, port_range_start, port_range_end}
  games: [],             // array of game defs
  instances: [],         // array of instance objects
  interfaces: [],        // array of live uplink interface objects
  maps: new Map(),       // game_id -> [map names] this node has installed
  rows: new Map(),       // instance_id -> row element
  inFlight: 0,           // mutating requests in flight; polling pauses while > 0
  pollTimer: null,
  toastTimer: null,
  activeTab: "servers",
  installing: new Map(), // game_id -> true (install in flight)
  installingMsg: new Map(), // game_id -> string message (post-completion)
  installDone: new Map(), // game_id -> "ok" | "error" marker
  usage: {
    timer: null,
    supported: true,     // false once an older agent answers /usage with 404
    prev: new Map(),     // instance_id -> last raw sample (Docker's counters)
    now: new Map(),      // instance_id -> {cpu, mem, memLimit, rx, tx, read, write, pids} per second
    hist: new Map(),     // instance_id -> {cpu: [], mem: [], net: []}
    players: new Map(),  // instance_id -> players_now per list poll (5 s)
  },
  storage: null,         // {path, free_bytes, total_bytes}, or null on an older agent
  logTimer: null,
  playersTimer: null,
  orphans: null,         // {count, bytes, skipped_recent} from /orphans/summary
  orphansCheckedAt: 0,
  installed: {
    timer: null,
    busy: false,
    supported: true,     // false once an older agent answers /content with 404/405
    copies: [],          // InstalledCopy rows (agent.rs)
    downloads: [],       // [{game, status}] running now
    prev: new Map(),     // game_id -> {bytes, at}: the last reading, for a rate
    rate: new Map(),     // game_id -> bytes per second
    deleting: new Set(), // "game/version" being deleted
    art: new Map(),      // row key -> its art element, kept across polls
  },
  // GET /updates (updates.rs): settings, the reserve, and each game's notice.
  updates: {
    supported: true,     // false once an older agent answers 404
    settings: null,      // {check_every_hours, reserve_bytes}
    reserve: null,       // bytes downloads keep free on this disk
    defaultReserve: null,
    games: new Map(),    // game_id -> UpdateNotice
    checking: new Set(), // game ids with a check running from this page
    moves: [],           // MoveRecord: servers waiting for, or done with, a move onto a new copy
    moving: new Set(),   // instance ids with a Move now running from this page
    timer: null,
  },
};

const ICON = {
  play: '<path d="M7 5l12 7-12 7z"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="2"/>',
  restart: '<path d="M20 11a8 8 0 10-2.3 5.7M20 4v7h-7"/>',
  map: '<path d="M9 4L3 6v14l6-2 6 2 6-2V4l-6 2-6-2zM9 4v14M15 6v14"/>',
  bot: '<rect x="5" y="8" width="14" height="11" rx="3"/><path d="M12 4v4M9 13h.01M15 13h.01"/>',
  megaphone: '<path d="M4 10v4h3l5 4V6L7 10H4z"/><path d="M16 9a4 4 0 010 6"/>',
  copy: '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 00-2-2H6a2 2 0 00-2 2v8a2 2 0 002 2h2"/>',
  trash: '<path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/>',
  more: '<circle cx="5" cy="12" r="1.3"/><circle cx="12" cy="12" r="1.3"/><circle cx="19" cy="12" r="1.3"/>',
  download: '<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>',
  edit: '<path d="M4 20h4L19 9l-4-4L4 16v4z"/>',
  link: '<path d="M10 14a4 4 0 005.7 0l3-3a4 4 0 00-5.7-5.7l-1 1"/><path d="M14 10a4 4 0 00-5.7 0l-3 3a4 4 0 005.7 5.7l1-1"/>',
  log: '<rect x="4" y="3" width="16" height="18" rx="2"/><path d="M8 8h8M8 12h8M8 16h5"/>',
  users: '<circle cx="9" cy="8" r="3.2"/><path d="M3 20c0-3.3 2.7-6 6-6s6 2.7 6 6"/><path d="M16 4.5a3.2 3.2 0 010 6.2M18 14.5c1.8.8 3 2.7 3 5"/>',
};
function icon(name) {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("aria-hidden", "true");
  s.innerHTML = ICON[name] || "";
  return s;
}

// ---------- small DOM helpers ----------

function $(id) { return document.getElementById(id); }
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = text;
  return e;
}
function gameOf(id) { return state.games.find(g => g.id === id) || null; }
function gameName(id) { const g = gameOf(id); return g ? g.display_name : id; }

// ---------- game artwork ----------
// The pack's Steam header where it names a Steam app, over a lettered tile in a
// colour derived from the game id — which is what an offline node shows.
function hueOf(id) {
  let h = 0;
  for (const c of String(id || "?")) h = (h * 31 + c.charCodeAt(0)) % 360;
  return h;
}
function initials(name) {
  const words = String(name || "?").replace(/[^A-Za-z0-9 ]/g, " ").split(/\s+/).filter(Boolean);
  if (!words.length) return "?";
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return (words[0][0] + words[1][0]).toUpperCase();
}
// Pictures that have loaded once. A list rebuilt on a poll makes new <img>
// elements, and each would start transparent and fade in again: a blink every
// poll. One already in the browser's cache is shown at once instead.
const ART_LOADED = new Set();
function art(gameId, cls) {
  const box = el("div", "art " + (cls || ""));
  box.style.setProperty("--h", String(hueOf(gameId)));
  const g = gameOf(gameId);
  box.appendChild(el("span", "art-letters", initials(g ? g.display_name : gameId)));
  // A Steam header, or a portrait box cover shown whole over a blurred copy.
  const url = g && (g.art_url || (g.steam_app_id
    ? "https://cdn.cloudflare.steamstatic.com/steam/apps/" + g.steam_app_id + "/header.jpg" : null));
  if (url) {
    const bg = el("img", "art-bg");
    const fg = el("img", "art-fg");
    for (const img of [bg, fg]) {
      img.alt = "";
      img.loading = "lazy";
      if (ART_LOADED.has(url)) img.classList.add("loaded");
      img.onload = () => { ART_LOADED.add(url); img.classList.add("loaded"); };
      img.onerror = () => { bg.remove(); fg.remove(); };
      img.src = url;
      box.appendChild(img);
    }
  }
  return box;
}

// ---------- token ----------

function getToken() {
  try { return localStorage.getItem(TOKEN_KEY); } catch (_) { return null; }
}
function setToken(t) {
  try {
    if (t === null) localStorage.removeItem(TOKEN_KEY);
    else localStorage.setItem(TOKEN_KEY, t);
  } catch (_) { /* a private window keeps it for this page only */ }
  state.token = t;
}

// ---------- api ----------

async function api(method, path, body) {
  const opts = { method, headers: { "Authorization": "Bearer " + (state.token || "") } };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  let resp;
  try {
    resp = await fetch(path, opts);
  } catch (e) {
    throw { __network: true, error: "Could not reach the node. " + (e && e.message ? e.message : "") };
  }
  if (resp.status === 401) {
    setToken(null);
    showTokenScreen();
    throw { __auth: true, error: "The token is wrong or no longer valid." };
  }
  let data = null;
  const ct = resp.headers.get("content-type") || "";
  if (ct.includes("application/json")) {
    try { data = await resp.json(); } catch (_) { data = null; }
  }
  if (!resp.ok) {
    const msg = (data && typeof data.error === "string") ? data.error
      : ("Request failed (" + resp.status + " " + resp.statusText + ").");
    throw { error: msg, status: resp.status, data };
  }
  return data;
}

// The same as api(), for an endpoint that answers plain text (a server log).
async function apiText(path) {
  let resp;
  try {
    resp = await fetch(path, { headers: { "Authorization": "Bearer " + (state.token || "") } });
  } catch (e) {
    throw { __network: true, error: "Could not reach the node. " + (e && e.message ? e.message : "") };
  }
  if (resp.status === 401) {
    setToken(null);
    showTokenScreen();
    throw { __auth: true, error: "The token is wrong or no longer valid." };
  }
  if (!resp.ok) {
    let msg = "Request failed (" + resp.status + ").";
    try { const d = await resp.json(); if (d && typeof d.error === "string") msg = d.error; } catch (_) {}
    throw { error: msg, status: resp.status };
  }
  return resp.text();
}

function withInFlight(promise) {
  // Mutating requests wrap their fetch with this so polling pauses.
  state.inFlight++;
  return promise.finally(() => {
    state.inFlight--;
    if (state.inFlight === 0 && state.token) maybeSchedulePoll();
  });
}

// Copy a value, on plain HTTP too: the clipboard API needs a secure origin and
// this UI is usually served over a LAN, so the textarea fallback is the one
// that usually runs.
async function copyValue(value) {
  let ok = false;
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(value);
      ok = true;
    }
  } catch (_) { /* fall through */ }
  if (!ok) {
    try {
      const ta = document.createElement("textarea");
      ta.value = value;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      ok = document.execCommand("copy");
      document.body.removeChild(ta);
    } catch (_) { ok = false; }
  }
  if (ok) toast("Copied.");
  else showError("Could not copy. The value is: " + value);
  return ok;
}

// A truncated hash that hands over the whole value on click.
function makeCopyable(node, value) {
  if (!node || !value) return;
  node.classList.add("copyable");
  node.title = value + " — click to copy";
  node.setAttribute("role", "button");
  node.setAttribute("tabindex", "0");
  node.onclick = () => copyValue(value);
  node.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); copyValue(value); } };
}

// ---------- banners ----------

function showError(sentence) {
  $("error-text").textContent = sentence;
  $("error-banner").classList.remove("hidden");
}
function clearError() { $("error-banner").classList.add("hidden"); }
function toast(msg) {
  const box = $("toast");
  box.textContent = "";
  box.appendChild(el("span", null, msg));
  box.classList.remove("hidden");
  clearTimeout(state.toastTimer);
  state.toastTimer = setTimeout(() => box.classList.add("hidden"), 4000);
}

// ---------- screen switching ----------

function showTokenScreen() {
  stopPolling();
  $("token-screen").classList.remove("hidden");
  $("main-ui").classList.add("hidden");
  $("token-input").value = "";
  $("token-input").focus();
}
function showMainUI() {
  $("token-screen").classList.add("hidden");
  $("main-ui").classList.remove("hidden");
  startPolling();
  renderAll();
}

// ---------- polling ----------

function startPolling() {
  stopPolling();
  maybeSchedulePoll();
  poll();
  pollUsage();
  loadUpdates();
}
function stopPolling() {
  if (state.pollTimer) { clearTimeout(state.pollTimer); state.pollTimer = null; }
  if (state.usage.timer) { clearTimeout(state.usage.timer); state.usage.timer = null; }
  if (state.updates.timer) { clearTimeout(state.updates.timer); state.updates.timer = null; }
}
function maybeSchedulePoll() {
  if (state.pollTimer) return;
  if (state.inFlight > 0) return; // rescheduled when inFlight drains
  state.pollTimer = setTimeout(() => { state.pollTimer = null; poll(); }, POLL_MS);
}
async function poll() {
  if (!state.token) return;
  if (state.inFlight > 0) { maybeSchedulePoll(); return; }
  try {
    const [cap, insts, storage] = await Promise.all([
      api("GET", "/capacity"), api("GET", "/instances"),
      // An agent older than this page has no /storage; the line just stays hidden.
      api("GET", "/storage").catch(e => { if (e && e.__auth) throw e; return null; }),
    ]);
    state.capacity = cap;
    state.instances = Array.isArray(insts) ? insts : [];
    state.storage = storage;
    recordPlayers();
    renderStatusPill();
    renderStorage();
    refreshOrphans(false);
    renderInstances();
  } catch (e) {
    if (e && e.__auth) return;
    if (e && e.__network) showError(e.error);
  } finally {
    if (state.token) maybeSchedulePoll();
  }
}

// ---------- disk space ----------

function renderStorage() {
  const line = $("disk-line");
  if (!line) return;
  const s = state.storage;
  line.classList.toggle("hidden", !s);
  if (!s) return;
  const low = s.free_bytes < LOW_DISK_BYTES;
  line.classList.toggle("low", low);
  $("disk-text").textContent = formatBytes(s.free_bytes) + " free";
  line.title = (low ? "Low on space. A game download may not fit. " : "") +
    formatBytes(s.free_bytes) + " free of " + formatBytes(s.total_bytes) + " where game files are kept (" + s.path + ").";
}

// Asked before anything that may download a game. True when there is room,
// when the node cannot say, or when the operator goes ahead anyway.
function roomForDownload() {
  const s = state.storage;
  if (!s || s.free_bytes >= LOW_DISK_BYTES) return true;
  return confirm("Only " + formatBytes(s.free_bytes) + " is free on this node's disk.\n\n" +
    "A game's files can take several GB, and a download that runs out of space fails partway. Go ahead anyway?");
}

// ---------- live usage ----------

// Docker's counters only grow, so a rate is the change between two readings
// (`InstanceUsage` in instance.rs). Read only while someone can see the
// servers list: a hidden tab or the Games page asks the node for nothing.
async function pollUsage() {
  const u = state.usage;
  if (u.timer) { clearTimeout(u.timer); u.timer = null; }
  // One read at a time: a tab switch during a read must not start a second
  // loop beside the first. The read in flight schedules the next.
  if (u.busy || !state.token || !u.supported) return;
  if (state.activeTab === "servers" && !document.hidden) {
    u.busy = true;
    try {
      ingestUsage(await api("GET", "/usage"));
    } catch (e) {
      if (e && e.__auth) return;
      // An agent older than this page has no /usage: hide the strips, once.
      if (e && e.status === 404) { u.supported = false; renderUsage(); return; }
    } finally {
      u.busy = false;
    }
  }
  if (state.token && !u.timer) u.timer = setTimeout(pollUsage, USAGE_MS);
}

function ingestUsage(samples) {
  const u = state.usage;
  const seen = new Set();
  for (const s of Array.isArray(samples) ? samples : []) {
    const id = s.instance_id;
    seen.add(id);
    const p = u.prev.get(id);
    u.prev.set(id, s);
    const dt = p ? (s.sampled_at_ms - p.sampled_at_ms) / 1000 : 0;
    // A counter that went backwards is a restarted container: start over
    // rather than draw a negative rate.
    if (!p || dt <= 0 || s.cpu_total_ns < p.cpu_total_ns || s.net_rx_bytes < p.net_rx_bytes) {
      u.now.set(id, { warming: true, mem: s.memory_bytes, memLimit: s.memory_limit_bytes, pids: s.pids });
      continue;
    }
    const dCpu = s.cpu_total_ns - p.cpu_total_ns;
    const dSys = (s.system_cpu_ns != null && p.system_cpu_ns != null) ? s.system_cpu_ns - p.system_cpu_ns : 0;
    const cpus = s.online_cpus || 1;
    // `docker stats`' formula, so the two agree: 100% is one whole core.
    // Without the machine's counter, wall-clock time stands in for it.
    const cpu = dSys > 0 ? (dCpu / dSys) * cpus * 100 : dCpu / (dt * 1e9) * 100;
    const now = {
      cpu, cpus,
      mem: s.memory_bytes, memLimit: s.memory_limit_bytes,
      rx: (s.net_rx_bytes - p.net_rx_bytes) / dt,
      tx: (s.net_tx_bytes - p.net_tx_bytes) / dt,
      read: Math.max(0, s.disk_read_bytes - p.disk_read_bytes) / dt,
      write: Math.max(0, s.disk_write_bytes - p.disk_write_bytes) / dt,
      pids: s.pids,
    };
    u.now.set(id, now);
    let h = u.hist.get(id);
    if (!h) { h = { cpu: [], mem: [], net: [] }; u.hist.set(id, h); }
    h.cpu.push(cpu);
    h.mem.push(now.mem || 0);
    h.net.push(now.rx + now.tx);
    for (const k of ["cpu", "mem", "net"]) if (h[k].length > USAGE_HISTORY) h[k].shift();
  }
  // A server that stopped has no usage; keeping its old numbers would show a
  // stopped server as busy.
  for (const map of [u.prev, u.now, u.hist]) for (const id of [...map.keys()]) if (!seen.has(id)) map.delete(id);
  renderUsage();
}

// Players come from the list's own poll (every 5 s), which already asks each
// game. "Could not ask" is a gap in the line, never a zero.
function recordPlayers() {
  const hist = state.usage.players;
  const seen = new Set();
  for (const inst of state.instances) {
    if (inst.state !== "running") continue;
    seen.add(inst.instance_id);
    let h = hist.get(inst.instance_id);
    if (!h) { h = []; hist.set(inst.instance_id, h); }
    h.push(inst.players_now == null ? null : inst.players_now);
    if (h.length > USAGE_HISTORY) h.shift();
  }
  for (const id of [...hist.keys()]) if (!seen.has(id)) hist.delete(id);
}

function formatRate(bytesPerSec) {
  if (bytesPerSec == null || !isFinite(bytesPerSec)) return "—";
  return formatBytes(Math.round(bytesPerSec)) + "/s";
}

// A sparkline as an SVG path. `floor` keeps an idle server's noise flat
// instead of stretched to the full height.
const SPARK_W = 120, SPARK_H = 26;
function sparkPath(values, floor) {
  if (!values || values.length < 2) return "";
  const max = Math.max(floor, ...values.filter(v => v != null));
  const step = SPARK_W / (USAGE_HISTORY - 1);
  const x0 = SPARK_W - step * (values.length - 1);
  // A null is a reading that could not be taken: the line breaks there
  // rather than dropping to zero.
  let out = "", pen = false;
  values.forEach((v, i) => {
    if (v == null) { pen = false; return; }
    out += (pen ? "L" : "M") + (x0 + i * step).toFixed(1) + " " + (SPARK_H - 1 - (v / max) * (SPARK_H - 2)).toFixed(1) + " ";
    pen = true;
  });
  return out.trim();
}

function buildUsageStrip() {
  const strip = el("div", "usage hidden");
  const metric = (key, label) => {
    const m = el("div", "metric metric-" + key);
    const top = el("div", "metric-top");
    top.append(el("span", "metric-label", label), el("span", "metric-value"));
    m.appendChild(top);
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("viewBox", "0 0 " + SPARK_W + " " + SPARK_H);
    svg.setAttribute("preserveAspectRatio", "none");
    svg.setAttribute("aria-hidden", "true");
    svg.classList.add("spark");
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    svg.appendChild(path);
    m.appendChild(svg);
    m.appendChild(el("div", "metric-sub"));
    return m;
  };
  strip.append(metric("players", "Players"), metric("cpu", "CPU"), metric("mem", "Memory"),
    metric("net", "Network"), metric("disk", "Disk"));
  return strip;
}

function setMetric(strip, key, value, sub, path, title) {
  const m = strip.querySelector(".metric-" + key);
  m.querySelector(".metric-value").textContent = value;
  m.querySelector(".metric-sub").textContent = sub || "";
  const p = m.querySelector(".spark path");
  if (p) p.setAttribute("d", path || "");
  if (title) m.title = title; else m.removeAttribute("title");
}

function renderUsage() {
  const u = state.usage;
  let cpu = 0, mem = 0, net = 0, counted = 0;
  for (const [id, row] of state.rows) {
    const strip = row.querySelector(".usage");
    if (!strip) continue;
    const inst = instById(id);
    const now = u.now.get(id);
    const show = u.supported && inst && inst.state === "running" && !!now;
    strip.classList.toggle("hidden", !show);
    if (!show) continue;
    const h = u.hist.get(id) || { cpu: [], mem: [], net: [] };
    const players = u.players.get(id) || [];
    if (inst.players_now == null) {
      setMetric(strip, "players", "—", "the game did not answer", sparkPath(players, 4),
        "This game answers no player query, or did not answer this time. Not the same as empty.");
    } else {
      const peak = Math.max(0, ...players.filter(v => v != null));
      const full = inst.max_players_now != null && inst.players_now >= inst.max_players_now;
      setMetric(strip, "players", playersText(inst),
        full ? "full" : (inst.bots ? inst.bots + " bots asked for · peak " + peak : "peak " + peak + " in view"),
        sparkPath(players, inst.max_players_now || 4), playersTitle(inst));
    }
    if (now.warming) {
      setMetric(strip, "cpu", "—", "measuring…");
      setMetric(strip, "net", "—", "measuring…");
      setMetric(strip, "disk", "—", "measuring…");
    } else {
      counted++;
      cpu += now.cpu; net += now.rx + now.tx;
      setMetric(strip, "cpu", Math.round(now.cpu) + "%",
        now.pids != null ? now.pids + " processes" : "",
        sparkPath(h.cpu, 25),
        "100% is one whole CPU core; this machine has " + now.cpus + ". The same figure docker stats shows.");
      setMetric(strip, "net", "↓ " + formatRate(now.rx), "↑ " + formatRate(now.tx),
        sparkPath(h.net, 2048), "Everything the game server receives and sends, players' traffic included.");
      setMetric(strip, "disk", formatRate(now.read + now.write),
        "read " + formatRate(now.read) + " · write " + formatRate(now.write), "");
    }
    if (now.mem != null) mem += now.mem;
    // Near its limit, Docker kills a server outright. Say so before it does.
    const tight = now.mem != null && now.memLimit && now.mem / now.memLimit > 0.9;
    strip.querySelector(".metric-mem").classList.toggle("warn", !!tight);
    setMetric(strip, "mem", formatBytes(now.mem),
      now.memLimit ? (tight ? "near its limit of " : "of ") + formatBytes(now.memLimit) : "",
      sparkPath(h.mem, 64 * 1024 * 1024),
      tight ? "Over 90% of this server's memory limit. If it reaches the limit, Docker stops it. Raise memory_limit_bytes for this game in agent.toml."
        : "Memory in use, not counting file cache the system can drop at any time.");
  }
  const total = $("node-usage");
  if (total) {
    total.classList.toggle("hidden", !u.supported || u.now.size === 0);
    $("node-usage-cpu").textContent = counted ? Math.round(cpu) + "%" : "—";
    $("node-usage-mem").textContent = formatBytes(mem);
    $("node-usage-net").textContent = counted ? formatRate(net) : "—";
  }
}

// ---------- initial connect ----------

async function tryConnect(token) {
  setToken(token);
  try {
    const health = await api("GET", "/health");
    renderBuildVersion(health && health.version);
    const games = await api("GET", "/games");
    state.games = Array.isArray(games) ? games : [];
    state.capacity = await api("GET", "/capacity");
    const insts = await api("GET", "/instances");
    state.instances = Array.isArray(insts) ? insts : [];
    clearError();
    showMainUI();
  } catch (e) {
    $("token-error").textContent = e && e.__auth ? "That token was not accepted." : ((e && e.error) || "Could not connect.");
    setToken(null);
    showTokenScreen();
  }
}

function renderBuildVersion(version) {
  const chip = $("build-version");
  if (!chip) return;
  chip.textContent = version ? "v" + version : "version unknown";
  chip.title = version
    ? "Agent build v" + version + " is serving this page"
    : "This agent is older than the build that started reporting its version";
}

// ---------- rendering: status ----------

function renderStatusPill() {
  const cap = state.capacity || {};
  const running = cap.running != null ? cap.running : 0;
  const max = cap.max_instances != null ? cap.max_instances : null;
  $("status-pill").textContent = running + (max != null ? " of " + max : "") + " running";
  const bar = $("capacity-bar");
  if (bar) bar.style.width = max ? Math.min(100, Math.round(100 * running / max)) + "%" : "0";
  const n = $("nav-servers-count");
  if (n) n.textContent = state.instances.length ? String(state.instances.length) : "";
}

// ---------- rendering: servers (updated in place by id) ----------

function formatUptime(secs) {
  if (secs === null || secs === undefined) return "—";
  if (secs < 0) secs = 0;
  const s = Math.floor(secs % 60);
  const m = Math.floor((secs / 60) % 60);
  const h = Math.floor(secs / 3600);
  if (h > 0) return h + "h " + m + "m";
  if (m > 0) return m + "m " + s + "s";
  return s + "s";
}

// "14/16": the cap is what the game reported in the same query, so an agent
// older than this page (no max_players_now) shows the count alone.
function playersText(inst) {
  return String(inst.players_now) + (inst.max_players_now != null ? "/" + inst.max_players_now : "");
}
function playersTitle(inst) {
  // A2S counts bots among players; say how many this node asked for, so a
  // full-looking server of bots reads as one.
  return "Read from the game just now" + (inst.bots ? ", including the " + inst.bots + " bots this node asked for" : "") + ".";
}

// How a stopped server ended, in words. The same rule as LastExit::is_crash in
// instance.rs: 0 and the signals a stop sends are a stop; anything else, an
// out-of-memory kill, or Docker failing to start it is a crash.
const SIGNALS = { 134: "aborted", 136: "arithmetic error", 139: "segmentation fault" };
function exitWords(exit) {
  if (!exit) return null;
  const when = exit.finished_at ? " at " + new Date(exit.finished_at).toLocaleString() : "";
  if (exit.oom_killed) {
    return { crash: true, short: "out of memory",
      long: "Killed for going over its memory limit" + when + ". Raise memory_limit_bytes for this game in agent.toml." };
  }
  if (exit.error) return { crash: true, short: "could not start", long: "Docker could not start it" + when + ": " + exit.error };
  if ([0, 130, 137, 143].includes(exit.code)) {
    return { crash: false, short: "stopped", long: "Stopped" + when + " (exit " + exit.code + ")." };
  }
  const what = SIGNALS[exit.code] || "exit code " + exit.code;
  const brief = { 134: "aborted", 136: "math error", 139: "segfault" }[exit.code] || "exit";
  return { crash: true, short: brief + " (" + exit.code + ")", long: "Crashed" + when + " with " + what + " (exit " + exit.code + "). Show log has its last lines." };
}
function stateTitle(inst, ending) {
  const parts = [];
  if (ending) parts.push(ending.long);
  if (inst.restart_on_crash) {
    parts.push("Started again if it crashes" + (inst.crash_restarts ? " (" + inst.crash_restarts + " this hour, of " + MAX_CRASH_RESTARTS + ")" : "") + ".");
  }
  return parts.join(" ");
}

function instById(id) { return state.instances.find(i => i.instance_id === id) || null; }

// What a server can be asked to do right now, and why not when it cannot. One
// place, so the row's buttons and its menu never disagree.
function capabilities(inst) {
  const game = gameOf(inst.game_id);
  const running = inst.state === "running";
  const speaks = !game || game.console !== false;
  const hasBots = !!(game && game.bots);
  return {
    running,
    canStop: !(inst.state === "stopped" || inst.state === "missing"),
    canMap: running && speaks,
    mapWhy: !running ? "The server has to be running to be told anything."
      : (!speaks ? "The " + inst.game_id + " pack declares no console, so the map cannot change without a restart."
        : "Change the map without restarting — players stay connected."),
    hasBots,
    canBots: hasBots && running,
    canAnnounce: running && !!inst.mesh_destination,
  };
}

function buildRow(inst) {
  const row = el("div", "inst");
  row.dataset.id = inst.instance_id;
  const artCell = el("div", "art-cell");
  artCell.appendChild(art(inst.game_id, "art-thumb"));
  const nameCell = el("div");
  nameCell.append(el("div", "name cell-name"), el("div", "sub cell-game"));
  const stateCell = el("div", "cell-state");
  stateCell.appendChild(el("span", "state-pill"));
  const actions = el("div", "actions cell-actions");
  const mk = (cls, ic, label, fn) => {
    const b = el("button", "icon-btn " + cls);
    b.type = "button";
    b.setAttribute("aria-label", label);
    b.title = label;
    b.appendChild(icon(ic));
    b.addEventListener("click", fn);
    return b;
  };
  const id = inst.instance_id;
  actions.append(
    mk("map-btn", "map", "Change map", () => onChangeMap(id, instById(id)?.game_id)),
    mk("restart-btn", "restart", "Restart", () => onRestart(id)),
    mk("stop-btn", "stop", "Stop", () => onStop(id)),
    mk("more-btn", "more", "More", (e) => {
      const r = e.currentTarget.getBoundingClientRect();
      openInstanceMenu(id, r.right - 220, r.bottom + 4);
    }),
  );
  row.append(artCell, nameCell, stateCell,
    el("div", "cell cell-map"), el("div", "cell cell-players"), el("div", "cell cell-port"),
    el("div", "cell cell-mesh"), el("div", "cell cell-uptime"), actions, buildUsageStrip());
  const playersCell = row.querySelector(".cell-players");
  playersCell.classList.add("clickable");
  playersCell.addEventListener("click", () => { if (instById(id)?.state === "running") onShowPlayers(id); });
  row.addEventListener("contextmenu", (e) => {
    if (e.target.closest("input, textarea")) return;
    e.preventDefault();
    openInstanceMenu(id, e.clientX, e.clientY);
  });
  // Ask for this game's maps as soon as a row for it exists, so the dialog
  // opens with the list already there.
  ensureMaps(inst.game_id);
  return row;
}

function renderInstances() {
  const list = $("instances-tbody");
  const seen = new Set();

  for (const inst of state.instances) {
    seen.add(inst.instance_id);
    let row = state.rows.get(inst.instance_id);
    if (!row) {
      row = buildRow(inst);
      list.appendChild(row);
      state.rows.set(inst.instance_id, row);
    }
    // Cells are updated in place; the row node is never replaced, which is what
    // keeps this list from stealing focus or scroll on a poll.
    row.querySelector(".cell-name").textContent = inst.name || inst.instance_id;
    row.querySelector(".cell-game").textContent = gameName(inst.game_id);
    const pill = row.querySelector(".state-pill");
    const ending = exitWords(inst.last_exit);
    const crashed = inst.state === "stopped" && ending && ending.crash;
    pill.textContent = crashed ? "crashed" : inst.state;
    pill.className = "state-pill state-" + (crashed ? "crashed" : String(inst.state || "unknown"));
    pill.title = stateTitle(inst, ending);

    // null is "the game could not be asked", not "no map" — and not zero
    // players either. A blank would make an unreachable server look like one
    // on an empty map.
    const mapCell = row.querySelector(".cell-map");
    if (inst.state === "stopped" && ending) {
      // A stopped server has no map to show; how it ended is the useful fact.
      mapCell.textContent = ending.short;
      mapCell.title = ending.long;
      mapCell.classList.toggle("muted-em", !ending.crash);
      mapCell.classList.toggle("bad-text", ending.crash);
    } else if (inst.map_now) {
      mapCell.classList.remove("bad-text");
      mapCell.textContent = inst.map_now;
      mapCell.title = "Read from the game just now";
      mapCell.classList.remove("muted-em");
    } else {
      mapCell.classList.remove("bad-text");
      mapCell.textContent = "—";
      mapCell.title = inst.state === "running"
        ? "This game answers no query, or did not answer — not the same as having no map."
        : "The server is not running.";
      mapCell.classList.add("muted-em");
    }

    const playersCell = row.querySelector(".cell-players");
    if (inst.players_now === null || inst.players_now === undefined) {
      playersCell.textContent = "—";
      playersCell.title = "could not ask this game";
      playersCell.classList.add("muted-em");
    } else {
      playersCell.textContent = playersText(inst);
      playersCell.title = playersTitle(inst);
      playersCell.classList.remove("muted-em");
    }

    row.querySelector(".cell-port").textContent = inst.port != null ? String(inst.port) : "—";

    // The mesh destination is the address a player joins from a launcher. Its
    // absence means this server exists on this machine's network and nowhere
    // else, so say that rather than showing a blank.
    const meshCell = row.querySelector(".cell-mesh");
    if (inst.mesh_destination) {
      meshCell.textContent = inst.mesh_destination.slice(0, 10) + "…";
      meshCell.classList.remove("muted-em");
      makeCopyable(meshCell, inst.mesh_destination);
    } else {
      meshCell.textContent = "LAN only";
      meshCell.title = "Not announced on the mesh. Add a [mesh] section to this node's config.";
      meshCell.classList.add("muted-em");
      meshCell.classList.remove("copyable");
      meshCell.onclick = null;
    }

    row.querySelector(".cell-uptime").textContent = formatUptime(inst.uptime_secs);

    const cap = capabilities(inst);
    const mapBtn = row.querySelector(".map-btn");
    mapBtn.disabled = !cap.canMap;
    mapBtn.title = cap.mapWhy;
    row.querySelector(".stop-btn").disabled = !cap.canStop;
  }

  for (const [id, row] of state.rows) {
    if (!seen.has(id)) {
      row.remove();
      state.rows.delete(id);
    }
  }
  renderUsage();
  $("instances-empty").classList.toggle("hidden", state.instances.length !== 0);
  document.querySelector("#tab-panel-servers .list-head").classList.toggle("hidden", state.instances.length === 0);
}

// ---------- menus ----------

function closeMenu() {
  const m = $("ctx-menu");
  if (!m.hidden) { m.hidden = true; m.textContent = ""; }
}
function showMenu(x, y, items) {
  const m = $("ctx-menu");
  m.textContent = "";
  for (const it of items) {
    if (it === "sep") { m.appendChild(el("div", "menu-sep")); continue; }
    if (it.head) { m.appendChild(el("div", "menu-head", it.head)); continue; }
    const b = el("button", "menu-item" + (it.danger ? " danger" : ""));
    b.type = "button";
    b.setAttribute("role", "menuitem");
    if (it.icon) b.appendChild(icon(it.icon));
    b.appendChild(el("span", null, it.label));
    b.disabled = !!it.disabled;
    if (it.title) b.title = it.title;
    b.onclick = () => { closeMenu(); it.action(); };
    m.appendChild(b);
  }
  m.hidden = false;
  const w = m.offsetWidth, h = m.offsetHeight;
  m.style.left = Math.max(4, Math.min(x, window.innerWidth - w - 4)) + "px";
  m.style.top = Math.max(4, Math.min(y, window.innerHeight - h - 4)) + "px";
  m.querySelector(".menu-item:not(:disabled)")?.focus({ preventScroll: true });
}

function openInstanceMenu(id, x, y) {
  const inst = instById(id);
  if (!inst) return;
  const cap = capabilities(inst);
  const items = [{ head: inst.name || id }];
  items.push({ label: "Change map…", icon: "map", disabled: !cap.canMap, title: cap.mapWhy, action: () => onChangeMap(id, inst.game_id) });
  if (cap.hasBots) {
    items.push({ label: "Bots…", icon: "bot", disabled: !cap.canBots,
      title: cap.canBots ? "Add or remove bots without restarting." : "The server has to be running.",
      action: () => onBots(id, inst.game_id) });
  }
  items.push({ label: "Restart", icon: "restart", action: () => onRestart(id) });
  items.push({ label: "Stop", icon: "stop", disabled: !cap.canStop, action: () => onStop(id) });
  items.push("sep");
  items.push({ label: "Announce now", icon: "megaphone", disabled: !cap.canAnnounce,
    title: cap.canAnnounce ? "Announce this server on the mesh now." : "Only a running server on the mesh can be announced.",
    action: () => onAnnounce(id) });
  items.push({ label: "Copy invite link", icon: "link", disabled: !inst.mesh_destination,
    title: inst.mesh_destination ? "A lanthorn:// link: anyone with Lanthorn who clicks it gets this server opened, ready to join."
      : "Only a server on the mesh has an invite link.",
    action: () => copyValue(inviteLink(inst)) });
  items.push({ label: "Copy mesh address", icon: "copy", disabled: !inst.mesh_destination, action: () => copyValue(inst.mesh_destination) });
  items.push({ label: "Show log", icon: "log", action: () => onShowLog(id) });
  items.push({ label: "Players…", icon: "users", disabled: !cap.running,
    title: cap.running ? "Who is on this server, as the game reports it." : "The server has to be running.",
    action: () => onShowPlayers(id) });
  if (inst.restart_on_crash != null) {
    items.push({ label: (inst.restart_on_crash ? "✓ " : "") + "Restart if it crashes", icon: "restart",
      title: "Up to " + MAX_CRASH_RESTARTS + " times an hour. A Stop is never undone.",
      action: () => onToggleRestartOnCrash(id, !inst.restart_on_crash) });
  }
  items.push("sep");
  items.push({ label: "Remove server…", icon: "trash", danger: true, action: () => onRemove(id, inst.name) });
  showMenu(x, y, items);
}

// ---------- rendering: games ----------

function renderGames() {
  const grid = $("games-grid");
  const n = $("nav-games-count");
  if (n) n.textContent = state.games.length ? String(state.games.length) : "";
  const sig = state.games.map(g => g.id + "|" + (g.runnable ? "1" : "0") + "|" + (g.reason || "")).join(";");
  if (grid.dataset.sig !== sig) {
    grid.dataset.sig = sig;
    grid.textContent = "";
    const sorted = [...state.games].sort((a, b) => (b.runnable - a.runnable) || a.display_name.localeCompare(b.display_name));
    for (const g of sorted) grid.appendChild(buildGameCard(g));
  }
  for (const g of state.games) refreshCardDynamic(g);
}

function buildGameCard(g) {
  const card = el("article", "game-card" + (g.runnable ? "" : " unrunnable"));
  card.dataset.gameId = g.id;
  card.appendChild(art(g.id));
  const body = el("div", "card-body");
  body.appendChild(el("h3", "card-title", g.display_name));
  const meta = el("div", "card-meta");
  meta.appendChild(el("span", "chip", (g.transport || "—").toUpperCase() + " " + g.default_port));
  if (g.extra_ports) meta.appendChild(el("span", "chip", "+" + g.extra_ports + (g.extra_ports === 1 ? " port" : " ports")));
  if (g.console) meta.appendChild(el("span", "chip", "Live map change"));
  if (g.bots) meta.appendChild(el("span", "chip", "Bots"));
  body.appendChild(meta);
  if (!g.runnable) body.appendChild(el("p", "card-reason", "Cannot start here: " + (g.reason || "unavailable on this node.")));
  body.appendChild(el("p", "install-status hidden"));
  const actions = el("div", "card-actions");
  const startBtn = el("button", "btn btn-primary start-btn", "Start server");
  startBtn.type = "button";
  if (!g.runnable) { startBtn.disabled = true; startBtn.title = g.reason || "not runnable"; }
  startBtn.addEventListener("click", () => openStartDialog(g.id));
  const installBtn = el("button", "btn install-btn");
  installBtn.type = "button";
  installBtn.title = "Download this game's files to the node now";
  installBtn.appendChild(icon("download"));
  installBtn.appendChild(el("span", null, "Install"));
  installBtn.addEventListener("click", () => onInstall(g.id));
  const removeBtn = el("button", "card-remove");
  removeBtn.type = "button";
  removeBtn.title = "Remove this game from the node: its pack goes, its files stay on the Installed page";
  removeBtn.setAttribute("aria-label", "Remove " + g.display_name);
  removeBtn.appendChild(icon("trash"));
  removeBtn.addEventListener("click", () => onRemovePack(g));
  actions.append(startBtn, installBtn);
  body.appendChild(actions);
  card.append(removeBtn, body);
  return card;
}

// Take a game's pack off the node (DELETE /packs/:id). The node refuses while
// any server of the game exists and says which.
async function onRemovePack(g) {
  if (!confirm("Remove " + g.display_name + " from this node?\n\nIts pack is deleted, so it leaves the Games page and no server of it can start. " +
    "Its game files stay; delete them on the Installed page if you want the space. It can be added back by importing the pack again.")) return;
  try {
    await withInFlight(api("DELETE", "/packs/" + encodeURIComponent(g.id)));
    const games = await api("GET", "/games");
    state.games = Array.isArray(games) ? games : [];
    renderGames();
    clearError();
    toast(g.display_name + " removed. Its files are still on the Installed page.");
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "The game was not removed.");
  }
}

function refreshCardDynamic(g) {
  const card = $("games-grid").querySelector('.game-card[data-game-id="' + CSS.escape(g.id) + '"]');
  if (!card) return;
  const installBtn = card.querySelector(".install-btn");
  const label = installBtn.querySelector("span");
  const status = card.querySelector(".install-status");
  status.classList.remove("ok", "err");
  if (state.installing.get(g.id)) {
    installBtn.disabled = true;
    label.textContent = "Installing…";
    status.classList.remove("hidden");
    status.textContent = state.installingMsg.get(g.id) || "Installing… this can take several minutes.";
  } else if (state.installingMsg.has(g.id)) {
    installBtn.disabled = false;
    label.textContent = "Install";
    status.classList.remove("hidden");
    status.classList.add(state.installDone.get(g.id) === "error" ? "err" : "ok");
    status.textContent = state.installingMsg.get(g.id);
  } else {
    installBtn.disabled = false;
    label.textContent = "Install";
    status.classList.add("hidden");
    status.textContent = "";
  }
}

// ---------- maps ----------

// What this node has for a game, read off its content copy. Asked for once per
// game and cached; the field stays usable as free text the whole time.
async function ensureMaps(gameId, onLoaded) {
  if (!gameId) return;
  if (state.maps.has(gameId)) { if (onLoaded && state.maps.get(gameId) !== null) onLoaded(state.maps.get(gameId)); return; }
  state.maps.set(gameId, null);
  try {
    const body = await api("GET", "/games/" + encodeURIComponent(gameId) + "/maps");
    state.maps.set(gameId, (body && body.maps) || []);
  } catch (e) {
    state.maps.set(gameId, []);
  }
  if (onLoaded) onLoaded(state.maps.get(gameId));
}

// A <datalist> rather than a <select>: the node lists what it has, and an
// operator can still type a map it does not know about.
function mapDatalist(id, gameId) {
  const dl = el("datalist");
  dl.id = id;
  (state.maps.get(gameId) || []).forEach(m => {
    const o = el("option");
    o.value = m;
    dl.appendChild(o);
  });
  return dl;
}

// ---------- dialogs ----------

function openModal(id, gameId, build) {
  closeModal(id);
  const back = el("div", "modal-back");
  back.id = id;
  const box = el("div", "modal");
  box.setAttribute("role", "dialog");
  box.setAttribute("aria-modal", "true");
  if (gameId) {
    const hero = el("div", "modal-hero");
    hero.appendChild(art(gameId));
    box.appendChild(hero);
  }
  const body = el("div", "modal-body");
  box.appendChild(body);
  back.appendChild(box);
  back.addEventListener("mousedown", e => { if (e.target === back) closeModal(id); });
  document.body.appendChild(back);
  build(body, () => closeModal(id));
  return back;
}
function closeModal(id) { const d = $(id); if (d) d.remove(); }

function field(parent, id, label, attrs) {
  const l = el("label", null, label);
  l.htmlFor = id;
  const input = el("input");
  input.id = id;
  Object.assign(input, attrs || {});
  parent.append(l, input);
  return input;
}

// Starting a server: a dialog over the games grid, so the grid itself never has
// a half-typed form inside a card that a refresh could rebuild.
function openStartDialog(gameId) {
  const game = gameOf(gameId);
  if (!game) return;
  openModal("start-dialog", gameId, (body, close) => {
    body.appendChild(el("h3", null, "New " + game.display_name + " server"));
    const form = el("form", "advanced-wrap");
    form.autocomplete = "off";
    const name = field(form, "sf-name", "Server name", { type: "text", required: true, placeholder: "My Server" });
    const mp = field(form, "sf-mp", "Max players", { type: "number", min: "1", max: "64", step: "1", value: "16", required: true });
    const map = field(form, "sf-map", "Starting map", { type: "text", placeholder: "the game's default" });
    map.setAttribute("list", "sf-maplist");
    const hint = el("p", "muted small", "The map name as the game knows it — svencoop1, de_dust2, cp_dustbowl.");
    form.append(mapDatalist("sf-maplist", gameId), hint);
    ensureMaps(gameId, (maps) => {
      const dl = $("sf-maplist");
      if (dl) dl.replaceWith(mapDatalist("sf-maplist", gameId));
      if (maps && maps.length) hint.textContent = maps.length + " maps installed here — click the field to pick one, or type any name.";
    });
    // Bots, and only for a game that has them: a disabled field would be
    // furniture, since nothing an operator does here could make it work.
    let bots = null;
    if (game.bots) {
      bots = field(form, "sf-bots", "Bots", { type: "number", min: "0", max: "32", step: "1", placeholder: "none" });
      form.appendChild(el("p", "muted small", "Bots join immediately, and the number can be changed while the server runs."));
    }
    const crashWrap = el("label", "check");
    const crash = el("input");
    crash.type = "checkbox";
    crash.id = "sf-crash";
    crashWrap.append(crash, el("span", null, "Start it again if it crashes"));
    form.append(crashWrap, el("p", "muted small",
      "Up to " + MAX_CRASH_RESTARTS + " times an hour. A Stop is never undone. Can be changed later from the server's menu."));
    const adv = el("details");
    adv.appendChild(el("summary", "muted", "Advanced"));
    const advWrap = el("div", "advanced-wrap");
    advWrap.style.marginTop = "8px";
    const port = field(advWrap, "sf-port", "Fixed host port", { type: "number", min: "1024", max: "65535", placeholder: "chosen by the node" });
    advWrap.appendChild(el("p", "muted small", "Left blank, the node picks a free port from its configured range."));
    adv.appendChild(advWrap);
    form.appendChild(adv);

    const actions = el("div", "form-actions");
    actions.style.marginTop = "8px";
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const go = el("button", "btn btn-primary", "Start server");
    go.type = "submit";
    actions.append(cancel, go);
    form.appendChild(actions);
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      onStartSubmit(gameId, {
        name: name.value, maxPlayers: parseInt(mp.value, 10), map: map.value,
        bots: bots ? bots.value : "", fixedPort: port.value, restartOnCrash: crash.checked,
      }, go, close);
    });
    body.appendChild(form);
    name.focus();
  });
}

function randomInstanceId(gameId) {
  const chars = "abcdefghijklmnopqrstuvwxyz0123456789";
  let s = "";
  for (let i = 0; i < 6; i++) s += chars[Math.floor(Math.random() * chars.length)];
  return gameId + "-" + s;
}

async function onStartSubmit(gameId, f, submitBtn, close) {
  const name = (f.name || "").trim();
  if (!name) { showError("A server needs a name."); return; }
  if (!(f.maxPlayers >= 1 && f.maxPlayers <= 64)) { showError("Max players must be between 1 and 64."); return; }
  // Blank means "say nothing about the map": the node only sets GPP_MAP when
  // given a name, so the image's own default stands.
  const map = (f.map || "").trim();
  if (map !== "" && !MAP_NAME_RE.test(map)) {
    showError("A map name may only contain letters, digits, '_', '-', '.' and '/'.");
    return;
  }
  let port = null;
  const pv = (f.fixedPort || "").trim();
  if (pv !== "") {
    const n = parseInt(pv, 10);
    if (isNaN(n) || n < 1024 || n > 65535) { showError("A fixed host port must be between 1024 and 65535, or blank."); return; }
    port = n;
  }
  const game = gameOf(gameId);
  if (!game) { showError("That game is no longer offered by this node."); return; }
  // Blank means "say nothing about bots", which is not the same as zero.
  let bots = null;
  if (game.bots) {
    const bv = (f.bots || "").trim();
    if (bv !== "") {
      const n = parseInt(bv, 10);
      if (isNaN(n) || n < 0 || n > 32) { showError("Bots must be between 0 and 32, or blank."); return; }
      bots = n;
    }
  }
  if (!game.runnable) { showError("This game cannot start here: " + (game.reason || "")); return; }

  // Starting a game that is not installed yet downloads it first.
  if (!roomForDownload()) return;

  const instance_id = randomInstanceId(gameId);
  if (!/^[a-z0-9._-]{1,64}$/.test(instance_id)) { showError("Could not make a valid id for this server."); return; }

  const body = {
    instance_id, game_id: gameId, name, max_players: f.maxPlayers, port,
    extra_ports: {}, map: map === "" ? null : map, bots, owner: null,
    restart_on_crash: !!f.restartOnCrash,
  };
  submitBtn.disabled = true;
  submitBtn.textContent = "Starting…";
  try {
    const result = await withInFlight(api("POST", "/instances", body));
    close();
    clearError();
    // 202: the game's files are not here yet, so the agent started the
    // download instead of refusing. The operator asked to play — wait for it
    // and then start the server without asking anything else.
    if (result && result.installing) {
      setActiveTab("games");
      state.installingMsg.set(gameId, "Downloading game files… this can take a while.");
      renderGames();
      const ok = await watchInstall(gameId);
      if (ok) {
        await withInFlight(api("POST", "/instances", body));
        setActiveTab("servers");
        toast("“" + name + "” is starting.");
        poll();
      }
      return;
    }
    setActiveTab("servers");
    toast("“" + name + "” is starting.");
    poll();
  } catch (e) {
    showError((e && e.error) || "The server did not start.");
    submitBtn.disabled = false;
    submitBtn.textContent = "Start server";
  }
}

// Poll one game's install until it finishes. Deliberately not `withInFlight`:
// an install runs for tens of minutes and must not freeze the server list.
async function watchInstall(gameId) {
  state.installing.set(gameId, true);
  state.installDone.delete(gameId);
  renderGames();
  try {
    for (;;) {
      await new Promise(r => setTimeout(r, 3000));
      let body;
      try {
        body = await api("GET", "/content/" + encodeURIComponent(gameId));
      } catch (e) {
        if (e && e.__auth) return false;
        continue; // a blip in polling is not a failed install
      }
      const st = (body && body.status) || {};
      if (st.state === "queued") {
        state.installingMsg.set(gameId, "Waiting for another download to finish first.");
        renderGames();
        continue;
      }
      if (st.state === "running") {
        state.installingMsg.set(gameId, "Downloading game files… " +
          (st.bytes ? formatBytes(st.bytes) + " in " : "") + formatUptime(st.since_secs) + " so far.");
        renderGames();
        continue;
      }
      if (st.state === "done") {
        state.installingMsg.set(gameId, st.already_installed ? "Already installed." : "Installed.");
        state.installDone.set(gameId, "ok");
        return true;
      }
      if (st.state === "failed" && st.error === "cancelled") {
        state.installingMsg.set(gameId, "Download cancelled.");
        state.installDone.set(gameId, "error");
        return false;
      }
      if (st.state === "failed") {
        state.installingMsg.set(gameId, st.error || "Install failed.");
        state.installDone.set(gameId, "error");
        showError(st.error || "Install failed.");
        return false;
      }
      return false; // "idle": the agent forgot, or never started
    }
  } finally {
    state.installing.delete(gameId);
    renderGames();
  }
}

async function onInstall(gameId) {
  if (state.installing.get(gameId)) return;
  if (!roomForDownload()) return;
  state.installing.set(gameId, true);
  state.installingMsg.delete(gameId);
  state.installDone.delete(gameId);
  renderGames();
  try {
    // Returns once the download has *started*: no browser holds a request open
    // for tens of minutes.
    await withInFlight(api("POST", "/content/" + encodeURIComponent(gameId)));
    clearError();
    state.installing.delete(gameId);
    await watchInstall(gameId);
  } catch (e) {
    if (e && e.__auth) return;
    state.installingMsg.set(gameId, (e && e.error) || "Install failed.");
    state.installDone.set(gameId, "error");
    state.installing.delete(gameId);
    renderGames();
  }
}

// ---------- invite links ----------

// The launcher's format (`launcher-core/src/invite.rs`):
// lanthorn://join/<destination>?game=<id>&name=<name>. Encoded the way that
// file encodes, so a link made here and one made in the launcher are the
// same string. A link only selects a server in the launcher; it joins nothing.
function inviteLink(inst) {
  const enc = v => encodeURIComponent(v).replace(/[!'()*]/g, c => "%" + c.charCodeAt(0).toString(16).toUpperCase());
  const q = [];
  if (inst.game_id) q.push("game=" + enc(inst.game_id));
  const name = (inst.name || "").trim();
  if (name) q.push("name=" + enc(Array.from(name).slice(0, 64).join("")));
  return "lanthorn://join/" + String(inst.mesh_destination).toLowerCase() + (q.length ? "?" + q.join("&") : "");
}

// ---------- server log ----------

// Read-only on purpose. A box that typed into the server's console would be a
// free-text console line from a web page, which `console.rs` exists to stop.
function onShowLog(instanceId) {
  const inst = instById(instanceId);
  let lines = LOG_LINE_CHOICES[0];
  const back = openModal("log-dialog", null, (body, close) => {
    const head = el("div", "log-head");
    head.appendChild(el("h3", null, "Log · " + ((inst && inst.name) || instanceId)));
    const spacer = el("div", "toolbar-spacer");
    const pick = el("select", "field log-lines");
    pick.setAttribute("aria-label", "How many lines");
    for (const n of LOG_LINE_CHOICES) {
      const o = el("option", null, "Last " + n + " lines");
      o.value = String(n);
      pick.appendChild(o);
    }
    pick.addEventListener("change", () => { lines = parseInt(pick.value, 10); refresh(true); });
    const copy = el("button", "btn", "Copy");
    copy.type = "button";
    copy.addEventListener("click", () => copyValue(pre.textContent));
    const shut = el("button", "btn", "Close");
    shut.type = "button";
    shut.addEventListener("click", close);
    head.append(spacer, pick, copy, shut);
    const pre = el("pre", "log-text", "Reading the log…");
    pre.tabIndex = 0;
    const foot = el("div", "muted small log-foot", "Updates every 2 s while open. Read-only.");
    body.append(head, pre, foot);

    let first = true;
    async function refresh(jump) {
      if (state.logTimer) { clearTimeout(state.logTimer); state.logTimer = null; }
      if (!$("log-dialog")) return;
      try {
        const text = await apiText("/instances/" + encodeURIComponent(instanceId) + "/logs?lines=" + lines);
        if (!$("log-dialog")) return;
        // Follow the end only for a reader who is already there; someone
        // scrolled up to read is left where they are.
        const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 8;
        if (pre.textContent !== text) {
          pre.textContent = text || "(nothing written yet)";
          if (first || jump || atEnd) pre.scrollTop = pre.scrollHeight;
        }
        first = false;
        foot.textContent = "Updates every 2 s while open. Read-only.";
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        foot.textContent = (e && e.error) || "Could not read the log.";
      }
      if ($("log-dialog")) state.logTimer = setTimeout(() => refresh(false), LOG_REFRESH_MS);
    }
    refresh(true);
  });
  back.querySelector(".modal").classList.add("modal-wide");
}

// ---------- players ----------

function formatDuration(secs) {
  if (secs == null) return "—";
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60);
  return h ? h + "h " + m + "m" : m ? m + "m" : Math.floor(secs) + "s";
}

// Who is on a server, from the game itself. Bots are in the list: A2S does not
// tell them apart. `null` from the node is "could not ask", never "nobody".
function onShowPlayers(instanceId) {
  const inst = instById(instanceId);
  const back = openModal("players-dialog", null, (body, close) => {
    const head = el("div", "log-head");
    head.appendChild(el("h3", null, "Players · " + ((inst && inst.name) || instanceId)));
    head.appendChild(el("div", "toolbar-spacer"));
    const shut = el("button", "btn", "Close");
    shut.type = "button";
    shut.addEventListener("click", close);
    head.appendChild(shut);
    const table = el("div", "players-table");
    table.textContent = "Asking the game…";
    const foot = el("div", "muted small log-foot", "Updates every 5 s while open.");
    body.append(head, table, foot);

    async function refresh() {
      if (state.playersTimer) { clearTimeout(state.playersTimer); state.playersTimer = null; }
      if (!$("players-dialog")) return;
      try {
        const list = await api("GET", "/instances/" + encodeURIComponent(instanceId) + "/players");
        if (!$("players-dialog")) return;
        table.textContent = "";
        if (list == null) {
          table.appendChild(el("p", "muted", "The game did not answer, or answers no player query. That is not the same as nobody being on."));
        } else if (!list.length) {
          table.appendChild(el("p", "muted", "Nobody is on this server."));
        } else {
          const headRow = el("div", "players-row players-headrow");
          headRow.append(el("div", null, "Name"), el("div", null, "Score"), el("div", null, "Connected"));
          table.appendChild(headRow);
          for (const p of [...list].sort((a, b) => b.score - a.score)) {
            const r = el("div", "players-row");
            r.append(el("div", "players-name", p.name || "(connecting)"), el("div", null, String(p.score)),
              el("div", null, formatDuration(p.connected_secs)));
            table.appendChild(r);
          }
        }
        foot.textContent = (list ? list.length + " listed" : "Not answered") + " · updates every 5 s while open.";
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        foot.textContent = (e && e.error) || "Could not ask the node.";
      }
      if ($("players-dialog")) state.playersTimer = setTimeout(refresh, PLAYERS_REFRESH_MS);
    }
    refresh();
  });
  back.querySelector(".modal").classList.add("modal-mid");
}

async function onToggleRestartOnCrash(instanceId, on) {
  try {
    await withInFlight(api("POST", "/instances/" + encodeURIComponent(instanceId) + "/restart-on-crash", { on }));
    clearError();
    toast(on ? "It will be started again if it crashes." : "It will stay stopped if it crashes.");
    poll();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Could not change that.");
  }
}

// ---------- leftovers ----------

// Folders of servers deleted earlier. Read once a minute, not every poll: it
// walks the disk. Deleting them is the operator's call (agent.rs,
// clean_orphans), and asked for every time.
const ORPHANS_EVERY_MS = 60000;
async function refreshOrphans(force) {
  if (!force && Date.now() - state.orphansCheckedAt < ORPHANS_EVERY_MS) return;
  state.orphansCheckedAt = Date.now();
  try {
    state.orphans = await api("GET", "/orphans/summary");
  } catch (e) {
    state.orphans = null; // an older agent: no notice
  }
  renderOrphans();
}
function renderOrphans() {
  const bar = $("orphans-bar");
  if (!bar) return;
  const o = state.orphans;
  bar.classList.toggle("hidden", !o || !o.count);
  if (!o || !o.count) return;
  $("orphans-text").textContent = o.count + " leftover folder" + (o.count === 1 ? "" : "s") +
    " from deleted servers, " + formatBytes(o.bytes) + ".";
}
async function onCleanOrphans() {
  const o = state.orphans;
  if (!o || !o.count) return;
  if (!confirm("Delete " + o.count + " leftover folder" + (o.count === 1 ? "" : "s") + " (" + formatBytes(o.bytes) + ")?\n\n" +
    "They belong to servers that no longer exist: their saves, logs and mesh identities. " +
    "A server started again under one of those names would get a new mesh address.\n\nThis cannot be undone.")) return;
  try {
    const r = await withInFlight(api("POST", "/orphans/clean"));
    clearError();
    toast("Removed " + r.count + " folder" + (r.count === 1 ? "" : "s") + ", " + formatBytes(r.bytes) + "." +
      (r.failed ? " " + r.failed + " could not be removed: " + r.error : ""));
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Could not clean up.");
  }
  refreshOrphans(true);
}

// ---------- server actions ----------

async function rowAction(instanceId, cls, busyText, request, failText, doneText) {
  const row = state.rows.get(instanceId);
  const btn = row && cls ? row.querySelector(cls) : null;
  if (btn) btn.disabled = true;
  if (row) row.style.opacity = "0.7";
  try {
    await withInFlight(request());
    clearError();
    if (doneText) toast(doneText);
    poll();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || failText);
  } finally {
    if (btn) btn.disabled = false;
    if (row) row.style.opacity = "";
  }
}

// Off and on again, keeping its ports and its mesh destination, so a player's
// saved address still works — which is why this is not remove-and-recreate.
function onRestart(id) {
  return rowAction(id, ".restart-btn", "…",
    () => api("POST", "/instances/" + encodeURIComponent(id) + "/restart"),
    "The server did not restart.", "Restarting.");
}
function onStop(id) {
  return rowAction(id, ".stop-btn", "…",
    () => api("POST", "/instances/" + encodeURIComponent(id) + "/stop"),
    "The server did not stop.", "Stopping.");
}
async function onRemove(id, name) {
  const confirmed = confirm(
    'Remove the server "' + (name || id) + '"?\n\n' +
    "This destroys its container. Its files stay on disk.\n\nThis cannot be undone.");
  if (!confirmed) return;
  return rowAction(id, null, null,
    () => api("DELETE", "/instances/" + encodeURIComponent(id)),
    "The server was not removed.", "Removed.");
}

// Announce now, rather than at each announcer's next tick. Each announcer keeps
// its own floor, so this cannot become a storm on someone's slow link.
async function onAnnounce(id) {
  try {
    const r = await withInFlight(api("POST", "/mesh/announce", id ? { instance: id } : {}));
    clearError();
    const n = r && r.announced != null ? r.announced : 0;
    toast(id ? "Announced." : (n ? "Announced " + n + (n === 1 ? " server." : " servers.") : "No server is running on the mesh."));
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Could not announce.");
  }
}

// Bots on a running server: the number is a quota, so the dialog asks "how
// many", never "add" or "kick". Asking twice for four leaves four.
function onBots(instanceId, gameId) {
  const inst = instById(instanceId);
  openModal("bots-dialog", gameId, (body, close) => {
    body.appendChild(el("h3", null, "Bots"));
    body.appendChild(el("p", "muted small",
      "How many bots this server should hold. 0 removes them all; nobody is disconnected."));
    const input = field(body, "bots-count", "Bots", { type: "number", min: "0", max: "32", step: "1",
      value: inst && inst.bots != null ? String(inst.bots) : "4" });
    const actions = el("div", "form-actions");
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const go = el("button", "btn btn-primary", "Set bots");
    go.type = "button";
    go.addEventListener("click", async () => {
      const n = parseInt(input.value, 10);
      if (isNaN(n) || n < 0 || n > 32) { showError("Bots must be between 0 and 32."); return; }
      go.disabled = true; go.textContent = "Setting…";
      try {
        await withInFlight(api("POST", "/instances/" + encodeURIComponent(instanceId) + "/bots", { count: n }));
        clearError();
        close();
        toast("Bots set to " + n + ".");
        poll();
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "Could not set the number of bots.");
        go.disabled = false; go.textContent = "Set bots";
      }
    });
    actions.append(cancel, go);
    body.appendChild(actions);
    input.focus();
    input.select();
  });
}

// Change the map on a live server: `changelevel` keeps every player connected.
// A picker, because recalling one exact name out of a hundred is not choosing.
function onChangeMap(instanceId, gameId) {
  if (!gameId) return;
  openModal("map-dialog", gameId, (body, close) => {
    body.appendChild(el("h3", null, "Change map"));
    const note = el("p", "muted small", "Reading the maps installed on this node…");
    body.appendChild(note);
    const input = field(body, "map-dialog-input", "Map", { type: "text", placeholder: "map name" });
    input.setAttribute("list", "map-dialog-list");
    const holder = el("div");
    body.appendChild(holder);
    const fill = (maps) => {
      holder.textContent = "";
      holder.appendChild(mapDatalist("map-dialog-list", gameId));
      note.textContent = maps && maps.length
        ? maps.length + " maps installed. Players stay connected while the level changes."
        : "This node lists no maps for this game. Type a name; players stay connected.";
      if (maps && maps.length) {
        const list = el("div", "map-list");
        maps.forEach(m => {
          const b = el("button", "map-choice", m);
          b.type = "button";
          b.addEventListener("click", () => {
            input.value = m;
            list.querySelectorAll(".map-choice").forEach(x => x.classList.remove("chosen"));
            b.classList.add("chosen");
          });
          list.appendChild(b);
        });
        holder.appendChild(list);
      }
    };
    ensureMaps(gameId, fill);
    const actions = el("div", "form-actions");
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const go = el("button", "btn btn-primary", "Change map");
    go.type = "button";
    go.addEventListener("click", async () => {
      const trimmed = (input.value || "").trim();
      if (trimmed === "") { input.focus(); return; }
      if (!MAP_NAME_RE.test(trimmed)) {
        showError("A map name may only contain letters, digits, '_', '-', '.' and '/'.");
        return;
      }
      go.disabled = true; go.textContent = "Changing…";
      try {
        await withInFlight(api("POST", "/instances/" + encodeURIComponent(instanceId) + "/map", { map: trimmed }));
        clearError();
        close();
        toast("Changing to " + trimmed + ".");
        poll();
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "Could not change the map.");
        go.disabled = false; go.textContent = "Change map";
      }
    });
    actions.append(cancel, go);
    body.appendChild(actions);
    input.focus();
  });
}

// ---------- mesh ----------

function formatBytes(n) {
  if (n === null || n === undefined) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = Number(n), i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return (i === 0 ? v : v.toFixed(1)) + " " + units[i];
}

// Uplink interfaces: three states — no uplink (501), up with interfaces, up
// but empty. A missing uplink is a configuration fact, not an error.
async function loadInterfaces() {
  renderMeshGames();
  renderMeshInterfaces();
  const offline = $("mesh-offline");
  const online = $("mesh-online");
  try {
    const status = await api("GET", "/interfaces");
    state.interfaces = Array.isArray(status.interfaces) ? status.interfaces : [];
    $("mesh-destination").textContent = status.destination || "unknown";
    offline.classList.add("hidden");
    online.classList.remove("hidden");
    renderInterfaces();
  } catch (e) {
    if (e && e.__auth) return;
    if (e && e.error && /uplink/i.test(e.error)) {
      offline.classList.remove("hidden");
      online.classList.add("hidden");
      return;
    }
    showError((e && e.error) || "Could not read the uplink's connections.");
  }
}

function renderInterfaces() {
  const list = $("interfaces-tbody");
  const empty = $("interfaces-empty");
  list.textContent = "";
  const items = state.interfaces || [];
  empty.classList.toggle("hidden", items.length !== 0);
  for (const iface of items) {
    const row = el("div", "item");
    row.title = "interface id: " + iface.id;
    row.appendChild(el("span", "dot" + (/connect/i.test(iface.connection || "") && !/dis/i.test(iface.connection || "") ? " on" : "")));
    row.appendChild(el("span", "item-main", iface.name || "—"));
    row.appendChild(el("span", "item-meta", (iface.mode || "—") + " · " + (iface.connection || "—")
      + " · ↓" + formatBytes(iface.rx_bytes) + " ↑" + formatBytes(iface.tx_bytes)
      + " · " + (iface.links != null ? iface.links : 0) + " links"));
    const rename = el("button", "btn btn-sm", "Rename");
    rename.type = "button";
    rename.addEventListener("click", () => onRenameInterface(iface.id, iface.name));
    const remove = el("button", "btn btn-sm btn-danger", "Remove");
    remove.type = "button";
    remove.addEventListener("click", () => onRemoveInterface(iface.id, iface.name));
    row.append(rename, remove);
    list.appendChild(row);
  }
}

async function onAddInterface() {
  const kind = $("iface-kind").value;
  const ifacOn = $("iface-ifac-toggle").checked;
  const ifac_name = ifacOn ? ($("iface-ifac-name").value.trim() || null) : null;
  const ifac_passphrase = ifacOn ? ($("iface-ifac-pass").value || null) : null;
  let body;
  if (kind === "tcp") {
    const addr = $("iface-addr").value.trim();
    if (!addr) { showError("A TCP connection needs a host:port address."); return; }
    body = { kind: "tcp", addr, ifac_name, ifac_passphrase };
  } else {
    body = { kind: "auto", ifac_name, ifac_passphrase };
  }
  const submitBtn = $("iface-form").querySelector("button[type=submit]");
  submitBtn.disabled = true;
  try {
    await withInFlight(api("POST", "/interfaces", body));
    clearError();
    $("iface-addr").value = "";
    $("iface-ifac-name").value = "";
    $("iface-ifac-pass").value = "";
    $("iface-ifac-toggle").checked = false;
    $("iface-ifac-wrap").classList.add("hidden");
    loadInterfaces();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Could not add that connection.");
  } finally {
    submitBtn.disabled = false;
  }
}

async function onRemoveInterface(id, name) {
  if (!confirm('Remove "' + (name || id) + '"?\n\nIt is detached and forgotten, so it will not come back on restart.')) return;
  try {
    await withInFlight(api("DELETE", "/interfaces/" + encodeURIComponent(id)));
    clearError();
    loadInterfaces();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Could not remove that connection.");
  }
}

async function onRenameInterface(id, current) {
  const name = prompt("New name for this connection:", current || "");
  if (name === null) return;
  const trimmed = name.trim();
  if (!trimmed) { showError("A name cannot be empty."); return; }
  try {
    await withInFlight(api("POST", "/interfaces/" + encodeURIComponent(id) + "/rename", { name: trimmed }));
    clearError();
    loadInterfaces();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Could not rename that connection.");
  }
}

// The `[mesh]` half of this node's Reticulum story: which running servers are
// announced, and under what destination. Kept apart from the uplink because
// they are separate jobs, and reporting them as one confused operators.
async function renderMeshGames() {
  const note = $("mesh-games-note");
  const list = $("mesh-games-list");
  let body;
  try {
    body = await api("GET", "/mesh");
  } catch (e) {
    if (e && e.__auth) return;
    note.textContent = (e && e.error) || "Could not read the mesh status.";
    return;
  }
  note.textContent = body.note || "";
  $("announce-btn").disabled = !body.enabled;
  $("announce-btn").title = body.enabled
    ? "Announce every server on the mesh now, instead of waiting for the next announce"
    : "This node runs its games LAN-only, so there is nothing to announce.";
  list.textContent = "";
  const servers = Array.isArray(body.servers) ? body.servers : [];
  if (!servers.length) {
    list.appendChild(el("p", "muted", body.enabled
      ? "No servers are running, so nothing is announced yet."
      : "Nothing is announced: this node runs its games LAN-only."));
    return;
  }
  for (const s of servers) {
    const row = el("div", "item");
    row.appendChild(art(s.game_id, "art-thumb"));
    row.appendChild(el("span", "item-main", s.name || s.instance_id));
    const dest = el("span", "item-meta", s.destination ? s.destination.slice(0, 12) + "…" : "starting…");
    if (s.destination) makeCopyable(dest, s.destination);
    row.appendChild(dest);
    // Interfaces a server could not attach. Shown, not left in the log: the
    // server is listed and announcing elsewhere, so nothing else looks wrong.
    (s.interface_notes || []).forEach(n => row.appendChild(el("div", "item-note", n)));
    list.appendChild(row);
  }
}

// Interfaces the hosted games use — what carries game traffic.
async function renderMeshInterfaces() {
  const list = $("mesh-iface-list");
  let body;
  try {
    body = await api("GET", "/mesh/interfaces");
  } catch (e) {
    if (e && e.__auth) return;
    list.textContent = (e && e.error) || "Could not read the connections.";
    return;
  }
  list.textContent = "";
  const configured = Array.isArray(body.configured) ? body.configured : [];
  if (!configured.length) {
    list.appendChild(el("p", "muted",
      "None added here. The [mesh] section in this node's config may still give every server a TCP connection or auto-discovery."));
    return;
  }
  for (const i of configured) {
    const row = el("div", "item");
    row.appendChild(el("span", "item-main", i.kind === "auto" ? "LAN auto-discovery" : i.addr));
    if (i.ifac) row.appendChild(el("span", "badge ok", "Protected" + (i.ifac_name ? " · " + i.ifac_name : "")));
    const del = el("button", "btn btn-sm btn-danger", "Forget");
    del.type = "button";
    del.onclick = async () => {
      del.disabled = true;
      try {
        const r = await withInFlight(api("DELETE", "/mesh/interfaces/" + encodeURIComponent(i.id)));
        // Forgetting is not detaching — the engine cannot remove a live
        // interface — so say so rather than let the list imply otherwise.
        if (r && r.note) toast(r.note);
        renderMeshInterfaces();
      } catch (e) {
        showError((e && e.error) || "Could not forget that connection.");
      } finally { del.disabled = false; }
    };
    row.appendChild(del);
    list.appendChild(row);
  }
}

function wireMeshInterfaceForm() {
  const form = $("mesh-iface-form");
  const kind = $("mesh-iface-kind");
  const addr = $("mesh-iface-addr");
  const addrLabel = form.querySelector('label[for="mesh-iface-addr"]');
  const udpWrap = $("mesh-iface-udp");
  const syncKind = () => {
    const k = kind.value;
    const usesAddr = k === "tcp" || k === "backbone";
    addr.disabled = !usesAddr;
    addr.classList.toggle("hidden", !usesAddr);
    addrLabel.classList.toggle("hidden", !usesAddr);
    udpWrap.classList.toggle("hidden", k !== "udp");
  };
  kind.addEventListener("change", syncKind);
  syncKind();
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const body = { kind: kind.value };
    if (kind.value === "tcp" || kind.value === "backbone") {
      const a = addr.value.trim();
      if (!a) { showError("That connection needs an address, like hub.example.org:4789."); return; }
      body.addr = a;
    } else if (kind.value === "udp") {
      const local = ($("mesh-iface-local").value || "").trim();
      const peer = ($("mesh-iface-peer").value || "").trim();
      if (!local || !peer) { showError("A UDP connection needs both a local address and a peer."); return; }
      body.local = local;
      body.peer = peer;
    }
    const name = $("mesh-iface-ifac").value.trim();
    const pass = $("mesh-iface-pass").value;
    if (name) body.ifac_name = name;
    if (pass) body.ifac_passphrase = pass;
    const btn = form.querySelector("button[type=submit]");
    btn.disabled = true;
    try {
      await withInFlight(api("POST", "/mesh/interfaces", body));
      clearError();
      // The passphrase is a secret; do not leave it sitting in the form.
      $("mesh-iface-pass").value = "";
      toast("Connection added.");
      renderMeshInterfaces();
    } catch (err) {
      showError((err && err.error) || "Could not add that connection.");
    } finally {
      btn.disabled = false;
    }
  });
}

// ---------- installed game files ----------

// The node's copies of game files (`content/<game>/<version>`) and its
// downloads. Packs are the Games page; this is what the downloads put on
// disk. Read only while this page is open: sizes come off the disk.
async function pollInstalled() {
  const s = state.installed;
  if (s.timer) { clearTimeout(s.timer); s.timer = null; }
  if (s.busy || !state.token || !s.supported) return;
  if (state.activeTab !== "installed" || document.hidden) return;
  s.busy = true;
  try {
    ingestInstalled(await api("GET", "/content"));
  } catch (e) {
    if (e && e.__auth) return;
    // An agent older than this page has no GET /content.
    if (e && (e.status === 404 || e.status === 405)) { s.supported = false; renderInstalled(); return; }
    if (e && e.__network) showError(e.error);
  } finally {
    s.busy = false;
  }
  if (state.token && state.activeTab === "installed" && !s.timer) s.timer = setTimeout(pollInstalled, INSTALLED_MS);
}

function ingestInstalled(body) {
  const s = state.installed;
  s.copies = Array.isArray(body && body.copies) ? body.copies : [];
  s.downloads = Array.isArray(body && body.downloads) ? body.downloads : [];
  const now = Date.now();
  const seen = new Set();
  for (const d of s.downloads) {
    const bytes = (d.status && d.status.bytes) || 0;
    const last = s.prev.get(d.game);
    // A rate needs two readings of the same download. Bytes can fall when
    // steamcmd moves files out of its download area, which is not a negative
    // speed: keep the last rate instead.
    if (last && now > last.at && bytes >= last.bytes) s.rate.set(d.game, (bytes - last.bytes) * 1000 / (now - last.at));
    s.prev.set(d.game, { bytes, at: now });
    seen.add(d.game);
  }
  for (const g of [...s.prev.keys()]) if (!seen.has(g)) { s.prev.delete(g); s.rate.delete(g); }
  renderInstalled();
  renderInstalledBadge();
}

// Downloads first, then new builds nobody has hidden, then the copy count.
function renderInstalledBadge() {
  const s = state.installed;
  const fresh = [...state.updates.games.values()].filter(g => g.available === true && !g.snoozed).length;
  const badge = $("nav-installed-count");
  badge.textContent = s.downloads.length ? "↓" + s.downloads.length
    : fresh ? "↑" + fresh
    : (s.copies.length ? String(s.copies.length) : "");
  badge.title = s.downloads.length ? "Downloading" : fresh ? fresh + " game" + (fresh === 1 ? " has" : "s have") + " a new build" : "";
}

// ---------- update checks and the reserve (updates.rs) ----------

const UPDATES_MS = 60000;

async function loadUpdates() {
  const u = state.updates;
  if (u.timer) { clearTimeout(u.timer); u.timer = null; }
  if (!state.token || !u.supported) return;
  try {
    ingestUpdates(await api("GET", "/updates"));
  } catch (e) {
    if (e && e.__auth) return;
    if (e && (e.status === 404 || e.status === 405)) { u.supported = false; renderUpdatesBar(); return; }
  }
  if (state.token && !u.timer) u.timer = setTimeout(loadUpdates, UPDATES_MS);
}

function ingestUpdates(body) {
  const u = state.updates;
  if (!body) return;
  u.settings = body.settings || null;
  u.reserve = body.reserve_bytes ?? null;
  u.defaultReserve = body.default_reserve_bytes ?? null;
  u.games = new Map((Array.isArray(body.games) ? body.games : []).map(g => [g.game_id, g]));
  u.moves = Array.isArray(body.moves) ? body.moves : [];
  renderUpdatesBar();
  renderInstalled();
  renderInstalledBadge();
}

const GB = 1024 * 1024 * 1024;

// The settings strip. Left alone while the operator is typing in it.
function renderUpdatesBar() {
  const u = state.updates;
  const bar = $("updates-bar");
  if (!bar) return;
  bar.classList.toggle("hidden", !u.supported || !u.settings);
  if (!u.settings) return;
  const every = $("updates-every");
  if (document.activeElement !== every) every.value = u.settings.check_every_hours ? String(u.settings.check_every_hours) : "";
  if (every.value === "" && u.settings.check_every_hours) {
    // An interval set elsewhere that the list does not offer: show it rather than "Off".
    const o = el("option", null, "Every " + u.settings.check_every_hours + " hours");
    o.value = String(u.settings.check_every_hours);
    every.appendChild(o);
    every.value = o.value;
  }
  const reserve = $("updates-reserve");
  if (document.activeElement !== reserve) {
    reserve.value = u.settings.reserve_bytes != null ? String(Math.round(u.settings.reserve_bytes / GB * 10) / 10) : "";
    reserve.placeholder = u.defaultReserve ? String(Math.round(u.defaultReserve / GB * 10) / 10) : "";
  }
  renderGameSettings();
  $("updates-reserve-note").textContent = u.reserve
    ? "A download does not start if it would leave less than " + formatBytes(u.reserve) + " free, and stops if the disk gets there." +
      (u.settings.reserve_bytes == null ? " Empty is the default: a tenth of the disk, at least 5 GB." : "")
    : "";
}

// Every game's own choices (updates.rs GamePolicy), saved one field at a time.
// Rebuilt from GET /updates only, and never while a control in it has focus.
function renderGameSettings() {
  const u = state.updates;
  const box = $("updates-games");
  const list = $("updates-games-list");
  if (!box || !list) return;
  const games = [...u.games.values()].sort((a, b) => gameName(a.game_id).localeCompare(gameName(b.game_id)));
  box.classList.toggle("hidden", !u.supported || !games.length);
  if (list.contains(document.activeElement)) return;
  list.textContent = "";
  const head = el("div", "ug-row ug-head");
  head.append(el("div", null, "Game"), el("div", null, "Check for new builds"), el("div", null, "When a new build is found"),
    el("div", null, "Servers move to it"));
  list.appendChild(head);
  const nodeEvery = u.settings && u.settings.check_every_hours;
  for (const n of games) {
    const p = n.policy || {};
    const row = el("div", "ug-row");
    const select = (label, options, value, onChange) => {
      const wrap = el("label", "ug-field");
      wrap.dataset.label = label;
      const sel = el("select", "field");
      for (const [v, text] of options) {
        const o = el("option", null, text);
        o.value = v;
        sel.appendChild(o);
      }
      sel.value = value;
      sel.setAttribute("aria-label", gameName(n.game_id) + ": " + label);
      sel.addEventListener("change", () => onChange(sel.value));
      wrap.appendChild(sel);
      return wrap;
    };
    const patch = body => saveGamePolicy(n.game_id, body);
    const checks = p.checks_off ? "off" : p.check_every_hours ? String(p.check_every_hours) : "";
    const checkOptions = [["", "Like the node (" + (nodeEvery ? everyText(nodeEvery) : "off") + ")"], ["off", "Never"],
      ["6", "Every 6 hours"], ["12", "Every 12 hours"], ["24", "Every day"], ["168", "Every week"]];
    if (checks && !checkOptions.some(([v]) => v === checks)) checkOptions.push([checks, "Every " + checks + " hours"]);
    row.append(el("div", "ug-name", gameName(n.game_id)),
      select("Check for new builds", checkOptions, checks, v => patch(v === "off" ? { checks_off: true }
        : { checks_off: false, check_every_hours: v ? Number(v) : 0 })),
      select("When a new build is found", [["notify", "Tell me"], ["download", "Download it"]], p.on_new_build || "notify",
        v => patch({ on_new_build: v })),
      select("Servers move to it", [["never", "Never"], ["when_empty", "When empty"], ["window", "In a daily window"], ["once", "On a date"]],
        p.move_servers || "never", v => {
          if (v === "once" && p.move_at_unix == null) {
            // A date needs one: tomorrow at 05:00 here, changeable below.
            const d = new Date(); d.setDate(d.getDate() + 1); d.setHours(5, 0, 0, 0);
            return patch({ move_servers: v, move_at_unix: Math.floor(d.getTime() / 1000) });
          }
          return patch({ move_servers: v });
        }));
    if (p.move_servers === "window" || p.move_servers === "once") {
      const more = el("div", "ug-more");
      if (p.move_servers === "window") {
        const hours = el("div", "ug-hours");
        const hourSel = (value, key) => {
          const sel = el("select", "field");
          for (let h = 0; h < 24; h++) { const o = el("option", null, localHour(h)); o.value = String(h); sel.appendChild(o); }
          sel.value = String(value ?? 0);
          sel.addEventListener("change", () => patch({ [key]: Number(sel.value) }));
          return sel;
        };
        hours.append(hourSel(p.window_start_utc, "window_start_utc"), el("span", "muted small", "to"), hourSel(p.window_end_utc, "window_end_utc"));
        more.appendChild(hours);
      } else {
        const at = el("input", "field");
        at.type = "datetime-local";
        const d = new Date((p.move_at_unix || 0) * 1000);
        const pad = x => String(x).padStart(2, "0");
        at.value = d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) + "T" + pad(d.getHours()) + ":" + pad(d.getMinutes());
        at.setAttribute("aria-label", gameName(n.game_id) + ": move on");
        at.addEventListener("change", () => { const t = new Date(at.value).getTime(); if (t) patch({ move_at_unix: Math.floor(t / 1000) }); });
        more.appendChild(at);
      }
      const force = el("label", "check");
      const box2 = el("input");
      box2.type = "checkbox";
      box2.checked = !!p.force_in_window;
      box2.addEventListener("change", () => patch({ force_in_window: box2.checked }));
      force.append(box2, el("span", "small", "Also with players on, after a warning in game"));
      more.appendChild(force);
      row.appendChild(more);
    }
    list.appendChild(row);
  }
}

function everyText(h) {
  return h % 168 === 0 ? (h === 168 ? "every week" : "every " + h / 168 + " weeks")
    : h % 24 === 0 ? (h === 24 ? "every day" : "every " + h / 24 + " days") : "every " + h + " hours";
}

async function saveGamePolicy(gameId, body) {
  try {
    if (document.activeElement && $("updates-games-list").contains(document.activeElement)) document.activeElement.blur();
    ingestUpdates(await withInFlight(api("PUT", "/updates/" + encodeURIComponent(gameId) + "/policy", body)));
    clearError();
    toast(gameName(gameId) + ": saved.");
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "That did not save.");
    renderGameSettings();
  }
}

async function saveUpdateSettings() {
  const hours = $("updates-every").value;
  const gb = $("updates-reserve").value.trim();
  const settings = {
    check_every_hours: hours ? Number(hours) : null,
    reserve_bytes: gb === "" ? null : Math.round(Number(gb) * GB),
  };
  if (gb !== "" && !(Number(gb) >= 0)) { showError("Enter how many GB to keep free, or leave it empty for the default."); return; }
  try {
    ingestUpdates(await withInFlight(api("PUT", "/updates/settings", settings)));
    clearError();
    toast(settings.check_every_hours ? "Checking for new builds every " + (settings.check_every_hours >= 24 && settings.check_every_hours % 24 === 0
      ? (settings.check_every_hours === 24 ? "day" : settings.check_every_hours / 24 + " days") : settings.check_every_hours + " hours") + "."
      : "Saved. New builds are checked only when you ask.");
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "The settings were not saved.");
    renderUpdatesBar();
  }
}

async function onCheckUpdate(gameId) {
  const u = state.updates;
  if (u.checking.has(gameId)) return;
  u.checking.add(gameId);
  renderInstalled();
  try {
    const n = await api("POST", "/updates/" + encodeURIComponent(gameId) + "/check");
    u.games.set(gameId, n);
    clearError();
    if (n.error) showError(gameName(gameId) + ": the check failed. " + n.error);
    else toast(n.available === true ? gameName(gameId) + ": build " + n.latest_build + " is out."
      : n.available === false ? gameName(gameId) + " is up to date." : gameName(gameId) + ": Steam's newest build is " + n.latest_build + ".");
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "The check did not run.");
  } finally {
    u.checking.delete(gameId);
    renderInstalled();
    renderInstalledBadge();
    // A game set to download found builds may have started one.
    pollInstalled();
  }
}

// What a found build does for this game (updates.rs OnNewBuild).
async function onUpdatePolicy(gameId, onNewBuild) {
  try {
    ingestUpdates(await withInFlight(api("PUT", "/updates/" + encodeURIComponent(gameId) + "/policy", { on_new_build: onNewBuild })));
    clearError();
    toast(onNewBuild === "download"
      ? gameName(gameId) + ": new builds download automatically. Servers keep running on this copy."
      : gameName(gameId) + ": new builds wait for you to press Update.");
    pollInstalled();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "That did not save.");
  }
}

async function onSnoozeUpdate(gameId, body) {
  const path = "/updates/" + encodeURIComponent(gameId) + "/snooze";
  try {
    ingestUpdates(await withInFlight(body ? api("POST", path, body) : api("DELETE", path)));
    clearError();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "That did not save.");
  }
}

function instanceName(id) {
  const inst = instById(id);
  return inst && inst.name ? inst.name : id;
}

function formatDate(unix) {
  if (!unix) return "—";
  return new Date(unix * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

function copyStatus(c) {
  if (c.used_by && c.used_by.length) return ["In use", "state-running",
    "Used by " + c.used_by.map(instanceName).join(", ") + ". Remove those servers before deleting these files."];
  if (c.configured) return ["Ready", "state-stopped", "New servers of this game use these files."];
  return ["Unused", "state-unused", "No server uses these files and this node's config does not name them. Deleting them frees the space."];
}

function renderInstalled() {
  const s = state.installed;
  const list = $("installed-list");
  const empty = $("installed-empty");
  const old = $("installed-old");
  if (!list) return;
  old.classList.toggle("hidden", s.supported);
  if (!s.supported) { list.textContent = ""; empty.classList.add("hidden"); return; }
  list.textContent = "";

  // Rows are rebuilt every poll, but each keeps its art element: a new picture
  // per poll is a picture that blinks every two seconds.
  const keep = new Set();
  const artFor = (key, gameId) => {
    keep.add(key);
    let a = s.art.get(key);
    if (!a) { a = art(gameId, "art-thumb"); s.art.set(key, a); }
    return a;
  };
  const downloading = new Set(s.downloads.map(d => d.game));
  for (const d of s.downloads) list.appendChild(buildDownloadRow(d, artFor("dl:" + d.game, d.game)));
  for (const c of s.copies) {
    list.appendChild(buildCopyRow(c, downloading.has(c.game_id), artFor(c.game_id + "/" + c.version, c.game_id)));
  }
  for (const k of [...s.art.keys()]) if (!keep.has(k)) s.art.delete(k);
  empty.classList.toggle("hidden", s.copies.length + s.downloads.length > 0);

  const unused = s.copies.filter(c => c.unused);
  const bar = $("installed-unused");
  bar.classList.toggle("hidden", !unused.length);
  if (unused.length) {
    const bytes = unused.reduce((a, c) => a + (c.bytes || 0), 0);
    $("installed-unused-text").textContent = unused.length + " cop" + (unused.length === 1 ? "y" : "ies") +
      " of game files nothing uses, " + formatBytes(bytes) + ".";
  }
}

function buildCopyRow(c, downloading, artEl) {
  const row = el("div", "copy-row" + (c.unused ? " unused" : ""));
  const artCell = el("div", "art-cell");
  artCell.appendChild(artEl);
  const nameCell = el("div");
  nameCell.append(el("div", "name", gameName(c.game_id)), el("div", "sub", "Version " + c.version + " · " + formatBytes(c.bytes)));
  const [word, cls, why] = copyStatus(c);
  const stateCell = el("div");
  const pill = el("span", "state-pill " + cls, word);
  pill.title = why;
  stateCell.appendChild(pill);
  const usedBy = el("div", "cell", c.used_by && c.used_by.length ? c.used_by.map(instanceName).join(", ") : "—");
  if (!(c.used_by && c.used_by.length)) usedBy.classList.add("muted-em");
  const actions = el("div", "actions");
  const key = c.game_id + "/" + c.version;
  const del = el("button", "btn btn-small" + (c.unused ? " btn-danger" : ""), isDeleting(key) ? "Deleting…" : "Delete…");
  del.type = "button";
  const blocked = (c.used_by && c.used_by.length) ? "Servers use these files. Remove them first."
    : downloading ? "This game is downloading. Cancel the download or wait for it to finish." : "";
  del.disabled = !!blocked || isDeleting(key);
  del.title = blocked || "Delete these files to free " + formatBytes(c.bytes) + ".";
  del.addEventListener("click", () => onDeleteCopy(c));
  if (c.updatable) {
    const upd = el("button", "btn btn-small", "Update…");
    upd.type = "button";
    upd.disabled = downloading;
    upd.title = downloading ? "This game is downloading already." : "Download the newest build into a new copy.";
    upd.addEventListener("click", () => onUpdateGame(c));
    actions.appendChild(upd);
  }
  actions.appendChild(del);
  row.append(artCell, nameCell, stateCell, el("div", "cell", formatBytes(c.bytes)), usedBy,
    el("div", "cell", formatDate(c.modified_unix)), actions);
  const notice = state.updates.games.get(c.game_id);
  if (c.updatable && notice && notice.version === c.version) row.appendChild(buildUpdateLine(notice, downloading));
  for (const m of state.updates.moves) {
    if (m.game_id === c.game_id && m.from === c.version) row.appendChild(buildMoveLine(m));
  }
  return row;
}

// One server on this copy and its move onto the copy new servers use (Agent::move_instance).
function buildMoveLine(m) {
  const line = el("div", "update-line");
  const who = el("strong", null, instanceName(m.instance_id));
  const text = el("span");
  const moving = state.updates.moving.has(m.instance_id);
  if (moving || m.state === "moving") text.append(who, " is moving to version " + m.to + "…");
  else if (m.state === "waiting") text.append(who, " stays on this copy for now, " + m.why + ".");
  else if (m.state === "warned") text.append(who, ": players were told it restarts in a minute for the update.");
  else if (m.state === "moved") text.append(who, " moved to version " + m.to + " " + ago(m.at_unix) + ".");
  else if (m.state === "rolled_back") {
    text.append(who, " did not stay up on version " + m.to + " and is back on this copy: " + m.why + ". It is not tried again until there is a newer copy.");
  } else if (m.state === "failed") {
    text.className = "bad-text";
    text.append(who, (m.lost ? " is on neither copy: " : " was not moved: ") + m.why);
  }
  line.appendChild(text);
  if (m.state === "waiting" && !moving && !/^stopped/.test(m.why || "")) {
    const b = el("button", "link-btn", "Move now");
    b.type = "button";
    b.title = "Restart it on version " + m.to + " now, whoever is playing. It keeps its name, port and address.";
    b.addEventListener("click", () => onMoveNow(m));
    line.appendChild(b);
  }
  return line;
}

async function onMoveNow(m) {
  const name = instanceName(m.instance_id);
  if (!confirm("Move " + name + " to version " + m.to + " now?\n\nIt restarts, so anyone playing is dropped without a warning. " +
    "It keeps its name, port and address, and goes back to version " + m.from + " if it does not stay up.")) return;
  const u = state.updates;
  u.moving.add(m.instance_id);
  renderInstalled();
  try {
    const r = await api("POST", "/instances/" + encodeURIComponent(m.instance_id) + "/move");
    clearError();
    if (r.state === "moved") toast(name + " moved to version " + r.to + ".");
    else showError(name + " did not stay up on version " + r.to + " and is back on version " + r.from + ": " + r.why);
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "The move did not run.");
  } finally {
    u.moving.delete(m.instance_id);
    loadUpdates();
    pollInstalled();
  }
}

// A UTC hour as this browser's clock reads it.
function localHour(utcHour) {
  return new Date(Date.UTC(2000, 0, 1, utcHour)).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function moveSummary(p) {
  if (!p || p.move_servers === "never") return "Servers stay on their copy";
  if (p.move_servers === "when_empty") return "Servers move when empty";
  if (p.move_servers === "once") return "Servers move from " + new Date((p.move_at_unix || 0) * 1000).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) +
    (p.force_in_window ? ", players warned" : ", when empty");
  return "Servers move " + localHour(p.window_start_utc) + "–" + localHour(p.window_end_utc) +
    (p.force_in_window ? ", players warned" : ", when empty");
}

// Open the per-game settings at this game's row.
function onMovePolicy(n) {
  const box = $("updates-games");
  if (!box) return;
  box.open = true;
  renderGameSettings();
  const sel = box.querySelector('select[aria-label="' + CSS.escape(gameName(n.game_id)) + ': Servers move to it"]');
  box.scrollIntoView({ behavior: "smooth", block: "start" });
  if (sel) sel.focus({ preventScroll: true });
}

function ago(unix) {
  if (!unix) return "";
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 60) return "just now";
  if (s < 3600) return Math.floor(s / 60) + " min ago";
  if (s < 86400) return Math.floor(s / 3600) + " h ago";
  return Math.floor(s / 86400) + " d ago";
}

// What is known about this copy's newest build, and what can be done about it.
// Built from the last GET /updates; the copies poll never changes it.
function buildUpdateLine(n, downloading) {
  const line = el("div", "update-line");
  const checking = state.updates.checking.has(n.game_id);
  const btn = (text, title, fn) => {
    const b = el("button", "link-btn", text);
    b.type = "button";
    b.title = title;
    b.disabled = checking;
    b.addEventListener("click", fn);
    return b;
  };
  const checkNow = btn("Check now", "Ask Steam for the newest build of this game (about half a minute).", () => onCheckUpdate(n.game_id));
  if (checking) {
    line.appendChild(el("span", null, "Checking with Steam…"));
    return line;
  }
  const when = n.checked_unix ? " Checked " + ago(n.checked_unix) + "." : "";
  const auto = n.on_new_build === "download";
  if (n.available === true && !n.snoozed) {
    const text = el("span");
    text.append(el("strong", "new", "New build " + n.latest_build), " — this copy has " + n.installed_build + "." + when +
      (downloading ? (auto ? " Downloading it by itself." : "")
        : auto ? " It downloads by itself when there is room; Update… does it now."
        : " Update… downloads it beside this copy."));
    line.appendChild(text);
    line.append(
      btn("Remind me tomorrow", "Hide this for a day.", () => onSnoozeUpdate(n.game_id, { hours: 24 })),
      btn("Skip build " + n.latest_build, "Hide this until a newer build than " + n.latest_build + " comes out.",
        () => onSnoozeUpdate(n.game_id, { skip_build: true })),
      checkNow);
  } else if (n.available === true) {
    const why = n.skip_build === n.latest_build ? "skipped" :
      "hidden until " + new Date(n.snooze_until_unix * 1000).toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit" });
    line.appendChild(el("span", null, "Build " + n.latest_build + " is out, " + why + "." + when));
    line.append(btn("Show", "Show this notice again.", () => onSnoozeUpdate(n.game_id, null)), checkNow);
  } else if (n.available === false) {
    line.appendChild(el("span", null, "Up to date (build " + n.installed_build + ")." + when));
    line.appendChild(checkNow);
  } else if (!n.installed_build && n.latest_build) {
    line.appendChild(el("span", null, "Steam's newest build is " + n.latest_build +
      "; this copy has no Steam manifest, so its own build is not known." + when));
    line.appendChild(checkNow);
  } else {
    line.appendChild(el("span", null, n.checked_unix ? "No answer from Steam yet." : "Not checked for new builds yet."));
    line.appendChild(checkNow);
  }
  line.appendChild(btn(moveSummary(n.policy) + " · change", "Choose when servers move to a new copy after an update.", () => onMovePolicy(n)));
  line.appendChild(auto
    ? btn("Downloads new builds automatically · turn off", "Go back to being told about a new build and pressing Update yourself.",
      () => onUpdatePolicy(n.game_id, "notify"))
    : btn("Download new builds automatically", "When a check finds a new build, download it beside this copy, within the disk reserve. " +
      "Servers keep running on this copy; nothing is deleted.", () => onUpdatePolicy(n.game_id, "download")));
  if (n.error) {
    const err = el("span", "bad-text", "Last check failed: " + n.error);
    err.title = n.error;
    line.appendChild(err);
  }
  return line;
}
function isDeleting(key) { return state.installed.deleting.has(key); }

function buildDownloadRow(d, artEl) {
  const st = d.status || {};
  const row = el("div", "copy-row downloading");
  const artCell = el("div", "art-cell");
  artCell.appendChild(artEl);
  if (st.state === "queued") {
    // One download at a time per node (install.rs): nothing is on disk yet.
    const nameCell = el("div");
    nameCell.append(el("div", "name", gameName(d.game)), el("div", "sub", "Waiting for another download"));
    const stateCell = el("div");
    stateCell.appendChild(el("span", "state-pill state-stopped", "Queued"));
    const actions = el("div", "actions");
    const cancel = el("button", "btn btn-small", "Cancel");
    cancel.type = "button";
    cancel.title = "Take it out of the queue.";
    cancel.addEventListener("click", () => onCancelDownload(d.game, cancel));
    actions.appendChild(cancel);
    row.append(artCell, nameCell, stateCell, el("div", "cell muted-em", "—"),
      el("div", "cell muted-em", "waiting " + formatUptime(st.since_secs || 0)), el("div", "cell muted-em", "—"), actions);
    return row;
  }
  const nameCell = el("div");
  nameCell.append(el("div", "name", gameName(d.game)),
    el("div", "sub", "Downloading · " + formatBytes((st.bytes || 0)) + " so far"));
  const stateCell = el("div");
  stateCell.appendChild(el("span", "state-pill state-starting", "Downloading"));
  const bytes = st.bytes || 0;
  const rate = state.installed.rate.get(d.game);
  // A percentage only with a total someone actually knows; otherwise bytes
  // and rate, never an invented end (install.rs, InstallState::Running).
  const total = st.total_bytes;
  const sizeCell = el("div", "cell", formatBytes(bytes) + (total ? " of " + formatBytes(total) : ""));
  const progress = el("div", "cell download-progress");
  if (total) {
    const bar = el("div", "dl-bar");
    const fill = el("i");
    fill.style.width = Math.min(100, Math.round(100 * bytes / total)) + "%";
    bar.appendChild(fill);
    progress.appendChild(bar);
  }
  progress.appendChild(el("span", null, (rate != null ? formatRate(rate) + " · " : "") + formatUptime(st.since_secs || 0)));
  progress.title = "Bytes on this node's disk so far, the rate between the last two readings, and how long it has run.";
  const actions = el("div", "actions");
  const cancel = el("button", "btn btn-small", "Cancel");
  cancel.type = "button";
  cancel.title = "Stop the download and discard what it fetched.";
  cancel.addEventListener("click", () => onCancelDownload(d.game, cancel));
  actions.appendChild(cancel);
  row.append(artCell, nameCell, stateCell, sizeCell, progress, el("div", "cell muted-em", "—"), actions);
  return row;
}

// An update downloads the newest build into a new copy, beside the old one:
// servers running now keep the files they started on, new ones get the new
// build. The checkbox removes the old copy too, at once if no server uses it,
// otherwise as soon as the last one using it is removed.
function onUpdateGame(c) {
  const name = gameName(c.game_id);
  openModal("update-dialog", c.game_id, (body, close) => {
    body.appendChild(el("h3", null, "Update " + name));
    body.appendChild(el("p", "muted small",
      "Downloads the newest build into a new copy beside version " + c.version + ". Only what changed is downloaded, " +
      "and anything added to this copy, such as bots, comes along. New servers use the new copy; servers that exist now keep theirs " +
      "until they move, as chosen under the game's row (" + moveSummary(state.updates.games.get(c.game_id)?.policy).toLowerCase() + ")."));
    const wrap = el("label", "check");
    const removeOld = el("input");
    removeOld.type = "checkbox";
    removeOld.id = "upd-remove-old";
    removeOld.checked = true;
    wrap.append(removeOld, el("span", null, "Remove the old version when the update is done"));
    const used = (c.used_by || []).map(instanceName);
    body.append(wrap, el("p", "muted small", used.length
      ? "Used now by " + used.join(", ") + ": it is removed once " + (used.length === 1 ? "that server is" : "those servers are") + " removed."
      : "Nothing uses it now, so it is removed as soon as the new copy is ready (" + formatBytes(c.bytes) + " freed)."));
    const reserve = state.updates.reserve;
    body.appendChild(el("p", "muted small",
      "Needs about " + formatBytes(Math.round(c.bytes * 1.1)) + " free while it runs: the copy is made first, then updated." +
      (reserve ? " It does not start if that would leave less than " + formatBytes(reserve) +
        " free, and stops if the disk gets there, so running servers never run out of space." : "")));
    const actions = el("div", "form-actions");
    actions.style.marginTop = "8px";
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const go = el("button", "btn btn-primary", "Update");
    go.type = "button";
    go.addEventListener("click", async () => {
      // The node decides whether it fits (Agent::room_for); a refusal
      // arrives through watchUpdate with the copies that would make room.
      go.disabled = true; go.textContent = "Starting…";
      try {
        await withInFlight(api("POST", "/content/" + encodeURIComponent(c.game_id), { update: true, remove_old: removeOld.checked }));
        clearError();
        close();
        toast("Updating " + name + ".");
        watchUpdate(c.game_id);
        pollInstalled();
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "The update did not start.");
        go.disabled = false; go.textContent = "Update";
      }
    });
    actions.append(cancel, go);
    body.appendChild(actions);
  });
}

// Say how an update ended, once. The Installed list shows the rest.
async function watchUpdate(gameId) {
  for (;;) {
    await new Promise(r => setTimeout(r, 3000));
    let body;
    try { body = await api("GET", "/content/" + encodeURIComponent(gameId)); }
    catch (e) { if (e && e.__auth) return; continue; }
    const st = (body && body.status) || {};
    if (st.state === "running" || st.state === "queued") continue;
    if (st.state === "failed") {
      if (st.error !== "cancelled") showError(gameName(gameId) + " was not updated: " + st.error);
      return;
    }
    const u = st.update;
    if (st.state === "done" && u) {
      if (!u.to_version) toast(gameName(gameId) + " is already up to date.");
      else toast(gameName(gameId) + " updated to " + u.to_version + "." +
        (u.old === "removed" ? " The old version was removed." :
         u.old === "waiting" ? " The old version is removed once " + u.old_used_by.map(instanceName).join(", ") + (u.old_used_by.length === 1 ? " is" : " are") + " removed." : ""));
    }
    pollInstalled();
    return;
  }
}

async function onDeleteCopy(c) {
  const key = c.game_id + "/" + c.version;
  if (!confirm("Delete " + gameName(c.game_id) + " version " + c.version + "?\n\n" +
    "This frees " + formatBytes(c.bytes) + ". No server uses these files. " +
    "The game stays on the Games page and can be downloaded again.\n\nThis cannot be undone.")) return;
  state.installed.deleting.add(key);
  renderInstalled();
  try {
    const r = await withInFlight(api("DELETE", "/content/" + encodeURIComponent(c.game_id) + "/" + encodeURIComponent(c.version)));
    clearError();
    toast("Deleted " + gameName(c.game_id) + " " + c.version + ", " + formatBytes(r && r.bytes) + " freed.");
  } catch (e) {
    if (e && e.__auth) return;
    const servers = e && e.data && Array.isArray(e.data.servers) ? e.data.servers : [];
    showError(servers.length
      ? "These files are used by " + servers.map(instanceName).join(", ") + ". Remove " + (servers.length === 1 ? "that server" : "those servers") + " first."
      : (e && e.error) || "The files were not deleted.");
  } finally {
    state.installed.deleting.delete(key);
  }
  pollInstalled();
}

async function onCancelDownload(game, btn) {
  if (!confirm("Cancel downloading " + gameName(game) + "?\n\nWhat it has fetched so far is discarded.")) return;
  btn.disabled = true;
  btn.textContent = "Cancelling…";
  try {
    await withInFlight(api("DELETE", "/content/" + encodeURIComponent(game)));
    clearError();
    toast("Download cancelled.");
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "The download was not cancelled.");
  }
  pollInstalled();
}

async function onDeleteUnused() {
  const unused = state.installed.copies.filter(c => c.unused);
  if (!unused.length) return;
  const bytes = unused.reduce((a, c) => a + (c.bytes || 0), 0);
  if (!confirm("Delete " + unused.length + " unused cop" + (unused.length === 1 ? "y" : "ies") + " of game files (" + formatBytes(bytes) + ")?\n\n" +
    unused.map(c => "• " + gameName(c.game_id) + " " + c.version + " — " + formatBytes(c.bytes)).join("\n") +
    "\n\nNo server uses them and this node's config does not name them.\n\nThis cannot be undone.")) return;
  let freed = 0, failed = null;
  for (const c of unused) {
    try {
      const r = await withInFlight(api("DELETE", "/content/" + encodeURIComponent(c.game_id) + "/" + encodeURIComponent(c.version)));
      freed += (r && r.bytes) || 0;
    } catch (e) {
      if (e && e.__auth) return;
      failed = failed || (e && e.error) || "A copy was not deleted.";
    }
  }
  if (failed) showError(failed); else clearError();
  toast(formatBytes(freed) + " freed.");
  pollInstalled();
}

// ---------- tabs ----------

const TABS = ["servers", "games", "installed", "interfaces"];
const TITLES = { servers: "Servers", games: "Games", installed: "Installed", interfaces: "Mesh" };

function setActiveTab(name) {
  if (!TABS.includes(name)) name = "servers";
  state.activeTab = name;
  for (const t of TABS) {
    const btn = $("tab-" + t);
    const on = t === name;
    btn.classList.toggle("active", on);
    btn.setAttribute("aria-selected", String(on));
    $("tab-panel-" + t).classList.toggle("hidden", !on);
  }
  $("page-title").textContent = TITLES[name];
  // Fetched on entry rather than polled: rare to change, never stale when
  // the operator is looking.
  if (name === "interfaces") loadInterfaces();
  if (name === "games") renderGames();
  if (name === "servers") pollUsage();
  if (name === "installed") pollInstalled();
}

function renderAll() {
  renderStatusPill();
  renderInstances();
  renderGames();
}

// ---------- bootstrap ----------

document.addEventListener("DOMContentLoaded", () => {
  $("error-dismiss").addEventListener("click", clearError);

  $("token-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const v = $("token-input").value;
    $("token-error").textContent = "";
    if (!v) { $("token-error").textContent = "Enter the token."; return; }
    tryConnect(v.trim());
  });

  $("disconnect-btn").addEventListener("click", () => {
    setToken(null);
    state.capacity = null;
    state.instances = [];
    state.games = [];
    state.interfaces = [];
    state.rows.forEach(r => r.remove());
    state.rows.clear();
    state.usage.prev.clear(); state.usage.now.clear(); state.usage.hist.clear();
    state.usage.supported = true;
    state.usage.players.clear();
    state.storage = null;
    renderStorage();
    state.orphans = null;
    state.orphansCheckedAt = 0;
    renderOrphans();
    state.installed.copies = [];
    state.installed.downloads = [];
    state.installed.supported = true;
    state.installed.prev.clear(); state.installed.rate.clear();
    state.updates.supported = true;
    state.updates.settings = null;
    state.updates.games.clear();
    renderUpdatesBar();
    $("nav-installed-count").textContent = "";
    renderInstalled();
    state.installing.clear();
    state.installingMsg.clear();
    state.installDone.clear();
    $("games-grid").dataset.sig = "";
    clearError();
    showTokenScreen();
  });

  for (const t of TABS) $("tab-" + t).addEventListener("click", () => setActiveTab(t));
  $("empty-goto-games").addEventListener("click", () => setActiveTab("games"));
  $("new-server-btn").addEventListener("click", () => setActiveTab("games"));
  $("announce-btn").addEventListener("click", () => onAnnounce(null));
  $("orphans-clean").addEventListener("click", onCleanOrphans);
  $("installed-unused-btn").addEventListener("click", onDeleteUnused);
  $("updates-every").addEventListener("change", saveUpdateSettings);
  $("updates-reserve").addEventListener("change", saveUpdateSettings);

  $("iface-kind").addEventListener("change", () => {
    const tcp = $("iface-kind").value === "tcp";
    $("iface-addr-wrap").classList.toggle("hidden", !tcp);
    $("iface-addr-label").classList.toggle("hidden", !tcp);
  });
  $("iface-ifac-toggle").addEventListener("change", () => {
    $("iface-ifac-wrap").classList.toggle("hidden", !$("iface-ifac-toggle").checked);
  });
  $("iface-form").addEventListener("submit", (e) => { e.preventDefault(); onAddInterface(); });
  wireMeshInterfaceForm();

  document.addEventListener("mousedown", (e) => { if (!e.target.closest("#ctx-menu")) closeMenu(); });
  document.addEventListener("scroll", closeMenu, true);
  window.addEventListener("resize", closeMenu);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeMenu();
      for (const id of ["start-dialog", "map-dialog", "bots-dialog", "log-dialog", "players-dialog", "update-dialog"]) closeModal(id);
    }
  });

  // Usage pauses while the page is hidden; pick it up the moment it is back.
  document.addEventListener("visibilitychange", () => { if (!document.hidden && state.token) { pollUsage(); pollInstalled(); } });

  const t = getToken();
  if (t) tryConnect(t);
  else showTokenScreen();
});
