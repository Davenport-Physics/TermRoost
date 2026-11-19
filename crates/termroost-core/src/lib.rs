//! GUI-agnostic core of TerminalRooster.
//!
//! A front end talks to [`Core`] only:
//!
//! * [`Core::state`] — read model (projects, tabs, panes, tasks) to render lists.
//! * [`Core::dispatch`] — every change, as a [`Command`].
//! * [`Core::terminal`] — a [`TerminalHandle`] per visible pane, for drawing
//!   ([`TerminalHandle::with_content`]) and input.
//! * [`Core::events`] — asynchronous [`Event`]s; pass each one to
//!   [`Core::apply_event`] before handling it.
//!
//! All state is persisted to SQLite as it changes, so the app comes back as it
//! was after a crash or restart. No GUI, terminal-emulator or database types
//! appear in this API.

pub mod agent;
pub mod alert;
pub mod codex_hook;
pub mod command;
pub mod detect;
pub mod error;
pub mod files;
pub mod git;
pub mod host;
pub mod layout;
pub mod model;
pub mod paths;
pub mod remote;
pub mod session;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

pub use agent::{AgentKind, AgentSession};
pub use alert::{AgentEvent, Alert};
pub use codex_hook::CodexHookStatus;
pub use command::{Command, Event, Outcome};
pub use detect::{AgentHomes, ObservedAgent, Program};
pub use error::{Error, Result};
pub use host::HostCommand;
pub use layout::{Axis, Layout};
pub use model::*;
pub use remote::{Machine, RemoteHost, RemoteStatus};
pub use session::input::{Key, KeyInput, Modifiers, MouseButton, MouseEventKind, MouseInput, NamedKey};
pub use session::render::{CellStyle, CursorInfo, CursorShape, Palette, RenderCell, RenderContent, Rgb};
pub use session::{Activity, GridSize, SelectionKind, TerminalHandle};

use paths::InstanceLock;
use remote::client::Remotes;
use remote::wire::{OpenMode, OpenTerminal, Request, Resume, StartSpec};
use session::{Background, HostPlan, NewHost, RemotePlan, SpawnSpec};
use store::{Store, StoreOp};

/// The name of a group made by [`Command::GroupProjects`].
pub const NEW_GROUP_NAME: &str = "New group";

/// Options for [`Core::open`].
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the database lives; `None` uses [`paths::default_data_dir`].
    pub data_dir: Option<PathBuf>,
    pub palette: Palette,
    /// How often changed scrollback is saved and terminals are checked for
    /// agents and other programs.
    pub autosave_interval: Duration,
    /// Where Claude Code, Codex and Grok keep their session records.
    pub agent_homes: AgentHomes,
    /// How to start the helper process a terminal that keeps running when
    /// TerminalRooster quits runs under (see [`host`]). Without one, such
    /// terminals run like the others.
    pub host: Option<HostCommand>,
    /// How remote hosts are reached (over SSH, unless a test says otherwise).
    pub remote: remote::Transport,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: None,
            palette: Palette::default(),
            autosave_interval: Duration::from_secs(5),
            agent_homes: AgentHomes::from_env(),
            host: None,
            remote: remote::Transport::default(),
        }
    }
}

/// What [`Core::open`] carried over from TermRoost, the app's previous name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Renamed {
    /// TermRoost's data directory, whose contents were moved into the data directory.
    pub moved_from: Option<PathBuf>,
    /// TermRoost's Codex hook was rewritten, so Codex needs it trusted again.
    pub codex_hook_updated: bool,
}

/// What [`Core::leave_dir`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeftDir {
    /// The tabs that worked in the directory, now deleted.
    pub closed: Vec<TabId>,
    /// Terminals of other tabs whose shell is in the directory and still
    /// there, because a program or an agent runs in them.
    pub busy: Vec<PaneId>,
    /// The processes of the terminals that were stopped or restarted.
    pub pids: Vec<u32>,
}

impl LeftDir {
    /// Waits until the processes that were in the directory have exited
    /// (stopping takes a moment), for at most `timeout`. Returns whether they
    /// all did. Blocks, so call it off the GUI thread.
    pub fn wait(&self, timeout: Duration) -> bool {
        detect::wait_for_exit(&self.pids, timeout)
    }
}

pub struct Core {
    state: AppState,
    store: Store,
    bg: Background,
    events_rx: async_channel::Receiver<Event>,
    palette: session::render::SharedPalette,
    next_id: i64,
    /// Panes loaded from the database whose terminal has not been started yet.
    restored: HashSet<PaneId>,
    /// Temporary terminals outside any project (negative ids), never saved.
    scratch: HashMap<PaneId, TerminalHandle>,
    next_scratch: i64,
    autosave: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
    data_dir: PathBuf,
    homes: AgentHomes,
    host: Option<HostCommand>,
    renamed: Renamed,
    /// Where the shell integration scripts are, if they could be written.
    integration: Option<PathBuf>,
    /// The remote hosts projects are on, connected as needed.
    remotes: Arc<Remotes>,
    _lock: InstanceLock,
}

impl Core {
    /// Opens the database (creating it if needed), restores the previous state
    /// and starts the terminals of the visible tab. With the default data
    /// directory, TermRoost's data is moved over first.
    pub fn open(config: Config) -> Result<Core> {
        let mut renamed = Renamed::default();
        let data_dir = match config.data_dir {
            Some(d) => d,
            None => {
                let dir = paths::default_data_dir()?;
                if std::env::var_os(paths::DATA_DIR_ENV).is_none()
                    && let Some(legacy) = paths::legacy_data_dir()
                    && paths::migrate_legacy(&legacy, &dir)?
                {
                    renamed.moved_from = Some(legacy);
                }
                dir
            }
        };
        std::fs::create_dir_all(&data_dir)?;
        let lock = InstanceLock::acquire(&data_dir)?;
        // Checked on every start, so a failed rewrite is retried next time.
        match codex_hook::migrate(&config.agent_homes.codex, &data_dir) {
            Ok(updated) => renamed.codex_hook_updated = updated,
            Err(e) => tracing::warn!(error = %e, "could not update TermRoost's Codex hook"),
        }
        let (store, mut state) = Store::open(&data_dir.join(paths::DB_FILE))?;
        let integration = session::integration::install(&data_dir)
            .map_err(|e| tracing::warn!(error = %e, "could not write the shell integration scripts"))
            .ok();

        let mut next_id = max_id(&state) + 1;
        let repairs = repair(&mut state, &mut next_id);
        if !repairs.is_empty() {
            tracing::warn!("repaired {} inconsistencies in saved state", repairs.len());
            store.write(repairs);
        }
        if let Some(dir) = &integration {
            session::integration::forget_history(dir, |pane| state.panes.contains_key(&pane));
        }

        let (events_tx, events_rx) = async_channel::unbounded();
        let remotes = Arc::new(Remotes::new(remote::client::Shared {
            client: client_id(&data_dir),
            transport: config.remote,
            events: events_tx.clone(),
            keep: Mutex::new(HashMap::new()),
        }));
        let bg = Background {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            detector: Arc::new(Mutex::new(detect::Detector::new(config.agent_homes.clone()))),
            data_dir: data_dir.clone(),
            started: SystemTime::now(),
            last_codex_report: Arc::new(Mutex::new(None)),
            store: store.writer(),
            events: events_tx,
            saved_lines: Arc::new(AtomicUsize::new(state.settings.saved_scrollback_lines)),
            ptys: Arc::new(AtomicUsize::new(0)),
        };
        codex_hook::set_running(&data_dir, true);
        let (stop_tx, stop_rx) = mpsc::channel();
        let autosave = bg.clone().spawn(stop_rx, config.autosave_interval);

        let restored = state.panes.keys().copied().collect();
        let mut core = Core {
            state,
            store,
            bg,
            events_rx,
            palette: Arc::new(std::sync::RwLock::new(config.palette)),
            next_id,
            restored,
            scratch: HashMap::new(),
            next_scratch: 1,
            autosave: Some((stop_tx, autosave)),
            data_dir,
            homes: config.agent_homes,
            host: config.host,
            renamed,
            integration,
            remotes,
            _lock: lock,
        };
        core.refresh_remote_terminals();
        tracing::info!(
            data_dir = %core.data_dir.display(),
            projects = core.state.projects.len(),
            tabs = core.state.tabs.len(),
            terminals = core.state.panes.len(),
            tasks = core.state.tasks.len(),
            "core opened"
        );
        core.end_stray_helpers();
        if let Some(tab) = core.state.active_tab().map(|t| t.id) {
            core.activate_tab(tab);
        }
        Ok(core)
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// The computer a folder of `host` is on: this one for `None`. A remote
    /// one connects when first asked something (see [`Machine`]).
    pub fn machine(&self, host: Option<&str>) -> Machine {
        match host {
            None => Machine::Local,
            Some(h) => Machine::Remote(self.remotes.host(h)),
        }
    }

    /// The computer a project's folder is on.
    pub fn project_machine(&self, project: ProjectId) -> Machine {
        self.machine(self.state.project(project).and_then(|p| p.host.as_deref()))
    }

    /// The computer a session's folder is on: its project's.
    pub fn tab_machine(&self, tab: TabId) -> Machine {
        self.state.tab(tab).map(|t| self.project_machine(t.project)).unwrap_or_default()
    }

    /// Where the connection to `host` stands.
    pub fn remote_status(&self, host: &str) -> RemoteStatus {
        self.remotes.host(host).status()
    }

    /// Drops the connection to `host`, if there is one. Its terminals run on
    /// there and connect again by themselves, as after a broken connection.
    pub fn reconnect(&self, host: &str) {
        tracing::info!(host, "dropping the connection to a remote host");
        self.remotes.host(host).disconnect();
    }

    /// The host of a pane's project, for a pane of a remote project.
    fn remote_of(&self, pane: PaneId) -> Option<RemoteHost> {
        let tab = self.state.tab(self.state.pane(pane)?.tab)?;
        let host = self.state.project(tab.project)?.host.as_deref()?;
        Some(self.remotes.host(host))
    }

    /// Tells each host which of its terminals there still are (see
    /// [`remote::wire::ClientHello::keep`]).
    fn refresh_remote_terminals(&self) {
        let mut keep: HashMap<String, Vec<PaneId>> = HashMap::new();
        for pane in self.state.panes.values() {
            let host =
                self.state.tab(pane.tab).and_then(|t| self.state.project(t.project)).and_then(|p| p.host.clone());
            if let Some(host) = host {
                keep.entry(host).or_default().push(pane.id);
            }
        }
        self.remotes.set_keep(keep);
    }

    /// What this start carried over from TermRoost (usually nothing).
    pub fn renamed(&self) -> &Renamed {
        &self.renamed
    }

    /// Receiver of asynchronous events (clone it freely).
    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events_rx.clone()
    }

