//! fumes: a small Steam client.
//!
//! Sign-in and the owned-games list come straight from Steam's servers via
//! steam-vent, and games are downloaded from Steam's content servers. To
//! run them, fumes hosts Valve's own Steam client engine (just the engine,
//! none of Steam's UI), so games get the real Steamworks API. Games
//! installed by the official client are still listed and launched through
//! it.

mod appinfo;
mod cdn;
mod dirs;
mod download;
mod favorites;
mod friends;
mod installs;
mod kv;
mod library;
mod local;
mod run;
mod session;
mod steam;
mod tui;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::dirs::Dirs;
use crate::installs::Install;
use crate::library::Game;
use crate::steam::{APP_FULLY_INSTALLED, APP_RUNNING, Login, SyncDirection, SyncState};
use crate::tui::game::{self, report, report_problem};

#[derive(Parser)]
#[command(version, about = "A small Steam client")]
struct Cli {
    /// Without a command, fumes opens its terminal UI.
    #[command(subcommand)]
    command: Option<Cmd>,
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
        /// Install folder. Defaults to <library>/steamapps/common/<game's
        /// folder name>, which the Steam engine also sees.
        #[arg(long)]
        dir: Option<PathBuf>,
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
    /// Play a game. For games fumes installed, fumes runs the Steam engine
    /// (logged in as you) while the game runs; Steam's installs go through
    /// Steam.
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
    /// The Steam engine fumes runs games with.
    Engine {
        #[command(subcommand)]
        action: EngineCmd,
    },
}

#[derive(Subcommand)]
enum EngineCmd {
    /// Download the engine now (otherwise the first launch does).
    Fetch,
    /// One-time setup so games can find the engine. On macOS this installs
    /// the launchd agent Steam itself uses; elsewhere there's nothing to do.
    Setup,
    /// Undo `setup`.
    Remove,
    /// Show the engine's build, whether it's downloaded and set up.
    Status,
    /// Keep the engine logged in until Ctrl-C, for games started by hand.
    Run {
        /// Use Steam's anonymous account instead of yours.
        #[arg(long)]
        anonymous: bool,
    },
    /// Check the engine end to end with Steam's anonymous account, and
    /// optionally that a game's Steamworks library can attach to it.
    Test {
        /// A libsteam_api.dylib / libsteam_api.so / steam_api64.dll to try.
        #[arg(long)]
        lib: Option<PathBuf>,
        /// App the test pretends to be (480 is Valve's Spacewar test app).
        #[arg(long, default_value_t = 480)]
        appid: u32,
    },
    /// Sign in and report what the engine knows about a game's Steam Cloud.
    #[command(hide = true)]
    CloudInfo {
        game: String,
        /// Also download the cloud's saves (stops at a conflict).
        #[arg(long)]
        pull: bool,
    },
    /// The game half of `test`, run as a separate process like a real game.
    #[command(hide = true)]
    AttachProbe {
        #[arg(long)]
        lib: PathBuf,
        #[arg(long)]
        appid: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Only fumes' own warnings unless RUST_LOG asks for more: libraries log
    // errors (a bad certificate on one content server, say) that fumes
    // already recovers from by retrying elsewhere.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("fumes=warn"));
    let cli = Cli::parse();
    let dirs = Dirs::new()?;
    let Some(command) = cli.command else {
        // The screen belongs to the UI, so logs go to a file instead.
        std::fs::create_dir_all(&dirs.cache)?;
        let log = std::fs::File::create(dirs.cache.join("fumes.log"))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(log))
            .with_ansi(false)
            .init();
        return tui::run(dirs);
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    match command {
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
            verify,
            steam,
        } => {
            if steam {
                let games = load_library(&dirs, true).await?;
                let game = library::find(&games, &game)?;
                return open_steam_url(&format!("steam://install/{}", game.appid));
            }
            install(&dirs, &game, os, language, dir, verify).await
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
                return play(&dirs, &install, option, &args).await;
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
        Cmd::Engine { action } => engine(&dirs, action).await,
    }
}

async fn install(
    dirs: &Dirs,
    query: &str,
    os: Option<String>,
    language: Option<String>,
    dir: Option<PathBuf>,
    verify: bool,
) -> Result<()> {
    println!("Connecting to Steam…");
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
        // Absolute: the Steam engine changes fumes' working folder later.
        (Some(dir), _) => std::path::absolute(dir)?,
        (None, Some(p)) => p.path.clone(),
        (None, None) => {
            let info = appinfo::fetch(&connection, game.appid).await?;
            dirs.library()
                .join("steamapps/common")
                .join(&info.installdir)
        }
    };

