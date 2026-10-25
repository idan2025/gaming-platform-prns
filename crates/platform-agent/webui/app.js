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
  controllerOn: false,   // this page places servers through a controller (controller.rs)
  regions: [],           // the controller's regions, offered on the create form
  hostedGames: [],       // the games the controller hosts: what can be started through it
  isAdmin: false,        // this node's identity may decide region requests on the controller
  role: "admin",         // this page's role here: the token and an admin account are "admin" (accounts.rs)
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
  removedBackups: null,  // [{instance_id, backups}] of removed servers, from /backups
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
  archive: '<path d="M4 5h16v4H4zM5 9v10h14V9M10 13h4"/>',
  edit: '<path d="M4 20h4L19 9l-4-4L4 16v4z"/>',
  link: '<path d="M10 14a4 4 0 005.7 0l3-3a4 4 0 00-5.7-5.7l-1 1"/><path d="M14 10a4 4 0 00-5.7 0l-3 3a4 4 0 005.7 5.7l1-1"/>',
  log: '<rect x="4" y="3" width="16" height="18" rx="2"/><path d="M8 8h8M8 12h8M8 16h5"/>',
  users: '<circle cx="9" cy="8" r="3.2"/><path d="M3 20c0-3.3 2.7-6 6-6s6 2.7 6 6"/><path d="M16 4.5a3.2 3.2 0 010 6.2M18 14.5c1.8.8 3 2.7 3 5"/>',
  shield: '<path d="M12 3l7 3v6c0 4.2-3 7.6-7 9-4-1.4-7-4.8-7-9V6z"/>',
  globe: '<circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17M12 3.5c2.5 2.6 2.5 14.4 0 17M12 3.5c-2.5 2.6-2.5 14.4 0 17"/>',
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

// With a controller, the server actions the controller carries go through it,
// and the list shows every worker's servers. This mirrors controller::allowed_path
// on the node, so a request the node would refuse is refused here first, with
// the same words, and nothing is sent. Anything else on a server stays local.
const CONTROLLER_ROUTES = [
  [/^\/instances$/, ["GET", "POST"]],
  [/^\/instances\/[a-z0-9-]{1,64}$/, ["GET", "DELETE"]],
  [/^\/instances\/[a-z0-9-]{1,64}\/(stop|start|relocate)$/, ["POST"]],
  [/^\/instances\/[a-z0-9-]{1,64}\/logs(\?lines=\d{1,4})?$/, ["GET"]],
  [/^\/instances\/[a-z0-9-]{1,64}\/backups$/, ["GET", "POST"]],
  [/^\/instances\/[a-z0-9-]{1,64}\/backups\/[A-Za-z0-9._-]+$/, ["DELETE"]],
  [/^\/instances\/[a-z0-9-]{1,64}\/backups\/[A-Za-z0-9._-]+\/restore$/, ["POST"]],
  [/^\/instances\/[a-z0-9-]{1,64}\/move-targets$/, ["GET"]],
  [/^\/instances\/[a-z0-9-]{1,64}\/region-request$/, ["POST"]],
];

// The URL a request really goes to: through the controller when this page has
// one and the controller carries the request; the node's own path otherwise.
function routeFor(method, path) {
  if (!state.controllerOn || !path.startsWith("/instances")) return path;
  const allowed = CONTROLLER_ROUTES.some(([re, methods]) => re.test(path) && methods.includes(method));
  if (!allowed) {
    throw { error: "Not available through the controller yet. This server's own page keeps it.", status: 404 };
  }
  return "/controller" + path;
}

async function loadControllerStatus() {
  try {
    const s = await api("GET", "/controller/status");
    state.controllerOn = !!(s && s.enabled);
    const note = $("controller-note");
    if (note) {
      note.hidden = !state.controllerOn;
      note.textContent = state.controllerOn ? "Servers are placed through the controller at " + s.url + ", across every worker it drives." : "";
    }
    state.regions = [];
    state.hostedGames = [];
    state.isAdmin = false;
    if (state.controllerOn) {
      const h = await api("GET", "/controller/hosting").catch(() => null);
      if (h && Array.isArray(h.regions)) state.regions = h.regions.filter(r => typeof r === "string");
      if (h && Array.isArray(h.games)) state.hostedGames = h.games.filter(g => typeof g === "string");
      // The index says who is admin: its list answers only an admin.
      state.isAdmin = await api("GET", "/controller/admin/region-requests").then(() => true, () => false);
    }
  } catch (_) {
    state.controllerOn = false;
    state.regions = [];
    state.hostedGames = [];
    state.isAdmin = false;
  }
  const reqTab = $("tab-requests");
  if (reqTab) reqTab.hidden = !state.controllerOn;
}

async function api(method, path, body) {
  const url = routeFor(method, path);
  const opts = { method, headers: { "Authorization": "Bearer " + (state.token || "") } };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  let resp;
  try {
    resp = await fetch(url, opts);
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
  const url = routeFor("GET", path);
  let resp;
  try {
    resp = await fetch(url, { headers: { "Authorization": "Bearer " + (state.token || "") } });
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

// How this node lets a person in (accounts.rs): sign in with an account, make
// the first account with the API token, or paste the token. A node with no
// token is loopback-only and open, so it just connects.
let loginMode = "account";
let authState = null;
let autoConnected = false;

async function showTokenScreen() {
  stopPolling();
  $("token-screen").classList.remove("hidden");
  $("main-ui").classList.add("hidden");
  for (const id of ["token-input", "login-user", "login-pass", "login-code", "setup-token", "setup-user", "setup-pass", "setup-pass2", "signup-user", "signup-pass", "signup-pass2"]) {
    const e = $(id);
    if (e) e.value = "";
  }
  $("login-code-wrap").hidden = true;
  try { authState = await (await fetch("/auth/state")).json(); } catch (_) { authState = null; }
  if (authState && !authState.token_required && !autoConnected) {
    autoConnected = true;
    tryConnect(null);
    return;
  }
  loginMode = !authState || !authState.accounts ? "token" : (authState.has_accounts ? "account" : "setup");
  renderLoginMode();
}

function renderLoginMode() {
  $("login-form").hidden = loginMode !== "account";
  $("setup-form").hidden = loginMode !== "setup";
  $("signup-form").hidden = loginMode !== "signup";
  $("report-form").hidden = loginMode !== "report";
  $("report-switch-wrap").hidden = !(authState && authState.signup && authState.signup.reports);
  $("report-switch").textContent = loginMode === "report" ? "Back to sign in" : "Report a server";
  $("token-form").hidden = loginMode !== "token";
  // Sign-up, where this node takes it: always with its two notices linked.
  const signup = authState && authState.signup;
  $("signup-switch-wrap").hidden = !signup || (loginMode !== "account" && loginMode !== "signup");
  $("signup-switch").textContent = loginMode === "signup" ? "I have an account: sign in" : "New here? Create an account";
  if (signup) {
    $("signup-terms-link").href = signup.terms_url;
    $("signup-privacy-link").href = signup.privacy_url;
  }
  $("login-help").textContent = loginMode === "report" ? "Tell the admins about a server here that carries illegal content or breaks the rules. No account needed."
    : loginMode === "signup" ? "Create an account to run servers here."
    : loginMode === "account" ? "Sign in to this node."
    : loginMode === "setup" ? "Make the account you will sign in with from now on. The API token proves this is your node: it is in the file named by api_token_file in the agent's config. You need it only this once."
    : "Paste this node's API token, from the file named by api_token_file in the agent's config.";
  const sw = $("login-switch");
  const canAccounts = authState && authState.accounts;
  sw.parentElement.hidden = !canAccounts;
  sw.textContent = loginMode === "token" ? (authState && authState.has_accounts ? "Sign in with an account instead" : "Make an account instead")
    : "Use the API token instead";
  const first = { account: "login-user", setup: "setup-token", signup: "signup-user", report: "report-target", token: "token-input" }[loginMode];
  $(first)?.focus();
}

async function onLoginSubmit(e) {
  e.preventDefault();
  const err = $("token-error");
  err.textContent = "";
  const body = { username: $("login-user").value.trim(), password: $("login-pass").value };
  const code = $("login-code").value.trim();
  if (code) body.code = code;
  let resp, out = {};
  try {
    resp = await fetch("/auth/login", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    out = await resp.json().catch(() => ({}));
  } catch (_) { err.textContent = "Could not reach this node."; return; }
  if (resp.ok && out.session) {
    state.signedInAs = out.username;
    tryConnect(out.session);
    return;
  }
  if (out.needs_code) {
    $("login-code-wrap").hidden = false;
    $("login-code").focus();
  }
  err.textContent = out.error || "Could not sign in.";
}

async function onSetupSubmit(e) {
  e.preventDefault();
  const err = $("token-error");
  err.textContent = "";
  const username = $("setup-user").value.trim();
  const password = $("setup-pass").value;
  if (password !== $("setup-pass2").value) { err.textContent = "The two passwords are not the same."; return; }
  let resp, out = {};
  try {
    resp = await fetch("/auth/setup", { method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token: $("setup-token").value.trim(), username, password }) });
    out = await resp.json().catch(() => ({}));
  } catch (_) { err.textContent = "Could not reach this node."; return; }
  if (!resp.ok) { err.textContent = out.error || "The account was not made."; return; }
  // Made: sign straight in with it.
  $("login-user").value = username;
  $("login-pass").value = password;
  loginMode = "account";
  renderLoginMode();
  $("login-form").requestSubmit();
}
async function onSignupSubmit(e) {
  e.preventDefault();
  const err = $("token-error");
  err.textContent = "";
  const username = $("signup-user").value.trim();
  const password = $("signup-pass").value;
  if (password !== $("signup-pass2").value) { err.textContent = "The two passwords are not the same."; return; }
  if (!$("signup-terms").checked || !$("signup-age").checked) { err.textContent = "Tick both boxes to create an account."; return; }
  let resp, out = {};
  try {
    resp = await fetch("/auth/signup", { method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ username, password, accept_terms: true, old_enough: true }) });
    out = await resp.json().catch(() => ({}));
  } catch (_) { err.textContent = "Could not reach this node."; return; }
  if (!resp.ok) { err.textContent = out.error || "The account was not made."; return; }
  $("login-user").value = username;
  $("login-pass").value = password;
  loginMode = "account";
  renderLoginMode();
  $("login-form").requestSubmit();
}

async function onReportSubmit(e) {
  e.preventDefault();
  const err = $("token-error");
  err.textContent = "";
  let resp, out = {};
  try {
    resp = await fetch("/reports", { method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ category: $("report-category").value, target: $("report-target").value, details: $("report-details").value, contact: $("report-contact").value }) });
    out = await resp.json().catch(() => ({}));
  } catch (_) { err.textContent = "Could not reach this node."; return; }
  if (!resp.ok) { err.textContent = out.error || "The report was not sent."; return; }
  for (const id of ["report-target", "report-details", "report-contact"]) $(id).value = "";
  err.textContent = "Thank you. The report was received (" + out.id + ").";
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
    if (Date.now() - state.orphansCheckedAt >= ORPHANS_EVERY_MS) refreshRemovedBackups();
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
  // This node's disk holds no server's files when a controller places them.
  line.classList.toggle("hidden", !s || state.controllerOn);
  if (!s || state.controllerOn) return;
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
    await loadControllerStatus();
    // The Account tab exists where this node has accounts.
    const acct = $("tab-account");
    if (acct) acct.hidden = !(authState ? authState.accounts : !!token);
    // A user runs servers; an admin also manages the node (api::user_may).
    const me = await api("GET", "/auth/me").catch(() => ({ role: "admin" }));
    state.role = me && me.role === "user" ? "user" : "admin";
    applyRole();
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
  // Through a controller the list is every worker's servers, so count those;
  // this node's own slots say nothing about the cluster.
  const running = state.controllerOn ? state.instances.filter(i => i.state === "running").length
    : (cap.running != null ? cap.running : 0);
  const max = state.controllerOn ? null : (cap.max_instances != null ? cap.max_instances : null);
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

// Said on whatever a controller does not carry (controller::allowed_path).
const NOT_THROUGH_CONTROLLER = "Not through the controller. The worker's own page has it.";

function instById(id) { return state.instances.find(i => i.instance_id === id) || null; }

