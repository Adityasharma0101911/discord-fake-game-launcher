// Backend for the UI in ui/: Discord's game list, the user's library, fake game processes, tray and updates.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_updater::{Update, UpdaterExt};

mod installed;

const DISCORD_DETECTABLE_URL: &str = "https://discord.com/api/applications/detectable";
// Public info (name, icon, cover) of any Discord app, for apps added by id.
const DISCORD_APP_URL: &str = "https://discord.com/api/v9/applications";
const STEAM_PLAYERS_URL: &str = "https://api.steampowered.com/ISteamChartsService/GetGamesByConcurrentPlayers/v1/";
const STEAMSPY_TOP_URL: &str = "https://steamspy.com/api.php?request=top100in2weeks";
const USER_AGENT: &str = "EndosDiscordGameSpoofer";
const DISCORD_PROCESSES: [&str; 4] = ["discord.exe", "discordptb.exe", "discordcanary.exe", "discorddevelopment.exe"];
// %APPDATA% folder of the old Electron app; its myGames.json is used until we write our own.
const ELECTRON_APP_DIR: &str = "discord-fake-game-launcher-ui";
// Built from src/dummygame by build.rs; written out under each game's exe name.
const DUMMY_EXE: &[u8] = include_bytes!(env!("DUMMY_EXE"));
// Bump when the index format changes: the next start then downloads the list again instead of trusting its ETag.
const INDEX_VERSION: u32 = 2;
const POPULAR_MAX_AGE_MS: u64 = 6 * 60 * 60 * 1000;
// Discord app ids of popular Rich Presence plugins (not in the detectable list), shown as the "Apps" shelf:
// VS Code, JetBrains IDE, Visual Studio, Neovim, Sublime Text, Zed, Xcode, Unity, GitHub, YouTube, YouTube Music,
// Netflix, Prime Video, Disney+, Crunchyroll, Twitch, SoundCloud, VLC.
const APPS: &[&str] = &[
    "383226320970055681",
    "384215522050572288",
    "551675228691103796",
    "793271441293967371",
    "389368374645227520",
    "1263505205522337886",
    "700358131481444403",
    "509465267630374935",
    "607587875122446359",
    "463097721130188830",
    "463151177836658699",
    "499981204045430784",
    "705139844883677224",
    "630236276829716483",
    "608065709741965327",
    "802958789555781663",
    "802958833214423081",
    "721748388143562852",
    // Figma, Apple Music, Canva, Duolingo, Jellyfin, Stack Overflow, Wikipedia, Google Meet, Zoom, Microsoft Teams,
    // Instagram, GitLab.
    "768942376403075073",
    "842112189618978897",
    "670612134878773297",
    "1177802176140156998",
    "669359568391766018",
    "610123745033584651",
    "609364070684033044",
    "701914032541794386",
    "765876503161733140",
    "846725225219489812",
    "547436289960574977",
    "709526684428271687",
];
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "EndosDiscordGameSpoofer";
// Hand-picked by their name in Discord's list: there is no keyless popularity data outside Steam.
const BEYOND_STEAM: &[&str] = &[
    "VALORANT",
    "League of Legends",
    "Fortnite",
    "Minecraft",
    "ROBLOX",
    "Genshin Impact",
    "Honkai: Star Rail",
    "Zenless Zone Zero",
    "Overwatch",
    "World of Warcraft",
    "Hearthstone",
    "Diablo IV",
    "Rocket League",
    "Teamfight Tactics",
    "Legends of Runeterra",
    "osu!",
];
const MOBILE: &[&str] = &[
    "Rise of Kingdoms",
    "Whiteout Survival",
    "Clash of Clans",
    "Clash Royale",
    "Candy Crush Saga",
    "Free Fire",
    "Mobile Legends: Bang Bang",
    "Raid: Shadow Legends",
    "Piano Tiles 2",
];
// A plain Rich Presence app with no name of its own on profiles, for games missing from Discord's list.
const GENERIC_APP_ID: &str = "1054951789318909972";

/// Mobile games missing from Discord's list. They are shown through the generic app under their own name
/// (Rich Presence entries send their library name), with art from a full image link.
fn custom_games() -> Vec<DbGame> {
    let game = |name: &str, cover: &str| DbGame {
        id: GENERIC_APP_ID.into(),
        exe: fallback_exe_name(name),
        name: name.into(),
        rpc: true,
        mobile: true,
        cover: Some(cover.into()),
        ..Default::default()
    };
    vec![game("Piano Tiles 2", "https://play-lh.googleusercontent.com/f7YcB5_a-gTjR3joD0qijksY2WYP7gcesj5vK3S2Wj5zGrF0H4xR7s_KHalQl9fS-dMJd1tF7dxd_Z1ZsDdh8pY=s512")]
}

fn with_custom(mut games: Vec<DbGame>) -> Vec<DbGame> {
    games.extend(custom_games());
    games
}

#[derive(Default)]
struct AppState {
    db: Mutex<Option<Db>>,
    popular: Mutex<Option<Popular>>,
    running: Mutex<HashMap<String, RunningGame>>,
    /// Auto-play sessions by game key; setting the flag stops one. Held while a status file is written.
    autoplay: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Started with Windows (--tray): stays in the tray with no window and nothing running.
    stay: AtomicBool,
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

/// Slim copy of Discord's detectable list, with lookups for search, import and the Discover page.
struct Db {
    games: Vec<DbGame>,
    /// Per game: folded name and aliases, each as (without spaces, with spaces).
    texts: Vec<Vec<(String, String)>>,
    by_id: HashMap<String, usize>,
    by_steam: HashMap<String, usize>,
    by_epic: HashMap<String, usize>,
    by_name: HashMap<String, usize>,
    updated_at: u64,
}

impl Db {
    fn new(games: Vec<DbGame>, updated_at: u64) -> Self {
        let mut db = Db {
            texts: Vec::with_capacity(games.len()),
            by_id: HashMap::new(),
            by_steam: HashMap::new(),
            by_epic: HashMap::new(),
            by_name: HashMap::new(),
            updated_at,
            games: Vec::new(),
        };
        for (i, g) in games.iter().enumerate() {
            let texts: Vec<(String, String)> =
                std::iter::once(&g.name).chain(&g.aliases).map(|t| fold(t)).map(|f| (f.replace(' ', ""), f)).collect();
            index_into(&mut db.by_name, texts[0].1.clone(), i, &games);
            db.texts.push(texts);
            index_into(&mut db.by_id, g.id.clone(), i, &games);
            if let Some(steam) = &g.steam {
                index_into(&mut db.by_steam, steam.clone(), i, &games);
            }
            for epic in &g.epic {
                index_into(&mut db.by_epic, epic.clone(), i, &games);
            }
        }
        db.games = games;
        db
    }

