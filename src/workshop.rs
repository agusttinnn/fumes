//! Steam Workshop: the items you're subscribed to for a game (on the
//! Workshop's web pages), kept downloaded as Steam keeps them.
//!
//! Items go where Steam puts them, `steamapps/workshop/content/<appid>/
//! <item>/` in the game's library, and are recorded in
//! `steamapps/workshop/appworkshop_<appid>.acf`. That record is what the
//! engine reads to tell the game (through `ISteamUGC`) which items are
//! installed and where. Most items are a depot manifest in the game's
//! Workshop depot, downloaded like a game; old ones are one file at a URL.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetUserFiles_Request, PublishedFileDetails,
};

use crate::cdn::{self, Cdn, DepotKey};
use crate::dirs::Dirs;
use crate::download::{self, human};
use crate::installs::Install;
use crate::tui::game::{report, report_problem};
use crate::{appinfo, kv, local};

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: u64,
    pub title: String,
    pub size: u64,
    /// Unix time of its latest version.
    pub updated: u32,
    /// Its depot manifest; 0 for an old single-file item.
    pub manifest: u64,
    /// Where an old single-file item is downloaded from, and its name.
    pub file_url: String,
    pub filename: String,
}

impl Item {
    fn from_details(details: &PublishedFileDetails) -> Option<Item> {
        // Deleted, hidden or banned items come back with an error result.
        if details.result() != 1 || details.publishedfileid() == 0 {
            return None;
        }
        Some(Item {
            id: details.publishedfileid(),
            title: details.title().trim().to_owned(),
            size: details.file_size(),
            updated: details.time_updated(),
            manifest: details.hcontent_file(),
            file_url: details.file_url().to_owned(),
            filename: details.filename().to_owned(),
        })
    }
}

/// A game's Workshop on the web, where items are found and subscribed to.
pub fn page(appid: u32) -> String {
    format!("https://steamcommunity.com/app/{appid}/workshop/")
}

const PER_PAGE: u32 = 100;
/// The items this account is subscribed to for `appid`.
pub async fn subscribed(connection: &Connection, appid: u32) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for page in 1.. {
        let response = connection
            .service_method(CPublishedFile_GetUserFiles_Request {
                steamid: Some(connection.steam_id().into()),
                appid: Some(appid),
                page: Some(page),
                numperpage: Some(PER_PAGE),
                type_: Some("mysubscriptions".into()),
                ..Default::default()
            })
            .await
            .context("listing your Workshop subscriptions failed")?;
        let got = response.publishedfiledetails.len();
        items.extend(
            response
                .publishedfiledetails
                .iter()
                .filter_map(Item::from_details),
        );
        if got < PER_PAGE as usize || page * PER_PAGE >= response.total() {
            break;
        }
    }
    Ok(items)
}

/// The library folder (`steamapps`) whose `workshop` holds a game's items:
/// the game's own, or fumes' for a game installed elsewhere.
pub fn steamapps_for(dirs: &Dirs, install: &Install) -> PathBuf {
    local::steamapps_of(&install.path)
        .map(|(steamapps, _)| steamapps)
        .unwrap_or_else(|| dirs.library().join("steamapps"))
}

/// Where a game's items go: `steamapps/workshop` of its library.
pub fn workshop_dir(steamapps: &Path) -> PathBuf {
    steamapps.join("workshop")
}

pub fn item_dir(steamapps: &Path, appid: u32, item: u64) -> PathBuf {
    workshop_dir(steamapps)
        .join("content")
        .join(appid.to_string())
        .join(item.to_string())
}

fn record_path(steamapps: &Path, appid: u32) -> PathBuf {
    workshop_dir(steamapps).join(format!("appworkshop_{appid}.acf"))
}

/// fumes' record of an item's files, outside its folder.
fn state_path(steamapps: &Path, appid: u32, item: u64) -> PathBuf {
    workshop_dir(steamapps)
        .join(".fumes")
        .join(appid.to_string())
        .join(format!("{item}.json"))
}

/// What `appworkshop_<appid>.acf` says is on disk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Installed {
    pub size: u64,
    pub updated: u32,
    pub manifest: u64,
}

pub fn installed(steamapps: &Path, appid: u32) -> BTreeMap<u64, Installed> {
    fs::read_to_string(record_path(steamapps, appid))
        .ok()
        .map(|text| parse_record(&text))
        .unwrap_or_default()
}

