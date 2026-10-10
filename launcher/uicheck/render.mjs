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
// The arguments of the latest call of each command, for checking what the UI
// sent and not only that it sent something.
const lastArgs = new Map();
function makeInvoke(scenario) {
  return async (cmd, args) => {
    calls.push(cmd);
    lastArgs.set(cmd, args);
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
      case 'app_version':
        // A shell too old to carry the command throws, exactly as Tauri does
        // for an unknown command; the UI has to survive that.
        if (scenario.noVersionCommand) throw new Error('unknown command app_version');
        return scenario.version ?? '9.9.9';
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
      case 'take_invite': {
        const inv = scenario.invite ?? null;
        scenario.invite = null;
        return inv;
      }
      case 'parse_invite':
        if (!/^lanthorn:\/\/join\//.test(args.text)) throw new Error('not an invite');
        return { destination_hash: args.text.slice(16, 48), game_id: null, name: null, room: false };
      case 'invite_link':
        return `lanthorn://join/${args.destinationHash}${args.room ? '?room=1' : ''}`;
      case 'auto_update':
        return scenario.autoUpdate ?? false;
      case 'set_auto_update':
        return null;
      case 'check_update':
        return scenario.update ?? { current: '1.0.0', available: null, notes: null, install: 'in-place', page: 'x', auto_check: true };
      case 'install_update':
      case 'open_release_page':
        return null;
      case 'trace_path':
        return { destination_hash: args.destinationHash, found: true, hops: 2, millis: 140, error: null };
      case 'announce_room':
        return !!(scenario.room && scenario.room.active && scenario.room.role === 'host');
      case 'clear_listen_port':
        return null;
      // Mode 3 rooms. A scenario without `lanHelper` is a shell that predates
      // them, and those commands throw exactly as Tauri does for an unknown one.
      case 'lan_helper':
      case 'grant_lan_helper':
      case 'revoke_lan_helper':
        if (!scenario.lanHelper) throw new Error(`unknown command ${cmd}`);
        if (cmd === 'revoke_lan_helper') scenario.lanHelper = scenario.afterRevoke ?? scenario.lanHelper;
        return scenario.lanHelper;
      case 'room_status':
        if (!scenario.lanHelper) throw new Error(`unknown command ${cmd}`);
        return scenario.room ?? noRoom;
      case 'host_room':
      case 'join_room':
        scenario.room = scenario.roomAfter;
        return scenario.roomAfter;
      case 'leave_room':
        scenario.room = noRoom;
        return null;
      case 'check_room':
        if (!scenario.roomCheck) throw new Error(`unknown command ${cmd}`);
        return scenario.roomCheck;
      case 'fix_room_firewall':
      case 'unblock_room_programs':
      case 'allow_room_ports':
      case 'reset_room_ports':
      case 'set_room_wide_open':
        if (scenario.firewallFails) throw new Error(scenario.firewallFails);
        if (!scenario.roomAfterFix) throw new Error(`unknown command ${cmd}`);
        scenario.room = scenario.roomAfterFix;
        return scenario.roomAfterFix;
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
  window.GPP_UPDATE_CHECK_DELAY_MS = 0;
  scenario.beforeApp?.(window);
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

// The game filter is built from the loaded packs, and its first entry clears
// the filter. An <option> with no `value` attribute reports its own text as its
// value, so this one once read back as the string "Any game" and was sent to
// the core as a game id nothing matches: picking it emptied the list instead of
// showing everything.
{
  const queries = [];
  await run('the game filter lists every pack and can be cleared', {
    status: running,
    games: [
      { id: 'sven-coop', display_name: 'Sven Co-op', trust: 'built in', trust_detail: '', signer: null, signature_expires_at: null },
      { id: 'counter-strike-16', display_name: 'Counter-Strike 1.6', trust: 'unsigned', trust_detail: '', signer: null, signature_expires_at: null },
      { id: 'half-life', display_name: 'Half-Life Deathmatch', trust: 'unsigned', trust_detail: '', signer: null, signature_expires_at: null },
    ],
    rows: q => { queries.push(q); return [row()]; },
  }, async (win, doc) => {
    const sel = doc.querySelector('#f-game');
    const opts = [...sel.options];
    check('every loaded pack is offered as a filter',
      ['sven-coop', 'counter-strike-16', 'half-life'].every(id => opts.some(o => o.value === id)),
      opts.map(o => `${o.value}=${o.textContent}`).join(', '));
    check('the first option clears the filter and has an empty value',
      opts[0].value === '', JSON.stringify(opts[0].value));

    // Pick a game, then go back to "any" and check what the core was asked.
    sel.value = 'counter-strike-16';
    sel.dispatchEvent(new win.Event('change', { bubbles: true }));
    for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
    // A poll with a metadata filter on also issues a second, unfiltered query
    // to find legacy peers, so look for the filtered one rather than the last.
    check('picking a game filters by its id',
      queries.some(q => q.game_id === 'counter-strike-16'),
      JSON.stringify(queries.map(q => q.game_id)));

    sel.value = '';
    sel.dispatchEvent(new win.Event('change', { bubbles: true }));
    for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
    const last = queries[queries.length - 1];
    check('clearing it asks for every game, not for a game called "Any game"',
      last.game_id === null || last.game_id === undefined, JSON.stringify(last));
    check('and the list still has rows', doc.querySelectorAll('#list .row').length > 0);
  });
}

// The build a player is running has to be on screen: it is the first thing
// anyone asks about a bug report, and the last thing a player can find out.
await run('the launcher shows which build it is', {
  status: running,
  version: '1.2.3',
  rows: () => [row()],
}, (win, doc) => {
  const chip = doc.querySelector('#build-version');
  check('there is a version chip', !!chip);
  check('it names the build', chip.textContent === 'v1.2.3', JSON.stringify(chip?.textContent));
  check('it is out of the way, not in the flow',
    win.getComputedStyle(chip).position === 'fixed', win.getComputedStyle(chip).position);
});

// A launcher shell built before `app_version` existed throws on the call. An
// empty corner is right; a corner reading "vundefined" is not.
await run('an older shell leaves the chip blank rather than lying', {
  status: running,
  rows: () => [row()],
  noVersionCommand: true,
}, (win, doc) => {
  const chip = doc.querySelector('#build-version');
  check('the chip is empty when the backend has no version command',
    chip.textContent === '', JSON.stringify(chip.textContent));
  check('and the failure is not shown as an error banner',
    doc.querySelector('#error').classList.contains('hidden'));
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

// ---------- Mode 3 LAN rooms (PLAN.md §14, step 5) ----------

const settle = async () => { for (let i = 0; i < 30; i++) await new Promise(r => setTimeout(r, 0)); };

const noRoom = {
  active: false, role: null, game_id: null, name: null, room_hash: null, address: null,
  subnet: null, members: [], adapter: 'none', error: null, refused: null,
  firewall: { dropped: [], advice: null, heads_up: null, command: null, can_fix: false, blocking_programs: [], other_firewalls: [] },
  pack_gaps: { ports: [], seen: [], report: null, can_allow: [], allowed: [], wide_open: false },
};

const lanGame = (lan) => ({
  id: 'openttd', display_name: 'OpenTTD', trust: 'unsigned local', trust_detail: 'x',
  signer: null, signature_expires_at: null,
  lan: { tested: false, inbound_any: false, ports: ['udp/3979', 'tcp/3979'], ...lan },
});
const svenGame = {
  id: 'sven-coop', display_name: 'Sven Co-op', trust: 'built in', trust_detail: 'x',
  signer: null, signature_expires_at: null, lan: null,
};

const roomRow = row({
  destination_hash: 'feedfacefeedfacefeedfacefeedface',
  name: 'OpenTTD night', game_id: 'openttd', map: null, players: 2, max_players: 8,
  dedicated: false, transport_mode: 3,
});

const readyHelper = {
  supported: true, path: '/usr/bin/lan-helper', ready: true, mode: 'granted', can_grant: false, can_revoke: false,
  detail: 'Ready.',
};
const perRoom = {
  supported: true, path: '/usr/bin/lan-helper', ready: true, mode: 'per-room', can_grant: true, can_revoke: false,
  detail: 'Ready. Your password is asked each time a room starts, and nothing is installed or left behind. Grant the permission once to stop being asked.',
};
const granted = { ...readyHelper, can_revoke: true, detail: 'Ready. lan-helper has its network permission, so rooms start without asking.' };
const needsGrant = {
  supported: true, path: '/usr/bin/lan-helper', ready: false, mode: 'none', can_grant: false, can_revoke: false,
  detail: 'LAN rooms need a way to ask for your password (polkit\u2019s pkexec), or a permission granted once: sudo setcap cap_net_admin+ep /usr/bin/lan-helper',
};

const memberRoom = {
  active: true, role: 'member', game_id: 'openttd', name: null, room_hash: roomRow.destination_hash,
  address: '198.19.4.2', subnet: '198.19.0.0/16',
  members: [{ address: '198.19.1.1', is_self: false }, { address: '198.19.4.2', is_self: true }],
  adapter: 'up', error: null, refused: null,
  firewall: { dropped: [], advice: null, heads_up: null, command: null, can_fix: false, blocking_programs: [], other_firewalls: [] },
  pack_gaps: { ports: [], seen: [], report: null, can_allow: [], allowed: [], wide_open: false },
};
const fw = (over) => ({ ...memberRoom.firewall, ...over });

await run('a LAN room is a room, not a server', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: readyHelper,
  roomAfter: memberRoom,
  rows: () => [row(), roomRow],
}, async (win, doc) => {
  const rows = doc.querySelectorAll('#list .row');
  const roomEl = [...rows].find(r => r.dataset.hash === roomRow.destination_hash);
  check('a room row is badged as a LAN room', roomEl && roomEl.textContent.includes('LAN room'));
  check('a server row is not', !rows[0].textContent.includes('LAN room'));

  const probesBefore = calls.filter(c => c === 'server_details').length;
  roomEl.dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  check('opening a room sends no detail probe it could only fail',
    calls.filter(c => c === 'server_details').length === probesBefore);

  const pane = doc.querySelector('#detail');
  const t = pane.textContent;
  const join = doc.querySelector('#detail-join');
  check('a room is joined as a room', join && join.textContent === 'Join room', join?.textContent);
  check('and it can be, with the helper ready', join && !join.disabled);
  check('a room offers no local port — the game talks to the adapter', !doc.querySelector('#detail-port'));
  check('the game\u2019s own ports are named', t.includes('udp/3979'), t.slice(0, 400));
  check('an untested game says so before joining', /untested/.test(t), t.slice(0, 400));
  check('no undefined leaked into a room pane', !t.includes('undefined'), t.slice(0, 400));

  join.click();
  await settle();
  const args = lastArgs.get('join_room');
  check('join_room is sent the room and the game',
    args && args.destinationHash === roomRow.destination_hash && args.gameId === 'openttd', JSON.stringify(args));
  check('join_server is not what a room join calls', !calls.includes('join_server') || lastArgs.get('join_server')?.destinationHash !== roomRow.destination_hash);

  const banner = doc.querySelector('#room-banner');
  check('being in a room shows a banner', banner && !banner.classList.contains('hidden'));
  check('the banner says where this machine is in it', banner.textContent.includes('198.19.4.2'), banner.textContent);
  check('the button now says so', doc.querySelector('#detail-join').textContent === 'In this room');

  const leave = doc.querySelector('#room-banner-leave');
  check('the banner offers to leave', !!leave);
  leave?.click();
  await settle();
  check('leaving takes the banner away', doc.querySelector('#room-banner').classList.contains('hidden'));
});

await run('a room whose game lets everything in warns before joining', {
  status: running,
  games: [lanGame({ inbound_any: true, ports: [] })],
  lanHelper: readyHelper,
  rows: () => [roomRow],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  const t = doc.querySelector('#detail').textContent;
  check('the every-port warning is shown', t.includes('every network service'), t.slice(0, 400));
});

await run('a room cannot be joined until the helper may make an adapter', {
  status: running,
  games: [lanGame()],
  lanHelper: needsGrant,
  rows: () => [roomRow],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  const join = doc.querySelector('#detail-join');
  check('Join room is disabled without the permission', join && join.disabled);
  check('the reason is shown', doc.querySelector('#detail').textContent.includes('sudo setcap'));
  check('no grant is offered where none is possible', !doc.querySelector('#detail #grant-helper'));
  const host = doc.querySelector('#host-btn');
  check('hosting is disabled for the same reason', host && host.disabled);
});

await run('hosting a room', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: readyHelper,
  roomAfter: { ...memberRoom, role: 'host', name: 'Friday', members: [{ address: '198.19.1.1', is_self: true }], address: '198.19.1.1', adapter: 'starting' },
  rows: () => [row()],
}, async (win, doc) => {
  const panel = doc.querySelector('#room-panel');
  check('a shell with rooms shows the room panel', panel && !panel.hidden);
  const select = doc.querySelector('#host-game');
  check('only games with a [lan] block can be hosted',
    select && [...select.options].map(o => o.value).join() === 'openttd',
    select && [...select.options].map(o => o.value).join());
  const name = doc.querySelector('#host-name');
  name.value = 'Friday';
  name.dispatchEvent(new win.Event('input', { bubbles: true }));
  win.eval('renderRoomPanel()');
  check('a half-typed room name survives a poll', doc.querySelector('#host-name').value === 'Friday');
  doc.querySelector('#host-btn').click();
  await settle();
  const args = lastArgs.get('host_room');
  check('host_room is sent the game and the name', args && args.gameId === 'openttd' && args.name === 'Friday', JSON.stringify(args));
  const banner = doc.querySelector('#room-banner').textContent;
  check('the banner says the room is being hosted', banner.includes('Hosting a LAN room for OpenTTD'), banner);
  check('and that the adapter is still coming up', banner.includes('bringing up the network adapter'), banner);
});

// The per-room path: a password each room, nothing installed. It is ready as
// it stands, and granting once is an offer, not a requirement.
await run('asking per room is ready, and granting is only an offer', {
  status: running,
  games: [lanGame()],
  lanHelper: perRoom,
  rows: () => [roomRow],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  const join = doc.querySelector('#detail-join');
  check('Join room is enabled without any permission granted', join && !join.disabled);
  check('the player is told nothing is installed', doc.querySelector('#detail').textContent.includes('nothing is installed'));
  check('granting once is offered', !!doc.querySelector('#detail #grant-helper'));
  check('there is nothing to revoke', !doc.querySelector('#revoke-helper'));
});

await run('a granted permission can be taken back', {
  status: running,
  games: [lanGame()],
  lanHelper: granted,
  afterRevoke: perRoom,
  rows: () => [row()],
}, async (win, doc) => {
  const revoke = doc.querySelector('#room-body #revoke-helper');
  check('Revoke is offered once granted', !!revoke);
  revoke?.click();
  await settle();
  check('revoke_lan_helper was called', calls.includes('revoke_lan_helper'));
  check('and the panel goes back to asking per room',
    doc.querySelector('#room-body').textContent.includes('asked each time'), doc.querySelector('#room-body').textContent);
});

// The room check: what a game's empty LAN list cannot say, said in words.
await run('checking a room shows what failed and who answered', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: memberRoom,
  roomCheck: {
    ok: false, address: '198.19.4.2', port: 3979,
    findings: [
      'The room does not work fully from this machine:',
      'A broadcast to 255.255.255.255 did not go through the room\u2019s adapter \u2014 this machine sent it out another network.',
    ],
    members: [{ address: '198.19.1.1', ok: false, round_trip_ms: 31 }],
  },
  rows: () => [row()],
}, async (win, doc) => {
  const btn = doc.querySelector('#room-body #room-check');
  check('Check room is offered in a room with another member', btn && !btn.disabled);
  btn?.click();
  await settle();
  check('check_room was called', calls.includes('check_room'));
  const out = doc.querySelector('#room-check-result');
  const t = out ? out.textContent : '';
  check('the verdict is shown', t.includes('does not work fully'), t);
  check('and what failed', t.includes('another network'), t);
  check('and each member, with its round trip', t.includes('198.19.1.1') && t.includes('did not answer everything'), t);
  win.eval('renderRoomPanel()');
  check('the result survives a poll', !!doc.querySelector('#room-check-result')?.textContent.includes('another network'));
});

// The first real NFSU2 race: the host's ufw dropped the member's join, and the
// member saw the race in the game and hung joining it. The host's pane must
// say so in plain words, and fix it with one click.
const blockedHost = {
  ...memberRoom, role: 'host',
  firewall: fw({
    dropped: [{ port: 9900, transport: 'tcp', from: '198.19.4.2' }],
    advice: 'ufw is refusing incoming connections here. Allow the room\u2019s adapter once, in a terminal:',
    heads_up: 'ufw is on here and refuses incoming connections by default',
    command: 'sudo ufw allow in on gbl0',
    can_fix: true,
  }),
};

await run('a firewall here dropping the room is said plainly, and fixed with one click', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: blockedHost,
  roomAfterFix: { ...blockedHost, firewall: fw({ can_fix: true, command: 'sudo ufw allow in on gbl0' }) },
  rows: () => [row()],
}, async (win, doc) => {
  const box = doc.querySelector('#room-body #room-firewall');
  const t = box ? box.textContent : '';
  check('the blocked join is named, without jargon first', t.includes('198.19.4.2 tried to join a game on this computer') && t.includes('blocked'), t);
  check('the verdict says not ready', doc.querySelector('#room-verdict')?.textContent.includes('cannot join'));
  check('the by-hand command is still there, tucked away', doc.querySelector('#room-firewall-command')?.textContent === 'sudo ufw allow in on gbl0');
  const banner = doc.querySelector('#room-banner');
  check('the banner says the room is blocked', banner.classList.contains('err') && banner.textContent.includes('firewall'), banner.textContent);
  const fix = doc.querySelector('#room-fix');
  check('a Fix button is offered', fix && !fix.disabled);
  fix?.click();
  await settle();
  check('Fix calls the launcher', calls.includes('fix_room_firewall'));
  const after = doc.querySelector('#room-firewall')?.textContent || '';
  check('and says it is done', after.includes('Done'), after);
  check('and the warning is gone', !after.includes('tried to join'), after);
  check('and the banner is calm again', !doc.querySelector('#room-banner').classList.contains('err'));
});

await run('a Fix that was refused says so', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: blockedHost,
  firewallFails: 'the password prompt was cancelled',
  rows: () => [row()],
}, async (win, doc) => {
  doc.querySelector('#room-fix')?.click();
  await settle();
  const t = doc.querySelector('#room-firewall')?.textContent || '';
  check('the refusal is shown', t.includes('Not changed') && t.includes('cancelled'), t);
  check('and the warning stays', t.includes('tried to join'), t);
});

