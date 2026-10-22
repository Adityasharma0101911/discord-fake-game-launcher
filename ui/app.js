// UI logic. Talks to the Rust backend (src-tauri/src/main.rs) through Tauri commands and events.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);
const PAGE_SIZE = 60;
const SHELF_SIZE = 18;
const LOG_LIMIT = 200;

const state = {
  library: [], // [{ key, appId, name, exe, isFavorite, rpc, steam?, cover?, icon? }]
  groups: [], // [{ id, name, keys }]
  running: {}, // key -> { startedAt, stopAt } (ms since epoch)
  view: parseView(load('view', '')), // { type: 'discover' | 'game' | 'group', id }
  autoStop: Number(load('autoStop', '0')),
  selecting: false,
  selected: new Set(), // library keys ticked in select mode
  catalog: { query: '', kind: 'all', offset: 0, hasMore: false, loading: false, token: 0 },
  picked: new Map(), // "id::exe" -> Discord list entry ticked on the Find games page
  discover: null // popular shelves, loaded on first visit
};

// The startup sync runs in the background; searching waits for it.
let catalogSync = Promise.resolve(false);
let discoverLoading = false;

// Remembered settings are only conveniences, so storage errors are ignored.
function load(name, fallback) {
  try { return localStorage.getItem(name) ?? fallback; } catch { return fallback; }
}
function save(name, value) {
  try { localStorage.setItem(name, value); } catch { /* not persisted */ }
}

function parseView(saved) {
  const [type, ...rest] = saved.split(':');
  return type === 'game' || type === 'group' ? { type, id: rest.join(':') } : { type: 'discover', id: '' };
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function clock(ms) {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, '0');
  return h ? `${h}:${String(m).padStart(2, '0')}:${s}` : `${m}:${s}`;
}

function runLabel(run) {
  const played = clock(Date.now() - run.startedAt);
  return run.stopAt ? `${played} · stops in ${clock(run.stopAt - Date.now())}` : played;
}

const plural = (n, word) => `${n.toLocaleString()} ${word}${n === 1 ? '' : 's'}`;

function log(message, kind = '') {
  const item = el('li', kind);
  item.append(el('time', '', new Date().toLocaleTimeString([], { hour12: false })), String(message));
  const list = $('log');
  list.append(item);
  while (list.children.length > LOG_LIMIT) list.firstElementChild.remove();
  item.scrollIntoView({ block: 'nearest' });
}

const gameByKey = (key) => state.library.find((g) => g.key === key);
const groupById = (id) => state.groups.find((g) => g.id === id);
const groupGames = (group) => group.keys.map(gameByKey).filter(Boolean);
const entryKey = (entry) => `${entry.id}::${entry.exe}`;
const inLibrary = (entry) => state.library.some((g) => g.key === entryKey(entry));

// ---- Cover art ----

// Steam's header art first, then Discord's cover and icon; initials when none load.
function artSources(game) {
  const id = encodeURIComponent(game.appId ?? game.id);
  const sources = [];
  if (game.steam) sources.push(`https://cdn.cloudflare.steamstatic.com/steam/apps/${encodeURIComponent(game.steam)}/header.jpg`);
  if (game.cover) sources.push(`https://cdn.discordapp.com/app-icons/${id}/${encodeURIComponent(game.cover)}.png?size=256`);
  if (game.icon) sources.push(`https://cdn.discordapp.com/app-icons/${id}/${encodeURIComponent(game.icon)}.png?size=128`);
  return sources;
}

