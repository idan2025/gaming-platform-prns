const { invoke } = window.__TAURI__.core;

const state = {
  browse: null,
  games: [],
  servers: [],
  legacyHiddenCount: 0,
  tcpPeer: '',
  autoDiscover: true,
  rowEls: new Map(),
  filters: {
    text: '', game_id: null, has_players: false, not_full: false,
    exclude_passworded: false, dedicated_only: false, include_legacy: true,
    max_hops: null,
  },
  sort: { sort: 'hops', descending: false },
  activeHash: null,
  detail: null,
  errorTimer: null,
  toastTimer: null,
  pollTimer: null,
  startingBrowse: false,
  interfaces: [],
  savedOpts: {},
  knownCount: 0,
  view: 'servers',
  // Whether the detail pane's "Advanced" block is open, carried across the
  // pane's re-renders so it does not snap shut every poll.
  advancedOpen: false,
  // Destination hash -> game id the player picked for a server whose announce
  // names no game. A legacy v0.1.10 announce carries a name and nothing else
  // (`PLAN.md` §3.3), so the launcher cannot know its game and must not guess
  // one: picking a wire protocol for the player is how a join silently talks
  // nonsense at a server. Remembered per destination so the choice survives
  // closing the detail pane.
  chosenGame: new Map(),
  // game id -> local port a join would bind. Read from the core rather than
  // assumed, because the core is where the pack default and the player's saved
  // choice are reconciled.
  listenPorts: new Map(),
  // The server this launcher's last join bound, so its row says so.
  joinedHash: null,
  // Rows removed this session, kept out of the list at once rather than
  // waiting for the next poll to agree.
  removed: new Set(),
  // Mode 3 LAN rooms (PLAN.md §14). `roomsAvailable` is false for an older
  // shell without the room commands, which then shows no room UI at all
  // rather than buttons that throw.
  room: null,
  lanHelper: null,
  roomsAvailable: false,
  roomBusy: false,
  // The last room check (`check_room`), for the room it was run in.
  roomCheck: { busy: false, result: null, error: null, hash: null },
  // The room pane's Fix and Unblock, while one runs and what it said.
  firewallAction: { busy: false, message: null, error: false },
  // The pack-gaps pane's Allow and Undo, while one runs and what it said.
  portsAction: { busy: false, message: null, error: false },
  // Which member set the room was last checked for by itself.
  autoCheckSig: null,
  autoCheckTimer: null,
  hostDraft: { game_id: null, name: '' },
  roomPanelSig: null,
};

// The headless render check shortens it before this script loads.
const AUTO_CHECK_DELAY_MS = window.GPP_AUTO_CHECK_DELAY_MS ?? 3000;

const LINK_CLASS = { 1: 'Low-rate', 2: 'TCP / bursty', 3: 'High-bitrate' };

// Inline icons, so the UI has no asset to go missing and works offline.
const ICON = {
  play: '<path d="M7 5l12 7-12 7z"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v6M12 7.5h.01"/>',
  route: '<circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="6" r="2.5"/><path d="M8.5 18H15a3 3 0 000-6H9a3 3 0 010-6h6.5"/>',
  copy: '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 00-2-2H6a2 2 0 00-2 2v8a2 2 0 002 2h2"/>',
  trash: '<path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/>',
  refresh: '<path d="M20 11a8 8 0 10-2.3 5.7M20 4v7h-7"/>',
  megaphone: '<path d="M4 10v4h3l5 4V6L7 10H4z"/><path d="M16 9a4 4 0 010 6"/>',
  filter: '<path d="M4 6h16M7 12h10M10 18h4"/>',
  search: '<circle cx="11" cy="11" r="6.5"/><path d="M20 20l-4.2-4.2"/>',
  door: '<path d="M14 4h5v16h-5M10 8l-4 4 4 4M6 12h10"/>',
  plug: '<path d="M9 3v5M15 3v5M7 8h10v4a5 5 0 01-10 0zM12 17v4"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  server: '<rect x="3" y="4" width="18" height="6" rx="1.5"/><rect x="3" y="14" width="18" height="6" rx="1.5"/><path d="M7 7h.01M7 17h.01"/>',
  lan: '<rect x="2" y="5" width="20" height="13" rx="2"/><path d="M8 21h8M12 18v3"/>',
  wifiOff: '<path d="M3 3l18 18M8.5 16.5a5 5 0 017 0M5 12.5a10 10 0 015-2.6M14 10a10 10 0 015 2.5M12 20h.01"/>',
  check: '<path d="M5 12.5l4.5 4.5L19 7"/>',
};
function icon(name, cls) {
  const s = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  s.setAttribute('aria-hidden', 'true');
  if (cls) s.setAttribute('class', cls);
  s.innerHTML = ICON[name] || '';
  return s;
}

// ---------- helpers ----------
function $(id) { return document.getElementById(id); }
function el(tag, cls, txt) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (txt != null) e.textContent = txt;
  return e;
}
function errText(err) { return String(err && err.message || err); }
function fmtSeen(secs) {
  if (secs == null) return '—';
  if (secs < 2) return 'just now';
  if (secs < 60) return secs + 's ago';
  const m = Math.floor(secs / 60);
  if (m < 60) return m + 'm ago';
  const h = Math.floor(m / 60);
  return h + 'h ' + (m % 60) + 'm ago';
}
function fmtDuration(secs) {
  if (secs == null) return '—';
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (h > 0) return h + 'h ' + m + 'm';
  if (m > 0) return m + 'm ' + (secs % 60) + 's';
  return secs + 's';
}
function hopsText(h) { return h + (h === 1 ? ' hop' : ' hops'); }
// The game a join would use: what the announce said, else what the player
// chose for this destination. `null` means neither, and nothing may join.
function effectiveGameId(d) {
  if (!d) return null;
  if (d.announce && d.announce.game_id) return d.announce.game_id;
  return state.chosenGame.get(d.hash) || null;
}
function gameName(id) {
  const g = gameById(id);
  return g ? (g.display_name || g.id) : id;
}
function showError(msg) {
  const box = $('error');
  box.textContent = '';
  box.appendChild(el('span', '', String(msg)));
  const btn = el('button', '', '×');
  btn.setAttribute('aria-label', 'Dismiss');
  btn.onclick = () => box.classList.add('hidden');
  box.appendChild(btn);
  box.classList.remove('hidden');
  clearTimeout(state.errorTimer);
  state.errorTimer = setTimeout(() => box.classList.add('hidden'), 7000);
}
function hideError() { $('error').classList.add('hidden'); }
// An outcome that is not an error: a path found, an announce sent.
function toast(msg) {
  const box = $('toast');
  box.textContent = '';
  box.appendChild(el('span', '', msg));
  const btn = el('button', '', '×');
  btn.setAttribute('aria-label', 'Dismiss');
  btn.onclick = () => box.classList.add('hidden');
  box.appendChild(btn);
  box.classList.remove('hidden');
  clearTimeout(state.toastTimer);
  state.toastTimer = setTimeout(() => box.classList.add('hidden'), 5000);
}
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast('Copied to the clipboard.');
  } catch (_) {
    showError('Could not reach the clipboard. Select the text and copy it instead.');
  }
}

// ---------- game artwork ----------
//
// The pack's Steam header where it names a Steam app, over a lettered tile in
// a colour derived from the game id. The tile is what shows offline, which is a
// supported way to run this launcher, so it has to look deliberate on its own.
function hueOf(id) {
  let h = 0;
  for (const c of String(id || '?')) h = (h * 31 + c.charCodeAt(0)) % 360;
  return h;
}
function initials(name) {
  const words = String(name || '?').replace(/[^A-Za-z0-9 ]/g, ' ').split(/\s+/).filter(Boolean);
  if (!words.length) return '?';
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return (words[0][0] + words[1][0]).toUpperCase();
}
function art(gameId, cls, opts = {}) {
  const box = el('div', 'art ' + (cls || ''));
  box.style.setProperty('--h', String(hueOf(gameId || opts.fallbackName)));
  const g = gameId ? gameById(gameId) : null;
  box.appendChild(el('span', 'art-letters', initials(g ? g.display_name : (opts.fallbackName || gameId || '?'))));
  // The pack's picture: a Steam header, or a box cover from Wikimedia. A
  // cover is portrait, so it sits whole over a blurred copy of itself rather
  // than being cropped to a sliver. An older core without `art_url` still
  // gets the Steam header.
  const url = g && (g.art_url || (g.steam_app_id
    ? 'https://cdn.cloudflare.steamstatic.com/steam/apps/' + g.steam_app_id + '/header.jpg' : null));
  if (url) {
    const bg = el('img', 'art-bg');
    const fg = el('img', 'art-fg');
    for (const img of [bg, fg]) {
      img.alt = '';
      img.decoding = 'async';
      img.loading = 'lazy';
      img.onload = () => img.classList.add('loaded');
      img.onerror = () => { bg.remove(); fg.remove(); };
      img.src = url;
      box.appendChild(img);
    }
  }
  if (opts.room) box.classList.add('art-room');
  return box;
}

// ---------- query building ----------
function hasMetadataFilter() {
  const f = state.filters;
  return !!(f.game_id || f.has_players || f.not_full || f.exclude_passworded || f.dedicated_only);
}
function buildQuery() {
  const f = state.filters;
  return {
    game_id: f.game_id,
    text: f.text.trim() || null,
    max_hops: f.max_hops,
    max_link_class: null,
    has_players: f.has_players,
    not_full: f.not_full,
    exclude_passworded: f.exclude_passworded,
    exclude_allowlisted: false,
    transport_modes: null,
    dedicated_only: f.dedicated_only,
    include_legacy: f.include_legacy,
    sort: state.sort.sort,
    descending: state.sort.descending,
    max_age_secs: null,
  };
}
function buildLegacyProbeQuery() {
  const q = buildQuery();
  q.game_id = null;
  q.has_players = false;
  q.not_full = false;
  q.exclude_passworded = false;
  q.dedicated_only = false;
  q.include_legacy = true;
  return q;
}

// ---------- sorting ----------
function sortServers(rows) {
  const { sort, descending } = state.sort;
  const dir = descending ? -1 : 1;
  return [...rows].sort((a, b) => {
    let av, bv;
    switch (sort) {
      case 'name':
        av = (a.name || '').toLowerCase(); bv = (b.name || '').toLowerCase();
        if (av < bv) return -1 * dir; if (av > bv) return 1 * dir; return 0;
      case 'players':
        av = a.players == null ? -1 : a.players; bv = b.players == null ? -1 : b.players;
        return (av - bv) * dir;
      case 'hops':
        return (a.hops - b.hops) * dir;
      case 'last_seen':
        return (a.last_seen_secs - b.last_seen_secs) * dir;
      default: return 0;
    }
  });
}

// ---------- rendering: node status ----------
function renderStatus() {
  const s = $('status');
  s.textContent = '';
  const b = state.browse;
  if (!b) { s.appendChild(el('div', 'node-sub', 'Starting…')); return; }

  const running = b.running;
  const ifaces = b.interfaces || [];
  const up = ifaces.filter(i => i.connected).length;
  const line = el('div', 'node-line');
  line.appendChild(el('span', 'dot ' + (running ? (up || !ifaces.length ? 'on' : 'warn') : '')));
  line.appendChild(el('span', '', running ? 'Online' : 'Offline'));
  s.appendChild(line);
  const sub = running
    ? (ifaces.length ? up + ' of ' + ifaces.length + (ifaces.length === 1 ? ' connection' : ' connections') + ' up' : 'Listening on the mesh')
    : 'Not listening for servers';
  s.appendChild(el('div', 'node-sub', sub));

  const btn = el('button', 'btn' + (running ? '' : ' btn-primary'));
  btn.type = 'button';
  btn.textContent = state.startingBrowse ? 'Connecting…' : (running ? 'Disconnect' : 'Connect');
  btn.disabled = state.startingBrowse;
  btn.onclick = running ? stopBrowse : startBrowse;
  s.appendChild(btn);

  renderStatusBar();
  const count = $('nav-servers-count');
  if (count) count.textContent = running && state.servers.length ? String(state.servers.length) : '';
}

function renderStatusBar() {
  const bar = $('statusbar-text');
  if (!bar) return;
  bar.textContent = '';
  const b = state.browse;
  if (!b || !b.running) {
    bar.appendChild(el('span', 'sb-item', 'Not connected to the mesh'));
    return;
  }
  const shown = state.servers.length;
  bar.appendChild(el('span', 'sb-item', shown + (shown === 1 ? ' server' : ' servers') + ' online'));
  (b.interfaces || []).forEach(i => {
    const item = el('span', 'sb-item' + (i.connected ? '' : ' sb-bad'));
    item.appendChild(el('span', 'dot ' + (i.connected ? 'on' : 'warn')));
    item.appendChild(el('span', '', i.label + (i.connected ? '' : ' (down)')));
    bar.appendChild(item);
  });
}

