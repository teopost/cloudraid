// cloudraid - interfaccia di amministrazione (senza librerie esterne)
'use strict';

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

const state = {
  status: null,        // ultima risposta di api/status
  view: 'panoramica',
  dir: '',             // cartella aperta nella vista File
  search: '',
  uploads: [],         // { name, dest, size, sent, phase: 'invio'|'attesa'|'fatto'|'errore', msg }
  timer: null,
  lastJobId: null,
};

// ---------------------------------------------------------------------------------------------
// Utilità

const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

function bytes(n) {
  if (n == null) return '-';
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return i === 0 ? `${n} B` : `${v.toLocaleString('it-IT', { maximumFractionDigits: v < 10 ? 1 : 0 })} ${u[i]}`;
}

const num = (n) => Number(n || 0).toLocaleString('it-IT');
const dateFmt = new Intl.DateTimeFormat('it-IT', { day: '2-digit', month: '2-digit', year: 'numeric', hour: '2-digit', minute: '2-digit' });
const when = (sec) => (sec ? dateFmt.format(new Date(sec * 1000)) : '-');

function ago(sec) {
  if (!sec) return '-';
  const d = Math.max(0, Math.round(Date.now() / 1000 - sec));
  if (d < 60) return 'adesso';
  if (d < 3600) return `${Math.round(d / 60)} min fa`;
  if (d < 86400) return `${Math.round(d / 3600)} h fa`;
  return `${Math.round(d / 86400)} g fa`;
}

function duration(a, b) {
  if (!a) return '-';
  const d = Math.max(0, (b || Date.now() / 1000) - a);
  if (d < 60) return `${Math.round(d)} s`;
  if (d < 3600) return `${Math.floor(d / 60)} min ${Math.round(d % 60)} s`;
  return `${Math.floor(d / 3600)} h ${Math.round((d % 3600) / 60)} min`;
}

const ICON = {
  plus: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>',
  ok: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m9 12 2 2 4-4"/></svg>',
  warn: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/></svg>',
  bad: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="m15 9-6 6M9 9l6 6"/></svg>',
  folder: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>',
  file: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/></svg>',
  download: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3"/></svg>',
  trash: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/></svg>',
  upload: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M17 8l-5-5-5 5M12 3v12"/></svg>',
  refresh: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/></svg>',
};

function toast(msg, kind = '') {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.textContent = msg;
  $('#toasts').append(el);
  setTimeout(() => el.remove(), kind === 'bad' ? 8000 : 4500);
}

function confirmDialog(title, text, okLabel = 'Conferma', danger = true) {
  const dlg = $('#confirm');
  $('#confirm-title').textContent = title;
  $('#confirm-text').textContent = text;
  const ok = $('#confirm-ok');
  ok.textContent = okLabel;
  ok.className = `btn ${danger ? 'btn-danger' : 'btn-primary'}`;
  dlg.returnValue = '';
  dlg.showModal();
  return new Promise((resolve) => dlg.addEventListener('close', () => resolve(dlg.returnValue === 'si'), { once: true }));
}

/** Imposta le larghezze delle barre (data-w in %): niente stili inline, per la Content-Security-Policy. */
function applyBars(root) {
  $$('[data-w]', root).forEach((el) => { el.style.width = `${Math.min(100, Math.max(0, Number(el.dataset.w)))}%`; });
}

// ---------------------------------------------------------------------------------------------
// API

async function api(path, opts = {}) {
  const headers = { 'X-Cloudraid': '1', ...(opts.headers || {}) };
  let body = opts.body;
  if (body && !(body instanceof Blob) && typeof body !== 'string') {
    body = JSON.stringify(body);
    headers['Content-Type'] = 'application/json';
  }
  const res = await fetch(path, { method: opts.method || 'GET', headers, body, credentials: 'same-origin' });
  let data = null;
  try { data = await res.json(); } catch { /* risposta senza JSON */ }
  if (res.status === 401 && path !== 'api/login') {
    showLogin();
    throw new Error('sessione scaduta');
  }
  if (!res.ok) throw new Error((data && data.error) || `errore ${res.status}`);
  return data;
}

async function startJob(path, body, okMsg) {
  try {
    await api(path, { method: path === 'api/files' ? 'DELETE' : 'POST', body });
    if (okMsg) toast(okMsg);
    await refresh();
  } catch (e) {
    toast(e.message, 'bad');
  }
}

// ---------------------------------------------------------------------------------------------
// Accesso

function showLogin(hint) {
  clearTimeout(state.timer);
  $('#app').hidden = true;
  $('#login').hidden = false;
  $('#login-hint').hidden = !hint;
  $('#login-hint').textContent = hint || '';
  $('#password').focus();
}

function showApp() {
  $('#login').hidden = true;
  $('#app').hidden = false;
}

$('#login-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const btn = $('#login-btn');
  const err = $('#login-error');
  err.hidden = true;
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner"></span> Apertura del repository…';
  try {
    await api('api/login', { method: 'POST', body: { password: $('#password').value } });
    $('#password').value = '';
    showApp();
    route();
    await refresh();
  } catch (e) {
    err.textContent = e.message === 'password errata' ? 'Password errata.' : e.message;
    err.hidden = false;
  } finally {
    btn.disabled = false;
    btn.textContent = 'Accedi';
  }
});

$('#logout').addEventListener('click', async () => {
  try { await api('api/logout', { method: 'POST' }); } catch { /* già uscito */ }
  showLogin();
});

// ---------------------------------------------------------------------------------------------
// Aggiornamento periodico

async function refresh() {
  clearTimeout(state.timer);
  try {
    const st = await api('api/status');
    const prevJob = state.status && state.status.job;
    state.status = st;
    // notifica la fine di un'operazione seguita da questa pagina
    if (prevJob && prevJob.state === 'running' && st.job && st.job.id === prevJob.id && st.job.state !== 'running') {
      toast(`${st.job.label}: ${st.job.message}`, st.job.state === 'ok' ? 'ok' : 'bad');
    }
    render();
  } catch (e) {
    if (e.message !== 'sessione scaduta') toast(`Aggiornamento non riuscito: ${e.message}`, 'bad');
  }
  if (!$('#app').hidden) {
    const busy = state.status && state.status.busy;
    state.timer = setTimeout(refresh, busy ? 1500 : 20000);
  }
}

// ---------------------------------------------------------------------------------------------
// Navigazione

