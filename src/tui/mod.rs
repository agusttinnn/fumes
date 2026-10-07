//! The terminal UI, opened by `fumes` without a command.
//!
//! Three columns: sections on the left (library, friends, the store,
//! profile), the selected section's items in the middle, and what can be
//! done with the selected item on the right. Arrow keys move within a
//! column; → or Enter goes a column deeper (or runs the action), ← or Esc
//! comes back.
//!
//! Anything long or interactive (installing, signing in) runs as the
//! ordinary `fumes` command in a child process: the UI hands it the
//! terminal and comes back when it's done, so progress output, prompts and
//! Ctrl-C behave exactly as they do on the command line. Games are the
//! exception: they run in the background (`game`) and the UI stays up, so
//! friends can be messaged and invited while playing. Friends and messages
//! go over a Steam connection the UI keeps open (`online`).

pub mod game;
mod online;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::dirs::Dirs;
use crate::favorites;
use crate::friends::{
    self, Friend, GameInvite, Join, Message, Playing, Relationship, Status, SteamId,
};
use crate::library::{self, Game, Owned};
use crate::{download, session, steam};
use game::Running;
use online::{Online, Request, Update};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const STORE_URL: &str = "https://store.steampowered.com/";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Library,
    Friends,
    Store,
    Profile,
}

impl Section {
    const ALL: [Section; 4] = [
        Section::Library,
        Section::Friends,
        Section::Store,
        Section::Profile,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Library => "Library",
            Section::Friends => "Friends",
            Section::Store => "Store ↗",
            Section::Profile => "Profile",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sections,
    Items,
    Actions,
}

/// A text box that has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    /// The search bar of the library or the friends list.
    Search,
    /// The message box under a conversation.
    Message,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Play,
    PlayInSteam,
    Install,
    Update,
    Verify,
    CloudSync,
    Dlc,
    StorePage,
    Uninstall,
    UninstallInSteam,
    Favorite,
    Unfavorite,
    Message,
    JoinGame,
    AcceptGameInvite,
    InviteToGame,
    AcceptRequest,
    DeclineRequest,
    CancelRequest,
    SignIn,
    SignOut,
    CommunityProfile,
    EngineStatus,
    EngineSetup,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Play => "Play",
            Action::PlayInSteam => "Play (Steam)",
            Action::Install => "Install",
            Action::Update => "Update",
            Action::Verify => "Verify files",
            Action::CloudSync => "Cloud sync",
            Action::Dlc => "DLC ↗",
            Action::StorePage => "Store page ↗",
            Action::Uninstall => "Uninstall",
            Action::UninstallInSteam => "Uninstall (Steam)",
            Action::Favorite => "Add to favorites ★",
            Action::Unfavorite => "Remove from favorites",
            Action::Message => "Send message",
            Action::JoinGame => "Join game",
            Action::AcceptGameInvite => "Accept game invite",
            Action::InviteToGame => "Invite to your game",
            Action::AcceptRequest => "Accept friend request",
            Action::DeclineRequest => "Decline friend request",
            Action::CancelRequest => "Cancel friend request",
            Action::SignIn => "Sign in",
            Action::SignOut => "Sign out",
            Action::CommunityProfile => "Community profile ↗",
            Action::EngineStatus => "Steam engine status",
            Action::EngineSetup => "Set up Steam engine",
        }
    }
}

/// What running an action amounts to.
enum Job {
    /// A `fumes` command, run with the terminal handed over to it.
    Command(Pending),
    /// A game, run in the background (`fumes launch …`).
    Play(Launch),
    /// A web page or `steam://` link.
    Open(String),
}

/// A `fumes` command waiting for the terminal.
#[derive(Debug)]
struct Pending {
    title: String,
    args: Vec<String>,
    /// What it prints is the point (a status report), so it stays on
    /// screen until Enter even when the command worked. Otherwise the UI
    /// comes back by itself, and only waits when something went wrong.
    show_output: bool,
}

/// A game waiting to be started in the background.
#[derive(Debug)]
struct Launch {
    appid: u32,
    title: String,
    args: Vec<String>,
}

/// An action that asks first (deleting a game, signing out).
struct Confirm {
    question: String,
    job: Job,
}

/// The UI's connection to Steam.
#[derive(Clone, Debug, PartialEq)]
enum Link {
    SignedOut,
    Connecting,
    Connected,
    /// Why it dropped; it's being opened again.
    Offline(String),
    /// Steam turned the saved session down; signing in again fixes it.
    Rejected,
}

/// A conversation with one friend.
#[derive(Default)]
struct Chat {
    messages: Vec<Message>,
    /// The history came back from Steam.
    loaded: bool,
    /// The history was asked for.
    requested: bool,
    unread: u32,
    /// Their latest invite into a game, until it's taken up.
    invite: Option<GameInvite>,
}

pub fn run(dirs: Dirs) -> Result<()> {
    // While a command has the terminal, Ctrl-C is the command's to handle
    // and the UI waits for it. (Inside the UI, Ctrl-C is just a key.)
    tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });

    let mut app = App::new(dirs);
    app.connect();
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

struct App {
    dirs: Dirs,
    focus: Focus,
    sections: ListState,
    games: Vec<Game>,
    favorites: BTreeSet<u32>,
    /// Indexes into `games` that match the search, in the order shown.
    shown: Vec<usize>,
    search: String,
    games_list: ListState,
    friends: HashMap<SteamId, Friend>,
    favorite_friends: BTreeSet<SteamId>,
    /// The friends column, in the order shown.
    friend_rows: Vec<SteamId>,
    friend_search: String,
    friends_list: ListState,
    chats: HashMap<SteamId, Chat>,
    draft: String,
    /// The friend whose name, status and game were last asked for again.
    looked_up: Option<SteamId>,
    editing: Option<Field>,
    actions_list: ListState,
    /// Rows the middle list showed last frame, for Page Up/Down.
    page: usize,
    /// Account name and SteamID64.
    account: Option<(String, u64)>,
    engine_fetched: bool,
    online: Option<Online>,
    link: Link,
    refreshing: bool,
    status: Option<String>,
    confirm: Option<Confirm>,
    /// A command waiting for the terminal: title and arguments.
    pending: Option<Pending>,
    /// A game waiting to start, and the one running in the background.
    launch: Option<Launch>,
    running: Option<Running>,
    /// The running game is asking which cloud saves to keep.
    conflict: bool,
    quit: bool,
    updates_tx: mpsc::Sender<Update>,
    updates_rx: mpsc::Receiver<Update>,
}

