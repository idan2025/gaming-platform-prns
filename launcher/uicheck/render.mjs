// Headless render pass over launcher/dist: real DOM (jsdom), mocked Tauri
// bridge, payloads shaped exactly like launcher-core serializes them.
//
// This is not a screenshot. It catches the class of bug the frontend contract
// actually has: a property that does not exist reads as undefined, and a null
// that must render as "unknown" rendering as 0.
import { JSDOM, VirtualConsole } from 'jsdom';
import fs from 'node:fs';

const DIST = new URL('../dist/', import.meta.url).pathname;
// The stylesheet is inlined so jsdom actually applies it: a `display` rule can
// defeat the `hidden` attribute, which is how the detail pane once held a third
// of the window while "closed".
const css = fs.readFileSync(`${DIST}style.css`, 'utf8');
const html = fs
  .readFileSync(`${DIST}index.html`, 'utf8')
  .replace('<link rel="stylesheet" href="style.css">', `<style>${css}</style>`);
const appJs = fs.readFileSync(`${DIST}app.js`, 'utf8');

const failures = [];
const consoleErrors = [];

function check(name, cond, detail = '') {
  if (!cond) failures.push(`${name}${detail ? ': ' + detail : ''}`);
}

const row = (over = {}) => ({
  destination_hash: 'aabbccddeeff00112233445566778899',
  name: 'Idan\'s Server',
  game_id: 'sven-coop',
  map: 'svencoop1',
  players: 4,
  max_players: 32,
  hops: 1,
  interface_label: 'tcp/127.0.0.1:4242',
  min_link_class: 1,
  passworded: false,
  allowlisted: false,
  dedicated: true,
  transport_mode: 0,
  last_seen_secs: 3,
  legacy: false,
  ...over,
});

// A deployed v0.1.10 peer: a name and nothing else. Every optional field null.
const legacyRow = row({
  destination_hash: '99887766554433221100ffeeddccbbaa',
  name: 'sc-rns-bridge',
  game_id: null, map: null, players: null, max_players: null,
  min_link_class: null, passworded: null, allowlisted: null,
  dedicated: null, transport_mode: null, legacy: true, hops: 3,
});

// A row the launcher remembers rather than heard. The mesh announces a server
// once and then stops repeating it, so this is how an already-running server
// gets into the list at all.
const rememberedRow = row({
  destination_hash: '1021110900000000000000000000beef',
  name: 'Test SErver',
  game_id: 'sven-coop',
  map: null, players: null, max_players: null,
  min_link_class: null, passworded: null, allowlisted: null,
  dedicated: null, transport_mode: null,
  hops: 0, legacy: false, remembered: true, last_seen_secs: 900,
});

const details = {
  destination_hash: row().destination_hash,
  reachable: true,
  rtt_ms: 84,
  players_online: 4,
  max_players: 32,
  player_names: ['a', 'b'],
  roster_truncated: false,
  map: 'svencoop1',
  uptime_secs: 7200,
  bridge_clients: 2,
  stats_source: 'live',
  stats_age_secs: 1,
  error: null,
};

const calls = [];
function makeInvoke(scenario) {
  return async (cmd, args) => {
    calls.push(cmd);
    switch (cmd) {
      case 'browse_status':
        return scenario.status;
      case 'list_servers':
        return scenario.rows(args?.query ?? {});
      case 'list_games':
        return scenario.games ?? [{
          id: 'sven-coop',
          display_name: 'Sven Co-op',
          trust: 'built in',
          trust_detail: 'shipped inside the program you are running, and exactly as trustworthy as it is.',
          signer: null,
          signature_expires_at: null,
        }];
      case 'list_interfaces':
        return scenario.interfaces ?? [];
      case 'saved_browse_opts':
        return scenario.savedOpts ?? { tcp: null, auto: false };
      case 'add_interface':
      case 'remove_interface':
        return null;
      case 'server_details':
        return scenario.details ?? details;
      case 'start_browse':
      case 'stop_browse':
      case 'leave':
        return null;
      case 'join_server':
        return scenario.join ?? { listen_addr: '127.0.0.1:27015', game_id: 'sven-coop', reachable: true };
      case 'listen_port':
        return 27015;
      case 'indexes':
        return scenario.indexes ?? [];
      case 'add_index':
      case 'remove_index':
        return null;
      case 'known_servers':
        return scenario.known ?? [];
      case 'refresh_known_servers':
        return (scenario.known ?? []).length;
      case 'forget_server':
        return null;
      case 'clear_listen_port':
        return null;
      default:
        throw new Error(`the UI called an unknown command: ${cmd}`);
    }
  };
}