// What a server can be asked to do right now, and why not when it cannot. One
// place, so the row's buttons and its menu never disagree.
function capabilities(inst) {
  const game = gameOf(inst.game_id);
  const running = inst.state === "running";
  const speaks = !game || game.console !== false;
  const oneWorld = !!(game && game.minecraft);
  const hasBots = !!(game && game.bots);
  return {
    running,
    canStop: !(inst.state === "stopped" || inst.state === "missing"),
    // Through a controller there is start but no restart, and no map change
    // (the controller never types at a server's console).
    canRestart: state.controllerOn ? inst.state === "stopped" : true,
    restartLabel: state.controllerOn ? "Start" : "Restart",
    restartWhy: !state.controllerOn ? "Restart"
      : inst.state === "stopped" ? "Start this server on its worker."
      : "Restart is not carried through the controller: stop it, then start it.",
    canMap: !state.controllerOn && running && speaks && !oneWorld,
    mapWhy: state.controllerOn ? NOT_THROUGH_CONTROLLER
      : oneWorld ? "A Minecraft server has one world. Upload a different one with Upload a world."
      : !running ? "The server has to be running to be told anything."
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
    row.querySelector(".cell-game").textContent = gameName(inst.game_id)
      + (state.controllerOn && inst.node ? " · on " + inst.node + (inst.region ? " (" + inst.region + ")" : "") : "");
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
    } else if (state.controllerOn) {
      // A worker's server has an address while it runs; this node's [mesh]
      // has nothing to do with it.
      meshCell.textContent = inst.state === "running" ? "LAN only" : "—";
      meshCell.title = inst.state === "running" ? "Its worker announces it on no mesh." : "No address while it is stopped.";
      meshCell.classList.add("muted-em");
      meshCell.classList.remove("copyable");
      meshCell.onclick = null;
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
    const restartBtn = row.querySelector(".restart-btn");
    restartBtn.disabled = !cap.canRestart;
    restartBtn.title = cap.restartWhy;
    restartBtn.setAttribute("aria-label", cap.restartLabel);
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
  items.push({ label: cap.restartLabel, icon: "restart", disabled: !cap.canRestart,
    title: state.controllerOn ? cap.restartWhy : undefined, action: () => onRestart(id) });
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
  const hasConsole = state.games.some(g => g.id === inst.game_id && g.console);
  items.push({ label: hasConsole && !state.controllerOn ? "Log and console" : "Show log", icon: "log", action: () => onShowLog(id) });
  items.push({ label: "Players…", icon: "users", disabled: !cap.running,
    title: cap.running ? "Who is on this server, as the game reports it." : "The server has to be running.",
    action: () => onShowPlayers(id) });
  if (hasConsole) {
    items.push({ label: "Kick, ban and operators…", icon: "shield",
      title: "Kick or ban a player, see the ban list, and choose who runs commands in game.",
      action: () => onModeration(id) });
  }
  const isMinecraft = state.games.some(g => g.id === inst.game_id && g.minecraft);
  if (isMinecraft) {
    items.push({ label: "Upload a world…", icon: "globe", disabled: cap.running,
      title: cap.running ? "Stop the server first: its world cannot be swapped while it runs."
        : "Replace this server's world with one from a file. The current world is backed up first.",
      action: () => onUploadWorld(id) });
  }
  if (inst.restart_on_crash != null) {
    items.push({ label: (inst.restart_on_crash ? "✓ " : "") + "Restart if it crashes", icon: "restart",
      title: "Up to " + MAX_CRASH_RESTARTS + " times an hour. A Stop is never undone.",
      action: () => onToggleRestartOnCrash(id, !inst.restart_on_crash) });
  }
  items.push({ label: "Backups…", icon: "archive", title: "Copies of this server's own files (saves, settings, logs), and restoring one.",
    action: () => onBackups(id) });
  items.push({ label: "Daily restart…", icon: "restart", title: "Restart this server once a day in a window you pick, when nobody is playing.",
    action: () => onRestartSchedule(id) });
  items.push({ label: "Chat…", icon: "users", disabled: !inst.mesh_destination,
    title: inst.mesh_destination ? "Talk with the players who joined through Lanthorn, and turn this server's chat on or off."
      : "Only a server on the mesh has chat.",
    action: () => onChat(id) });
  if (state.controllerOn) {
    items.push({ label: "Move to another worker…", icon: "restart",
      title: "Copy its files to the worker with the most room while it runs, then switch over. It keeps its mesh address.",
      action: () => onRelocate(id, inst.name) });
  }
  items.push("sep");
  items.push({ label: "Remove server…", icon: "trash", danger: true, action: () => onRemove(id, inst.name) });
  // Through a controller, only what it carries is live; the rest says where it is.
  if (state.controllerOn) {
    const carried = new Set(["Start", "Stop", "Copy invite link", "Copy mesh address", "Show log", "Log and console",
      "Backups…", "Move to another worker…", "Remove server…"]);
    for (const it of items) {
      if (typeof it === "object" && it.label && !carried.has(it.label)) {
        it.disabled = true;
        it.title = NOT_THROUGH_CONTROLLER;
      }
    }
  }
  showMenu(x, y, items);
}

// ---------- rendering: games ----------

// Whether a game can be started from this page, and why not. Through a
// controller the index's workers host it, so this node's own setup (its
// runtimes, its installed files) says nothing: what the controller hosts does.
function startable(g) {
  if (state.controllerOn) {
    return state.hostedGames.includes(g.id)
      ? { ok: true, reason: "" }
      : { ok: false, reason: "the controller does not host this game." };
  }
  return g.runnable ? { ok: true, reason: "" } : { ok: false, reason: g.reason || "unavailable on this node." };
}

function renderGames() {
  const grid = $("games-grid");
  const n = $("nav-games-count");
  if (n) n.textContent = state.games.length ? String(state.games.length) : "";
  const sig = (state.controllerOn ? "c:" + state.hostedGames.join(",") + ";" : "") + state.games.map(g => g.id + "|" + (g.runnable ? "1" : "0") + "|" + (g.reason || "") + "|" + (g.trust || "")
    + "|" + (g.content ? (g.content.installed ? "i" : "-") + g.content.missing.join(",") : "")).join(";");
  if (grid.dataset.sig !== sig) {
    grid.dataset.sig = sig;
    grid.textContent = "";
    const sorted = [...state.games].sort((a, b) => (startable(b).ok - startable(a).ok) || a.display_name.localeCompare(b.display_name));
    for (const g of sorted) grid.appendChild(buildGameCard(g));
  }
  for (const g of state.games) refreshCardDynamic(g);
}

function buildGameCard(g) {
  const can = startable(g);
  const card = el("article", "game-card" + (can.ok ? "" : " unrunnable"));
  card.dataset.gameId = g.id;
  card.appendChild(art(g.id));
  const body = el("div", "card-body");
  body.appendChild(el("h3", "card-title", g.display_name));
  const meta = el("div", "card-meta");
  meta.appendChild(el("span", "chip", (g.transport || "—").toUpperCase() + " " + g.default_port));
  if (g.extra_ports) meta.appendChild(el("span", "chip", "+" + g.extra_ports + (g.extra_ports === 1 ? " port" : " ports")));
  if (g.console) meta.appendChild(el("span", "chip", "Live map change"));
  if (g.bots) meta.appendChild(el("span", "chip", "Bots"));
  // Whose word the pack is, under this node's [pack_trust] (§11.4's words).
  if (g.trust && g.trust !== "built in") {
    const t = el("span", "chip" + (g.signer ? "" : " chip-muted"), g.trust);
    t.title = g.signer ? "Signed by " + g.signer : "Nobody signed this pack; it is trusted only as far as whoever wrote it.";
    meta.appendChild(t);
  }
  body.appendChild(meta);
  if (!can.ok) body.appendChild(el("p", "card-reason", "Cannot start here: " + can.reason));
  // Through a controller the files live on the workers, which fetch them.
  const manualMissing = state.controllerOn ? false : renderContentHelp(body, g);
  body.appendChild(el("p", "install-status hidden"));
  const actions = el("div", "card-actions");
  const startBtn = el("button", "btn btn-primary start-btn", "Start server");
  startBtn.type = "button";
  if (!can.ok) { startBtn.disabled = true; startBtn.title = can.reason; }
  startBtn.addEventListener("click", () => openStartDialog(g.id));
  const installBtn = el("button", "btn install-btn");
  installBtn.type = "button";
  installBtn.title = "Download this game's files to the node now";
  installBtn.appendChild(icon("download"));
  installBtn.appendChild(el("span", null, "Install"));
  if (manualMissing) {
    // Nothing here can download a manual game: the button looks again.
    installBtn.textContent = "";
    installBtn.title = "Look for the files again once they are in place";
    installBtn.dataset.label = "Recheck";
    installBtn.appendChild(el("span", null, "Recheck"));
    installBtn.addEventListener("click", recheckGames);
  } else {
    installBtn.addEventListener("click", () => onInstall(g.id));
  }
  const removeBtn = el("button", "card-remove");
  removeBtn.type = "button";
  removeBtn.title = "Remove this game from the node: its pack goes, its files stay on the Installed page";
  removeBtn.setAttribute("aria-label", "Remove " + g.display_name);
  removeBtn.appendChild(icon("trash"));
  removeBtn.addEventListener("click", () => onRemovePack(g));
  actions.append(startBtn);
  // Game files and packs are an admin's (api::user_may); through a controller
  // the workers fetch them.
  const manages = state.role === "admin";
  if (!state.controllerOn && manages) actions.append(installBtn);
  body.appendChild(actions);
  if (manages) card.appendChild(removeBtn);
  card.appendChild(body);
  return card;
}

// Where a game's files stand (PLAN.md §13.3 step 3, Agent::content_state):
// a manual game says exactly where its files go and which folders the copy
// must have, so an operator never learns it from a failed Start. Returns
// true for a manual game that is not there yet.
function renderContentHelp(body, g) {
  const c = g.content;
  if (!c || !g.runnable) return false;
  if (c.driver === "manual" && !c.installed) {
    const box = el("div", "content-help");
    box.appendChild(el("p", null, "This game's files are installed by hand. Put them here, on the node:"));
    const line = el("div", "content-path");
    line.appendChild(el("code", null, c.install_dir || ""));
    const copy = el("button", "btn btn-small", "Copy");
    copy.type = "button";
    copy.addEventListener("click", async () => {
      try { await navigator.clipboard.writeText(c.install_dir || ""); toast("Path copied."); }
      catch (_) { toast("Select the path and copy it."); }
    });
    line.appendChild(copy);
    box.appendChild(line);
    if (c.must_contain.length) {
      box.appendChild(el("p", "muted small", "Inside it, these folders must exist (servers mount over them): " + c.must_contain.join(", ")));
    }
    if (c.note) box.appendChild(el("p", "muted small", "The pack says: " + c.note));
    body.appendChild(box);
    return true;
  }
  if (c.installed && c.missing.length) {
    body.appendChild(el("p", "card-reason", "The files are there but these folders are missing, and a server will not start until they exist: "
      + c.missing.join(", ")));
  } else if (!c.installed && c.driver !== "manual" && !c.fetch_allowed) {
    body.appendChild(el("p", "card-reason", "Not installed, and this node does not download games by itself: set allow_content_fetch = true in agent.toml once you trust this pack, or put the files in place by hand."));
  }
  return false;
}
async function recheckGames() {
  try {
    const games = await api("GET", "/games");
    state.games = Array.isArray(games) ? games : [];
    renderGames();
    const left = state.games.filter(g => g.content && g.content.driver === "manual" && !g.content.installed).length;
    toast(left ? "Still not found. Check the path on the card." : "Found the files.");
  } catch (err) { showError(err.message || String(err)); }
}

// ---------- adding a game (POST /packs) ----------
//
// From a pack repository (game_bridge::repo: the node fetches the index, the
// pack and its signature, and refuses bytes that are not the listed ones), or
// from a pack's own address. The node reads the repository, not this page, so
// no other origin is fetched from here. The last repository is remembered in
// this browser only.
const REPO_KEY = "lanthorn-host.repository";
async function afterImport(res) {
  const games = await api("GET", "/games");
  state.games = Array.isArray(games) ? games : [];
  renderGames();
  clearError();
  const p = res && res.imported;
  if (!p) return;
  let msg = p.display_name + (p.replaced ? " updated" : " added") + " — " + p.trust + ".";
  if (!res.loaded) msg += " Not loaded: " + (res.refused_reason || "the node's pack trust refused it") + ".";
  else if (!p.runnable) msg += " It needs a runtime in agent.toml before a server can start.";
  toast(msg);
}
// ---------- the official pack repository (official.rs) ----------
//
// On unless the operator turns it off: new games arrive by themselves, and a
// pack is never replaced while a server of its game exists.
async function loadOfficial() {
  try { state.official = await api("GET", "/packs/official"); } catch (_) { state.official = null; }
  renderOfficial();
}
function renderOfficial() {
  const row = $("official-row");
  if (!row) return;
  const o = state.official;
  row.classList.toggle("hidden", !o);
  if (!o) return;
  row.textContent = "";
  const text = el("div", "official-text");
  text.appendChild(el("strong", null, o.on ? "Official packs: on" : "Official packs: off"));
  const bits = [];
  if (!o.on) bits.push("new games from the Lanthorn project do not arrive");
  else {
    bits.push(o.checked_unix ? "checked " + ago(o.checked_unix) : "not checked yet");
    if ((o.added || []).length) bits.push("new: " + o.added.join(", "));
    if ((o.updated || []).length) bits.push("updated: " + o.updated.join(", "));
    if ((o.waiting || []).length) bits.push("waiting for their servers to go: " + o.waiting.join(", "));
    if (o.error) bits.push("could not read it: " + o.error);
  }
  text.appendChild(el("span", "muted small", " · " + bits.join(" · ")));
  // Seeding (official.rs): an exact mirror, served on the mesh through this
  // node's hub, so launchers and nodes with no internet still get the packs.
  const sd = o.seed || {};
  const seedBits = [];
  if (!sd.on) seedBits.push("not shared on the mesh");
  else {
    seedBits.push(sd.address ? "shared on the mesh" + (sd.packs != null ? " (" + sd.packs + " packs)" : "") : "sharing on the mesh starts with the next check");
    if (sd.mirrored_unix) seedBits.push("copied " + ago(sd.mirrored_unix));
    if (sd.error) seedBits.push(sd.error);
  }
  const seedLine = el("div", "muted small", seedBits.join(" · "));
  if (sd.address) seedLine.title = "Its address: " + sd.address;
  text.appendChild(seedLine);
  row.appendChild(text);
  const actions = el("div", "official-actions");
  const share = el("button", "btn btn-small", sd.on ? "Stop sharing" : "Share on the mesh");
  share.type = "button";
  share.title = sd.on
    ? "Stop serving the official packs on the mesh. The copy stays in mirrors/official."
    : "Keep an exact copy of the official packs and serve it on the mesh, so launchers and nodes without internet still get them. Only the project's signed files are served.";
  share.addEventListener("click", async () => {
    share.disabled = true; share.textContent = sd.on ? "Stopping…" : "Copying…";
    try {
      await withInFlight(api("PUT", "/packs/official", { seed: !sd.on }));
      state.official = await withInFlight(api("POST", "/packs/official/sync"));
      clearError();
      const now = state.official.seed || {};
      if (now.on && now.error) showError("Sharing on the mesh: " + now.error);
    } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "That did not change."); }
    renderOfficial();
  });
  actions.appendChild(share);
  if (o.on) {
    const now = el("button", "btn btn-small", "Check now");
    now.type = "button";
    now.addEventListener("click", async () => {
      now.disabled = true; now.textContent = "Checking…";
      try {
        state.official = await withInFlight(api("POST", "/packs/official/sync"));
        const games = await api("GET", "/games");
        state.games = Array.isArray(games) ? games : [];
        renderGames();
        clearError();
        const n = (state.official.added || []).length + (state.official.updated || []).length;
        toast(n ? n + " pack(s) taken from the official repository." : "The official packs are up to date.");
      } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "Could not check."); }
      renderOfficial();
    });
    actions.appendChild(now);
  }
  const flip = el("button", "btn btn-small", o.on ? "Turn off" : "Turn on");
  flip.type = "button";
  flip.title = o.on ? "Stop taking packs from the Lanthorn project. What is here stays." : "Take new and updated packs from the Lanthorn project.";
  flip.addEventListener("click", async () => {
    try { state.official = await withInFlight(api("PUT", "/packs/official", { on: !o.on })); clearError(); }
    catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "That did not change."); }
    renderOfficial();
  });
  actions.appendChild(flip);
  row.appendChild(actions);
}

function openAddGameDialog() {
  openModal("add-game-dialog", null, (body, close) => {
    body.appendChild(el("h3", null, "Add a game"));
    body.appendChild(el("p", "muted small", "A game is added by its pack: a small file that says which ports and "
      + "files it needs. It never says what program runs; that is this node's own config. Each pack shows whether its author signed it."));
    const form = el("form", "advanced-wrap");
    form.autocomplete = "off";
    let saved = "";
    try { saved = localStorage.getItem(REPO_KEY) || ""; } catch (_) {}
    const repo = field(form, "ag-repo", "Pack repository", { type: "text", placeholder: "https://… a repository's address", value: saved });
    const list = el("div", "repo-packs");
    const show = el("button", "btn", "Show its games");
    show.type = "submit";
    form.append(show, list);
    const status = el("p", "muted small");
    const render = (data, url) => {
      list.textContent = "";
      if (!data.packs.length) { list.appendChild(el("p", "muted small", "It lists no games.")); return; }
      list.appendChild(el("p", "muted small", data.name));
      data.packs.forEach(p => {
        const row = el("div", "repo-pack");
        const t = el("div", "repo-pack-text");
        t.appendChild(el("strong", null, p.display_name));
        t.appendChild(el("span", "muted small", " " + (p.signed ? "signed" : "unsigned") + (p.lan ? " · LAN rooms" : "")));
        row.appendChild(t);
        const label = { available: "Add", update: "Update", installed: "Added" }[p.status] || p.status;
        const b = el("button", p.status === "installed" ? "btn" : "btn btn-primary", label);
        b.type = "button";
        b.dataset.pack = p.id;
        b.disabled = p.status === "installed";
        b.addEventListener("click", async () => {
          b.disabled = true; b.textContent = "Adding…";
          try {
            const res = await withInFlight(api("POST", "/packs", { repository: url, id: p.id, replace: p.status === "update" }));
            await afterImport(res);
            close();
          } catch (e) {
            if (e && e.__auth) return;
            status.textContent = (e && e.error) || "Not added.";
            b.disabled = false; b.textContent = label;
          }
        });
        row.appendChild(b);
        list.appendChild(row);
      });
    };
    form.addEventListener("submit", async (e) => {
      e.preventDefault();
      const url = repo.value.trim();
      if (!url) return;
      show.disabled = true; status.textContent = "Reading…";
      try {
        const data = await withInFlight(api("GET", "/packs/repository?url=" + encodeURIComponent(url)));
        try { localStorage.setItem(REPO_KEY, url); } catch (_) {}
        status.textContent = "";
        render(data, url);
      } catch (err) {
        if (err && err.__auth) return;
        status.textContent = (err && err.error) || "Could not read it.";
      } finally { show.disabled = false; }
    });
    body.appendChild(form);
    const one = el("form", "advanced-wrap");
    one.autocomplete = "off";
    const url = field(one, "ag-url", "Or one pack's address", { type: "text", placeholder: "https://…/game.toml" });
    const go = el("button", "btn", "Add");
    go.type = "submit";
    one.appendChild(go);
    one.addEventListener("submit", async (e) => {
      e.preventDefault();
      if (!url.value.trim()) return;
      go.disabled = true;
      try {
        const res = await withInFlight(api("POST", "/packs", { url: url.value.trim() }));
        await afterImport(res);
        close();
      } catch (err) {
        if (err && err.__auth) return;
        status.textContent = (err && err.error) || "Not added.";
      } finally { go.disabled = false; }
    });
    body.append(one, status);
    const actions = el("div", "form-actions");
    const cancel = el("button", "btn", "Close");
    cancel.type = "button";
    cancel.addEventListener("click", close);
    actions.appendChild(cancel);
    body.appendChild(actions);
    repo.focus();
  });
}