function art(game, size) {
  const box = el('div', `art art-${size}`);
  box.dataset.initials = (game.name.match(/[\p{L}\p{N}]+/gu) ?? ['?']).slice(0, 2).map((w) => [...w][0]).join('').toUpperCase();
  let hue = 0;
  for (const c of game.name) hue = (hue * 31 + c.codePointAt(0)) % 360;
  box.style.setProperty('--hue', hue);
  const sources = artSources(game);
  if (!sources.length) return box;
  const img = document.createElement('img');
  img.alt = '';
  img.loading = 'lazy';
  img.decoding = 'async';
  let next = 0;
  img.onerror = () => {
    next += 1;
    if (next < sources.length) img.src = sources[next];
    else img.remove();
  };
  // Discord's art is square: show it whole on the tinted background instead of cropping it.
  img.onload = () => box.classList.toggle('square', img.naturalWidth < img.naturalHeight * 1.3);
  img.src = sources[0];
  box.append(img);
  return box;
}

// Library art is reused across re-renders so images don't flicker.
const artCache = new Map();
function cachedArt(game, size) {
  const id = [size, game.key, game.steam, game.cover, game.icon].join('|');
  if (!artCache.has(id)) artCache.set(id, art(game, size));
  return artCache.get(id);
}

// ---- Rendering ----

function render() {
  renderGroups();
  renderLibrary();
  renderSelectionBar();
  renderView();
  tick();
}

function renderGroups() {
  $('groups').replaceChildren(...state.groups.map(groupItem));
  $('groupsEmpty').hidden = state.groups.length > 0;
}

function groupItem(group) {
  const games = groupGames(group);
  const running = games.filter((g) => state.running[g.key]).length;
  const current = state.view.type === 'group' && state.view.id === group.id;
  const item = el('li', `nav-item${current ? ' selected' : ''}${running ? ' running' : ''}`);

  const open = el('button', 'nav-main');
  if (current) open.setAttribute('aria-current', 'true');
  open.append(el('span', 'group-badge', String(games.length)), el('span', 'nav-name', group.name));
  if (running) open.append(el('span', 'nav-meta', `${running} on`));
  open.onclick = () => setView('group', group.id);

  // Launches what isn't running; once everything runs it stops them all.
  const allRunning = games.length > 0 && running === games.length;
  const play = el('button', 'icon-btn', allRunning ? '■' : '▶');
  play.setAttribute('aria-label', `${allRunning ? 'Stop' : 'Launch'} ${group.name}`);
  play.disabled = !games.length;
  play.onclick = () => (allRunning ? stopMany(games) : launchMany(games));

  item.append(open, play);
  return item;
}

function renderLibrary() {
  const term = $('libraryFilter').value.trim().toLowerCase();
  const games = state.library
    .filter((g) => g.name.toLowerCase().includes(term))
    .sort((a, b) => b.isFavorite - a.isFavorite || a.name.localeCompare(b.name));
  $('library').replaceChildren(...games.map(libraryItem));
  $('libraryEmpty').hidden = state.library.length > 0;
  $('launchFavoritesBtn').disabled = !state.library.some((g) => g.isFavorite);
}

function libraryItem(game) {
  const current = !state.selecting && state.view.type === 'game' && state.view.id === game.key;
  const ticked = state.selected.has(game.key);
  const item = el('li', `nav-item${current ? ' selected' : ''}${state.running[game.key] ? ' running' : ''}${ticked ? ' ticked' : ''}`);

  const main = el('button', 'nav-main');
  if (state.selecting) {
    main.setAttribute('role', 'checkbox');
    main.setAttribute('aria-checked', String(ticked));
    main.append(el('span', 'tick', ticked ? '✓' : ''));
  } else if (current) {
    main.setAttribute('aria-current', 'true');
  }
  const time = el('span', 'nav-meta');
  time.dataset.timeFor = game.key;
  main.append(cachedArt(game, 'thumb'), el('span', 'nav-name', game.name), time);
  main.onclick = () => (state.selecting ? toggleSelected(game.key) : setView('game', game.key));

  const star = el('button', 'star', '★');
  star.setAttribute('aria-pressed', String(game.isFavorite));
  star.setAttribute('aria-label', `Favorite ${game.name}`);
  star.onclick = () => toggleFavorite(game.key);

  item.append(main, star);
  return item;
}