async function run(label, scenario, assertions) {
  const virtualConsole = new VirtualConsole();
  virtualConsole.on('jsdomError', e => consoleErrors.push(`${label}: ${e.message}`));
  virtualConsole.on('error', (...a) => consoleErrors.push(`${label}: ${a.join(' ')}`));

  const dom = new JSDOM(html, { runScripts: 'outside-only', pretendToBeVisual: true, virtualConsole });
  const { window } = dom;
  window.__TAURI__ = { core: { invoke: makeInvoke(scenario) } };
  window.eval(appJs);
  // init() runs a few awaits deep; let the microtask queue drain.
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  await assertions(window, window.document);
  window.clearInterval?.(undefined);
  dom.window.close();
}

const running = { running: true, interfaces: [{ id: '1', label: 'tcp/127.0.0.1:4242', connected: true }], heard_total: 2 };

await run('closed panes take no space', {
  status: running,
  rows: () => [row()],
}, (win, doc) => {
  // `.detail { display: flex }` is an author rule and beats the UA's
  // `[hidden] { display: none }`, so the attribute alone is not enough.
  const detail = doc.querySelector('#detail');
  check('the detail pane is hidden before anything is selected', detail.hidden);
  check(
    'a hidden detail pane is display:none, not an empty column',
    win.getComputedStyle(detail).display === 'none',
    win.getComputedStyle(detail).display
  );
});

await run('two servers', {
  status: running,
  rows: () => [row(), legacyRow],
}, (win, doc) => {
  const rows = doc.querySelectorAll('#list .row');
  check('list renders a row per server', rows.length === 2, `got ${rows.length}`);

  const text = doc.querySelector('#list').textContent;
  check('the named server is shown', text.includes("Idan's Server"));
  check('the map is shown', text.includes('svencoop1'));
  check('a known player count renders', text.includes('4/32'));

  // The rule from launcher-core: unknown must not render as zero.
  const legacyEl = rows[1];
  const players = legacyEl.querySelector('[data-cell="players"]').textContent;
  check('a legacy row shows unknown players as a dash, not 0', players === '—', `got ${JSON.stringify(players)}`);
  const game = legacyEl.querySelector('[data-cell="game"]').textContent;
  check('a legacy row shows its game as Unknown', game === 'Unknown', `got ${JSON.stringify(game)}`);
  check('a legacy row is badged', legacyEl.textContent.includes('legacy'));
  check('no undefined leaked into the list', !text.includes('undefined'), text.slice(0, 200));
});

await run('detail pane', {
  status: running,
  rows: () => [row()],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const pane = doc.querySelector('#detail');
  check('the detail pane opens on click', !pane.hidden);
  const t = pane.textContent;
  check('the detail pane shows the live source', /live/i.test(t), t.slice(0, 300));
  check('the detail pane shows an rtt', t.includes('84'), t.slice(0, 300));
  check('no undefined leaked into the detail pane', !t.includes('undefined'), t.slice(0, 300));
});

// A poll re-renders the detail pane every few seconds — it shows "Last seen:
// 3s ago", so it has to. What it must not do is throw away where the reader was
// and what they were typing in, which is what made the local-port field
// impossible to use: scrolling down to it bounced back to the top.
await run('the detail pane survives a re-render', {
  status: running,
  rows: () => [row()],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));

  const port = doc.querySelector('#detail-port');
  check('the detail pane offers a local port field', !!port);
  check('the local port field has a stable id to be restored by', port.id === 'detail-port');
  check('the local port is prefilled from the core', port.value === '27015', `got ${JSON.stringify(port.value)}`);

  port.focus();
  port.value = '270';
  port.dispatchEvent(new win.Event('input', { bubbles: true }));
  check('the field has focus before the re-render', doc.activeElement.id === 'detail-port');

  // What a poll does.
  win.eval('renderDetail()');

  const after = doc.querySelector('#detail-port');
  check('the field still has focus after a re-render',
    doc.activeElement && doc.activeElement.id === 'detail-port',
    `activeElement is ${doc.activeElement && doc.activeElement.id}`);
  check('a half-typed port is not thrown away by a re-render',
    after.value === '270', `got ${JSON.stringify(after.value)}`);
  check('the scroll container is present to be restored',
    !!doc.querySelector('#detail .detail-body'));
});

