//! Where fumes keeps its own files (the platform's config/cache folders).

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Clone)]
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

    /// Games marked as favorites in the UI.
    pub fn favorites_file(&self) -> PathBuf {
        self.data.join("favorites.json")
    }

    /// The Steam engine fumes runs games with, per client build.
    pub fn engine(&self) -> PathBuf {
        self.data.join("engine")
    }

    /// Where new games go, laid out like a Steam library
    /// (`steamapps/common/<game>`). `FUMES_LIBRARY` overrides it.
    pub fn library(&self) -> PathBuf {
        std::env::var_os("FUMES_LIBRARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.data.join("library"))
    }
}
