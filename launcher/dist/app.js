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
  pollTimer: null,
  startingBrowse: false,
  interfaces: [],
  savedOpts: {},
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
};

const LINK_CLASS = { 1: 'Low-rate', 2: 'TCP / bursty', 3: 'High-bitrate' };

// ---------- helpers ----------
function $(id) { return document.getElementById(id); }
function el(tag, cls, txt) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (txt != null) e.textContent = txt;
  return e;
}
function fmtSeen(secs) {
  if (secs == null) return '—';
  if (secs < 2) return 'just now';
  if (secs < 60) return secs + 's ago';
  const m = Math.floor(secs / 60);
  if (m < 60) return m + 'm ' + (secs % 60) + 's ago';
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
// The game a join would use: what the announce said, else what the player
// chose for this destination. `null` means neither, and nothing may join.
function effectiveGameId(d) {
  if (!d) return null;
  if (d.announce && d.announce.game_id) return d.announce.game_id;
  return state.chosenGame.get(d.hash) || null;
}
function shortHash(h) {
  if (!h || h.length < 12) return h || '';
  return h.slice(0, 8) + '…' + h.slice(-4);
}
function showError(msg) {
  const box = $('error');
  box.innerHTML = '';
  box.appendChild(el('span', '', String(msg)));
  const btn = el('button', '', '×');
  btn.setAttribute('aria-label', 'Dismiss');
  btn.onclick = () => box.classList.add('hidden');
  box.appendChild(btn);
  box.classList.remove('hidden');
  clearTimeout(state.errorTimer);
  state.errorTimer = setTimeout(() => box.classList.add('hidden'), 6000);
}
function hideError() { $('error').classList.add('hidden'); }

// ---------- query building ----------
function hasMetadataFilter() {
  const f = state.filters;
  return !!(f.game_id || f.has_players || f.not_full || f.exclude_passworded ||
            f.exclude_allowlisted || f.dedicated_only);
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
  q.exclude_allowlisted = false;
  q.dedicated_only = false;
  q.transport_modes = null;
  q.max_link_class = null;
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

// ---------- rendering: status ----------
function renderStatus() {
  const s = $('status');
  s.innerHTML = '';
  const b = state.browse;
  if (!b) { s.textContent = '…'; return; }

  const running = b.running;
  const node = el('span', 'pill');
  const dot = el('span', 'dot ' + (running ? 'on' : 'off'));
  node.appendChild(dot);
  node.appendChild(el('span', '', running ? 'Browse node running' : 'Browse node stopped'));
  s.appendChild(node);

  if (b.interfaces && b.interfaces.length) {
    const ifaces = el('span', 'ifaces');
    ifaces.appendChild(el('span', '', 'Interfaces: '));
    b.interfaces.forEach((iface, i) => {
      const span = el('span', 'iface' + (iface.connected ? '' : ' bad'));
      span.textContent = iface.label + (iface.connected ? ' ✓' : ' ✗');
      ifaces.appendChild(span);
      if (i < b.interfaces.length - 1) ifaces.appendChild(document.createTextNode(', '));
    });
    s.appendChild(ifaces);
  }

  const count = el('span', 'count');
  const shown = state.servers.length;
  if (b.heard_total != null) {
    count.innerHTML = 'Heard <b>' + b.heard_total + '</b> · shown <b>' + shown + '</b>';
  } else {
    count.innerHTML = 'Shown <b>' + shown + '</b>';
  }
  s.appendChild(count);

  const btn = el('button', running ? 'ghost' : '');
  btn.textContent = running ? 'Stop' : 'Start';
  btn.onclick = running ? stopBrowse : startBrowse;
  s.appendChild(btn);
}

// ---------- rendering: list ----------
function createRowEl(row) {
  const e = el('div', 'row');
  e.dataset.hash = row.destination_hash;
  e.setAttribute('role', 'option');
  e.setAttribute('aria-selected', 'false');
  e.tabIndex = -1;
  e.innerHTML =
    '<div class="cell col-name"><div class="v" data-cell="name"></div></div>' +
    '<div class="cell col-game"><div class="v" data-cell="game"></div></div>' +
    '<div class="cell col-map"><div class="v" data-cell="map"></div></div>' +
    '<div class="cell col-players"><div class="v" data-cell="players"></div></div>' +
    '<div class="cell col-hops"><div class="v" data-cell="hops"></div><div class="sub" data-cell="iface"></div></div>' +
    '<div class="cell col-seen"><div class="v" data-cell="seen"></div></div>';
  updateRowEl(e, row);
  e.addEventListener('click', () => { setActive(row.destination_hash); openDetail(row.destination_hash); });
  e.addEventListener('focus', () => { state.activeHash = row.destination_hash; });
  return e;
}

function updateRowEl(e, row) {
  const legacy = row.legacy;
  e.classList.toggle('legacy', legacy);

  const nameV = e.querySelector('[data-cell="name"]');
  nameV.textContent = row.name || 'Unnamed server';
  // badges
  nameV.parentElement.querySelectorAll('.badge').forEach(b => b.remove());
  if (legacy) {
    const b = el('span', 'badge legacy-badge', 'legacy');
    nameV.parentElement.appendChild(b);
  } else {
    if (row.passworded === true) nameV.parentElement.appendChild(el('span', 'badge pw', 'pw'));
    if (row.allowlisted === true) nameV.parentElement.appendChild(el('span', 'badge lock', 'list'));
    if (row.dedicated === true) nameV.parentElement.appendChild(el('span', 'badge ded', 'ded'));
  }

  e.querySelector('[data-cell="game"]').textContent = legacy ? 'Unknown' : (row.game_id || 'Unknown');
  e.querySelector('[data-cell="map"]').textContent = legacy ? 'Unknown' : (row.map || 'Unknown');

  const playersCell = e.querySelector('[data-cell="players"]');
  if (legacy || row.players == null) {
    playersCell.textContent = '—';
    playersCell.title = 'Player count unknown — not present in the announce';
  } else {
    const max = row.max_players == null ? '?' : row.max_players;
    playersCell.textContent = row.players + '/' + max;
    playersCell.title = 'Player count from the announce, heard ' + fmtSeen(row.last_seen_secs) + '. Not live.';
  }

  const hopsCell = e.querySelector('[data-cell="hops"]');
  hopsCell.textContent = String(row.hops);
  hopsCell.title = row.hops + (row.hops === 1 ? ' hop' : ' hops') + ' across the mesh';

  const ifaceCell = e.querySelector('[data-cell="iface"]');
  ifaceCell.textContent = row.interface_label ? 'via ' + row.interface_label : '';
  ifaceCell.title = row.interface_label ? 'Heard on interface ' + row.interface_label : '';

  e.querySelector('[data-cell="seen"]').textContent = fmtSeen(row.last_seen_secs);

  e.setAttribute('aria-selected', String(row.destination_hash === state.detail?.hash));
  e.classList.toggle('selected', row.destination_hash === state.detail?.hash);
}

function renderList() {
  const list = $('list');
  const b = state.browse;

  // empty states
  if (!b || !b.running) {
    // Built once, not on every poll. This panel holds the peer-address input,
    // and rebuilding it every two seconds destroyed the element the player was
    // typing into — the caret vanished mid-word, which is what "something takes
    // focus every second" was. The panel's contents do not depend on anything
    // that changes while the node is stopped, so there is nothing to redraw.
    if (!list.querySelector('.empty')) {
      list.innerHTML = '';
      state.rowEls.clear();
      renderEmptyNotRunning();
    }
    return;
  }
  if (state.servers.length === 0) {
    list.innerHTML = '';
    state.rowEls.clear();
    if (b.heard_total === 0) renderEmptyNothingHeard();
    else renderEmptyFiltered();
    return;
  }
  // remove empty placeholder if present
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

  // ensure active row has tabindex 0
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

// The one part of the stopped-state panel that changes: the Start button while
// a start is in flight. Updated in place, because redrawing the panel to move a
// label would take the input with it.
function refreshStartButton() {
  const btn = $('start-browse-btn');
  if (!btn) return;
  btn.disabled = state.startingBrowse;
  btn.textContent = state.startingBrowse ? 'Starting…' : 'Start browse node';
}

function renderEmptyNotRunning() {
  const list = $('list');
  list.innerHTML = '';
  const wrap = el('div', 'empty');
  wrap.appendChild(el('h2', '', 'Browse node is not running'));
  wrap.appendChild(el('p', '', 'Discovery happens passively over the mesh. Start the browse node to listen for server announces on your connected interfaces.'));

  const conn = el('div', 'connect-opts');
  const lbl = el('label', '', 'Connect via');
  lbl.htmlFor = 'f-tcp';
  const tcp = el('input');
  tcp.id = 'f-tcp';
  tcp.type = 'text';
  tcp.placeholder = 'host:port of a TCP peer — leave blank for local network only';
  tcp.value = state.tcpPeer || '';
  tcp.oninput = () => { state.tcpPeer = tcp.value; };
  // Remembered rather than retyped. Reticulum has no directory, so a relay
  // address is something a person had to be told — losing it between runs makes
  // them go and ask again.
  const save = el('button', 'ghost', 'Remember');
  save.type = 'button';
  save.title = 'Save these so this launcher uses them every time';
  save.onclick = async () => {
    save.disabled = true;
    try {
      const addr = (state.tcpPeer || '').trim();
      if (addr) await invoke('add_interface', { kind: 'tcp', addr });
      if (state.autoDiscover) await invoke('add_interface', { kind: 'auto', addr: null });
      await loadInterfaces();
      hideError();
    } catch (e) {
      showError('Could not save that interface: ' + String(e && e.message || e));
    } finally { save.disabled = false; }
  };
  const autoWrap = el('label', 'toggle');
  const auto = el('input');
  auto.type = 'checkbox';
  auto.checked = state.autoDiscover;
  auto.onchange = () => { state.autoDiscover = auto.checked; };
  autoWrap.appendChild(auto);
  autoWrap.appendChild(el('span', '', 'Also discover peers on this network'));
  conn.appendChild(lbl);
  conn.appendChild(tcp);
  conn.appendChild(autoWrap);
  conn.appendChild(save);
  wrap.appendChild(conn);
  wrap.appendChild(renderInterfaceList());

  const btn = el('button', '', 'Start browse node');
  btn.id = 'start-browse-btn';
  btn.onclick = startBrowse;
  btn.disabled = state.startingBrowse;
  btn.textContent = state.startingBrowse ? 'Starting…' : 'Start browse node';
  wrap.appendChild(btn);
  list.appendChild(wrap);
}
function renderEmptyNothingHeard() {
  const list = $('list');
  list.innerHTML = '';
  const wrap = el('div', 'empty');
  wrap.appendChild(el('h2', '', 'No servers heard yet'));
  wrap.appendChild(el('p', '', 'Discovery is passive — your node listens for announces broadcast by other peers. It can take a few announce intervals for servers to appear, especially on a quiet mesh.'));
  wrap.appendChild(el('p', 'muted', 'The list will populate automatically as announces arrive.'));
  list.appendChild(wrap);
}
function renderEmptyFiltered() {
  const list = $('list');
  list.innerHTML = '';
  const wrap = el('div', 'empty');
  wrap.appendChild(el('h2', '', 'No servers match the current filter'));
  wrap.appendChild(el('p', '', state.browse.heard_total + ' server' + (state.browse.heard_total === 1 ? '' : 's') + ' have been heard, but none pass every active filter. Legacy peers (which announce only a name) are excluded by filters on game, tier, players, flags or mode.'));
  const btn = el('button', '', 'Clear filters');
  btn.onclick = clearAllFilters;
  wrap.appendChild(btn);
  list.appendChild(wrap);
}

// ---------- legacy hidden notice ----------
function renderLegacyNotice() {
  const box = $('legacy-notice');
  const show = state.filters.include_legacy && hasMetadataFilter() && state.legacyHiddenCount > 0;
  if (!show) { box.classList.add('hidden'); return; }
  box.innerHTML = '';
  box.appendChild(el('span', '',
    state.legacyHiddenCount + ' legacy server' + (state.legacyHiddenCount === 1 ? '' : 's') +
    ' hidden because they predate the metadata these filters require.'));
  const btn = el('button', '', 'Clear metadata filters');
  btn.onclick = clearMetadataFilters;
  box.appendChild(btn);
  box.classList.remove('hidden');
}

// ---------- detail pane ----------
function openDetail(hash) {
  const row = state.servers.find(s => s.destination_hash === hash);
  if (!row) return;
  state.detail = {
    hash,
    announce: { ...row },
    loading: true,
    data: null,
    error: null,
    joining: false,
    joined: false,
    joinMsg: null,
  };
  renderList();
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
        state.detail.error = String(err && err.message || err);
        state.detail.loading = false;
        renderDetail();
      }
    });
}

