# Endo’s Discord Game Spoofer

A small Windows app that makes Discord think you are playing a game. Pick a game from Discord’s own list of
detectable games and the app runs a tiny stand-in program with that game’s exe name. Discord sees the process and
shows the game in your status, which also counts toward Discord Quests.

> [!IMPORTANT]
> Use this at your own risk and only on your own account. Faking game activity can go against Discord’s Terms of
> Service, and Discord can change how detection works at any time.
> This project has nothing to do with Discord Inc. or any game publisher; their names and trademarks belong to them
> and are mentioned only to describe what the app does.

## Features

- **Find games:** search all ~24,800 games and apps in Discord’s list, with typo-tolerant search that also
  matches other names and ignores accents and punctuation. Filter to games Discord detects by exe (best for quests),
  Rich Presence ones, or mobile games. Paste a Discord application ID to add any app, even one missing from the list.
- **Popular shelves:** most played on Steam right now, top rated on Steam, big games from other launchers, and
  mobile games, all with cover art.
- **Mobile games and games without a PC exe** (e.g. Rise of Kingdoms) are shown through Discord Rich Presence.
- **Import from PC:** finds games installed through Steam, Epic Games and Windows’ installed apps (Riot, Battle.net,
  EA, Ubisoft…) that are in Discord’s list.
- **Custom Discord status:** give any game extra lines like the mode, map and score (“Competing in VALORANT ·
  Competitive • Ascent · In Match • 12 - 3 (5 of 5)”), with party size and image links. There are examples for
  popular games with one-click stages (in lobby, in queue, agent select, in match) and timers that restart
  or count down; a running game picks up changes within a few seconds.
- **Groups:** save sets of games and launch or stop them together.
- **Multi-select:** pick several games to add at once, or launch, stop, group or remove several library games.
- Favorites, auto-stop after 15–60 minutes, play time, desktop shortcuts and a warning when Discord isn’t running.
- Closing the window keeps running games going from the tray icon (about 10 MB of memory); the app quits when the last
  one stops.
- Signed automatic updates from GitHub releases.

## How it works

1. The app downloads Discord’s detectable-games list (only when it has changed) and keeps a small index of it.
2. When you launch a game it writes the stand-in program to
   `%APPDATA%/com.endo.discordgamespoofer/games/<GameID>/<path from Discord’s list>` and runs it.
3. The stand-in shows a small window with the game’s name. For games with no PC exe in Discord’s list it also
   connects to the Discord app on your PC and sets “Playing <game>” through Rich Presence. Close it, or stop it from
   the app, to stop “playing”. Discord may not count Rich Presence games toward quests.
4. A custom status is saved next to the stand-in as `<exe>.presence`. The stand-in checks it every 2 seconds and
   sends it to Discord through Rich Presence, so it replaces the plain “Playing <game>” line.

Besides Discord, the app contacts Steam’s public charts API and SteamSpy for the popular shelves, and loads cover
art from Steam’s and Discord’s image servers. No account or API key is needed and no personal data is sent,
though like any web request those servers see your IP address.

The app is built with [Tauri](https://tauri.app). The window is plain HTML/CSS/JS in `ui/`, running in Windows’
built-in WebView2, and the backend is Rust in `src-tauri/`. The stand-in program (`src/dummygame`) is a ~170 KB
native Win32 exe that is built and embedded into the app automatically.

## Build

You need [Rust](https://rustup.rs) and Node.js (for the Tauri CLI).

```bash
npm install
npm run dev        # run the app while developing
npm run build      # build the installer into src-tauri/target/release/bundle/nsis/
npm test           # Rust unit tests
npm run test:e2e   # end-to-end test of the built app (run npm run build first)
```

The end-to-end test drives the real app: it uses Discord’s live game list and your app data folder, adds and then
removes a test game, and creates and deletes a desktop shortcut.

## Releases

Releases are built and signed on your own machine:

```bash
npm run release -- 0.2.0 --dry-run   # build and check the files in dist/, change nothing else
npm run release -- 0.2.0             # build, commit the version, tag release_0.2.0, push, publish on GitHub
```

The script signs the update with `~/.tauri/endos-discord-game-spoofer.key` (or the `TAURI_SIGNING_PRIVATE_KEY`
environment variable) and needs the [GitHub CLI](https://cli.github.com) logged in. It uploads the installer, its
signature and `latest.json`; the app checks `latest.json` on startup and offers the update. Updates download from the
public release URL, so they only work while the repository’s releases are public.

The Build workflow on GitHub only runs when started by hand (Actions → Build → Run workflow).

## License

[MIT](LICENSE)
