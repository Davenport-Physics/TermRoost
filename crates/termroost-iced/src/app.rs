//! Application state, messages and update logic.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::widget::{column, container, operation, pane_grid, row, stack};
use iced::{Element, Length, Subscription, Task, keyboard, window};
use serde::{Deserialize, Serialize};
use termroost_core::session::shell::{self, ShellOption};
use termroost_core::session::{bundled_bash, bundled_nvim};
use termroost_core::{
    Activity, AgentKind, Axis, Command, Core, Event, GroupId, NoteId, Outcome, PaneId, ProjectId, TabId, TaskId, git,
};

use crate::color_picker::{ColorPicker, Hsl};
use crate::editor::{DocId, EditorMsg, FileEditor, SessionFiles};
use crate::files::{FilesMsg, FilesState};
use crate::git_panel::{GitLog, GitSync, LogRead, SyncOp};
use crate::git_view::GitViewMsg;
use crate::groups;
use crate::keymap::{Action, Combo, Keymap};
use crate::layout_conv;
use crate::notes::{NOTE_EDITOR, Notes, NotesMsg};
use crate::notify::{self, Category, Level, Notice, PaneNotices, Toast};
use crate::remotes::{RemotesForm, RemotesMsg};
use crate::side_panel::SideTab;
use crate::terminal_view::{self, KeyboardFocus};
use crate::theme;
use crate::themes::{self, ColorField, Hex, ThemeDef, ThemeStore};
use crate::ui;
use crate::worktree::{WorktreeForm, WorktreeMsg};
use crate::worktree_remove::{RemoveForm, RemoveMsg};

/// How often the branches of the visible sessions, and the git log when it
/// shows, are checked again.
const GIT_POLL: Duration = Duration::from_secs(2);

const PREFS_KEY: &str = "iced.prefs";
pub const RENAME_INPUT: &str = "rename-input";
pub const PROJECT_SEARCH: &str = "project-search";
pub const TASK_INPUT: &str = "task-input";
pub const TASK_EDIT_INPUT: &str = "task-edit-input";
pub const DIALOG_FIRST_INPUT: &str = "dialog-first-input";

/// Front-end preferences, stored in the core as an opaque JSON value.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub font_size: f32,
    /// The right panel is shown (its old name is from when it only held tasks).
    #[serde(rename = "tasks_open")]
    pub panel_open: bool,
    /// What the right panel shows.
    pub side_tab: SideTab,
    pub show_done: bool,
    pub tasks_this_tab_only: bool,
    pub window: Option<WindowGeom>,
    /// Extra arguments last used per agent kind.
    pub agent_args: HashMap<String, String>,
    /// Switch between `light_theme` and `dark_theme` with the system setting.
    pub follow_system: bool,
    /// Theme used when not following the system.
    pub theme: String,
    /// Theme used in light mode when following the system (the last light theme picked).
    pub light_theme: String,
    /// Theme used in dark mode when following the system (the last dark theme picked).
    pub dark_theme: String,
    /// Terminal colors come from this theme; `None` matches the interface theme.
    pub terminal_theme: Option<String>,
    /// The user was already asked about the Codex hook at startup.
    pub codex_hook_asked: bool,
    /// Keyboard shortcuts the user changed: action id -> its shortcuts, such
    /// as `"Ctrl+Shift+T"`. Actions not listed use their defaults.
    pub shortcuts: BTreeMap<String, Vec<String>>,
    /// How to tell the user that a command failed.
    pub notify_errors: Level,
    /// How to tell the user that a coding agent finished or needs them.
    pub notify_agents: Level,
    /// Ask before quitting while terminals run programs.
    pub confirm_quit: bool,
    /// The git log shows every branch, not just the session's.
    pub log_all_branches: bool,
    /// Pull rebases the branch's own commits on top of what came in,
    /// instead of merging.
    pub pull_rebase: bool,
    /// What files open in: the built-in editor, or Neovim/Vim in a terminal.
    pub file_editor: FileEditor,
    /// The terminal editor's command line (the file is added at the end);
    /// empty finds Neovim or Vim.
    pub editor_command: String,
    /// The note the Notes tab shows.
    pub note: Option<NoteId>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            font_size: theme::DEFAULT_FONT_SIZE,
            panel_open: true,
            side_tab: SideTab::Tasks,
            show_done: true,
            tasks_this_tab_only: false,
            window: None,
            agent_args: HashMap::new(),
            follow_system: true,
            theme: themes::DARK.into(),
            light_theme: themes::LIGHT.into(),
            dark_theme: themes::DARK.into(),
            terminal_theme: None,
            codex_hook_asked: false,
            shortcuts: BTreeMap::new(),
            notify_errors: Level::Subtle,
            notify_agents: Level::Prominent,
            confirm_quit: true,
            log_all_branches: false,
            pull_rebase: true,
            file_editor: FileEditor::Builtin,
            editor_command: String::new(),
            note: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WindowGeom {
    pub width: f32,
    pub height: f32,
    pub x: Option<f32>,
    pub y: Option<f32>,
}

impl Prefs {
    pub fn load(core: &Core) -> Prefs {
        core.state().gui_value(PREFS_KEY).and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
    }

    pub fn level(&self, category: Category) -> Level {
        match category {
            Category::Error => self.notify_errors,
            Category::Agent => self.notify_agents,
        }
    }
}

/// A terminal with programs running in it, listed when asking before quitting.
#[derive(Clone, Debug)]
pub struct RunningTerminal {
    pub pane: PaneId,
    /// "Session 2 · my-service".
    pub place: String,
    /// "Claude Code (working), cargo".
    pub programs: String,
    /// One of them is a coding agent.
    pub agent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameTarget {
    Project(ProjectId),
    Group(GroupId),
    Tab(TabId),
    Note(NoteId),
}

#[derive(Clone, Debug)]
pub struct Rename {
    pub target: RenameTarget,
    pub value: String,
}

#[derive(Clone, Debug)]
pub enum Dialog {
    Project {
        editing: Option<ProjectId>,
        name: String,
        dir: String,
        shell: String,
        error: Option<String>,
    },
    ConfirmDeleteProject(ProjectId),
    ConfirmDeleteTab(TabId),
    Pane {
        pane: PaneId,
        resume: String,
        args: String,
        /// Keep the terminal running when Terminal Rooster quits.
        keep: bool,
    },
    /// Offer to add the Codex hook (shown once at startup).
    CodexHook,
    /// A small Codex window for trusting the hook (`/hooks`).
    CodexTrust(termroost_core::TerminalHandle),
    /// Says once what was carried over from TermRoost, the app's old name.
    Renamed(termroost_core::Renamed),
    /// Switch the session to another git worktree, or make one.
    Worktree(Box<WorktreeForm>),
    /// Ask before removing a worktree, which closes its sessions.
    RemoveWorktree(Box<RemoveForm>),
    /// Add remotes to the Git tab's repository, or change their URLs.
    Remotes(Box<RemotesForm>),
    /// Quitting while terminals still run programs, or files have unsaved changes.
    ConfirmQuit {
        running: Vec<RunningTerminal>,
        /// Files with unsaved changes ("src/main.rs (agents)").
        unsaved: Vec<String>,
        dont_ask: bool,
    },
    /// Closing a file with unsaved changes.
    CloseFile {
        doc: DocId,
        name: String,
    },
    /// Saving a file that changed on disk since it was read.
    SaveConflict {
        doc: DocId,
        name: String,
    },
    /// Closing every note.
    CloseAllNotes,
}

/// State of the settings page while it is open.
#[derive(Clone, Debug, Default)]
pub struct SettingsState {
    /// Custom theme being edited.
    pub editing: Option<String>,
    /// Raw text of color fields (kept while not yet a valid color).
    pub inputs: HashMap<ColorField, String>,
    /// Color picker open under one of the edited theme's colors.
    pub picker: Option<ColorPicker>,
    pub name_input: String,
    pub name_error: Option<String>,
    pub shell_input: String,
    /// Waiting for the keys of a new shortcut for this action.
    pub recording: Option<Action>,
    /// What the last shortcut change did, e.g. which action lost its keys.
    pub shortcut_note: Option<String>,
    /// The terminal editor files open in when no command is set (looked
    /// for when settings open).
    pub editor_found: Option<PathBuf>,
    /// The user's own Neovim, on PATH.
    pub nvim_on_path: Option<PathBuf>,
    /// Why the file manager didn't open on the data or logs folder.
    pub folder_error: Option<String>,
}

/// Choice in a task's "linked session" dropdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabChoice {
    pub id: Option<TabId>,
    pub label: String,
}

impl std::fmt::Display for TabChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Installing TerminalRooster's own bash or Neovim (Settings, Windows only).
#[derive(Clone, Debug, Default)]
pub enum InstallState {
    #[default]
    Idle,
    Running(bundled_bash::Progress),
    Failed(String),
}

/// News from the thread installing the bash or Neovim.
#[derive(Clone, Debug)]
pub enum InstallEvent {
    Progress(bundled_bash::Progress),
    Done(Result<PathBuf, String>),
}

#[derive(Clone, Debug)]
pub enum Message {
    Core(Event),
    Tick,
    WindowMoved(iced::Point),
    WindowResized(iced::Size),
    /// The window gained (`true`) or lost focus.
    WindowFocus(window::Id, bool),
    WindowFound(Option<window::Id>),
    CloseRequested,
    /// Bring a terminal into view and focus it (from a notification).
    ShowPane(PaneId),
    DismissToast(PaneId),
    NotifyLevel(Category, Level),
    ShellIntegration(bool),
    ConfirmQuitPref(bool),
    /// "Don't ask again" in the quit dialog.
    DialogDontAsk(bool),
    TestNotification,
    Key(keyboard::Event),
    /// Escape, even when a text input captured it (closes dialogs and editors).
    Escape,
    /// Tab/Shift+Tab, even when captured (moves between dialog fields).
    TabKey {
        shift: bool,
    },
    /// Enter, even when captured (confirms dialogs without text fields).
    Enter,
    Shortcut(Action),

    SelectProject(ProjectId),
    SelectTab(TabId),
    NewTab,
    /// Move or group projects (dragged, or moved with the keyboard): a
    /// `MoveProject`, `MoveGroup` or `GroupProjects` command.
    ArrangeProjects(Command),
    /// Show or hide a group's projects.
    ToggleGroup(GroupId),
    /// A double click on a group: rename it. Its first click opened or
    /// closed the group, so that is undone.
    GroupDoubleClicked(GroupId),
    /// Move a session to this index of its project's list without it.
    MoveTab(TabId, usize),
    /// The text in the projects search box changed.
    ProjectSearch(String),
    /// Enter in the projects search box: open the first project it lists.
    ProjectSearchSubmit,
    /// Empty the projects search box (its ✕).
    ProjectSearchClear,
    /// Escape in the projects search box: empty it and go back to the terminal.
    ProjectSearchEscape,

    NewProject,
    EditProject(ProjectId),