await run('Windows blocking the game, and a third-party firewall, are named', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: {
    ...memberRoom,
    firewall: fw({
      dropped: [{ port: 3282, transport: 'tcp', from: '198.19.1.1' }],
      can_fix: true,
      blocking_programs: [{ program: 'speed2.exe', path: 'C:\\Games\\speed2.exe' }],
      other_firewalls: ['Norton Firewall'],
    }),
  },
  roomAfterFix: memberRoom,
  rows: () => [row()],
}, async (win, doc) => {
  const t = doc.querySelector('#room-firewall')?.textContent || '';
  check('the blocked program is named', t.includes('Windows is blocking speed2.exe'), t);
  check('the third-party firewall is named, with what to do', t.includes('Norton Firewall is on') && t.includes('allow Lanthorn'), t);
  doc.querySelector('#room-unblock')?.click();
  await settle();
  check('Unblock calls the launcher', calls.includes('unblock_room_programs'));
});

await run('a firewall that bites by default is warned about before anything is dropped', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: { ...memberRoom, firewall: fw({ heads_up: 'ufw is on here.', command: 'sudo ufw allow in on gbl0', can_fix: true }) },
  rows: () => [row()],
}, async (win, doc) => {
  const t = doc.querySelector('#room-firewall')?.textContent || '';
  check('the heads-up is shown', t.includes('may stop the others'), t);
  check('with a Fix button', !!doc.querySelector('#room-fix'));
  check('the banner is not an error yet', !doc.querySelector('#room-banner').classList.contains('err'));
});