function closeDetail() {
  state.detail = null;
  $('detail').hidden = true;
  renderList();
  const list = $('list');
  const active = list.querySelector('.row[tabindex="0"]');
  if (active) active.focus();
}

function renderDetail() {
  const d = state.detail;
  const pane = $('detail');
  if (!d) { pane.hidden = true; return; }

  // This pane re-renders on every poll, and legitimately so: it shows "Last
  // seen: 3s ago", which is different every second. Rebuilding it wholesale
  // therefore threw away the scroll position several times a minute — the
  // local-port field is near the bottom, so scrolling to it bounced straight
  // back to the top — and took the caret with it.
  //
  // Same lesson `renderList` already learned about the peer-address input, and
  // the same fix in the form this pane can take: `.detail-body` is the element
  // that scrolls and it is replaced on each pass, so its offset and the
  // focused control are carried across by hand. Every control this pane builds
  // that a person can land on therefore needs a stable id.
  const prevBody = pane.querySelector('.detail-body');
  const prevScroll = prevBody ? prevBody.scrollTop : 0;
  const active = document.activeElement;
  const keepId = active && active.id && pane.contains(active) ? active.id : null;
  const selStart = keepId && active.selectionStart != null ? active.selectionStart : null;
  const selEnd = keepId && active.selectionEnd != null ? active.selectionEnd : null;

  pane.innerHTML = '';

  // head
  const head = el('div', 'detail-head');
  const titleWrap = el('div');
  titleWrap.appendChild(el('h2', '', d.announce.name || 'Unnamed server'));
  titleWrap.appendChild(el('div', 'hash', d.announce.destination_hash));
  head.appendChild(titleWrap);
  const close = el('button', 'detail-close', '×');
  close.setAttribute('aria-label', 'Close details');
  close.onclick = closeDetail;
  head.appendChild(close);
  pane.appendChild(head);

  // body
  const body = el('div', 'detail-body');
  const a = d.announce;

  const announceSec = el('div', 'section');
  announceSec.appendChild(el('h3', '', 'From the announce'));
  const kv = el('div', 'kv');
  const legacy = a.legacy;
  kvRow(kv, 'Game', legacy ? null : (a.game_id || null), 'Unknown');
  kvRow(kv, 'Map', legacy ? null : (a.map || null), 'Unknown');
  if (legacy || a.players == null) {
    kvRow(kv, 'Players', null, 'Unknown', true);
  } else {
    const max = a.max_players == null ? '?' : a.max_players;
    kvRow(kv, 'Players', a.players + '/' + max + '  (as of ' + fmtSeen(a.last_seen_secs) + ')', null);
  }
  kvRow(kv, 'Mesh hops', String(a.hops) + (a.hops === 1 ? ' hop' : ' hops'), null);
  kvRow(kv, 'Heard on', a.interface_label || null, 'Unknown');
  kvRow(kv, 'Link tier', a.min_link_class == null ? null : LINK_CLASS[a.min_link_class] || ('Tier ' + a.min_link_class), 'Unknown');
  kvRow(kv, 'Last seen', fmtSeen(a.last_seen_secs), null);
  if (legacy) {
    kvRow(kv, 'Type', null, 'Legacy peer');
  } else {
    kvRow(kv, 'Transport', a.transport_mode == null ? null : ('Mode ' + a.transport_mode), 'Unknown');
  }
  announceSec.appendChild(kv);

  if (!legacy) {
    const flags = [];
    if (a.passworded === true) flags.push(['pw', 'Passworded']);
    if (a.allowlisted === true) flags.push(['lock', 'Allowlisted']);
    if (a.dedicated === true) flags.push(['ded', 'Dedicated']);
    if (flags.length) {
      const fw = el('div', 'flags');
      flags.forEach(([cls, txt]) => fw.appendChild(el('span', 'badge ' + cls, txt)));
      announceSec.appendChild(fw);
    }
  }
  if (legacy) {
    const note = el('p', '', 'This is a legacy peer. It announces only a name and predates the richer metadata format, so game, map, player count and flags are genuinely unknown — not zero.');
    note.style.color = 'var(--fg-muted)';
    note.style.fontStyle = 'italic';
    note.style.marginTop = '8px';
    announceSec.appendChild(note);
  }
  body.appendChild(announceSec);

  // live probe
  const probeSec = el('div', 'section');
  probeSec.appendChild(el('h3', '', 'Live probe'));
  if (d.loading) {
    const p = el('div', 'probe-pending');
    p.appendChild(el('span', 'spinner'));
    p.appendChild(el('span', '', 'Opening a direct connection to this server…'));
    probeSec.appendChild(p);
  } else if (d.error) {
    const pe = el('div', 'probe-error');
    pe.appendChild(el('div', 'title', 'No direct response'));
    pe.appendChild(el('div', '', 'The server did not answer a direct probe. This does not mean it is offline — mesh routing can be asymmetric, and an announce may still reach you from a peer even when a direct path back is unavailable. It may still be joinable.'));
    probeSec.appendChild(pe);
  } else if (d.data) {
    const ok = el('div', 'probe-ok');
    const kv2 = el('div', 'kv');
    kvRow(kv2, 'Reachable', d.data.reachable ? 'Yes' : 'No', null);
    if (d.data.rtt_ms != null) {
      kvRow(kv2, 'Probe RTT', d.data.rtt_ms + ' ms', null);
    }
    const live = d.data.stats_source === 'live';
    if (d.data.players_online != null) {
      const max = d.data.max_players == null ? '?' : d.data.max_players;
      kvRow(kv2, live ? 'Players now' : 'Players (configured)',
            d.data.players_online + '/' + max, null);
    } else {
      kvRow(kv2, 'Players', null, 'Unknown', true);
    }
    // The honesty field. "announced" means the server handed back the same
    // static configuration the list row already showed — it could not query the
    // running game — so this must never read as a live number.
    if (d.data.stats_source) {
      kvRow(kv2, 'Figures',
            live
              ? ('read from the game ' + fmtDuration(d.data.stats_age_secs || 0) + ' ago — not this instant')
              : 'from the server\u2019s configuration, not the running game',
            null);
    }
    if (d.data.uptime_secs != null) {
      kvRow(kv2, 'Bridge uptime', fmtDuration(d.data.uptime_secs), null);
    }
    if (d.data.bridge_clients != null) {
      kvRow(kv2, 'Players bridged', String(d.data.bridge_clients), null);
    }
    kvRow(kv2, 'Map', d.data.map || null, 'Unknown');
    if (d.data.error) {
      kvRow(kv2, 'Note', d.data.error, null);
    }
    ok.appendChild(kv2);
    if (d.data.player_names && d.data.player_names.length) {
      const h = el('div', '', '');
      h.style.marginTop = '8px';
      h.style.color = 'var(--fg-muted)';
      h.textContent = 'Players online (' + d.data.player_names.length + '):';
      ok.appendChild(h);
      const ul = el('ul', 'player-list');
      d.data.player_names.forEach(n => ul.appendChild(el('li', '', n)));
      ok.appendChild(ul);
      if (d.data.roster_truncated) {
        const t = el('p', '', 'The list was too long to fit in one response and was cut short.');
        t.style.color = 'var(--fg-muted)';
        t.style.fontStyle = 'italic';
        ok.appendChild(t);
      }
    } else if (live && d.data.player_names && d.data.player_names.length === 0) {
      const ul = el('ul', 'player-list');
      ul.appendChild(el('li', 'legacy-row', 'No players online'));
      ok.appendChild(ul);
    } else if (!live) {
      // No roster is not an empty roster: this server cannot be queried.
      const n = el('p', '', 'This game answers no query protocol, so who is playing is unknown.');
      n.style.color = 'var(--fg-muted)';
      n.style.fontStyle = 'italic';
      ok.appendChild(n);
    }
    probeSec.appendChild(ok);
  }
  body.appendChild(probeSec);

  const gameId = effectiveGameId(d);
  if (!a.game_id) body.appendChild(renderGamePicker(d));
  body.appendChild(renderPackSection(gameId));
  if (gameId) body.appendChild(renderPortSection(d, gameId));

  pane.appendChild(body);

  // foot
  const foot = el('div', 'detail-foot');
  const join = el('button', 'btn-join', 'Join server');
  join.id = 'detail-join';
  join.onclick = joinServer;
  if (d.joining) { join.disabled = true; join.textContent = 'Joining…'; }
  else if (d.joined) { join.textContent = 'Join again'; }
  // No game, no join. Before the picker this button was live on a legacy row
  // and failed in the error line instead of saying what was missing.
  if (!gameId) {
    join.disabled = true;
    join.title = state.games.length
      ? 'Choose which game this server runs first.'
      : 'No game packs are installed, so nothing can be joined.';
  }
  foot.appendChild(join);

  // The Play button (PLAN.md §13.3). It appears only once a join has bound a
  // local port and only for a game whose pack can start it (`can_launch`).
  // When the game cannot yet be found on this machine (`launch_ready` is
  // false), the button becomes "Locate game" so the player points the launcher
  // at their own copy once — the launcher never guesses an executable.
  if (d.joined && d.canLaunch) {
    if (d.launchReady) {
      const play = el('button', 'btn-play', d.playing ? 'Starting…' : 'Play');
      play.disabled = !!d.playing;
      play.onclick = playServer;
      foot.appendChild(play);
    } else {
      const locate = el('button', 'btn-locate', 'Locate game');
      locate.onclick = () => locateGame(effectiveGameId(d));
      foot.appendChild(locate);
    }
  }

  if (d.joinMsg) {
    const m = el('span', 'join-msg ' + (d.joinErr ? 'err' : 'ok'), d.joinMsg);
    foot.appendChild(m);
  }
  pane.appendChild(foot);

  // Put the reader back where they were. Order matters: the scroll offset is
  // restored before focus, because focusing an element the browser considers
  // off-screen scrolls it into view and would undo the line above.
  const scroller = pane.querySelector('.detail-body');
  if (scroller && prevScroll) scroller.scrollTop = prevScroll;
  if (keepId) {
    const again = document.getElementById(keepId);
    if (again) {
      again.focus({ preventScroll: true });
      if (selStart != null && again.setSelectionRange) {
        // A number input refuses selection in some engines, and losing the
        // caret offset is not worth failing the render over.
        try { again.setSelectionRange(selStart, selEnd); } catch (_) { /* ignore */ }
      }
    }
  }
}

