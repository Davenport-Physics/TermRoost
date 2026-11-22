//! Files opened from the Files tab. Each session has its own open files, and
//! one of them can be shown in place of the session's terminals. A commit
//! picked in the Git tab opens among them too (see `git_view`). Pictures
//! open in a viewer instead of the editor; an SVG can be switched between
//! the two.

mod disk;
mod external;
mod highlight;
pub(crate) mod history;
mod keys;
pub(crate) mod picture;
mod saved;
mod text;
mod view;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use iced::Task;
use iced::widget::operation;
use iced::widget::text_editor::{self, Action, Cursor, Edit, Position};
use serde::{Deserialize, Serialize};
use termroost_core::{GridSize, PaneId, TabId, TerminalHandle};

use crate::app::{App, Dialog, Message, RunningTerminal};
use crate::git_view::{ChangesDoc, CommitDoc};
use disk::{FileText, OnDisk, Reaction, SaveError, Stamp};
use history::{History, Snapshot};
use picture::{FilePicture, PictureDoc, Zoom};
pub use text::{Format, Indent};

/// What files open in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileEditor {
    /// TerminalRooster's own editor.
    #[default]
    Builtin,
    /// Neovim or Vim (or the editor command set), in a terminal.
    Terminal,
}

/// Widget id of the file editor, for focusing it.
pub const FILE_EDITOR: &str = "file-editor";

/// An open file, unique across sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocId(u64);

/// The files a session has open.
#[derive(Debug, Default)]
pub struct SessionFiles {
    pub open: Vec<OpenFile>,
    /// The file shown in place of the terminals; `None` shows the terminals.
    pub shown: Option<DocId>,
}

#[derive(Debug)]
pub struct OpenFile {
    pub id: DocId,
    pub path: PathBuf,
    pub kind: DocKind,
}

impl OpenFile {
    /// It has changes not written to disk yet.
    pub fn modified(&self) -> bool {
        matches!(&self.kind, DocKind::Text(t) if t.modified())
    }

    /// Its name in the strip of open files.
    pub fn label(&self) -> String {
        match &self.kind {
            DocKind::Commit(c) => format!("Commit {}", c.short()),
            DocKind::Changes(_) => "Changes".to_owned(),
            _ => self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into()),
        }
    }

    /// It is a file (not a commit or the changes), so it is opened again
    /// after a restart.
    pub fn is_file(&self) -> bool {
        !matches!(self.kind, DocKind::Commit(_) | DocKind::Changes(_))
    }
}

#[derive(Debug)]
pub enum DocKind {
    /// Being read.
    Loading,
    Text(Box<TextDoc>),
    Picture(Box<PictureDoc>),
    /// Open before a restart, not read (or its editor not started) yet.
    Restored(OpenAs),
    /// A terminal editor (Neovim, Vim) has it open.
    Terminal {
        handle: TerminalHandle,
        /// The editor's name, e.g. `nvim`.
        name: String,
        started: Instant,
    },
    /// A commit of the repository at `path`.
    Commit(Box<CommitDoc>),
    /// The uncommitted changes of the repository at `path`.
    Changes(Box<ChangesDoc>),
}

/// What a file was shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAs {
    /// The built-in editor.
    Text,
    /// Neovim or Vim (or the editor command set).
    Terminal,
    Picture,
}

/// A text file in the built-in editor.
pub struct TextDoc {
    pub content: text_editor::Content,
    pub format: Format,
    pub indent: Indent,
    /// Changes the text so far, as a number unique to this state of it.
    pub version: u64,
    /// The `version` on disk.
    pub saved_version: u64,
    /// Being written now.
    pub saving: bool,
    /// Close it once it is written (Save in the close dialog).
    close_after_save: bool,
    pub history: History,
    /// The file on disk when it was last read or written.
    disk: Stamp,
    /// The file changed on disk in a way that needs the user to choose.
    pub conflict: Option<Conflict>,
}

/// How the file on disk went its own way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// It changed while there are unsaved changes here (its new stamp).
    Changed(Stamp),
    Deleted,
}

impl TextDoc {
    pub fn modified(&self) -> bool {
        self.version != self.saved_version
    }

    /// Before an edit of `kind`: keeps the text for undo if the edit starts a new step.
    fn keep_for_undo(&mut self, kind: history::Kind) {
        self.history.before_edit(kind, &self.content, self.version);
    }

