//! Commits clicked in the Git tab, shown in place of the session's
//! terminals like an open file: the message, who made it and when, and
//! what it changed, file by file and hunk by hunk. And the changes not
//! committed yet, which are staged and unstaged there (see `changes`).

mod changes;
mod changes_view;
mod view;

use std::collections::HashSet;

use iced::Task;
use iced::widget::operation::{self, RelativeOffset};
use termroost_core::git::{CommitDetails, FileDiff, Repo, WorkingChanges};

pub use changes::{ChangesDoc, Pick, Section};

use crate::app::{App, Message};
use crate::editor::{DocId, DocKind, OpenFile};

/// Widget id of the scrollable a commit is shown in, so a commit picked
/// next starts at its top.
const COMMIT_SCROLL: &str = "commit-scroll";

/// Widget id of the scrollable the uncommitted changes are listed in, so
/// folding or unfolding them all starts at the top.
const CHANGES_SCROLL: &str = "changes-scroll";

/// Files with more changed lines than this start folded.
const FOLD_LINES: usize = 1000;

/// A commit shown in place of the terminals. A session has one at most:
/// another commit picked in the Git tab shows in it instead.
pub struct CommitDoc {
    pub repo: Repo,
    /// The full id.
    pub id: String,
    /// `None` while it is read.
    pub details: Option<Result<CommitDetails, String>>,
    /// Files the user folded or unfolded, by path (big ones start folded).
    pub toggled: HashSet<String>,
}

impl CommitDoc {
    /// The abbreviated id, for the strip of open files.
    pub fn short(&self) -> &str {
        self.id.get(..7).unwrap_or(&self.id)
    }

    fn folded(&self, file: &FileDiff) -> bool {
        starts_folded(file) != self.toggled.contains(&file.path)
    }
}

impl std::fmt::Debug for CommitDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the changes: logs must not show what is in people's files.
        f.debug_struct("CommitDoc").field("repo", &self.repo.worktree).field("id", &self.id).finish()
    }
}

/// Whether a file's changes start folded: too many lines to scroll past.
fn starts_folded(file: &FileDiff) -> bool {
    file.hunks.iter().map(|h| h.lines.len()).sum::<usize>() > FOLD_LINES
}

#[derive(Clone, Debug)]
pub enum GitViewMsg {
    /// Show a commit (by id) of the repository of the session on screen.
    OpenCommit(String),
    /// A commit was read, for the commit view of `DocId`.
    CommitRead(DocId, String, Box<Result<CommitDetails, String>>),
    /// Fold or unfold a file of the commit shown.
    ToggleFile(DocId, String),
    /// Show the uncommitted changes of the session's repository.
    OpenChanges,
    /// The changes were read (the read's number, for the changes view of `DocId`).
    ChangesRead(DocId, u64, Box<Result<WorkingChanges, String>>),
    /// Fold or unfold a file of the changes.
    ToggleChange(DocId, Section, String),
    /// Fold or unfold a section of the changes.
    ToggleSection(DocId, Section),
    /// Fold (`true`) or unfold every file of the changes.
    FoldAll(DocId, bool),
    /// Stage what is picked, or unstage it when it is staged.
    Move(DocId, Pick),
    /// It was staged or unstaged (or why not), and the changes read again.
    Moved(DocId, u64, Result<(), String>, Box<Result<WorkingChanges, String>>),
    /// A line of a hunk was pressed (section, path, hunk, line, keys held).
    ChooseLine(DocId, Section, String, usize, usize, iced::keyboard::Modifiers),
    /// The pointer, pressed on a line, was dragged onto this line of its hunk.
    DragLine(DocId, usize),
    /// Choose no lines.
    ClearChosen(DocId),
    /// Typing and moving about in the commit message.
    MessageEdit(DocId, iced::widget::text_editor::Action),
    /// Amend the last commit instead of making a new one.
    Amend(DocId, bool),
    /// The last commit's message, read to amend it.
    LastMessage(DocId, Result<String, String>),
    /// Commit what is staged with the message written.
    Commit(DocId),
    /// It was committed (what happened) or why not, and the changes read again.
    Committed(DocId, u64, Result<String, String>, Box<Result<WorkingChanges, String>>),
    /// Hide the note about the last stage, unstage or commit.
    DismissNote(DocId),
}

