// An invite on the web (ROADMAP item 2): https://…/join/#<hash>?game=…&name=…&room=1.
// The invite rides in the fragment, which a browser never sends to the web
// host. Read with the same rules as launcher-core's invite::parse: anything
// that is not one is refused, never repaired, and every value goes in as text.
(function () {
  const raw = location.hash.replace(/^#/, '');
  const [hashPart, query] = raw.split('?');
  const hash = (hashPart || '').replace(/\/+$/, '').toLowerCase();
  const show = (id, on) => { document.getElementById(id).hidden = !on; };
  const bad = () => {
    document.getElementById('title').textContent = 'This invite is damaged';
    document.getElementById('what').textContent = 'Ask whoever sent it for the link again.';
    show('bad', true);
  };
  if (!/^[0-9a-f]{32}$/.test(hash)) { bad(); return; }
  let game = null, name = null, room = false;
  for (const pair of (query || '').split('&').filter(Boolean)) {
    const i = pair.indexOf('=');
    const k = i < 0 ? pair : pair.slice(0, i);
    let v;
    try { v = decodeURIComponent((i < 0 ? '' : pair.slice(i + 1)).replace(/\+/g, ' ')); }
    catch (_) { bad(); return; }
    if (k === 'game') {
      if (!/^[a-z0-9-]{1,24}$/.test(v)) { bad(); return; }
      game = v;
    } else if (k === 'name') {
      const clean = v.replace(/[\u0000-\u001f\u007f-\u009f]/g, '').trim();
      if (clean) name = Array.from(clean).slice(0, 64).join('');
    } else if (k === 'room') {
      room = v === '1';
    }
  }
  const enc = s => encodeURIComponent(s).replace(/[!'()*]/g, c => '%' + c.charCodeAt(0).toString(16).toUpperCase());
  let link = 'lanthorn://join/' + hash;
  const q = [];
  if (game) q.push('game=' + enc(game));
  if (name) q.push('name=' + enc(name));
  if (room) q.push('room=1');
  if (q.length) link += '?' + q.join('&');

  const what = room ? 'LAN room' : 'server';
  document.getElementById('title').textContent = name ? name : 'You\'re invited to a ' + what;
  document.getElementById('what').textContent = name
    ? 'You\'re invited to this ' + what + '. Open it in Lanthorn to see it and join.'
    : 'Open it in Lanthorn to see it and join.';
  if (game) {
    const g = document.getElementById('game');
    g.textContent = 'Game: ' + game;
    g.hidden = false;
    fetch('../packs/index.json', { cache: 'no-cache' })
      .then(r => r.ok ? r.json() : null)
      .then(index => {
        const p = index && (index.packs || []).find(p => p.id === game);
        if (p && p.display_name) g.textContent = 'Game: ' + p.display_name;
      })
      .catch(() => {});
  }
  document.getElementById('open').href = link;
  document.getElementById('address').textContent = link;
  document.title = (name || 'Lanthorn') + ' — Lanthorn invite';
  show('ok', true);
})();
