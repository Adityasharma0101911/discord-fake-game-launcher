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

- Search Discord’s full list of detectable games and add the ones you want to a library.
- Launch, stop and favorite games; launch all favorites at once.
- Auto-stop a game after 15–60 minutes, with play time and a countdown shown in the app.
- Close the window and running games keep going from the tray icon; the app quits when the last one stops.
- Warns you when Discord isn’t running.
- Desktop shortcuts that start a fake game without opening the app.
- Signed automatic updates from GitHub releases.

## How it works

1. The app downloads Discord’s detectable-games list (only when it has changed) and keeps a small index of it.
2. When you launch a game it writes the stand-in program to
   `%APPDATA%/com.endo.discordgamespoofer/games/<GameID>/<path from Discord’s list>` and runs it.
3. The stand-in just shows a small window with the game’s name. Close it, or stop it from the app, to stop “playing”.

The app is built with [Tauri](https://tauri.app). The window is plain HTML/CSS/JS in `ui/`, running in Windows’
built-in WebView2, and the backend is Rust in `src-tauri/`. The stand-in program (`src/dummygame`) is a ~110 KB
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
