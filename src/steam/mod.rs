//! Valve's Steam client engine, hosted by fumes.
//!
//! Games talk to Steam through `steam_api`, which connects to a running
//! Steam client engine (`steamclient`). Steam's own UI is one host for that
//! engine; fumes is another. It downloads just the engine (`runtime`), loads
//! it and becomes the Steam client (`engine`), logs in with fumes' saved
//! session, and makes itself findable by games the way each platform's
//! `steam_api` expects (`register`). Games then get the real Steamworks API:
//! ownership, achievements, cloud saves, friends, networking.

pub mod attach;
mod engine;
pub mod register;
pub mod runtime;

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;

pub use self::engine::{APP_FULLY_INSTALLED, APP_RUNNING, SyncDirection, SyncState};
use self::engine::{ClientUser, Engine, RemoteStorage};

/// How the engine signs in.
pub enum Login {
    /// fumes' saved session: account name and refresh token.
    Token { account: String, token: String },
    /// Steam's anonymous account; enough to check the engine works.
    Anonymous,
}

/// A running, logged-in engine. Dropping it (or `stop`) logs off and undoes
/// the registration.
pub struct Client {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
    commands: mpsc::Sender<Command>,
}

/// Work for the engine thread, which alone may call into the engine. Each
/// carries where to send its answer.
enum Command {
    Launch(u32, u32, String, mpsc::Sender<Result<u64>>),
    AppState(u32, mpsc::Sender<Result<u32>>),
    CloudState(u32, mpsc::Sender<Result<SyncState>>),
    CloudBusy(u32, mpsc::Sender<Result<bool>>),
    CloudEnabled(u32, mpsc::Sender<Result<(bool, bool)>>),
    CloudSync(u32, SyncDirection, mpsc::Sender<Result<SyncState>>),
    Resolve(u32, bool, mpsc::Sender<Result<bool>>),
}

impl Client {
    /// Load the engine from `runtime`, register it for games and log in.
    /// Returns once logged on.
    pub fn start(runtime: &Path, login: Login) -> Result<Client> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (commands, command_rx) = mpsc::channel::<Command>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let runtime = runtime.to_owned();
        // The engine's objects aren't thread-safe, so one thread owns them.
        let thread = std::thread::Builder::new()
            .name("steam-engine".into())
            .spawn(move || {
                let result = host(&runtime, login, &thread_stop, &ready_tx, &command_rx);
                if let Err(e) = &result {
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
                result
            })?;
        let mut client = Client {
            stop,
            thread: Some(thread),
            commands,
        };
        match ready_rx.recv_timeout(LOGON_TIMEOUT + Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(client),
            Ok(Err(e)) => {
                client.finish().ok();
                Err(anyhow!(e))
            }
            Err(_) => {
                client.stop.store(true, Ordering::SeqCst);
                bail!("the Steam engine didn't start in time")
            }
        }
    }

    /// Launch an installed app the way Steam's Play button does (cloud
    /// sync, Steam's environment, playtime). `option` is the app info
    /// launch entry's key; `args` go on the game's command line after the
    /// entry's own, as Steam adds them to join a friend's game. Returns
    /// once the engine has accepted it; the game starts after its cloud
    /// saves are down (`app_state` & `APP_RUNNING`).
    pub fn launch(&self, appid: u32, option: u32, args: &str) -> Result<()> {
        let args = args.to_owned();
        let call = self.ask(|reply| Command::Launch(appid, option, args, reply))?;
        if call == 0 {
            bail!("the Steam engine refused to launch app {appid}");
        }
        Ok(())
    }

    /// `EAppState` flags for an app (`APP_FULLY_INSTALLED`, `APP_RUNNING`).
    pub fn app_state(&self, appid: u32) -> Result<u32> {
        self.ask(|reply| Command::AppState(appid, reply))
    }

    pub fn cloud_state(&self, appid: u32) -> Result<SyncState> {
        self.ask(|reply| Command::CloudState(appid, reply))
    }