function renderSelectionBar() {
  const n = state.selected.size;
  $('selectionBar').hidden = !state.selecting;
  $('selectionCount').textContent = n ? `${n} selected` : 'Tick games below';
  for (const id of ['selLaunchBtn', 'selStopBtn', 'selGroupBtn', 'selRemoveBtn']) $(id).disabled = n === 0;
}

function renderView() {
  const { type } = state.view;
  $('discoverView').hidden = type !== 'discover';
  $('gameView').hidden = type !== 'game';
  $('groupView').hidden = type !== 'group';
  $('discoverBtn').setAttribute('aria-current', String(type === 'discover'));
  if (type === 'game') renderGame();
  if (type === 'group') renderGroup();
  if (type === 'discover' && !state.discover) loadDiscover();
}

function renderGame() {
  const game = gameByKey(state.view.id);
  const run = state.running[game.key];
  $('gameArt').replaceChildren(cachedArt(game, 'hero'));
  $('gameName').textContent = game.name;
  $('gameMode').textContent = game.rpc
    ? 'Shown in Discord through Rich Presence: Discord’s list has no PC exe for it. Discord may not count it for quests.'
    : 'Discord detects it by its exe name.';
  $('appId').textContent = game.appId || '—';
  $('factExe').textContent = game.rpc ? 'None (Rich Presence)' : game.exe;
  $('status').classList.toggle('on', Boolean(run));
  $('statusText').textContent = run ? 'Playing' : 'Not running';
  $('statusTime').dataset.timeFor = game.key;
  $('launchBtn').textContent = run ? 'Stop' : 'Launch';
  $('launchBtn').classList.toggle('stop', Boolean(run));
  $('favoriteBtn').textContent = game.isFavorite ? '★ Favorited' : '☆ Favorite';
  $('favoriteBtn').setAttribute('aria-pressed', String(game.isFavorite));
}

function renderGroup() {
  const group = groupById(state.view.id);
  const games = groupGames(group);
  const running = games.filter((g) => state.running[g.key]).length;
  $('groupName').textContent = group.name;
  $('groupStatus').textContent = `${plural(games.length, 'game')}${running ? ` · ${running} running` : ''}`;
  $('groupLaunchBtn').textContent = running === games.length && games.length ? 'Stop all' : 'Launch all';
  $('groupLaunchBtn').classList.toggle('stop', running === games.length && games.length > 0);
  $('groupLaunchBtn').disabled = !games.length;
  $('groupEmpty').hidden = games.length > 0;
  $('groupGames').replaceChildren(...games.map((game) => memberItem(group, game)));
}

function memberItem(group, game) {
  const item = el('li', `member${state.running[game.key] ? ' running' : ''}`);
  const open = el('button', 'member-main');
  const time = el('span', 'nav-meta');
  time.dataset.timeFor = game.key;
  open.append(cachedArt(game, 'member'), el('span', 'member-name', game.name), time);
  open.onclick = () => setView('game', game.key);
  const remove = el('button', 'icon-btn', '✕');
  remove.setAttribute('aria-label', `Remove ${game.name} from ${group.name}`);
  remove.onclick = () => saveGroup({ ...group, keys: group.keys.filter((k) => k !== game.key) });
  item.append(open, remove);
  return item;
}

// Play time and auto-stop countdowns, refreshed every second without re-rendering.
function tick() {
  for (const node of document.querySelectorAll('[data-time-for]')) {
    const run = state.running[node.dataset.timeFor];
    node.textContent = run ? runLabel(run) : '';
  }
}

function setView(type, id = '') {
  state.view = { type, id };
  save('view', type === 'discover' ? 'discover' : `${type}:${id}`);
  render();
  if (type === 'discover') $('catalogSearch').focus();
}

// ---- Library ----

async function refresh() {
  [state.library, state.running, state.groups] = await Promise.all([
    invoke('get_library'),
    invoke('get_running'),
    invoke('get_groups')
  ]);
  const { type, id } = state.view;
  if ((type === 'game' && !gameByKey(id)) || (type === 'group' && !groupById(id))) state.view = { type: 'discover', id: '' };
  for (const key of state.selected) if (!gameByKey(key)) state.selected.delete(key);
  render();
  markCards();
}

