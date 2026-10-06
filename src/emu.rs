//! Running games without the Steam client, through gbe_fork (the
//! maintained fork of Goldberg's Steam emulator).
//!
//! Games built on Steamworks load `steam_api(64).dll` / `libsteam_api.so`,
//! which talks to a running Steam client. gbe_fork is a drop-in replacement
//! that answers those calls itself, configured by a `steam_settings` folder
//! next to it. fumes fetches gbe_fork's release, swaps it in (keeping the
//! original as `<name>.fumes-orig` so `disable` can put it back), and
//! writes the settings from what it knows about the account and game.
//!
//! Limits, all from gbe_fork or from the games themselves:
//! - gbe_fork only builds for Windows and Linux; native macOS builds
//!   (`libsteam_api.dylib`) can't be emulated. On a Mac, install the
//!   Windows build and run it through Wine.
//! - Games with DRM on top of Steamworks (SteamStub, Denuvo, …) still check
//!   for the real client. fumes detects SteamStub and says so; it doesn't
//!   remove DRM.
//! - Multiplayer works over LAN only; Steam's online services (matchmaking,
//!   workshop, cloud saves, achievements on your profile) aren't there.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::dirs::Dirs;

const REPO: &str = "Detanup01/gbe_fork";
const BACKUP_SUFFIX: &str = ".fumes-orig";

/// What the emulator should tell the game.
pub struct Profile {
    pub appid: u32,
    /// Shown to the game and to LAN peers: the persona, never the login.
    pub persona: String,
    pub steam_id: u64,
    pub language: String,
    pub dlcs: BTreeMap<u32, String>,
    pub depots: Vec<u32>,
}

/// Recorded in `<game>/.fumes/emu.json` so `disable` undoes exactly what
/// `enable` did.
#[derive(Default, Serialize, Deserialize)]
struct Record {
    release: String,
    /// Replaced libraries, relative to the game folder.
    libs: Vec<String>,
    /// `steam_settings` folders fumes created (and may delete).
    settings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LibOs {
    Windows,
    Linux,
    MacOs,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Arch {
    X64,
    X86,
}

impl Arch {
    fn dir(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::X86 => "x86",
        }
    }
}

pub struct Report {
    pub replaced: Vec<PathBuf>,
    pub release: String,
    pub warnings: Vec<String>,
}

/// Swap gbe_fork into every Steamworks library in the game folder.
pub async fn enable(
    dirs: &Dirs,
    game: &Path,
    profile: &Profile,
    exes: &[PathBuf],
) -> Result<Report> {
    let libs = find_steam_api(game);
    let mut warnings = Vec::new();
    for exe in exes {
        if fs::read(exe).is_ok_and(|bytes| has_steamstub(&bytes)) {
            warnings.push(format!(
                "{} is wrapped in SteamStub DRM, which checks for the real Steam client before \
                 the emulator is ever loaded; the game will likely refuse to start",
                exe.display()
            ));
        }
    }

    let (mac, libs): (Vec<_>, Vec<_>) = libs.into_iter().partition(|(_, os)| *os == LibOs::MacOs);
    if libs.is_empty() {
        if !mac.is_empty() {
            bail!(
                "this is a native macOS build (libsteam_api.dylib) and gbe_fork only exists for \
                 Windows and Linux; reinstall with `--os windows` and run it through Wine"
            );
        }
        bail!(
            "no steam_api library in {}: the game may not use Steamworks at all (then it \
             runs as is), or loads it from somewhere fumes doesn't look",
            game.display()
        );
    }
    if !mac.is_empty() {
        warnings.push("skipped libsteam_api.dylib files: gbe_fork has no macOS build".into());
    }

    let mut release_tag = String::new();
    let mut releases: BTreeMap<&'static str, PathBuf> = BTreeMap::new();
    for (_, os) in &libs {
        let key = os_key(*os);
        if !releases.contains_key(key) {
            let (tag, dir) = release(dirs, key, false).await?;
            release_tag = tag;
            releases.insert(key, dir);
        }
    }

    let mut record = load_record(game);
    record.release = release_tag.clone();
    let mut replaced = Vec::new();
    for (lib, os) in libs {
        let backup = backup_path(&lib);
        if !backup.exists() {
            fs::rename(&lib, &backup).with_context(|| format!("backing up {}", lib.display()))?;
        }
        let original = fs::read(&backup)?;
        let Some(arch) = arch_of(&original) else {
            // Leave it as it was.
            fs::rename(&backup, &lib)?;
            warnings.push(format!(
                "{}: unknown architecture, left alone",
                lib.display()
            ));
            continue;
        };
        let name = lib.file_name().unwrap().to_string_lossy().into_owned();
        let emu_name = match (os, arch) {
            (LibOs::Windows, Arch::X64) => "steam_api64.dll",
            (LibOs::Windows, Arch::X86) => "steam_api.dll",
            _ => "libsteam_api.so",
        };
        let source = releases[os_key(os)]
            .join("regular")
            .join(arch.dir())
            .join(emu_name);
        fs::copy(&source, &lib).with_context(|| format!("installing the emulator as {name}"))?;

        let settings = lib.parent().unwrap().join("steam_settings");
        let rel_settings = relative(game, &settings);
        if !settings.exists() && !record.settings.contains(&rel_settings) {
            record.settings.push(rel_settings);
        }
        write_settings(&settings, profile, &interfaces(&original))?;

        let rel_lib = relative(game, &lib);
        if !record.libs.contains(&rel_lib) {
            record.libs.push(rel_lib);
        }
        replaced.push(lib);
    }
    save_record(game, &record)?;
    Ok(Report {
        replaced,
        release: release_tag,
        warnings,
    })
}

/// Put the original libraries back. Returns how many were restored.
pub fn disable(game: &Path) -> Result<usize> {
    let record = load_record(game);
    let mut libs: Vec<PathBuf> = record.libs.iter().map(|l| game.join(l)).collect();
    // Backups the record doesn't know about (an older or interrupted run).
    for (lib, _) in find_steam_api(game) {
        if backup_path(&lib).exists() && !libs.contains(&lib) {
            libs.push(lib);
        }
    }
    let mut restored = 0;
    for lib in &libs {
        let backup = backup_path(lib);
        if backup.exists() {
            fs::rename(&backup, lib).with_context(|| format!("restoring {}", lib.display()))?;
            restored += 1;
        }
    }
    for settings in &record.settings {
        let _ = fs::remove_dir_all(game.join(settings));
    }
    let _ = fs::remove_file(record_path(game));
    Ok(restored)
}

pub fn is_enabled(game: &Path) -> bool {
    record_path(game).exists()
}

fn os_key(os: LibOs) -> &'static str {
    match os {
        LibOs::Windows => "windows",
        LibOs::Linux | LibOs::MacOs => "linux",
    }
}

