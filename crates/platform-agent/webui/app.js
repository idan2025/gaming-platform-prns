"use strict";

// Mesh Host agent UI. Vanilla ES2020. No frameworks, no imports.
// Talks to the same-origin API. Polls every 5s, pausing while a mutating request is in flight.

const TOKEN_KEY = "agent_token";
const POLL_MS = 5000;

const state = {
  token: null,
  capacity: null,        // {max_instances, running, port_range_start, port_range_end}
  games: [],             // array of game defs
  instances: [],         // array of instance objects
  interfaces: [],        // array of live mesh interface objects
  rows: new Map(),       // instance_id -> <tr>
  inFlight: 0,           // mutating requests in flight; polling pauses while > 0
  pollTimer: null,
  activeTab: "servers",
  // Track open per-card start forms so re-render doesn't blow away typed values.
  openForms: new Map(),  // game_id -> {name, maxPlayers, advanced, fixedPort}
  installing: new Map(), // game_id -> true (install in flight)
  installingMsg: new Map(), // game_id -> string message (post-completion, dismissible)
  installDone: new Map(), // game_id -> "ok" | "error" marker
};

// ---------- small DOM helpers ----------

function $(id) { return document.getElementById(id); }
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
function esc(s) {
  // Use a thrown-away <div>'s textContent setter, which escapes for us.
  const d = document.createElement("div");
  d.textContent = s === null || s === undefined ? "" : String(s);
  return d.innerHTML;
}

// ---------- token ----------

function getToken() {
  return localStorage.getItem(TOKEN_KEY);
}
function setToken(t) {
  if (t === null) localStorage.removeItem(TOKEN_KEY);
  else localStorage.setItem(TOKEN_KEY, t);
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
    // Network error — surface as a generic sentence but mark it clearly.
    throw { __network: true, error: "Network error: could not reach the agent. " + (e && e.message ? e.message : "") };
  }
  if (resp.status === 401) {
    setToken(null);
    showTokenScreen();
    throw { __auth: true, error: "Authentication failed. The token is wrong or expired." };
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

// Make an element copy a full value on click. Hashes are shown truncated
// because a 32-character destination does not fit a table cell, but truncated
// is useless if you cannot get the whole thing out — so the element carries the
// full value and hands it over on click.
function makeCopyable(el, value, label) {
  if (!el || !value) return;
  el.classList.add("copyable");
  el.title = value + " — click to copy";
  el.setAttribute("role", "button");
  el.setAttribute("tabindex", "0");
  const copy = async () => {
    let ok = false;
    try {
      // Only available on a secure origin, and this UI is plain HTTP on a LAN,
      // so the fallback is the one that usually runs.
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
    const previous = el.textContent;
    el.textContent = ok ? "copied" : value;
    if (!ok) el.classList.add("copy-failed");
    setTimeout(() => { el.textContent = previous; el.classList.remove("copy-failed"); }, 1200);
  };
  el.onclick = copy;
  el.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); copy(); } };
}

// ---------- error banner ----------

function showError(sentence) {
  const banner = $("error-banner");
  $("error-text").textContent = sentence;
  banner.classList.remove("hidden");
}
function clearError() { $("error-banner").classList.add("hidden"); }

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
  // Immediate first fetch.
  poll();
}
function stopPolling() {
  if (state.pollTimer) { clearTimeout(state.pollTimer); state.pollTimer = null; }
}
function maybeSchedulePoll() {
  if (state.pollTimer) return;
  if (state.inFlight > 0) return; // will be rescheduled when inFlight drains.
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
    if (e && e.__auth) return; // already handled
    if (e && e.__network) showError(e.error);
    // Don't spam banner for transient poll errors of other kinds.
  } finally {
    if (state.token) maybeSchedulePoll();
  }
}

// ---------- initial connect ----------

