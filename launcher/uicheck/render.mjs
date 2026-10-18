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

const settle = async () => { for (let i = 0; i < 30; i++) await new Promise(r => setTimeout(r, 0)); };

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
      case 'notify':
        (scenario.notified ??= []).push(args.title);
        return true;
      case 'notifications_on':
        return scenario.notificationsOn ?? true;
      case 'set_notifications_on':
        return null;
      case 'favorites':
        return typeof scenario.favorites === 'function' ? scenario.favorites() : (scenario.favorites ?? []);
      case 'set_favorite':
        return null;
      case 'installed_games':
        return scenario.installed ?? [];
      case 'get_game':
        (scenario.gotten ??= []).push(args.gameId);
        return null;
      case 'pick_game_path':
        scenario.picked = true;
        return scenario.pickResult ?? null;
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
      case 'hosting':
        return scenario.hostingNow ?? { active: false };
      case 'host_server':
        scenario.hostAsked = args.request;
        scenario.hostingNow = { active: true, game_id: args.request.game_id, name: args.request.name, game_port: args.request.game_port, max_players: args.request.max_players, destination_hash: 'ab'.repeat(16), invite: 'lanthorn://x', clients: 0 };
        return scenario.hostingNow;
      case 'stop_hosting':
        scenario.hostingNow = { active: false };
        return null;
      case 'first_run':
        return !!scenario.firstRun;
      case 'finish_first_run':
        scenario.firstRunFinished = true;
        return null;
      case 'heard_pack_repos':
        if (!scenario.heard) throw new Error(`unknown command ${cmd}`);
        return scenario.heard;
      case 'official_packs':
        if (!scenario.official) throw new Error(`unknown command ${cmd}`);
        return scenario.official;
      case 'sync_official_packs':
        if (!scenario.official) throw new Error(`unknown command ${cmd}`);
        (scenario.officialCalls ??= []).push('sync');
        scenario.official = { ...scenario.official, checked_unix: Math.floor(Date.now() / 1000), added: scenario.officialAdds ?? [], updated: [] };
        return scenario.official;
      case 'set_official_packs':
        (scenario.officialCalls ??= []).push(args.on ? 'on' : 'off');
        scenario.official = { ...scenario.official, on: args.on };
        if (!args.on) scenario.repos = (scenario.repos ?? []).filter(r => !r.official);
        return null;
      case 'pack_repos':
        return scenario.repos ?? [];
      case 'add_pack_repo':
        (scenario.repoCalls ??= []).push(['add', args.url]);
        return { url: args.url, name: 'Added', error: null, packs: [] };
      case 'remove_pack_repo':
        (scenario.repoCalls ??= []).push(['drop', args.url]);
        return null;
      case 'install_repo_pack':
        (scenario.repoCalls ??= []).push(['install', args.url, args.id]);
        return { id: args.id, display_name: 'OpenTTD', trust: 'signed by an unknown key', trust_detail: 'x', replaced: false };
      case 'remove_installed_pack':
        (scenario.repoCalls ??= []).push(['remove', args.id]);
        return null;
      case 'player_name':
        return scenario.playerName ?? null;
      case 'set_player_name':
        (scenario.namesSet ??= []).push(args.name);
        return null;
      case 'room_chat':
        return (scenario.chat ?? []).filter(l => l.seq > args.since);
      case 'send_room_chat':
        (scenario.said ??= []).push(args.text);
        return null;
      case 'server_chat_view':
        if (!scenario.serverChat) throw new Error(`unknown command ${cmd}`);
        return (args.hosted ? scenario.serverChat.hosted : scenario.serverChat.joined) ?? { state: 'none', members: [] };
      case 'server_chat':
        return ((args.hosted ? scenario.serverChat.hostedLines : scenario.serverChat.lines) ?? []).filter(l => l.seq > args.since);
      case 'send_server_chat':
        (scenario.serverSaid ??= []).push([args.hosted, args.text]);
        return null;
      case 'set_hosted_chat':
        (scenario.serverChatCalls ??= []).push(['chat', args.on]);
        return null;
      case 'mute_in_hosted_chat':
        (scenario.serverChatCalls ??= []).push(['mute', args.identity, args.muted]);
        return null;
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
  await assertions(window, window.document, scenario);
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

await run('favorites lead the list, and an offline one is shown as offline', {
  status: running,
  rows: () => [row(), row({ destination_hash: '11112222333344445555666677778888', name: 'Starred one', hops: 4 })],
  favorites: [
    { destination_hash: '11112222333344445555666677778888', name: 'Starred one', game_id: 'sven-coop', last_seen_secs: 2, online: true },
    { destination_hash: 'ffffeeeeddddccccbbbbaaaa99998888', name: 'Friday night', game_id: 'sven-coop', last_seen_secs: 2 * 86400, online: false },
  ],
}, (win, doc) => {
  const rows = [...doc.querySelectorAll('#list .row')];
  check('a favorite that is up leads the list, whatever the sort', rows[0]?.dataset.hash === '11112222333344445555666677778888');
  check('a favorite row is starred', rows[0]?.classList.contains('fav') && !rows[1]?.classList.contains('fav'));
  const off = doc.querySelector('#list .fav-offline');
  check('an offline favorite has its own section', !!off);
  check('the offline section names it and says when it was heard',
    off && off.textContent.includes('Friday night') && off.textContent.includes('last heard'), off?.textContent);
  check('an offline favorite is never a live row', !rows.some(r => r.textContent.includes('Friday night')));
});

{
  let up = false;
  await run('a favorite coming up is told once', {
    status: running,
    rows: () => [row()],
    favorites: () => [{ destination_hash: 'ffffeeeeddddccccbbbbaaaa99998888', name: 'Friday night', game_id: 'sven-coop', last_seen_secs: up ? 1 : 900, online: up }],
  }, async (win, doc, scenario) => {
    const toast = () => doc.querySelector('#toast')?.textContent || '';
    check('nothing is announced on the first look', !toast().includes('Friday night is up'));
    up = true;
    await win.eval('pollAll()');
    await settle();
    check('a favorite coming up is announced', toast().includes('Friday night is up'), toast());
    check('and the desktop is told too', (scenario.notified || []).includes('Friday night is up'), JSON.stringify(scenario.notified));
  });
}

await run('a game on this computer is badged Installed', {
  status: running,
  rows: () => [row(), legacyRow, row({ destination_hash: '0000aaaa0000aaaa0000aaaa0000aaaa', name: 'Other game', game_id: 'half-life' })],
  installed: [{ game_id: 'sven-coop', installed: true, launch_ready: true }, { game_id: 'half-life', installed: false, launch_ready: false }],
}, async (win, doc) => {
  await settle();
  const rows = [...doc.querySelectorAll('#list .row')];
  const badged = rows.filter(r => r.querySelector('.installed-badge')).map(r => r.dataset.hash);
  check('only the installed game\'s row is badged', badged.length === 1 && badged[0] === 'aabbccddeeff00112233445566778899', JSON.stringify(badged));
});

