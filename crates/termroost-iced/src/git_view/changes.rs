//! What is not committed yet in a session's repository, shown in place of
//! its terminals: files with conflicts, staged changes, changes not staged,
//! and new files. Whole files, single hunks or all of a section are staged
//! and unstaged from here, and so are lines chosen in a hunk: pressed one
//! by one, dragged over, or with Shift for a range and Ctrl for one more.
//! What is staged is then committed with a message written above it (or
//! added to the last commit, amending it).
//!
//! While it is on screen, it is read again at every git poll, since agents
//! change files all the time. Each read is numbered: a read that started
//! before one already shown (a poll overtaken by a stage) is dropped.

use std::collections::{BTreeSet, HashSet};

use iced::widget::operation::{self, RelativeOffset};
use iced::widget::text_editor;
use iced::{Task, keyboard};
use termroost_core::git::{FileDiff, FileStatus, Hunk, LineKind, Repo, WorkingChanges};

use super::{CHANGES_SCROLL, GitViewMsg, starts_folded};
use crate::app::{App, Message};
use crate::editor::{DocId, DocKind, OpenFile};

/// More untracked files than this start folded.
const FOLD_UNTRACKED: usize = 20;

/// A part of the uncommitted changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    /// Files with a merge conflict.
    Conflicts,
    Staged,
    /// Changes not staged yet.
    Unstaged,
    /// Files git doesn't track yet.
    Untracked,
}

impl Section {
    /// In the order shown: conflicts first, as they stop a commit, then what
    /// the next commit takes, under its message, then what it doesn't yet.
    pub const ALL: [Section; 4] = [Section::Conflicts, Section::Staged, Section::Unstaged, Section::Untracked];

    pub fn title(self) -> &'static str {
        match self {
            Section::Conflicts => "Merge conflicts",
            Section::Unstaged => "Unstaged changes",
            Section::Untracked => "Untracked files",
            Section::Staged => "Staged changes",
        }
    }

    /// Its files of `changes`.
    pub fn files(self, changes: &WorkingChanges) -> Box<dyn Iterator<Item = &FileDiff> + '_> {
        let conflict = |f: &&FileDiff| f.status == FileStatus::Conflicted;
        match self {
            Section::Conflicts => Box::new(changes.unstaged.iter().filter(conflict)),
            Section::Unstaged => Box::new(changes.unstaged.iter().filter(move |f| !conflict(f))),
            Section::Untracked => Box::new(changes.untracked.iter()),
            Section::Staged => Box::new(changes.staged.iter()),
        }
    }
}

/// What a stage or unstage acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Every file of a section.
    All(Section),
    /// A file of a section, by path.
    File(Section, String),
    /// A hunk of a file of a section.
    Hunk(Section, String, usize),
    /// Some lines of a hunk (indexes into its lines).
    Lines(Section, String, usize, BTreeSet<usize>),
}

impl Pick {
    fn section(&self) -> Section {
        match self {
            Pick::All(s) | Pick::File(s, _) | Pick::Hunk(s, _, _) | Pick::Lines(s, _, _, _) => *s,
        }
    }
}

/// Lines chosen in a hunk, to stage or unstage only those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub section: Section,
    pub path: String,
    pub hunk: usize,
    /// Changed lines only (indexes into the hunk's lines).
    pub lines: BTreeSet<usize>,
    /// Where a range starts: the line pressed last without Shift.
    anchor: usize,
}

impl Chosen {
    pub fn is(&self, section: Section, path: &str, hunk: usize) -> bool {
        self.section == section && self.path == path && self.hunk == hunk
    }
}

/// The hunk `hunk` of file `path` in `section`, as `changes` has it.
fn find_hunk<'a>(changes: &'a WorkingChanges, section: Section, path: &str, hunk: usize) -> Option<&'a Hunk> {
    section.files(changes).find(|f| f.path == path && f.splittable())?.hunks.get(hunk)
}

