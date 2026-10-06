//! Games installed by the official Steam client, read from its own files.
//!
//! Steam keeps one or more library folders. `steamapps/libraryfolders.vdf`
//! in the Steam root lists them, and each library has an
//! `steamapps/appmanifest_<appid>.acf` per installed game, with the game
//! files under `steamapps/common/<installdir>`. Both are text KeyValues
//! (VDF). Steam isn't consistent about key case across versions, so keys are
//! matched case-insensitively.
//!
//! fumes also writes these files for its own installs (its library is laid
//! out the same way), so the Steam engine it hosts sees those games as
//! installed.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use keyvalues_parser::Value;

use crate::installs::Install;
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

/// `<library>/steamapps` and the install folder name, if `game` sits where
/// Steam keeps games (`<library>/steamapps/common/<installdir>`).
fn steamapps_of(game: &Path) -> Option<(PathBuf, String)> {
    let common = game.parent()?;
    let steamapps = common.parent()?;
    let is = |p: &Path, name: &str| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name));
    if !(is(common, "common") && is(steamapps, "steamapps")) {
        return None;
    }
    Some((
        steamapps.to_owned(),
        game.file_name()?.to_string_lossy().into_owned(),
    ))
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Write `appmanifest_<appid>.acf` for a fumes install, the way Steam
/// describes a fully installed game. Returns where, or `None` if the game
/// isn't in a Steam-style library (installed with `--dir` elsewhere).
pub fn write_manifest(install: &Install) -> Result<Option<PathBuf>> {
    let Some((steamapps, installdir)) = steamapps_of(&install.path) else {
        return Ok(None);
    };
    let updated = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut text = String::from("\"AppState\"\n{\n");
    for (key, value) in [
        ("appid", install.appid.to_string()),
        ("Universe", "1".into()),
        ("name", install.name.clone()),
        ("StateFlags", STATE_FULLY_INSTALLED.to_string()),
        ("installdir", installdir),
        ("LastUpdated", updated.to_string()),
        ("SizeOnDisk", install.size.to_string()),
        ("buildid", install.buildid.to_string()),
        // Update only when launched through Steam, which fumes doesn't do.
        ("AutoUpdateBehavior", "1".into()),
    ] {
        let _ = writeln!(text, "\t{}\t\t{}", quote(key), quote(&value));
    }
    text.push_str("\t\"InstalledDepots\"\n\t{\n");
    for (depot, manifest) in &install.depots {
        let _ = writeln!(
            text,
            "\t\t\"{depot}\"\n\t\t{{\n\t\t\t\"manifest\"\t\t\"{manifest}\"\n\t\t}}"
        );
    }
    text.push_str("\t}\n");
    for section in ["UserConfig", "MountedConfig"] {
        let _ = writeln!(
            text,
            "\t\"{section}\"\n\t{{\n\t\t\"language\"\t\t{}\n\t}}",
            quote(&install.language)
        );
    }
    text.push_str("}\n");
    fs::create_dir_all(&steamapps)?;
    let path = steamapps.join(format!("appmanifest_{}.acf", install.appid));
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path))
}

pub fn remove_manifest(install: &Install) {
    if let Some((steamapps, _)) = steamapps_of(&install.path) {
        let _ = fs::remove_file(steamapps.join(format!("appmanifest_{}.acf", install.appid)));
    }
}

