//! Signing in to Steam and keeping the session between runs.
//!
//! Only the refresh token is stored, never the password. Steam issues it on a
//! password login; it is a JWT valid for months and is enough to start new
//! sessions without asking again. Steam may rotate it on use, so whatever the
//! server hands back replaces the stored one.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use steam_vent::auth::{
    ClientInfo, ConsoleAuthConfirmationHandler, DeviceConfirmationHandler, FileGuardDataStore, Os,
    RefreshToken,
};
use steam_vent::{Connection, ServerList};

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

    let servers = ServerList::discover().await?;
    let connection = Connection::login(
        &servers,
        &account,
        &password,
        // The machine token lets Steam skip the Guard prompt on later
        // password logins from this device.
        FileGuardDataStore::new(dirs.data.join("machine_tokens.json")),
        (
            ConsoleAuthConfirmationHandler::default(),
            DeviceConfirmationHandler,
        ),
        &client_info(),
    )
    .await
    .context("login failed")?;

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