function route() {
  const hash = location.hash.replace(/^#/, '');
  const [view, ...rest] = hash.split('/');
  state.view = ['panoramica', 'file', 'spazi', 'manutenzione', 'attivita'].includes(view) ? view : 'panoramica';
  if (state.view === 'file') state.dir = decodeURIComponent(rest.join('/'));
  $$('.tabs a').forEach((a) => a.classList.toggle('active', a.dataset.view === state.view));
  $$('.view').forEach((v) => { v.hidden = v.id !== `view-${state.view}`; });
  render();
}
window.addEventListener('hashchange', route);

function render() {
  const st = state.status;
  if (!st) return;
  const busy = st.busy && st.job;
  $('#busy').hidden = !busy;
  if (busy) {
    const pct = st.job.total ? ` ${Math.floor((st.job.done / st.job.total) * 100)}%` : '';
    $('#busy-label').textContent = `${st.job.label}${pct}`;
  }
  const view = $(`#view-${state.view}`);
  const html = { panoramica: renderOverview, file: renderFiles, spazi: renderSpaces, manutenzione: renderMaintenance, attivita: renderActivity }[state.view](st);
  // non ridisegnare la vista File mentre si scrive nella ricerca
  const active = document.activeElement;
  const keepSearch = active && active.id === 'search';
  const caret = keepSearch ? active.selectionStart : 0;
  view.innerHTML = html;
  applyBars(view);
  if (keepSearch) {
    const s = $('#search');
    s.focus();
    s.setSelectionRange(caret, caret);
  }
}

// ---------------------------------------------------------------------------------------------
// Panoramica

function lastScrub(history) {
  return (history || []).find((j) => (j.kind === 'scrub' || j.kind === 'repair') && j.report);
}

function health(snap, history) {
  // un backend dismesso e già svuotato può essere spento senza danni
  const down = snap.backends.filter((b) => !b.available && !(b.retired && b.shards === 0)).length;
  const m = snap.parity_shards;
  const scrub = lastScrub(history);
  if (down > m) {
    return ['bad', 'Dati non leggibili', `${down} backend non disponibili su ${snap.backends.length}: ne servono almeno ${snap.data_shards} per ricostruire i dati. Controlla gli account.`];
  }
  if (scrub && scrub.report.lost_chunks > 0) {
    return ['bad', 'Dati persi', `L'ultima verifica (${when(scrub.finished)}) ha trovato ${scrub.report.lost_chunks} blocchi irrecuperabili in ${scrub.report.lost_files.length} file.`];
  }
  if (down > 0) {
    return ['warn', 'Ridondanza ridotta', `${down} backend non disponibil${down === 1 ? 'e' : 'i'}: i dati restano leggibili, ma ora se ne possono perdere solo altri ${m - down}. Sostituisci l'account e lancia Verifica e ripara.`];
  }
  if (scrub && scrub.report.degraded_chunks > 0 && !(scrub.report.repair && scrub.report.unrepaired_shards === 0)) {
    return ['warn', 'Frammenti da riparare', `L'ultima verifica ha trovato ${scrub.report.degraded_chunks} blocchi con frammenti mancanti o danneggiati: lancia Verifica e ripara.`];
  }
  const tail = scrub ? `Ultima verifica ${ago(scrub.finished)}: tutto integro.` : 'Nessuna verifica ancora eseguita: conviene lanciarne una dalla Manutenzione.';
  return ['ok', 'Ridondanza piena', `Tutti i ${snap.backends.length} backend sono disponibili e si possono perdere fino a ${m} backend senza perdere dati. ${tail}`];
}

function renderOverview(st) {
  const snap = st.snapshot;
  if (!snap) return '<div class="card empty">Stato non ancora disponibile.</div>';
  const [kind, title, text] = health(snap, st.history);
  const used = snap.backends.reduce((a, b) => a + b.bytes, 0);
  const maxBytes = Math.max(1, ...snap.backends.map((b) => b.bytes));
  const scrub = lastScrub(st.history);
  const k = snap.data_shards;
  const m = snap.parity_shards;
  const overhead = ((k + m) / k).toLocaleString('it-IT', { maximumFractionDigits: 2 });

  const dis = st.busy ? 'disabled' : '';
  const backendState = (b) => {
    if (b.retired && b.shards === 0) return ['ok', 'dismesso'];
    if (b.retired) return ['warn', 'in dismissione'];
    return b.available ? ['ok', 'disponibile'] : ['bad', 'non disponibile'];
  };
  const backendActions = (b) => {
    if (!b.retired) {
      return `<div class="card-actions"><button class="btn btn-ghost btn-sm" data-action="retire" data-name="${esc(b.name)}" ${dis}>Dismetti</button></div>`;
    }
    const note = b.shards === 0
      ? '<p class="note">Vuoto: ora puoi toglierlo dalla configurazione o sostituirlo.</p>'
      : `<p class="note">Non riceve più frammenti nuovi; ne restano ${num(b.shards)} da spostare.</p>`;
    return `${note}<div class="card-actions">
      ${b.shards > 0 ? `<button class="btn btn-sm" data-action="retire-resume" data-name="${esc(b.name)}" ${dis}>Riprendi lo svuotamento</button>` : ''}
      <button class="btn btn-ghost btn-sm" data-action="unretire" data-name="${esc(b.name)}" ${dis}>Rimetti in servizio</button></div>`;
  };
  const backends = snap.backends.map((b) => `
    <div class="card backend ${b.available || (b.retired && b.shards === 0) ? '' : 'down'}">
      <div class="backend-head">
        <div class="backend-name"><span>${esc(b.name)}</span><span class="tag">${esc(typeLabel(b.kind))}</span></div>
        <span class="pill ${backendState(b)[0]}">${backendState(b)[1]}</span>
      </div>
      <div class="backend-loc" title="${esc(b.location)}">${esc(b.location)}</div>
      <div class="bar"><div data-w="${(b.bytes / maxBytes) * 100}"></div></div>
      <div class="backend-nums"><span>${num(b.shards)} frammenti</span><span>${bytes(b.bytes)}</span></div>
      ${b.latency_ms != null ? `<div class="muted small">lettura di un frammento: ${b.latency_ms < 1000 ? `${Math.round(b.latency_ms)} ms` : `${(b.latency_ms / 1000).toFixed(1)} s`}</div>` : ''}
      ${backendActions(b)}
    </div>`).join('');

  const locks = snap.locks.length ? `
    <div class="card">
      <h3>Lock presenti</h3>
      <p class="muted">Un processo sta modificando il repository, oppure ne ha lasciato uno interrompendosi.</p>
      <ul class="list-plain">${snap.locks.map((l) => `<li><span>${esc(l.host)} <span class="muted">pid ${l.pid}</span></span><span class="pill ${l.stale ? 'warn' : 'run'}">${l.stale ? 'scaduto' : 'attivo'} · ${duration(st.now - l.age, st.now)}</span></li>`).join('')}</ul>
    </div>` : '';

  return `
    <div class="banner ${kind}">${ICON[kind]}<div><strong>${esc(title)}</strong><p>${esc(text)}</p></div></div>
    <div class="stats">
      <div class="card stat"><div class="label">File</div><div class="value">${num(snap.files.length)}</div><div class="sub">${num(snap.chunks)} blocchi</div></div>
      <div class="card stat"><div class="label">Dati</div><div class="value">${bytes(snap.total_size)}</div><div class="sub">${bytes(snap.stored_size)} dopo la deduplicazione</div></div>
      <div class="card stat"><div class="label">Spazio occupato</div><div class="value">${bytes(used)}</div><div class="sub">sui backend, ${overhead}× con la parità</div></div>
      <div class="card stat"><div class="label">Schema</div><div class="value">${k} + ${m}</div><div class="sub">si possono perdere ${m} backend</div></div>
      <div class="card stat"><div class="label">Ultima verifica</div><div class="value">${scrub ? ago(scrub.finished) : 'mai'}</div><div class="sub">${scrub ? esc(scrub.message) : 'da Manutenzione'}</div></div>
    </div>
    <div class="section-head">
      <h2>Backend</h2>
      <span class="muted">Indice generazione ${num(snap.generation)} · stato delle ${new Date(snap.taken * 1000).toLocaleTimeString('it-IT', { hour: '2-digit', minute: '2-digit' })}
        <button class="btn btn-ghost btn-sm" data-action="refresh-backends" ${st.busy ? 'disabled' : ''}>${ICON.refresh} Ricontrolla</button></span>
    </div>
    <div class="backends">${backends}</div>
    ${locks}`;
}

// ---------------------------------------------------------------------------------------------
// File

function listing(files, dir, emptyDirs = []) {
  const prefix = dir ? `${dir}/` : '';
  const dirs = new Map();
  const items = [];
  // cartelle create dal mount, anche vuote
  for (const p of emptyDirs) {
    if (!p.startsWith(prefix)) continue;
    const name = p.slice(prefix.length).split('/')[0];
    if (name && !dirs.has(name)) dirs.set(name, { name, count: 0, size: 0, mtime: 0 });
  }
  for (const f of files) {
    if (!f.path.startsWith(prefix)) continue;
    const rest = f.path.slice(prefix.length);
    const slash = rest.indexOf('/');
    if (slash >= 0) {
      const name = rest.slice(0, slash);
      const d = dirs.get(name) || { name, count: 0, size: 0, mtime: 0 };
      d.count += 1;
      d.size += f.size;
      d.mtime = Math.max(d.mtime, f.mtime || 0);
      dirs.set(name, d);
    } else {
      items.push({ ...f, name: rest });
    }
  }
  const sorted = [...dirs.values()].sort((a, b) => a.name.localeCompare(b.name, 'it'));
  items.sort((a, b) => a.name.localeCompare(b.name, 'it'));
  return { dirs: sorted, files: items };
}

const fileHref = (dir) => `#file/${dir.split('/').map(encodeURIComponent).join('/')}`;

function renderFiles(st) {
  const snap = st.snapshot;
  if (!snap) return '<div class="card empty">Stato non ancora disponibile.</div>';
  const q = state.search.trim().toLowerCase();
  const parts = state.dir ? state.dir.split('/') : [];
  const crumbs = [`<a href="#file/">Archivio</a>`]
    .concat(parts.map((p, i) => (i === parts.length - 1 ? `<span class="here">${esc(p)}</span>` : `<a href="${fileHref(parts.slice(0, i + 1).join('/'))}">${esc(p)}</a>`)))
    .join('<span class="sep">/</span>');

  let rows;
  if (q) {
    const found = snap.files.filter((f) => f.path.toLowerCase().includes(q)).slice(0, 500);
    rows = found.map((f) => fileRow(f, f.path)).join('');
    if (!found.length) rows = `<tr><td colspan="5" class="empty">Nessun file contiene “${esc(state.search)}”.</td></tr>`;
  } else {
    const { dirs, files } = listing(snap.files, state.dir, snap.dirs || []);
    rows = dirs.map((d) => {
      const path = state.dir ? `${state.dir}/${d.name}` : d.name;
      return `<tr>
        <td><div class="fname dir">${ICON.folder}<a class="dirlink" href="${fileHref(path)}">${esc(d.name)}</a></div></td>
        <td class="num">${bytes(d.size)}</td>
        <td class="hide-sm">${d.mtime ? when(d.mtime) : '-'}</td>
        <td class="num hide-sm">${num(d.count)} file</td>
        <td class="actions"><button class="icon-btn danger" title="Elimina la cartella" data-action="delete" data-path="${esc(path)}" data-dir="1">${ICON.trash}</button></td>
      </tr>`;
    }).join('') + files.map((f) => fileRow(f, f.name)).join('');
    if (!dirs.length && !files.length) {
      rows = `<tr><td colspan="5" class="empty">${state.dir ? 'Cartella vuota.' : 'Archivio vuoto: carica i primi file con il pulsante qui sopra o trascinandoli nella pagina.'}</td></tr>`;
    }
  }

  return `
    <div class="section-head">
      <div class="crumbs">${crumbs}</div>
      <div class="toolbar">
        <input id="search" class="search" type="search" placeholder="Cerca nell'archivio" value="${esc(state.search)}">
        <button class="btn btn-primary" data-action="upload">${ICON.upload} Carica</button>
      </div>
    </div>
    ${renderUploads()}
    <div class="table-wrap">
      <table>
        <thead><tr><th>Nome</th><th class="num">Dimensione</th><th class="hide-sm">Modificato</th><th class="num hide-sm">Contenuto</th><th></th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
    <p class="muted">${num(snap.files.length)} file in tutto l'archivio, ${bytes(snap.total_size)}. I file vengono ricostruiti e decifrati dal server al momento dello scaricamento.</p>`;
}

function fileRow(f, label) {
  return `<tr>
    <td><div class="fname">${ICON.file}<span title="${esc(f.path)}">${esc(label)}</span></div></td>
    <td class="num">${bytes(f.size)}</td>
    <td class="hide-sm">${f.mtime ? when(f.mtime) : '-'}</td>
    <td class="num hide-sm">${num(f.chunks)} ${f.chunks === 1 ? 'blocco' : 'blocchi'}</td>
    <td class="actions">
      <a class="icon-btn" title="Scarica" href="api/download?path=${encodeURIComponent(f.path)}" download data-action="download">${ICON.download}</a>
      <button class="icon-btn danger" title="Elimina" data-action="delete" data-path="${esc(f.path)}">${ICON.trash}</button>
    </td>
  </tr>`;
}

function renderUploads() {
  if (!state.uploads.length) return '';
  const label = { invio: 'invio al server', attesa: 'cifratura e distribuzione', fatto: 'fatto', errore: 'errore', coda: 'in coda' };
  const rows = state.uploads.map((u) => {
    const pct = u.phase === 'invio' ? (u.size ? (u.sent / u.size) * 100 : 0) : u.phase === 'coda' ? 0 : 100;
    return `<div class="upload-row">
      <span class="name">${esc(u.dest)} <span class="muted">${bytes(u.size)}</span></span>
      <span class="pill ${u.phase === 'fatto' ? 'ok' : u.phase === 'errore' ? 'bad' : 'run'}">${label[u.phase]}${u.msg ? `: ${esc(u.msg)}` : ''}</span>
      <div class="bar ${u.phase === 'attesa' ? 'indet' : ''}"><div data-w="${pct}"></div></div>
    </div>`;
  }).join('');
  const done = state.uploads.every((u) => u.phase === 'fatto' || u.phase === 'errore');
  return `<div class="card"><div class="section-head"><h3>Caricamenti</h3>${done ? '<button class="btn btn-ghost btn-sm" data-action="clear-uploads">Chiudi</button>' : ''}</div><div class="uploads">${rows}</div></div>`;
}

function sendFile(u, file) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', `api/upload?path=${encodeURIComponent(u.dest)}`);
    xhr.setRequestHeader('X-Cloudraid', '1');
    xhr.upload.onprogress = (e) => { u.sent = e.loaded; if (state.view === 'file') render(); };
    xhr.onload = () => {
      let data = null;
      try { data = JSON.parse(xhr.responseText); } catch { /* niente JSON */ }
      if (xhr.status === 401) { showLogin(); reject(new Error('sessione scaduta')); return; }
      if (xhr.status >= 200 && xhr.status < 300) resolve(data);
      else reject(new Error((data && data.error) || `errore ${xhr.status}`));
    };
    xhr.onerror = () => reject(new Error('connessione interrotta'));
    xhr.send(file);
  });
}