    /// Shows `file` as read from disk now, with nothing left unsaved. The
    /// text before can be had back with undo.
    fn replace_text(&mut self, file: FileText, version: u64) {
        self.history.push(Snapshot::of(&self.content, self.version));
        self.history.close_step();
        let at = self.content.cursor().position;
        let text = file.decoded.text;
        let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
        Snapshot { text: lines.join("\n"), cursor: (at.line, at.column), version }.restore(&mut self.content);
        self.format = file.decoded.format;
        self.indent = text::detect_indent(&text);
        self.disk = file.stamp;
        self.version = version;
        self.saved_version = version;
        self.conflict = None;
    }
}

impl std::fmt::Debug for TextDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the text: logs must not show what is in people's files.
        f.debug_struct("TextDoc")
            .field("lines", &self.content.line_count())
            .field("format", &self.format)
            .field("version", &self.version)
            .field("saved_version", &self.saved_version)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum EditorMsg {
    /// Show one of the visible session's files, or its terminals (`None`).
    Show(Option<DocId>),
    /// Close a file, asking first if it has unsaved changes. Neovim or Vim
    /// is asked to quit instead, and the file closes when it does.
    Close(DocId),
    /// Neovim or Vim was asked to quit a moment ago: if it hasn't (it has
    /// changes not saved, and says so), show it.
    QuitCheck(DocId),
    /// Close a file without saving its changes.
    Discard(DocId),
    /// A file was read (or can't be edited, and why).
    Loaded(DocId, Result<FileText, String>),
    /// Something done in the editor: typing, moving, selecting, scrolling.
    Action(DocId, text_editor::Action),
    /// Tab (`false`) or Shift+Tab (`true`).
    Indent(DocId, bool),
    Undo(DocId),
    Redo(DocId),
    Save(DocId),
    /// Save over a file that changed on disk, or write a deleted one again.
    SaveAnyway(DocId),
    /// A file was written as it was at that version.
    Saved(DocId, u64, Result<Stamp, SaveError>),
    /// Time to look at the open files on disk.
    DiskPoll,
    /// How each file looked on disk, compared to the stamp it had then.
    DiskChecked(Vec<(DocId, Stamp, OnDisk)>),
    /// Show what is on disk instead of the unsaved changes here.
    Reload(DocId),
    Reloaded(DocId, Result<FileText, String>),
    /// Keep the changes here over what changed on disk (until saved).
    KeepMine(DocId),
    /// A picture was read (or can't be shown, and why).
    PictureLoaded(DocId, Result<FilePicture, String>),
    /// A picture that changed on disk was read again.
    PictureReloaded(DocId, Result<FilePicture, String>),
    /// A picture was zoomed or moved.
    Zoom(DocId, Zoom),
    /// Show an SVG's text instead of its picture.
    AsText(DocId),
    /// Show an SVG's picture instead of its text.
    AsPicture(DocId),
}

impl EditorMsg {
    /// Whether the message may be logged: some carry what is in a file or
    /// what the user types. Zooming isn't, as it comes with every move of
    /// the pointer while a picture is dragged.
    pub fn loggable(&self) -> bool {
        !matches!(
            self,
            EditorMsg::Loaded(..)
                | EditorMsg::Action(..)
                | EditorMsg::Reloaded(..)
                | EditorMsg::PictureLoaded(..)
                | EditorMsg::PictureReloaded(..)
                | EditorMsg::Zoom(..)
        )
    }
}

impl App {
    /// The open files of the visible session.
    pub(crate) fn session_files(&self) -> Option<&SessionFiles> {
        self.editors.get(&self.active_tab_id()?)
    }

    fn session_files_mut(&mut self) -> Option<&mut SessionFiles> {
        let tab = self.active_tab_id()?;
        self.editors.get_mut(&tab)
    }

    /// The file shown in place of the visible session's terminals.
    pub(crate) fn shown_doc(&self) -> Option<&OpenFile> {
        let files = self.session_files()?;
        let id = files.shown?;
        files.open.iter().find(|d| d.id == id)
    }

