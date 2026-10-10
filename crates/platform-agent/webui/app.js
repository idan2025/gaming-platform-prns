"use strict";

// Mesh Host agent UI. Vanilla ES2020. No frameworks, no imports.
// Talks to the same-origin API. Polls every 5s, pausing while a mutating request is in flight.

const TOKEN_KEY = "agent_token";
const POLL_MS = 5000;

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
function art(gameId, cls) {
  const box = el("div", "art " + (cls || ""));
  box.style.setProperty("--h", String(hueOf(gameId)));
  const g = gameOf(gameId);
  box.appendChild(el("span", "art-letters", initials(g ? g.display_name : gameId)));
  if (g && g.steam_app_id) {
    const img = el("img");
    img.alt = "";
    img.loading = "lazy";
    img.onload = () => img.classList.add("loaded");
    img.onerror = () => img.remove();
    img.src = "https://cdn.cloudflare.steamstatic.com/steam/apps/" + g.steam_app_id + "/header.jpg";
    box.appendChild(img);
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
    throw { error: msg };
  }
  return data;
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
}
function stopPolling() {
  if (state.pollTimer) { clearTimeout(state.pollTimer); state.pollTimer = null; }
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
    const [cap, insts] = await Promise.all([api("GET", "/capacity"), api("GET", "/instances")]);
    state.capacity = cap;
    state.instances = Array.isArray(insts) ? insts : [];
    renderStatusPill();
    renderInstances();
  } catch (e) {
    if (e && e.__auth) return;
    if (e && e.__network) showError(e.error);
  } finally {
    if (state.token) maybeSchedulePoll();
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
    el("div", "cell cell-mesh"), el("div", "cell cell-uptime"), actions);
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
    pill.textContent = inst.state;
    pill.className = "state-pill state-" + String(inst.state || "unknown");

    // null is "the game could not be asked", not "no map" — and not zero
    // players either. A blank would make an unreachable server look like one
    // on an empty map.
    const mapCell = row.querySelector(".cell-map");
    if (inst.map_now) {
      mapCell.textContent = inst.map_now;
      mapCell.title = "Read from the game just now";
      mapCell.classList.remove("muted-em");
    } else {
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
      playersCell.textContent = String(inst.players_now);
      playersCell.removeAttribute("title");
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
  items.push({ label: "Copy mesh address", icon: "copy", disabled: !inst.mesh_destination, action: () => copyValue(inst.mesh_destination) });
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
  actions.append(startBtn, installBtn);
  body.appendChild(actions);
  card.appendChild(body);
  return card;
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
        bots: bots ? bots.value : "", fixedPort: port.value,
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

  const instance_id = randomInstanceId(gameId);
  if (!/^[a-z0-9._-]{1,64}$/.test(instance_id)) { showError("Could not make a valid id for this server."); return; }

  const body = {
    instance_id, game_id: gameId, name, max_players: f.maxPlayers, port,
    extra_ports: {}, map: map === "" ? null : map, bots, owner: null,
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
      if (st.state === "running") {
        state.installingMsg.set(gameId, "Downloading game files… " + formatUptime(st.since_secs) + " so far.");
        renderGames();
        continue;
      }
      if (st.state === "done") {
        state.installingMsg.set(gameId, st.already_installed ? "Already installed." : "Installed.");
        state.installDone.set(gameId, "ok");
        return true;
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

// ---------- tabs ----------

const TABS = ["servers", "games", "interfaces"];
const TITLES = { servers: "Servers", games: "Games", interfaces: "Mesh" };

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
      for (const id of ["start-dialog", "map-dialog", "bots-dialog"]) closeModal(id);
    }
  });

  const t = getToken();
  if (t) tryConnect(t);
  else showTokenScreen();
});