// Take a game's pack off the node (DELETE /packs/:id). The node refuses while
// any server of the game exists and says which.
async function onRemovePack(g) {
  if (!confirm("Remove " + g.display_name + " from this node?\n\nIts pack is deleted, so it leaves the Games page and no server of it can start. " +
    "Its game files stay; delete them on the Installed page if you want the space. It can be added back with Add a game.")) return;
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
  // Through a controller a card has no Install: the workers fetch the files.
  if (!installBtn) return;
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
    label.textContent = installBtn.dataset.label || "Install";
    status.classList.remove("hidden");
    status.classList.add(state.installDone.get(g.id) === "error" ? "err" : "ok");
    status.textContent = state.installingMsg.get(g.id);
  } else {
    installBtn.disabled = false;
    label.textContent = installBtn.dataset.label || "Install";
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
  if (game.minecraft) { openMinecraftDialog(gameId); return; }
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
    // Minecraft's world settings, for a game that takes them without the
    // Java form (Bedrock Edition).
    let world = null;
    if (game.world_settings) {
      const ws = el("details");
      ws.appendChild(el("summary", "muted", "Game settings"));
      const g = el("div", "advanced-wrap");
      g.style.marginTop = "8px";
      const mode = selectField(g, "sf-mode", "Game mode", [["survival", "Survival"], ["creative", "Creative"], ["adventure", "Adventure"]], "survival");
      const diff = selectField(g, "sf-diff", "Difficulty", [["peaceful", "Peaceful"], ["easy", "Easy"], ["normal", "Normal"], ["hard", "Hard"]], "normal");
      const wl = checkField(g, "sf-wl", "Only players on the allowlist can join", false);
      const online = checkField(g, "sf-online", "Check accounts with Xbox Live (online mode)", true);
      const seed = field(g, "sf-seed", "Seed (optional)", { type: "text", placeholder: "random", maxLength: 32 });
      ws.appendChild(g);
      form.appendChild(ws);
      world = () => {
        const out = { gamemode: mode.value, difficulty: diff.value, pvp: true, whitelist: wl.checked, online_mode: online.checked };
        if (seed.value.trim()) out.seed = seed.value.trim();
        return out;
      };
    }
    // Through a controller with workers in named regions: where to put it.
    // A wish the controller keeps when a worker there has room.
    let region = null;
    if (state.controllerOn && state.regions.length) {
      region = selectField(form, "sf-region", "Region",
        [["", "Any: the worker with the most room"]].concat(state.regions.map(r => [r, r])), "");
    }
    const adv = el("details");
    adv.appendChild(el("summary", "muted", "Advanced"));
    const advWrap = el("div", "advanced-wrap");
    advWrap.style.marginTop = "8px";
    const port = field(advWrap, "sf-port", "Fixed host port", { type: "number", min: "1024", max: "65535", placeholder: "chosen by the node" });
    advWrap.appendChild(el("p", "muted small", "Left blank, the node picks a free port from its configured range."));
    adv.appendChild(advWrap);
    form.appendChild(adv);

    // A game whose publisher requires its EULA: the owner ticks it here, and
    // nothing is created before.
    let eulaBox = null;
    if (game.eula) {
      const wrap = el("label", "check eula");
      eulaBox = el("input");
      eulaBox.type = "checkbox";
      eulaBox.id = "sf-eula";
      const text = el("span");
      const a = el("a", null, game.eula.name);
      a.href = game.eula.url; a.target = "_blank"; a.rel = "noopener noreferrer";
      text.append("I accept the ", a, ". The game's publisher requires it of every server's owner.");
      wrap.append(eulaBox, text);
      form.appendChild(wrap);
    }
    const actions = el("div", "form-actions");
    actions.style.marginTop = "8px";
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const go = el("button", "btn btn-primary", "Start server");
    go.type = "submit";
    go.id = "sf-start";
    actions.append(cancel, go);
    form.appendChild(actions);
    if (eulaBox) {
      const sync = () => { go.disabled = !eulaBox.checked; go.title = eulaBox.checked ? "" : "Accept the EULA first."; };
      eulaBox.addEventListener("change", sync);
      sync();
    }
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      if (go.disabled) return;
      onStartSubmit(gameId, {
        name: name.value, maxPlayers: parseInt(mp.value, 10), map: map.value,
        bots: bots ? bots.value : "", fixedPort: port.value, restartOnCrash: crash.checked,
        eula: !!(eulaBox && eulaBox.checked), world: world ? world() : null,
        region: region ? region.value : "",
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
  const can = startable(game);
  if (!can.ok) { showError("This game cannot start here: " + can.reason); return; }

  // Starting a game that is not installed yet downloads it first, here; through
  // a controller a worker does that, on its own disk.
  if (!state.controllerOn && !roomForDownload()) return;

  const instance_id = randomInstanceId(gameId);
  if (!/^[a-z0-9._-]{1,64}$/.test(instance_id)) { showError("Could not make a valid id for this server."); return; }

  const body = {
    instance_id, game_id: gameId, name, max_players: f.maxPlayers, port,
    extra_ports: {}, map: map === "" ? null : map, bots, owner: null,
    restart_on_crash: !!f.restartOnCrash,
  };
  if (f.eula) body.eula_accepted_at = Math.floor(Date.now() / 1000);
  if (f.region) body.region = f.region;
  if (f.world) {
    if (f.world.seed && !/^[A-Za-z0-9-]{1,32}$/.test(f.world.seed)) { showError("A seed may only hold letters, digits and '-', up to 32."); return; }
    body.minecraft = f.world;
  }
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

// ---------- Minecraft (MINECRAFT.md) ----------
//
// A Minecraft server is picked by type and version, sized in plain words, and
// created only with its owner's EULA acceptance. The node judges the size
// against its own memory (`/resources/check`) and refuses what would starve
// it; this page asks the same question first so nobody is surprised.

const MC_TYPES = [
  { id: "paper", name: "Paper", line: "Recommended. Vanilla gameplay, runs faster, handles more players." },
  { id: "vanilla", name: "Vanilla", line: "Mojang's own server, exactly as released." },
  { id: "fabric", name: "Fabric", line: "The Fabric loader. No mods yet: this node runs it as it comes." },
  { id: "forge", name: "Forge", line: "The Forge loader. No mods yet: this node runs it as it comes." },
];
const MC_MEMORY = [
  { id: "small", name: "Small", min: 1024, max: 2048, line: "2 GB. A few friends on Vanilla or Paper." },
  { id: "medium", name: "Medium", min: 2048, max: 4096, line: "4 GB. Up to about 20 players. A good default." },
  { id: "large", name: "Large", min: 4096, max: 6144, line: "6 GB. Bigger groups, or a Fabric or Forge server." },
  { id: "custom", name: "Custom", line: "Pick the numbers yourself." },
];
const MC_SLOTS = [10, 20, 50, 100];
const MC_VERSIONS = new Map();

function chips(options, current, onPick) {
  const wrap = el("div", "chip-row");
  for (const o of options) {
    const b = el("button", "chip-btn" + (o.id === current ? " active" : ""), o.name);
    b.type = "button";
    b.dataset.id = o.id;
    b.addEventListener("click", () => {
      for (const x of wrap.children) x.classList.toggle("active", x === b);
      onPick(o.id);
    });
    wrap.appendChild(b);
  }
  return wrap;
}

function gbText(mb) { return (mb / 1024).toFixed(mb % 1024 ? 1 : 0) + " GB"; }

async function mcVersions(server) {
  if (MC_VERSIONS.has(server)) return MC_VERSIONS.get(server);
  const list = await api("GET", "/minecraft/versions?server=" + encodeURIComponent(server));
  MC_VERSIONS.set(server, list || []);
  return list || [];
}

function selectField(parent, id, label, options, value) {
  const l = el("label", null, label);
  l.htmlFor = id;
  const sel = el("select", "field");
  sel.id = id;
  for (const [v, text] of options) { const o = el("option", null, text); o.value = v; sel.appendChild(o); }
  if (value != null) sel.value = value;
  parent.append(l, sel);
  return sel;
}

function checkField(parent, id, text, checked) {
  const wrap = el("label", "check");
  const box = el("input");
  box.type = "checkbox";
  box.id = id;
  box.checked = !!checked;
  wrap.append(box, el("span", null, text));
  parent.appendChild(wrap);
  return box;
}

// Local hour to the UTC hour a restart schedule stores.
function utcHourOf(localHourValue) {
  const offset = -new Date().getTimezoneOffset() / 60;
  return ((Math.round(localHourValue - offset) % 24) + 24) % 24;
}

function openMinecraftDialog(gameId) {
  const game = gameOf(gameId);
  const back = openModal("start-dialog", gameId, (body, close) => {
    body.appendChild(el("h3", null, "New " + game.display_name + " server"));
    const form = el("form", "advanced-wrap mc-form");
    form.autocomplete = "off";
    const name = field(form, "mc-name", "Server name", { type: "text", required: true, placeholder: "My Minecraft server", maxLength: 60 });
    form.appendChild(el("p", "muted small", "Shown in Lanthorn's server list and as the server's message in Minecraft."));

    // Server software and version.
    const defaultType = ((game.default_build || "paper-").split("-")[0]) || "paper";
    let type = MC_TYPES.some(t => t.id === defaultType) ? defaultType : "paper";
    form.appendChild(el("label", null, "Server software"));
    const typeLine = el("p", "muted small");
    form.appendChild(chips(MC_TYPES, type, (id) => { type = id; typeLine.textContent = MC_TYPES.find(t => t.id === id).line; loadVersions(); judge(); }));
    typeLine.textContent = MC_TYPES.find(t => t.id === type).line;
    form.appendChild(typeLine);
    const version = selectField(form, "mc-version", "Minecraft version", [["", "Loading versions…"]], "");
    const versionNote = el("p", "muted small", "Releases from 1.18.1 on. Older ones let any player run code on the server (Log4Shell), so this node does not host them.");
    form.appendChild(versionNote);
    async function loadVersions() {
      version.disabled = true;
      version.textContent = "";
      const loading = el("option", null, "Loading versions…");
      loading.value = "";
      version.appendChild(loading);
      try {
        const list = await mcVersions(type);
        version.textContent = "";
        for (const v of list) {
          const label = v.game_version + (v.loader ? " · " + (type === "forge" ? "Forge " : "loader ") + v.loader : "") + (v.recommended && type === "forge" ? " (recommended)" : "");
          const o = el("option", null, label);
          o.value = v.build;
          version.appendChild(o);
        }
        const pick = list.find(v => v.build === game.default_build) || list.find(v => v.recommended && type === "forge") || list[0];
        if (pick) version.value = pick.build;
        version.disabled = list.length === 0;
        if (!list.length) versionNote.textContent = "The publisher lists no version this node can host right now.";
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        version.textContent = "";
        const o = el("option", null, "Could not load versions");
        o.value = "";
        version.appendChild(o);
        versionNote.textContent = (e && e.error) || "The version list could not be read. Is this node online?";
      }
      refresh();
    }

    // Player slots.
    let slots = 20;
    form.appendChild(el("label", null, "Player slots"));
    const slotOpts = MC_SLOTS.map(n => ({ id: String(n), name: String(n) })).concat([{ id: "custom", name: "Custom" }]);
    const slotCustom = el("input");
    slotCustom.type = "number"; slotCustom.min = "1"; slotCustom.max = "255"; slotCustom.value = "20";
    slotCustom.className = "hidden";
    slotCustom.setAttribute("aria-label", "Player slots");
    form.appendChild(chips(slotOpts, "20", (id) => {
      slotCustom.classList.toggle("hidden", id !== "custom");
      if (id !== "custom") slots = parseInt(id, 10);
      else { slots = parseInt(slotCustom.value, 10); slotCustom.focus(); }
    }));
    slotCustom.addEventListener("input", () => { slots = parseInt(slotCustom.value, 10); });
    form.appendChild(slotCustom);

    // Memory.
    let mem = "medium";
    form.appendChild(el("label", null, "Memory"));
    const memLine = el("p", "muted small");
    const custom = el("div", "mc-mem hidden");
    const xms = field(custom, "mc-xms", "Starting memory (Xms), GB", { type: "number", min: "0.5", max: "256", step: "0.5", value: "2" });
    const xmx = field(custom, "mc-xmx", "Most it may use (Xmx), GB", { type: "number", min: "0.5", max: "256", step: "0.5", value: "4" });
    form.appendChild(chips(MC_MEMORY, mem, (id) => {
      mem = id;
      memLine.textContent = MC_MEMORY.find(m => m.id === id).line;
      custom.classList.toggle("hidden", id !== "custom");
      judge();
    }));
    memLine.textContent = MC_MEMORY.find(m => m.id === mem).line;
    form.appendChild(memLine);
    custom.appendChild(el("p", "muted small", "Xmx is the most memory the server may ever use. Xms is what it takes the moment it starts; " +
      "keep it at or below Xmx. The node adds room for Java itself on top, and stops the server if it goes past that."));
    form.appendChild(custom);
    xms.addEventListener("input", judge);
    xmx.addEventListener("input", judge);
    const verdict = el("div", "verdict");
    form.appendChild(verdict);
    function heap() {
      if (mem !== "custom") { const m = MC_MEMORY.find(x => x.id === mem); return { min_mb: m.min, max_mb: m.max }; }
      return { min_mb: Math.round(parseFloat(xms.value) * 1024), max_mb: Math.round(parseFloat(xmx.value) * 1024) };
    }
    let refused = false;
    let judgeSeq = 0;
    async function judge() {
      const h = heap();
      const seq = ++judgeSeq;
      if (!(h.min_mb > 0 && h.max_mb > 0)) { verdict.className = "verdict bad"; verdict.textContent = "Enter both numbers."; refused = true; refresh(); return; }
      if (h.min_mb > h.max_mb) { verdict.className = "verdict bad"; verdict.textContent = "Starting memory cannot be more than the most it may use."; refused = true; refresh(); return; }
      try {
        const v = await api("GET", "/resources/check?game=" + encodeURIComponent(gameId) + "&min_mb=" + h.min_mb + "&max_mb=" + h.max_mb);
        if (seq !== judgeSeq) return;
        refused = v.level === "refuse";
        verdict.className = "verdict " + (v.level === "ok" ? "ok" : v.level === "warn" ? "warn" : "bad");
        verdict.textContent = "";
        const head = v.level === "ok" ? "Fits this host." : v.level === "warn" ? "It fits, with a warning." : "Too big for this host.";
        verdict.appendChild(el("strong", null, head + " "));
        verdict.appendChild(document.createTextNode((v.messages || []).join(" ")));
        if (v.available_mb != null) {
          verdict.appendChild(el("div", "muted small", "This server: up to " + gbText(v.limit_mb) + " with Java's overhead. Host: " +
            gbText(v.available_mb) + " free of " + gbText(v.total_mb) + ", " + gbText(v.reserve_mb) + " always kept for itself."));
        }
      } catch (e) {
        if (seq !== judgeSeq) return;
        verdict.className = "verdict warn";
        verdict.textContent = "Could not check this size against the host. The node checks again when you create it.";
        refused = false;
      }
      refresh();
    }

    // The world.
    form.appendChild(el("label", null, "World"));
    const worldNew = el("label", "check");
    const rNew = el("input"); rNew.type = "radio"; rNew.name = "mc-world"; rNew.checked = true;
    worldNew.append(rNew, el("span", null, "A new world"));
    const worldImport = el("label", "check");
    const rImport = el("input"); rImport.type = "radio"; rImport.name = "mc-world";
    worldImport.append(rImport, el("span", null, "Bring a world I already have"));
    form.append(worldNew, worldImport);
    const seed = field(form, "mc-seed", "Seed (optional)", { type: "text", placeholder: "random", maxLength: 32 });
    const file = el("input");
    file.type = "file";
    file.accept = ".zip,.tar.gz,.tgz,.tar,application/zip,application/gzip";
    file.className = "hidden";
    const fileNote = el("p", "muted small hidden", "A zip or tar.gz of the world folder: the one with level.dat in it, from your saves folder or another server. Up to 4 GB.");
    form.append(file, fileNote);
    const syncWorld = () => {
      const imp = rImport.checked;
      file.classList.toggle("hidden", !imp);
      fileNote.classList.toggle("hidden", !imp);
      seed.classList.toggle("hidden", imp);
      seed.previousSibling.classList.toggle("hidden", imp);
      refresh();
    };
    rNew.addEventListener("change", syncWorld);
    rImport.addEventListener("change", syncWorld);
    file.addEventListener("change", refresh);

    // Game settings.
    const adv = el("details");
    adv.appendChild(el("summary", "muted", "Game settings"));
    const g = el("div", "advanced-wrap");
    g.style.marginTop = "8px";
    const mode = selectField(g, "mc-mode", "Game mode", [["survival", "Survival"], ["creative", "Creative"], ["adventure", "Adventure"], ["spectator", "Spectator"]], "survival");
    const diff = selectField(g, "mc-diff", "Difficulty", [["peaceful", "Peaceful"], ["easy", "Easy"], ["normal", "Normal"], ["hard", "Hard"]], "normal");
    const pvp = checkField(g, "mc-pvp", "Players can hurt each other (PvP)", true);
    const wl = checkField(g, "mc-wl", "Only players on the whitelist can join", false);
    const online = checkField(g, "mc-online", "Check accounts with Mojang (online mode)", true);
    const onlineWarn = el("p", "muted small warn-text hidden", "Off, anyone can join under any name, including an operator's. Leave it on unless you know why not.");
    g.appendChild(onlineWarn);
    online.addEventListener("change", () => onlineWarn.classList.toggle("hidden", online.checked));
    adv.appendChild(g);
    form.appendChild(adv);

    // Upkeep.
    form.appendChild(el("label", null, "Looking after it"));
    const crash = checkField(form, "mc-crash", "Start it again if it crashes", true);
    const backups = checkField(form, "mc-backups", "Back it up every day, keeping the last 7", true);
    const restart = checkField(form, "mc-restart", "Restart it daily between 4 and 6 in the morning, when nobody is playing", false);

    // EULA.
    const eula = game.eula || { name: "Minecraft End User License Agreement", url: "https://aka.ms/MinecraftEULA" };
    const eulaWrap = el("label", "check eula");
    const eulaBox = el("input");
    eulaBox.type = "checkbox";
    eulaBox.id = "mc-eula";
    const eulaText = el("span");
    eulaText.append("I accept the ");
    const a = el("a", null, eula.name);
    a.href = eula.url; a.target = "_blank"; a.rel = "noopener noreferrer";
    eulaText.append(a, ". Mojang requires it of every server's owner, and the server will not start without it.");
    eulaWrap.append(eulaBox, eulaText);
    form.appendChild(eulaWrap);
    eulaBox.addEventListener("change", refresh);

    const actions = el("div", "form-actions");
    actions.style.marginTop = "8px";
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const go = el("button", "btn btn-primary", "Create server");
    go.type = "submit";
    go.id = "mc-create";
    actions.append(cancel, go);
    form.appendChild(actions);
    function refresh() {
      const why = !eulaBox.checked ? "Accept the EULA first."
        : refused ? "Pick a size this host can give."
        : !version.value ? "Pick a version."
        : (rImport.checked && !(file.files && file.files[0])) ? "Choose the world file."
        : "";
      go.disabled = !!why;
      go.title = why;
    }
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      if (go.disabled) return;
      onMinecraftSubmit(gameId, {
        name: name.value, build: version.value, slots, heap: heap(), seed: rImport.checked ? "" : seed.value.trim(),
        world: rImport.checked ? file.files[0] : null,
        settings: { gamemode: mode.value, difficulty: diff.value, pvp: pvp.checked, whitelist: wl.checked, online_mode: online.checked },
        crash: crash.checked, backups: backups.checked, restart: restart.checked,
      }, go, close);
    });
    body.appendChild(form);
    refresh();
    loadVersions();
    judge();
    name.focus();
  });
  back.querySelector(".modal").classList.add("modal-mid");
}

async function uploadWorld(instanceId, file) {
  const resp = await withInFlight(fetch("/instances/" + encodeURIComponent(instanceId) + "/world", {
    method: "POST",
    headers: { "Authorization": "Bearer " + (state.token || ""), "Content-Type": "application/octet-stream" },
    body: file,
  }));
  if (resp.status === 401) { setToken(null); showTokenScreen(); throw { __auth: true, error: "The token is wrong or no longer valid." }; }
  let data = null;
  try { data = await resp.json(); } catch (_) {}
  if (!resp.ok) throw { error: (data && data.error) || "The world was not imported." };
  return data;
}

async function onMinecraftSubmit(gameId, f, go, close) {
  const name = (f.name || "").trim();
  if (!name) { showError("A server needs a name."); return; }
  if (!(f.slots >= 1 && f.slots <= 255)) { showError("Player slots must be between 1 and 255."); return; }
  if (f.seed && !/^[A-Za-z0-9-]{1,32}$/.test(f.seed)) { showError("A seed may only hold letters, digits and '-', up to 32."); return; }
  if (!state.controllerOn && !roomForDownload()) return;
  const instance_id = randomInstanceId(gameId);
  const settings = Object.assign({}, f.settings);
  if (f.seed) settings.seed = f.seed;
  const body = {
    instance_id, game_id: gameId, name, max_players: f.slots, port: null, extra_ports: {}, map: null, bots: null, owner: null,
    restart_on_crash: !!f.crash, content_version: f.build, memory: f.heap,
    eula_accepted_at: Math.floor(Date.now() / 1000), minecraft: settings,
  };
  if (f.world) body.stopped = true;
  go.disabled = true;
  go.textContent = "Creating…";
  const id = encodeURIComponent(instance_id);
  try {
    let result = await withInFlight(api("POST", "/instances", body));
    close();
    clearError();
    if (result && result.installing) {
      setActiveTab("games");
      state.installingMsg.set(gameId, "Downloading Minecraft " + f.build + "…");
      renderGames();
      if (!(await watchInstall(gameId))) return;
      result = await withInFlight(api("POST", "/instances", body));
    }
    if (f.world) {
      toast("Uploading the world…");
      await uploadWorld(instance_id, f.world);
      await withInFlight(api("POST", "/instances/" + id + "/start"));
    }
    // The schedules are the node's own features; a failure here leaves a
    // running server and says which part did not take.
    if (f.backups) {
      try { await api("PUT", "/instances/" + id + "/backup-schedule", { schedule: { every_hours: 24, keep: 7 } }); }
      catch (e) { showError("The server runs, but its daily backup was not set: " + ((e && e.error) || "")); }
    }
    if (f.restart) {
      try { await api("PUT", "/instances/" + id + "/restart-schedule", { schedule: { start_utc: utcHourOf(4), end_utc: utcHourOf(6), with_players: false } }); }
      catch (e) { showError("The server runs, but its daily restart was not set: " + ((e && e.error) || "")); }
    }
    setActiveTab("servers");
    toast("“" + name + "” is starting. The first start takes a minute while it sets itself up.");
    poll();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "The server was not created.");
    go.disabled = false;
    go.textContent = "Create server";
  }
}

async function onUploadWorld(instanceId) {
  const inst = instById(instanceId);
  const name = (inst && inst.name) || instanceId;
  openModal("world-dialog", inst && inst.game_id, (body, close) => {
    body.appendChild(el("h3", null, "Upload a world · " + name));
    body.appendChild(el("p", "muted small", "A zip or tar.gz of a world folder (the one with level.dat in it). The server's current world is " +
      "backed up first, so nothing is lost; restore it from Backups if you change your mind."));
    const file = el("input");
    file.type = "file";
    file.accept = ".zip,.tar.gz,.tgz,.tar,application/zip,application/gzip";
    body.appendChild(file);
    const actions = el("div", "form-actions");
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const go = el("button", "btn btn-primary", "Upload and replace");
    go.type = "button";
    go.disabled = true;
    file.addEventListener("change", () => { go.disabled = !(file.files && file.files[0]); });
    go.addEventListener("click", async () => {
      go.disabled = true; go.textContent = "Uploading…";
      try {
        const res = await uploadWorld(instanceId, file.files[0]);
        clearError();
        close();
        toast("World uploaded" + (res && res.backup ? "; the old one is backup " + res.backup : "") + ". Start the server to play it.");
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "The world was not imported.");
        go.disabled = false; go.textContent = "Upload and replace";
      }
    });
    actions.append(cancel, go);
    body.appendChild(actions);
  });
}