// ---------- rendering: list ----------
function createRowEl(row) {
  const e = el('div', 'row');
  e.dataset.hash = row.destination_hash;
  e.setAttribute('role', 'option');
  e.setAttribute('aria-selected', 'false');
  e.tabIndex = -1;
  e.innerHTML =
    '<div class="cell col-art" data-cell="art"></div>' +
    '<div class="cell col-name"><div class="name-line"><div class="v" data-cell="name"></div></div><div class="sub" data-cell="iface"></div></div>' +
    '<div class="cell col-game"><div class="v" data-cell="game"></div></div>' +
    '<div class="cell col-map"><div class="v" data-cell="map"></div></div>' +
    '<div class="cell col-players"><div class="players"><div class="v" data-cell="players"></div><div class="bar" data-cell="bar"><i></i></div></div></div>' +
    '<div class="cell col-hops"><div class="v" data-cell="hops"></div></div>' +
    '<div class="cell col-seen"><div class="v" data-cell="seen"></div></div>';
  updateRowEl(e, row);
  e.addEventListener('click', () => { setActive(row.destination_hash); openDetail(row.destination_hash); });
  e.addEventListener('dblclick', () => { openDetail(row.destination_hash); primaryAction(row.destination_hash); });
  e.addEventListener('focus', () => { state.activeHash = row.destination_hash; });
  e.addEventListener('contextmenu', ev => {
    ev.preventDefault();
    ev.stopPropagation();
    setActive(row.destination_hash);
    openRowMenu(row.destination_hash, ev.clientX, ev.clientY);
  });
  return e;
}

function updateRowEl(e, row) {
  const legacy = row.legacy;
  e.classList.toggle('legacy', legacy);

  // Artwork only changes with the game, so it is not rebuilt every poll (an
  // <img> rebuilt every two seconds flickers and refetches).
  const artCell = e.querySelector('[data-cell="art"]');
  const artKey = (row.game_id || '') + '|' + isRoom(row) + '|' + (row.name || '');
  if (artCell.dataset.key !== artKey) {
    artCell.dataset.key = artKey;
    artCell.textContent = '';
    artCell.appendChild(art(legacy ? null : row.game_id, 'art-thumb', { room: isRoom(row), fallbackName: row.name }));
  }

  const nameV = e.querySelector('[data-cell="name"]');
  nameV.textContent = row.name || 'Unnamed server';
  const line = nameV.parentElement;
  line.querySelectorAll('.badge').forEach(b => b.remove());
  const inUse = row.destination_hash === state.joinedHash
    || (state.room && state.room.active && state.room.room_hash === row.destination_hash);
  if (inUse) line.appendChild(el('span', 'badge joined-badge', isRoom(row) ? 'In room' : 'Joined'));
  if (row.from_index) {
    // Second-hand evidence. The numbers are real, but somebody else heard
    // them, and `hops` is the distance from the index rather than from here.
    const b = el('span', 'badge index-badge', 'via index');
    b.title = 'Reported by an index, not heard directly. Distance is measured from the index.';
    line.appendChild(b);
  }
  if (legacy) {
    const b = el('span', 'badge legacy-badge', 'legacy');
    b.title = 'An older server that announces only its name.';
    line.appendChild(b);
  } else {
    if (row.passworded === true) line.appendChild(el('span', 'badge pw', 'Password'));
    // A LAN room is not a server: its destination speaks the room protocol,
    // and joining it puts this machine on a virtual LAN (PLAN.md §14).
    if (isRoom(row)) {
      const b = el('span', 'badge room-badge', 'LAN room');
      b.title = 'A LAN room: joining it makes the players in it look like one local network.';
      line.appendChild(b);
    }
  }

  const ifaceCell = e.querySelector('[data-cell="iface"]');
  const subParts = [];
  if (!legacy && row.dedicated === true) subParts.push('Dedicated server');
  else if (isRoom(row)) subParts.push('Virtual LAN');
  else if (legacy) subParts.push('Older server');
  ifaceCell.textContent = subParts.join(' · ');

  e.querySelector('[data-cell="game"]').textContent = legacy ? 'Unknown' : (row.game_id ? gameName(row.game_id) : 'Unknown');
  // A room has no map; "Unknown" would suggest it has one nobody reported.
  e.querySelector('[data-cell="map"]').textContent =
    isRoom(row) ? '—' : (legacy ? 'Unknown' : (row.map || 'Unknown'));

  const playersCell = e.querySelector('[data-cell="players"]');
  const bar = e.querySelector('[data-cell="bar"]');
  if (legacy || row.players == null) {
    playersCell.textContent = '—';
    playersCell.title = 'Player count unknown — not present in the announce';
    bar.hidden = true;
  } else {
    const max = row.max_players == null ? '?' : row.max_players;
    playersCell.textContent = row.players + '/' + max;
    playersCell.title = 'Player count from the announce, heard ' + fmtSeen(row.last_seen_secs) + '. Not live.';
    bar.hidden = !row.max_players;
    if (row.max_players) {
      const pct = Math.min(100, Math.round(100 * row.players / row.max_players));
      bar.firstElementChild.style.width = pct + '%';
      bar.classList.toggle('full', pct >= 100);
    }
  }

  const hopsCell = e.querySelector('[data-cell="hops"]');
  hopsCell.textContent = hopsText(row.hops);
  hopsCell.title = row.hops + (row.hops === 1 ? ' hop' : ' hops') + ' across the mesh';

  e.querySelector('[data-cell="seen"]').textContent = fmtSeen(row.last_seen_secs);

  const selected = row.destination_hash === state.detail?.hash;
  e.setAttribute('aria-selected', String(selected));
  e.classList.toggle('selected', selected);
}

function renderList() {
  const list = $('list');
  const b = state.browse;

  if (!b || !b.running) {
    // Built once, not on every poll: rebuilding it every two seconds took the
    // focused button with it.
    if (!list.querySelector('.empty-stopped')) {
      list.textContent = '';
      state.rowEls.clear();
      renderEmptyNotRunning();
    }
    refreshStartButton();
    return;
  }
  if (state.servers.length === 0) {
    list.textContent = '';
    state.rowEls.clear();
    if (b.heard_total === 0) renderEmptyNothingHeard();
    else renderEmptyFiltered();
    return;
  }
  const empty = list.querySelector('.empty');
  if (empty) empty.remove();

  const sorted = sortServers(state.servers);
  const seen = new Set();
  for (const row of sorted) {
    seen.add(row.destination_hash);
    let e = state.rowEls.get(row.destination_hash);
    if (!e) {
      e = createRowEl(row);
      state.rowEls.set(row.destination_hash, e);
    } else {
      updateRowEl(e, row);
    }
  }
  for (const [hash, e] of state.rowEls) {
    if (!seen.has(hash)) { e.remove(); state.rowEls.delete(hash); }
  }
  for (const row of sorted) {
    list.appendChild(state.rowEls.get(row.destination_hash));
  }

  let anyActive = false;
  for (const [hash, e] of state.rowEls) {
    const isActive = hash === state.activeHash;
    e.tabIndex = isActive ? 0 : -1;
    if (isActive) anyActive = true;
  }
  if (!anyActive && state.rowEls.size > 0) {
    const first = list.firstElementChild;
    if (first) { state.activeHash = first.dataset.hash; first.tabIndex = 0; }
  }
}

function refreshStartButton() {
  const btn = $('start-browse-btn');
  if (!btn) return;
  btn.disabled = state.startingBrowse;
  btn.textContent = state.startingBrowse ? 'Connecting…' : 'Connect to the mesh';
}

function emptyBox(cls, iconName, title, text) {
  const wrap = el('div', 'empty ' + (cls || ''));
  const ic = el('div', 'empty-icon');
  ic.appendChild(icon(iconName));
  wrap.appendChild(ic);
  wrap.appendChild(el('h2', '', title));
  if (text) wrap.appendChild(el('p', '', text));
  return wrap;
}

function renderEmptyNotRunning() {
  const wrap = emptyBox('empty-stopped', 'wifiOff', 'Not connected to the mesh',
    'Connect to start hearing game servers. Discovery is passive: this launcher listens for '
    + 'the servers that announce themselves, and announces nothing of its own.');
  const saved = state.savedOpts || {};
  const via = saved.tcp || (state.tcpPeer || '').trim();
  wrap.appendChild(el('p', 'small muted', via
    ? 'Connects through ' + via + (saved.auto || state.autoDiscover ? ' and your local network.' : '.')
    : 'Connects over your local network. Add a relay under Connections to reach further.'));
  const row = el('div', 'btn-row');
  const btn = el('button', 'btn btn-primary btn-lg', 'Connect to the mesh');
  btn.id = 'start-browse-btn';
  btn.type = 'button';
  btn.onclick = startBrowse;
  const settings = el('button', 'btn btn-lg', 'Connections…');
  settings.type = 'button';
  settings.onclick = () => setView('connections');
  row.append(btn, settings);
  wrap.appendChild(row);
  $('list').appendChild(wrap);
  refreshStartButton();
}
function renderEmptyNothingHeard() {
  const wrap = emptyBox('', 'search', 'Listening for servers',
    'Nothing has announced yet. Servers appear here the moment they are heard, and drop off '
    + 'when they go quiet.');
  const row = el('div', 'btn-row');
  const look = el('button', 'btn', 'Look for saved servers');
  look.type = 'button';
  look.onclick = refreshKnown;
  row.appendChild(look);
  wrap.appendChild(row);
  $('list').appendChild(wrap);
}
function renderEmptyFiltered() {
  const n = state.browse.heard_total;
  const wrap = emptyBox('', 'filter', 'No servers match your filters',
    n + ' server' + (n === 1 ? ' is' : 's are') + ' online, but none pass every filter. Older '
    + 'servers that announce only a name are left out by filters on game, players or flags.');
  const btn = el('button', 'btn', 'Reset filters');
  btn.type = 'button';
  btn.onclick = clearAllFilters;
  const row = el('div', 'btn-row');
  row.appendChild(btn);
  wrap.appendChild(row);
  $('list').appendChild(wrap);
}

// ---------- legacy hidden notice ----------
function renderLegacyNotice() {
  const box = $('legacy-notice');
  const show = state.filters.include_legacy && hasMetadataFilter() && state.legacyHiddenCount > 0;
  if (!show) { box.classList.add('hidden'); return; }
  box.textContent = '';
  box.appendChild(el('span', '',
    state.legacyHiddenCount + ' older server' + (state.legacyHiddenCount === 1 ? ' is' : 's are') +
    ' hidden: they predate the details these filters need.'));
  const btn = el('button', 'btn', 'Show them');
  btn.type = 'button';
  btn.onclick = clearMetadataFilters;
  box.appendChild(btn);
  box.classList.remove('hidden');
}

// ---------- context menus ----------
//
// The webview's own menu is a browser's — Back, Reload, Inspect — and Reload
// tears the page out from under a running launcher. So every right-click gets
// this menu instead, and the native one survives only inside text fields,
// where cut and paste are what a person wants.
function closeMenu() {
  const m = $('ctx-menu');
  if (m && !m.hidden) { m.hidden = true; m.textContent = ''; }
}
function showMenu(x, y, items) {
  const m = $('ctx-menu');
  m.textContent = '';
  for (const it of items) {
    if (it === 'sep') { m.appendChild(el('div', 'menu-sep')); continue; }
    if (it.head) { m.appendChild(el('div', 'menu-head', it.head)); continue; }
    const b = el('button', 'menu-item' + (it.danger ? ' danger' : ''));
    b.type = 'button';
    b.setAttribute('role', 'menuitem');
    if (it.icon) b.appendChild(icon(it.icon));
    b.appendChild(el('span', 'mi-label', it.label));
    if (it.hint) b.appendChild(el('span', 'mi-hint', it.hint));
    b.disabled = !!it.disabled;
    if (it.id) b.id = it.id;
    b.onclick = () => { closeMenu(); it.action && it.action(); };
    m.appendChild(b);
  }
  m.hidden = false;
  // Keep it on screen.
  const w = m.offsetWidth || 240, h = m.offsetHeight || 200;
  const vw = window.innerWidth || 1100, vh = window.innerHeight || 720;
  m.style.left = Math.max(4, Math.min(x, vw - w - 4)) + 'px';
  m.style.top = Math.max(4, Math.min(y, vh - h - 4)) + 'px';
  const first = m.querySelector('.menu-item:not(:disabled)');
  if (first) first.focus({ preventScroll: true });
}

function rowByHash(hash) { return state.servers.find(s => s.destination_hash === hash) || null; }