// The second real NFSU2 try: the firewall open, and still no lobby. The
// room's filter is the only witness to a port the pack lacks, so it says so.
await run('ports the pack lacks are named, with a report to send', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: {
    ...memberRoom,
    pack_gaps: {
      ports: ['UDP 3660'],
      seen: ['UDP from 198.19.1.1:3660 to this computer\u2019s port 3660 (12 packets)'],
      report: 'The room blocked traffic the game\u2019s pack does not list. The pack probably needs: UDP 3660.',
      can_allow: ['UDP 3660'],
      allowed: [],
    },
  },
  roomAfterFix: { ...memberRoom, pack_gaps: { ports: [], seen: [], report: null, can_allow: [], allowed: ['UDP 3660'], wide_open: false } },
  rows: () => [row()],
}, async (win, doc) => {
  const t = doc.querySelector('#room-pack-gaps')?.textContent || '';
  check('the missing port is named in plain words', t.includes('ports its pack does not list') && t.includes('UDP 3660'), t);
  check('what was blocked is listed', t.includes('12 packets'), t);
  check('the report is there to copy', doc.querySelector('#room-pack-gaps-report')?.textContent.includes('probably needs'));
  check('the verdict says not ready', doc.querySelector('#room-verdict')?.textContent.includes('ports its pack does not list'));
  check('the banner says so too', doc.querySelector('#room-banner').classList.contains('err'));
  const allow = doc.querySelector('#room-allow-ports');
  check('Allow is offered', allow && !allow.disabled);
  allow?.click();
  await settle();
  check('Allow calls the launcher', calls.includes('allow_room_ports'));
  const after = doc.querySelector('#room-pack-gaps')?.textContent || '';
  check('the allowed port is shown, with Undo', after.includes('Also allowed on this computer for this game: UDP 3660') && !!doc.querySelector('#room-reset-ports'), after);
  check('and asks for the report still', after.includes('send the report'), after);
  check('the warning is gone', !after.includes('ports its pack does not list'), after);
  doc.querySelector('#room-reset-ports')?.click();
  await settle();
  check('Undo calls the launcher', calls.includes('reset_room_ports'));
});