async function tryConnect(token) {
  setToken(token);
  try {
    const health = await api("GET", "/health");
    // We could store max_instances from health too; /capacity is the source for the pill.
    const games = await api("GET", "/games");
    state.games = Array.isArray(games) ? games : [];
    const cap = await api("GET", "/capacity");
    state.capacity = cap;
    const insts = await api("GET", "/instances");
    state.instances = Array.isArray(insts) ? insts : [];
    clearError();
    showMainUI();
  } catch (e) {
    if (e && e.__auth) {
      $("token-error").textContent = "Authentication failed. Check the token.";
    } else {
      $("token-error").textContent = (e && e.error) || "Connection failed.";
    }
    setToken(null);
  }
}

// ---------- rendering: status pill ----------

function renderStatusPill() {
  const pill = $("status-pill");
  const cap = state.capacity || {};
  const running = cap.running != null ? cap.running : "—";
  const max = cap.max_instances != null ? cap.max_instances : "—";
  pill.textContent = running + " of " + max + " running";
}

// ---------- rendering: instances table (in-place by id) ----------

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

function stateClass(s) {
  return "state-" + String(s || "unknown");
}

function renderInstances() {
  const tbody = $("instances-tbody");
  const seen = new Set();

  for (const inst of state.instances) {
    seen.add(inst.instance_id);
    let row = state.rows.get(inst.instance_id);
    if (!row) {
      row = $("instance-row-template").content.firstElementChild.cloneNode(true);
      // Wire row action buttons once.
      const stopBtn = row.querySelector(".stop-btn");
      const restartBtn = row.querySelector(".restart-btn");
      const removeBtn = row.querySelector(".remove-btn");
      stopBtn.addEventListener("click", () => onStop(inst.instance_id));
      if (restartBtn) restartBtn.addEventListener("click", () => onRestart(inst.instance_id));
      removeBtn.addEventListener("click", () => onRemove(inst.instance_id, inst.name));
      tbody.appendChild(row);
      state.rows.set(inst.instance_id, row);
    }
    // Update cells in place; never replace the row node.
    row.querySelector(".cell-name").textContent = inst.name || inst.instance_id;
    row.querySelector(".cell-game").textContent = inst.game_id;
    const statePill = row.querySelector(".cell-state .state-pill");
    statePill.textContent = inst.state;
    statePill.className = "state-pill " + stateClass(inst.state);

    row.querySelector(".cell-port").textContent = (inst.port != null ? String(inst.port) : "—");

    // The mesh destination is the address a player actually joins from a
    // launcher. Its absence is not a detail: it means this server exists on
    // this machine's network and nowhere else, so say that rather than showing
    // a blank cell.
    const meshCell = row.querySelector(".cell-mesh");
    if (meshCell) {
      if (inst.mesh_destination) {
        meshCell.textContent = inst.mesh_destination.slice(0, 8) + "…";
        meshCell.classList.remove("muted-em");
        makeCopyable(meshCell, inst.mesh_destination, "destination");
      } else {
        meshCell.textContent = "LAN only";
        meshCell.title = "Not announced on the mesh. Add a [mesh] section to this node's config.";
        meshCell.classList.add("muted-em");
        meshCell.classList.remove("copyable");
        meshCell.onclick = null;
      }
    }

    // Players: null is NOT zero. null means "the agent couldn't ask this game" and
    // must display as "—" with a tooltip explaining the distinction; 0 is a real
    // count of zero players and must display as the literal "0".
    const playersCell = row.querySelector(".cell-players");
    if (inst.players_now === null || inst.players_now === undefined) {
      playersCell.textContent = "—";
      playersCell.setAttribute("title", "could not ask this game");
      playersCell.classList.add("muted-em");
    } else {
      playersCell.textContent = String(inst.players_now);
      playersCell.removeAttribute("title");
      playersCell.classList.remove("muted-em");
    }

    row.querySelector(".cell-uptime").textContent = formatUptime(inst.uptime_secs);

    // Stop button only meaningful for running/creating/unknown states.
    const stopBtn = row.querySelector(".stop-btn");
    const removeBtn = row.querySelector(".remove-btn");
    stopBtn.disabled = (inst.state === "stopped" || inst.state === "missing");
  }

  // Remove rows whose instances have disappeared.
  for (const [id, row] of state.rows) {
    if (!seen.has(id)) {
      row.remove();
      state.rows.delete(id);
    }
  }

  const empty = $("instances-empty");
  if (state.instances.length === 0) empty.classList.remove("hidden");
  else empty.classList.add("hidden");
}