// A remembered row must be visibly different from a live one. It is joinable —
// a destination hash is all a join needs — but nothing about it is current, and
// rendering a stale player count as a live one is the single thing a server
// browser must not do.
await run('a remembered server is marked as memory, not as live', {
  status: running,
  rows: () => [rememberedRow],
}, async (win, doc) => {
  const el = doc.querySelector('#list .row');
  check('a remembered server still appears in the list', !!el);
  check('the row is marked as remembered', el.classList.contains('remembered'));
  check('the row carries a badge saying so', el.textContent.includes('remembered'));

  const players = el.querySelector('[data-cell="players"]').textContent;
  check('unknown players render as a dash, never a stale number',
    players === '—', `got ${JSON.stringify(players)}`);
  // "Unknown" is the list's existing word for a field it does not have, and it
  // is the honest one here: the launcher knows the server existed, not what it
  // is running now.
  const map = el.querySelector('[data-cell="map"]').textContent;
  check('the map is not presented as known', map === 'Unknown' || map === '—',
    `got ${JSON.stringify(map)}`);

  el.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const pane = doc.querySelector('#detail');
  check('the detail pane explains that it is from memory', /Remembered/i.test(pane.textContent));
  check('and offers to forget it', /Forget this server/i.test(pane.textContent));
  check('and offers to look for it now', /Look for it now/i.test(pane.textContent));
  check('no undefined in a remembered pane', !pane.textContent.includes('undefined'));
});

// Binding a local port always succeeds. If nobody could route to the server,
// the launcher has to say so — otherwise the game sits on "establishing
// connection" while the launcher claims success, which is exactly what a stale
// address looks like.
await run('a join that cannot reach the server says so', {
  status: running,
  rows: () => [row()],
  join: { listen_addr: '127.0.0.1:27015', game_id: 'sven-coop', reachable: false },
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  doc.querySelector('#detail .btn-join').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const t = doc.querySelector('#detail').textContent;
  check('an unroutable join is not reported as success', /did not answer/i.test(t), t.slice(-300));
  check('and names the likely cause', /address may have changed/i.test(t), t.slice(-300));
  check('no undefined in the warning', !t.includes('undefined'));
});

// An index row is somebody else's sighting. Real numbers, but second-hand, and
// the list must show which is which — an index is a cache of the mesh, never
// the source of truth.
await run('an index row is marked as second-hand', {
  status: running,
  indexes: ['aa'.repeat(16)],
  rows: () => [row({
    destination_hash: 'cafebabe00000000000000000000feed',
    name: 'Someone Else\u2019s Server',
    from_index: true, hops: 4,
  })],
}, async (win, doc) => {
  const el = doc.querySelector('#list .row');
  check('an index row appears in the list', !!el);
  check('it is badged as coming via an index', /via index/i.test(el.textContent));
  check('no undefined in an index row', !el.textContent.includes('undefined'));
  const panel = doc.querySelector('#index-panel');
  check('the launcher offers an index panel', !!panel);
  check('and says an index is optional', /works without one/i.test(panel.textContent));
});

await run('unreachable server', {
  status: running,
  rows: () => [row()],
  details: { ...details, reachable: false, rtt_ms: null, players_online: null,
             player_names: null, uptime_secs: null, bridge_clients: null,
             stats_source: 'announced', stats_age_secs: null, error: 'no answer' },
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const pane = doc.querySelector('#detail');
  const t = pane.textContent;
  check('a probe that did not answer is a state, not an error banner',
    doc.querySelector('#error').classList.contains('hidden'), doc.querySelector('#error').textContent);
  check('the pane says the numbers are announced, not live', /announce/i.test(t), t.slice(0, 300));
  check('no undefined in an unreachable pane', !t.includes('undefined'), t.slice(0, 300));
});

await run('join', {
  status: running,
  rows: () => [row()],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const join = doc.querySelector('#detail .btn-join');
  check('the detail pane offers a join button', !!join);
  join.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const t = doc.querySelector('#detail').textContent;
  // The launcher does not launch the game (a pack cannot name a command), so
  // the address it hands back is the whole product of a join.
  check('a join tells the player where to point their game', t.includes('127.0.0.1:27015'), t.slice(-300));
  check('no undefined after a join', !t.includes('undefined'), t.slice(-300));
});