// Dana's report: her firewall off, and still no lobby. When nothing names the
// port, one switch opens every high port for the game, and is undone as easily.
await run('the last-resort switch opens more ports, and says so, and turns off', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: memberRoom,
  roomAfterFix: { ...memberRoom, pack_gaps: { ...memberRoom.pack_gaps, wide_open: true } },
  rows: () => [row()],
}, async (win, doc) => {
  const btn = doc.querySelector('#room-still-stuck #room-wide-on');
  check('a still-stuck switch is offered, tucked away', !!btn);
  btn?.click();
  await settle();
  check('it calls the launcher', calls.includes('set_room_wide_open'));
  const t = doc.querySelector('#room-wide-open')?.textContent || '';
  check('the pane says every high port is open, plainly', t.includes('Every port above 1024 is open'), t);
  check('with a way back', !!doc.querySelector('#room-wide-off'));
});

await run('no firewall to speak of shows nothing', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: memberRoom,
  rows: () => [row()],
}, async (win, doc) => {
  check('no firewall box', !doc.querySelector('#room-firewall'));
  check('and no pack-gap box', !doc.querySelector('#room-pack-gaps'));
});

// Nobody should have to know there is a check: it runs when someone joins.
await run('the room checks itself when someone joins, and says ready', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: memberRoom,
  roomCheck: {
    ok: true, address: '198.19.4.2', port: 3979,
    findings: ['The room works: every member heard this machine\u2019s LAN broadcasts and answered.'],
    members: [{ address: '198.19.1.1', ok: true, round_trip_ms: 31, tcp_unanswered: [] }],
  },
  rows: () => [row()],
  beforeApp: (win) => { win.GPP_AUTO_CHECK_DELAY_MS = 0; },
}, async (win, doc) => {
  await new Promise(r => setTimeout(r, 20));
  await settle();
  check('check_room ran without a click', calls.includes('check_room'));
  check('the verdict is ready to play', doc.querySelector('#room-verdict')?.textContent.includes('Ready to play'), doc.querySelector('#room-verdict')?.textContent);
});