impl GitViewMsg {
    /// Whether the message may be logged: some carry what is in files.
    pub fn loggable(&self) -> bool {
        !matches!(
            self,
            GitViewMsg::CommitRead(..)
                | GitViewMsg::ChangesRead(..)
                | GitViewMsg::Moved(..)
                | GitViewMsg::MessageEdit(..)
                | GitViewMsg::LastMessage(..)
                | GitViewMsg::Committed(..)
        )
    }
}

impl App {
    /// The commit shown in place of the session's terminals, if any.
    pub(crate) fn shown_commit(&self) -> Option<&str> {
        match &self.shown_doc()?.kind {
            DocKind::Commit(c) => Some(&c.id),
            _ => None,
        }
    }

    fn commit_doc_mut(&mut self, id: DocId) -> Option<&mut CommitDoc> {
        let doc = self.editors.values_mut().flat_map(|f| f.open.iter_mut()).find(|d| d.id == id)?;
        match &mut doc.kind {
            DocKind::Commit(c) => Some(c),
            _ => None,
        }
    }

    /// Shows commit `id` of the session's repository, in the session's
    /// commit view (made if it has none), reading it on a thread.
    fn open_commit(&mut self, id: String) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else { return Task::none() };
        let Some(repo) = self.active_repo().cloned() else { return Task::none() };
        self.open_files_dirty = true;
        // (The session's commit view, and whether it shows this commit already.)
        let existing = self.session_files().and_then(|f| {
            f.open.iter().find_map(|d| match &d.kind {
                DocKind::Commit(c) => Some((d.id, c.id == id && c.repo == repo && matches!(c.details, Some(Ok(_))))),
                _ => None,
            })
        });
        let fresh = existing.is_none().then(|| self.new_doc_id());
        let files = self.editors.entry(tab).or_default();
        let kind = DocKind::Commit(Box::new(CommitDoc {
            repo: repo.clone(),
            id: id.clone(),
            details: None,
            toggled: HashSet::new(),
        }));
        let doc = match (existing, fresh) {
            (Some((doc, true)), _) => {
                files.shown = Some(doc);
                return Task::none();
            }
            (Some((doc, false)), _) => {
                if let Some(d) = files.open.iter_mut().find(|d| d.id == doc) {
                    d.path = repo.worktree.clone();
                    d.kind = kind;
                }
                doc
            }
            (None, Some(doc)) => {
                files.open.push(OpenFile { id: doc, path: repo.worktree.clone(), kind });
                doc
            }
            (None, None) => return Task::none(),
        };
        files.shown = Some(doc);
        let (tx, rx) = async_channel::bounded(1);
        let wanted = id.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(repo.commit_details(&wanted));
        });
        let read = Task::perform(
            async move { rx.recv().await.unwrap_or_else(|_| Err("reading the commit stopped".into())) },
            move |r| Message::GitView(GitViewMsg::CommitRead(doc, id.clone(), Box::new(r))),
        );
        Task::batch([read, operation::snap_to(COMMIT_SCROLL, RelativeOffset::START)])
    }

    pub(crate) fn update_git_view(&mut self, msg: GitViewMsg) -> Task<Message> {
        match msg {
            GitViewMsg::OpenCommit(id) => return self.open_commit(id),
            GitViewMsg::CommitRead(doc, id, result) => {
                if let Err(e) = result.as_ref() {
                    tracing::warn!(commit = %id, error = %e, "reading a commit failed");
                }
                // Another commit may have been picked meanwhile.
                if let Some(c) = self.commit_doc_mut(doc).filter(|c| c.id == id) {
                    c.details = Some(*result);
                }
            }
            GitViewMsg::ToggleFile(doc, path) => {
                if let Some(c) = self.commit_doc_mut(doc)
                    && !c.toggled.remove(&path)
                {
                    c.toggled.insert(path);
                }
            }
            other => return self.update_changes(other),
        }
        Task::none()
    }
}