function openRowMenu(hash, x, y) {
  const row = rowByHash(hash);
  if (!row) return;
  const room = isRoom(row);
  const inRoomHere = state.room && state.room.active && state.room.room_hash === hash;
  const items = [{ head: row.name || 'Unnamed server' }];
  if (room) {
    items.push(inRoomHere
      ? { label: 'Leave room', icon: 'door', action: leaveRoom }
      : { label: 'Join room', icon: 'play', action: () => { openDetail(hash); joinRoom(); },
          disabled: !(state.lanHelper && state.lanHelper.ready) });
  } else {
    items.push({ label: 'Join server', icon: 'play', action: () => { openDetail(hash); joinServer(); },
                 disabled: !row.game_id && !state.chosenGame.get(hash) });
  }
  items.push({ label: 'View details', icon: 'info', action: () => openDetail(hash) });
  items.push('sep');
  items.push({ label: 'Trace path', icon: 'route', hint: 'ask the mesh', action: () => tracePath(hash) });
  items.push({ label: 'Copy address', icon: 'copy', action: () => copyText(hash) });
  items.push('sep');
  items.push({ label: 'Remove from list', icon: 'trash', danger: true, id: 'ctx-remove',
               action: () => forgetServer(hash) });
  showMenu(x, y, items);
}

function openListMenu(x, y) {
  const running = state.browse && state.browse.running;
  const items = [
    { label: 'Refresh list', icon: 'refresh', disabled: !running, action: () => pollServers() },
    { label: 'Announce', icon: 'megaphone', disabled: !running, action: announceAll },
    { label: 'Look for saved servers', icon: 'search', disabled: !running, action: refreshKnown },
    'sep',
    { label: 'Reset filters', icon: 'filter', action: clearAllFilters },
  ];
  showMenu(x, y, items);
}

function wireMenus() {
  document.addEventListener('contextmenu', e => {
    const t = e.target;
    if (t.closest && t.closest('input, textarea, [contenteditable="true"]')) return;
    e.preventDefault();
    if (t.closest && t.closest('#list')) { openListMenu(e.clientX, e.clientY); return; }
    const sel = String(window.getSelection ? window.getSelection() : '').trim();
    if (sel) {
      showMenu(e.clientX, e.clientY, [{ label: 'Copy', icon: 'copy', action: () => copyText(sel) }]);
      return;
    }
    closeMenu();
  });
  document.addEventListener('mousedown', e => {
    if (!e.target.closest || !e.target.closest('#ctx-menu')) closeMenu();
    if (!e.target.closest || !e.target.closest('.pop-anchor')) closePopovers();
  });
  window.addEventListener('blur', () => { closeMenu(); });
  window.addEventListener('resize', closeMenu);
  document.addEventListener('scroll', closeMenu, true);
  document.addEventListener('keydown', e => {
    // A reload throws away the page while the core keeps running under it.
    const k = (e.key || '').toLowerCase();
    if (k === 'f5' || ((e.ctrlKey || e.metaKey) && k === 'r')) { e.preventDefault(); return; }
    if (k === 'escape') { closeMenu(); closePopovers(); }
    const m = $('ctx-menu');
    if (!m.hidden && (k === 'arrowdown' || k === 'arrowup')) {
      e.preventDefault();
      const items = [...m.querySelectorAll('.menu-item:not(:disabled)')];
      const i = items.indexOf(document.activeElement);
      const next = k === 'arrowdown' ? (i + 1) % items.length : (i - 1 + items.length) % items.length;
      items[next]?.focus();
    }
  });
}

// ---------- popovers (filters, announce) ----------
function closePopovers() {
  for (const [pop, btn] of [['filters-pop', 'filters-btn'], ['announce-pop', 'announce-more']]) {
    const p = $(pop);
    if (p && !p.hidden) { p.hidden = true; $(btn)?.setAttribute('aria-expanded', 'false'); }
  }
}
function togglePopover(popId, btnId) {
  const p = $(popId);
  const open = p.hidden;
  closePopovers();
  if (open) {
    p.hidden = false;
    $(btnId).setAttribute('aria-expanded', 'true');
    if (popId === 'announce-pop') refreshAnnounceMenu();
  }
}
function refreshAnnounceMenu() {
  const hosting = !!(state.room && state.room.active && state.room.role === 'host');
  const running = !!(state.browse && state.browse.running);
  $('announce-room').disabled = !hosting;
  $('announce-room-hint').textContent = hosting ? '' : 'not hosting';
  $('f-refresh').disabled = !running;
  const sel = state.detail && state.detail.hash;
  $('announce-trace').disabled = !running || !sel;
  $('announce-trace-hint').textContent = sel ? '' : 'select one first';
}
function updateFilterBadge() {
  const f = state.filters;
  const n = [f.has_players, f.not_full, f.exclude_passworded, f.dedicated_only, !f.include_legacy, f.max_hops != null]
    .filter(Boolean).length;
  const b = $('filters-count');
  if (!b) return;
  b.hidden = n === 0;
  b.textContent = String(n);
}

// ---------- views ----------
const VIEWS = ['servers', 'rooms', 'connections', 'indexes'];
function setView(name) {
  if (!VIEWS.includes(name)) name = 'servers';
  state.view = name;
  for (const v of VIEWS) {
    const sec = $('view-' + v);
    const nav = $('nav-' + v);
    if (sec) sec.hidden = v !== name;
    if (nav) { nav.classList.toggle('active', v === name); nav.setAttribute('aria-selected', String(v === name)); }
  }
  // The toolbar's search and filters act on the server list only.
  $('filters').classList.toggle('toolbar-dim', name !== 'servers');
  if (name === 'rooms') { loadLanHelper().then(() => renderRoomPanel(true)); }
  if (name === 'connections') renderConnections();
}

// ---------- detail pane ----------
function openDetail(hash) {
  const row = rowByHash(hash);
  if (!row) return;
  if (state.detail && state.detail.hash === hash) { renderDetail(); return; }
  state.detail = {
    hash,
    announce: { ...row },
    loading: true,
    data: null,
    error: null,
    joining: false,
    joined: hash === state.joinedHash,
    joinMsg: null,
  };
  renderList();
  // A room answers no detail probe — it has no such endpoint — so asking
  // would only ever render "No direct response" under a room that is fine.
  if (isRoom(row)) {
    state.detail.loading = false;
    renderDetail();
    $('detail').hidden = false;
    return;
  }
  renderDetail();
  $('detail').hidden = false;
  invoke('server_details', { destinationHash: hash })
    .then(d => {
      if (state.detail && state.detail.hash === hash) {
        state.detail.data = d;
        state.detail.loading = false;
        renderDetail();
      }
    })
    .catch(err => {
      if (state.detail && state.detail.hash === hash) {
        state.detail.error = errText(err);
        state.detail.loading = false;
        renderDetail();
      }
    });
}

function closeDetail() {
  state.detail = null;
  $('detail').hidden = true;
  $('view-servers').classList.remove('has-detail');
  renderList();
  const active = $('list').querySelector('.row[tabindex="0"]');
  if (active) active.focus();
}

function stat(parent, k, v, unknown) {
  const box = el('div', 'stat');
  box.appendChild(el('div', 'stat-k', k));
  box.appendChild(el('div', 'stat-v' + (unknown ? ' unknown' : ''), v));
  parent.appendChild(box);
}

function renderDetail() {
  const d = state.detail;
  const pane = $('detail');
  $('view-servers').classList.toggle('has-detail', !!d);
  if (!d) { pane.hidden = true; return; }

  // This pane re-renders on every poll ("heard 3s ago" changes every second).
  // `.detail-body` is the element that scrolls and it is replaced on each pass,
  // so its offset and the focused control are carried across by hand. Every
  // control this pane builds that a person can land on needs a stable id.
  const prevBody = pane.querySelector('.detail-body');
  const prevScroll = prevBody ? prevBody.scrollTop : 0;
  const active = document.activeElement;
  const keepId = active && active.id && pane.contains(active) ? active.id : null;
  const selStart = keepId && active.selectionStart != null ? active.selectionStart : null;
  const selEnd = keepId && active.selectionEnd != null ? active.selectionEnd : null;

  pane.textContent = '';
  const a = d.announce;
  const legacy = a.legacy;
  const room = isRoom(a);
  const gameId = effectiveGameId(d);

  // hero
  const hero = el('div', 'detail-hero');
  hero.appendChild(art(legacy ? null : gameId, '', { room, fallbackName: a.name }));
  const heroText = el('div', 'hero-text');
  heroText.appendChild(el('h2', '', a.name || 'Unnamed server'));
  heroText.appendChild(el('div', 'hero-game',
    (gameId ? gameName(gameId) : 'Unknown game') + (room ? ' · LAN room' : (a.map && !legacy ? ' · ' + a.map : ''))));
  hero.appendChild(heroText);
  const close = el('button', 'detail-close');
  close.type = 'button';
  close.setAttribute('aria-label', 'Close details');
  close.appendChild(icon('x'));
  close.onclick = closeDetail;
  hero.appendChild(close);
  pane.appendChild(hero);

  // actions
  const actions = el('div', 'detail-actions');
  if (room) renderRoomFoot(actions, d, gameId);
  else renderJoinActions(actions, d, gameId);
  pane.appendChild(actions);

  // body
  const body = el('div', 'detail-body');

  const overview = el('div', 'section');
  const stats = el('div', 'stats');
  const who = room ? 'Members' : 'Players';
  if (legacy || a.players == null) stat(stats, who, 'Unknown', true);
  else stat(stats, who, a.players + '/' + (a.max_players == null ? '?' : a.max_players));
  stat(stats, 'Distance', hopsText(a.hops));
  if (!room && d.data && d.data.rtt_ms != null) stat(stats, 'Round trip', d.data.rtt_ms + ' ms');
  else stat(stats, 'Heard', fmtSeen(a.last_seen_secs));
  overview.appendChild(stats);
  if (legacy) {
    overview.appendChild(el('p', 'note',
      'An older server that announces only its name, so its game, map and player count are '
      + 'genuinely unknown — not zero.'));
  } else if (a.passworded === true) {
    overview.appendChild(el('p', 'note', 'This server asks for a password when you connect.'));
  }
  body.appendChild(overview);

  if (room) body.appendChild(renderRoomSection(d));

  // live probe
  if (!room) {
    const probeSec = el('div', 'section');
    probeSec.appendChild(el('h3', '', 'Right now'));
    if (d.loading) {
      const p = el('div', 'probe-pending');
      p.appendChild(el('span', 'spinner'));
      p.appendChild(el('span', '', 'Asking the server directly…'));
      probeSec.appendChild(p);
    } else if (d.error) {
      const pe = el('div', 'probe-error');
      pe.appendChild(el('div', 'title', 'No direct response'));
      pe.appendChild(el('div', '', 'The server did not answer a direct question. That does not mean it is '
        + 'offline: mesh routes can be one-way, so its announce can reach you while a reply cannot. '
        + 'It may still be joinable.'));
      probeSec.appendChild(pe);
    } else if (d.data) {
      const live = d.data.stats_source === 'live';
      const kv2 = el('div', 'kv');
      kvRow(kv2, 'Reachable', d.data.reachable ? 'Yes' : 'No', null);
      if (d.data.rtt_ms != null) kvRow(kv2, 'Round trip', d.data.rtt_ms + ' ms', null);
      if (d.data.players_online != null) {
        const max = d.data.max_players == null ? '?' : d.data.max_players;
        kvRow(kv2, live ? 'Players now' : 'Players (configured)', d.data.players_online + '/' + max, null);
      } else {
        kvRow(kv2, 'Players', null, 'Unknown', true);
      }
      kvRow(kv2, 'Map', d.data.map || null, 'Unknown');
      // The honesty field. "announced" means the server handed back the same
      // static configuration the list row already showed — it could not query
      // the running game — so this must never read as a live number.
      if (d.data.stats_source) {
        kvRow(kv2, 'Figures',
          live
            ? ('live from the game, ' + fmtDuration(d.data.stats_age_secs || 0) + ' old')
            : 'from the server’s configuration, as announced — not the running game',
          null);
      }
      if (d.data.uptime_secs != null) kvRow(kv2, 'Up for', fmtDuration(d.data.uptime_secs), null);
      if (d.data.error) kvRow(kv2, 'Note', d.data.error, null);
      probeSec.appendChild(kv2);
      if (d.data.player_names && d.data.player_names.length) {
        const ul = el('ul', 'player-list');
        d.data.player_names.forEach(n => ul.appendChild(el('li', '', n)));
        probeSec.appendChild(ul);
        if (d.data.roster_truncated) probeSec.appendChild(el('p', 'note', 'The list was too long for one answer and was cut short.'));
      } else if (live && d.data.player_names && d.data.player_names.length === 0) {
        const ul = el('ul', 'player-list');
        ul.appendChild(el('li', 'legacy-row', 'Nobody is playing'));
        probeSec.appendChild(ul);
      } else if (!live) {
        // No roster is not an empty roster: this server cannot be queried.
        probeSec.appendChild(el('p', 'note', 'This game answers no query, so who is playing is unknown.'));
      }
    }
    body.appendChild(probeSec);
  }

  if (!a.game_id) body.appendChild(renderGamePicker(d));
  // A room binds no local port: the game talks to the virtual adapter.
  if (gameId && !room) body.appendChild(renderPortSection(d, gameId));

  // Everything a player does not need to join, folded away but one click off.
  const adv = el('details', 'section advanced');
  adv.id = 'detail-advanced';
  adv.open = state.advancedOpen;
  adv.addEventListener('toggle', () => { state.advancedOpen = adv.open; });
  adv.appendChild(el('summary', '', 'Advanced'));
  const kv = el('div', 'kv');
  const addr = el('span', 'v hash selectable', a.destination_hash);
  kv.appendChild(el('span', 'k', 'Address'));
  kv.appendChild(addr);
  kvRow(kv, 'Heard on', a.interface_label || null, 'Unknown');
  kvRow(kv, 'Link tier', a.min_link_class == null ? null : LINK_CLASS[a.min_link_class] || ('Tier ' + a.min_link_class), 'Unknown');
  if (legacy) kvRow(kv, 'Type', null, 'Legacy peer');
  else kvRow(kv, 'Transport', a.transport_mode == null ? null : (room ? 'LAN room (Mode 3)' : 'Mode ' + a.transport_mode), 'Unknown');
  if (!legacy && a.dedicated != null) kvRow(kv, 'Dedicated', a.dedicated ? 'Yes' : 'No', null);
  if (!legacy && a.allowlisted === true) kvRow(kv, 'Access', 'Allowlisted players only', null);
  if (!room && d.data && d.data.bridge_clients != null) kvRow(kv, 'Players bridged', String(d.data.bridge_clients), null);
  adv.appendChild(kv);
  const advActions = el('div', 'port-row');
  const trace = el('button', 'btn', 'Trace path');
  trace.id = 'detail-trace';
  trace.type = 'button';
  trace.onclick = () => tracePath(d.hash);
  const copy = el('button', 'btn', 'Copy address');
  copy.id = 'detail-copy';
  copy.type = 'button';
  copy.onclick = () => copyText(d.hash);
  const remove = el('button', 'btn btn-danger', 'Remove from list');
  remove.id = 'detail-remove';
  remove.type = 'button';
  remove.onclick = () => forgetServer(d.hash);
  advActions.append(trace, copy, remove);
  adv.appendChild(advActions);
  adv.appendChild(renderPackSection(gameId));
  body.appendChild(adv);

  pane.appendChild(body);
  restoreDetailFocus(pane, prevScroll, keepId, selStart, selEnd);
}