await run('a room check needs somebody to answer', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: { ...memberRoom, members: [{ address: '198.19.4.2', is_self: true }] },
  roomCheck: { ok: true, address: '198.19.4.2', port: 3979, findings: [], members: [] },
  rows: () => [row()],
}, async (win, doc) => {
  const btn = doc.querySelector('#room-body #room-check');
  check('Check room is disabled when alone in the room', btn && btn.disabled);
});

await run('an older shell shows no room controls at all', {
  status: running,
  rows: () => [row()],
}, (win, doc) => {
  check('the room panel is hidden', doc.querySelector('#room-panel').hidden);
  check('and no error is shown for the missing commands', doc.querySelector('#error').classList.contains('hidden'));
});

// The webview's own menu is a browser's, and its Reload tears the page out from
// under a running launcher. A right-click on a row gets the launcher's menu, and
// Remove takes the row off the list at once — it used to wait out a probe, and
// then come straight back on the next poll.
{
  let listed = [row(), legacyRow];
  await run('right-click gives the launcher\u2019s menu, and Remove removes', {
    status: running,
    rows: () => listed,
  }, async (win, doc) => {
    const target = doc.querySelector('#list .row');
    const hash = target.dataset.hash;
    const ev = new win.MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 50, clientY: 50 });
    target.dispatchEvent(ev);
    check('the browser menu is suppressed on a row', ev.defaultPrevented);
    const menu = doc.querySelector('#ctx-menu');
    check('the launcher menu opens', menu && !menu.hidden);
    const labels = [...menu.querySelectorAll('.menu-item')].map(b => b.textContent);
    for (const want of ['Join server', 'View details', 'Trace path', 'Copy address', 'Remove from list']) {
      check(`the menu offers ${want}`, labels.some(l => l.includes(want)), labels.join(' | '));
    }

    [...menu.querySelectorAll('.menu-item')].find(b => b.textContent.includes('Trace path')).click();
    await settle();
    check('Trace path asks the core for that server', calls.includes('trace_path') && lastArgs.get('trace_path')?.destinationHash === hash);
    check('and says what it found', doc.querySelector('#toast').textContent.includes('2 hops'), doc.querySelector('#toast').textContent);

    target.dispatchEvent(new win.MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
    // The core forgets it everywhere; a later poll must agree.
    listed = [legacyRow];
    doc.querySelector('#ctx-remove').click();
    check('the row is gone at once, before any reply', !doc.querySelector(`#list .row[data-hash="${hash}"]`));
    await settle();
    check('forget_server was sent that server', lastArgs.get('forget_server')?.destinationHash === hash);
    check('the menu closed', doc.querySelector('#ctx-menu').hidden);

    const blank = new win.MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    doc.querySelector('.toolbar').dispatchEvent(blank);
    check('the browser menu is suppressed everywhere else too', blank.defaultPrevented);
    const field = new win.MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    doc.querySelector('#f-text').dispatchEvent(field);
    check('but not inside a text field, where cut and paste live', !field.defaultPrevented);

    for (const init of [{ key: 'F5' }, { key: 'r', ctrlKey: true }]) {
      const k = new win.KeyboardEvent('keydown', { ...init, bubbles: true, cancelable: true });
      doc.dispatchEvent(k);
      check(`reload by ${init.ctrlKey ? 'Ctrl+R' : 'F5'} is blocked`, k.defaultPrevented);
    }
  });
}

