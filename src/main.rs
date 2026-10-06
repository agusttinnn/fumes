//! fumes: a small Steam library client.
//!
//! Sign-in and the owned-games list come straight from Steam's servers via
//! steam-vent. Games are downloaded from Steam's content servers and, where
//! possible, started without the Steam client by swapping in gbe_fork's
//! Steamworks emulator. Games installed by the official client are still
//! listed and launched through it.

mod appinfo;
mod cdn;
mod dirs;
mod download;
mod emu;
mod installs;
mod kv;
mod library;
mod local;
mod run;
mod session;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::dirs::Dirs;
use crate::installs::Install;
use crate::library::Game;

#[derive(Parser)]
#[command(version, about = "A small Steam library client")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign in with your Steam account (stores a refresh token, not the password).
    Login,
    /// Forget the stored session.
    Logout,
    /// List your games.
    Library {
        /// Only games installed on this machine.
        #[arg(long)]
        installed: bool,
        /// Use the last fetched list instead of asking Steam.
        #[arg(long)]
        offline: bool,
    },
    /// Download a game from Steam's content servers. Run it again to update
    /// or repair an install; only what changed is downloaded.
    Install {
        game: String,
        /// Which build to get: windows, macos or linux. Defaults to this
        /// machine's platform, else windows.
        #[arg(long)]
        os: Option<String>,
        /// Game language (Steam's API name, e.g. english, german, schinese).
        #[arg(long)]
        language: Option<String>,
        /// Install folder. Defaults to <library>/<game's folder name>.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Don't set up the Steamworks emulator afterwards.
        #[arg(long)]
        no_emu: bool,
        /// Re-check every file against Steam's hashes (repairs edited or
        /// corrupted files).
        #[arg(long)]
        verify: bool,
        /// Hand the install to the official Steam client instead.
        #[arg(long)]
        steam: bool,
    },
    /// Delete a game fumes installed.
    Uninstall {
        game: String,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Start a game. Games fumes installed run directly (through Wine for
    /// Windows builds off Windows); Steam's installs go through Steam.
    Launch {
        game: String,
        /// Which of the game's launch options to use (0 is the default).
        #[arg(long, default_value_t = 0)]
        option: usize,
        /// Use the Steam client even if fumes installed the game too.
        #[arg(long)]
        steam: bool,
        /// Extra arguments for the game.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Manage the Steamworks emulator (gbe_fork) in a game fumes installed.
    Emu {
        #[command(subcommand)]
        action: EmuCmd,
    },
}

#[derive(Subcommand)]
enum EmuCmd {
    /// Swap the emulator in (keeps the original libraries).
    Enable { game: String },
    /// Put the original Steamworks libraries back.
    Disable { game: String },
    /// Fetch the latest gbe_fork release.
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let dirs = Dirs::new()?;
    match cli.command {
        Cmd::Login => session::login(&dirs).await,
        Cmd::Logout => session::logout(&dirs),
        Cmd::Library { installed, offline } => {
            let games = load_library(&dirs, offline).await?;
            print_library(&games, installed);
            Ok(())
        }
        Cmd::Install {
            game,
            os,
            language,
            dir,
            no_emu,
            verify,
            steam,
        } => {
            if steam {
                let games = load_library(&dirs, true).await?;
                let game = library::find(&games, &game)?;
                return open_steam_url(&format!("steam://install/{}", game.appid));
            }
            install(&dirs, &game, os, language, dir, verify, !no_emu).await
        }
        Cmd::Uninstall { game, yes } => uninstall(&dirs, &game, yes).await,
        Cmd::Launch {
            game,
            option,
            steam,
            args,
        } => {
            // Offline lookup: launching shouldn't wait on a network round trip.
            let games = load_library(&dirs, true).await?;
            let game = library::find(&games, &game)?;
            if !steam && let Some(install) = installs::load(&dirs).remove(&game.appid) {
                if !emu::is_enabled(&install.path) && install.os != "macos" {
                    eprintln!(
                        "note: the emulator isn't set up for {}; if it uses Steamworks it will \
                         look for the Steam client (`fumes emu enable {}`)",
                        install.name, install.appid
                    );
                }
                return run::launch(&install, option, &args);
            }
            if game.installed.is_none() {
                bail!(
                    "{} isn't installed; run `fumes install {}`",
                    game.name,
                    game.appid
                );
            }
            println!("Launching {} through Steam…", game.name);
            open_steam_url(&format!("steam://rungameid/{}", game.appid))
        }
        Cmd::Emu { action } => match action {
            EmuCmd::Enable { game } => {
                let install = find_install(&dirs, &game).await?;
                enable_emu(&dirs, &install).await
            }
            EmuCmd::Disable { game } => {
                let install = find_install(&dirs, &game).await?;
                let restored = emu::disable(&install.path)?;
                println!(
                    "Restored {restored} original Steamworks libraries in {}.",
                    install.name
                );
                Ok(())
            }
            EmuCmd::Update => {
                for os in ["windows", "linux"] {
                    let (tag, _) = emu::release(&dirs, os, true).await?;
                    println!("gbe_fork {tag} ({os}) is ready.");
                }
                println!("Run `fumes emu enable <game>` to apply it to a game.");
                Ok(())
            }
        },
    }
}

async fn install(
    dirs: &Dirs,
    query: &str,
    os: Option<String>,
    language: Option<String>,
    dir: Option<PathBuf>,
    verify: bool,
    with_emu: bool,
) -> Result<()> {
    let connection = session::connect(dirs).await?;
    let owned = library::fetch_owned(&connection).await?;
    library::save_cache(&dirs.library_cache(), &owned)?;
    let games = library::merge(owned, Vec::new());
    let game = library::find(&games, query)?;

    // Updating keeps the earlier choices unless told otherwise.
    let previous = installs::load(dirs).remove(&game.appid);
    let os = os.or_else(|| previous.as_ref().map(|p| p.os.clone()));
    let language = language
        .or_else(|| previous.as_ref().map(|p| p.language.clone()))
        .unwrap_or_else(|| "english".into());
    let dir = match (dir, &previous) {
        (Some(dir), _) => dir,
        (None, Some(p)) => p.path.clone(),
        (None, None) => {
            let info = appinfo::fetch(&connection, game.appid).await?;
            dirs.library().join(&info.installdir)
        }
    };

    // The emulator's libraries were swapped in; put the originals back so
    // the download sees (and if needed, updates) Steam's real files.
    let had_emu = emu::is_enabled(&dir);
    if had_emu {
        emu::disable(&dir)?;
    }

    let opts = download::Options {
        os,
        language,
        dir,
        verify,
    };
    let install = download::install(&connection, game.appid, &opts).await?;
    installs::put(dirs, install.clone())?;
    println!(
        "Installed {} ({}, {}) in {}",
        install.name,
        install.os,
        download::human(install.size),
        install.path.display()
    );

    if (with_emu || had_emu) && install.os != "macos" {
        if let Err(e) = enable_emu(dirs, &install).await {
            eprintln!("warning: emulator not set up: {e:#}");
        }
    } else if install.os == "macos" {
        println!(
            "Native macOS builds can't use the emulator (gbe_fork has no macOS build); \
             games that need Steamworks will look for the Steam client. \
             `fumes install {} --os windows` gets the Windows build for Wine instead.",
            install.appid
        );
    }
    Ok(())
}

async fn enable_emu(dirs: &Dirs, install: &Install) -> Result<()> {
    let identity = session::identity(dirs)?;
    let profile = emu::Profile {
        appid: install.appid,
        persona: identity.persona,
        steam_id: identity.steam_id,
        language: install.language.clone(),
        dlcs: install.dlcs.clone(),
        depots: install.depots.keys().copied().collect(),
    };
    let report = emu::enable(dirs, &install.path, &profile, &run::executables(install)).await?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    println!(
        "Steamworks emulator (gbe_fork {}) set up for {}: {} libraries replaced. \
         `fumes emu disable {}` undoes it.",
        report.release,
        install.name,
        report.replaced.len(),
        install.appid
    );
    Ok(())
}

async fn uninstall(dirs: &Dirs, query: &str, yes: bool) -> Result<()> {
    let install = find_install(dirs, query).await?;
    if !yes {
        print!(
            "Delete {} ({})? [y/N] ",
            install.name,
            install.path.display()
        );
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Kept it.");
            return Ok(());
        }
    }
    if install.path.exists() {
        std::fs::remove_dir_all(&install.path)
            .with_context(|| format!("deleting {}", install.path.display()))?;
    }
    installs::remove(dirs, install.appid)?;
    println!("Uninstalled {}.", install.name);
    Ok(())
}

/// A game fumes installed, by app id or name.
async fn find_install(dirs: &Dirs, query: &str) -> Result<Install> {
    let games = load_library(dirs, true).await?;
    let game = library::find(&games, query)?;
    installs::load(dirs).remove(&game.appid).with_context(|| {
        format!(
            "fumes didn't install {}; run `fumes install {}`",
            game.name, game.appid
        )
    })
}

/// Owned games (from Steam, or the cache when offline or unreachable) joined
/// with a fresh scan of what's installed, by Steam and by fumes.
async fn load_library(dirs: &Dirs, offline: bool) -> Result<Vec<Game>> {
    let cache = dirs.library_cache();
    let owned = if offline {
        library::load_cache(&cache).unwrap_or_default()
    } else {
        match fetch(dirs).await {
            Ok(owned) => {
                library::save_cache(&cache, &owned)?;
                owned
            }
            Err(e) => match library::load_cache(&cache) {
                Some(owned) => {
                    eprintln!("warning: {e:#}; showing the cached library");
                    owned
                }
                None => return Err(e),
            },
        }
    };
    let mut installed = local::steam_root()
        .map(|r| local::scan(&r))
        .unwrap_or_default();
    // Listed after Steam's so a game installed both ways shows as fumes'.
    installed.extend(
        installs::load(dirs)
            .into_values()
            .filter(|i| i.path.is_dir())
            .map(|i| local::Installed {
                appid: i.appid,
                name: i.name,
                path: i.path,
                size_on_disk: i.size,
                complete: true,
                by_fumes: true,
            }),
    );
    Ok(library::merge(owned, installed))
}

async fn fetch(dirs: &Dirs) -> Result<Vec<library::Owned>> {
    let connection = session::connect(dirs).await?;
    library::fetch_owned(&connection).await
}

fn print_library(games: &[Game], installed_only: bool) {
    let mut shown = 0;
    for game in games {
        if installed_only && game.installed.is_none() {
            continue;
        }
        let state = match &game.installed {
            Some(i) if i.by_fumes => "fumes",
            Some(i) if i.complete => "steam",
            Some(_) => "updating",
            None => "",
        };
        let hours = game
            .owned
            .as_ref()
            .map(|o| format!("{:.1} h", o.playtime_minutes as f64 / 60.0))
            .unwrap_or_default();
        println!(
            "{:>8}  {:<9}  {:>8}  {}",
            game.appid, state, hours, game.name
        );
        shown += 1;
    }
    println!("\n{shown} games");
}

fn open_steam_url(url: &str) -> Result<()> {
    if local::steam_root().is_none() {
        eprintln!("warning: no Steam install found; the link may not open anything");
    }
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        // `start`'s first quoted argument is the window title.
        Command::new("cmd").args(["/C", "start", "", url]).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    }
    .context("could not open the steam:// link")?;
    if !status.success() {
        bail!("opening {url} failed");
    }
    Ok(())
}