    let opts = download::Options {
        os,
        language,
        dir,
        verify,
    };
    let install = download::install(&connection, game.appid, &opts).await?;
    installs::put(dirs, install.clone())?;
    if local::write_manifest(&install)?.is_none() {
        eprintln!(
            "note: {} isn't in a Steam library folder, so the Steam engine won't list it \
             as installed (games still run)",
            install.path.display()
        );
    }
    println!(
        "Installed {} ({}, {}) in {}",
        install.name,
        install.os,
        download::human(install.size),
        install.path.display()
    );
    Ok(())
}

/// Run a game fumes installed with the Steam engine logged in behind it.
async fn play(dirs: &Dirs, install: &Install, option: usize, args: &[String]) -> Result<()> {
    let native = install.os == download::host_os();
    let command = run::command(install, option, args)?;
    let key = run::launch_key(install, option)?;

    let runtime = engine_runtime(dirs).await?;
    if !steam::register::ready()? {
        bail!("games can't find the Steam engine yet; run `fumes engine setup` once");
    }
    // So the engine lists fumes' games as installed.
    if let Some(root) = steam::data_dir() {
        local::add_library_folder(&root, &dirs.library())?;
    }
    let (account, token) = session::credentials(dirs)?;
    report(format_args!("Signing in to Steam…"));
    let client = start_engine(&runtime, Login::Token { account, token }).await?;

    // The engine launches the game itself when it can, exactly like
    // Steam's Play button: cloud saves come down first and go up after,
    // with Steam's environment and playtime tracking. Otherwise fumes
    // starts it directly, without cloud sync.
    let installed = block(|| client.app_state(install.appid))? & APP_FULLY_INSTALLED != 0;
    let result = if native && installed {
        play_through_engine(&client, install, key, &args.join(" ")).await
    } else {
        let why = if !native {
            format!("it's a {} build running through Wine", install.os)
        } else {
            "the Steam engine doesn't list it as installed".to_owned()
        };
        report_problem(format_args!(
            "note: starting {} directly ({why}); cloud saves won't sync",
            install.name
        ));
        play_directly(command, &install.name).await
    };
    stop_engine(client).await?;
    result
}

/// `args` go on the game's command line after its launch entry's own.
async fn play_through_engine(
    client: &steam::Client,
    install: &Install,
    key: u32,
    args: &str,
) -> Result<()> {
    let appid = install.appid;
    let name = &install.name;

    // What Steam does before starting a game: bring the cloud saves down,
    // and stop if both sides changed until you pick which to keep.
    report(format_args!("Syncing cloud saves…"));
    if !sync_cloud(client, appid, name, SyncDirection::Down)? {
        report(format_args!("Launch cancelled; nothing was changed."));
        return Ok(());
    }

    report(format_args!("Launching {name}…"));
    block(|| client.launch(appid, key, args))?;
    // With neither the game nor any Steam activity for a minute, the
    // engine has given up on the launch.
    let started = std::time::Instant::now();
    while block(|| client.app_state(appid))? & APP_RUNNING == 0 {
        if started.elapsed() > std::time::Duration::from_secs(60) {
            bail!(
                "the Steam engine didn't start {name}; its reason is in \
                 ~/Library/Application Support/Steam/logs/content_log.txt"
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            _ = tokio::signal::ctrl_c() => {
                report(format_args!("Launch cancelled."));
                return Ok(());
            }
        }
    }
    report(format_args!("{name} is running."));

    // Ctrl-C reaches the game too (the engine started it from this
    // process); keep going until it's gone so the saves get uploaded.
    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                if block(|| client.app_state(appid))? & APP_RUNNING == 0 {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                report_problem(format_args!("Waiting for {name} to quit before uploading saves and signing out…"));
            }
        }
    }

    report(format_args!("Uploading cloud saves…"));
    if !sync_cloud(client, appid, name, SyncDirection::Up)? {
        report_problem(format_args!(
            "warning: {name}'s saves weren't uploaded; they're kept on this machine"
        ));
    }
    Ok(())
}