// ---------- kick, ban and operators (MINECRAFT.md §4) ----------

const TARGET_HINT = {
  "minecraft-name": "Minecraft name",
  "gamertag": "Gamertag",
  "player-name": "Name in game",
  "steam-id": "Steam ID, like STEAM_0:1:12345",
};
const ACTION_WORD = { kick: "Kick", ban: "Ban", unban: "Unban", op: "Make operator", deop: "Remove operator" };

async function onModeration(instanceId) {
  const inst = instById(instanceId);
  const name = (inst && inst.name) || instanceId;
  const path = "/instances/" + encodeURIComponent(instanceId);
  openModal("moderation-dialog", inst && inst.game_id, (body, close) => {
    body.appendChild(el("h3", null, "Kick, ban and operators · " + name));
    const content = el("div", "moderation");
    body.appendChild(content);
    const actions = el("div", "form-actions");
    const done = el("button", "btn", "Close");
    done.type = "button";
    done.onclick = close;
    actions.appendChild(done);
    body.appendChild(actions);

    async function act(action, target, reason) {
      try {
        await withInFlight(api("POST", path + "/moderation", { action, target, reason: reason || null }));
        clearError();
        toast(ACTION_WORD[action] + ": " + target + ".");
        await load();
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "That did not go through.");
      }
    }

    async function load() {
      let view;
      try { view = await api("GET", path + "/moderation"); }
      catch (e) {
        if (e && e.__auth) { close(); return; }
        content.textContent = "";
        content.appendChild(el("p", "muted", (e && e.error) || "This node's agent has no moderation. Update the agent."));
        return;
      }
      const running = (instById(instanceId) || {}).state === "running";
      let players = null;
      if (running) { try { players = await api("GET", path + "/players"); } catch (_) { players = null; } }
      const can = new Map((view.actions || []).map(a => [a.action, a.target]));
      content.textContent = "";

      // Who is on now, with a button per action that takes their name.
      content.appendChild(el("h4", null, "Online now"));
      if (!running) content.appendChild(el("p", "muted small", "The server is stopped. Bans still change and reach it when it starts."));
      else if (!players || !players.length) content.appendChild(el("p", "muted small", players
        ? "Nobody is listed. Some games, Bedrock among them, say how many are on but not who: type a name below."
        : "This game does not say who is on."));
      else {
        const list = el("div", "mod-list");
        for (const p of players) {
          const row = el("div", "mod-row");
          row.appendChild(el("span", "mod-name", p.name || "(no name)"));
          const btns = el("div", "mod-actions");
          for (const a of ["kick", "ban", "op"]) {
            const kind = can.get(a);
            if (!kind || kind === "steam-id") continue;
            const b = el("button", "btn btn-sm" + (a === "ban" ? " btn-danger" : ""), ACTION_WORD[a]);
            b.type = "button";
            b.onclick = () => act(a, p.name, null);
            btns.appendChild(b);
          }
          row.appendChild(btns);
          list.appendChild(row);
        }
        content.appendChild(list);
      }

      // Act on anyone by name or Steam ID.
      content.appendChild(el("h4", null, "Act on a player"));
      const form = el("div", "mod-form");
      const action = el("select", "field");
      for (const [a] of can) { const o = el("option", null, ACTION_WORD[a]); o.value = a; action.appendChild(o); }
      const target = el("input", "field");
      target.type = "text";
      const reason = el("input", "field");
      reason.type = "text";
      reason.placeholder = "Reason (optional)";
      reason.maxLength = 120;
      const go = el("button", "btn btn-primary", "Go");
      go.type = "button";
      const sync = () => {
        target.placeholder = TARGET_HINT[can.get(action.value)] || "Player";
        reason.classList.toggle("hidden", !["kick", "ban"].includes(action.value));
      };
      action.addEventListener("change", sync);
      sync();
      go.onclick = () => { if (target.value.trim()) act(action.value, target.value.trim(), reason.value.trim()); };
      form.append(action, target, reason, go);
      content.appendChild(form);
      if (view.ban_list === "none") {
        content.appendChild(el("p", "muted small", "This game's server has no ban. To keep someone out, turn on the allowlist " +
          "in the server's settings and list only the players you want."));
      }
      if (view.ban_list === "node-record") {
        content.appendChild(el("p", "muted small", "A kick takes the name shown in game. A ban takes the player's Steam ID, because a name can be anything: " +
          "type status in the console to see everyone's. This game cannot keep its own ban list, so the node keeps it and gives it back to the server at every start."));
      }

      // The ban list, for a game that has one.
      if (view.ban_list !== "none") {
        content.appendChild(el("h4", null, "Banned"));
        const banned = view.banned || [];
        if (!banned.length) content.appendChild(el("p", "muted small", "Nobody."));
        else {
          const list = el("div", "mod-list");
          for (const b of banned) {
            const row = el("div", "mod-row");
            const who = el("span", "mod-name", b.target);
            if (b.reason) who.appendChild(el("span", "muted small", " · " + b.reason));
            row.appendChild(who);
            const un = el("button", "btn btn-sm", "Unban");
            un.type = "button";
            un.onclick = () => act("unban", b.target, null);
            row.appendChild(un);
            list.appendChild(row);
          }
          content.appendChild(list);
        }
      }

      // Operators, for a game that lists them.
      if (view.ops) {
        content.appendChild(el("h4", null, "Operators"));
        content.appendChild(el("p", "muted small", "Operators can run any command in game, including ban and stop."));
        if (!view.ops.length) content.appendChild(el("p", "muted small", "None."));
        else {
          const list = el("div", "mod-list");
          for (const o of view.ops) {
            const row = el("div", "mod-row");
            row.appendChild(el("span", "mod-name", o.target));
            const de = el("button", "btn btn-sm", "Remove operator");
            de.type = "button";
            de.disabled = !running;
            de.title = running ? "" : "The server has to be running.";
            de.onclick = () => act("deop", o.target, null);
            row.appendChild(de);
            list.appendChild(row);
          }
          content.appendChild(list);
        }
      }
    }
    load();
  });
  const box = $("moderation-dialog");
  if (box) box.querySelector(".modal").classList.add("modal-mid");
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
    const game = inst && state.games.find(g => g.id === inst.game_id);
    // Never through a controller: it carries no console (controller::allowed_path).
    const canType = !state.controllerOn && !!(game && game.console && inst.state === "running");
    const footText = canType ? "Updates every 2 s while open. A line you send shows here with the game's answer."
      : "Updates every 2 s while open. Read-only.";
    const foot = el("div", "muted small log-foot", footText);
    body.append(head, pre);
    if (canType) {
      // One line at a time, typed by the operator (Agent::send_console).
      const form = el("form", "console-form");
      const input = el("input", "field console-input");
      input.type = "text";
      input.maxLength = 256;
      input.placeholder = "Type a console command, such as status";
      input.setAttribute("aria-label", "Console command");
      input.autocomplete = "off";
      input.spellcheck = false;
      const send = el("button", "btn btn-primary", "Send");
      send.type = "submit";
      form.append(input, send);
      form.addEventListener("submit", async ev => {
        ev.preventDefault();
        const line = input.value.trim();
        if (!line) return;
        send.disabled = true;
        try {
          await api("POST", "/instances/" + encodeURIComponent(instanceId) + "/console", { line });
          input.value = "";
          foot.textContent = "Sent: " + line;
          setTimeout(() => refresh(true), 600);
        } catch (e) {
          if (e && e.__auth) { close(); return; }
          foot.textContent = (e && e.error) || "The line was not sent.";
        } finally {
          send.disabled = false;
          input.focus();
        }
      });
      body.appendChild(form);
    }
    body.appendChild(foot);

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
        if (!foot.textContent.startsWith("Sent: ")) foot.textContent = footText;
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

// ---------- backups (backups.rs) ----------

// A backup's file, saved by the browser. Fetched with the token like every
// other request, so a link cannot do it.
async function downloadBackup(instanceId, name) {
  const path = "/instances/" + encodeURIComponent(instanceId) + "/backups/" + encodeURIComponent(name) + "/file";
  let resp;
  try { resp = await withInFlight(fetch(path, { headers: { "Authorization": "Bearer " + (state.token || "") } })); }
  catch (e) { showError("Could not reach the node."); return; }
  if (resp.status === 401) { setToken(null); showTokenScreen(); return; }
  if (!resp.ok) {
    let msg = "The backup could not be downloaded.";
    try { const d = await resp.json(); if (d && d.error) msg = d.error; } catch (_) {}
    showError(msg);
    return;
  }
  const url = URL.createObjectURL(await resp.blob());
  const a = el("a");
  a.href = url;
  a.download = instanceId + "-" + name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

function onBackups(instanceId) {
  const inst = instById(instanceId);
  const name = (inst && inst.name) || instanceId;
  const path = "/instances/" + encodeURIComponent(instanceId) + "/backups";
  openModal("backups-dialog", inst && inst.game_id, (body, close) => {
    body.appendChild(el("h3", null, "Backups · " + name));
    body.appendChild(el("p", "muted small", "A backup holds this server's own files: saves, settings and logs. " +
      "The game's files are shared and never copied. Restoring one needs the server stopped."));
    const list = el("div", "backup-list");
    body.appendChild(list);
    const note = el("p", "muted small");
    body.appendChild(note);

    // On a schedule (backups.rs): only while the server runs, and it deletes
    // only the backups it made itself.
    const sched = el("div", "backup-schedule");
    const every = el("select", "field");
    for (const [v, t] of [["0", "Off"], ["6", "Every 6 hours"], ["12", "Every 12 hours"], ["24", "Every day"], ["168", "Every week"]]) {
      const o = el("option", null, t); o.value = v; every.appendChild(o);
    }
    const keep = el("input", "field");
    keep.type = "number"; keep.min = "1"; keep.max = "100"; keep.value = "7";
    const keepLabel = el("label", "muted small", "keep the newest ");
    keepLabel.appendChild(keep);
    const saveSched = el("button", "btn btn-small", "Save");
    saveSched.type = "button";
    const schedNote = el("div", "muted small");
    const schedLabel = el("label", null, "Back up automatically ");
    schedLabel.appendChild(every);
    sched.append(schedLabel, keepLabel, saveSched);
    body.append(sched, schedNote);
    function schedText() {
      keepLabel.classList.toggle("hidden", every.value === "0");
      schedNote.textContent = every.value === "0" ? "" :
        "While it runs. Only backups made on this schedule are deleted to keep " + (keep.value || "?") + "; ones made with Back up now stay.";
    }
    every.addEventListener("change", schedText);
    keep.addEventListener("input", schedText);
    const schedPath = "/instances/" + encodeURIComponent(instanceId) + "/backup-schedule";
    api("GET", schedPath).then((st) => {
      if (st && st.schedule) {
        const h = String(st.schedule.every_hours);
        if (![...every.options].some((o) => o.value === h)) { const o = el("option", null, "Every " + h + " hours"); o.value = h; every.appendChild(o); }
        every.value = h;
        keep.value = String(st.schedule.keep);
      }
      schedText();
    }).catch((e) => {
      if (e && e.__auth) { close(); return; }
      sched.classList.add("hidden"); // an older agent
    });
    saveSched.addEventListener("click", async () => {
      const hours = Number(every.value);
      const k = Number(keep.value);
      if (hours && !(k >= 1 && k <= 100)) { showError("Keep 1 to 100 backups."); return; }
      try {
        await withInFlight(api("PUT", schedPath, { schedule: hours ? { every_hours: hours, keep: k } : null }));
        clearError();
        toast(hours ? name + " is backed up " + every.options[every.selectedIndex].text.toLowerCase() + "." : name + " is no longer backed up automatically.");
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "The schedule was not saved.");
      }
    });

    const actions = el("div", "form-actions");
    actions.style.marginTop = "8px";
    const shut = el("button", "btn", "Close");
    shut.type = "button";
    shut.onclick = close;
    const make = el("button", "btn btn-primary", "Back up now");
    make.type = "button";
    const picker = el("input");
    picker.type = "file";
    picker.accept = ".gz,.tar.gz,application/gzip";
    picker.className = "hidden";
    const importBtn = el("button", "btn", "Import a file…");
    importBtn.type = "button";
    importBtn.title = "Add a backup file saved with Download, from this node or another. Restore it once it is listed.";
    importBtn.addEventListener("click", () => picker.click());
    picker.addEventListener("change", async () => {
      const f = picker.files && picker.files[0];
      picker.value = "";
      if (!f) return;
      importBtn.disabled = true; importBtn.textContent = "Importing…";
      try {
        const resp = await withInFlight(fetch(path + "/import", {
          method: "POST",
          headers: { "Authorization": "Bearer " + (state.token || ""), "Content-Type": "application/gzip" },
          body: f,
        }));
        if (resp.status === 401) { setToken(null); showTokenScreen(); close(); return; }
        let data = null;
        try { data = await resp.json(); } catch (_) {}
        if (!resp.ok) { showError((data && data.error) || "The file was not imported."); return; }
        clearError();
        toast(f.name + " imported. Restore it from the list when the server is stopped.");
        await load();
      } catch (e) {
        showError("Could not reach the node.");
      } finally {
        importBtn.disabled = false; importBtn.textContent = "Import a file…";
      }
    });
    actions.append(picker, shut, importBtn, make);
    body.appendChild(actions);

    async function load() {
      list.textContent = "";
      let items;
      try { items = await api("GET", path); }
      catch (e) {
        if (e && e.__auth) { close(); return; }
        note.textContent = (e && e.error) || "This node's agent cannot make backups. Update the agent.";
        make.disabled = true;
        return;
      }
      const running = (instById(instanceId) || {}).state === "running";
      note.textContent = running ? "It is running: a backup now may catch a file mid-write. Stop it first for a clean one." : "";
      if (!items.length) { list.appendChild(el("div", "muted small", "No backups yet.")); return; }
      for (const b of items) {
        const row = el("div", "backup-row");
        const when = b.created_unix ? new Date(b.created_unix * 1000).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : b.name;
        row.append(el("span", "backup-when", when), el("span", "muted small", formatBytes(b.bytes)));
        const restore = el("button", "btn btn-small", "Restore");
        restore.type = "button";
        restore.disabled = running;
        restore.title = running ? "Stop the server first." : "Replace this server's files with this backup.";
        restore.addEventListener("click", async () => {
          if (!confirm("Restore the backup from " + when + "?\n\nThis server's current files are replaced. Back up first if you may want them.")) return;
          try {
            await withInFlight(api("POST", path + "/" + encodeURIComponent(b.name) + "/restore"));
            toast(name + ": files restored from " + when + ".");
            clearError();
          } catch (e) {
            if (e && e.__auth) { close(); return; }
            showError((e && e.error) || "The backup was not restored.");
          }
        });
        const del = el("button", "btn btn-small btn-danger-text", "Delete");
        del.type = "button";
        del.addEventListener("click", async () => {
          if (!confirm("Delete the backup from " + when + "?")) return;
          try { await withInFlight(api("DELETE", path + "/" + encodeURIComponent(b.name))); load(); }
          catch (e) { if (e && e.__auth) { close(); return; } showError((e && e.error) || "The backup was not deleted."); }
        });
        const down = el("button", "btn btn-small", "Download");
        down.type = "button";
        down.title = "Save this backup as a file, to keep elsewhere or bring back later with Import.";
        down.addEventListener("click", () => downloadBackup(instanceId, b.name));
        const acts = el("span", "backup-actions");
        acts.append(down, restore, del);
        row.appendChild(acts);
        list.appendChild(row);
      }
    }
    make.addEventListener("click", async () => {
      make.disabled = true; make.textContent = "Backing up…";
      try {
        const b = await withInFlight(api("POST", path));
        toast(name + " backed up, " + formatBytes(b.bytes) + ".");
        clearError();
        await load();
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "The backup was not made.");
      } finally {
        make.disabled = false; make.textContent = "Back up now";
      }
    });
    load();
  });
}

// ---------- chat (ROADMAP.md §6.1) ----------
//
// Off on a node until the operator turns it on, per server. Every name and
// line is a player's text, so it only ever goes in as textContent. The dialog
// polls while it is open and stops when it closes.
async function onChat(instanceId) {
  const inst = instById(instanceId);
  const name = (inst && inst.name) || instanceId;
  const path = "/instances/" + encodeURIComponent(instanceId) + "/chat";
  let view;
  try { view = await api("GET", path + "?since=0"); } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "This node's agent has no chat. Update the agent.");
    return;
  }
  openModal("chat-dialog", inst && inst.game_id, (body, close) => {
    body.appendChild(el("h3", null, "Chat · " + name));
    body.appendChild(el("p", "muted small", "Players who joined this server through Lanthorn can talk here, and so can you. " +
      "Only what is said while the server runs is kept, and nothing after it stops."));
    const onWrap = el("label", "check");
    const on = el("input");
    on.type = "checkbox";
    on.id = "chat-on";
    onWrap.append(on, el("span", null, "Chat on this server"));
    body.appendChild(onWrap);
    const members = el("div", "chat-members");
    members.id = "chat-members";
    body.appendChild(members);
    const log = el("div", "chat-log");
    log.id = "chat-log";
    log.setAttribute("role", "log");
    body.appendChild(log);
    const form = el("form", "chat-form");
    const input = el("input", "field");
    input.id = "chat-input";
    input.maxLength = 280;
    input.autocomplete = "off";
    input.placeholder = "Say something to the players";
    const send = el("button", "btn btn-primary", "Send");
    send.type = "submit";
    form.append(input, send);
    body.appendChild(form);
    const actions = el("div", "form-actions");
    const done = el("button", "btn", "Close");
    done.type = "button";
    done.onclick = close;
    actions.appendChild(done);
    body.appendChild(actions);

    let seq = 0;
    let memberSig = "";
    const render = v => {
      on.checked = !!v.on;
      form.hidden = !(v.on && v.live);
      const sig = JSON.stringify(v.members || []);
      if (sig !== memberSig && !members.contains(document.activeElement)) {
        memberSig = sig;
        members.textContent = "";
        (v.members || []).forEach(m => {
          const chip = el("span", "chip chat-member" + (m.muted ? " muted" : ""), m.name + (m.host ? " (you)" : "") + (m.muted ? " · muted" : ""));
          if (!m.host) {
            const b = el("button", "btn btn-small", m.muted ? "Unmute" : "Mute");
            b.type = "button";
            b.onclick = async () => {
              try { await withInFlight(api("POST", path + "/mute", { identity: m.identity, muted: !m.muted })); clearError(); }
              catch (e) { showError((e && e.error) || "That did not change."); }
              refresh();
            };
            chip.appendChild(b);
          }
          members.appendChild(chip);
        });
      }
      for (const l of v.lines || []) {
        if (l.seq <= seq) continue;
        seq = l.seq;
        const row = el("div", "chat-line");
        row.append(el("strong", null, l.host ? "You" : (l.name || "Someone who left")), document.createTextNode(" "), el("span", null, l.text));
        log.appendChild(row);
        log.scrollTop = log.scrollHeight;
      }
    };
    const refresh = async () => {
      try { render(await api("GET", path + "?since=" + seq)); } catch (_) { /* the next tick tries again */ }
    };
    on.addEventListener("change", async () => {
      try { await withInFlight(api("PUT", path, { on: on.checked })); clearError();
        toast(on.checked ? "Chat is on for " + name + "." : "Chat is off for " + name + "."); }
      catch (e) { if (e && e.__auth) { close(); return; } showError((e && e.error) || "That did not change."); }
      refresh();
    });
    form.addEventListener("submit", async e => {
      e.preventDefault();
      const text = input.value.trim();
      if (!text) return;
      try { await withInFlight(api("POST", path, { text })); input.value = ""; clearError(); }
      catch (err) { if (err && err.__auth) { close(); return; } showError((err && err.error) || "Not sent."); }
      refresh();
    });
    render(view);
    const tick = setInterval(() => {
      if (!body.isConnected) { clearInterval(tick); return; }
      refresh();
    }, 2000);
  });
}