    OpenSettings,
    CloseSettings,
    /// Open the file manager on one of Terminal Rooster's own folders (settings).
    OpenFolder(PathBuf),
    FolderOpened(Result<(), String>),
    AddCodexHook,
    RemoveCodexHook,
    /// Open the Codex window to trust the hook.
    TrustCodexHook,
    Noop,
    SystemTheme(iced::theme::Mode),
    FollowSystem(bool),
    /// Use a theme for the interface.
    UseTheme(String),
    /// Take terminal colors from a theme (`None`: match the interface).
    TerminalTheme(Option<String>),
    EditTheme(String),
    DuplicateTheme(String),
    DeleteTheme(String),
    ThemeNameInput(String),
    ThemeColor(ColorField, String),
    /// Open the color picker for a color of the edited theme (`None`: close it).
    PickColor(Option<ColorField>),
    /// A hue/saturation/lightness slider of the color picker moved.
    PickerHsl(Hsl),
    FontSize(f32),
    ShellInput(String),
    ShellCommit,
    /// Wait for the keys of a new shortcut for an action (`None`: stop waiting).
    RecordShortcut(Option<Action>),
    RemoveShortcut(Action, Combo),
    /// Back to the default shortcuts of one action, or of all (`None`).
    ResetShortcuts(Option<Action>),

    PaneSettings(PaneId),
    AskDeleteProject(ProjectId),
    AskDeleteTab(TabId),
    DialogName(String),
    DialogDir(String),
    DialogShell(String),
    DialogResume(String),
    DialogArgs(String),
    DialogKeepRunning(bool),
    BrowseDir,
    DirPicked(Option<PathBuf>),
    DialogSubmit,
    DialogCancel,
    DialogStartAgent(AgentKind),
    DialogClearAgent,

    StartRename(RenameTarget),
    RenameInput(String),
    RenameCommit,
    RenameCancel,

    FocusPane(PaneId),
    PaneClicked(pane_grid::Pane),
    PaneDragged(pane_grid::DragEvent),
    PaneResized(pane_grid::ResizeEvent),
    Split(PaneId, Axis),
    ClosePane(PaneId),
    RestartPane(PaneId),
    StartAgent(PaneId, AgentKind),
    /// Show a terminal that keeps running in a console window of its own.
    OpenInWindow(PaneId),
    /// Show a terminal here again that a window of its own shows.
    BringBack(PaneId),

    /// Show the right panel on this tab, or hide it if it already shows it.
    TogglePanel(SideTab),
    /// Show the right panel on this tab.
    ShowPanel(SideTab),
    ClosePanel,
    Files(FilesMsg),
    Editor(EditorMsg),
    Notes(NotesMsg),
    /// Edit files in the built-in editor or in a terminal editor (settings).
    FileEditor(FileEditor),
    /// The terminal editor's command line changed (settings).
    EditorCommand(String),
    NewTaskInput(String),
    AddTask,
    TaskDone(TaskId, bool),
    TaskTab(TaskId, TabChoice),
    DeleteTask(TaskId),
    EditTask(TaskId),
    TaskEditInput(String),
    TaskEditCommit,
    TaskEditCancel,
    ShowDone(bool),
    ThisTabOnly(bool),
    /// A task was pressed: it may be dragged onto a session.
    TaskGrab(TaskId),
    /// The pointer moved while a task is held (window coordinates).
    TaskDragMove(iced::Point),
    /// The pointer went onto (`true`) or off a place a held task can be dropped.
    TaskDragOver(DropTarget, bool),
    /// A held task was let go over a session: link it to that session.
    TaskDrop(DropTarget),
    /// The button was let go (or the window lost focus): the task is put down.
    TaskDragEnd,

    /// Shells found on this machine (detected in the background).
    ShellsDetected(Vec<ShellOption>),
    /// A shell picked from the list in settings becomes the default.
    PickDefaultShell(ShellOption),
    InstallBash,
    BashInstall(InstallEvent),
    InstallNvim,
    NvimInstall(InstallEvent),

    /// Time to read the visible sessions' branches again.
    GitPoll,
    GitStatus {
        /// Where each session directory of the selected project stands in
        /// git (`None`: not in a repository).
        statuses: Vec<(PathBuf, Option<git::Status>)>,
        /// The directories of all sessions that no longer exist.
        gone: Vec<PathBuf>,
    },
    Worktree(WorktreeMsg),
    RemoveWorktree(RemoveMsg),
    LogRead(LogRead),
    LogAllBranches(bool),
    /// Pull, push or change the remote of the Git tab's repository.
    GitSync(SyncOp),
    GitSynced(PathBuf, Result<String, String>),
    PullRebase(bool),
    DismissSync,
    Remotes(RemotesMsg),
    /// Commits shown in place of the terminals.
    GitView(GitViewMsg),

    /// Puts text on the clipboard.
    CopyText(String),
    DismissError,
    DismissNotice,
}

pub struct App {
    pub(crate) core: Core,
    pub(crate) grids: HashMap<TabId, pane_grid::State<PaneId>>,
    pub(crate) prefs: Prefs,
    prefs_dirty: bool,
    layout_dirty: HashSet<TabId>,
    pub(crate) rename: Option<Rename>,
    /// The projects list shows only projects matching this.
    pub(crate) project_search: String,
    pub(crate) dialog: Option<Dialog>,
    pub(crate) new_task: String,
    pub(crate) task_edit: Option<(TaskId, String)>,
    /// The folders read for the Files tab.
    pub(crate) files: FilesState,
    /// The files each session has open.
    pub(crate) editors: HashMap<TabId, SessionFiles>,
    /// The last id given to an open file.
    pub(crate) next_doc: u64,
    /// The last version number given to the text of an open file.
    pub(crate) next_version: u64,
    /// The list of open files changed since it was last saved.
    pub(crate) open_files_dirty: bool,
    /// Open files are being compared with the disk.
    pub(crate) disk_polling: bool,
    /// The text of the notes, being edited.
    pub(crate) notes: Notes,
    /// Tabs with output the user has not looked at yet.
    pub(crate) activity: HashSet<TabId>,
    /// What goes on in the terminals that are not idle (see [`Activity`]).
    pub(crate) busy: HashMap<PaneId, Activity>,
    pub(crate) error: Option<String>,
    /// Information worth a banner that is not an error.
    pub(crate) notice: Option<String>,
    /// Git status of session directories, read in the background.
    pub(crate) git: HashMap<PathBuf, Option<git::Status>>,
    /// A periodic git read is running.
    git_polling: bool,
    /// Session folders deleted outside the app (see `gone_folders`).
    pub(crate) gone: HashSet<PathBuf>,
    /// The log in the side panel's Git tab.
    pub(crate) git_log: Option<GitLog>,
    /// The Git tab's last pull, push or change of remote.
    pub(crate) git_sync: Option<GitSync>,
    /// The log is being read.
    pub(crate) log_reading: bool,
    pub(crate) themes: ThemeStore,
    themes_dirty: bool,
    pub(crate) settings: Option<SettingsState>,
    /// Shells installed on this machine, for the shell pickers.
    pub(crate) shells: Vec<ShellOption>,
    pub(crate) bash_install: InstallState,
    pub(crate) nvim_install: InstallState,
    pub(crate) system_mode: iced::theme::Mode,
    ui_theme: iced::Theme,
    /// Cached Codex integration state (reads files, so refreshed on demand).
    pub(crate) codex_hook: termroost_core::CodexHookStatus,
    /// Shortcuts in effect (from `prefs.shortcuts`); shared with the terminals.
    pub(crate) keymap: Arc<Keymap>,
    /// Which terminal has the keyboard; shared with the terminals.
    pub(crate) keyboard_focus: KeyboardFocus,
    /// A task held with the mouse, to be dropped on a session.
    pub(crate) task_drag: Option<TaskDrag>,
    /// Alerts in terminals the user has not looked at since.
    pub(crate) notices: HashMap<PaneId, PaneNotices>,
    /// Pop-ups about terminals that are out of view, oldest first.
    pub(crate) toasts: Vec<Toast>,
    window_focused: bool,
    window_id: Option<window::Id>,
    /// The terminal the user is looking at: the focused one of the visible
    /// session, while the window has focus. Programs in it are told they
    /// have focus (if they asked to know), and its alerts are dropped.
    attended: Option<PaneId>,
    /// Clicks on desktop notifications, by pane.
    clicks: (async_channel::Sender<PaneId>, async_channel::Receiver<PaneId>),
    closing: bool,
}

/// Where a task can be dropped to link it to a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropTarget {
    /// A session in the sessions list.
    Session(TabId),
    /// The terminals of the session on screen.
    Terminals,
}

/// A task being dragged from the task panel onto a session.
#[derive(Clone, Copy, Debug)]
pub struct TaskDrag {
    pub task: TaskId,
    /// Where the pointer was when it first moved after the press.
    origin: Option<iced::Point>,
    /// Where the pointer is now (window coordinates).
    pub cursor: Option<iced::Point>,
    /// Far enough from where it was picked up to be a drag rather than a click.
    pub moving: bool,
    /// The drop target under the pointer.
    pub over: Option<DropTarget>,
}

/// How far the pointer moves with a task held before it counts as dragging it.
const TASK_DRAG_THRESHOLD: f32 = 5.0;

/// Hashable wrapper so the event receiver can back a subscription.
struct Events(async_channel::Receiver<Event>);

impl Hash for Events {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "terminalrooster-core-events".hash(state);
    }
}

/// Hashable wrapper for the clicks on desktop notifications.
struct Clicks(async_channel::Receiver<PaneId>);

impl Hash for Clicks {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "terminalrooster-notification-clicks".hash(state);
    }
}

impl App {
    pub fn new(core: Core) -> (App, Task<Message>) {
        let prefs = Prefs::load(&core);
        let themes = ThemeStore::load(&core);
        let keymap = Arc::new(Keymap::new(&prefs.shortcuts));
        let notes = Notes::of(&core.state().notes);
        let mut app = App {
            core,
            grids: HashMap::new(),
            prefs,
            prefs_dirty: false,
            layout_dirty: HashSet::new(),
            rename: None,
            project_search: String::new(),
            dialog: None,
            new_task: String::new(),
            task_edit: None,
            files: FilesState::default(),
            editors: HashMap::new(),
            next_doc: 0,
            next_version: 0,
            open_files_dirty: false,
            disk_polling: false,
            notes,
            activity: HashSet::new(),
            busy: HashMap::new(),
            error: None,
            notice: None,
            git: HashMap::new(),
            git_polling: false,
            gone: HashSet::new(),
            git_log: None,
            git_sync: None,
            log_reading: false,
            themes,
            themes_dirty: false,
            settings: None,
            shells: Vec::new(),
            bash_install: InstallState::Idle,
            nvim_install: InstallState::Idle,
            system_mode: iced::theme::Mode::None,
            ui_theme: ThemeDef::dark().iced_theme(),
            codex_hook: termroost_core::CodexHookStatus::default(),
            keymap,
            keyboard_focus: KeyboardFocus::default(),
            task_drag: None,
            notices: HashMap::new(),
            toasts: Vec::new(),
            window_focused: true,
            window_id: None,
            attended: None,
            clicks: async_channel::unbounded(),
            closing: false,
        };
        notify::init(app.core.data_dir());
        app.apply_theme();
        app.ensure_active_grid();
        app.restore_open_files();
        app.codex_hook = app.core.codex_hook_status();
        if app.codex_hook.codex_found && app.codex_hook.installed_in.is_none() && !app.prefs.codex_hook_asked {
            app.dialog = Some(Dialog::CodexHook);
        }
        let renamed = app.core.renamed();
        if renamed.moved_from.is_some() || renamed.codex_hook_updated {
            app.dialog = Some(Dialog::Renamed(renamed.clone()));
        }
        // A file shown before the restart is read again (or its editor started).
        let revived = app.revive_shown();
        app.sync_attention();
        let task = Task::batch([
            revived,
            app.focus_active(),
            iced::system::theme().map(Message::SystemTheme),
            detect_shells(),
            app.read_git(),
            window::latest().map(Message::WindowFound),
        ]);
        (app, task)
    }