function renderJoinActions(foot, d, gameId) {
  const join = el('button', 'btn btn-join', 'Join server');
  join.id = 'detail-join';
  join.type = 'button';
  join.onclick = joinServer;
  if (d.joining) { join.disabled = true; join.textContent = 'Joining…'; }
  else if (d.joined) { join.textContent = 'Join again'; }
  // No game, no join: say what is missing rather than failing in the error line.
  if (!gameId) {
    join.disabled = true;
    join.title = state.games.length
      ? 'Choose which game this server runs first.'
      : 'No game packs are installed, so nothing can be joined.';
  }
  foot.appendChild(join);

  // The Play button (PLAN.md §13.3), once a join has bound a local port and
  // only for a game whose pack can start it. When the game cannot be found on
  // this machine it becomes "Locate game": the launcher never guesses an
  // executable.
  if (d.joined && d.canLaunch) {
    if (d.launchReady) {
      const play = el('button', 'btn btn-play', d.playing ? 'Starting…' : 'Play');
      play.id = 'detail-play';
      play.type = 'button';
      play.disabled = !!d.playing;
      play.onclick = playServer;
      foot.appendChild(play);
    } else {
      const locate = el('button', 'btn btn-locate', 'Locate game');
      locate.type = 'button';
      locate.onclick = () => locateGame(effectiveGameId(d));
      foot.appendChild(locate);
    }
  }
  if (d.joinMsg) foot.appendChild(el('div', 'join-msg ' + (d.joinErr ? 'err' : 'ok'), d.joinMsg));
}

function restoreDetailFocus(pane, prevScroll, keepId, selStart, selEnd) {
  // Scroll before focus: focusing an element the browser considers off-screen
  // scrolls it into view and would undo the line above.
  const scroller = pane.querySelector('.detail-body');
  if (scroller && prevScroll) scroller.scrollTop = prevScroll;
  if (keepId) {
    const again = document.getElementById(keepId);
    if (again) {
      again.focus({ preventScroll: true });
      if (selStart != null && again.setSelectionRange) {
        try { again.setSelectionRange(selStart, selEnd); } catch (_) { /* number inputs refuse */ }
      }
    }
  }
}

// A server whose announce names no game — every deployed v0.1.10 peer — cannot
// be matched to a pack, and the launcher must not guess one. So the player
// picks, and the choice is remembered per destination.
function renderGamePicker(d) {
  const sec = el('div', 'section');
  sec.appendChild(el('h3', '', 'Which game is this?'));
  if (!state.games.length) {
    sec.appendChild(el('p', 'pack-none',
      'This server did not say what game it runs, and no game packs are installed, '
      + 'so there is nothing to match it to.'));
    return sec;
  }
  sec.appendChild(el('p', 'pack-detail',
    'This server announces only its name, as servers before 0.2 do. Pick the game it runs '
    + 'and the launcher will remember it.'));
  const sel = el('select', 'game-picker');
  sel.id = 'detail-game';
  sel.setAttribute('aria-label', 'Game this server runs');
  const blank = el('option', '', 'Choose a game…');
  blank.value = '';
  sel.appendChild(blank);
  state.games.forEach(g => {
    const o = el('option', '', g.display_name || g.id);
    o.value = g.id;
    sel.appendChild(o);
  });
  sel.value = state.chosenGame.get(d.hash) || '';
  sel.addEventListener('change', () => {
    if (sel.value) state.chosenGame.set(d.hash, sel.value);
    else state.chosenGame.delete(d.hash);
    // A different game is a different bridge, so a previous join no longer
    // describes what this button would do.
    d.joined = false;
    d.joinMsg = null;
    d.joinErr = false;
    renderDetail();
  });
  sec.appendChild(sel);
  return sec;
}

// Which local port this machine binds for the game to connect to. The pack's
// default is the port the game's own dedicated server uses, so anything already
// running one owns it; making it settable is the fix, showing it is what makes
// the failure legible.
function ensureListenPort(gameId) {
  if (!gameId || state.listenPorts.has(gameId)) return;
  state.listenPorts.set(gameId, null);
  invoke('listen_port', { gameId })
    .then(p => {
      state.listenPorts.set(gameId, p == null ? null : p);
      if (state.detail) renderDetail();
    })
    .catch(() => { /* a port we cannot read just renders empty */ });
}

function renderPortSection(d, gameId) {
  ensureListenPort(gameId);
  const sec = el('div', 'section');
  sec.appendChild(el('h3', '', 'Connection'));
  sec.appendChild(el('p', 'pack-detail',
    'Your game connects to this port on this computer. Change it only if something else '
    + 'here already uses it — usually a dedicated server of the same game.'));
  const row = el('div', 'port-row');
  const input = el('input', 'port-input');
  input.id = 'detail-port';
  input.type = 'number';
  input.min = '1';
  input.max = '65535';
  input.setAttribute('aria-label', 'Local port to bind');
  const current = state.listenPorts.get(gameId);
  input.value = d.portDraft != null ? d.portDraft : (current != null ? String(current) : '');
  input.placeholder = 'default';
  input.addEventListener('input', () => { d.portDraft = input.value; });
  row.appendChild(input);
  const reset = el('button', 'btn', 'Use default');
  reset.id = 'detail-port-reset';
  reset.type = 'button';
  reset.onclick = async () => {
    try {
      await invoke('clear_listen_port', { gameId });
      const back = await invoke('listen_port', { gameId });
      if (back != null) state.listenPorts.set(gameId, back);
      d.portDraft = null;
      d.joined = false;
      d.joinMsg = null;
      d.joinErr = false;
    } catch (err) {
      d.joinErr = true;
      d.joinMsg = 'Could not reset the port: ' + errText(err);
    }
    renderDetail();
  };
  row.appendChild(reset);
  sec.appendChild(row);
  const shown = d.portDraft != null && d.portDraft !== '' ? d.portDraft : current;
  if (shown) {
    sec.appendChild(el('p', 'note',
      'Point your game at 127.0.0.1:' + shown + ' — the Play button does this for you.'));
  }
  return sec;
}

// PLAN.md §11.4: a pack's provenance is shown at the moment it matters. The
// launcher shows and never refuses: no code runs here because of a pack.
const TRUST_CLASS = {
  'first-party': 'trust-ok',
  'built in': 'trust-ok',
  'signed community': 'trust-ok',
  'signed by an unknown key': 'trust-warn',
  'unsigned local': 'trust-warn',
};

function renderPackSection(gameId) {
  const sec = el('div', 'pack-section');
  sec.style.marginTop = '14px';
  const pack = gameId ? gameById(gameId) : null;
  if (!pack) {
    sec.appendChild(el('p', 'pack-none', gameId
      ? 'You have no pack for ' + gameId + ', so this launcher cannot tell your game where to connect. Install one to join.'
      : 'This server did not say what game it runs, so no pack can be matched to it.'));
    return sec;
  }
  const line = el('div', 'pack-line');
  line.appendChild(el('span', 'pack-name', 'Game pack: ' + (pack.display_name || pack.id)));
  line.appendChild(el('span', 'badge ' + (TRUST_CLASS[pack.trust] || 'trust-warn'), pack.trust));
  sec.appendChild(line);
  sec.appendChild(el('p', 'pack-detail', pack.trust_detail || ''));
  if (pack.signer) {
    const kv = el('div', 'kv');
    kv.style.marginTop = '8px';
    kvRow(kv, 'Signed by', pack.signer, null);
    if (pack.signature_expires_at != null) {
      // Floored at 0: a signature past its window would not have loaded at all.
      const left = Math.max(0, pack.signature_expires_at - Math.floor(Date.now() / 1000));
      kvRow(kv, 'Signature valid for', fmtDuration(left) + ' more', null);
    }
    sec.appendChild(kv);
  }
  return sec;
}

function kvRow(parent, k, v, fallback, unknown) {
  parent.appendChild(el('span', 'k', k));
  if (v == null || v === '') parent.appendChild(el('span', 'v unknown', fallback || 'Unknown'));
  else if (unknown) parent.appendChild(el('span', 'v unknown', v));
  else parent.appendChild(el('span', 'v', v));
}

// ---------- actions ----------
// Where to attach. `auto` alone finds peers on the local network; a TCP peer
// is how anyone not on the same LAN reaches the mesh at all.
function browseOpts() {
  const tcp = (state.tcpPeer || '').trim();
  // What is typed wins; the saved set fills in when nothing is.
  const saved = state.savedOpts || {};
  return {
    tcp: tcp !== '' ? tcp : (saved.tcp || null),
    auto: state.autoDiscover || !!saved.auto,
  };
}

async function startBrowse() {
  state.startingBrowse = true;
  renderStatus();
  refreshStartButton();
  try {
    await invoke('start_browse', { opts: browseOpts() });
    hideError();
  } catch (err) {
    showError('Could not connect to the mesh: ' + errText(err));
  } finally {
    state.startingBrowse = false;
    await pollStatus();
    renderStatus();
    renderList();
    renderConnectionsNode();
  }
}
async function stopBrowse() {
  try {
    await invoke('stop_browse');
    hideError();
  } catch (err) {
    showError('Could not disconnect: ' + errText(err));
  }
  await pollStatus();
  state.servers = [];
  renderStatus();
  renderList();
  renderConnectionsNode();
}

// What a double-click or Enter does on a row.
function primaryAction(hash) {
  const row = rowByHash(hash);
  if (!row) return;
  if (isRoom(row)) {
    if (!(state.room && state.room.active && state.room.room_hash === hash)) joinRoom();
  } else if (state.detail && state.detail.joined && state.detail.canLaunch && state.detail.launchReady) {
    playServer();
  } else {
    joinServer();
  }
}