// ---------- rendering: games grid ----------

function renderGames() {
  const grid = $("games-grid");
  // We rebuild cards only when the game set changes (by id + runnable). This keeps
  // any open start form's typed values intact because the form state is held in
  // state.openForms, and we re-apply it after rebuild.
  // A rebuild replaces the DOM nodes an open form lives in, and putting focus
  // back afterwards is not the same as never having taken it: the caret jumps,
  // an IME composition is dropped, and a half-typed name can be lost. So while
  // any form is open the grid is never rebuilt — only its dynamic bits are
  // refreshed. The card set is stable in practice (games change when a pack is
  // imported, not on a timer), so this costs nothing real.
  if (state.openForms.size > 0) {
    for (const g of state.games) refreshCardDynamic(g);
    return;
  }
  const sig = state.games.map(g => g.id + "|" + (g.runnable ? "1" : "0") + "|" + (g.reason || "")).join(";");
  if (grid.dataset.sig === sig) {
    // Just refresh dynamic bits (install state) on existing cards.
    for (const g of state.games) refreshCardDynamic(g);
    return;
  }
  grid.dataset.sig = sig;
  grid.textContent = "";
  for (const g of state.games) grid.appendChild(buildGameCard(g));
}

function buildGameCard(g) {
  const card = el("article", "game-card" + (g.runnable ? "" : " unrunnable"));
  card.dataset.gameId = g.id;

  const head = el("div", "card-head");
  const title = el("h3", "card-title", g.display_name);
  const idLine = el("div", "card-id", g.id);
  head.append(title, idLine);
  card.append(head);

  const meta = el("dl", "card-meta");
  const transport = el("div"); transport.append(el("dt", null, "Transport"), el("dd", null, g.transport || "—"));
  const dport = el("div"); dport.append(el("dt", null, "Default port"), el("dd", null, String(g.default_port)));
  const extra = el("div"); extra.append(el("dt", null, "Extra ports"), el("dd", null, String(g.extra_ports || 0)));
  meta.append(transport, dport, extra);
  card.append(meta);

  if (!g.runnable) {
    const reason = el("p", "card-reason", "Not startable: " + (g.reason || "unavailable on this host."));
    card.append(reason);
  }

  // Start a server button.
  const startBtn = el("button", "primary start-btn", "Start a server");
  startBtn.type = "button";
  if (!g.runnable) { startBtn.disabled = true; startBtn.title = g.reason || "not runnable"; }
  startBtn.addEventListener("click", () => onOpenStartForm(g.id));
  card.append(startBtn);

  // Install button + status area.
  const installWrap = el("div", "install-wrap");
  const installBtn = el("button", "quiet install-btn", "Install game files");
  installBtn.type = "button";
  installBtn.addEventListener("click", () => onInstall(g.id));
  installWrap.append(installBtn);
  const installStatus = el("p", "install-status hidden");
  installWrap.append(installStatus);
  card.append(installWrap);

  // Start form placeholder (filled on demand).
  const formHolder = el("div", "form-holder");
  card.append(formHolder);

  refreshCardDynamic(g);
  return card;
}

function refreshCardDynamic(g) {
  const grid = $("games-grid");
  const card = grid.querySelector('.game-card[data-game-id="' + CSS.escape(g.id) + '"]');
  if (!card) return;
  const installBtn = card.querySelector(".install-btn");
  const installStatus = card.querySelector(".install-status");
  if (state.installing.get(g.id)) {
    installBtn.disabled = true;
    installBtn.textContent = "Installing…";
    installStatus.classList.remove("hidden");
    installStatus.classList.remove("ok", "err");
    installStatus.textContent = "Installing… this can take several minutes.";
  } else if (state.installingMsg.has(g.id)) {
    installBtn.disabled = false;
    installBtn.textContent = "Install game files";
    installStatus.classList.remove("hidden");
    const msg = state.installingMsg.get(g.id);
    if (state.installDone.get(g.id) === "error") {
      installStatus.classList.add("err");
      installStatus.textContent = msg;
    } else {
      installStatus.classList.add("ok");
      installStatus.textContent = msg;
    }
  } else {
    installBtn.disabled = false;
    installBtn.textContent = "Install game files";
    installStatus.classList.add("hidden");
    installStatus.textContent = "";
  }
}