    pub fn theme(&self) -> iced::Theme {
        self.ui_theme.clone()
    }

    pub(crate) fn system_is_dark(&self) -> bool {
        self.system_mode != iced::theme::Mode::Light
    }

    /// The interface theme currently applied.
    pub(crate) fn active_theme(&self) -> ThemeDef {
        let name = if !self.prefs.follow_system {
            &self.prefs.theme
        } else if self.system_is_dark() {
            &self.prefs.dark_theme
        } else {
            &self.prefs.light_theme
        };
        self.themes.find(name).cloned().unwrap_or_else(|| {
            if self.prefs.follow_system && !self.system_is_dark() { ThemeDef::light() } else { ThemeDef::dark() }
        })
    }

    /// The theme the terminal colors come from.
    pub(crate) fn terminal_theme(&self) -> ThemeDef {
        self.prefs
            .terminal_theme
            .as_deref()
            .and_then(|n| self.themes.find(n))
            .cloned()
            .unwrap_or_else(|| self.active_theme())
    }

    /// Applies the active theme to the interface and to every terminal.
    fn apply_theme(&mut self) {
        let def = self.active_theme();
        let term = self.terminal_theme();
        tracing::debug!(theme = %def.name, terminal = %term.name, "applying theme");
        self.ui_theme = def.iced_theme();
        self.core.set_palette(term.palette());
    }