async function waitJob(id) {
  for (;;) {
    await new Promise((r) => setTimeout(r, 1000));
    const st = await api('api/status');
    state.status = st;
    const done = (st.history || []).find((j) => j.id === id);
    if (done) return done;
    if (!st.job || st.job.id !== id) return null;
  }
}

async function uploadFiles(fileList) {
  const files = Array.from(fileList);
  if (!files.length) return;
  const queue = files.map((f) => ({ file: f, u: { name: f.name, dest: state.dir ? `${state.dir}/${f.name}` : f.name, size: f.size, sent: 0, phase: 'coda', msg: '' } }));
  const existing = new Set((state.status.snapshot ? state.status.snapshot.files : []).map((f) => f.path));
  const overwrite = queue.filter((q) => existing.has(q.u.dest));
  if (overwrite.length && !(await confirmDialog('Sostituire i file?', `${overwrite.length} file esistono già nell'archivio e verranno sostituiti.`, 'Sostituisci'))) return;
  state.uploads = state.uploads.filter((u) => u.phase !== 'fatto' && u.phase !== 'errore').concat(queue.map((q) => q.u));
  render();
  // un caricamento alla volta: il server esegue un'operazione alla volta
  for (const { file, u } of queue) {
    try {
      while (state.status && state.status.busy) {
        await new Promise((r) => setTimeout(r, 1500));
        state.status = await api('api/status');
      }
      u.phase = 'invio';
      render();
      const res = await sendFile(u, file);
      u.phase = 'attesa';
      render();
      const job = await waitJob(res.job.id);
      if (job && job.state === 'ok') u.phase = 'fatto';
      else { u.phase = 'errore'; u.msg = job ? job.message : 'esito sconosciuto'; }
    } catch (e) {
      u.phase = 'errore';
      u.msg = e.message;
    }
    render();
  }
  const errors = state.uploads.filter((u) => u.phase === 'errore').length;
  toast(errors ? `Caricamento finito con ${errors} errori` : 'Caricamento completato', errors ? 'bad' : 'ok');
  await refresh();
}