async function toggleFavorite(key) {
  const updated = await invoke('toggle_favorite', { key });
  if (updated) state.library = state.library.map((g) => (g.key === key ? updated : g));
  render();
}

async function checkDiscord() {
  const running = await invoke('discord_running');
  $('discordWarning').hidden = running;
  return running;
}

async function warnIfDiscordClosed() {
  if (!(await checkDiscord())) log('Discord isn’t running, so it won’t show the game until you open it.', 'warn');
}

async function launch(game) {
  const minutes = state.autoStop || null;
  const r = await invoke('launch_game', { key: game.key, minutes });
  if (!r.ok) {
    log(`Couldn’t launch ${game.name}: ${r.error}`, 'error');
    return;
  }
  state.running[game.key] = r.run;
  if (r.alreadyRunning) log(`${game.name} is already running.`);
  else log(`Launched ${game.name}${minutes ? `; it stops after ${minutes} min` : ''}.`, 'ok');
  render();
}

async function stop(game) {
  // Cleared first so the game-exited event for this stop isn't logged twice.
  delete state.running[game.key];
  render();
  await invoke('stop_game', { key: game.key });
  log(`Stopped ${game.name}.`);
}

async function launchMany(games) {
  const idle = games.filter((g) => !state.running[g.key]);
  if (!idle.length) {
    log('Those games are already running.');
    return;
  }
  await warnIfDiscordClosed();
  for (const game of idle) await launch(game);
}

async function stopMany(games) {
  for (const game of games.filter((g) => state.running[g.key])) await stop(game);
}

async function addGames(entries) {
  try {
    const added = await invoke('add_games', { games: entries });
    await refresh();
    log(added.length === 1 ? `Added ${added[0].name}.` : `Added ${plural(added.length, 'game')}.`, 'ok');
    return added;
  } catch (e) {
    log(`Couldn’t add games: ${e}`, 'error');
    return [];
  }
}

async function removeGames(games) {
  if (!games.length) return;
  if (games.length > 1 && !(await confirmAction('Remove games?', `Remove ${games.length} games from your library? Running ones are stopped.`, 'Remove'))) return;
  for (const game of games) delete state.running[game.key];
  await invoke('remove_games', { keys: games.map((g) => g.key) });
  state.selected.clear();
  await refresh();
  log(games.length === 1 ? `Removed ${games[0].name} from your library.` : `Removed ${games.length} games from your library.`);
}

async function createShortcut(game) {
  const r = await invoke('create_shortcut', { key: game.key });
  if (r.ok) log(`Created a desktop shortcut for ${game.name}.`, 'ok');
  else log(`Couldn’t create a shortcut: ${r.error}`, 'error');
}

function setSelecting(on) {
  state.selecting = on;
  state.selected.clear();
  $('selectBtn').setAttribute('aria-pressed', String(on));
  $('selectBtn').textContent = on ? 'Done' : 'Select';
  render();
}

function toggleSelected(key) {
  if (state.selected.has(key)) state.selected.delete(key);
  else state.selected.add(key);
  render();
}

const selectedGames = () => [...state.selected].map(gameByKey).filter(Boolean);

// ---- Groups ----

async function saveGroup(group) {
  const saved = await invoke('save_group', { group });
  state.groups = await invoke('get_groups');
  render();
  return saved;
}

async function newGroup(keys = []) {
  const name = await askName('New group', '');
  if (name === null) return null;
  const group = await saveGroup({ id: '', name, keys });
  log(`Created the group ${group.name}.`, 'ok');
  return group;
}

