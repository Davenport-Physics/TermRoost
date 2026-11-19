//! What has the keyboard, what is selected, and what each key, click and
//! core event does. Drawing is in `view.rs`.
//!
//! The screen has two places: the list of projects and their sessions on the
//! left, and the selected session's terminals. Alt+arrows move between them
//! from anywhere; the list takes plain keys, terminals get every other key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton as CButton, MouseEvent,
    MouseEventKind as CMouse,
};
use ratatui::layout::{Position, Rect};
use termroost_core::{
    Alert, Axis, Command, Core, Event, Layout, MouseEventKind, MouseInput, Outcome, PaneId, ProjectId, RemoteStatus,
    SelectionKind, TabId, TerminalHandle, git,
};

use crate::panes::{Divider, Placed};
use crate::text_field::TextField;
use crate::{clipboard, folders, input, panes};

pub enum Msg {
    Input(TermEvent),
    Core(Event),
    /// The keyboard can't be read any more.
    InputClosed,
    /// SSH host `host` said where the folder asked for is, or why not.
    Resolved {
        host: String,
        result: Result<PathBuf, String>,
    },
}

/// What to show at start: a folder's project (adding it if there is none).
pub enum Open {
    Here(PathBuf),
    /// A folder of an SSH host, as typed (`~/app`).
    Remote {
        host: String,
        path: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Project(ProjectId),
    Session(TabId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    List,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Something being typed or confirmed in the list.
pub enum Edit {
    Rename {
        row: Row,
        field: TextField,
    },
    NewProject {
        field: TextField,
        error: Option<&'static str>,
    },
    Delete(Row),
    /// Quitting would stop these programs.
    Quit(Vec<String>),
}

pub enum Note {
    Done(String),
    Problem(String),
}

/// A mouse button held down.
enum Drag {
    /// Moving a divider; the layout is saved on release.
    Resize { tab: TabId, divider: Divider, layout: Layout },
    /// The program in the terminal gets the mouse.
    Forward { pane: PaneId, button: termroost_core::MouseButton },
    /// Selecting text, from this cell.
    Select { pane: PaneId, from: (usize, usize), moved: bool },
}

/// Where things were drawn, for the mouse.
#[derive(Default)]
pub struct Hits {
    pub list: Rect,
    pub rows: Vec<(u16, Row)>,
    pub tab: Option<TabId>,
    pub panes: Vec<Placed>,
    pub dividers: Vec<Divider>,
}

const NOTE_FOR: Duration = Duration::from_millis(2500);
const MULTI_CLICK: Duration = Duration::from_millis(450);

pub struct App {
    pub core: Core,
    pub focus: Focus,
    /// The row picked in the list; `None` follows the shown session.
    cursor: Option<Row>,
    pub edit: Option<Edit>,
    /// What happened in terminals out of sight, until they are looked at.
    pub alerts: HashMap<PaneId, Alert>,
    /// Whether the terminal window this runs in has the keyboard.
    pub window_focused: bool,
    pub note: Option<(Note, Instant)>,
    pub hits: Hits,
    pub home: Option<PathBuf>,
    /// A layout being resized with the mouse, shown before it is saved.
    pub resizing: Option<(TabId, Layout)>,
    /// First row of the list in view.
    pub list_scroll: usize,
    /// Some session's agent is working, so its spinner turns.
    pub animating: bool,
    /// Escape sequences for the outer terminal (clipboard, title).
    pub out: Vec<String>,
    pub title: String,
    pub cursor_shape: Option<termroost_core::CursorShape>,
    pub branches: HashMap<PathBuf, (Instant, Option<String>)>,
    pub dirty: bool,
    pub quit: bool,
    launch_dir: PathBuf,
    drag: Option<Drag>,
    /// The last left press in a terminal: when, where, and how many in a row.
    last_click: Option<(Instant, PaneId, u16, u16, u8)>,
    /// The terminal last told it has the keyboard.
    keyboard: Option<PaneId>,
    /// Where answers from other threads go (see [`Msg::Resolved`]).
    msgs: std::sync::mpsc::Sender<Msg>,
}

impl App {
    pub fn new(core: Core, launch_dir: PathBuf, open: Option<Open>, msgs: std::sync::mpsc::Sender<Msg>) -> App {
        let focus = if core.state().active_tab().is_some() { Focus::Terminal } else { Focus::List };
        let mut app = App {
            core,
            focus,
            cursor: None,
            edit: None,
            alerts: HashMap::new(),
            window_focused: true,
            note: None,
            hits: Hits::default(),
            home: directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()),
            resizing: None,
            list_scroll: 0,
            animating: false,
            out: Vec::new(),
            title: String::new(),
            cursor_shape: None,
            branches: HashMap::new(),
            dirty: true,
            quit: false,
            launch_dir,
            drag: None,
            last_click: None,
            keyboard: None,
            msgs,
        };
        match open {
            Some(Open::Here(dir)) => app.open_folder(None, dir),
            Some(Open::Remote { host, path }) => app.open_remote(host, path),
            None if app.core.state().projects.is_empty() => app.start_new_project(),
            None => {}
        }
        app.settle();
        app
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input(ev) => {
                self.input(ev);
                self.dirty = true;
            }
            Msg::Core(ev) => self.core_event(ev),
            Msg::InputClosed => self.quit = true,
            Msg::Resolved { host, result } => {
                match result {
                    Ok(dir) => {
                        self.note = None;
                        self.open_folder(Some(host), dir);
                    }
                    Err(e) => self.problem(e),
                }
                self.dirty = true;
            }
        }
        self.settle();
    }

    /// Nothing happened for a while: time moves spinners and notes along.
    pub fn tick(&mut self) {
        if self.note.as_ref().is_some_and(|(_, at)| at.elapsed() >= NOTE_FOR) {
            self.note = None;
        }
        self.dirty = true;
    }

    /// How long to wait for input before [`App::tick`].
    pub fn timeout(&self) -> Duration {
        if self.animating {
            return Duration::from_millis(120);
        }
        match &self.note {
            Some((_, at)) => NOTE_FOR.saturating_sub(at.elapsed()).max(Duration::from_millis(10)),
            // Branches can change under us; look again now and then.
            None => Duration::from_secs(3),
        }
    }

    // ---- reading the state ----

    /// The rows of the list, in order.
    pub fn rows(&self) -> Vec<Row> {
        let s = self.core.state();
        let mut rows = Vec::new();
        for p in &s.projects {
            rows.push(Row::Project(p.id));
            rows.extend(s.tabs_of(p.id).map(|t| Row::Session(t.id)));
        }
        rows
    }

    /// The row the list points at.
    pub fn cursor(&self) -> Option<Row> {
        let s = self.core.state();
        let exists = |r: &Row| match r {
            Row::Project(p) => s.project(*p).is_some(),
            Row::Session(t) => s.tab(*t).is_some(),
        };
        self.cursor
            .filter(exists)
            .or_else(|| s.active_tab().map(|t| Row::Session(t.id)))
            .or_else(|| s.selected_project.map(Row::Project))
            .or_else(|| s.projects.first().map(|p| Row::Project(p.id)))
    }

    fn project_of(&self, row: Row) -> Option<ProjectId> {
        match row {
            Row::Project(p) => Some(p),
            Row::Session(t) => self.core.state().tab(t).map(|t| t.project),
        }
    }

    /// The terminal with the keyboard in the shown session.
    pub fn focused_pane(&self) -> Option<PaneId> {
        let tab = self.core.state().active_tab()?;
        tab.focused_pane.filter(|p| tab.layout.contains(*p)).or(Some(tab.layout.first_pane()))
    }

    fn focused_terminal(&self) -> Option<TerminalHandle> {
        self.core.terminal(self.focused_pane()?)
    }

    /// The terminal that gets typed keys, if any.
    pub fn keyboard_pane(&self) -> Option<PaneId> {
        (self.focus == Focus::Terminal && self.edit.is_none()).then(|| self.focused_pane()).flatten()
    }

    /// Whether the pane is in the session on screen.
    fn shown(&self, pane: PaneId) -> bool {
        self.core.state().active_tab().is_some_and(|t| t.layout.contains(pane))
    }

    // ---- after every change ----

    /// Keeps derived things in step: what was looked at stops alerting, and
    /// terminals hear when they gain or lose the keyboard.
    fn settle(&mut self) {
        if self.window_focused
            && let Some(tab) = self.core.state().active_tab()
        {
            for pane in tab.layout.panes() {
                self.alerts.remove(&pane);
            }
        }
        if self.focus == Focus::Terminal && self.core.state().active_tab().is_none() {
            self.focus = Focus::List;
        }
        let target = self.keyboard_pane().filter(|_| self.window_focused);
        if target != self.keyboard {
            if let Some(old) = self.keyboard.and_then(|p| self.core.terminal(p)) {
                old.focus_changed(false);
            }
            if let Some(new) = target.and_then(|p| self.core.terminal(p)) {
                new.focus_changed(true);
            }
            self.keyboard = target;
        }
    }

    fn dispatch(&mut self, cmd: Command) -> Option<Outcome> {
        match self.core.dispatch(cmd) {
            Ok(outcome) => Some(outcome),
            Err(e) => {
                self.problem(e.to_string());
                None
            }
        }
    }

    fn problem(&mut self, text: impl Into<String>) {
        self.note = Some((Note::Problem(text.into()), Instant::now()));
    }

    // ---- core events ----

    fn core_event(&mut self, ev: Event) {
        self.core.apply_event(&ev);
        match ev {
            Event::TerminalDirty(pane) | Event::TitleChanged { pane, .. } => {
                if self.shown(pane) {
                    self.dirty = true;
                }
                return;
            }
            Event::ProcessExited { pane, code } => self.exited(pane, code),
            Event::ClipboardStore(text) => self.out.push(clipboard::osc52(&text)),
            Event::Alert { pane, alert } => {
                if !(self.window_focused && self.shown(pane)) {
                    self.alerts.insert(pane, alert);
                }
            }
            Event::Remote { host, status } => match status {
                RemoteStatus::Copying => {
                    self.note =
                        Some((Note::Done(format!("Copying Terminal Rooster's helper to {host}…")), Instant::now()))
                }
                RemoteStatus::Failed(e) => self.problem(e),
                _ => {}
            },
            Event::Bell(_)
            | Event::CodexHookReported
            | Event::AgentObserved { .. }
            | Event::CwdChanged { .. }
            | Event::ActivityChanged(_)
            | Event::ShownElsewhere(_) => {}
        }
        self.dirty = true;
    }

    /// A shell that was ended on purpose (`exit`) closes its terminal, unless
    /// it is the session's last one; that one waits to be restarted.
    fn exited(&mut self, pane: PaneId, code: Option<i32>) {
        let s = self.core.state();
        let others = s.pane(pane).and_then(|p| s.tab(p.tab)).is_some_and(|t| t.layout.panes().len() > 1);
        let restarted = self.core.terminal(pane).is_some_and(|h| h.is_running());
        if code == Some(0) && others && !restarted {
            self.dispatch(Command::ClosePane(pane));
        }
    }

    // ---- input ----

    fn input(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => self.key(k),
            TermEvent::Mouse(m) => self.mouse(m),
            TermEvent::Paste(text) => self.paste(&text),
            TermEvent::FocusGained => self.window_focused = true,
            TermEvent::FocusLost => self.window_focused = false,
            _ => {}
        }
    }