$('#upload-input').addEventListener('change', (e) => {
  uploadFiles(e.target.files);
  e.target.value = '';
});

// trascinare i file nella pagina
let dragDepth = 0;
document.addEventListener('dragenter', (e) => {
  if (state.view !== 'file' || $('#app').hidden || !e.dataTransfer || !Array.from(e.dataTransfer.types).includes('Files')) return;
  dragDepth += 1;
  $('#drop-target').textContent = state.dir || 'Archivio';
  $('#dropzone').hidden = false;
});
document.addEventListener('dragleave', () => {
  dragDepth = Math.max(0, dragDepth - 1);
  if (!dragDepth) $('#dropzone').hidden = true;
});
document.addEventListener('dragover', (e) => { if (state.view === 'file') e.preventDefault(); });
document.addEventListener('drop', (e) => {
  if (state.view !== 'file') return;
  e.preventDefault();
  dragDepth = 0;
  $('#dropzone').hidden = true;
  if (e.dataTransfer && e.dataTransfer.files.length) uploadFiles(e.dataTransfer.files);
});

// ---------------------------------------------------------------------------------------------
// Manutenzione

/** Che cosa conta l'avanzamento di un'operazione. */
function jobUnit(kind) {
  return { gc: 'frammenti', rebalance: 'spostamenti', retire: 'frammenti' }[kind] || 'blocchi';
}

function reportBlock(j) {
  const r = j.report;
  if (!r) return '';
  const lost = r.lost_files.length ? `<p class="muted">File danneggiati:</p><ul class="lost">${r.lost_files.slice(0, 50).map((f) => `<li>${esc(f)}</li>`).join('')}</ul>` : '';
  return `<dl class="kv">
      <div><dt>Blocchi controllati</dt><dd>${num(r.chunks)}</dd></div>
      <div><dt>Frammenti integri</dt><dd class="ok">${num(r.shards_ok)}</dd></div>
      <div><dt>Mancanti</dt><dd class="${r.shards_missing ? 'warn' : ''}">${num(r.shards_missing)}</dd></div>
      <div><dt>Danneggiati</dt><dd class="${r.shards_corrupt ? 'warn' : ''}">${num(r.shards_corrupt)}</dd></div>
      <div><dt>Blocchi degradati</dt><dd class="${r.degraded_chunks ? 'warn' : ''}">${num(r.degraded_chunks)}</dd></div>
      ${r.repair ? `<div><dt>Riparati</dt><dd class="${r.repaired_shards ? 'ok' : ''}">${num(r.repaired_shards)}</dd></div><div><dt>Non riparabili</dt><dd class="${r.unrepaired_shards ? 'bad' : ''}">${num(r.unrepaired_shards)}</dd></div>` : ''}
      <div><dt>Blocchi persi</dt><dd class="${r.lost_chunks ? 'bad' : ''}">${num(r.lost_chunks)}</dd></div>
    </dl>${lost}`;
}

function renderMaintenance(st) {
  const snap = st.snapshot;
  const job = st.job;
  const running = st.busy && job;
  const scrub = lastScrub(st.history);
  const pct = running && job.total ? (job.done / job.total) * 100 : 0;
  const current = running ? `
    <div class="card job">
      <div class="job-head"><h3>${esc(job.label)}</h3><span class="pill run">in corso da ${duration(job.started, st.now)}</span></div>
      <div class="bar lg ${job.total ? '' : 'indet'}"><div data-w="${pct}"></div></div>
      <span class="muted">${job.total ? `${num(job.done)} ${jobUnit(job.kind)} su ${num(job.total)} (${Math.floor(pct)}%)` : 'In preparazione…'}</span>
    </div>` : '';
  const locks = snap ? snap.locks : [];
  const dis = st.busy ? 'disabled' : '';
  const lastOf = (...kinds) => (st.history || []).find((j) => kinds.includes(j.kind));
  const lastLine = (j, none) => (j
    ? `<p class="note"><span class="pill ${j.state === 'ok' ? 'ok' : 'bad'}">${when(j.finished)}</span> ${esc(j.label)}: ${esc(j.message)}</p>`
    : `<p class="note">${none}</p>`);
  return `
    ${current}
    <div class="grid-2">
      <div class="card">
        <h3>Verifica dei frammenti (scrub)</h3>
        <p class="muted">Legge tutti i frammenti da tutti i backend e ne controlla l'impronta. Con la riparazione ricostruisce
          quelli mancanti o danneggiati sul backend originale, oppure su un altro libero. Con i cloud lenti può richiedere molto tempo.</p>
        <div class="card-actions">
          <button class="btn" data-action="scrub" ${dis}>Verifica</button>
          <button class="btn btn-primary" data-action="repair" ${dis}>Verifica e ripara</button>
        </div>
      </div>
      <div class="card">
        <h3>Ricontrolla backend e indice</h3>
        <p class="muted">Verifica di nuovo quali backend sono raggiungibili e rilegge l'indice più recente, per esempio dopo aver
          sostituito un account o usato la riga di comando.</p>
        <div class="card-actions"><button class="btn" data-action="refresh-backends" ${dis}>${ICON.refresh} Ricontrolla</button></div>
      </div>
    </div>
    <div class="grid-2">
      <div class="card">
        <h3>Pulizia dei frammenti orfani</h3>
        <p class="muted">Cerca sui backend i frammenti che l'indice non usa più: avanzi di caricamenti interrotti, copie
          vecchie rimaste su un backend tornato dopo una riparazione. Cancellarli libera spazio e non tocca i file.</p>
        ${lastLine(lastOf('gc', 'gc-check'), 'Nessuna pulizia eseguita da questa interfaccia.')}
        <div class="card-actions">
          <button class="btn" data-action="gc-check" ${dis}>Controlla</button>
          <button class="btn btn-primary" data-action="gc" ${dis}>Pulisci</button>
        </div>
      </div>
      <div class="card">
        <h3>Ribilanciamento</h3>
        <p class="muted">Sposta frammenti dai backend più pieni a quelli più vuoti, per esempio dopo aver aggiunto un
          backend: i blocchi già scritti restano dove sono finché non li sposti. Ogni frammento viene verificato prima della copia.</p>
        ${lastLine(lastOf('rebalance', 'rebalance-check'), 'Nessun ribilanciamento eseguito da questa interfaccia.')}
        <div class="card-actions">
          <button class="btn" data-action="rebalance-check" ${dis}>Simula</button>
          <button class="btn btn-primary" data-action="rebalance" ${dis}>Ribilancia</button>
        </div>
      </div>
    </div>
    <div class="card">
      <div class="job-head"><h3>Ultima verifica</h3>${scrub ? `<span class="pill ${scrub.state === 'ok' && !(scrub.report.degraded_chunks && !scrub.report.repair) ? 'ok' : scrub.state === 'ok' ? 'warn' : 'bad'}">${when(scrub.finished)}</span>` : ''}</div>
      ${scrub ? `<p>${esc(scrub.message)}</p>${reportBlock(scrub)}` : '<p class="muted">Nessuna verifica eseguita da questa interfaccia.</p>'}
    </div>
    <div class="card">
      <h3>Lock del repository</h3>
      <p class="muted">Chi modifica il repository prende un lock, rinnovato ogni 5 minuti. Se un processo si interrompe il suo lock resta,
        e dopo 30 minuti diventa scaduto.</p>
      ${locks.length ? `<ul class="list-plain">${locks.map((l) => `<li><span>${esc(l.host)} <span class="muted">pid ${l.pid}</span></span><span class="pill ${l.stale ? 'warn' : 'run'}">${l.stale ? 'scaduto' : 'attivo'}</span></li>`).join('')}</ul>` : '<p class="note">Nessun lock presente.</p>'}
      <div class="card-actions">
        <button class="btn" data-action="unlock" ${dis || (locks.some((l) => l.stale) ? '' : 'disabled')}>Cancella i lock scaduti</button>
        <button class="btn btn-ghost" data-action="unlock-all" ${dis || (locks.length ? '' : 'disabled')}>Cancella tutti i lock</button>
      </div>
    </div>`;
}