// A server whose announce names no game — every deployed v0.1.10 peer, whose
// app_data is a bare name (`PLAN.md` §3.3, §5) — cannot be matched to a pack,
// and a pack is what tells this machine how to talk to it. The launcher must
// not guess one: picking a wire protocol for the player is how a join ends up
// talking nonsense at a server that looked fine.
//
// So the player picks, and the choice is remembered per destination. Until
// they do, Join is disabled rather than failing in the error line, which is
// what it did before: `this server did not say which game it runs`.
function renderGamePicker(d) {
  const sec = el('div', 'section');
  sec.appendChild(el('h3', '', 'Which game is this?'));

  if (!state.games.length) {
    sec.appendChild(el('p', 'pack-none',
      'This server did not say what game it runs, and no game packs are installed, ' +
      'so there is nothing to match it to.'));
    return sec;
  }

  sec.appendChild(el('p', 'pack-detail',
    'This server announces only a name, which is what a pre-0.2 peer does. ' +
    'Pick the game it runs and the launcher will remember it for this server.'));

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

// Which local port this machine binds for the game to connect to.
//
// The pack's default is the port the game's *own* dedicated server listens on,
// so any machine already running one — or Docker publishing one — owns it, and
// the join fails with `Address already in use` on a number the player never
// chose. Making it settable is the fix; showing it is what makes the failure
// legible when it happens.
// Ask the core once per game what port a join would bind. Fire-and-forget: the
// field renders empty until it answers, and answers by re-rendering.
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
  sec.appendChild(el('h3', '', 'Local port'));
  sec.appendChild(el('p', 'pack-detail',
    'Your game connects to this port on this machine. Change it if something ' +
    'else here already uses the default — a dedicated server of the same game, ' +
    'usually.'));

  const row = el('div', 'port-row');
  const input = el('input', 'port-input');
  input.id = 'detail-port';
  input.type = 'number';
  input.min = '1';
  input.max = '65535';
  input.setAttribute('aria-label', 'Local port to bind');
  const current = state.listenPorts.get(gameId);
  input.value = d.portDraft != null ? d.portDraft : (current != null ? String(current) : '');
  input.placeholder = 'the pack default';
  input.addEventListener('input', () => { d.portDraft = input.value; });
  row.appendChild(input);

  const reset = el('button', 'btn-locate', 'Use default');
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
      d.joinMsg = 'Could not reset the port: ' + String(err && err.message || err);
    }
    renderDetail();
  };
  row.appendChild(reset);
  sec.appendChild(row);

  const shown = d.portDraft != null && d.portDraft !== '' ? d.portDraft : current;
  if (shown) {
    sec.appendChild(el('p', 'pack-detail',
      'Point your game at 127.0.0.1:' + shown + ' — the Play button does this for you.'));
  }
  return sec;
}