/// Sync one way. A conflict (both sides changed) is settled by asking;
/// returns false if it was left unsettled, with nothing overwritten.
fn sync_cloud(
    client: &steam::Client,
    appid: u32,
    name: &str,
    direction: SyncDirection,
) -> Result<bool> {
    let mut state = block(|| client.cloud_sync(appid, direction))?;
    if state == SyncState::Conflict {
        let Some(keep_local) = ask_conflict(name)? else {
            return Ok(false);
        };
        block(|| client.resolve_conflict(appid, keep_local))?;
        state = block(|| client.cloud_sync(appid, direction))?;
    }
    match state {
        SyncState::Synchronized | SyncState::ChangesLocally if direction == SyncDirection::Down => {
        }
        SyncState::Synchronized => {}
        SyncState::Disabled => report(format_args!("Steam Cloud is off for {name}.")),
        SyncState::Conflict => return Ok(false),
        other => report_problem(format_args!(
            "warning: {name}'s cloud saves ended up {other:?}"
        )),
    }
    Ok(true)
}

async fn play_directly(mut command: Command, name: &str) -> Result<()> {
    report(format_args!("Launching {name}…"));
    let child = command
        .spawn()
        .with_context(|| format!("starting {name}"))?;
    let status = wait_for(child, name).await?;
    if !status.success() {
        report_problem(format_args!("{name} exited with {status}"));
    }
    Ok(())
}

/// Run a blocking call to the engine without stalling the async runtime.
fn block<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    tokio::task::block_in_place(f)
}

/// Both sides changed since the last sync. Keep which? `None` cancels.
/// When the terminal UI runs this, it does the asking.
fn ask_conflict(name: &str) -> Result<Option<bool>> {
    if game::from_ui() {
        println!("{}", game::CONFLICT_PROMPT);
    } else {
        eprintln!(
            "{name}'s saves changed both in Steam Cloud and on this machine since they were \
             last in sync. Whichever you don't keep is overwritten."
        );
        print!("Keep the [c]loud saves or the [l]ocal ones? Anything else cancels: ");
    }
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(match answer.trim().to_lowercase().as_str() {
        "c" | "cloud" => Some(false),
        "l" | "local" => Some(true),
        _ => None,
    })
}

/// Wait for the game. Ctrl-C in this terminal reaches the game too; fumes
/// stays up until it's gone so it can sign out of Steam afterwards.
async fn wait_for(child: std::process::Child, name: &str) -> Result<std::process::ExitStatus> {
    let mut child = child;
    let wait = tokio::task::spawn_blocking(move || child.wait());
    tokio::pin!(wait);
    loop {
        tokio::select! {
            status = &mut wait => return Ok(status??),
            _ = tokio::signal::ctrl_c() => {
                report_problem(format_args!("Waiting for {name} to quit before signing out of Steam…"));
            }
        }
    }
}

async fn start_engine(runtime: &Path, login: Login) -> Result<steam::Client> {
    let runtime = runtime.to_owned();
    tokio::task::spawn_blocking(move || steam::Client::start(&runtime, login)).await?
}

async fn stop_engine(client: steam::Client) -> Result<()> {
    tokio::task::spawn_blocking(move || client.stop()).await?
}

/// The engine for the pinned client build, downloaded on first use.
async fn engine_runtime(dirs: &Dirs) -> Result<PathBuf> {
    let platform = steam::runtime::host();
    if !platform.is_fetched(&dirs.engine()) {
        report(format_args!(
            "Downloading the Steam engine (client build {})…",
            platform.version()
        ));
        platform.fetch(&dirs.engine(), false).await?;
    }
    Ok(platform.runtime_dir(&dirs.engine()))
}