// Announce: a browser has nothing of its own to announce, so it asks the mesh
// where its saved servers are; a host's room goes out too.
await run('Announce asks for saved servers, and announces a hosted room', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: { supported: true, path: '/x', ready: true, mode: 'granted', can_grant: false, can_revoke: false, detail: 'Ready.' },
  room: { ...noRoom, active: true, role: 'host', game_id: 'openttd', room_hash: 'ab'.repeat(16), address: '198.19.0.1', members: [{ address: '198.19.0.1', is_self: true }], adapter: 'up' },
  known: [{ destination_hash: 'cd'.repeat(16), name: 'x', game_id: 'sven-coop', last_seen_secs: 9 }],
  rows: () => [row()],
}, async (win, doc) => {
  doc.querySelector('#announce-btn').click();
  await settle();
  check('the hosted room is announced', calls.includes('announce_room'));
  check('saved servers are looked for', calls.includes('refresh_known_servers'));
  const t = doc.querySelector('#toast').textContent;
  check('and it says both', t.includes('room was announced') && t.includes('1 saved server'), t);
  check('the room page offers Announce now', !!doc.querySelector('#room-announce'));
});


// An invite link opens the server it names — listed or not — and never joins.
await run('an invite opens its server and waits for a click', {
  status: running,
  rows: () => [],
  invite: { destination_hash: 'ab'.repeat(16), game_id: 'sven-coop', name: 'Idan\u2019s invite', room: false },
}, async (win, doc) => {
  await settle();
  const pane = doc.querySelector('#detail');
  check('the invite opens a detail pane', pane && !pane.hidden);
  check('it shows the invite\u2019s name', pane.textContent.includes('Idan\u2019s invite'), pane.textContent.slice(0, 200));
  check('Join is offered', !!doc.querySelector('#detail-join') && !doc.querySelector('#detail-join').disabled);
  check('nothing was joined by the link', !calls.includes('join_server') || lastArgs.get('join_server')?.destinationHash !== 'ab'.repeat(16));
  check('the mesh is asked where an unheard server is', lastArgs.get('trace_path')?.destinationHash === 'ab'.repeat(16));
});