fn backup_path(lib: &Path) -> PathBuf {
    let mut name = lib.file_name().unwrap().to_os_string();
    name.push(BACKUP_SUFFIX);
    lib.with_file_name(name)
}

fn record_path(game: &Path) -> PathBuf {
    game.join(".fumes/emu.json")
}

fn load_record(game: &Path) -> Record {
    fs::read(record_path(game))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_record(game: &Path, record: &Record) -> Result<()> {
    let path = record_path(game);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, serde_json::to_vec_pretty(record)?)?;
    Ok(())
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every Steamworks client library under the game folder (some games carry
/// several: launcher, engine, 32- and 64-bit builds).
fn find_steam_api(game: &Path) -> Vec<(PathBuf, LibOs)> {
    let mut found = Vec::new();
    let mut stack = vec![game.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if kind.is_dir() {
                if name != ".fumes" && name != "steam_settings" {
                    stack.push(entry.path());
                }
            } else if kind.is_file() {
                let os = match name.as_str() {
                    "steam_api.dll" | "steam_api64.dll" => LibOs::Windows,
                    "libsteam_api.so" => LibOs::Linux,
                    "libsteam_api.dylib" => LibOs::MacOs,
                    _ => continue,
                };
                found.push((entry.path(), os));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// From the PE or ELF header.
fn arch_of(bytes: &[u8]) -> Option<Arch> {
    if bytes.starts_with(b"\x7fELF") {
        return match bytes.get(4) {
            Some(1) => Some(Arch::X86),
            Some(2) => Some(Arch::X64),
            _ => None,
        };
    }
    let pe = pe_header(bytes)?;
    match u16::from_le_bytes(bytes.get(pe + 4..pe + 6)?.try_into().ok()?) {
        0x8664 => Some(Arch::X64),
        0x014c => Some(Arch::X86),
        _ => None,
    }
}

/// Offset of the `PE\0\0` signature.
fn pe_header(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"MZ") {
        return None;
    }
    let offset = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    (bytes.get(offset..offset + 4)? == b"PE\0\0").then_some(offset)
}

/// SteamStub adds a `.bind` section holding its loader.
pub fn has_steamstub(bytes: &[u8]) -> bool {
    let Some(pe) = pe_header(bytes) else {
        return false;
    };
    let read_u16 = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let (Some(sections), Some(optional_size)) = (read_u16(pe + 6), read_u16(pe + 20)) else {
        return false;
    };
    let table = pe + 24 + optional_size;
    (0..sections).any(|i| {
        bytes
            .get(table + i * 40..table + i * 40 + 8)
            .is_some_and(|name| name.starts_with(b".bind"))
    })
}

/// The interface versions the original library was built against, which
/// gbe_fork needs to answer with matching vtables. Same patterns and rule
/// as gbe_fork's `generate_interfaces` tool.
pub fn interfaces(original: &[u8]) -> Vec<String> {
    const PATTERNS: &[&str] = &[
        r"STEAMAPPS_INTERFACE_VERSION\d+",
        r"SteamApps\d+",
        r"STEAMAPPLIST_INTERFACE_VERSION\d+",
        r"STEAMAPPTICKET_INTERFACE_VERSION\d+",
        r"SteamClient\d+",
        r"STEAMCONTROLLER_INTERFACE_VERSION",
        r"SteamController\d+",
        r"SteamFriends\d+",
        r"SteamGameServerStats\d+",
        r"SteamGameCoordinator\d+",
        r"SteamGameServer\d+",
        r"STEAMHTMLSURFACE_INTERFACE_VERSION_\d+",
        r"STEAMHTTP_INTERFACE_VERSION\d+",
        r"SteamInput\d+",
        r"STEAMINVENTORY_INTERFACE_V\d+",
        r"SteamMatchMakingServers\d+",
        r"SteamMatchMaking\d+",
        r"SteamMatchGameSearch\d+",
        r"SteamParties\d+",
        r"STEAMMUSIC_INTERFACE_VERSION\d+",
        r"STEAMMUSICREMOTE_INTERFACE_VERSION\d+",
        r"SteamNetworkingMessages\d+",
        r"SteamNetworkingSockets\d+",
        r"SteamNetworkingUtils\d+",
        r"SteamNetworking\d+",
        r"STEAMPARENTALSETTINGS_INTERFACE_VERSION\d+",
        r"STEAMREMOTEPLAY_INTERFACE_VERSION\d+",
        r"STEAMREMOTESTORAGE_INTERFACE_VERSION\d+",
        r"STEAMSCREENSHOTS_INTERFACE_VERSION\d+",
        r"STEAMTIMELINE_INTERFACE_V\d+",
        r"STEAMUGC_INTERFACE_VERSION\d+",
        r"SteamUser\d+",
        r"STEAMUSERSTATS_INTERFACE_VERSION\d+",
        r"SteamUtils\d+",
        r"STEAMVIDEO_INTERFACE_V\d+",
        r"STEAMUNIFIEDMESSAGES_INTERFACE_VERSION\d+",
        r"SteamMasterServerUpdater\d+",
    ];
    let mut out: Vec<String> = Vec::new();
    for pattern in PATTERNS {
        let re = regex::bytes::Regex::new(pattern).unwrap();
        let mut matches: Vec<String> = re
            .find_iter(original)
            .map(|m| String::from_utf8_lossy(m.as_bytes()).into_owned())
            .collect();
        // Newer SDKs keep only SteamClient017 for the legacy SteamClient()
        // export; the other SteamClientNNN strings are noise.
        if *pattern == r"SteamClient\d+"
            && matches.len() > 1
            && matches.iter().any(|m| m == "SteamClient017")
        {
            matches.retain(|m| m == "SteamClient017");
        }
        for m in matches {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

fn write_settings(dir: &Path, profile: &Profile, interfaces: &[String]) -> Result<()> {
    fs::create_dir_all(dir)?;
    let line = |s: &str| s.replace(['\r', '\n'], " ");
    fs::write(dir.join("steam_appid.txt"), profile.appid.to_string())?;
    fs::write(
        dir.join("configs.user.ini"),
        format!(
            "[user::general]\naccount_name={}\naccount_steamid={}\nlanguage={}\n",
            line(&profile.persona),
            profile.steam_id,
            line(&profile.language)
        ),
    )?;
    // Only DLC the account owns.
    let mut app = String::from("[app::dlcs]\nunlock_all=0\n");
    for (id, name) in &profile.dlcs {
        app.push_str(&format!("{id}={}\n", line(name)));
    }
    fs::write(dir.join("configs.app.ini"), app)?;
    let depots: Vec<String> = profile.depots.iter().map(u32::to_string).collect();
    fs::write(dir.join("depots.txt"), depots.join("\n") + "\n")?;
    if !interfaces.is_empty() {
        fs::write(
            dir.join("steam_interfaces.txt"),
            interfaces.join("\n") + "\n",
        )?;
    }
    Ok(())
}

/// gbe_fork's libraries for one platform, downloaded once and cached.
/// `FUMES_GBE_DIR` points at an extracted release folder instead (the one
/// holding `regular/`), for pinning a version or working offline.
pub async fn release(dirs: &Dirs, os: &str, refresh: bool) -> Result<(String, PathBuf)> {
    if let Some(dir) = std::env::var_os("FUMES_GBE_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.join("regular").is_dir() {
            bail!("FUMES_GBE_DIR={} has no regular/ folder", dir.display());
        }
        return Ok(("FUMES_GBE_DIR".into(), dir));
    }
    let root = dirs.cache.join("gbe");
    if !refresh && let Some(found) = cached_release(&root, os) {
        return Ok(found);
    }

    let http = reqwest::Client::builder().user_agent("fumes").build()?;
    let latest: GithubRelease = http
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("asking GitHub for the latest gbe_fork release")?
        .json()
        .await?;
    let asset = pick_asset(&latest.assets, os)
        .with_context(|| format!("gbe_fork {} has no {os} build", latest.tag_name))?;
    println!("Downloading gbe_fork {} ({})…", latest.tag_name, asset.name);
    let bytes = http
        .get(&asset.browser_download_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())?
        .bytes()
        .await?;
    if let Some(expected) = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        let actual = hex::encode(sha2::Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("{} doesn't match GitHub's checksum", asset.name);
        }
    }

    let dest = root.join(&latest.tag_name).join(os);
    extract(&bytes, os, &dest).with_context(|| format!("unpacking {}", asset.name))?;
    Ok((latest.tag_name, dest))
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

fn pick_asset<'a>(assets: &'a [GithubAsset], os: &str) -> Option<&'a GithubAsset> {
    let mut candidates: Vec<&GithubAsset> = assets
        .iter()
        .filter(|a| match os {
            "windows" => a.name.starts_with("emu-win-release") && a.name.ends_with(".7z"),
            _ => a.name.starts_with("emu-linux-release") && a.name.ends_with(".tar.bz2"),
        })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    candidates.into_iter().next()
}

/// The newest release already unpacked for this platform. Tags are dated
/// (`release-2026_09_27`), so they sort by name.
fn cached_release(root: &Path, os: &str) -> Option<(String, PathBuf)> {
    let mut tags: Vec<String> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|tag| root.join(tag).join(os).join("regular/x64").is_dir())
        .collect();
    tags.sort();
    let tag = tags.pop()?;
    let dir = root.join(&tag).join(os);
    Some((tag, dir))
}

/// The files fumes needs from a release archive, relative to its `release/`
/// folder.
fn wanted(os: &str) -> &'static [&'static str] {
    match os {
        "windows" => &["regular/x64/steam_api64.dll", "regular/x86/steam_api.dll"],
        _ => &["regular/x64/libsteam_api.so", "regular/x86/libsteam_api.so"],
    }
}

fn extract(bytes: &[u8], os: &str, dest: &Path) -> Result<()> {
    let mut files: Vec<(&str, Vec<u8>)> = Vec::new();
    if os == "windows" {
        let mut archive =
            sevenz_rust2::ArchiveReader::new(Cursor::new(bytes), sevenz_rust2::Password::empty())?;
        for name in wanted(os) {
            files.push((name, archive.read_file(&format!("release/{name}"))?));
        }
    } else {
        let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(bytes));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_string_lossy().into_owned();
            if let Some(name) = wanted(os).iter().find(|n| path == format!("release/{n}")) {
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                files.push((name, data));
            }
        }
    }
    for name in wanted(os) {
        let Some((_, data)) = files.iter().find(|(n, _)| n == name) else {
            bail!("release is missing {name}");
        };
        let path = dest.join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE: DOS header pointing at a COFF header with the given
    /// machine and section names.
    fn pe(machine: u16, sections: &[&str]) -> Vec<u8> {
        let mut b = vec![0u8; 0x80];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b.extend_from_slice(b"PE\0\0");
        let mut coff = [0u8; 20];
        coff[..2].copy_from_slice(&machine.to_le_bytes());
        coff[2..4].copy_from_slice(&(sections.len() as u16).to_le_bytes());
        coff[16..18].copy_from_slice(&16u16.to_le_bytes()); // optional header size
        b.extend_from_slice(&coff);
        b.extend_from_slice(&[0u8; 16]);
        for name in sections {
            let mut s = [0u8; 40];
            s[..name.len()].copy_from_slice(name.as_bytes());
            b.extend_from_slice(&s);
        }
        b
    }

    fn profile() -> Profile {
        Profile {
            appid: 4000,
            persona: "Orca\nInjected=1".into(),
            steam_id: 76561197960287930,
            language: "english".into(),
            dlcs: BTreeMap::from([(4010, "Soundtrack".into())]),
            depots: vec![4001, 4002],
        }
    }

    #[test]
    fn reads_architecture_and_steamstub() {
        assert_eq!(arch_of(&pe(0x8664, &[".text"])), Some(Arch::X64));
        assert_eq!(arch_of(&pe(0x014c, &[".text"])), Some(Arch::X86));
        assert_eq!(arch_of(b"\x7fELF\x02rest"), Some(Arch::X64));
        assert_eq!(arch_of(b"\x7fELF\x01rest"), Some(Arch::X86));
        assert_eq!(arch_of(b"not a binary"), None);
        assert!(has_steamstub(&pe(0x8664, &[".text", ".rdata", ".bind"])));
        assert!(!has_steamstub(&pe(0x8664, &[".text", ".rdata"])));
        assert!(!has_steamstub(b"MZ"));
    }

    #[test]
    fn finds_interfaces_like_generate_interfaces() {
        let lib = b"\0SteamClient017\0SteamClient020\0SteamUser023\0SteamUserStats\0\
                    STEAMUSERSTATS_INTERFACE_VERSION012\0SteamNetworkingSockets012\0\
                    SteamUser023\0STEAMCONTROLLER_INTERFACE_VERSION\0";
        assert_eq!(
            interfaces(lib),
            [
                "SteamClient017",
                "STEAMCONTROLLER_INTERFACE_VERSION",
                "SteamNetworkingSockets012",
                "SteamUser023",
                "STEAMUSERSTATS_INTERFACE_VERSION012",
            ]
        );
    }

    #[test]
    fn enable_and_disable_round_trip() {
        let game = tempfile::tempdir().unwrap();
        let release = tempfile::tempdir().unwrap();
        let x64 = release.path().join("regular/x64");
        let x86 = release.path().join("regular/x86");
        fs::create_dir_all(&x64).unwrap();
        fs::create_dir_all(&x86).unwrap();
        let mut emu64 = pe(0x8664, &[".text"]);
        emu64.extend_from_slice(b"gbe x64");
        let mut emu32 = pe(0x014c, &[".text"]);
        emu32.extend_from_slice(b"gbe x86");
        fs::write(x64.join("steam_api64.dll"), &emu64).unwrap();
        fs::write(x86.join("steam_api.dll"), &emu32).unwrap();

        let mut orig64 = pe(0x8664, &[".text"]);
        orig64.extend_from_slice(b"\0SteamUser023\0SteamFriends017\0");
        let orig32 = pe(0x014c, &[".text"]);
        fs::create_dir_all(game.path().join("bin/win64")).unwrap();
        fs::create_dir_all(game.path().join("launcher")).unwrap();
        fs::write(game.path().join("bin/win64/steam_api64.dll"), &orig64).unwrap();
        fs::write(game.path().join("launcher/steam_api.dll"), &orig32).unwrap();

        let dirs = Dirs::for_tests(tempfile::tempdir().unwrap().keep());
        // SAFETY: tests touching FUMES_GBE_DIR run in this one test only.
        unsafe { std::env::set_var("FUMES_GBE_DIR", release.path()) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let report = rt
            .block_on(enable(&dirs, game.path(), &profile(), &[]))
            .unwrap();
        assert_eq!(report.replaced.len(), 2);
        assert!(is_enabled(game.path()));

        let lib64 = game.path().join("bin/win64/steam_api64.dll");
        assert_eq!(fs::read(&lib64).unwrap(), emu64);
        assert_eq!(
            fs::read(game.path().join("launcher/steam_api.dll")).unwrap(),
            emu32
        );
        assert_eq!(fs::read(backup_path(&lib64)).unwrap(), orig64);

        let settings = game.path().join("bin/win64/steam_settings");
        assert_eq!(
            fs::read_to_string(settings.join("steam_appid.txt")).unwrap(),
            "4000"
        );
        let user = fs::read_to_string(settings.join("configs.user.ini")).unwrap();
        // A newline in the persona can't smuggle in another setting.
        assert!(user.contains("account_name=Orca Injected=1\n"), "{user}");
        assert!(user.contains("account_steamid=76561197960287930\n"));
        let app = fs::read_to_string(settings.join("configs.app.ini")).unwrap();
        assert!(app.contains("unlock_all=0\n4010=Soundtrack\n"), "{app}");
        assert_eq!(
            fs::read_to_string(settings.join("steam_interfaces.txt")).unwrap(),
            "SteamFriends017\nSteamUser023\n"
        );

        // Enabling again doesn't back up the emulator over the original.
        rt.block_on(enable(&dirs, game.path(), &profile(), &[]))
            .unwrap();
        assert_eq!(fs::read(backup_path(&lib64)).unwrap(), orig64);

        assert_eq!(disable(game.path()).unwrap(), 2);
        assert_eq!(fs::read(&lib64).unwrap(), orig64);
        assert!(!backup_path(&lib64).exists());
        assert!(!settings.exists());
        assert!(!is_enabled(game.path()));
        unsafe { std::env::remove_var("FUMES_GBE_DIR") };
    }

    #[test]
    fn mac_builds_are_refused_with_a_way_forward() {
        let game = tempfile::tempdir().unwrap();
        let frameworks = game.path().join("Game.app/Contents/Frameworks");
        fs::create_dir_all(&frameworks).unwrap();
        fs::write(frameworks.join("libsteam_api.dylib"), b"\xcf\xfa\xed\xfe").unwrap();
        let dirs = Dirs::for_tests(tempfile::tempdir().unwrap().keep());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(enable(&dirs, game.path(), &profile(), &[]))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("--os windows"), "{err}");
    }

    #[test]
    fn picks_release_assets() {
        let asset = |name: &str| GithubAsset {
            name: name.into(),
            browser_download_url: String::new(),
            digest: None,
        };
        let assets = [
            asset("emu-linux-debug.tar.bz2"),
            asset("emu-linux-release.tar.bz2"),
            asset("emu-win-debug-vs22.7z"),
            asset("emu-win-release-vs26.7z"),
            asset("emu-win-release-vs22.7z"),
            asset("migrate_gse-win.7z"),
        ];
        assert_eq!(
            pick_asset(&assets, "windows").unwrap().name,
            "emu-win-release-vs22.7z"
        );
        assert_eq!(
            pick_asset(&assets, "linux").unwrap().name,
            "emu-linux-release.tar.bz2"
        );
    }

    /// Needs real release archives: `FUMES_GBE_ARCHIVES=<folder with
    /// emu-win-release-vs22.7z and emu-linux-release.tar.bz2> cargo test
    /// -- --ignored unpacks_real_releases`.
    #[test]
    #[ignore]
    fn unpacks_real_releases() {
        let Some(archives) = std::env::var_os("FUMES_GBE_ARCHIVES").map(PathBuf::from) else {
            eprintln!("FUMES_GBE_ARCHIVES not set; skipping");
            return;
        };
        for (os, file) in [
            ("windows", "emu-win-release-vs22.7z"),
            ("linux", "emu-linux-release.tar.bz2"),
        ] {
            let dest = tempfile::tempdir().unwrap();
            extract(&fs::read(archives.join(file)).unwrap(), os, dest.path()).unwrap();
            for name in wanted(os) {
                let lib = fs::read(dest.path().join(name)).unwrap();
                let expected = if name.contains("x64") {
                    Arch::X64
                } else {
                    Arch::X86
                };
                assert_eq!(arch_of(&lib), Some(expected), "{name}");
            }
        }
    }
}