impl App {
    fn new(dirs: Dirs) -> App {
        let (updates_tx, updates_rx) = mpsc::channel();
        let mut app = App {
            favorites: favorites::load(&dirs.favorites_file()),
            favorite_friends: favorites::load(&dirs.favorite_friends_file()),
            dirs,
            focus: Focus::Sections,
            sections: ListState::default().with_selected(Some(0)),
            games: Vec::new(),
            shown: Vec::new(),
            search: String::new(),
            games_list: ListState::default(),
            friends: HashMap::new(),
            friend_rows: Vec::new(),
            friend_search: String::new(),
            friends_list: ListState::default(),
            chats: HashMap::new(),
            draft: String::new(),
            looked_up: None,
            editing: None,
            actions_list: ListState::default().with_selected(Some(0)),
            page: 10,
            account: None,
            engine_fetched: false,
            online: None,
            link: Link::SignedOut,
            refreshing: false,
            status: None,
            confirm: None,
            pending: None,
            launch: None,
            running: None,
            conflict: false,
            quit: false,
            updates_tx,
            updates_rx,
        };
        app.reload();
        app
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(update) = self.updates_rx.try_recv() {
                self.update(update);
            }
            self.watch_game();
            self.sync();
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.key(key);
            }
            if let Some(launch) = self.launch.take() {
                self.start_game(launch);
            }
            if let Some(pending) = self.pending.take() {
                let worked = hand_over(terminal, &pending)?;
                self.status = Some(if worked {
                    format!("{}: done.", pending.title)
                } else {
                    format!("{}: didn't finish.", pending.title)
                });
                let before = self.account.clone();
                self.reload();
                // A new sign-in replaces a session Steam turned down, even
                // for the same account.
                if self.account != before || self.link == Link::Rejected {
                    self.connect();
                }
            }
        }
        Ok(())
    }

    /// Re-read what's on disk: installs, the session, the cached owned
    /// list. Cheap, so it runs after every command.
    fn reload(&mut self) {
        self.account = session::signed_in(&self.dirs);
        self.engine_fetched = steam::runtime::host().is_fetched(&self.dirs.engine());
        let owned = library::load_cache(&self.dirs.library_cache()).unwrap_or_default();
        self.set_games(library::merge(owned, crate::installed_here(&self.dirs)));
    }

    /// (Re)open the connection to Steam for whoever is signed in now.
    /// Signing in fetches the library too.
    fn connect(&mut self) {
        // Dropping the old one signs it out.
        self.online = None;
        self.friends.clear();
        self.chats.clear();
        self.apply_friend_search(None);
        if self.account.is_none() {
            self.link = Link::SignedOut;
            self.refreshing = false;
            return;
        }
        self.link = Link::Connecting;
        self.refreshing = true;
        self.online = Some(Online::start(self.dirs.clone(), self.updates_tx.clone()));
    }

    /// Ask Steam for the owned games again.
    fn refresh(&mut self) {
        let Some(online) = &self.online else {
            self.status = Some(if self.link == Link::Rejected {
                "Steam needs you to sign in again: Profile → Sign in.".into()
            } else {
                "Not signed in: Profile → Sign in to see your games.".into()
            });
            return;
        };
        if !self.refreshing {
            online.send(Request::RefreshLibrary);
            self.refreshing = true;
        }
    }

    fn request(&mut self, request: Request) {
        match &self.online {
            Some(online) => online.send(request),
            None if self.link == Link::Rejected => {
                self.status = Some("Steam needs you to sign in again: Profile → Sign in.".into())
            }
            None => self.status = Some("Not signed in: Profile → Sign in.".into()),
        }
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Connected => {
                self.link = Link::Connected;
                // Every new connection fetches the library.
                self.refreshing = true;
                for chat in self.chats.values_mut().filter(|c| !c.loaded) {
                    chat.requested = false;
                }
                let keep = self.friend_row();
                self.apply_friend_search(keep);
            }
            Update::Offline(why) => {
                self.link = Link::Offline(why);
                self.refreshing = false;
                let keep = self.friend_row();
                self.apply_friend_search(keep);
            }
            Update::Rejected(why) => {
                tracing::warn!("Steam turned down the saved session: {why}");
                self.link = Link::Rejected;
                self.refreshing = false;
                self.online = None;
                self.status =
                    Some("Steam ended fumes' saved sign-in. Profile → Sign in again.".into());
                let keep = self.friend_row();
                self.apply_friend_search(keep);
            }
            Update::Library(owned) => self.refreshed(owned),
            Update::Friends { full, changes } => self.friends_changed(full, changes),
            Update::Persona(persona) => {
                if let Some(friend) = self.friends.get_mut(&persona.steamid) {
                    friend.apply(&persona);
                    let keep = self.friend_row();
                    self.apply_friend_search(keep);
                }
            }
            Update::Message { with, message } => {
                let shown = self.viewed_friend() == Some(with);
                let text = message.text.clone();
                let from_them = !message.from_me;
                let invite = message.invite.clone().filter(|_| from_them);
                let name = self.friend_name(with);
                let chat = self.chats.entry(with).or_default();
                chat.messages.push(message);
                if let Some(invite) = invite {
                    chat.invite = Some(invite);
                    self.status = Some(format!(
                        "{name} invited you to play: Friends → {name} → Accept game invite."
                    ));
                } else if from_them && !shown {
                    self.status = Some(format!("{name}: {text}"));
                }
                if from_them && !shown {
                    self.chats.entry(with).or_default().unread += 1;
                }
            }
            Update::History { with, messages } => {
                let chat = self.chats.entry(with).or_default();
                chat.messages = messages;
                chat.loaded = true;
            }
            Update::Notice(text) if text.is_empty() => {}
            Update::Notice(text) => self.status = Some(text),
        }
    }

    fn refreshed(&mut self, owned: Result<Vec<Owned>>) {
        self.refreshing = false;
        let saved = owned.and_then(|owned| {
            library::save_cache(&self.dirs.library_cache(), &owned)?;
            Ok(owned)
        });
        match saved {
            Ok(owned) => self.set_games(library::merge(owned, crate::installed_here(&self.dirs))),
            Err(e) => {
                self.status = Some(format!(
                    "Couldn't reach Steam ({e:#}); showing the last list."
                ))
            }
        }
    }

    fn friends_changed(&mut self, full: bool, changes: Vec<friends::ListChange>) {
        if full {
            let listed: HashSet<SteamId> = changes
                .iter()
                .filter(|(_, relationship)| relationship.is_some())
                .map(|&(steamid, _)| steamid)
                .collect();
            self.friends.retain(|steamid, _| listed.contains(steamid));
        }
        for (steamid, relationship) in changes {
            match relationship {
                Some(relationship) => {
                    let new = !self.friends.contains_key(&steamid);
                    self.friends
                        .entry(steamid)
                        .or_insert_with(|| Friend::new(steamid, relationship))
                        .relationship = relationship;
                    if new && !full && relationship == Relationship::RequestReceived {
                        self.status = Some("New friend request: see Friends.".into());
                    }
                }
                None => {
                    self.friends.remove(&steamid);
                }
            }
        }
        let keep = self.friend_row();
        self.apply_friend_search(keep);
    }

    /// Keep things in step with what's on screen: load a conversation's
    /// history the first time it's shown, mark it read, and check again
    /// what game they're in and whether it can be joined.
    fn sync(&mut self) {
        let Some(steamid) = self.viewed_friend() else {
            self.looked_up = None;
            return;
        };
        if self.looked_up != Some(steamid)
            && self.link == Link::Connected
            && let Some(online) = &self.online
        {
            self.looked_up = Some(steamid);
            if self.friends[&steamid].playing.is_some() {
                online.send(Request::Personas(vec![steamid]));
            }
        }
        let chat = self.chats.entry(steamid).or_default();
        chat.unread = 0;
        if !chat.requested
            && self.link == Link::Connected
            && let Some(online) = &self.online
        {
            chat.requested = true;
            online.send(Request::History(steamid));
        }
    }

    /// Take in what the game in the background printed, and notice when
    /// its session is over.
    fn watch_game(&mut self) {
        let Some(game) = &mut self.running else {
            return;
        };
        if game.poll().conflict {
            self.conflict = true;
        }
        let Some(status) = game.finished() else {
            return;
        };
        game.drain();
        let title = game.title.clone();
        self.status = Some(match &game.error {
            Some(error) => format!("{title}: {error}"),
            None if !status.success() => format!("{title} stopped ({status})."),
            // "Launch cancelled; nothing was changed." and the like.
            None if game.state.contains("cancelled") => game.state.clone(),
            None => format!("{title} closed."),
        });
        self.running = None;
        self.conflict = false;
        self.reload();
    }

    fn start_game(&mut self, launch: Launch) {
        if let Some(running) = &self.running {
            self.status = Some(format!("{} is running; one game at a time.", running.title));
            return;
        }
        match Running::start(launch.appid, launch.title, &launch.args) {
            Ok(running) => self.running = Some(running),
            Err(e) => self.status = Some(format!("{e:#}")),
        }
    }

    /// Quitting while a game runs would cut its session short (saves not
    /// uploaded, the engine not signed out), so it waits for the game.
    fn try_quit(&mut self) {
        match &self.running {
            Some(game) => {
                self.status = Some(format!(
                    "{} is still running. Close it first, so its saves upload.",
                    game.title
                ))
            }
            None => self.quit = true,
        }
    }

    fn set_games(&mut self, games: Vec<Game>) {
        let selected = self.game().map(|g| g.appid);
        self.games = games;
        self.apply_search(selected);
    }

    /// Recompute which games match the search and in what order (best
    /// matches, then favorites, then by name), keeping `keep` selected if
    /// it's still there.
    fn apply_search(&mut self, keep: Option<u32>) {
        let query = words(&self.search);
        let mut ranked: Vec<(u8, bool, usize)> = (0..self.games.len())
            .filter_map(|i| {
                let game = &self.games[i];
                let rank = rank(&query, &self.search, &game.name, game.appid.into())?;
                Some((rank, !self.favorites.contains(&game.appid), i))
            })
            .collect();
        ranked.sort_unstable();
        self.shown = ranked.into_iter().map(|(_, _, i)| i).collect();
        let at = keep.and_then(|appid| {
            self.shown
                .iter()
                .position(|&i| self.games[i].appid == appid)
        });
        self.games_list
            .select((!self.shown.is_empty()).then(|| at.unwrap_or(0)));
    }

    /// The friends column: friend requests for us first, then favorites,
    /// then everyone else by what they're doing, then requests we sent. A
    /// search puts the best matches first.
    fn apply_friend_search(&mut self, keep: Option<SteamId>) {
        let query = words(&self.friend_search);
        let mut ranked: Vec<(u8, u8, u8, String, SteamId)> = self
            .friends
            .values()
            .filter_map(|f| {
                let rank = rank(&query, &self.friend_search, &f.name, f.steamid)?;
                let group = match f.relationship {
                    Relationship::RequestReceived => 0,
                    Relationship::Friend if self.favorite_friends.contains(&f.steamid) => 1,
                    Relationship::Friend => 2,
                    Relationship::RequestSent => 3,
                };
                let activity = match (&f.playing, f.status) {
                    (Some(_), _) => 0,
                    (None, Status::Online) => 1,
                    (None, Status::Busy) => 2,
                    (None, Status::Away) => 3,
                    (None, Status::Offline) => 4,
                };
                Some((rank, group, activity, f.name.to_lowercase(), f.steamid))
            })
            .collect();
        ranked.sort_unstable();
        self.friend_rows = ranked.into_iter().map(|(.., id)| id).collect();
        let at = keep.and_then(|id| self.friend_rows.iter().position(|&r| r == id));
        self.friends_list
            .select((!self.friend_rows.is_empty()).then(|| at.unwrap_or(0)));
    }

    fn section(&self) -> Section {
        Section::ALL[self.sections.selected().unwrap_or(0)]
    }

    fn game(&self) -> Option<&Game> {
        let i = *self.shown.get(self.games_list.selected()?)?;
        self.games.get(i)
    }

    fn friend_row(&self) -> Option<SteamId> {
        self.friend_rows.get(self.friends_list.selected()?).copied()
    }

    fn friend(&self) -> Option<&Friend> {
        self.friends.get(&self.friend_row()?)
    }

    /// The friend whose conversation is on screen.
    fn viewed_friend(&self) -> Option<SteamId> {
        if self.section() != Section::Friends {
            return None;
        }
        self.friend()
            .filter(|f| f.relationship == Relationship::Friend)
            .map(|f| f.steamid)
    }

    fn friend_name(&self, steamid: SteamId) -> String {
        match self.friends.get(&steamid) {
            Some(friend) if !friend.name.is_empty() => friend.name.clone(),
            _ => "Someone".into(),
        }
    }

    fn actions(&self) -> Vec<Action> {
        use Action::*;
        match self.section() {
            Section::Library => {
                let Some(game) = self.game() else {
                    return Vec::new();
                };
                let favorite = if self.favorites.contains(&game.appid) {
                    Unfavorite
                } else {
                    Favorite
                };
                let running = self.running.as_ref().is_some_and(|g| g.appid == game.appid);
                match &game.installed {
                    // Its files are in use.
                    Some(i) if i.by_fumes && running => vec![Dlc, StorePage, favorite],
                    Some(i) if i.by_fumes => vec![
                        Play, CloudSync, Update, Verify, Dlc, StorePage, favorite, Uninstall,
                    ],
                    Some(_) => vec![PlayInSteam, Dlc, StorePage, favorite, UninstallInSteam],
                    None => vec![Install, Dlc, StorePage, favorite],
                }
            }
            Section::Friends => {
                let Some(friend) = self.friend() else {
                    return Vec::new();
                };
                match friend.relationship {
                    Relationship::Friend => {
                        let mut actions = Vec::new();
                        let invited = self
                            .chats
                            .get(&friend.steamid)
                            .is_some_and(|c| c.invite.is_some());
                        if invited {
                            actions.push(AcceptGameInvite);
                        }
                        if friend.join().is_some() {
                            actions.push(JoinGame);
                        }
                        if self.running.is_some() {
                            actions.push(InviteToGame);
                        }
                        actions.push(Message);
                        actions.push(if self.favorite_friends.contains(&friend.steamid) {
                            Unfavorite
                        } else {
                            Favorite
                        });
                        actions
                    }
                    Relationship::RequestReceived => vec![AcceptRequest, DeclineRequest],
                    Relationship::RequestSent => vec![CancelRequest],
                }
            }
            Section::Store => Vec::new(),
            Section::Profile if self.link == Link::Rejected => {
                vec![SignIn, CommunityProfile, EngineStatus, EngineSetup, SignOut]
            }
            Section::Profile if self.account.is_some() => {
                vec![CommunityProfile, EngineStatus, EngineSetup, SignOut]
            }
            Section::Profile => vec![SignIn, EngineStatus, EngineSetup],
        }
    }

    // Keys

    fn key(&mut self, key: KeyEvent) {
        self.status = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.try_quit();
        }
        if self.conflict {
            let keep_local = match key.code {
                KeyCode::Char('c' | 'C') => Some(false),
                KeyCode::Char('l' | 'L') => Some(true),
                KeyCode::Esc => None,
                _ => return,
            };
            if let Some(game) = &mut self.running {
                game.answer(keep_local);
            }
            self.conflict = false;
            return;
        }
        if let Some(confirm) = self.confirm.take() {
            // Anything but yes cancels.
            if matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter) {
                self.start(confirm.job);
            }
            return;
        }
        if let Some(field) = self.editing {
            return self.edit(field, key.code);
        }
        let section = self.section();
        match key.code {
            KeyCode::Char('q') => self.try_quit(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('/') => {
                if section != Section::Friends {
                    self.sections.select(Some(0));
                }
                self.focus_on(Focus::Items);
                self.editing = Some(Field::Search);
            }
            KeyCode::Char('f')
                if matches!(section, Section::Library | Section::Friends)
                    && self.focus != Focus::Sections =>
            {
                self.toggle_favorite()
            }
            KeyCode::Esc if self.focus == Focus::Items && !self.search_text().is_empty() => {
                self.search_text_mut().clear();
                self.searched();
            }
            code => self.navigate(code),
        }
    }

    /// A key while a text box has the keyboard.
    fn edit(&mut self, field: Field, code: KeyCode) {
        let text = match field {
            Field::Search => self.search_text_mut(),
            Field::Message => &mut self.draft,
        };
        match (field, code) {
            (_, KeyCode::Char(c)) => text.push(c),
            (_, KeyCode::Backspace) => {
                text.pop();
            }
            (Field::Search, KeyCode::Esc) => {
                text.clear();
                self.editing = None;
            }
            (Field::Search, KeyCode::Enter) | (_, KeyCode::Esc) => {
                self.editing = None;
                return;
            }
            // Arrows still move through what the search found.
            (Field::Search, code) => return self.navigate(code),
            (Field::Message, KeyCode::Enter) => return self.send_draft(),
            _ => return,
        }
        if field == Field::Search {
            self.searched();
        }
    }

    /// The search of the section on screen.
    fn search_text(&self) -> &str {
        match self.section() {
            Section::Friends => &self.friend_search,
            _ => &self.search,
        }
    }

    fn search_text_mut(&mut self) -> &mut String {
        match self.section() {
            Section::Friends => &mut self.friend_search,
            _ => &mut self.search,
        }
    }

    fn searched(&mut self) {
        match self.section() {
            Section::Friends => {
                let keep = self.friend_row();
                self.apply_friend_search(keep);
            }
            _ => {
                let keep = self.game().map(|g| g.appid);
                self.apply_search(keep);
            }
        }
    }

    fn navigate(&mut self, code: KeyCode) {
        let page = self.page as isize;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-page),
            KeyCode::PageDown => self.step(page),
            KeyCode::Home => self.step(isize::MIN),
            KeyCode::End => self.step(isize::MAX),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.forward(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc | KeyCode::Backspace => self.back(),
            _ => {}
        }
    }

    /// Move the selection in the focused column, stopping at the ends.
    fn step(&mut self, by: isize) {
        let section = self.section();
        let actions = self.actions().len();
        let (state, len) = match (self.focus, section) {
            (Focus::Sections, _) => (&mut self.sections, Section::ALL.len()),
            (Focus::Items, Section::Library) => (&mut self.games_list, self.shown.len()),
            (Focus::Items, Section::Friends) => (&mut self.friends_list, self.friend_rows.len()),
            (Focus::Items, _) => return,
            (Focus::Actions, _) => (&mut self.actions_list, actions),
        };
        if len == 0 {
            state.select(None);
            return;
        }
        let at = state
            .selected()
            .unwrap_or(0)
            .saturating_add_signed(by)
            .min(len - 1);
        state.select(Some(at));
        if self.focus != Focus::Actions {
            // A different item has different actions.
            self.actions_list.select(Some(0));
        }
    }

    fn forward(&mut self) {
        match self.focus {
            Focus::Sections => match self.section() {
                Section::Store => self.start(Job::Open(STORE_URL.into())),
                Section::Profile => self.focus_on(Focus::Actions),
                Section::Library if self.shown.is_empty() => {}
                Section::Friends if self.friend_rows.is_empty() => {}
                _ => self.focus_on(Focus::Items),
            },
            Focus::Items if !self.actions().is_empty() => self.focus_on(Focus::Actions),
            Focus::Items => {}
            Focus::Actions => self.activate(),
        }
    }

    fn back(&mut self) {
        match self.focus {
            Focus::Sections => {}
            Focus::Items => self.focus_on(Focus::Sections),
            // The profile has no middle column to go back to.
            Focus::Actions if self.section() == Section::Profile => self.focus_on(Focus::Sections),
            Focus::Actions => self.focus_on(Focus::Items),
        }
    }

    fn focus_on(&mut self, focus: Focus) {
        if focus == Focus::Actions {
            self.actions_list.select(Some(0));
        }
        // The search bar lives in the middle column, the message box in
        // the right one.
        let keeps_keyboard = match self.editing {
            Some(Field::Search) => focus == Focus::Items,
            Some(Field::Message) => focus == Focus::Actions,
            _ => true,
        };
        if !keeps_keyboard {
            self.editing = None;
        }
        self.focus = focus;
    }

    fn toggle_favorite(&mut self) {
        match self.section() {
            Section::Library => self.toggle_favorite_game(),
            Section::Friends => self.toggle_favorite_friend(),
            _ => {}
        }
    }

    fn toggle_favorite_game(&mut self) {
        let Some(game) = self.game() else {
            return;
        };
        let (appid, name) = (game.appid, display_name(game));
        let added = toggle(&mut self.favorites, appid);
        self.status = Some(
            match favorites::save(&self.dirs.favorites_file(), &self.favorites) {
                Ok(()) if added => format!("★ {name} is a favorite."),
                Ok(()) => format!("{name} is no longer a favorite."),
                Err(e) => format!("Couldn't save favorites: {e:#}"),
            },
        );
        // Favorites move to the top; the selection follows the game.
        self.apply_search(Some(appid));
    }

    fn toggle_favorite_friend(&mut self) {
        let Some(friend) = self
            .friend()
            .filter(|f| f.relationship == Relationship::Friend)
        else {
            return;
        };
        let steamid = friend.steamid;
        let name = self.friend_name(steamid);
        let added = toggle(&mut self.favorite_friends, steamid);
        let path = self.dirs.favorite_friends_file();
        self.status = Some(match favorites::save(&path, &self.favorite_friends) {
            Ok(()) if added => format!("★ {name} is a favorite."),
            Ok(()) => format!("{name} is no longer a favorite."),
            Err(e) => format!("Couldn't save favorites: {e:#}"),
        });
        self.apply_friend_search(Some(steamid));
    }

    /// Getting into the selected friend's game, the way Steam's "Join
    /// Game" does: the game starts with their session on its command line.
    /// Through fumes if fumes installed it, through Steam if Steam did.
    fn join(&self) -> Result<Job, String> {
        let friend = self.friend().ok_or_else(String::new)?;
        let who = self.friend_name(friend.steamid);
        let (appid, join) = friend
            .join()
            .ok_or_else(|| format!("{who}'s game can't be joined right now."))?;
        self.join_with(friend.steamid, appid, &join)
    }

    /// Taking up the selected friend's invite: joining the session it's for.
    fn accept_game_invite(&mut self) -> Result<Job, String> {
        let friend = self.friend().ok_or_else(String::new)?;
        let steamid = friend.steamid;
        let playing = match friend.playing {
            Some(Playing::App(appid)) => Some(appid),
            _ => None,
        };
        let invite = self
            .chats
            .get(&steamid)
            .and_then(|c| c.invite.clone())
            .ok_or_else(String::new)?;
        let appid = invite.appid.or(playing).ok_or_else(|| {
            format!(
                "The invite doesn't say which game it's for, and {} isn't in one now.",
                self.friend_name(steamid)
            )
        })?;
        let job = self.join_with(steamid, appid, &invite.join)?;
        if let Some(chat) = self.chats.get_mut(&steamid) {
            chat.invite = None;
        }
        Ok(job)
    }

    fn join_with(&self, steamid: SteamId, appid: u32, join: &Join) -> Result<Job, String> {
        let who = self.friend_name(steamid);
        let game = self.games.iter().find(|g| g.appid == appid);
        let title = game.map_or_else(|| format!("app {appid}"), display_name);
        match game.map(|g| &g.installed) {
            Some(Some(installed)) if installed.by_fumes => {
                let mut args = vec!["launch".to_owned(), appid.to_string(), "--".to_owned()];
                args.extend(join.args());
                Ok(Job::Play(Launch { appid, title, args }))
            }
            Some(Some(_)) => Ok(Job::Open(match join {
                Join::Lobby(lobby) => {
                    format!("steam://joinlobby/{appid}/{lobby}/{steamid}")
                }
                Join::Connect(connect) => {
                    format!("steam://rungameid/{appid}//{}", connect.replace(' ', "%20"))
                }
            })),
            Some(None) => Err(format!(
                "Install {title} first (Library → Install) to join {who}."
            )),
            None => Err(format!(
                "{who} is playing {title}, which this account doesn't own."
            )),
        }
    }

    fn send_draft(&mut self) {
        let text = self.draft.trim().to_owned();
        let Some(steamid) = self.viewed_friend() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if self.link != Link::Connected {
            // Keep the draft for when the connection is back.
            self.status = Some("Not connected to Steam; the message wasn't sent.".into());
            return;
        }
        self.draft.clear();
        self.request(Request::Send { to: steamid, text });
    }

    /// Run the selected action.
    fn activate(&mut self) {
        let Some(&action) = self
            .actions()
            .get(self.actions_list.selected().unwrap_or(0))
        else {
            return;
        };
        let (appid, name) = self
            .game()
            .map(|g| (g.appid, g.name.clone()))
            .unwrap_or_default();
        let id = appid.to_string();
        let friend = self.friend().map(|f| f.steamid);
        let command = |title: String, args: &[&str], show_output| {
            Job::Command(Pending {
                title,
                args: args.iter().map(|a| a.to_string()).collect(),
                show_output,
            })
        };
        let fumes = |title, args: &[&str]| command(title, args, false);
        let report = |title, args: &[&str]| command(title, args, true);
        let job = match action {
            Action::Play => Job::Play(Launch {
                appid,
                title: if name.is_empty() {
                    format!("App {appid}")
                } else {
                    name.clone()
                },
                args: vec!["launch".into(), id.clone()],
            }),
            Action::PlayInSteam => Job::Open(format!("steam://rungameid/{appid}")),
            Action::Install => fumes(format!("Installing {name}"), &["install", &id]),
            Action::Update => fumes(format!("Updating {name}"), &["install", &id]),
            Action::Verify => fumes(format!("Verifying {name}"), &["install", &id, "--verify"]),
            Action::CloudSync => report(
                format!("Syncing {name}'s cloud saves"),
                &["engine", "cloud-info", &id, "--pull"],
            ),
            Action::Dlc => Job::Open(format!("https://store.steampowered.com/dlc/{appid}")),
            Action::StorePage => Job::Open(format!("https://store.steampowered.com/app/{appid}")),
            Action::Uninstall => {
                self.confirm = Some(Confirm {
                    question: format!("Delete {name} from this machine?"),
                    job: fumes(format!("Uninstalling {name}"), &["uninstall", &id, "--yes"]),
                });
                return;
            }
            Action::UninstallInSteam => Job::Open(format!("steam://uninstall/{appid}")),
            Action::Favorite | Action::Unfavorite => return self.toggle_favorite(),
            Action::Message => {
                self.editing = Some(Field::Message);
                return;
            }
            Action::JoinGame | Action::AcceptGameInvite => {
                let job = if action == Action::JoinGame {
                    self.join()
                } else {
                    self.accept_game_invite()
                };
                match job {
                    Ok(job) => job,
                    Err(why) => {
                        self.status = Some(why);
                        return;
                    }
                }
            }
            Action::InviteToGame => {
                if let (Some(steamid), Some(game)) = (friend, &self.running) {
                    let status =
                        format!("Inviting {} to {}…", self.friend_name(steamid), game.title);
                    self.request(Request::Invite(steamid));
                    self.status = Some(status);
                }
                return;
            }
            Action::AcceptRequest => {
                if let Some(steamid) = friend {
                    self.request(Request::Accept(steamid));
                }
                return;
            }
            Action::DeclineRequest | Action::CancelRequest => {
                if let Some(steamid) = friend {
                    self.request(Request::Remove(steamid));
                    self.status = Some(if action == Action::DeclineRequest {
                        "Friend request declined.".into()
                    } else {
                        "Friend request cancelled.".into()
                    });
                }
                return;
            }
            Action::SignIn => fumes("Signing in to Steam".into(), &["login"]),
            Action::SignOut => {
                self.confirm = Some(Confirm {
                    question: "Sign out of Steam? Installed games stay.".into(),
                    job: fumes("Signing out".into(), &["logout"]),
                });
                return;
            }
            Action::CommunityProfile => match &self.account {
                Some((_, steamid)) => {
                    Job::Open(format!("https://steamcommunity.com/profiles/{steamid}"))
                }
                None => return,
            },
            Action::EngineStatus => report("Steam engine status".into(), &["engine", "status"]),
            Action::EngineSetup => {
                fumes("Setting up the Steam engine".into(), &["engine", "setup"])
            }
        };
        self.start(job);
    }

    fn start(&mut self, job: Job) {
        match job {
            Job::Command(pending) => self.pending = Some(pending),
            Job::Play(launch) => self.launch = Some(launch),
            Job::Open(url) => {
                self.status = Some(match crate::open_url(&url) {
                    Ok(()) if url.starts_with("steam://") => "Sent to Steam.".into(),
                    Ok(()) => format!("Opened {url} in the browser."),
                    Err(e) => format!("{e:#}"),
                })
            }
        }
    }

    // Drawing

    fn draw(&mut self, frame: &mut Frame) {
        let [main, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        // Conversations need the room more than the friends list does.
        let (middle_width, right_width) = match self.section() {
            Section::Friends => (Constraint::Length(36), Constraint::Fill(1)),
            _ => (Constraint::Fill(1), Constraint::Length(36)),
        };
        let [left, middle, right] =
            Layout::horizontal([Constraint::Length(16), middle_width, right_width]).areas(main);

        self.draw_sections(frame, left);
        match self.section() {
            Section::Library => self.draw_library(frame, middle),
            Section::Friends => self.draw_friends(frame, middle),
            Section::Store => self.draw_store(frame, middle),
            Section::Profile => self.draw_profile(frame, middle),
        }
        self.draw_details(frame, right);
        self.draw_footer(frame, footer);
        if let Some(confirm) = &self.confirm {
            draw_confirm(frame, &confirm.question);
        }
        if self.conflict
            && let Some(game) = &self.running
        {
            draw_conflict(frame, &game.title);
        }
    }

    fn column(&self, title: String, focus: Focus) -> Block<'static> {
        let focused = self.focus == focus;
        Block::bordered()
            .border_type(if focused {
                BorderType::Thick
            } else {
                BorderType::Rounded
            })
            .border_style(Style::new().fg(if focused { ACCENT } else { DIM }))
            .title(title)
    }

    /// The selected row: filled in the focused column, just colored in the
    /// others so it's clear where focus would land.
    fn highlight(&self, focus: Focus) -> Style {
        let style = Style::new().add_modifier(Modifier::BOLD);
        if self.focus == focus {
            style.bg(ACCENT).fg(Color::Black)
        } else {
            style.fg(ACCENT)
        }
    }

    fn draw_sections(&mut self, frame: &mut Frame, area: Rect) {
        let unread: u32 = self.chats.values().map(|c| c.unread).sum();
        let items: Vec<ListItem> = Section::ALL
            .iter()
            .map(|&s| match s {
                Section::Friends if unread > 0 => ListItem::new(Line::from(vec![
                    Span::raw(format!(" {} ", s.label())),
                    Span::styled(format!("({unread})"), Style::new().fg(Color::Yellow)),
                ])),
                _ => ListItem::new(format!(" {}", s.label())),
            })
            .collect();
        let list = List::new(items)
            .block(self.column(" fumes ".into(), Focus::Sections))
            .highlight_style(self.highlight(Focus::Sections));
        frame.render_stateful_widget(list, area, &mut self.sections);
    }

    /// The search bar at the top of a list, and the rest of the column
    /// for the list itself.
    fn draw_search(&mut self, frame: &mut Frame, inner: Rect, text: &str) -> Rect {
        let [search_area, _, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        self.page = list_area.height.max(1) as usize;

        let typing = self.editing == Some(Field::Search);
        let icon = Span::styled(" / ", Style::new().fg(if typing { ACCENT } else { DIM }));
        let bar = if text.is_empty() && !typing {
            Line::from(vec![icon, Span::styled("Search", Style::new().fg(DIM))])
        } else {
            Line::from(vec![icon, Span::raw(text.to_owned())])
        };
        frame.render_widget(Paragraph::new(bar), search_area);
        if typing {
            let typed = text.chars().count() as u16;
            frame.set_cursor_position((
                (search_area.x + 3 + typed).min(search_area.right().saturating_sub(1)),
                search_area.y,
            ));
        }
        list_area
    }

    fn draw_library(&mut self, frame: &mut Frame, area: Rect) {
        let mut title = if self.search.is_empty() {
            format!(" Library ({}) ", self.games.len())
        } else {
            format!(" Library ({} of {}) ", self.shown.len(), self.games.len())
        };
        if self.refreshing {
            title.push_str("· refreshing… ");
        }
        let block = self.column(title, Focus::Items);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let search = self.search.clone();
        let list_area = self.draw_search(frame, inner, &search);

        if self.shown.is_empty() {
            let message = if !self.games.is_empty() {
                "No games match."
            } else if self.refreshing {
                "Loading your games…"
            } else if self.account.is_none() {
                "Not signed in. Go to Profile → Sign in."
            } else {
                "No games."
            };
            let text = Paragraph::new(format!(" {message}")).style(Style::new().fg(DIM));
            frame.render_widget(text, list_area);
            return;
        }

        let items: Vec<ListItem> = self
            .shown
            .iter()
            .map(|&i| {
                let game = &self.games[i];
                game_item(game, self.favorites.contains(&game.appid))
            })
            .collect();
        let list = List::new(items).highlight_style(self.highlight(Focus::Items));
        frame.render_stateful_widget(list, list_area, &mut self.games_list);
        let selected = self.games_list.selected().unwrap_or(0);
        draw_scrollbar(frame, area, list_area, self.shown.len(), selected);
    }

    fn draw_friends(&mut self, frame: &mut Frame, area: Rect) {
        let mates = self
            .friends
            .values()
            .filter(|f| f.relationship == Relationship::Friend);
        let total = mates.clone().count();
        let online = mates.filter(|f| f.status != Status::Offline).count();
        let title = match &self.link {
            Link::Connected => format!(" Friends ({online} of {total} online) "),
            Link::Offline(_) | Link::Rejected => " Friends · offline ".to_owned(),
            Link::Connecting | Link::SignedOut => " Friends ".to_owned(),
        };
        let block = self.column(title, Focus::Items);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let search = self.friend_search.clone();
        let list_area = self.draw_search(frame, inner, &search);

        if self.friend_rows.is_empty() {
            let message = match &self.link {
                Link::SignedOut => "Not signed in. Go to Profile → Sign in.".to_owned(),
                Link::Connecting => "Connecting to Steam…".to_owned(),
                Link::Offline(why) => format!("Offline ({why}). Trying again…"),
                Link::Rejected => {
                    "Steam ended fumes' saved sign-in, so friends can't load. Go to Profile → Sign in."
                        .into()
                }
                Link::Connected if !self.friend_search.is_empty() => "No friends match.".into(),
                Link::Connected => "No friends yet.".into(),
            };
            let text = Paragraph::new(message)
                .style(Style::new().fg(DIM))
                .wrap(Wrap { trim: true })
                .block(Block::new().padding(Padding::horizontal(1)));
            frame.render_widget(text, list_area);
            return;
        }

        let items: Vec<ListItem> = self
            .friend_rows
            .iter()
            .map(|steamid| self.friend_item(&self.friends[steamid]))
            .collect();
        let list = List::new(items).highlight_style(self.highlight(Focus::Items));
        frame.render_stateful_widget(list, list_area, &mut self.friends_list);
        let selected = self.friends_list.selected().unwrap_or(0);
        draw_scrollbar(frame, area, list_area, self.friend_rows.len(), selected);
    }

    fn friend_item(&self, friend: &Friend) -> ListItem<'static> {
        let (presence, color) = self.presence(friend);
        let favorite = self.favorite_friends.contains(&friend.steamid);
        let unread = self.chats.get(&friend.steamid).map_or(0, |c| c.unread);
        let mut spans = vec![
            Span::styled(" ●", Style::new().fg(color)),
            Span::styled(
                if favorite { " ★ " } else { "   " },
                Style::new().fg(Color::Yellow),
            ),
            Span::styled(
                friend_label(friend),
                if unread > 0 {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                },
            ),
        ];
        if unread > 0 {
            spans.push(Span::styled(
                format!(" ({unread})"),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
        // Online is what the dot already says; the rest is worth spelling out.
        if friend.playing.is_some() || friend.relationship != Relationship::Friend {
            spans.push(Span::styled(format!(" · {presence}"), Style::new().fg(DIM)));
        }
        ListItem::new(Line::from(spans))
    }

    /// What a friend is up to, and the color for it.
    fn presence(&self, friend: &Friend) -> (String, Color) {
        match (friend.relationship, &friend.playing, friend.status) {
            (Relationship::RequestReceived, ..) => {
                ("sent you a friend request".into(), Color::Yellow)
            }
            (Relationship::RequestSent, ..) => ("friend request sent".into(), DIM),
            (_, Some(Playing::Named(game)), _) => (format!("playing {game}"), Color::Green),
            (_, Some(Playing::App(appid)), _) => {
                let game = self.games.iter().find(|g| g.appid == *appid);
                let text = match game {
                    Some(game) => format!("playing {}", display_name(game)),
                    None => "in a game".into(),
                };
                (text, Color::Green)
            }
            (_, None, Status::Online) => ("online".into(), Color::LightBlue),
            (_, None, Status::Busy) => ("busy".into(), Color::Blue),
            (_, None, Status::Away) => ("away".into(), Color::Blue),
            (_, None, Status::Offline) => ("offline".into(), DIM),
        }
    }

    fn draw_store(&self, frame: &mut Frame, area: Rect) {
        let text = Paragraph::new(vec![
            Line::raw("The Steam store opens in your browser."),
            Line::raw(""),
            Line::styled("Press → or Enter.", Style::new().fg(DIM)),
        ])
        .wrap(Wrap { trim: false })
        .block(
            self.column(" Store ".into(), Focus::Items)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(text, area);
    }

    fn draw_profile(&self, frame: &mut Frame, area: Rect) {
        let mut lines = match &self.account {
            Some((account, steamid)) => vec![
                Line::styled(account.clone(), Style::new().add_modifier(Modifier::BOLD)),
                field("Steam ID", steamid.to_string()),
            ],
            None => vec![
                Line::styled("Not signed in", Style::new().add_modifier(Modifier::BOLD)),
                Line::styled(
                    "Sign in from the column on the right.",
                    Style::new().fg(DIM),
                ),
            ],
        };
        let owned = self.games.iter().filter(|g| g.owned.is_some()).count();
        let by_fumes = self
            .games
            .iter()
            .filter(|g| g.installed.as_ref().is_some_and(|i| i.by_fumes))
            .count();
        let installed = self.games.iter().filter(|g| g.installed.is_some()).count();
        let minutes: u64 = self
            .games
            .iter()
            .filter_map(|g| g.owned.as_ref())
            .map(|o| o.playtime_minutes as u64)
            .sum();
        let platform = steam::runtime::host();
        lines.extend([
            Line::raw(""),
            field("Games", owned.to_string()),
            field("Favorites", self.favorites.len().to_string()),
            field(
                "Installed",
                format!(
                    "{installed} ({by_fumes} by fumes, {} by Steam)",
                    installed - by_fumes
                ),
            ),
            field("Playtime", format!("{:.0} h", minutes as f64 / 60.0)),
            Line::raw(""),
            field("Library", self.dirs.library().display().to_string()),
            field(
                "Engine",
                format!(
                    "client build {}, {}",
                    platform.version(),
                    if self.engine_fetched {
                        "downloaded"
                    } else {
                        "not downloaded yet"
                    }
                ),
            ),
        ]);
        let text = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            self.column(" Profile ".into(), Focus::Items)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(text, area);
    }

    /// The right column: what's selected, what can be done with it, then
    /// more about it (or the conversation, for a friend).
    fn draw_details(&mut self, frame: &mut Frame, area: Rect) {
        let block = self
            .column(" Actions ".into(), Focus::Actions)
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut chat = None;
        let (heading, details): (Vec<Line>, Vec<Line>) = match self.section() {
            Section::Library => match self.game() {
                Some(game) => {
                    let star = if self.favorites.contains(&game.appid) {
                        "★ "
                    } else {
                        ""
                    };
                    let mut details = game_details(game);
                    if let Some(running) = self.running.as_ref().filter(|g| g.appid == game.appid) {
                        details.splice(
                            0..0,
                            [
                                Line::styled(
                                    format!("▶ {}", running.state),
                                    Style::new().fg(Color::Green),
                                ),
                                Line::raw(""),
                            ],
                        );
                        details.push(Line::raw(""));
                        details.extend(
                            running
                                .log
                                .iter()
                                .map(|l| Line::styled(l.clone(), Style::new().fg(DIM))),
                        );
                    }
                    (
                        vec![Line::styled(format!("{star}{}", display_name(game)), bold)],
                        details,
                    )
                }
                None => return,
            },
            Section::Friends => match self.friend_row() {
                None => return,
                Some(steamid) => {
                    let friend = &self.friends[&steamid];
                    let (presence, color) = self.presence(friend);
                    let star = if self.favorite_friends.contains(&steamid) {
                        "★ "
                    } else {
                        ""
                    };
                    if friend.relationship == Relationship::Friend {
                        chat = Some(steamid);
                    }
                    (
                        vec![
                            Line::styled(format!("{star}{}", friend_label(friend)), bold),
                            Line::styled(presence, Style::new().fg(color)),
                        ],
                        Vec::new(),
                    )
                }
            },
            Section::Store => (vec![Line::styled("Steam Store", bold)], Vec::new()),
            Section::Profile => match &self.account {
                Some((account, _)) => (vec![Line::styled(account.clone(), bold)], Vec::new()),
                None => (vec![Line::styled("Not signed in", bold)], Vec::new()),
            },
        };
        let actions = self.actions();

        let width = inner.width.max(1) as usize;
        let heading_rows: usize = heading
            .iter()
            .map(|line| line.width().div_ceil(width).max(1))
            .sum();
        let [heading_area, _, actions_area, _, details_area] = Layout::vertical([
            Constraint::Length(heading_rows as u16),
            Constraint::Length(1),
            Constraint::Length(actions.len() as u16),
            Constraint::Length(if actions.is_empty() { 0 } else { 1 }),
            Constraint::Fill(1),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(heading).wrap(Wrap { trim: true }),
            heading_area,
        );

        let items: Vec<ListItem> = actions.iter().map(|a| ListItem::new(a.label())).collect();
        let list = List::new(items).highlight_style(self.highlight(Focus::Actions));
        if self.focus == Focus::Actions {
            frame.render_stateful_widget(list, actions_area, &mut self.actions_list);
        } else {
            frame.render_widget(list, actions_area);
        }

        match chat {
            Some(steamid) => self.draw_chat(frame, details_area, steamid),
            None => {
                let details = Paragraph::new(details).wrap(Wrap { trim: false });
                frame.render_widget(details, details_area);
            }
        }
    }

    /// The conversation, newest at the bottom, with the message box under
    /// it.
    fn draw_chat(&self, frame: &mut Frame, area: Rect, steamid: SteamId) {
        let [rule_area, messages_area, input_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new("─".repeat(rule_area.width as usize)).style(Style::new().fg(DIM)),
            rule_area,
        );

        let name = self.friend_name(steamid);
        let chat = self.chats.get(&steamid);
        let lines: Vec<Line> = match chat {
            Some(chat) if !chat.messages.is_empty() => chat
                .messages
                .iter()
                .flat_map(|m| message_lines(m, &name))
                .collect(),
            Some(chat) if chat.loaded => {
                vec![Line::styled("No messages yet.", Style::new().fg(DIM))]
            }
            _ if self.link == Link::Connected => {
                vec![Line::styled("Loading messages…", Style::new().fg(DIM))]
            }
            _ => vec![Line::styled(
                "Messages load once fumes is connected to Steam.",
                Style::new().fg(DIM),
            )],
        };
        let messages = Paragraph::new(lines).wrap(Wrap { trim: false });
        // Stick to the newest messages.
        let rows = messages.line_count(messages_area.width) as u16;
        let scroll = rows.saturating_sub(messages_area.height);
        frame.render_widget(messages.scroll((scroll, 0)), messages_area);

        let typing = self.editing == Some(Field::Message);
        let prompt = Span::styled("> ", Style::new().fg(if typing { ACCENT } else { DIM }));
        let room = (input_area.width as usize).saturating_sub(3);
        let line = if typing || !self.draft.is_empty() {
            // The end of the draft, where the typing happens.
            let chars: Vec<char> = self.draft.chars().collect();
            let tail: String = chars[chars.len().saturating_sub(room)..].iter().collect();
            if typing {
                frame.set_cursor_position((
                    input_area.x + 2 + tail.chars().count() as u16,
                    input_area.y,
                ));
            }
            Line::from(vec![prompt, Span::raw(tail)])
        } else {
            Line::from(vec![
                prompt,
                Span::styled("Send message to type", Style::new().fg(DIM)),
            ])
        };
        frame.render_widget(Paragraph::new(line), input_area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let mut line = match &self.status {
            Some(status) => Line::styled(format!(" {status}"), Style::new().fg(Color::Yellow)),
            None => {
                let hints = match (self.editing, self.focus, self.section()) {
                    (Some(Field::Search), ..) => {
                        "type to search · ↑↓ move · Enter done · Esc clear"
                    }
                    (Some(Field::Message), ..) => "type a message · Enter send · Esc stop typing",
                    (None, Focus::Sections, _) => "↑↓ move · → open · q quit",
                    (None, Focus::Items, Section::Library) => {
                        "↑↓ PgUp PgDn scroll · → actions · / search · f favorite · r refresh · ← back · q quit"
                    }
                    (None, Focus::Items, Section::Friends) => {
                        "↑↓ move · → actions · / search · f favorite · ← back · q quit"
                    }
                    (None, Focus::Items, _) => "↑↓ move · → actions · ← back · q quit",
                    (None, Focus::Actions, Section::Library | Section::Friends) => {
                        "↑↓ move · Enter run · f favorite · ← back · q quit"
                    }
                    (None, Focus::Actions, _) => "↑↓ move · Enter run · ← back · q quit",
                };
                Line::styled(format!(" {hints}"), Style::new().fg(DIM))
            }
        };
        if let Some(game) = &self.running {
            let mut spans = vec![
                Span::styled(
                    format!(" ▶ {}: {} ", game.title, game.state.trim_end_matches('.')),
                    Style::new().fg(Color::Green),
                ),
                Span::styled("│", Style::new().fg(DIM)),
            ];
            spans.append(&mut line.spans);
            line = Line::from(spans);
        }
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// Add `item` if it's missing, take it out if it's there. True if added.
fn toggle<T: Ord>(set: &mut BTreeSet<T>, item: T) -> bool {
    if set.remove(&item) {
        false
    } else {
        set.insert(item);
        true
    }
}

fn draw_scrollbar(frame: &mut Frame, column: Rect, list: Rect, len: usize, selected: usize) {
    if len <= list.height as usize {
        return;
    }
    let mut state = ScrollbarState::new(len).position(selected);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
    // On the column's right border, beside the list.
    let track = Rect {
        x: column.x,
        width: column.width,
        ..list
    };
    frame.render_stateful_widget(scrollbar, track, &mut state);
}

fn display_name(game: &Game) -> String {
    if game.name.is_empty() {
        format!("App {}", game.appid)
    } else {
        game.name.clone()
    }
}

fn friend_label(friend: &Friend) -> String {
    if friend.name.is_empty() {
        // For the moment until Steam sends the name.
        "…".to_owned()
    } else {
        friend.name.clone()
    }
}

fn message_lines(message: &Message, name: &str) -> Vec<Line<'static>> {
    let (who, color) = if message.from_me {
        ("You".to_owned(), Color::Green)
    } else {
        (name.to_owned(), ACCENT)
    };
    let mut lines = message.text.lines();
    let first = lines.next().unwrap_or_default().to_owned();
    let mut out = vec![Line::from(vec![
        Span::styled(
            format!("{who}: "),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(first),
    ])];
    out.extend(lines.map(|line| Line::raw(format!("  {line}"))));
    out
}

fn game_item(game: &Game, favorite: bool) -> ListItem<'static> {
    let (mark, color) = match &game.installed {
        Some(i) if i.by_fumes => ("●", Color::Green),
        Some(i) if i.complete => ("●", Color::Blue),
        Some(_) => ("◐", Color::Yellow),
        None => ("○", DIM),
    };
    let star = if favorite { "★ " } else { "  " };
    ListItem::new(Line::from(vec![
        Span::styled(format!(" {mark}"), Style::new().fg(color)),
        Span::styled(format!(" {star}"), Style::new().fg(Color::Yellow)),
        Span::raw(display_name(game)),
    ]))
}

/// Lowercase words with punctuation dropped:
/// "ARK: Survival Evolved" → ["ark", "survival", "evolved"].
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// How well a name matches a search (lower is better), or `None` if it
/// doesn't. Every word searched for has to be in the name, or be part of
/// its initials ("gta"); a search for the id (app id, SteamID) finds that
/// one.
fn rank(query: &[String], raw: &str, name: &str, id: u64) -> Option<u8> {
    if query.is_empty() || raw.trim() == id.to_string() {
        return Some(0);
    }
    let name = words(name);
    let joined = name.join(" ");
    let initials: String = name.iter().filter_map(|w| w.chars().next()).collect();
    let found =
        |q: &String| joined.contains(q.as_str()) || (q.len() > 1 && initials.contains(q.as_str()));
    if !query.iter().all(found) {
        return None;
    }
    if joined.starts_with(&query.join(" ")) {
        // "portal" → "Portal", "Portal 2"
        Some(0)
    } else if query
        .iter()
        .all(|q| name.iter().any(|w| w.starts_with(q.as_str())))
    {
        // "survival ark" → "ARK: Survival Evolved"
        Some(1)
    } else {
        Some(2)
    }
}

fn game_details(game: &Game) -> Vec<Line<'static>> {
    let status = match &game.installed {
        Some(i) if i.by_fumes => "Installed",
        Some(i) if i.complete => "Installed by Steam",
        Some(_) => "Steam is updating it",
        None => "Not installed",
    };
    let mut lines = vec![field("Status", status.to_owned())];
    match &game.owned {
        Some(owned) => {
            lines.push(field(
                "Played",
                format!("{:.1} h", owned.playtime_minutes as f64 / 60.0),
            ));
            lines.push(field("Last played", ago(owned.last_played)));
        }
        None => lines.push(field("Owned", "not by this account".to_owned())),
    }
    if let Some(installed) = &game.installed {
        lines.push(field("Size", download::human(installed.size_on_disk)));
        lines.push(field("Folder", installed.path.display().to_string()));
    }
    lines.push(field("App ID", game.appid.to_string()));
    lines
}

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), Style::new().fg(DIM)),
        Span::raw(value),
    ])
}