fn parse_record(text: &str) -> BTreeMap<u64, Installed> {
    let Some(vdf) = kv::parse(text) else {
        return BTreeMap::new();
    };
    let Some(items) = vdf
        .value
        .get_obj()
        .and_then(|root| kv::get_obj(root, "WorkshopItemsInstalled"))
    else {
        return BTreeMap::new();
    };
    items
        .iter()
        .filter_map(|(key, values)| {
            let id = key.parse().ok()?;
            let entry = values.first()?.get_obj()?;
            Some((
                id,
                Installed {
                    size: kv::get_num(entry, "size").unwrap_or(0),
                    updated: kv::get_num(entry, "timeupdated").unwrap_or(0),
                    manifest: kv::get_num(entry, "manifest").unwrap_or(0),
                },
            ))
        })
        .collect()
}

/// `appworkshop_<appid>.acf` as Steam writes it for items that are all
/// downloaded and up to date. `account` is the subscriber's account id.
fn record(appid: u32, account: u32, items: &BTreeMap<u64, Installed>, now: u64) -> String {
    let size: u64 = items.values().map(|i| i.size).sum();
    let mut text = String::from("\"AppWorkshop\"\n{\n");
    for (key, value) in [
        ("appid", appid.to_string()),
        ("SizeOnDisk", size.to_string()),
        ("NeedsUpdate", "0".into()),
        ("NeedsDownload", "0".into()),
        ("TimeLastUpdated", now.to_string()),
        ("TimeLastAppRan", "0".into()),
        ("LastBuildID", "0".into()),
    ] {
        let _ = writeln!(text, "\t\"{key}\"\t\t\"{value}\"");
    }
    text.push_str("\t\"WorkshopItemsInstalled\"\n\t{\n");
    for (id, item) in items {
        let _ = writeln!(
            text,
            "\t\t\"{id}\"\n\t\t{{\n\t\t\t\"size\"\t\t\"{}\"\n\t\t\t\"timeupdated\"\t\t\"{}\"\n\
             \t\t\t\"manifest\"\t\t\"{}\"\n\t\t}}",
            item.size, item.updated, item.manifest
        );
    }
    text.push_str("\t}\n\t\"WorkshopItemDetails\"\n\t{\n");
    for (id, item) in items {
        let _ = writeln!(
            text,
            "\t\t\"{id}\"\n\t\t{{\n\t\t\t\"manifest\"\t\t\"{m}\"\n\t\t\t\"timeupdated\"\t\t\"{u}\"\n\
             \t\t\t\"timetouched\"\t\t\"{now}\"\n\t\t\t\"subscribedby\"\t\t\"{account}\"\n\
             \t\t\t\"latest_timeupdated\"\t\t\"{u}\"\n\t\t\t\"latest_manifest\"\t\t\"{m}\"\n\t\t}}",
            m = item.manifest,
            u = item.updated,
        );
    }
    text.push_str("\t}\n}\n");
    text
}

/// What `update` should do: items to download (new, or changed since),
/// and installed items no longer subscribed to, to remove.
fn plan(
    subscribed: &[Item],
    installed: &BTreeMap<u64, Installed>,
    present: impl Fn(u64) -> bool,
) -> (Vec<Item>, Vec<u64>) {
    let fetch = subscribed
        .iter()
        .filter(|item| match installed.get(&item.id) {
            Some(had) => {
                had.manifest != item.manifest || had.updated != item.updated || !present(item.id)
            }
            None => true,
        })
        .cloned()
        .collect();
    let drop = installed
        .keys()
        .filter(|id| !subscribed.iter().any(|item| item.id == **id))
        .copied()
        .collect();
    (fetch, drop)
}

/// Bring a game's Workshop items in line with the subscriptions: download
/// new and updated ones, remove those no longer subscribed to. An item
/// that fails is reported and left as it was; the rest go ahead. Returns
/// how many items are installed.
pub async fn update(connection: &Connection, appid: u32, steamapps: &Path) -> Result<usize> {
    let items = subscribed(connection, appid).await?;
    let mut have = installed(steamapps, appid);
    let (fetch, remove) = plan(&items, &have, |id| item_dir(steamapps, appid, id).is_dir());
    if items.is_empty() && have.is_empty() {
        return Ok(0);
    }

    for id in remove {
        let _ = fs::remove_dir_all(item_dir(steamapps, appid, id));
        let _ = fs::remove_file(state_path(steamapps, appid, id));
        have.remove(&id);
    }

    if !fetch.is_empty() {
        let total: u64 = fetch.iter().map(|i| i.size).sum();
        report(format_args!(
            "Downloading {} Workshop item{} ({})…",
            fetch.len(),
            if fetch.len() == 1 { "" } else { "s" },
            human(total)
        ));
        let mut source: Option<Source> = None;
        for item in &fetch {
            let name = if item.title.is_empty() {
                format!("item {}", item.id)
            } else {
                item.title.clone()
            };
            report(format_args!("Workshop: {name}…"));
            match fetch_item(connection, appid, steamapps, item, &mut source).await {
                Ok(size) => {
                    have.insert(
                        item.id,
                        Installed {
                            size,
                            updated: item.updated,
                            manifest: item.manifest,
                        },
                    );
                }
                Err(e) => report_problem(format_args!("warning: Workshop item {name}: {e:#}")),
            }
        }
    }

    let account = (u64::from(connection.steam_id()) & 0xffff_ffff) as u32;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = record_path(steamapps, appid);
    fs::create_dir_all(workshop_dir(steamapps))?;
    fs::write(&path, record(appid, account, &have, now))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(have.len())
}

