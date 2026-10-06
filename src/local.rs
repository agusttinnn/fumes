//! Games installed by the official Steam client, read from its own files.
//!
//! Steam keeps one or more library folders. `steamapps/libraryfolders.vdf`
//! in the Steam root lists them, and each library has an
//! `steamapps/appmanifest_<appid>.acf` per installed game, with the game
//! files under `steamapps/common/<installdir>`. Both are text KeyValues
//! (VDF). Steam isn't consistent about key case across versions, so keys are
//! matched case-insensitively.

use std::fs;
use std::path::{Path, PathBuf};

use keyvalues_parser::Value;

use crate::kv::get_str;

#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub appid: u32,
    pub name: String,
    pub path: PathBuf,
    pub size_on_disk: u64,
    /// False while Steam is still downloading or updating it.
    pub complete: bool,
    /// Downloaded by fumes rather than the Steam client.
    pub by_fumes: bool,
}

/// `StateFlags` bit Steam sets once every file is in place.
const STATE_FULLY_INSTALLED: u64 = 4;

/// The Steam root, if Steam is installed. `FUMES_STEAM_DIR` overrides the
/// platform default.
pub fn steam_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FUMES_STEAM_DIR") {
        return Some(dir.into());
    }
    candidates()
        .into_iter()
        .find(|p| p.join("steamapps").is_dir())
}

fn candidates() -> Vec<PathBuf> {
    let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_owned()) else {
        return Vec::new();
    };
    if cfg!(target_os = "macos") {
        vec![home.join("Library/Application Support/Steam")]
    } else if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Program Files (x86)\Steam"),
            PathBuf::from(r"C:\Program Files\Steam"),
        ]
    } else {
        vec![
            home.join(".steam/steam"),
            home.join(".local/share/Steam"),
            // Flatpak
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ]
    }
}

/// Every installed game across all library folders. Unreadable or malformed
/// manifests are skipped: Steam rewrites them while it works, and one bad
/// file shouldn't hide the rest of the library.
pub fn scan(root: &Path) -> Vec<Installed> {
    let mut games: Vec<Installed> = library_folders(root)
        .iter()
        .flat_map(|lib| scan_library(lib))
        .collect();
    games.sort_by_key(|g| g.appid);
    games.dedup_by_key(|g| g.appid);
    games
}

/// The root is always a library; `libraryfolders.vdf` adds the others.
fn library_folders(root: &Path) -> Vec<PathBuf> {
    let mut folders = vec![root.to_owned()];
    // Older clients kept the list under config/.
    let list = ["steamapps/libraryfolders.vdf", "config/libraryfolders.vdf"]
        .iter()
        .find_map(|p| fs::read_to_string(root.join(p)).ok());
    if let Some(text) = list {
        folders.extend(parse_library_folders(&text));
    }
    folders.dedup();
    folders
}

/// Entries are numbered objects (`"0" { "path" "…" … }`). Very old files
/// had bare strings (`"1" "D:\\Games"`) and stray keys like
/// `ContentStatsID`; take paths from either numbered form, ignore the rest.
fn parse_library_folders(text: &str) -> Vec<PathBuf> {
    let Ok(vdf) = keyvalues_parser::parse(text) else {
        return Vec::new();
    };
    let Some(obj) = vdf.value.get_obj() else {
        return Vec::new();
    };
    obj.iter()
        .filter(|(key, _)| key.parse::<u32>().is_ok())
        .flat_map(|(_, values)| values)
        .filter_map(|value| match value {
            Value::Obj(entry) => get_str(entry, "path").map(PathBuf::from),
            Value::Str(path) => Some(PathBuf::from(path.as_ref())),
        })
        .collect()
}

fn scan_library(lib: &Path) -> Vec<Installed> {
    let steamapps = lib.join("steamapps");
    let Ok(entries) = fs::read_dir(&steamapps) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("appmanifest_") && name.ends_with(".acf")
        })
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .filter_map(|text| parse_manifest(&text, &steamapps))
        .collect()
}

fn parse_manifest(text: &str, steamapps: &Path) -> Option<Installed> {
    let vdf = keyvalues_parser::parse(text).ok()?;
    let state = vdf.value.get_obj()?;
    let appid = get_str(state, "appid")?.parse().ok()?;
    let installdir = get_str(state, "installdir")?;
    let flags: u64 = get_str(state, "StateFlags")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Some(Installed {
        appid,
        name: get_str(state, "name").unwrap_or_default().to_owned(),
        path: steamapps.join("common").join(installdir),
        size_on_disk: get_str(state, "SizeOnDisk")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        complete: flags & STATE_FULLY_INSTALLED != 0,
        by_fumes: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(appid: u32, name: &str, dir: &str, flags: u32) -> String {
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"{name}\"\n\
             \t\"StateFlags\"\t\t\"{flags}\"\n\t\"installdir\"\t\t\"{dir}\"\n\
             \t\"SizeOnDisk\"\t\t\"1024\"\n}}\n"
        )
    }

    #[test]
    fn scans_every_library_folder() {
        let root = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let root_apps = root.path().join("steamapps");
        let extra_apps = extra.path().join("steamapps");
        fs::create_dir_all(&root_apps).unwrap();
        fs::create_dir_all(&extra_apps).unwrap();

        fs::write(
            root_apps.join("libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"contentstatsid\"\t\"123\"\n\
                 \t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\
                 \t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"apps\" {{ \"12210\" \"1024\" }}\n\t}}\n}}\n",
                root.path().display(),
                extra.path().display()
            ),
        )
        .unwrap();
        fs::write(
            root_apps.join("appmanifest_440.acf"),
            manifest(440, "Team Fortress 2", "Team Fortress 2", 4),
        )
        .unwrap();
        // Mid-update: StateFlags without the fully-installed bit.
        fs::write(
            extra_apps.join("appmanifest_12210.acf"),
            manifest(12210, "Grand Theft Auto IV", "GTAIV", 1026),
        )
        .unwrap();
        fs::write(extra_apps.join("appmanifest_1.acf"), "not vdf {").unwrap();

        let games = scan(root.path());
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].appid, 440);
        assert!(games[0].complete);
        assert_eq!(games[0].path, root_apps.join("common/Team Fortress 2"));
        assert_eq!(games[1].name, "Grand Theft Auto IV");
        assert!(!games[1].complete);
        assert_eq!(games[1].size_on_disk, 1024);
    }

    #[test]
    fn reads_old_style_folder_list() {
        let text = "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\"1\"\n\t\"1\"\t\"D:\\\\Games\"\n}\n";
        assert_eq!(
            parse_library_folders(text),
            vec![PathBuf::from("D:\\Games")]
        );
    }

    #[test]
    fn key_case_does_not_matter() {
        let text = "\"appstate\" { \"AppID\" \"7\" \"InstallDir\" \"x\" \"stateflags\" \"4\" }";
        let game = parse_manifest(text, Path::new("/lib/steamapps")).unwrap();
        assert_eq!(game.appid, 7);
        assert!(game.complete);
    }
}
