//! Starting a game fumes installed, without the Steam client.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::appinfo::Launch;
use crate::download::host_os;
use crate::installs::Install;

/// The launch entries that apply to this install, best first: ones for its
/// platform and bitness, the default entry before alternatives, nothing
/// tied to a beta branch, no dedicated servers or editors.
pub fn launch_options(install: &Install) -> Vec<&Launch> {
    let mut options: Vec<&Launch> = install
        .launch
        .iter()
        .filter(|l| l.oslist.is_empty() || l.oslist.contains(&install.os))
        .filter(|l| l.osarch.as_deref().is_none_or(|a| a == install.arch))
        .filter(|l| l.betakey.is_none())
        .filter(|l| {
            !matches!(
                l.kind.as_str(),
                "server" | "editor" | "none" | "vr" | "othervr"
            )
        })
        .collect();
    options.sort_by_key(|l| !matches!(l.kind.as_str(), "" | "default"));
    options
}

/// The executables a launch option might start, for the SteamStub check.
pub fn executables(install: &Install) -> Vec<PathBuf> {
    launch_options(install)
        .iter()
        .map(|l| install.path.join(normalize(&l.executable)))
        .collect()
}

pub fn launch(install: &Install, option: usize, extra: &[String]) -> Result<()> {
    let options = launch_options(install);
    let Some(launch) = options.get(option) else {
        if options.is_empty() {
            bail!(
                "Steam lists no way to start {} on {}",
                install.name,
                install.os
            );
        }
        bail!(
            "{} has {} launch options (0–{})",
            install.name,
            options.len(),
            options.len() - 1
        );
    };

    let exe = install.path.join(normalize(&launch.executable));
    let workdir = if launch.workingdir.trim().is_empty() {
        exe.parent().unwrap_or(&install.path).to_owned()
    } else {
        install.path.join(normalize(&launch.workingdir))
    };
    let mut args = split_args(&launch.arguments);
    args.extend(extra.iter().cloned());

    let mut command = match (install.os.as_str(), host_os()) {
        (game, host) if game == host => native(&exe)?,
        ("windows", _) => {
            let wine = find_wine().context(
                "this is a Windows build; install Wine (or CrossOver) or set FUMES_WINE to its \
                 `wine` binary",
            )?;
            let mut c = Command::new(wine);
            c.arg(&exe);
            c
        }
        (game, host) => bail!("{} is a {game} build and this is {host}", install.name),
    };
    command
        .args(&args)
        .current_dir(&workdir)
        // What the Steam client sets for the games it starts.
        .env("SteamAppId", install.appid.to_string())
        .env("SteamGameId", install.appid.to_string())
        .env("SteamOverlayGameId", install.appid.to_string());

    println!("Launching {}…", install.name);
    let status = command
        .status()
        .with_context(|| format!("starting {}", exe.display()))?;
    if !status.success() {
        eprintln!("{} exited with {status}", install.name);
    }
    Ok(())
}

/// Run the file itself, or for a macOS bundle the binary inside it (going
/// through `open` would drop the environment the game needs).
fn native(exe: &Path) -> Result<Command> {
    if exe.extension().is_some_and(|e| e == "app") {
        return Ok(Command::new(bundle_binary(exe)?));
    }
    Ok(Command::new(exe))
}

fn bundle_binary(app: &Path) -> Result<PathBuf> {
    let macos = app.join("Contents/MacOS");
    let name = app.file_stem().unwrap_or_default();
    // Usually the binary is named after the bundle; otherwise take the only
    // file there.
    let named = macos.join(name);
    if named.is_file() {
        return Ok(named);
    }
    let files: Vec<PathBuf> = fs::read_dir(&macos)
        .with_context(|| format!("{} isn't an app bundle", app.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    match files.as_slice() {
        [one] => Ok(one.clone()),
        _ => bail!("can't tell which binary in {} to run", macos.display()),
    }
}

fn find_wine() -> Option<PathBuf> {
    if let Some(wine) = std::env::var_os("FUMES_WINE") {
        return Some(wine.into());
    }
    let path = std::env::var_os("PATH")?;
    let on_path = ["wine", "wine64"]
        .iter()
        .flat_map(|name| std::env::split_paths(&path).map(move |dir| dir.join(name)))
        .find(|p| p.is_file());
    on_path.or_else(|| {
        [
            "/Applications/Wine Stable.app/Contents/Resources/wine/bin/wine",
            "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/wine",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    })
}

/// Launch paths in app info use `\` on every platform.
fn normalize(path: &str) -> PathBuf {
    path.split(['\\', '/']).filter(|p| !p.is_empty()).collect()
}

/// Split a launch argument string the way a shell would for plain words
/// and double-quoted phrases.
pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut has_word = false;
    for c in args.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has_word {
                    out.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            c => {
                current.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn launch(exe: &str, kind: &str, oslist: &[&str], osarch: Option<&str>) -> Launch {
        Launch {
            executable: exe.into(),
            arguments: String::new(),
            workingdir: String::new(),
            description: String::new(),
            kind: kind.into(),
            oslist: oslist.iter().map(|s| s.to_string()).collect(),
            osarch: osarch.map(str::to_owned),
            betakey: None,
        }
    }

    #[test]
    fn chooses_launch_options_for_the_install() {
        let mut beta = launch("beta.exe", "", &[], None);
        beta.betakey = Some("beta".into());
        let install = Install {
            appid: 1,
            name: "Game".into(),
            path: PathBuf::from("/games/Game"),
            os: "windows".into(),
            arch: "64".into(),
            language: "english".into(),
            buildid: 0,
            depots: BTreeMap::new(),
            size: 0,
            launch: vec![
                launch("tools\\editor.exe", "editor", &[], None),
                launch("safe.exe", "option1", &["windows"], None),
                launch("game32.exe", "default", &["windows"], Some("32")),
                launch("Game.app", "default", &["macos"], None),
                launch("bin\\win64\\game.exe", "default", &["windows"], Some("64")),
                beta,
            ],
            dlcs: BTreeMap::new(),
        };
        let exes: Vec<&str> = launch_options(&install)
            .iter()
            .map(|l| l.executable.as_str())
            .collect();
        assert_eq!(exes, ["bin\\win64\\game.exe", "safe.exe"]);
        assert_eq!(
            executables(&install)[0],
            PathBuf::from("/games/Game/bin/win64/game.exe")
        );
    }

    #[test]
    fn splits_arguments() {
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("  -novid   -high "), ["-novid", "-high"]);
        assert_eq!(
            split_args(r#"-config "my settings.cfg" -x """#),
            ["-config", "my settings.cfg", "-x", ""]
        );
    }

    #[test]
    fn finds_the_binary_in_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Some Game.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/MacOS/launcher_bin"), b"").unwrap();
        assert_eq!(
            bundle_binary(&app).unwrap(),
            app.join("Contents/MacOS/launcher_bin")
        );
        fs::write(app.join("Contents/MacOS/Some Game"), b"").unwrap();
        assert_eq!(
            bundle_binary(&app).unwrap(),
            app.join("Contents/MacOS/Some Game")
        );
    }
}
