//! Games fumes downloaded itself, kept in `installs.json` in the data
//! folder. Steam's own installs are read from its files instead (`local`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::appinfo::Launch;
use crate::dirs::Dirs;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Install {
    pub appid: u32,
    pub name: String,
    pub path: PathBuf,
    /// The platform whose build was downloaded (`windows`, `macos`, `linux`).
    pub os: String,
    pub arch: String,
    pub language: String,
    pub buildid: u32,
    /// Depot → manifest that's on disk.
    pub depots: BTreeMap<u32, u64>,
    pub size: u64,
    pub launch: Vec<Launch>,
    /// Owned DLC, for the emulator's config.
    #[serde(default)]
    pub dlcs: BTreeMap<u32, String>,
}

pub type Installs = BTreeMap<u32, Install>;

pub fn load(dirs: &Dirs) -> Installs {
    load_from(&dirs.installs_file())
}

fn load_from(path: &Path) -> Installs {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(dirs: &Dirs, installs: &Installs) -> Result<()> {
    let path = dirs.installs_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(installs)?)?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

pub fn put(dirs: &Dirs, install: Install) -> Result<()> {
    let mut installs = load(dirs);
    installs.insert(install.appid, install);
    save(dirs, &installs)
}

pub fn remove(dirs: &Dirs, appid: u32) -> Result<Option<Install>> {
    let mut installs = load(dirs);
    let removed = installs.remove(&appid);
    save(dirs, &installs)?;
    Ok(removed)
}