await run('a game not here offers Install with Steam, and sends only its id', {
  status: running,
  rows: () => [row()],
  installed: [{ game_id: 'sven-coop', installed: false, launch_ready: false, steam_download: null, get_url: 'steam://install/225840' }],
}, async (win, doc, scenario) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  const btn = [...doc.querySelectorAll('#detail button')].find(b => b.textContent === 'Install with Steam');
  check('a game not here offers Install with Steam', !!btn, doc.querySelector('#detail')?.textContent.slice(0, 400));
  btn?.click();
  await settle();
  check('the page sends a game id, never a link', JSON.stringify(scenario.gotten) === '["sven-coop"]', JSON.stringify(scenario.gotten));
});

await run('a game Steam is fetching says how far, in Steam\'s numbers', {
  status: running,
  rows: () => [row()],
  installed: [{ game_id: 'sven-coop', installed: false, launch_ready: false, steam_download: { bytes_done: 1500000000, bytes_total: 3000000000 }, get_url: 'steam://install/225840' }],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  const t = doc.querySelector('#detail').textContent;
  check('Steam\'s progress is shown', t.includes('1.5 GB of 3.0 GB (50%)'), t.slice(0, 500));
  check('and no install button while it runs', ![...doc.querySelectorAll('#detail button')].some(b => b.textContent === 'Install with Steam'));
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

// The pane re-renders on every poll; its picture must not. A new <img> each
// time starts transparent and fades in, which made the artwork blink.
await run('the detail picture is not rebuilt by a re-render', {
  status: running,
  rows: () => [row()],
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  const before = doc.querySelector('#detail .detail-hero > .art');
  check('the detail pane has artwork', !!before);
  win.eval('renderDetail()');
  await win.pollAll();
  const after = doc.querySelector('#detail .detail-hero > .art');
  check('the same artwork element survives a re-render and a poll', before === after);
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

// The round trip in words before Join. A server that did not answer gets no
// hint: no number is not a good number.
for (const [rtt, want] of [[84, /should play smoothly/], [180, /little delay/], [400, /expect lag/], [900, /Very far/]]) {
  await run('a ' + rtt + ' ms round trip is put in words', {
    status: running,
    rows: () => [row()],
    details: { ...details, rtt_ms: rtt },
  }, async (win, doc) => {
    doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
    const hint = doc.querySelector('#detail-playability');
    check(rtt + ' ms gets a playability hint', !!hint && want.test(hint.textContent), hint && hint.textContent);
  });
}
// The link check: a game needing a fast link, heard over radio, is warned
// about; a low-rate one is told joins will be slow; a TCP relay says nothing.
for (const [iface, tier, want] of [['LoRa', 2, /needs a fast link/], ['LoRa', 1, /low-rate game/], ['TcpClient', 2, null]]) {
  await run(`a tier ${tier} game heard over ${iface}`, {
    status: running,
    rows: () => [row({ interface_label: iface, min_link_class: tier })],
  }, async (win, doc) => {
    doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
    const fit = doc.querySelector('#detail-linkfit');
    if (want) check(`tier ${tier} over ${iface} is named`, !!fit && want.test(fit.textContent), fit && fit.textContent);
    else check(`tier ${tier} over ${iface} needs no warning`, !fit, fit && fit.textContent);
  });
}

// Before Join, not after: a game that needs a fast link, heard over radio, is
// marked in the list, and Join asks once more — and then joins.
await run('a slow link is marked in the list and Join asks once', {
  status: running,
  rows: () => [row({ interface_label: 'LoRa', min_link_class: 2 }), row({ destination_hash: '0101'.repeat(8), name: 'Near', interface_label: 'TcpClient', min_link_class: 2 })],
}, async (win, doc) => {
  const rows = [...doc.querySelectorAll('#list .row')];
  const marked = rows.filter(r => r.querySelector('.slow-badge')).map(r => r.dataset.hash);
  check('only the radio-heard row is marked', marked.length === 1 && marked[0] === 'aabbccddeeff00112233445566778899', JSON.stringify(marked));
  rows.find(r => r.dataset.hash === 'aabbccddeeff00112233445566778899').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  const joins = () => calls.filter(c => c === 'join_server').length;
  const before = joins();
  doc.querySelector('#detail-join').click();
  await settle();
  check('the first press warns instead of joining', joins() === before && /Join anyway/.test(doc.querySelector('#detail').textContent));
  check('the button says what a second press does', doc.querySelector('#detail-join').textContent === 'Join anyway');
  doc.querySelector('#detail-join').click();
  await settle();
  check('a second press joins: a warning, never a block', joins() === before + 1);
});

await run('many hops and no round trip yet is said', {
  status: running,
  rows: () => [row({ hops: 7 })],
  details: { ...details, reachable: false, rtt_ms: null },
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  await settle();
  const hint = doc.querySelector('#detail-playability');
  check('seven hops is a hint', !!hint && /7 hops away/.test(hint.textContent), hint && hint.textContent);
});

// A controller drives the list: down moves, A opens the detail on Join, B
// goes back, Start switches big screen. The pad is a stub read by the page's
// own loop, frame by frame.
{
  const pad = { buttons: Array.from({ length: 17 }, () => ({ pressed: false })), axes: [0, 0, 0, 0] };
  let frames = [];
  await run('a controller drives the list', {
    status: running,
    rows: () => [row(), row({ destination_hash: '0202'.repeat(8), name: 'Second', hops: 2 })],
    beforeApp(w) {
      Object.defineProperty(w.navigator, 'getGamepads', { value: () => [pad] });
      w.requestAnimationFrame = cb => { frames.push(cb); return frames.length; };
      w.performance.now = () => Date.now();
      w.Element.prototype.scrollIntoView ??= () => {};
      w.CSS ??= { escape: v => String(v).replace(/[^A-Za-z0-9_-]/g, c => '\\' + c) };
    },
  }, async (win, doc) => {
    await settle();
    const frame = () => { const f = frames; frames = []; f.forEach(cb => cb()); };
    const press = async (i) => {
      pad.buttons[i].pressed = true; frame();
      pad.buttons[i].pressed = false; frame();
      await settle();
    };
    await win.pollAll();
    check('the list is there to drive', doc.querySelectorAll('#list .row').length === 2, String(doc.querySelectorAll('#list .row').length));
    win.dispatchEvent(new win.Event('gamepadconnected'));
    frame();
    await press(13); // down
    await press(13);
    const active = () => doc.querySelector('#list .row[tabindex="0"]')?.dataset.hash;
    check('down moves through the list', active() === '0202'.repeat(8), active());
    await press(0); // A
    check('A opens the detail pane', !doc.querySelector('#detail').hidden);
    await new Promise(r => setTimeout(r, 5));
    check('and lands on Join', doc.activeElement && doc.activeElement.id === 'detail-join', doc.activeElement && doc.activeElement.id);
    await press(1); // B
    check('B closes it', doc.querySelector('#detail').hidden);
    await press(9); // Start
    check('Start turns big screen on', doc.body.classList.contains('bigscreen'));
    await press(9);
    check('and off', !doc.body.classList.contains('bigscreen'));
  });
}

// With no Gamepad API at all (WebKitGTK without libmanette), the launcher's
// own reader (gamepad.rs) reports a pad over the `pad` event, and it drives
// the page the same way, once per press.
{
  let frames = [];
  let padListener = null;
  await run('a pad the launcher reads drives the page without the Gamepad API', {
    status: running,
    rows: () => [row(), row({ destination_hash: '0202'.repeat(8), name: 'Second', hops: 2 })],
    beforeApp(w) {
      w.__TAURI__.event = { listen: (name, cb) => { if (name === 'pad') padListener = cb; return Promise.resolve(() => {}); } };
      w.requestAnimationFrame = cb => { frames.push(cb); return frames.length; };
      w.performance.now = () => Date.now();
      w.Element.prototype.scrollIntoView ??= () => {};
      w.CSS ??= { escape: v => String(v).replace(/[^A-Za-z0-9_-]/g, c => '\\' + c) };
    },
  }, async (win, doc) => {
    await settle();
    check('the page listens for the launcher\'s pad', !!padListener);
    const frame = () => { const f = frames; frames = []; f.forEach(cb => cb()); };
    const send = held => { padListener({ payload: { connected: 1, held } }); frame(); };
    await win.pollAll();
    send([]);
    check('a found controller is told', (doc.querySelector('#toast')?.textContent || '').includes('Controller found'));
    send(['DOWN']); send([]); send(['DOWN']); send([]);
    await settle();
    const active = () => doc.querySelector('#list .row[tabindex="0"]')?.dataset.hash;
    check('down moves through the list', active() === '0202'.repeat(8), active());
    send(['START']); send(['START']); send([]);
    await settle();
    check('one Start held over two frames is one press', doc.body.classList.contains('bigscreen'));
    padListener({ payload: { connected: 0, held: [] } });
    frame();
    check('the loop stops when the pad goes', frames.length === 0);
  });
}

await run('no round trip, no playability hint', {
  status: running,
  rows: () => [row()],
  details: { ...details, reachable: false, rtt_ms: null },
}, async (win, doc) => {
  doc.querySelector('#list .row').dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
  for (let i = 0; i < 20; i++) await new Promise(r => setTimeout(r, 0));
  check('a server that did not answer gets no hint', !doc.querySelector('#detail-playability'));
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

// The Connections card was drawn right after Connect, while a TCP interface
// was still dialling, and the poll never redrew it: it said "down" for a
// connection that was up a second later.
await run('the Connections card follows the poll', {
  status: { running: true, interfaces: [{ id: '1', label: 'hub.example.org:4789', connected: false }], heard_total: 0 },
  rows: () => [],
}, async (win, doc, scenario) => {
  win.setView('connections');
  await settle();
  const card = () => doc.querySelector('#conn-node').textContent;
  check('a dialling interface reads as down', card().includes('down'), card());
  const btn = doc.querySelector('#conn-node button');
  await win.pollAll();
  check('an unchanged state does not rebuild the card', doc.querySelector('#conn-node button') === btn);
  scenario.status = { running: true, interfaces: [{ id: '1', label: 'hub.example.org:4789', connected: true }], heard_total: 0 };
  await win.pollAll();
  check('once it connects the card says up', card().includes('up') && !card().includes('down'), card());
});

// On a LAN with no other node, auto-discovery reports Disconnected. That is
// searching, and showing it as down read as a broken launcher.
await run('auto-discovery with nobody around is searching, not down', {
  status: { running: true, heard_total: 0, interfaces: [
    { id: '1', label: 'hub:4789', connected: true, idle: false },
    { id: '2', label: 'LAN auto-discovery', connected: false, idle: true },
  ] },
  rows: () => [],
}, async (win, doc) => {
  const side = doc.querySelector('#status').textContent;
  check('the idle interface is not counted as down', side.includes('1 of 1 connection up'), side);
  const bar = doc.querySelector('#statusbar-text');
  check('the status bar says searching', bar.textContent.includes('LAN auto-discovery (searching)'), bar.textContent);
  check('and does not flag it', !bar.querySelector('.sb-bad'));
  win.setView('connections');
  await settle();
  const card = doc.querySelector('#conn-node').textContent;
  check('the card says searching', card.includes('searching') && !card.includes('down'), card);
});

// The checkbox was OR-ed with the saved setting and never saved itself, so
// once auto-discovery was saved there was no way to switch it off.
await run('LAN auto-discovery can be switched off', {
  status: { running: false, interfaces: [], heard_total: 0 },
  rows: () => [],
  interfaces: [{ id: 'auto', label: 'LAN auto-discovery', kind: 'auto', in_use: true }],
  savedOpts: { tcp: null, auto: true },
}, async (win, doc, scenario) => {
  win.setView('connections');
  await settle();
  const box = doc.querySelector('#connections-body .toggle-row input');
  check('the saved setting ticks the box', box && box.checked);
  scenario.savedOpts = { tcp: null, auto: false };
  box.checked = false;
  box.dispatchEvent(new win.Event('change'));
  await settle();
  check('unticking forgets the saved interface', lastArgs.get('remove_interface')?.id === 'auto', JSON.stringify(lastArgs.get('remove_interface')));
  await win.startBrowse();
  check('and Connect no longer asks for it', lastArgs.get('start_browse')?.opts?.auto === false, JSON.stringify(lastArgs.get('start_browse')));
});

// Unticking with only a relay saved removes nothing, so the box must start
// from the saved set; defaulting it on brought auto-discovery back each start.
await run('a relay saved without auto-discovery starts unticked', {
  status: { running: false, interfaces: [], heard_total: 0 },
  rows: () => [],
  interfaces: [{ id: 'tcp:hub:4789', label: 'hub:4789', kind: 'tcp', in_use: true }],
  savedOpts: { tcp: 'hub:4789', auto: false },
}, async (win, doc) => {
  const t = doc.body.textContent;
  check('the empty list does not promise the local network', t.includes('Connects through hub:4789.'), t.slice(0, 600));
  win.setView('connections');
  await settle();
  check('the box is unticked', !doc.querySelector('#connections-body .toggle-row input').checked);
  await win.startBrowse();
  check('Connect does not ask for it', lastArgs.get('start_browse')?.opts?.auto === false, JSON.stringify(lastArgs.get('start_browse')));
});

await run('with nothing saved, the local network is the default', {
  status: { running: false, interfaces: [], heard_total: 0 },
  rows: () => [],
}, async (win) => {
  await win.startBrowse();
  check('Connect asks for auto-discovery', lastArgs.get('start_browse')?.opts?.auto === true, JSON.stringify(lastArgs.get('start_browse')));
});

// A browse node dials one relay. An older settings file can hold several;
// the ones not dialled must not read as working.
await run('a saved relay that is not dialled says so', {
  status: { running: false, interfaces: [], heard_total: 0 },
  rows: () => [],
  interfaces: [
    { id: 'tcp:a:1', label: 'a:1', kind: 'tcp', in_use: true },
    { id: 'tcp:b:2', label: 'b:2', kind: 'tcp', in_use: false },
  ],
  savedOpts: { tcp: 'a:1', auto: false },
}, async (win, doc) => {
  win.setView('connections');
  await settle();
  const items = [...doc.querySelectorAll('#iface-list .item')].map(i => i.textContent);
  check('the dialled relay is not marked', !items[0].includes('unused'), items[0]);
  check('the other one is', items[1].includes('unused'), items[1]);
});

// The count was read at start and after Forget only.
await run('the saved-server count is re-read when Connections opens', {
  status: { running: false, interfaces: [], heard_total: 0 },
  rows: () => [],
  known: [],
}, async (win, doc, scenario) => {
  scenario.known = ['x', 'y', 'z'];
  win.setView('connections');
  await settle();
  const t = doc.querySelector('#connections-body').textContent;
  check('the count is current', t.includes('remembers 3 servers'), t.slice(-300));
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

// Hosting from the launcher (PLAN.md §13.3 step 2): the form sends what was
// chosen, the card then says it is on the mesh, and Stop takes it off.
await run('a server on this computer is put on the mesh and taken off', {
  status: running,
  rows: () => [row()],
  games: [svenGame],
  playerName: 'Ada',
}, async (win, doc, scenario) => {
  win.setView('games');
  await settle();
  await settle();
  check('the name starts as the player\u2019s server', doc.querySelector('#host-srv-name')?.value === 'Ada\u2019s server', doc.querySelector('#host-srv-name')?.value);
  const port = doc.querySelector('#host-srv-port');
  port.value = '27016';
  port.dispatchEvent(new win.Event('input'));
  doc.querySelector('#host-srv-start').click();
  await settle();
  await settle();
  check('it asks for that game, name and port', scenario.hostAsked && scenario.hostAsked.game_id === 'sven-coop'
    && scenario.hostAsked.name === 'Ada\u2019s server' && scenario.hostAsked.game_port === 27016, JSON.stringify(scenario.hostAsked));
  const t = doc.querySelector('#games-body').textContent;
  check('the card says it is on the mesh', t.includes('On the mesh: Ada\u2019s server'), t);
  doc.querySelector('#host-stop').click();
  await settle();
  await settle();
  check('Stop takes it off', !!doc.querySelector('#host-srv-start'));
});

// First run (PLAN.md §13.3 step 4): one screen, then connected with exactly
// what was chosen; and never shown when the core says it is done.
await run('a fresh install gets one welcome screen and connects', {
  status: { ...running, running: false },
  rows: () => [],
  firstRun: true,
}, async (win, doc, scenario) => {
  await settle();
  check('the welcome screen shows', !!doc.querySelector('#welcome'));
  doc.querySelector('#welcome-name').value = 'Ada';
  doc.querySelector('#welcome-hub').value = 'hub.example:4242';
  doc.querySelector('#welcome-start').click();
  await settle();
  await settle();
  check('the name is saved', (scenario.namesSet || []).includes('Ada'), JSON.stringify(scenario.namesSet));
  check('it is marked done', scenario.firstRunFinished === true);
  check('and it is gone', !doc.querySelector('#welcome'));
  check('it connects with the hub it was given', calls.includes('start_browse'), JSON.stringify(calls.slice(-6)));
});

await run('a launcher past its first run never shows it', {
  status: running,
  rows: () => [row()],
}, async (win, doc) => {
  await settle();
  check('no welcome screen', !doc.querySelector('#welcome'));
});

// Games and pack repositories (ROADMAP item 8): installed games with their
// tier, a pack from a repository installs by its repository and id, a shipped
// game offers no install, and only a repository pack can be removed.
// The official pack repository: listed as such with Check now and Turn off,
// a game that arrives by itself is announced, and an official list that was
// turned off offers Turn on.
await run('the official packs are on, sync by themselves, and can be turned off', {
  status: running,
  rows: () => [row()],
  official: { on: true, checked_unix: null, added: [], updated: [], error: null },
  officialAdds: ['sven-coop'],
  repos: [{
    url: 'https://idan2025.github.io/gaming-platform-prns/packs/index.json', official: true, name: 'Lanthorn official packs',
    error: null, signer: '7a656c1da3347bfcfe6b3981f71099c8',
    packs: [{ id: 'sven-coop', display_name: 'Sven Co-op', art_url: null, lan: false, signed: true, state: 'installed' }],
  }],
}, async (win, doc, scenario) => {
  await win.syncOfficial();
  await settle();
  check('the background sync ran', (scenario.officialCalls || []).includes('sync'), JSON.stringify(scenario.officialCalls));
  check('a game that arrived by itself is announced', (doc.querySelector('#toast')?.textContent || '').includes('New game'), doc.querySelector('#toast')?.textContent);
  win.setView('games');
  await settle();
  const t = doc.querySelector('#games-body')?.textContent || '';
  check('the official repository is named as such', t.includes('Official packs') && t.includes('signed by the Lanthorn project'), t);
  check('it offers Check now and Turn off, not Remove', !!doc.querySelector('#official-sync') && doc.querySelector('#official-off')?.textContent === 'Turn off');
  doc.querySelector('#official-off').click();
  await settle();
  await settle();
  check('Turn off reaches the core', (scenario.officialCalls || []).includes('off'), JSON.stringify(scenario.officialCalls));
  check('turned off, it offers Turn on', !!doc.querySelector('#official-on'), doc.querySelector('#games-body')?.textContent);
});

// Seeds heard on the mesh: listed with what they say, a stranger's with Add,
// and a name is only ever text.
await run('repositories heard on the mesh are listed and can be added', {
  status: running,
  rows: () => [row()],
  official: { on: true, checked_unix: null, added: [], updated: [], error: null },
  repos: [],
  heard: [
    { address: 'rns://' + 'ab'.repeat(16) + '/', name: 'Mesh <b>packs</b>', packs: 3, signer: 'cd'.repeat(16), official: false, added: false, hops: 2 },
    { address: 'rns://' + 'ef'.repeat(16) + '/', name: 'x', packs: 21, signer: '7a656c1da3347bfcfe6b3981f71099c8', official: true, added: false, hops: 1 },
  ],
}, async (win, doc, scenario) => {
  win.setView('games');
  await settle();
  await settle();
  const t = doc.querySelector('#games-body')?.textContent || '';
  check('seeds heard on the mesh are listed', t.includes('Heard on the mesh') && t.includes('Official packs, seeded on the mesh'), t);
  check('a seed\u2019s name is text, never markup', t.includes('Mesh <b>packs</b>') && !doc.querySelector('#games-body b'));
  const add = [...doc.querySelectorAll('#games-body button')].find(b => b.textContent === 'Add');
  check('a stranger\u2019s seed can be added', !!add);
  add.click();
  await settle();
  check('Add takes its rns:// address', (scenario.repoCalls || []).some(c => c[0] === 'add' && c[1].startsWith('rns://')), JSON.stringify(scenario.repoCalls));
});

await run('the Games page lists games and installs from a repository', {
  status: running,
  rows: () => [row()],
  games: [svenGame, { ...lanGame(), removable: true, trust: 'signed by an unknown key' }],
  repos: [{
    url: 'https://packs.example/index.json', name: 'Example packs', error: null, signer: 'ab12cd34ef56ab12cd34ef56ab12cd34',
    packs: [
      { id: 'sven-coop', display_name: 'Sven Co-op', art_url: null, lan: false, signed: false, state: 'shipped' },
      { id: 'quake', display_name: 'Quake <b>1</b>', art_url: null, lan: false, signed: true, state: 'available' },
    ],
  }, { url: 'https://down.example/index.json', name: null, error: 'connection refused', packs: [] }],
}, async (win, doc, scenario) => {
  win.setView('games');
  await settle();
  const t = doc.querySelector('#games-body')?.textContent || '';
  check('installed games are listed with their tier', t.includes('Sven Co-op') && t.includes('signed by an unknown key'), t);
  check('a repository is listed by name', t.includes('Example packs'), t);
  check('who signed its list is shown', t.includes('List signed by ab12cd34ef56'), t);
  check('a repository that failed says why', t.includes('connection refused'), t);
  check('a pack name is text, never markup', !doc.querySelector('#games-body b') && t.includes('Quake <b>1</b>'));
  const shipped = doc.querySelector('[data-pack="sven-coop"]');
  check('a shipped game offers no install', shipped && shipped.disabled && shipped.textContent.includes('Ships with'));
  const removes = [...doc.querySelectorAll('#games-body .game-list .btn')].map(b => b.textContent);
  check('only a repository pack can be removed', removes.length === 1, JSON.stringify(removes));
  const input = doc.querySelector('#repo-url');
  input.value = 'https://typed.example';
  input.dispatchEvent(new win.Event('input'));
  doc.querySelector('[data-pack="quake"]').click();
  await settle();
  await settle();
  check('Install asks for that pack from that repository',
    (scenario.repoCalls || []).some(c => c[0] === 'install' && c[1] === 'https://packs.example/index.json' && c[2] === 'quake'),
    JSON.stringify(scenario.repoCalls));
  check('a repository address being typed survives a re-render', doc.querySelector('#repo-url')?.value === 'https://typed.example');
});

// Room chat (ROADMAP item 6): names beside addresses, lines as text only,
// a line typed survives the panel's rebuild, and an old host is explained.
await run('a room chats, by name, and a line is only ever text', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: readyHelper,
  rows: () => [row()],
  room: { ...memberRoom, chat: true, members: [{ address: '198.19.1.1', is_self: false, name: 'Ada' }, { address: '198.19.4.2', is_self: true, name: 'Me' }] },
  playerName: 'Me',
  chat: [
    { seq: 1, name: 'Ada', address: '198.19.1.1', is_self: false, text: '<img src=x onerror=alert(1)> gg', at_unix: 0 },
    { seq: 2, name: 'Me', address: '198.19.4.2', is_self: true, text: 'one more?', at_unix: 0 },
  ],
}, async (win, doc, scenario) => {
  win.setView('rooms');
  await win.pollRoom();
  await settle();
  const members = doc.querySelector('#room-body .member-list')?.textContent || '';
  check('a member is shown by name beside the address', members.includes('Ada · 198.19.1.1') && members.includes('(you)'), members);
  const log = doc.querySelector('#chat-log');
  check('the chat is shown', !!log && !doc.querySelector('#room-chat').hidden);
  check('lines are listed with who said them', log && log.textContent.includes('Ada') && log.textContent.includes('You'), log && log.textContent);
  check('a line is text, never markup', log && !log.querySelector('img') && log.textContent.includes('<img'), log && log.innerHTML);
  const input = doc.querySelector('#chat-input');
  input.value = 'half typed';
  await win.pollRoom();
  await settle();
  check('a line being typed survives the poll', doc.querySelector('#chat-input')?.value === 'half typed');
  check('nothing is shown twice', doc.querySelectorAll('#chat-log .chat-line').length === 2, String(doc.querySelectorAll('#chat-log .chat-line').length));
  doc.querySelector('#chat-send').click();
  await settle();
  check('Send says it to the room', (scenario.said || []).includes('half typed'), JSON.stringify(scenario.said));
});

// Server chat (ROADMAP.md §6.1): the joined server's card lives in its own
// slot under the list, so a poll that rebuilds the list keeps a typed line;
// lines are text only; a server with no chat shows no card; and the host can
// mute anyone but themselves.
await run('a joined server chats, and a line is only ever text', {
  status: running,
  rows: () => [row()],
  playerName: 'Me',
  serverChat: {
    joined: { state: 'open', hosting: false, members: [
      { identity: 'aa', name: 'Host', is_self: false, is_host: true, muted: false },
      { identity: 'bb', name: 'Me', is_self: true, is_host: false, muted: false },
    ] },
    lines: [
      { seq: 1, identity: 'aa', name: 'Host', is_self: false, is_host: true, text: '<img src=x onerror=alert(1)> welcome', at_unix: 0 },
      { seq: 2, identity: 'bb', name: 'Me', is_self: true, is_host: false, text: 'hi', at_unix: 0 },
    ],
  },
}, async (win, doc, scenario) => {
  await win.pollAll();
  await settle();
  const box = doc.querySelector('#sc-joined');
  check('the joined server\u2019s chat is shown under the list', !!box && !box.hidden && !!box.closest('#server-chat-dock'));
  const log = doc.querySelector('#sc-joined-log');
  check('a server chat line is text, never markup', log && !log.querySelector('img') && log.textContent.includes('<img'), log && log.innerHTML);
  check('the host is marked', log && log.textContent.includes('Host (host)'), log && log.textContent);
  const input = doc.querySelector('#sc-joined-input');
  input.value = 'half typed';
  await win.pollAll();
  await settle();
  check('a server chat line being typed survives the poll', doc.querySelector('#sc-joined-input')?.value === 'half typed');
  check('no server chat line is shown twice', doc.querySelectorAll('#sc-joined-log .chat-line').length === 2);
  doc.querySelector('#sc-joined .chat-form button[type=submit]').click();
  await settle();
  check('Send says it on the server', JSON.stringify(scenario.serverSaid) === JSON.stringify([[false, 'half typed']]), JSON.stringify(scenario.serverSaid));
  scenario.serverChat.joined = { state: 'unavailable', detail: 'this server has no chat', members: [] };
  await win.pollAll();
  await settle();
  check('a server without chat shows no card', doc.querySelector('#sc-joined').hidden && doc.querySelector('#server-chat-dock').hidden);
});

await run('the host mutes in their server\u2019s chat and can turn it off', {
  status: running,
  rows: () => [row()],
  hostingNow: { active: true, game_id: 'sven-coop', name: 'Mine', clients: 1, invite: 'lanthorn://x', chat: true },
  serverChat: {
    hosted: { state: 'open', hosting: true, members: [
      { identity: 'aa', name: 'Me', is_self: true, is_host: true, muted: false },
      { identity: 'cc', name: 'Loud', is_self: false, is_host: false, muted: false },
    ] },
  },
}, async (win, doc, scenario) => {
  win.setView('games');
  await win.pollAll();
  await settle();
  const box = doc.querySelector('#sc-hosted');
  check('the hosted server\u2019s chat is on the Games page, outside its rebuilt body', !!box && !box.hidden && !box.closest('#games-body'));
  const mutes = [...doc.querySelectorAll('#sc-hosted-members button')];
  check('everyone but the host can be muted', mutes.length === 1, String(mutes.length));
  mutes[0].click();
  await settle();
  doc.querySelector('#sc-hosted-toggle').click();
  await settle();
  check('Mute and Turn chat off reach the core', JSON.stringify(scenario.serverChatCalls) === JSON.stringify([['mute', 'cc', true], ['chat', false]]), JSON.stringify(scenario.serverChatCalls));
});

await run('a room whose host has no chat says so', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: readyHelper,
  rows: () => [row()],
  room: { ...memberRoom, chat: false },
}, async (win, doc) => {
  win.setView('rooms');
  await win.pollRoom();
  await settle();
  const t = doc.querySelector('#room-chat')?.textContent || '';
  check('an old host is explained, not a dead box', t.includes('without chat') && doc.querySelector('#room-chat .chat-form').hidden, t);
});

// The banner was rebuilt on every 2 s poll, so a press on Leave that spanned
// a tick went down on one button and up on its replacement: no click.
await run('the room banner is not rebuilt under the pointer', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: readyHelper,
  room: memberRoom,
  rows: () => [row()],
}, async (win, doc, scenario) => {
  const leave = doc.querySelector('#room-banner-leave');
  check('a room shows Leave in the banner', !!leave);
  await win.pollAll();
  check('an unchanged room keeps the same button', doc.querySelector('#room-banner-leave') === leave);
  leave.dispatchEvent(new win.Event('pointerdown', { bubbles: true }));
  scenario.room = { ...memberRoom, members: [...memberRoom.members, { address: '198.19.9.9', is_self: false }] };
  await win.pollAll();
  check('a change waits while Leave is pressed', doc.querySelector('#room-banner-leave') === leave);
  leave.dispatchEvent(new win.Event('pointerup', { bubbles: true }));
  await win.pollAll();
  check('and lands once the press ends', doc.querySelector('#room-banner').textContent.includes('3 members'),
    doc.querySelector('#room-banner').textContent);
});

await run('someone joining the room is a desktop notification, once', {
  status: running,
  games: [svenGame, lanGame()],
  lanHelper: readyHelper,
  room: memberRoom,
  rows: () => [row()],
}, async (win, doc, scenario) => {
  await win.pollAll();
  check('nobody is announced on the first look', !(scenario.notified || []).length, JSON.stringify(scenario.notified));
  scenario.room = { ...memberRoom, members: [...memberRoom.members, { address: '198.19.9.9', is_self: false }] };
  await win.pollAll();
  await win.pollAll();
  const joins = (scenario.notified || []).filter(t => t.includes('joined your LAN room'));
  check('a newcomer is told once', joins.length === 1, JSON.stringify(scenario.notified));
});

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

// Nobody should need a command or a diagnosis: what is wrong with Windows
// Firewall's rule is known when the room starts, said before any game fails,
// and fixed with a click where Lanthorn can fix it.
const ruleMissing = {
  ...memberRoom,
  firewall: fw({
    can_fix: true,
    rule_missing: true,
    rule_problems: ['Lanthorn\u2019s Windows Firewall rule is not there, so Windows will drop other players\u2019 connections to games you host. Press Fix it to add it for good.'],
  }),
};

await run('a missing Windows Firewall rule is said before any game fails, with a Fix button', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: ruleMissing,
  roomAfterFix: { ...memberRoom, firewall: fw({ can_fix: true }) },
  rows: () => [row()],
}, async (win, doc) => {
  const t = doc.querySelector('#room-firewall')?.textContent || '';
  check('the rule problem is shown with nothing dropped', t.includes('rule is not there'), t);
  check('with a Fix button, and no command pasted at the player', !!doc.querySelector('#room-fix') && !doc.querySelector('#room-firewall-command'));
  doc.querySelector('#room-fix')?.click();
  await settle();
  check('Fix calls the launcher', calls.includes('fix_room_firewall'));
  check('and the problem is gone', !(doc.querySelector('#room-firewall')?.textContent || '').includes('rule is not there'));
});

await run('a Windows setting only the player can change is explained in steps, with no button', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: {
    ...memberRoom,
    firewall: fw({
      can_fix: true,
      rule_problems: ['Windows is set to block all incoming connections on its Public network profile, which ignores every allow rule, Lanthorn\u2019s included. Turn off "Block all incoming connections" for that network under Windows Security \u2192 Firewall & network protection.'],
    }),
  },
  rows: () => [row()],
}, async (win, doc) => {
  const t = doc.querySelector('#room-firewall')?.textContent || '';
  check('the setting is named', t.includes('Block all incoming connections') && t.includes('Public'), t);
  check('there is no Fix button for what a rule cannot fix', !doc.querySelector('#room-fix'));
});

await run('a healthy firewall shows no firewall box at all', {
  status: running,
  games: [lanGame()],
  lanHelper: readyHelper,
  room: { ...memberRoom, firewall: fw({ can_fix: true, rule_problems: [], rule_missing: false }) },
  rows: () => [row()],
}, async (win, doc) => {
  check('nothing to say, so nothing said', !doc.querySelector('#room-firewall'));
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

// Lanthorn Host makes invite links too, in its own JavaScript. They must be the
// launcher's links byte for byte; launcher-core's
// `a_link_from_lanthorn_host_is_the_link_this_side_makes` holds the same string.
{
  const nodeJs = fs.readFileSync(new URL('../../crates/platform-agent/webui/app.js', import.meta.url), 'utf8');
  const start = nodeJs.indexOf('function inviteLink(');
  const end = nodeJs.indexOf('\n}\n', start) + 2;
  check('Lanthorn Host still has its invite builder', start >= 0);
  const inviteLink = new Function(nodeJs.slice(start, end) + '\nreturn inviteLink;')();
  const made = inviteLink({ mesh_destination: 'A1B2C3D4E5F60718293A4B5C6D7E8F90', game_id: 'counter-strike-16', name: "  Dust2 (EU)! ñ*' ~ok  " });
  check('a node-made invite is the launcher\'s own link',
    made === 'lanthorn://join/a1b2c3d4e5f60718293a4b5c6d7e8f90?game=counter-strike-16&name=Dust2%20%28EU%29%21%20%C3%B1%2A%27%20~ok', made);
}

// Lanthorn Host's Installed page polls every two seconds and rebuilds its rows.
// A row that got a new picture each poll blinked: the new <img> starts
// transparent and fades in. Each row must keep its picture, and a picture the
// browser has loaded once must not fade in again anywhere.
{
  const WEBUI = new URL('../../crates/platform-agent/webui/', import.meta.url).pathname;
  const nodeHtml = fs.readFileSync(`${WEBUI}index.html`, 'utf8').replace('<script src="app.js"></script>', '');
  const nodeJs = fs.readFileSync(`${WEBUI}app.js`, 'utf8');
  const ART = 'https://upload.wikimedia.org/x.png';
  const routes = {
    '/health': { ok: true, version: 'test' },
    '/capacity': { max_instances: 4, running: 0 },
    '/instances': [],
    '/storage': { path: '/x', free_bytes: 1e9, total_bytes: 2e9 },
    '/usage': [],
    '/orphans/summary': { count: 0, bytes: 0, skipped_recent: 0 },
    '/games': [{ id: 'g1', display_name: 'Game One', runnable: true, transport: 'udp', default_port: 27015, extra_ports: 0, art_url: ART }],
    '/content': {
      copies: [
        { game_id: 'g1', version: 'a', bytes: 1, modified_unix: 1, configured: true, used_by: [], unused: false },
        { game_id: 'g1', version: 'b', bytes: 1, modified_unix: 1, configured: false, used_by: [], unused: true },
      ],
      downloads: [{ game: 'g1', status: { state: 'running', since_secs: 1, bytes: 5 } }],
    },
  };
  const vc = new VirtualConsole();
  vc.on('jsdomError', e => consoleErrors.push('node webui: ' + e.message));
  const dom = new JSDOM(nodeHtml, { runScripts: 'outside-only', pretendToBeVisual: true, url: 'http://node.test/', virtualConsole: vc });
  const w = dom.window;
  w.localStorage.setItem('agent_token', 'x');
  // jsdom has no CSS.escape; the page uses it for attribute selectors only.
  w.CSS = { escape: v => String(v).replace(/[^A-Za-z0-9_-]/g, c => '\\' + c) };
  w.fetch = async (path) => {
    const body = routes[String(path).split('?')[0]];
    return { ok: body !== undefined, status: body !== undefined ? 200 : 404, statusText: '',
      headers: { get: () => 'application/json' }, json: async () => body ?? { error: 'no route' }, text: async () => JSON.stringify(body ?? {}) };
  };
  w.eval(nodeJs + '\n;window.__installed = () => state.installed; window.__poll = pollInstalled; window.__art = art;');
  w.document.dispatchEvent(new w.Event('DOMContentLoaded'));
  await settle();
  w.document.getElementById('tab-installed').click();
  await settle();
  const imgs = () => [...w.document.querySelectorAll('#installed-list .art-fg')];
  const first = imgs();
  check('the Installed page shows a picture per row', first.length === 3, String(first.length));
  for (const img of first) img.dispatchEvent(new w.Event('load'));
  w.__installed().timer && w.clearTimeout(w.__installed().timer);
  w.__installed().timer = null;
  await w.__poll();
  await settle();
  const second = imgs();
  check('a poll keeps every row\'s picture, so nothing blinks',
    second.length === first.length && second.every((img, i) => img === first[i]));
  check('a kept picture stays shown after a poll', second.every(img => img.classList.contains('loaded')));
  // Any other list that builds a new picture: one already loaded is shown at
  // once rather than faded in.
  const fresh = w.__art('g1', 'art-thumb').querySelector('.art-fg');
  check('a picture loaded once is not faded in again', fresh && fresh.classList.contains('loaded'));
  w.__installed().timer && w.clearTimeout(w.__installed().timer);
  w.close();
}

// Lanthorn Host's Minecraft form (MINECRAFT.md): nothing is created before the
// owner ticks the EULA, a size the host refuses cannot be submitted, and what
// is sent carries the build, the heap and the acceptance.
{
  const WEBUI = new URL('../../crates/platform-agent/webui/', import.meta.url).pathname;
  const nodeHtml = fs.readFileSync(`${WEBUI}index.html`, 'utf8').replace('<script src="app.js"></script>', '');
  const nodeJs = fs.readFileSync(`${WEBUI}app.js`, 'utf8');
  let verdict = { level: 'ok', messages: [], total_mb: 32000, available_mb: 24000, reserve_mb: 3200, limit_mb: 5632, host_cpus: 8 };
  const posts = [];
  const routes = {
    '/health': { ok: true, version: 'test' },
    '/capacity': { max_instances: 4, running: 0 },
    '/instances': [],
    '/storage': { path: '/x', free_bytes: 1e12, total_bytes: 2e12 },
    '/usage': [],
    '/orphans/summary': { count: 0, bytes: 0, skipped_recent: 0 },
    '/content': { copies: [], downloads: [] },
    '/games': [{ id: 'minecraft-java', display_name: 'Minecraft: Java Edition', runnable: true, transport: 'tcp', default_port: 25565,
      extra_ports: 0, console: true, minecraft: true, default_build: 'paper-26.3',
      eula: { name: 'Minecraft End User License Agreement', url: 'https://aka.ms/MinecraftEULA' } }],
    '/minecraft/versions': [{ build: 'paper-26.3', game_version: '26.3', loader: null, recommended: false },
      { build: 'paper-1.21.11', game_version: '1.21.11', loader: null, recommended: false }],
  };
  const vc = new VirtualConsole();
  vc.on('jsdomError', e => consoleErrors.push('node webui: ' + e.message));
  const dom = new JSDOM(nodeHtml, { runScripts: 'outside-only', pretendToBeVisual: true, url: 'http://node.test/', virtualConsole: vc });
  const w = dom.window;
  w.localStorage.setItem('agent_token', 'x');
  w.CSS = { escape: v => String(v).replace(/[^A-Za-z0-9_-]/g, c => '\\' + c) };
  w.fetch = async (path, opts = {}) => {
    const route = String(path).split('?')[0];
    if ((opts.method || 'GET') !== 'GET') {
      posts.push({ path: route, body: opts.body ? JSON.parse(opts.body) : null });
      const out = route === '/instances' ? { instance_id: 'x', state: 'running' } : {};
      return { ok: true, status: 200, statusText: '', headers: { get: () => 'application/json' }, json: async () => out, text: async () => '{}' };
    }
    const body = route === '/resources/check' ? verdict : routes[route];
    return { ok: body !== undefined, status: body !== undefined ? 200 : 404, statusText: '',
      headers: { get: () => 'application/json' }, json: async () => body ?? { error: 'no route' }, text: async () => JSON.stringify(body ?? {}) };
  };
  w.eval(nodeJs + '\n;window.__start = openStartDialog; window.__nodePoll = poll; window.__mod = onModeration;');
  w.document.dispatchEvent(new w.Event('DOMContentLoaded'));
  await settle();
  w.__start('minecraft-java');
  await settle();
  const d = w.document;
  const create = d.getElementById('mc-create');
  check('the Minecraft form opens instead of the plain one', !!create && !!d.getElementById('mc-version'));
  check('it offers the versions the node lists', d.getElementById('mc-version').options.length === 2 && d.getElementById('mc-version').value === 'paper-26.3');
  check('Create stays off until the EULA is ticked', create.disabled && /EULA/.test(create.title), create.title);
  const eula = d.getElementById('mc-eula');
  eula.checked = true;
  eula.dispatchEvent(new w.Event('change'));
  check('ticking the EULA turns Create on', !create.disabled, create.title);
  check('the EULA links to the licence', d.querySelector('.eula a').href === 'https://aka.ms/MinecraftEULA');

  verdict = { level: 'refuse', messages: ['Not enough free memory.'], total_mb: 8000, available_mb: 3000, reserve_mb: 1024, limit_mb: 7680, host_cpus: 4 };
  [...d.querySelectorAll('.chip-btn')].find(b => b.textContent === 'Large').click();
  await settle();
  check('a size the host refuses cannot be submitted', create.disabled && d.querySelector('.verdict').classList.contains('bad'));
  verdict = { level: 'ok', messages: [], total_mb: 32000, available_mb: 24000, reserve_mb: 3200, limit_mb: 5632, host_cpus: 8 };
  [...d.querySelectorAll('.chip-btn')].find(b => b.textContent === 'Medium').click();
  [...d.querySelectorAll('.chip-btn')].find(b => b.textContent === '50').click();
  await settle();
  d.getElementById('mc-name').value = 'Friday';
  d.querySelector('.mc-form').dispatchEvent(new w.Event('submit', { cancelable: true }));
  await settle();
  const sent = posts.find(p => p.path === '/instances');
  check('the create carries the build, heap, slots and the acceptance',
    sent && sent.body.content_version === 'paper-26.3' && sent.body.memory.max_mb === 4096 && sent.body.memory.min_mb === 2048
      && sent.body.max_players === 50 && Number.isInteger(sent.body.eula_accepted_at) && sent.body.minecraft.online_mode === true,
    JSON.stringify(sent && sent.body));
  check('a daily backup is set by default', posts.some(p => p.path.endsWith('/backup-schedule') && p.body.schedule.every_hours === 24));

  // Kick, ban and operators: online players get buttons, the ban list gets
  // Unban, and a button sends exactly that action.
  routes['/instances'] = [{ instance_id: 'mc1', game_id: 'minecraft-java', name: 'Friday', state: 'running', port: 27300, ports: [] }];
  routes['/instances/mc1/moderation'] = {
    actions: ['kick', 'ban', 'unban', 'op', 'deop'].map(a => ({ action: a, target: 'minecraft-name' })),
    ban_list: 'game-file', banned: [{ target: 'Griefer', reason: 'broke spawn' }], ops: [{ target: 'Notch' }],
  };
  routes['/instances/mc1/players'] = [{ name: 'Alex', score: 0, connected_secs: 0 }];
  await w.__nodePoll();
  await settle();
  w.__mod('mc1');
  await settle();
  const m = d.getElementById('moderation-dialog');
  const rows = m ? [...m.querySelectorAll('.mod-row')].map(r => r.textContent) : [];
  check('the dialog lists who is on, the banned and the operators',
    rows.some(t => t.startsWith('Alex')) && rows.some(t => t.includes('Griefer')) && rows.some(t => t.startsWith('Notch')), JSON.stringify(rows));
  posts.length = 0;
  [...m.querySelectorAll('.mod-row')].find(r => r.textContent.startsWith('Alex')).querySelector('button').click();
  await settle();
  const kick = posts.find(p => p.path === '/instances/mc1/moderation');
  check('a player\'s Kick button sends a kick for that name', kick && kick.body.action === 'kick' && kick.body.target === 'Alex', JSON.stringify(kick));
  w.close();
}

for (const e of consoleErrors) failures.push(`uncaught: ${e}`);

if (failures.length) {
  console.log('FAIL');
  for (const f of failures) console.log('  - ' + f);
  process.exit(1);
}
console.log(`OK — ${new Set(calls).size} commands exercised: ${[...new Set(calls)].join(', ')}`);