    fn key(&mut self, k: KeyEvent) {
        if self.edit.is_some() {
            return self.key_edit(k);
        }
        if k.modifiers == KeyModifiers::ALT {
            let dir = match k.code {
                KeyCode::Left => Some(Dir::Left),
                KeyCode::Right => Some(Dir::Right),
                KeyCode::Up => Some(Dir::Up),
                KeyCode::Down => Some(Dir::Down),
                _ => None,
            };
            if let Some(dir) = dir {
                return self.go(dir);
            }
        }
        match self.focus {
            Focus::List => self.key_list(k),
            Focus::Terminal => self.key_terminal(k),
        }
    }

    fn key_terminal(&mut self, k: KeyEvent) {
        let only = |m: KeyModifiers| k.modifiers == m;
        let Some(pane) = self.focused_pane() else { return };
        let Some(h) = self.core.terminal(pane) else { return };
        match k.code {
            KeyCode::PageUp if only(KeyModifiers::SHIFT) => h.scroll_page(true),
            KeyCode::PageDown if only(KeyModifiers::SHIFT) => h.scroll_page(false),
            _ if !h.is_running() => {
                if k.code == KeyCode::Enter {
                    self.dispatch(Command::RestartPane(pane));
                }
            }
            _ => {
                if let Some(key) = input::key(&k) {
                    h.key(&key);
                }
            }
        }
    }

