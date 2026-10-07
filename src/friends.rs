//! Steam friends over the CM connection: the friends list, who's online,
//! what they're playing and whether it can be joined, game invites, friend
//! requests, and one-to-one messages.
//!
//! Steam pushes the friends list right after sign-in, then pushes changes
//! to it (and, once this session says it's online, to friends' status)
//! for as long as the connection stays up.

use std::pin::pin;
use std::time::Duration;

use anyhow::{Result, bail};
use futures::StreamExt;
use steam_vent::{Connection, ConnectionTrait, EResult};
use steam_vent_proto::steammessages_clientserver::CMsgClientInviteToGame;
use steam_vent_proto::steammessages_clientserver_friends::{
    CMsgClientAddFriend, CMsgClientAddFriendResponse, CMsgClientChangeStatus,
    CMsgClientFriendsList, CMsgClientPersonaState, CMsgClientRemoveFriend,
    CMsgClientRequestFriendData,
};
use steam_vent_proto::steammessages_clientserver_mms::CMsgClientMMSInviteToLobby;
use steam_vent_proto::steammessages_friendmessages_steamclient::{
    CFriendMessages_GetRecentMessages_Request, CFriendMessages_IncomingMessage_Notification,
    CFriendMessages_SendMessage_Request,
};
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetPlayerLinkDetails_Request;

pub type SteamId = u64;

/// `k_EChatEntryTypeChatMsg`; the other entry types are typing
/// notifications and the like.
const CHAT_MESSAGE: i32 = 1;
/// `EClientPersonaStateFlag` RichPresence: the key/values a game publishes,
/// among them how to join it.
const RICH_PRESENCE: u32 = 4096;
/// Name, status, what they're playing and how to join it
/// (`EClientPersonaStateFlag` Status | PlayerName | Presence |
/// GameExtraInfo | RichPresence).
const PERSONA_FIELDS: u32 = 1 | 2 | 16 | 256 | RICH_PRESENCE;
/// People asked about per `details` request.
const DETAILS_BATCH: usize = 100;
/// How long to wait for Steam to say what our own session is.
const OWN_SESSION_WAIT: Duration = Duration::from_secs(5);
/// Messages fetched when a conversation is opened.
const HISTORY: u32 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relationship {
    /// They sent us a friend request.
    RequestReceived,
    Friend,
    /// We sent them a friend request.
    RequestSent,
}