    /// Bring an app's cloud saves down (before playing) or up (after), the
    /// way Steam does around a launch. Returns the state afterwards;
    /// `SyncState::Conflict` means both sides changed and nothing was
    /// overwritten: settle it with `resolve_conflict`.
    pub fn cloud_sync(&self, appid: u32, direction: SyncDirection) -> Result<SyncState> {
        self.ask(|reply| Command::CloudSync(appid, direction, reply))
    }

    /// Whether Steam Cloud is on for the account, and for this app.
    pub fn cloud_enabled(&self, appid: u32) -> Result<(bool, bool)> {
        self.ask(|reply| Command::CloudEnabled(appid, reply))
    }

    /// Whether the engine is still moving an app's cloud saves.
    pub fn cloud_busy(&self, appid: u32) -> Result<bool> {
        self.ask(|reply| Command::CloudBusy(appid, reply))
    }

    /// After a conflict: keep this machine's files (uploaded over the
    /// cloud's) or the cloud's (downloaded over these).
    pub fn resolve_conflict(&self, appid: u32, keep_local: bool) -> Result<()> {
        if !self.ask(|reply| Command::Resolve(appid, keep_local, reply))? {
            bail!("Steam wouldn't resolve the cloud conflict");
        }
        Ok(())
    }

    fn ask<T>(&self, command: impl FnOnce(mpsc::Sender<Result<T>>) -> Command) -> Result<T> {
        let (reply, answer) = mpsc::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| anyhow!("the Steam engine has stopped"))?;
        answer
            .recv()
            .map_err(|_| anyhow!("the Steam engine has stopped"))?
    }

    /// Log off, release the engine and undo the registration.
    pub fn stop(mut self) -> Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| anyhow!("the Steam engine thread panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

const LOGON_TIMEOUT: Duration = Duration::from_secs(60);

fn host(
    runtime: &Path,
    login: Login,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(), String>>,
    commands: &mpsc::Receiver<Command>,
) -> Result<()> {
    // One engine at a time: games connect to whichever registered first, so
    // a second one (say, an anonymous `engine test`) could answer for the
    // first. The OS drops the lock if this process dies.
    let lock = std::fs::File::create(runtime.join("engine.lock"))?;
    if let Err(e) = lock.try_lock() {
        match e {
            std::fs::TryLockError::WouldBlock => bail!(
                "fumes is already running the Steam engine (a game or `fumes engine run`); \
                 quit that first"
            ),
            std::fs::TryLockError::Error(e) => return Err(e.into()),
        }
    }

    // The engine loads its optional libraries (controllers, voice chat) by
    // bare name, which only finds them in the working folder, where
    // Steam's own launcher puts it. fumes uses absolute paths from here on.
    std::env::set_current_dir(runtime)
        .with_context(|| format!("entering {}", runtime.display()))?;
    // The engine installs its own handlers for Ctrl-C and termination,
    // which would leave fumes impossible to interrupt or kill. Put the
    // process's back once it's up (crash handlers stay the engine's).
    let signals = signals::save();
    let path = runtime.join(runtime::host().engine);
    let mut engine = Engine::load(&path)?;
    engine.create_global_user()?;
    tracing::debug!(
        pipe = engine.pipe,
        user = engine.user,
        universe = engine.universe_name(1),
        "engine up"
    );
    let result = (|| {
        let _registration = register::Registration::acquire(runtime)?;
        let user = engine.client_user()?;
        let steam_id = match &login {
            Login::Token { account, token } => {
                user.set_login_token(token, account)?;
                steam_id_from_jwt(token)?
            }
            Login::Anonymous => {
                user.set_login_information("anonymous", "")?;
                ANONYMOUS_STEAM_ID
            }
        };
        log_on(&engine, &user, steam_id)?;
        if matches!(login, Login::Token { .. }) {
            wait_for_licenses(&engine);
        }
        signals::restore(&signals);
        let _ = ready.send(Ok(()));
        while !stop.load(Ordering::SeqCst) {
            frame(&engine);
            while let Ok(command) = commands.try_recv() {
                run_command(&engine, command);
            }
            std::thread::sleep(FRAME);
        }
        user.log_off();
        pump(&engine, Duration::from_secs(2), || !user.connected());
        Ok(())
    })();
    engine.release();
    result
}

fn run_command(engine: &Engine, command: Command) {
    match command {
        Command::Launch(appid, option, args, reply) => {
            let launched = engine
                .app_manager()
                .and_then(|m| Ok(m.launch(appid, option, &CString::new(args)?)));
            let _ = reply.send(launched);
        }
        Command::AppState(appid, reply) => {
            let _ = reply.send(engine.app_manager().map(|m| m.install_state(appid)));
        }
        Command::CloudState(appid, reply) => {
            let _ = reply.send(engine.remote_storage().map(|s| s.state(appid)));
        }
        Command::CloudEnabled(appid, reply) => {
            let _ = reply.send(
                engine
                    .remote_storage()
                    .map(|s| (s.enabled_for_account(), s.enabled_for_app(appid))),
            );
        }
        Command::CloudSync(appid, direction, reply) => {
            let _ = reply.send(
                engine
                    .remote_storage()
                    .and_then(|s| cloud_sync(engine, &s, appid, direction)),
            );
        }
        Command::CloudBusy(appid, reply) => {
            let _ = reply.send(engine.remote_storage().map(|s| s.in_progress(appid)));
        }
        Command::Resolve(appid, keep_local, reply) => {
            let _ = reply.send(
                engine
                    .remote_storage()
                    .map(|s| s.resolve_conflict(appid, keep_local)),
            );
        }
    }
}

/// Init the app's cloud state if needed, then sync and wait it out.
fn cloud_sync(
    engine: &Engine,
    storage: &RemoteStorage,
    appid: u32,
    direction: SyncDirection,
) -> Result<SyncState> {
    if storage.state(appid) == SyncState::NotInitialized {
        storage.load_local_cache(appid);
        wait_until(engine, Duration::from_secs(30), || {
            storage.state(appid) != SyncState::NotInitialized
        })
        .context("Steam didn't load the game's local cloud files")?;
    }
    wait_until(engine, SYNC_TIMEOUT, || !storage.in_progress(appid))?;
    if !storage.synchronize(appid, direction) {
        let state = storage.state(appid);
        tracing::debug!(appid, ?direction, ?state, "sync refused");
        return Ok(state);
    }
    // Give the sync a moment to register before waiting on it.
    wait_until(engine, Duration::from_millis(500), || false).ok();
    wait_until(engine, SYNC_TIMEOUT, || !storage.in_progress(appid))
        .context("cloud sync didn't finish")?;
    Ok(storage.state(appid))
}

/// How long moving one game's cloud saves may take.
const SYNC_TIMEOUT: Duration = Duration::from_secs(300);

/// Run engine frames until `done()`; an error if `limit` passes first.
fn wait_until(engine: &Engine, limit: Duration, done: impl Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out after {} s", limit.as_secs());
        }
        frame(engine);
        std::thread::sleep(FRAME);
    }
}

