//! What Steam says about an app: its depots, launch options and DLC.
//!
//! This is PICS ("product info"), the same data SteamDB shows. Owned apps
//! need an access token first; with it the response carries the app's full
//! KeyValues, usually inline and sometimes as a link to fetch over HTTP when
//! it's large.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::steammessages_clientserver::{
    CMsgClientGetAppOwnershipTicket, CMsgClientGetAppOwnershipTicketResponse,
};
use steam_vent_proto::steammessages_clientserver_appinfo::{
    CMsgClientPICSAccessTokenRequest, CMsgClientPICSAccessTokenResponse,
    CMsgClientPICSProductInfoRequest, CMsgClientPICSProductInfoResponse,
    cmsg_client_picsproduct_info_request,
};
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetPlayerLinkDetails_Request;

use crate::kv;

#[derive(Debug, Clone, PartialEq)]
pub struct AppInfo {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
    /// Platforms the app runs on (`windows`, `macos`, `linux`).
    pub oslist: Vec<String>,
    pub depots: Vec<Depot>,
    pub launch: Vec<Launch>,
    /// Every DLC the store lists for this app, owned or not.
    pub dlcs: Vec<u32>,
    pub buildid: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Depot {
    pub id: u32,
    pub name: String,
    pub oslist: Vec<String>,
    pub osarch: Option<String>,
    pub language: Option<String>,
    pub lowviolence: bool,
    /// Manifest of the public branch.
    pub manifest: Option<u64>,
    pub size: u64,
    /// Set when the depot belongs to a DLC; only owners can download it.
    pub dlcappid: Option<u32>,
    /// The depot's manifests live in another app's info.
    pub depotfromapp: Option<u32>,
    /// Redistributables Steam installs once for every game that needs them.
    pub sharedinstall: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Launch {
    pub executable: String,
    pub arguments: String,
    pub workingdir: String,
    pub description: String,
    /// `default`, `option1`, `server`, `editor`, … (empty means default).
    pub kind: String,
    pub oslist: Vec<String>,
    pub osarch: Option<String>,
    /// Only offered on this beta branch.
    pub betakey: Option<String>,
}

/// The app's full info. Fails if the account can't see it.
pub async fn fetch(connection: &Connection, appid: u32) -> Result<AppInfo> {
    let mut texts = fetch_raw(connection, &[appid]).await?;
    let text = texts
        .remove(&appid)
        .with_context(|| format!("Steam returned no info for app {appid}"))?;
    parse(appid, &text).with_context(|| format!("app {appid}'s info is malformed"))
}

/// Names of several apps (DLCs, mostly). Apps Steam won't describe are left
/// out.
pub async fn names(connection: &Connection, appids: &[u32]) -> Result<BTreeMap<u32, String>> {
    if appids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let texts = fetch_raw(connection, appids).await?;
    Ok(texts
        .into_iter()
        .filter_map(|(appid, text)| {
            let vdf = kv::parse(&text)?;
            let common = kv::get_obj(vdf.value.get_obj()?, "common")?;
            Some((appid, kv::get_str(common, "name")?.to_owned()))
        })
        .collect())
}

async fn fetch_raw(connection: &Connection, appids: &[u32]) -> Result<BTreeMap<u32, String>> {
    let tokens: CMsgClientPICSAccessTokenResponse = connection
        .job(CMsgClientPICSAccessTokenRequest {
            appids: appids.to_vec(),
            ..Default::default()
        })
        .await
        .context("PICS access token request failed")?;
    let token = |appid: u32| {
        tokens
            .app_access_tokens
            .iter()
            .find(|t| t.appid() == appid)
            .map(|t| t.access_token())
            .unwrap_or(0)
    };

    let request = CMsgClientPICSProductInfoRequest {
        apps: appids
            .iter()
            .map(|&appid| cmsg_client_picsproduct_info_request::AppInfo {
                appid: Some(appid),
                access_token: Some(token(appid)),
                ..Default::default()
            })
            .collect(),
        meta_data_only: Some(false),
        ..Default::default()
    };
    let responses: Vec<CMsgClientPICSProductInfoResponse> = connection
        .job_multi(request)
        .try_collect()
        .await
        .context("PICS product info request failed")?;

    let mut texts = BTreeMap::new();
    for response in &responses {
        for app in &response.apps {
            let bytes = match app.buffer.as_deref() {
                Some(buffer) if !buffer.is_empty() => buffer.to_vec(),
                // Too big to send inline: Steam names a host to fetch it from.
                _ if response.has_http_host() && !app.sha().is_empty() => {
                    fetch_over_http(response.http_host(), app.appid(), app.sha()).await?
                }
                _ => continue,
            };
            let text = String::from_utf8_lossy(&bytes);
            texts.insert(app.appid(), text.trim_end_matches('\0').to_owned());
        }
    }
    Ok(texts)
}

async fn fetch_over_http(host: &str, appid: u32, sha: &[u8]) -> Result<Vec<u8>> {
    let url = format!(
        "https://{host}/appinfo/{appid}/sha/{}.txt.gz",
        hex::encode(sha)
    );
    let gz = reqwest::get(&url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("fetching {url}"))?
        .bytes()
        .await?;
    let mut text = Vec::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut text)?;
    Ok(text)
}

/// Whether the account has a license for the app. Steam only hands out an
/// ownership ticket to owners.
pub async fn owns(connection: &Connection, appid: u32) -> Result<bool> {
    let response: CMsgClientGetAppOwnershipTicketResponse = connection
        .job(CMsgClientGetAppOwnershipTicket {
            app_id: Some(appid),
            ..Default::default()
        })
        .await
        .with_context(|| format!("ownership check for app {appid} failed"))?;
    Ok(response.eresult() == 1)
}

/// The subset of `appids` the account owns, checked a few at a time.
pub async fn owned_of(connection: &Connection, appids: &[u32]) -> Result<Vec<u32>> {
    let checks: Vec<(u32, bool)> = stream::iter(appids.iter().copied())
        .map(|appid| async move { Ok::<_, anyhow::Error>((appid, owns(connection, appid).await?)) })
        .buffered(8)
        .try_collect()
        .await?;
    Ok(checks
        .into_iter()
        .filter(|(_, owned)| *owned)
        .map(|(appid, _)| appid)
        .collect())
}

/// The account's public display name, which is what games should show
/// (the login name is a credential and stays private).
pub async fn persona_name(connection: &Connection) -> Result<String> {
    let response = connection
        .service_method(CPlayer_GetPlayerLinkDetails_Request {
            steamids: vec![connection.steam_id().into()],
            ..Default::default()
        })
        .await?;
    let name = response
        .accounts
        .first()
        .map(|a| a.public_data.persona_name().to_owned())
        .unwrap_or_default();
    if name.is_empty() {
        bail!("Steam returned no persona name");
    }
    Ok(name)
}

pub fn parse(appid: u32, text: &str) -> Option<AppInfo> {
    let vdf = kv::parse(text)?;
    let root = vdf.value.get_obj()?;
    let common = kv::get_obj(root, "common");
    let config = kv::get_obj(root, "config");
    let extended = kv::get_obj(root, "extended");
    let depots = kv::get_obj(root, "depots");

    let name = common
        .and_then(|c| kv::get_str(c, "name"))
        .unwrap_or_default()
        .to_owned();
    let installdir = config
        .and_then(|c| kv::get_str(c, "installdir"))
        .filter(|d| !d.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| appid.to_string());

    let launch = config
        .and_then(|c| kv::get_obj(c, "launch"))
        .map(|l| {
            numbered(l)
                .filter_map(|(_, entry)| parse_launch(entry))
                .collect()
        })
        .unwrap_or_default();

    // `listofdlc` is the store's list; DLC depots with `dlcappid` can name
    // DLCs it leaves out.
    let mut dlcs: Vec<u32> = extended
        .and_then(|e| kv::get_str(e, "listofdlc"))
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let depot_list: Vec<Depot> = depots
        .map(|d| {
            numbered(d)
                .filter_map(|(id, entry)| parse_depot(id, entry))
                .collect()
        })
        .unwrap_or_default();
    dlcs.extend(depot_list.iter().filter_map(|d| d.dlcappid));
    dlcs.sort_unstable();
    dlcs.dedup();

    let buildid = depots
        .and_then(|d| kv::get_obj(d, "branches"))
        .and_then(|b| kv::get_obj(b, "public"))
        .and_then(|p| kv::get_num(p, "buildid"))
        .unwrap_or(0);

    Some(AppInfo {
        appid,
        name,
        installdir,
        oslist: common
            .map(|c| kv::get_list(c, "oslist"))
            .unwrap_or_default(),
        depots: depot_list,
        launch,
        dlcs,
        buildid,
    })
}

/// Children keyed by number (`"0" {…}`, `"228981" {…}`), skipping the
/// named siblings Steam mixes in (`branches`, `baselanguages`, …).
fn numbered<'a>(
    obj: &'a keyvalues_parser::Obj<'a>,
) -> impl Iterator<Item = (u32, &'a keyvalues_parser::Obj<'a>)> {
    obj.iter().filter_map(|(key, values)| {
        let id = key.parse().ok()?;
        Some((id, values.first()?.get_obj()?))
    })
}

