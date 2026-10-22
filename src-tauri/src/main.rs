// Backend for the UI in ui/: Discord's game list, the user's library, fake game processes, tray and updates.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_updater::{Update, UpdaterExt};

const DISCORD_DETECTABLE_URL: &str = "https://discord.com/api/applications/detectable";
const DISCORD_PROCESSES: [&str; 4] = ["discord.exe", "discordptb.exe", "discordcanary.exe", "discorddevelopment.exe"];
// %APPDATA% folder of the old Electron app; its myGames.json is used until we write our own.
const ELECTRON_APP_DIR: &str = "discord-fake-game-launcher-ui";
// Built from src/dummygame by build.rs; written out under each game's exe name.
const DUMMY_EXE: &[u8] = include_bytes!(env!("DUMMY_EXE"));

#[derive(Default)]
struct AppState {
    db: Mutex<Option<Db>>,
    running: Mutex<HashMap<String, RunningGame>>,
    update: Mutex<Option<Update>>,
    downloaded: Mutex<Option<(Update, Vec<u8>)>>,
}

struct RunningGame {
    child: Child,
    info: RunInfo,
}

/// Times in milliseconds since the Unix epoch, for the UI's play time and auto-stop countdown.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
struct RunInfo {
    started_at: u64,
    stop_at: Option<u64>,
}

/// Slim copy of Discord's detectable list; `names` are lowercased for search.
struct Db {
    games: Vec<DbGame>,
    names: Vec<String>,
}