    pub fn title(&self) -> String {
        use crate::DISPLAY_NAME as NAME;
        let s = self.core.state();
        match (s.selected_project.and_then(|p| s.project(p)), s.active_tab()) {
            (Some(p), Some(t)) => format!("{} · {} — {NAME}", t.title, p.name),
            (Some(p), None) => format!("{} — {NAME}", p.name),
            _ => NAME.to_owned(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            Subscription::run_with(Events(self.core.events()), |e| e.0.clone()).map(Message::Core),
            Subscription::run_with(Clicks(self.clicks.1.clone()), |c| c.0.clone()).map(Message::ShowPane),
            keyboard::listen().map(Message::Key),
            iced::system::theme_changes().map(Message::SystemTheme),
            iced::event::listen_with(|event, _status, id| match event {
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::Escape),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Tab),
                    modifiers,
                    ..
                }) if !modifiers.control() && !modifiers.alt() => Some(Message::TabKey { shift: modifiers.shift() }),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    ..
                }) => Some(Message::Enter),
                iced::Event::Window(window::Event::Moved(p)) => Some(Message::WindowMoved(p)),
                iced::Event::Window(window::Event::Resized(s)) => Some(Message::WindowResized(s)),
                iced::Event::Window(window::Event::CloseRequested) => Some(Message::CloseRequested),
                iced::Event::Window(window::Event::Focused) => Some(Message::WindowFocus(id, true)),
                iced::Event::Window(window::Event::Unfocused) => Some(Message::WindowFocus(id, false)),
                _ => None,
            }),
            iced::time::every(GIT_POLL).map(|_| Message::GitPoll),
        ];
        if self.files_shown() {
            subs.push(
                Subscription::run_with(self.files.watch.changes(), |c| c.0.clone())
                    .map(|c| Message::Files(FilesMsg::Changed(c))),
            );
            // Folders that can't be watched are read now and then instead.
            if self.window_focused && self.files.watch.needs_poll() {
                subs.push(iced::time::every(crate::files::POLL).map(|_| Message::Files(FilesMsg::Poll)));
            }
        }
        // Agents change open files: look at them now and then, and when coming back to the window.
        if self.has_files_to_check() && self.window_focused {
            subs.push(iced::time::every(crate::files::POLL).map(|_| Message::Editor(EditorMsg::DiskPoll)));
        }
        if self.task_drag.is_some() {
            // Only while a task is held, so moving the mouse doesn't redraw the app otherwise.
            // A drop on a session arrives from that widget before this end does.
            subs.push(iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::TaskDragMove(position))
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left))
                | iced::Event::Window(window::Event::Unfocused) => Some(Message::TaskDragEnd),
                _ => None,
            }));
        }
        if !self.layout_dirty.is_empty()
            || self.prefs_dirty
            || self.themes_dirty
            || self.open_files_dirty
            || self.notes.unsaved()
            || !self.toasts.is_empty()
        {
            subs.push(iced::time::every(Duration::from_millis(500)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    pub(crate) fn dispatch(&mut self, cmd: Command) -> Option<Outcome> {
        match self.core.dispatch(cmd) {
            Ok(o) => Some(o),
            Err(e) => {
                tracing::warn!(error = %e, "command failed");
                self.error = Some(e.to_string());
                None
            }
        }
    }

    pub(crate) fn active_tab_id(&self) -> Option<TabId> {
        self.core.state().active_tab().map(|t| t.id)
    }

    /// The session a task dropped on `target` gets linked to.
    pub(crate) fn drop_tab(&self, target: DropTarget) -> Option<TabId> {
        match target {
            DropTarget::Session(tab) => Some(tab),
            DropTarget::Terminals => self.active_tab_id(),
        }
    }

    /// Whether a task is being dragged and is over `target`.
    pub(crate) fn is_drop_target(&self, target: DropTarget) -> bool {
        self.task_drag.is_some_and(|d| d.moving && d.over == Some(target))
    }

    fn ensure_active_grid(&mut self) {
        if let Some(tab) = self.core.state().active_tab() {
            self.grids.entry(tab.id).or_insert_with(|| layout_conv::grid_state(&tab.layout));
        }
    }

    fn rebuild_grid(&mut self, tab: TabId) {
        if let Some(t) = self.core.state().tab(tab) {
            self.grids.insert(tab, layout_conv::grid_state(&t.layout));
        }
    }

    /// Git status of a tab's working directory, from the last background read.
    pub(crate) fn git_of(&self, tab: &termroost_core::Tab) -> Option<&git::Status> {
        self.git.get(&self.core.state().tab_working_dir(tab))?.as_ref()
    }

    /// Reads the git status of the selected project's sessions on a thread
    /// (directories may be on a slow network drive), and looks whether the
    /// folders of all sessions still exist.
    pub(crate) fn read_git(&self) -> Task<Message> {
        let s = self.core.state();
        let sorted = |mut dirs: Vec<PathBuf>| {
            dirs.sort();
            dirs.dedup();
            dirs
        };
        let dirs = sorted(match s.selected_project {
            Some(project) => s.tabs_of(project).map(|t| s.tab_working_dir(t)).collect(),
            None => Vec::new(),
        });
        let all = sorted(s.tabs.iter().map(|t| s.tab_working_dir(t)).collect());
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let statuses = dirs
                .into_iter()
                .map(|d| {
                    let status = git::Status::of(&d);
                    (d, status)
                })
                .collect();
            let gone = all.into_iter().filter(|d| !d.is_dir()).collect();
            let _ = tx.send_blocking((statuses, gone));
        });
        Task::perform(async move { rx.recv().await.unwrap_or_default() }, |(statuses, gone)| Message::GitStatus {
            statuses,
            gone,
        })
    }

    /// Moves keyboard focus to the focused terminal of the visible tab, or to
    /// the file shown in place of its terminals.
    pub(crate) fn focus_active(&self) -> Task<Message> {
        if self.dialog.is_some() || self.rename.is_some() || self.task_edit.is_some() {
            return Task::none();
        }
        if let Some(focus) = self.focus_doc() {
            return focus;
        }
        match self.core.state().active_tab().and_then(|t| t.focused_pane) {
            Some(p) => operation::focus(terminal_view::widget_id(p)),
            None => Task::none(),
        }
    }

    pub(crate) fn mark_prefs(&mut self) {
        self.prefs_dirty = true;
    }

    fn flush_pending(&mut self) {
        for tab in std::mem::take(&mut self.layout_dirty) {
            if let Some(layout) = self.grids.get(&tab).and_then(layout_conv::layout_of) {
                self.dispatch(Command::SetLayout { tab, layout });
            }
        }
        if self.prefs_dirty {
            self.prefs_dirty = false;
            let value = serde_json::to_string(&self.prefs).expect("prefs serialize");
            self.dispatch(Command::SetGuiValue { key: PREFS_KEY.into(), value });
        }
        if self.themes_dirty {
            self.themes_dirty = false;
            let value = self.themes.to_json();
            self.dispatch(Command::SetGuiValue { key: themes::THEMES_KEY.into(), value });
        }
        if let Some(value) = self.take_open_files_value() {
            self.dispatch(Command::SetGuiValue { key: App::OPEN_FILES_KEY.into(), value });
        }
        for (note, text) in self.notes.take_unsaved() {
            self.dispatch(Command::SetNoteText { note, text });
        }
    }

    /// Where the keyboard goes once a rename ends: back to the note's text
    /// for a note, else to the terminal.
    fn after_rename(&self, target: RenameTarget) -> Task<Message> {
        match target {
            RenameTarget::Note(_) => operation::focus(NOTE_EDITOR),
            _ => self.focus_active(),
        }
    }

    pub(crate) fn after_select(&mut self) -> Task<Message> {
        if let Some(tab) = self.active_tab_id() {
            self.activity.remove(&tab);
        }
        self.ensure_active_grid();
        Task::batch([self.focus_active(), self.read_git(), self.refresh_files()])
    }

    fn grid_pane(&self, pane: pane_grid::Pane) -> Option<(TabId, PaneId)> {
        let tab = self.active_tab_id()?;
        let id = *self.grids.get(&tab)?.get(pane)?;
        Some((tab, id))
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        let revived = self.revive_shown();
        self.sync_attention();
        let task = Task::batch([task, revived]);
        // Lets go of the folders when the Files tab is hidden, and watches
        // another session's when it comes on screen.
        self.watch_files();
        match self.ensure_files_root() {
            Some(read) => Task::batch([task, read]),
            None => task,
        }
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        if self.closing {
            return Task::none();
        }
        // Key events are never logged: they can carry what the user types.
        // Nor are editor actions and folder listings: what is in people's files.
        let quiet = match &message {
            Message::Core(_)
            | Message::Tick
            | Message::Key(_)
            | Message::Files(FilesMsg::Listed(_) | FilesMsg::Changed(_) | FilesMsg::NewFileName(_)) => true,
            Message::Editor(msg) => !msg.loggable(),
            Message::Notes(msg) => !msg.loggable(),
            Message::GitView(msg) => !msg.loggable(),
            _ => false,
        };
        if !quiet {
            tracing::trace!(?message, "update");
        }
        match message {
            Message::Core(ev) => {
                self.core.apply_event(&ev);
                // A terminal editor quit: its file closes.
                if let Event::ProcessExited { pane, code } = ev
                    && let Some(task) = self.editor_exited(pane, code)
                {
                    return task;
                }
                match ev {
                    Event::TerminalDirty(pane) | Event::Bell(pane) => {
                        let tab = self.core.state().pane(pane).map(|p| p.tab);
                        if let Some(tab) = tab.filter(|t| Some(*t) != self.active_tab_id()) {
                            self.activity.insert(tab);
                        }
                    }
                    Event::ActivityChanged(pane) => {
                        match self.core.terminal(pane).map(|h| h.activity()).filter(|a| !a.is_idle()) {
                            Some(activity) => self.busy.insert(pane, activity),
                            None => self.busy.remove(&pane),
                        };
                    }
                    Event::ClipboardStore(text) => return iced::clipboard::write(text),
                    Event::Alert { pane, alert } => return self.on_notice(pane, Notice::from_alert(&alert)),
                    Event::ProcessExited { pane, code: Some(code) } if code != 0 => {
                        return self.on_notice(pane, Notice::shell_exited(code));
                    }
                    _ => {}
                }
            }
            Message::Tick => {
                self.flush_pending();
                self.toasts.retain(|t| t.shown.elapsed() < notify::TOAST_LIFETIME);
            }
            Message::WindowFocus(id, focused) => {
                self.window_id = Some(id);
                self.window_focused = focused;
                if focused {
                    let changes = self.poll_changes();
                    // In case the system lost track of a change (its queue can overflow).
                    let files = self.refresh_files();
                    return Task::batch([self.update(Message::Editor(EditorMsg::DiskPoll)), changes, files]);
                }
            }
            Message::WindowFound(id) => self.window_id = self.window_id.or(id),
            Message::ShowPane(pane) => return self.show_pane(pane),
            Message::DismissToast(pane) => self.toasts.retain(|t| t.pane != pane),
            Message::NotifyLevel(category, level) => {
                match category {
                    Category::Error => self.prefs.notify_errors = level,
                    Category::Agent => self.prefs.notify_agents = level,
                }
                if level == Level::Off {
                    for n in self.notices.values_mut() {
                        match category {
                            Category::Error => n.error = None,
                            Category::Agent => n.agent = None,
                        }
                    }
                    self.toasts.retain(|t| t.notice.category != category);
                }
                self.mark_prefs();
            }
            Message::ShellIntegration(on) => {
                self.dispatch(Command::SetShellIntegration(on));
            }
            Message::ConfirmQuitPref(on) => {
                self.prefs.confirm_quit = on;
                self.mark_prefs();
            }
            Message::DialogDontAsk(v) => {
                if let Some(Dialog::ConfirmQuit { dont_ask, .. }) = &mut self.dialog {
                    *dont_ask = v;
                }
            }
            Message::TestNotification => {
                let pane = self.core.state().active_tab().and_then(|t| t.focused_pane).unwrap_or(PaneId(0));
                notify::desktop(
                    crate::DISPLAY_NAME.into(),
                    "Desktop notifications work. Clicking one brings its terminal into view.".into(),
                    pane,
                    self.clicks.0.clone(),
                );
            }
            Message::WindowMoved(p) => {
                let w = self.prefs.window.get_or_insert_with(Default::default);
                w.x = Some(p.x);
                w.y = Some(p.y);
                self.mark_prefs();
            }
            Message::WindowResized(s) => {
                let w = self.prefs.window.get_or_insert_with(Default::default);
                w.width = s.width;
                w.height = s.height;
                self.mark_prefs();
            }
            Message::CloseRequested => {
                if matches!(self.dialog, Some(Dialog::ConfirmQuit { .. })) {
                    return Task::none();
                }
                // Unsaved files are always asked about; running programs as the user chose.
                let unsaved = self.unsaved_files();
                let running = if self.prefs.confirm_quit {
                    // Terminals under helpers go on running.
                    let s = self.core.state();
                    let stops = |t: &RunningTerminal| {
                        self.core.host_command().is_none() || !s.pane(t.pane).is_some_and(|p| p.keep_running)
                    };
                    self.running_terminals().into_iter().filter(stops).chain(self.running_editors()).collect()
                } else {
                    Vec::new()
                };
                if !running.is_empty() || !unsaved.is_empty() {
                    self.rename = None;
                    self.dialog = Some(Dialog::ConfirmQuit { running, unsaved, dont_ask: false });
                    return Task::none();
                }
                return self.quit();
            }
            Message::Escape => {
                if self.task_drag.take().is_some() || self.close_files_menu() {
                    return Task::none();
                }
                if matches!(self.dialog, Some(Dialog::CodexTrust(_))) {
                    return Task::none();
                }
                // A remote's URL being changed: stop changing it, not the dialog.
                if let Some(Dialog::Remotes(f)) = &self.dialog
                    && f.editing.is_some()
                {
                    return self.update(Message::Remotes(RemotesMsg::CancelEdit));
                }
                if self.dialog.is_some() {
                    return self.update(Message::DialogCancel);
                }
                if let Some(st) = &mut self.settings
                    && (st.recording.is_some() || st.picker.is_some())
                {
                    if st.recording.take().is_none() {
                        st.picker = None;
                    }
                    return Task::none();
                }
                if self.settings.is_some() {
                    return self.update(Message::CloseSettings);
                }
                if self.rename.is_some() {
                    return self.update(Message::RenameCancel);
                }
                if self.task_edit.is_some() {
                    return self.update(Message::TaskEditCancel);
                }
                if self.cancel_new_file() {
                    return self.focus_active();
                }
            }
            Message::Enter => {
                // Dialogs with text fields submit through the fields themselves.
                if matches!(
                    self.dialog,
                    Some(
                        Dialog::ConfirmDeleteProject(_)
                            | Dialog::ConfirmDeleteTab(_)
                            | Dialog::ConfirmQuit { .. }
                            | Dialog::CloseFile { .. }
                            | Dialog::CloseAllNotes
                    )
                ) {
                    return self.submit_dialog();
                }
            }
            Message::TabKey { shift } => {
                if self.dialog.is_some() && !matches!(self.dialog, Some(Dialog::CodexTrust(_))) {
                    return if shift { operation::focus_previous() } else { operation::focus_next() };
                }
            }
            Message::Key(keyboard::Event::KeyPressed { key, physical_key, modifiers, .. }) => {
                if let Some(action) = self.settings.as_ref().and_then(|st| st.recording) {
                    // A lone modifier or Escape isn't a shortcut: keep waiting.
                    if let Some(combo) = Combo::from_event(&key, physical_key, modifiers) {
                        self.record_shortcut(action, combo);
                    }
                    return Task::none();
                }
                if let Some(action) = self.keymap.action(&key, physical_key, modifiers)
                    && !action.in_terminal()
                {
                    return self.update(Message::Shortcut(action));
                }
            }
            Message::Key(_) => {}
            Message::Shortcut(sc) => return self.shortcut(sc),

            Message::SelectProject(p) => {
                self.rename = None;
                self.dispatch(Command::SelectProject(p));
                return self.after_select();
            }
            Message::SelectTab(t) => {
                if self.rename.as_ref().is_some_and(|r| r.target != RenameTarget::Tab(t)) {
                    self.rename = None;
                }
                self.dispatch(Command::SelectTab(t));
                return self.after_select();
            }
            Message::NewTab => {
                if let Some(project) = self.core.state().selected_project {
                    self.dispatch(Command::CreateTab { project, title: None, working_dir: None });
                    return self.after_select();
                }
            }
            Message::ArrangeProjects(cmd) => {
                // A new group gets its name right away.
                if let Some(Outcome::Group(group)) = self.dispatch(cmd) {
                    return self.update(Message::StartRename(RenameTarget::Group(group)));
                }
            }
            Message::ToggleGroup(group) => {
                if let Some(g) = self.core.state().group(group) {
                    let collapsed = !g.collapsed;
                    self.dispatch(Command::SetGroupCollapsed { group, collapsed });
                }
            }
            Message::GroupDoubleClicked(group) => {
                // While searching, clicks don't open or close groups.
                if !self.searching_projects() {
                    let _ = self.update(Message::ToggleGroup(group));
                }
                return self.update(Message::StartRename(RenameTarget::Group(group)));
            }
            Message::MoveTab(tab, to_index) => {
                self.dispatch(Command::MoveTab { tab, to_index });
            }
            Message::ProjectSearch(query) => self.project_search = query,
            Message::ProjectSearchSubmit => {
                // Done searching: the whole list comes back, with the project selected.
                let first = self.listed_projects().next().map(|p| p.id);
                if let Some(first) = first {
                    self.project_search.clear();
                    return self.update(Message::SelectProject(first));
                }
            }
            Message::ProjectSearchClear => {
                self.project_search.clear();
                return operation::focus(PROJECT_SEARCH);
            }
            Message::ProjectSearchEscape => {
                self.project_search.clear();
                return self.focus_active();
            }

            Message::NewProject => {
                self.dialog = Some(Dialog::Project {
                    editing: None,
                    name: String::new(),
                    dir: String::new(),
                    shell: String::new(),
                    error: None,
                });
                return operation::focus(DIALOG_FIRST_INPUT);
            }
            Message::EditProject(id) => {
                if let Some(p) = self.core.state().project(id) {
                    self.dialog = Some(Dialog::Project {
                        editing: Some(id),
                        name: p.name.clone(),
                        dir: p.working_dir.display().to_string(),
                        shell: p.shell.clone().unwrap_or_default(),
                        error: None,
                    });
                    return operation::focus(DIALOG_FIRST_INPUT);
                }
            }
            Message::OpenSettings => {
                self.rename = None;
                self.task_edit = None;
                let shell_input = self.core.state().settings.default_shell.clone().unwrap_or_default();
                let editor_found = shell::find_terminal_editor().map(|e| e.program);
                let path = std::env::var_os("PATH").unwrap_or_default();
                let nvim_on_path = shell::find_program("nvim", &path);
                self.settings =
                    Some(SettingsState { shell_input, editor_found, nvim_on_path, ..SettingsState::default() });
                self.codex_hook = self.core.codex_hook_status();
                // Pick up shells installed since the last look.
                return detect_shells();
            }
            Message::AddCodexHook => {
                self.prefs.codex_hook_asked = true;
                self.mark_prefs();
                let added = self.core.install_codex_hook();
                self.codex_hook = self.core.codex_hook_status();
                match added {
                    Ok(_) => return self.update(Message::TrustCodexHook),
                    Err(e) => self.error = Some(format!("Could not add the Codex hook: {e}")),
                }
            }
            Message::TrustCodexHook => {
                let s = self.core.state();
                let cwd = s
                    .selected_project
                    .and_then(|p| s.project(p))
                    .map(|p| p.working_dir.clone())
                    .filter(|d| d.is_dir())
                    .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
                    .unwrap_or_default();
                let handle = self.core.open_scratch_terminal(&cwd, Some("codex"));
                let focus = operation::focus(terminal_view::widget_id(handle.pane()));
                self.dialog = Some(Dialog::CodexTrust(handle));
                return focus;
            }
            Message::ShellsDetected(list) => self.shells = list,
            Message::PickDefaultShell(option) => {
                if let Some(st) = &mut self.settings {
                    st.shell_input = option.command.clone();
                }
                self.dispatch(Command::SetDefaultShell(Some(option.command)));
            }
            Message::InstallBash => {
                if matches!(self.bash_install, InstallState::Running(_)) {
                    return Task::none();
                }
                let total = bundled_bash::download_size();
                self.bash_install = InstallState::Running(bundled_bash::Progress::Downloading { done: 0, total });
                return Task::run(run_install(|p| bundled_bash::install(p)), Message::BashInstall);
            }
            Message::InstallNvim => {
                if matches!(self.nvim_install, InstallState::Running(_)) {
                    return Task::none();
                }
                let total = bundled_nvim::download_size();
                self.nvim_install = InstallState::Running(bundled_nvim::Progress::Downloading { done: 0, total });
                return Task::run(run_install(|p| bundled_nvim::install(p)), Message::NvimInstall);
            }
            Message::NvimInstall(InstallEvent::Progress(p)) => {
                if matches!(self.nvim_install, InstallState::Running(_)) {
                    self.nvim_install = InstallState::Running(p);
                }
            }
            Message::NvimInstall(InstallEvent::Done(Ok(_))) => {
                self.nvim_install = InstallState::Idle;
                // The button says "Install and use": files open in Neovim now.
                self.prefs.file_editor = FileEditor::Terminal;
                self.prefs.editor_command.clear();
                self.mark_prefs();
                if let Some(st) = &mut self.settings {
                    st.editor_found = shell::find_terminal_editor().map(|e| e.program);
                }
            }
            Message::NvimInstall(InstallEvent::Done(Err(e))) => {
                tracing::warn!(error = %e, "installing Neovim failed");
                self.nvim_install = InstallState::Failed(e);
            }
            Message::BashInstall(InstallEvent::Progress(p)) => {
                if matches!(self.bash_install, InstallState::Running(_)) {
                    self.bash_install = InstallState::Running(p);
                }
            }
            Message::BashInstall(InstallEvent::Done(Ok(exe))) => {
                self.bash_install = InstallState::Idle;
                // The button says "Install and use": new terminals start bash.
                let command = bundled_bash::shell_option(&exe).command;
                if let Some(st) = &mut self.settings {
                    st.shell_input = command.clone();
                }
                self.dispatch(Command::SetDefaultShell(Some(command)));
                return detect_shells();
            }
            Message::BashInstall(InstallEvent::Done(Err(e))) => {
                tracing::warn!(error = %e, "installing TerminalRooster bash failed");
                self.bash_install = InstallState::Failed(e);
            }
            Message::Noop => {}
            Message::RemoveCodexHook => {
                if let Err(e) = self.core.remove_codex_hook() {
                    self.error = Some(format!("Could not remove the Codex hook: {e}"));
                }
                self.codex_hook = self.core.codex_hook_status();
            }
            Message::CloseSettings => {
                if self.settings.is_some() {
                    self.commit_shell();
                    self.settings = None;
                }
                return self.focus_active();
            }
            Message::OpenFolder(dir) => {
                if let Some(st) = &mut self.settings {
                    st.folder_error = None;
                }
                return crate::reveal::in_background(dir, true).map(Message::FolderOpened);
            }
            Message::FolderOpened(result) => {
                if let (Some(st), Err(e)) = (&mut self.settings, result) {
                    st.folder_error = Some(format!("Could not open the file manager: {e}"));
                }
            }
            Message::SystemTheme(mode) => {
                self.system_mode = mode;
                self.apply_theme();
            }
            Message::FollowSystem(on) => {
                self.prefs.follow_system = on;
                if !on {
                    // Keep showing what was visible.
                    self.prefs.theme = self.active_theme().name;
                }
                self.mark_prefs();
                self.apply_theme();
            }
            Message::UseTheme(name) => {
                if let Some(t) = self.themes.find(&name) {
                    let dark = t.is_dark();
                    if dark {
                        self.prefs.dark_theme = name.clone();
                    } else {
                        self.prefs.light_theme = name.clone();
                    }
                    // Picking a theme of the other tone than the system shows it
                    // now, instead of silently waiting for the system to switch.
                    if self.prefs.follow_system && dark != self.system_is_dark() {
                        self.prefs.follow_system = false;
                    }
                    self.prefs.theme = name;
                    self.mark_prefs();
                    self.apply_theme();
                }
            }
            Message::TerminalTheme(name) => {
                self.prefs.terminal_theme = name;
                self.mark_prefs();
                self.apply_theme();
            }
            Message::EditTheme(name) => {
                if let Some(st) = &mut self.settings {
                    st.inputs.clear();
                    st.picker = None;
                    st.name_error = None;
                    st.name_input = name.clone();
                    st.editing = Some(name);
                }
            }
            Message::DuplicateTheme(name) => {
                if let Some(mut copy) = self.themes.find(&name).cloned() {
                    copy.name = self.themes.unique_name(&format!("{name} copy"));
                    let new_name = copy.name.clone();
                    self.themes.custom.push(copy);
                    self.themes_dirty = true;
                    return self.update(Message::EditTheme(new_name));
                }
            }
            Message::DeleteTheme(name) => {
                if !self.themes.is_builtin(&name) {
                    self.themes.custom.retain(|t| t.name != name);
                    self.themes_dirty = true;
                    let p = &mut self.prefs;
                    for (slot, fallback) in [
                        (&mut p.theme, themes::DARK),
                        (&mut p.light_theme, themes::LIGHT),
                        (&mut p.dark_theme, themes::DARK),
                    ] {
                        if *slot == name {
                            *slot = fallback.into();
                        }
                    }
                    if p.terminal_theme.as_deref() == Some(name.as_str()) {
                        p.terminal_theme = None;
                    }
                    self.mark_prefs();
                    if let Some(st) = &mut self.settings
                        && st.editing.as_deref() == Some(name.as_str())
                    {
                        st.editing = None;
                        st.picker = None;
                    }
                    self.apply_theme();
                }
            }
            Message::ThemeNameInput(value) => self.rename_theme(value),
            Message::ThemeColor(field, value) => {
                let Some(st) = &mut self.settings else { return Task::none() };
                let hex = Hex::parse(&value);
                st.inputs.insert(field, value);
                if let Some(hex) = hex {
                    if let Some(p) = st.picker.as_mut().filter(|p| p.field == field) {
                        p.hsl = Hsl::of(hex);
                    }
                    self.set_theme_color(field, hex);
                }
            }
            Message::PickColor(field) => {
                if let Some(st) = &mut self.settings {
                    let theme = st.editing.as_deref().and_then(|n| self.themes.find(n));
                    st.picker = field.zip(theme).map(|(f, t)| ColorPicker::new(f, t.get(f)));
                }
            }
            Message::PickerHsl(hsl) => {
                let Some(st) = &mut self.settings else { return Task::none() };
                let Some(p) = &mut st.picker else { return Task::none() };
                p.hsl = hsl;
                let field = p.field;
                // The text field shows the theme's color again.
                st.inputs.remove(&field);
                self.set_theme_color(field, hsl.to_hex());
            }
            Message::FontSize(size) => {
                self.prefs.font_size = size.round().clamp(8.0, 32.0);
                self.mark_prefs();
            }
            Message::ShellInput(v) => {
                if let Some(st) = &mut self.settings {
                    st.shell_input = v;
                }
            }
            Message::ShellCommit => self.commit_shell(),
            Message::RecordShortcut(action) => {
                if let Some(st) = &mut self.settings {
                    st.recording = action;
                    st.shortcut_note = None;
                }
            }
            Message::RemoveShortcut(action, combo) => {
                let rest = self.keymap.combos(action).iter().copied().filter(|c| *c != combo).collect();
                self.set_shortcuts(action, rest);
                if let Some(st) = &mut self.settings {
                    st.shortcut_note = None;
                }
            }
            Message::ResetShortcuts(None) => {
                self.prefs.shortcuts.clear();
                self.shortcuts_changed();
                if let Some(st) = &mut self.settings {
                    st.recording = None;
                    st.shortcut_note = None;
                }
            }
            Message::ResetShortcuts(Some(action)) => {
                let notes = self.set_shortcuts(action, action.defaults());
                if let Some(st) = &mut self.settings {
                    st.shortcut_note = (!notes.is_empty()).then(|| notes.join(" "));
                }
            }
            Message::PaneSettings(pane) => {
                if let Some(p) = self.core.state().pane(pane) {
                    let args = p.agent.as_ref().map(|a| a.args.clone()).unwrap_or_default();
                    let resume = p.resume_command.clone().unwrap_or_default();
                    self.dialog = Some(Dialog::Pane { pane, resume, args, keep: p.keep_running });
                    return operation::focus(DIALOG_FIRST_INPUT);
                }
            }
            Message::AskDeleteProject(p) => self.dialog = Some(Dialog::ConfirmDeleteProject(p)),
            Message::AskDeleteTab(t) => self.dialog = Some(Dialog::ConfirmDeleteTab(t)),
            Message::DialogName(v) => {
                if let Some(Dialog::Project { name, .. }) = &mut self.dialog {
                    *name = v;
                }
            }
            Message::DialogDir(v) => {
                if let Some(Dialog::Project { dir, .. }) = &mut self.dialog {
                    *dir = v;
                }
            }
            Message::DialogShell(v) => {
                if let Some(Dialog::Project { shell, .. }) = &mut self.dialog {
                    *shell = v;
                }
            }
            Message::DialogResume(v) => {
                if let Some(Dialog::Pane { resume, .. }) = &mut self.dialog {
                    *resume = v;
                }
            }
            Message::DialogArgs(v) => {
                if let Some(Dialog::Pane { args, .. }) = &mut self.dialog {
                    *args = v;
                }
            }
            Message::DialogKeepRunning(on) => {
                if let Some(Dialog::Pane { keep, .. }) = &mut self.dialog {
                    *keep = on;
                }
            }
            Message::BrowseDir => {
                let start = match &self.dialog {
                    Some(Dialog::Project { dir, .. }) if !dir.is_empty() => Some(PathBuf::from(dir)),
                    _ => directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()),
                };
                return Task::perform(
                    async move {
                        let mut dlg = rfd::AsyncFileDialog::new().set_title("Project working directory");
                        if let Some(start) = start {
                            dlg = dlg.set_directory(start);
                        }
                        dlg.pick_folder().await.map(|h| h.path().to_path_buf())
                    },
                    Message::DirPicked,
                );
            }
            Message::DirPicked(path) => {
                if let (Some(path), Some(Dialog::Project { dir, name, .. })) = (path, &mut self.dialog) {
                    *dir = path.display().to_string();
                    fill_name_from_dir(name, dir);
                }
            }
            Message::DialogSubmit => return self.submit_dialog(),
            Message::DialogCancel => {
                match &self.dialog {
                    // git is making the worktree; the result moves the session.
                    Some(Dialog::Worktree(f)) if f.creating => return Task::none(),
                    Some(Dialog::RemoveWorktree(f)) if f.removing => return Task::none(),
                    Some(Dialog::Remotes(f)) if f.busy.is_some() => return Task::none(),
                    Some(Dialog::RemoveWorktree(_)) => {
                        self.cancel_remove();
                        return self.focus_active();
                    }
                    Some(Dialog::CodexHook) => {
                        // "Not now": don't ask again; Settings → Integrations offers it.
                        self.prefs.codex_hook_asked = true;
                        self.mark_prefs();
                    }
                    Some(Dialog::CodexTrust(h)) => {
                        let pane = h.pane();
                        self.core.close_scratch_terminal(pane);
                        self.codex_hook = self.core.codex_hook_status();
                    }
                    _ => {}
                }
                self.dialog = None;
                return self.focus_active();
            }
            Message::DialogStartAgent(kind) => {
                if let Some(Dialog::Pane { pane, resume, args, keep }) = self.dialog.take() {
                    self.dispatch(Command::SetResumeCommand { pane, command: Some(resume) });
                    // First: it starts the terminal again, and the agent in the new one.
                    self.dispatch(Command::SetKeepRunning { pane, on: keep });
                    self.prefs.agent_args.insert(kind.as_str().to_owned(), args.trim().to_owned());
                    self.mark_prefs();
                    self.dispatch(Command::StartAgent { pane, kind, args });
                    return self.focus_active();
                }
            }
            Message::DialogClearAgent => {
                if let Some(Dialog::Pane { pane, .. }) = &self.dialog {
                    let pane = *pane;
                    self.dispatch(Command::ClearAgent(pane));
                }
            }

            Message::StartRename(target) => {
                let s = self.core.state();
                let value = match target {
                    RenameTarget::Project(p) => s.project(p).map(|p| p.name.clone()),
                    RenameTarget::Group(g) => s.group(g).map(|g| g.name.clone()),
                    RenameTarget::Tab(t) => s.tab(t).map(|t| t.title.clone()),
                    RenameTarget::Note(n) => s.note(n).map(|n| n.title.clone()),
                };
                if let Some(value) = value {
                    self.rename = Some(Rename { target, value });
                    return Task::batch([operation::focus(RENAME_INPUT), operation::select_all(RENAME_INPUT)]);
                }
            }
            Message::RenameInput(v) => {
                if let Some(r) = &mut self.rename {
                    r.value = v;
                }
            }
            Message::RenameCommit => {
                let Some(r) = self.rename.take() else { return self.focus_active() };
                let cmd = match r.target {
                    RenameTarget::Project(project) => Command::RenameProject { project, name: r.value },
                    RenameTarget::Group(group) => Command::RenameGroup { group, name: r.value },
                    RenameTarget::Tab(tab) => Command::RenameTab { tab, title: r.value },
                    RenameTarget::Note(note) => Command::RenameNote { note, title: r.value },
                };
                self.dispatch(cmd);
                return self.after_rename(r.target);
            }
            Message::RenameCancel => {
                let Some(r) = self.rename.take() else { return self.focus_active() };
                return self.after_rename(r.target);
            }

            Message::FocusPane(pane) => {
                self.dispatch(Command::FocusPane(pane));
                return self.focus_active();
            }
            Message::PaneClicked(gp) => {
                if let Some((_, pane)) = self.grid_pane(gp) {
                    return self.update(Message::FocusPane(pane));
                }
            }
            Message::PaneDragged(pane_grid::DragEvent::Dropped { pane, target }) => {
                if let Some(tab) = self.active_tab_id()
                    && let Some(grid) = self.grids.get_mut(&tab)
                {
                    grid.drop(pane, target);
                    if let Some(layout) = layout_conv::layout_of(grid) {
                        self.dispatch(Command::SetLayout { tab, layout });
                    }
                }
            }
            Message::PaneDragged(_) => {}
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                if let Some(tab) = self.active_tab_id()
                    && let Some(grid) = self.grids.get_mut(&tab)
                {
                    grid.resize(split, ratio);
                    self.layout_dirty.insert(tab);
                }
            }
            Message::Split(pane, axis) => {
                self.flush_pending();
                if let Some(Outcome::Pane(_)) = self.dispatch(Command::SplitPane { pane, axis })
                    && let Some(tab) = self.core.state().pane(pane).map(|p| p.tab)
                {
                    self.rebuild_grid(tab);
                }
                return self.focus_active();
            }
            Message::ClosePane(pane) => {
                self.flush_pending();
                let tab = self.core.state().pane(pane).map(|p| p.tab);
                if self.dispatch(Command::ClosePane(pane)).is_some()
                    && let Some(tab) = tab
                {
                    self.rebuild_grid(tab);
                }
                return self.focus_active();
            }
            Message::RestartPane(pane) => {
                self.dispatch(Command::RestartPane(pane));
            }
            Message::OpenInWindow(pane) => {
                let s = self.core.state();
                let title = s
                    .pane(pane)
                    .and_then(|p| s.tab(p.tab))
                    .and_then(|t| s.project(t.project).map(|p| format!("{} · {}", p.name, t.title)))
                    .unwrap_or_else(|| crate::DISPLAY_NAME.to_owned());
                if let Some(host) = self.core.host_command()
                    && let Err(e) = crate::host_exe::open_window(host, pane, &title)
                {
                    tracing::warn!(%pane, error = %e, "could not open a window for the terminal");
                    self.error = Some(format!("Could not open a window: {e}"));
                }
            }
            Message::BringBack(pane) => self.core.bring_back(pane),
            Message::StartAgent(pane, kind) => {
                let args = self.prefs.agent_args.get(kind.as_str()).cloned().unwrap_or_default();
                self.dispatch(Command::StartAgent { pane, kind, args });
                self.dispatch(Command::FocusPane(pane));
                return self.focus_active();
            }

            Message::TogglePanel(tab) => {
                if self.prefs.panel_open && self.shown_side_tab() == tab {
                    return self.update(Message::ClosePanel);
                }
                return self.update(Message::ShowPanel(tab));
            }
            Message::ShowPanel(tab) => {
                self.prefs.panel_open = true;
                self.prefs.side_tab = tab;
                self.mark_prefs();
                return match tab {
                    SideTab::Tasks => operation::focus(TASK_INPUT),
                    // Looking at the log or the files doesn't take the
                    // keyboard from the terminal.
                    SideTab::Git => Task::batch([self.refresh_log(), self.focus_active()]),
                    SideTab::Files => Task::batch([self.refresh_files(), self.focus_active()]),
                    // Opened to type or paste into.
                    SideTab::Notes => operation::focus(NOTE_EDITOR),
                };
            }
            Message::ClosePanel => {
                self.prefs.panel_open = false;
                self.mark_prefs();
                return self.focus_active();
            }
            Message::Files(msg) => return self.update_files(msg),
            Message::Editor(msg) => return self.update_editor(msg),
            Message::Notes(msg) => return self.update_notes(msg),
            Message::FileEditor(editor) => {
                self.prefs.file_editor = editor;
                self.mark_prefs();
            }
            Message::EditorCommand(command) => {
                self.prefs.editor_command = command;
                self.mark_prefs();
            }
            Message::NewTaskInput(v) => self.new_task = v,
            Message::AddTask => {
                let title = std::mem::take(&mut self.new_task);
                if let Some(project) = self.core.state().selected_project
                    && !title.trim().is_empty()
                {
                    let tab = if self.prefs.tasks_this_tab_only { self.active_tab_id() } else { None };
                    self.dispatch(Command::CreateTask { project, title, tab });
                }
            }
            Message::TaskDone(task, done) => {
                self.dispatch(Command::SetTaskDone { task, done });
            }
            Message::TaskTab(task, choice) => {
                self.dispatch(Command::AssignTask { task, tab: choice.id });
            }
            Message::DeleteTask(task) => {
                if self.task_edit.as_ref().is_some_and(|(t, _)| *t == task) {
                    self.task_edit = None;
                }
                self.dispatch(Command::DeleteTask(task));
            }
            Message::EditTask(task) => {
                if let Some(t) = self.core.state().task(task) {
                    self.task_edit = Some((task, t.title.clone()));
                    return operation::focus(TASK_EDIT_INPUT);
                }
            }
            Message::TaskEditInput(v) => {
                if let Some((_, value)) = &mut self.task_edit {
                    *value = v;
                }
            }
            Message::TaskEditCommit => {
                if let Some((task, title)) = self.task_edit.take() {
                    let notes = self.core.state().task(task).map(|t| t.notes.clone()).unwrap_or_default();
                    self.dispatch(Command::EditTask { task, title, notes });
                }
            }
            Message::TaskEditCancel => self.task_edit = None,
            Message::ShowDone(v) => {
                self.prefs.show_done = v;
                self.mark_prefs();
            }
            Message::ThisTabOnly(v) => {
                self.prefs.tasks_this_tab_only = v;
                self.mark_prefs();
            }
            Message::TaskGrab(task) => {
                self.task_drag = Some(TaskDrag { task, origin: None, cursor: None, moving: false, over: None });
            }
            Message::TaskDragMove(p) => {
                if let Some(d) = &mut self.task_drag {
                    let origin = *d.origin.get_or_insert(p);
                    d.cursor = Some(p);
                    d.moving |= origin.distance(p) >= TASK_DRAG_THRESHOLD;
                }
            }
            Message::TaskDragOver(target, on) => {
                if let Some(d) = &mut self.task_drag {
                    if on {
                        d.over = Some(target);
                    } else if d.over == Some(target) {
                        d.over = None;
                    }
                }
            }
            Message::TaskDrop(target) => {
                if let Some(drag) = self.task_drag.take().filter(|d| d.moving)
                    && let Some(tab) = self.drop_tab(target)
                    && self.core.state().task(drag.task).is_some_and(|t| t.tab != Some(tab))
                {
                    self.dispatch(Command::AssignTask { task: drag.task, tab: Some(tab) });
                }
            }
            Message::TaskDragEnd => self.task_drag = None,
            Message::GitPoll => {
                // The changes on screen follow what agents do to the files.
                let changes = if self.window_focused { self.poll_changes() } else { Task::none() };
                if !self.git_polling {
                    self.git_polling = true;
                    return Task::batch([self.read_git(), changes]);
                }
                return changes;
            }
            Message::GitStatus { statuses, gone } => {
                self.git_polling = false;
                // Before the statuses: a worktree is told apart from a plain
                // folder by what git said before it went.
                self.folders_gone(gone);
                self.git.extend(statuses);
                // The session on screen may be in another repository now.
                return self.refresh_log();
            }
            Message::Worktree(msg) => return self.update_worktree(msg),
            Message::RemoveWorktree(msg) => return self.update_remove(msg),
            Message::LogRead(read) => return self.log_read(read),
            Message::GitSync(op) => return self.start_sync(op),
            Message::GitSynced(worktree, result) => return self.sync_done(worktree, result),
            Message::PullRebase(on) => {
                self.prefs.pull_rebase = on;
                self.mark_prefs();
            }
            Message::DismissSync => self.git_sync = None,
            Message::Remotes(msg) => return self.update_remotes(msg),
            Message::GitView(msg) => return self.update_git_view(msg),
            Message::LogAllBranches(all) => {
                self.prefs.log_all_branches = all;
                self.mark_prefs();
                return self.refresh_log();
            }
            Message::CopyText(text) => return iced::clipboard::write(text),
            Message::DismissError => self.error = None,
            Message::DismissNotice => self.notice = None,
        }
        Task::none()
    }

    fn submit_dialog(&mut self) -> Task<Message> {
        let Some(dialog) = self.dialog.take() else { return Task::none() };
        match dialog {
            Dialog::Project { editing, name, dir, shell, .. } => {
                let path = expand_home(dir.trim());
                let problem = if name.trim().is_empty() {
                    Some("Give the project a name.")
                } else if dir.trim().is_empty() {
                    Some("Choose a working directory.")
                } else if !path.is_dir() {
                    Some("That directory does not exist.")
                } else {
                    None
                };
                if let Some(problem) = problem {
                    self.dialog = Some(Dialog::Project { editing, name, dir, shell, error: Some(problem.into()) });
                    return Task::none();
                }
                let shell = Some(shell).filter(|s| !s.trim().is_empty());
                match editing {
                    None => {
                        self.dispatch(Command::CreateProject { name, working_dir: path, shell });
                        return self.after_select();
                    }
                    Some(project) => {
                        self.dispatch(Command::RenameProject { project, name });
                        self.dispatch(Command::SetProjectDir { project, working_dir: path });
                        self.dispatch(Command::SetProjectShell { project, shell });
                    }
                }
            }
            Dialog::ConfirmDeleteProject(project) => {
                let tabs: Vec<TabId> = self.core.state().tabs_of(project).map(|t| t.id).collect();
                self.dispatch(Command::DeleteProject(project));
                for t in tabs {
                    self.grids.remove(&t);
                    self.activity.remove(&t);
                    self.drop_session_files(t);
                }
                return self.after_select();
            }
            Dialog::ConfirmDeleteTab(tab) => {
                self.dispatch(Command::DeleteTab(tab));
                self.grids.remove(&tab);
                self.activity.remove(&tab);
                self.drop_session_files(tab);
                return self.after_select();
            }
            Dialog::Pane { pane, resume, keep, .. } => {
                self.dispatch(Command::SetResumeCommand { pane, command: Some(resume) });
                self.dispatch(Command::SetKeepRunning { pane, on: keep });
            }
            Dialog::CodexHook => return self.update(Message::AddCodexHook),
            Dialog::CodexTrust(h) => {
                self.dialog = Some(Dialog::CodexTrust(h));
                return self.update(Message::DialogCancel);
            }
            Dialog::Renamed(renamed) if renamed.codex_hook_updated => return self.update(Message::TrustCodexHook),
            Dialog::Renamed(_) => {}
            Dialog::Worktree(form) => {
                self.dialog = Some(Dialog::Worktree(form));
                return self.update_worktree(WorktreeMsg::Submit);
            }
            Dialog::RemoveWorktree(form) => {
                self.dialog = Some(Dialog::RemoveWorktree(form));
                return self.update_remove(RemoveMsg::Confirm);
            }
            Dialog::Remotes(form) => {
                self.dialog = Some(Dialog::Remotes(form));
                return self.update_remotes(RemotesMsg::Add);
            }
            Dialog::CloseFile { doc, .. } => return self.save_doc(doc, true, false),
            Dialog::SaveConflict { doc, .. } => return self.update(Message::Editor(EditorMsg::SaveAnyway(doc))),
            Dialog::CloseAllNotes => return self.update(Message::Notes(NotesMsg::CloseAll)),
            Dialog::ConfirmQuit { dont_ask, .. } => {
                if dont_ask {
                    self.prefs.confirm_quit = false;
                    self.mark_prefs();
                }
                return self.quit();
            }
        }
        self.focus_active()
    }

    /// Saves everything, stops the terminals and exits.
    fn quit(&mut self) -> Task<Message> {
        self.flush_pending();
        self.core.shutdown();
        self.closing = true;
        iced::exit()
    }

    /// Terminals running programs, in sidebar order, for asking before quitting.
    pub(crate) fn running_terminals(&self) -> Vec<RunningTerminal> {
        let s = self.core.state();
        let mut out: Vec<(usize, RunningTerminal)> = self
            .core
            .running_programs()
            .into_iter()
            .filter_map(|(pane, found)| {
                let tab = s.pane(pane)?.tab;
                let rank = s.tabs.iter().position(|t| t.id == tab).unwrap_or(usize::MAX);
                let working = self.core.terminal(pane).is_some_and(|h| h.agent_working());
                let programs = found
                    .iter()
                    .map(|p| match (p.agent, p.started_by) {
                        (Some(_), _) if working => format!("{} (working)", p.name),
                        (None, Some(by)) => format!("{} (started by {by})", p.name),
                        _ => p.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let agent = found.iter().any(|p| p.agent.is_some());
                Some((rank, RunningTerminal { pane, place: self.place_of(pane)?, programs, agent }))
            })
            .collect();
        out.sort_by_key(|(rank, t)| (*rank, t.pane));
        out.into_iter().map(|(_, t)| t).collect()
    }

    /// Where a terminal is, for notifications: "Session 2 · my-service".
    pub(crate) fn place_of(&self, pane: PaneId) -> Option<String> {
        let s = self.core.state();
        let tab = s.tab(s.pane(pane)?.tab)?;
        let project = s.project(tab.project)?;
        Some(format!("{} · {}", tab.title, project.name))
    }

    /// The terminal is on screen: in the visible session, settings closed, no
    /// file shown in place of the terminals.
    fn pane_visible(&self, pane: PaneId) -> bool {
        self.settings.is_none()
            && self.shown_doc().is_none()
            && self.core.state().pane(pane).map(|p| p.tab) == self.active_tab_id()
    }

    /// An alert from a terminal: mark it, and say so as loudly as the user chose.
    fn on_notice(&mut self, pane: PaneId, notice: Notice) -> Task<Message> {
        let level = self.prefs.level(notice.category);
        // Scratch terminals (the Codex window) are not in the state.
        let Some(place) = self.place_of(pane) else { return Task::none() };
        if level == Level::Off || self.attended == Some(pane) {
            return Task::none();
        }
        tracing::debug!(%pane, headline = %notice.headline, "notification");
        let category = notice.category;
        let notices = self.notices.entry(pane).or_default();
        if !notices.add(notice.clone()) {
            // Repeats an alert from a moment ago; keep its pop-up's text current.
            let detail = notices.iter().find(|n| n.category == category).and_then(|n| n.detail.clone());
            if let Some(t) = self.toasts.iter_mut().find(|t| t.pane == pane && t.notice.category == category) {
                t.notice.detail = detail;
            }
            return Task::none();
        }
        if level != Level::Prominent {
            return Task::none();
        }
        if !self.window_focused {
            let body = notice.detail.iter().chain([&place]).cloned().collect::<Vec<_>>().join(" · ");
            notify::desktop(notice.headline.clone(), body, pane, self.clicks.0.clone());
            if let Some(id) = self.window_id {
                return window::request_user_attention(id, Some(window::UserAttention::Informational));
            }
        } else if !self.pane_visible(pane) {
            self.toasts.retain(|t| t.pane != pane);
            self.toasts.push(Toast { pane, notice, place, shown: std::time::Instant::now() });
            if self.toasts.len() > 4 {
                self.toasts.remove(0);
            }
        }
        Task::none()
    }

    /// Brings a terminal into view and focuses it, and the window too.
    fn show_pane(&mut self, pane: PaneId) -> Task<Message> {
        let Some(tab) = self.core.state().pane(pane).map(|p| p.tab) else { return Task::none() };
        if self.settings.is_some() {
            self.commit_shell();
            self.settings = None;
        }
        self.rename = None;
        self.toasts.retain(|t| t.pane != pane);
        self.dispatch(Command::SelectTab(tab));
        self.dispatch(Command::FocusPane(pane));
        self.show_terminals();
        let focus = self.after_select();
        match self.window_id {
            Some(id) => Task::batch([focus, window::gain_focus(id)]),
            None => focus,
        }
    }

    /// Works out which terminal the user is looking at (see `attended`): tells
    /// programs about focus changes, and clears that terminal's alerts.
    fn sync_attention(&mut self) {
        if self.closing {
            return;
        }
        // A file shown in place of the terminals hides them; a terminal
        // editor showing it is looked at instead (so Neovim hears of focus
        // and can pick up changes agents made meanwhile).
        let attended = match self.shown_doc() {
            Some(_) => self.shown_editor_pane(),
            None => self.core.state().active_tab().and_then(|t| t.focused_pane),
        }
        .filter(|_| self.window_focused && self.settings.is_none());
        if attended != self.attended {
            if let Some(h) = self.attended.and_then(|p| self.handle_of(p)) {
                h.focus_changed(false);
            }
            if let Some(h) = attended.and_then(|p| self.handle_of(p)) {
                h.focus_changed(true);
            }
            self.attended = attended;
        }
        if let Some(p) = attended
            && self.notices.remove(&p).is_some()
        {
            self.toasts.retain(|t| t.pane != p);
        }
        let s = self.core.state();
        self.notices.retain(|p, n| s.pane(*p).is_some() && n.first().is_some());
        self.toasts.retain(|t| s.pane(t.pane).is_some());
    }

    fn commit_shell(&mut self) {
        let Some(st) = &self.settings else { return };
        let wanted = Some(st.shell_input.trim().to_owned()).filter(|s| !s.is_empty());
        if wanted != self.core.state().settings.default_shell {
            self.dispatch(Command::SetDefaultShell(wanted));
        }
    }

    /// Sets one color of the custom theme being edited, applying it right away if it's in use.
    fn set_theme_color(&mut self, field: ColorField, hex: Hex) {
        let Some(name) = self.settings.as_ref().and_then(|st| st.editing.clone()) else { return };
        let Some(t) = self.themes.custom_mut(&name) else { return };
        if t.get(field) == hex {
            return;
        }
        t.set(field, hex);
        self.themes_dirty = true;
        if self.active_theme().name == name || self.terminal_theme().name == name {
            self.apply_theme();
        }
    }

    /// Renames the edited custom theme when the new name is valid and free.
    fn rename_theme(&mut self, value: String) {
        let Some(st) = &mut self.settings else { return };
        st.name_input = value.clone();
        let Some(old) = st.editing.clone() else { return };
        let new = value.trim().to_owned();
        st.name_error = if new.is_empty() {
            Some("A theme needs a name.".into())
        } else if new != old && self.themes.find(&new).is_some() {
            Some("Another theme already has this name.".into())
        } else {
            None
        };
        if st.name_error.is_some() || new == old {
            return;
        }
        st.editing = Some(new.clone());
        if let Some(t) = self.themes.custom_mut(&old) {
            t.name = new.clone();
        }
        let p = &mut self.prefs;
        for slot in [&mut p.theme, &mut p.light_theme, &mut p.dark_theme] {
            if *slot == old {
                *slot = new.clone();
            }
        }
        if p.terminal_theme.as_deref() == Some(old.as_str()) {
            p.terminal_theme = Some(new);
        }
        self.themes_dirty = true;
        self.mark_prefs();
        self.apply_theme();
    }

    /// Adds the keys just pressed as a shortcut for `action` (from Settings).
    fn record_shortcut(&mut self, action: Action, combo: Combo) {
        let Some(st) = &mut self.settings else { return };
        if !combo.leaves_typing_alone() {
            st.shortcut_note = Some(format!(
                "{combo} would get in the way of typing. Hold Ctrl, Alt or {} as well (for keys like Enter or the \
                 arrows, Shift is enough), or press Escape to cancel.",
                crate::keymap::LOGO
            ));
            return;
        }
        st.recording = None;
        if self.keymap.owner(&combo) == Some(action) {
            st.shortcut_note = Some(format!("{combo} already does this."));
            return;
        }
        let mut combos = self.keymap.combos(action).to_vec();
        combos.push(combo);
        let notes = self.set_shortcuts(action, combos);
        if let Some(st) = &mut self.settings {
            st.shortcut_note = (!notes.is_empty()).then(|| notes.join(" "));
        }
    }

    /// Gives `action` exactly these shortcuts, taking them away from any other
    /// action that had them. Returns a note for each one taken away.
    fn set_shortcuts(&mut self, action: Action, combos: Vec<Combo>) -> Vec<String> {
        let taken = self.keymap.assign(&mut self.prefs.shortcuts, action, combos);
        self.shortcuts_changed();
        taken.into_iter().map(|(c, other)| format!("{c} no longer does “{}”.", other.label())).collect()
    }

    fn shortcuts_changed(&mut self) {
        self.keymap = Arc::new(Keymap::new(&self.prefs.shortcuts));
        self.mark_prefs();
    }

    fn shortcut(&mut self, action: Action) -> Task<Message> {
        if self.dialog.is_some() || self.settings.is_some() {
            return Task::none();
        }
        // With a file in place of the terminals, terminal shortcuts don't act
        // on terminals out of sight: closing closes the file, and splitting or
        // moving between terminals brings them back first.
        if let Some(doc) = self.shown_doc().map(|d| d.id) {
            match action {
                Action::CloseTerminal => return self.update(Message::Editor(EditorMsg::Close(doc))),
                Action::SplitRight
                | Action::SplitDown
                | Action::FocusLeft
                | Action::FocusRight
                | Action::FocusUp
                | Action::FocusDown => self.show_terminals(),
                _ => {}
            }
        }
        let s = self.core.state();
        let focused = s.active_tab().and_then(|t| t.focused_pane);
        match action {
            Action::NewProject => return self.update(Message::NewProject),
            Action::NewSession => return self.update(Message::NewTab),
            Action::SplitRight => {
                if let Some(p) = focused {
                    return self.update(Message::Split(p, Axis::Vertical));
                }
            }
            Action::SplitDown => {
                if let Some(p) = focused {
                    return self.update(Message::Split(p, Axis::Horizontal));
                }
            }
            Action::CloseTerminal => {
                if let Some(p) = focused {
                    return self.update(Message::ClosePane(p));
                }
            }
            Action::NextSession | Action::PrevSession => {
                if let Some(project) = s.selected_project {
                    let tabs: Vec<TabId> = s.tabs_of(project).map(|t| t.id).collect();
                    if let Some(next) = cycle(&tabs, self.active_tab_id(), action == Action::NextSession) {
                        return self.update(Message::SelectTab(next));
                    }
                }
            }
            Action::NextProject | Action::PrevProject => {
                // Through the projects the list shows, while searching.
                let projects: Vec<ProjectId> = self.listed_projects().map(|p| p.id).collect();
                if let Some(next) = cycle(&projects, s.selected_project, action == Action::NextProject) {
                    return self.update(Message::SelectProject(next));
                }
            }
            Action::SearchProjects => {
                self.rename = None;
                // Selected, so typing replaces the last search.
                return Task::batch([operation::focus(PROJECT_SEARCH), operation::select_all(PROJECT_SEARCH)]);
            }
            Action::MoveSessionUp | Action::MoveSessionDown => {
                if let Some(project) = s.selected_project
                    && let Some(tab) = self.active_tab_id()
                {
                    let tabs: Vec<TabId> = s.tabs_of(project).map(|t| t.id).collect();
                    if let Some(to) = step(&tabs, tab, action == Action::MoveSessionDown) {
                        return self.update(Message::MoveTab(tab, to));
                    }
                }
            }
            Action::MoveProjectUp | Action::MoveProjectDown => {
                if let Some(project) = s.selected_project {
                    // One place up or down in the list as shown, while searching.
                    let all = groups::entries(&s.projects);
                    let shown = groups::entries(self.listed_projects());
                    if let Some(cmd) = groups::step(&all, &shown, project, action == Action::MoveProjectDown) {
                        return self.update(Message::ArrangeProjects(cmd));
                    }
                }
            }
            Action::FocusLeft | Action::FocusRight | Action::FocusUp | Action::FocusDown => {
                let d = match action {
                    Action::FocusLeft => pane_grid::Direction::Left,
                    Action::FocusRight => pane_grid::Direction::Right,
                    Action::FocusUp => pane_grid::Direction::Up,
                    _ => pane_grid::Direction::Down,
                };
                let (Some(tab), Some(pane)) = (self.active_tab_id(), focused) else { return Task::none() };
                let Some(grid) = self.grids.get(&tab) else { return Task::none() };
                let Some(gp) = layout_conv::find(grid, pane) else { return Task::none() };
                if let Some(target) = grid.adjacent(gp, d).and_then(|g| grid.get(g)).copied() {
                    return self.update(Message::FocusPane(target));
                }
            }
            Action::ToggleTasks => return self.update(Message::TogglePanel(SideTab::Tasks)),
            Action::ToggleFiles => return self.update(Message::TogglePanel(SideTab::Files)),
            Action::ToggleNotes => return self.update(Message::TogglePanel(SideTab::Notes)),
            Action::FontBigger | Action::FontSmaller | Action::FontReset => {
                self.prefs.font_size = match action {
                    Action::FontBigger => (self.prefs.font_size + 1.0).min(32.0),
                    Action::FontSmaller => (self.prefs.font_size - 1.0).max(8.0),
                    _ => theme::DEFAULT_FONT_SIZE,
                };
                self.mark_prefs();
            }
            // Handled by the focused terminal.
            Action::Copy
            | Action::Paste
            | Action::ScrollPageUp
            | Action::ScrollPageDown
            | Action::ScrollTop
            | Action::ScrollBottom => {}
        }
        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message> {
        let base: Element<'_, Message> = if self.settings.is_some() {
            self.view_settings()
        } else {
            let terminals = self.drop_zone(self.view_main());
            let mut main =
                row![self.view_projects(), ui::vertical_divider(), self.view_tabs(), ui::vertical_divider(), terminals]
                    .height(Length::Fill);
            if self.prefs.panel_open && self.core.state().selected_project.is_some() {
                main = main.push(ui::vertical_divider()).push(self.view_side_panel());
            }
            // The second layer shows a dragged task next to the pointer.
            container(stack![main, self.view_task_drag()]).width(Length::Fill).height(Length::Fill).into()
        };
        let base: Element<'_, Message> =
            if self.toasts.is_empty() { base } else { stack![base, self.view_toasts()].into() };
        match &self.dialog {
            Some(d) => stack![base, self.view_dialog(d)].into(),
            None => column![base].into(),
        }
    }
}

/// Runs an install of TerminalRooster's own bash or Neovim on a thread,
/// passing on its progress and then how it ended.
fn run_install(
    install: impl FnOnce(&dyn Fn(bundled_bash::Progress)) -> termroost_core::Result<PathBuf> + Send + 'static,
) -> async_channel::Receiver<InstallEvent> {
    let (tx, rx) = async_channel::unbounded();
    std::thread::spawn(move || {
        let report = tx.clone();
        let result = install(&move |p| {
            let _ = report.send_blocking(InstallEvent::Progress(p));
        });
        let _ = tx.send_blocking(InstallEvent::Done(result.map_err(|e| e.to_string())));
    });
    rx
}

/// Looks for installed shells on a thread (it may wait on `wsl.exe`).
fn detect_shells() -> Task<Message> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(shell::detect_shells());
    });
    Task::perform(async move { rx.recv().await.unwrap_or_default() }, Message::ShellsDetected)
}

