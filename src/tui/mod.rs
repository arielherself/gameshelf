//! The interactive TUI.

mod input;
mod ui;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::library::{self, Library, LookupOutcome};
use crate::lockfile::{Game, LockFile};
use crate::runtime::{self, Available, Installed, Progress, Source};
use crate::session::{self, Session};
use crate::steam::{SearchItem, SteamClient};
use crate::{launch, names};
use input::TextInput;

pub fn run(lib: Library, offline: bool) -> Result<()> {
    let mut app = App::new(lib, offline)?;
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    if let Some(d) = &app.runtimes.download {
        d.cancel.store(true, Ordering::Relaxed);
    }
    result
}

/// Messages from background threads.
enum Msg {
    Lookup { dir: String, result: Result<LookupOutcome, String> },
    LookupsDone,
    Search { dir: String, result: Result<Vec<SearchItem>, String> },
    AppIdSet { dir: String, appid: Option<u32>, result: Result<Option<String>, String> },
    Releases { source: Source, result: Result<Vec<Available>, String> },
    Download(Progress),
    DownloadDone(Result<String, String>),
    Stopped { dir: String, result: Result<(), String> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Name,
    Recent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Games,
    Runtimes,
}

pub struct SelectItem {
    pub label: String,
    pub detail: String,
    pub value: String,
    pub current: bool,
}

#[derive(Clone)]
pub enum InputKind {
    Filter,
    AppId(String),
    SearchQuery(String),
    Args(String),
    PrefixName(String),
    ExePath(String),
}

#[derive(Clone)]
pub enum SelectKind {
    Exe(String),
    Prefix(String),
    Runtime(String),
    Match(String),
}

pub enum ConfirmKind {
    RemoveRuntime(String),
    StopGame(String),
}

pub enum Modal {
    Input { kind: InputKind, title: String, hint: String, input: TextInput },
    Select { kind: SelectKind, title: String, items: Vec<SelectItem>, state: ListState },
    Confirm { kind: ConfirmKind, message: String },
    Help,
}

pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

pub struct Download {
    pub name: String,
    pub progress: Progress,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Installed,
    Available,
}

/// One entry in the runtime screen's "available" pane.
pub enum AvailEntry {
    Release(Available),
    Steam { name: String, path: std::path::PathBuf },
}

pub struct RuntimeScreen {
    pub focus: Pane,
    pub installed: Vec<Installed>,
    pub installed_state: ListState,
    /// Index into [`RuntimeScreen::TABS`].
    pub tab: usize,
    pub releases: HashMap<&'static str, Load<Vec<Available>>>,
    pub available_state: ListState,
    pub download: Option<Download>,
}

impl RuntimeScreen {
    /// Download sources plus Proton builds already present in Steam.
    pub const TABS: [&'static str; 4] = ["GE-Proton", "UMU-Proton", "Proton-CachyOS", "From Steam"];

    fn source(&self) -> Option<Source> {
        Source::ALL.get(self.tab).copied()
    }

    pub fn entries(&self) -> Vec<AvailEntry> {
        match self.source() {
            Some(src) => match self.releases.get(src.key()) {
                Some(Load::Ready(list)) => list.iter().cloned().map(AvailEntry::Release).collect(),
                _ => Vec::new(),
            },
            None => runtime::steam_compat_tools()
                .into_iter()
                .map(|(name, path)| AvailEntry::Steam { name, path })
                .collect(),
        }
    }
}

pub struct App {
    pub lib: Library,
    pub lock: LockFile,
    pub offline: bool,
    /// Visible game directories, filtered and sorted.
    pub rows: Vec<String>,
    pub list_state: ListState,
    pub filter: String,
    pub sort: Sort,
    pub screen: Screen,
    pub modal: Option<Modal>,
    pub status: Option<(String, Level, Instant)>,
    /// Running games (from any gameshelf process), refreshed periodically.
    pub running: HashMap<String, Session>,
    /// Launched from here but the supervisor hasn't registered yet.
    pub launching: HashMap<String, Instant>,
    pub lookup_running: bool,
    pub runtimes: RuntimeScreen,
    pub tick: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
}

impl App {
    fn new(lib: Library, offline: bool) -> Result<Self> {
        let lock = lib.load()?;
        let (tx, rx) = channel();
        let mut app = Self {
            runtimes: RuntimeScreen {
                focus: Pane::Installed,
                installed: runtime::list_installed(&lib),
                installed_state: ListState::default(),
                tab: 0,
                releases: HashMap::new(),
                available_state: ListState::default(),
                download: None,
            },
            lib,
            lock,
            offline,
            rows: Vec::new(),
            list_state: ListState::default(),
            filter: String::new(),
            sort: Sort::Name,
            screen: Screen::Games,
            modal: None,
            status: None,
            running: HashMap::new(),
            launching: HashMap::new(),
            lookup_running: false,
            tick: 0,
            tx,
            rx,
            quit: false,
        };
        app.refresh_rows(None);
        app.refresh_sessions();
        app.start_lookups();
        Ok(app)
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| ui::draw(f, self))?;
            if event::poll(Duration::from_millis(120))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
                    _ => {}
                }
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.on_msg(msg);
            }
            self.tick = self.tick.wrapping_add(1);
            if self.tick.is_multiple_of(8) {
                self.refresh_sessions();
            }
        }
        Ok(())
    }

    // -- state helpers ------------------------------------------------------

    pub fn is_running(&self, dir: &str) -> bool {
        self.running.contains_key(dir) || self.launching.contains_key(dir)
    }

    /// Pick up games started or stopped by any gameshelf process.
    fn refresh_sessions(&mut self) {
        let now_running = session::active(&self.lib);
        let exited: Vec<Session> =
            self.running.values().filter(|s| !now_running.contains_key(&s.dir)).cloned().collect();
        let mut failed = Vec::new();
        self.launching.retain(|dir, at| {
            if now_running.contains_key(dir) {
                return false;
            }
            // The supervisor registers within milliseconds; if it didn't, it
            // exited early (the reason is in the log).
            let pending = at.elapsed() < Duration::from_secs(5);
            if !pending {
                failed.push(dir.clone());
            }
            pending
        });
        self.running = now_running;
        if !exited.is_empty() {
            self.reload();
        }
        for s in exited {
            let name = self.game_name(&s.dir);
            let secs = crate::util::now().saturating_sub(s.started);
            self.set_status(
                format!("{name} exited after {}", crate::util::format_duration(secs)),
                Level::Info,
            );
        }
        for dir in failed {
            let log = crate::util::tilde(&launch::log_path(&dir));
            self.set_status(format!("{} failed to start — see {log}", self.game_name(&dir)), Level::Error);
        }
    }

    fn game_name(&self, dir: &str) -> String {
        self.lock.games.get(dir).map(|g| g.display_name(dir).to_string()).unwrap_or_else(|| dir.to_string())
    }

    pub fn selected(&self) -> Option<&str> {
        self.list_state.selected().and_then(|i| self.rows.get(i)).map(String::as_str)
    }

    pub fn selected_game(&self) -> Option<(&str, &Game)> {
        let dir = self.selected()?;
        Some((dir, self.lock.games.get(dir)?))
    }

    fn set_status(&mut self, msg: impl Into<String>, level: Level) {
        self.status = Some((msg.into(), level, Instant::now()));
    }

    fn report<T>(&mut self, r: Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.set_status(format!("{e:#}"), Level::Error);
                None
            }
        }
    }

    fn reload(&mut self) {
        match self.lib.load() {
            Ok(lock) => {
                self.lock = lock;
                let keep = self.selected().map(str::to_string);
                self.refresh_rows(keep.as_deref());
            }
            Err(e) => self.set_status(format!("reloading lock file: {e:#}"), Level::Error),
        }
    }

    fn refresh_rows(&mut self, keep: Option<&str>) {
        let key = names::compare_key(&self.filter);
        let mut rows: Vec<String> = self
            .lock
            .games
            .iter()
            .filter(|(dir, g)| {
                key.is_empty()
                    || names::compare_key(dir).contains(&key)
                    || names::compare_key(g.display_name(dir)).contains(&key)
            })
            .map(|(d, _)| d.clone())
            .collect();
        let lock = &self.lock;
        let name = |d: &String| lock.games[d].display_name(d).to_lowercase();
        match self.sort {
            Sort::Name => rows.sort_by_key(name),
            Sort::Recent => rows.sort_by(|a, b| {
                let la = lock.games[a].last_played.unwrap_or(0);
                let lb = lock.games[b].last_played.unwrap_or(0);
                lb.cmp(&la).then_with(|| name(a).cmp(&name(b)))
            }),
        }
        let prev = self.list_state.selected().unwrap_or(0);
        self.rows = rows;
        let idx = keep
            .and_then(|k| self.rows.iter().position(|r| r == k))
            .unwrap_or(prev.min(self.rows.len().saturating_sub(1)));
        self.list_state.select((!self.rows.is_empty()).then_some(idx));
    }

    fn start_lookups(&mut self) {
        if self.offline || self.lookup_running {
            return;
        }
        let pending = library::pending_lookups(&self.lock);
        if pending.is_empty() {
            return;
        }
        self.lookup_running = true;
        let lib = self.lib.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let client = SteamClient::new();
            for dir in pending {
                let result = library::lookup_game(&lib, &client, &dir).map_err(|e| format!("{e:#}"));
                let failed = result.is_err();
                if tx.send(Msg::Lookup { dir, result }).is_err() || failed {
                    break;
                }
            }
            let _ = tx.send(Msg::LookupsDone);
        });
    }

    fn rescan(&mut self) {
        if let Some((lock, report)) = self.report(self.lib.scan()) {
            self.lock = lock;
            let keep = self.selected().map(str::to_string);
            self.refresh_rows(keep.as_deref());
            let msg = match (report.added.len(), report.missing.len()) {
                (0, 0) => "No new games".to_string(),
                (a, 0) => format!("Found {a} new game(s): {}", report.added.join(", ")),
                (a, m) => format!("Found {a} new game(s); {m} directory(ies) missing"),
            };
            self.set_status(msg, Level::Info);
            self.start_lookups();
        }
    }

    fn update_game(&mut self, dir: &str, f: impl FnOnce(&mut Game)) {
        let r = self.lib.update(|lock| {
            if let Some(g) = lock.games.get_mut(dir) {
                f(g);
            }
            Ok(())
        });
        if let Some((lock, ())) = self.report(r) {
            self.lock = lock;
            self.refresh_rows(Some(dir));
        }
    }

    fn need_online(&mut self) -> bool {
        if self.offline {
            self.set_status("Offline mode: Steam and GitHub are not contacted", Level::Warn);
        }
        !self.offline
    }

    // -- messages -----------------------------------------------------------

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Lookup { dir, result } => {
                self.reload();
                match result {
                    Ok(LookupOutcome::Matched { name, appid, .. }) => {
                        self.set_status(format!("{dir}: matched {name} ({appid})"), Level::Ok)
                    }
                    Ok(LookupOutcome::Unmatched) => self.set_status(
                        format!("{dir}: no confident Steam match — press m to search or a to set an app id"),
                        Level::Warn,
                    ),
                    Ok(_) => {}
                    Err(e) => self.set_status(
                        format!("Steam lookup failed (will retry next start): {e}"),
                        Level::Error,
                    ),
                }
            }
            Msg::LookupsDone => self.lookup_running = false,
            Msg::Search { dir, result } => match result {
                Ok(items) if items.is_empty() => self.set_status("No Steam results", Level::Warn),
                Ok(items) => {
                    let current = self.lock.games.get(&dir).and_then(|g| g.appid);
                    let items = items
                        .into_iter()
                        .map(|i| SelectItem {
                            detail: format!(
                                "app {} · score {:.2}",
                                i.appid,
                                names::match_score(&dir, &i.name)
                            ),
                            current: current == Some(i.appid),
                            label: i.name,
                            value: i.appid.to_string(),
                        })
                        .collect();
                    self.open_select(
                        SelectKind::Match(dir.clone()),
                        format!("Steam results for {dir}"),
                        items,
                    );
                    self.set_status("", Level::Info);
                }
                Err(e) => self.set_status(format!("Steam search failed: {e}"), Level::Error),
            },
            Msg::AppIdSet { dir, appid, result } => {
                self.reload();
                match (result, appid) {
                    (Ok(Some(name)), Some(id)) => {
                        self.set_status(format!("{dir}: app {id} is {name}"), Level::Ok)
                    }
                    (Ok(None), Some(id)) => self.set_status(
                        format!("{dir}: app id {id} saved, but Steam has no store page for it"),
                        Level::Warn,
                    ),
                    (Ok(_), None) => self.set_status(format!("{dir}: app id cleared"), Level::Info),
                    (Err(e), _) => self.set_status(format!("Setting app id failed: {e}"), Level::Error),
                }
            }
            Msg::Releases { source, result } => {
                let load = match result {
                    Ok(list) => Load::Ready(list),
                    Err(e) => Load::Failed(e),
                };
                self.runtimes.releases.insert(source.key(), load);
                self.clamp_available();
            }
            Msg::Download(p) => {
                if let Some(d) = &mut self.runtimes.download {
                    d.progress = p;
                }
            }
            Msg::DownloadDone(result) => {
                let name = self.runtimes.download.take().map(|d| d.name).unwrap_or_default();
                match result {
                    Ok(installed) => self.set_status(format!("Installed {installed}"), Level::Ok),
                    Err(e) if e.contains("cancelled") => {
                        self.set_status(format!("Cancelled {name}"), Level::Warn)
                    }
                    Err(e) => self.set_status(format!("Installing {name} failed: {e}"), Level::Error),
                }
                self.refresh_installed();
            }
            Msg::Stopped { dir, result } => {
                if let Err(e) = result {
                    self.set_status(format!("Stopping {} failed: {e}", self.game_name(&dir)), Level::Error);
                }
                self.refresh_sessions();
            }
        }
    }

    // -- keys ---------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        match self.screen {
            Screen::Games => self.on_games_key(key),
            Screen::Runtimes => self.on_runtimes_key(key),
        }
    }

    fn on_games_key(&mut self, key: KeyEvent) {
        let len = self.rows.len();
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                let keep = self.selected().map(str::to_string);
                self.refresh_rows(keep.as_deref());
            }
            KeyCode::Down | KeyCode::Char('j') => move_sel(&mut self.list_state, len, 1),
            KeyCode::Up | KeyCode::Char('k') => move_sel(&mut self.list_state, len, -1),
            KeyCode::PageDown => move_sel(&mut self.list_state, len, 10),
            KeyCode::PageUp => move_sel(&mut self.list_state, len, -10),
            KeyCode::Home | KeyCode::Char('g') => self.list_state.select((len > 0).then_some(0)),
            KeyCode::End | KeyCode::Char('G') => self.list_state.select(len.checked_sub(1)),
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('/') => {
                self.modal = Some(Modal::Input {
                    kind: InputKind::Filter,
                    title: "Filter".into(),
                    hint: "Type to filter · Enter keep · Esc clear".into(),
                    input: TextInput::new(self.filter.clone()),
                })
            }
            KeyCode::Char('o') => {
                self.sort = if self.sort == Sort::Name { Sort::Recent } else { Sort::Name };
                let keep = self.selected().map(str::to_string);
                self.refresh_rows(keep.as_deref());
            }
            KeyCode::Char('s') => self.rescan(),
            KeyCode::Char('R') => self.open_runtimes(),
            KeyCode::Enter => self.launch_selected(),
            KeyCode::Char('x') => self.stop_selected(),
            _ => self.on_game_action(key),
        }
    }

    /// Keys that act on the selected game.
    fn on_game_action(&mut self, key: KeyEvent) {
        let Some((dir, game)) = self.selected_game() else { return };
        let dir = dir.to_string();
        let game = game.clone();
        match key.code {
            KeyCode::Char('a') => {
                self.modal = Some(Modal::Input {
                    kind: InputKind::AppId(dir.clone()),
                    title: format!("Steam app id for {dir}"),
                    hint: "Number · empty = none · \"auto\" = search again".into(),
                    input: TextInput::new(game.appid.map(|a| a.to_string()).unwrap_or_default()),
                })
            }
            KeyCode::Char('m') => {
                self.modal = Some(Modal::Input {
                    kind: InputKind::SearchQuery(dir.clone()),
                    title: format!("Search Steam for {dir}"),
                    hint: "Enter to search".into(),
                    input: TextInput::new(
                        game.steam
                            .as_ref()
                            .map(|s| s.name.clone())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| names::search_query(&dir)),
                    ),
                })
            }
            KeyCode::Char('e') => {
                let mut items: Vec<SelectItem> = library::find_executables(&self.lib.game_dir(&dir), &dir)
                    .into_iter()
                    .map(|rel| SelectItem {
                        current: game.executable.as_deref() == Some(rel.as_str()),
                        detail: String::new(),
                        label: rel.clone(),
                        value: rel,
                    })
                    .collect();
                if let Some(cur) = &game.executable
                    && !items.iter().any(|i| &i.value == cur)
                {
                    items.insert(
                        0,
                        SelectItem {
                            label: cur.clone(),
                            detail: "current".into(),
                            value: cur.clone(),
                            current: true,
                        },
                    );
                }
                items.push(SelectItem {
                    label: "Enter a path…".into(),
                    detail: String::new(),
                    value: String::new(),
                    current: false,
                });
                self.open_select(SelectKind::Exe(dir.clone()), format!("Executable for {dir}"), items);
            }
            KeyCode::Char('p') => {
                let fresh = library::fresh_prefix(&dir, &self.lock, &self.lib.prefixes_dir());
                let mut items = vec![SelectItem {
                    label: format!("New prefix: {fresh}"),
                    detail: "created on first launch".into(),
                    value: fresh,
                    current: false,
                }];
                let mut names_seen = Vec::new();
                for p in self.lib.prefixes().into_iter().chain(std::iter::once(game.prefix.clone())) {
                    if names_seen.contains(&p) {
                        continue;
                    }
                    let users: Vec<&str> = self
                        .lock
                        .games
                        .iter()
                        .filter(|(d, g)| g.prefix == p && **d != dir)
                        .map(|(d, _)| d.as_str())
                        .collect();
                    items.push(SelectItem {
                        current: p == game.prefix,
                        detail: if users.is_empty() {
                            String::new()
                        } else {
                            format!("used by {}", users.join(", "))
                        },
                        label: p.clone(),
                        value: p.clone(),
                    });
                    names_seen.push(p);
                }
                items.push(SelectItem {
                    label: "Enter a name or path…".into(),
                    detail: String::new(),
                    value: String::new(),
                    current: false,
                });
                let sel = items.iter().position(|i| i.current).unwrap_or(0);
                self.open_select(SelectKind::Prefix(dir.clone()), format!("Wine prefix for {dir}"), items);
                if let Some(Modal::Select { state, .. }) = &mut self.modal {
                    state.select(Some(sel));
                }
            }
            KeyCode::Char('r') => {
                self.refresh_installed();
                let default_label = match runtime::resolve(&self.lib, &self.lock, &Game::default()) {
                    Ok((name, _)) => format!("Library default ({name})"),
                    Err(_) => "Library default (none installed)".into(),
                };
                let mut items = vec![SelectItem {
                    label: default_label,
                    detail: String::new(),
                    value: String::new(),
                    current: game.runtime.is_none(),
                }];
                for rt in &self.runtimes.installed {
                    items.push(SelectItem {
                        current: game.runtime.as_deref() == Some(rt.name.as_str()),
                        detail: if rt.valid { String::new() } else { "broken".into() },
                        label: rt.name.clone(),
                        value: rt.name.clone(),
                    });
                }
                if let Some(cur) = game.runtime.as_ref().filter(|c| !items.iter().any(|i| &i.value == *c)) {
                    items.push(SelectItem {
                        label: cur.clone(),
                        detail: "not installed".into(),
                        value: cur.clone(),
                        current: true,
                    });
                }
                if items.len() == 1 {
                    self.set_status("No runtimes installed — press R to download one", Level::Warn);
                }
                let sel = items.iter().position(|i| i.current).unwrap_or(0);
                self.open_select(
                    SelectKind::Runtime(dir.clone()),
                    format!("Proton runtime for {dir}"),
                    items,
                );
                if let Some(Modal::Select { state, .. }) = &mut self.modal {
                    state.select(Some(sel));
                }
            }
            KeyCode::Char('A') => {
                self.modal = Some(Modal::Input {
                    kind: InputKind::Args(dir.clone()),
                    title: format!("Launch arguments for {dir}"),
                    hint: "Shell-quoted, e.g. -windowed \"-name=Some Value\"".into(),
                    input: TextInput::new(
                        shlex::try_join(game.args.iter().map(String::as_str)).unwrap_or_default(),
                    ),
                })
            }
            _ => {}
        }
    }

    fn open_select(&mut self, kind: SelectKind, title: String, items: Vec<SelectItem>) {
        let mut state = ListState::default();
        state.select(Some(items.iter().position(|i| i.current).unwrap_or(0)));
        self.modal = Some(Modal::Select { kind, title, items, state });
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        match modal {
            Modal::Help => {}
            Modal::Confirm { kind, message } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.confirm(kind),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.modal = Some(Modal::Confirm { kind, message }),
            },
            Modal::Input { kind, title, hint, mut input } => match key.code {
                KeyCode::Esc => {
                    if matches!(kind, InputKind::Filter) {
                        self.filter.clear();
                        self.refresh_rows(None);
                    }
                }
                KeyCode::Enter => self.submit_input(kind, input.text().trim().to_string()),
                _ => {
                    if input.handle(key) && matches!(kind, InputKind::Filter) {
                        self.filter = input.text().to_string();
                        self.refresh_rows(None);
                    }
                    self.modal = Some(Modal::Input { kind, title, hint, input });
                }
            },
            Modal::Select { kind, title, items, mut state } => {
                let len = items.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Enter => {
                        if let Some(item) = state.selected().and_then(|i| items.get(i)) {
                            let value = item.value.clone();
                            self.submit_select(kind, value);
                        }
                    }
                    code => {
                        match code {
                            KeyCode::Down | KeyCode::Char('j') => move_sel(&mut state, len, 1),
                            KeyCode::Up | KeyCode::Char('k') => move_sel(&mut state, len, -1),
                            KeyCode::PageDown => move_sel(&mut state, len, 10),
                            KeyCode::PageUp => move_sel(&mut state, len, -10),
                            KeyCode::Home | KeyCode::Char('g') => state.select(Some(0)),
                            KeyCode::End | KeyCode::Char('G') => state.select(len.checked_sub(1)),
                            _ => {}
                        }
                        self.modal = Some(Modal::Select { kind, title, items, state });
                    }
                }
            }
        }
    }

    fn submit_input(&mut self, kind: InputKind, text: String) {
        match kind {
            InputKind::Filter => {}
            InputKind::AppId(dir) => match text.as_str() {
                "auto" => {
                    if let Some(()) = self.report(library::reset_match(&self.lib, &dir)) {
                        self.reload();
                        if self.need_online() {
                            self.set_status(format!("{dir}: searching Steam…"), Level::Info);
                            self.start_lookups();
                        }
                    }
                }
                "" | "none" => self.set_appid(dir, None),
                id => match id.parse::<u32>() {
                    Ok(id) => self.set_appid(dir, Some(id)),
                    Err(_) => self.set_status(format!("{id:?} is not an app id"), Level::Error),
                },
            },
            InputKind::SearchQuery(dir) => {
                if text.is_empty() || !self.need_online() {
                    return;
                }
                self.set_status(format!("Searching Steam for {text:?}…"), Level::Info);
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let result = SteamClient::new().search(&text).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Search { dir, result });
                });
            }
            InputKind::Args(dir) => match shlex::split(&text) {
                Some(args) => {
                    self.update_game(&dir, |g| g.args = args);
                    self.set_status(format!("{dir}: arguments saved"), Level::Ok);
                }
                None => self.set_status("Unbalanced quotes in arguments", Level::Error),
            },
            InputKind::PrefixName(dir) => {
                if text.is_empty() {
                    return;
                }
                self.update_game(&dir, |g| g.prefix = text.clone());
                self.set_status(format!("{dir}: prefix set to {text}"), Level::Ok);
            }
            InputKind::ExePath(dir) => {
                if text.is_empty() {
                    return;
                }
                let path = self.lib.game_dir(&dir).join(crate::util::expand_tilde(&text));
                if !path.is_file() {
                    self.set_status(format!("{} does not exist", path.display()), Level::Error);
                    return;
                }
                self.update_game(&dir, |g| g.executable = Some(text.clone()));
                self.set_status(format!("{dir}: executable set to {text}"), Level::Ok);
            }
        }
    }

    fn set_appid(&mut self, dir: String, appid: Option<u32>) {
        if appid.is_some() && !self.offline {
            self.set_status(format!("{dir}: fetching Steam details…"), Level::Info);
        }
        let lib = self.lib.clone();
        let tx = self.tx.clone();
        let offline = self.offline;
        thread::spawn(move || {
            let client = (!offline).then(SteamClient::new);
            let result = library::set_appid(&lib, client.as_ref(), &dir, appid)
                .map(|info| info.map(|i| i.name))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::AppIdSet { dir, appid, result });
        });
    }

    fn submit_select(&mut self, kind: SelectKind, value: String) {
        match kind {
            SelectKind::Exe(dir) if value.is_empty() => {
                let current =
                    self.lock.games.get(&dir).and_then(|g| g.executable.clone()).unwrap_or_default();
                self.modal = Some(Modal::Input {
                    kind: InputKind::ExePath(dir.clone()),
                    title: format!("Executable for {dir}"),
                    hint: "Relative to the game directory, or absolute".into(),
                    input: TextInput::new(current),
                });
            }
            SelectKind::Exe(dir) => {
                self.update_game(&dir, |g| g.executable = Some(value.clone()));
                self.set_status(format!("{dir}: executable set to {value}"), Level::Ok);
            }
            SelectKind::Prefix(dir) if value.is_empty() => {
                let current = self.lock.games.get(&dir).map(|g| g.prefix.clone()).unwrap_or_default();
                self.modal = Some(Modal::Input {
                    kind: InputKind::PrefixName(dir.clone()),
                    title: format!("Wine prefix for {dir}"),
                    hint: "Name inside .proton-prefixes, or an absolute path".into(),
                    input: TextInput::new(current),
                });
            }
            SelectKind::Prefix(dir) => {
                self.update_game(&dir, |g| g.prefix = value.clone());
                self.set_status(format!("{dir}: prefix set to {value}"), Level::Ok);
            }
            SelectKind::Runtime(dir) => {
                let rt = (!value.is_empty()).then_some(value);
                let msg = format!("{dir}: runtime set to {}", rt.as_deref().unwrap_or("library default"));
                self.update_game(&dir, |g| g.runtime = rt);
                self.set_status(msg, Level::Ok);
            }
            SelectKind::Match(dir) => {
                if let Ok(id) = value.parse() {
                    self.set_appid(dir, Some(id));
                }
            }
        }
    }

    fn launch_selected(&mut self) {
        let Some(dir) = self.selected().map(str::to_string) else { return };
        if self.is_running(&dir) {
            self.set_status(
                format!("{} is already running (x to stop it)", self.game_name(&dir)),
                Level::Warn,
            );
            return;
        }
        let Some(plan) = self.report(launch::plan(&self.lib, &self.lock, &dir, &[])) else { return };
        let Some((_, log)) = self.report(session::spawn_detached(&self.lib, &dir, &[])) else { return };
        self.launching.insert(dir.clone(), Instant::now());
        let name = self.game_name(&dir);
        match plan.environment_warning() {
            Some(w) => self.set_status(format!("Launched {name}, but: {w}"), Level::Warn),
            None => self.set_status(
                format!("Launched {name} with {} · log: {}", plan.runtime, crate::util::tilde(&log)),
                Level::Ok,
            ),
        }
    }

    fn stop_selected(&mut self) {
        let Some(dir) = self.selected().map(str::to_string) else { return };
        if !self.running.contains_key(&dir) {
            self.set_status(format!("{} is not running", self.game_name(&dir)), Level::Warn);
            return;
        }
        let message = format!(
            "Stop {}?\nThis ends the game and its wine processes; unsaved progress is lost.",
            self.game_name(&dir)
        );
        self.modal = Some(Modal::Confirm { kind: ConfirmKind::StopGame(dir), message });
    }

    // -- runtime screen ----------------------------------------------------

    fn open_runtimes(&mut self) {
        self.screen = Screen::Runtimes;
        self.refresh_installed();
        self.load_tab();
    }

    fn refresh_installed(&mut self) {
        self.runtimes.installed = runtime::list_installed(&self.lib);
        let len = self.runtimes.installed.len();
        let sel = self.runtimes.installed_state.selected().unwrap_or(0).min(len.saturating_sub(1));
        self.runtimes.installed_state.select((len > 0).then_some(sel));
        if let Ok(lock) = self.lib.load() {
            self.lock = lock;
        }
    }

    fn load_tab(&mut self) {
        self.runtimes.available_state.select(Some(0));
        let Some(source) = self.runtimes.source() else {
            self.clamp_available();
            return;
        };
        if matches!(self.runtimes.releases.get(source.key()), Some(Load::Ready(_) | Load::Loading)) {
            return;
        }
        if self.offline {
            self.runtimes.releases.insert(source.key(), Load::Failed("offline".into()));
            return;
        }
        self.runtimes.releases.insert(source.key(), Load::Loading);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = runtime::fetch_available(source).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Releases { source, result });
        });
    }

    fn clamp_available(&mut self) {
        let len = self.runtimes.entries().len();
        let sel = self.runtimes.available_state.selected().unwrap_or(0).min(len.saturating_sub(1));
        self.runtimes.available_state.select((len > 0).then_some(sel));
    }

    fn on_runtimes_key(&mut self, key: KeyEvent) {
        let rs = &mut self.runtimes;
        let (state, len) = match rs.focus {
            Pane::Installed => (&mut rs.installed_state, rs.installed.len()),
            Pane::Available => {
                let len = rs.entries().len();
                (&mut rs.available_state, len)
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Games,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Down | KeyCode::Char('j') => move_sel(state, len, 1),
            KeyCode::Up | KeyCode::Char('k') => move_sel(state, len, -1),
            KeyCode::PageDown => move_sel(state, len, 10),
            KeyCode::PageUp => move_sel(state, len, -10),
            KeyCode::Tab | KeyCode::BackTab => {
                rs.focus = if rs.focus == Pane::Installed { Pane::Available } else { Pane::Installed };
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Char('[')
            | KeyCode::Char(']') => {
                let n = RuntimeScreen::TABS.len();
                let back = matches!(key.code, KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('['));
                rs.tab = if back { (rs.tab + n - 1) % n } else { (rs.tab + 1) % n };
                rs.focus = Pane::Available;
                self.load_tab();
            }
            KeyCode::Char('c') => {
                if let Some(d) = &rs.download {
                    d.cancel.store(true, Ordering::Relaxed);
                    let msg = format!("Cancelling {}…", d.name);
                    self.set_status(msg, Level::Warn);
                }
            }
            KeyCode::Char('u') => {
                // Refresh the release list.
                if let Some(src) = rs.source() {
                    rs.releases.remove(src.key());
                }
                self.load_tab();
            }
            KeyCode::Enter => match rs.focus {
                Pane::Installed => self.set_default_runtime(),
                Pane::Available => self.install_selected(),
            },
            KeyCode::Char('D') if rs.focus == Pane::Installed => self.set_default_runtime(),
            KeyCode::Char('d') | KeyCode::Delete if rs.focus == Pane::Installed => {
                if let Some(rt) = rs.installed_state.selected().and_then(|i| rs.installed.get(i)) {
                    let what = if rt.link_target.is_some() { "Remove the link" } else { "Delete" };
                    let users = runtime::users_of(&self.lock, &rt.name);
                    let mut message = format!("{what} {}?", rt.name);
                    if !users.is_empty() {
                        message.push_str(&format!("\nUsed by: {}", users.join(", ")));
                    }
                    self.modal =
                        Some(Modal::Confirm { kind: ConfirmKind::RemoveRuntime(rt.name.clone()), message });
                }
            }
            _ => {}
        }
    }

    fn set_default_runtime(&mut self) {
        let rs = &self.runtimes;
        let Some(rt) = rs.installed_state.selected().and_then(|i| rs.installed.get(i)) else { return };
        let name = rt.name.clone();
        let unset = self.lock.settings.default_runtime.as_deref() == Some(name.as_str());
        let r = self.lib.update(|lock| {
            lock.settings.default_runtime = (!unset).then(|| name.clone());
            Ok(())
        });
        if let Some((lock, ())) = self.report(r) {
            self.lock = lock;
            if unset {
                self.set_status("Default runtime unset (newest installed is used)", Level::Info);
            } else {
                self.set_status(format!("Default runtime: {name}"), Level::Ok);
            }
        }
    }

    fn install_selected(&mut self) {
        let entries = self.runtimes.entries();
        let Some(entry) = self.runtimes.available_state.selected().and_then(|i| entries.into_iter().nth(i))
        else {
            return;
        };
        match entry {
            AvailEntry::Steam { name, path } => {
                if let Some(n) = self.report(runtime::link(&self.lib, &path, Some(&name))) {
                    self.set_status(format!("Linked {n} from Steam"), Level::Ok);
                }
                self.refresh_installed();
            }
            AvailEntry::Release(avail) => {
                if self.runtimes.download.is_some() {
                    self.set_status("A download is already running", Level::Warn);
                    return;
                }
                if self.runtimes.installed.iter().any(|r| r.name == avail.name) {
                    self.set_status(format!("{} is already installed", avail.name), Level::Warn);
                    return;
                }
                let cancel = Arc::new(AtomicBool::new(false));
                self.runtimes.download = Some(Download {
                    name: avail.name.clone(),
                    progress: Progress { phase: runtime::Phase::Downloading, done: 0, total: avail.size },
                    cancel: cancel.clone(),
                });
                let lib = self.lib.clone();
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let ptx = tx.clone();
                    let result = runtime::install(&lib, &avail, &cancel, &mut |p| {
                        let _ = ptx.send(Msg::Download(p));
                    })
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::DownloadDone(result));
                });
            }
        }
    }

    fn confirm(&mut self, kind: ConfirmKind) {
        match kind {
            ConfirmKind::RemoveRuntime(name) => {
                if let Some(()) = self.report(runtime::remove(&self.lib, &name)) {
                    if self.lock.settings.default_runtime.as_deref() == Some(name.as_str()) {
                        let r = self.lib.update(|lock| {
                            lock.settings.default_runtime = None;
                            Ok(())
                        });
                        self.report(r);
                    }
                    self.set_status(format!("Removed {name}"), Level::Ok);
                }
                self.refresh_installed();
            }
            ConfirmKind::StopGame(dir) => {
                self.set_status(format!("Stopping {}…", self.game_name(&dir)), Level::Info);
                let lib = self.lib.clone();
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let result =
                        session::stop(&lib, &dir, Duration::from_secs(5)).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Stopped { dir, result });
                });
            }
        }
    }
}

fn move_sel(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    let next = (cur + delta).clamp(0, len as isize - 1);
    state.select(Some(next as usize));
}