/// One engine frame: let it work and drain its callbacks.
fn frame(engine: &Engine) {
    engine.run_frame();
    engine.drain_callbacks(|id, payload| match id {
        103 => tracing::info!("disconnected from Steam; the engine will reconnect"),
        _ => tracing::trace!(id, len = payload.len(), "engine callback"),
    });
}

/// `LicensesUpdated_t`: the account's licenses have arrived. Until then
/// the engine refuses to launch anything ("missing license info").
const LICENSES_UPDATED: i32 = 125;

fn wait_for_licenses(engine: &Engine) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut arrived = false;
    while !arrived && Instant::now() < deadline {
        engine.run_frame();
        engine.drain_callbacks(|id, _| arrived |= id == LICENSES_UPDATED);
        std::thread::sleep(FRAME);
    }
    if !arrived {
        tracing::warn!("Steam didn't send the account's licenses within 30 s");
    }
}

#[cfg(unix)]
mod signals {
    /// Interrupt, terminate, hangup: how a terminal or `kill` stops fumes.
    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    pub struct Saved(Vec<(libc::c_int, libc::sigaction)>);

    pub fn save() -> Saved {
        Saved(
            SIGNALS
                .iter()
                .filter_map(|&signal| unsafe {
                    let mut action: libc::sigaction = std::mem::zeroed();
                    (libc::sigaction(signal, std::ptr::null(), &mut action) == 0)
                        .then_some((signal, action))
                })
                .collect(),
        )
    }

