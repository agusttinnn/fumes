//! Installing a game straight from Steam's content servers.
//!
//! For each depot that applies to this machine (platform, language, DLC
//! the account owns): get its key and manifest, then fetch only the chunks
//! that aren't already on disk. Existing files are checked chunk by chunk
//! against the manifest's hashes, so the same code installs, resumes an
//! interrupted install, repairs, and updates.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use steam_vent::Connection;

use crate::appinfo::{self, AppInfo, Depot};
use crate::cdn::{self, Cdn, Chunk, DepotKey, FileEntry, Manifest, flags};
use crate::installs::Install;

/// Bitness of the builds to fetch. Every platform fumes runs on is 64-bit.
const ARCH: &str = "64";
/// Chunks in flight at once.
const PARALLEL: usize = 16;

pub struct Options {
    /// Platform build to fetch; defaults to this machine's if the game has one.
    pub os: Option<String>,
    pub language: String,
    pub dir: PathBuf,
    /// Re-hash every file instead of trusting the last install's record.
    pub verify: bool,
}

/// Download (or update, or repair) a game into `opts.dir`.
pub async fn install(connection: &Connection, appid: u32, opts: &Options) -> Result<Install> {
    if !appinfo::owns(connection, appid).await? {
        bail!("this account doesn't own app {appid}");
    }
    let info = appinfo::fetch(connection, appid).await?;
    let os = pick_os(&info.oslist, opts.os.as_deref(), host_os())?;

    let owned_dlcs = appinfo::owned_of(connection, &info.dlcs).await?;
    let dlc_names = appinfo::names(connection, &owned_dlcs)
        .await
        .unwrap_or_default();
    let depots = select_depots(&info.depots, &os, ARCH, &opts.language, &owned_dlcs);
    if depots.is_empty() {
        bail!("{} has no downloadable {os} content", info.name);
    }

    println!("Preparing {} ({os}, {} depots)…", info.name, depots.len());
    let cdn = Arc::new(Cdn::discover(connection, appid).await?);
    let mut manifests: Vec<(Manifest, DepotKey)> = Vec::new();
    for depot in &depots {
        match fetch_depot(connection, &cdn, &info, depot).await {
            Ok(m) => manifests.push(m),
            // A DLC depot Steam won't serve shouldn't sink the whole game.
            Err(e) if depot.dlcappid.is_some() => {
                eprintln!("warning: skipping DLC depot {}: {e:#}", depot.id)
            }
            Err(e) => return Err(e.context(format!("depot {}", depot.id))),
        }
    }

    fs::create_dir_all(&opts.dir).with_context(|| format!("creating {}", opts.dir.display()))?;
    let state_path = opts.dir.join(".fumes/state.json");
    let previous = State::load(&state_path);

    let files = merge(&manifests);
    let size: u64 = files.values().map(|(f, _)| f.size).sum();

    let keys: HashMap<u32, DepotKey> = manifests.iter().map(|(m, k)| (m.depot, *k)).collect();
    let dir = opts.dir.clone();
    let files_for_plan = files.clone();
    let prev_files: BTreeMap<String, String> = if opts.verify {
        BTreeMap::new()
    } else {
        previous
            .files
            .iter()
            .map(|(n, f)| (n.clone(), f.sha.clone()))
            .collect()
    };
    let plan = tokio::task::spawn_blocking(move || plan(&dir, &files_for_plan, &keys, &prev_files))
        .await??;

    let needed: u64 = plan.jobs.iter().map(|j| j.chunk.size as u64).sum();
    if plan.jobs.is_empty() {
        println!("All files are up to date.");
    } else {
        println!(
            "Downloading {} ({} chunks)…",
            human(needed),
            plan.jobs.len()
        );
        download(cdn, plan.jobs, needed).await?;
    }

    finish(&opts.dir, &files, &previous)?;
    let state = State {
        files: files
            .iter()
            .map(|(name, (f, depot))| {
                let entry = FileState {
                    sha: hex::encode(&f.sha),
                    depot: *depot,
                };
                (name.clone(), entry)
            })
            .collect(),
    };
    state.save(&state_path)?;

    Ok(Install {
        appid,
        name: info.name.clone(),
        path: opts.dir.clone(),
        os,
        arch: ARCH.into(),
        language: opts.language.clone(),
        buildid: info.buildid,
        depots: manifests.iter().map(|(m, _)| (m.depot, m.gid)).collect(),
        size,
        launch: info.launch.clone(),
        dlcs: owned_dlcs
            .iter()
            .map(|id| {
                let name = dlc_names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("DLC {id}"));
                (*id, name)
            })
            .collect(),
    })
}