fn ago(unix: u32) -> String {
    if unix == 0 {
        return "never".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let days = now.saturating_sub(unix as u64) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        2..60 => format!("{days} days ago"),
        60..730 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

fn draw_confirm(frame: &mut Frame, question: &str) {
    let area = centered(frame.area(), 52, 6);
    let text = Paragraph::new(vec![
        Line::raw(question.to_owned()),
        Line::raw(""),
        Line::styled("[y] yes   [n] no", Style::new().fg(DIM)),
    ])
    .wrap(Wrap { trim: true })
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::Yellow))
            .title(" Confirm "),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(text, area);
}

fn draw_conflict(frame: &mut Frame, title: &str) {
    let area = centered(frame.area(), 60, 8);
    let text = Paragraph::new(vec![
        Line::raw(format!(
            "{title}'s saves changed both in Steam Cloud and on this machine since they were \
             last in sync. Whichever you don't keep is overwritten."
        )),
        Line::raw(""),
        Line::styled(
            "[c] keep the cloud's   [l] keep this machine's   [Esc] don't play",
            Style::new().fg(DIM),
        ),
    ])
    .wrap(Wrap { trim: true })
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::Yellow))
            .title(" Cloud saves ")
            .padding(Padding::horizontal(1)),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(text, area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Run a `fumes` command with the terminal handed over to it, then take
/// the terminal back: straight away if it worked, or once Enter is pressed
/// if it didn't (or if its output is the point), so it can be read.
/// Returns whether it worked.
fn hand_over(terminal: &mut DefaultTerminal, pending: &Pending) -> Result<bool> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, Show)?;
    println!("── {} ──\n", pending.title);
    let exe = std::env::current_exe().context("finding fumes' own executable")?;
    let worked = match Command::new(exe).args(&pending.args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            println!("\nIt stopped ({status}).");
            false
        }
        Err(e) => {
            println!("\nCouldn't run it: {e}");
            false
        }
    };
    if !worked || pending.show_output {
        print!("\nPress Enter to go back to fumes… ");
        io::stdout().flush()?;
        io::stdin().read_line(&mut String::new())?;
    }
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(worked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::friends::Persona;
    use crate::local::Installed;
    use futures::channel::mpsc::UnboundedReceiver;
    use std::path::PathBuf;

    fn game(appid: u32, name: &str, installed: Option<bool>) -> Game {
        Game {
            appid,
            name: name.into(),
            owned: None,
            installed: installed.map(|by_fumes| Installed {
                appid,
                name: name.into(),
                path: PathBuf::from("/x"),
                size_on_disk: 0,
                complete: true,
                by_fumes,
            }),
        }
    }

    fn app(games: Vec<Game>) -> App {
        let root = tempfile::tempdir().unwrap().keep();
        let dirs = Dirs {
            data: root.join("data"),
            cache: root.join("cache"),
        };
        let mut app = App::new(dirs);
        app.set_games(games);
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn arrows_walk_the_columns() {
        let mut app = app(vec![
            game(1, "Alpha", None),
            game(2, "Beta", Some(true)),
            game(3, "Gamma", Some(false)),
        ]);
        press(&mut app, KeyCode::Right);
        assert!(app.focus == Focus::Items);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.game().unwrap().appid, 2);
        assert!(app.actions().contains(&Action::Play));
        press(&mut app, KeyCode::Right);
        assert!(app.focus == Focus::Actions);
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Down);
        assert!(app.actions().contains(&Action::PlayInSteam));
        // The list stops at its ends.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.game().unwrap().appid, 3);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.game().unwrap().appid, 1);
        press(&mut app, KeyCode::Left);
        assert!(app.focus == Focus::Sections);
    }

    #[test]
    fn search_keeps_the_selection_when_it_still_matches() {
        let mut app = app(vec![
            game(1, "Alpha", None),
            game(2, "Beta", None),
            game(3, "Alphabet Soup", None),
        ]);
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "alp");
        assert_eq!(app.shown.len(), 2);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.game().unwrap().appid, 3);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.shown.len(), 3);
        assert_eq!(app.game().unwrap().appid, 3);
    }

    #[test]
    fn uninstall_asks_first() {
        let mut app = app(vec![game(2, "Beta", Some(true))]);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::End);
        assert!(app.actions()[app.actions_list.selected().unwrap()] == Action::Uninstall);
        press(&mut app, KeyCode::Enter);
        assert!(app.confirm.is_some() && app.pending.is_none());
        press(&mut app, KeyCode::Char('n'));
        assert!(app.confirm.is_none() && app.pending.is_none());
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.pending.unwrap().args, ["uninstall", "2", "--yes"]);
    }

    fn ids(app: &App) -> Vec<u32> {
        app.shown.iter().map(|&i| app.games[i].appid).collect()
    }

    fn search(app: &mut App, text: &str) {
        press(app, KeyCode::Char('/'));
        type_text(app, text);
        press(app, KeyCode::Enter);
    }

    #[test]
    fn search_matches_words_and_initials_best_first() {
        let mut app = app(vec![
            game(1, "Aperture Portal Tales", None),
            game(2, "ARK: Survival Evolved", None),
            game(3, "Grand Theft Auto: San Andreas", None),
            game(4, "Portal", None),
            game(5, "Portal 2", None),
        ]);
        search(&mut app, "portal");
        assert_eq!(ids(&app), [4, 5, 1]);
        press(&mut app, KeyCode::Esc);

        search(&mut app, "survival ark");
        assert_eq!(ids(&app), [2]);
        press(&mut app, KeyCode::Esc);

        search(&mut app, "gta");
        assert_eq!(ids(&app), [3]);
        press(&mut app, KeyCode::Esc);

        search(&mut app, "12120");
        assert!(app.shown.is_empty());
        press(&mut app, KeyCode::Esc);
        search(&mut app, "5");
        assert_eq!(ids(&app), [5]);
    }

    #[test]
    fn favorites_go_first_and_are_saved() {
        let mut app = app(vec![
            game(1, "Alpha", None),
            game(2, "Beta", None),
            game(3, "Gamma", None),
        ]);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(ids(&app), [3, 1, 2]);
        // The selection follows the game to the top.
        assert_eq!(app.game().unwrap().appid, 3);
        assert!(app.actions().contains(&Action::Unfavorite));
        let file = app.dirs.favorites_file();
        assert_eq!(favorites::load::<u32>(&file), BTreeSet::from([3]));

        // Unfavoriting from the actions column puts it back.
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::End);
        assert!(app.actions()[app.actions_list.selected().unwrap()] == Action::Unfavorite);
        press(&mut app, KeyCode::Enter);
        assert_eq!(ids(&app), [1, 2, 3]);
        assert!(favorites::load::<u32>(&file).is_empty());
    }

    // Friends. Steam is stood in for by feeding the app updates and
    // reading the requests it makes.

    const MARA: SteamId = 76561198000000001;
    const NIGHTOWL: SteamId = 76561198000000002;
    const KESTREL: SteamId = 76561198000000003;
    const DUSK: SteamId = 76561198000000004;
    const PIXEL: SteamId = 76561198000000005;
    const PORTAL_2: u32 = 620;
    const TF2: u32 = 440;

    fn persona(steamid: SteamId, name: &str, status: Status, playing: Option<u32>) -> Persona {
        Persona {
            steamid,
            name: Some(name.into()),
            status: Some((status, playing.map(Playing::App))),
            connect: None,
            lobby: None,
        }
    }

    fn online_app() -> (App, UnboundedReceiver<Request>) {
        let mut app = app(vec![
            game(PORTAL_2, "Portal 2", Some(true)),
            game(TF2, "Team Fortress 2", None),
        ]);
        let (online, requests) = Online::detached();
        app.online = Some(online);
        app.update(Update::Connected);
        app.update(Update::Friends {
            full: true,
            changes: vec![
                (MARA, Some(Relationship::Friend)),
                (NIGHTOWL, Some(Relationship::Friend)),
                (KESTREL, Some(Relationship::RequestReceived)),
                (DUSK, Some(Relationship::RequestSent)),
                (PIXEL, Some(Relationship::Friend)),
            ],
        });
        for p in [
            persona(MARA, "Mara", Status::Online, None),
            persona(NIGHTOWL, "nightowl", Status::Offline, None),
            persona(KESTREL, "kestrel", Status::Online, None),
            persona(DUSK, "dusk", Status::Online, None),
            persona(PIXEL, "pixelpirate", Status::Online, Some(PORTAL_2)),
        ] {
            app.update(Update::Persona(p));
        }
        // Over to the friends column.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        (app, requests)
    }

    fn rows(app: &App) -> Vec<String> {
        app.friend_rows
            .iter()
            .map(|id| app.friends[id].name.clone())
            .collect()
    }

    fn select(app: &mut App, name: &str) {
        let at = rows(app).iter().position(|n| n == name).unwrap();
        app.friends_list.select(Some(at));
        app.actions_list.select(Some(0));
        app.sync();
    }

    fn requests(requests: &mut UnboundedReceiver<Request>) -> Vec<Request> {
        std::iter::from_fn(|| requests.try_recv().ok()).collect()
    }

    #[test]
    fn friends_are_ordered_requests_then_by_activity() {
        let (app, _) = online_app();
        assert_eq!(
            rows(&app),
            ["kestrel", "pixelpirate", "Mara", "nightowl", "dusk"]
        );
        let (presence, _) = app.presence(&app.friends[&PIXEL]);
        assert_eq!(presence, "playing Portal 2");
    }

    #[test]
    fn friends_can_be_searched_and_favorited() {
        let (mut app, _) = online_app();
        search(&mut app, "night");
        assert_eq!(rows(&app), ["nightowl"]);
        press(&mut app, KeyCode::Char('f'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            rows(&app),
            ["kestrel", "nightowl", "pixelpirate", "Mara", "dusk"]
        );
        let file = app.dirs.favorite_friends_file();
        assert_eq!(
            favorites::load::<SteamId>(&file),
            BTreeSet::from([NIGHTOWL])
        );

        // Friend requests can't be favorites.
        select(&mut app, "kestrel");
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(
            favorites::load::<SteamId>(&file),
            BTreeSet::from([NIGHTOWL])
        );
    }

    #[test]
    fn friend_requests_are_accepted_declined_and_cancelled() {
        let (mut app, mut sent) = online_app();
        select(&mut app, "kestrel");
        assert_eq!(
            app.actions(),
            [Action::AcceptRequest, Action::DeclineRequest]
        );
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        select(&mut app, "dusk");
        assert_eq!(app.actions(), [Action::CancelRequest]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            requests(&mut sent),
            [
                Request::Accept(KESTREL),
                Request::Remove(KESTREL),
                Request::Remove(DUSK),
            ]
        );

        // Steam's answer: kestrel is a friend now, dusk is gone.
        app.update(Update::Friends {
            full: false,
            changes: vec![(KESTREL, Some(Relationship::Friend)), (DUSK, None)],
        });
        assert_eq!(rows(&app), ["pixelpirate", "kestrel", "Mara", "nightowl"]);
    }

    #[test]
    fn joining_a_friend_starts_the_game_with_their_session() {
        let (mut app, mut sent) = online_app();
        select(&mut app, "pixelpirate");
        // Opening someone who's playing checks their game again.
        assert!(requests(&mut sent).contains(&Request::Personas(vec![PIXEL])));
        // Nothing to join yet: no connect string, no lobby.
        assert!(!app.actions().contains(&Action::JoinGame));

        app.update(Update::Persona(Persona {
            connect: Some("+connect 10.0.0.2:27015".into()),
            ..persona(PIXEL, "pixelpirate", Status::Online, Some(PORTAL_2))
        }));
        assert_eq!(app.actions()[0], Action::JoinGame);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        // In the background, not with the terminal handed over.
        assert!(app.pending.is_none());
        let launch = app.launch.take().unwrap();
        assert_eq!(
            launch.args,
            ["launch", "620", "--", "+connect", "10.0.0.2:27015"]
        );
        assert_eq!(launch.title, "Portal 2");

        // A game that isn't installed says so instead.
        app.update(Update::Persona(Persona {
            lobby: Some(109775241234567890),
            ..persona(MARA, "Mara", Status::Online, Some(TF2))
        }));
        press(&mut app, KeyCode::Left);
        select(&mut app, "Mara");
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        assert!(app.launch.is_none());
        assert_eq!(
            app.status.as_deref(),
            Some("Install Team Fortress 2 first (Library → Install) to join Mara.")
        );
    }

    #[test]
    fn games_play_in_the_background_and_hold_off_quitting() {
        let (mut app, _) = online_app();
        // Library → Portal 2 → Play.
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.game().unwrap().appid, PORTAL_2);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        assert!(app.pending.is_none());
        assert_eq!(app.launch.take().unwrap().args, ["launch", "620"]);

        app.running = Some(game::fake(
            PORTAL_2,
            "Portal 2",
            &["\u{1e}Portal 2 is running.", game::CONFLICT_PROMPT],
        ));
        // Its files are in use, and it's already playing.
        assert_eq!(
            app.actions(),
            [Action::Dlc, Action::StorePage, Action::Favorite]
        );
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        assert!(app.status.as_deref().unwrap().contains("still running"));

        // Its cloud question comes up, and a key answers it.
        app.running.as_mut().unwrap().poll();
        app.conflict = true;
        press(&mut app, KeyCode::Char('j'));
        assert!(app.conflict);
        press(&mut app, KeyCode::Char('c'));
        assert!(!app.conflict);
    }

    #[test]
    fn friends_can_be_invited_while_playing_and_invites_accepted() {
        let (mut app, mut sent) = online_app();
        select(&mut app, "Mara");
        assert!(!app.actions().contains(&Action::InviteToGame));
        app.running = Some(game::fake(PORTAL_2, "Portal 2", &[]));
        assert!(app.actions().contains(&Action::InviteToGame));
        requests(&mut sent);
        let at = app
            .actions()
            .iter()
            .position(|&a| a == Action::InviteToGame)
            .unwrap();
        press(&mut app, KeyCode::Right);
        app.actions_list.select(Some(at));
        press(&mut app, KeyCode::Enter);
        assert_eq!(requests(&mut sent), [Request::Invite(MARA)]);
        assert_eq!(app.status.as_deref(), Some("Inviting Mara to Portal 2…"));
        app.running = None;

        // pixelpirate invites us into their lobby.
        app.update(Update::Message {
            with: PIXEL,
            message: Message {
                from_me: false,
                text: "invited you to play".into(),
                time: 1,
                invite: Some(GameInvite {
                    appid: None,
                    join: Join::Lobby(42),
                }),
            },
        });
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("Accept game invite")
        );
        press(&mut app, KeyCode::Left);
        select(&mut app, "pixelpirate");
        assert_eq!(app.actions()[0], Action::AcceptGameInvite);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        // The game they're playing, since the invite didn't say.
        let launch = app.launch.take().unwrap();
        assert_eq!(launch.args, ["launch", "620", "--", "+connect_lobby", "42"]);
        assert!(!app.actions().contains(&Action::AcceptGameInvite));
    }

    #[test]
    fn messages_are_sent_and_counted_until_read() {
        let (mut app, mut sent) = online_app();
        select(&mut app, "Mara");
        // Showing a conversation loads it, once.
        app.sync();
        assert_eq!(requests(&mut sent), [Request::History(MARA)]);
        app.update(Update::History {
            with: MARA,
            messages: vec![],
        });

        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.editing, Some(Field::Message));
        type_text(&mut app, "hey! q to quit?");
        press(&mut app, KeyCode::Enter);
        assert!(!app.quit && app.draft.is_empty());
        assert_eq!(
            requests(&mut sent),
            [Request::Send {
                to: MARA,
                text: "hey! q to quit?".into()
            }]
        );

        // A message from someone else is counted, and announced.
        let message = |text: &str| Message {
            from_me: false,
            text: text.into(),
            time: 1,
            invite: None,
        };
        app.update(Update::Message {
            with: NIGHTOWL,
            message: message("you there?"),
        });
        assert_eq!(app.chats[&NIGHTOWL].unread, 1);
        assert_eq!(app.status.as_deref(), Some("nightowl: you there?"));
        // One from whoever is on screen isn't.
        app.update(Update::Message {
            with: MARA,
            message: message("hi"),
        });
        assert_eq!(app.chats[&MARA].unread, 0);

        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Left);
        select(&mut app, "nightowl");
        assert_eq!(app.chats[&NIGHTOWL].unread, 0);
    }

    #[test]
    fn a_rejected_session_asks_to_sign_in_again() {
        let (mut app, _) = online_app();
        app.update(Update::Rejected("AccessDenied".into()));
        assert!(app.online.is_none());
        assert!(app.status.as_deref().unwrap().contains("Sign in again"));
        assert!(!app.status.as_deref().unwrap().contains("AccessDenied"));
        app.sections.select(Some(3));
        assert_eq!(app.actions()[0], Action::SignIn);
    }

    #[test]
    fn a_message_waits_while_offline() {
        let (mut app, mut sent) = online_app();
        select(&mut app, "Mara");
        requests(&mut sent);
        app.update(Update::Offline("timed out".into()));
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "later");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.draft, "later");
        assert!(requests(&mut sent).is_empty());
    }
}