// PLAN.md §11.4: the tier is shown, not buried. These three scenarios are the
// three answers a user can get — vouched for, nobody vouched for it, and no
// pack at all — and the last is the one that reads as undefined if the frontend
// ever assumes a pack exists for every announced game.
await run('pack provenance: a signed pack names its signer', {
  status: running,
  rows: () => [row()],
  games: [{
    id: 'sven-coop',
    display_name: 'Sven Co-op',
    trust: 'signed community',
    trust_detail: "signed by a key this node's operator trusts.",
    signer: 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6',
    signature_expires_at: Math.floor(Date.now() / 1000) + 7200,
  }],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const t = doc.querySelector('#detail').textContent;
  check('the pane shows the pack tier', t.includes('signed community'), t.slice(-400));
  check('the pane names the signing key', t.includes('a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6'), t.slice(-400));
  check('the pane counts down to the signature going stale', /valid for/i.test(t), t.slice(-400));
  check('no undefined in the pack section', !t.includes('undefined'), t.slice(-400));
});

await run('pack provenance: an unsigned pack is shown as such, not hidden', {
  status: running,
  rows: () => [row()],
  games: [{
    id: 'sven-coop',
    display_name: 'Sven Co-op',
    trust: 'unsigned local',
    trust_detail: 'nobody signed this; it is a file someone wrote.',
    signer: null,
    signature_expires_at: null,
  }],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const pane = doc.querySelector('#detail');
  const t = pane.textContent;
  check('an unsigned pack is labelled rather than omitted', t.includes('unsigned local'), t.slice(-400));
  check('an unsigned pack is badged as unvouched, not as trusted',
    !!pane.querySelector('.badge.trust-warn'), t.slice(-400));
  check('a pack with no signer shows no signer row', !/Signed by/i.test(t), t.slice(-400));
  check('no undefined for a pack with null signer fields', !t.includes('undefined'), t.slice(-400));
});

await run('pack provenance: no pack for this game', {
  status: running,
  rows: () => [row({ game_id: 'quake-3' })],
  games: [],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const t = doc.querySelector('#detail').textContent;
  check('a game with no installed pack says so', /no pack for quake-3/i.test(t), t.slice(-400));
  check('no undefined when no pack matches', !t.includes('undefined'), t.slice(-400));
});

// Reticulum has no directory, so a saved relay address is knowledge the player
// was given. It has to be visible and removable, not just remembered.
await run('saved mesh connections are shown', {
  status: { running: false, interfaces: [], heard_total: 0 },
  rows: () => [],
  interfaces: [
    { id: 'tcp:hub.example.org:4789', label: 'hub.example.org:4789', kind: 'tcp' },
    { id: 'auto', label: 'LAN auto-discovery', kind: 'auto' },
  ],
  savedOpts: { tcp: 'hub.example.org:4789', auto: true },
}, (win, doc) => {
  const t = doc.body.textContent;
  check('a saved peer address is shown', t.includes('hub.example.org:4789'), t.slice(0, 300));
  check('a saved auto interface is shown', t.includes('LAN auto-discovery'), t.slice(0, 300));
  check('saved connections can be forgotten', /Forget/.test(t));
  check('no undefined in the interface list', !t.includes('undefined'), t.slice(0, 300));
});

await run('nothing heard yet', {
  status: { running: true, interfaces: [], heard_total: 0 },
  rows: () => [],
}, (win, doc) => {
  const t = doc.querySelector('#list').textContent;
  check('an empty list explains itself', t.trim().length > 0);
  check('no undefined in the empty state', !t.includes('undefined'), t.slice(0, 200));
});

await run('browse not running', {
  status: { running: false, interfaces: [], heard_total: 0 },
  rows: () => [],
}, (win, doc) => {
  const t = doc.querySelector('#list').textContent;
  check('a stopped browser explains how to start', t.trim().length > 0, t.slice(0, 200));
});

for (const e of consoleErrors) failures.push(`uncaught: ${e}`);

if (failures.length) {
  console.log('FAIL');
  for (const f of failures) console.log('  - ' + f);
  process.exit(1);
}
console.log(`OK — ${new Set(calls).size} commands exercised: ${[...new Set(calls)].join(', ')}`);