    /// How a file of session `tab` is named to the user: its path inside the
    /// session's folder, else its whole path.
    pub(crate) fn file_label(&self, tab: Option<TabId>, path: &Path) -> String {
        let s = self.core.state();
        let root = tab.and_then(|t| s.tab(t)).map(|t| s.tab_working_dir(t));
        match root.and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf)) {
            Some(rel) => rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"),
            None => crate::sidebar::display_dir(path),
        }
    }

    /// [`App::file_label`] for a file of the visible session.
    pub(crate) fn doc_label(&self, path: &Path) -> String {
        self.file_label(self.active_tab_id(), path)
    }

    /// Shows the visible session's terminals instead of a file.
    pub(crate) fn show_terminals(&mut self) {
        if let Some(files) = self.session_files_mut() {
            files.shown = None;
            self.open_files_dirty = true;
        }
    }

    /// Brings back the files that were open before the last restart.
    pub(crate) fn restore_open_files(&mut self) {
        let Some(saved) =
            self.core.state().gui_value(saved::KEY).and_then(|v| serde_json::from_str::<saved::Saved>(v).ok())
        else {
            return;
        };
        let mut next = self.next_doc;
        let state = self.core.state();
        let restored = saved.restore(
            |tab| state.tab(tab).is_some(),
            || {
                next += 1;
                DocId(next)
            },
        );
        self.next_doc = next;
        self.editors.extend(restored);
    }

    /// The open files to keep across a restart, as stored, if they changed.
    pub(crate) fn take_open_files_value(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.open_files_dirty) {
            return None;
        }
        serde_json::to_string(&saved::Saved::of(&self.editors)).ok()
    }

    /// The key [`App::take_open_files_value`] is stored under.
    pub(crate) const OPEN_FILES_KEY: &str = saved::KEY;

    /// A file brought back from before a restart is read, or its terminal
    /// editor started, once it is shown. Called after every update, so no way
    /// of showing a file misses it.
    pub(crate) fn revive_shown(&mut self) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else { return Task::none() };
        let Some(&OpenFile { id, ref path, kind: DocKind::Restored(how) }) = self.shown_doc() else {
            return Task::none();
        };
        let path = path.clone();
        if how != OpenAs::Terminal {
            if let Some(doc) = self.doc_mut(id) {
                doc.kind = DocKind::Loading;
            }
            return if how == OpenAs::Picture { self.read_picture(id, path) } else { self.read_doc(id, path) };
        }
        match self.launch_editor(tab, &path) {
            Some(kind) => {
                if let Some(doc) = self.doc_mut(id) {
                    doc.kind = kind;
                }
            }
            None => self.close_doc(id),
        }
        self.focus_active()
    }

    fn doc(&self, id: DocId) -> Option<(TabId, &OpenFile)> {
        self.editors.iter().find_map(|(tab, f)| Some((*tab, f.open.iter().find(|d| d.id == id)?)))
    }

    fn doc_mut(&mut self, id: DocId) -> Option<&mut OpenFile> {
        self.editors.values_mut().flat_map(|f| f.open.iter_mut()).find(|d| d.id == id)
    }

    fn text_mut(&mut self, id: DocId) -> Option<&mut TextDoc> {
        match &mut self.doc_mut(id)?.kind {
            DocKind::Text(t) => Some(t),
            DocKind::Loading
            | DocKind::Picture(_)
            | DocKind::Restored(_)
            | DocKind::Terminal { .. }
            | DocKind::Commit(_)
            | DocKind::Changes(_) => None,
        }
    }

    fn picture_mut(&mut self, id: DocId) -> Option<&mut PictureDoc> {
        match &mut self.doc_mut(id)?.kind {
            DocKind::Picture(p) => Some(p),
            _ => None,
        }
    }

    /// The terminal of a terminal editor, or of a session (see `Core::terminal`).
    pub(crate) fn handle_of(&self, pane: PaneId) -> Option<TerminalHandle> {
        self.core.terminal(pane).or_else(|| {
            self.editors.values().flat_map(|f| &f.open).find_map(|d| match &d.kind {
                DocKind::Terminal { handle, .. } if handle.pane() == pane => Some(handle.clone()),
                _ => None,
            })
        })
    }

    /// The open file a terminal editor in `pane` has.
    fn editor_in(&self, pane: PaneId) -> Option<DocId> {
        self.editors.values().flat_map(|f| &f.open).find_map(|d| match &d.kind {
            DocKind::Terminal { handle, .. } if handle.pane() == pane => Some(d.id),
            _ => None,
        })
    }

    /// The pane of the terminal editor shown in place of the visible session's terminals.
    pub(crate) fn shown_editor_pane(&self) -> Option<PaneId> {
        match &self.shown_doc()?.kind {
            DocKind::Terminal { handle, .. } => Some(handle.pane()),
            _ => None,
        }
    }

    /// Running terminal editors, for asking before quitting: "nvim (src/main.rs)".
    pub(crate) fn running_editors(&self) -> Vec<RunningTerminal> {
        let s = self.core.state();
        s.tabs
            .iter()
            .filter_map(|t| Some((t, self.editors.get(&t.id)?)))
            .flat_map(|(t, files)| {
                files.open.iter().filter_map(move |d| match &d.kind {
                    DocKind::Terminal { handle, name, .. } if handle.exit_status().is_none() => {
                        let project = s.project(t.project).map_or("", |p| p.name.as_str());
                        Some(RunningTerminal {
                            pane: handle.pane(),
                            place: format!("{} · {project}", t.title),
                            programs: format!("{name} ({})", self.file_label(Some(t.id), &d.path)),
                            agent: false,
                        })
                    }
                    _ => None,
                })
            })
            .collect()
    }

    /// A terminal editor ended (`:q`): its file closes. An editor that ends
    /// at once with an error probably couldn't start; the banner says so.
    pub(crate) fn editor_exited(&mut self, pane: PaneId, code: Option<i32>) -> Option<Task<Message>> {
        let id = self.editor_in(pane)?;
        if let Some(OpenFile { kind: DocKind::Terminal { name, started, .. }, .. }) = self.doc_mut(id)
            && started.elapsed() < std::time::Duration::from_secs(3)
            && code.is_some_and(|c| c != 0)
        {
            self.error = Some(format!("{name} ended right away (exit code {}).", code.unwrap_or_default()));
        }
        self.close_doc(id);
        Some(self.focus_active())
    }

    /// Closes the files of a session that is going away, ending their terminal editors.
    pub(crate) fn drop_session_files(&mut self, tab: TabId) {
        self.open_files_dirty = true;
        for doc in self.editors.remove(&tab).map(|f| f.open).unwrap_or_default() {
            if let DocKind::Terminal { handle, .. } = doc.kind {
                self.core.close_scratch_terminal(handle.pane());
            }
        }
    }

    /// Files with unsaved changes, named for the quit dialog: "src/main.rs (agents)".
    pub(crate) fn unsaved_files(&self) -> Vec<String> {
        let s = self.core.state();
        // In the order of the sessions list.
        s.tabs
            .iter()
            .filter_map(|t| Some((t, self.editors.get(&t.id)?)))
            .flat_map(|(t, files)| {
                files
                    .open
                    .iter()
                    .filter(|d| d.modified())
                    .map(|d| format!("{} ({})", self.file_label(Some(t.id), &d.path), t.title))
            })
            .collect()
    }

    /// How many files of session `tab` have unsaved changes.
    pub(crate) fn unsaved_in(&self, tab: TabId) -> usize {
        self.editors.get(&tab).map_or(0, |f| f.open.iter().filter(|d| d.modified()).count())
    }

    /// Moves the keyboard to the shown file, if there is one. `None` when
    /// the terminals are shown.
    pub(crate) fn focus_doc(&self) -> Option<Task<Message>> {
        Some(match &self.shown_doc()?.kind {
            DocKind::Text(_) => operation::focus(FILE_EDITOR),
            DocKind::Terminal { handle, .. } => operation::focus(crate::terminal_view::widget_id(handle.pane())),
            DocKind::Loading
            | DocKind::Picture(_)
            | DocKind::Restored(_)
            | DocKind::Commit(_)
            | DocKind::Changes(_) => Task::none(),
        })
    }

    /// Opens a file in the visible session (or shows it, if it is open
    /// already), reading it on a thread.
    pub(crate) fn open_file(&mut self, path: PathBuf) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            self.error = Some("Files open in place of a session's terminals: start a session first.".into());
            return Task::none();
        };
        self.open_files_dirty = true;
        let files = self.editors.entry(tab).or_default();
        if let Some(doc) = files.open.iter().find(|d| d.path == path) {
            files.shown = Some(doc.id);
            return self.focus_active();
        }
        let id = self.new_doc_id();
        // Pictures open in the viewer, whichever editor files open in.
        let picture = picture::kind_of(&path).is_some();
        if self.prefs.file_editor == FileEditor::Terminal && !picture {
            let Some(kind) = self.launch_editor(tab, &path) else { return Task::none() };
            let files = self.editors.entry(tab).or_default();
            files.open.push(OpenFile { id, path, kind });
            files.shown = Some(id);
            return self.focus_active();
        }
        let files = self.editors.entry(tab).or_default();
        files.open.push(OpenFile { id, path: path.clone(), kind: DocKind::Loading });
        files.shown = Some(id);
        if picture { self.read_picture(id, path) } else { self.read_doc(id, path) }
    }

    /// Reads a file for the built-in editor, on a thread.
    fn read_doc(&self, id: DocId, path: PathBuf) -> Task<Message> {
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(disk::read(&path));
        });
        Task::perform(async move { rx.recv().await.unwrap_or_else(|_| Err("reading it stopped".into())) }, move |r| {
            Message::Editor(EditorMsg::Loaded(id, r))
        })
    }

    /// Reads and decodes a picture, on a thread.
    fn read_picture(&self, id: DocId, path: PathBuf) -> Task<Message> {
        self.read_picture_as(id, path, EditorMsg::PictureLoaded)
    }

    /// [`App::read_picture`], answered with `done`.
    fn read_picture_as(
        &self,
        id: DocId,
        path: PathBuf,
        done: fn(DocId, Result<FilePicture, String>) -> EditorMsg,
    ) -> Task<Message> {
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(picture::read(&path));
        });
        Task::perform(async move { rx.recv().await.unwrap_or_else(|_| Err("reading it stopped".into())) }, move |r| {
            Message::Editor(done(id, r))
        })
    }

    /// Shows an open SVG's text, in the editor files open in.
    fn show_as_text(&mut self, id: DocId) -> Task<Message> {
        if !matches!(self.doc(id), Some((_, d)) if matches!(d.kind, DocKind::Picture(_))) {
            return Task::none();
        }
        self.open_as_text(id)
    }

    /// Opens an open file's text in place of what it shows now.
    fn open_as_text(&mut self, id: DocId) -> Task<Message> {
        let Some((tab, doc)) = self.doc(id) else { return Task::none() };
        let path = doc.path.clone();
        self.open_files_dirty = true;
        if self.prefs.file_editor == FileEditor::Terminal {
            let Some(kind) = self.launch_editor(tab, &path) else { return Task::none() };
            if let Some(doc) = self.doc_mut(id) {
                doc.kind = kind;
            }
            return self.focus_active();
        }
        if let Some(doc) = self.doc_mut(id) {
            doc.kind = DocKind::Loading;
        }
        self.read_doc(id, path)
    }

    /// Shows an open SVG's picture instead of its text, once the text is saved.
    fn show_as_picture(&mut self, id: DocId) -> Task<Message> {
        let Some(doc) = self.doc_mut(id) else { return Task::none() };
        if !matches!(&doc.kind, DocKind::Text(t) if !t.modified() && !t.saving) {
            return Task::none();
        }
        doc.kind = DocKind::Loading;
        let path = doc.path.clone();
        self.open_files_dirty = true;
        self.read_picture(id, path)
    }

    /// Starts Neovim or Vim (or the editor command set) on `path`, in a
    /// terminal in session `tab`'s folder. `None`, with a banner saying why,
    /// when it can't.
    fn launch_editor(&mut self, tab: TabId, path: &Path) -> Option<DocKind> {
        let s = self.core.state();
        let root = s.tab(tab).map(|t| s.tab_working_dir(t))?;
        let launch = match external::launch_here(&self.prefs.editor_command, path, &root) {
            Ok(l) => l,
            Err(e) => {
                self.error = Some(format!("Can't open {} in an editor: {e}.", self.file_label(Some(tab), path)));
                return None;
            }
        };
        match self.core.open_program_terminal(&root, launch.command, launch.env, GridSize::default()) {
            Ok(handle) => Some(DocKind::Terminal { handle, name: launch.name, started: Instant::now() }),
            Err(e) => {
                self.error = Some(format!("Could not start {}: {e}", launch.name));
                None
            }
        }
    }

    /// Reads an open file again, to show what is on disk now.
    fn reload_doc(&mut self, id: DocId) -> Task<Message> {
        let Some((_, doc)) = self.doc(id) else { return Task::none() };
        let path = doc.path.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(disk::read(&path));
        });
        Task::perform(async move { rx.recv().await.unwrap_or_else(|_| Err("reading it stopped".into())) }, move |r| {
            Message::Editor(EditorMsg::Reloaded(id, r))
        })
    }

    /// Whether any open file is shown in the built-in editor or as a picture
    /// (and so is compared with the disk now and then).
    pub(crate) fn has_files_to_check(&self) -> bool {
        self.editors.values().flat_map(|f| &f.open).any(|d| matches!(d.kind, DocKind::Text(_) | DocKind::Picture(_)))
    }

    /// Looks at every open file on disk, on a thread.
    fn poll_disk(&mut self) -> Task<Message> {
        if self.disk_polling {
            return Task::none();
        }
        let files: Vec<(DocId, PathBuf, Stamp)> = self
            .editors
            .values()
            .flat_map(|f| &f.open)
            .filter_map(|d| match &d.kind {
                DocKind::Text(t) if !t.saving => Some((d.id, d.path.clone(), t.disk)),
                DocKind::Picture(p) => Some((d.id, d.path.clone(), p.file.stamp)),
                _ => None,
            })
            .collect();
        if files.is_empty() {
            return Task::none();
        }
        self.disk_polling = true;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let checked = files.into_iter().map(|(id, path, known)| {
                let now = disk::check(&path, &known);
                (id, known, now)
            });
            let _ = tx.send_blocking(checked.collect());
        });
        Task::perform(async move { rx.recv().await.unwrap_or_default() }, |c| {
            Message::Editor(EditorMsg::DiskChecked(c))
        })
    }

    /// Closes a file; its session shows the file next to it, else its terminals.
    /// A terminal editor is ended.
    fn close_doc(&mut self, id: DocId) {
        self.open_files_dirty = true;
        let mut closed = None;
        for files in self.editors.values_mut() {
            let Some(i) = files.open.iter().position(|d| d.id == id) else { continue };
            closed = Some(files.open.remove(i));
            if files.shown == Some(id) {
                files.shown = files.open.get(i).or_else(|| files.open.last()).map(|d| d.id);
            }
            break;
        }
        if let Some(OpenFile { kind: DocKind::Terminal { handle, .. }, .. }) = closed {
            self.core.close_scratch_terminal(handle.pane());
        }
    }

    /// Writes a file with changes on a thread; `then_close` closes it after.
    /// Unless `force`, a file that changed on disk is not written over, and
    /// one without changes is not written at all.
    pub(crate) fn save_doc(&mut self, id: DocId, then_close: bool, force: bool) -> Task<Message> {
        let Some(OpenFile { path, kind: DocKind::Text(doc), .. }) = self.doc_mut(id) else { return Task::none() };
        doc.close_after_save |= then_close;
        if doc.saving {
            return Task::none();
        }
        if !doc.modified() && !force {
            if doc.close_after_save {
                self.close_doc(id);
                return self.focus_active();
            }
            return Task::none();
        }
        doc.saving = true;
        let bytes = text::encode(doc.content.lines().map(|l| l.text), doc.format);
        let version = doc.version;
        let known = doc.disk;
        let path = path.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(disk::write(&path, &bytes, Some(&known), force));
        });
        Task::perform(
            async move { rx.recv().await.unwrap_or_else(|_| Err(SaveError::Io("writing it stopped".into()))) },
            move |r| Message::Editor(EditorMsg::Saved(id, version, r)),
        )
    }

    /// An id for a new open file.
    pub(crate) fn new_doc_id(&mut self) -> DocId {
        self.next_doc += 1;
        DocId(self.next_doc)
    }

    fn next_version(&mut self) -> u64 {
        self.next_version += 1;
        self.next_version
    }

    pub(crate) fn update_editor(&mut self, msg: EditorMsg) -> Task<Message> {
        match msg {
            EditorMsg::Show(id) => {
                if let Some(files) = self.session_files_mut() {
                    files.shown = id.filter(|id| files.open.iter().any(|d| d.id == *id));
                    self.open_files_dirty = true;
                }
                self.focus_active()
            }
            EditorMsg::Close(id) => {
                let Some((tab, doc)) = self.doc(id) else { return Task::none() };
                match &doc.kind {
                    DocKind::Terminal { handle, name, .. } if handle.exit_status().is_none() => {
                        // Its own `:qa`, so changes not saved in it keep it open
                        // (it says why); the file closes when it ends. Other
                        // editors are just ended.
                        if let Some(keys) = external::quit_keys(name) {
                            handle.write(keys);
                            return Task::batch([self.focus_active(), quit_check(id)]);
                        }
                    }
                    _ if doc.modified() => {
                        let name = self.file_label(Some(tab), &doc.path);
                        self.dialog = Some(Dialog::CloseFile { doc: id, name });
                        return Task::none();
                    }
                    _ => {}
                }
                self.close_doc(id);
                self.focus_active()
            }
            EditorMsg::QuitCheck(id) => {
                let running = |d: &OpenFile| matches!(&d.kind, DocKind::Terminal { handle, .. } if handle.exit_status().is_none());
                let Some((tab, _)) = self.doc(id).filter(|(_, d)| running(d)) else { return Task::none() };
                if let Some(files) = self.editors.get_mut(&tab) {
                    files.shown = Some(id);
                    self.open_files_dirty = true;
                }
                if Some(tab) == self.active_tab_id() { self.focus_active() } else { Task::none() }
            }
            EditorMsg::Discard(id) => {
                self.dialog = None;
                self.close_doc(id);
                self.focus_active()
            }
            EditorMsg::Loaded(id, Ok(FileText { decoded, stamp })) => {
                let version = self.next_version();
                if let Some(doc) = self.doc_mut(id) {
                    let mut content = text_editor::Content::with_text(&decoded.text);
                    // It starts at the end of the text; files are read from the top.
                    content.perform(Action::Move(text_editor::Motion::DocumentStart));
                    doc.kind = DocKind::Text(Box::new(TextDoc {
                        content,
                        format: decoded.format,
                        indent: text::detect_indent(&decoded.text),
                        version,
                        saved_version: version,
                        saving: false,
                        close_after_save: false,
                        history: History::default(),
                        disk: stamp,
                        conflict: None,
                    }));
                }
                self.focus_active()
            }
            EditorMsg::Loaded(id, Err(e)) => {
                let name = self.doc(id).map(|(tab, d)| self.file_label(Some(tab), &d.path));
                self.close_doc(id);
                if let Some(name) = name {
                    self.error = Some(format!("Can't open {name}: {e}."));
                }
                self.focus_active()
            }
            EditorMsg::Action(id, action) => {
                let version = self.next_version();
                let Some(doc) = self.text_mut(id) else { return Task::none() };
                match &action {
                    Action::Edit(edit) => {
                        let kind = history::Kind::of(edit);
                        doc.keep_for_undo(kind);
                        doc.content.perform(action);
                        doc.version = version;
                    }
                    // Scrolling doesn't end what is being typed; moving the cursor does.
                    Action::Scroll { .. } => doc.content.perform(action),
                    _ => {
                        doc.history.close_step();
                        doc.content.perform(action);
                    }
                }
                Task::none()
            }
            EditorMsg::Indent(id, back) => {
                let version = self.next_version();
                if let Some(doc) = self.text_mut(id) {
                    let before = Snapshot::of(&doc.content, doc.version);
                    if indent(doc, back) {
                        doc.history.push(before);
                        doc.history.edited(history::Kind::Single, Instant::now());
                        doc.version = version;
                    }
                }
                Task::none()
            }
            EditorMsg::Undo(id) | EditorMsg::Redo(id) => {
                let undo = matches!(msg, EditorMsg::Undo(_));
                if let Some(doc) = self.text_mut(id) {
                    let current = Snapshot::of(&doc.content, doc.version);
                    let target = if undo { doc.history.undo(current) } else { doc.history.redo(current) };
                    if let Some(target) = target {
                        target.restore(&mut doc.content);
                        // Back at the saved text, the file shows as unchanged again.
                        doc.version = target.version;
                    }
                }
                Task::none()
            }
            EditorMsg::Save(id) | EditorMsg::SaveAnyway(id) => {
                let force = matches!(msg, EditorMsg::SaveAnyway(_));
                if force {
                    self.dialog = None;
                }
                if let Some(doc) = self.text_mut(id) {
                    doc.history.close_step();
                }
                Task::batch([self.save_doc(id, false, force), self.focus_active()])
            }
            EditorMsg::Saved(id, version, result) => {
                let name = self.doc(id).map(|(tab, d)| self.file_label(Some(tab), &d.path)).unwrap_or_default();
                let Some(doc) = self.text_mut(id) else { return Task::none() };
                doc.saving = false;
                match result {
                    Ok(stamp) => {
                        doc.saved_version = version;
                        doc.disk = stamp;
                        doc.conflict = None;
                        if doc.close_after_save {
                            self.close_doc(id);
                            return self.focus_active();
                        }
                    }
                    Err(SaveError::Conflict) => {
                        doc.close_after_save = false;
                        self.dialog = Some(Dialog::SaveConflict { doc: id, name });
                    }
                    Err(SaveError::Io(e)) => {
                        doc.close_after_save = false;
                        self.error = Some(format!("Could not save {name}: {e}"));
                    }
                }
                Task::none()
            }
            EditorMsg::DiskPoll => self.poll_disk(),
            EditorMsg::DiskChecked(checked) => {
                self.disk_polling = false;
                let mut reload = Vec::new();
                let mut reload_pictures = Vec::new();
                for (id, known, now) in checked {
                    if let Some(p) = self.picture_mut(id) {
                        if p.file.stamp != known {
                            continue;
                        }
                        match now {
                            OnDisk::Same(stamp) => {
                                p.file.stamp = stamp;
                                p.deleted = false;
                            }
                            OnDisk::Changed(stamp) => {
                                // Read once per change, however long decoding takes.
                                p.file.stamp = stamp;
                                p.deleted = false;
                                reload_pictures.push(id);
                            }
                            OnDisk::Deleted => p.deleted = true,
                            OnDisk::Unreadable => {}
                        }
                        continue;
                    }
                    let Some(doc) = self.text_mut(id) else { continue };
                    // Saved or reloaded meanwhile: this look is out of date.
                    if doc.saving || doc.disk != known {
                        continue;
                    }
                    match disk::react(doc.modified(), now) {
                        Reaction::Keep(stamp) => {
                            doc.disk = stamp;
                            // Back after being deleted, unchanged.
                            if doc.conflict == Some(Conflict::Deleted) {
                                doc.conflict = None;
                            }
                        }
                        Reaction::Reload => reload.push(id),
                        Reaction::Ask(stamp) => doc.conflict = Some(Conflict::Changed(stamp)),
                        Reaction::Deleted => doc.conflict = Some(Conflict::Deleted),
                        Reaction::Nothing => {}
                    }
                }
                let pictures: Vec<_> = reload_pictures
                    .into_iter()
                    .filter_map(|id| Some((id, self.doc(id)?.1.path.clone())))
                    .map(|(id, path)| self.read_picture_as(id, path, EditorMsg::PictureReloaded))
                    .collect();
                Task::batch(reload.into_iter().map(|id| self.reload_doc(id)).chain(pictures))
            }
            EditorMsg::Reload(id) => {
                self.dialog = None;
                Task::batch([self.reload_doc(id), self.focus_active()])
            }
            EditorMsg::Reloaded(id, Ok(file)) => {
                let version = self.next_version();
                if let Some(doc) = self.text_mut(id) {
                    doc.replace_text(file, version);
                }
                Task::none()
            }
            EditorMsg::Reloaded(id, Err(e)) => {
                let name = self.doc(id).map(|(tab, d)| self.file_label(Some(tab), &d.path)).unwrap_or_default();
                self.error = Some(format!("Can't read {name} again: {e}."));
                Task::none()
            }
            EditorMsg::KeepMine(id) => {
                if let Some(doc) = self.text_mut(id)
                    && let Some(Conflict::Changed(stamp)) = doc.conflict.take()
                {
                    // Asked again only if it changes again.
                    doc.disk = stamp;
                }
                self.focus_active()
            }
            EditorMsg::PictureLoaded(id, Ok(file)) => {
                if let Some(doc) = self.doc_mut(id).filter(|d| matches!(d.kind, DocKind::Loading)) {
                    doc.kind = DocKind::Picture(Box::new(PictureDoc { file, zoom: Zoom::default(), deleted: false }));
                    // Kept as a picture across a restart, even if it was kept while being read.
                    self.open_files_dirty = true;
                }
                Task::none()
            }
            EditorMsg::PictureLoaded(id, Err(e)) => {
                let Some((tab, doc)) = self.doc(id) else { return Task::none() };
                let name = self.file_label(Some(tab), &doc.path);
                // An SVG that can't be drawn opens as text, to be put right.
                if picture::has_text(&doc.path) {
                    self.error = Some(format!("Can't show {name} as a picture: {e}."));
                    return self.open_as_text(id);
                }
                self.close_doc(id);
                self.error = Some(format!("Can't open {name}: {e}."));
                self.focus_active()
            }
            EditorMsg::PictureReloaded(id, result) => {
                match (self.picture_mut(id), result) {
                    // Same zoom, so a picture an agent keeps changing can be watched.
                    (Some(p), Ok(file)) => p.file = file,
                    // Likely read while still being written: the change that
                    // finishes it is read too, and until then the last good
                    // picture is better than an error.
                    (Some(_), Err(e)) => tracing::debug!("picture not read again: {e}"),
                    (None, _) => {}
                }
                Task::none()
            }
            EditorMsg::Zoom(id, zoom) => {
                if let Some(p) = self.picture_mut(id) {
                    p.zoom = zoom;
                }
                Task::none()
            }
            EditorMsg::AsText(id) => self.show_as_text(id),
            EditorMsg::AsPicture(id) => self.show_as_picture(id),
        }
    }
}