/// The changed lines of `hunk` from `a` to `b`, either way round.
fn changed_between(hunk: &Hunk, a: usize, b: usize) -> BTreeSet<usize> {
    (a.min(b)..=a.max(b)).filter(|i| hunk.lines.get(*i).is_some_and(|l| l.kind != LineKind::Context)).collect()
}

/// The uncommitted changes of a repository, in place of a session's
/// terminals. A session has one at most.
pub struct ChangesDoc {
    pub repo: Repo,
    /// `None` until first read.
    pub changes: Option<Result<WorkingChanges, String>>,
    /// Files the user folded or unfolded (big ones, and staged ones, start
    /// folded).
    toggled: HashSet<(Section, String)>,
    /// Every file folded (by Collapse all) or unfolded (by Expand all),
    /// those that come later too, instead of as they start. Files toggled
    /// since are the exceptions.
    all_folded: Option<bool>,
    /// Sections showing only their title.
    folded_sections: HashSet<Section>,
    /// Reads started so far, and the number of the one shown.
    reads: u64,
    shown_read: u64,
    /// A read is running: polls wait for it.
    reading: bool,
    /// A stage or unstage is running.
    pub busy: bool,
    /// Why the last stage, unstage or commit failed, until the next one.
    pub error: Option<String>,
    /// What the last commit did ("Committed 1a2b3c4."), until the next change.
    pub done: Option<String>,
    /// Lines chosen in one hunk.
    pub chosen: Option<Chosen>,
    /// The message of the next commit.
    pub message: text_editor::Content,
    /// The next commit replaces the last one.
    pub amend: bool,
    /// The last commit's message, put in the box when Amend was ticked: it
    /// goes again when Amend is unticked, unless it was edited.
    amend_text: Option<String>,
    /// A commit is running.
    pub committing: bool,
}

impl ChangesDoc {
    fn new(repo: Repo) -> ChangesDoc {
        ChangesDoc {
            repo,
            changes: None,
            toggled: HashSet::new(),
            all_folded: None,
            folded_sections: HashSet::new(),
            reads: 0,
            shown_read: 0,
            reading: false,
            busy: false,
            error: None,
            done: None,
            chosen: None,
            message: text_editor::Content::new(),
            amend: false,
            amend_text: None,
            committing: false,
        }
    }

    /// Whether Commit can be pressed: there is a message, and something
    /// staged to commit (or a commit to amend).
    pub fn can_commit(&self) -> bool {
        let staged = matches!(&self.changes, Some(Ok(c)) if !c.staged.is_empty());
        !self.busy && (staged || self.amend) && !self.message.text().trim().is_empty()
    }

    /// Whether a file shows only its header. Staged files start folded: a
    /// list of what the next commit takes, under its message, which grows
    /// by a line (not a whole diff) for each file staged further down.
    pub fn folded(&self, section: Section, file: &FileDiff, files_in_section: usize) -> bool {
        let starts = self.all_folded.unwrap_or_else(|| {
            starts_folded(file)
                || section == Section::Staged
                || (section == Section::Untracked && files_in_section > FOLD_UNTRACKED)
        });
        starts != self.toggled.contains(&(section, file.path.clone()))
    }

    /// Folds a file, or unfolds it.
    fn toggle_file(&mut self, section: Section, path: String) {
        let key = (section, path);
        if !self.toggled.remove(&key) {
            self.toggled.insert(key);
        }
    }

    /// Folds every file, or unfolds every file.
    fn fold_all(&mut self, fold: bool) {
        self.all_folded = Some(fold);
        self.toggled.clear();
    }

    /// Whether a file of a section not folded shows its changes (of those
    /// shown, the first `max` of each section): Collapse all would fold it.
    pub fn any_unfolded(&self, changes: &WorkingChanges, max: usize) -> bool {
        Section::ALL.into_iter().filter(|s| !self.section_folded(*s)).any(|s| {
            let n = s.files(changes).count();
            s.files(changes).take(max).any(|f| !self.folded(s, f, n))
        })
    }