pub fn host_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// The requested platform, else this machine's, else Windows (the build
/// nearly every game has, runnable elsewhere through Wine).
pub fn pick_os(oslist: &[String], requested: Option<&str>, host: &str) -> Result<String> {
    let supports = |os: &str| oslist.is_empty() || oslist.iter().any(|o| o == os);
    if let Some(os) = requested {
        if !matches!(os, "windows" | "macos" | "linux") {
            bail!("unknown platform {os:?}; use windows, macos or linux");
        }
        if !supports(os) {
            bail!("no {os} build; this game is for {}", oslist.join(", "));
        }
        return Ok(os.to_owned());
    }
    for os in [host, "windows"] {
        if supports(os) {
            return Ok(os.to_owned());
        }
    }
    bail!(
        "no build for {host} or windows; this game is for {}",
        oslist.join(", ")
    )
}

/// The depots Steam would install for this platform, bitness and language.
pub fn select_depots<'a>(
    depots: &'a [Depot],
    os: &str,
    arch: &str,
    language: &str,
    owned_dlcs: &[u32],
) -> Vec<&'a Depot> {
    let candidates: Vec<&Depot> = depots
        .iter()
        .filter(|d| d.manifest.is_some() || (d.depotfromapp.is_some() && !d.sharedinstall))
        .filter(|d| !d.sharedinstall)
        .filter(|d| d.oslist.is_empty() || d.oslist.iter().any(|o| o == os))
        .filter(|d| {
            d.language
                .as_deref()
                .is_none_or(|l| l.eq_ignore_ascii_case(language))
        })
        .filter(|d| !d.lowviolence)
        .filter(|d| d.dlcappid.is_none_or(|dlc| owned_dlcs.contains(&dlc)))
        .collect();
    // When a game has builds for both bitnesses, take ours; when it only
    // has 32-bit depots, those are what runs.
    let has_ours = candidates.iter().any(|d| d.osarch.as_deref() == Some(arch));
    candidates
        .into_iter()
        .filter(|d| !has_ours || d.osarch.as_deref().is_none_or(|a| a == arch))
        .collect()
}

async fn fetch_depot(
    connection: &Connection,
    cdn: &Cdn,
    info: &AppInfo,
    depot: &Depot,
) -> Result<(Manifest, DepotKey)> {
    let (manifest_app, gid) = match (depot.manifest, depot.depotfromapp) {
        (Some(gid), _) => (info.appid, gid),
        (None, Some(other)) => {
            let other_info = appinfo::fetch(connection, other).await?;
            let gid = other_info
                .depots
                .iter()
                .find(|d| d.id == depot.id)
                .and_then(|d| d.manifest)
                .with_context(|| format!("app {other} has no public manifest for it"))?;
            (other, gid)
        }
        (None, None) => bail!("no public manifest"),
    };
    let key = cdn::depot_key(connection, info.appid, depot.id).await?;
    let code = cdn::manifest_request_code(connection, manifest_app, depot.id, gid).await?;
    let manifest = cdn.manifest(depot.id, gid, code, &key).await?;
    Ok((manifest, key))
}

/// path → (file, depot). Depots are layered in id order, so a language
/// depot's copy of a file wins over the base depot's, as in Steam.
type Files = BTreeMap<String, (FileEntry, u32)>;

fn merge(manifests: &[(Manifest, DepotKey)]) -> Files {
    let mut ordered: Vec<&Manifest> = manifests.iter().map(|(m, _)| m).collect();
    ordered.sort_by_key(|m| m.depot);
    let mut files = Files::new();
    for manifest in ordered {
        for file in &manifest.files {
            files.insert(file.name.clone(), (file.clone(), manifest.depot));
        }
    }
    files
}

struct Job {
    depot: u32,
    key: DepotKey,
    chunk: Chunk,
    /// Every (file, offset) holding this chunk: identical chunks are
    /// fetched once.
    targets: Vec<(PathBuf, u64)>,
}

struct Plan {
    jobs: Vec<Job>,
}