    pub fn restore(saved: &Saved) {
        for (signal, action) in &saved.0 {
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(not(unix))]
mod signals {
    pub struct Saved;
    pub fn save() -> Saved {
        Saved
    }
    pub fn restore(_: &Saved) {}
}

/// Steam's anonymous user: account 1, public universe, type AnonUser (10),
/// default instance (1).
const ANONYMOUS_STEAM_ID: u64 = (1 << 56) | (10 << 52) | (1 << 32) | 1;

/// Start a logon and wait for it to finish.
fn log_on(engine: &Engine, user: &ClientUser, steam_id: u64) -> Result<()> {
    let result = user.log_on(steam_id);
    if result != 1 {
        bail!("the Steam engine refused to log on (EResult {result})");
    }
    let mut failure = None;
    let deadline = Instant::now() + LOGON_TIMEOUT;
    while Instant::now() < deadline {
        engine.run_frame();
        engine.drain_callbacks(|id, payload| {
            // SteamServerConnectFailure_t: EResult first.
            if id == 102 {
                failure = payload
                    .get(..4)
                    .map(|b| i32::from_le_bytes(b.try_into().unwrap()));
            }
        });
        if user.logged_on() {
            return Ok(());
        }
        if let Some(code) = failure {
            bail!("Steam rejected the login (EResult {code}); try `fumes login` again");
        }
        std::thread::sleep(FRAME);
    }
    bail!(
        "couldn't log in to Steam within {} s (logon state {})",
        LOGON_TIMEOUT.as_secs(),
        user.logon_state()
    )
}

/// How often the engine gets a frame (what OpenSteamClient uses).
const FRAME: Duration = Duration::from_millis(15);

/// Run the engine's frame and drain callbacks until `done()` or `limit`.
fn pump(engine: &Engine, limit: Duration, done: impl Fn() -> bool) {
    let start = Instant::now();
    while start.elapsed() < limit && !done() {
        frame(engine);
        std::thread::sleep(FRAME);
    }
}

/// The SteamID is the refresh token's `sub` claim.
fn steam_id_from_jwt(token: &str) -> Result<u64> {
    let payload = token
        .split('.')
        .nth(1)
        .context("the session token isn't a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("the session token is malformed")?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes)?;
    claims["sub"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .context("the session token names no SteamID")
}

/// Where the engine keeps its own data (config, logs, its list of library
/// folders). Only known for sure on macOS, where it uses Steam's usual
/// folder under `HOME`.
pub fn data_dir() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join("Library/Application Support/Steam"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_steam_id_from_token() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"iss":"steam","sub":"76561197960287930","aud":["client","web"]}"#);
        let token = format!("eyJhbGciOiJFZERTQSJ9.{payload}.sig");
        assert_eq!(steam_id_from_jwt(&token).unwrap(), 76561197960287930);
        assert!(steam_id_from_jwt("nope").is_err());
    }

    #[test]
    fn anonymous_id_is_steams() {
        assert_eq!(ANONYMOUS_STEAM_ID, 117093594606600193);
    }
}