// PLAN.md §11.4: a pack's provenance is shown at the moment it matters, not
// buried. Here that moment is joining — a pack is what tells this machine how
// to talk to the server, so the tier belongs beside the Join button. The
// launcher shows and never refuses: no code runs here because of a pack.
const TRUST_CLASS = {
  'first-party': 'trust-ok',
  'built in': 'trust-ok',
  'signed community': 'trust-ok',
  'signed by an unknown key': 'trust-warn',
  'unsigned local': 'trust-warn',
};

function renderPackSection(gameId) {
  const sec = el('div', 'section');
  sec.appendChild(el('h3', '', 'Game pack'));
  const pack = gameId ? state.games.find(g => g.id === gameId) : null;

  if (!pack) {
    const p = el('p', 'pack-none');
    p.textContent = gameId
      ? 'You have no pack for ' + gameId + ', so this launcher cannot tell your game where to connect. Install one to join.'
      : 'This server did not say what game it runs, so no pack can be matched to it.';
    sec.appendChild(p);
    return sec;
  }

  const line = el('div', 'pack-line');
  line.appendChild(el('span', 'pack-name', pack.display_name || pack.id));
  line.appendChild(el('span', 'badge ' + (TRUST_CLASS[pack.trust] || 'trust-warn'), pack.trust));
  sec.appendChild(line);
  sec.appendChild(el('p', 'pack-detail', pack.trust_detail || ''));

  if (pack.signer) {
    const kv = el('div', 'kv');
    kvRow(kv, 'Signed by', pack.signer, null);
    if (pack.signature_expires_at != null) {
      // Seconds from now, floored at 0: a signature already past its window
      // would not have loaded at all, but never render a negative countdown.
      const left = Math.max(0, pack.signature_expires_at - Math.floor(Date.now() / 1000));
      kvRow(kv, 'Signature valid for', fmtDuration(left) + ' more', null);
    }
    sec.appendChild(kv);
  }
  return sec;
}