impl Db {
    fn new(games: Vec<DbGame>) -> Self {
        let names = games.iter().map(|g| g.name.to_lowercase()).collect();
        Db { games, names }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DbGame {
    id: String,
    name: String,
    exe: String,
    is_launcher: bool,
    uses_new_detection: bool,
}

#[derive(Serialize, Deserialize)]
struct Index {
    etag: Option<String>,
    games: Vec<DbGame>,
}

#[derive(Deserialize)]
struct DetectableApp {
    id: Option<String>,
    name: Option<String>,
    executables: Option<Vec<Executable>>,
}

#[derive(Deserialize)]
struct Executable {
    name: Option<String>,
    os: Option<String>,
    is_launcher: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct MyGame {
    // The add-game dialog sends Discord's `id`; the library stores it as `appId`.
    #[serde(alias = "id")]
    app_id: String,
    name: String,
    exe: String,
    is_favorite: bool,
}

/// A library game as sent to the UI, with the key every other command takes.
#[derive(Serialize)]
struct GameView {
    #[serde(flatten)]
    game: MyGame,
    key: String,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn sanitize_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    replaced.trim().chars().take(128).collect()
}

fn fallback_exe_name(title: &str) -> String {
    let base: String = sanitize_name(title).split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
    let base = if base.is_empty() { "Game".to_string() } else { base };
    if base.to_lowercase().ends_with(".exe") { base } else { format!("{base}.exe") }
}

fn to_db_games(apps: Vec<DetectableApp>) -> Vec<DbGame> {
    let mut games: Vec<DbGame> = apps
        .into_iter()
        .filter_map(|app| {
            let name = app.name.filter(|n| !n.is_empty())?;
            let exes = app.executables.unwrap_or_default();
            let win32 = |e: &&Executable| {
                e.os.as_deref().is_some_and(|os| os.eq_ignore_ascii_case("win32"))
                    && e.name.as_deref().is_some_and(|n| !n.is_empty())
            };
            // Prefer the real game exe over its launcher.
            let best = exes.iter().filter(win32).find(|e| e.is_launcher != Some(true)).or_else(|| exes.iter().find(win32));
            Some(DbGame {
                id: app.id.unwrap_or_default(),
                exe: best.and_then(|e| e.name.clone()).unwrap_or_else(|| fallback_exe_name(&name)),
                is_launcher: best.is_some_and(|e| e.is_launcher == Some(true)),
                uses_new_detection: best.is_none(),
                name,
            })
        })
        .collect();
    games.sort_by_cached_key(|g| g.name.to_lowercase());
    games
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(err)?;
    fs::create_dir_all(&dir).map_err(err)?;
    Ok(dir)
}

fn read_index(app: &AppHandle) -> Option<Index> {
    serde_json::from_slice(&fs::read(data_dir(app).ok()?.join("games-index.json")).ok()?).ok()
}

fn read_my_games(app: &AppHandle) -> Result<Vec<MyGame>, String> {
    let mut path = data_dir(app)?.join("myGames.json");
    if !path.exists() {
        path = app.path().data_dir().map_err(err)?.join(ELECTRON_APP_DIR).join("myGames.json");
    }
    Ok(fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
}

fn write_my_games(app: &AppHandle, games: &[MyGame]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(games).map_err(err)?;
    fs::write(data_dir(app)?.join("myGames.json"), json).map_err(err)
}

fn find_game(app: &AppHandle, key: &str) -> Result<MyGame, String> {
    read_my_games(app)?.into_iter().find(|g| game_key(g) == key).ok_or_else(|| "Game not found in library.".into())
}

fn game_key(game: &MyGame) -> String {
    format!("{}::{}", game.app_id, game.exe)
}

fn view(game: MyGame) -> GameView {
    GameView { key: game_key(&game), game }
}

/// games/-relative path of a game's fake exe: <appId>/<exe path from Discord>.
/// These values come from the UI, so only plain names that stay inside games/ are allowed.
fn fake_exe_rel_path(game: &MyGame) -> Result<PathBuf, String> {
    let folder = if game.app_id.is_empty() { sanitize_name(&game.name) } else { game.app_id.clone() };
    let parts: Vec<&str> = std::iter::once(folder.as_str()).chain(game.exe.split(['/', '\\'])).collect();
    let unsafe_part = |p: &&str| {
        p.is_empty() || *p == "." || *p == ".." || p.chars().any(|c| "<>:\"/\\|?*".contains(c) || c.is_control())
    };
    if parts.iter().any(unsafe_part) {
        return Err(format!("Unsupported exe path: {}", game.exe));
    }
    Ok(parts.iter().collect())
}

/// Writes the fake exe for a game (if missing or outdated) and returns its path.
fn ensure_fake_exe(app: &AppHandle, game: &MyGame) -> Result<PathBuf, String> {
    let dest = data_dir(app)?.join("games").join(fake_exe_rel_path(game)?);
    fs::create_dir_all(dest.parent().unwrap()).map_err(err)?;

    // Also replaces outdated copies (e.g. the .NET DummyGame from older versions).
    if fs::read(&dest).ok().as_deref() != Some(DUMMY_EXE) {
        if let Err(e) = fs::write(&dest, DUMMY_EXE) {
            // A copy that is still running (e.g. started from a desktop shortcut) is locked; keep using it.
            if !dest.exists() {
                return Err(e.to_string());
            }
        }
    }
    Ok(dest)
}

fn discord_is_running() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more && !found {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            found = DISCORD_PROCESSES.contains(&String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase().as_str());
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        found
    }
}

/// Keeps the tray tooltip current and quits once there is no window and nothing running.
fn games_changed(app: &AppHandle) {
    let count = app.state::<AppState>().running.lock().unwrap().len();
    if let Some(tray) = app.tray_by_id("main") {
        let product = app.config().product_name.clone().unwrap_or_default();
        let tip = match count {
            0 => product,
            1 => format!("{product} – 1 game running"),
            n => format!("{product} – {n} games running"),
        };
        let _ = tray.set_tooltip(Some(tip));
    }
    if count == 0 && app.webview_windows().is_empty() {
        app.exit(0);
    }
}

/// Reports a fake game that exits on its own (closed, auto-stop, killed); stop_game reports its own.
fn watch_exit(app: AppHandle, key: String, pid: u32) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        {
            let state = app.state::<AppState>();
            let mut running = state.running.lock().unwrap();
            match running.get_mut(&key) {
                Some(game) if game.child.id() == pid => {
                    if let Ok(None) = game.child.try_wait() {
                        continue;
                    }
                    running.remove(&key);
                }
                _ => return,
            }
        }
        let _ = app.emit("game-exited", &key);
        games_changed(&app);
        return;
    });
}

fn stop(app: &AppHandle, key: &str) -> bool {
    let game = app.state::<AppState>().running.lock().unwrap().remove(key);
    let Some(mut game) = game else { return false };
    let _ = game.child.kill();
    let _ = game.child.wait();
    let _ = app.emit("game-exited", key);
    true
}

fn stop_all(app: &AppHandle) {
    let keys: Vec<String> = app.state::<AppState>().running.lock().unwrap().keys().cloned().collect();
    for key in keys {
        stop(app, &key);
    }
    games_changed(app);
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    // Closing destroys the window (freeing WebView2's memory), so build it again from the config.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(config) = app.config().app.windows.first().cloned() {
            let _ = WebviewWindowBuilder::from_config(&app, &config).and_then(|b| b.build());
        }
    });
}