    fn key_list(&mut self, k: KeyEvent) {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            if k.modifiers == KeyModifiers::CONTROL && k.code == KeyCode::Char('c') {
                self.ask_quit();
            }
            return;
        }
        match k.code {
            KeyCode::Up if shift => self.move_row(-1),
            KeyCode::Down if shift => self.move_row(1),
            KeyCode::Char('K') => self.move_row(-1),
            KeyCode::Char('J') => self.move_row(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Home | KeyCode::Char('g') => self.move_cursor(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(isize::MAX),
            KeyCode::Enter => self.open_cursor(true),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Esc | KeyCode::Tab => self.open_cursor(false),
            KeyCode::Char('n') => self.new_session(),
            KeyCode::Char('p') => self.start_new_project(),
            KeyCode::Char('r') | KeyCode::F(2) => self.start_rename(),
            KeyCode::Char('d') | KeyCode::Delete => {
                self.edit = self.cursor().map(Edit::Delete);
            }
            KeyCode::Char('v') => self.split(Axis::Vertical),
            KeyCode::Char('s') => self.split(Axis::Horizontal),
            KeyCode::Char('x') => self.close_pane(),
            KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    fn key_edit(&mut self, k: KeyEvent) {
        let Some(mut edit) = self.edit.take() else { return };
        let keep = match &mut edit {
            Edit::Rename { row, field } => match k.code {
                KeyCode::Enter => {
                    let (row, name) = (*row, field.text());
                    self.rename(row, name);
                    false
                }
                KeyCode::Esc => false,
                _ => {
                    field.key(&k);
                    true
                }
            },
            Edit::NewProject { field, error } => match k.code {
                KeyCode::Enter => {
                    let text = field.text();
                    let dir = folders::resolve(&text, &self.launch_dir, self.home.as_deref());
                    if dir.is_dir() {
                        self.open_folder(None, dir);
                        false
                    } else if let Some((host, path)) = folders::split_remote(&text) {
                        self.open_remote(host, path);
                        false
                    } else {
                        *error = Some("No such folder");
                        true
                    }
                }
                KeyCode::Esc => false,
                KeyCode::Tab => {
                    let text = field.text();
                    let done = match folders::split_remote(&text) {
                        Some((host, path)) => self.complete_remote(&host, &path).map(|p| format!("{host}:{p}")),
                        None => folders::complete(&text, &self.launch_dir, self.home.as_deref()),
                    };
                    if let Some(done) = done {
                        field.set(&done);
                        *error = None;
                    }
                    true
                }
                _ => {
                    if field.key(&k) {
                        *error = None;
                    }
                    true
                }
            },
            Edit::Delete(row) => {
                if matches!(k.code, KeyCode::Char('y' | 'Y')) {
                    let cmd = match *row {
                        Row::Project(p) => Command::DeleteProject(p),
                        Row::Session(t) => Command::DeleteTab(t),
                    };
                    self.dispatch(cmd);
                    self.cursor = None;
                }
                false
            }
            Edit::Quit(_) => {
                self.quit = matches!(k.code, KeyCode::Char('y' | 'Y'));
                false
            }
        };
        if keep {
            self.edit = Some(edit);
        }
    }

    fn paste(&mut self, text: &str) {
        match &mut self.edit {
            Some(Edit::Rename { field, .. } | Edit::NewProject { field, .. }) => field.insert(text),
            Some(_) => {}
            None if self.focus == Focus::Terminal => {
                if let Some(h) = self.focused_terminal() {
                    h.paste(text);
                }
            }
            None => {}
        }
    }

    // ---- moving around ----

    fn select(&mut self, row: Row) {
        self.cursor = Some(row);
        match row {
            Row::Session(t) => self.dispatch(Command::SelectTab(t)),
            Row::Project(p) => self.dispatch(Command::SelectProject(p)),
        };
    }

    fn move_cursor(&mut self, by: isize) {
        let rows = self.rows();
        let Some(last) = rows.len().checked_sub(1) else { return };
        let at = self.cursor().and_then(|c| rows.iter().position(|r| *r == c)).unwrap_or(0);
        let to = at.saturating_add_signed(by).min(last);
        if to != at || self.cursor.is_none() {
            self.select(rows[to]);
        }
    }

    /// The next or previous session, in list order, round the end.
    fn step_session(&mut self, by: isize) {
        let tabs: Vec<TabId> = self.core.state().tabs.iter().map(|t| t.id).collect();
        if tabs.is_empty() {
            return;
        }
        let at = self.core.state().active_tab().and_then(|t| tabs.iter().position(|id| *id == t.id));
        let to = match at {
            Some(i) => (i as isize + by).rem_euclid(tabs.len() as isize) as usize,
            None => 0,
        };
        self.select(Row::Session(tabs[to]));
    }

    /// Alt+arrows: to the terminal that way. Past the left edge is the list;
    /// past the top and bottom, the previous and next sessions.
    fn go(&mut self, dir: Dir) {
        if self.focus == Focus::List {
            match dir {
                Dir::Up => self.step_session(-1),
                Dir::Down => self.step_session(1),
                Dir::Right => self.open_cursor(false),
                Dir::Left => {}
            }
            return;
        }
        let Some(from) = self.focused_pane() else { return };
        if let Some(to) = self.neighbor(from, dir) {
            self.dispatch(Command::FocusPane(to));
            return;
        }
        match dir {
            Dir::Left => {
                self.focus = Focus::List;
                self.cursor = self.core.state().active_tab().map(|t| Row::Session(t.id));
            }
            Dir::Up => self.step_session(-1),
            Dir::Down => self.step_session(1),
            Dir::Right => {}
        }
    }

    /// The terminal on screen next to `from` in that direction: the nearest,
    /// then the one sharing the most rows (or columns) with it.
    fn neighbor(&self, from: PaneId, dir: Dir) -> Option<PaneId> {
        // The places are those of the last drawing, which may show another session.
        if self.hits.tab != self.core.state().active_tab().map(|t| t.id) {
            return None;
        }
        let cur = self.hits.panes.iter().find(|p| p.pane == from)?.area;
        let overlap = |a: (u16, u16), b: (u16, u16)| a.1.min(b.1) as i32 - a.0.max(b.0) as i32;
        let rows = |r: Rect| (r.y, r.bottom());
        let cols = |r: Rect| (r.x, r.right());
        self.hits
            .panes
            .iter()
            .filter(|p| p.pane != from)
            .filter_map(|p| {
                let a = p.area;
                let (gap, shared) = match dir {
                    Dir::Left => (cur.x as i32 - a.right() as i32, overlap(rows(cur), rows(a))),
                    Dir::Right => (a.x as i32 - cur.right() as i32, overlap(rows(cur), rows(a))),
                    Dir::Up => (cur.y as i32 - a.bottom() as i32, overlap(cols(cur), cols(a))),
                    Dir::Down => (a.y as i32 - cur.bottom() as i32, overlap(cols(cur), cols(a))),
                };
                (gap >= 0 && shared > 0).then_some((gap, -shared, p.pane))
            })
            .min()
            .map(|(_, _, pane)| pane)
    }

    /// Goes to the terminals of the row in the list. `create`: a project
    /// without sessions gets one.
    fn open_cursor(&mut self, create: bool) {
        let Some(row) = self.cursor() else { return };
        if let Row::Project(p) = row
            && self.core.state().tabs_of(p).next().is_none()
        {
            if create {
                self.new_session();
            }
            return;
        }
        self.select(row);
        if let Some(tab) = self.core.state().active_tab() {
            self.cursor = Some(Row::Session(tab.id));
            self.focus = Focus::Terminal;
        }
    }

    fn click_row(&mut self, row: Row) {
        self.edit = None;
        self.select(row);
        self.focus = match row {
            Row::Session(_) => Focus::Terminal,
            Row::Project(_) => Focus::List,
        };
    }

    // ---- changes ----

    fn new_session(&mut self) {
        let project = self.cursor().and_then(|r| self.project_of(r));
        let Some(project) = project else { return self.start_new_project() };
        if let Some(Outcome::Tab(tab)) = self.dispatch(Command::CreateTab { project, title: None, working_dir: None }) {
            self.cursor = Some(Row::Session(tab));
            self.focus = Focus::Terminal;
        }
    }

    fn start_new_project(&mut self) {
        let s = self.core.state();
        let taken = |dir: &Path| s.projects.iter().any(|p| git::same_path(&p.working_dir, dir));
        let home = self.home.as_deref();
        let text = if taken(&self.launch_dir) {
            // Most likely a sibling of the project at hand.
            let near = self.cursor().and_then(|r| self.project_of(r)).and_then(|p| s.project(p));
            let parent = near.and_then(|p| p.working_dir.parent()).unwrap_or(&self.launch_dir);
            let shown = folders::pretty(parent, home);
            if shown.ends_with(std::path::is_separator) {
                shown
            } else {
                format!("{shown}{}", std::path::MAIN_SEPARATOR)
            }
        } else {
            folders::pretty(&self.launch_dir, home)
        };
        self.edit = Some(Edit::NewProject { field: TextField::new(&text), error: None });
        self.focus = Focus::List;
    }

    /// Asks SSH host `host` where folder `path` is there (on a thread:
    /// connecting takes a moment); its project is shown once it answers.
    fn open_remote(&mut self, host: String, path: String) {
        if let Err(e) = termroost_core::remote::check_host(&host) {
            return self.problem(e);
        }
        self.note = Some((Note::Done(format!("Connecting to {host}…")), Instant::now()));
        let machine = self.core.machine(Some(&host));
        let msgs = self.msgs.clone();
        std::thread::spawn(move || {
            let result = machine.resolve_dir(Path::new(&path));
            let _ = msgs.send(Msg::Resolved { host, result });
        });
    }

    /// Tab completion of a folder on SSH host `host`, while connected
    /// (asking it to connect would hold up the keyboard).
    fn complete_remote(&self, host: &str, path: &str) -> Option<String> {
        if self.core.remote_status(host) != RemoteStatus::Connected || !path.starts_with('/') {
            return None;
        }
        let found = self.core.machine(Some(host)).complete_dir(path, 200);
        let names =
            found.iter().filter_map(|f| f.trim_end_matches('/').rsplit('/').next().map(str::to_owned)).collect();
        folders::complete_from(path, names)
    }

    /// Shows the project working in `dir` (on SSH host `host`, or this
    /// computer), adding one if there is none.
    fn open_folder(&mut self, host: Option<String>, dir: PathBuf) {
        let dir: PathBuf = dir.components().collect();
        let existing = self
            .core
            .state()
            .projects
            .iter()
            .find(|p| p.host == host && git::same_path(&p.working_dir, &dir))
            .map(|p| p.id);
        match existing {
            Some(p) => {
                self.select(Row::Project(p));
                if self.core.state().tabs_of(p).next().is_none() {
                    self.new_session();
                }
            }
            None => {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                self.dispatch(Command::CreateProject { name, working_dir: dir, shell: None, host });
            }
        }
        self.cursor = None;
        self.focus = Focus::Terminal;
    }

    fn start_rename(&mut self) {
        let Some(row) = self.cursor() else { return };
        let s = self.core.state();
        let name = match row {
            Row::Project(p) => s.project(p).map(|p| p.name.clone()),
            Row::Session(t) => s.tab(t).map(|t| t.title.clone()),
        };
        if let Some(name) = name {
            self.edit = Some(Edit::Rename { row, field: TextField::new(&name) });
        }
    }

    fn rename(&mut self, row: Row, name: String) {
        if name.trim().is_empty() {
            return;
        }
        self.dispatch(match row {
            Row::Project(project) => Command::RenameProject { project, name },
            Row::Session(tab) => Command::RenameTab { tab, title: name },
        });
    }

    fn split(&mut self, axis: Axis) {
        let Some(pane) = self.focused_pane() else { return };
        if self.dispatch(Command::SplitPane { pane, axis }).is_some() {
            self.cursor = None;
            self.focus = Focus::Terminal;
        }
    }

    /// Closes the terminal with the keyboard; the last one closes its session (after asking).
    fn close_pane(&mut self) {
        let Some(tab) = self.core.state().active_tab() else { return };
        let (tab, alone) = (tab.id, tab.layout.panes().len() == 1);
        if alone {
            self.edit = Some(Edit::Delete(Row::Session(tab)));
        } else if let Some(pane) = self.focused_pane() {
            self.dispatch(Command::ClosePane(pane));
        }
    }

    /// Moves the row up or down among its siblings.
    fn move_row(&mut self, by: isize) {
        let Some(row) = self.cursor() else { return };
        let s = self.core.state();
        let cmd = match row {
            Row::Session(tab) => {
                let Some(project) = s.tab(tab).map(|t| t.project) else { return };
                let at = s.tabs_of(project).position(|t| t.id == tab).unwrap_or(0);
                Command::MoveTab { tab, to_index: at.saturating_add_signed(by) }
            }
            Row::Project(project) => {
                let at = s.projects.iter().position(|p| p.id == project).unwrap_or(0);
                let group = s.project(project).and_then(|p| p.group);
                Command::MoveProject { project, to_index: at.saturating_add_signed(by), group }
            }
        };
        // Refused at a group's edge; nothing to say about that.
        let _ = self.core.dispatch(cmd);
        self.cursor = Some(row);
    }

    fn ask_quit(&mut self) {
        let mut names: Vec<String> = Vec::new();
        for (_, programs) in self.core.running_programs() {
            for p in programs {
                if !names.contains(&p.name) {
                    names.push(p.name);
                }
            }
        }
        if names.is_empty() {
            self.quit = true;
        } else {
            self.edit = Some(Edit::Quit(names));
        }
    }

    // ---- mouse ----

    fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let mods = input::modifiers(m.modifiers);
        match m.kind {
            CMouse::Down(b) => self.press(b, x, y, mods),
            CMouse::Drag(_) => self.drag_to(x, y),
            CMouse::Up(_) => self.release(x, y),
            CMouse::Moved => {
                if let Some((placed, col, row)) = self.cell_at(x, y)
                    && let Some(h) = self.core.terminal(placed.pane)
                    && h.wants_mouse()
                {
                    h.mouse(&MouseInput { kind: MouseEventKind::Move { held: None }, col, row, mods });
                }
            }
            CMouse::ScrollUp | CMouse::ScrollDown => {
                let delta = if m.kind == CMouse::ScrollUp { 3 } else { -3 };
                if let Some((placed, col, row)) = self.cell_at(x, y)
                    && let Some(h) = self.core.terminal(placed.pane)
                {
                    h.scroll_lines(delta, col, row);
                }
            }
            _ => {}
        }
    }

    /// The terminal cell under the mouse, if it is over one (and not under the list).
    fn cell_at(&self, x: u16, y: u16) -> Option<(Placed, usize, usize)> {
        if self.hits.list.contains(Position { x, y }) {
            return None;
        }
        let placed = *self.hits.panes.iter().find(|p| p.content().contains(Position { x, y }))?;
        let c = placed.content();
        Some((placed, (x - c.x) as usize, (y - c.y) as usize))
    }

    /// The cell in the pane's content nearest to the mouse.
    fn clamped_cell(&self, pane: PaneId, x: u16, y: u16) -> Option<(usize, usize)> {
        let c = self.hits.panes.iter().find(|p| p.pane == pane)?.content();
        if c.width == 0 || c.height == 0 {
            return None;
        }
        let col = x.clamp(c.x, c.right() - 1) - c.x;
        let row = y.clamp(c.y, c.bottom() - 1) - c.y;
        Some((col as usize, row as usize))
    }

    fn press(&mut self, button: CButton, x: u16, y: u16, mods: termroost_core::Modifiers) {
        let left = button == CButton::Left;
        if self.hits.list.contains(Position { x, y }) {
            if left && let Some((_, row)) = self.hits.rows.iter().find(|(ry, _)| *ry == y).copied() {
                self.click_row(row);
            }
            return;
        }
        if left
            && let Some(tab) = self.hits.tab
            && let Some(divider) = self.hits.dividers.iter().find(|d| d.hit(x, y)).cloned()
            && let Some(layout) = self.core.state().tab(tab).map(|t| t.layout.clone())
        {
            self.resizing = Some((tab, layout.clone()));
            self.drag = Some(Drag::Resize { tab, divider, layout });
            return;
        }
        let Some(placed) = self.hits.panes.iter().find(|p| p.area.contains(Position { x, y })).copied() else {
            return;
        };
        self.edit = None;
        self.focus = Focus::Terminal;
        if self.focused_pane() != Some(placed.pane) {
            self.dispatch(Command::FocusPane(placed.pane));
        }
        let Some((_, col, row)) = self.cell_at(x, y) else { return };
        let Some(h) = self.core.terminal(placed.pane) else { return };
        let pane = placed.pane;
        if h.wants_mouse() && !mods.shift {
            let button = input::button(button);
            h.mouse(&MouseInput { kind: MouseEventKind::Press(button), col, row, mods });
            self.drag = Some(Drag::Forward { pane, button });
            return;
        }
        if !left {
            return;
        }
        let count = match self.last_click {
            Some((at, p, cx, cy, n)) if p == pane && (cx, cy) == (x, y) && at.elapsed() < MULTI_CLICK => n % 3 + 1,
            _ => 1,
        };
        self.last_click = Some((Instant::now(), pane, x, y, count));
        let kind = match count {
            1 => SelectionKind::Simple,
            2 => SelectionKind::Word,
            _ => SelectionKind::Line,
        };
        h.select_start(col, row, false, kind);
        self.drag = Some(Drag::Select { pane, from: (col, row), moved: count > 1 });
    }

    fn drag_to(&mut self, x: u16, y: u16) {
        match &mut self.drag {
            Some(Drag::Resize { tab, divider, layout }) => {
                panes::set_ratio(layout, &divider.path, divider.ratio_at(x, y));
                self.resizing = Some((*tab, layout.clone()));
            }
            Some(Drag::Forward { pane, button }) => {
                let (pane, button) = (*pane, *button);
                if let Some((col, row)) = self.clamped_cell(pane, x, y)
                    && let Some(h) = self.core.terminal(pane)
                {
                    let kind = MouseEventKind::Move { held: Some(button) };
                    h.mouse(&MouseInput { kind, col, row, mods: termroost_core::Modifiers::NONE });
                }
            }
            Some(Drag::Select { pane, from, moved }) => {
                *moved = true;
                let (pane, from) = (*pane, *from);
                if let Some((col, row)) = self.clamped_cell(pane, x, y)
                    && let Some(h) = self.core.terminal(pane)
                {
                    h.select_update(col, row, (row, col) >= (from.1, from.0));
                }
            }
            None => {}
        }
    }

    fn release(&mut self, x: u16, y: u16) {
        match self.drag.take() {
            Some(Drag::Resize { tab, layout, .. }) => {
                self.resizing = None;
                self.dispatch(Command::SetLayout { tab, layout });
            }
            Some(Drag::Forward { pane, button }) => {
                if let Some((col, row)) = self.clamped_cell(pane, x, y)
                    && let Some(h) = self.core.terminal(pane)
                {
                    let kind = MouseEventKind::Release(button);
                    h.mouse(&MouseInput { kind, col, row, mods: termroost_core::Modifiers::NONE });
                }
            }
            Some(Drag::Select { pane, moved, .. }) => {
                let Some(h) = self.core.terminal(pane) else { return };
                if !moved {
                    h.clear_selection();
                } else if let Some(text) = h.selection_text() {
                    self.out.push(clipboard::osc52(&text));
                    self.note = Some((Note::Done("Copied".into()), Instant::now()));
                }
            }
            None => {}
        }
    }
}