// ---------- daily restart (schedule.rs) ----------

async function onRestartSchedule(instanceId) {
  const inst = instById(instanceId);
  const name = (inst && inst.name) || instanceId;
  let current = null;
  try {
    const all = await api("GET", "/restart-schedules");
    current = all && all[instanceId] ? all[instanceId] : null;
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "This node's agent cannot schedule restarts. Update the agent.");
    return;
  }
  const game = inst && state.games.find(g => g.id === inst.game_id);
  const canWarn = !!(game && game.console);
  const sched = (current && current.schedule) || { start_utc: 4, end_utc: 6, with_players: false };
  openModal("schedule-dialog", inst && inst.game_id, (body, close) => {
    body.appendChild(el("h3", null, "Daily restart · " + name));
    body.appendChild(el("p", "muted small", "Once a day, inside the window, when nobody is playing. A server that does not say " +
      "how many are on is never treated as empty. A stopped server is never started by this."));
    const onWrap = el("label", "check");
    const on = el("input");
    on.type = "checkbox";
    on.checked = !!current;
    onWrap.append(on, el("span", null, "Restart this server every day"));
    body.appendChild(onWrap);
    const hours = el("div", "ug-hours schedule-hours");
    const hourSel = value => {
      const sel = el("select", "field");
      for (let h = 0; h < 24; h++) { const o = el("option", null, localHour(h)); o.value = String(h); sel.appendChild(o); }
      sel.value = String(value);
      return sel;
    };
    const start = hourSel(sched.start_utc);
    const end = hourSel(sched.end_utc);
    hours.append(el("span", "muted small", "Between"), start, el("span", "muted small", "and"), end, el("span", "muted small", "your time"));
    body.appendChild(hours);
    const forceWrap = el("label", "check");
    const force = el("input");
    force.type = "checkbox";
    force.checked = !!sched.with_players;
    force.disabled = !canWarn;
    forceWrap.append(force, el("span", null, canWarn ? "Also with players on: they are warned in game a minute before"
      : "This game has no console to warn players on, so it always waits for empty"));
    body.appendChild(forceWrap);
    if (current && current.waiting) body.appendChild(el("p", "muted small", "Now: " + current.waiting + "."));
    else if (current && current.last_unix) body.appendChild(el("p", "muted small", "Last restarted by this " + ago(current.last_unix) + "."));
    const sync = () => { start.disabled = end.disabled = !on.checked; force.disabled = !on.checked || !canWarn; };
    on.addEventListener("change", sync);
    sync();
    const actions = el("div", "form-actions");
    actions.style.marginTop = "8px";
    const cancel = el("button", "btn", "Cancel");
    cancel.type = "button";
    cancel.onclick = close;
    const save = el("button", "btn btn-primary", "Save");
    save.type = "button";
    save.addEventListener("click", async () => {
      if (on.checked && start.value === end.value) { showError("A window has to be at least an hour long."); return; }
      const schedule = on.checked ? { start_utc: Number(start.value), end_utc: Number(end.value), with_players: force.checked && canWarn } : null;
      save.disabled = true;
      try {
        await withInFlight(api("PUT", "/instances/" + encodeURIComponent(instanceId) + "/restart-schedule", { schedule }));
        clearError();
        close();
        toast(schedule ? name + " restarts daily between " + localHour(schedule.start_utc) + " and " + localHour(schedule.end_utc) + "."
          : name + " no longer restarts on a schedule.");
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "That did not save.");
        save.disabled = false;
      }
    });
    actions.append(cancel, save);
    body.appendChild(actions);
  });
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

// Backups of servers that were removed: they stay until the operator deletes
// them (backups.rs). Read with the leftover folders, once a minute.
async function refreshRemovedBackups() {
  try {
    const sets = await api("GET", "/backups");
    state.removedBackups = sets.filter((s) => !s.server_exists);
  } catch (e) {
    state.removedBackups = null; // an older agent: no notice
  }
  renderRemovedBackups();
}
function renderRemovedBackups() {
  const bar = $("removed-backups-bar");
  if (!bar) return;
  const sets = state.removedBackups || [];
  bar.classList.toggle("hidden", !sets.length);
  if (!sets.length) return;
  const count = sets.reduce((n, s) => n + s.backups.length, 0);
  const bytes = sets.reduce((n, s) => n + s.backups.reduce((m, b) => m + b.bytes, 0), 0);
  $("removed-backups-text").textContent = count + " backup" + (count === 1 ? "" : "s") + " of " + sets.length +
    " removed server" + (sets.length === 1 ? "" : "s") + ", " + formatBytes(bytes) + ".";
}
function onRemovedBackups() {
  openModal("removed-backups-dialog", null, (body, close) => {
    body.appendChild(el("h3", null, "Backups of removed servers"));
    body.appendChild(el("p", "muted small", "These servers no longer exist; their backups stayed. " +
      "Download one to keep it, or to bring it into another server with Import in that server's Backups."));
    const list = el("div", "backup-list");
    body.appendChild(list);
    const actions = el("div", "form-actions");
    const shut = el("button", "btn", "Close");
    shut.type = "button";
    shut.onclick = close;
    actions.appendChild(shut);
    body.appendChild(actions);
    async function del(path, what) {
      try { await withInFlight(api("DELETE", path)); clearError(); }
      catch (e) { if (e && e.__auth) { close(); return; } showError((e && e.error) || what + " was not deleted."); }
      await refreshRemovedBackups();
      render();
    }
    function render() {
      list.textContent = "";
      const sets = state.removedBackups || [];
      if (!sets.length) { list.appendChild(el("div", "muted small", "None left.")); return; }
      for (const set of sets) {
        const head = el("div", "backup-row");
        head.appendChild(el("span", "backup-when", set.instance_id));
        const all = el("button", "btn btn-small btn-danger-text", "Delete all");
        all.type = "button";
        all.addEventListener("click", () => {
          if (!confirm("Delete all " + set.backups.length + " backups of " + set.instance_id + "?\n\nThis cannot be undone.")) return;
          del("/backups/" + encodeURIComponent(set.instance_id), "The backups");
        });
        head.appendChild(all);
        list.appendChild(head);
        for (const b of set.backups) {
          const row = el("div", "backup-row backup-sub");
          const when = b.created_unix ? new Date(b.created_unix * 1000).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : b.name;
          row.append(el("span", "backup-when", when), el("span", "muted small", formatBytes(b.bytes)));
          const d = el("button", "btn btn-small btn-danger-text", "Delete");
          d.type = "button";
          d.addEventListener("click", () => {
            if (!confirm("Delete the backup from " + when + "?")) return;
            del("/instances/" + encodeURIComponent(set.instance_id) + "/backups/" + encodeURIComponent(b.name), "The backup");
          });
          const down = el("button", "btn btn-small", "Download");
          down.type = "button";
          down.addEventListener("click", () => downloadBackup(set.instance_id, b.name));
          const acts = el("span", "backup-actions");
          acts.append(down, d);
          row.appendChild(acts);
          list.appendChild(row);
        }
      }
    }
    render();
  });
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
  // Through a controller a stopped server is started (`/start`); restart is not
  // carried, and the button is off for a running one.
  if (state.controllerOn) {
    return rowAction(id, ".restart-btn", "…",
      () => api("POST", "/instances/" + encodeURIComponent(id) + "/start"),
      "The server did not start.", "Starting.");
  }
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

// Through the controller only. The owner picks a worker in the server's region
// (its home), or asks an admin for another region (regions.rs on the index):
// an approved request moves it there and makes that region its home.
function onRelocate(id, name) {
  openModal("move-dialog", null, async (body, close) => {
    body.appendChild(el("h3", null, "Move \u201c" + (name || id) + "\u201d"));
    const status = el("p", "muted small", "Asking the controller where it could go\u2026");
    body.appendChild(status);
    const actions = el("div", "form-actions");
    const closeBtn = el("button", "btn", "Close");
    closeBtn.type = "button";
    closeBtn.onclick = close;
    let t, mine;
    try {
      [t, mine] = await Promise.all([
        api("GET", "/instances/" + encodeURIComponent(id) + "/move-targets"),
        api("GET", "/controller/region-requests").catch(() => []),
      ]);
    } catch (e) {
      status.textContent = (e && e.error) || "Could not ask the controller.";
      actions.appendChild(closeBtn);
      body.appendChild(actions);
      return;
    }
    status.textContent = "On " + t.worker + (t.region ? ", in region " + t.region : ", in no region") +
      ". Its files are copied while it runs; then it switches over, dropping players for a moment, and keeps its mesh address.";

    body.appendChild(el("h4", null, t.region ? "Another worker in " + t.region : "Another worker"));
    if (!t.workers.length) {
      body.appendChild(el("p", "muted small", "No other worker is in this server's region."));
    } else {
      const roomy = t.workers.filter(w => w.room === "room");
      const pick = selectField(body, "mv-worker", "Worker",
        [["", "Any: the one with the most room"]].concat(t.workers.map(w =>
          [w.name, w.name + (w.room === "room" ? "" : w.room === "full" ? " (full)" : " (not answering)")])), "");
      for (const o of pick.options) {
        const w = t.workers.find(x => x.name === o.value);
        if (w && w.room !== "room") o.disabled = true;
      }
      const go = el("button", "btn btn-primary", "Move");
      go.type = "button";
      go.disabled = !roomy.length;
      if (!roomy.length) go.title = "No worker in this region has room.";
      go.onclick = async () => {
        go.disabled = true;
        go.textContent = "Moving\u2026";
        try {
          await withInFlight(api("POST", "/instances/" + encodeURIComponent(id) + "/relocate",
            pick.value ? { node: pick.value } : {}));
          close();
          clearError();
          toast("Moved.");
          poll();
        } catch (e) {
          if (e && e.__auth) return;
          showError((e && e.error) || "The server was not moved.");
          go.disabled = false;
          go.textContent = "Move";
        }
      };
      const row = el("div", "form-actions");
      row.style.justifyContent = "flex-start";
      row.appendChild(go);
      body.appendChild(row);
    }

    if (t.regions.length) {
      body.appendChild(el("h4", null, "Another region"));
      body.appendChild(el("p", "muted small",
        "An admin of the controller decides. Once approved, the server moves to a worker in that region, " +
        "and that region becomes its home: coming back is another request."));
      const pending = (Array.isArray(mine) ? mine : []).find(r => r.instance_id === id && r.state === "pending");
      if (pending) {
        body.appendChild(el("p", null, "Waiting for an admin: move to " + pending.to_region + "." +
          (pending.note ? " Last try: " + pending.note : "")));
        const cancel = el("button", "btn", "Take the request back");
        cancel.type = "button";
        cancel.onclick = async () => {
          try {
            await withInFlight(api("DELETE", "/controller/region-requests/" + encodeURIComponent(pending.id)));
            close();
            toast("Request taken back.");
          } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "The request was not taken back."); }
        };
        const row = el("div", "form-actions");
        row.style.justifyContent = "flex-start";
        row.appendChild(cancel);
        body.appendChild(row);
      } else {
        const region = selectField(body, "mv-region", "Region", t.regions.map(r => [r, r]), t.regions[0]);
        const ask = el("button", "btn", "Ask an admin");
        ask.type = "button";
        ask.onclick = async () => {
          ask.disabled = true;
          try {
            await withInFlight(api("POST", "/instances/" + encodeURIComponent(id) + "/region-request", { region: region.value }));
            close();
            toast("Asked. An admin of the controller will decide.");
            if (state.activeTab === "requests") loadRequests();
          } catch (e) {
            if (!(e && e.__auth)) showError((e && e.error) || "The request was not made.");
            ask.disabled = false;
          }
        };
        const row = el("div", "form-actions");
        row.style.justifyContent = "flex-start";
        row.appendChild(ask);
        body.appendChild(row);
      }
    }
    actions.appendChild(closeBtn);
    body.appendChild(actions);
  });
}

// ---------- roles and the Admin tab ----------

// What this page shows for its role. The node refuses a user's admin request
// anyway (api::user_may); this only stops offering what would be refused.
function applyRole() {
  const admin = state.role === "admin";
  for (const id of ["tab-admin", "tab-installed", "tab-interfaces"]) {
    const t = $(id);
    if (!t) continue;
    if (id === "tab-admin") t.hidden = !admin || !(authState ? authState.accounts || state.controllerOn : true);
    else t.hidden = !admin;
  }
  if (!admin && ["admin", "installed", "interfaces"].includes(state.activeTab)) setActiveTab("servers");
  $("games-grid").dataset.sig = "";
  renderGames();
}

// ---------- admin: workers, requests, accounts, adding a worker ----------
//
// One sub-view at a time, refreshed on a timer without a flicker: each pass
// fetches what the current view needs and rebuilds the view only when what it
// shows has changed, and never while a field in it has focus or a menu is open.
// Adding a worker is its own page (`#admin/add-worker`) with steps, so the
// lists are never mixed with a form.

const adm = {
  route: "workers",
  data: {},
  sig: { nav: "", view: "" },
  timer: null,
  loading: false,
  menu: null,
  loaded: false,
  code: null, // the join code just made: the secret is held here only, in memory
  done: null, // a worker that joined through the code
  method: "script",
  shell: null,
};

// ---- small pieces ----

function help(text) {
  const h = el("span", "help", "?");
  h.tabIndex = 0;
  h.dataset.tip = text;
  h.setAttribute("role", "img");
  h.setAttribute("aria-label", text);
  return h;
}

function titled(text, tip) {
  const t = el("h3", "panel-title");
  t.appendChild(document.createTextNode(text));
  if (tip) t.appendChild(help(tip));
  return t;
}

function chip(text, kind) {
  return el("span", "chip st-" + (kind || "muted"), text);
}

function labelled(parent, id, text, tip, input) {
  const wrap = el("div", "fld");
  const l = el("label", null, text);
  l.htmlFor = id;
  if (tip) l.appendChild(help(tip));
  input.id = id;
  input.classList.add("field");
  wrap.append(l, input);
  parent.appendChild(wrap);
  return input;
}

function textInput(attrs) {
  const i = el("input");
  i.type = "text";
  Object.assign(i, attrs || {});
  return i;
}

// One floating tip for every `?`, positioned from the icon so a table or a card
// that scrolls never clips it.
let tipEl = null;
function showTip(target) {
  if (!tipEl) { tipEl = el("div", "tip"); tipEl.setAttribute("role", "tooltip"); document.body.appendChild(tipEl); }
  tipEl.textContent = target.dataset.tip || "";
  tipEl.style.display = "block";
  const r = target.getBoundingClientRect();
  const w = tipEl.offsetWidth, h = tipEl.offsetHeight;
  let left = r.left + r.width / 2 - w / 2;
  left = Math.max(8, Math.min(left, window.innerWidth - w - 8));
  let top = r.bottom + 8;
  if (top + h > window.innerHeight - 8) top = Math.max(8, r.top - h - 8);
  tipEl.style.left = left + "px";
  tipEl.style.top = top + "px";
}
function hideTip() { if (tipEl) tipEl.style.display = "none"; }
document.addEventListener("mouseover", (e) => { const t = e.target.closest && e.target.closest(".help"); if (t) showTip(t); });
document.addEventListener("mouseout", (e) => { if (e.target.closest && e.target.closest(".help")) hideTip(); });
document.addEventListener("focusin", (e) => { const t = e.target.closest && e.target.closest(".help"); if (t) showTip(t); });
document.addEventListener("focusout", (e) => { if (e.target.closest && e.target.closest(".help")) hideTip(); });

function admCloseMenu() {
  if (adm.menu) adm.menu.remove();
  adm.menu = null;
}
document.addEventListener("keydown", (e) => { if (e.key === "Escape") admCloseMenu(); });

function admMenu(anchor, items) {
  admCloseMenu();
  const m = el("div", "pop-menu");
  for (const [label, fn, danger] of items) {
    const b = el("button", "pop-item" + (danger ? " danger" : ""), label);
    b.type = "button";
    b.onclick = () => { admCloseMenu(); fn(); };
    m.appendChild(b);
  }
  document.body.appendChild(m);
  const r = anchor.getBoundingClientRect();
  m.style.top = Math.max(8, Math.min(r.bottom + 4, window.innerHeight - m.offsetHeight - 8)) + "px";
  m.style.left = Math.max(8, Math.min(r.right - m.offsetWidth, window.innerWidth - m.offsetWidth - 8)) + "px";
  adm.menu = m;
  setTimeout(() => document.addEventListener("click", admCloseMenu, { once: true }), 0);
}

// A request that changes something: one at a time, then the view catches up.
async function admDo(call, done) {
  try {
    await withInFlight(call());
    clearError();
    if (done) toast(done);
    admRefresh(true);
  } catch (e) {
    if (!(e && e.__auth)) showError((e && e.error) || "That did not work.");
  }
}

// ---- routes ----

function admRoutes() {
  const out = [];
  const me = adm.data.me;
  if (state.controllerOn) {
    if (me && me.admin) out.push(["workers", "Workers"], ["servers", "Servers"], ["requests", "Requests"], ["admins", "Admins"]);
    else out.push(["admins", "Access"]);
  }
  if (authState && authState.signup && authState.signup.reports) out.push(["reports", "Reports"]);
  if (authState && authState.accounts) out.push(["accounts", "Accounts"]);
  return out;
}

function admRouteFromHash() {
  const m = /^#admin\/([a-z-]+)$/.exec(location.hash);
  return m ? m[1] : null;
}

function admGo(route) {
  if (adm.route === route) return;
  history.pushState(null, "", "#admin/" + route);
  adm.route = route;
  adm.sig.view = "";
  admRender();
  admRefresh(true);
}

