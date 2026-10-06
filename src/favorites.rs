//! Games marked as favorites, kept in `favorites.json` in the data folder.
//! Local to this machine; Steam's own collections aren't touched.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::dirs::Dirs;

pub type Favorites = BTreeSet<u32>;

pub fn load(dirs: &Dirs) -> Favorites {
    load_from(&dirs.favorites_file())
}

fn load_from(path: &Path) -> Favorites {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(dirs: &Dirs, favorites: &Favorites) -> Result<()> {
    save_to(&dirs.favorites_file(), favorites)
}

fn save_to(path: &Path, favorites: &Favorites) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(favorites)?)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("favorites.json");
        assert!(load_from(&path).is_empty());
        let favorites = Favorites::from([440, 12120]);
        save_to(&path, &favorites).unwrap();
        assert_eq!(load_from(&path), favorites);
    }
}