async function joinServer() {
  const d = state.detail;
  if (!d || d.joining) return;
  const gameId = effectiveGameId(d);
  if (!gameId) {
    d.joinErr = true;
    d.joinMsg = state.games.length
      ? 'Choose which game this server runs first.'
      : 'No game packs are installed, so this launcher cannot join anything.';
    renderDetail();
    return;
  }
  // A typed port is sent and remembered; a blank field means "whatever is
  // already remembered, else the pack default", which the core decides.
  let listenPort = null;
  const draft = (d.portDraft == null ? '' : String(d.portDraft)).trim();
  if (draft !== '') {
    const n = parseInt(draft, 10);
    if (isNaN(n) || n < 1 || n > 65535) {
      d.joinErr = true;
      d.joinMsg = 'A local port must be a number between 1 and 65535.';
      renderDetail();
      return;
    }
    listenPort = n;
  }
  d.joining = true;
  d.joinMsg = null;
  d.joinErr = false;
  renderDetail();
  try {
    const res = await invoke('join_server', { destinationHash: d.hash, gameId, listenPort });
    d.joined = true;
    state.joinedHash = d.hash;
    d.listenAddr = res.listen_addr;
    if (listenPort != null) state.listenPorts.set(gameId, listenPort);
    d.canLaunch = !!res.can_launch;
    d.launchReady = !!res.launch_ready;
    d.joinErr = false;
    // Binding a local port always succeeds and says nothing about the server.
    // If nobody could route to it, say so rather than letting the game sit on
    // "establishing connection". The commonest cause is an address that is
    // gone: a recreated server gets a new destination.
    if (res.reachable === false) {
      d.joinErr = true;
      d.joinMsg = 'Ready on ' + res.listen_addr + ', but this server did not answer. '
        + 'It may be offline, or its address may have changed — a server that was recreated '
        + 'gets a new one. You can still try: mesh routes can be one-way, so a server can work '
        + 'even when it does not answer a question.';
      renderDetail();
      renderList();
      return;
    }
    if (d.canLaunch && d.launchReady) {
      d.joinMsg = 'Connected. Press Play to start the game (listening on ' + res.listen_addr + ').';
    } else if (d.canLaunch) {
      d.joinMsg = 'Connected on ' + res.listen_addr + '. Locate your game once to enable Play, or point your game at that address.';
    } else {
      d.joinMsg = 'Connected. Point your game at ' + res.listen_addr + ' — this pack does not start the game for you.';
    }
  } catch (err) {
    d.joined = false;
    d.joinErr = true;
    d.joinMsg = 'Could not join: ' + errText(err);
  } finally {
    d.joining = false;
    renderDetail();
    renderList();
  }
}

// Everything that decides *how* to start the game is in launcher-core; this
// only asks it to. The arguments are spawned as a vector there, never a shell.
async function playServer() {
  const d = state.detail;
  if (!d || d.playing) return;
  d.playing = true;
  d.joinMsg = null;
  renderDetail();
  try {
    const res = await invoke('play_server');
    d.joinErr = false;
    d.joinMsg = res.method === 'steam' ? 'Starting the game through Steam…' : 'Starting your game…';
  } catch (err) {
    d.joinErr = true;
    d.joinMsg = 'Could not start the game: ' + errText(err);
  } finally {
    d.playing = false;
    renderDetail();
  }
}

// "Locate game": the launcher never guesses an executable, so the player points
// it at their own copy once and it is remembered (settings.rs).
async function locateGame(gameId) {
  const d = state.detail;
  if (!d || !gameId) return;
  let hint = '';
  try {
    const loc = await invoke('game_location', { gameId });
    hint = loc && loc.detail ? '\n\n' + loc.detail : '';
  } catch (_) { /* a missing location just means an empty hint */ }
  const path = window.prompt(
    'Enter the full path to your ' + gameName(gameId) + ' executable (the launcher will remember it):' + hint,
    (d.savedPath || ''));
  if (path == null) return;
  const trimmed = path.trim();
  if (trimmed === '') return;
  try {
    await invoke('set_game_path', { gameId, path: trimmed });
    const loc = await invoke('game_location', { gameId });
    d.savedPath = loc.saved_path || trimmed;
    d.launchReady = !!loc.launch_ready;
    d.joinErr = !d.launchReady;
    d.joinMsg = d.launchReady ? 'Game located. Press Play to start it.' : (loc.detail || 'That path did not work — try again.');
  } catch (err) {
    d.joinErr = true;
    d.joinMsg = 'Could not set the game path: ' + errText(err);
  }
  renderDetail();
}

function clearMetadataFilters() {
  state.filters.game_id = null;
  state.filters.has_players = false;
  state.filters.not_full = false;
  state.filters.exclude_passworded = false;
  state.filters.dedicated_only = false;
  syncFilterUI();
  pollServers();
}
function clearAllFilters() {
  state.filters = {
    text: '', game_id: null, has_players: false, not_full: false,
    exclude_passworded: false, dedicated_only: false, include_legacy: true,
    max_hops: null,
  };
  syncFilterUI();
  pollServers();
}

// ---------- announce, trace, remove ----------

// The toolbar's Announce: this launcher's own room, if it hosts one, goes out
// now; and the mesh is asked where every saved server is. A browser announces
// nothing of its own — it has no destination — so with no room, asking is the
// whole of it.
async function announceAll() {
  closePopovers();
  let roomSent = false;
  if (state.room && state.room.active && state.room.role === 'host') {
    try { roomSent = await invoke('announce_room'); } catch (_) { roomSent = false; }
  }
  if (!(state.browse && state.browse.running)) {
    if (roomSent) toast('Your room was announced.');
    else showError('Connect to the mesh first.');
    return;
  }
  await refreshKnown({ quiet: true, roomSent });
}

async function announceRoom() {
  closePopovers();
  try {
    const sent = await invoke('announce_room');
    if (sent) toast('Your room was announced to the mesh.');
    else showError('You are not hosting a room, so there is nothing of yours to announce.');
  } catch (err) {
    showError('Could not announce: ' + errText(err));
  }
}

// Ask the mesh where every saved server is. The mesh floods an announce once
// and then suppresses the repeats, so a server that was already running is
// found by a path request; the answer is its announce, and it arrives through
// the normal path, so its row simply appears.
async function refreshKnown(opts = {}) {
  closePopovers();
  try {
    const asked = await invoke('refresh_known_servers');
    hideError();
    const lead = opts.roomSent ? 'Your room was announced. ' : '';
    if (asked) toast(lead + 'The mesh knows a path to ' + asked + ' saved server' + (asked === 1 ? '' : 's') + '; they appear as they answer.');
    else toast(lead + (opts.quiet ? 'None of your saved servers answered. Servers you hear are saved automatically.'
      : 'No saved server answered. Servers are saved as you hear them.'));
  } catch (err) {
    showError(errText(err));
  } finally {
    // Answers arrive as announces over the next moment, not synchronously.
    setTimeout(pollServers, 1500);
    setTimeout(pollServers, 4000);
  }
}

async function tracePath(hash) {
  closePopovers();
  if (!hash) return;
  const row = rowByHash(hash);
  const name = row && row.name ? '“' + row.name + '”' : 'that server';
  toast('Tracing a path to ' + name + '…');
  try {
    const r = await invoke('trace_path', { destinationHash: hash });
    if (r.found) {
      toast('Path to ' + name + ': ' + (r.hops == null ? 'found' : hopsText(r.hops)) + ', answered in ' + r.millis + ' ms.');
    } else {
      showError('No path to ' + name + ' — ' + (r.error || 'the mesh did not answer') + '.');
    }
  } catch (err) {
    showError('Could not trace a path: ' + errText(err));
  }
}

// Remove one server from the list. Gone from the screen at once — the core is
// then asked to drop it from memory, the live list and any index — and back
// only if it announces again, which a server that is really up will.
async function forgetServer(hash) {
  if (!hash) return;
  state.removed.add(hash);
  state.servers = state.servers.filter(s => s.destination_hash !== hash);
  if (state.detail && state.detail.hash === hash) closeDetail();
  renderList();
  renderStatus();
  try {
    await invoke('forget_server', { destinationHash: hash });
    hideError();
    toast('Removed. It returns only if it announces again.');
  } catch (err) {
    state.removed.delete(hash);
    showError('Could not remove that server: ' + errText(err));
  }
  // Re-admitted from the next real poll onwards: a fresh announce after this
  // point is a server that is up, and hiding it would be a lie.
  setTimeout(() => state.removed.delete(hash), 3000);
}

// ---------- keyboard nav ----------
function onListKey(e) {
  const rows = Array.from($('list').querySelectorAll('.row'));
  if (rows.length === 0) return;
  let idx = rows.findIndex(r => r.dataset.hash === state.activeHash);
  if (e.key === 'ArrowDown') {
    e.preventDefault();
    idx = idx < 0 ? 0 : Math.min(rows.length - 1, idx + 1);
    focusRow(rows[idx]);
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    idx = idx < 0 ? 0 : Math.max(0, idx - 1);
    focusRow(rows[idx]);
  } else if (e.key === 'Home') {
    e.preventDefault();
    focusRow(rows[0]);
  } else if (e.key === 'End') {
    e.preventDefault();
    focusRow(rows[rows.length - 1]);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (idx >= 0) openDetail(rows[idx].dataset.hash);
  } else if (e.key === 'Delete' && idx >= 0) {
    e.preventDefault();
    forgetServer(rows[idx].dataset.hash);
  } else if ((e.key === 'ContextMenu' || (e.shiftKey && e.key === 'F10')) && idx >= 0) {
    e.preventDefault();
    const r = rows[idx].getBoundingClientRect();
    openRowMenu(rows[idx].dataset.hash, r.left + 120, r.top + r.height / 2);
  } else if (e.key === 'Escape') {
    if (state.detail) { e.preventDefault(); closeDetail(); }
  }
}
function focusRow(row) {
  if (!row) return;
  state.activeHash = row.dataset.hash;
  for (const [hash, e] of state.rowEls) e.tabIndex = (hash === state.activeHash) ? 0 : -1;
  row.focus();
  row.scrollIntoView({ block: 'nearest' });
}
function setActive(hash) {
  state.activeHash = hash;
  for (const [hash2, e] of state.rowEls) e.tabIndex = (hash2 === state.activeHash) ? 0 : -1;
}

// ---------- polling ----------
async function pollStatus() {
  try {
    state.browse = await invoke('browse_status');
    hideError();
  } catch (err) {
    state.browse = state.browse || { running: false, interfaces: [], heard_total: 0 };
    showError('Could not read the mesh status: ' + errText(err));
  }
}
async function pollServers() {
  try {
    const rows = await invoke('list_servers', { query: buildQuery() });
    state.servers = (Array.isArray(rows) ? rows : []).filter(r => !state.removed.has(r.destination_hash));
  } catch (err) {
    state.servers = [];
    showError('Could not list servers: ' + errText(err));
  }
  state.legacyHiddenCount = 0;
  if (state.filters.include_legacy && hasMetadataFilter()) {
    try {
      const legacyRows = await invoke('list_servers', { query: buildLegacyProbeQuery() });
      state.legacyHiddenCount = (legacyRows || []).filter(r => r.legacy).length;
    } catch (e) { /* ignore secondary failure */ }
  }
  renderStatus();
  renderList();
  renderLegacyNotice();
  if (state.detail) {
    const fresh = rowByHash(state.detail.hash);
    if (fresh) { state.detail.announce = { ...fresh }; renderDetail(); }
  }
}
async function pollAll() {
  await pollStatus();
  await pollServers();
  await pollRoom();
}

// ---------- indexes ----------
//
// An index is a cache of the mesh, never the source of truth (`DESIGN.md` §0).
// The list is empty by default and the launcher is complete with it empty.
async function renderIndexes() {
  const list = $('index-list');
  const count = $('index-count');
  if (!list) return;
  let items = [];
  try {
    items = await invoke('indexes') || [];
  } catch (err) { /* an unreadable list renders as none */ }
  if (count) count.textContent = items.length ? String(items.length) : '';
  list.textContent = '';
  if (!items.length) {
    list.appendChild(el('p', 'item-empty', 'No indexes added. The launcher works without one.'));
    return;
  }
  items.forEach(hash => {
    const row = el('div', 'item');
    const code = el('code', 'item-main', hash);
    code.title = hash;
    row.appendChild(code);
    const drop = el('button', 'btn btn-danger', 'Remove');
    drop.type = 'button';
    drop.onclick = async () => {
      try {
        await invoke('remove_index', { destinationHash: hash });
        await renderIndexes();
        pollServers();
      } catch (err) {
        showError(errText(err));
      }
    };
    row.appendChild(drop);
    list.appendChild(row);
  });
}

function wireIndexPanel() {
  const btn = $('index-add-btn');
  const input = $('index-hash');
  if (!btn || !input) return;
  const add = async () => {
    const hash = (input.value || '').trim();
    if (!hash) return;
    try {
      await invoke('add_index', { destinationHash: hash });
      input.value = '';
      hideError();
      await renderIndexes();
      pollServers();
    } catch (err) {
      showError(errText(err));
    }
  };
  btn.addEventListener('click', add);
  input.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); add(); } });
}