window.addEventListener("popstate", () => {
  if (state.activeTab !== "admin") return;
  const r = admRouteFromHash();
  if (r && r !== adm.route) { adm.route = r; adm.sig.view = ""; admRender(); admRefresh(true); }
});

function admEnter() {
  const wanted = admRouteFromHash();
  if (wanted) adm.route = wanted;
  adm.sig = { nav: "", view: "" };
  adm.shell = null;
  adm.loaded = false;
  adm.data = {};
  const first = $("admin-body");
  if (first) first.replaceChildren(el("p", "empty", "Loading\u2026"));
  admRefresh(true);
  if (!adm.timer) adm.timer = setInterval(admTick, 4000);
}

async function admTick() {
  if (state.activeTab !== "admin") { clearInterval(adm.timer); adm.timer = null; return; }
  if (document.hidden || adm.menu) return;
  await admRefresh(false);
}

// Kept under its old name for the code that asks the Admin tab to catch up.
function loadAdmin() { return admRefresh(true); }

// ---- data ----

async function admFetch() {
  const d = { me: null };
  const r = adm.route;
  if (state.controllerOn) d.me = await api("GET", "/controller/me").catch(() => null);
  const admin = !!(d.me && d.me.admin);
  const jobs = [];
  const want = (key, path) => jobs.push(api("GET", path).then((v) => { d[key] = v; }).catch(() => { d[key] = adm.data[key] || null; d[key + "_failed"] = true; }));
  if (admin) {
    // The counts on the nav need these whichever view is open.
    want("moves", "/controller/admin/region-requests");
    want("asks", "/controller/admin/admin-requests");
    want("notices", "/controller/admin/notices");
    if (r === "workers") want("workers", "/controller/admin/workers");
    if (r === "servers") want("all", "/controller/admin/instances");
    if (r === "admins") want("admins", "/controller/admin/admins");
    if (r === "add-worker") { want("codes", "/controller/admin/join-codes"); want("info", "/controller/admin/join-info"); want("workers", "/controller/admin/workers"); }
  }
  if (authState && authState.signup && authState.signup.reports) want("reports", "/reports");
  if (authState && authState.accounts) {
    want("users", "/auth/users");
    want("self", "/auth/me");
  }
  await Promise.all(jobs);
  return d;
}

async function admRefresh(force) {
  if (!$("admin-body") || adm.loading) return;
  adm.loading = true;
  try {
    adm.data = await admFetch();
    adm.loaded = true;
    admWatchCode();
    admRender(force);
  } finally {
    adm.loading = false;
  }
}

// A join code turning "used" means the machine joined.
function admWatchCode() {
  if (!adm.code || !adm.data.codes) return;
  const mine = adm.data.codes.find((c) => c.id === adm.code.id);
  if (mine && mine.state === "used") {
    adm.done = { name: mine.used_name || "A worker", region: mine.spec && mine.spec.region };
    adm.code = null;
    adm.sig.view = "";
    toast(adm.done.name + " joined.");
  } else if (mine && mine.state !== "pending") {
    adm.code = null;
    adm.sig.view = "";
  } else if (adm.code && Date.now() / 1000 > adm.code.expires) {
    adm.code = null;
    adm.sig.view = "";
  }
}

// ---- rendering ----

function admPendingCount() {
  const d = adm.data;
  let n = 0;
  if (d.moves) n += d.moves.filter((r) => r.state === "pending").length;
  if (d.asks) n += d.asks.filter((r) => r.state === "pending").length;
  if (d.users) n += d.users.filter((u) => u.admin_requested_unix).length;
  if (d.reports) n += d.reports.filter((r) => !r.closed_unix).length;
  if (d.notices) n += d.notices.filter((x) => x.review_asked_unix).length;
  return n;
}

function admRender(force) {
  const body = $("admin-body");
  if (!body || !adm.loaded) return;
  const routes = admRoutes();
  if (!routes.length) {
    body.replaceChildren(el("p", "muted", "Nothing to manage on this node."));
    adm.shell = null;
    return;
  }
  const known = routes.some(([k]) => k === adm.route) || adm.route === "add-worker" && routes.some(([k]) => k === "workers");
  if (!known) adm.route = routes[0][0];

  const pending = admPendingCount();
  const n = $("nav-admin-count");
  if (n) n.textContent = pending ? String(pending) : "";

  if (!adm.shell) {
    adm.shell = { nav: el("div", "adm-nav"), view: el("div", "adm-view") };
    body.replaceChildren(adm.shell.nav, adm.shell.view);
    adm.sig = { nav: "", view: "" };
  }

  // Navigation: only on change.
  const navSig = JSON.stringify([routes, adm.route, pending]);
  if (adm.route === "add-worker") {
    if (adm.sig.nav !== "wizard") { adm.shell.nav.replaceChildren(); adm.sig.nav = "wizard"; }
  } else if (navSig !== adm.sig.nav) {
    adm.sig.nav = navSig;
    const tabs = el("div", "adm-tabs");
    for (const [key, label] of routes) {
      const b = el("button", "adm-tab" + (key === adm.route ? " active" : ""), label);
      b.type = "button";
      b.onclick = () => admGo(key);
      const requestsWaiting = admPendingBy("moves") + admPendingBy("asks");
      if (key === "requests" && requestsWaiting) b.appendChild(el("span", "adm-badge", String(requestsWaiting)));
      if (key === "reports" && adm.data.reports) {
        const open = adm.data.reports.filter((r) => !r.closed_unix).length;
        if (open) b.appendChild(el("span", "adm-badge", String(open)));
      }
      if (key === "servers" && adm.data.notices) {
        const asked = adm.data.notices.filter((x) => x.review_asked_unix).length;
        if (asked) b.appendChild(el("span", "adm-badge", String(asked)));
      }
      if (key === "accounts" && adm.data.users) {
        const asked = adm.data.users.filter((u) => u.admin_requested_unix).length;
        if (asked) b.appendChild(el("span", "adm-badge", String(asked)));
      }
      tabs.appendChild(b);
    }
    adm.shell.nav.replaceChildren(tabs);
  }

  // The view: only when what it shows changed, and not under a typing hand.
  const d = adm.data;
  const sig = JSON.stringify([adm.route, adm.route === "add-worker" ? [adm.code && adm.code.id, adm.done, adm.method] : null,
    adm.route === "workers" ? workersSig(d.workers) : null, adm.route === "requests" ? [d.moves, d.asks] : null,
    adm.route === "admins" ? [d.admins, d.me] : null, adm.route === "accounts" ? [d.users, d.self] : null,
    adm.route === "add-worker" ? (d.codes || []).map((c) => [c.id, c.state]) : null,
    adm.route === "servers" ? [(d.all || []).map((i) => [i.instance_id, i.state, i.node, i.owner]), d.notices] : null,
    adm.route === "reports" ? d.reports : null]);
  if (!force && sig === adm.sig.view) { admLive(); return; }
  const focused = document.activeElement;
  if (!force && focused && adm.shell.view.contains(focused) && /^(INPUT|SELECT|TEXTAREA)$/.test(focused.tagName)) return;
  if (sig === adm.sig.view && !force) { admLive(); return; }
  adm.sig.view = sig;
  const keep = {};
  for (const id of ["wz-name", "wz-region", "wz-class", "wz-priority", "wz-reserved", "wz-life", "adm-add-user", "adm-add-role"]) {
    const f = $(id);
    if (f) keep[id] = f.value;
  }
  const addOpen = $("adm-add-box") && !$("adm-add-box").classList.contains("hidden");

  let view;
  switch (adm.route) {
    case "workers": view = admWorkersView(); break;
    case "requests": view = admRequestsView(); break;
    case "servers": view = admServersView(); break;
    case "reports": view = admReportsView(); break;
    case "admins": view = admAdminsView(); break;
    case "accounts": view = admAccountsView(); break;
    case "add-worker": view = admWizardView(); break;
    default: view = el("div");
  }
  adm.shell.view.replaceChildren(view);
  for (const [id, v] of Object.entries(keep)) { const f = $(id); if (f) f.value = v; }
  if (addOpen && $("adm-add-box")) $("adm-add-box").classList.remove("hidden");
  admLive();
}

// What the workers table shows, as shown: free memory moves every poll, and a
// rebuild for a change nobody can see would be a flicker.
function workersSig(ws) {
  return (ws || []).map((w) => {
    const c = w.capacity || {};
    return [w.name, w.region, w.class, w.state, w.note, w.answered, w.servers, w.reserved_for, c.running, c.max_instances,
      c.memory_available_bytes != null ? formatBytes(c.memory_available_bytes) : null,
      c.disk_free_bytes != null ? formatBytes(c.disk_free_bytes) : null];
  });
}

function admPendingBy(key) {
  const v = adm.data[key];
  return v ? v.filter((r) => r.state === "pending").length : 0;
}

// Things that tick without a rebuild: a code's countdown.
function admLive() {
  const live = $("wz-live");
  if (live && adm.code) {
    const left = Math.max(0, Math.floor(adm.code.expires - Date.now() / 1000));
    const m = Math.floor(left / 60), s = left % 60;
    live.textContent = "Waiting for the machine to join · the code ends in " + m + ":" + String(s).padStart(2, "0");
  }
}

// -- workers --

function statTile(label, value) {
  const t = el("div", "stat");
  t.append(el("div", "stat-value", String(value)), el("div", "stat-label", label));
  return t;
}

function admWorkersView() {
  const ws = adm.data.workers || [];
  const root = el("div", "adm-stack");

  const stats = el("div", "stats");
  stats.append(
    statTile("Workers", ws.length),
    statTile("Answering", ws.filter((w) => w.answered).length + " of " + ws.length),
    statTile("Servers", ws.reduce((n, w) => n + (w.servers || 0), 0)),
    statTile("Regions", new Set(ws.map((w) => w.region).filter(Boolean)).size),
  );
  root.appendChild(stats);

  const panel = el("div", "panel");
  const head = el("div", "panel-head");
  head.appendChild(titled("Workers", "A worker is a machine that runs game servers for this controller. Cordon stops it taking new servers; Drain also moves its servers to other workers (within each one's region, class and reservation); Restart restarts the games on it, not the agent on the machine."));
  const add = el("button", "btn btn-primary", "Add worker");
  add.type = "button";
  add.onclick = () => { adm.done = null; admGo("add-worker"); };
  head.appendChild(add);
  panel.appendChild(head);

  if (!ws.length) {
    panel.appendChild(el("p", "empty", adm.data.workers_failed ? "The controller did not answer." : "No workers yet. Add one to start placing servers."));
    root.appendChild(panel);
    return root;
  }
  const showClass = ws.some((w) => w.class);
  const wrap = el("div", "table-wrap");
  const table = el("table", "adm-table");
  const tr0 = el("tr");
  for (const h of ["Worker", "Region"].concat(showClass ? ["Class"] : [], ["State", "Servers", "Load", "Memory", "Disk", ""])) tr0.appendChild(el("th", null, h));
  table.appendChild(tr0);
  for (const w of ws) {
    const c = w.capacity || {};
    const tr = el("tr");
    const who = el("td", "who");
    const line = el("div", "who-line");
    const dot = el("span", "dot " + (w.answered ? "up" : "down"));
    dot.title = w.answered ? "Answering" : "Not answering";
    line.append(dot, el("span", "who-name", w.name));
    if (w.reserved_for) { const c2 = chip("reserved", "info"); c2.title = "Reserved for account " + w.reserved_for; line.appendChild(c2); }
    who.appendChild(line);
    if (w.note) who.appendChild(el("div", "who-note", w.note));
    tr.appendChild(who);
    tr.appendChild(el("td", null, w.region || "—"));
    if (showClass) tr.appendChild(el("td", null, w.class || "—"));
    const st = el("td");
    st.appendChild(chip(w.state || "active", w.state === "draining" ? "busy" : w.state === "cordoned" ? "warn" : "ok"));
    tr.appendChild(st);
    tr.appendChild(el("td", null, String(w.servers)));
    const load = el("td");
    if (c.max_instances != null) {
      const bar = el("div", "bar");
      const fill = el("div", "bar-fill");
      fill.style.width = Math.min(100, Math.round((c.running / Math.max(1, c.max_instances)) * 100)) + "%";
      bar.appendChild(fill);
      load.append(el("span", "load-text", c.running + " / " + c.max_instances), bar);
    } else load.textContent = "—";
    tr.appendChild(load);
    tr.appendChild(el("td", null, c.memory_available_bytes != null ? formatBytes(c.memory_available_bytes) : "—"));
    tr.appendChild(el("td", null, c.disk_free_bytes != null ? formatBytes(c.disk_free_bytes) : "—"));
    const acts = el("td", "act");
    const menu = el("button", "btn btn-sm", "Manage");
    menu.type = "button";
    menu.setAttribute("aria-haspopup", "menu");
    menu.onclick = (e) => {
      e.stopPropagation();
      const post = (action, done) => () => admDo(() => api("POST", "/controller/admin/workers/" + encodeURIComponent(w.name) + "/" + action), done);
      const items = [];
      if (w.state === "active") {
        items.push(["Cordon", () => { if (confirm("Stop " + w.name + " taking new servers? Its servers keep running.")) post("cordon", w.name + " is cordoned.")(); }]);
        items.push(["Drain", () => { if (confirm("Drain " + w.name + "? It takes no new servers and its servers are moved to other workers, one at a time.")) post("drain", w.name + " is draining.")(); }]);
      } else {
        items.push(["Put back in service", post("uncordon", w.name + " is back in service.")]);
        if (w.state === "cordoned") items.push(["Drain", () => { if (confirm("Move " + w.name + "'s servers to other workers, one at a time?")) post("drain", w.name + " is draining.")(); }]);
      }
      items.push(["Restart servers", () => { if (confirm("Restart every running server on " + w.name + ", one at a time? Players on them are dropped.")) post("restart", "Restarting servers on " + w.name + ".")(); }, true]);
      admMenu(menu, items);
    };
    acts.appendChild(menu);
    tr.appendChild(acts);
    table.appendChild(tr);
  }
  wrap.appendChild(table);
  panel.appendChild(wrap);
  root.appendChild(panel);
  return root;
}

// -- requests --

// -- every server, for acting on a report or a breach --

function admServersView() {
  const all = adm.data.all || [];
  const root = el("div", "adm-stack");
  const panel = el("div", "panel");
  panel.appendChild(titled("All servers", "Every server on every worker that answered, whoever owns it. Stop or remove one that breaks the rules: you give a reason, and its owner sees what was done and why on their Account tab, and may ask you to look again."));
  if (!all.length) {
    panel.appendChild(el("p", "empty", adm.data.all_failed ? "The controller did not answer." : "No servers."));
  } else {
    const wrap = el("div", "table-wrap");
    const table = el("table", "adm-table");
    const head = el("tr");
    for (const h of ["Server", "Game", "Owner", "Worker", "State", ""]) head.appendChild(el("th", null, h));
    table.appendChild(head);
    for (const i of all) {
      const tr = el("tr");
      tr.append(el("td", null, i.name || i.instance_id), el("td", null, gameName(i.game_id)),
        el("td", "mono small", String(i.owner || "").slice(0, 12) + (i.owner ? "\u2026" : "")),
        el("td", null, i.node || ""), el("td", null, i.state || ""));
      const td = el("td", "row-actions");
      const act = el("button", "btn btn-sm", "Act\u2026");
      act.type = "button";
      const moderate = (action, label) => () => {
        const reason = prompt(label + " \u201c" + (i.name || i.instance_id) + "\u201d. The owner reads this reason:", "");
        if (reason === null) return;
        if (!reason.trim()) { showError("Give the owner a reason."); return; }
        admDo(() => api("POST", "/controller/admin/instances/" + encodeURIComponent(i.instance_id) + "/" + action, { reason }),
          action === "stop" ? "Stopped. The owner was told why." : "Removed. The owner was told why.");
      };
      act.onclick = (e) => {
        e.stopPropagation();
        admMenu(act, [["Stop with a reason\u2026", moderate("stop", "Stop")], ["Remove with a reason\u2026", moderate("remove", "Remove"), true]]);
      };
      td.appendChild(act);
      tr.appendChild(td);
      table.appendChild(tr);
    }
    wrap.appendChild(table);
    panel.appendChild(wrap);
  }
  root.appendChild(panel);

  const notices = adm.data.notices || [];
  if (notices.length) {
    const np = el("div", "panel");
    np.appendChild(titled("What admins did", "Each stop or removal and its reason, as the owner sees it. One whose owner asked for another look says what they wrote."));
    for (const n of notices) np.appendChild(noticeCard(n, true));
    root.appendChild(np);
  }
  return root;
}

function noticeCard(n, admin) {
  const card = el("div", "req-card");
  const head = el("div", "req-head");
  head.appendChild(el("span", "req-line", (n.action === "removed" ? "Removed " : "Stopped ") + (n.name || n.instance_id) +
    " \u00b7 " + new Date(n.created_unix * 1000).toLocaleString()));
  if (n.review_asked_unix) head.appendChild(chip("look again asked", "warn"));
  card.appendChild(head);
  if (admin) card.appendChild(el("p", "muted small", "Owner " + String(n.owner || "").slice(0, 12) + "\u2026"));
  card.appendChild(el("p", "notice-text", "Reason: " + n.reason));
  if (n.review_note) card.appendChild(el("p", "notice-text", "Owner wrote: " + n.review_note));
  return card;
}

// -- reports from anyone --

function admReportsView() {
  const reports = adm.data.reports || [];
  const root = el("div", "adm-stack");
  const panel = el("div", "panel");
  panel.appendChild(titled("Reports", "Anyone can report a server from the sign-in page, without an account. Look at the server, act on it from Servers if it breaks the rules (the owner is told why), then close the report saying what was done."));
  if (!reports.length) panel.appendChild(el("p", "empty", adm.data.reports_failed ? "Reports could not be read." : "No reports."));
  for (const r of reports) {
    const card = el("div", "req-card");
    const head = el("div", "req-head");
    head.appendChild(el("span", "req-line", r.target + " \u00b7 " + new Date(r.created_unix * 1000).toLocaleString()));
    head.appendChild(chip(r.category, r.category === "illegal" ? "bad" : "info"));
    if (r.closed_unix) head.appendChild(chip("closed", "muted"));
    card.appendChild(head);
    card.appendChild(el("p", "notice-text", r.details));
    if (r.contact) card.appendChild(el("p", "muted small", "Contact: " + r.contact));
    if (r.closed_unix) {
      card.appendChild(el("p", "muted small", "Closed by " + (r.closed_by || "?") + ": " + (r.outcome || "")));
    } else {
      const row = el("div", "form-actions start");
      const close = el("button", "btn", "Close\u2026");
      close.type = "button";
      close.onclick = () => {
        const outcome = prompt("What was done?", "");
        if (outcome === null) return;
        if (!outcome.trim()) { showError("Say what was done."); return; }
        admDo(() => api("POST", "/reports/" + encodeURIComponent(r.id) + "/close", { outcome }), "Report closed.");
      };
      row.appendChild(close);
      card.appendChild(row);
    }
    panel.appendChild(card);
  }
  root.appendChild(panel);
  return root;
}