    pub fn section_folded(&self, section: Section) -> bool {
        self.folded_sections.contains(&section)
    }

    /// Folds a section, or unfolds it.
    fn toggle_section(&mut self, section: Section) {
        if !self.folded_sections.remove(&section) {
            self.folded_sections.insert(section);
        }
    }

    /// Takes in a read numbered `n`, unless a later one is shown already.
    fn take(&mut self, n: u64, read: Result<WorkingChanges, String>) {
        if n == self.reads {
            self.reading = false;
        }
        if n < self.shown_read {
            return;
        }
        self.shown_read = n;
        if let Err(e) = &read {
            tracing::warn!(repo = %self.repo.worktree.display(), error = %e, "reading the changes failed");
        }
        if self.changes.as_ref() == Some(&read) {
            return;
        }
        // Chosen lines go when their hunk changed: they may be other lines now.
        if let Some(c) = &self.chosen {
            let hunk = |r: Option<&Result<WorkingChanges, String>>| {
                r.and_then(|r| r.as_ref().ok()).and_then(|w| find_hunk(w, c.section, &c.path, c.hunk)).cloned()
            };
            if hunk(self.changes.as_ref()) != hunk(Some(&read)) {
                self.chosen = None;
            }
        }
        self.changes = Some(read);
    }

    /// A line of a hunk was pressed: it alone is chosen (or no line, if it
    /// alone was), or with Shift the changed lines from the last one pressed
    /// to it, or with Ctrl it is added to the lines chosen (or taken out).
    fn choose(&mut self, section: Section, path: String, hunk: usize, line: usize, mods: keyboard::Modifiers) {
        let Some(Ok(changes)) = &self.changes else { return };
        let Some(h) = find_hunk(changes, section, &path, hunk) else { return };
        let changed = h.lines.get(line).is_some_and(|l| l.kind != LineKind::Context);
        let same = self.chosen.as_mut().filter(|c| c.is(section, &path, hunk));
        match same {
            Some(c) if mods.shift() => c.lines = changed_between(h, c.anchor, line),
            Some(c) if mods.command() => {
                if changed && !c.lines.remove(&line) {
                    c.lines.insert(line);
                }
                c.anchor = line;
                if c.lines.is_empty() {
                    self.chosen = None;
                }
            }
            Some(c) if c.lines.len() == 1 && c.lines.contains(&line) => self.chosen = None,
            _ if !changed => self.chosen = None,
            _ => self.chosen = Some(Chosen { section, path, hunk, lines: BTreeSet::from([line]), anchor: line }),
        }
    }

    /// The pointer was dragged onto `line` while pressed: the changed lines
    /// from where it was pressed to there are chosen.
    fn drag_to(&mut self, line: usize) {
        let (Some(Ok(changes)), Some(c)) = (&self.changes, &mut self.chosen) else { return };
        if let Some(h) = find_hunk(changes, c.section, &c.path, c.hunk) {
            c.lines = changed_between(h, c.anchor, line);
        }
    }
}

impl std::fmt::Debug for ChangesDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the changes: logs must not show what is in people's files.
        f.debug_struct("ChangesDoc").field("repo", &self.repo.worktree).field("busy", &self.busy).finish()
    }
}

impl App {
    /// Whether the session on screen shows its uncommitted changes.
    pub(crate) fn changes_shown(&self) -> bool {
        matches!(self.shown_doc().map(|d| &d.kind), Some(DocKind::Changes(_)))
    }

    pub(super) fn changes_doc_mut(&mut self, id: DocId) -> Option<&mut ChangesDoc> {
        let doc = self.editors.values_mut().flat_map(|f| f.open.iter_mut()).find(|d| d.id == id)?;
        match &mut doc.kind {
            DocKind::Changes(c) => Some(c),
            _ => None,
        }
    }

