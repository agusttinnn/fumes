//! The UI's connection to Steam, kept open in the background while it
//! runs: the owned games, the friends list and who's online (and in what
//! game), friend requests and messages. Requests go in through a channel, and everything Steam says
//! comes back to the UI as `Update`s, which it reads between frames.
//!
//! If the connection drops, it's opened again, waiting longer each time.

use std::pin::pin;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use steam_vent::{Connection, ConnectionError, ConnectionTrait};
use steam_vent_proto::steammessages_clientserver_friends::{
    CMsgClientFriendsList, CMsgClientPersonaState,
};
use steam_vent_proto::steammessages_friendmessages_steamclient::CFriendMessages_IncomingMessage_Notification;
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetPlayerLinkDetails_Request;
use tokio::task::JoinSet;

use crate::dirs::Dirs;
use crate::friends::{self, ListChange, Message, Persona, SteamId};
use crate::library::{self, Owned};
use crate::session;

/// How often to check that the connection is still alive.
const KEEPALIVE: Duration = Duration::from_secs(60);
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq)]
pub enum Request {
    RefreshLibrary,
    History(SteamId),
    Send {
        to: SteamId,
        text: String,
    },
    /// Fresh names, status and rich presence (which says whether their
    /// game can be joined); the answers come as `Update::Persona`.
    Personas(Vec<SteamId>),
    /// Invite a friend into the game session this account is in.
    Invite(SteamId),
    /// Accept a friend request.
    Accept(SteamId),
    /// Declines their friend request, or takes back ours.
    Remove(SteamId),
    Stop,
}

pub enum Update {
    Connected,
    /// Lost the connection (or never got it); trying again.
    Offline(String),
    /// Steam turned the saved session down, so there's no point trying
    /// again until someone signs in anew.
    Rejected(String),
    Library(Result<Vec<Owned>>),
    Friends {
        full: bool,
        changes: Vec<ListChange>,
    },
    Persona(Persona),
    Message {
        with: SteamId,
        message: Message,
    },
    History {
        with: SteamId,
        messages: Vec<Message>,
    },
    /// Something to tell the user: a request was accepted, a send failed.
    Notice(String),
}

pub struct Online {
    requests: UnboundedSender<Request>,
}

impl Online {
    pub fn start(dirs: Dirs, updates: mpsc::Sender<Update>) -> Online {
        let (requests, incoming) = unbounded();
        tokio::spawn(run(dirs, incoming, updates));
        Online { requests }
    }

    /// One with nothing behind it; the requests come out of the receiver.
    #[cfg(test)]
    pub fn detached() -> (Online, UnboundedReceiver<Request>) {
        let (requests, incoming) = unbounded();
        (Online { requests }, incoming)
    }

    pub fn send(&self, request: Request) {
        self.requests.unbounded_send(request).ok();
    }
}

impl Drop for Online {
    fn drop(&mut self) {
        self.send(Request::Stop);
    }
}

async fn run(dirs: Dirs, mut requests: UnboundedReceiver<Request>, updates: mpsc::Sender<Update>) {
    let mut wait = RETRY_FIRST;
    loop {
        match connected(&dirs, &mut requests, &updates).await {
            Ok(()) => return,
            Err(e) if rejected(&e) => {
                updates.send(Update::Rejected(format!("{e:#}"))).ok();
                return;
            }
            Err(e) => {
                updates.send(Update::Offline(format!("{e:#}"))).ok();
            }
        }
        // Until the next try, answer what can't be done offline.
        let retry = tokio::time::sleep(wait);
        let mut retry = pin!(retry);
        loop {
            tokio::select! {
                _ = &mut retry => break,
                request = requests.next() => match request {
                    None | Some(Request::Stop) => return,
                    Some(Request::RefreshLibrary) => {
                        let offline = anyhow::anyhow!("not connected to Steam");
                        updates.send(Update::Library(Err(offline))).ok();
                    }
                    Some(_) => {
                        let notice = "Not connected to Steam right now; trying again soon.";
                        updates.send(Update::Notice(notice.into())).ok();
                    }
                },
            }
        }
        wait = (wait * 2).min(RETRY_MAX);
    }
}

/// Steam said no to signing in (as opposed to not being reachable).
fn rejected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<ConnectionError>(),
            Some(ConnectionError::LoginError(_))
        )
    })
}