/// The entry of `shells` whose command line is `command`, for showing the
/// current choice in a shell picker.
pub(crate) fn shell_matching<'a>(shells: &'a [ShellOption], command: &str) -> Option<&'a ShellOption> {
    let wanted = shell::split_command_line(command)?;
    shells.iter().find(|o| shell::split_command_line(&o.command).as_ref() == Some(&wanted))
}

/// The item after (or before) `current`, wrapping around. When `current` isn't
/// in the list (say, a project the search hides), the first (or last) one.
fn cycle<T: Copy + PartialEq>(items: &[T], current: Option<T>, forward: bool) -> Option<T> {
    let n = items.len();
    let i = match current.and_then(|c| items.iter().position(|x| *x == c)) {
        Some(i) if forward => (i + 1) % n,
        Some(i) => (i + n - 1) % n,
        None if forward => 0,
        None => n.checked_sub(1)?,
    };
    items.get(i).copied()
}

/// The index `item` gets when moved one place down (or up); `None` at the end
/// of the list, where it stays.
fn step<T: PartialEq>(items: &[T], item: T, down: bool) -> Option<usize> {
    let i = items.iter().position(|x| *x == item)?;
    if down { (i + 1 < items.len()).then_some(i + 1) } else { i.checked_sub(1) }
}