    fn get(&self, map: &HashMap<String, usize>, key: &str) -> Option<&DbGame> {
        map.get(key).map(|&i| &self.games[i])
    }
}

/// Several entries can share a Steam id or name; keep the first, unless a later one has a real exe.
fn index_into(map: &mut HashMap<String, usize>, key: String, i: usize, games: &[DbGame]) {
    match map.get(&key) {
        Some(&j) if !(games[j].rpc && !games[i].rpc) => {}
        _ => {
            map.insert(key, i);
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct DbGame {
    id: String,
    name: String,
    /// Fake exe path; for Rich Presence games a file name made from the title.
    exe: String,
    is_launcher: bool,
    /// No Windows exe in Discord's list: shown through Rich Presence instead of the process name.
    rpc: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    steam: Option<String>,
    /// Lowercased Epic ids, for matching installed Epic games.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    epic: Vec<String>,
    /// Discord image hashes for cover art.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    /// Also on Google Play.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    mobile: bool,
}

#[derive(Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    version: u32,
    etag: Option<String>,
    games: Vec<DbGame>,
}

#[derive(Deserialize)]
struct DetectableApp {
    id: Option<String>,
    name: Option<String>,
    executables: Option<Vec<Executable>>,
    aliases: Option<Vec<String>>,
    third_party_skus: Option<Vec<Sku>>,
    icon_hash: Option<String>,
    cover_image_hash: Option<String>,
}

#[derive(Deserialize)]
struct Executable {
    name: Option<String>,
    os: Option<String>,
    is_launcher: Option<bool>,
}

#[derive(Deserialize)]
struct Sku {
    distributor: Option<String>,
    id: Option<String>,
}

/// Steam charts, cached in popular.json for the Discover page and search ranking.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct Popular {
    fetched_at: u64,
    /// Steam app ids with current player counts, most played first.
    most_played: Vec<(String, u64)>,
    /// Steam app ids with (positive, negative) review counts, from SteamSpy's top 100 of the last two weeks.
    rated: Vec<(String, u64, u64)>,
    /// The `APPS` that have been looked up on Discord.
    apps: Vec<DbGame>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct MyGame {
    // The UI sends Discord list entries, whose `id` the library stores as `appId`.
    #[serde(alias = "id")]
    app_id: String,
    name: String,
    exe: String,
    is_favorite: bool,
    rpc: bool,
    // Art is copied from the list entry so apps added by id (not in the list) keep theirs.
    #[serde(skip_serializing_if = "Option::is_none")]
    steam: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cover: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence: Option<Presence>,
    /// Saved statuses to switch between.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    presets: Vec<Preset>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Preset {
    name: String,
    presence: Presence,
}

/// A custom Discord status (Rich Presence) shown while the game runs, e.g. mode, map and score.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
struct Presence {
    /// Replaces the app's own name in the status, so one app id can pose as any app.
    app_name: String,
    /// Discord activity type: 0 Playing, 2 Listening to, 3 Watching, 5 Competing in.
    activity_type: u8,
    details: String,
    state: String,
    /// Links that make the top and second line clickable.
    details_url: String,
    state_url: String,
    /// Discord's "Ask to Join" button. Only shown for Playing; friends who ask get no answer.
    ask_to_join: bool,
    /// Keeps the status "Playing": Discord only lets friends ask you to stream an app you are playing.
    ask_to_stream: bool,
    party_size: u32,
    party_max: u32,
    large_image: String,
    large_text: String,
    small_image: String,
    small_text: String,
    /// "" counts up from launch, "reset" counts up from when the status is saved, "left" counts down `timer_minutes`,
    /// "ago" is locked at `timer_minutes` (the fake exe keeps sending that time, see `frozen`), "from" started at `timer_minutes` when it
    /// was first saved and continues from where it was at each launch.
    timer: String,
    timer_minutes: u32,
    /// For "from": when the clock reads zero, in ms. Set when the status is saved.
    timer_start: u64,
    /// For "from": the clock also runs while the game is stopped.
    timer_always: bool,
    /// For "from" without `timer_always`: when the game stopped, in ms; 0 while it runs.
    timer_paused_at: u64,
    /// Up to two link buttons under the status. Discord refuses them together with Ask to Join.
    button1_label: String,
    button1_url: String,
    button2_label: String,
    button2_url: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    /// Also send Rich Presence for games Discord detects by exe, so several running games can show at once.
    show_all: bool,
}

/// One status in an auto-play session, shown for `seconds`. `keep_timer` continues the previous step's timer.
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlanStep {
    presence: Presence,
    seconds: u32,
    #[serde(default)]
    keep_timer: bool,
}

/// A library game as sent to the UI, with the key every other command takes.
#[derive(Serialize)]
struct GameView {
    #[serde(flatten)]
    game: MyGame,
    key: String,
}

/// A named set of library games that are launched together.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct Group {
    id: String,
    name: String,
    keys: Vec<String>,
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

fn is_app_id(id: &str) -> bool {
    (1..=25).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Lowercase, accents removed, punctuation as single spaces: "Pokémon: Legends Z-A" -> "pokemon legends z a".
fn fold(text: &str) -> String {
    const ACCENTS: [(&str, char); 14] = [
        ("àáâãäåāăą", 'a'),
        ("çćč", 'c'),
        ("ď", 'd'),
        ("èéêëēėęě", 'e'),
        ("ìíîïīį", 'i'),
        ("ł", 'l'),
        ("ñńň", 'n'),
        ("òóôõöøōő", 'o'),
        ("ŕř", 'r'),
        ("śšşß", 's'),
        ("ť", 't'),
        ("ùúûüūůűų", 'u'),
        ("ýÿ", 'y'),
        ("źżž", 'z'),
    ];
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = ACCENTS.iter().find(|(from, _)| from.contains(c)).map_or(c, |&(_, to)| to);
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// How well a game's texts match the query; lower is better: 0 exact, 1 starts with,
/// 2 every typed word starts a word, 3 contained once spaces are ignored ("counterstrike").
fn match_rank(texts: &[(String, String)], query: &str, compact: &str, words: &[&str]) -> Option<u8> {
    texts
        .iter()
        .filter_map(|(squashed, text)| {
            if text == query {
                Some(0)
            } else if text.starts_with(query) {
                Some(1)
            } else if words.iter().all(|w| text.split(' ').any(|t| t.starts_with(w))) {
                Some(2)
            } else if squashed.contains(compact) {
                Some(3)
            } else {
                None
            }
        })
        .min()
}

/// Fallback when nothing matches: every typed word is a word of the name with a typo or two.
fn typo_match(texts: &[(String, String)], words: &[&str]) -> bool {
    texts.iter().any(|(_, text)| words.iter().all(|w| text.split(' ').any(|t| t.starts_with(w) || close_enough(w, t))))
}

/// Edit distance of at most 1, or 2 for words of 8+ letters; short words must match exactly.
fn close_enough(typed: &str, word: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (typed.chars().collect(), word.chars().collect());
    let max = match a.len() {
        0..=3 => return false,
        4..=7 => 1,
        _ => 2,
    };
    if a.len().abs_diff(b.len()) > max {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            row.push((prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()] <= max
}

fn to_db_games(apps: Vec<DetectableApp>) -> Vec<DbGame> {
    let mut games: Vec<DbGame> = apps
        .into_iter()
        .filter_map(|app| {
            let name = app.name.filter(|n| !n.is_empty())?;
            let exes = app.executables.unwrap_or_default();
            // ">name.exe" entries are matched differently by Discord; a plain path works with a fake exe.
            let win32 = |e: &&Executable| {
                e.os.as_deref().is_some_and(|os| os.eq_ignore_ascii_case("win32"))
                    && e.name.as_deref().is_some_and(|n| !n.is_empty() && !n.starts_with('>'))
            };
            // Prefer the real game exe over its launcher.
            let best = exes.iter().filter(win32).find(|e| e.is_launcher != Some(true)).or_else(|| exes.iter().find(win32));
            let skus = app.third_party_skus.unwrap_or_default();
            let sku_ids = |distributor: &str| {
                skus.iter()
                    .filter(|s| s.distributor.as_deref() == Some(distributor))
                    .filter_map(|s| s.id.clone().filter(|id| !id.is_empty()))
                    .collect::<Vec<_>>()
            };
            Some(DbGame {
                id: app.id.unwrap_or_default(),
                exe: best.and_then(|e| e.name.clone()).unwrap_or_else(|| fallback_exe_name(&name)),
                is_launcher: best.is_some_and(|e| e.is_launcher == Some(true)),
                rpc: best.is_none(),
                aliases: app.aliases.unwrap_or_default().into_iter().filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case(&name)).collect(),
                steam: sku_ids("steam").into_iter().find(|id| id.bytes().all(|b| b.is_ascii_digit())),
                epic: sku_ids("epic").into_iter().map(|id| id.to_lowercase()).collect(),
                mobile: !sku_ids("google_play").is_empty(),
                cover: app.cover_image_hash.filter(|h| !h.is_empty()),
                icon: app.icon_hash.filter(|h| !h.is_empty()),
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

fn read_json<T: serde::de::DeserializeOwned>(app: &AppHandle, file: &str) -> Option<T> {
    serde_json::from_slice(&fs::read(data_dir(app).ok()?.join(file)).ok()?).ok()
}

fn write_json(app: &AppHandle, file: &str, value: &impl Serialize) -> Result<(), String> {
    fs::write(data_dir(app)?.join(file), serde_json::to_vec_pretty(value).map_err(err)?).map_err(err)
}

fn read_index(app: &AppHandle) -> Option<Index> {
    read_json::<Index>(app, "games-index.json").filter(|i| i.version == INDEX_VERSION)
}

/// Runs `f` on Discord's list, loading it from disk the first time.
fn with_db<T>(app: &AppHandle, f: impl FnOnce(&Db) -> T) -> T {
    let state = app.state::<AppState>();
    let mut db = state.db.lock().unwrap();
    f(db.get_or_insert_with(|| {
        let modified = data_dir(app)
            .ok()
            .and_then(|d| fs::metadata(d.join("games-index.json")).ok()?.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64);
        Db::new(with_custom(read_index(app).map(|i| i.games).unwrap_or_default()), modified)
    }))
}

fn with_popular<T>(app: &AppHandle, f: impl FnOnce(&Popular) -> T) -> T {
    let state = app.state::<AppState>();
    let mut popular = state.popular.lock().unwrap();
    f(popular.get_or_insert_with(|| read_json(app, "popular.json").unwrap_or_default()))
}

fn read_my_games(app: &AppHandle) -> Result<Vec<MyGame>, String> {
    let mut path = data_dir(app)?.join("myGames.json");
    if !path.exists() {
        path = app.path().data_dir().map_err(err)?.join(ELECTRON_APP_DIR).join("myGames.json");
    }
    Ok(fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
}

fn write_my_games(app: &AppHandle, games: &[MyGame]) -> Result<(), String> {
    write_json(app, "myGames.json", &games)
}

fn read_groups(app: &AppHandle) -> Vec<Group> {
    read_json(app, "groups.json").unwrap_or_default()
}

fn find_game(app: &AppHandle, key: &str) -> Result<MyGame, String> {
    read_my_games(app)?.into_iter().find(|g| game_key(g) == key).ok_or_else(|| "Game not found in library.".into())
}

fn game_key(game: &MyGame) -> String {
    format!("{}::{}", game.app_id, game.exe)
}

/// Fills in art from Discord's list for games added before art was stored.
fn view(mut game: MyGame, db: Option<&Db>) -> GameView {
    if let Some(entry) = db.and_then(|db| db.get(&db.by_id, &game.app_id)) {
        game.steam = game.steam.or_else(|| entry.steam.clone());
        game.cover = game.cover.or_else(|| entry.cover.clone());
        game.icon = game.icon.or_else(|| entry.icon.clone());
    }
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

fn fake_exe_path(app: &AppHandle, game: &MyGame) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("games").join(fake_exe_rel_path(game)?))
}

/// Writes the fake exe for a game (if missing or outdated) and returns its path.
fn ensure_fake_exe(app: &AppHandle, game: &MyGame) -> Result<PathBuf, String> {
    let dest = fake_exe_path(app, game)?;
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

/// Arguments for the fake exe: display name, then the Rich Presence app id for games without an exe.
fn fake_exe_args(game: &MyGame) -> Vec<String> {
    let mut args = vec![if game.name.is_empty() { "Game".into() } else { game.name.clone() }];
    if game.rpc {
        args.push(format!("--rpc={}", game.app_id));
    }
    args
}

/// The activity fields for a custom status. Discord rejects the whole status if a field is out of its limits
/// (texts 2–128 characters, images 1–300), so those fields are left out instead.
/// `now` is when the status is written: on save, and again at each launch.
fn presence_activity(p: &Presence, now: u64) -> Value {
    let fit = |s: &str, min: usize, max: usize| {
        let s = s.trim();
        (min..=max).contains(&s.chars().count()).then(|| json!(s))
    };
    let link = |s: &str, max: usize| fit(s, 1, max).filter(|_| s.trim().starts_with("https://") || s.trim().starts_with("http://"));
    let button = |label: &str, url: &str| Some(json!({ "label": fit(label, 1, 32)?, "url": link(url, 512)? }));
    let buttons: Vec<Value> =
        [button(&p.button1_label, &p.button1_url), button(&p.button2_label, &p.button2_url)].into_iter().flatten().collect();
    let pick = |fields: Vec<(&str, Option<Value>)>| -> serde_json::Map<String, Value> {
        fields.into_iter().filter_map(|(k, v)| Some((k.to_string(), v?))).collect()
    };
    // Discord only offers Ask to Join on a party with a join secret; the two ids just have to differ.
    let party = pick(vec![
        ("id", p.ask_to_join.then(|| json!("endo-party"))),
        ("size", (p.party_max > 0 && p.party_size <= p.party_max).then(|| json!([p.party_size, p.party_max]))),
    ]);
    let mut activity = pick(vec![
        ("name", fit(&p.app_name, 1, 128)),
        ("type", (!p.ask_to_join && !p.ask_to_stream && [2, 3, 5].contains(&p.activity_type)).then(|| json!(p.activity_type))),
        ("details", fit(&p.details, 2, 128)),
        ("details_url", link(&p.details_url, 256)),
        ("state", fit(&p.state, 2, 128)),
        ("state_url", link(&p.state_url, 256)),
        ("party", (!party.is_empty()).then_some(Value::Object(party))),
        ("secrets", p.ask_to_join.then(|| json!({ "join": "endo-join" }))),
        ("buttons", (!p.ask_to_join && !buttons.is_empty()).then(|| json!(buttons))),
        // Without timestamps the fake exe adds the launch time.
        (
            "timestamps",
            match p.timer.as_str() {
                "reset" => Some(json!({ "start": now })),
                "left" if p.timer_minutes > 0 => Some(json!({ "end": now + u64::from(p.timer_minutes) * 60_000 })),
                "from" if p.timer_start > 0 => Some(json!({ "start": p.timer_start })),
                _ => None,
            },
        ),
    ]);
    let assets = pick(vec![
        ("large_image", fit(&p.large_image, 1, 300)),
        ("large_text", fit(&p.large_text, 2, 128)),
        ("small_image", fit(&p.small_image, 1, 300)),
        ("small_text", fit(&p.small_text, 2, 128)),
    ]);
    if !assets.is_empty() {
        activity.insert("assets".into(), Value::Object(assets));
    }
    Value::Object(activity)
}

/// Writes (or removes) <fake exe>.presence. The fake exe re-reads it every 2 s, so running games update too.
/// `now` anchors "reset" and "left" timers.
fn write_presence_file(app: &AppHandle, game: &MyGame, now: u64) -> Result<(), String> {
    let mut path = fake_exe_path(app, game)?.into_os_string();
    path.push(".presence");
    let path = PathBuf::from(path);
    // Rich Presence entries always send their library name, so a game shown through another app (like the
    // custom mobile games) isn't shown under that app's name.
    let named = game.rpc && is_app_id(&game.app_id) && (1..=128).contains(&game.name.chars().count());
    let shown = game.presence.clone().or_else(|| named.then(Presence::default));
    match &shown {
        Some(p) => {
            fs::create_dir_all(path.parent().unwrap()).map_err(err)?;
            // A locked clock has no timestamps here: the fake exe makes them from the "frozen" line.
            let frozen = if p.timer == "ago" && p.timer_minutes > 0 {
                format!("frozen {}\n", u64::from(p.timer_minutes) * 60_000)
            } else {
                String::new()
            };
            let mut activity = presence_activity(p, now);
            if named {
                name_and_art(&mut activity, game);
            }
            fs::write(&path, format!("{}\n{activity}\n{frozen}", game.app_id)).map_err(err)
        }
        None => match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
            _ => Ok(()),
        },
    }
}

/// A Rich Presence entry's name and, when the status has no big image, its cover link: a game shown through
/// another app (like the custom mobile games) would otherwise show that app's name and its missing icon.
fn name_and_art(activity: &mut Value, game: &MyGame) {
    if activity.get("name").is_none() {
        activity["name"] = json!(game.name);
    }
    let cover = game.cover.as_deref().filter(|c| c.starts_with("https://") && c.len() <= 300);
    if let (Some(cover), Some(map)) = (cover, activity.as_object_mut()) {
        let assets = map.entry("assets").or_insert_with(|| json!({}));
        if assets.get("large_image").is_none() {
            assets["large_image"] = json!(cover);
            if assets.get("large_text").is_none() && game.name.chars().count() >= 2 {
                assets["large_text"] = json!(game.name);
            }
        }
    }
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
    if count == 0 && app.webview_windows().is_empty() && !app.state::<AppState>().stay.load(Ordering::Relaxed) {
        app.exit(0);
    }
}

/// Pauses (game stopped) or resumes (game launched) a "from" clock that only runs while the game does.
fn set_clock_running(app: &AppHandle, key: &str, running: bool) {
    let Ok(mut games) = read_my_games(app) else { return };
    let now = now_ms();
    let Some(p) = games.iter_mut().find(|g| game_key(g) == key).and_then(|g| g.presence.as_mut()) else { return };
    if p.timer != "from" || p.timer_always || p.timer_start == 0 || running == (p.timer_paused_at == 0) {
        return;
    }
    if running {
        p.timer_start += now.saturating_sub(p.timer_paused_at);
        p.timer_paused_at = 0;
    } else {
        p.timer_paused_at = now;
    }
    let _ = write_my_games(app, &games);
}

/// Reports a fake game that exits on its own (closed, auto-stop, killed); stop_game reports its own.
fn watch_exit(app: AppHandle, key: String, pid: u32) {
    let mut ticks = 0u32;
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        ticks += 1;
        // Every six hours of a running game, so an app left in the tray still finds updates.
        if ticks.is_multiple_of(43_200) {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = check_update(handle).await;
            });
        }
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
        set_clock_running(&app, &key, false);
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
    set_clock_running(app, key, false);
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

/// Open, one "Launch group" entry per group, Stop all, Quit.
fn tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(app, "open", "Open", true, None::<&str>)?)?;
    for group in read_groups(app) {
        let label = format!("Launch group: {}", group.name);
        menu.append(&MenuItem::with_id(app, format!("group:{}", group.id), label, true, None::<&str>)?)?;
    }
    let plans: HashMap<String, Vec<PlanStep>> = read_json(app, "autoplay.json").unwrap_or_default();
    for game in read_my_games(app).unwrap_or_default() {
        let key = game_key(&game);
        if plans.contains_key(&key) {
            let label = format!("Auto-play: {}", game.name);
            menu.append(&MenuItem::with_id(app, format!("auto:{key}"), label, true, None::<&str>)?)?;
        }
    }
    menu.append(&MenuItem::with_id(app, "stop-all", "Stop all games", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

/// Rebuilds the tray menu after the groups change.
fn refresh_tray(app: &AppHandle) {
    if let (Some(tray), Ok(menu)) = (app.tray_by_id("main"), tray_menu(app)) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip(app.config().product_name.clone().unwrap_or_default())
        .menu(&tray_menu(app.handle())?)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "stop-all" => stop_all(app),
            "quit" => {
                stop_all(app);
                app.exit(0);
            }
            id => {
                if let Some(key) = id.strip_prefix("auto:") {
                    let plans: HashMap<String, Vec<PlanStep>> = read_json(app, "autoplay.json").unwrap_or_default();
                    launch(app, key, None);
                    let _ = start_autoplay(app, key, plans.get(key).cloned().unwrap_or_default());
                }
                let group = id.strip_prefix("group:").and_then(|id| read_groups(app).into_iter().find(|g| g.id == id));
                for key in group.iter().flat_map(|g| &g.keys) {
                    launch(app, key, None);
                }
                // An open window reloads what is running.
                let _ = app.emit("games-changed", ());
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Runs reg.exe without flashing a console window; true when it succeeded.
fn reg(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn starts_with_windows() -> bool {
    reg(&["query", RUN_KEY, "/v", RUN_NAME])
}

/// Adds or removes this exe (with --tray) in the current user's Run key.
fn set_start_with_windows(on: bool) -> Result<(), String> {
    let done = if on {
        let exe = std::env::current_exe().map_err(err)?;
        reg(&["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &format!("\"{}\" --tray", exe.display()), "/f"])
    } else {
        !starts_with_windows() || reg(&["delete", RUN_KEY, "/v", RUN_NAME, "/f"])
    };
    done.then_some(()).ok_or_else(|| "Couldn’t change the Windows startup setting.".into())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().user_agent(USER_AGENT).timeout(Duration::from_secs(60)).build().unwrap_or_default()
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value, reqwest::Error> {
    client.get(url).send().await?.error_for_status()?.json().await
}

/// Refreshes the Steam charts; a source that fails keeps its previous data.
async fn fetch_popular(mut popular: Popular) -> Popular {
    let client = http();
    let mut fetched = false;
    if let Ok(v) = get_json(&client, STEAM_PLAYERS_URL).await {
        let ranks = v["response"]["ranks"].as_array().cloned().unwrap_or_default();
        let list: Vec<(String, u64)> = ranks
            .iter()
            .filter_map(|r| Some((r["appid"].as_u64()?.to_string(), r["concurrent_in_game"].as_u64().unwrap_or(0))))
            .collect();
        if !list.is_empty() {
            popular.most_played = list;
            fetched = true;
        }
    }
    if let Ok(v) = get_json(&client, STEAMSPY_TOP_URL).await {
        let list: Vec<(String, u64, u64)> = v
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
            .filter_map(|g| Some((g["appid"].as_u64()?.to_string(), g["positive"].as_u64()?, g["negative"].as_u64()?)))
            .collect();
        if !list.is_empty() {
            popular.rated = list;
            fetched = true;
        }
    }
    if fetched {
        popular.fetched_at = now_ms();
    }
    // Looked up once: an app's name and icon rarely change.
    for id in APPS {
        if !popular.apps.iter().any(|a| a.id == *id) {
            if let Ok(v) = get_json(&client, &format!("{DISCORD_APP_URL}/{id}/rpc")).await {
                popular.apps.push(app_entry(id, &v));
            }
        }
    }
    popular
}

/// A Rich Presence entry from Discord's public info about an app.
fn app_entry(id: &str, v: &Value) -> DbGame {
    let name = v["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("App").to_string();
    let hash = |key: &str| v[key].as_str().filter(|h| !h.is_empty()).map(String::from);
    DbGame { exe: fallback_exe_name(&name), rpc: true, icon: hash("icon"), cover: hash("cover_image"), name, id: id.into(), ..Default::default() }
}

fn short_number(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}K", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[tauri::command]
async fn sync_game_list(app: AppHandle, state: State<'_, AppState>) -> Result<Value, String> {
    let mut req = http().get(DISCORD_DETECTABLE_URL);
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
    // Keep only what the app uses (~2.5x smaller than Discord's raw list).
    let index = Index { version: INDEX_VERSION, etag, games: to_db_games(res.json().await.map_err(err)?) };
    fs::write(data_dir(&app)?.join("games-index.json"), serde_json::to_vec(&index).map_err(err)?).map_err(err)?;
    *state.db.lock().unwrap() = Some(Db::new(with_custom(index.games), now_ms()));
    Ok(json!({ "updated": true }))
}

/// `kind`: "exe" (detected by process name), "rpc" (Rich Presence only), "mobile", or anything else for all.
#[tauri::command]
fn search_games(
    app: AppHandle,
    filter: Option<String>,
    kind: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Value {
    let query = fold(filter.as_deref().unwrap_or(""));
    let compact = query.replace(' ', "");
    let words: Vec<&str> = query.split(' ').filter(|w| !w.is_empty()).collect();
    let (offset, limit) = (offset.unwrap_or(0), limit.unwrap_or(200).clamp(1, 500));
    let steam_rank: HashMap<String, usize> =
        with_popular(&app, |p| p.most_played.iter().enumerate().map(|(i, (id, _))| (id.clone(), i)).collect());
    with_db(&app, |db| {
        let wanted = |g: &DbGame| match kind.as_deref() {
            Some("exe") => !g.rpc,
            Some("rpc") => g.rpc,
            Some("mobile") => g.mobile,
            _ => true,
        };
        let find = |typos: bool| -> Vec<(u8, usize)> {
            db.games
                .iter()
                .enumerate()
                .filter(|(_, g)| wanted(g))
                .filter_map(|(i, g)| {
                    if query.is_empty() || g.id == query {
                        Some((0, i))
                    } else if typos {
                        typo_match(&db.texts[i], &words).then_some((4, i))
                    } else {
                        match_rank(&db.texts[i], &query, &compact, &words).map(|r| (r, i))
                    }
                })
                .collect()
        };
        let mut hits = find(false);
        if hits.is_empty() && compact.chars().count() >= 4 {
            hits = find(true);
        }
        // Best matches first, then what's popular on Steam right now, then games detected by exe.
        hits.sort_by_key(|&(rank, i)| {
            let g = &db.games[i];
            (rank, g.steam.as_ref().and_then(|s| steam_rank.get(s)).copied().unwrap_or(usize::MAX), g.rpc, i)
        });
        let items: Vec<&DbGame> = hits.iter().skip(offset).take(limit).map(|&(_, i)| &db.games[i]).collect();
        json!({ "items": items, "hasMore": hits.len() > offset + limit, "total": hits.len() })
    })
}

/// The Discover page: Steam's most played and best rated games, plus hand-picked ones from elsewhere.
#[tauri::command]
async fn discover(app: AppHandle) -> Result<Value, String> {
    let cached = with_popular(&app, Popular::clone);
    if cached.fetched_at + POPULAR_MAX_AGE_MS < now_ms() || cached.apps.len() < APPS.len() {
        let fresh = fetch_popular(cached).await;
        let _ = write_json(&app, "popular.json", &fresh);
        *app.state::<AppState>().popular.lock().unwrap() = Some(fresh);
    }
    let popular = with_popular(&app, Popular::clone);
    Ok(with_db(&app, |db| {
        let mut seen = HashSet::new();
        let mut card = |i: usize, note: Option<String>| seen.insert(i).then(|| json!({ "game": &db.games[i], "note": note }));
        let most_played: Vec<Value> = popular
            .most_played
            .iter()
            .filter_map(|(id, players)| card(*db.by_steam.get(id)?, Some(format!("{} playing", short_number(*players)))))
            .take(30)
            .collect();
        // Only games with plenty of reviews, so a handful of fans can't top the list.
        let mut rated: Vec<(usize, f64)> = popular
            .rated
            .iter()
            .filter(|(_, pos, neg)| pos + neg >= 20_000)
            .filter_map(|(id, pos, neg)| Some((*db.by_steam.get(id)?, *pos as f64 / (pos + neg) as f64)))
            .collect();
        rated.sort_by(|a, b| b.1.total_cmp(&a.1));
        let top_rated: Vec<Value> =
            rated.iter().filter_map(|&(i, score)| card(i, Some(format!("{:.0}% positive", score * 100.0)))).take(30).collect();
        let mut named = |names: &[&str]| -> Vec<Value> {
            names.iter().filter_map(|n| db.by_name.get(&fold(n))).filter_map(|&i| card(i, None)).collect()
        };
        let beyond_steam = named(BEYOND_STEAM);
        let mobile = named(MOBILE);
        let apps: Vec<Value> = APPS
            .iter()
            .filter_map(|id| popular.apps.iter().find(|a| a.id == *id))
            .map(|game| json!({ "game": game, "note": null }))
            .collect();
        json!({
            "apps": apps,
            "mostPlayed": most_played,
            "topRated": top_rated,
            "beyondSteam": beyond_steam,
            "mobile": mobile,
            "count": db.games.len(),
            "updatedAt": db.updated_at,
        })
    }))
}

/// Any Discord app by id, including ones missing from the detectable list; shown through Rich Presence.
#[tauri::command]
async fn lookup_app(app: AppHandle, id: String) -> Result<DbGame, String> {
    if !is_app_id(&id) {
        return Err("A Discord application id is a long number.".into());
    }
    if let Some(game) = with_db(&app, |db| db.get(&db.by_id, &id).cloned()) {
        return Ok(game);
    }
    let res = http().get(format!("{DISCORD_APP_URL}/{id}/rpc")).send().await.map_err(err)?;
    if !res.status().is_success() {
        return Err(format!("Discord has no app with id {id}."));
    }
    Ok(app_entry(&id, &res.json().await.map_err(err)?))
}

/// The images an app has uploaded to Discord; each name can be used as a status image.
#[tauri::command]
async fn app_assets(id: String) -> Result<Vec<Value>, String> {
    if !is_app_id(&id) {
        return Ok(Vec::new());
    }
    let v = get_json(&http(), &format!("https://discord.com/api/v9/oauth2/applications/{id}/assets")).await.map_err(err)?;
    // `id` is the image's own id, for its preview on Discord's image server.
    let images = v.as_array().into_iter().flatten().filter(|a| a["name"].is_string() && a["id"].is_string());
    Ok(images.map(|a| json!({ "name": a["name"], "id": a["id"] })).collect())
}

/// Games installed on this PC (Steam, Epic, Windows' installed apps) that are in Discord's list.
#[tauri::command]
async fn scan_installed(app: AppHandle) -> Result<Vec<Value>, String> {
    // Registry and disk walks: keep them off the UI thread.
    tauri::async_runtime::spawn_blocking(move || {
        let found = installed::scan();
        with_db(&app, |db| {
            let mut seen = HashSet::new();
            let mut out: Vec<Value> = Vec::new();
            for f in &found {
                let by_name = || {
                    let g = db.get(&db.by_name, &fold(&f.name))?;
                    // A program with a game's name isn't necessarily that game: its exe must be in the install folder.
                    let confirmed = g.rpc || f.install_dir.as_deref().is_some_and(|dir| installed::has_exe(dir, &g.exe));
                    confirmed.then_some(g)
                };
                let hit = f
                    .steam_app_id
                    .as_deref()
                    .and_then(|id| db.get(&db.by_steam, id))
                    .or_else(|| f.epic_ids.iter().find_map(|id| db.get(&db.by_epic, id)))
                    .or_else(by_name);
                if let Some(game) = hit.filter(|g| seen.insert(g.id.clone())) {
                    out.push(json!({ "game": game, "source": f.source }));
                }
            }
            out.sort_by_cached_key(|v| v["game"]["name"].as_str().unwrap_or_default().to_lowercase());
            out
        })
    })
    .await
    .map_err(err)
}

#[tauri::command]
fn get_library(app: AppHandle) -> Result<Vec<GameView>, String> {
    let games = read_my_games(&app)?;
    Ok(with_db(&app, |db| games.into_iter().map(|g| view(g, Some(db))).collect()))
}

#[tauri::command]
fn add_games(app: AppHandle, games: Vec<MyGame>) -> Result<Vec<GameView>, String> {
    let mut library = read_my_games(&app)?;
    let mut added = Vec::new();
    // Everything is checked before anything is saved.
    for mut game in games {
        if game.name.is_empty() {
            game.name = "Game".into();
        }
        game.is_favorite = false;
        fake_exe_rel_path(&game)?;
        if game.rpc && !is_app_id(&game.app_id) {
            return Err(format!("Unsupported app id: {}", game.app_id));
        }
        if !library.iter().any(|g| game_key(g) == game_key(&game)) {
            library.push(game.clone());
        }
        added.push(game);
    }
    write_my_games(&app, &library)?;
    Ok(with_db(&app, |db| added.into_iter().map(|g| view(g, Some(db))).collect()))
}

#[tauri::command]
fn toggle_favorite(app: AppHandle, key: String) -> Result<Option<GameView>, String> {
    let mut games = read_my_games(&app)?;
    let updated = games.iter_mut().find(|g| game_key(g) == key).map(|g| {
        g.is_favorite = !g.is_favorite;
        g.clone()
    });
    write_my_games(&app, &games)?;
    Ok(updated.map(|g| with_db(&app, |db| view(g, Some(db)))))
}

/// Saves, or with `None` clears, a game's custom Discord status. Stops the game's auto-play.
#[tauri::command]
fn set_presence(app: AppHandle, state: State<'_, AppState>, key: String, presence: Option<Presence>) -> Result<GameView, String> {
    let mut games = read_my_games(&app)?;
    let game = games.iter_mut().find(|g| game_key(g) == key).ok_or("Game not found in library.")?;
    if !is_app_id(&game.app_id) {
        return Err("This game has no Discord app ID, so it can’t have a custom status.".into());
    }
    let running = state.running.lock().unwrap().contains_key(&key);
    game.presence = presence.filter(|p| *p != Presence::default()).map(|mut p| {
        let now = now_ms();
        // "from" keeps its clock through later saves (stage buttons, score) unless its settings change.
        (p.timer_start, p.timer_paused_at) = match &game.presence {
            _ if p.timer != "from" || p.timer_minutes == 0 => (0, 0),
            Some(old)
                if old.timer == "from"
                    && old.timer_minutes == p.timer_minutes
                    && old.timer_always == p.timer_always
                    && old.timer_start > 0 =>
            {
                (old.timer_start, old.timer_paused_at)
            }
            // A stopped game's clock waits at the set time until it is launched.
            _ => (now.saturating_sub(u64::from(p.timer_minutes) * 60_000), if running || p.timer_always { 0 } else { now }),
        };
        p
    });
    let game = game.clone();
    {
        let mut autoplay = state.autoplay.lock().unwrap();
        if let Some(stop) = autoplay.remove(&key) {
            stop.store(true, Ordering::SeqCst);
        }
        write_presence_file(&app, &game, now_ms())?;
    }
    write_my_games(&app, &games)?;
    Ok(with_db(&app, |db| view(game, Some(db))))
}

/// Starts an auto-play session for a running game: each step's status in turn, written to the status file only
/// (the library keeps the saved status). No steps stops the session and puts the saved status back.
/// The session ends when the game stops; it then emits "autoplay-ended".
#[tauri::command]
fn play_presence(app: AppHandle, key: String, steps: Vec<PlanStep>) -> Result<(), String> {
    start_autoplay(&app, &key, steps.clone())?;
    if !steps.is_empty() {
        // Kept so the tray menu can start this session again without the window.
        // shortcut: the tray replays the last session made in the window; build plans in Rust for a fresh one each time.
        let mut plans: HashMap<String, Vec<PlanStep>> = read_json(&app, "autoplay.json").unwrap_or_default();
        plans.insert(key, steps);
        write_json(&app, "autoplay.json", &plans)?;
        refresh_tray(&app);
    }
    Ok(())
}

fn start_autoplay(app: &AppHandle, key: &str, steps: Vec<PlanStep>) -> Result<(), String> {
    let (app, key) = (app.clone(), key.to_string());
    let state = app.state::<AppState>();
    let game = find_game(&app, &key)?;
    let mut autoplay = state.autoplay.lock().unwrap();
    if let Some(stop) = autoplay.remove(&key) {
        stop.store(true, Ordering::SeqCst);
    }
    if steps.is_empty() {
        return write_presence_file(&app, &game, now_ms());
    }
    if !is_app_id(&game.app_id) {
        return Err("This game has no Discord app ID, so it can’t have a custom status.".into());
    }
    if !state.running.lock().unwrap().contains_key(&key) {
        return Err("Launch the game first.".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    autoplay.insert(key.clone(), stop.clone());
    let thread_app = app.clone();
    std::thread::spawn(move || {
        let app = thread_app;
        let state = app.state::<AppState>();
        let running =|| !stop.load(Ordering::SeqCst) && state.running.lock().unwrap().contains_key(&key);
        let mut timer_from = now_ms();
        'steps: for step in steps {
            {
                // Checked under the lock so a status saved meanwhile is never overwritten.
                let _autoplay = state.autoplay.lock().unwrap();
                if !running() {
                    break;
                }
                if !step.keep_timer {
                    timer_from = now_ms();
                }
                let mut presence = step.presence.clone();
                if presence.timer == "from" {
                    // The saved status owns this clock; a launch may have moved it.
                    let saved = find_game(&app, &key).ok().and_then(|g| g.presence).filter(|p| p.timer == "from");
                    presence.timer_start = saved.map_or(0, |p| p.timer_start);
                }
                let shown = MyGame { presence: Some(presence), ..game.clone() };
                if write_presence_file(&app, &shown, timer_from).is_err() {
                    break;
                }
            }
            let _ = app.emit("presence-step", json!({ "key": key, "presence": step.presence }));
            for _ in 0..step.seconds.max(1) {
                std::thread::sleep(Duration::from_secs(1));
                if !running() {
                    break 'steps;
                }
            }
        }
        let mut autoplay = state.autoplay.lock().unwrap();
        // Only if this session wasn't stopped or replaced from the UI, which updates itself then.
        if autoplay.get(&key).is_some_and(|s| Arc::ptr_eq(s, &stop)) {
            autoplay.remove(&key);
            let _ = app.emit("autoplay-ended", &key);
        }
    });
    Ok(())
}

/// Replaces a game's saved statuses.
#[tauri::command]
fn set_presets(app: AppHandle, key: String, presets: Vec<Preset>) -> Result<GameView, String> {
    let mut games = read_my_games(&app)?;
    let game = games.iter_mut().find(|g| game_key(g) == key).ok_or("Game not found in library.")?;
    game.presets = presets.into_iter().filter(|p| !p.name.trim().is_empty()).take(30).collect();
    let game = game.clone();
    write_my_games(&app, &games)?;
    Ok(with_db(&app, |db| view(game, Some(db))))
}

/// Puts a "start from a set time" clock back to its set time.
#[tauri::command]
fn reset_clock(app: AppHandle, state: State<'_, AppState>, key: String) -> Result<GameView, String> {
    let mut games = read_my_games(&app)?;
    let game = games.iter_mut().find(|g| game_key(g) == key).ok_or("Game not found in library.")?;
    let running = state.running.lock().unwrap().contains_key(&key);
    let now = now_ms();
    if let Some(p) = game.presence.as_mut().filter(|p| p.timer == "from") {
        p.timer_start = now.saturating_sub(u64::from(p.timer_minutes) * 60_000);
        p.timer_paused_at = if running || p.timer_always { 0 } else { now };
    }
    let game = game.clone();
    {
        // Not while an auto-play step owns the status file.
        let autoplay = state.autoplay.lock().unwrap();
        if !autoplay.contains_key(&key) {
            write_presence_file(&app, &game, now)?;
        }
    }
    write_my_games(&app, &games)?;
    Ok(with_db(&app, |db| view(game, Some(db))))
}

/// The update found by a check made while the window was closed.
#[tauri::command]
fn pending_update(state: State<'_, AppState>) -> Option<Value> {
    let update = state.update.lock().unwrap();
    update.as_ref().map(|u| json!({ "version": u.version, "notes": u.body.clone().unwrap_or_default() }))
}

#[tauri::command]
fn get_autoplay(state: State<'_, AppState>) -> Vec<String> {
    state.autoplay.lock().unwrap().keys().cloned().collect()
}

#[tauri::command]
fn remove_games(app: AppHandle, keys: Vec<String>) -> Result<(), String> {
    for key in &keys {
        stop(&app, key);
    }
    games_changed(&app);
    let mut games = read_my_games(&app)?;
    games.retain(|g| !keys.contains(&game_key(g)));
    write_my_games(&app, &games)?;
    let mut groups = read_groups(&app);
    for group in &mut groups {
        group.keys.retain(|k| !keys.contains(k));
    }
    write_json(&app, "groups.json", &groups)
}

#[tauri::command]
fn get_groups(app: AppHandle) -> Vec<Group> {
    read_groups(&app)
}

/// Creates (empty id) or replaces a group.
#[tauri::command]
fn save_group(app: AppHandle, group: Group) -> Result<Group, String> {
    let mut group = group;
    group.name = group.name.trim().chars().take(60).collect();
    if group.name.is_empty() {
        group.name = "Group".into();
    }
    if group.id.is_empty() {
        group.id = now_ms().to_string();
    }
    let mut seen = HashSet::new();
    group.keys.retain(|k| seen.insert(k.clone()));
    let mut groups = read_groups(&app);
    match groups.iter_mut().find(|g| g.id == group.id) {
        Some(existing) => *existing = group.clone(),
        None => groups.push(group.clone()),
    }
    write_json(&app, "groups.json", &groups)?;
    refresh_tray(&app);
    Ok(group)
}

#[tauri::command]
fn delete_group(app: AppHandle, id: String) -> Result<(), String> {
    let mut groups = read_groups(&app);
    groups.retain(|g| g.id != id);
    write_json(&app, "groups.json", &groups)?;
    refresh_tray(&app);
    Ok(())
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
        let args: Vec<String> = fake_exe_args(&game).iter().map(|a| format!("\"{}\"", a.replace('"', "\\\""))).collect();
        link.set_arguments(Some(args.join(" ")));
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
fn launch_game(app: AppHandle, key: String, minutes: Option<f64>) -> Value {
    launch(&app, &key, minutes)
}

fn launch(app: &AppHandle, key: &str, minutes: Option<f64>) -> Value {
    let state = app.state::<AppState>();
    if let Some(game) = state.running.lock().unwrap().get_mut(key) {
        if let Ok(None) = game.child.try_wait() {
            return json!({ "ok": true, "alreadyRunning": true, "run": game.info });
        }
    }
    let minutes = minutes.filter(|m| m.is_finite() && *m > 0.0);
    let started = (|| -> Result<Child, String> {
        set_clock_running(app, key, true);
        let game = find_game(app, key)?;
        let exe = ensure_fake_exe(app, &game)?;
        // Also removes a status file left over from a removed and re-added game.
        write_presence_file(app, &game, now_ms())?;
        // The fake exe shows the name as its window title and closes itself after `minutes`.
        let mut command = Command::new(&exe);
        command.args(fake_exe_args(&game)).current_dir(exe.parent().unwrap());
        if !game.rpc && is_app_id(&game.app_id) && read_json::<Settings>(app, "settings.json").unwrap_or_default().show_all {
            command.arg(format!("--rpc={}", game.app_id));
        }
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
    state.running.lock().unwrap().insert(key.to_string(), RunningGame { child, info });
    watch_exit(app.clone(), key.to_string(), pid);
    games_changed(app);
    json!({ "ok": true, "run": info })
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Value {
    let settings: Settings = read_json(&app, "settings.json").unwrap_or_default();
    json!({ "showAll": settings.show_all, "startWithWindows": starts_with_windows(), "version": app.package_info().version.to_string() })
}

#[tauri::command]
fn set_settings(app: AppHandle, show_all: bool, start_with_windows: bool) -> Result<(), String> {
    write_json(&app, "settings.json", &Settings { show_all })?;
    if start_with_windows != starts_with_windows() {
        set_start_with_windows(start_with_windows)?;
    }
    Ok(())
}

/// Saves the library and groups as one file in Downloads and returns its path.
#[tauri::command]
fn export_library(app: AppHandle) -> Result<PathBuf, String> {
    let path = app.path().download_dir().map_err(err)?.join("endo-spoofer-library.json");
    let data = json!({ "games": read_my_games(&app)?, "groups": read_groups(&app) });
    fs::write(&path, serde_json::to_vec_pretty(&data).map_err(err)?).map_err(err)?;
    Ok(path)
}

/// Adds the games and groups of an exported file that aren't here yet; returns how many of each.
#[tauri::command]
fn import_library(app: AppHandle, games: Vec<MyGame>, groups: Vec<Group>) -> Result<(usize, usize), String> {
    let mut library = read_my_games(&app)?;
    let before = library.len();
    // Everything is checked before anything is saved.
    for game in games {
        fake_exe_rel_path(&game)?;
        if (game.rpc || game.presence.is_some()) && !is_app_id(&game.app_id) {
            return Err(format!("Unsupported app id: {}", game.app_id));
        }
        if !library.iter().any(|g| game_key(g) == game_key(&game)) {
            library.push(game);
        }
    }
    let mut all_groups = read_groups(&app);
    let groups_before = all_groups.len();
    for group in groups {
        if !group.id.is_empty() && !all_groups.iter().any(|g| g.id == group.id) {
            all_groups.push(group);
        }
    }
    write_my_games(&app, &library)?;
    write_json(&app, "groups.json", &all_groups)?;
    refresh_tray(&app);
    Ok((library.len() - before, all_groups.len() - groups_before))
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

/// True when an update was found; the window is told through "update-available".
async fn check_for_update(app: &AppHandle) -> Result<bool, String> {
    let Some(update) = app.updater().map_err(err)?.check().await.map_err(err)? else {
        return Ok(false);
    };
    let _ = app.emit(
        "update-available",
        json!({ "version": update.version, "notes": update.body.clone().unwrap_or_default() }),
    );
    *app.state::<AppState>().update.lock().unwrap() = Some(update);
    Ok(true)
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<bool, String> {
    // The updater is only set up in release builds with an update address.
    if cfg!(debug_assertions) || !app.config().plugins.0.contains_key("updater") {
        return Err("Updates are turned off in this build.".into());
    }
    check_for_update(&app).await
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
            if std::env::args().any(|a| a == "--tray") {
                app.state::<AppState>().stay.store(true, Ordering::Relaxed);
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.destroy();
                }
            }
            // Updates turn on once tauri.conf.json has plugins.updater (endpoints + pubkey); off in dev builds.
            if !cfg!(debug_assertions) && app.config().plugins.0.contains_key("updater") {
                app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;
                let handle = app.handle().clone();
                // A failed check (offline, no release yet, private repo) is not worth an error in the log.
                // Later checks come from the window (every six hours) and, in the tray, from watch_exit.
                // Not a loop here: a task that never ends keeps the app from saving its window position on exit.
                tauri::async_runtime::spawn(async move {
                    let _ = check_for_update(&handle).await;
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::Resized(_) => set_low_memory(window, window.is_minimized().unwrap_or(false)),
            // In the tray only running games matter; the lists are read from disk again when the window reopens.
            WindowEvent::Destroyed => {
                let state = window.state::<AppState>();
                *state.db.lock().unwrap() = None;
                *state.popular.lock().unwrap() = None;
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            sync_game_list,
            search_games,
            discover,
            lookup_app,
            scan_installed,
            get_library,
            add_games,
            toggle_favorite,
            set_presence,
            app_assets,
            play_presence,
            get_autoplay,
            remove_games,
            get_groups,
            save_group,
            delete_group,
            create_shortcut,
            launch_game,
            stop_game,
            get_running,
            discord_running,
            get_settings,
            set_settings,
            export_library,
            import_library,
            check_update,
            pending_update,
            set_presets,
            reset_clock,
            update_install,
            update_quit_and_install
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Closing the last window keeps the app in the tray while fake games run.
            if let RunEvent::ExitRequested { code: None, api, .. } = event {
                let state = app.state::<AppState>();
                if state.stay.load(Ordering::Relaxed) || !state.running.lock().unwrap().is_empty() {
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
        DetectableApp {
            id: Some(id.into()),
            name: name.map(String::from),
            executables: Some(executables),
            aliases: None,
            third_party_skus: None,
            icon_hash: None,
            cover_image_hash: None,
        }
    }

    fn game(app_id: &str, exe: &str) -> MyGame {
        MyGame { app_id: app_id.into(), name: "Game".into(), exe: exe.into(), ..Default::default() }
    }

    fn db_game(name: &str, aliases: &[&str]) -> DbGame {
        DbGame { name: name.into(), aliases: aliases.iter().map(|a| a.to_string()).collect(), ..Default::default() }
    }

    fn search(db: &Db, query: &str) -> Vec<String> {
        let q = fold(query);
        let compact = q.replace(' ', "");
        let words: Vec<&str> = q.split(' ').collect();
        let mut hits: Vec<(u8, usize)> =
            (0..db.games.len()).filter_map(|i| match_rank(&db.texts[i], &q, &compact, &words).map(|r| (r, i))).collect();
        if hits.is_empty() {
            hits = (0..db.games.len()).filter(|&i| typo_match(&db.texts[i], &words)).map(|i| (4, i)).collect();
        }
        hits.sort();
        hits.iter().map(|&(_, i)| db.games[i].name.clone()).collect()
    }

    #[test]
    fn picks_game_exe_over_launcher_and_falls_back_to_rich_presence() {
        let mut steam_game = detectable("5", Some("Steamy"), vec![exe(">javaw.exe", "win32", false)]);
        steam_game.third_party_skus = Some(vec![
            Sku { distributor: Some("epic".into()), id: Some("ABC123".into()) },
            Sku { distributor: Some("steam".into()), id: Some("730".into()) },
            Sku { distributor: Some("google_play".into()), id: Some("com.x".into()) },
        ]);
        steam_game.aliases = Some(vec!["steamy".into(), "Steamy 2".into()]);
        let games = to_db_games(vec![
            detectable("2", Some("Zeta"), vec![exe("launcher.exe", "win32", true), exe("zeta/game.exe", "win32", false), exe("zeta", "linux", false)]),
            detectable("1", Some("alpha: the game?"), vec![exe("Alpha.app", "darwin", false)]),
            detectable("3", Some("Only Launcher"), vec![exe("ol.exe", "win32", true)]),
            detectable("4", None, vec![exe("x.exe", "win32", false)]),
            steam_game,
        ]);
        let got: Vec<_> = games.iter().map(|g| (g.name.as_str(), g.exe.as_str(), g.is_launcher, g.rpc)).collect();
        assert_eq!(
            got,
            vec![
                ("alpha: the game?", "alpha_ the game_.exe", false, true),
                ("Only Launcher", "ol.exe", true, false),
                ("Steamy", "Steamy.exe", false, true),
                ("Zeta", "zeta/game.exe", false, false),
            ]
        );
        let steamy = &games[2];
        assert_eq!((steamy.steam.as_deref(), steamy.epic.clone(), steamy.mobile), (Some("730"), vec!["abc123".to_string()], true));
        assert_eq!(steamy.aliases, vec!["Steamy 2"]);
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
    fn rich_presence_games_pass_their_app_id() {
        let mut g = game("1441992604911665204", "Rise of Kingdoms.exe");
        assert_eq!(fake_exe_args(&g), vec!["Game"]);
        g.rpc = true;
        assert_eq!(fake_exe_args(&g), vec!["Game", "--rpc=1441992604911665204"]);
        assert!(is_app_id("1441992604911665204") && !is_app_id("") && !is_app_id("12a") && !is_app_id(&"9".repeat(26)));
    }

    #[test]
    fn custom_status_keeps_only_fields_discord_accepts() {
        let named = Presence {
            app_name: " My App ".into(),
            large_image: "rust".into(),
            button1_label: "View Repository".into(),
            button1_url: "https://github.com/me/app".into(),
            button2_label: "No link".into(),
            ..Default::default()
        };
        assert_eq!(
            presence_activity(&named, 1000),
            json!({ "name": "My App", "buttons": [{ "label": "View Repository", "url": "https://github.com/me/app" }], "assets": { "large_image": "rust" } })
        );
        let stream = Presence { activity_type: 3, details: "Watching".into(), ask_to_stream: true, ..Default::default() };
        assert_eq!(presence_activity(&stream, 1000), json!({ "details": "Watching" }));
        let tiles = MyGame { name: "Piano Tiles 2".into(), cover: Some("https://x.test/a.png".into()), ..Default::default() };
        let mut plain = json!({});
        name_and_art(&mut plain, &tiles);
        assert_eq!(plain, json!({ "name": "Piano Tiles 2", "assets": { "large_image": "https://x.test/a.png", "large_text": "Piano Tiles 2" } }));
        let mut own = json!({ "name": "Mine", "assets": { "large_image": "https://x.test/b.png" } });
        name_and_art(&mut own, &tiles);
        assert_eq!(own, json!({ "name": "Mine", "assets": { "large_image": "https://x.test/b.png" } }), "the status's own name and image win");
        let from = Presence { timer: "from".into(), timer_minutes: 90, timer_start: 1234, ..Default::default() };
        assert_eq!(presence_activity(&from, 6_000_000), json!({ "timestamps": { "start": 1234 } }));
        let locked = Presence { timer: "ago".into(), timer_minutes: 90, ..Default::default() };
        assert_eq!(presence_activity(&locked, 6_000_000), json!({}), "the fake exe makes a locked clock's timestamps");
        // Discord refuses buttons together with Ask to Join.
        assert!(presence_activity(&Presence { ask_to_join: true, ..named }, 1000).get("buttons").is_none());
        let p = Presence {
            activity_type: 5,
            details: " Competitive • Ascent ".into(),
            state: "7".into(), // Discord wants at least 2 characters
            party_size: 2,
            party_max: 5,
            large_image: "https://example.com/ascent.png".into(),
            small_text: "x".repeat(129),
            ..Default::default()
        };
        assert_eq!(
            presence_activity(&p, 1000),
            json!({ "type": 5, "details": "Competitive • Ascent", "party": { "size": [2, 5] }, "assets": { "large_image": "https://example.com/ascent.png" } })
        );
        let odd = Presence { activity_type: 1, party_size: 6, party_max: 5, timer: "left".into(), ..Default::default() };
        assert_eq!(presence_activity(&odd, 1000), json!({}));
        let reset = Presence { timer: "reset".into(), ..Default::default() };
        assert_eq!(presence_activity(&reset, 1000), json!({ "timestamps": { "start": 1000 } }));
        let left = Presence { timer: "left".into(), timer_minutes: 2, ..Default::default() };
        assert_eq!(presence_activity(&left, 1000), json!({ "timestamps": { "end": 121_000 } }));
        // Ask to Join needs a party id, a join secret and Playing; only http(s) links are kept.
        let join = Presence {
            activity_type: 5,
            ask_to_join: true,
            details_url: "https://tracker.gg/valorant".into(),
            state_url: "javascript:alert(1)".into(),
            ..Default::default()
        };
        assert_eq!(
            presence_activity(&join, 1000),
            json!({ "details_url": "https://tracker.gg/valorant", "party": { "id": "endo-party" }, "secrets": { "join": "endo-join" } })
        );
    }

    #[test]
    fn library_keys_round_trip_through_json() {
        let parsed: MyGame = serde_json::from_str(r#"{"id":"7","name":"N","exe":"n.exe","icon":"h","isLauncher":true}"#).unwrap();
        assert_eq!(game_key(&parsed), "7::n.exe");
        let shown = serde_json::to_value(view(parsed, None)).unwrap();
        assert_eq!(
            shown,
            json!({ "appId": "7", "name": "N", "exe": "n.exe", "isFavorite": false, "rpc": false, "icon": "h", "key": "7::n.exe" })
        );
    }

    #[test]
    fn folding_ignores_case_accents_and_punctuation() {
        assert_eq!(fold("Pokémon™: Legends Z-A"), "pokemon legends z a");
        assert_eq!(fold("  Counter-Strike 2 "), "counter strike 2");
        assert_eq!(fold("ÀÉÎÕÜ ßñ ťž"), "aeiou sn tz");
    }

    #[test]
    fn search_ranks_exact_then_prefix_then_words_and_forgives_typos() {
        let db = Db::new(
            vec![
                db_game("Counter-Strike 2", &["CS2"]),
                db_game("Pokémon Legends: Arceus", &[]),
                db_game("Valorant Mobile", &[]),
                db_game("VALORANT", &[]),
                db_game("Strike Force", &[]),
            ],
            0,
        );
        assert_eq!(search(&db, "valorant"), vec!["VALORANT", "Valorant Mobile"]);
        assert_eq!(search(&db, "cs2"), vec!["Counter-Strike 2"]);
        assert_eq!(search(&db, "strike"), vec!["Strike Force", "Counter-Strike 2"]);
        assert_eq!(search(&db, "counterstrike"), vec!["Counter-Strike 2"]);
        assert_eq!(search(&db, "pokemon arc"), vec!["Pokémon Legends: Arceus"]);
        assert_eq!(search(&db, "valornt"), vec!["Valorant Mobile", "VALORANT"]);
        assert!(close_enough("minecarft", "minecraft") && !close_enough("cs3", "cs2"));
    }

    #[test]
    fn rich_presence_entries_lose_lookups_to_entries_with_an_exe() {
        let mut rpc = db_game("Call of Duty", &[]);
        rpc.rpc = true;
        rpc.steam = Some("1".into());
        let mut real = db_game("Call of Duty", &[]);
        real.exe = "cod.exe".into();
        real.steam = Some("1".into());
        let db = Db::new(vec![rpc, real], 0);
        assert_eq!(db.get(&db.by_steam, "1").unwrap().exe, "cod.exe");
        assert_eq!(db.get(&db.by_name, "call of duty").unwrap().exe, "cod.exe");
    }
}