function admRequestsView() {
  const root = el("div", "adm-stack");
  const moves = (adm.data.moves || []).filter((r) => r.state === "pending");
  const asks = (adm.data.asks || []).filter((r) => r.state === "pending");

  const mp = el("div", "panel");
  const mh = el("div", "panel-head");
  mh.appendChild(titled("Region moves", "A server stays in its worker's region. Its owner can ask for another; approving moves it to the worker with the most room there, and that becomes its home."));
  if (moves.length) mh.appendChild(chip(String(moves.length), "warn"));
  mp.appendChild(mh);
  if (!moves.length) mp.appendChild(el("p", "empty", "Nothing waiting."));
  for (const r of moves) mp.appendChild(requestCard(r, true));
  root.appendChild(mp);

  const ap = el("div", "panel");
  const ah = el("div", "panel-head");
  ah.appendChild(titled("Admin requests", "Anyone with an account on this controller can ask to become an admin of it. An admin decides here."));
  if (asks.length) ah.appendChild(chip(String(asks.length), "warn"));
  ap.appendChild(ah);
  if (!asks.length) ap.appendChild(el("p", "empty", "Nothing waiting."));
  for (const r of asks) {
    const row = el("div", "row");
    const text = el("div", "row-main");
    text.appendChild(el("div", "row-title", r.identity.slice(0, 12) + "…"));
    text.appendChild(el("div", "row-sub", "asked " + new Date(r.created_unix * 1000).toLocaleString() + (r.note ? " · “" + r.note + "”" : "")));
    const acts = el("div", "row-acts");
    const ok = el("button", "btn btn-primary btn-sm", "Make admin");
    ok.type = "button";
    ok.onclick = () => admDo(() => api("POST", "/controller/admin/admin-requests/" + r.id + "/approve"), "Made an admin.");
    const no = el("button", "btn btn-sm", "Turn down");
    no.type = "button";
    no.onclick = () => {
      const why = prompt("Why? They see this (optional).", "");
      if (why === null) return;
      admDo(() => api("POST", "/controller/admin/admin-requests/" + r.id + "/deny", why ? { note: why } : {}));
    };
    acts.append(ok, no);
    row.append(text, acts);
    ap.appendChild(row);
  }
  root.appendChild(ap);
  return root;
}

// -- admins / access --

function admAdminsView() {
  const root = el("div", "adm-stack");
  const me = adm.data.me;
  const panel = el("div", "panel");
  if (!me || !me.admin) {
    panel.appendChild(titled("Access to the controller", "An admin of the controller decides region moves, who else is an admin, and the workers. This node is not one."));
    if (!me) { panel.appendChild(el("p", "empty", "The controller did not answer.")); root.appendChild(panel); return root; }
    const r = me.admin_request;
    if (r && r.state === "pending") {
      panel.appendChild(el("p", null, "You asked to become an admin. Waiting for one to decide."));
    } else {
      if (r && r.state === "denied") panel.appendChild(el("p", "muted", "The last request was turned down" + (r.reason ? ": " + r.reason : ".")));
      const note = textInput({ maxLength: 300, placeholder: "Who you are, why" });
      const form = el("div", "form-grid one");
      labelled(form, "adm-ask-note", "Note for the admins", "Optional. Shown to the admins who decide.", note);
      panel.appendChild(form);
      const ask = el("button", "btn btn-primary", "Ask to become an admin");
      ask.type = "button";
      ask.onclick = () => admDo(() => api("POST", "/controller/admin-requests", note.value.trim() ? { note: note.value.trim() } : {}), "Asked.");
      panel.appendChild(ask);
    }
    root.appendChild(panel);
    return root;
  }
  const head = el("div", "panel-head");
  head.appendChild(titled("Admins", "Admins written in the hosting file can only be changed there. Others were granted here and can be revoked."));
  panel.appendChild(head);
  for (const a of adm.data.admins || []) {
    const row = el("div", "row");
    const text = el("div", "row-main");
    text.appendChild(el("div", "row-title mono", a.identity.slice(0, 16) + "…" + (a.identity === me.identity ? "  (this node)" : "")));
    text.appendChild(el("div", "row-sub", a.source === "file" ? "In the hosting file" : "Granted"));
    row.appendChild(text);
    if (a.source === "granted") {
      const rv = el("button", "btn btn-sm", "Revoke");
      rv.type = "button";
      rv.onclick = () => { if (confirm("Stop " + a.identity.slice(0, 12) + "… being an admin of the controller?")) admDo(() => api("DELETE", "/controller/admin/admins/" + a.identity), "Revoked."); };
      row.appendChild(rv);
    }
    panel.appendChild(row);
  }
  root.appendChild(panel);
  return root;
}

// -- accounts --

function admAccountsView() {
  const root = el("div", "adm-stack");
  const users = adm.data.users || [];
  const self = adm.data.self || {};
  const panel = el("div", "panel");
  const head = el("div", "panel-head");
  head.appendChild(titled("Accounts on this node", "An admin has the API token's reach. A user runs servers: create, start, stop, console, backups and the like, and nothing else. A user can ask to become an admin; an admin decides."));
  const add = el("button", "btn btn-primary", "Add account");
  add.type = "button";
  add.onclick = () => {
    const box = $("adm-add-box");
    if (box) { box.classList.toggle("hidden"); if (!box.classList.contains("hidden")) $("adm-add-user").focus(); }
  };
  head.appendChild(add);
  panel.appendChild(head);

  const box = el("div", "subpanel hidden");
  box.id = "adm-add-box";
  const grid = el("div", "form-grid");
  const nu = labelled(grid, "adm-add-user", "Username", null, textInput({ autocapitalize: "none", spellcheck: false }));
  const pw = el("input"); pw.type = "password"; pw.autocomplete = "new-password";
  const np = labelled(grid, "adm-add-pass", "Password", "At least 10 characters.", pw);
  const sel = el("select");
  for (const [v, t] of [["user", "User"], ["admin", "Admin"]]) { const o = el("option", null, t); o.value = v; sel.appendChild(o); }
  const nr = labelled(grid, "adm-add-role", "Role", "A user runs servers. An admin also manages accounts, games, the mesh and the controller.", sel);
  box.appendChild(grid);
  const go = el("button", "btn btn-primary", "Create account");
  go.type = "button";
  go.onclick = () => admDo(() => api("POST", "/auth/users", { username: nu.value.trim(), password: np.value, role: nr.value }), "Account added.");
  box.appendChild(go);
  panel.appendChild(box);

  const wrap = el("div", "table-wrap");
  const table = el("table", "adm-table");
  const tr0 = el("tr");
  for (const h of ["Account", "Role", "Codes", ""]) tr0.appendChild(el("th", null, h));
  table.appendChild(tr0);
  for (const u of users) {
    const tr = el("tr");
    const nm = el("td", "who");
    nm.appendChild(el("div", "who-name", u.username + (u.username === self.username ? "  (you)" : "")));
    if (u.admin_requested_unix) nm.appendChild(el("div", "who-note", "asked to become an admin"));
    tr.appendChild(nm);
    const rc = el("td"); rc.appendChild(chip(u.role, u.role === "admin" ? "info" : "muted")); tr.appendChild(rc);
    tr.appendChild(el("td", null, u.totp_enabled ? "on" : "off"));
    const acts = el("td", "act");
    const path = "/auth/users/" + encodeURIComponent(u.username);
    const menu = el("button", "btn btn-sm", "Manage");
    menu.type = "button";
    menu.onclick = (e) => {
      e.stopPropagation();
      const items = [];
      if (u.admin_requested_unix) {
        items.push(["Approve as admin", () => admDo(() => api("POST", path + "/role", { role: "admin" }), u.username + " is an admin.")]);
        items.push(["Turn the request down", () => admDo(() => api("POST", path + "/deny-admin"))]);
      } else if (u.role === "user") {
        items.push(["Make admin", () => admDo(() => api("POST", path + "/role", { role: "admin" }), u.username + " is an admin.")]);
      } else if (u.username !== self.username) {
        items.push(["Make user", () => admDo(() => api("POST", path + "/role", { role: "user" }), u.username + " is a user.")]);
      }
      if (u.username !== self.username) {
        items.push(["Remove", () => { if (confirm("Remove the account " + u.username + "? It is signed out everywhere.")) admDo(() => api("DELETE", path), "Removed."); }, true]);
      }
      if (!items.length) { toast("Nothing to change on your own account here."); return; }
      admMenu(menu, items);
    };
    acts.appendChild(menu);
    tr.appendChild(acts);
    table.appendChild(tr);
  }
  wrap.appendChild(table);
  panel.appendChild(wrap);
  root.appendChild(panel);
  return root;
}

// -- adding a worker: its own page --

function joinCommands(url, token) {
  const dir = '"$HOME/lanthorn-worker"';
  return {
    script: "curl -fsSL " + url + "/join.sh | bash -s -- --url " + url + " --token " + token,
    docker: "mkdir -p " + dir + " && docker run -d --name lanthorn-worker --restart unless-stopped \\\n" +
      "  --user \"$(id -u):$(id -g)\" --group-add \"$(getent group docker | cut -d: -f3)\" \\\n" +
      "  -v /var/run/docker.sock:/var/run/docker.sock -v " + dir + ":" + dir + " \\\n" +
      "  ghcr.io/idan2025/gaming-platform-prns:latest \\\n" +
      "  platform-agent join --url " + url + " --token " + token + " --dir " + dir + " --run",
    binary: "platform-agent join --url " + url + " --token " + token + " --run",
  };
}

function wizardSteps(active) {
  const ol = el("ol", "steps");
  ["Details", "Connect", "Done"].forEach((label, i) => {
    const li = el("li", i < active ? "done" : i === active ? "active" : "", label);
    li.prepend(el("span", "step-n", String(i + 1)));
    ol.appendChild(li);
  });
  return ol;
}

function admWizardView() {
  const root = el("div", "adm-page");
  const top = el("div", "page-top");
  const back = el("button", "link", "← Workers");
  back.type = "button";
  back.onclick = () => admGo("workers");
  top.appendChild(back);
  root.appendChild(top);
  root.appendChild(el("h2", "page-h", "Add a worker"));

  const info = adm.data.info;
  if (adm.done) {
    root.appendChild(wizardSteps(3));
    const p = el("div", "panel center");
    p.appendChild(el("div", "big-ok", "✓"));
    p.appendChild(el("h3", null, adm.done.name + " joined" + (adm.done.region ? " in " + adm.done.region : "")));
    p.appendChild(el("p", "muted", "It shows in the Workers table once the controller has asked it for its room. It runs no games until you add them on that machine."));
    const row = el("div", "btn-row center");
    const again = el("button", "btn", "Add another");
    again.type = "button";
    again.onclick = () => { adm.done = null; adm.sig.view = ""; admRender(true); };
    const view = el("button", "btn btn-primary", "View workers");
    view.type = "button";
    view.onclick = () => { adm.done = null; admGo("workers"); };
    row.append(again, view);
    p.appendChild(row);
    root.appendChild(p);
    return root;
  }

  if (!info) {
    root.appendChild(el("p", "empty", "Asking the controller…"));
    return root;
  }
  if (!info.hubs || !info.hubs.length) {
    const p = el("div", "panel");
    p.appendChild(titled("One setting first", "New workers dial a hub to reach the mesh. Write one that they can reach into the index's hosting file; it is read within 10 seconds."));
    p.appendChild(el("pre", "mono code", '[join]\nhubs = ["host:4790"]\npublic_url = "http://host:4761"'));
    root.appendChild(p);
    return root;
  }

  if (adm.code) {
    root.appendChild(wizardSteps(1));
    const j = adm.code;
    const p = el("div", "panel");
    const where = [j.spec.name, j.spec.region && ("region " + j.spec.region), j.spec.class && ("class " + j.spec.class), j.spec.reserved_for && "reserved"].filter(Boolean).join(" · ");
    p.appendChild(titled("Run this on the new machine", "The code works once and is shown only on this page. The machine makes its own identity and tells the controller who it is; where it sits comes from the code."));
    if (where) p.appendChild(el("p", "muted", "It will join as " + where + "."));
    const url = textInput({ value: j.url, placeholder: "http://192.168.1.10:4761" });
    const grid = el("div", "form-grid one");
    labelled(grid, "wz-url", "Controller address", "The index's HTTP address as the new machine reaches it. The page cannot know it, so check it.", url);
    p.appendChild(grid);

    const tabs = el("div", "seg");
    const pre = el("pre", "mono code");
    pre.id = "wz-cmd";
    const tips = {
      script: "Needs Docker. Pulls the platform image and starts one container that joins and then runs as the worker. Docker restarts it after a reboot.",
      docker: "The same docker run the script does, to paste by hand.",
      binary: "For a machine that already has platform-agent. Add --run, as here, to start it after joining.",
    };
    const copy = el("button", "btn btn-sm", "Copy");
    copy.type = "button";
    const draw = () => {
      j.url = url.value.trim().replace(/\/+$/, "");
      const ok = /^https?:\/\/[A-Za-z0-9.:-]+$/.test(j.url);
      pre.textContent = ok ? joinCommands(j.url, j.token)[adm.method] : "Type the controller's address above.";
      copy.disabled = !ok;
      for (const b of tabs.querySelectorAll("button")) b.classList.toggle("on", b.dataset.m === adm.method);
      tipHolder.dataset.tip = tips[adm.method];
    };
    const tipHolder = help(tips[adm.method]);
    for (const [m, label] of [["script", "Script"], ["docker", "Docker"], ["binary", "platform-agent"]]) {
      const b = el("button", "seg-b", label);
      b.type = "button"; b.dataset.m = m;
      b.onclick = () => { adm.method = m; draw(); };
      tabs.appendChild(b);
    }
    const methodRow = el("div", "method-row");
    methodRow.append(tabs, tipHolder);
    p.appendChild(methodRow);
    p.appendChild(pre);
    url.oninput = draw;
    copy.onclick = () => copyValue(pre.textContent);
    const row = el("div", "btn-row");
    const revoke = el("button", "btn", "Revoke and start over");
    revoke.type = "button";
    revoke.onclick = () => {
      const id = j.id;
      adm.code = null;
      adm.sig.view = "";
      api("DELETE", "/controller/admin/join-codes/" + id).catch(() => {}).then(() => admRefresh(true));
    };
    row.append(copy, revoke);
    p.appendChild(row);
    const live = el("div", "live");
    live.append(el("span", "spinner"), (() => { const s = el("span", null, ""); s.id = "wz-live"; return s; })());
    p.appendChild(live);
    root.appendChild(p);
    draw();
    return root;
  }

  root.appendChild(wizardSteps(0));
  const p = el("div", "panel");
  p.appendChild(titled("Where it will sit", "A join code is good for one machine, once. It fixes these settings; the machine only says who it is."));
  const grid = el("div", "form-grid");
  const regions = [...new Set((adm.data.workers || []).map((w) => w.region).filter(Boolean))].sort();
  const name = labelled(grid, "wz-name", "Name", "Shown in the Workers table. Leave it empty to use the machine's host name.", textInput({ maxLength: 64, placeholder: "basement-pi" }));
  const region = labelled(grid, "wz-region", "Region", "Where the machine is. A create can ask for a region, and a move stays inside one. Existing regions are suggested.", textInput({ maxLength: 32, placeholder: "eu, il, us-east" }));
  const list = el("datalist");
  list.id = "wz-regions";
  for (const r of regions) { const o = el("option"); o.value = r; list.appendChild(o); }
  region.setAttribute("list", "wz-regions");
  grid.appendChild(list);
  const klass = labelled(grid, "wz-class", "Class", "A label for a special kind of worker. A worker with a class takes only servers that ask for it; one without takes the rest.", textInput({ maxLength: 32 }));
  const prio = el("input"); prio.type = "number"; prio.step = "1"; prio.value = "0";
  labelled(grid, "wz-priority", "Priority", "Higher fills first. Workers with equal priority go by free memory, then free slots.", prio);
  const reserved = labelled(grid, "wz-reserved", "Reserved for", "Optional. An account's identity hash (32 hex characters). Only that account's servers are placed on this worker, and the shared pool never uses it.", textInput({ maxLength: 32, placeholder: "32 hex characters", spellcheck: false }));
  const life = el("select");
  for (const [m, label] of [[15, "15 minutes"], [60, "1 hour"], [240, "4 hours"], [1440, "1 day"]]) { const o = el("option", null, label); o.value = String(m); if (m === 60) o.selected = true; life.appendChild(o); }
  labelled(grid, "wz-life", "Code valid for", "The code works once, and stops working after this.", life);
  p.appendChild(grid);
  const row = el("div", "btn-row");
  const cancel = el("button", "btn", "Cancel");
  cancel.type = "button";
  cancel.onclick = () => admGo("workers");
  const make = el("button", "btn btn-primary", "Create join code");
  make.type = "button";
  make.onclick = async () => {
    if (make.disabled) return;
    make.disabled = true;
    try {
      const made = await withInFlight(api("POST", "/controller/admin/join-codes", {
        name: name.value.trim(), region: region.value.trim(), class: klass.value.trim(),
        priority: parseInt(prio.value, 10) || 0, valid_minutes: parseInt(life.value, 10),
        reserved_for: reserved.value.trim().toLowerCase(),
      }));
      adm.code = { id: made.code.id, token: made.token, url: made.public_url || "", spec: made.code.spec, expires: made.code.expires_unix };
      adm.sig.view = "";
      admRender(true);
    } catch (e) {
      make.disabled = false;
      if (!(e && e.__auth)) showError((e && e.error) || "No code was made.");
    }
  };
  row.append(cancel, make);
  p.appendChild(row);
  root.appendChild(p);

  // Codes made before: waiting ones can be taken back.
  const codes = (adm.data.codes || []).filter((c) => c.state === "pending");
  if (codes.length) {
    const cp = el("div", "panel");
    cp.appendChild(titled("Codes waiting", "A code that was made but not used yet. Revoke one you no longer want."));
    for (const c of codes) {
      const r = el("div", "row");
      const where = [c.spec.name, c.spec.region && ("region " + c.spec.region), c.spec.class && ("class " + c.spec.class)].filter(Boolean).join(" · ") || "any worker";
      const text = el("div", "row-main");
      text.append(el("div", "row-title", where), el("div", "row-sub", "until " + new Date(c.expires_unix * 1000).toLocaleString()));
      const rv = el("button", "btn btn-sm", "Revoke");
      rv.type = "button";
      rv.onclick = () => admDo(() => api("DELETE", "/controller/admin/join-codes/" + c.id), "Revoked.");
      r.append(text, rv);
      cp.appendChild(r);
    }
    root.appendChild(cp);
  }
  return root;
}