// ---------- start form ----------

function onOpenStartForm(gameId) {
  if (!state.openForms.has(gameId)) {
    state.openForms.set(gameId, { name: "", maxPlayers: 16, advanced: false, fixedPort: "" });
  }
  const opening = state.openForms.get(gameId);
  if (opening) opening._opened = true;
  renderStartForm(gameId);
}

function renderStartForm(gameId) {
  const card = $("games-grid").querySelector('.game-card[data-game-id="' + CSS.escape(gameId) + '"]');
  if (!card) return;
  const holder = card.querySelector(".form-holder");
  const f = state.openForms.get(gameId);
  if (!f) { holder.textContent = ""; return; }

  // Build form (preserving values from state.openForms, not from the DOM).
  holder.textContent = "";
  const form = el("form", "start-form");
  form.autocomplete = "off";

  const nameLabel = el("label", null, "Name");
  nameLabel.htmlFor = "sf-name-" + gameId;
  const nameInput = el("input"); nameInput.type = "text"; nameInput.id = "sf-name-" + gameId;
  nameInput.required = true; nameInput.value = f.name;
  nameInput.placeholder = "My Server";
  nameInput.addEventListener("input", () => { f.name = nameInput.value; });

  const mpLabel = el("label", null, "Max players");
  mpLabel.htmlFor = "sf-mp-" + gameId;
  const mpInput = el("input"); mpInput.type = "number"; mpInput.id = "sf-mp-" + gameId;
  mpInput.min = "1"; mpInput.max = "64"; mpInput.step = "1"; mpInput.value = String(f.maxPlayers);
  mpInput.required = true;
  mpInput.addEventListener("input", () => { const v = parseInt(mpInput.value, 10); f.maxPlayers = isNaN(v) ? 16 : v; });

  const advLabel = el("label", "checkbox-row");
  const advInput = el("input"); advInput.type = "checkbox"; advInput.id = "sf-adv-" + gameId;
  advInput.checked = f.advanced;
  advInput.addEventListener("change", () => {
    f.advanced = advInput.checked;
    advancedWrap.classList.toggle("hidden", !f.advanced);
  });
  advLabel.append(advInput, document.createTextNode("Advanced"));
  const advancedWrap = el("div", "advanced-wrap" + (f.advanced ? "" : " hidden"));

  const portLabel = el("label", null, "Fixed host port");
  portLabel.htmlFor = "sf-port-" + gameId;
  const portInput = el("input"); portInput.type = "number"; portInput.id = "sf-port-" + gameId;
  portInput.min = "1024"; portInput.max = "65535"; portInput.value = f.fixedPort || "";
  portInput.placeholder = "leave blank to let the host choose";
  portInput.addEventListener("input", () => { f.fixedPort = portInput.value; });
  const portHint = el("p", "muted small", "If left blank the host assigns a free port from its configured range.");
  advancedWrap.append(portLabel, portInput, portHint);

  const actions = el("div", "form-actions");
  const startBtn = el("button", "primary", "Start");
  startBtn.type = "submit";
  const cancelBtn = el("button", "quiet", "Cancel");
  cancelBtn.type = "button";
  cancelBtn.addEventListener("click", () => closeStartForm(gameId));
  actions.append(startBtn, cancelBtn);

  form.append(nameLabel, nameInput, mpLabel, mpInput, advLabel, advancedWrap, actions);
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    onStartSubmit(gameId);
  });

  holder.append(form);
  // Focus **only** when a person just opened this form, never on a re-render.
  // Restoring focus on every render is what made the page feel like it was
  // grabbing the cursor: a render the user did not cause would pull the caret
  // back into a field they had deliberately left. `_opened` is set once, by the
  // click that created the form, and cleared here.
  if (f._opened) {
    f._opened = false;
    nameInput.focus();
  }
}