/// One connection, until it drops (an error) or the UI stops it (`Ok`).
async fn connected(
    dirs: &Dirs,
    requests: &mut UnboundedReceiver<Request>,
    updates: &mpsc::Sender<Update>,
) -> Result<()> {
    let connection = session::connect(dirs).await?;
    let me: SteamId = connection.steam_id().into();

    // Steam's pushes, each kind read by its own task, started before
    // anything can set them off. They come in bursts (going online sends
    // everyone's status at once) and steam-vent keeps only 16 unread
    // before dropping the rest. The tasks end with this function.
    let mut pushes = JoinSet::new();
    let lists = connection.on::<CMsgClientFriendsList>();
    let personas = connection.on::<CMsgClientPersonaState>();
    let messages = connection.on_notification::<CFriendMessages_IncomingMessage_Notification>();
    let (conn, tx) = (connection.clone(), updates.clone());
    pushes.spawn(async move {
        let mut lists = pin!(lists);
        while let Some(list) = lists.next().await {
            if let Ok(list) = list
                && friends_list(&conn, &list, &tx).await.is_err()
            {
                return;
            }
        }
    });
    let tx = updates.clone();
    pushes.spawn(async move {
        let mut personas = pin!(personas);
        while let Some(state) = personas.next().await {
            for persona in state.iter().flat_map(friends::personas) {
                tx.send(Update::Persona(persona)).ok();
            }
        }
    });
    let tx = updates.clone();
    pushes.spawn(async move {
        let mut messages = pin!(messages);
        while let Some(notification) = messages.next().await {
            if let Some((with, message)) = notification.ok().as_ref().and_then(friends::incoming) {
                tx.send(Update::Message { with, message }).ok();
            }
        }
    });

    // Steam only answers requests for names and status once this session
    // shows as online.
    friends::go_online(&connection).await?;
    // The friends list came right after sign-in, before anything was
    // listening; the connection keeps it among the unhandled messages.
    for raw in connection.take_unprocessed() {
        if let Ok(list) = raw.into_message::<CMsgClientFriendsList>() {
            friends_list(&connection, &list, updates).await?;
        }
    }

    updates.send(Update::Connected).ok();
    spawn(&connection, me, Request::RefreshLibrary, updates);

    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await;
    loop {
        tokio::select! {
            request = requests.next() => match request {
                // Just closing the connection, never logging off: steam-vent
                // signs in without "remember me" (no should_remember_password),
                // and for such a session Steam takes a log-off as signing
                // out, revoking the saved refresh token.
                None | Some(Request::Stop) => return Ok(()),
                // Each in its own task, so a slow answer doesn't hold up
                // the next request.
                Some(request) => spawn(&connection, me, request, updates),
            },
            _ = keepalive.tick() => {
                let ping = CPlayer_GetPlayerLinkDetails_Request {
                    steamids: vec![me],
                    ..Default::default()
                };
                connection
                    .service_method(ping)
                    .await
                    .context("lost the connection to Steam")?;
            }
        }
    }
}

async fn friends_list(
    connection: &Connection,
    list: &CMsgClientFriendsList,
    updates: &mpsc::Sender<Update>,
) -> Result<()> {
    let (full, changes) = friends::list(list);
    let people: Vec<SteamId> = changes
        .iter()
        .filter(|(_, relationship)| relationship.is_some())
        .map(|&(steamid, _)| steamid)
        .collect();
    updates.send(Update::Friends { full, changes }).ok();
    for persona in friends::details(connection, &people).await? {
        updates.send(Update::Persona(persona)).ok();
    }
    Ok(())
}

fn spawn(connection: &Connection, me: SteamId, request: Request, updates: &mpsc::Sender<Update>) {
    let connection = connection.clone();
    let updates = updates.clone();
    tokio::spawn(async move {
        let update = handle(&connection, me, request, &updates).await;
        updates.send(update).ok();
    });
}

/// Answers come back as the returned update, plus any sent along the way.
async fn handle(
    connection: &Connection,
    me: SteamId,
    request: Request,
    updates: &mpsc::Sender<Update>,
) -> Update {
    let notice = |result: Result<String>| {
        Update::Notice(match result {
            Ok(done) => done,
            Err(e) => format!("{e:#}"),
        })
    };
    match request {
        Request::RefreshLibrary => Update::Library(library::fetch_owned(connection).await),
        Request::History(with) => match friends::history(connection, me, with).await {
            Ok(messages) => Update::History { with, messages },
            Err(e) => Update::Notice(format!("Couldn't load the conversation: {e:#}")),
        },
        Request::Send { to, text } => match friends::send_message(connection, to, &text).await {
            Ok(message) => Update::Message { with: to, message },
            Err(e) => Update::Notice(format!("Message not sent: {e:#}")),
        },
        // Status straight away, rich presence (how to join) as a push.
        Request::Personas(steamids) => notice(
            async {
                for persona in friends::details(connection, &steamids).await? {
                    updates.send(Update::Persona(persona)).ok();
                }
                friends::request_personas(connection, steamids).await?;
                Ok(String::new())
            }
            .await,
        ),
        Request::Invite(to) => notice(
            async {
                let Some((appid, join)) = friends::own_session(connection, me).await? else {
                    bail!(
                        "your game hasn't opened a session friends can join yet \
                         (many only do once you're in a lobby or match)"
                    );
                };
                friends::invite(connection, me, to, appid, &join).await?;
                Ok("Invite sent.".to_owned())
            }
            .await
            .context("invite not sent"),
        ),
        Request::Accept(steamid) => notice(
            friends::accept(connection, steamid)
                .await
                .map(|name| format!("You and {} are now friends.", or_them(&name)))
                .context("couldn't accept the friend request"),
        ),
        // The friends list change that follows shows it worked.
        Request::Remove(steamid) => notice(
            friends::remove(connection, steamid)
                .await
                .map(|()| String::new()),
        ),
        Request::Stop => Update::Notice(String::new()),
    }
}

fn or_them(name: &str) -> &str {
    if name.is_empty() { "them" } else { name }
}