fn parse_launch(entry: &keyvalues_parser::Obj) -> Option<Launch> {
    let executable = kv::get_str(entry, "executable")?.trim();
    if executable.is_empty() {
        return None;
    }
    let config = kv::get_obj(entry, "config");
    Some(Launch {
        executable: executable.to_owned(),
        arguments: kv::get_str(entry, "arguments")
            .unwrap_or_default()
            .to_owned(),
        workingdir: kv::get_str(entry, "workingdir")
            .unwrap_or_default()
            .to_owned(),
        description: kv::get_str(entry, "description")
            .unwrap_or_default()
            .to_owned(),
        kind: kv::get_str(entry, "type")
            .unwrap_or_default()
            .to_lowercase(),
        oslist: config
            .map(|c| kv::get_list(c, "oslist"))
            .unwrap_or_default(),
        osarch: config
            .and_then(|c| kv::get_str(c, "osarch"))
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        betakey: config
            .and_then(|c| kv::get_str(c, "betakey"))
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    })
}

fn parse_depot(id: u32, entry: &keyvalues_parser::Obj) -> Option<Depot> {
    let config = kv::get_obj(entry, "config");
    let conf_str = |key: &str| {
        config
            .and_then(|c| kv::get_str(c, key))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };

    // Newer info: `manifests { public { gid … size … } }`.
    // Older info: `manifests { public "<gid>" }` with the size in `maxsize`.
    let public = kv::get_obj(entry, "manifests").and_then(|m| {
        m.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("public"))
            .and_then(|(_, v)| v.first())
    });
    let (manifest, size) = match public {
        Some(keyvalues_parser::Value::Obj(p)) => (kv::get_num(p, "gid"), kv::get_num(p, "size")),
        Some(keyvalues_parser::Value::Str(gid)) => (gid.trim().parse().ok(), None),
        None => (None, None),
    };

    Some(Depot {
        id,
        name: kv::get_str(entry, "name").unwrap_or_default().to_owned(),
        oslist: config
            .map(|c| kv::get_list(c, "oslist"))
            .unwrap_or_default(),
        osarch: conf_str("osarch"),
        language: conf_str("language"),
        lowviolence: conf_str("lowviolence").as_deref() == Some("1"),
        manifest,
        size: size.or_else(|| kv::get_num(entry, "maxsize")).unwrap_or(0),
        dlcappid: kv::get_num(entry, "dlcappid"),
        depotfromapp: kv::get_num(entry, "depotfromapp"),
        sharedinstall: kv::get_str(entry, "sharedinstall") == Some("1"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real app's info, with both manifest formats, a DLC
    /// depot, a shared redistributable and per-OS launch entries.
    pub const SAMPLE: &str = r#""appinfo"
{
	"appid"		"4000"
	"common"
	{
		"name"		"Some Game"
		"type"		"Game"
		"oslist"		"windows,macos,linux"
	}
	"extended"
	{
		"listofdlc"		"4010,4020"
	}
	"config"
	{
		"installdir"		"SomeGame"
		"launch"
		{
			"0"
			{
				"executable"		"bin\\win64\\game.exe"
				"arguments"		"-steam -novid"
				"type"		"default"
				"config"
				{
					"oslist"		"windows"
					"osarch"		"64"
				}
			}
			"1"
			{
				"executable"		"SomeGame.app"
				"config"
				{
					"oslist"		"macos"
				}
			}
			"2"
			{
				"executable"		"game.sh"
				"description"		"Linux"
				"config"
				{
					"oslist"		"linux"
				}
			}
		}
	}
	"depots"
	{
		"228988"
		{
			"depotfromapp"		"228980"
			"sharedinstall"		"1"
		}
		"4001"
		{
			"name"		"Some Game Content"
			"manifests"
			{
				"public"
				{
					"gid"		"1234567890123456789"
					"size"		"1048576"
					"download"		"524288"
				}
			}
		}
		"4002"
		{
			"config"
			{
				"oslist"		"windows"
				"osarch"		"64"
			}
			"manifests"
			{
				"public"		"987654321"
			}
			"maxsize"		"2048"
		}
		"4011"
		{
			"dlcappid"		"4030"
			"manifests"
			{
				"public"		"5555"
			}
		}
		"branches"
		{
			"public"
			{
				"buildid"		"777"
			}
		}
	}
}
"#;

    #[test]
    fn parses_depots_launch_and_dlc() {
        let info = parse(4000, SAMPLE).unwrap();
        assert_eq!(info.name, "Some Game");
        assert_eq!(info.installdir, "SomeGame");
        assert_eq!(info.oslist, ["windows", "macos", "linux"]);
        assert_eq!(info.buildid, 777);
        // listofdlc plus the DLC named only by a depot.
        assert_eq!(info.dlcs, [4010, 4020, 4030]);

        assert_eq!(info.depots.len(), 4);
        let redist = &info.depots[0];
        assert!(redist.sharedinstall && redist.manifest.is_none());
        assert_eq!(redist.depotfromapp, Some(228980));
        let content = &info.depots[1];
        assert_eq!(content.manifest, Some(1234567890123456789));
        assert_eq!(content.size, 1048576);
        let win = &info.depots[2];
        assert_eq!(win.manifest, Some(987654321));
        assert_eq!(win.size, 2048);
        assert_eq!(win.oslist, ["windows"]);
        assert_eq!(win.osarch.as_deref(), Some("64"));
        assert_eq!(info.depots[3].dlcappid, Some(4030));

        assert_eq!(info.launch.len(), 3);
        assert_eq!(info.launch[0].executable, r"bin\win64\game.exe");
        assert_eq!(info.launch[0].arguments, "-steam -novid");
        assert_eq!(info.launch[1].oslist, ["macos"]);
    }

    #[test]
    fn reads_unescaped_windows_paths() {
        let text = "\"appinfo\" { \"config\" { \"launch\" { \"0\" { \"executable\" \"bin\\win32\\game.exe\" } } } }";
        let info = parse(1, text).unwrap();
        assert_eq!(info.launch[0].executable, r"bin\win32\game.exe");
        // No installdir: fall back to the app id like Steam does.
        assert_eq!(info.installdir, "1");
    }
}
