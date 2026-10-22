// End-to-end test of the built app, driven through WebView2's DevTools protocol.
// Usage: npm run build && npm run test:e2e [-- screenshot-prefix]
// Uses the real Discord API and your app data folder (it adds, then removes, a test game),
// and creates then deletes a desktop shortcut.
import { spawn, execSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';

const root = path.resolve(import.meta.dirname, '..');
const appExe = path.join(root, 'src-tauri', 'target', 'release', 'endos-discord-game-spoofer.exe');
// Optional screenshot prefix: <prefix>-discover.png, -search.png, -game.png, -group.png.
const shotPrefix = process.argv[2]?.replace(/\.png$/i, '');
const PORT = 9333;
const FAKE = 'e2egame_endo.exe';
const TEST_GAME = { id: '999000', name: 'E2E <img id="xss" src=x> "Game"', exe: `e2e/${FAKE}` };
const KEY = `${TEST_GAME.id}::${TEST_GAME.exe}`;
const RPC_GAME = { id: '999001', name: 'E2E RPC', exe: 'E2E RPC.exe', rpc: true };
const RPC_KEY = `${RPC_GAME.id}::${RPC_GAME.exe}`;
// The test moves the window; the user's saved position is put back afterwards.
const windowStateFile = path.join(process.env.APPDATA, 'com.endo.discordgamespoofer', '.window-state.json');
const savedWindowState = fs.existsSync(windowStateFile) ? fs.readFileSync(windowStateFile) : null;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ps = (script) =>
  execSync(`powershell -NoProfile -NonInteractive -EncodedCommand ${Buffer.from(`$ProgressPreference = 'SilentlyContinue'
${script}`, 'utf16le').toString('base64')}`, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
const fakeRunning = () => execSync(`tasklist /FI "IMAGENAME eq ${FAKE}" /NH`).toString().includes(FAKE);
const alive = (pid) => ps(`[bool](Get-Process -Id ${pid} -ErrorAction SilentlyContinue)`) === 'True';

// Memory of the app and its WebView2 processes (fake games excluded): private MB / working set MB.
function appMemoryMB(pid) {
  return ps(`
    $all = Get-CimInstance Win32_Process
    $ids = [System.Collections.Generic.HashSet[int]]::new(); [void]$ids.Add(${pid})
    do { $n = 0; foreach ($p in $all) { if ($ids.Contains([int]$p.ParentProcessId) -and $ids.Add([int]$p.ProcessId)) { $n++ } } } while ($n)
    $procs = $all | Where-Object { $ids.Contains([int]$_.ProcessId) -and $_.Name -ne '${FAKE}' }
    "$([math]::Round(($procs | Measure-Object PrivatePageCount -Sum).Sum / 1MB)) MB private, $([math]::Round(($procs | Measure-Object WorkingSetSize -Sum).Sum / 1MB)) MB working set"`);
}

const USER32 = `Add-Type -Name W -Namespace U -MemberDefinition '
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  public struct RECT { public int L, T, R, B; }'`;
// MainWindowHandle points at another window while the app is minimized, so the handle is read once up front.
const mainWindow = (pid) => ps(`(Get-Process -Id ${pid}).MainWindowHandle`);
function windowRect(hwnd, move) {
  return ps(`${USER32}
    $h = [IntPtr]${hwnd}
    ${move ? `[void][U.W]::SetWindowPos($h, [IntPtr]::Zero, ${move.join(', ')}, 0x14)` : ''}
    $r = New-Object U.W+RECT; [void][U.W]::GetWindowRect($h, [ref]$r); "$($r.L),$($r.T),$($r.R),$($r.B)"`);
}

async function startApp() {
  const app = spawn(appExe, [], {
    env: {
      ...process.env,
      // Replaces the app's own WebView2 flags, so they are repeated here.
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT} --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-gpu`
    },
    stdio: 'ignore'
  });
  try {
    return await connect(app);
  } catch (e) {
    app.kill();
    throw e;
  }
}

async function connect(app) {
  let target;
  for (let i = 0; i < 100 && !target; i++) {
    await sleep(200);
    try {
      target = (await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()).find((t) => t.type === 'page');
    } catch { /* not up yet */ }
  }
  assert.ok(target, 'app page found');

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener('open', r));
  const pending = new Map();
  const problems = [];
  let nextId = 1;
  ws.addEventListener('message', (m) => {
    const msg = JSON.parse(m.data);
    if (pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); }
    if (msg.method === 'Runtime.exceptionThrown') problems.push(msg.params.exceptionDetails.exception?.description);
    if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') problems.push(JSON.stringify(msg.params.args.map((a) => a.value ?? a.description)));
  });
  const send = (method, params = {}) => new Promise((r) => { const id = nextId++; pending.set(id, r); ws.send(JSON.stringify({ id, method, params })); });
  const run = async (expr) => {
    const r = await send('Runtime.evaluate', { expression: `(async () => { ${expr} })()`, awaitPromise: true, returnByValue: true });
    const ex = r.result.exceptionDetails?.exception;
    if (ex) throw new Error(ex.description ?? JSON.stringify(ex.value));
    return r.result.result.value;
  };
  await send('Runtime.enable');
  // The DevTools target can appear before the app page has loaded.
  for (let i = 0; i < 50 && !(await run(`return Boolean(window.__TAURI__) && typeof state === 'object'`)); i++) await sleep(200);
  await run(`window.__exited = []; window.__TAURI__.event.listen('game-exited', (e) => window.__exited.push(e.payload)); return 1`);
  return { app, ws, send, run, problems };
}

async function shot(name) {
  if (!shotPrefix) return;
  const r = await session.send('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(`${shotPrefix}-${name}.png`, Buffer.from(r.result.data, 'base64'));
}

const call = (cmd, args = {}) => `return window.__TAURI__.core.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)})`;

let session = await startApp();
let { app, run } = session;
try {
  let logText = '';
  for (let i = 0; i < 100 && !/game list/.test(logText); i++) {
    await sleep(200);
    logText = await run(`return document.getElementById('log').innerText`);
  }
  assert.match(logText, /game list/, 'startup sync logged');
  console.log('startup:', logText.trim().split('\n').pop());

  const page = await run(call('search_games', { filter: 'apex legends', limit: 5 }));
  assert.ok(page.items.some((g) => g.name === 'Apex Legends'));
  assert.deepEqual(await run(call('sync_game_list')), { updated: false });
  console.log('catalog search + 304 resync: ok');

  // Search forgives typos and punctuation; mobile-only games come back as Rich Presence entries.
  const first = async (filter, kind) => (await run(call('search_games', { filter, kind, limit: 5 }))).items[0]?.name;
  assert.equal(await first('valornt'), 'VALORANT', 'typo forgiven');
  assert.equal(await first('counterstrike 2'), 'Counter-Strike 2');
  const rok = (await run(call('search_games', { filter: 'rise of kingdoms', kind: 'mobile', limit: 5 }))).items[0];
  assert.ok(rok.name === 'Rise of Kingdoms' && rok.rpc && rok.mobile, 'mobile game shown through Rich Presence');
  assert.equal((await run(call('lookup_app', { id: rok.id }))).name, 'Rise of Kingdoms');
  assert.match(await run(`try { await window.__TAURI__.core.invoke('lookup_app', { id: 'abc' }); return 'accepted'; } catch (e) { return String(e); }`), /long number/);
  console.log('search (typos, punctuation, mobile filter) / app lookup: ok');

  const d = await run(call('discover'));
  assert.ok(d.beyondSteam.some((c) => c.game.name === 'VALORANT'));
  console.log(`discover: ${d.mostPlayed.length} most played, ${d.topRated.length} top rated, ${d.beyondSteam.length} beyond Steam, ${d.mobile.length} mobile, ${d.count} in Discord's list`);
  const installedCount = (await run(call('scan_installed'))).length;
  console.log(`import from PC: ${installedCount} installed games found in Discord's list`);
  await run(`setView('discover'); return 1`);
  await sleep(3000);
  await shot('discover');
  await run(`const s = document.getElementById('catalogSearch'); s.value = 'rise'; s.dispatchEvent(new Event('input')); return 1`);
  await sleep(2500);
  await shot('search');

  console.log('discord running:', await run(call('discord_running')));
  const previousView = await run(`return localStorage.getItem('view')`);

  await run(call('remove_games', { keys: [KEY, RPC_KEY] })); // leftovers from an interrupted run
  const [added] = await run(call('add_games', { games: [TEST_GAME] }));
  assert.equal(added.key, KEY);
  const rejected = await run(`try { await window.__TAURI__.core.invoke('add_games', { games: [{ id: '1', name: 'x', exe: '../../evil.exe' }] }); return 'accepted'; } catch (e) { return String(e); }`);
  assert.match(rejected, /Unsupported exe path/);
  assert.equal((await run(call('toggle_favorite', { key: KEY }))).isFavorite, true);
  await run(`await refresh(); setView('game', ${JSON.stringify(KEY)}); return 1`);
  assert.equal(await run(`return document.getElementById('xss')`), null, 'game names are shown as text');
  console.log('add / reject unsafe path / favorite / escaping: ok');

  // "Launch favorites" button, unless the real library has other favorites it would also launch.
  const favorites = await run(`return state.library.filter((g) => g.isFavorite).map((g) => g.key)`);
  if (favorites.length === 1) await run(`document.getElementById('launchFavoritesBtn').click(); return 1`);
  else await run(call('launch_game', { key: KEY }));
  await sleep(1500);
  assert.ok(fakeRunning(), 'fake game running');
  const timeText = await run(`return document.querySelector('[data-time-for="${KEY}"]').textContent`);
  assert.match(timeText, /^\d+:\d\d$/, 'play time shown');
  assert.ok((await run(call('launch_game', { key: KEY }))).alreadyRunning);
  console.log(`launch${favorites.length === 1 ? ' via favorites button' : ''} / play time "${timeText}" / alreadyRunning: ok`);

  await shot('game');

  assert.equal(await run(call('stop_game', { key: KEY })), true);
  await sleep(500);
  assert.ok(!fakeRunning(), 'fake game stopped');

  // Auto-stop: 0.05 min = 3 s; the fake game closes itself and the app reports it.
  const timed = await run(call('launch_game', { key: KEY, minutes: 0.05 }));
  assert.equal(timed.run.stopAt - timed.run.startedAt, 3000);
  await sleep(5000);
  assert.ok(!fakeRunning(), 'auto-stopped');
  assert.deepEqual(await run(call('get_running')), {});

  await run(call('launch_game', { key: KEY }));
  await sleep(800);
  execSync(`taskkill /IM ${FAKE} /F`);
  await sleep(1200);
  const exited = await run(`return window.__exited`);
  assert.deepEqual(exited, [KEY, KEY, KEY], 'stop, auto-stop and external kill all reported');
  console.log('stop / auto-stop / external exit events: ok');

  // Custom status: saved to the library and written next to the fake exe, which re-reads it while running.
  const presenceFile = path.join(process.env.APPDATA, 'com.endo.discordgamespoofer', 'games', TEST_GAME.id, 'e2e', `${FAKE}.presence`);
  const presence = { activityType: 5, details: 'Competitive', state: 'x', partySize: 2, partyMax: 5, timer: 'left', timerMinutes: 5 };
  const saved = await run(call('set_presence', { key: KEY, presence }));
  assert.equal(saved.presence.details, 'Competitive');
  const [fileId, activity] = fs.readFileSync(presenceFile, 'utf8').split('\n');
  assert.equal(fileId, TEST_GAME.id);
  const { timestamps, ...rest } = JSON.parse(activity);
  assert.ok(Math.abs(timestamps.end - (Date.now() + 5 * 60_000)) < 10_000, 'countdown ends in 5 minutes');
  assert.deepEqual(rest, { type: 5, details: 'Competitive', party: { size: [2, 5] } }, 'too-short state left out');
  assert.equal((await run(call('set_presence', { key: KEY, presence: null }))).presence, undefined);
  assert.ok(!fs.existsSync(presenceFile), 'cleared status removes the file');
  // Auto-play steps through statuses while the game runs and ends when it stops.
  assert.match(await run(`try { await window.__TAURI__.core.invoke('play_presence', { key: ${JSON.stringify(KEY)}, steps: [{ presence: {}, seconds: 1 }] }); return 'accepted'; } catch (e) { return String(e); }`), /Launch the game first/);
  await run(call('launch_game', { key: KEY }));
  const steps = [{ presence: { state: 'In Lobby' }, seconds: 1 }, { presence: { state: 'In Match • 1 - 0' }, seconds: 60, keepTimer: true }];
  await run(call('play_presence', { key: KEY, steps }));
  await sleep(2500);
  assert.match(fs.readFileSync(presenceFile, 'utf8'), /In Match • 1 - 0/, 'second step shown');
  assert.deepEqual(await run(call('get_autoplay')), [KEY]);
  await run(call('stop_game', { key: KEY }));
  await sleep(1500);
  assert.deepEqual(await run(call('get_autoplay')), [], 'auto-play ends with the game');
  // A generated VALORANT session: lobby, queue, agent select, then rounds up to 13.
  const plan = await run(`return sessionPlan(presenceExample({ name: 'VALORANT' }), {})`);
  const lines = plan.map((s) => s.presence.state);
  assert.deepEqual(lines.slice(0, 4), ['In Lobby', 'In Queue', 'Agent Select', 'In Match • 0 - 0']);
  assert.ok(lines.some((s) => /^(Victory • 13 - \d+|Defeat • \d+ - 13)$/.test(s)), 'a match ends at 13');
  assert.ok(plan.reduce((t, s) => t + s.seconds, 0) >= 4 * 3600, 'about four hours of sessions');
  // +1 you: the last score in the second line goes up and is saved.
  await run(`setView('game', ${JSON.stringify(KEY)}); document.getElementById('presenceForm').elements.state.value = 'In Match • 12 - 3'; bumpScore(0); return 1`);
  await sleep(800);
  assert.equal((await run(call('get_library'))).find((g) => g.key === KEY).presence.state, 'In Match • 13 - 3');
  await run(call('set_presence', { key: KEY, presence: null }));
  console.log('custom Discord status file / auto-play / session plan / score button: ok');

  const sc = await run(call('create_shortcut', { key: KEY }));
  assert.ok(sc.ok, JSON.stringify(sc));
  const lnk = ps(`$s = (New-Object -ComObject WScript.Shell).CreateShortcut('${sc.path.replace(/'/g, "''")}'); $s.TargetPath; $s.Arguments`).split(/\r?\n/);
  fs.unlinkSync(sc.path);
  assert.ok(lnk[0].endsWith(FAKE));
  console.log('desktop shortcut: ok');

  // Rich Presence game: the fake exe gets --rpc=<app id>. The id is made up, so Discord ignores it.
  await run(call('add_games', { games: [RPC_GAME] }));
  assert.match(await run(`try { await window.__TAURI__.core.invoke('add_games', { games: [{ id: 'x1', name: 'x', exe: 'x.exe', rpc: true }] }); return 'accepted'; } catch (e) { return String(e); }`), /Unsupported app id/);
  // A group launches both games from its page, then stops them together.
  const group = await run(call('save_group', { group: { id: '', name: 'E2E group', keys: [KEY, RPC_KEY, KEY] } }));
  assert.deepEqual(group.keys, [KEY, RPC_KEY], 'duplicate keys dropped');
  await run(`await refresh(); setView('group', ${JSON.stringify(group.id)}); document.getElementById('groupLaunchBtn').click(); return 1`);
  await sleep(2000);
  assert.ok(fakeRunning(), 'group launched the exe game');
  assert.match(ps(`(Get-CimInstance Win32_Process -Filter "Name='${RPC_GAME.exe}'").CommandLine`), /--rpc=999001/);
  assert.equal(await run(`return document.getElementById('groupLaunchBtn').textContent`), 'Stop all');
  await shot('group');
  await run(`document.getElementById('groupLaunchBtn').click(); return 1`);
  await sleep(1000);
  assert.deepEqual(await run(call('get_running')), {}, 'group stopped');
  await run(call('delete_group', { id: group.id }));
  await run(call('remove_games', { keys: [RPC_KEY] }));
  console.log('Rich Presence launch / group launch + stop: ok');

  // Import dialog and library multi-select, UI only: nothing is added or removed.
  if (installedCount) {
    await run(`importFromPc(); return 1`);
    await sleep(2500);
    assert.ok(await run(`return document.getElementById('pickDialog').open`), 'import dialog opened');
    await shot('import');
    await run(`document.getElementById('pickDialog').close('cancel'); return 1`);
  }
  assert.equal(await run(`setSelecting(true); toggleSelected(${JSON.stringify(KEY)}); return document.getElementById('selectionCount').textContent`), '1 selected');
  await shot('select');
  await run(`setSelecting(false); return 1`);
  console.log('import dialog / multi-select: ok');

  // Minimized: WebView2 is asked to trim memory (informational).
  const hwnd = mainWindow(app.pid);
  const before = appMemoryMB(app.pid);
  await run(`window.__TAURI__.window.getCurrentWindow().minimize(); return 1`);
  await sleep(4000);
  console.log(`memory open: ${before}
memory minimized: ${appMemoryMB(app.pid)}`);
  ps(`${USER32}
[void][U.W]::ShowWindow([IntPtr]${hwnd}, 9)`);
  await sleep(1500);

  // Remembered window position: move it, close, reopen (checked after restart).
  const rect = windowRect(hwnd, [140, 90, 1000, 640]);

  // Tray mode: closing the window with a game running keeps the app alive without WebView2.
  await run(call('launch_game', { key: KEY }));
  await sleep(800);
  assert.deepEqual(session.problems, [], 'no page errors');
  await run(`window.__TAURI__.window.getCurrentWindow().close(); return 1`).catch(() => {});
  session.ws.close();
  await sleep(3000);
  assert.ok(alive(app.pid), 'app stays in the tray while a game runs');
  console.log(`tray mode: app alive, ${appMemoryMB(app.pid)} with the window closed`);
  execSync(`taskkill /IM ${FAKE} /F`);
  await sleep(2000);
  assert.ok(!alive(app.pid), 'app quits once nothing is running and no window is open');
  console.log('tray mode: quits when the last game exits');

  session = await startApp();
  ({ app, run } = session);
  await sleep(1000);
  assert.equal(windowRect(mainWindow(app.pid)), rect, 'window position and size restored');
  console.log('window position remembered: ok');
  await run(call('remove_games', { keys: [KEY] }));
  assert.ok(!(await run(call('get_library'))).some((g) => g.key === KEY));
  await run(`localStorage.setItem('view', ${JSON.stringify(previousView ?? 'discover')}); return 1`);
  assert.deepEqual(session.problems, [], 'no page errors');
  console.log('remove: ok');
  await run(`window.__TAURI__.window.getCurrentWindow().close(); return 1`).catch(() => {});
  await sleep(1500);
  assert.ok(!alive(app.pid), 'closing with nothing running quits');
  console.log('ALL OK');
} finally {
  session.ws.close();
  if (alive(app.pid)) app.kill();
  if (fakeRunning()) execSync(`taskkill /IM ${FAKE} /F`);
  if (savedWindowState) fs.writeFileSync(windowStateFile, savedWindowState);
  else fs.rmSync(windowStateFile, { force: true });
}
