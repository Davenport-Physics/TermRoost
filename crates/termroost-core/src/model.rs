//! Plain data types describing everything TerminalRooster persists.
//!
//! These are the read model handed to a GUI via [`crate::Core::state`]. They
//! contain no terminal or database handles, so any front end can render them.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentSession;
use crate::layout::Layout;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// Identifier of a [`Project`].
    ProjectId
);
id_type!(
    /// Identifier of a [`Tab`].
    TabId
);
id_type!(
    /// Identifier of a [`Pane`] (one terminal).
    PaneId
);
id_type!(
    /// Identifier of a [`Task`].
    TaskId
);
id_type!(
    /// Identifier of a [`ProjectGroup`].
    GroupId
);
id_type!(
    /// Identifier of a [`Note`].
    NoteId
);

/// A project: a named working directory that owns tabs and tasks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub working_dir: PathBuf,
    /// Shell command line override for this project (e.g. `pwsh -NoLogo`).
    pub shell: Option<String>,
    /// The SSH host the project's folder is on (as `ssh` takes it:
    /// `droplet`, `me@example.com`); `None` on this computer. Its terminals,
    /// files and git are there (see [`crate::remote`]).
    #[serde(default)]
    pub host: Option<String>,
    pub sort_order: i64,
    /// The tab shown when this project is selected.
    pub active_tab: Option<TabId>,
    /// The group the project is listed in.
    pub group: Option<GroupId>,
    pub created_at: i64,
}

/// Projects listed together under a name. A group's projects are next to each
/// other in [`AppState::projects`], and the group is where they are. It has at
/// least one: it goes away with its last.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectGroup {
    pub id: GroupId,
    pub name: String,
    /// The list shows the group without its projects.
    pub collapsed: bool,
    pub created_at: i64,
}

/// A tab: a named group of terminal panes arranged in a split layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub project: ProjectId,
    pub title: String,
    /// Overrides the project working directory for terminals in this tab.
    pub working_dir: Option<PathBuf>,
    pub layout: Layout,
    pub focused_pane: Option<PaneId>,
    pub sort_order: i64,
    pub created_at: i64,
}

/// A pane: one terminal inside a tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub tab: TabId,
    /// Command typed into the fresh shell when this pane is restored
    /// (used when no tracked agent is set).
    pub resume_command: Option<String>,
    /// Coding agent launched in this pane, with its session id once known.
    pub agent: Option<AgentSession>,
    /// Directory the shell was last in, as the shell reported it. The shell
    /// starts here again when the pane is restored; `None` means the tab's
    /// working directory.
    pub cwd: Option<PathBuf>,
    /// The terminal's program runs under a helper process of its own and
    /// keeps running when TerminalRooster quits (see [`crate::host`]).
    #[serde(default)]
    pub keep_running: bool,
    pub created_at: i64,
}

/// A to-do item belonging to a project, optionally linked to one tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project: ProjectId,
    pub tab: Option<TabId>,
    pub title: String,
    pub notes: String,
    pub done: bool,
    pub sort_order: i64,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

/// Text kept for a while, like a scratch file: it stays, across restarts,
/// until it is closed. Notes belong to no project.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: NoteId,
    pub title: String,
    pub text: String,
    pub sort_order: i64,
    pub created_at: i64,
}

impl std::fmt::Debug for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the text: logs must not show what people paste (passwords, keys).
        f.debug_struct("Note")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("bytes", &self.text.len())
            .field("sort_order", &self.sort_order)
            .finish()
    }
}

/// Global settings stored in the database.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Default shell command line; `None` picks the platform default.
    pub default_shell: Option<String>,
    /// Maximum number of lines of scrollback saved per pane.
    pub saved_scrollback_lines: usize,
    /// Lines of scrollback kept in memory per terminal.
    pub scrollback_lines: usize,
    /// Start bash and PowerShell with TerminalRooster's integration, which
    /// reports each command's exit status (see `session::integration`).
    pub shell_integration: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { default_shell: None, saved_scrollback_lines: 5_000, scrollback_lines: 10_000, shell_integration: true }
    }
}