async function addToGroup(keys) {
  if (!keys.length) return;
  const choice = await chooseGroup();
  if (!choice) return;
  let group;
  if (choice === 'new') {
    group = await newGroup(keys);
  } else {
    const existing = groupById(choice);
    group = await saveGroup({ ...existing, keys: [...existing.keys, ...keys] });
  }
  if (group) log(`Added ${keys.length === 1 ? gameByKey(keys[0]).name : plural(keys.length, 'game')} to ${group.name}.`, 'ok');
}

async function addFromLibrary(group) {
  const items = state.library
    .filter((g) => !group.keys.includes(g.key))
    .map((g) => ({ value: g.key, game: g, label: g.name, sub: g.rpc ? 'Rich Presence' : g.exe, checked: false }));
  if (!items.length) {
    log('Every game in your library is already in this group.');
    return;
  }
  const keys = await pickDialog({
    title: `Add to ${group.name}`,
    hint: 'Pick games from your library.',
    items,
    confirm: (n) => (n ? `Add ${plural(n, 'game')}` : 'Add')
  });
  if (keys.length) await saveGroup({ ...group, keys: [...group.keys, ...keys] });
}

async function deleteGroup(group) {
  await invoke('delete_group', { id: group.id });
  state.groups = await invoke('get_groups');
  setView('discover');
  log(`Deleted the group ${group.name}. Its games are still in your library.`);
}

// ---- Dialogs ----

function dialogResult(dialog) {
  return new Promise((resolve) => {
    dialog.returnValue = '';
    dialog.addEventListener('close', () => resolve(dialog.returnValue), { once: true });
    dialog.showModal();
  });
}

async function confirmAction(title, text, action) {
  $('confirmTitle').textContent = title;
  $('confirmText').textContent = text;
  $('confirmBtn').textContent = action;
  return (await dialogResult($('confirmDialog'))) === 'confirm';
}

async function askName(title, value) {
  $('nameTitle').textContent = title;
  $('nameInput').value = value;
  const result = dialogResult($('nameDialog'));
  $('nameInput').select();
  return (await result) === 'confirm' ? $('nameInput').value.trim() || 'Group' : null;
}

// Returns a group id, 'new', or null when cancelled.
async function chooseGroup() {
  if (!state.groups.length) return 'new';
  $('groupChoices').replaceChildren(
    ...state.groups.map((group) => {
      const button = el('button', 'group-choice');
      button.value = `g:${group.id}`;
      button.append(el('span', 'group-badge', String(groupGames(group).length)), el('span', 'nav-name', group.name));
      const item = el('li');
      item.append(button);
      return item;
    })
  );
  const result = await dialogResult($('groupDialog'));
  if (result === 'new') return 'new';
  return result.startsWith('g:') ? result.slice(2) : null;
}

// Checklist dialog; resolves with the ticked values ([] when cancelled).
async function pickDialog({ title, hint, items, confirm }) {
  $('pickTitle').textContent = title;
  $('pickHint').textContent = hint;
  const rows = items.map((item) => {
    const box = document.createElement('input');
    box.type = 'checkbox';
    box.value = item.value;
    box.checked = item.checked;
    box.disabled = Boolean(item.disabled);
    const text = el('span', 'pick-text');
    text.append(el('span', 'pick-name', item.label), el('span', 'pick-sub', item.sub));
    const label = el('label', 'pick-item');
    label.append(box, art(item.game, 'pick'), text);
    const li = el('li');
    li.append(label);
    return { li, box };
  });
  $('pickList').replaceChildren(...rows.map((r) => r.li));
  const update = () => {
    const n = rows.filter((r) => r.box.checked).length;
    $('pickConfirmBtn').textContent = confirm(n);
    $('pickConfirmBtn').disabled = n === 0;
  };
  $('pickList').onchange = update;
  $('pickAllBtn').onclick = () => {
    for (const r of rows) if (!r.box.disabled) r.box.checked = true;
    update();
  };
  $('pickNoneBtn').onclick = () => {
    for (const r of rows) r.box.checked = false;
    update();
  };
  update();
  const result = await dialogResult($('pickDialog'));
  return result === 'confirm' ? rows.filter((r) => r.box.checked).map((r) => r.box.value) : [];
}

