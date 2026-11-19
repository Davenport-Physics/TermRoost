//! The command/event contract between the core and any front end.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::alert::Alert;
use crate::detect::ObservedAgent;
use crate::layout::{Axis, Layout};
use crate::model::{GroupId, NoteId, PaneId, ProjectId, TabId, TaskId};

/// A state change requested by the front end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Creates a project with one tab (whose terminal starts immediately) and selects it.
    CreateProject {
        name: String,
        working_dir: PathBuf,
        shell: Option<String>,
        /// The SSH host its folder is on (see [`crate::Project::host`]).
        #[serde(default)]
        host: Option<String>,
    },
    RenameProject {
        project: ProjectId,
        name: String,
    },
    SetProjectDir {
        project: ProjectId,
        working_dir: PathBuf,
    },
    SetProjectShell {
        project: ProjectId,
        shell: Option<String>,
    },
    /// Deletes a project, and its group if it was the group's last.
    DeleteProject(ProjectId),
    /// Moves a project to `to_index` of the list without it, into `group`
    /// (`None`: outside any group). A group's projects stay together, so the
    /// place has to be next to or among `group`'s projects and not among
    /// another group's. A group left without projects is deleted.
    MoveProject {
        project: ProjectId,
        to_index: usize,
        group: Option<GroupId>,
    },
    SelectProject(ProjectId),

    /// Puts `project` just after `with`, in `with`'s group. If `with` has
    /// none, a new group holding the two takes `with`'s place
    /// ([`Outcome::Group`]).
    GroupProjects {
        project: ProjectId,
        with: ProjectId,
    },
    /// Moves a group's projects, together, to `to_index` of the list
    /// without them. The place can't be among another group's projects.
    MoveGroup {
        group: GroupId,
        to_index: usize,
    },
    RenameGroup {
        group: GroupId,
        name: String,
    },
    /// Hides (`true`) or shows a group's projects in the list.
    SetGroupCollapsed {
        group: GroupId,
        collapsed: bool,
    },

    /// Creates a tab (with one terminal) and selects it. Its terminals start
    /// in `working_dir`, or in the project's directory when that is `None`.
    CreateTab {
        project: ProjectId,
        title: Option<String>,
        working_dir: Option<PathBuf>,
    },
    RenameTab {
        tab: TabId,
        title: String,
    },
    SetTabDir {
        tab: TabId,
        working_dir: Option<PathBuf>,
    },
    /// Moves a tab to another working directory (e.g. a new git worktree):
    /// sets its directory and restarts its shells there, keeping their
    /// scrollback. Terminals running a program or an agent are left alone.
    /// Moving back to the project's directory makes the tab follow the
    /// project again.
    RelocateTab {
        tab: TabId,
        working_dir: PathBuf,
    },
    DeleteTab(TabId),
    MoveTab {
        tab: TabId,
        to_index: usize,
    },
    SelectTab(TabId),

    /// Splits a pane; the new terminal goes right of (vertical) or below (horizontal) it.
    SplitPane {
        pane: PaneId,
        axis: Axis,
    },
    /// Closes a pane. Refused for the last pane of a tab.
    ClosePane(PaneId),
    FocusPane(PaneId),
    /// Replaces a tab's layout (after resizing or rearranging). Must contain the same panes.
    SetLayout {
        tab: TabId,
        layout: Layout,
    },
    SetResumeCommand {
        pane: PaneId,
        command: Option<String>,
    },
    /// Launches a coding agent in the pane's shell with a session id TerminalRooster
    /// tracks, so the exact session can be resumed after a restart.
    StartAgent {
        pane: PaneId,
        kind: AgentKind,
        args: String,
    },
    /// Stops tracking the pane's agent (it will not be resumed).
    ClearAgent(PaneId),
    /// Starts a new shell in an exited (or running) pane, keeping its scrollback.
    RestartPane(PaneId),
    /// Whether the pane's terminal keeps running when TerminalRooster quits.
    /// A running program can't move in or out of a helper, so the terminal
    /// starts again (an agent resumes its conversation).
    SetKeepRunning {
        pane: PaneId,
        on: bool,
    },

    CreateTask {
        project: ProjectId,
        title: String,
        tab: Option<TabId>,
    },
    EditTask {
        task: TaskId,
        title: String,
        notes: String,
    },
    SetTaskDone {
        task: TaskId,
        done: bool,
    },
    AssignTask {
        task: TaskId,
        tab: Option<TabId>,
    },
    MoveTask {
        task: TaskId,
        to_index: usize,
    },
    DeleteTask(TaskId),

    /// Adds a note after the others. Without a title, it is "Note N" with
    /// the lowest N no other note is named.
    CreateNote {
        title: Option<String>,
    },
    RenameNote {
        note: NoteId,
        title: String,
    },
    /// Replaces a note's text.
    SetNoteText {
        note: NoteId,
        text: String,
    },
    /// Closes a note; its text is gone.
    CloseNote(NoteId),
    /// Closes every note.
    CloseAllNotes,

    SetDefaultShell(Option<String>),
    /// Turns the bash and PowerShell integration that reports failed commands
    /// on or off, for shells started from now on.
    SetShellIntegration(bool),
    /// Stores an opaque front-end value (window geometry, panel widths...).
    SetGuiValue {
        key: String,
        value: String,
    },
}

/// What a successful command created, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    None,
    Project(ProjectId),
    /// A new group (named [`crate::NEW_GROUP_NAME`]).
    Group(GroupId),
    Tab(TabId),
    Pane(PaneId),
    Task(TaskId),
    Note(NoteId),
    /// A tab was relocated; this many of its terminals were busy and stayed
    /// in the old directory.
    Relocated {
        left_running: usize,
    },
}

/// Asynchronous notifications from the core.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// The terminal has new content. Sent at most once until the front end
    /// next reads the content, so a busy terminal cannot flood the channel.
    TerminalDirty(PaneId),
    TitleChanged {
        pane: PaneId,
        title: String,
    },
    Bell(PaneId),
    ProcessExited {
        pane: PaneId,
        code: Option<i32>,
    },
    /// A program asked to copy text to the clipboard (OSC 52).
    ClipboardStore(String),
    /// The Codex hook reported (front ends may refresh its status display).
    CodexHookReported,
    /// The agent running in a terminal appeared, switched session, or exited
    /// (`None`). Front ends must pass every event to [`crate::Core::apply_event`],
    /// which records this so the right session is resumed after a restart.
    AgentObserved {
        pane: PaneId,
        agent: Option<ObservedAgent>,
    },
    /// The shell in a terminal reported a new working directory.
    /// [`crate::Core::apply_event`] records it, so the terminal starts there
    /// again after a restart.
    CwdChanged {
        pane: PaneId,
        dir: std::path::PathBuf,
    },
    /// Something the user may want a notification for: a command failed, or
    /// the coding agent in the terminal finished its turn.
    Alert {
        pane: PaneId,
        alert: Alert,
    },
    /// What runs in a terminal changed: its agent started or finished
    /// working, or programs started or stopped. Read it with
    /// [`crate::TerminalHandle::activity`].
    ActivityChanged(PaneId),
    /// A terminal that keeps running when TerminalRooster quits is now
    /// shown elsewhere (a console window of its own); its program runs on.
    /// [`crate::Core::bring_back`] shows it here again.
    ShownElsewhere(PaneId),
    /// The connection to a remote host changed (see [`crate::RemoteHost::status`]).
    Remote {
        host: String,
        status: crate::remote::RemoteStatus,
    },
}