function kvRow(parent, k, v, fallback, unknown) {
  parent.appendChild(el('span', 'k', k));
  if (v == null || v === '') {
    parent.appendChild(el('span', 'v unknown', fallback || 'Unknown'));
  } else if (unknown) {
    parent.appendChild(el('span', 'v unknown', v));
  } else {
    parent.appendChild(el('span', 'v', v));
  }
}

// ---------- actions ----------
// Where to attach. `auto` alone finds peers on the local network; a TCP peer
// is how anyone not on the same LAN reaches the mesh at all, so the UI has to
// offer it rather than assuming a LAN neighbour exists.
function browseOpts() {
  const tcp = (state.tcpPeer || '').trim();
  // What is typed wins; the saved set fills in when nothing is. A player who
  // configured a relay once should not have to retype it to press Start.
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
    showError('Failed to start browse node: ' + String(err && err.message || err));
  } finally {
    state.startingBrowse = false;
    await pollStatus();
    renderStatus();
    renderList();
  }
}
async function stopBrowse() {
  try {
    await invoke('stop_browse');
    hideError();
  } catch (err) {
    showError('Failed to stop browse node: ' + String(err && err.message || err));
  }
  await pollStatus();
  renderStatus();
  renderList();
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
    const res = await invoke('join_server', {
      destinationHash: d.hash,
      gameId,
      listenPort,
    });
    d.joined = true;
    d.listenAddr = res.listen_addr;
    if (listenPort != null) state.listenPorts.set(gameId, listenPort);
    d.canLaunch = !!res.can_launch;
    d.launchReady = !!res.launch_ready;
    d.joinErr = false;
    if (d.canLaunch && d.launchReady) {
      d.joinMsg = 'Connected. Press Play to start the game (listening on ' + res.listen_addr + ').';
    } else if (d.canLaunch) {
      d.joinMsg = 'Connected on ' + res.listen_addr +
                  '. Locate your game once to enable Play, or point your game at that address.';
    } else {
      d.joinMsg = 'Connected. Point your game at ' + res.listen_addr +
                  ' — this pack does not start the game for you.';
    }
  } catch (err) {
    d.joined = false;
    d.joinErr = true;
    d.joinMsg = 'Could not join: ' + String(err && err.message || err);
  } finally {
    d.joining = false;
    renderDetail();
  }
}