/// Make sure `library` is one of the library folders of the Steam root
/// `root`, adding it to `steamapps/libraryfolders.vdf` if needed (the file
/// is created, with `root` itself as library 0, if it doesn't exist yet).
/// Returns whether anything changed.
pub fn add_library_folder(root: &Path, library: &Path) -> Result<bool> {
    let path = root.join("steamapps/libraryfolders.vdf");
    let existing = fs::read_to_string(&path).ok();
    let folders = existing
        .as_deref()
        .map(parse_library_folders)
        .unwrap_or_default();
    if folders.iter().any(|f| f == library) {
        return Ok(false);
    }
    let entry = |index: usize, folder: &Path| {
        format!(
            "\t\"{index}\"\n\t{{\n\t\t\"path\"\t\t{}\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n",
            quote(&folder.to_string_lossy())
        )
    };
    let text = match existing
        .as_deref()
        .and_then(|t| t.rfind('}').map(|end| (t, end)))
    {
        // Append before the closing brace, leaving Steam's own entries
        // (and formatting) untouched.
        Some((text, end)) => {
            let next = numbered_keys(text).max().map_or(0, |n| n + 1);
            format!("{}{}{}", &text[..end], entry(next, library), &text[end..])
        }
        None => format!(
            "\"libraryfolders\"\n{{\n{}{}}}\n",
            entry(0, root),
            entry(1, library)
        ),
    };
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// The numbered top-level keys of a `libraryfolders.vdf`.
fn numbered_keys(text: &str) -> impl Iterator<Item = usize> + '_ {
    keyvalues_parser::parse(text)
        .ok()
        .and_then(|vdf| {
            vdf.value.get_obj().map(|obj| {
                obj.keys()
                    .filter_map(|k| k.parse().ok())
                    .collect::<Vec<usize>>()
            })
        })
        .unwrap_or_default()
        .into_iter()
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

    fn install(path: PathBuf) -> Install {
        Install {
            appid: 480,
            name: "Spacewar \"Deluxe\"".into(),
            path,
            os: "macos".into(),
            arch: "64".into(),
            language: "english".into(),
            buildid: 12345,
            depots: std::collections::BTreeMap::from([(481, 111), (482, 222)]),
            size: 2048,
            launch: vec![],
        }
    }

    #[test]
    fn writes_a_manifest_steam_and_fumes_can_read() {
        let lib = tempfile::tempdir().unwrap();
        let steamapps = lib.path().join("steamapps");
        let game = install(steamapps.join("common/Spacewar"));
        let path = write_manifest(&game).unwrap().unwrap();
        assert_eq!(path, steamapps.join("appmanifest_480.acf"));

        let text = fs::read_to_string(&path).unwrap();
        let parsed = parse_manifest(&text, &steamapps).unwrap();
        assert_eq!(parsed.appid, 480);
        assert_eq!(parsed.name, "Spacewar \"Deluxe\"");
        assert_eq!(parsed.path, game.path);
        assert!(parsed.complete);
        assert_eq!(parsed.size_on_disk, 2048);
        assert!(
            text.contains("\"482\"\n\t\t{\n\t\t\t\"manifest\"\t\t\"222\""),
            "{text}"
        );

        remove_manifest(&game);
        assert!(!path.exists());
    }

    #[test]
    fn no_manifest_outside_a_steam_library() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            write_manifest(&install(dir.path().join("Games/Spacewar")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn adds_library_folders_without_touching_steams_entries() {
        let root = tempfile::tempdir().unwrap();
        let library = PathBuf::from("/Users/me/fumes library");

        // No file yet: the root becomes library 0, ours library 1.
        assert!(add_library_folder(root.path(), &library).unwrap());
        assert!(!add_library_folder(root.path(), &library).unwrap());
        let file = root.path().join("steamapps/libraryfolders.vdf");
        assert_eq!(
            parse_library_folders(&fs::read_to_string(&file).unwrap()),
            [root.path().to_owned(), library.clone()]
        );

        // Steam's own file: appended after its highest entry.
        let steam = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/root\"\n\t\t\"contentid\"\t\t\"77\"\n\t}\n\t\"3\"\n\t{\n\t\t\"path\"\t\t\"/ext\"\n\t}\n}\n";
        fs::write(&file, steam).unwrap();
        assert!(add_library_folder(root.path(), &library).unwrap());
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(&steam[..steam.len() - 2]), "{text}");
        assert!(text.contains("\t\"4\"\n"), "{text}");
        assert_eq!(
            parse_library_folders(&text),
            [PathBuf::from("/root"), PathBuf::from("/ext"), library]
        );
    }
}