// ---------- build version ----------
// A launcher built before `app_version` existed has no command to call. Leave
// the chip empty then: a blank corner beats one insisting it is "undefined".
async function loadVersion() {
  const chip = $('build-version');
  if (!chip) return;
  try {
    const v = await invoke('app_version');
    chip.textContent = v ? 'v' + v : '';
  } catch (_) {
    chip.textContent = '';
  }
}

// ---------- games ----------
async function loadGames() {
  try {
    state.games = await invoke('list_games') || [];
  } catch (err) {
    state.games = [];
    showError('Could not load the installed games: ' + errText(err));
  }
  const sel = $('f-game');
  sel.textContent = '';
  // `value` must be set explicitly, and empty. An <option> with no value
  // attribute reports its own *text* as its value, so this one once read back
  // as "Any game" and was sent to the core as a game id nothing matches.
  const any = el('option', '', 'All games');
  any.value = '';
  sel.appendChild(any);
  [...state.games].sort((a, b) => (a.display_name || a.id).localeCompare(b.display_name || b.id)).forEach(g => {
    const o = el('option', '', g.display_name || g.id);
    o.value = g.id;
    sel.appendChild(o);
  });
}

// ---------- filter UI ----------
function syncFilterUI() {
  $('f-text').value = state.filters.text;
  $('f-game').value = state.filters.game_id || '';
  $('f-players').checked = state.filters.has_players;
  $('f-notfull').checked = state.filters.not_full;
  $('f-pw').checked = state.filters.exclude_passworded;
  $('f-dedicated').checked = state.filters.dedicated_only;
  $('f-legacy').checked = state.filters.include_legacy;
  $('f-maxhops').value = state.filters.max_hops == null ? '' : state.filters.max_hops;
  updateFilterBadge();
}
function bindFilters() {
  const changed = fn => e => { fn(e); updateFilterBadge(); pollServers(); };
  $('f-text').addEventListener('input', e => { state.filters.text = e.target.value; schedulePoll(); });
  $('f-game').addEventListener('change', changed(e => { state.filters.game_id = e.target.value || null; }));
  $('f-players').addEventListener('change', changed(e => { state.filters.has_players = e.target.checked; }));
  $('f-notfull').addEventListener('change', changed(e => { state.filters.not_full = e.target.checked; }));
  $('f-pw').addEventListener('change', changed(e => { state.filters.exclude_passworded = e.target.checked; }));
  $('f-dedicated').addEventListener('change', changed(e => { state.filters.dedicated_only = e.target.checked; }));
  $('f-legacy').addEventListener('change', changed(e => { state.filters.include_legacy = e.target.checked; }));
  $('f-maxhops').addEventListener('input', e => {
    const v = e.target.value.trim();
    if (v === '') state.filters.max_hops = null;
    else { const n = parseInt(v, 10); state.filters.max_hops = isNaN(n) || n < 0 ? null : n; }
    updateFilterBadge();
    schedulePoll();
  });
  $('filters-btn').addEventListener('click', () => togglePopover('filters-pop', 'filters-btn'));
  $('filters-clear').addEventListener('click', () => { clearAllFilters(); closePopovers(); });
  $('announce-btn').addEventListener('click', announceAll);
  $('announce-more').addEventListener('click', () => togglePopover('announce-pop', 'announce-more'));
  $('announce-room').addEventListener('click', announceRoom);
  $('f-refresh').addEventListener('click', () => refreshKnown());
  $('announce-trace').addEventListener('click', () => tracePath(state.detail && state.detail.hash));
}
let pollSchedule = null;
function schedulePoll() {
  clearTimeout(pollSchedule);
  pollSchedule = setTimeout(pollServers, 250);
}

// ---------- sort headers ----------
function bindSort() {
  document.querySelectorAll('.list-head button.col[data-sort]').forEach(btn => {
    btn.addEventListener('click', () => {
      const key = btn.dataset.sort;
      if (state.sort.sort === key) state.sort.descending = !state.sort.descending;
      else { state.sort.sort = key; state.sort.descending = false; }
      updateSortIndicators();
      renderList();
    });
  });
  updateSortIndicators();
}
function updateSortIndicators() {
  document.querySelectorAll('.list-head button.col[data-sort]').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.sort === state.sort.sort);
    btn.classList.toggle('desc', state.sort.descending);
    btn.setAttribute('aria-sort',
      btn.dataset.sort === state.sort.sort ? (state.sort.descending ? 'descending' : 'ascending') : 'none');
  });
}

// ---------- connections ----------
//
// How this launcher reaches the mesh, kept between runs. Applied when the
// browse node starts: the engine cannot add an interface to a running node, so
// a change here takes effect on the next connect rather than pretending to act
// immediately. Built when its inputs change, never on the poll — rebuilding it
// every two seconds would take the caret out of the address being typed.
async function loadInterfaces() {
  try {
    state.interfaces = await invoke('list_interfaces') || [];
  } catch (err) {
    state.interfaces = [];
  }
  try {
    const known = await invoke('known_servers');
    state.knownCount = Array.isArray(known) ? known.length : 0;
  } catch (_) { /* an older shell */ }
  renderConnections();
}

function renderConnectionsNode() {
  const host = $('conn-node');
  if (!host) return;
  host.textContent = '';
  const b = state.browse;
  const running = !!(b && b.running);
  const head = el('div', 'card-head');
  head.appendChild(el('span', 'dot ' + (running ? 'on' : '')));
  head.appendChild(el('h2', '', running ? 'Connected' : 'Not connected'));
  const btn = el('button', 'btn' + (running ? '' : ' btn-primary'), running ? 'Disconnect' : 'Connect');
  btn.type = 'button';
  btn.disabled = state.startingBrowse;
  btn.onclick = running ? stopBrowse : startBrowse;
  head.appendChild(btn);
  host.appendChild(head);
  const ifaces = (b && b.interfaces) || [];
  if (running && ifaces.length) {
    const list = el('div', 'item-list');
    ifaces.forEach(i => {
      const row = el('div', 'item');
      row.appendChild(el('span', 'dot ' + (i.connected ? 'on' : 'warn')));
      row.appendChild(el('span', 'item-main', i.label));
      row.appendChild(el('span', 'muted small', i.connected ? 'up' : 'down'));
      list.appendChild(row);
    });
    host.appendChild(list);
  } else {
    host.appendChild(el('p', 'card-text', running
      ? 'Listening on the mesh.'
      : 'Changes below apply the next time you connect.'));
  }
}

function renderConnections() {
  const body = $('connections-body');
  if (!body) return;
  body.textContent = '';

  const node = el('div', 'card');
  node.id = 'conn-node';
  body.appendChild(node);

  const add = el('div', 'card');
  const ah = el('div', 'card-head');
  ah.appendChild(el('h2', '', 'Add a connection'));
  add.appendChild(ah);
  add.appendChild(el('p', 'card-text',
    'A relay is someone who passes Reticulum along — a friend’s machine, a community hub. '
    + 'Reticulum has no directory, so its address is something you were told.'));
  const form = el('div', 'inline-form');
  const tcp = el('input');
  tcp.id = 'f-tcp';
  tcp.type = 'text';
  tcp.spellcheck = false;
  tcp.placeholder = 'Relay address, like hub.example.org:4242';
  tcp.value = state.tcpPeer || '';
  tcp.oninput = () => { state.tcpPeer = tcp.value; };
  const save = el('button', 'btn btn-primary', 'Save');
  save.type = 'button';
  save.title = 'Save these so this launcher uses them every time';
  save.onclick = async () => {
    save.disabled = true;
    try {
      const addr = (state.tcpPeer || '').trim();
      if (addr) await invoke('add_interface', { kind: 'tcp', addr });
      if (state.autoDiscover) await invoke('add_interface', { kind: 'auto', addr: null });
      state.savedOpts = await invoke('saved_browse_opts') || state.savedOpts;
      await loadInterfaces();
      hideError();
      toast(state.browse && state.browse.running ? 'Saved. Reconnect to use it.' : 'Saved.');
    } catch (e) {
      showError('Could not save that connection: ' + errText(e));
    } finally { save.disabled = false; }
  };
  form.append(tcp, save);
  add.appendChild(form);
  const autoWrap = el('label', 'toggle-row');
  const auto = el('input');
  auto.type = 'checkbox';
  auto.checked = state.autoDiscover;
  auto.onchange = () => { state.autoDiscover = auto.checked; };
  autoWrap.appendChild(auto);
  autoWrap.appendChild(el('span', '', 'Also find neighbours on this local network'));
  add.appendChild(autoWrap);
  body.appendChild(add);

  const saved = el('div', 'card');
  const sh = el('div', 'card-head');
  sh.appendChild(el('h2', '', 'Saved connections'));
  saved.appendChild(sh);
  const list = el('div', 'item-list');
  list.id = 'iface-list';
  if (!(state.interfaces || []).length) {
    list.appendChild(el('p', 'item-empty', 'None yet. Without one the launcher looks on your local network only.'));
  }
  (state.interfaces || []).forEach(i => {
    const row = el('div', 'item');
    row.appendChild(icon(i.kind === 'auto' ? 'lan' : 'plug'));
    row.appendChild(el('span', 'item-main', i.label));
    const del = el('button', 'btn btn-danger', 'Forget');
    del.type = 'button';
    del.onclick = async () => {
      del.disabled = true;
      try {
        await invoke('remove_interface', { id: i.id });
        await loadInterfaces();
      } catch (e) {
        showError('Could not forget that: ' + errText(e));
      } finally { del.disabled = false; }
    };
    row.appendChild(del);
    list.appendChild(row);
  });
  saved.appendChild(list);
  body.appendChild(saved);

  // Saved servers: what the launcher asks the mesh about, never what it lists.
  const mem = el('div', 'card');
  const mh = el('div', 'card-head');
  mh.appendChild(el('h2', '', 'Saved servers'));
  mem.appendChild(mh);
  mem.appendChild(el('p', 'card-text',
    (state.knownCount
      ? 'This launcher remembers ' + state.knownCount + ' server' + (state.knownCount === 1 ? '' : 's') + ' it has heard. '
      : 'Servers you hear are remembered. ')
    + 'It asks the mesh about them now and then, so a server that is up appears in the list even '
    + 'when its announce did not reach you. A server that does not answer is never listed.'));
  const clear = el('button', 'btn btn-danger', 'Forget all saved servers');
  clear.type = 'button';
  clear.id = 'forget-all';
  clear.disabled = !state.knownCount;
  clear.onclick = async () => {
    try {
      await invoke('forget_server', { destinationHash: null });
      await loadInterfaces();
      toast('Saved servers forgotten.');
    } catch (e) {
      showError('Could not forget them: ' + errText(e));
    }
  };
  mem.appendChild(clear);
  body.appendChild(mem);

  renderConnectionsNode();
}

// ---------- init ----------
async function init() {
  wireMenus();
  bindFilters();
  wireIndexPanel();
  renderIndexes();
  bindSort();
  syncFilterUI();
  document.querySelectorAll('.nav-item').forEach(b => b.addEventListener('click', () => setView(b.dataset.view)));
  $('list').addEventListener('keydown', onListKey);
  document.addEventListener('keydown', e => {
    if (e.key === 'Escape' && state.detail && document.activeElement?.closest('.detail')) {
      e.preventDefault(); closeDetail();
    }
  });
  await loadGames();
  await loadVersion();
  // The saved interfaces, so Connect uses what was configured rather than
  // asking the player to retype a relay address they were given once.
  try {
    state.interfaces = await invoke('list_interfaces') || [];
    state.savedOpts = await invoke('saved_browse_opts') || {};
    if (state.savedOpts.tcp && !state.tcpPeer) state.tcpPeer = state.savedOpts.tcp;
    if (state.savedOpts.auto) state.autoDiscover = true;
  } catch (_) { /* an older backend simply has none */ }
  renderConnections();
  loadInterfaces();
  await loadLanHelper();
  await pollAll();
  state.pollTimer = setInterval(pollAll, 2000);
}
init().catch(err => showError('The launcher could not start: ' + errText(err)));


// ---------- LAN rooms (PLAN.md §14, step 5) ----------
//
// Everything that decides anything is in launcher-core (`lan.rs`); this shows
// it. Two rules carry over from there: a room row is never joined as a server,
// and the pack's warnings — every port reachable, or never tested — are shown
// before joining, unsoftened.

function isRoom(r) { return !!r && r.transport_mode === 3; }
function lanGames() { return (state.games || []).filter(g => g.lan); }
function gameById(id) { return (state.games || []).find(g => g.id === id) || null; }

const ADAPTER_TEXT = {
  none: '',
  waiting: 'waiting for a seat in the room…',
  starting: 'bringing up the network adapter… (approve the password or administrator prompt)',
  up: 'network adapter up',
  stopped: 'network adapter stopped',
  failed: 'network adapter failed',
};