/// Everything the GUI needs to draw the project/tab/task lists.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    /// Sorted by `sort_order`, except that a group's projects stay together.
    pub projects: Vec<Project>,
    /// Sorted by id; a group's place in the list is that of its projects.
    pub groups: Vec<ProjectGroup>,
    /// Sorted by project, then `sort_order`.
    pub tabs: Vec<Tab>,
    pub panes: HashMap<PaneId, Pane>,
    /// Sorted by project, then `sort_order`.
    pub tasks: Vec<Task>,
    /// Sorted by `sort_order`.
    pub notes: Vec<Note>,
    pub selected_project: Option<ProjectId>,
    pub settings: Settings,
    /// Opaque key/value storage for front ends (window geometry, panel widths...).
    pub gui: HashMap<String, String>,
}

impl AppState {
    pub fn project(&self, id: ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub(crate) fn project_mut(&mut self, id: ProjectId) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    pub fn group(&self, id: GroupId) -> Option<&ProjectGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub(crate) fn group_mut(&mut self, id: GroupId) -> Option<&mut ProjectGroup> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    /// Projects of a group in display order.
    pub fn projects_in(&self, group: GroupId) -> impl Iterator<Item = &Project> {
        self.projects.iter().filter(move |p| p.group == Some(group))
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub(crate) fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub(crate) fn task_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn note(&self, id: NoteId) -> Option<&Note> {
        self.notes.iter().find(|n| n.id == id)
    }

    pub(crate) fn note_mut(&mut self, id: NoteId) -> Option<&mut Note> {
        self.notes.iter_mut().find(|n| n.id == id)
    }

    /// Tabs of a project in display order.
    pub fn tabs_of(&self, project: ProjectId) -> impl Iterator<Item = &Tab> {
        self.tabs.iter().filter(move |t| t.project == project)
    }

    /// Tasks of a project in display order.
    pub fn tasks_of(&self, project: ProjectId) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(move |t| t.project == project)
    }

    /// Number of unfinished tasks linked to a tab.
    pub fn open_task_count(&self, tab: TabId) -> usize {
        self.tasks.iter().filter(|t| t.tab == Some(tab) && !t.done).count()
    }

    /// The tab currently shown for the selected project.
    pub fn active_tab(&self) -> Option<&Tab> {
        let project = self.project(self.selected_project?)?;
        self.tab(project.active_tab?)
    }

    /// Working directory a new terminal in `tab` starts in.
    pub fn tab_working_dir(&self, tab: &Tab) -> PathBuf {
        tab.working_dir
            .clone()
            .or_else(|| self.project(tab.project).map(|p| p.working_dir.clone()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }

    pub fn gui_value(&self, key: &str) -> Option<&str> {
        self.gui.get(key).map(String::as_str)
    }

    pub(crate) fn sort(&mut self) {
        // A group's projects follow its first one, even if saved out of order.
        let mut first: HashMap<GroupId, (i64, ProjectId)> = HashMap::new();
        for p in &self.projects {
            if let Some(g) = p.group {
                let key = (p.sort_order, p.id);
                first.entry(g).and_modify(|k| *k = (*k).min(key)).or_insert(key);
            }
        }
        self.projects.sort_by_key(|p| {
            let own = (p.sort_order, p.id);
            (p.group.and_then(|g| first.get(&g).copied()).unwrap_or(own), own)
        });
        self.groups.sort_by_key(|g| g.id);
        let order: HashMap<ProjectId, usize> = self.projects.iter().enumerate().map(|(i, p)| (p.id, i)).collect();
        let rank = |p: &ProjectId| order.get(p).copied().unwrap_or(usize::MAX);
        self.tabs.sort_by_key(|t| (rank(&t.project), t.sort_order, t.id));
        self.tasks.sort_by_key(|t| (rank(&t.project), t.sort_order, t.id));
        self.notes.sort_by_key(|n| (n.sort_order, n.id));
    }
}

pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