/// Asks WebView2 to trim its memory while the window is minimized.
fn set_low_memory(window: &tauri::Window, low: bool) {
    let Some(webview) = window.app_handle().get_webview_window(window.label()) else { return };
    let _ = webview.with_webview(move |w| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
        };
        use windows_core::Interface;
        if let Ok(core) = w.controller().CoreWebView2().and_then(|c| c.cast::<ICoreWebView2_19>()) {
            let level = if low { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL };
            let _ = core.SetMemoryUsageTargetLevel(level);
        }
    });
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop-all", "Stop all games", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip(app.config().product_name.clone().unwrap_or_default())
        .menu(&Menu::with_items(app, &[&open, &stop, &quit])?)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "stop-all" => stop_all(app),
            "quit" => {
                stop_all(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

#[tauri::command]
async fn sync_game_list(app: AppHandle, state: State<'_, AppState>) -> Result<Value, String> {
    let mut req = reqwest::Client::new()
        .get(DISCORD_DETECTABLE_URL)
        .header("User-Agent", "EndosDiscordGameSpoofer/0.1.0")
        .timeout(Duration::from_secs(60));
    // Discord answers 304 when the list is unchanged, skipping the ~13 MB download.
    if let Some(etag) = read_index(&app).and_then(|i| i.etag) {
        req = req.header("If-None-Match", etag);
    }
    let res = req.send().await.map_err(err)?;
    if res.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(json!({ "updated": false }));
    }
    if !res.status().is_success() {
        return Err(format!("Discord API error: {}", res.status()));
    }
    let etag = res.headers().get("etag").and_then(|v| v.to_str().ok()).map(String::from);
    // Keep only what the UI needs (~4x smaller than Discord's raw list).
    let index = Index { etag, games: to_db_games(res.json().await.map_err(err)?) };
    fs::write(data_dir(&app)?.join("games-index.json"), serde_json::to_vec(&index).map_err(err)?).map_err(err)?;
    *state.db.lock().unwrap() = Some(Db::new(index.games));
    Ok(json!({ "updated": true }))
}

#[tauri::command]
fn search_games(
    app: AppHandle,
    state: State<'_, AppState>,
    filter: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Value {
    let mut db = state.db.lock().unwrap();
    let db = db.get_or_insert_with(|| Db::new(read_index(&app).map(|i| i.games).unwrap_or_default()));
    let term = filter.unwrap_or_default().trim().to_lowercase();
    let (offset, limit) = (offset.unwrap_or(0), limit.unwrap_or(200).clamp(1, 500));
    let mut matches = db.games.iter().zip(&db.names).filter(|(_, n)| n.contains(&term)).map(|(g, _)| g).skip(offset);
    let items: Vec<&DbGame> = matches.by_ref().take(limit).collect();
    json!({ "items": items, "hasMore": matches.next().is_some() })
}

#[tauri::command]
fn get_library(app: AppHandle) -> Result<Vec<GameView>, String> {
    Ok(read_my_games(&app)?.into_iter().map(view).collect())
}

#[tauri::command]
fn add_game(app: AppHandle, game: MyGame) -> Result<GameView, String> {
    let mut entry = game;
    if entry.name.is_empty() {
        entry.name = "Game".into();
    }
    entry.is_favorite = false;
    fake_exe_rel_path(&entry)?;
    let mut games = read_my_games(&app)?;
    if !games.iter().any(|g| game_key(g) == game_key(&entry)) {
        games.push(entry.clone());
        write_my_games(&app, &games)?;
    }
    Ok(view(entry))
}

#[tauri::command]
fn toggle_favorite(app: AppHandle, key: String) -> Result<Option<GameView>, String> {
    let mut games = read_my_games(&app)?;
    let updated = games.iter_mut().find(|g| game_key(g) == key).map(|g| {
        g.is_favorite = !g.is_favorite;
        g.clone()
    });
    write_my_games(&app, &games)?;
    Ok(updated.map(view))
}

#[tauri::command]
fn remove_game(app: AppHandle, key: String) -> Result<(), String> {
    stop(&app, &key);
    let mut games = read_my_games(&app)?;
    games.retain(|g| game_key(g) != key);
    write_my_games(&app, &games)
}

#[tauri::command]
fn create_shortcut(app: AppHandle, key: String) -> Value {
    let result = (|| -> Result<PathBuf, String> {
        let game = find_game(&app, &key)?;
        let target = ensure_fake_exe(&app, &game)?;
        let desktop = app.path().desktop_dir().map_err(err)?;
        let base = Some(sanitize_name(&game.name)).filter(|n| !n.is_empty()).unwrap_or_else(|| "Game".into());
        // Don't overwrite existing shortcuts: Game.lnk, Game (2).lnk, ...
        let path = std::iter::once(desktop.join(format!("{base}.lnk")))
            .chain((2..1000).map(|i| desktop.join(format!("{base} ({i}).lnk"))))
            .find(|p| !p.exists())
            .ok_or("Too many shortcuts with this name.")?;
        let mut link = mslnk::ShellLink::new(&target).map_err(err)?;
        link.set_arguments(Some(format!("\"{}\"", game.name.replace('"', "\\\""))));
        link.set_working_dir(target.parent().map(|p| p.display().to_string()));
        link.create_lnk(&path).map_err(err)?;
        Ok(path)
    })();
    match result {
        Ok(path) => json!({ "ok": true, "path": path }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

#[tauri::command]
fn launch_game(app: AppHandle, state: State<'_, AppState>, key: String, minutes: Option<f64>) -> Value {
    if let Some(game) = state.running.lock().unwrap().get_mut(&key) {
        if let Ok(None) = game.child.try_wait() {
            return json!({ "ok": true, "alreadyRunning": true, "run": game.info });
        }
    }
    let minutes = minutes.filter(|m| m.is_finite() && *m > 0.0);
    let started = (|| -> Result<Child, String> {
        let game = find_game(&app, &key)?;
        let exe = ensure_fake_exe(&app, &game)?;
        // The fake exe shows the name as its window title and closes itself after `minutes`.
        let mut command = Command::new(&exe);
        command.arg(if game.name.is_empty() { "Game" } else { &game.name }).current_dir(exe.parent().unwrap());
        if let Some(m) = minutes {
            command.arg(m.to_string());
        }
        command.spawn().map_err(err)
    })();
    let child = match started {
        Ok(child) => child,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let started_at = now_ms();
    let info = RunInfo { started_at, stop_at: minutes.map(|m| started_at + (m * 60_000.0) as u64) };
    let pid = child.id();
    state.running.lock().unwrap().insert(key.clone(), RunningGame { child, info });
    watch_exit(app.clone(), key, pid);
    games_changed(&app);
    json!({ "ok": true, "run": info })
}

#[tauri::command]
fn stop_game(app: AppHandle, key: String) -> bool {
    let stopped = stop(&app, &key);
    games_changed(&app);
    stopped
}

#[tauri::command]
fn get_running(state: State<'_, AppState>) -> HashMap<String, RunInfo> {
    state.running.lock().unwrap().iter().map(|(key, game)| (key.clone(), game.info)).collect()
}

#[tauri::command]
fn discord_running() -> bool {
    discord_is_running()
}

async fn check_for_update(app: &AppHandle) -> Result<(), String> {
    let Some(update) = app.updater().map_err(err)?.check().await.map_err(err)? else {
        return Ok(());
    };
    let _ = app.emit(
        "update-available",
        json!({ "version": update.version, "notes": update.body.clone().unwrap_or_default() }),
    );
    *app.state::<AppState>().update.lock().unwrap() = Some(update);
    Ok(())
}

#[tauri::command]
async fn update_install(app: AppHandle, state: State<'_, AppState>) -> Result<Value, String> {
    let Some(update) = state.update.lock().unwrap().take() else {
        return Ok(json!({ "ok": false, "error": "No update available." }));
    };
    let (started, mut transferred) = (Instant::now(), 0u64);
    let on_chunk = |chunk: usize, total: Option<u64>| {
        transferred += chunk as u64;
        let total = total.unwrap_or(0);
        let _ = app.emit(
            "update-progress",
            json!({
                "percent": if total > 0 { transferred as f64 * 100.0 / total as f64 } else { 0.0 },
                "transferred": transferred,
                "total": total,
                "bytesPerSecond": transferred as f64 / started.elapsed().as_secs_f64().max(0.001),
            }),
        );
    };
    match update.download(on_chunk, || {}).await {
        Ok(bytes) => {
            *state.downloaded.lock().unwrap() = Some((update, bytes));
            let _ = app.emit("update-downloaded", ());
            Ok(json!({ "ok": true }))
        }
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

#[tauri::command]
fn update_quit_and_install(state: State<'_, AppState>) -> Value {
    let Some((update, bytes)) = state.downloaded.lock().unwrap().take() else {
        return json!({ "ok": false, "error": "Update not downloaded." });
    };
    // On Windows this starts the installer and exits the app.
    match update.install(bytes) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    tauri::Builder::default()
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(AppState::default())
        .setup(|app| {
            build_tray(app)?;
            // Updates turn on once tauri.conf.json has plugins.updater (endpoints + pubkey); off in dev builds.
            if !cfg!(debug_assertions) && app.config().plugins.0.contains_key("updater") {
                app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;
                let handle = app.handle().clone();
                // A failed check (offline, no release yet, private repo) is not worth an error in the log.
                tauri::async_runtime::spawn(async move {
                    let _ = check_for_update(&handle).await;
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Resized(_) = event {
                set_low_memory(window, window.is_minimized().unwrap_or(false));
            }
        })
        .invoke_handler(tauri::generate_handler![
            sync_game_list,
            search_games,
            get_library,
            add_game,
            toggle_favorite,
            remove_game,
            create_shortcut,
            launch_game,
            stop_game,
            get_running,
            discord_running,
            update_install,
            update_quit_and_install
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Closing the last window keeps the app in the tray while fake games run.
            if let RunEvent::ExitRequested { code: None, api, .. } = event {
                if !app.state::<AppState>().running.lock().unwrap().is_empty() {
                    api.prevent_exit();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(name: &str, os: &str, is_launcher: bool) -> Executable {
        Executable { name: Some(name.into()), os: Some(os.into()), is_launcher: Some(is_launcher) }
    }

    fn detectable(id: &str, name: Option<&str>, executables: Vec<Executable>) -> DetectableApp {
        DetectableApp { id: Some(id.into()), name: name.map(String::from), executables: Some(executables) }
    }

    fn game(app_id: &str, exe: &str) -> MyGame {
        MyGame { app_id: app_id.into(), name: "Game".into(), exe: exe.into(), is_favorite: false }
    }

    #[test]
    fn picks_game_exe_over_launcher_and_falls_back_to_title() {
        let games = to_db_games(vec![
            detectable("2", Some("Zeta"), vec![exe("launcher.exe", "win32", true), exe("zeta/game.exe", "win32", false), exe("zeta", "linux", false)]),
            detectable("1", Some("alpha: the game?"), vec![exe("Alpha.app", "darwin", false)]),
            detectable("3", Some("Only Launcher"), vec![exe("ol.exe", "win32", true)]),
            detectable("4", None, vec![exe("x.exe", "win32", false)]),
        ]);
        let got: Vec<_> = games.iter().map(|g| (g.name.as_str(), g.exe.as_str(), g.is_launcher, g.uses_new_detection)).collect();
        assert_eq!(
            got,
            vec![
                ("alpha: the game?", "alpha_ the game_.exe", false, true),
                ("Only Launcher", "ol.exe", true, false),
                ("Zeta", "zeta/game.exe", false, false),
            ]
        );
    }

    #[test]
    fn fake_exe_paths_stay_inside_games_folder() {
        let expected: PathBuf = ["42", "win64", "cs2.exe"].iter().collect();
        assert_eq!(fake_exe_rel_path(&game("42", "win64\\cs2.exe")).unwrap(), expected);
        assert_eq!(fake_exe_rel_path(&game("", "a.exe")).unwrap(), PathBuf::from("Game").join("a.exe"));
        for bad in ["../evil.exe", "a/../../evil.exe", "C:/evil.exe", ">hl2.exe", "a//b.exe", ""] {
            assert!(fake_exe_rel_path(&game("42", bad)).is_err(), "{bad} should be rejected");
        }
        assert!(fake_exe_rel_path(&game("4/2", "a.exe")).is_err());
        assert!(fake_exe_rel_path(&game("..", "a.exe")).is_err());
    }

    #[test]
    fn fallback_exe_names_are_safe_file_names() {
        assert_eq!(fallback_exe_name("  Foo:  Bar? "), "Foo_ Bar_.exe");
        assert_eq!(fallback_exe_name(""), "Game.exe");
        assert_eq!(fallback_exe_name("Tool.EXE"), "Tool.EXE");
    }

    #[test]
    fn library_keys_round_trip_through_json() {
        let parsed: MyGame = serde_json::from_str(r#"{"id":"7","name":"N","exe":"n.exe","icon":"dropped"}"#).unwrap();
        assert_eq!(game_key(&parsed), "7::n.exe");
        let shown = serde_json::to_value(view(parsed)).unwrap();
        assert_eq!(shown, json!({ "appId": "7", "name": "N", "exe": "n.exe", "isFavorite": false, "key": "7::n.exe" }));
    }
}
