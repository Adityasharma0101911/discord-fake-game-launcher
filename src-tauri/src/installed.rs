//! Finds games installed by launchers on this PC, for the "Import installed games" dialog.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ,
    RRF_NOEXPAND, RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
};

/// A game found on this PC; main.rs matches it against Discord's list.
pub struct Found {
    /// "Steam", "Epic Games" or "Windows" (the installed-apps list).
    pub source: &'static str,
    pub name: String,
    pub steam_app_id: Option<String>,
    /// Lowercased Epic ids (catalog item ids, namespaces, app names).
    pub epic_ids: Vec<String>,
    pub install_dir: Option<PathBuf>,
}

/// Every source is best-effort: anything unreadable just contributes nothing.
pub fn scan() -> Vec<Found> {
    let mut found = steam();
    found.extend(epic());
    found.extend(windows_apps());
    found
}

/// True if a file whose path ends with `suffix` (e.g. "win64/cs2.exe") exists under `dir`.
pub fn has_exe(dir: &Path, suffix: &str) -> bool {
    let suffix = format!("/{}", suffix.replace('\\', "/").trim_start_matches('/').to_lowercase());
    if suffix == "/" {
        return false;
    }
    let mut stack = vec![(dir.to_path_buf(), 0)];
    let mut visited = 0;
    while let Some((path, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else { continue };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 20_000 {
                return false;
            }
            // file_type() doesn't follow links; junctions count as symlinks, so they're skipped too.
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if depth < 6 {
                    stack.push((entry.path(), depth + 1));
                }
            } else if ft.is_file() {
                let p = entry.path().to_string_lossy().replace('\\', "/").to_lowercase();
                if p.ends_with(&suffix) {
                    return true;
                }
            }
        }
    }
    false
}

// ---- Steam ----

fn steam() -> Vec<Found> {
    let root = reg_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
        .or_else(|| reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"))
        .map(|p| PathBuf::from(p.replace('/', "\\")))
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)\Steam"));
    let mut libraries = vec![root.clone()];
    if let Ok(text) = fs::read_to_string(root.join(r"steamapps\libraryfolders.vdf")) {
        libraries.extend(parse_library_folders(&text).into_iter().map(PathBuf::from));
    }
    let mut seen = Vec::new();
    libraries.retain(|l| {
        let key = l.to_string_lossy().to_lowercase().trim_end_matches('\\').to_string();
        !seen.contains(&key) && {
            seen.push(key);
            true
        }
    });

    let mut found = Vec::new();
    for lib in libraries {
        let Ok(entries) = fs::read_dir(lib.join("steamapps")) else { continue };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().to_lowercase();
            let Some(id) = file.strip_prefix("appmanifest_").and_then(|f| f.strip_suffix(".acf")) else { continue };
            let Ok(text) = fs::read_to_string(entry.path()) else { continue };
            if let Some(mut f) = parse_app_manifest(&text, &lib) {
                f.steam_app_id = f.steam_app_id.or_else(|| Some(id.to_string()));
                found.push(f);
            }
        }
    }
    found
}

/// `"key"  "value"` pairs from Valve KeyValues text, in file order; braces and nesting are ignored.
fn vdf_pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('"').collect();
            (parts.len() >= 5).then(|| (parts[1].to_lowercase(), parts[3].replace(r"\\", r"\")))
        })
        .collect()
}

fn parse_library_folders(text: &str) -> Vec<String> {
    vdf_pairs(text).into_iter().filter(|(k, _)| k == "path").map(|(_, v)| v).collect()
}

fn parse_app_manifest(text: &str, library: &Path) -> Option<Found> {
    let pairs = vdf_pairs(text);
    // First match wins: the top-level keys come before nested sections like InstalledDepots.
    let get = |key: &str| pairs.iter().find(|(k, v)| k == key && !v.is_empty()).map(|(_, v)| v.clone());
    Some(Found {
        source: "Steam",
        name: get("name")?,
        steam_app_id: get("appid"),
        epic_ids: Vec::new(),
        install_dir: get("installdir").map(|d| library.join("steamapps").join("common").join(d)),
    })
}

// ---- Epic Games ----

fn epic() -> Vec<Found> {
    let data = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let Ok(entries) = fs::read_dir(data.join(r"Epic\EpicGamesLauncher\Data\Manifests")) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("item")))
        .filter_map(|e| parse_epic_item(&fs::read_to_string(e.path()).ok()?))
        .collect()
}

fn parse_epic_item(text: &str) -> Option<Found> {
    let v: Value = serde_json::from_str(text).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let mut epic_ids: Vec<String> = Vec::new();
    for k in ["CatalogItemId", "CatalogNamespace", "AppName", "MainGameCatalogItemId", "MainGameCatalogNamespace", "MainGameAppName"] {
        if let Some(id) = s(k).map(str::to_lowercase) {
            if !epic_ids.contains(&id) {
                epic_ids.push(id);
            }
        }
    }
    Some(Found {
        source: "Epic Games",
        name: s("DisplayName")?.to_string(),
        steam_app_id: None,
        epic_ids,
        install_dir: s("InstallLocation").map(PathBuf::from),
    })
}

// ---- Windows installed apps ----

