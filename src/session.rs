//! Signing in to Steam and keeping the session between runs.
//!
//! Only the refresh token is stored, never the password. Steam issues it on a
//! password login; it is a JWT valid for months and is enough to start new
//! sessions without asking again. Steam may rotate it on use, so whatever the
//! server hands back replaces the stored one.

use std::fs;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use steam_vent::auth::{
    AuthConfirmationHandler, ClientInfo, ConfirmationAction, ConfirmationMethod,
    ConfirmationMethodClass, FileGuardDataStore, Os, RefreshToken,
    UserProvidedAuthConfirmationHandler,
};
use steam_vent::{Connection, ServerList};
use tokio::io::{AsyncWriteExt, DuplexStream, Sink};

use crate::dirs::Dirs;

#[derive(Serialize, Deserialize)]
struct SavedSession {
    account: String,
    refresh_token: String,
}

/// Interactive password login. Steam Guard is handled either by typing the
/// emailed/authenticator code or by approving the sign-in in the mobile app,
/// whichever comes first.
pub async fn login(dirs: &Dirs) -> Result<()> {
    let account = prompt("Steam account name: ")?;
    let password = rpassword::prompt_password("Password: ")?;

    println!("Signing in…");
    let servers = ServerList::discover().await?;
    let guard = GuardPrompt::new();
    let waiting = guard.waiting.clone();
    let connection = Connection::login(
        &servers,
        &account,
        &password,
        // The machine token lets Steam skip the Guard prompt on later
        // password logins from this device.
        FileGuardDataStore::new(dirs.data.join("machine_tokens.json")),
        guard,
        &client_info(),
    )
    .await
    .context("login failed")?;
    if waiting.load(Ordering::Relaxed) {
        // Approved in the app while the code prompt was up.
        println!();
    }

    save(
        &dirs.session_file(),
        &SavedSession {
            account: account.clone(),
            refresh_token: connection.refresh_token().token().to_owned(),
        },
    )?;
    println!("Signed in as {account}.");
    Ok(())
}

pub fn logout(dirs: &Dirs) -> Result<()> {
    match fs::remove_file(dirs.session_file()) {
        Ok(()) => println!("Signed out."),
        Err(e) if e.kind() == io::ErrorKind::NotFound => println!("Not signed in."),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Open an authenticated connection from the stored refresh token.
pub async fn connect(dirs: &Dirs) -> Result<Connection> {
    let path = dirs.session_file();
    let mut saved = load(&path)?;
    let token = valid_token(&saved)?;

    let servers = ServerList::discover().await?;
    let connection = Connection::login_with_refresh_token(&servers, &token)
        .await
        .context("could not resume the session; run `fumes login`")?;

    if connection.refresh_token().token() != saved.refresh_token {
        saved.refresh_token = connection.refresh_token().token().to_owned();
        save(&path, &saved)?;
    }
    Ok(connection)
}

/// The account name and refresh token, for the Steam engine's own login
/// (no network).
pub fn credentials(dirs: &Dirs) -> Result<(String, String)> {
    let saved = load(&dirs.session_file())?;
    valid_token(&saved)?;
    Ok((saved.account, saved.refresh_token))
}

/// Who's signed in: the account name and SteamID64 (no network). `None`
/// without a session, or once it has expired.
pub fn signed_in(dirs: &Dirs) -> Option<(String, u64)> {
    let saved = load(&dirs.session_file()).ok()?;
    let token = valid_token(&saved).ok()?;
    Some((saved.account, token.subject.into()))
}

fn valid_token(saved: &SavedSession) -> Result<RefreshToken> {
    let token = RefreshToken::new(saved.refresh_token.clone())?;
    if token.expired() {
        bail!("session expired; run `fumes login`");
    }
    Ok(token)
}

fn load(path: &Path) -> Result<SavedSession> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("session file is corrupt"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!("not signed in; run `fumes login`"),
        Err(e) => Err(e.into()),
    }
}

/// How this device shows up in the account's "authorized devices" list.
fn client_info() -> ClientInfo {
    let mut info = ClientInfo::new("fumes".into());
    info.os = if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else {
        Os::Windows
    };
    info
}

fn save(path: &Path, session: &SavedSession) -> Result<()> {
    let json = serde_json::to_vec_pretty(session)?;
    write_private(path, &json).with_context(|| format!("writing {}", path.display()))
}

/// The token is as good as a password until it expires, so keep it readable
/// by the owner only.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp: PathBuf = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&tmp)?.write_all(bytes)?;
    fs::rename(tmp, path)
}

/// Asks for a Steam Guard code in plain words, while a sign-in approved
/// in the Steam mobile app goes through by itself (Steam is polled for
/// that the whole time).
///
/// steam-vent's console handler would do the asking, but it reads the
/// terminal on the async runtime, which then can't shut down until a line
/// arrives: after an approval in the app, fumes would sit there until
/// Enter. Here a plain thread reads the terminal and hands lines over, and
/// is simply left behind when fumes exits.
struct GuardPrompt {
    codes: UserProvidedAuthConfirmationHandler<DuplexStream, Sink>,
    /// A prompt is on screen and nothing has been typed yet.
    waiting: Arc<AtomicBool>,
}

impl GuardPrompt {
    fn new() -> GuardPrompt {
        let (lines, mut feed) = tokio::io::duplex(256);
        let waiting = Arc::new(AtomicBool::new(false));
        let runtime = tokio::runtime::Handle::current();
        let typed = waiting.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { return };
                typed.store(false, Ordering::Relaxed);
                let line = format!("{line}\n");
                if runtime.block_on(feed.write_all(line.as_bytes())).is_err() {
                    return;
                }
            }
        });
        GuardPrompt {
            // Its own description of what's needed is replaced by ours.
            codes: UserProvidedAuthConfirmationHandler::new(lines, tokio::io::sink()),
            waiting,
        }
    }
}

impl AuthConfirmationHandler for GuardPrompt {
    fn handle_confirmation<'this>(
        &'this mut self,
        allowed: &[ConfirmationMethod],
    ) -> Box<dyn Future<Output = Option<ConfirmationAction>> + 'this> {
        let in_app = allowed
            .iter()
            .any(|m| m.class() == ConfirmationMethodClass::Confirmation);
        let code = allowed.iter().find(|m| m.token_type().is_some());
        let ask = match code.map(|m| (m.confirmation_type(), m.confirmation_details())) {
            Some(("email", to)) => format!("Steam Guard: type the code Steam emailed to {to}"),
            Some(_) if in_app => {
                "Steam Guard: approve this sign-in in the Steam mobile app, or type the code it shows"
                    .to_owned()
            }
            Some(_) => "Steam Guard: type the code from the Steam mobile app".to_owned(),
            None if in_app => {
                println!("Steam Guard: approve this sign-in in the Steam mobile app…");
                return Box::new(async { Some(ConfirmationAction::None) });
            }
            None => return Box::new(async { None }),
        };
        print!("{ask} (Enter alone cancels): ");
        io::stdout().flush().ok();
        self.waiting.store(true, Ordering::Relaxed);
        self.codes.handle_confirmation(allowed)
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let line = line.trim().to_owned();
    if line.is_empty() {
        bail!("no input");
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the network: `cargo test -- --ignored`. Stops at the encrypted
    /// handshake: anonymous logons are broken at the pinned steam-vent
    /// commit (`send_logon` demands a refresh token they don't have).
    #[tokio::test]
    #[ignore]
    async fn reaches_steam() {
        let servers = ServerList::discover().await.unwrap();
        steam_vent::connection::UnAuthenticatedConnection::connect(&servers)
            .await
            .unwrap();
    }
}