// ---- Import from this PC ----

async function importFromPc() {
  $('importBtn').disabled = true;
  log('Looking for games installed on this PC…');
  try {
    await catalogSync;
    const found = await invoke('scan_installed');
    if (!found.length) {
      log('No installed games from Discord’s list were found on this PC.');
      return;
    }
    const items = found.map(({ game, source }) => {
      const added = inLibrary(game);
      return { value: entryKey(game), game, label: game.name, sub: added ? `${source} · in your library` : source, checked: !added, disabled: added };
    });
    const keys = await pickDialog({
      title: 'Import installed games',
      hint: `Found ${plural(found.length, 'game')} from Discord’s list on this PC.`,
      items,
      confirm: (n) => (n ? `Add ${plural(n, 'game')}` : 'Add')
    });
    const games = found.map((f) => f.game).filter((g) => keys.includes(entryKey(g)));
    if (games.length) await addGames(games);
  } catch (e) {
    log(`Couldn’t scan this PC: ${e}`, 'error');
  } finally {
    $('importBtn').disabled = false;
  }
}

// ---- Find games (Discover) ----

const showingShelves = () => !state.catalog.query.trim() && state.catalog.kind === 'all';

async function loadDiscover() {
  if (discoverLoading) return;
  discoverLoading = true;
  if (showingShelves()) $('discoverBody').replaceChildren(el('p', 'hint', 'Loading popular games…'));
  try {
    await catalogSync;
    state.discover = await invoke('discover');
  } catch (e) {
    log(`Couldn’t load popular games: ${e}`, 'error');
    state.discover = { mostPlayed: [], topRated: [], beyondSteam: [], mobile: [], count: 0 };
  }
  discoverLoading = false;
  const d = state.discover;
  if (d.count) {
    $('listInfo').textContent = `${plural(d.count, 'game')} and apps in Discord’s list${d.updatedAt ? `, updated ${new Date(d.updatedAt).toLocaleDateString()}` : ''}. Every one can be shown in Discord.`;
  }
  if (showingShelves()) renderShelves();
}

function renderShelves() {
  const d = state.discover;
  const shelves = [
    ['Most played on Steam right now', d.mostPlayed],
    ['Top rated on Steam', d.topRated],
    ['Popular beyond Steam', d.beyondSteam],
    ['Mobile games', d.mobile]
  ].filter(([, cards]) => cards.length);
  if (!shelves.length) {
    $('discoverBody').replaceChildren(el('p', 'hint', 'Couldn’t load popular games right now. Search still works.'));
    return;
  }
  $('discoverBody').replaceChildren(
    ...shelves.map(([title, cards]) => {
      const shelf = el('section', 'shelf');
      const grid = el('ul', 'card-grid');
      grid.append(...cards.slice(0, SHELF_SIZE).map((c) => card(c.game, c.note)));
      shelf.append(el('h2', '', title), grid);
      return shelf;
    })
  );
}

function card(entry, note = '') {
  const button = el('button', 'card');
  button.dataset.key = entryKey(entry);
  button.dataset.note = note;
  const badges = el('span', 'badges');
  if (entry.rpc) badges.append(el('span', 'badge', 'Rich Presence'));
  if (entry.mobile) badges.append(el('span', 'badge', 'Mobile'));
  button.append(art(entry, 'tile'), el('span', 'card-name', entry.name), el('span', 'card-meta'), badges);
  button.onclick = () => togglePick(entry);
  markCard(button);
  const item = el('li');
  item.append(button);
  return item;
}

// "In library" cards can't be picked; picked ones show a check.
function markCard(button) {
  const added = state.library.some((g) => g.key === button.dataset.key);
  button.disabled = added;
  button.setAttribute('aria-pressed', String(!added && state.picked.has(button.dataset.key)));
  button.querySelector('.card-meta').textContent = added ? 'In your library' : button.dataset.note;
}