async function loadLanHelper() {
  try {
    state.lanHelper = await invoke('lan_helper');
  } catch (_) {
    state.lanHelper = null; // an older shell has no rooms
  }
}

async function pollRoom() {
  try {
    state.room = await invoke('room_status');
    state.roomsAvailable = true;
  } catch (_) {
    state.room = null;
    state.roomsAvailable = false;
  }
  const nav = $('nav-rooms');
  if (nav) nav.hidden = !state.roomsAvailable;
  const dot = $('nav-rooms-dot');
  if (dot) {
    const r = state.room;
    dot.hidden = !(r && r.active);
    dot.classList.toggle('bad', !!(r && r.active && (r.adapter === 'failed' || roomFirewallDropped(r).length || roomPackGaps(r).length)));
  }
  renderRoomBanner();
  renderRoomPanel();
  maybeAutoCheck();
}

function roomLine(r) {
  const g = gameById(r.game_id);
  const who = r.role === 'host' ? 'Hosting a LAN room' : 'In a LAN room';
  const parts = [who + ' for ' + (g ? g.display_name : (r.game_id || 'a game'))];
  if (r.address) parts.push('as ' + r.address);
  const adapter = ADAPTER_TEXT[r.adapter];
  if (adapter) parts.push(adapter);
  parts.push(r.members.length + (r.members.length === 1 ? ' member' : ' members'));
  return parts.join(' — ');
}

function renderRoomBanner() {
  const b = $('room-banner');
  if (!b) return;
  const r = state.room;
  if (!r || !r.active) { b.classList.add('hidden'); b.textContent = ''; return; }
  b.classList.remove('hidden');
  const blocked = roomFirewallDropped(r).length > 0;
  b.classList.toggle('err', r.adapter === 'failed' || !!r.refused || blocked || roomPackGaps(r).length > 0);
  b.textContent = '';
  b.appendChild(icon('lan', 'rb-icon'));
  const text = el('div', 'rb-text');
  text.appendChild(el('div', 'rb-title', roomLine(r)));
  const sub = el('div', 'rb-sub');
  if (r.error) sub.appendChild(el('span', 'room-err', r.error + ' '));
  if (r.refused) sub.appendChild(el('span', 'room-err', 'Refused: ' + r.refused + ' '));
  if (blocked) sub.appendChild(el('span', 'room-err', 'This computer’s firewall is blocking the room; see LAN room.'));
  else if (roomPackGaps(r).length) sub.appendChild(el('span', 'room-err', 'The game uses ports its pack does not list; see LAN room.'));
  if (sub.childNodes.length) text.appendChild(sub);
  b.appendChild(text);
  const open = el('button', 'btn', 'Open');
  open.type = 'button';
  open.onclick = () => setView('rooms');
  b.appendChild(open);
  const leave = el('button', 'btn', 'Leave room');
  leave.type = 'button';
  leave.id = 'room-banner-leave';
  leave.onclick = leaveRoom;
  b.appendChild(leave);
}

function renderHelperStatus(parent) {
  const h = state.lanHelper;
  if (!h) return;
  const box = el('div', 'helper-status ' + (h.ready ? 'ok' : 'warn'));
  box.appendChild(el('span', '', h.detail));
  // Granting is optional on an installed Linux launcher — without it a room
  // asks for the password each time — so it is offered even when ready.
  if (h.can_grant) {
    const g = el('button', 'quiet', 'Grant permission');
    g.type = 'button';
    g.id = 'grant-helper';
    g.onclick = grantHelper;
    box.appendChild(g);
  }
  if (h.can_revoke) {
    const r = el('button', 'quiet', 'Revoke permission');
    r.type = 'button';
    r.id = 'revoke-helper';
    r.onclick = revokeHelper;
    box.appendChild(r);
  }
  parent.appendChild(box);
}

function renderLanWarnings(parent, gameId) {
  const g = gameById(gameId);
  if (!g || !g.lan) {
    parent.appendChild(el('p', 'warn-line',
      'This launcher has no LAN room support for this game, so it cannot join this room.'));
    return;
  }
  if (g.lan.inbound_any) {
    parent.appendChild(el('p', 'warn-line',
      'While you are in this room, other members can reach every network service on this '
      + 'machine through it — this game’s ports cannot be predicted. Only join rooms of people you trust.'));
  } else {
    parent.appendChild(el('p', 'muted small',
      'Other members can reach this computer only on the game’s own ports: ' + g.lan.ports.join(', ') + '.'));
  }
  if (!g.lan.tested) {
    parent.appendChild(el('p', 'warn-line',
      'Nobody has played ' + g.display_name + ' in a LAN room yet. It may work; it is untested.'));
  }
}

function renderRoomSection(d) {
  const sec = el('div', 'section');
  sec.appendChild(el('h3', '', 'LAN room'));
  sec.appendChild(el('p', '',
    'Joining puts this computer on a virtual LAN with the room’s members. Then start the game '
    + 'and use its own LAN server list: the room makes the other players look local.'));
  renderLanWarnings(sec, effectiveGameId(d));
  renderHelperStatus(sec);
  return sec;
}

function renderRoomFoot(foot, d, gameId) {
  const r = state.room;
  const here = r && r.active && r.room_hash === d.hash;
  const g = gameById(gameId);
  const helperReady = !!(state.lanHelper && state.lanHelper.ready);
  const join = el('button', 'btn btn-join', here ? 'In this room' : (d.joining ? 'Joining…' : 'Join room'));
  join.id = 'detail-join';
  join.type = 'button';
  join.onclick = joinRoom;
  join.disabled = here || !!d.joining || !g || !g.lan || !helperReady;
  if (!helperReady && state.lanHelper) join.title = state.lanHelper.detail;
  foot.appendChild(join);
  if (here) {
    const leave = el('button', 'btn', 'Leave');
    leave.type = 'button';
    leave.onclick = leaveRoom;
    foot.appendChild(leave);
  }
  if (d.joinMsg) foot.appendChild(el('div', 'join-msg ' + (d.joinErr ? 'err' : 'ok'), d.joinMsg));
}

async function joinRoom() {
  const d = state.detail;
  if (!d || d.joining) return;
  const gameId = effectiveGameId(d);
  d.joining = true;
  d.joinMsg = null;
  d.joinErr = false;
  renderDetail();
  try {
    state.room = await invoke('join_room', { destinationHash: d.hash, gameId });
    d.joinMsg = 'Joining the room. When the adapter is up, start the game and open its LAN server list.';
  } catch (err) {
    d.joinErr = true;
    d.joinMsg = 'Could not join the room: ' + errText(err);
  } finally {
    d.joining = false;
    renderRoomBanner();
    renderRoomPanel(true);
    renderDetail();
    renderList();
  }
}

async function hostRoom() {
  const games = lanGames();
  const gameId = state.hostDraft.game_id || (games[0] && games[0].id);
  if (!gameId || state.roomBusy) return;
  state.roomBusy = true;
  renderRoomPanel(true);
  try {
    const name = (state.hostDraft.name || '').trim();
    state.room = await invoke('host_room', { gameId, name: name || null });
    hideError();
  } catch (err) {
    showError('Could not open the room: ' + errText(err));
  } finally {
    state.roomBusy = false;
    renderRoomBanner();
    renderRoomPanel(true);
  }
}

function roomPackGaps(r) {
  return (r && r.pack_gaps && r.pack_gaps.ports) || [];
}

// Traffic the room's own filter refused because the game's pack does not list
// its port (`game_bridge::lan_filter::RefusedLog`). This is the only witness to
// a missing port, so it is said plainly, with a report the player can send.
function renderRoomPackGaps(parent, r) {
  const g = r.pack_gaps;
  if (!g) return; // an older shell
  const missing = g.ports || [];
  const allowed = g.allowed || [];
  const act = state.portsAction;
  if (!missing.length && !allowed.length && !act.message) return;
  const box = el('div', 'room-firewall');
  box.id = 'room-pack-gaps';
  if (missing.length) {
    box.appendChild(el('p', 'room-err',
      'This game used ports its pack does not list, so the room blocked them. That is probably why '
      + 'joining fails. Ports it needs: ' + missing.join(', ') + '.'));
  }
  if ((g.can_allow || []).length) {
    const b = el('button', 'btn btn-join', act.busy ? 'Working…' : 'Allow on this computer');
    b.type = 'button';
    b.id = 'room-allow-ports';
    b.disabled = act.busy;
    b.title = 'Lets ' + g.can_allow.join(', ') + ' through for this game on this computer, and opens '
      + 'its firewall to the room if it is not already. Only room members can use them. Undo any time.';
    b.onclick = () => portsAction('allow_room_ports', {});
    box.appendChild(b);
    box.appendChild(el('p', 'small',
      'The other computer may need the same: its room page will offer it after the next try.'));
  }
  if (allowed.length) {
    const line = el('p', 'small', 'Also allowed on this computer for this game: ' + allowed.join(', ') + ' ');
    const undo = el('button', 'quiet', 'Undo');
    undo.type = 'button';
    undo.id = 'room-reset-ports';
    undo.disabled = act.busy;
    undo.onclick = () => portsAction('reset_room_ports', {});
    line.appendChild(undo);
    box.appendChild(line);
  }
  if (act.message) box.appendChild(el('p', act.error ? 'room-err' : 'check-ok', act.message));
  if (!missing.length) { parent.appendChild(box); return; }
  const ul = el('ul', 'player-list rows');
  (g.seen || []).forEach(line => ul.appendChild(el('li', 'small', line)));
  box.appendChild(ul);
  if (g.report) {
    const pre = el('pre', 'room-command', g.report);
    pre.id = 'room-pack-gaps-report';
    box.appendChild(pre);
    const copy = el('button', 'quiet', 'Copy report');
    copy.type = 'button';
    copy.id = 'room-pack-gaps-copy';
    copy.onclick = async () => {
      try {
        await navigator.clipboard.writeText(g.report);
        copy.textContent = 'Copied';
      } catch (_) {
        copy.textContent = 'Select the text above and copy it';
      }
    };
    box.appendChild(copy);
  }
  parent.appendChild(box);
}

// The last resort, for a join that hangs with nothing named: every port above
// 1024 for this game on this computer. Never system ports, remote desktops or
// databases — the filter refuses those whatever is asked (lan_filter::is_allowable).
function renderRoomWideOpen(parent, r) {
  const g = r.pack_gaps;
  if (!g || r.adapter !== 'up') return;
  const act = state.portsAction;
  if (g.wide_open) {
    const p = el('p', 'warn-line',
      'Every port above 1024 is open to this room’s members for this game on this computer. ');
    p.id = 'room-wide-open';
    const off = el('button', 'quiet', 'Turn off');
    off.type = 'button';
    off.id = 'room-wide-off';
    off.disabled = act.busy;
    off.onclick = () => portsAction('set_room_wide_open', { on: false });
    p.appendChild(off);
    parent.appendChild(p);
    return;
  }
  if (!r.members.some(m => !m.is_self)) return;
  const d = el('details', 'small');
  d.id = 'room-still-stuck';
  d.style.marginTop = '14px';
  d.appendChild(el('summary', '', 'Still can’t join?'));
  d.appendChild(el('p', 'small',
    'If the game shows up but joining hangs and nothing above names a port, let this game use every '
    + 'port above 1024 on this computer. Only room members can reach them; system ports, remote desktop and databases stay shut. '
    + 'Do it on both computers, then try again.'));
  const on = el('button', 'quiet', act.busy ? 'Working…' : 'Open more ports for this game');
  on.type = 'button';
  on.id = 'room-wide-on';
  on.disabled = act.busy;
  on.onclick = () => portsAction('set_room_wide_open', { on: true });
  d.appendChild(on);
  parent.appendChild(d);
}

function roomFirewallDropped(r) {
  return (r && r.firewall && r.firewall.dropped) || [];
}