/// Create folders and size files, and work out which chunks are missing.
fn plan(
    dir: &Path,
    files: &Files,
    keys: &HashMap<u32, DepotKey>,
    previous: &BTreeMap<String, String>,
) -> Result<Plan> {
    let mut jobs: HashMap<(u32, Vec<u8>), Job> = HashMap::new();
    for (name, (file, depot)) in files {
        let path = dir.join(name);
        if file.flags & flags::DIRECTORY != 0 {
            fs::create_dir_all(&path)?;
            continue;
        }
        if file.flags & flags::SYMLINK != 0 {
            continue; // made in `finish`, once their targets exist
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let existing = fs::metadata(&path).ok().filter(|m| m.is_file());
        let unchanged = previous.get(name) == Some(&hex::encode(&file.sha))
            && existing.as_ref().is_some_and(|m| m.len() == file.size);
        if unchanged {
            continue;
        }

        let mut handle = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        handle.set_len(file.size)?;
        let had_data = existing.is_some_and(|m| m.len() > 0);
        for chunk in &file.chunks {
            if had_data && chunk_on_disk(&mut handle, chunk)? {
                continue;
            }
            jobs.entry((*depot, chunk.id.clone()))
                .or_insert_with(|| Job {
                    depot: *depot,
                    key: keys[depot],
                    chunk: chunk.clone(),
                    targets: Vec::new(),
                })
                .targets
                .push((path.clone(), chunk.offset));
        }
    }
    let mut jobs: Vec<Job> = jobs.into_values().collect();
    // Roughly file order, which keeps writes local.
    jobs.sort_by(|a, b| a.targets[0].cmp(&b.targets[0]));
    Ok(Plan { jobs })
}

fn chunk_on_disk(file: &mut File, chunk: &Chunk) -> Result<bool> {
    let mut buf = vec![0; chunk.size as usize];
    file.seek(SeekFrom::Start(chunk.offset))?;
    match file.read_exact(&mut buf) {
        Ok(()) => Ok(cdn::sha1(&buf)[..] == chunk.id[..]),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn download(cdn: Arc<Cdn>, jobs: Vec<Job>, total: u64) -> Result<()> {
    let done = Arc::new(AtomicU64::new(0));
    let progress = tokio::spawn(report_progress(done.clone(), total));
    let result = stream::iter(jobs)
        .map(|job| {
            let cdn = cdn.clone();
            let done = done.clone();
            async move {
                let data = cdn.chunk(job.depot, &job.chunk, &job.key).await?;
                let size = data.len() as u64;
                tokio::task::spawn_blocking(move || write_targets(&data, &job.targets)).await??;
                done.fetch_add(size, Ordering::Relaxed);
                Ok::<_, anyhow::Error>(())
            }
        })
        .buffer_unordered(PARALLEL)
        .try_collect::<()>()
        .await;
    progress.abort();
    eprintln!();
    result
}

/// Files are opened per write rather than held open: a game can have tens
/// of thousands of them, far past the default open-file limit.
fn write_targets(data: &[u8], targets: &[(PathBuf, u64)]) -> Result<()> {
    for (path, offset) in targets {
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.seek(SeekFrom::Start(*offset))?;
        file.write_all(data)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

async fn report_progress(done: Arc<AtomicU64>, total: u64) {
    let start = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let bytes = done.load(Ordering::Relaxed);
        let rate = bytes as f64 / start.elapsed().as_secs_f64().max(0.001);
        eprint!(
            "\r  {:5.1}%  {} / {}  {}/s   ",
            bytes as f64 * 100.0 / total.max(1) as f64,
            human(bytes),
            human(total),
            human(rate as u64)
        );
        let _ = io::stderr().flush();
    }
}

/// Symlinks and permissions, then remove files the new version dropped.
/// Only files of depots fetched this time are candidates: a depot that's
/// no longer selected (or a DLC Steam didn't serve) keeps its files.
fn finish(dir: &Path, files: &Files, previous: &State) -> Result<()> {
    let fetched: std::collections::HashSet<u32> = files.values().map(|(_, d)| *d).collect();
    for (name, (file, _)) in files {
        if file.flags & flags::SYMLINK != 0 {
            make_symlink(dir, name, file.link_target.as_deref().unwrap_or_default())?;
        } else if file.flags & flags::EXECUTABLE != 0 {
            make_executable(&dir.join(name))?;
        }
    }
    for (name, old) in &previous.files {
        if fetched.contains(&old.depot) && !files.contains_key(name) {
            let _ = fs::remove_file(dir.join(name));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_symlink(dir: &Path, name: &str, target: &str) -> Result<()> {
    let target = target.replace('\\', "/");
    if !stays_inside(name, &target) {
        bail!("symlink {name} points outside the game");
    }
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&path);
    std::os::unix::fs::symlink(&target, &path)
        .with_context(|| format!("creating symlink {}", path.display()))
}

#[cfg(not(unix))]
fn make_symlink(_dir: &Path, name: &str, _target: &str) -> Result<()> {
    eprintln!("warning: skipping symlink {name} (unsupported here)");
    Ok(())
}

/// Whether a link at `name` (relative to the game folder) pointing at
/// `target` resolves to somewhere inside the game folder.
fn stays_inside(name: &str, target: &str) -> bool {
    if target.starts_with('/') {
        return false;
    }
    // Start in the link's own folder.
    let mut depth = name.split('/').count() as i64 - 1;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => depth -= 1,
            _ => depth += 1,
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// What a finished install left on disk. Lets an update skip files that
/// didn't change without rehashing them, and know what it may delete.
#[derive(Default, Serialize, Deserialize)]
struct State {
    files: BTreeMap<String, FileState>,
}

#[derive(Clone, Serialize, Deserialize)]
struct FileState {
    /// Content SHA-1, hex.
    sha: String,
    depot: u32,
}

impl State {
    fn load(path: &Path) -> State {
        fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec(self)?)?;
        Ok(())
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depot(id: u32) -> Depot {
        Depot {
            id,
            name: String::new(),
            oslist: vec![],
            osarch: None,
            language: None,
            lowviolence: false,
            manifest: Some(id as u64 * 10),
            size: 0,
            dlcappid: None,
            depotfromapp: None,
            sharedinstall: false,
        }
    }

    fn ids(depots: &[&Depot]) -> Vec<u32> {
        depots.iter().map(|d| d.id).collect()
    }

    #[test]
    fn selects_depots_like_steam() {
        let mut win64 = depot(2);
        win64.oslist = vec!["windows".into()];
        win64.osarch = Some("64".into());
        let mut win32 = depot(3);
        win32.oslist = vec!["windows".into()];
        win32.osarch = Some("32".into());
        let mut mac = depot(4);
        mac.oslist = vec!["macos".into()];
        let mut german = depot(5);
        german.language = Some("german".into());
        let mut lowviolence = depot(6);
        lowviolence.lowviolence = true;
        let mut owned_dlc = depot(7);
        owned_dlc.dlcappid = Some(70);
        let mut other_dlc = depot(8);
        other_dlc.dlcappid = Some(80);
        let mut redist = depot(9);
        redist.sharedinstall = true;
        let mut no_manifest = depot(10);
        no_manifest.manifest = None;
        let all = [
            depot(1),
            win64,
            win32,
            mac,
            german,
            lowviolence,
            owned_dlc,
            other_dlc,
            redist,
            no_manifest,
        ];

        assert_eq!(
            ids(&select_depots(&all, "windows", "64", "english", &[70])),
            [1, 2, 7]
        );
        assert_eq!(
            ids(&select_depots(&all, "macos", "64", "german", &[])),
            [1, 4, 5]
        );
    }

    #[test]
    fn falls_back_to_32_bit_when_that_is_all_there_is() {
        let mut win32 = depot(2);
        win32.osarch = Some("32".into());
        let all = [depot(1), win32];
        assert_eq!(
            ids(&select_depots(&all, "windows", "64", "english", &[])),
            [1, 2]
        );
    }

    #[test]
    fn picks_platform() {
        let all = vec!["windows".to_string(), "macos".to_string()];
        let win = vec!["windows".to_string()];
        assert_eq!(pick_os(&all, None, "macos").unwrap(), "macos");
        assert_eq!(pick_os(&win, None, "macos").unwrap(), "windows");
        assert_eq!(pick_os(&[], None, "linux").unwrap(), "linux");
        assert_eq!(pick_os(&all, Some("windows"), "macos").unwrap(), "windows");
        assert!(pick_os(&win, Some("linux"), "macos").is_err());
        assert!(pick_os(&["linux".to_string()], None, "macos").is_err());
        assert!(pick_os(&all, Some("amiga"), "macos").is_err());
    }

    fn entry(name: &str, data: &[u8], chunk_size: usize) -> FileEntry {
        let chunks = data
            .chunks(chunk_size)
            .enumerate()
            .map(|(i, c)| Chunk {
                id: cdn::sha1(c).to_vec(),
                checksum: cdn::steam_adler32(c),
                offset: (i * chunk_size) as u64,
                size: c.len() as u32,
            })
            .collect();
        FileEntry {
            name: name.into(),
            size: data.len() as u64,
            flags: 0,
            sha: cdn::sha1(data).to_vec(),
            chunks,
            link_target: None,
        }
    }

    #[test]
    fn plans_only_missing_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let new_a: Vec<u8> = (0..30u8).collect();
        let b = b"unchanged file".to_vec();
        let mut files = Files::new();
        files.insert("a.bin".into(), (entry("a.bin", &new_a, 10), 1));
        files.insert("sub/b.txt".into(), (entry("sub/b.txt", &b, 10), 1));
        let mut dir_entry = entry("empty", b"", 10);
        dir_entry.flags = flags::DIRECTORY;
        files.insert("empty".into(), (dir_entry, 1));
        let keys = HashMap::from([(1, [0u8; 32])]);

        // Fresh install: every chunk, folders made, files sized.
        let fresh = plan(dir.path(), &files, &keys, &BTreeMap::new()).unwrap();
        assert_eq!(fresh.jobs.len(), 5);
        assert!(dir.path().join("empty").is_dir());
        assert_eq!(fs::metadata(dir.path().join("a.bin")).unwrap().len(), 30);

        // Interrupted: the first chunk of a.bin and all of b.txt landed,
        // the middle of a.bin is garbage.
        let mut partial = new_a.clone();
        partial[15] ^= 0xFF;
        partial[25..].fill(0);
        fs::write(dir.path().join("a.bin"), &partial).unwrap();
        fs::write(dir.path().join("sub/b.txt"), &b).unwrap();
        let resumed = plan(dir.path(), &files, &keys, &BTreeMap::new()).unwrap();
        let offsets: Vec<u64> = resumed.jobs.iter().map(|j| j.targets[0].1).collect();
        assert_eq!(offsets, [10, 20]);

        // Recorded as installed with the same hash: not even read.
        let previous = BTreeMap::from([("sub/b.txt".to_string(), hex::encode(cdn::sha1(&b)))]);
        fs::write(dir.path().join("a.bin"), &new_a).unwrap();
        assert!(
            plan(dir.path(), &files, &keys, &previous)
                .unwrap()
                .jobs
                .is_empty()
        );
    }

    #[test]
    fn identical_chunks_are_fetched_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![7u8; 20];
        let mut files = Files::new();
        files.insert("x".into(), (entry("x", &data, 10), 1));
        files.insert("y".into(), (entry("y", &data, 10), 1));
        let keys = HashMap::from([(1, [0u8; 32])]);
        let plan = plan(dir.path(), &files, &keys, &BTreeMap::new()).unwrap();
        assert_eq!(plan.jobs.len(), 1);
        assert_eq!(plan.jobs[0].targets.len(), 4);
    }

    #[test]
    fn writes_chunks_at_their_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, vec![0u8; 6]).unwrap();
        write_targets(b"ab", &[(path.clone(), 0), (path.clone(), 4)]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"ab\0\0ab");
    }

    #[test]
    fn symlinks_must_stay_in_the_game() {
        assert!(stays_inside("link", "bin/game"));
        assert!(stays_inside("a/b/link", "../c"));
        assert!(stays_inside(
            "Game.app/Contents/Frameworks/X.framework/X",
            "Versions/Current/X"
        ));
        assert!(!stays_inside("link", "../outside"));
        assert!(!stays_inside("a/link", "../../outside"));
        assert!(!stays_inside("link", "/etc/passwd"));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn removes_dropped_files_only_from_fetched_depots() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["kept", "dropped", "other_depot"] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        let old = |depot| FileState {
            sha: String::new(),
            depot,
        };
        let previous = State {
            files: BTreeMap::from([
                ("kept".into(), old(1)),
                ("dropped".into(), old(1)),
                ("other_depot".into(), old(2)),
            ]),
        };
        let mut files = Files::new();
        files.insert("kept".into(), (entry("kept", b"x", 10), 1));
        finish(dir.path(), &files, &previous).unwrap();
        assert!(dir.path().join("kept").exists());
        assert!(!dir.path().join("dropped").exists());
        // Depot 2 wasn't part of this run (refused, or deselected).
        assert!(dir.path().join("other_depot").exists());
    }
}