    /// Replaces the terminal color palette (e.g. on a theme change). All
    /// terminals pick it up on their next redraw, and color queries from
    /// programs (OSC 10/11) answer with the new colors.
    pub fn set_palette(&self, palette: Palette) {
        tracing::debug!("terminal palette replaced");
        *self.palette.write().unwrap() = palette;
        for h in self.bg.sessions.lock().unwrap().values() {
            h.invalidate();
        }
    }

    /// Whether Codex is installed and TerminalRooster's Codex hook is set up.
    pub fn codex_hook_status(&self) -> CodexHookStatus {
        codex_hook::status(&self.homes.codex)
    }

    /// When the Codex hook last reported (since TerminalRooster started).
    pub fn codex_hook_last_report(&self) -> Option<SystemTime> {
        *self.bg.last_codex_report.lock().unwrap()
    }

    /// Adds TerminalRooster's hook to Codex (`~/.codex/hooks.json`) so Codex
    /// sessions are tracked exactly. Codex asks the user to trust it once
    /// (`/hooks`). Returns the file that was changed.
    pub fn install_codex_hook(&self) -> Result<PathBuf> {
        Ok(codex_hook::install(&self.homes.codex, &self.data_dir)?)
    }

    /// Removes TerminalRooster's Codex hook, leaving any other hooks in place.
    pub fn remove_codex_hook(&self) -> Result<()> {
        Ok(codex_hook::remove(&self.homes.codex)?)
    }

    /// The terminal of a pane, if it has been started in this run.
    pub fn terminal(&self, pane: PaneId) -> Option<TerminalHandle> {
        self.bg.sessions.lock().unwrap().get(&pane).cloned()
    }

    /// Process ids of the running shells of all terminals.
    pub fn shell_pids(&self) -> Vec<u32> {
        let sessions = self.bg.sessions.lock().unwrap();
        sessions.values().chain(self.scratch.values()).filter_map(|h| h.shell_pid()).collect()
    }