/// How long Neovim or Vim gets to quit before the refusal is shown.
const QUIT_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// [`EditorMsg::QuitCheck`] once Neovim or Vim had time to quit.
fn quit_check(id: DocId) -> Task<Message> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        std::thread::sleep(QUIT_WAIT);
        let _ = tx.send_blocking(());
    });
    Task::perform(async move { rx.recv().await.ok() }, move |_| Message::Editor(EditorMsg::QuitCheck(id)))
}

/// Tab inserts the file's indent at the cursor (replacing a selection);
/// Shift+Tab takes one level off the start of the cursor's line. Whether
/// the text changed.
fn indent(doc: &mut TextDoc, back: bool) -> bool {
    let at = doc.content.cursor().position;
    let line = doc.content.line(at.line).map(|l| l.text.into_owned()).unwrap_or_default();
    if !back {
        let unit = doc.indent.unit(text::char_column(&line, at.column));
        doc.content.perform(Action::Edit(Edit::Paste(Arc::new(unit))));
        return true;
    }
    let n = doc.indent.outdent_len(&line);
    if n == 0 {
        return false;
    }
    let start = Position { line: at.line, column: 0 };
    doc.content.move_to(Cursor { position: start, selection: Some(Position { line: at.line, column: n }) });
    doc.content.perform(Action::Edit(Edit::Delete));
    doc.content
        .move_to(Cursor { position: Position { line: at.line, column: at.column.saturating_sub(n) }, selection: None });
    true
}