// This machine's firewall (`game_bridge::lan_firewall`). The room's helper
// opens it on its own wherever it already runs with full privilege, so most
// players never see this.
function renderRoomFirewall(parent, r) {
  const f = r.firewall;
  if (!f) return; // an older shell
  const dropped = roomFirewallDropped(r);
  const blocks = f.blocking_programs || [];
  const others = f.other_firewalls || [];
  const fw = state.firewallAction;
  if (!dropped.length && !f.heads_up && !fw.message) return;
  const box = el('div', 'room-firewall');
  box.id = 'room-firewall';
  dropped.forEach(d => box.appendChild(el('p', 'room-err',
    d.from + ' tried to join a game on this computer (' + d.transport.toUpperCase() + ' ' + d.port
    + '), but this computer’s firewall blocked it.')));
  if (!dropped.length && f.heads_up) {
    box.appendChild(el('p', 'warn-line',
      'This computer’s firewall may stop the others from joining games you host here.'));
  }
  if (others.length) {
    box.appendChild(el('p', 'room-err',
      others.join(' and ') + (others.length === 1 ? ' is' : ' are')
      + ' on, and blocks the room no matter what Windows allows. Open it and allow Lanthorn '
      + 'and your game, or switch it off while you play.'));
  }
  if (blocks.length) {
    box.appendChild(el('p', 'room-err',
      'Windows is blocking ' + blocks.map(b => b.program).join(', ')
      + ' — somebody once said no to its firewall question.'));
    const u = el('button', 'btn btn-join', fw.busy ? 'Working…' : 'Unblock');
    u.type = 'button';
    u.id = 'room-unblock';
    u.disabled = fw.busy;
    u.title = 'Windows asks for administrator rights once.';
    u.onclick = () => firewallAction('unblock_room_programs');
    box.appendChild(u);
  }
  if (f.can_fix && (dropped.length || f.heads_up)) {
    const b = el('button', 'btn btn-join', fw.busy ? 'Working…' : 'Fix it');
    b.type = 'button';
    b.id = 'room-fix';
    b.disabled = fw.busy;
    b.title = 'Lets LAN rooms through this computer’s firewall, for good. You are asked for your '
      + 'password (or administrator rights) once.';
    b.onclick = () => firewallAction('fix_room_firewall');
    box.appendChild(b);
  }
  if (fw.message) box.appendChild(el('p', fw.error ? 'room-err' : 'check-ok', fw.message));
  if (f.command && (dropped.length || f.heads_up)) {
    const manual = el('details', 'small');
    manual.appendChild(el('summary', '', 'Or do it yourself'));
    if (f.advice) manual.appendChild(el('p', 'small', f.advice));
    const pre = el('pre', 'room-command', f.command);
    pre.id = 'room-firewall-command';
    manual.appendChild(pre);
    box.appendChild(manual);
  }
  parent.appendChild(box);
}

const PORTS_DONE = {
  allow_room_ports: 'Allowed. Try joining again — and please still send the report, so the game gets fixed for everyone.',
  reset_room_ports: 'Done: only the game’s own ports again.',
  wide_on: 'Every port above 1024 is open to the room for this game. Try joining again.',
  wide_off: 'Done: only the game’s own ports again.',
};

async function portsAction(cmd, args) {
  if (state.portsAction.busy) return;
  state.portsAction = { busy: true, message: null, error: false };
  renderRoomPanel(true);
  const key = cmd === 'set_room_wide_open' ? (args.on ? 'wide_on' : 'wide_off') : cmd;
  try {
    state.room = await invoke(cmd, args);
    state.portsAction = { busy: false, error: false, message: PORTS_DONE[key] };
    state.autoCheckSig = null;
  } catch (err) {
    state.portsAction = { busy: false, error: true, message: 'Not changed: ' + errText(err) };
    try { state.room = await invoke('room_status'); } catch (_) { /* keep the last view */ }
  }
  renderRoomBanner();
  renderRoomPanel(true);
}

async function firewallAction(cmd) {
  if (state.firewallAction.busy) return;
  state.firewallAction = { busy: true, message: null, error: false };
  renderRoomPanel(true);
  try {
    state.room = await invoke(cmd);
    state.firewallAction = { busy: false, error: false, message: 'Done. Ask the others to try joining again.' };
    // What was wrong may now be right: check again.
    state.autoCheckSig = null;
  } catch (err) {
    state.firewallAction = { busy: false, error: true, message: 'Not changed: ' + errText(err) };
  }
  renderRoomBanner();
  renderRoomPanel(true);
}

// One line a player can read without knowing what a port is.
function renderRoomVerdict(parent, r) {
  const others = r.members.filter(m => !m.is_self).length;
  const c = state.roomCheck;
  let text = null, cls = 'small';
  if (roomFirewallDropped(r).length) {
    text = 'Not ready: the others cannot join games on this computer yet.';
    cls = 'room-err';
  } else if (roomPackGaps(r).length) {
    text = 'Not ready: this game needs ports its pack does not list yet — see below.';
    cls = 'room-err';
  } else if (r.adapter !== 'up') {
    return;
  } else if (!others) {
    text = 'Waiting for someone to join the room.';
  } else if (c.busy) {
    text = 'Checking the room…';
  } else if (c.result && c.hash === r.room_hash) {
    text = c.result.ok ? 'Ready to play. Start the game and use its LAN list.' : 'Not ready yet — see below.';
    cls = c.result.ok ? 'check-ok' : 'room-err';
  }
  if (!text) return;
  const p = el('p', cls, text);
  p.id = 'room-verdict';
  parent.appendChild(p);
}

// Check the room by itself whenever someone joins or leaves, so nobody has to
// know there is a check to run. A few seconds after the change: the newcomer's
// adapter needs a moment to come up and answer.
function maybeAutoCheck() {
  const r = state.room;
  if (!r || !r.active || r.adapter !== 'up') return;
  const others = r.members.filter(m => !m.is_self).map(m => m.address).sort();
  if (!others.length) return;
  const sig = r.room_hash + '|' + others.join(',');
  if (sig === state.autoCheckSig) return;
  state.autoCheckSig = sig;
  clearTimeout(state.autoCheckTimer);
  state.autoCheckTimer = setTimeout(() => {
    if (state.room && state.room.active && !state.roomCheck.busy) checkRoom();
  }, AUTO_CHECK_DELAY_MS);
}

// The room check sends a real broadcast on the game's own port from this
// machine and every member's launcher answers it, so it finds what a game's
// empty LAN list cannot say.
async function checkRoom() {
  const r = state.room;
  if (!r || !r.active || state.roomCheck.busy) return;
  state.roomCheck = { busy: true, result: null, error: null, hash: r.room_hash };
  renderRoomPanel(true);
  try {
    state.roomCheck.result = await invoke('check_room');
  } catch (err) {
    state.roomCheck.error = errText(err);
  }
  state.roomCheck.busy = false;
  renderRoomPanel(true);
}

function renderRoomCheck(actions, parent, r) {
  const c = state.roomCheck;
  const btn = el('button', 'btn', c.busy ? 'Checking…' : 'Check room');
  btn.type = 'button';
  btn.id = 'room-check';
  const others = r.members.filter(m => !m.is_self).length;
  btn.disabled = c.busy || r.adapter !== 'up' || others === 0;
  btn.title = others === 0
    ? 'Needs another member in the room to answer.'
    : 'Send a LAN broadcast on this game’s port and connect to its TCP ports, and see who answers. Run it before starting the game.';
  btn.onclick = checkRoom;
  actions.appendChild(btn);
  if (c.hash !== r.room_hash) return;
  const out = el('div', 'room-check');
  out.id = 'room-check-result';
  if (c.error) {
    out.appendChild(el('p', 'room-err', 'The check could not run: ' + c.error));
  } else if (c.result) {
    const res = c.result;
    res.findings.forEach((line, i) => out.appendChild(el('p', i === 0 ? (res.ok ? 'check-ok' : 'room-err') : 'small', line)));
    const ul = el('ul', 'player-list rows');
    res.members.forEach(m => ul.appendChild(el('li', '',
      m.address + ' — ' + (m.ok ? 'answered' + (m.round_trip_ms != null ? ' in ' + m.round_trip_ms + ' ms' : '')
        : ((m.tcp_unanswered || []).length ? 'did not answer TCP ' + m.tcp_unanswered.join(', ') : 'did not answer everything')))));
    out.appendChild(ul);
  }
  parent.appendChild(out);
}

async function leaveRoom() {
  state.firewallAction = { busy: false, message: null, error: false };
  state.portsAction = { busy: false, message: null, error: false };
  state.autoCheckSig = null;
  try {
    await invoke('leave_room');
    hideError();
  } catch (err) {
    showError('Could not leave the room: ' + errText(err));
  }
  await pollRoom();
  if (state.detail) renderDetail();
  renderList();
}

async function grantHelper() {
  try {
    state.lanHelper = await invoke('grant_lan_helper');
    hideError();
  } catch (err) {
    showError('The permission was not granted: ' + errText(err));
  }
  renderRoomPanel(true);
  if (state.detail) renderDetail();
}

async function revokeHelper() {
  try {
    state.lanHelper = await invoke('revoke_lan_helper');
    hideError();
  } catch (err) {
    showError('The permission was not revoked: ' + errText(err));
  }
  renderRoomPanel(true);
  if (state.detail) renderDetail();
}

// Rebuilt only when what it shows changed, so a room name being typed and a
// game being picked survive the two-second poll.
function renderRoomPanel(force) {
  const panel = $('room-panel');
  const body = $('room-body');
  if (!panel || !body) return;
  panel.hidden = !state.roomsAvailable;
  const sig = JSON.stringify([state.room, state.lanHelper, state.roomBusy, state.roomCheck, state.firewallAction, state.portsAction, lanGames().map(g => g.id)]);
  if (!force && sig === state.roomPanelSig) return;
  state.roomPanelSig = sig;

  const r = state.room;
  body.textContent = '';

  if (r && r.active) {
    const card = el('div', 'card');
    const status = el('div', 'room-status');
    status.appendChild(art(r.game_id, '', { room: true }));
    const st = el('div', 'room-status-text');
    st.appendChild(el('div', 'room-status-title', (r.role === 'host' ? 'Hosting' : 'In a room') + ' · ' + gameName(r.game_id)));
    st.appendChild(el('div', 'room-status-sub', roomLine(r)));
    status.appendChild(st);
    card.appendChild(status);
    renderRoomVerdict(card, r);
    const ul = el('ul', 'member-list player-list');
    r.members.forEach(m => {
      const li = el('li', m.is_self ? 'self' : '', m.address + (m.is_self ? ' (you)' : ''));
      ul.appendChild(li);
    });
    card.appendChild(ul);
    if (r.room_hash) {
      const code = el('code', 'room-hash', r.room_hash);
      code.title = 'This room’s address. Others find it in their server list.';
      card.appendChild(code);
    }
    const actions = el('div', 'room-actions');
    card.appendChild(actions);
    const results = el('div');
    card.appendChild(results);
    renderRoomCheck(actions, results, r);
    if (r.role === 'host') {
      const ann = el('button', 'btn', 'Announce now');
      ann.type = 'button';
      ann.id = 'room-announce';
      ann.title = 'Announce this room to the mesh now instead of waiting for the next announce.';
      ann.onclick = announceRoom;
      actions.appendChild(ann);
    }
    const leave = el('button', 'btn btn-danger', 'Leave room');
    leave.type = 'button';
    leave.id = 'room-leave';
    leave.onclick = leaveRoom;
    actions.appendChild(leave);
    renderRoomFirewall(card, r);
    renderRoomPackGaps(card, r);
    renderRoomWideOpen(card, r);
    body.appendChild(card);
    const helper = el('div');
    renderHelperStatus(helper);
    body.appendChild(helper);
    return;
  }

  renderHelperStatus(body);
  const games = lanGames();
  const card = el('div', 'card');
  const head = el('div', 'card-head');
  head.appendChild(el('h2', '', 'Host a room'));
  card.appendChild(head);
  if (!games.length) {
    card.appendChild(el('p', 'card-text', 'No installed game offers LAN rooms.'));
    body.appendChild(card);
    return;
  }
  card.appendChild(el('p', 'card-text',
    'Open a room for a game and the others join it from their server list. To join someone '
    + 'else’s room, find it in Servers — rooms carry a LAN room badge.'));
  if (!state.hostDraft.game_id || !gameById(state.hostDraft.game_id)?.lan) {
    state.hostDraft.game_id = games[0].id;
  }
  const form = el('div', 'host-form');
  const select = el('select');
  select.id = 'host-game';
  select.setAttribute('aria-label', 'Game for the room');
  games.forEach(g => {
    const o = el('option', '', g.display_name);
    o.value = g.id;
    o.selected = g.id === state.hostDraft.game_id;
    select.appendChild(o);
  });
  select.onchange = () => { state.hostDraft.game_id = select.value; renderRoomPanel(true); };
  form.appendChild(select);
  const name = el('input');
  name.id = 'host-name';
  name.type = 'text';
  name.placeholder = 'Room name (optional)';
  name.setAttribute('aria-label', 'Room name');
  name.value = state.hostDraft.name || '';
  name.oninput = () => { state.hostDraft.name = name.value; };
  form.appendChild(name);
  const host = el('button', 'btn btn-primary', state.roomBusy ? 'Opening…' : 'Host a LAN room');
  host.type = 'button';
  host.id = 'host-btn';
  host.disabled = state.roomBusy || !(state.lanHelper && state.lanHelper.ready);
  host.onclick = hostRoom;
  form.appendChild(host);
  card.appendChild(form);
  const warn = el('div');
  warn.style.marginTop = '12px';
  renderLanWarnings(warn, state.hostDraft.game_id);
  card.appendChild(warn);
  body.appendChild(card);
}