function closeStartForm(gameId) {
  state.openForms.delete(gameId);
  const card = $("games-grid").querySelector('.game-card[data-game-id="' + CSS.escape(gameId) + '"]');
  if (card) card.querySelector(".form-holder").textContent = "";
}

function randomInstanceId(gameId) {
  const chars = "abcdefghijklmnopqrstuvwxyz0123456789";
  let s = "";
  for (let i = 0; i < 6; i++) s += chars[Math.floor(Math.random() * chars.length)];
  return gameId + "-" + s;
}

async function onStartSubmit(gameId) {
  const f = state.openForms.get(gameId);
  if (!f) return;
  const name = (f.name || "").trim();
  if (!name) { showError("Server name is required."); return; }
  const maxPlayers = f.maxPlayers;
  if (!(maxPlayers >= 1 && maxPlayers <= 64)) { showError("Max players must be a number between 1 and 64."); return; }
  let port = null;
  if (f.advanced) {
    const pv = (f.fixedPort || "").trim();
    if (pv !== "") {
      const n = parseInt(pv, 10);
      if (isNaN(n) || n < 1024 || n > 65535) { showError("Fixed host port must be between 1024 and 65535, or blank."); return; }
      port = n;
    }
  }
  const game = state.games.find(g => g.id === gameId);
  if (!game) { showError("Game not found."); return; }
  if (!game.runnable) { showError("This game is not runnable: " + (game.reason || "")); return; }

  const instance_id = randomInstanceId(gameId);
  // Validate against the documented pattern just in case gameId has odd chars.
  if (!/^[a-z0-9._-]{1,64}$/.test(instance_id)) { showError("Generated instance id is invalid."); return; }

  const body = {
    instance_id,
    game_id: gameId,
    name,
    max_players: maxPlayers,
    port,
    extra_ports: {},
    owner: null,
  };

  const card = $("games-grid").querySelector('.game-card[data-game-id="' + CSS.escape(gameId) + '"]');
  const submitBtn = card ? card.querySelector(".start-form button.primary") : null;
  if (submitBtn) { submitBtn.disabled = true; submitBtn.textContent = "Starting…"; }

  try {
    const result = await withInFlight(api("POST", "/instances", body));
    // 202: this game's files are not here yet, so the agent started the
    // download instead of refusing. The operator asked to play, not to learn
    // the difference between a pack and an install — so wait for it and then
    // start the server, without making them press anything else.
    if (result && result.installing) {
      closeStartForm(gameId);
      clearError();
      state.installingMsg.set(gameId, "Downloading game files… this can take a while.");
      renderGames();
      const ok = await watchInstall(gameId);
      if (ok) {
        // Same body, now that the files are there.
        await withInFlight(api("POST", "/instances", body));
        setActiveTab("servers");
        poll();
      }
      return;
    }
    closeStartForm(gameId);
    clearError();
    // Switch to Servers tab so the operator sees their new instance.
    setActiveTab("servers");
    // Immediate refresh.
    poll();
  } catch (e) {
    showError((e && e.error) || "Failed to start server.");
  } finally {
    if (submitBtn) { submitBtn.disabled = false; submitBtn.textContent = "Start"; }
  }
}

// Poll one game's install until it finishes. Resolves true when the files are
// there, false when it failed — the failure sentence is put in front of the
// operator either way, because it names the thing they have to fix.
//
// Deliberately not wrapped in `withInFlight`: an install runs for tens of
// minutes and pausing the instance list for all of it would freeze the rest of
// the UI.
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
        state.installingMsg.set(gameId, st.already_installed ? "Already installed." : "Install completed.");
        state.installDone.set(gameId, "ok");
        return true;
      }
      if (st.state === "failed") {
        state.installingMsg.set(gameId, st.error || "Install failed.");
        state.installDone.set(gameId, "error");
        showError(st.error || "Install failed.");
        return false;
      }
      // "idle" means the agent forgot, or never started. Not an error to loop on.
      return false;
    }
  } finally {
    state.installing.delete(gameId);
    renderGames();
  }
}

