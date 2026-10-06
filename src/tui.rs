//! The terminal UI, opened by `fumes` without a command.
//!
//! Three columns: sections on the left (library, friends, the store,
//! profile), the selected section's items in the middle, and what can be
//! done with the selected item on the right. Arrow keys move within a
//! column; → or Enter goes a column deeper (or runs the action), ← or Esc
//! comes back.
//!
//! Anything long or interactive (installing, playing, signing in) runs as
//! the ordinary `fumes` command in a child process: the UI hands it the
//! terminal and comes back when it's done, so progress output, prompts and
//! Ctrl-C behave exactly as they do on the command line.

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
use crate::favorites::{self, Favorites};
use crate::library::{self, Game, Owned};
use crate::{download, session, steam};

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

#[derive(Clone, Copy, PartialEq, Eq)]
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
    FriendProfile,
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
            Action::FriendProfile => "View profile",
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
    Command { title: String, args: Vec<String> },
    /// A web page or `steam://` link.
    Open(String),
    /// Not there yet; the text says so.
    Unavailable(&'static str),
}

/// An action that asks first (deleting a game, signing out).
struct Confirm {
    question: String,
    job: Job,
}

/// Stand-ins until the friends list comes from Steam.
struct Friend {
    name: &'static str,
    presence: Presence,
}

enum Presence {
    Playing(&'static str),
    Online,
    Away,
    Offline,
}

impl Presence {
    fn describe(&self) -> String {
        match self {
            Presence::Playing(game) => format!("Playing {game}"),
            Presence::Online => "Online".into(),
            Presence::Away => "Away".into(),
            Presence::Offline => "Offline".into(),
        }
    }

    fn color(&self) -> Color {
        match self {
            Presence::Playing(_) => Color::Green,
            Presence::Online => Color::Blue,
            Presence::Away => Color::Yellow,
            Presence::Offline => DIM,
        }
    }
}

const FRIENDS: &[Friend] = &[
    Friend {
        name: "pixelpirate",
        presence: Presence::Playing("Portal 2"),
    },
    Friend {
        name: "turbo_tortoise",
        presence: Presence::Playing("Team Fortress 2"),
    },
    Friend {
        name: "Mara",
        presence: Presence::Online,
    },
    Friend {
        name: "kestrel",
        presence: Presence::Away,
    },
    Friend {
        name: "nightowl",
        presence: Presence::Offline,
    },
    Friend {
        name: "dusk",
        presence: Presence::Offline,
    },
];

pub fn run(dirs: Dirs) -> Result<()> {
    // While a command has the terminal, Ctrl-C is the command's to handle
    // and the UI waits for it. (Inside the UI, Ctrl-C is just a key.)
    tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });

    let mut app = App::new(dirs);
    app.refresh();
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
    favorites: Favorites,
    /// Indexes into `games` that match the search, in the order shown.
    shown: Vec<usize>,
    search: String,
    /// Keys go to the search bar.
    typing: bool,
    games_list: ListState,
    friends_list: ListState,
    actions_list: ListState,
    /// Rows the middle list showed last frame, for Page Up/Down.
    page: usize,
    /// Account name and SteamID64.
    account: Option<(String, u64)>,
    engine_fetched: bool,
    refreshing: bool,
    status: Option<String>,
    confirm: Option<Confirm>,
    /// A command waiting for the terminal: title and arguments.
    pending: Option<(String, Vec<String>)>,
    quit: bool,
    owned_tx: mpsc::Sender<Result<Vec<Owned>>>,
    owned_rx: mpsc::Receiver<Result<Vec<Owned>>>,
}

