//! Favorites marked in the UI: games (by app id) in `favorites.json` and
//! friends (by SteamID) in `favorite_friends.json`, in the data folder.
//! Local to this machine; Steam's own collections aren't touched.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn load<T: DeserializeOwned + Ord>(path: &Path) -> BTreeSet<T> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save<T: Serialize>(path: &Path, favorites: &BTreeSet<T>) -> Result<()> {
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
        assert!(load::<u32>(&path).is_empty());
        let favorites = BTreeSet::from([440u32, 12120]);
        save(&path, &favorites).unwrap();
        assert_eq!(load(&path), favorites);
    }
}