fn windows_apps() -> Vec<Found> {
    const UNINSTALL: [(HKEY, &str); 3] = [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    let mut found = Vec::new();
    for (root, path) in UNINSTALL {
        for sub in reg_subkeys(root, path) {
            let key = format!(r"{path}\{sub}");
            let value = |name: &str| reg_string(root, &key, name).map(|s| s.trim().trim_matches('"').trim().to_string());
            let Some(name) = value("DisplayName").filter(|n| !n.is_empty()) else { continue };
            let uninstall = value("UninstallString").unwrap_or_default().to_lowercase();
            // Updates/components, and games the Steam/Epic sources already report.
            if reg_dword(root, &key, "SystemComponent") == Some(1)
                || value("ParentKeyName").is_some()
                || uninstall.contains("steam://uninstall")
                || uninstall.contains("com.epicgames.launcher://")
            {
                continue;
            }
            found.push(Found {
                source: "Windows",
                name,
                steam_app_id: None,
                epic_ids: Vec::new(),
                install_dir: value("InstallLocation").filter(|d| !d.is_empty()).map(PathBuf::from),
            });
        }
    }
    found
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A REG_SZ / REG_EXPAND_SZ value.
// shortcut: %VARS% in REG_EXPAND_SZ are left unexpanded (RegGetValueW rejects EXPAND_SZ without NOEXPAND); expand if a source needs it.
fn reg_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let (subkey, value) = (wide(subkey), wide(value));
    let mut buf = vec![0u16; 512];
    loop {
        let mut bytes = (buf.len() * 2) as u32;
        let err = unsafe {
            RegGetValueW(
                root,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        match err {
            ERROR_SUCCESS => {
                // `bytes` includes the trailing NUL that RegGetValueW guarantees.
                let s = &buf[..(bytes as usize / 2).min(buf.len())];
                let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                return Some(String::from_utf16_lossy(&s[..end]));
            }
            ERROR_MORE_DATA if bytes as usize > buf.len() * 2 && bytes < 1 << 20 => buf = vec![0; bytes as usize / 2 + 1],
            _ => return None,
        }
    }
}

fn reg_dword(root: HKEY, subkey: &str, value: &str) -> Option<u32> {
    let (subkey, value) = (wide(subkey), wide(value));
    let (mut data, mut bytes) = (0u32, 4u32);
    let err = unsafe {
        RegGetValueW(root, subkey.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut bytes)
    };
    (err == ERROR_SUCCESS).then_some(data)
}

fn reg_subkeys(root: HKEY, path: &str) -> Vec<String> {
    let mut key: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_READ, &mut key) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut buf = [0u16; 256]; // registry key names are at most 255 chars
    for i in 0.. {
        let mut len = buf.len() as u32;
        let null = std::ptr::null_mut();
        match unsafe { RegEnumKeyExW(key, i, buf.as_mut_ptr(), &mut len, std::ptr::null(), null, null.cast(), null.cast()) } {
            ERROR_SUCCESS => names.push(String::from_utf16_lossy(&buf[..len as usize])),
            // No more keys, or an error that would repeat for every index: stop rather than spin.
            _ => break,
        }
    }
    unsafe { RegCloseKey(key) };
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_folders() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"730\"\t\t\"123\"\n\t\t}\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(parse_library_folders(vdf), [r"C:\Program Files (x86)\Steam", r"D:\SteamLibrary"]);
    }

    #[test]
    fn app_manifest() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n\t\"InstalledDepots\"\n\t{\n\t\t\"731\"\n\t\t{\n\t\t\t\"name\"\t\t\"other\"\n\t\t}\n\t}\n}\n";
        let f = parse_app_manifest(acf, Path::new(r"D:\SteamLibrary")).unwrap();
        assert_eq!((f.source, f.name.as_str(), f.steam_app_id.as_deref()), ("Steam", "Counter-Strike 2", Some("730")));
        assert_eq!(f.install_dir.unwrap(), PathBuf::from(r"D:\SteamLibrary\steamapps\common\Counter-Strike Global Offensive"));
    }

    #[test]
    fn epic_item() {
        let json = r#"{"DisplayName": "Fortnite", "InstallLocation": "C:\\Program Files\\Epic Games\\Fortnite",
            "CatalogItemId": "4FE75BBC5A674F4F9B356B5C90567DA5", "CatalogNamespace": "fn", "AppName": "Fortnite",
            "MainGameCatalogItemId": "4fe75bbc5a674f4f9b356b5c90567da5", "MainGameCatalogNamespace": "fn", "MainGameAppName": ""}"#;
        let f = parse_epic_item(json).unwrap();
        assert_eq!(f.name, "Fortnite");
        assert_eq!(f.epic_ids, ["4fe75bbc5a674f4f9b356b5c90567da5", "fn", "fortnite"]);
        assert_eq!(f.install_dir.unwrap(), PathBuf::from(r"C:\Program Files\Epic Games\Fortnite"));
        assert!(parse_epic_item("not json").is_none());
    }

    #[test]
    fn exe_search() {
        let dir = std::env::temp_dir().join(format!("endo-has-exe-{}", std::process::id()));
        fs::create_dir_all(dir.join(r"Game\Bin\Win64")).unwrap();
        fs::write(dir.join(r"Game\Bin\Win64\CS2.exe"), b"").unwrap();
        let ok = (
            has_exe(&dir, "win64/cs2.exe"),
            has_exe(&dir, "cs2.exe"),
            has_exe(&dir, r"bin\win64\cs2.exe"),
            has_exe(&dir, "s2.exe"),
            has_exe(&dir, "win32/cs2.exe"),
            has_exe(&dir.join("missing"), "cs2.exe"),
        );
        fs::remove_dir_all(&dir).ok();
        assert_eq!(ok, (true, true, true, false, false, false));
    }

    #[test]
    #[ignore = "diagnostic: prints what scan() finds on this machine"]
    fn scan_this_pc() {
        let t = std::time::Instant::now();
        let found = scan();
        println!("scan took {:?}", t.elapsed());
        for source in ["Steam", "Epic Games", "Windows"] {
            let names: Vec<_> = found.iter().filter(|f| f.source == source).map(|f| f.name.as_str()).collect();
            println!("{source}: {} found, e.g. {:?}", names.len(), &names[..names.len().min(10)]);
        }
    }
}