async fn engine(dirs: &Dirs, action: EngineCmd) -> Result<()> {
    let platform = steam::runtime::host();
    match action {
        EngineCmd::Fetch => {
            let dir = platform.fetch(&dirs.engine(), false).await?;
            println!(
                "Steam engine (client build {}) in {}",
                platform.version(),
                dir.display()
            );
            Ok(())
        }
        EngineCmd::Setup => {
            let runtime = engine_runtime(dirs).await?;
            steam::register::install(&runtime)
        }
        EngineCmd::Remove => steam::register::uninstall(),
        EngineCmd::Status => {
            let fetched = platform.is_fetched(&dirs.engine());
            println!(
                "client build {} ({}): {}",
                platform.version(),
                platform.name,
                if fetched {
                    format!(
                        "downloaded to {}",
                        platform.runtime_dir(&dirs.engine()).display()
                    )
                } else {
                    "not downloaded yet".into()
                }
            );
            steam::register::status()
        }
        EngineCmd::Run { anonymous } => {
            let runtime = engine_runtime(dirs).await?;
            let login = if anonymous {
                Login::Anonymous
            } else {
                let (account, token) = session::credentials(dirs)?;
                Login::Token { account, token }
            };
            let client = start_engine(&runtime, login).await?;
            println!("The Steam engine is up; start games now. Ctrl-C signs out.");
            tokio::signal::ctrl_c().await?;
            stop_engine(client).await
        }
        EngineCmd::Test { lib, appid } => {
            let lib = lib.map(std::path::absolute).transpose()?;
            let runtime = engine_runtime(dirs).await?;
            steam::register::status()?;
            let client = start_engine(&runtime, Login::Anonymous).await?;
            println!("Engine logged in to Steam (anonymous account).");
            // What the engine makes of fumes' installs, which is what lets
            // it launch them (and sync their cloud saves).
            for install in installs::load(dirs).values() {
                let state = block(|| client.app_state(install.appid))?;
                let seen = if state & APP_FULLY_INSTALLED != 0 {
                    "listed as installed"
                } else {
                    "not listed as installed (launches without cloud sync)"
                };
                println!("  {}: {seen}", install.name);
            }
            let probe = match &lib {
                Some(lib) => Command::new(std::env::current_exe()?)
                    .args([
                        "engine",
                        "attach-probe",
                        "--appid",
                        &appid.to_string(),
                        "--lib",
                    ])
                    .arg(lib)
                    .status()
                    .map(|s| s.success()),
                None => Ok(true),
            };
            stop_engine(client).await?;
            if !probe? {
                bail!("the game library couldn't attach");
            }
            Ok(())
        }
        EngineCmd::CloudInfo { game, pull } => {
            let games = load_library(dirs, true).await?;
            let game = library::find(&games, &game)?;
            let runtime = engine_runtime(dirs).await?;
            let (account, token) = session::credentials(dirs)?;
            println!("Signing in to Steam…");
            let client = start_engine(&runtime, Login::Token { account, token }).await?;
            let report = (|| -> Result<()> {
                let (account, app) = block(|| client.cloud_enabled(game.appid))?;
                println!("Steam Cloud on for the account: {account}");
                println!("Steam Cloud on for {}: {app}", game.name);
                println!(
                    "sync state: {:?}",
                    block(|| client.cloud_state(game.appid))?
                );
                println!("syncing now: {}", block(|| client.cloud_busy(game.appid))?);
                if pull {
                    let state = block(|| client.cloud_sync(game.appid, SyncDirection::Down))?;
                    println!("after download: {state:?}");
                }
                Ok(())
            })();
            stop_engine(client).await?;
            report
        }
        EngineCmd::AttachProbe { lib, appid } => steam::attach::attach(&lib, appid),
    }
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
    local::remove_manifest(&install);
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
    Ok(library::merge(owned, installed_here(dirs)))
}

/// What's on this machine, installed by Steam or by fumes.
fn installed_here(dirs: &Dirs) -> Vec<local::Installed> {
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
    installed
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
    open_url(url)
}

/// Open a link (web page or `steam://`) with the system's handler.
fn open_url(url: &str) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // `start`'s first quoted argument is the window title.
        let mut start = Command::new("cmd");
        start.args(["/C", "start", ""]);
        start
    } else {
        Command::new("xdg-open")
    };
    // Quiet, so it can't scribble over the terminal UI; failures still
    // show in the exit status.
    let status = command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("could not open {url}"))?;
    if !status.success() {
        bail!("opening {url} failed");
    }
    Ok(())
}