    /// Records the effects of an event on the state. Call for every event.
    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::AgentObserved { pane, agent } => self.record_agent(*pane, agent.as_ref()),
            Event::CwdChanged { pane, dir } => {
                if let Some(p) = self.state.panes.get_mut(pane)
                    && p.cwd.as_ref() != Some(dir)
                {
                    tracing::debug!(%pane, dir = %dir.display(), "working directory changed");
                    p.cwd = Some(dir.clone());
                    self.store.write(vec![StoreOp::UpsertPane(p.clone())]);
                }
            }
            _ => {}
        }
    }

    /// Updates a pane's agent from what the detector saw running in it.
    fn record_agent(&mut self, pane: PaneId, observed: Option<&ObservedAgent>) {
        let Some(p) = self.state.panes.get_mut(&pane) else { return };
        let new = observed.map(|obs| {
            let same_kind = p.agent.as_ref().filter(|a| a.kind == obs.kind);
            AgentSession {
                kind: obs.kind,
                // A Codex without a thread yet keeps the session it was resumed into.
                session_id: obs.session_id.clone().or_else(|| same_kind.and_then(|a| a.session_id.clone())),
                args: same_kind.map(|a| a.args.clone()).unwrap_or_default(),
            }
        });
        if p.agent != new {
            p.agent = new;
            self.store.write(vec![StoreOp::UpsertPane(p.clone())]);
        }
        if let Some(h) = self.terminal(pane) {
            h.set_agent(observed.map(|o| o.kind));
        }
        self.sync_resume(pane);
    }

    /// What a pane types once its program starts again (see [`Core::resume_line`]),
    /// for a remote host to work out.
    fn remote_resume(&self, pane: PaneId) -> Resume {
        let p = self.state.pane(pane);
        Resume { agent: p.and_then(|p| p.agent.clone()), command: p.and_then(|p| p.resume_command.clone()) }
    }

    /// Tells a remote terminal what it types should it start anew there.
    fn sync_resume(&self, pane: PaneId) {
        if let Some(h) = self.terminal(pane).filter(|h| h.remote().is_some()) {
            h.set_resume(self.remote_resume(pane));
        }
    }

    /// The programs running in each terminal (below its shell), for asking
    /// before quitting. Terminals with nothing running are left out.
    /// Inspects every process, so it can take a moment.
    pub fn running_programs(&self) -> Vec<(PaneId, Vec<Program>)> {
        let shells: Vec<(PaneId, u32)> =
            self.bg.sessions.lock().unwrap().values().filter_map(|h| Some((h.pane(), h.shell_pid()?))).collect();
        let mut found: Vec<(PaneId, Vec<Program>)> =
            self.bg.detector.lock().unwrap().programs(&shells).into_iter().filter(|(_, n)| !n.is_empty()).collect();
        // Remote ones as their host's last scan saw them.
        let remote: Vec<TerminalHandle> =
            self.bg.sessions.lock().unwrap().values().filter(|h| h.remote().is_some()).cloned().collect();
        found.extend(
            remote.iter().map(|h| (h.pane(), h.activity().programs)).filter(|(_, programs)| !programs.is_empty()),
        );
        found.sort_by_key(|(p, _)| *p);
        found
    }

    /// Leaves a directory that is about to be deleted, such as a git worktree.
    /// Tabs working in it (or below it), in any project, are deleted as
    /// [`Command::DeleteTab`] does. Terminals of other tabs whose shell is in
    /// it restart in their tab's directory when idle; ones running a program
    /// stay, see [`LeftDir::busy`]. Wait with [`LeftDir::wait`] before
    /// deleting the directory: Windows cannot delete a folder that a process
    /// is still working in (other programs: [`detect::processes_in`]).
    /// `host`: the computer the directory is on (`None`: this one); only
    /// that computer's sessions are in it.
    pub fn leave_dir(&mut self, host: Option<&str>, dir: &std::path::Path) -> LeftDir {
        let s = &self.state;
        let on_host =
            |tab: TabId| s.tab(tab).and_then(|t| s.project(t.project)).is_some_and(|p| p.host.as_deref() == host);
        let closed: Vec<TabId> = s
            .tabs
            .iter()
            .filter(|t| on_host(t.id) && git::is_within(&s.tab_working_dir(t), dir))
            .map(|t| t.id)
            .collect();
        let strays: Vec<PaneId> = s
            .panes
            .values()
            .filter(|p| on_host(p.tab) && !closed.contains(&p.tab))
            .filter(|p| p.cwd.as_deref().is_some_and(|c| git::is_within(c, dir)))
            .map(|p| p.id)
            .collect();

        let shell_of = |pane: &PaneId| self.terminal(*pane).filter(|h| h.is_running()).and_then(|h| h.shell_pid());
        let mut stopped: Vec<u32> =
            s.panes.values().filter(|p| closed.contains(&p.tab)).filter_map(|p| shell_of(&p.id)).collect();
        let running_strays: Vec<(PaneId, u32)> = strays.iter().filter_map(|p| Some((*p, shell_of(p)?))).collect();
        let mut idle = detect::idle_shells(&running_strays);
        let remote_strays: Vec<PaneId> = strays
            .iter()
            .copied()
            .filter(|p| self.terminal(*p).is_some_and(|h| h.remote().is_some() && h.is_running()))
            .collect();
        idle.extend(
            remote_strays.iter().copied().filter(|p| self.terminal(*p).is_some_and(|h| h.activity().is_idle())),
        );
        let busy: Vec<PaneId> = running_strays
            .iter()
            .map(|(p, _)| *p)
            .chain(remote_strays.iter().copied())
            .filter(|p| !idle.contains(p) || s.pane(*p).is_some_and(|p| p.agent.is_some()))
            .collect();
        stopped.extend(running_strays.iter().filter(|(p, _)| !busy.contains(p)).map(|(_, pid)| *pid));
        let pids = detect::process_trees(&stopped);

        for tab in &closed {
            if let Err(e) = self.dispatch(Command::DeleteTab(*tab)) {
                tracing::warn!(%tab, error = %e, "could not close a session in a folder being removed");
            }
        }
        // The other terminals forget the directory, and idle ones leave it now.
        for pane in strays.iter().filter(|p| !busy.contains(p)) {
            if let Some(p) = self.state.panes.get_mut(pane)
                && p.cwd.take().is_some()
            {
                self.store.write(vec![StoreOp::UpsertPane(p.clone())]);
            }
            if let Some(handle) = self.terminal(*pane).filter(|h| h.is_running()) {
                handle.print_notice(&format!("─── left {}, which is being removed ───", dir.display()));
                self.start_process(*pane, &handle, false);
            }
        }
        // And so does this process.
        if host.is_none()
            && let (Ok(cwd), Some(parent)) = (std::env::current_dir(), dir.parent())
            && git::is_within(&cwd, dir)
            && let Err(e) = std::env::set_current_dir(parent)
        {
            tracing::warn!(error = %e, "could not leave a folder being removed");
        }
        tracing::info!(dir = %dir.display(), closed = closed.len(), busy = busy.len(), "left a directory");
        LeftDir { closed, busy, pids }
    }

    /// Blocks until all queued database writes are committed.
    pub fn flush(&self) {
        self.store.flush();
    }

    /// Saves the scrollback of every running terminal now and waits for it.
    pub fn save_scrollback_now(&self) {
        self.bg.save_dirty(true);
        self.store.flush();
    }

    /// Applies a command. Changes are persisted before the next command runs.
    pub fn dispatch(&mut self, cmd: Command) -> Result<Outcome> {
        match &cmd {
            // Not the text: people paste passwords and keys into notes.
            Command::SetNoteText { note, text } => tracing::debug!(%note, bytes = text.len(), "dispatch SetNoteText"),
            _ => tracing::debug!(?cmd, "dispatch"),
        }
        let mut ops = Vec::new();
        let result = self.apply(cmd, &mut ops);
        if let Err(e) = &result {
            tracing::debug!(error = %e, "command rejected");
        }
        self.store.write(ops);
        self.refresh_remote_terminals();
        result
    }

    fn alloc(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn apply(&mut self, cmd: Command, ops: &mut Vec<StoreOp>) -> Result<Outcome> {
        use Command as C;
        match cmd {
            C::CreateProject { name, working_dir, shell, host } => {
                let name = non_empty(&name, "project name")?;
                let host = host.map(|h| h.trim().to_owned()).filter(|h| !h.is_empty());
                if let Some(h) = &host {
                    remote::check_host(h).map_err(Error::Invalid)?;
                }
                let id = ProjectId(self.alloc());
                let sort_order = self.state.projects.iter().map(|p| p.sort_order + 1).max().unwrap_or(0);
                let project = Project {
                    id,
                    name,
                    working_dir,
                    shell: shell.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
                    host,
                    sort_order,
                    active_tab: None,
                    group: None,
                    created_at: model::now(),
                };
                ops.push(StoreOp::UpsertProject(project.clone()));
                self.state.projects.push(project);
                self.state.sort();
                self.create_tab(id, None, None, ops)?;
                Ok(Outcome::Project(id))
            }
            C::RenameProject { project, name } => {
                let name = non_empty(&name, "project name")?;
                let p = self.state.project_mut(project).ok_or(Error::NotFound("project"))?;
                p.name = name;
                ops.push(StoreOp::UpsertProject(p.clone()));
                Ok(Outcome::None)
            }
            C::SetProjectDir { project, working_dir } => {
                let p = self.state.project_mut(project).ok_or(Error::NotFound("project"))?;
                p.working_dir = working_dir;
                ops.push(StoreOp::UpsertProject(p.clone()));
                Ok(Outcome::None)
            }
            C::SetProjectShell { project, shell } => {
                let p = self.state.project_mut(project).ok_or(Error::NotFound("project"))?;
                p.shell = shell.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
                ops.push(StoreOp::UpsertProject(p.clone()));
                Ok(Outcome::None)
            }
            C::DeleteProject(project) => {
                let index =
                    self.state.projects.iter().position(|p| p.id == project).ok_or(Error::NotFound("project"))?;
                let tabs: Vec<TabId> = self.state.tabs_of(project).map(|t| t.id).collect();
                for tab in &tabs {
                    self.stop_tab_sessions(*tab);
                }
                self.state.panes.retain(|_, p| !tabs.contains(&p.tab));
                self.state.tabs.retain(|t| t.project != project);
                self.state.tasks.retain(|t| t.project != project);
                self.state.projects.remove(index);
                ops.push(StoreOp::DeleteProject(project));
                self.drop_empty_groups(ops);
                if self.state.selected_project == Some(project) {
                    let next = self
                        .state
                        .projects
                        .get(index)
                        .or_else(|| index.checked_sub(1).and_then(|i| self.state.projects.get(i)))
                        .map(|p| p.id);
                    self.select_project(next, ops);
                }
                Ok(Outcome::None)
            }
            C::MoveProject { project, to_index, group } => {
                if let Some(g) = group {
                    self.state.group(g).ok_or(Error::NotFound("group"))?;
                }
                let mut ids: Vec<ProjectId> = self.state.projects.iter().map(|p| p.id).collect();
                reorder(&mut ids, project, to_index).ok_or(Error::NotFound("project"))?;
                self.arrange(ids, &[(project, group)], None, ops)?;
                Ok(Outcome::None)
            }
            C::SelectProject(project) => {
                self.state.project(project).ok_or(Error::NotFound("project"))?;
                self.select_project(Some(project), ops);
                Ok(Outcome::None)
            }

            C::GroupProjects { project, with } => {
                self.state.project(project).ok_or(Error::NotFound("project"))?;
                let joined = self.state.project(with).ok_or(Error::NotFound("project"))?.group;
                if project == with {
                    return Err(Error::Invalid("a project can't be grouped with itself".into()));
                }
                let mut ids: Vec<ProjectId> =
                    self.state.projects.iter().map(|p| p.id).filter(|p| *p != project).collect();
                let at = ids.iter().position(|p| *p == with).expect("listed") + 1;
                ids.insert(at, project);
                let (group, new) = match joined {
                    Some(g) => (g, None),
                    None => {
                        let id = GroupId(self.alloc());
                        let name = NEW_GROUP_NAME.to_owned();
                        (id, Some(ProjectGroup { id, name, collapsed: false, created_at: model::now() }))
                    }
                };
                let outcome = if new.is_some() { Outcome::Group(group) } else { Outcome::None };
                self.arrange(ids, &[(project, Some(group)), (with, Some(group))], new, ops)?;
                Ok(outcome)
            }
            C::MoveGroup { group, to_index } => {
                self.state.group(group).ok_or(Error::NotFound("group"))?;
                let members: Vec<ProjectId> = self.state.projects_in(group).map(|p| p.id).collect();
                let mut ids: Vec<ProjectId> =
                    self.state.projects.iter().map(|p| p.id).filter(|p| !members.contains(p)).collect();
                let at = to_index.min(ids.len());
                ids.splice(at..at, members);
                self.arrange(ids, &[], None, ops)?;
                Ok(Outcome::None)
            }
            C::RenameGroup { group, name } => {
                let name = non_empty(&name, "group name")?;
                let g = self.state.group_mut(group).ok_or(Error::NotFound("group"))?;
                g.name = name;
                ops.push(StoreOp::UpsertGroup(g.clone()));
                Ok(Outcome::None)
            }
            C::SetGroupCollapsed { group, collapsed } => {
                let g = self.state.group_mut(group).ok_or(Error::NotFound("group"))?;
                if g.collapsed != collapsed {
                    g.collapsed = collapsed;
                    ops.push(StoreOp::UpsertGroup(g.clone()));
                }
                Ok(Outcome::None)
            }

            C::CreateTab { project, title, working_dir } => {
                self.state.project(project).ok_or(Error::NotFound("project"))?;
                let tab = self.create_tab(project, title, working_dir, ops)?;
                Ok(Outcome::Tab(tab))
            }
            C::RenameTab { tab, title } => {
                let title = non_empty(&title, "session name")?;
                let t = self.state.tab_mut(tab).ok_or(Error::NotFound("session"))?;
                t.title = title;
                ops.push(StoreOp::UpsertTab(t.clone()));
                Ok(Outcome::None)
            }
            C::SetTabDir { tab, working_dir } => {
                let t = self.state.tab_mut(tab).ok_or(Error::NotFound("session"))?;
                t.working_dir = working_dir.filter(|d| !d.as_os_str().is_empty());
                ops.push(StoreOp::UpsertTab(t.clone()));
                Ok(Outcome::None)
            }
            C::RelocateTab { tab, working_dir } => {
                let t = self.state.tab(tab).ok_or(Error::NotFound("session"))?;
                if self.state.tab_working_dir(t) == working_dir {
                    return Ok(Outcome::Relocated { left_running: 0 });
                }
                let follows_project = self.state.project(t.project).is_some_and(|p| p.working_dir == working_dir);
                let panes = t.layout.panes();
                let t = self.state.tab_mut(tab).expect("checked");
                t.working_dir = (!follows_project).then(|| working_dir.clone());
                ops.push(StoreOp::UpsertTab(t.clone()));

                // Terminals not started yet, or exited, start in the new directory anyway.
                let running: Vec<(PaneId, TerminalHandle)> = panes
                    .iter()
                    .filter_map(|p| Some((*p, self.terminal(*p)?)))
                    .filter(|(_, h)| h.is_running())
                    .collect();
                let shells: Vec<(PaneId, u32)> =
                    running.iter().filter_map(|(p, h)| Some((*p, h.shell_pid()?))).collect();
                let mut idle = detect::idle_shells(&shells);
                // A remote one as its host's last scan saw it.
                idle.extend(
                    running.iter().filter(|(_, h)| h.remote().is_some() && h.activity().is_idle()).map(|(p, _)| *p),
                );
                let stay: HashSet<PaneId> = running
                    .iter()
                    .map(|(p, _)| *p)
                    .filter(|p| !idle.contains(p) || self.state.pane(*p).is_some_and(|p| p.agent.is_some()))
                    .collect();
                // The others forget where their old shell was.
                for pane in panes.iter().filter(|p| !stay.contains(p)) {
                    if let Some(p) = self.state.panes.get_mut(pane)
                        && p.cwd.take().is_some()
                    {
                        ops.push(StoreOp::UpsertPane(p.clone()));
                    }
                }
                for (pane, handle) in running {
                    if !stay.contains(&pane) {
                        handle.print_notice(&format!("─── moved to {} ───", working_dir.display()));
                        self.start_process(pane, &handle, false);
                    }
                    // Terminals that stay get nothing printed: it would garble a running program's screen.
                }
                let left_running = stay.len();
                tracing::info!(%tab, dir = %working_dir.display(), left_running, "session relocated");
                Ok(Outcome::Relocated { left_running })
            }
            C::DeleteTab(tab) => {
                let project = self.state.tab(tab).ok_or(Error::NotFound("session"))?.project;
                let siblings: Vec<TabId> = self.state.tabs_of(project).map(|t| t.id).collect();
                let index = siblings.iter().position(|t| *t == tab).expect("tab in project");
                self.stop_tab_sessions(tab);
                self.state.panes.retain(|_, p| p.tab != tab);
                for task in self.state.tasks.iter_mut().filter(|t| t.tab == Some(tab)) {
                    task.tab = None; // the database does this via ON DELETE SET NULL
                }
                self.state.tabs.retain(|t| t.id != tab);
                ops.push(StoreOp::DeleteTab(tab));

                let p = self.state.project_mut(project).expect("project exists");
                if p.active_tab == Some(tab) {
                    let next =
                        siblings.get(index + 1).or_else(|| index.checked_sub(1).and_then(|i| siblings.get(i))).copied();
                    p.active_tab = next;
                    ops.push(StoreOp::UpsertProject(p.clone()));
                    if self.state.selected_project == Some(project)
                        && let Some(next) = next
                    {
                        self.activate_tab(next);
                    }
                }
                Ok(Outcome::None)
            }
            C::MoveTab { tab, to_index } => {
                let project = self.state.tab(tab).ok_or(Error::NotFound("session"))?.project;
                let mut ids: Vec<TabId> = self.state.tabs_of(project).map(|t| t.id).collect();
                reorder(&mut ids, tab, to_index).ok_or(Error::NotFound("session"))?;
                for (i, id) in ids.iter().enumerate() {
                    let t = self.state.tab_mut(*id).expect("exists");
                    t.sort_order = i as i64;
                    ops.push(StoreOp::UpsertTab(t.clone()));
                }
                self.state.sort();
                Ok(Outcome::None)
            }
            C::SelectTab(tab) => {
                let project = self.state.tab(tab).ok_or(Error::NotFound("session"))?.project;
                let p = self.state.project_mut(project).expect("project exists");
                if p.active_tab != Some(tab) {
                    p.active_tab = Some(tab);
                    ops.push(StoreOp::UpsertProject(p.clone()));
                }
                if self.state.selected_project != Some(project) {
                    self.state.selected_project = Some(project);
                    ops.push(StoreOp::SelectedProject(Some(project)));
                }
                self.activate_tab(tab);
                Ok(Outcome::None)
            }

            C::SplitPane { pane, axis } => {
                let tab_id = self.state.pane(pane).ok_or(Error::NotFound("pane"))?.tab;
                let new = PaneId(self.alloc());
                let tab = self.state.tab_mut(tab_id).ok_or(Error::NotFound("session"))?;
                if !tab.layout.split(pane, axis, new) {
                    return Err(Error::Invalid("pane is not in its tab layout".into()));
                }
                tab.focused_pane = Some(new);
                ops.push(StoreOp::UpsertTab(tab.clone()));
                let record = Pane {
                    id: new,
                    tab: tab_id,
                    resume_command: None,
                    agent: None,
                    cwd: None,
                    keep_running: false,
                    created_at: model::now(),
                };
                ops.push(StoreOp::UpsertPane(record.clone()));
                self.state.panes.insert(new, record);

                let size = self.terminal(pane).map(|h| h.size()).unwrap_or_default();
                let size = match axis {
                    Axis::Vertical => GridSize { cols: (size.cols / 2).max(2), ..size },
                    Axis::Horizontal => GridSize { rows: (size.rows / 2).max(1), ..size },
                };
                self.ensure_session(new, size);
                Ok(Outcome::Pane(new))
            }
            C::ClosePane(pane) => {
                let tab_id = self.state.pane(pane).ok_or(Error::NotFound("pane"))?.tab;
                let tab = self.state.tab_mut(tab_id).ok_or(Error::NotFound("session"))?;
                if !tab.layout.remove(pane) {
                    return Err(Error::Invalid(
                        "the last terminal of a session cannot be closed; delete the session".into(),
                    ));
                }
                if tab.focused_pane == Some(pane) || !tab.focused_pane.is_some_and(|f| tab.layout.contains(f)) {
                    tab.focused_pane = Some(tab.layout.first_pane());
                }
                ops.push(StoreOp::UpsertTab(tab.clone()));
                ops.push(StoreOp::DeletePane(pane));
                self.state.panes.remove(&pane);
                self.stop_session(pane);
                Ok(Outcome::None)
            }
            C::FocusPane(pane) => {
                let tab_id = self.state.pane(pane).ok_or(Error::NotFound("pane"))?.tab;
                let tab = self.state.tab_mut(tab_id).ok_or(Error::NotFound("session"))?;
                if tab.focused_pane != Some(pane) {
                    tab.focused_pane = Some(pane);
                    ops.push(StoreOp::UpsertTab(tab.clone()));
                }
                Ok(Outcome::None)
            }
            C::SetLayout { tab, mut layout } => {
                let t = self.state.tab_mut(tab).ok_or(Error::NotFound("session"))?;
                let mut a = t.layout.panes();
                let mut b = layout.panes();
                a.sort();
                b.sort();
                if a != b {
                    return Err(Error::Invalid("layout must contain exactly the tab's panes".into()));
                }
                layout.sanitize();
                if t.layout != layout {
                    t.layout = layout;
                    ops.push(StoreOp::UpsertTab(t.clone()));
                }
                Ok(Outcome::None)
            }
            C::SetResumeCommand { pane, command } => {
                let p = self.state.panes.get_mut(&pane).ok_or(Error::NotFound("pane"))?;
                p.resume_command = command.map(|c| c.trim().to_owned()).filter(|c| !c.is_empty());
                ops.push(StoreOp::UpsertPane(p.clone()));
                self.sync_resume(pane);
                Ok(Outcome::None)
            }
            C::StartAgent { pane, kind, args } => {
                self.state.pane(pane).ok_or(Error::NotFound("pane"))?;
                let handle = self.ensure_session(pane, GridSize::default());
                let fresh = !handle.is_running();
                if fresh {
                    self.start_process(pane, &handle, false);
                }
                let session = AgentSession::new(kind, args);
                tracing::info!(%pane, agent = kind.as_str(), session = ?session.session_id, "starting agent");
                let line = session.launch_command();
                if fresh {
                    type_later(&handle, line)
                } else {
                    handle.write(format!("{line}\r"))
                }
                handle.set_agent(Some(kind));
                let p = self.state.panes.get_mut(&pane).expect("checked");
                p.agent = Some(session);
                ops.push(StoreOp::UpsertPane(p.clone()));
                self.sync_resume(pane);
                Ok(Outcome::None)
            }
            C::ClearAgent(pane) => {
                let p = self.state.panes.get_mut(&pane).ok_or(Error::NotFound("pane"))?;
                p.agent = None;
                ops.push(StoreOp::UpsertPane(p.clone()));
                if let Some(h) = self.terminal(pane) {
                    h.set_agent(None);
                }
                self.sync_resume(pane);
                Ok(Outcome::None)
            }
            C::SetKeepRunning { pane, on } => {
                let p = self.state.panes.get_mut(&pane).ok_or(Error::NotFound("pane"))?;
                if p.keep_running == on {
                    return Ok(Outcome::None);
                }
                p.keep_running = on;
                ops.push(StoreOp::UpsertPane(p.clone()));
                tracing::info!(%pane, on, "keep running changed");
                if let Some(handle) = self.terminal(pane).filter(|h| h.remote().is_some()) {
                    // It runs under a helper there either way; quitting ends it or not.
                    handle.print_notice(if on {
                        "─── keeps running when Terminal Rooster quits ───"
                    } else {
                        "─── stops when Terminal Rooster quits ───"
                    });
                } else if let Some(handle) = self.terminal(pane) {
                    // Shown elsewhere, the helper isn't this process's to end.
                    if handle.is_shown_elsewhere() {
                        self.end_helper(pane);
                    }
                    handle.print_notice(if on {
                        "─── keeps running when Terminal Rooster quits ───"
                    } else {
                        "─── stops when Terminal Rooster quits ───"
                    });
                    self.start_process(pane, &handle, true);
                } else if !on && self.remote_of(pane).is_none() {
                    // Its session isn't open: the helper from an earlier run.
                    self.end_helper(pane);
                }
                Ok(Outcome::None)
            }
            C::RestartPane(pane) => {
                self.state.pane(pane).ok_or(Error::NotFound("pane"))?;
                match self.terminal(pane) {
                    Some(handle) => {
                        handle.print_notice("─── restarted ───");
                        self.start_process(pane, &handle, true);
                    }
                    None => {
                        self.ensure_session(pane, GridSize::default());
                    }
                }
                Ok(Outcome::None)
            }

            C::CreateTask { project, title, tab } => {
                let title = non_empty(&title, "task title")?;
                self.state.project(project).ok_or(Error::NotFound("project"))?;
                self.check_tab_in_project(tab, project)?;
                let id = TaskId(self.alloc());
                let sort_order = self.state.tasks_of(project).map(|t| t.sort_order + 1).max().unwrap_or(0);
                let task = Task {
                    id,
                    project,
                    tab,
                    title,
                    notes: String::new(),
                    done: false,
                    sort_order,
                    created_at: model::now(),
                    completed_at: None,
                };
                ops.push(StoreOp::UpsertTask(task.clone()));
                self.state.tasks.push(task);
                self.state.sort();
                Ok(Outcome::Task(id))
            }
            C::EditTask { task, title, notes } => {
                let title = non_empty(&title, "task title")?;
                let t = self.state.task_mut(task).ok_or(Error::NotFound("task"))?;
                t.title = title;
                t.notes = notes;
                ops.push(StoreOp::UpsertTask(t.clone()));
                Ok(Outcome::None)
            }
            C::SetTaskDone { task, done } => {
                let t = self.state.task_mut(task).ok_or(Error::NotFound("task"))?;
                t.done = done;
                t.completed_at = done.then(model::now);
                ops.push(StoreOp::UpsertTask(t.clone()));
                Ok(Outcome::None)
            }
            C::AssignTask { task, tab } => {
                let project = self.state.task(task).ok_or(Error::NotFound("task"))?.project;
                self.check_tab_in_project(tab, project)?;
                let t = self.state.task_mut(task).expect("checked");
                t.tab = tab;
                ops.push(StoreOp::UpsertTask(t.clone()));
                Ok(Outcome::None)
            }
            C::MoveTask { task, to_index } => {
                let project = self.state.task(task).ok_or(Error::NotFound("task"))?.project;
                let mut ids: Vec<TaskId> = self.state.tasks_of(project).map(|t| t.id).collect();
                reorder(&mut ids, task, to_index).ok_or(Error::NotFound("task"))?;
                for (i, id) in ids.iter().enumerate() {
                    let t = self.state.task_mut(*id).expect("exists");
                    t.sort_order = i as i64;
                    ops.push(StoreOp::UpsertTask(t.clone()));
                }
                self.state.sort();
                Ok(Outcome::None)
            }
            C::DeleteTask(task) => {
                let before = self.state.tasks.len();
                self.state.tasks.retain(|t| t.id != task);
                if self.state.tasks.len() == before {
                    return Err(Error::NotFound("task"));
                }
                ops.push(StoreOp::DeleteTask(task));
                Ok(Outcome::None)
            }

            C::CreateNote { title } => {
                let title = title
                    .map(|t| t.trim().to_owned())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| new_note_title(&self.state.notes));
                let id = NoteId(self.alloc());
                let sort_order = self.state.notes.iter().map(|n| n.sort_order + 1).max().unwrap_or(0);
                let note = Note { id, title, text: String::new(), sort_order, created_at: model::now() };
                ops.push(StoreOp::UpsertNote(note.clone()));
                self.state.notes.push(note);
                self.state.sort();
                Ok(Outcome::Note(id))
            }
            C::RenameNote { note, title } => {
                let title = non_empty(&title, "note name")?;
                let n = self.state.note_mut(note).ok_or(Error::NotFound("note"))?;
                n.title = title;
                ops.push(StoreOp::UpsertNote(n.clone()));
                Ok(Outcome::None)
            }
            C::SetNoteText { note, text } => {
                let n = self.state.note_mut(note).ok_or(Error::NotFound("note"))?;
                if n.text != text {
                    n.text = text;
                    ops.push(StoreOp::UpsertNote(n.clone()));
                }
                Ok(Outcome::None)
            }
            C::CloseNote(note) => {
                let before = self.state.notes.len();
                self.state.notes.retain(|n| n.id != note);
                if self.state.notes.len() == before {
                    return Err(Error::NotFound("note"));
                }
                ops.push(StoreOp::DeleteNote(note));
                Ok(Outcome::None)
            }
            C::CloseAllNotes => {
                ops.extend(self.state.notes.drain(..).map(|n| StoreOp::DeleteNote(n.id)));
                Ok(Outcome::None)
            }

            C::SetDefaultShell(shell) => {
                self.state.settings.default_shell = shell.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
                ops.push(StoreOp::Settings(self.state.settings.clone()));
                Ok(Outcome::None)
            }
            C::SetShellIntegration(on) => {
                if self.state.settings.shell_integration != on {
                    self.state.settings.shell_integration = on;
                    ops.push(StoreOp::Settings(self.state.settings.clone()));
                }
                Ok(Outcome::None)
            }
            C::SetGuiValue { key, value } => {
                if self.state.gui.get(&key) != Some(&value) {
                    ops.push(StoreOp::Gui(key.clone(), value.clone()));
                    self.state.gui.insert(key, value);
                }
                Ok(Outcome::None)
            }
        }
    }

    fn check_tab_in_project(&self, tab: Option<TabId>, project: ProjectId) -> Result<()> {
        match tab {
            Some(t) if self.state.tab(t).map(|t| t.project) != Some(project) => {
                Err(Error::Invalid("tab belongs to another project".into()))
            }
            _ => Ok(()),
        }
    }

    fn create_tab(
        &mut self,
        project: ProjectId,
        title: Option<String>,
        working_dir: Option<PathBuf>,
        ops: &mut Vec<StoreOp>,
    ) -> Result<TabId> {
        let project_dir = &self.state.project(project).ok_or(Error::NotFound("project"))?.working_dir;
        // The project's own directory is stored as "follow the project".
        let working_dir = working_dir.filter(|d| !d.as_os_str().is_empty() && d != project_dir);
        let tab_id = TabId(self.alloc());
        let pane_id = PaneId(self.alloc());
        let count = self.state.tabs_of(project).count();
        let sort_order = self.state.tabs_of(project).map(|t| t.sort_order + 1).max().unwrap_or(0);
        let title = title
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("Session {}", count + 1));
        let tab = Tab {
            id: tab_id,
            project,
            title,
            working_dir,
            layout: Layout::Pane(pane_id),
            focused_pane: Some(pane_id),
            sort_order,
            created_at: model::now(),
        };
        let pane = Pane {
            id: pane_id,
            tab: tab_id,
            resume_command: None,
            agent: None,
            cwd: None,
            keep_running: false,
            created_at: model::now(),
        };
        ops.push(StoreOp::UpsertTab(tab.clone()));
        ops.push(StoreOp::UpsertPane(pane.clone()));
        self.state.tabs.push(tab);
        self.state.panes.insert(pane_id, pane);
        self.state.sort();

        let p = self.state.project_mut(project).ok_or(Error::NotFound("project"))?;
        p.active_tab = Some(tab_id);
        ops.push(StoreOp::UpsertProject(p.clone()));
        if self.state.selected_project != Some(project) {
            self.state.selected_project = Some(project);
            ops.push(StoreOp::SelectedProject(Some(project)));
        }
        self.activate_tab(tab_id);
        Ok(tab_id)
    }

    fn select_project(&mut self, project: Option<ProjectId>, ops: &mut Vec<StoreOp>) {
        if self.state.selected_project != project {
            self.state.selected_project = project;
            ops.push(StoreOp::SelectedProject(project));
        }
        if let Some(tab) = self.state.active_tab().map(|t| t.id) {
            self.activate_tab(tab);
        }
    }

    /// Puts the projects in the order of `ids` (all of them), moving those
    /// in `regroup` into the given groups, and creates `new` first if some
    /// join it. Refused, with nothing changed, if a group's projects wouldn't
    /// be together. Groups left without projects are deleted.
    fn arrange(
        &mut self,
        ids: Vec<ProjectId>,
        regroup: &[(ProjectId, Option<GroupId>)],
        new: Option<ProjectGroup>,
        ops: &mut Vec<StoreOp>,
    ) -> Result<()> {
        let group_of = |id: ProjectId| match regroup.iter().find(|(p, _)| *p == id) {
            Some((_, g)) => *g,
            None => self.state.project(id).and_then(|p| p.group),
        };
        let groups: Vec<Option<GroupId>> = ids.iter().map(|p| group_of(*p)).collect();
        if !together(&groups) {
            return Err(Error::Invalid("a group's projects have to stay together".into()));
        }
        if let Some(g) = new {
            // Saved before the projects that refer to it.
            ops.push(StoreOp::UpsertGroup(g.clone()));
            self.state.groups.push(g);
        }
        for (i, (id, group)) in ids.iter().zip(groups).enumerate() {
            let p = self.state.project_mut(*id).expect("exists");
            if p.sort_order != i as i64 || p.group != group {
                p.sort_order = i as i64;
                p.group = group;
                ops.push(StoreOp::UpsertProject(p.clone()));
            }
        }
        self.drop_empty_groups(ops);
        self.state.sort();
        Ok(())
    }

    fn drop_empty_groups(&mut self, ops: &mut Vec<StoreOp>) {
        let projects = &self.state.projects;
        self.state.groups.retain(|g| {
            let used = projects.iter().any(|p| p.group == Some(g.id));
            if !used {
                ops.push(StoreOp::DeleteGroup(g.id));
            }
            used
        });
    }

    /// Starts the terminals of a tab that are not running yet.
    fn activate_tab(&mut self, tab: TabId) {
        let Some(panes) = self.state.tab(tab).map(|t| t.layout.panes()) else { return };
        for pane in panes {
            self.ensure_session(pane, GridSize::default());
        }
    }

    /// Returns the pane's terminal, creating and starting it if needed.
    fn ensure_session(&mut self, pane: PaneId, size: GridSize) -> TerminalHandle {
        if let Some(h) = self.terminal(pane) {
            return h;
        }
        let handle = TerminalHandle::new(
            pane,
            self.bg.events.clone(),
            self.palette.clone(),
            self.state.settings.scrollback_lines,
            size,
            self.bg.ptys.clone(),
        );
        let restored = self.restored.remove(&pane);
        // A coding agent shows its conversation again when it resumes, so its
        // terminal starts clean instead of under a copy of its old screen.
        let had_agent = self.state.pane(pane).is_some_and(|p| p.agent.is_some());
        if restored
            && !had_agent
            && let Some((data, _)) = self.store.load_scrollback(pane)
            && !session::just_a_prompt(&data)
        {
            handle.replay(&session::without_restore_banners(&data));
        }
        handle.set_agent(self.state.pane(pane).and_then(|p| p.agent.as_ref()).map(|a| a.kind));
        if let Some(host) = self.remote_of(pane) {
            // Before it is listed, so no scan takes its shell for one here.
            handle.set_remote(host.clone());
            self.bg.sessions.lock().unwrap().insert(pane, handle.clone());
            // One that keeps running is shown as it is, if its helper runs on
            // there. Others start anew, as here: one left running there (as
            // Terminal Rooster quit or crashed without reaching the host) ends.
            let keeps = self.state.pane(pane).is_some_and(|p| p.keep_running);
            let mode = if restored && keeps { OpenMode::ExistingOrNew } else { OpenMode::New };
            self.attach_remote(pane, &handle, host, mode, restored);
            return handle;
        }
        self.bg.sessions.lock().unwrap().insert(pane, handle.clone());
        // A helper that kept the terminal running while TerminalRooster was closed.
        let running = self
            .state
            .pane(pane)
            .is_some_and(|p| p.keep_running)
            .then(|| host::proto::conn_file(&self.data_dir, pane))
            .and_then(|file| host::proto::read_conn(&file).map(|info| (file, info)));
        match running {
            Some(existing) => self.attach_running(pane, &handle, existing, restored),
            None => self.start_process(pane, &handle, restored),
        }
        handle
    }

    /// Shows a remote pane's terminal: its program runs on the host, under
    /// a helper found there (`mode`) or started. `resume`: a new one types
    /// the pane's resume command, as in [`Core::start_process`].
    fn attach_remote(&self, pane: PaneId, handle: &TerminalHandle, host: RemoteHost, mode: OpenMode, resume: bool) {
        let Some(record) = self.state.pane(pane) else { return };
        let Some(tab) = self.state.tab(record.tab) else { return };
        let Some(project) = self.state.project(tab.project) else { return };
        let mut env = base_env();
        env.insert("TERMINALROOSTER_PANE_ID".to_owned(), pane.to_string());
        env.insert("TERMINALROOSTER_PROJECT".to_owned(), project.name.clone());
        let settings = &self.state.settings;
        let preload = handle
            .snapshot(settings.saved_scrollback_lines)
            .filter(|d| !d.is_empty())
            .map(|d| String::from_utf8_lossy(&d).into_owned());
        let start = StartSpec {
            shell: project.shell.clone(),
            // Where the shell last was, else the session's folder.
            cwd: record.cwd.iter().cloned().chain([self.state.tab_working_dir(tab)]).collect(),
            env,
            shell_integration: settings.shell_integration,
            scrollback_lines: settings.scrollback_lines,
            preload,
            resume: if resume { self.remote_resume(pane) } else { Resume::default() },
        };
        tracing::info!(%pane, host = host.name(), ?mode, resume, "showing a remote terminal");
        handle.set_resume(self.remote_resume(pane));
        let open = OpenTerminal { pane, mode, size: handle.size(), start };
        handle.attach_remote(RemotePlan { host, open, preload_lines: settings.saved_scrollback_lines });
    }

    /// Whether the pane's program runs under a helper of its own (see [`host`]).
    fn keeps_running(&self, pane: PaneId) -> bool {
        self.host.is_some() && self.state.pane(pane).is_some_and(|p| p.keep_running)
    }

    /// Shows a terminal that a helper kept running, or starts it anew should
    /// the helper be gone. `restored`: as in [`Core::start_process`].
    fn attach_running(
        &mut self,
        pane: PaneId,
        handle: &TerminalHandle,
        existing: (PathBuf, host::proto::ConnInfo),
        restored: bool,
    ) {
        tracing::info!(%pane, helper = existing.1.pid, "showing a terminal that kept running");
        let line = if restored { self.resume_line(pane) } else { None };
        let start = self.process_spec(pane, handle).and_then(|spec| self.new_host(pane, &spec, handle, line));
        handle.attach_host(HostPlan { existing: Some(existing), steal: false, start });
    }

    /// How to start a helper for the pane's program, if it keeps running when
    /// TerminalRooster quits. What the terminal shows now stays above the
    /// program's output; `then_type` is typed once the program runs.
    fn new_host(
        &self,
        pane: PaneId,
        spec: &SpawnSpec,
        handle: &TerminalHandle,
        then_type: Option<String>,
    ) -> Option<NewHost> {
        if !self.keeps_running(pane) {
            return None;
        }
        let command = self.host.clone()?;
        let dir = host::proto::hosts_dir(&self.data_dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "no folder for terminal helpers; the terminal won't keep running");
            return None;
        }
        let preload = handle
            .snapshot(self.state.settings.saved_scrollback_lines)
            .filter(|d| !d.is_empty())
            .map(|d| String::from_utf8_lossy(&d).into_owned());
        Some(NewHost {
            command,
            cwd: dir,
            then_type,
            spec: host::proto::Spec {
                pane,
                conn_file: host::proto::conn_file(&self.data_dir, pane),
                token: host::proto::new_token(),
                program: spec.shell.program.clone(),
                args: spec.shell.args.clone(),
                cwd: spec.cwd.clone(),
                env: spec.env.clone(),
                size: handle.size(),
                scrollback_lines: self.state.settings.scrollback_lines,
                preload,
            },
        })
    }

    /// Ends the helper of a pane, if one runs, whether this process shows
    /// its terminal or not (it may be shown elsewhere, or not shown yet).
    fn end_helper(&self, pane: PaneId) {
        let file = host::proto::conn_file(&self.data_dir, pane);
        let Some(info) = host::proto::read_conn(&file) else { return };
        tracing::info!(%pane, helper = info.pid, "ending the terminal's helper");
        std::thread::spawn(move || {
            if let Err(e) = host::client::end(&info) {
                tracing::debug!(%pane, error = %e, "could not end the terminal's helper (gone already?)");
                host::proto::remove_conn_of(&file, info.pid);
            }
        });
    }

    /// How helper processes are started, if they can be.
    pub fn host_command(&self) -> Option<&HostCommand> {
        self.host.as_ref()
    }

    /// Shows a terminal here again that is shown elsewhere (see
    /// [`Event::ShownElsewhere`]). The other client lets go.
    pub fn bring_back(&mut self, pane: PaneId) {
        let Some(handle) = self.terminal(pane) else { return };
        if handle.take_back() {
            tracing::info!(%pane, "bringing a terminal back");
            return;
        }
        let file = host::proto::conn_file(&self.data_dir, pane);
        let existing = host::proto::read_conn(&file).map(|info| (file, info));
        tracing::info!(%pane, found = existing.is_some(), "bringing a terminal back");
        handle.attach_host(HostPlan { existing, steal: true, start: None });
    }

    /// Ends the helpers of terminals that no longer keep running (their
    /// pane was deleted, or switched over while TerminalRooster was down).
    fn end_stray_helpers(&self) {
        let Ok(entries) = std::fs::read_dir(host::proto::hosts_dir(&self.data_dir)) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.ends_with(".tmp") {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let Some(id) = name.strip_suffix(".json").and_then(|id| id.parse().ok()) else { continue };
            let pane = PaneId(id);
            // Not `keeps_running`: without a way to start helpers, those
            // that run are still shown.
            if !self.state.pane(pane).is_some_and(|p| p.keep_running) {
                tracing::info!(%pane, "a terminal helper runs for a terminal that doesn't keep running");
                self.end_helper(pane);
            }
        }
    }

    fn spawn_spec(&self, pane: PaneId) -> Option<SpawnSpec> {
        let record = self.state.pane(pane)?;
        let tab = self.state.tab(record.tab)?;
        let project = self.state.project(tab.project)?;
        let mut shell = session::shell::resolve(project.shell.as_deref(), self.state.settings.default_shell.as_deref());
        // Where the shell last was, unless that directory is gone.
        let cwd = record.cwd.clone().filter(|d| d.is_dir()).unwrap_or_else(|| self.state.tab_working_dir(tab));
        let mut env = base_env();
        env.insert("TERMINALROOSTER_PANE_ID".to_owned(), pane.to_string());
        env.insert("TERMINALROOSTER_PROJECT".to_owned(), project.name.clone());
        if self.state.settings.shell_integration
            && let Some(dir) = &self.integration
        {
            session::integration::apply(&mut shell, &mut env, dir, pane);
        }
        Some(SpawnSpec { shell, cwd, env })
    }

    /// Opens a terminal that belongs to no project: it is not saved, not
    /// restored and not checked for agents. `command` is typed into its shell
    /// once it has started (e.g. `codex`). Close it with [`Core::close_scratch_terminal`].
    pub fn open_scratch_terminal(&mut self, cwd: &std::path::Path, command: Option<&str>) -> TerminalHandle {
        let handle = self.new_scratch(GridSize::default());
        let pane = handle.pane();
        let spec = SpawnSpec {
            shell: session::shell::resolve(None, self.state.settings.default_shell.as_deref()),
            cwd: cwd.to_path_buf(),
            env: base_env(),
        };
        match handle.spawn(&spec) {
            Ok(()) => {
                if let Some(command) = command {
                    type_later(&handle, command.to_owned());
                }
            }
            Err(e) => handle.print_notice(&format!("failed to start {}: {e}", spec.shell.program)),
        }
        self.scratch.insert(pane, handle.clone());
        handle
    }

    /// Runs `command` itself, without a shell, in a terminal that belongs to
    /// no project (as [`Core::open_scratch_terminal`] does), with `env` added
    /// to the usual environment. Programs such as an editor end the terminal
    /// when they quit: [`Event::ProcessExited`] says so, and then the terminal
    /// is closed with [`Core::close_scratch_terminal`].
    pub fn open_program_terminal(
        &mut self,
        cwd: &std::path::Path,
        command: session::shell::ShellCommand,
        env: Vec<(String, String)>,
        size: GridSize,
    ) -> Result<TerminalHandle> {
        let handle = self.new_scratch(size);
        let mut all = base_env();
        all.extend(env);
        tracing::info!(pane = %handle.pane(), program = %command.program, cwd = %cwd.display(), "starting program");
        handle.spawn(&SpawnSpec { shell: command, cwd: cwd.to_path_buf(), env: all })?;
        self.scratch.insert(handle.pane(), handle.clone());
        Ok(handle)
    }

    /// A terminal for [`Core::open_scratch_terminal`] and the like, with nothing running yet.
    fn new_scratch(&mut self, size: GridSize) -> TerminalHandle {
        let pane = PaneId(-self.next_scratch);
        self.next_scratch += 1;
        TerminalHandle::new(
            pane,
            self.bg.events.clone(),
            self.palette.clone(),
            self.state.settings.scrollback_lines,
            size,
            self.bg.ptys.clone(),
        )
    }

    /// Stops a terminal opened with [`Core::open_scratch_terminal`].
    pub fn close_scratch_terminal(&mut self, pane: PaneId) {
        if let Some(h) = self.scratch.remove(&pane) {
            h.shutdown();
        }
    }

    /// Starts the pane's shell, ending what ran in the terminal before.
    /// `resume`: types the pane's resume command (its agent's, say) once
    /// the shell runs.
    fn start_process(&mut self, pane: PaneId, handle: &TerminalHandle, resume: bool) {
        if let Some(host) = handle.remote().cloned() {
            // Ends the program there, then starts it anew.
            handle.shutdown();
            self.attach_remote(pane, handle, host, OpenMode::New, resume);
            return;
        }
        let Some(spec) = self.process_spec(pane, handle) else { return };
        let line = if resume { self.resume_line(pane) } else { None };
        if let Some(new) = self.new_host(pane, &spec, handle, line.clone()) {
            tracing::info!(%pane, shell = %spec.shell.program, cwd = %spec.cwd.display(), resume, "starting shell under a helper");
            handle.shutdown();
            handle.attach_host(HostPlan { existing: None, steal: false, start: Some(new) });
            return;
        }
        tracing::info!(%pane, shell = %spec.shell.program, cwd = %spec.cwd.display(), resume, "starting shell");
        if let Err(e) = handle.spawn(&spec) {
            tracing::error!(%pane, shell = %spec.shell.program, error = %e, "failed to start shell");
            handle.print_notice(&format!("failed to start {}: {e}", spec.shell.program));
            return;
        }
        if let Some(line) = line {
            tracing::debug!(%pane, command = %line, "typing resume command");
            type_later(handle, line);
        }
    }

    /// What to run in the pane, in a directory that exists.
    fn process_spec(&self, pane: PaneId, handle: &TerminalHandle) -> Option<SpawnSpec> {
        let mut spec = self.spawn_spec(pane)?;
        if !spec.cwd.is_dir() {
            let fallback = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()).unwrap_or_default();
            handle.print_notice(&format!(
                "working directory {} does not exist; starting in {}",
                spec.cwd.display(),
                fallback.display()
            ));
            spec.cwd = fallback;
        }
        Some(spec)
    }

    /// What to type into a restored terminal: the agent's exact session when it
    /// is saved on disk, a fresh start of the agent when it is not (the session
    /// never got a message), else the pane's own resume command.
    fn resume_line(&self, pane: PaneId) -> Option<String> {
        let p = self.state.pane(pane)?;
        match &p.agent {
            Some(a) => Some(match &a.session_id {
                Some(id) if self.homes.session_exists(a.kind, id) => a.resume_command()?,
                _ => AgentSession { session_id: None, ..a.clone() }.launch_command(),
            }),
            None => p.resume_command.clone(),
        }
    }

    fn stop_session(&mut self, pane: PaneId) {
        let handle = self.bg.sessions.lock().unwrap().remove(&pane);
        if let Some(h) = &handle {
            h.shutdown();
        }
        let remote = handle.as_ref().and_then(|h| h.remote().cloned()).or_else(|| self.remote_of(pane));
        if let Some(host) = remote {
            // Also when it wasn't shown in this run: its helper there may run.
            std::thread::spawn(move || {
                let _ = host.call_if_up::<()>(Request::EndTerminals(vec![pane]), Duration::from_secs(10));
            });
        } else if handle.is_none_or(|h| h.is_shown_elsewhere()) {
            // A helper this process doesn't show is ended through its file.
            self.end_helper(pane);
        }
        self.restored.remove(&pane);
    }

    fn stop_tab_sessions(&mut self, tab: TabId) {
        let panes: Vec<PaneId> = self.state.panes.values().filter(|p| p.tab == tab).map(|p| p.id).collect();
        for pane in panes {
            self.stop_session(pane);
        }
    }
}