function markCards() {
  for (const button of document.querySelectorAll('.card')) markCard(button);
}

function togglePick(entry) {
  const key = entryKey(entry);
  if (state.picked.has(key)) state.picked.delete(key);
  else state.picked.set(key, entry);
  markCards();
  renderPickBar();
}

function renderPickBar() {
  const n = state.picked.size;
  $('pickBar').hidden = n === 0;
  $('pickCount').textContent = `${n} selected`;
  $('pickAddBtn').textContent = `Add ${plural(n, 'game')}`;
}

async function addPicked() {
  const entries = [...state.picked.values()];
  state.picked.clear();
  renderPickBar();
  await addGames(entries);
}

async function searchCatalog(reset) {
  const c = state.catalog;
  if (reset) {
    Object.assign(c, { offset: 0, hasMore: false, loading: false, token: c.token + 1 });
    if (showingShelves()) {
      if (state.discover) renderShelves();
      else loadDiscover();
      return;
    }
    $('discoverBody').replaceChildren(el('p', 'hint', 'Searching…'), el('ul', 'card-grid results'));
  }
  if (c.loading) return;
  c.loading = true;
  const token = c.token;
  await catalogSync;
  const page = await invoke('search_games', { filter: c.query, kind: c.kind, offset: c.offset, limit: PAGE_SIZE });
  if (token !== c.token) return; // a newer search replaced this one
  const body = $('discoverBody');
  if (c.offset === 0) {
    body.firstElementChild.textContent = page.total ? plural(page.total, 'result') : 'No games found.';
    // Apps missing from Discord's game list can still be shown by their application id.
    const id = c.query.trim();
    if (!page.total && /^\d{15,25}$/.test(id)) body.append(lookupButton(id));
  }
  body.querySelector('.results').append(...page.items.map((g) => card(g)));
  c.offset += page.items.length;
  c.hasMore = page.hasMore;
  c.loading = false;
}

function lookupButton(id) {
  const button = el('button', 'btn', `Look up Discord app ${id}`);
  button.onclick = async () => {
    try {
      const entry = await invoke('lookup_app', { id });
      $('discoverBody').querySelector('.results').append(card(entry, 'Discord app'));
      $('discoverBody').firstElementChild.textContent = '1 result';
      button.remove();
    } catch (e) {
      log(String(e), 'error');
    }
  };
  return button;
}

// ---- Updates ----

listen('update-available', ({ payload }) => {
  $('updateVersion').textContent = `Version ${payload.version} is ready to install.`;
  $('updateNotes').textContent = payload.notes || 'No release notes.';
  $('updateProgressWrap').hidden = true;
  $('updateInstallBtn').disabled = false;
  $('updateLaterBtn').disabled = false;
  $('updateDialog').showModal();
  log(`Update ${payload.version} available.`, 'ok');
});

listen('update-progress', ({ payload }) => {
  const mb = (bytes) => (bytes / 1048576).toFixed(1);
  $('updateProgress').value = payload.percent;
  $('updateProgressText').textContent = payload.total
    ? `Downloading… ${Math.round(payload.percent)}% (${mb(payload.transferred)} / ${mb(payload.total)} MB)`
    : 'Downloading…';
});

listen('update-downloaded', async () => {
  $('updateProgressText').textContent = 'Download complete. Installing…';
  // Starts the installer, which closes this app.
  await invoke('update_quit_and_install');
});

$('updateInstallBtn').onclick = async () => {
  $('updateInstallBtn').disabled = true;
  $('updateLaterBtn').disabled = true;
  $('updateProgressWrap').hidden = false;
  const r = await invoke('update_install');
  if (!r.ok) {
    log(`Update failed: ${r.error}`, 'error');
    $('updateDialog').close();
  }
};
$('updateLaterBtn').onclick = () => $('updateDialog').close();

// ---- Events and startup ----