// The Play button (PLAN.md §13.3). Everything that decides *how* to start the
// game — the player's own saved binary, or Steam `-applaunch` — is in
// launcher-core; this only asks it to, and reports what it started. The
// arguments are spawned as a vector there, never a shell.
async function playServer() {
  const d = state.detail;
  if (!d || d.playing) return;
  d.playing = true;
  d.joinMsg = null;
  renderDetail();
  try {
    const res = await invoke('play_server');
    d.joinErr = false;
    d.joinMsg = res.method === 'steam'
      ? 'Starting the game through Steam…'
      : 'Starting your game…';
  } catch (err) {
    d.joinErr = true;
    d.joinMsg = 'Could not start the game: ' + String(err && err.message || err);
  } finally {
    d.playing = false;
    renderDetail();
  }
}

// "Locate game": the launcher never guesses an executable, so when a game
// cannot be found automatically the player points it at their own copy once and
// it is remembered (settings.rs). No file-dialog plugin is required — a prompt
// keeps the shell's permission surface unchanged.
async function locateGame(gameId) {
  const d = state.detail;
  if (!d || !gameId) return;
  let hint = '';
  try {
    const loc = await invoke('game_location', { gameId });
    hint = loc && loc.detail ? '\n\n' + loc.detail : '';
  } catch (_) { /* a missing location just means an empty hint */ }
  const path = window.prompt(
    'Enter the full path to your ' + gameId + ' executable' +
    ' (the launcher will remember it):' + hint,
    (d.savedPath || ''));
  if (path == null) return; // cancelled
  const trimmed = path.trim();
  if (trimmed === '') return;
  try {
    await invoke('set_game_path', { gameId, path: trimmed });
    const loc = await invoke('game_location', { gameId });
    d.savedPath = loc.saved_path || trimmed;
    d.launchReady = !!loc.launch_ready;
    d.joinErr = !d.launchReady;
    d.joinMsg = d.launchReady
      ? 'Game located. Press Play to start it.'
      : (loc.detail || 'That path did not work — try again.');
  } catch (err) {
    d.joinErr = true;
    d.joinMsg = 'Could not set the game path: ' + String(err && err.message || err);
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

// ---------- keyboard nav ----------
function onListKey(e) {
  const rows = Array.from($('list').querySelectorAll('.row'));
  if (rows.length === 0) return;
  let idx = rows.findIndex(r => r.dataset.hash === state.activeHash);
  if (e.key === 'ArrowDown') {
    e.preventDefault();
    if (idx < 0) idx = 0; else idx = Math.min(rows.length - 1, idx + 1);
    focusRow(rows[idx]);
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    if (idx < 0) idx = 0; else idx = Math.max(0, idx - 1);
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
    showError('browse_status failed: ' + String(err && err.message || err));
  }
}
async function pollServers() {
  try {
    const rows = await invoke('list_servers', { query: buildQuery() });
    state.servers = Array.isArray(rows) ? rows : [];
    hideError();
  } catch (err) {
    state.servers = [];
    showError('list_servers failed: ' + String(err && err.message || err));
  }
  // legacy probe only when needed
  state.legacyHiddenCount = 0;
  if (state.filters.include_legacy && hasMetadataFilter()) {
    try {
      const legacyRows = await invoke('list_servers', { query: buildLegacyProbeQuery() });
      state.legacyHiddenCount = (legacyRows || []).filter(r => r.legacy).length;
    } catch (e) { /* ignore secondary failure */ }
  } else if (state.filters.include_legacy && !hasMetadataFilter()) {
    state.legacyHiddenCount = state.servers.filter(r => r.legacy).length;
  }
  renderStatus();
  renderList();
  renderLegacyNotice();
  // refresh detail announce if open
  if (state.detail) {
    const fresh = state.servers.find(s => s.destination_hash === state.detail.hash);
    if (fresh) { state.detail.announce = { ...fresh }; renderDetail(); }
  }
}
async function pollAll() {
  await pollStatus();
  await pollServers();
}

// ---------- games ----------
async function loadGames() {
  try {
    state.games = await invoke('list_games') || [];
  } catch (err) {
    state.games = [];
    showError('list_games failed: ' + String(err && err.message || err));
  }
  const sel = $('f-game');
  sel.innerHTML = '';
  sel.appendChild(el('option', '', 'Any game'));
  state.games.forEach(g => {
    const o = el('option', '', g.display_name || g.id);
    o.value = g.id;
    sel.appendChild(o);
  });
}

// ---------- filter UI sync ----------
function syncFilterUI() {
  $('f-text').value = state.filters.text;
  $('f-game').value = state.filters.game_id || '';
  $('f-players').checked = state.filters.has_players;
  $('f-notfull').checked = state.filters.not_full;
  $('f-pw').checked = state.filters.exclude_passworded;
  $('f-dedicated').checked = state.filters.dedicated_only;
  $('f-legacy').checked = state.filters.include_legacy;
  $('f-maxhops').value = state.filters.max_hops == null ? '' : state.filters.max_hops;
}
function bindFilters() {
  $('f-text').addEventListener('input', e => { state.filters.text = e.target.value; schedulePoll(); });
  $('f-game').addEventListener('change', e => { state.filters.game_id = e.target.value || null; pollServers(); });
  $('f-players').addEventListener('change', e => { state.filters.has_players = e.target.checked; pollServers(); });
  $('f-notfull').addEventListener('change', e => { state.filters.not_full = e.target.checked; pollServers(); });
  $('f-pw').addEventListener('change', e => { state.filters.exclude_passworded = e.target.checked; pollServers(); });
  $('f-dedicated').addEventListener('change', e => { state.filters.dedicated_only = e.target.checked; pollServers(); });
  $('f-legacy').addEventListener('change', e => { state.filters.include_legacy = e.target.checked; pollServers(); });
  $('f-maxhops').addEventListener('input', e => {
    const v = e.target.value.trim();
    if (v === '') state.filters.max_hops = null;
    else { const n = parseInt(v, 10); state.filters.max_hops = isNaN(n) || n < 0 ? null : n; }
    schedulePoll();
  });
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
      if (state.sort.sort === key) {
        state.sort.descending = !state.sort.descending;
      } else {
        state.sort.sort = key;
        state.sort.descending = false;
      }
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
      btn.dataset.sort === state.sort.sort
        ? (state.sort.descending ? 'descending' : 'ascending')
        : 'none');
  });
}