impl Relationship {
    /// From `EFriendRelationship`. Blocked and ignored accounts are left out.
    fn from_steam(value: u32) -> Option<Relationship> {
        match value {
            2 => Some(Relationship::RequestReceived),
            3 => Some(Relationship::Friend),
            4 => Some(Relationship::RequestSent),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Online,
    Busy,
    Away,
    #[default]
    Offline,
}

impl Status {
    /// From `EPersonaState`. Invisible looks offline, as it does in Steam.
    fn from_steam(value: u32) -> Status {
        match value {
            1 | 5 | 6 => Status::Online,
            2 => Status::Busy,
            3 | 4 => Status::Away,
            _ => Status::Offline,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Playing {
    App(u32),
    /// A non-Steam game, by the name Steam was given.
    Named(String),
}

/// How to get into a friend's game, the way Steam's "Join Game" does it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Join {
    /// The game's `connect` rich presence: what to put on the command line.
    Connect(String),
    /// The Steam lobby they're in.
    Lobby(u64),
}

impl Join {
    /// What Steam adds to the game's command line to join (games read
    /// `+connect_lobby` for lobbies).
    pub fn args(&self) -> Vec<String> {
        match self {
            Join::Connect(connect) => connect.split_whitespace().map(str::to_owned).collect(),
            Join::Lobby(lobby) => vec!["+connect_lobby".into(), lobby.to_string()],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Friend {
    pub steamid: SteamId,
    pub relationship: Relationship,
    /// Empty until Steam sends it.
    pub name: String,
    pub status: Status,
    pub playing: Option<Playing>,
    /// The game's `connect` rich presence, empty if it has none.
    pub connect: String,
    /// The lobby they're in, 0 for none.
    pub lobby: u64,
}

impl Friend {
    pub fn new(steamid: SteamId, relationship: Relationship) -> Friend {
        Friend {
            steamid,
            relationship,
            name: String::new(),
            status: Status::Offline,
            playing: None,
            connect: String::new(),
            lobby: 0,
        }
    }

    pub fn apply(&mut self, update: &Persona) {
        if let Some(name) = &update.name {
            self.name = name.clone();
        }
        if let Some((status, playing)) = &update.status {
            self.status = *status;
            self.playing = playing.clone();
        }
        if let Some(connect) = &update.connect {
            self.connect = connect.clone();
        }
        if let Some(lobby) = update.lobby {
            self.lobby = lobby;
        }
        if self.playing.is_none() {
            // Out of the game, out of its session.
            self.connect.clear();
            self.lobby = 0;
        }
    }

    /// The Steam game they're in and how to join it, if it can be.
    pub fn join(&self) -> Option<(u32, Join)> {
        let Some(Playing::App(appid)) = self.playing else {
            return None;
        };
        if !self.connect.is_empty() {
            Some((appid, Join::Connect(self.connect.clone())))
        } else if self.lobby != 0 {
            Some((appid, Join::Lobby(self.lobby)))
        } else {
            None
        }
    }
}

/// A change in the friends list: who, and what they are now (`None`:
/// no longer on it).
pub type ListChange = (SteamId, Option<Relationship>);

/// The friends list as Steam sent it. `full` replaces the whole list;
/// otherwise only the listed entries changed.
pub fn list(message: &CMsgClientFriendsList) -> (bool, Vec<ListChange>) {
    let changes = message
        .friends
        .iter()
        .filter(|f| is_individual(f.ulfriendid()))
        .map(|f| {
            let relationship = Relationship::from_steam(f.efriendrelationship());
            (f.ulfriendid(), relationship)
        })
        .collect();
    (!message.bincremental(), changes)
}

/// Groups share the friends list with people; only people count here.
fn is_individual(steamid: SteamId) -> bool {
    (steamid >> 52) & 0xf == 1
}

/// A friend's name or status changing. Each part is only there if Steam
/// sent it.
#[derive(Clone, Debug, PartialEq)]
pub struct Persona {
    pub steamid: SteamId,
    pub name: Option<String>,
    pub status: Option<(Status, Option<Playing>)>,
    /// The `connect` rich presence (empty: none), when rich presence came.
    pub connect: Option<String>,
    pub lobby: Option<u64>,
}

pub fn personas(message: &CMsgClientPersonaState) -> Vec<Persona> {
    let with_rich_presence = message.status_flags() & RICH_PRESENCE != 0;
    message
        .friends
        .iter()
        .map(|f| {
            let playing = match (f.game_played_app_id(), f.game_name()) {
                (0, "") => None,
                (0, name) => Some(Playing::Named(name.to_owned())),
                (appid, _) => Some(Playing::App(appid)),
            };
            let connect = with_rich_presence.then(|| {
                f.rich_presence
                    .iter()
                    .find(|kv| kv.key() == "connect")
                    .map(|kv| kv.value().to_owned())
                    .unwrap_or_default()
            });
            Persona {
                steamid: f.friendid(),
                name: f.player_name.clone(),
                status: f
                    .persona_state
                    .map(|state| (Status::from_steam(state), playing)),
                connect,
                lobby: f.game_lobby_id,
            }
        })
        .collect()
}

/// Names, status and what they're playing, for many people in one
/// request (`Player.GetPlayerLinkDetails`), to fill the list. Persona
/// pushes carry the same, but for a whole friends list they come as one
/// burst and steam-vent keeps only the last 16 of a burst. Rich presence
/// (how to join) still comes from `request_personas`, one friend at a time.
pub async fn details(connection: &Connection, steamids: &[SteamId]) -> Result<Vec<Persona>> {
    let mut personas = Vec::with_capacity(steamids.len());
    for batch in steamids.chunks(DETAILS_BATCH) {
        let response = connection
            .service_method(CPlayer_GetPlayerLinkDetails_Request {
                steamids: batch.to_vec(),
                ..Default::default()
            })
            .await?;
        for account in &response.accounts {
            let (public, private) = (&account.public_data, &account.private_data);
            let status = private.as_ref().map(|p| {
                (
                    Status::from_steam(p.persona_state() as u32),
                    playing(p.game_id(), p.game_extra_info()),
                )
            });
            personas.push(Persona {
                steamid: public.steamid(),
                name: Some(public.persona_name().to_owned()).filter(|n| !n.is_empty()),
                status,
                connect: None,
                lobby: private.as_ref().map(|p| p.lobby_steam_id()),
            });
        }
    }
    Ok(personas)
}

/// What a `CGameID` says someone is playing: a Steam app (type 0, the
/// app id in the low 24 bits) or anything else by the name Steam has.
fn playing(game_id: u64, name: &str) -> Option<Playing> {
    let appid = (game_id & 0xff_ffff) as u32;
    match (game_id >> 24) & 0xff {
        _ if game_id == 0 => None,
        0 if appid != 0 => Some(Playing::App(appid)),
        _ if !name.is_empty() => Some(Playing::Named(name.to_owned())),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub from_me: bool,
    pub text: String,
    /// Unix seconds, from Steam's clock.
    pub time: u32,
    /// The message is an invite into their game.
    pub invite: Option<GameInvite>,
}

/// An invite into someone's game, as it comes in a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameInvite {
    /// The game, when the invite says (lobby invites may not; then it's
    /// whatever the sender is playing).
    pub appid: Option<u32>,
    pub join: Join,
}

/// A game invite in a message's markup: a tag with "invite" in its name
/// and `appid`, `connect` or `lobbyid` attributes, like
/// `[lobbyinvite lobbyid="…"][/lobbyinvite]`.
///
/// Read from what Steam's own client draws; not checked against a real
/// invite yet.
fn game_invite(markup: &str) -> Option<GameInvite> {
    let mut rest = markup;
    while let Some(open) = rest.find('[') {
        let tag = &rest[open + 1..];
        let close = tag.find(']')?;
        let (name, attributes) = tag[..close]
            .split_once(char::is_whitespace)
            .unwrap_or((&tag[..close], ""));
        rest = &tag[close..];
        if !name.to_lowercase().contains("invite") {
            continue;
        }
        let value = |key: &str| {
            let start = attributes.find(&format!("{key}=\""))? + key.len() + 2;
            let end = start + attributes[start..].find('"')?;
            Some(attributes[start..end].to_owned())
        };
        let appid = value("appid").and_then(|v| v.parse().ok());
        let join = match (
            value("connect"),
            value("lobbyid").and_then(|v| v.parse().ok()),
        ) {
            (Some(connect), _) if !connect.is_empty() => Join::Connect(connect),
            (_, Some(lobby)) if lobby != 0 => Join::Lobby(lobby),
            _ => continue,
        };
        return Some(GameInvite { appid, join });
    }
    None
}

/// What's shown for a message that's only an image, sticker or invite
/// (nothing left once Steam's markup is taken out).
const NOT_TEXT: &str = "(something fumes doesn't show: an image, sticker or invite)";

/// A message arriving on this connection: who the conversation is with,
/// and the message. Typing notifications and the like are `None`.
pub fn incoming(
    notification: &CFriendMessages_IncomingMessage_Notification,
) -> Option<(SteamId, Message)> {
    if notification.chat_entry_type() != CHAT_MESSAGE {
        return None;
    }
    let invite = game_invite(notification.message());
    // Steam fills `message_no_bbcode` only for messages with markup in
    // them; for plain ones it's empty and `message` is the text.
    let text = match notification.message_no_bbcode() {
        "" => plain(notification.message()),
        plain => plain.to_owned(),
    };
    let text = match text.trim() {
        _ if invite.is_some() => "invited you to play".to_owned(),
        "" if notification.message().is_empty() => return None,
        "" => NOT_TEXT.to_owned(),
        _ => text,
    };
    Some((
        notification.steamid_friend(),
        Message {
            // Sent by this account from another device.
            from_me: notification.local_echo(),
            text,
            time: notification.rtime32_server_timestamp(),
            invite,
        },
    ))
}

/// A message without Steam's markup: tags like `[b]`, `[/url]` or
/// `[img src="…"]` go, the text between them stays, and brackets the
/// sender typed (escaped as `\[`) come back.
fn plain(markup: &str) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(at) = rest.find(['[', '\\']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(escaped) = rest.strip_prefix('\\') {
            let mut chars = escaped.chars();
            match chars.next() {
                Some(c @ ('[' | ']' | '\\')) => out.push(c),
                Some(c) => {
                    out.push('\\');
                    out.push(c);
                }
                None => out.push('\\'),
            }
            rest = chars.as_str();
            continue;
        }
        let tag = &rest[1..];
        let name = tag.strip_prefix('/').unwrap_or(tag);
        let is_tag = name.starts_with(|c: char| c.is_ascii_alphabetic())
            && name.find(']').is_some_and(|end| {
                name[..end].split_whitespace().next().is_some_and(|n| {
                    n.split('=')
                        .next()
                        .unwrap_or("")
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric())
                })
            });
        match tag.find(']') {
            Some(end) if is_tag => rest = &tag[end + 1..],
            _ => {
                out.push('[');
                rest = tag;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Show this session as online, so Steam delivers messages to it and
/// pushes friends' status changes.
pub async fn go_online(connection: &Connection) -> Result<()> {
    connection
        .send(CMsgClientChangeStatus {
            persona_state: Some(1),
            ..Default::default()
        })
        .await?;
    Ok(())
}

/// Ask for names, status and rich presence; the answers come as persona
/// state pushes.
pub async fn request_personas(connection: &Connection, steamids: Vec<SteamId>) -> Result<()> {
    if steamids.is_empty() {
        return Ok(());
    }
    connection
        .send(CMsgClientRequestFriendData {
            persona_state_requested: Some(PERSONA_FIELDS),
            friends: steamids,
            ..Default::default()
        })
        .await?;
    Ok(())
}

pub async fn send_message(connection: &Connection, to: SteamId, text: &str) -> Result<Message> {
    let response = connection
        .service_method(CFriendMessages_SendMessage_Request {
            steamid: Some(to),
            chat_entry_type: Some(CHAT_MESSAGE),
            message: Some(text.to_owned()),
            contains_bbcode: Some(false),
            ..Default::default()
        })
        .await?;
    Ok(Message {
        from_me: true,
        text: match response.message_without_bb_code() {
            "" => text.to_owned(),
            plain => plain.to_owned(),
        },
        time: response.server_timestamp(),
        invite: None,
    })
}

/// The latest messages with a friend, oldest first.
pub async fn history(connection: &Connection, me: SteamId, with: SteamId) -> Result<Vec<Message>> {
    let response = connection
        .service_method(CFriendMessages_GetRecentMessages_Request {
            steamid1: Some(me),
            steamid2: Some(with),
            count: Some(HISTORY),
            bbcode_format: Some(false),
            ..Default::default()
        })
        .await?;
    let my_account = me as u32;
    let mut messages: Vec<(u32, u32, Message)> = response
        .messages
        .iter()
        .map(|m| {
            let text = match m.message() {
                "" => NOT_TEXT,
                text => text,
            };
            let message = Message {
                from_me: m.accountid() == my_account,
                text: text.to_owned(),
                time: m.timestamp(),
                invite: None,
            };
            (m.timestamp(), m.ordinal(), message)
        })
        .collect();
    messages.sort_by_key(|&(time, ordinal, _)| (time, ordinal));
    Ok(messages.into_iter().map(|(_, _, m)| m).collect())
}

/// The game this account is in and how friends can join it, as Steam
/// shows it to them: asks Steam about ourselves (details, then rich
/// presence) and waits a little for the answer. `None` if not in a game,
/// or in one that hasn't opened a joinable session.
pub async fn own_session(connection: &Connection, me: SteamId) -> Result<Option<(u32, Join)>> {
    let mut answers = pin!(connection.on::<CMsgClientPersonaState>());
    let mut own = Friend::new(me, Relationship::Friend);
    for persona in details(connection, &[me]).await? {
        own.apply(&persona);
    }
    request_personas(connection, vec![me]).await?;
    let wait = tokio::time::sleep(OWN_SESSION_WAIT);
    let mut wait = pin!(wait);
    loop {
        tokio::select! {
            Some(answer) = answers.next() => {
                let Ok(state) = answer else { continue };
                if let Some(persona) = personas(&state).into_iter().find(|p| p.steamid == me) {
                    own.apply(&persona);
                    return Ok(own.join());
                }
            }
            _ = &mut wait => return Ok(own.join()),
        }
    }
}

/// Invite a friend into a game session, the way Steam's "Invite to Game"
/// does: the session's connect string when the game publishes one
/// (`InviteUserToGame`), a lobby invite otherwise (`InviteUserToLobby`).
pub async fn invite(
    connection: &Connection,
    me: SteamId,
    to: SteamId,
    appid: u32,
    join: &Join,
) -> Result<()> {
    match join {
        Join::Connect(connect) => {
            connection
                .send(CMsgClientInviteToGame {
                    steam_id_dest: Some(to),
                    steam_id_src: Some(me),
                    connect_string: Some(connect.clone()),
                    ..Default::default()
                })
                .await?
        }
        Join::Lobby(lobby) => {
            connection
                .send(CMsgClientMMSInviteToLobby {
                    app_id: Some(appid),
                    steam_id_lobby: Some(*lobby),
                    steam_id_user_invited: Some(to),
                    ..Default::default()
                })
                .await?
        }
    }
    Ok(())
}

/// Accept someone's friend request. Returns their name.
pub async fn accept(connection: &Connection, steamid: SteamId) -> Result<String> {
    let response: CMsgClientAddFriendResponse = connection
        .job(CMsgClientAddFriend {
            steamid_to_add: Some(steamid),
            ..Default::default()
        })
        .await?;
    if let Err(e) = EResult::from_result(response.eresult()) {
        bail!(match e {
            EResult::LimitExceeded => "the friends list is full".to_owned(),
            other => format!("Steam said {other:?}"),
        });
    }
    Ok(response.persona_name_added().to_owned())
}

/// Take someone off the list: declines their friend request, or takes
/// back ours.
pub async fn remove(connection: &Connection, steamid: SteamId) -> Result<()> {
    connection
        .send(CMsgClientRemoveFriend {
            friendid: Some(steamid),
            ..Default::default()
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent_proto::steammessages_clientserver_friends::{
        cmsg_client_friends_list, cmsg_client_persona_state,
    };

    #[test]
    fn keeps_people_and_drops_groups_and_ignored_accounts() {
        let entry = |id: u64, rel: u32| cmsg_client_friends_list::Friend {
            ulfriendid: Some(id),
            efriendrelationship: Some(rel),
            ..Default::default()
        };
        let message = CMsgClientFriendsList {
            bincremental: Some(false),
            friends: vec![
                entry(76561198000000001, 3),
                entry(76561198000000002, 2),
                entry(76561198000000003, 5),
                // A group.
                entry(103582791429521408, 3),
            ],
            ..Default::default()
        };
        let (full, changes) = list(&message);
        assert!(full);
        assert_eq!(
            changes,
            [
                (76561198000000001, Some(Relationship::Friend)),
                (76561198000000002, Some(Relationship::RequestReceived)),
                (76561198000000003, None),
            ]
        );
    }

    fn persona_message(
        flags: u32,
        friend: cmsg_client_persona_state::Friend,
    ) -> CMsgClientPersonaState {
        CMsgClientPersonaState {
            status_flags: Some(flags),
            friends: vec![friend],
            ..Default::default()
        }
    }

    #[test]
    fn persona_updates_only_touch_what_was_sent() {
        let mut friend = Friend::new(1, Relationship::Friend);
        let message = persona_message(
            1 | 2,
            cmsg_client_persona_state::Friend {
                friendid: Some(1),
                player_name: Some("Mara".into()),
                persona_state: Some(1),
                game_played_app_id: Some(620),
                ..Default::default()
            },
        );
        friend.apply(&personas(&message)[0]);
        assert_eq!(friend.name, "Mara");
        assert_eq!(friend.status, Status::Online);
        assert_eq!(friend.playing, Some(Playing::App(620)));

        // A status change without the name keeps the name.
        let message = persona_message(
            1,
            cmsg_client_persona_state::Friend {
                friendid: Some(1),
                persona_state: Some(0),
                ..Default::default()
            },
        );
        friend.apply(&personas(&message)[0]);
        assert_eq!(friend.name, "Mara");
        assert_eq!(friend.status, Status::Offline);
        assert_eq!(friend.playing, None);
    }

    #[test]
    fn takes_steams_markup_out_of_messages() {
        assert_eq!(plain("hello there"), "hello there");
        assert_eq!(plain("look [b]at this[/b]!"), "look at this!");
        assert_eq!(plain(r#"[img src="https://x/y.png"][/img]"#), "");
        assert_eq!(plain(r#"[url=https://a.b]a link[/url]"#), "a link");
        assert_eq!(plain(r"\[not a tag\] [1] [ ok"), "[not a tag] [1] [ ok");
        assert_eq!(plain("a\\b"), "a\\b");
    }

    #[test]
    fn plain_messages_show_their_text() {
        let notification =
            |message: &str, no_bbcode: &str| CFriendMessages_IncomingMessage_Notification {
                steamid_friend: Some(1),
                chat_entry_type: Some(CHAT_MESSAGE),
                message: Some(message.into()),
                message_no_bbcode: Some(no_bbcode.into()),
                ..Default::default()
            };
        let text = |n| incoming(&n).map(|(_, m)| m.text);
        // What Steam sends for plain text: nothing in message_no_bbcode.
        assert_eq!(text(notification("hi!", "")).as_deref(), Some("hi!"));
        assert_eq!(text(notification("[b]hi[/b]", "hi")).as_deref(), Some("hi"));
        assert_eq!(
            text(notification(r#"[sticker type="Cat"][/sticker]"#, "")).as_deref(),
            Some(NOT_TEXT)
        );
    }

    #[test]
    fn finds_game_invites_in_message_markup() {
        assert_eq!(
            game_invite(r#"[lobbyinvite lobbyid="109775241234567890"][/lobbyinvite]"#),
            Some(GameInvite {
                appid: None,
                join: Join::Lobby(109775241234567890)
            })
        );
        assert_eq!(
            game_invite(
                r#"hey [gameinvite appid="440" connect="+connect 1.2.3.4:27015"][/gameinvite]"#
            ),
            Some(GameInvite {
                appid: Some(440),
                join: Join::Connect("+connect 1.2.3.4:27015".into())
            })
        );
        assert_eq!(game_invite("[b]bold[/b] [img src=\"x\"][/img]"), None);
        assert_eq!(game_invite("[unclosed"), None);
    }

    #[test]
    fn reads_what_a_game_id_says_is_being_played() {
        assert_eq!(playing(0, ""), None);
        assert_eq!(playing(620, ""), Some(Playing::App(620)));
        // A non-Steam shortcut: type 2, with a name.
        let shortcut = (0x8000_0000u64 << 32) | (2 << 24);
        assert_eq!(
            playing(shortcut, "Minecraft"),
            Some(Playing::Named("Minecraft".into()))
        );
        assert_eq!(playing(shortcut, ""), None);
    }

    #[test]
    fn games_can_be_joined_by_connect_string_or_lobby() {
        let rich_presence = |key: &str, value: &str| cmsg_client_persona_state::friend::KV {
            key: Some(key.into()),
            value: Some(value.into()),
            ..Default::default()
        };
        let in_game = |connect: Vec<_>, lobby| cmsg_client_persona_state::Friend {
            friendid: Some(1),
            persona_state: Some(1),
            game_played_app_id: Some(620),
            game_lobby_id: Some(lobby),
            rich_presence: connect,
            ..Default::default()
        };

        let mut friend = Friend::new(1, Relationship::Friend);
        let connect = vec![
            rich_presence("status", "In a match"),
            rich_presence("connect", "+connect 10.0.0.2:27015"),
        ];
        friend.apply(&personas(&persona_message(1 | RICH_PRESENCE, in_game(connect, 0)))[0]);
        let (appid, join) = friend.join().unwrap();
        assert_eq!(appid, 620);
        assert_eq!(join.args(), ["+connect", "10.0.0.2:27015"]);

        // Rich presence without `connect`, in a lobby.
        let lobby = 109775241234567890;
        friend.apply(&personas(&persona_message(1 | RICH_PRESENCE, in_game(vec![], lobby)))[0]);
        assert_eq!(
            friend.join().unwrap().1.args(),
            ["+connect_lobby", &lobby.to_string()]
        );

        // Leaving the game leaves its session.
        let left = cmsg_client_persona_state::Friend {
            friendid: Some(1),
            persona_state: Some(1),
            ..Default::default()
        };
        friend.apply(&personas(&persona_message(1, left))[0]);
        assert_eq!(friend.join(), None);
    }
}