// ---------------------------------------------------------------------------------------------
// Spazi di archiviazione: elenco, aggiunta guidata, impostazioni delle app OAuth

const SPACE_TYPES = {
  local: {
    label: 'Cartella su questo server',
    short: 'Cartella',
    slug: 'cartella',
    desc: 'Un disco o una cartella del server. Meglio un disco diverso da quello di sistema: se si guasta quello, i dati restano sugli altri spazi.',
  },
  gdrive: {
    label: 'Google Drive',
    short: 'Google Drive',
    slug: 'gdrive',
    oauth: true,
    desc: 'Una cartella nel tuo Drive. cloudraid vede solo i file che crea lui, non il resto del Drive.',
  },
  onedrive: {
    label: 'OneDrive',
    short: 'OneDrive',
    slug: 'onedrive',
    oauth: true,
    desc: 'La cartella dell\'app in OneDrive (Applicazioni). cloudraid non vede il resto dei tuoi file.',
  },
  dropbox: {
    label: 'Dropbox',
    short: 'Dropbox',
    slug: 'dropbox',
    oauth: true,
    desc: 'La cartella dell\'app in Dropbox (Applicazioni). cloudraid non vede il resto dei tuoi file.',
  },
  webdav: {
    label: 'Nextcloud o WebDAV',
    short: 'WebDAV',
    slug: 'nextcloud',
    desc: 'Nextcloud, ownCloud, molti NAS e servizi di archiviazione con accesso WebDAV.',
  },
  s3: {
    label: 'Archivio S3',
    short: 'S3',
    slug: 's3',
    desc: 'Backblaze B2, Wasabi, AWS, MinIO e gli altri compatibili S3: servono un bucket e le chiavi d\'accesso.',
  },
};

const typeLabel = (t) => (SPACE_TYPES[t] ? SPACE_TYPES[t].short : t);

function renderSpaces(st) {
  const snap = st.snapshot;
  if (!snap) return '<div class="card empty">Caricamento…</div>';
  const k = snap.data_shards;
  const m = snap.parity_shards;
  const dis = st.busy ? 'disabled' : '';
  const active = snap.backends.filter((b) => !b.retired);
  const fresh = snap.backends.filter((b) => !b.retired && b.available && b.shards === 0);
  const rows = snap.backends.map((b) => {
    let state;
    if (b.retired && b.shards === 0) state = ['ok', 'dismesso, si può togliere'];
    else if (b.retired) state = ['warn', `in dismissione, ${num(b.shards)} frammenti da spostare`];
    else if (!b.available) state = ['bad', 'non raggiungibile'];
    else if (b.shards === 0) state = ['run', 'nuovo, ancora vuoto'];
    else state = ['ok', 'in uso'];
    const actions = [];
    if (!b.retired) actions.push(`<button class="btn btn-ghost btn-sm" data-action="retire" data-name="${esc(b.name)}" ${dis}>Dismetti</button>`);
    if (b.retired && b.shards > 0) actions.push(`<button class="btn btn-sm" data-action="retire-resume" data-name="${esc(b.name)}" ${dis}>Riprendi lo svuotamento</button>`);
    if (b.retired) actions.push(`<button class="btn btn-ghost btn-sm" data-action="unretire" data-name="${esc(b.name)}" ${dis}>Rimetti in servizio</button>`);
    if (b.shards === 0 && (b.retired || active.length > k + m)) {
      actions.push(`<button class="btn btn-ghost btn-sm danger-text" data-action="remove-space" data-name="${esc(b.name)}" ${dis}>Togli</button>`);
    }
    return `
      <div class="card space">
        <div class="space-main">
          <div class="space-title"><strong>${esc(b.name)}</strong><span class="tag">${esc(typeLabel(b.kind))}</span></div>
          <div class="backend-loc" title="${esc(b.location)}">${esc(b.location)}</div>
        </div>
        <div class="space-side">
          <span class="pill ${state[0]}">${state[1]}</span>
          <span class="muted small">${num(b.shards)} frammenti · ${bytes(b.bytes)}</span>
        </div>
        <div class="card-actions">${actions.join('')}</div>
      </div>`;
  }).join('');
  return `
    <div class="card intro">
      <h3>Dove sono i tuoi dati</h3>
      <p class="muted">Ogni file viene cifrato e diviso in <strong>${k + m} pezzi</strong>, ognuno su uno spazio diverso.
        Per ricostruirlo ne bastano <strong>${k}</strong>: puoi perdere <strong>${m} spazi qualsiasi</strong> (un account chiuso,
        un disco rotto) senza perdere niente. Per questo servono almeno ${k + m} spazi attivi; con qualcuno in più c'è margine
        per sostituirne uno senza fretta.</p>
      <div class="card-actions">
        <button class="btn btn-primary" data-action="add-space" ${dis}>${ICON.plus} Aggiungi uno spazio</button>
        <button class="btn btn-ghost" data-action="apps-settings" ${dis}>Collegamento con Google, Microsoft e Dropbox</button>
      </div>
    </div>
    ${fresh.length && snap.chunks > 0 ? `
    <div class="banner warn"><div>${ICON.warn}</div><div><strong>${fresh.length === 1 ? `${esc(fresh[0].name)} è ancora vuoto` : `${fresh.length} spazi sono ancora vuoti`}</strong>
      <p>Riceve i pezzi dei file nuovi. Con il ribilanciamento gli passi subito anche una parte di quelli già salvati.</p>
      <div class="card-actions"><button class="btn btn-sm" data-action="rebalance" ${dis}>Ribilancia ora</button></div></div></div>` : ''}
    <div class="spaces">${rows}</div>
    <p class="note">Per sostituire un account: aggiungi quello nuovo, poi "Dismetti" su quello vecchio. Quando è vuoto lo puoi togliere.</p>`;
}

// ---- procedura guidata ----

const wiz = { step: 'type', type: null, link: null, poll: null, job: null, error: null, values: {} };

function suggestName(type) {
  const taken = new Set(((state.status && state.status.snapshot && state.status.snapshot.backends) || []).map((b) => b.name));
  const base = SPACE_TYPES[type].slug;
  if (!taken.has(base)) return base;
  for (let i = 2; ; i += 1) if (!taken.has(`${base}${i}`)) return `${base}${i}`;
}

function openWizard() {
  Object.assign(wiz, { step: 'type', type: null, link: null, job: null, error: null, values: {} });
  stopLinkPoll();
  renderWizard();
  $('#wizard').showModal();
}

function closeWizard() {
  stopLinkPoll();
  $('#wizard').close();
}

function stopLinkPoll() {
  if (wiz.poll) clearInterval(wiz.poll);
  wiz.poll = null;
}

/** Legge i campi del modulo nella memoria della procedura (sopravvivono al ridisegno). */
function saveWizardFields() {
  $$('#wizard [data-field]').forEach((el) => {
    wiz.values[el.dataset.field] = el.type === 'checkbox' ? el.checked : el.value;
  });
}

const field = (name, label, opts = {}) => {
  const v = wiz.values[name] !== undefined ? wiz.values[name] : (opts.value || '');
  const type = opts.type || 'text';
  return `<label class="field-label" for="wf-${name}">${label}</label>
    <input id="wf-${name}" type="${type}" data-field="${name}" value="${esc(v)}" ${opts.placeholder ? `placeholder="${esc(opts.placeholder)}"` : ''}
      ${type === 'password' ? 'autocomplete="new-password"' : 'autocomplete="off"'} spellcheck="false">
    ${opts.help ? `<p class="field-help">${opts.help}</p>` : ''}`;
};