// ---------- init ----------
async function init() {
  bindFilters();
  bindSort();
  syncFilterUI();
  $('list').addEventListener('keydown', onListKey);
  document.addEventListener('keydown', e => {
    if (e.key === 'Escape' && state.detail && document.activeElement?.closest('.detail')) {
      e.preventDefault(); closeDetail();
    }
  });
  await loadGames();
  // The saved interfaces, so pressing Start uses what was configured rather
  // than asking the player to retype a relay address they were given once.
  try {
    state.interfaces = await invoke('list_interfaces') || [];
    state.savedOpts = await invoke('saved_browse_opts') || {};
    if (state.savedOpts.tcp && !state.tcpPeer) state.tcpPeer = state.savedOpts.tcp;
    if (state.savedOpts.auto) state.autoDiscover = true;
  } catch (_) { /* an older backend simply has none */ }
  await pollAll();
  state.pollTimer = setInterval(pollAll, 2000);
}
init().catch(err => showError('Init failed: ' + String(err && err.message || err)));


// ---------- saved mesh interfaces ----------
//
// How this launcher reaches the mesh, kept between runs. Applied when the
// browse node starts: the engine cannot add an interface to a node that is
// already running, so a change here takes effect on the next start rather than
// pretending to act immediately.

async function loadInterfaces() {
  try {
    state.interfaces = await invoke('list_interfaces') || [];
  } catch (err) {
    state.interfaces = [];
  }
  const host = $('iface-list');
  if (host) host.replaceWith(renderInterfaceList());
}

function renderInterfaceList() {
  const wrap = el('div', 'iface-list');
  wrap.id = 'iface-list';
  const saved = state.interfaces || [];
  if (!saved.length) {
    const p = el('p', 'iface-empty', 'No saved connections yet. Enter a peer address or tick local discovery, then press Remember.');
    wrap.appendChild(p);
    return wrap;
  }
  wrap.appendChild(el('div', 'iface-title', 'Saved connections — used every time this launcher starts'));
  saved.forEach(i => {
    const row = el('div', 'iface-row');
    row.appendChild(el('span', 'iface-label', i.label));
    const del = el('button', 'ghost', 'Forget');
    del.type = 'button';
    del.onclick = async () => {
      del.disabled = true;
      try {
        await invoke('remove_interface', { id: i.id });
        await loadInterfaces();
      } catch (e) {
        showError('Could not forget that: ' + String(e && e.message || e));
      } finally { del.disabled = false; }
    };
    row.appendChild(del);
    wrap.appendChild(row);
  });
  return wrap;
}