// ---------- install ----------

async function onInstall(gameId) {
  if (state.installing.get(gameId)) return;
  state.installing.set(gameId, true);
  state.installingMsg.delete(gameId);
  state.installDone.delete(gameId);
  renderGames();
  try {
    // Returns as soon as the download has been *started*: it takes tens of
    // minutes and no browser will hold a request open that long.
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

// ---------- stop / remove ----------

// Turn a server off and on again. It keeps its ports and its mesh destination,
// so a player's bookmark still works afterwards — which is why this is a
// restart and not a remove-and-recreate.
async function onRestart(instanceId) {
  const row = state.rows.get(instanceId);
  const btn = row ? row.querySelector(".restart-btn") : null;
  if (btn) { btn.disabled = true; btn.textContent = "…"; }
  try {
    await withInFlight(api("POST", "/instances/" + encodeURIComponent(instanceId) + "/restart"));
    clearError();
    poll();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Failed to restart the server.");
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = "Restart"; }
  }
}

async function onStop(instanceId) {
  const row = state.rows.get(instanceId);
  const btn = row && row.querySelector(".stop-btn");
  if (btn) { btn.disabled = true; const old = btn.textContent; btn.textContent = "Stopping…"; btn.dataset.oldText = old; }
  try {
    await withInFlight(api("POST", "/instances/" + encodeURIComponent(instanceId) + "/stop"));
    clearError();
    poll();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Failed to stop server.");
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = btn.dataset.oldText || "Stop"; }
  }
}

async function onRemove(instanceId, name) {
  const confirmed = confirm(
    'Remove the server "' + (name || instanceId) + '"?\n\n' +
    "This destroys the container. The instance's files stay on disk and are not deleted.\n\n" +
    "This cannot be undone."
  );
  if (!confirmed) return;
  const row = state.rows.get(instanceId);
  const btn = row && row.querySelector(".remove-btn");
  if (btn) { btn.disabled = true; const old = btn.textContent; btn.textContent = "Removing…"; btn.dataset.oldText = old; }
  try {
    await withInFlight(api("DELETE", "/instances/" + encodeURIComponent(instanceId)));
    clearError();
    poll();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Failed to remove server.");
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = btn.dataset.oldText || "Remove"; }
  }
}

// ---------- mesh interfaces ----------

// Format a byte count compactly for the RX/TX columns.
function formatBytes(n) {
  if (n === null || n === undefined) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = Number(n), i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return (i === 0 ? v : v.toFixed(1)) + " " + units[i];
}

// Fetch the interface status and render it. Handles the three states: no uplink
// (501), uplink up with interfaces, and uplink up but empty. A poll error is
// surfaced in the banner rather than silently leaving a stale list.
async function loadInterfaces() {
  // Games on the mesh, which is a different question from uplink interfaces.
  renderMeshGames();
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
    // 501 with no manager: the agent has no uplink. Show the offline note and
    // hide the controls rather than an error banner — it is a configuration
    // fact, not a failure.
    if (e && e.error && /uplink/i.test(e.error)) {
      offline.classList.remove("hidden");
      online.classList.add("hidden");
      return;
    }
    showError((e && e.error) || "Could not read mesh interfaces.");
  }
}