impl App {
    fn new(dirs: Dirs) -> App {
        let (owned_tx, owned_rx) = mpsc::channel();
        let mut app = App {
            favorites: favorites::load(&dirs),
            dirs,
            focus: Focus::Sections,
            sections: ListState::default().with_selected(Some(0)),
            games: Vec::new(),
            shown: Vec::new(),
            search: String::new(),
            typing: false,
            games_list: ListState::default(),
            friends_list: ListState::default().with_selected(Some(0)),
            actions_list: ListState::default().with_selected(Some(0)),
            page: 10,
            account: None,
            engine_fetched: false,
            refreshing: false,
            status: None,
            confirm: None,
            pending: None,
            quit: false,
            owned_tx,
            owned_rx,
        };
        app.reload();
        app
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(owned) = self.owned_rx.try_recv() {
                self.refreshed(owned);
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.key(key);
            }
            if let Some((title, args)) = self.pending.take() {
                hand_over(terminal, &title, &args)?;
                let was_signed_in = self.account.is_some();
                self.reload();
                if !was_signed_in && self.account.is_some() {
                    self.refresh();
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

    /// Ask Steam for the owned games, in the background.
    fn refresh(&mut self) {
        if self.refreshing {
            return;
        }
        if self.account.is_none() {
            self.status = Some("Not signed in: Profile → Sign in to see your games.".into());
            return;
        }
        self.refreshing = true;
        let dirs = self.dirs.clone();
        let tx = self.owned_tx.clone();
        tokio::spawn(async move {
            tx.send(crate::fetch(&dirs).await).ok();
        });
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
                let rank = rank(&query, &self.search, game)?;
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

    fn section(&self) -> Section {
        Section::ALL[self.sections.selected().unwrap_or(0)]
    }

    fn game(&self) -> Option<&Game> {
        let i = *self.shown.get(self.games_list.selected()?)?;
        self.games.get(i)
    }

    fn friend(&self) -> Option<&'static Friend> {
        FRIENDS.get(self.friends_list.selected()?)
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
                match &game.installed {
                    Some(i) if i.by_fumes => vec![
                        Play, CloudSync, Update, Verify, Dlc, StorePage, favorite, Uninstall,
                    ],
                    Some(_) => vec![PlayInSteam, Dlc, StorePage, favorite, UninstallInSteam],
                    None => vec![Install, Dlc, StorePage, favorite],
                }
            }
            Section::Friends => vec![Message, FriendProfile],
            Section::Store => Vec::new(),
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
            self.quit = true;
            return;
        }
        if let Some(confirm) = self.confirm.take() {
            // Anything but yes cancels.
            if matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter) {
                self.start(confirm.job);
            }
            return;
        }
        if self.typing {
            let keep = self.game().map(|g| g.appid);
            match key.code {
                KeyCode::Char(c) => self.search.push(c),
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Esc => {
                    self.search.clear();
                    self.typing = false;
                }
                KeyCode::Enter => {
                    self.typing = false;
                    return;
                }
                code => return self.navigate(code),
            }
            self.apply_search(keep);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('/') => {
                self.sections.select(Some(0));
                self.focus_on(Focus::Items);
                self.typing = true;
            }
            KeyCode::Char('f')
                if self.section() == Section::Library && self.focus != Focus::Sections =>
            {
                self.toggle_favorite()
            }
            KeyCode::Esc if self.focus == Focus::Items && !self.search.is_empty() => {
                let keep = self.game().map(|g| g.appid);
                self.search.clear();
                self.apply_search(keep);
            }
            code => self.navigate(code),
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
            (Focus::Items, Section::Friends) => (&mut self.friends_list, FRIENDS.len()),
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
        if focus != Focus::Items {
            self.typing = false;
        }
        self.focus = focus;
    }

    fn toggle_favorite(&mut self) {
        let Some(game) = self.game() else {
            return;
        };
        let (appid, name) = (game.appid, display_name(game));
        let added = self.favorites.insert(appid);
        if !added {
            self.favorites.remove(&appid);
        }
        self.status = Some(match favorites::save(&self.dirs, &self.favorites) {
            Ok(()) if added => format!("★ {name} is a favorite."),
            Ok(()) => format!("{name} is no longer a favorite."),
            Err(e) => format!("Couldn't save favorites: {e:#}"),
        });
        // Favorites move to the top; the selection follows the game.
        self.apply_search(Some(appid));
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
        let fumes = |title: String, args: &[&str]| Job::Command {
            title,
            args: args.iter().map(|a| a.to_string()).collect(),
        };
        let job = match action {
            Action::Play => fumes(format!("Playing {name}"), &["launch", &id]),
            Action::PlayInSteam => Job::Open(format!("steam://rungameid/{appid}")),
            Action::Install => fumes(format!("Installing {name}"), &["install", &id]),
            Action::Update => fumes(format!("Updating {name}"), &["install", &id]),
            Action::Verify => fumes(format!("Verifying {name}"), &["install", &id, "--verify"]),
            Action::CloudSync => fumes(
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
            Action::Message | Action::FriendProfile => {
                Job::Unavailable("Friends aren't connected to Steam yet; these are placeholders.")
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
            Action::EngineStatus => fumes("Steam engine status".into(), &["engine", "status"]),
            Action::EngineSetup => {
                fumes("Setting up the Steam engine".into(), &["engine", "setup"])
            }
        };
        self.start(job);
    }

    fn start(&mut self, job: Job) {
        match job {
            Job::Command { title, args } => self.pending = Some((title, args)),
            Job::Open(url) => {
                self.status = Some(match crate::open_url(&url) {
                    Ok(()) if url.starts_with("steam://") => "Sent to Steam.".into(),
                    Ok(()) => format!("Opened {url} in the browser."),
                    Err(e) => format!("{e:#}"),
                })
            }
            Job::Unavailable(why) => self.status = Some(why.into()),
        }
    }

    // Drawing

    fn draw(&mut self, frame: &mut Frame) {
        let [main, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        let [left, middle, right] = Layout::horizontal([
            Constraint::Length(16),
            Constraint::Fill(1),
            Constraint::Length(36),
        ])
        .areas(main);

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
        let items: Vec<ListItem> = Section::ALL
            .iter()
            .map(|s| ListItem::new(format!(" {}", s.label())))
            .collect();
        let list = List::new(items)
            .block(self.column(" fumes ".into(), Focus::Sections))
            .highlight_style(self.highlight(Focus::Sections));
        frame.render_stateful_widget(list, area, &mut self.sections);
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
        let [search_area, _, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        self.page = list_area.height.max(1) as usize;

        let icon = Span::styled(
            " / ",
            Style::new().fg(if self.typing { ACCENT } else { DIM }),
        );
        let bar = if self.search.is_empty() && !self.typing {
            Line::from(vec![icon, Span::styled("Search", Style::new().fg(DIM))])
        } else {
            Line::from(vec![icon, Span::raw(self.search.clone())])
        };
        frame.render_widget(Paragraph::new(bar), search_area);
        if self.typing {
            let typed = self.search.chars().count() as u16;
            frame.set_cursor_position((
                (search_area.x + 3 + typed).min(search_area.right().saturating_sub(1)),
                search_area.y,
            ));
        }

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

        if self.shown.len() > self.page {
            let mut bar = ScrollbarState::new(self.shown.len())
                .position(self.games_list.selected().unwrap_or(0));
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            // On the column's right border, beside the list.
            let track = Rect {
                x: area.x,
                width: area.width,
                ..list_area
            };
            frame.render_stateful_widget(scrollbar, track, &mut bar);
        }
    }

    fn draw_friends(&mut self, frame: &mut Frame, area: Rect) {
        let online = FRIENDS
            .iter()
            .filter(|f| !matches!(f.presence, Presence::Offline))
            .count();
        let items: Vec<ListItem> = FRIENDS
            .iter()
            .map(|f| {
                ListItem::new(Line::from(vec![
                    Span::styled(" ● ", Style::new().fg(f.presence.color())),
                    Span::raw(f.name),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(self.column(
                format!(" Friends ({online} online) · sample data "),
                Focus::Items,
            ))
            .highlight_style(self.highlight(Focus::Items));
        frame.render_stateful_widget(list, area, &mut self.friends_list);
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
    /// more about it.
    fn draw_details(&mut self, frame: &mut Frame, area: Rect) {
        let block = self
            .column(" Actions ".into(), Focus::Actions)
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let (heading, details) = match self.section() {
            Section::Library => match self.game() {
                Some(game) if self.favorites.contains(&game.appid) => {
                    (format!("★ {}", display_name(game)), game_details(game))
                }
                Some(game) => (display_name(game), game_details(game)),
                None => return,
            },
            Section::Friends => match self.friend() {
                Some(friend) => (
                    friend.name.to_owned(),
                    vec![
                        Line::styled(
                            friend.presence.describe(),
                            Style::new().fg(friend.presence.color()),
                        ),
                        Line::raw(""),
                        Line::styled(
                            "Sample friend: the friends list isn't connected to Steam yet.",
                            Style::new().fg(DIM),
                        ),
                    ],
                ),
                None => return,
            },
            Section::Store => ("Steam Store".to_owned(), Vec::new()),
            Section::Profile => match &self.account {
                Some((account, _)) => (account.clone(), Vec::new()),
                None => ("Not signed in".to_owned(), Vec::new()),
            },
        };
        let actions = self.actions();

        let width = inner.width.max(1);
        let heading_rows = (heading.chars().count() as u16).div_ceil(width).max(1);
        let [heading_area, _, actions_area, _, details_area] = Layout::vertical([
            Constraint::Length(heading_rows),
            Constraint::Length(1),
            Constraint::Length(actions.len() as u16),
            Constraint::Length(if actions.is_empty() { 0 } else { 1 }),
            Constraint::Fill(1),
        ])
        .areas(inner);

        let heading = Paragraph::new(heading)
            .style(Style::new().add_modifier(Modifier::BOLD))
            .wrap(Wrap { trim: true });
        frame.render_widget(heading, heading_area);

        let items: Vec<ListItem> = actions.iter().map(|a| ListItem::new(a.label())).collect();
        let list = List::new(items).highlight_style(self.highlight(Focus::Actions));
        if self.focus == Focus::Actions {
            frame.render_stateful_widget(list, actions_area, &mut self.actions_list);
        } else {
            frame.render_widget(list, actions_area);
        }

        let details = Paragraph::new(details).wrap(Wrap { trim: false });
        frame.render_widget(details, details_area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.status {
            Some(status) => Line::styled(format!(" {status}"), Style::new().fg(Color::Yellow)),
            None => {
                let hints = if self.typing {
                    "type to search · ↑↓ move · Enter done · Esc clear"
                } else {
                    match (self.focus, self.section()) {
                        (Focus::Sections, _) => "↑↓ move · → open · q quit",
                        (Focus::Items, Section::Library) => {
                            "↑↓ PgUp PgDn scroll · → actions · / search · f favorite · r refresh · ← back · q quit"
                        }
                        (Focus::Items, _) => "↑↓ move · → actions · ← back · q quit",
                        (Focus::Actions, Section::Library) => {
                            "↑↓ move · Enter run · f favorite · ← back · q quit"
                        }
                        (Focus::Actions, _) => "↑↓ move · Enter run · ← back · q quit",
                    }
                };
                Line::styled(format!(" {hints}"), Style::new().fg(DIM))
            }
        };
        frame.render_widget(Paragraph::new(line), area);
    }
}

fn display_name(game: &Game) -> String {
    if game.name.is_empty() {
        format!("App {}", game.appid)
    } else {
        game.name.clone()
    }
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

/// How well a game matches a search (lower is better), or `None` if it
/// doesn't. Every word searched for has to be in the name, or be part of
/// its initials ("gta"); a search for the app id finds that game.
fn rank(query: &[String], raw: &str, game: &Game) -> Option<u8> {
    if query.is_empty() || raw.trim() == game.appid.to_string() {
        return Some(0);
    }
    let name = words(&game.name);
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
/// the terminal back once it's done and the output has been read.
fn hand_over(terminal: &mut DefaultTerminal, title: &str, args: &[String]) -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, Show)?;
    println!("── {title} ──\n");
    let exe = std::env::current_exe().context("finding fumes' own executable")?;
    match Command::new(exe).args(args).status() {
        Ok(status) if status.success() => println!("\nDone."),
        Ok(status) => println!("\nStopped ({status})."),
        Err(e) => println!("\nCouldn't run it: {e}"),
    }
    print!("Press Enter to go back to fumes…");
    io::stdout().flush()?;
    io::stdin().read_line(&mut String::new())?;
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::Installed;
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
        for c in "alp".chars() {
            press(&mut app, KeyCode::Char(c));
        }
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
        let (_, args) = app.pending.unwrap();
        assert_eq!(args, ["uninstall", "2", "--yes"]);
    }

    fn ids(app: &App) -> Vec<u32> {
        app.shown.iter().map(|&i| app.games[i].appid).collect()
    }

    fn search(app: &mut App, text: &str) {
        press(app, KeyCode::Char('/'));
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
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
        assert_eq!(favorites::load(&app.dirs), Favorites::from([3]));

        // Unfavoriting from the actions column puts it back.
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::End);
        assert!(app.actions()[app.actions_list.selected().unwrap()] == Action::Unfavorite);
        press(&mut app, KeyCode::Enter);
        assert_eq!(ids(&app), [1, 2, 3]);
        assert!(favorites::load(&app.dirs).is_empty());
    }
}
