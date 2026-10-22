// UI logic. Talks to the Rust backend (src-tauri/src/main.rs) through Tauri commands and events.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);
const PAGE_SIZE = 200;
const LOG_LIMIT = 200;

const state = {
  library: [], // [{ key, appId, name, exe, isFavorite }]
  running: {}, // key -> { startedAt, stopAt } (ms since epoch)
  selectedKey: load('selectedKey', ''),
  autoStop: Number(load('autoStop', '0')),
  catalog: { term: '', offset: 0, hasMore: false, loading: false, token: 0 }
};

// The startup sync runs in the background; the add-game dialog waits for it.
let catalogSync = Promise.resolve();

// Remembered settings are only conveniences, so storage errors are ignored.
function load(name, fallback) {
  try { return localStorage.getItem(name) ?? fallback; } catch { return fallback; }
}
function save(name, value) {
  try { localStorage.setItem(name, value); } catch { /* not persisted */ }
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

function log(message, kind = '') {
  const item = el('li', kind);
  item.append(el('time', '', new Date().toLocaleTimeString([], { hour12: false })), String(message));
  const list = $('log');
  list.append(item);
  while (list.children.length > LOG_LIMIT) list.firstElementChild.remove();
  item.scrollIntoView({ block: 'nearest' });
}

const selectedGame = () => state.library.find((g) => g.key === state.selectedKey);

// ---- Rendering ----

function render() {
  renderLibrary();
  renderDetails();
  tick();
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
  const selected = game.key === state.selectedKey;
  const item = el('li', `game${selected ? ' selected' : ''}${state.running[game.key] ? ' running' : ''}`);

  const select = el('button', 'game-select');
  if (selected) select.setAttribute('aria-current', 'true');
  const dot = el('span', 'dot');
  dot.setAttribute('aria-hidden', 'true');
  const time = el('span', 'game-time');
  time.dataset.timeFor = game.key;
  select.append(dot, el('span', 'game-name', game.name), time);
  select.onclick = () => selectGame(game.key);

  const star = el('button', 'star', '★');
  star.setAttribute('aria-pressed', String(game.isFavorite));
  star.setAttribute('aria-label', `Favorite ${game.name}`);
  star.onclick = () => toggleFavorite(game.key);

  item.append(select, star);
  return item;
}

function renderDetails() {
  const game = selectedGame();
  $('emptyState').hidden = Boolean(game);
  $('details').hidden = !game;
  if (!game) return;

  const run = state.running[game.key];
  $('gameName').textContent = game.name;
  $('gameExe').textContent = game.exe;
  $('appId').textContent = game.appId || '—';
  $('factExe').textContent = game.exe;
  $('status').classList.toggle('on', Boolean(run));
  $('statusText').textContent = run ? 'Playing' : 'Not running';
  $('statusTime').dataset.timeFor = game.key;
  $('launchBtn').textContent = run ? 'Stop' : 'Launch';
  $('launchBtn').classList.toggle('stop', Boolean(run));
  $('favoriteBtn').textContent = game.isFavorite ? '★ Favorited' : '☆ Favorite';
  $('favoriteBtn').setAttribute('aria-pressed', String(game.isFavorite));
}

// Play time and auto-stop countdowns, refreshed every second without re-rendering.
function tick() {
  for (const node of document.querySelectorAll('[data-time-for]')) {
    const run = state.running[node.dataset.timeFor];
    node.textContent = run ? runLabel(run) : '';
  }
}

// ---- Library actions ----

async function refresh() {
  [state.library, state.running] = await Promise.all([invoke('get_library'), invoke('get_running')]);
  render();
}

function selectGame(key) {
  state.selectedKey = key;
  save('selectedKey', key);
  render();
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

async function launchFavorites() {
  const games = state.library.filter((g) => g.isFavorite && !state.running[g.key]);
  if (!games.length) {
    log('All your favorites are already running.');
    return;
  }
  await warnIfDiscordClosed();
  for (const game of games) await launch(game);
}

async function createShortcut(game) {
  const r = await invoke('create_shortcut', { key: game.key });
  if (r.ok) log(`Created a desktop shortcut for ${game.name}.`, 'ok');
  else log(`Couldn’t create a shortcut: ${r.error}`, 'error');
}

async function removeGame(game) {
  delete state.running[game.key];
  await invoke('remove_game', { key: game.key });
  await refresh();
  log(`Removed ${game.name} from your library.`);
}

// ---- Add-game dialog ----

function openAddDialog() {
  $('catalogSearch').value = '';
  $('addDialog').showModal();
  searchCatalog(true);
}

async function searchCatalog(reset) {
  const c = state.catalog;
  if (reset) {
    Object.assign(c, { term: $('catalogSearch').value, offset: 0, hasMore: false, loading: false, token: c.token + 1 });
    $('catalog').replaceChildren();
  }
  if (c.loading) return;
  c.loading = true;
  const token = c.token;
  await catalogSync;
  const page = await invoke('search_games', { filter: c.term, offset: c.offset, limit: PAGE_SIZE });
  if (token !== c.token) return; // a newer search replaced this one
  $('catalog').append(...page.items.map(catalogItem));
  c.offset += page.items.length;
  c.hasMore = page.hasMore;
  c.loading = false;
  $('catalogEmpty').hidden = $('catalog').children.length > 0;
}

function catalogItem(item) {
  const added = state.library.some((g) => g.appId === item.id && g.exe === item.exe);
  const button = el('button', 'catalog-item');
  button.disabled = added;
  button.append(el('span', 'catalog-name', item.name), el('span', 'catalog-action', added ? 'Added' : '+ Add'), el('span', 'catalog-exe mono', item.exe));
  button.onclick = () => addGame(item);
  const li = el('li');
  li.append(button);
  return li;
}

function confirmDialog(dialog) {
  return new Promise((resolve) => {
    dialog.returnValue = '';
    dialog.addEventListener('close', () => resolve(dialog.returnValue === 'confirm'), { once: true });
    dialog.showModal();
  });
}

async function addGame(item) {
  if (item.usesNewDetection && !(await confirmDialog($('warnDialog')))) return;
  try {
    const game = await invoke('add_game', { game: item });
    $('addDialog').close();
    await refresh();
    selectGame(game.key);
    log(`Added ${game.name}.`, 'ok');
  } catch (e) {
    log(`Couldn’t add ${item.name}: ${e}`, 'error');
  }
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
  const game = state.library.find((g) => g.key === key);
  log(`${game ? game.name : 'A game'} closed.`);
  render();
});

$('minimizeBtn').onclick = () => appWindow.minimize();
$('closeBtn').onclick = () => appWindow.close();
$('libraryFilter').oninput = renderLibrary;
$('addGameBtn').onclick = openAddDialog;
$('launchFavoritesBtn').onclick = launchFavorites;
$('launchBtn').onclick = async () => {
  const game = selectedGame();
  if (!game) return;
  if (state.running[game.key]) return stop(game);
  await warnIfDiscordClosed();
  await launch(game);
};
$('favoriteBtn').onclick = () => toggleFavorite(state.selectedKey);
$('shortcutBtn').onclick = () => createShortcut(selectedGame());
$('removeBtn').onclick = () => removeGame(selectedGame());
$('autoStop').onchange = (e) => {
  state.autoStop = Number(e.target.value);
  save('autoStop', String(state.autoStop));
};

let searchTimer;
$('catalogSearch').oninput = () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => searchCatalog(true), 200);
};
$('catalog').onscroll = (e) => {
  const list = e.target;
  if (state.catalog.hasMore && list.scrollTop + list.clientHeight > list.scrollHeight - 300) searchCatalog(false);
};
window.addEventListener('focus', checkDiscord);
setInterval(tick, 1000);

async function syncCatalog() {
  try {
    const r = await invoke('sync_game_list');
    log(r.updated ? 'Downloaded Discord’s latest game list.' : 'Discord’s game list is up to date.', 'ok');
  } catch (e) {
    log(`Couldn’t update Discord’s game list: ${e}`, 'error');
  }
}

(async function init() {
  $('autoStop').value = String(state.autoStop);
  catalogSync = syncCatalog();
  await refresh();
  checkDiscord();
})();