listen('game-exited', ({ payload: key }) => {
  if (!state.running[key]) return; // stopped from this window, already handled
  delete state.running[key];
  const game = gameByKey(key);
  log(`${game ? game.name : 'A game'} closed.`);
  render();
});

$('minimizeBtn').onclick = () => appWindow.minimize();
$('closeBtn').onclick = () => appWindow.close();
$('discoverBtn').onclick = () => setView('discover');
$('importBtn').onclick = importFromPc;
$('newGroupBtn').onclick = async () => {
  const group = await newGroup();
  if (group) setView('group', group.id);
};
$('libraryFilter').oninput = renderLibrary;
$('launchFavoritesBtn').onclick = () => launchMany(state.library.filter((g) => g.isFavorite));
$('selectBtn').onclick = () => setSelecting(!state.selecting);
$('selLaunchBtn').onclick = () => launchMany(selectedGames());
$('selStopBtn').onclick = () => stopMany(selectedGames());
$('selGroupBtn').onclick = () => addToGroup([...state.selected]);
$('selRemoveBtn').onclick = () => removeGames(selectedGames());

$('launchBtn').onclick = async () => {
  const game = gameByKey(state.view.id);
  if (state.running[game.key]) return stop(game);
  await warnIfDiscordClosed();
  await launch(game);
};
$('favoriteBtn').onclick = () => toggleFavorite(state.view.id);
$('gameGroupBtn').onclick = () => addToGroup([state.view.id]);
$('shortcutBtn').onclick = () => createShortcut(gameByKey(state.view.id));
$('removeBtn').onclick = () => removeGames([gameByKey(state.view.id)]);
$('autoStop').onchange = (e) => {
  state.autoStop = Number(e.target.value);
  save('autoStop', String(state.autoStop));
};

$('groupLaunchBtn').onclick = () => {
  const games = groupGames(groupById(state.view.id));
  return games.every((g) => state.running[g.key]) ? stopMany(games) : launchMany(games);
};
$('groupAddBtn').onclick = () => addFromLibrary(groupById(state.view.id));
$('groupRenameBtn').onclick = async () => {
  const group = groupById(state.view.id);
  const name = await askName('Rename group', group.name);
  if (name !== null) await saveGroup({ ...group, name });
};
$('groupDeleteBtn').onclick = () => deleteGroup(groupById(state.view.id));

// Enter in the name box would otherwise press the form's first button (Cancel).
$('nameInput').onkeydown = (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    $('nameDialog').close('confirm');
  }
};

let searchTimer;
$('catalogSearch').oninput = (e) => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => {
    state.catalog.query = e.target.value;
    searchCatalog(true);
  }, 200);
};
for (const chip of document.querySelectorAll('.chip')) {
  chip.onclick = () => {
    state.catalog.kind = chip.dataset.kind;
    for (const c of document.querySelectorAll('.chip')) c.setAttribute('aria-pressed', String(c === chip));
    searchCatalog(true);
  };
}
$('discoverBody').onscroll = (e) => {
  const body = e.target;
  if (!showingShelves() && state.catalog.hasMore && body.scrollTop + body.clientHeight > body.scrollHeight - 400) searchCatalog(false);
};
$('pickClearBtn').onclick = () => {
  state.picked.clear();
  markCards();
  renderPickBar();
};
$('pickAddBtn').onclick = addPicked;
window.addEventListener('focus', checkDiscord);
setInterval(tick, 1000);

async function syncCatalog() {
  try {
    const r = await invoke('sync_game_list');
    log(r.updated ? 'Downloaded Discord’s latest game list.' : 'Discord’s game list is up to date.', 'ok');
    return r.updated;
  } catch (e) {
    log(`Couldn’t update Discord’s game list: ${e}`, 'error');
    return false;
  }
}

(async function init() {
  $('autoStop').value = String(state.autoStop);
  catalogSync = syncCatalog();
  await refresh();
  checkDiscord();
  // A fresh list can fill in cover art for games already in the library.
  if (await catalogSync) await refresh();
})();
