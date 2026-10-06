//! Where fumes keeps its own files (the platform's config/cache folders).

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

pub struct Dirs {
    /// Session and machine tokens, the install list, and by default the
    /// games themselves.
    pub data: PathBuf,
    /// The last library fetched from Steam, so listing works offline.
    pub cache: PathBuf,
}

impl Dirs {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "fumes").context("no home directory")?;
        Ok(Dirs {
            data: dirs.data_dir().to_owned(),
            cache: dirs.cache_dir().to_owned(),
        })
    }

    pub fn session_file(&self) -> PathBuf {
        self.data.join("session.json")
    }

    pub fn library_cache(&self) -> PathBuf {
        self.cache.join("library.json")
    }

    /// Games fumes installed itself.
    pub fn installs_file(&self) -> PathBuf {
        self.data.join("installs.json")
    }

    /// Where new games go, one folder each. `FUMES_LIBRARY` overrides it.
    pub fn library(&self) -> PathBuf {
        std::env::var_os("FUMES_LIBRARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.data.join("library"))
    }

    #[cfg(test)]
    pub fn for_tests(root: PathBuf) -> Self {
        Dirs {
            data: root.join("data"),
            cache: root.join("cache"),
        }
    }
}