function renderInterfaces() {
  const tbody = $("interfaces-tbody");
  const empty = $("interfaces-empty");
  tbody.textContent = "";
  const list = state.interfaces || [];
  if (list.length === 0) { empty.classList.remove("hidden"); return; }
  empty.classList.add("hidden");
  for (const iface of list) {
    const tr = el("tr");
    tr.append(
      el("td", null, iface.name || "—"),
      el("td", null, iface.mode || "—"),
      el("td", null, iface.connection || "—"),
      el("td", null, formatBytes(iface.rx_bytes)),
      el("td", null, formatBytes(iface.tx_bytes)),
      el("td", null, String(iface.links != null ? iface.links : 0)),
    );
    const actions = el("td", "actions-col cell-actions");
    const renameBtn = el("button", "quiet", "Rename");
    renameBtn.type = "button";
    renameBtn.addEventListener("click", () => onRenameInterface(iface.id, iface.name));
    const removeBtn = el("button", "quiet danger", "Remove");
    removeBtn.type = "button";
    removeBtn.addEventListener("click", () => onRemoveInterface(iface.id, iface.name));
    actions.append(renameBtn, removeBtn);
    tr.append(actions);
    // The hex id is long and not useful in a column; keep it as a tooltip on the
    // row so an operator can still see exactly what they are acting on.
    tr.title = "interface id: " + iface.id;
    tbody.append(tr);
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
    if (!addr) { showError("A TCP interface needs a host:port address."); return; }
    body = { kind: "tcp", addr, ifac_name, ifac_passphrase };
  } else {
    body = { kind: "auto", ifac_name, ifac_passphrase };
  }
  const submitBtn = $("iface-form").querySelector("button.primary");
  if (submitBtn) { submitBtn.disabled = true; submitBtn.textContent = "Adding…"; }
  try {
    await withInFlight(api("POST", "/interfaces", body));
    clearError();
    // Reset the form's inputs but keep the kind selection.
    $("iface-addr").value = "";
    $("iface-ifac-name").value = "";
    $("iface-ifac-pass").value = "";
    $("iface-ifac-toggle").checked = false;
    $("iface-ifac-wrap").classList.add("hidden");
    loadInterfaces();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Failed to add interface.");
  } finally {
    if (submitBtn) { submitBtn.disabled = false; submitBtn.textContent = "Add interface"; }
  }
}

async function onRemoveInterface(id, name) {
  const confirmed = confirm(
    'Remove the interface "' + (name || id) + '"?\n\n' +
    "This detaches it from the mesh and forgets it, so it will not come back on restart."
  );
  if (!confirmed) return;
  try {
    await withInFlight(api("DELETE", "/interfaces/" + encodeURIComponent(id)));
    clearError();
    loadInterfaces();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Failed to remove interface.");
  }
}

async function onRenameInterface(id, current) {
  const name = prompt("New name for this interface:", current || "");
  if (name === null) return; // cancelled
  const trimmed = name.trim();
  if (!trimmed) { showError("An interface name cannot be empty."); return; }
  try {
    await withInFlight(api("POST", "/interfaces/" + encodeURIComponent(id) + "/rename", { name: trimmed }));
    clearError();
    loadInterfaces();
  } catch (e) {
    if (e && e.__auth) return;
    showError((e && e.error) || "Failed to rename interface.");
  }
}

// ---------- tabs ----------

const TABS = ["servers", "games", "interfaces"];

function setActiveTab(name) {
  if (!TABS.includes(name)) name = "servers";
  state.activeTab = name;
  for (const t of TABS) {
    const btn = $("tab-" + t);
    const panel = $("tab-panel-" + t);
    const on = t === name;
    if (btn) { btn.classList.toggle("active", on); btn.setAttribute("aria-selected", String(on)); }
    if (panel) panel.classList.toggle("hidden", !on);
  }
  // The interface list is only meaningful while the tab is open, and adding one
  // is rare, so it is fetched on entry rather than polled on the 5s timer with
  // the instance list. A fresh read every time the operator opens the tab is
  // both cheaper and never stale when it matters.
  if (name === "interfaces") loadInterfaces();
}

// ---------- top-level render ----------

function renderAll() {
  renderStatusPill();
  renderInstances();
  renderGames();
}

// ---------- bootstrap ----------

