//! Download only the parts of the Steam client the engine needs.
//!
//! Valve ships the client as zip packages listed in a per-platform manifest
//! (`client-update.steamstatic.com/steam_client_<platform>`). The engine is
//! one library, `steamclient`, plus a few small Valve libraries it links;
//! everything else, Chromium and the UI included, is skipped. Packages come
//! from the manifests pinned in `manifests/` (client build 1788652215), so
//! the engine's internal layout matches the interface slots in `engine.rs`.
//! Valve keeps old packages on its CDN, so a pinned build stays fetchable.
//!
//! fumes hosts the 64-bit engine on every platform (on Linux the one Steam
//! ships for 64-bit games, as OpenSteamClient does) and also takes the
//! 32-bit client library 32-bit games load to talk to it.

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::Digest;

const CDN: &str = "https://client-update.steamstatic.com";

/// (package, files to take from it). `None` takes the whole package.
type Wanted = &'static [(&'static str, Option<&'static [&'static str]>)];

pub struct Platform {
    pub name: &'static str,
    manifest: &'static str,
    packages: Wanted,
    /// The engine library, relative to the runtime folder.
    pub engine: &'static str,
    /// Files Valve's zips don't mark executable but must be.
    executables: &'static [&'static str],
}

pub const MACOS: Platform = Platform {
    name: "macos",
    manifest: include_str!("../../manifests/steam_client_osx.vdf"),
    packages: &[
        ("bins_client_osx", Some(&["steamclient.dylib"])),
        (
            "bins_osx",
            Some(&[
                // steamclient.dylib's direct dependencies.
                "libtier0_s.dylib",
                "libvstdlib_s.dylib",
                "libaudio.dylib",
                // Referenced by the engine for its IPC service.
                "steamservice.dylib",
                // libtier0_s links Valve's crash reporter, which links
                // Breakpad (next package).
                "crashhandler.dylib",
                // The broker games and the engine find each other through
                // (launchd starts it on demand; see register.rs).
                "ipcserver",
                // Loaded on demand: controllers (Steam Input) and voice
                // chat. The engine runs without them, but says so loudly.
                "libSDL3.dylib",
                "libvideo.dylib",
            ]),
        ),
        (
            "bins_codecs_osx",
            // libvideo's dependencies.
            Some(&[
                "libavcodec.62.dylib",
                "libavfilter.11.dylib",
                "libavformat.62.dylib",
                "libavutil.60.dylib",
                "libswresample.6.dylib",
                "libswscale.9.dylib",
                "libogg.0.dylib",
                "libvorbis.0.dylib",
                "libvorbisenc.2.dylib",
                "libvorbisfile.3.dylib",
            ]),
        ),
        // The whole bundle: the framework's code signature covers its
        // Info.plist, resources and symlinks, not just the binary.
        ("breakpad_osx", None),
    ],
    engine: "steamclient.dylib",
    executables: &["ipcserver"],
};

pub const LINUX: Platform = Platform {
    name: "linux",
    manifest: include_str!("../../manifests/steam_client_ubuntu12.vdf"),
    packages: &[(
        "bins_sdk_ubuntu12",
        // The 64-bit engine only links system libraries (RUNPATH $ORIGIN);
        // it loads the crash handler next to it at runtime. The 32-bit pair
        // is what 32-bit games load (via ~/.steam/sdk32) to talk to it.
        Some(&[
            "linux64/steamclient.so",
            "linux64/crashhandler.so",
            "linux32/steamclient.so",
            "linux32/crashhandler.so",
        ]),
    )],
    engine: "linux64/steamclient.so",
    executables: &[],
};

pub const WINDOWS: Platform = Platform {
    name: "windows",
    manifest: include_str!("../../manifests/steam_client_win64.vdf"),
    packages: &[(
        "bins_win64",
        Some(&[
            "steamclient64.dll",
            // Imported by steamclient64.dll.
            "tier0_s64.dll",
            "vstdlib_s64.dll",
            // Loaded at runtime for crash reports.
            "crashhandler64.dll",
            // What 32-bit games load (ActiveProcess\SteamClientDll) to
            // talk to the engine.
            "steamclient.dll",
            "tier0_s.dll",
            "vstdlib_s.dll",
            "crashhandler.dll",
        ]),
    )],
    engine: "steamclient64.dll",
    executables: &[],
};

/// The platform this build of the spike runs on.
pub fn host() -> &'static Platform {
    if cfg!(target_os = "macos") {
        &MACOS
    } else if cfg!(windows) {
        &WINDOWS
    } else {
        &LINUX
    }
}

pub struct Package {
    pub file: String,
    pub sha2: String,
}

impl Platform {
    pub fn version(&self) -> String {
        field(self.manifest, "version").unwrap_or_default()
    }

    /// One package's entry. The manifest is KeyValues with one level of
    /// package blocks; a token scan is enough.
    fn package(&self, name: &str) -> Result<Package> {
        let start = self
            .manifest
            .find(&format!("\"{name}\""))
            .with_context(|| format!("{} manifest has no {name}", self.name))?;
        let block = &self.manifest[start..];
        let block = &block[..block.find('}').unwrap_or(block.len())];
        Ok(Package {
            file: field(block, "file").context("no file")?,
            sha2: field(block, "sha2").context("no sha2")?,
        })
    }

