//! The library: games the account owns, joined with what's installed here.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetOwnedGames_Request;

use crate::local::Installed;

/// One owned game as Steam reports it. This is what gets cached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Owned {
    pub appid: u32,
    pub name: String,
    pub playtime_minutes: u32,
    /// Unix seconds, 0 if never played.
    pub last_played: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Game {
    pub appid: u32,
    pub name: String,
    pub owned: Option<Owned>,
    pub installed: Option<Installed>,
}

/// `IPlayerService.GetOwnedGames` over the CM connection, the same call the
/// Web API exposes but authenticated by the session instead of an API key.
pub async fn fetch_owned(connection: &Connection) -> Result<Vec<Owned>> {
    let req = CPlayer_GetOwnedGames_Request {
        steamid: Some(connection.steam_id().into()),
        include_appinfo: Some(true),
        include_played_free_games: Some(true),
        ..Default::default()
    };
    let res = connection.service_method(req).await?;
    Ok(res
        .games
        .iter()
        .map(|g| Owned {
            appid: g.appid() as u32,
            name: g.name().to_owned(),
            playtime_minutes: g.playtime_forever() as u32,
            last_played: g.rtime_last_played(),
        })
        .collect())
}

pub fn load_cache(path: &Path) -> Option<Vec<Owned>> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

pub fn save_cache(path: &Path, owned: &[Owned]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec(owned)?)?;
    Ok(())
}

/// Join by app id. Installed games the account doesn't list (family-shared
/// games, tools like the Steamworks redistributables) are kept: they're on
/// disk either way.
pub fn merge(owned: Vec<Owned>, installed: Vec<Installed>) -> Vec<Game> {
    let mut games: BTreeMap<u32, Game> = BTreeMap::new();
    for o in owned {
        games.insert(
            o.appid,
            Game {
                appid: o.appid,
                name: o.name.clone(),
                owned: Some(o),
                installed: None,
            },
        );
    }
    for i in installed {
        let game = games.entry(i.appid).or_insert_with(|| Game {
            appid: i.appid,
            name: i.name.clone(),
            owned: None,
            installed: None,
        });
        if game.name.is_empty() {
            game.name = i.name.clone();
        }
        game.installed = Some(i);
    }
    let mut games: Vec<Game> = games.into_values().collect();
    games.sort_by_cached_key(|g| g.name.to_lowercase());
    games
}

/// Find a game by app id, exact name, or a unique part of its name
/// (all case-insensitive).
pub fn find<'a>(games: &'a [Game], query: &str) -> Result<&'a Game> {
    if let Ok(appid) = query.parse::<u32>()
        && let Some(game) = games.iter().find(|g| g.appid == appid)
    {
        return Ok(game);
    }
    let q = query.to_lowercase();
    if let Some(game) = games.iter().find(|g| g.name.to_lowercase() == q) {
        return Ok(game);
    }
    let matches: Vec<&Game> = games
        .iter()
        .filter(|g| g.name.to_lowercase().contains(&q))
        .collect();
    match matches.as_slice() {
        [game] => Ok(game),
        [] => bail!("no game matches {query:?}"),
        many => {
            let list: Vec<String> = many
                .iter()
                .take(10)
                .map(|g| format!("  {:>8}  {}", g.appid, g.name))
                .collect();
            bail!(
                "{query:?} matches {} games; use the app id or a longer name:\n{}",
                many.len(),
                list.join("\n")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn owned(appid: u32, name: &str) -> Owned {
        Owned {
            appid,
            name: name.into(),
            playtime_minutes: 0,
            last_played: 0,
        }
    }

    fn installed(appid: u32, name: &str) -> Installed {
        Installed {
            appid,
            name: name.into(),
            path: PathBuf::from("/x"),
            size_on_disk: 0,
            complete: true,
            by_fumes: false,
        }
    }

    #[test]
    fn merges_by_appid_and_keeps_unowned_installs() {
        let games = merge(
            vec![
                owned(12120, "Grand Theft Auto: San Andreas"),
                owned(440, "Team Fortress 2"),
            ],
            vec![
                installed(440, "Team Fortress 2"),
                installed(228980, "Steamworks Common Redistributables"),
            ],
        );
        let names: Vec<&str> = games.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Grand Theft Auto: San Andreas",
                "Steamworks Common Redistributables",
                "Team Fortress 2"
            ]
        );
        assert!(games[2].owned.is_some() && games[2].installed.is_some());
        assert!(games[1].owned.is_none());
        assert!(games[0].installed.is_none());
    }

    #[test]
    fn finds_by_id_exact_name_or_unique_substring() {
        let games = merge(
            vec![
                owned(12120, "Grand Theft Auto: San Andreas"),
                owned(12210, "Grand Theft Auto IV"),
                owned(570, "Dota 2"),
            ],
            vec![],
        );
        assert_eq!(find(&games, "12210").unwrap().appid, 12210);
        assert_eq!(find(&games, "dota 2").unwrap().appid, 570);
        assert_eq!(find(&games, "san andreas").unwrap().appid, 12120);
        let err = find(&games, "grand theft").unwrap_err().to_string();
        assert!(err.contains("matches 2 games"), "{err}");
        assert!(find(&games, "half-life").is_err());
    }
}