document.addEventListener("DOMContentLoaded", () => {
  // Error banner dismiss.
  $("error-dismiss").addEventListener("click", clearError);

  // Token form.
  $("token-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const v = $("token-input").value;
    $("token-error").textContent = "";
    if (!v) { $("token-error").textContent = "Enter a token."; return; }
    tryConnect(v.trim());
  });

  // Disconnect.
  $("disconnect-btn").addEventListener("click", () => {
    setToken(null);
    state.capacity = null;
    state.instances = [];
    state.games = [];
    state.interfaces = [];
    state.rows.forEach(r => r.remove());
    state.rows.clear();
    state.openForms.clear();
    state.installing.clear();
    state.installingMsg.clear();
    state.installDone.clear();
    showError = showError; // no-op; keep ref
    clearError();
    showTokenScreen();
  });

  // Tabs.
  $("tab-servers").addEventListener("click", () => setActiveTab("servers"));
  $("tab-games").addEventListener("click", () => setActiveTab("games"));
  $("tab-interfaces").addEventListener("click", () => setActiveTab("interfaces"));
  $("empty-goto-games").addEventListener("click", () => setActiveTab("games"));

  // Mesh interface form: the address field only applies to TCP, and the IFAC
  // fields only when the operator opts in — mirror that in what is shown.
  $("iface-kind").addEventListener("change", () => {
    const tcp = $("iface-kind").value === "tcp";
    $("iface-addr-wrap").classList.toggle("hidden", !tcp);
  });
  $("iface-ifac-toggle").addEventListener("change", () => {
    $("iface-ifac-wrap").classList.toggle("hidden", !$("iface-ifac-toggle").checked);
  });
  $("iface-form").addEventListener("submit", (e) => {
    e.preventDefault();
    onAddInterface();
  });

  // Track focus within start forms so we can restore it after re-render.
  document.addEventListener("focusin", (e) => {
    const form = e.target.closest && e.target.closest(".start-form");
    if (!form) return;
    const card = form.closest(".game-card");
    if (!card) return;
    const gameId = card.dataset.gameId;
    const f = state.openForms.get(gameId);
    if (!f) return;
    f._focusedField = e.target.id || "";
    if (e.target.setSelectionRange) {
      try { f._selStart = e.target.selectionStart; f._selEnd = e.target.selectionEnd; } catch (_) {}
    }
  });

  // Auto-load games list once when we have a token but the grid is empty.
  // (Refresh on connect is handled in tryConnect; here we hook renderAll
  // to refresh the games list if it has never been loaded.)
  const origRenderAll = renderAll;
  // No-op alias kept for clarity; renderGames compares signature to avoid rebuilds.

  const t = getToken();
  if (t) tryConnect(t);
  else showTokenScreen();
});

// Refresh the games grid whenever we render (cheap due to signature check).
const _origRenderAll2 = renderAll;
renderAll = function() { _origRenderAll2(); renderGames(); };


// The `[mesh]` half of this node's Reticulum story: which running servers are
// announced, and under what destination. Kept separate from the uplink panel
// because they are separate jobs and reporting them as one is what made the tab
// confusing.
async function renderMeshGames() {
  const note = $("mesh-games-note");
  const list = $("mesh-games-list");
  if (!note || !list) return;
  let body;
  try {
    body = await api("GET", "/mesh");
  } catch (e) {
    if (e && e.__auth) return;
    note.textContent = (e && e.error) || "Could not read mesh status.";
    return;
  }
  note.textContent = body.note || "";
  list.textContent = "";
  const servers = Array.isArray(body.servers) ? body.servers : [];
  if (!servers.length) {
    const p = el("p", "muted", body.enabled
      ? "No servers are running, so nothing is announced yet."
      : "Nothing is announced: this node runs its games LAN-only.");
    list.appendChild(p);
    return;
  }
  for (const s of servers) {
    const row = el("div", "mesh-row");
    row.appendChild(el("span", "pack-name", s.name || s.instance_id));
    row.appendChild(el("span", "muted", s.game_id));
    const dest = el("code", "", s.destination ? s.destination.slice(0, 12) + "…" : "starting…");
    if (s.destination) makeCopyable(dest, s.destination, "destination");
    row.appendChild(dest);
    list.appendChild(row);
  }
}