// ---------- account (accounts.rs) ----------

function section(parent, title, tip) {
  const box = el("div", "panel");
  const head = el("div", "panel-head");
  head.appendChild(titled(title, tip));
  box.appendChild(head);
  parent.appendChild(box);
  return box;
}

async function loadAccount() {
  const root = $("account-body");
  if (!root) return;
  root.textContent = "";
  let me = { via: "token" };
  try {
    me = await api("GET", "/auth/me");
  } catch (e) {
    if (!(e && e.__auth)) root.appendChild(el("p", "muted small", (e && e.error) || "Could not read the accounts."));
    return;
  }
  const isAccount = me.via === "account";
  const who = el("div", "acct-who");
  who.appendChild(el("span", "who-name", isAccount ? me.username : "API token"));
  who.appendChild(chip(isAccount ? me.role : "token", isAccount && me.role === "user" ? "muted" : "info"));
  if (!isAccount) who.appendChild(help("You signed in with the API token. Make an account on the Admin tab and sign in with it to change a password or turn on codes."));
  root.appendChild(who);

  if (isAccount) {
    const pw = section(root, "Password", "Changing it signs this account out everywhere else. At least 10 characters.");
    const cur = field(pw, "acct-cur", "Current password", { type: "password", autocomplete: "current-password" });
    const nw = field(pw, "acct-new", "New password (at least 10 characters)", { type: "password", autocomplete: "new-password" });
    const nw2 = field(pw, "acct-new2", "The new password again", { type: "password", autocomplete: "new-password" });
    const save = el("button", "btn btn-primary", "Change password");
    save.type = "button";
    save.onclick = async () => {
      if (nw.value !== nw2.value) { showError("The two new passwords are not the same."); return; }
      try {
        await withInFlight(api("POST", "/auth/password", { current: cur.value, new: nw.value }));
        cur.value = nw.value = nw2.value = "";
        clearError();
        toast("Password changed. Other sign-ins of this account were ended.");
      } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "The password was not changed."); }
    };
    const r = el("div", "form-actions"); r.style.justifyContent = "flex-start"; r.appendChild(save); pw.appendChild(r);

    const tf = section(root, "Sign-in codes", "Ask for a code from an authenticator app (Aegis, Google Authenticator, 1Password) at every sign-in. If the phone is lost, the API token still lets you in.");
    tf.querySelector(".panel-head").appendChild(chip(me.totp_enabled ? "on" : "off", me.totp_enabled ? "ok" : "muted"));
    if (me.totp_enabled) {
      const code = field(tf, "acct-off-code", "A current code", { type: "text", inputMode: "numeric", maxLength: 8 });
      const off = el("button", "btn", "Turn codes off");
      off.type = "button";
      off.onclick = async () => {
        try { await withInFlight(api("POST", "/auth/totp/disable", { code: code.value.trim() })); toast("Codes are off."); loadAccount(); }
        catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "Codes were not turned off."); }
      };
      const r2 = el("div", "form-actions"); r2.style.justifyContent = "flex-start"; r2.appendChild(off); tf.appendChild(r2);
    } else {
      const begin = el("button", "btn btn-primary", "Turn codes on");
      begin.type = "button";
      begin.onclick = async () => {
        let t;
        try { t = await withInFlight(api("POST", "/auth/totp/begin")); }
        catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "Could not start."); return; }
        begin.remove();
        tf.appendChild(el("p", "muted", "Scan this with your authenticator app, or type the key into it, then enter the code it shows."));
        const img = el("img", "totp-qr");
        img.alt = "QR code for your authenticator app";
        img.src = "data:image/svg+xml;base64," + btoa(t.qr_svg);
        tf.appendChild(img);
        const key = el("p", "mono small", t.secret.replace(/(.{4})/g, "$1 ").trim());
        tf.appendChild(key);
        const code = field(tf, "acct-on-code", "The code it shows", { type: "text", inputMode: "numeric", maxLength: 8 });
        code.classList.add("field");
        const confirmBtn = el("button", "btn btn-primary", "Confirm");
        confirmBtn.type = "button";
        confirmBtn.onclick = async () => {
          try { await withInFlight(api("POST", "/auth/totp/enable", { code: code.value.trim() })); toast("Codes are on."); loadAccount(); }
          catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "That code was not accepted."); }
        };
        const r3 = el("div", "form-actions"); r3.style.justifyContent = "flex-start"; r3.appendChild(confirmBtn); tf.appendChild(r3);
        code.focus();
      };
      const r4 = el("div", "form-actions"); r4.style.justifyContent = "flex-start"; r4.appendChild(begin); tf.appendChild(r4);
    }
  }

  if (isAccount && me.role === "user" && state.controllerOn) {
    let notices = [];
    try { notices = await api("GET", "/controller/me/notices"); } catch (_) { notices = null; }
    if (notices && notices.length) {
      const nb = section(root, "Notices", "When an admin stops or removes one of your servers, this says what and why. If you think it was a mistake, ask them to look again, once per notice.");
      for (const n of notices) {
        const card = noticeCard(n, false);
        if (!n.review_asked_unix) {
          const row = el("div", "form-actions start");
          const ask = el("button", "btn", "Ask to look again\u2026");
          ask.type = "button";
          ask.onclick = async () => {
            const note = prompt("Why should the admins look again?", "");
            if (note === null) return;
            if (!note.trim()) { showError("Say why."); return; }
            try { await withInFlight(api("POST", "/controller/me/notices/" + encodeURIComponent(n.id) + "/review", { note })); toast("Asked. An admin will look again."); loadAccount(); }
            catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "That was not sent."); }
          };
          row.appendChild(ask);
          card.appendChild(row);
        }
        nb.appendChild(card);
      }
    }
  }

  if (isAccount) {
    const data = section(root, "Your data", state.controllerOn
      ? "Everything kept about you: this account, and at the controller your servers, their backups and your requests, as one file."
      : "Everything this node keeps about this account, as one file.");
    const dl = el("button", "btn", "Download my data");
    dl.type = "button";
    dl.onclick = async () => {
      try {
        const out = { account: me };
        if (state.controllerOn) out.controller = await withInFlight(api("GET", "/controller/me/data"));
        const blob = new Blob([JSON.stringify(out, null, 2)], { type: "application/json" });
        const a = el("a");
        a.href = URL.createObjectURL(blob);
        a.download = "lanthorn-" + me.username + "-data.json";
        document.body.appendChild(a);
        a.click();
        a.remove();
        setTimeout(() => URL.revokeObjectURL(a.href), 1000);
      } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "Your data could not be read."); }
    };
    const r7 = el("div", "form-actions start"); r7.appendChild(dl); data.appendChild(r7);

    const del = section(root, "Delete my account", me.role === "user" && state.controllerOn
      ? "Removes your servers, their backups and everything the controller keeps about you, then this account. It cannot be undone."
      : "Removes this account. It cannot be undone.");
    const pass = field(del, "acct-del-pass", "Your password", { type: "password", autocomplete: "current-password" });
    const go = el("button", "btn btn-danger", "Delete my account");
    go.type = "button";
    go.onclick = async () => {
      if (!pass.value) { showError("Type your password to delete the account."); return; }
      if (!confirm("Delete your account and everything kept about it? This cannot be undone.")) return;
      go.disabled = true;
      try {
        await withInFlight(api("POST", "/auth/delete-me", { password: pass.value }));
        toast("Your account was deleted.");
        $("disconnect-btn").click();
      } catch (e) {
        go.disabled = false;
        if (!(e && e.__auth)) showError((e && e.error) || "The account was not deleted.");
      }
    };
    const r8 = el("div", "form-actions start"); r8.appendChild(go); del.appendChild(r8);
  }

  if (isAccount && me.role === "user") {
    const ad = section(root, "Admin access", "You run servers here. An admin also manages accounts, game files and the mesh. Ask, and an admin decides.");
    if (me.admin_requested_unix) {
      ad.appendChild(el("p", "muted", "You asked to become an admin of this node; an admin will decide."));
    } else {
      const ask = el("button", "btn", "Ask to become an admin");
      ask.type = "button";
      ask.onclick = async () => {
        try { await withInFlight(api("POST", "/auth/admin-request")); toast("Asked. An admin will decide."); loadAccount(); }
        catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "The request was not made."); }
      };
      const r6 = el("div", "form-actions"); r6.style.justifyContent = "flex-start"; r6.appendChild(ask); ad.appendChild(r6);
    }
  }
  for (const input of root.querySelectorAll("input")) input.classList.add("field");
}

// ---------- region requests ----------

function requestLine(r) {
  const when = r.created_unix ? new Date(r.created_unix * 1000).toLocaleString() : "";
  return (r.name || r.instance_id) + " \u00b7 " + (r.from_region || "no region") + " \u2192 " + r.to_region +
    (when ? " \u00b7 asked " + when : "");
}

function requestCard(r, admin) {
  const card = el("div", "req-card");
  const head = el("div", "req-head");
  head.appendChild(el("span", "req-line", requestLine(r)));
  head.appendChild(el("span", "chip req-" + r.state, r.state));
  card.appendChild(head);
  if (admin) card.appendChild(el("p", "muted small", "Owner " + String(r.owner || "").slice(0, 12) + "\u2026"));
  if (r.note) card.appendChild(el("p", "muted small", (r.state === "pending" ? "Last try: " : "Note: ") + r.note));
  if (r.state === "pending") {
    const row = el("div", "form-actions");
    row.style.justifyContent = "flex-start";
    if (admin) {
      const approve = el("button", "btn btn-primary", "Approve and move");
      approve.type = "button";
      approve.onclick = async () => {
        approve.disabled = true;
        approve.textContent = "Moving\u2026";
        try {
          await withInFlight(api("POST", "/controller/admin/region-requests/" + encodeURIComponent(r.id) + "/approve"));
          toast("Approved. The server moved to " + r.to_region + ".");
          poll();
        } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "The request was not approved."); }
        loadRequests();
      };
      const deny = el("button", "btn", "Deny");
      deny.type = "button";
      deny.onclick = async () => {
        const note = prompt("Why? The owner sees this (optional).", "");
        if (note === null) return;
        try {
          await withInFlight(api("POST", "/controller/admin/region-requests/" + encodeURIComponent(r.id) + "/deny", note ? { note } : {}));
          toast("Denied.");
        } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "The request was not denied."); }
        loadRequests();
      };
      row.append(approve, deny);
    } else {
      const cancel = el("button", "btn", "Take back");
      cancel.type = "button";
      cancel.onclick = async () => {
        try {
          await withInFlight(api("DELETE", "/controller/region-requests/" + encodeURIComponent(r.id)));
          toast("Request taken back.");
        } catch (e) { if (!(e && e.__auth)) showError((e && e.error) || "The request was not taken back."); }
        loadRequests();
      };
      row.appendChild(cancel);
    }
    card.appendChild(row);
  }
  return card;
}

async function loadRequests() {
  const adminBox = $("requests-admin");
  const mineBox = $("requests-mine");
  if (!adminBox || !mineBox) return;
  const [all, mine] = await Promise.all([
    // Deciding is on the Admin tab; this tab is the account's own requests.
    Promise.resolve(null),
    api("GET", "/controller/region-requests").catch(() => []),
  ]);
  adminBox.textContent = "";
  mineBox.textContent = "";
  if (Array.isArray(all)) {
    adminBox.appendChild(el("h3", null, "To decide"));
    const pending = all.filter(r => r.state === "pending");
    if (!pending.length) adminBox.appendChild(el("p", "muted small", "Nothing waiting."));
    for (const r of pending) adminBox.appendChild(requestCard(r, true));
    const decided = all.filter(r => r.state !== "pending").slice(0, 20);
    if (decided.length) {
      adminBox.appendChild(el("h3", null, "Decided"));
      for (const r of decided) adminBox.appendChild(requestCard(r, true));
    }
    const n = $("nav-requests-count");
    if (n) n.textContent = pending.length ? String(pending.length) : "";
  }
  mineBox.appendChild(el("h3", null, "Your requests"));
  const own = Array.isArray(mine) ? mine : [];
  if (!own.length) mineBox.appendChild(el("p", "muted small", "None yet. Ask from a server's menu: Move to another worker\u2026"));
  for (const r of own) mineBox.appendChild(requestCard(r, false));
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
  if (c.updatable && notice && notice.version === c.version) row.appendChild(buildUpdateLine(notice, downloading, c));
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
function buildUpdateLine(n, downloading, copy) {
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
  if (copy && !downloading) {
    line.appendChild(btn("Verify files", "Check these files against Valve's, for a server that crashes on a damaged file. " +
      "A copy is checked; servers keep running on this one.", () => onVerifyGame(copy)));
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
async function onVerifyGame(c) {
  const name = gameName(c.game_id);
  if (!confirm("Check " + name + " version " + c.version + " against Valve's files?\n\n" +
    "A copy is made and checked; servers keep running on this one. Needs about " + formatBytes(Math.round(c.bytes * 1.1)) +
    " free while it runs. If anything differs, the repaired copy is kept beside this one and you choose whether servers use it.")) return;
  try {
    await withInFlight(api("POST", "/content/" + encodeURIComponent(c.game_id), { verify: true }));
    clearError();
    toast("Checking " + name + "'s files.");
    watchUpdate(c.game_id);
    pollInstalled();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "The check did not start.");
  }
}

// What a check of the files found (Agent::verify_content), and the choice it leaves.
function showVerifyResult(gameId, v) {
  const name = gameName(gameId);
  if (!v.repaired_version) {
    toast(name + ": every file matches Valve's" + (v.to_build ? " (build " + v.to_build + ")" : "") + ". Nothing was kept.");
    return;
  }
  openModal("verify-dialog", gameId, (body, close) => {
    body.appendChild(el("h3", null, name + ": " + v.changed_count + " file" + (v.changed_count === 1 ? "" : "s") + " differed"));
    body.appendChild(el("p", "muted small", "These files in version " + v.version + " were not Valve's. The repaired copy is version " +
      v.repaired_version + ", beside it. Files you changed on purpose are in this list too and were put back, such as liblist.gam " +
      "for bots: if so, keep the current copy." + (v.from_build !== v.to_build && v.to_build ? " The check also brought it to build " + v.to_build + "." : "")));
    const list = el("ul", "verify-list");
    for (const f of v.changed) list.appendChild(el("li", null, f));
    if (v.changed_count > v.changed.length) list.appendChild(el("li", "muted", "and " + (v.changed_count - v.changed.length) + " more"));
    body.appendChild(list);
    const actions = el("div", "form-actions");
    actions.style.marginTop = "8px";
    const keep = el("button", "btn", "Keep the current copy");
    keep.type = "button";
    keep.title = "The repaired copy stays on the Installed page as unused; delete it there.";
    keep.onclick = close;
    const use = el("button", "btn btn-primary", "Use the repaired copy");
    use.type = "button";
    use.addEventListener("click", async () => {
      use.disabled = true;
      try {
        await withInFlight(api("POST", "/content/" + encodeURIComponent(gameId) + "/" + encodeURIComponent(v.repaired_version) + "/use"));
        clearError();
        close();
        toast("New " + name + " servers use version " + v.repaired_version + ". Existing ones move by the game's rule.");
        loadUpdates();
        pollInstalled();
      } catch (e) {
        if (e && e.__auth) { close(); return; }
        showError((e && e.error) || "That did not change.");
        use.disabled = false;
      }
    });
    actions.append(keep, use);
    body.appendChild(actions);
  });
}

async function watchUpdate(gameId) {
  for (;;) {
    await new Promise(r => setTimeout(r, 3000));
    let body;
    try { body = await api("GET", "/content/" + encodeURIComponent(gameId)); }
    catch (e) { if (e && e.__auth) return; continue; }
    const st = (body && body.status) || {};
    if (st.state === "running" || st.state === "queued") continue;
    if (st.state === "failed") {
      if (st.error !== "cancelled") showError(gameName(gameId) + ": that did not finish. " + st.error);
      return;
    }
    if (st.state === "done" && st.verify) {
      showVerifyResult(gameId, st.verify);
      pollInstalled();
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

const TABS = ["servers", "games", "installed", "interfaces", "requests", "admin", "account"];
const TITLES = { servers: "Servers", games: "Games", installed: "Installed", interfaces: "Mesh", requests: "Region requests", admin: "Admin", account: "Account" };

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
  if (name === "games") { renderGames(); loadOfficial(); }
  if (name === "servers") pollUsage();
  if (name === "installed") pollInstalled();
  if (name === "requests") loadRequests();
  if (name === "account") loadAccount();
  if (name === "admin") admEnter();
}

function renderAll() {
  renderStatusPill();
  renderInstances();
  renderGames();
}

// ---------- bootstrap ----------

document.addEventListener("DOMContentLoaded", () => {
  $("error-dismiss").addEventListener("click", clearError);

  $("login-form").addEventListener("submit", onLoginSubmit);
  $("setup-form").addEventListener("submit", onSetupSubmit);
  $("signup-form").addEventListener("submit", onSignupSubmit);
  $("report-form").addEventListener("submit", onReportSubmit);
  $("report-switch").addEventListener("click", (e) => {
    e.preventDefault();
    $("token-error").textContent = "";
    loginMode = loginMode === "report" ? (authState && authState.has_accounts ? "account" : "token") : "report";
    renderLoginMode();
  });
  $("signup-switch").addEventListener("click", (e) => {
    e.preventDefault();
    $("token-error").textContent = "";
    loginMode = loginMode === "signup" ? "account" : "signup";
    renderLoginMode();
  });
  $("login-switch").addEventListener("click", (e) => {
    e.preventDefault();
    $("token-error").textContent = "";
    loginMode = loginMode === "token" ? (authState && authState.has_accounts ? "account" : "setup") : "token";
    renderLoginMode();
  });

  $("token-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const v = $("token-input").value;
    $("token-error").textContent = "";
    if (!v) { $("token-error").textContent = "Enter the token."; return; }
    tryConnect(v.trim());
  });

  $("disconnect-btn").addEventListener("click", () => {
    // End the session on the node too, so it is not left live; the token has
    // nothing to end.
    const t = getToken();
    if (t && t.startsWith("s.")) fetch("/auth/logout", { method: "POST", headers: { "Authorization": "Bearer " + t } }).catch(() => {});
    state.signedInAs = null;
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
  $("requests-refresh")?.addEventListener("click", loadRequests);
  $("admin-refresh")?.addEventListener("click", loadAdmin);
  $("empty-goto-games").addEventListener("click", () => setActiveTab("games"));
  $("add-game-btn").addEventListener("click", openAddGameDialog);
  $("new-server-btn").addEventListener("click", () => setActiveTab("games"));
  $("announce-btn").addEventListener("click", () => onAnnounce(null));
  $("orphans-clean").addEventListener("click", onCleanOrphans);
  $("removed-backups-show").addEventListener("click", onRemovedBackups);
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