await run('a room row copies a room invite', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: readyHelper,
  rows: () => [roomRow],
}, async (win, doc) => {
  const r = doc.querySelector('#list .row');
  r.dispatchEvent(new win.MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
  doc.querySelector('#ctx-invite').click();
  await settle();
  check('invite_link is asked for a room link', lastArgs.get('invite_link')?.room === true && lastArgs.get('invite_link')?.destinationHash === roomRow.destination_hash);
});

await run('an update that installs itself is offered once found', {
  status: running,
  rows: () => [row()],
  autoUpdate: true,
  update: { current: '1.0.0', available: '1.1.0', notes: 'Better things.', install: 'in-place', page: 'x', auto_check: true },
}, async (win, doc) => {
  await settle();
  const banner = doc.querySelector('#update-banner');
  check('the banner says a version is available', banner && !banner.classList.contains('hidden') && banner.textContent.includes('1.1.0'), banner?.textContent);
  check('it offers Update now', doc.querySelector('#update-now')?.textContent === 'Update now');
  doc.querySelector('#update-now').click();
  await settle();
  check('install_update is called', calls.includes('install_update'));
});

await run('a copy that cannot replace itself links to the download page', {
  status: running,
  rows: () => [row()],
  autoUpdate: true,
  update: { current: '1.0.0', available: '1.1.0', notes: null, install: 'link-only', page: 'x', auto_check: true },
}, async (win, doc) => {
  await settle();
  const before = calls.filter(c => c === 'install_update').length;
  doc.querySelector('#update-now').click();
  await settle();
  check('it says Download', doc.querySelector('#update-now')?.textContent === 'Download');
  check('it opens the release page', calls.includes('open_release_page'));
  check('and never installs', calls.filter(c => c === 'install_update').length === before);
});

{
  const before = () => calls.filter(c => c === 'check_update').length;
  const n0 = before();
  await run('with the check off, nothing is asked at start', {
    status: running,
    rows: () => [row()],
    autoUpdate: false,
  }, async () => { await settle(); });
  check('check_update is not called when turned off', before() === n0);
}

for (const e of consoleErrors) failures.push(`uncaught: ${e}`);

if (failures.length) {
  console.log('FAIL');
  for (const f of failures) console.log('  - ' + f);
  process.exit(1);
}
console.log(`OK — ${new Set(calls).size} commands exercised: ${[...new Set(calls)].join(', ')}`);