function renderWizard() {
  const body = $('#wizard-body');
  const snap = state.status && state.status.snapshot;
  const apps = (snap && snap.apps) || {};
  if (wiz.step === 'type') {
    const cards = Object.entries(SPACE_TYPES).map(([t, info]) => {
      const missing = info.oauth && !apps[t];
      return `<button type="button" class="service" data-action="wiz-type" data-type="${t}">
        <strong>${esc(info.label)}</strong><span>${esc(info.desc)}</span>
        ${missing ? '<em>serve prima il collegamento con il provider (una volta sola)</em>' : ''}</button>`;
    }).join('');
    body.innerHTML = `<h3 id="wizard-title">Aggiungi uno spazio</h3>
      <p class="muted">Dove vuoi salvare una parte dei tuoi dati?</p>
      <div class="services">${cards}</div>
      <div class="dialog-actions"><button type="button" class="btn btn-ghost" data-action="wiz-close">Annulla</button></div>`;
    return;
  }
  const info = SPACE_TYPES[wiz.type];
  if (wiz.step === 'running' || wiz.step === 'done') {
    const done = wiz.step === 'done';
    const ok = done && !wiz.error;
    body.innerHTML = `<h3 id="wizard-title">${esc(info.label)}</h3>
      ${!done ? `<div class="wiz-wait"><span class="spinner" aria-hidden="true"></span><div><strong>Prova in corso…</strong>
        <p class="muted">cloudraid crea la cartella, scrive un file di prova, lo rilegge e lo cancella. Con i servizi cloud può
        richiedere qualche secondo.</p></div></div>` : ''}
      ${ok ? `<div class="banner ok"><div>${ICON.ok}</div><div><strong>Fatto: ${esc(wiz.values.name)} è pronto.</strong>
        <p>${esc(wiz.message || '')}</p></div></div>` : ''}
      ${done && !ok ? `<div class="banner bad"><div>${ICON.bad}</div><div><strong>Lo spazio non è stato aggiunto</strong>
        <p>${esc(wiz.error)}</p><p class="muted">Non è stato salvato niente: correggi i dati e riprova.</p></div></div>` : ''}
      <div class="dialog-actions">
        ${done && !ok ? '<button type="button" class="btn" data-action="wiz-back-form">Correggi</button>' : ''}
        ${ok ? '<button type="button" class="btn" data-action="wiz-rebalance">Ribilancia ora</button>' : ''}
        <button type="button" class="btn ${ok ? 'btn-primary' : 'btn-ghost'}" data-action="wiz-close" ${done ? '' : 'disabled'}>Chiudi</button>
      </div>`;
    return;
  }
  // modulo dei dettagli
  let fields = field('name', 'Nome dello spazio', {
    value: suggestName(wiz.type),
    help: 'Solo lettere minuscole, cifre, - e _. Serve a riconoscerlo nell\'elenco.',
  });
  if (wiz.type === 'local') {
    const root = (snap && snap.local_root) || '/mnt/cloudraid/dischi';
    const name = wiz.values.name || suggestName('local');
    fields += field('path', 'Cartella', {
      value: `${root}/${name}`,
      help: `Deve stare dentro ${esc(root)} (è la cartella del server visibile a cloudraid). Per usare un altro disco, montalo lì dentro.`,
    });
  } else if (wiz.type === 'webdav') {
    fields += field('url', 'Indirizzo della cartella', {
      placeholder: 'https://cloud.esempio.it/remote.php/dav/files/utente/cloudraid',
      help: 'Per Nextcloud: <code>https://&lt;server&gt;/remote.php/dav/files/&lt;utente&gt;/cloudraid</code>. La cartella viene creata se manca.',
    });
    fields += field('user', 'Utente');
    fields += field('password', 'Password', {
      type: 'password',
      help: 'Su Nextcloud meglio una "password per app": Impostazioni personali, Sicurezza, Crea una nuova password per app.',
    });
  } else if (wiz.type === 's3') {
    fields += field('endpoint', 'Endpoint', { placeholder: 'https://s3.eu-central-003.backblazeb2.com', help: 'L\'indirizzo del servizio, lo trovi nella pagina del bucket.' });
    fields += field('region', 'Regione', { placeholder: 'eu-central-003', help: 'Per MinIO e simili va bene us-east-1.' });
    fields += field('bucket', 'Bucket', { help: 'Meglio un bucket solo per cloudraid. Se non esiste e le chiavi lo permettono, viene creato.' });
    fields += field('access_key', 'Access key (ID della chiave)');
    fields += field('secret_key', 'Secret key', { type: 'password', help: 'Viene salvata cifrata sul server, non nel file di configurazione.' });
  } else if (info.oauth) {
    fields += field('folder', wiz.type === 'gdrive' ? 'Nome della cartella nel Drive' : 'Sottocartella (facoltativa)', {
      value: wiz.type === 'gdrive' ? 'cloudraid' : '',
      help: wiz.type === 'gdrive' ? 'Viene creata nel tuo Drive.' : 'Lascia vuoto per usare direttamente la cartella dell\'app.',
    });
    fields += renderLinkBox();
  }
  const canSubmit = !info.oauth || (wiz.link && wiz.link.state === 'done');
  body.innerHTML = `<h3 id="wizard-title">${esc(info.label)}</h3>
    <p class="muted">${esc(info.desc)}</p>
    <form id="wizard-form" autocomplete="off">${fields}
      ${wiz.error ? `<p class="form-error" role="alert">${esc(wiz.error)}</p>` : ''}
      <div class="dialog-actions">
        <button type="button" class="btn btn-ghost" data-action="wiz-close">Annulla</button>
        <button type="button" class="btn" data-action="wiz-back">Indietro</button>
        <button type="submit" class="btn btn-primary" ${canSubmit ? '' : 'disabled'}>Prova e aggiungi</button>
      </div>
    </form>`;
}

function renderLinkBox() {
  const apps = (state.status && state.status.snapshot && state.status.snapshot.apps) || {};
  const label = SPACE_TYPES[wiz.type].label;
  if (!apps[wiz.type]) {
    return `<div class="linkbox"><strong>Prima un passaggio da fare una volta sola</strong>
      <p>Per collegare ${esc(label)} cloudraid deve essere registrato presso il provider.</p>
      <div class="card-actions"><button type="button" class="btn" data-action="apps-settings">Apri la guida</button></div></div>`;
  }
  const l = wiz.link;
  if (!l) {
    return `<div class="linkbox"><strong>Collega l'account</strong>
      <p>Ti verrà chiesto di accedere a ${esc(label)} e di autorizzare cloudraid. Puoi farlo da qualsiasi dispositivo, anche dal telefono.</p>
      <div class="card-actions"><button type="button" class="btn btn-primary" data-action="wiz-link">Collega l'account ${esc(label)}</button></div></div>`;
  }
  if (l.state === 'done') {
    return `<div class="linkbox done">${ICON.ok}<div><strong>Account collegato</strong><p>Ora premi "Prova e aggiungi".</p></div></div>`;
  }
  if (l.kind === 'device') {
    return `<div class="linkbox"><strong>Collega l'account in tre passi</strong>
      <ol class="steps">
        <li>Apri <a href="${esc(l.url)}" target="_blank" rel="noopener noreferrer">${esc(l.url)}</a> (anche dal telefono).</li>
        <li>Scrivi questo codice: <span class="codebox">${esc(l.code)}</span>
          <button type="button" class="btn btn-ghost btn-sm" data-action="copy-code">Copia</button></li>
        <li>Accedi con il tuo account e consenti l'accesso a cloudraid.</li>
      </ol>
      <p class="muted small"><span class="spinner" aria-hidden="true"></span> Questa finestra si accorge da sola quando hai finito.</p>
      ${l.error ? `<p class="form-error">${esc(l.error)}</p><div class="card-actions"><button type="button" class="btn" data-action="wiz-link">Ricomincia</button></div>` : ''}</div>`;
  }
  return `<div class="linkbox"><strong>Collega l'account in tre passi</strong>
    <ol class="steps">
      <li><a class="btn btn-sm" href="${esc(l.url)}" target="_blank" rel="noopener noreferrer">Apri Dropbox</a> e accedi.</li>
      <li>Premi "Consenti".</li>
      <li>Dropbox ti mostra un codice: copialo e incollalo qui.</li>
    </ol>
    ${field('link_code', 'Codice mostrato da Dropbox')}
    ${l.error ? `<p class="form-error">${esc(l.error)}</p>` : ''}
    <div class="card-actions"><button type="button" class="btn btn-primary" data-action="wiz-link-code">Conferma il codice</button>
      ${l.error ? '<button type="button" class="btn btn-ghost" data-action="wiz-link">Ricomincia</button>' : ''}</div></div>`;
}

