// Lists the packs in packs/index.json. Every value comes from the index, so
// it only ever goes in as text; art is shown only from the hosts a pack's
// `art` may name (pack.rs ART_HOSTS), the same rule the launcher applies.
(function () {
  const ART_HOSTS = ['upload.wikimedia.org', 'cdn.cloudflare.steamstatic.com', 'shared.cloudflare.steamstatic.com', 'cdn.akamai.steamstatic.com'];
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };
  const artOk = (url) => {
    try { const u = new URL(url); return u.protocol === 'https:' && ART_HOSTS.includes(u.host); } catch (_) { return false; }
  };
  fetch('packs/index.json', { cache: 'no-cache' })
    .then(r => { if (!r.ok) throw new Error('index.json answered ' + r.status); return r.json(); })
    .then(index => {
      const packs = (index.packs || []).slice().sort((a, b) => a.display_name.localeCompare(b.display_name));
      document.getElementById('count').textContent = packs.length + (packs.length === 1 ? ' pack' : ' packs');
      const grid = document.getElementById('packs');
      for (const p of packs) {
        const card = el('article', 'card');
        const pic = el('div', 'art');
        if (p.art && artOk(p.art)) {
          const img = el('img');
          img.src = p.art;
          img.alt = '';
          img.loading = 'lazy';
          img.referrerPolicy = 'no-referrer';
          img.onerror = () => img.remove();
          pic.appendChild(img);
        }
        pic.appendChild(el('span', 'letters', (p.display_name || p.id).slice(0, 2)));
        card.appendChild(pic);
        const body = el('div', 'body');
        body.appendChild(el('h3', null, p.display_name));
        const tags = el('p', 'tags');
        tags.appendChild(el('span', 'tag', p.id));
        if (p.signed) tags.appendChild(el('span', 'tag ok', 'signed'));
        if (p.lan) tags.appendChild(el('span', 'tag', 'LAN rooms'));
        body.appendChild(tags);
        const link = el('a', 'file', p.file);
        link.href = 'packs/' + encodeURIComponent(p.file);
        body.appendChild(link);
        const sha = el('p', 'sha', 'sha256 ' + p.sha256.slice(0, 16) + '…');
        sha.title = p.sha256;
        body.appendChild(sha);
        card.appendChild(body);
        grid.appendChild(card);
      }
    })
    .catch(err => {
      document.getElementById('count').textContent = 'Packs unavailable';
      const e = document.getElementById('error');
      e.hidden = false;
      e.textContent = 'Could not read the list: ' + err.message;
    });
})();