    /// `<base>/<version>/<platform>`, so builds and platforms never mix.
    pub fn runtime_dir(&self, base: &Path) -> PathBuf {
        base.join(self.version()).join(self.name)
    }

    /// Whether the engine for this build is already downloaded.
    /// Whether every file this build needs is downloaded (so a list that
    /// grows tops up earlier downloads).
    pub fn is_fetched(&self, base: &Path) -> bool {
        let dir = self.runtime_dir(base);
        self.packages.iter().all(|(_, files)| match files {
            Some(files) => files.iter().all(|f| dir.join(f).is_file()),
            None => true,
        }) && dir.join(self.engine).is_file()
    }

    pub async fn fetch(&self, base: &Path, all: bool) -> Result<PathBuf> {
        let dest = self.runtime_dir(base);
        fs::create_dir_all(&dest)?;
        let http = reqwest::Client::builder()
            .user_agent("Valve/Steam HTTP Client 1.0")
            .build()?;
        for &(name, files) in self.packages {
            let files = if all { None } else { files };
            let pkg = self.package(name)?;
            println!("{name}: {}", pkg.file);
            let bytes = http
                .get(format!("{CDN}/{}", pkg.file))
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .with_context(|| format!("downloading {}", pkg.file))?
                .bytes()
                .await?;
            let sha = hex::encode(sha2::Sha256::digest(&bytes));
            if !sha.eq_ignore_ascii_case(&pkg.sha2) {
                bail!("{} doesn't match the manifest's sha2", pkg.file);
            }
            let taken = self.extract(&bytes, files, &dest)?;
            if let Some(files) = files
                && taken != files.len()
            {
                bail!("{} is missing some of {files:?}", pkg.file);
            }
            println!("  {taken} files");
        }
        Ok(dest)
    }

    fn extract(&self, zip: &[u8], files: Option<&[&str]>, dest: &Path) -> Result<usize> {
        let mut zip = zip::ZipArchive::new(Cursor::new(zip))?;
        let mut taken = 0;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            // Valve builds the zips on Windows: some paths use `\`.
            let name = entry.name().replace('\\', "/");
            if let Some(files) = files
                && !files.contains(&name.as_str())
            {
                continue;
            }
            if name.split('/').any(|p| p == "..") || name.starts_with('/') || name.contains(':') {
                bail!("unsafe path {name:?} in package");
            }
            let mut mode = entry.unix_mode().unwrap_or(0o644);
            // Valve's zips often carry no Unix mode at all.
            if self.executables.contains(&name.as_str()) {
                mode |= 0o755;
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let path = dest.join(&name);
            fs::create_dir_all(path.parent().unwrap())?;
            write_entry(&path, &data, mode)?;
            taken += 1;
        }
        Ok(taken)
    }
}

fn field(text: &str, key: &str) -> Option<String> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = &text[at..];
    let open = rest.find('"')? + 1;
    let close = rest[open..].find('"')? + open;
    Some(rest[open..close].to_owned())
}

const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

/// Files keep their executable bits; symlinks (framework bundles use them)
/// are recreated, as long as they point inside the package.
#[cfg(unix)]
fn write_entry(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::remove_file(path);
    if mode & S_IFMT == S_IFLNK {
        let target = String::from_utf8(data.to_vec())?;
        if target.starts_with('/') || target.split('/').any(|p| p == "..") {
            bail!("symlink {} points outside the package", path.display());
        }
        std::os::unix::fs::symlink(target, path)?;
        return Ok(());
    }
    fs::write(path, data)?;
    let exec = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(exec))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_entry(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    if mode & S_IFMT == S_IFLNK {
        bail!(
            "{} is a symlink, which this platform's packages shouldn't have",
            path.display()
        );
    }
    fs::write(path, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_platform_is_pinned_to_the_same_build() {
        for platform in [&MACOS, &LINUX, &WINDOWS] {
            assert_eq!(platform.version(), "1788652215", "{}", platform.name);
            for (name, _) in platform.packages {
                let pkg = platform.package(name).unwrap();
                assert!(
                    pkg.file.starts_with(name),
                    "{}: {}",
                    platform.name,
                    pkg.file
                );
                assert_eq!(pkg.sha2.len(), 64);
            }
        }
        // Not confused by the zipvz/sha2vz fields that follow.
        assert_eq!(
            MACOS.package("bins_osx").unwrap().sha2,
            "8f8b075397f5b933bf7dcf9d6ae611487016f8554592edaa28f1589fbba2c152"
        );
    }

    #[test]
    fn the_engine_is_among_the_fetched_files() {
        for platform in [&MACOS, &LINUX, &WINDOWS] {
            assert!(
                platform
                    .packages
                    .iter()
                    .any(|(_, files)| files.is_some_and(|f| f.contains(&platform.engine))),
                "{}",
                platform.name
            );
        }
    }
}