impl Core {
    /// Saves all scrollback, commits pending writes and stops every terminal.
    /// Idempotent; also run on drop. The core should not be used afterwards.
    pub fn shutdown(&mut self) {
        let Some((stop, thread)) = self.autosave.take() else { return };
        tracing::info!(terminals = self.bg.sessions.lock().unwrap().len(), "shutting down; saving scrollback");
        let _ = stop.send(());
        let _ = thread.join();
        // Last look at what runs where, while the agents are still alive.
        for (pane, agent) in self.bg.scan_terminals() {
            self.record_agent(pane, agent.as_ref());
        }
        codex_hook::set_running(&self.data_dir, false);
        self.bg.save_dirty(true);
        self.store.flush();
        // Terminals under helpers run on; the next start shows them again.
        // Remote ones run under helpers anyway: those that don't keep
        // running end now.
        let mut ending: HashMap<String, (RemoteHost, Vec<PaneId>)> = HashMap::new();
        for h in self.bg.sessions.lock().unwrap().drain().map(|(_, h)| h) {
            match h.remote() {
                Some(host) if !self.state.pane(h.pane()).is_some_and(|p| p.keep_running) => {
                    h.shutdown();
                    ending.entry(host.name().to_owned()).or_insert_with(|| (host.clone(), Vec::new())).1.push(h.pane());
                }
                _ => h.detach(),
            }
        }
        for (host, panes) in ending.into_values() {
            let _ = host.call_if_up::<()>(Request::EndTerminals(panes), Duration::from_secs(3));
        }
        for host in self.remotes.all() {
            host.disconnect();
        }
        for (_, h) in self.scratch.drain() {
            h.shutdown();
        }
        // Their threads close the terminals, and end shells closing left
        // running; this process may exit right after.
        let deadline = Instant::now() + PTY_CLOSE_WAIT;
        while self.bg.ptys.load(std::sync::atomic::Ordering::Acquire) > 0 {
            if Instant::now() >= deadline {
                tracing::warn!("terminals still closing at shutdown");
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// How long [`Core::shutdown`] waits for the terminals to close, longer
/// than a closed terminal's shell gets to exit.
const PTY_CLOSE_WAIT: Duration = Duration::from_secs(7);

impl Drop for Core {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Identifies this Terminal Rooster to remote hosts (see
/// [`remote::wire::ClientHello::client`]): made once, kept in the data directory.
fn client_id(data_dir: &std::path::Path) -> String {
    let file = data_dir.join("remote-client-id");
    if let Ok(id) = std::fs::read_to_string(&file)
        && !id.trim().is_empty()
    {
        return id.trim().to_owned();
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    if let Err(e) = std::fs::write(&file, &id) {
        tracing::warn!(error = %e, "could not save this computer's id for remote hosts");
    }
    id
}

/// Environment every terminal gets.
fn base_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    #[cfg(not(windows))]
    env.insert("TERM".to_owned(), "xterm-256color".to_owned());
    env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
    env.insert("TERM_PROGRAM".to_owned(), "terminalrooster".to_owned());
    env
}

/// Types a line into a freshly started shell once it had a moment to start.
fn type_later(handle: &TerminalHandle, line: String) {
    let handle = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        handle.write(format!("{line}\r"));
    });
}

fn non_empty(s: &str, what: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() { Err(Error::Invalid(format!("{what} cannot be empty"))) } else { Ok(s.to_owned()) }
}

fn reorder<T: PartialEq + Copy>(ids: &mut Vec<T>, id: T, to: usize) -> Option<()> {
    let from = ids.iter().position(|x| *x == id)?;
    let item = ids.remove(from);
    ids.insert(to.min(ids.len()), item);
    Some(())
}

/// Whether each group's projects are next to each other, given the group of
/// each project in list order.
fn together(groups: &[Option<GroupId>]) -> bool {
    let mut passed = HashSet::new();
    let mut current = None;
    for &g in groups {
        if g != current {
            passed.extend(current);
            if g.is_some_and(|g| passed.contains(&g)) {
                return false;
            }
            current = g;
        }
    }
    true
}

fn max_id(s: &AppState) -> i64 {
    let p = s.projects.iter().map(|x| x.id.0);
    let g = s.groups.iter().map(|x| x.id.0);
    let t = s.tabs.iter().map(|x| x.id.0);
    let pa = s.panes.keys().map(|x| x.0);
    let ta = s.tasks.iter().map(|x| x.id.0);
    let n = s.notes.iter().map(|x| x.id.0);
    p.chain(g).chain(t).chain(pa).chain(ta).chain(n).max().unwrap_or(0)
}

/// "Note N", with the lowest N none of `notes` is named.
fn new_note_title(notes: &[Note]) -> String {
    let taken: HashSet<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    (1..).map(|n| format!("Note {n}")).find(|t| !taken.contains(t.as_str())).expect("a free number")
}

/// Fixes saved state that does not hang together (e.g. after a crash between
/// writes or a manual database edit). Returns the writes that persist the fixes.
fn repair(state: &mut AppState, next_id: &mut i64) -> Vec<StoreOp> {
    let mut ops = Vec::new();
    let project_ids: HashSet<ProjectId> = state.projects.iter().map(|p| p.id).collect();
    state.tabs.retain(|t| project_ids.contains(&t.project));
    let tab_ids: HashSet<TabId> = state.tabs.iter().map(|t| t.id).collect();
    state.panes.retain(|_, p| tab_ids.contains(&p.tab));

    for tab in &mut state.tabs {
        let mut owned: Vec<PaneId> = state.panes.values().filter(|p| p.tab == tab.id).map(|p| p.id).collect();
        owned.sort();
        let before = tab.clone();
        let leaves = tab.layout.panes();
        let unique: HashSet<PaneId> = leaves.iter().copied().collect();
        let valid = unique.len() == leaves.len() && leaves.iter().all(|l| owned.contains(l));
        if !valid {
            if let Some(l) = Layout::chain(&owned) {
                tab.layout = l;
            }
        } else {
            for p in owned.iter().filter(|p| !unique.contains(p)) {
                let last = *tab.layout.panes().last().expect("non-empty");
                tab.layout.split(last, Axis::Vertical, *p);
            }
        }
        if owned.is_empty() {
            let id = PaneId(*next_id);
            *next_id += 1;
            let pane = Pane {
                id,
                tab: tab.id,
                resume_command: None,
                agent: None,
                cwd: None,
                keep_running: false,
                created_at: model::now(),
            };
            state.panes.insert(id, pane.clone());
            tab.layout = Layout::Pane(id);
            ops.push(StoreOp::UpsertTab(tab.clone()));
            ops.push(StoreOp::UpsertPane(pane));
        }
        tab.layout.sanitize();
        if !tab.focused_pane.is_some_and(|f| tab.layout.contains(f)) {
            tab.focused_pane = Some(tab.layout.first_pane());
        }
        if *tab != before {
            ops.push(StoreOp::UpsertTab(tab.clone()));
        }
    }

    for project in &mut state.projects {
        let valid = project.active_tab.is_some_and(|t| state.tabs.iter().any(|x| x.id == t && x.project == project.id));
        if !valid {
            let first = state.tabs.iter().find(|t| t.project == project.id).map(|t| t.id);
            if first != project.active_tab {
                project.active_tab = first;
                ops.push(StoreOp::UpsertProject(project.clone()));
            }
        }
    }

    for task in &mut state.tasks {
        if let Some(tab) = task.tab
            && !state.tabs.iter().any(|t| t.id == tab && t.project == task.project)
        {
            task.tab = None;
            ops.push(StoreOp::UpsertTask(task.clone()));
        }
    }

    let group_ids: HashSet<GroupId> = state.groups.iter().map(|g| g.id).collect();
    for project in &mut state.projects {
        if project.group.is_some_and(|g| !group_ids.contains(&g)) {
            project.group = None;
            ops.push(StoreOp::UpsertProject(project.clone()));
        }
    }
    let projects = &state.projects;
    state.groups.retain(|g| {
        let used = projects.iter().any(|p| p.group == Some(g.id));
        if !used {
            ops.push(StoreOp::DeleteGroup(g.id));
        }
        used
    });
    state.sort();

    let selected_ok = state.selected_project.is_some_and(|p| project_ids.contains(&p));
    if !selected_ok {
        let first = state.projects.first().map(|p| p.id);
        if first != state.selected_project {
            state.selected_project = first;
            ops.push(StoreOp::SelectedProject(first));
        }
    }
    ops
}