pub(crate) fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~")
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
        && let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
    {
        return home.join(rest.trim_start_matches(['/', '\\']));
    }
    PathBuf::from(p)
}

/// Suggests the folder name as project name while the name is still empty.
fn fill_name_from_dir(name: &mut String, dir: &str) {
    if name.trim().is_empty()
        && let Some(base) = PathBuf::from(dir.trim_end_matches(['/', '\\'])).file_name()
    {
        *name = base.to_string_lossy().into_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_one_place_stops_at_the_ends() {
        let items = ['a', 'b', 'c'];
        assert_eq!(step(&items, 'a', true), Some(1));
        assert_eq!(step(&items, 'b', true), Some(2));
        assert_eq!(step(&items, 'c', true), None);
        assert_eq!(step(&items, 'c', false), Some(1));
        assert_eq!(step(&items, 'a', false), None);
        assert_eq!(step(&items, 'x', false), None);
    }

    #[test]
    fn cycling_wraps_and_starts_at_an_end() {
        let items = ['a', 'b', 'c'];
        assert_eq!(cycle(&items, Some('a'), true), Some('b'));
        assert_eq!(cycle(&items, Some('c'), true), Some('a'));
        assert_eq!(cycle(&items, Some('a'), false), Some('c'));
        assert_eq!(cycle(&items, Some('x'), true), Some('a'), "not listed: the first");
        assert_eq!(cycle(&items, None, false), Some('c'), "not listed: the last");
        assert_eq!(cycle(&[] as &[char], None, true), None);
        assert_eq!(cycle(&[] as &[char], None, false), None);
    }
}