async function startLink() {
  saveWizardFields();
  stopLinkPoll();
  try {
    const r = await api('api/link/start', { method: 'POST', body: { provider: wiz.type } });
    wiz.link = { ...r, state: 'pending' };
    wiz.error = null;
    if (r.kind === 'device') {
      wiz.poll = setInterval(checkLink, (r.interval || 4) * 1000);
    }
  } catch (e) {
    wiz.link = null;
    wiz.error = e.message;
  }
  renderWizard();
}

async function checkLink(code) {
  if (!wiz.link || !$('#wizard').open) return stopLinkPoll();
  try {
    const r = await api('api/link/check', { method: 'POST', body: { id: wiz.link.id, code } });
    if (r.state === 'pending') return undefined; // ancora in attesa: niente da ridisegnare
    if (r.state === 'done') {
      wiz.link.state = 'done';
      wiz.link.error = null;
    } else {
      wiz.link.error = r.message;
    }
  } catch (e) {
    wiz.link.error = e.message;
  }
  stopLinkPoll();
  saveWizardFields();
  renderWizard();
  return undefined;
}

async function submitWizard() {
  saveWizardFields();
  const v = wiz.values;
  if (!/^[a-z0-9_-]{1,40}$/.test(v.name || '')) {
    wiz.error = 'Il nome può contenere solo lettere minuscole, cifre, - e _ (al massimo 40 caratteri).';
    return renderWizard();
  }
  const body = { ...v, type: wiz.type };
  delete body.link_code;
  if (wiz.link) body.link_id = wiz.link.id;
  try {
    const r = await api('api/backends', { method: 'POST', body });
    wiz.job = r.job && r.job.id;
    wiz.step = 'running';
    wiz.error = null;
    renderWizard();
    refresh();
    watchWizardJob();
  } catch (e) {
    wiz.error = e.message;
    renderWizard();
  }
}

/** Aspetta la fine della prova e mostra l'esito. */
async function watchWizardJob() {
  for (;;) {
    await new Promise((res) => { setTimeout(res, 1000); });
    if (!$('#wizard').open) return;
    let st;
    try { st = await api('api/status'); } catch { continue; }
    const running = st.busy && st.job && st.job.id === wiz.job;
    if (running) continue;
    const j = (st.history || []).find((h) => h.id === wiz.job);
    wiz.step = 'done';
    wiz.error = j && j.state !== 'ok' ? j.message : null;
    wiz.message = j ? j.message : '';
    if (!wiz.error) {
      wiz.link = null;
    }
    renderWizard();
    refresh();
    return;
  }
}

// ---- impostazioni delle app OAuth ----

const APP_GUIDES = [
  {
    key: 'gdrive',
    title: 'Google Drive',
    fields: [['google_client_id', 'ID client'], ['google_client_secret', 'Client secret']],
    steps: [
      'Apri <a href="https://console.cloud.google.com/" target="_blank" rel="noopener noreferrer">console.cloud.google.com</a> con il tuo account Google e crea un progetto (per esempio "cloudraid").',
      'In "API e servizi", "Libreria", cerca <strong>Google Drive API</strong> e premi "Abilita".',
      'In "Schermata consenso OAuth" (o "Google Auth Platform"): tipo <strong>Esterno</strong>, nome dell\'app "cloudraid", la tua email. Tra gli ambiti aggiungi <code>.../auth/drive.file</code>.',
      'Pubblica l\'app ("In produzione"): con il solo ambito drive.file non serve la verifica di Google, e i collegamenti non scadono dopo 7 giorni come in modalità test.',
      'In "Credenziali", "Crea credenziali", "ID client OAuth", tipo <strong>TV e dispositivi con input limitato</strong>. Copia qui ID client e client secret.',
    ],
  },
  {
    key: 'onedrive',
    title: 'OneDrive',
    fields: [['onedrive_client_id', 'ID applicazione (client)']],
    steps: [
      'Apri <a href="https://entra.microsoft.com/" target="_blank" rel="noopener noreferrer">entra.microsoft.com</a> (o portal.azure.com) con un account Microsoft.',
      '"Registrazioni app", "Nuova registrazione": nome "cloudraid", tipi di account: <strong>account Microsoft personali</strong> (o anche aziendali, se ti servono).',
      'In "Autenticazione", alla voce "Consenti flussi client pubblici", scegli <strong>Sì</strong> e salva.',
      'In "Autorizzazioni API" aggiungi per Microsoft Graph (delegate) <code>Files.ReadWrite.AppFolder</code> e <code>offline_access</code>.',
      'Copia qui l\'"ID applicazione (client)" dalla pagina Panoramica dell\'app.',
    ],
  },
  {
    key: 'dropbox',
    title: 'Dropbox',
    fields: [['dropbox_app_key', 'App key']],
    steps: [
      'Apri <a href="https://www.dropbox.com/developers/apps" target="_blank" rel="noopener noreferrer">dropbox.com/developers/apps</a> e premi "Create app".',
      'Scegli <strong>Scoped access</strong> e <strong>App folder</strong> (cloudraid vedrà solo la sua cartella), nome per esempio "cloudraid-casa".',
      'Nella scheda "Permissions" spunta <code>files.content.write</code>, <code>files.content.read</code> e <code>files.metadata.read</code>, poi "Submit".',
      'Nella scheda "Settings" copia qui la <strong>App key</strong> (non serve l\'App secret).',
    ],
  },
];

function openApps() {
  const apps = (state.status && state.status.snapshot && state.status.snapshot.apps) || {};
  $('#apps-fields').innerHTML = APP_GUIDES.map((g) => `
    <details class="guide" ${apps[g.key] ? '' : 'open'}>
      <summary><strong>${g.title}</strong> <span class="pill ${apps[g.key] ? 'ok' : 'warn'}">${apps[g.key] ? 'configurato' : 'da configurare'}</span></summary>
      <ol class="steps">${g.steps.map((s) => `<li>${s}</li>`).join('')}</ol>
      ${g.fields.map(([k, label]) => `<label class="field-label" for="af-${k}">${label}</label>
        <input id="af-${k}" type="text" name="${k}" autocomplete="off" spellcheck="false"
          placeholder="${apps[g.key] ? 'già impostato: scrivi solo per cambiarlo' : ''}">`).join('')}
    </details>`).join('');
  $('#apps-error').hidden = true;
  $('#apps-dialog').showModal();
}

$('#apps-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const body = {};
  $$('#apps-form input[name]').forEach((el) => { if (el.value.trim()) body[el.name] = el.value.trim(); });
  if (!Object.keys(body).length) {
    $('#apps-dialog').close();
    return;
  }
  try {
    await api('api/apps', { method: 'POST', body });
    $('#apps-dialog').close();
    toast('Impostazioni in salvataggio');
    // aspetta che lo snapshot rifletta le app nuove, poi aggiorna la procedura se è aperta
    for (let i = 0; i < 20; i += 1) {
      await new Promise((res) => { setTimeout(res, 700); });
      await refresh();
      if (!(state.status && state.status.busy)) break;
    }
    if ($('#wizard').open) renderWizard();
  } catch (e) {
    $('#apps-error').textContent = e.message;
    $('#apps-error').hidden = false;
  }
});

document.addEventListener('submit', (ev) => {
  if (ev.target && ev.target.id === 'wizard-form') {
    ev.preventDefault();
    submitWizard();
  }
});

$('#wizard').addEventListener('close', stopLinkPoll);

// ---------------------------------------------------------------------------------------------
// Attività