/// The game's Workshop depot, its key and content servers: looked up once
/// per update, the first time an item needs them.
struct Source {
    depot: u32,
    key: DepotKey,
    cdn: Arc<Cdn>,
}

impl Source {
    async fn new(connection: &Connection, appid: u32) -> Result<Source> {
        let info = appinfo::fetch(connection, appid).await?;
        let depot = info.workshop_depot.unwrap_or(appid);
        Ok(Source {
            depot,
            key: cdn::depot_key(connection, appid, depot).await?,
            cdn: Arc::new(Cdn::discover(connection, appid).await?),
        })
    }
}

/// One item's files, into its folder. Returns its size on disk.
async fn fetch_item(
    connection: &Connection,
    appid: u32,
    steamapps: &Path,
    item: &Item,
    source: &mut Option<Source>,
) -> Result<u64> {
    let dir = item_dir(steamapps, appid, item.id);
    if item.manifest != 0 {
        if source.is_none() {
            *source = Some(Source::new(connection, appid).await?);
        }
        let source = source.as_ref().unwrap();
        let code =
            cdn::manifest_request_code(connection, appid, source.depot, item.manifest).await?;
        let manifest = source
            .cdn
            .manifest(source.depot, item.manifest, code, &source.key)
            .await?;
        let size = manifest.files.iter().map(|f| f.size).sum();
        let state = state_path(steamapps, appid, item.id);
        download::fill(source.cdn.clone(), &dir, &state, &[(manifest, source.key)]).await?;
        return Ok(size);
    }
    if item.file_url.is_empty() {
        bail!("Steam gave no way to download it");
    }
    // An old single-file item.
    let name = cdn::clean_path(item.filename.rsplit(['/', '\\']).next().unwrap_or_default())
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| item.id.to_string());
    let bytes = reqwest::get(&item.file_url)
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(&name), &bytes)?;
    Ok(bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, manifest: u64, updated: u32) -> Item {
        Item {
            id,
            title: format!("Item {id}"),
            size: 10,
            updated,
            manifest,
            file_url: String::new(),
            filename: String::new(),
        }
    }

    fn had(manifest: u64, updated: u32) -> Installed {
        Installed {
            size: 10,
            updated,
            manifest,
        }
    }

    #[test]
    fn fetches_new_and_changed_items_and_drops_unsubscribed_ones() {
        let subscribed = [item(1, 11, 100), item(2, 22, 200), item(3, 33, 300)];
        let installed = BTreeMap::from([
            (1, had(11, 100)), // up to date
            (2, had(21, 150)), // updated since
            (4, had(44, 400)), // unsubscribed
        ]);
        let (fetch, drop) = plan(&subscribed, &installed, |_| true);
        let ids: Vec<u64> = fetch.iter().map(|i| i.id).collect();
        assert_eq!(ids, [2, 3]);
        assert_eq!(drop, [4]);
    }

    #[test]
    fn fetches_an_up_to_date_item_whose_folder_is_gone() {
        let installed = BTreeMap::from([(1, had(11, 100))]);
        let (fetch, _) = plan(&[item(1, 11, 100)], &installed, |_| false);
        assert_eq!(fetch.len(), 1);
    }

    #[test]
    fn the_record_reads_back() {
        let items = BTreeMap::from([(7, had(70, 700)), (8, had(80, 800))]);
        let text = record(4000, 1234, &items, 999);
        assert_eq!(parse_record(&text), items);
        assert!(text.contains("\"subscribedby\"\t\t\"1234\""));
        assert!(text.contains("\"SizeOnDisk\"\t\t\"20\""));
    }

    #[test]
    fn items_live_where_steam_puts_them() {
        let dir = item_dir(Path::new("/lib/steamapps"), 4000, 123);
        assert_eq!(dir, Path::new("/lib/steamapps/workshop/content/4000/123"));
    }
}