    /// Shows the uncommitted changes of the session's repository, in its
    /// changes view (made if it has none), and reads them again.
    pub(super) fn open_changes(&mut self) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else { return Task::none() };
        let Some(repo) = self.active_repo().cloned() else { return Task::none() };
        self.open_files_dirty = true;
        let existing = self.session_files().and_then(|f| {
            f.open.iter().find_map(|d| match &d.kind {
                DocKind::Changes(c) => Some((d.id, c.repo == repo)),
                _ => None,
            })
        });
        let fresh = existing.is_none().then(|| self.new_doc_id());
        let files = self.editors.entry(tab).or_default();
        let doc = match (existing, fresh) {
            (Some((doc, same)), _) => {
                // The session moved to another worktree since: start over there.
                if !same && let Some(d) = files.open.iter_mut().find(|d| d.id == doc) {
                    d.path = repo.worktree.clone();
                    d.kind = DocKind::Changes(Box::new(ChangesDoc::new(repo)));
                }
                doc
            }
            (None, Some(doc)) => {
                let kind = DocKind::Changes(Box::new(ChangesDoc::new(repo.clone())));
                files.open.push(OpenFile { id: doc, path: repo.worktree, kind });
                doc
            }
            (None, None) => return Task::none(),
        };
        files.shown = Some(doc);
        self.read_changes(doc)
    }

    /// Reads the changes of a changes view again, on a thread.
    fn read_changes(&mut self, doc: DocId) -> Task<Message> {
        let Some(c) = self.changes_doc_mut(doc) else { return Task::none() };
        c.reads += 1;
        c.reading = true;
        let n = c.reads;
        let repo = c.repo.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(repo.working_changes());
        });
        Task::perform(
            async move { rx.recv().await.unwrap_or_else(|_| Err("reading the changes stopped".into())) },
            move |r| Message::GitView(GitViewMsg::ChangesRead(doc, n, Box::new(r))),
        )
    }

    /// At a git poll: reads the changes on screen again, unless they are
    /// being read or changed already.
    pub(crate) fn poll_changes(&mut self) -> Task<Message> {
        let Some(doc) = self.shown_doc() else { return Task::none() };
        match &doc.kind {
            DocKind::Changes(c) if !c.reading && !c.busy => {
                let id = doc.id;
                self.read_changes(id)
            }
            _ => Task::none(),
        }
    }

    /// Stages what is picked (or unstages it, from the staged changes) on a
    /// thread, then reads the changes again there.
    fn move_pick(&mut self, doc: DocId, pick: Pick) -> Task<Message> {
        let Some(c) = self.changes_doc_mut(doc) else { return Task::none() };
        let Some(Ok(changes)) = &c.changes else { return Task::none() };
        if c.busy {
            return Task::none();
        }
        let section = pick.section();
        let unstage = section == Section::Staged;
        // What the user saw: the hunk is applied as it was shown.
        let files: Vec<FileDiff> = match &pick {
            Pick::All(_) => section.files(changes).cloned().collect(),
            Pick::File(_, path) | Pick::Hunk(_, path, _) | Pick::Lines(_, path, _, _) => {
                section.files(changes).filter(|f| &f.path == path).cloned().collect()
            }
        };
        if files.is_empty() {
            return Task::none();
        }
        let (hunk, lines) = match pick {
            Pick::Hunk(_, _, i) => (Some(i), None),
            Pick::Lines(_, _, i, lines) => (Some(i), Some(lines)),
            Pick::All(_) | Pick::File(..) => (None, None),
        };
        c.chosen = None;
        c.busy = true;
        c.error = None;
        c.done = None;
        c.reads += 1;
        let n = c.reads;
        let repo = c.repo.clone();
        let chosen = lines.as_ref().map(BTreeSet::len);
        tracing::info!(repo = %repo.worktree.display(), ?section, files = files.len(), ?hunk, ?chosen, "staging changes");
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let paths = || files.iter().flat_map(FileDiff::paths);
            let done = match (hunk, unstage) {
                (Some(i), false) => repo.stage_hunk(&files[0], i, lines.as_ref()),
                (Some(i), true) => repo.unstage_hunk(&files[0], i, lines.as_ref()),
                (None, false) => repo.stage_paths(paths()),
                (None, true) => repo.unstage_paths(paths()),
            };
            let _ = tx.send_blocking((done, repo.working_changes()));
        });
        Task::perform(
            async move {
                let stopped = || Err("staging stopped".to_owned());
                rx.recv().await.unwrap_or_else(|_| (stopped(), Err("reading the changes stopped".into())))
            },
            move |(done, read)| Message::GitView(GitViewMsg::Moved(doc, n, done, Box::new(read))),
        )
    }

    /// Ticks or unticks Amend. Ticked with no message written, the last
    /// commit's message is read into the box, to be changed or kept.
    fn set_amend(&mut self, doc: DocId, on: bool) -> Task<Message> {
        let Some(c) = self.changes_doc_mut(doc).filter(|c| !c.committing) else { return Task::none() };
        c.amend = on;
        let text = c.message.text();
        if !on {
            // The last message, untouched: it was only there for amending.
            if c.amend_text.take().is_some_and(|t| t.trim_end() == text.trim_end()) {
                c.message = text_editor::Content::new();
            }
            return Task::none();
        }
        if !text.trim().is_empty() {
            return Task::none();
        }
        let repo = c.repo.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(repo.last_message());
        });
        Task::perform(async move { rx.recv().await.unwrap_or_else(|_| Err("reading it stopped".into())) }, move |r| {
            Message::GitView(GitViewMsg::LastMessage(doc, r))
        })
    }

    /// Commits what is staged with the message written, on a thread, then
    /// reads the changes again there.
    fn commit(&mut self, doc: DocId) -> Task<Message> {
        let Some(c) = self.changes_doc_mut(doc) else { return Task::none() };
        if !c.can_commit() {
            return Task::none();
        }
        let message = c.message.text();
        let amend = c.amend;
        c.busy = true;
        c.committing = true;
        c.error = None;
        c.done = None;
        c.chosen = None;
        c.reads += 1;
        let n = c.reads;
        let repo = c.repo.clone();
        tracing::info!(repo = %repo.worktree.display(), amend, "committing");
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let done = repo.commit(&message, amend);
            let _ = tx.send_blocking((done, repo.working_changes()));
        });
        Task::perform(
            async move {
                let stopped = || Err("committing stopped".to_owned());
                rx.recv().await.unwrap_or_else(|_| (stopped(), Err("reading the changes stopped".into())))
            },
            move |(done, read)| Message::GitView(GitViewMsg::Committed(doc, n, done, Box::new(read))),
        )
    }

    pub(super) fn update_changes(&mut self, msg: GitViewMsg) -> Task<Message> {
        match msg {
            GitViewMsg::OpenChanges => return self.open_changes(),
            GitViewMsg::ChangesRead(doc, n, read) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.take(n, *read);
                }
            }
            GitViewMsg::ToggleChange(doc, section, path) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.toggle_file(section, path);
                }
            }
            GitViewMsg::ToggleSection(doc, section) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.toggle_section(section);
                }
            }
            GitViewMsg::FoldAll(doc, fold) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.fold_all(fold);
                }
                // What was in view has moved: start from the top.
                return operation::snap_to(CHANGES_SCROLL, RelativeOffset::START);
            }
            GitViewMsg::Move(doc, pick) => return self.move_pick(doc, pick),
            GitViewMsg::Moved(doc, n, done, read) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.busy = false;
                    if let Err(e) = done {
                        tracing::warn!(repo = %c.repo.worktree.display(), error = %e, "staging failed");
                        c.error = Some(e);
                    }
                    c.take(n, *read);
                }
            }
            GitViewMsg::ChooseLine(doc, section, path, hunk, line, mods) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.choose(section, path, hunk, line, mods);
                }
            }
            GitViewMsg::DragLine(doc, line) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.drag_to(line);
                }
            }
            GitViewMsg::ClearChosen(doc) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.chosen = None;
                }
            }
            GitViewMsg::DismissNote(doc) => {
                if let Some(c) = self.changes_doc_mut(doc) {
                    c.error = None;
                    c.done = None;
                }
            }
            GitViewMsg::MessageEdit(doc, action) => {
                if let Some(c) = self.changes_doc_mut(doc).filter(|c| !c.committing) {
                    c.message.perform(action);
                }
            }
            GitViewMsg::Amend(doc, on) => return self.set_amend(doc, on),
            GitViewMsg::LastMessage(doc, read) => {
                let Some(c) = self.changes_doc_mut(doc) else { return Task::none() };
                match read {
                    // Only into an empty box: what the user wrote meanwhile stays.
                    Ok(text) if c.amend && c.message.text().trim().is_empty() => {
                        c.message = text_editor::Content::with_text(&text);
                        c.amend_text = Some(text);
                    }
                    Ok(_) => {}
                    Err(e) => c.error = Some(format!("Could not read the last commit's message: {e}")),
                }
            }
            GitViewMsg::Commit(doc) => return self.commit(doc),
            GitViewMsg::Committed(doc, n, result, read) => {
                let Some(c) = self.changes_doc_mut(doc) else { return Task::none() };
                c.busy = false;
                c.committing = false;
                match result {
                    Ok(done) => {
                        tracing::info!(repo = %c.repo.worktree.display(), "{done}");
                        c.done = Some(done);
                        c.message = text_editor::Content::new();
                        c.amend = false;
                        c.amend_text = None;
                    }
                    Err(e) => {
                        tracing::warn!(repo = %c.repo.worktree.display(), error = %e, "committing failed");
                        c.error = Some(e);
                    }
                }
                c.take(n, *read);
                // The log shows the new commit.
                return self.refresh_log();
            }
            _ => {}
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use termroost_core::git::DiffLine;

    use super::*;

    fn doc() -> ChangesDoc {
        let line = |kind, text: &str| DiffLine { kind, text: text.into(), old: None, new: None, no_newline: false };
        let hunk = Hunk {
            old_start: 1,
            old_lines: 4,
            new_start: 1,
            new_lines: 4,
            context: String::new(),
            lines: vec![
                line(LineKind::Context, "a"),
                line(LineKind::Removed, "b"),
                line(LineKind::Added, "B"),
                line(LineKind::Context, "c"),
                line(LineKind::Removed, "d"),
                line(LineKind::Added, "D"),
            ],
        };
        let file = FileDiff {
            path: "f.txt".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            large: false,
            mode: None,
            hunks: vec![hunk],
            lossy: false,
        };
        let changes = WorkingChanges { unstaged: vec![file], ..WorkingChanges::default() };
        let repo = Repo { worktree: "/w".into(), git_dir: "/w/.git".into(), common_dir: "/w/.git".into() };
        let mut doc = ChangesDoc::new(repo);
        doc.changes = Some(Ok(changes));
        doc
    }

    fn chosen(doc: &ChangesDoc) -> Vec<usize> {
        doc.chosen.as_ref().map(|c| c.lines.iter().copied().collect()).unwrap_or_default()
    }

    #[test]
    fn lines_are_chosen_one_by_one_or_as_a_range() {
        let none = keyboard::Modifiers::empty();
        let (shift, ctrl) = (keyboard::Modifiers::SHIFT, keyboard::Modifiers::CTRL);
        let mut d = doc();
        let press = |d: &mut ChangesDoc, line, mods| d.choose(Section::Unstaged, "f.txt".into(), 0, line, mods);

        press(&mut d, 2, none);
        assert_eq!(chosen(&d), vec![2]);
        // A range keeps to changed lines.
        press(&mut d, 5, shift);
        assert_eq!(chosen(&d), vec![2, 4, 5]);
        press(&mut d, 4, ctrl);
        assert_eq!(chosen(&d), vec![2, 5]);
        // Dragging from where it was last pressed.
        d.drag_to(1);
        assert_eq!(chosen(&d), vec![1, 2, 4]);

        // Pressing the only line chosen, or a line that didn't change, chooses none.
        press(&mut d, 1, none);
        press(&mut d, 1, none);
        assert_eq!(d.chosen, None);
        press(&mut d, 1, none);
        press(&mut d, 3, none);
        assert_eq!(d.chosen, None);

        // Taking out the last line with Ctrl chooses none too.
        press(&mut d, 5, none);
        press(&mut d, 5, ctrl);
        assert_eq!(d.chosen, None);

        // Not a file whose hunks can be staged on their own.
        d.choose(Section::Staged, "f.txt".into(), 0, 1, none);
        assert_eq!(d.chosen, None);
    }

    #[test]
    fn chosen_lines_go_when_their_hunk_changes() {
        let mut d = doc();
        d.choose(Section::Unstaged, "f.txt".into(), 0, 1, keyboard::Modifiers::empty());
        let Some(Ok(mut changes)) = d.changes.clone() else { unreachable!() };
        // Another file changed: they stay.
        changes.untracked.push(FileDiff { path: "new.txt".into(), ..changes.unstaged[0].clone() });
        d.take(1, Ok(changes.clone()));
        assert_eq!(chosen(&d), vec![1]);
        // Their hunk changed: they go.
        changes.unstaged[0].hunks[0].lines[1].text = "b2".into();
        d.take(2, Ok(changes));
        assert_eq!(d.chosen, None);
    }

    #[test]
    fn staged_files_start_folded() {
        let mut d = doc();
        let Some(Ok(changes)) = d.changes.clone() else { unreachable!() };
        let file = &changes.unstaged[0];
        assert!(!d.folded(Section::Unstaged, file, 1));
        assert!(d.folded(Section::Staged, file, 1));
        // Unfolded by hand, a staged file stays so; the same file unstaged is apart.
        d.toggle_file(Section::Staged, file.path.clone());
        assert!(!d.folded(Section::Staged, file, 1));
        assert!(!d.folded(Section::Unstaged, file, 1));
    }

    #[test]
    fn every_file_folds_and_unfolds_at_once() {
        let mut d = doc();
        let Some(Ok(mut changes)) = d.changes.clone() else { unreachable!() };
        let file = changes.unstaged[0].clone();
        assert!(d.any_unfolded(&changes, 200));

        d.fold_all(true);
        assert!(d.folded(Section::Unstaged, &file, 1));
        assert!(!d.any_unfolded(&changes, 200));
        // Files that come later fold too.
        let later = FileDiff { path: "later.txt".into(), ..file.clone() };
        changes.unstaged.push(later.clone());
        assert!(d.folded(Section::Unstaged, &later, 2));
        // One unfolded by hand is the exception, until all unfold.
        d.toggle_file(Section::Unstaged, later.path.clone());
        assert!(!d.folded(Section::Unstaged, &later, 2));
        assert!(d.any_unfolded(&changes, 200));
        d.fold_all(false);
        assert!(!d.folded(Section::Unstaged, &file, 2));
        assert!(!d.folded(Section::Unstaged, &later, 2));
        assert!(!d.folded(Section::Staged, &file, 1));
        // Folded by hand after that, a file stays folded.
        d.toggle_file(Section::Unstaged, file.path.clone());
        assert!(d.folded(Section::Unstaged, &file, 2));

        // Files of folded sections don't count as showing their changes.
        d.fold_all(false);
        d.toggle_section(Section::Unstaged);
        assert!(!d.any_unfolded(&changes, 200));
        d.toggle_section(Section::Unstaged);
        assert!(d.any_unfolded(&changes, 200));
    }
}