function renderActivity(st) {
  const jobs = [];
  if (st.busy && st.job) jobs.push(st.job);
  jobs.push(...(st.history || []));
  if (!jobs.length) return '<div class="card empty">Nessuna operazione ancora.</div>';
  const label = { running: ['run', 'in corso'], ok: ['ok', 'riuscita'], error: ['bad', 'errore'] };
  return `
    <div class="section-head"><h2>Attività</h2><span class="muted">Le ultime ${jobs.length} operazioni lanciate da questa interfaccia</span></div>
    <div class="table-wrap"><table>
      <thead><tr><th>Quando</th><th>Operazione</th><th>Esito</th><th class="num hide-sm">Durata</th><th class="hide-sm">Dettagli</th></tr></thead>
      <tbody>${jobs.map((j) => `<tr>
        <td class="nowrap">${when(j.started)}</td>
        <td>${esc(j.label)}</td>
        <td><span class="pill ${label[j.state][0]}">${label[j.state][1]}</span></td>
        <td class="num hide-sm">${duration(j.started, j.finished || st.now)}</td>
        <td class="msg hide-sm muted">${esc(j.message)}</td>
      </tr>`).join('')}</tbody>
    </table></div>`;
}

// ---------------------------------------------------------------------------------------------
// Azioni

document.addEventListener('click', async (ev) => {
  const el = ev.target.closest('[data-action]');
  if (!el) return;
  const action = el.dataset.action;
  if (action === 'download') {
    if (state.status && state.status.busy) {
      ev.preventDefault();
      toast("Un'operazione è in corso: lo scaricamento sarà possibile quando finisce", 'bad');
    }
    return;
  }
  ev.preventDefault();
  switch (action) {
    case 'refresh-backends':
      return startJob('api/refresh', null, 'Controllo dei backend avviato');
    case 'scrub':
      return startJob('api/scrub', { repair: false }, 'Verifica avviata');
    case 'repair':
      return startJob('api/scrub', { repair: true }, 'Verifica e riparazione avviate');
    case 'gc-check':
      return startJob('api/gc', { dry_run: true }, 'Ricerca dei frammenti orfani avviata');
    case 'gc':
      if (await confirmDialog('Cancellare i frammenti orfani?', 'Vengono cancellati dai backend solo i frammenti che l\'indice non usa. Durante la pulizia il repository è bloccato per le altre scritture (anche quelle dalla cartella montata).', 'Pulisci', false)) {
        return startJob('api/gc', { dry_run: false }, 'Pulizia avviata');
      }
      return undefined;
    case 'rebalance-check':
      return startJob('api/rebalance', { dry_run: true }, 'Simulazione avviata');
    case 'rebalance':
      if (await confirmDialog('Ribilanciare i frammenti?', 'I frammenti vengono copiati sui backend più vuoti e poi cancellati da quelli più pieni: con i cloud può richiedere molto tempo e traffico. Durante il lavoro il repository è bloccato per le altre scritture.', 'Ribilancia', false)) {
        return startJob('api/rebalance', { dry_run: false }, 'Ribilanciamento avviato');
      }
      return undefined;
    case 'retire': {
      const name = el.dataset.name;
      const others = state.status.snapshot.backends.filter((b) => b.name !== name && !b.retired).length;
      const need = state.status.snapshot.data_shards + state.status.snapshot.parity_shards;
      if (others < need) {
        toast(`Senza ${name} restano ${others} backend e ne servono ${need}: aggiungi prima alla configurazione quello che lo sostituisce`, 'bad');
        return undefined;
      }
      if (await confirmDialog(`Dismettere ${name}?`, `${name} non riceverà più frammenti nuovi e i suoi verranno spostati sugli altri backend (ricostruiti dalla parità se ${name} non risponde). La ridondanza resta piena per tutto il tempo. Con i cloud può richiedere molto tempo e traffico; durante il lavoro il repository è bloccato per le altre scritture. Alla fine potrai toglierlo dalla configurazione o sostituirlo.`, 'Dismetti', false)) {
        return startJob('api/retire', { name, undo: false }, `Dismissione di ${name} avviata`);
      }
      return undefined;
    }
    case 'retire-resume':
      return startJob('api/retire', { name: el.dataset.name, undo: false }, 'Svuotamento ripreso');
    case 'unretire':
      return startJob('api/retire', { name: el.dataset.name, undo: true }, `${el.dataset.name} di nuovo in servizio`);
    case 'add-space':
      return openWizard();
    case 'apps-settings':
      return openApps();
    case 'apps-close':
      return $('#apps-dialog').close();
    case 'wiz-type':
      wiz.type = el.dataset.type;
      wiz.step = 'form';
      wiz.values = {};
      wiz.link = null;
      wiz.error = null;
      return renderWizard();
    case 'wiz-back':
      stopLinkPoll();
      wiz.step = 'type';
      wiz.error = null;
      return renderWizard();
    case 'wiz-back-form':
      wiz.step = 'form';
      return renderWizard();
    case 'wiz-close':
      return closeWizard();
    case 'wiz-link':
      return startLink();
    case 'wiz-link-code':
      saveWizardFields();
      return checkLink(wiz.values.link_code || '');
    case 'copy-code':
      try {
        await navigator.clipboard.writeText(wiz.link.code);
        toast('Codice copiato');
      } catch {
        toast('Copia non riuscita: scrivilo a mano', 'bad');
      }
      return undefined;
    case 'wiz-rebalance':
      closeWizard();
      return startJob('api/rebalance', { dry_run: false }, 'Ribilanciamento avviato');
    case 'remove-space': {
      const name = el.dataset.name;
      if (await confirmDialog(`Togliere ${name}?`, `${name} è vuoto: viene tolto dalla configurazione e le sue credenziali vengono cancellate. I file sul servizio (se ne restano) non vengono toccati.`, 'Togli')) {
        try {
          await api(`api/backends?name=${encodeURIComponent(name)}`, { method: 'DELETE' });
          toast(`Rimozione di ${name} avviata`);
          await refresh();
        } catch (e) {
          toast(e.message, 'bad');
        }
      }
      return undefined;
    }
    case 'unlock':
      return startJob('api/unlock', { all: false });
    case 'unlock-all':
      if (await confirmDialog('Cancellare tutti i lock?', 'Fallo solo se sei sicuro che nessun processo (riga di comando o altro server) stia lavorando sul repository: due processi insieme possono danneggiare l\'indice.', 'Cancella tutti')) {
        return startJob('api/unlock', { all: true });
      }
      return undefined;
    case 'upload':
      return $('#upload-input').click();
    case 'clear-uploads':
      state.uploads = [];
      return render();
    case 'delete': {
      const path = el.dataset.path;
      const isDir = el.dataset.dir === '1';
      const ok = await confirmDialog(isDir ? 'Eliminare la cartella?' : 'Eliminare il file?',
        `${path}${isDir ? ' e tutto il suo contenuto' : ''} verrà tolto dall'archivio e i frammenti non più usati verranno cancellati da tutti i backend. Non si può annullare.`, 'Elimina');
      if (ok) {
        try {
          await api(`api/files?path=${encodeURIComponent(path)}`, { method: 'DELETE' });
          toast(`Eliminazione di ${path} avviata`);
          await refresh();
        } catch (e) {
          toast(e.message, 'bad');
        }
      }
      return undefined;
    }
    default:
      return undefined;
  }
});

document.addEventListener('input', (ev) => {
  if (ev.target.id === 'search') {
    state.search = ev.target.value;
    render();
  }
});

// ---------------------------------------------------------------------------------------------
// Avvio

(async function init() {
  try {
    const s = await api('api/session');
    if (s.logged) {
      showApp();
      route();
      await refresh();
    } else {
      showLogin(s.unlocked ? '' : 'Il primo accesso apre il repository: con i backend cloud può richiedere qualche decina di secondi.');
    }
  } catch (e) {
    showLogin();
    toast(`Server non raggiungibile: ${e.message}`, 'bad');
  }
}());
