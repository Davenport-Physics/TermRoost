//! Removing a git worktree from the Worktrees dialog. The sessions working
//! in it are closed, and git deletes the folder once the processes that ran
//! there have exited (Windows cannot delete a folder a process works in).
//! That goes on in the background: the dialog goes back to the list, which
//! marks the worktree until git is done.

use std::path::{Path, PathBuf};
use std::time::Duration;

use iced::widget::{column, container, row, text};
use iced::{Alignment, Element, Length, Task};
use termroost_core::TabId;
use termroost_core::detect::{self, FolderProcess};
use termroost_core::git::{self, Head, Repo};

use crate::app::{App, Dialog, Message, RunningTerminal};
use crate::dialogs::DialogParts;
use crate::sidebar::{branch_label, display_dir, muted_text};
use crate::theme::{self, Tone};
use crate::ui;
use crate::worktree::WorktreeForm;

/// How long to wait for the stopped terminals' processes before git deletes
/// the folder anyway.
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// State of the "Remove worktree?" dialog.
#[derive(Clone, Debug)]
pub struct RemoveForm {
    /// The Worktrees dialog to go back to.
    pub back: Box<WorktreeForm>,
    pub path: PathBuf,
    pub head: Option<Head>,
    /// The sessions working in it, which get closed.
    pub sessions: Vec<TabId>,
    /// Terminals of those sessions running a program.
    pub running: Vec<RunningTerminal>,
    /// Terminals of other sessions in the folder running a program: they
    /// stay, and can keep git from deleting the folder.
    pub blocking: Vec<RunningTerminal>,
    /// Programs outside the terminals working in the folder, looked for in
    /// the background. git cannot delete the folder while they run.
    pub others: Option<Vec<FolderProcess>>,
    /// End `others` first.
    pub end_others: bool,
    /// Uncommitted changes, counted in the background.
    pub changes: Option<Result<usize, String>>,
    /// Delete uncommitted changes too.
    pub discard: bool,
    /// An earlier attempt removed the worktree from git but left part of
    /// its folder.
    pub leftover: bool,
    /// Why an earlier attempt failed.
    pub error: Option<String>,
}

/// A worktree being removed in the background, or whose removal failed
/// (see [`App::removals`]).
#[derive(Clone, Debug)]
pub struct Removal {
    /// The repository's shared git directory, to tell its worktrees from
    /// other repositories'.
    pub common_dir: PathBuf,
    pub path: PathBuf,
    pub head: Option<Head>,
    /// Why git did not remove it; `None` while it is at it.
    pub failed: Option<String>,
    /// git no longer knows the worktree, but part of its folder is left.
    pub leftover: bool,
    /// The error banner put up about the failure, taken down on a retry.
    pub banner: Option<String>,
}

#[derive(Clone, Debug)]
pub enum RemoveMsg {
    /// Ask before removing this worktree (from the Worktrees dialog).
    Ask(PathBuf),
    Changes(PathBuf, Result<usize, String>),
    Others(PathBuf, Vec<FolderProcess>),
    Discard(bool),
    EndOthers(bool),
    Confirm,
    Removed {
        path: PathBuf,
        result: Result<(), String>,
        /// Part of the folder is left, and git no longer knows the worktree.
        leftover: bool,
    },
}

impl RemoveForm {
    fn can_remove(&self) -> bool {
        let changes = self.leftover
            || match &self.changes {
                None => false,
                Some(Ok(n)) => *n == 0 || self.discard,
                // git checks again, and refuses without `discard`.
                Some(Err(_)) => true,
            };
        // Deleting around a program would leave part of the folder.
        let others = self.others.as_ref().is_some_and(|o| o.is_empty() || self.end_others);
        changes && others
    }
}

fn msg(m: RemoveMsg) -> Message {
    Message::RemoveWorktree(m)
}

impl App {
    pub(crate) fn update_remove(&mut self, m: RemoveMsg) -> Task<Message> {
        match m {
            RemoveMsg::Ask(path) => return self.ask_remove(path),
            RemoveMsg::Changes(path, changes) => {
                if let Some(f) = self.remove_form().filter(|f| f.path == path) {
                    f.changes = Some(changes);
                }
            }
            RemoveMsg::Others(path, found) => {
                if let Some(f) = self.remove_form().filter(|f| f.path == path) {
                    f.others = Some(found);
                }
            }
            RemoveMsg::Discard(on) => {
                if let Some(f) = self.remove_form() {
                    f.discard = on;
                }
            }
            RemoveMsg::EndOthers(on) => {
                if let Some(f) = self.remove_form() {
                    f.end_others = on;
                }
            }
            RemoveMsg::Confirm => return self.confirm_remove(),
            RemoveMsg::Removed { path, result, leftover } => return self.removed(path, result, leftover),
        }
        Task::none()
    }

    pub(crate) fn remove_form(&mut self) -> Option<&mut RemoveForm> {
        match &mut self.dialog {
            Some(Dialog::RemoveWorktree(f)) => Some(f),
            _ => None,
        }
    }

    /// The removal of the worktree at `path`, running or failed.
    pub(crate) fn removal(&self, path: &Path) -> Option<&Removal> {
        self.removals.iter().find(|r| git::same_path(&r.path, path))
    }

    /// Folders of the worktrees git is removing now, for the quit dialog.
    pub(crate) fn removing_worktrees(&self) -> Vec<String> {
        self.removals.iter().filter(|r| r.failed.is_none()).map(|r| display_dir(&r.path)).collect()
    }

    /// Swaps the Worktrees dialog for the question whether to remove `path`.
    fn ask_remove(&mut self, path: PathBuf) -> Task<Message> {
        let Some(Dialog::Worktree(back)) = self.dialog.take() else { return Task::none() };
        let earlier = self.removal(&path).cloned();
        if earlier.as_ref().is_some_and(|r| r.failed.is_none()) {
            // git is at it already.
            self.dialog = Some(Dialog::Worktree(back));
            return Task::none();
        }
        let leftover = earlier.as_ref().is_some_and(|r| r.leftover);
        let s = self.core.state();
        let sessions: Vec<TabId> =
            s.tabs.iter().filter(|t| git::is_within(&s.tab_working_dir(t), &path)).map(|t| t.id).collect();
        let in_folder =
            |pane| s.pane(pane).and_then(|p| p.cwd.as_deref()).is_some_and(|cwd| git::is_within(cwd, &path));
        let (running, blocking) = self
            .running_terminals()
            .into_iter()
            .filter(|t| s.pane(t.pane).is_some_and(|p| sessions.contains(&p.tab)) || in_folder(t.pane))
            .partition(|t| s.pane(t.pane).is_some_and(|p| sessions.contains(&p.tab)));
        let head = back
            .branches
            .as_ref()
            .and_then(|b| b.as_ref().ok())
            .and_then(|b| b.worktrees.iter().find(|w| w.path == path))
            .and_then(|w| w.head.clone())
            .or_else(|| earlier.as_ref().and_then(|r| r.head.clone()));
        self.dialog = Some(Dialog::RemoveWorktree(Box::new(RemoveForm {
            back,
            path: path.clone(),
            head,
            sessions,
            running,
            blocking,
            others: None,
            end_others: false,
            changes: None,
            discard: false,
            leftover,
            error: earlier.and_then(|r| r.failed),
        })));
        let others = self.look_for_others(path.clone());
        // What is left of a worktree git forgot has no changes to count.
        if leftover {
            return others;
        }
        let (tx, rx) = async_channel::bounded(1);
        let dir = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(Repo::changes(&dir));
        });
        let changes = Task::perform(async move { rx.recv().await.unwrap_or_else(|e| Err(e.to_string())) }, move |c| {
            msg(RemoveMsg::Changes(path.clone(), c))
        });
        Task::batch([changes, others])
    }

    /// Looks for programs working in the folder other than the terminals'.
    fn look_for_others(&self, path: PathBuf) -> Task<Message> {
        let shells = self.core.shell_pids();
        let (tx, rx) = async_channel::bounded(1);
        let dir = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(detect::processes_in(&dir, &shells));
        });
        Task::perform(async move { rx.recv().await.unwrap_or_default() }, move |found| {
            msg(RemoveMsg::Others(path.clone(), found))
        })
    }

    /// Back to the Worktrees dialog, unless its session was closed meanwhile.
    pub(crate) fn cancel_remove(&mut self) {
        if let Some(Dialog::RemoveWorktree(f)) = self.dialog.take()
            && self.core.state().tab(f.back.tab).is_some()
        {
            self.dialog = Some(Dialog::Worktree(f.back));
        }
    }

    /// Closes the worktree's sessions (and ends the other programs working
    /// there, if asked to), then has git remove it on a thread once their
    /// processes are gone. After git left part of the folder, deletes the rest.
    /// Meanwhile the Worktrees dialog is back, unless its own session closed.
    fn confirm_remove(&mut self) -> Task<Message> {
        if !self.remove_form().is_some_and(|f| f.can_remove()) {
            return Task::none();
        }
        let Some(Dialog::RemoveWorktree(f)) = self.dialog.take() else { return Task::none() };
        let RemoveForm { back, path, head, others, end_others, discard: force, leftover: was_leftover, .. } = *f;
        let others = if end_others { others.unwrap_or_default() } else { Vec::new() };
        let repo = back.repo.clone();
        let mut left = self.core.leave_dir(&path);
        for tab in &left.closed {
            self.grids.remove(tab);
            self.activity.remove(tab);
        }
        if self.core.state().tab(back.tab).is_some() {
            self.dialog = Some(Dialog::Worktree(back));
        }
        if let Some(earlier) = self.removal(&path)
            && earlier.banner.is_some()
            && earlier.banner == self.error
        {
            self.error = None;
        }
        self.removals.retain(|r| !git::same_path(&r.path, &path));
        self.removals.push(Removal {
            common_dir: repo.common_dir.clone(),
            path: path.clone(),
            head,
            failed: None,
            leftover: false,
            banner: None,
        });
        let (tx, rx) = async_channel::bounded(1);
        let target = path.clone();
        std::thread::spawn(move || {
            left.pids.extend(detect::end_processes(&others));
            if !left.wait(EXIT_WAIT) {
                tracing::warn!(path = %target.display(), "processes still running in a worktree being removed");
            }
            let result =
                if was_leftover { repo.remove_leftover(&target) } else { repo.remove_worktree(&target, force) };
            let leftover = result.is_err() && target.exists() && repo.has_worktree(&target) == Ok(false);
            let _ = tx.send_blocking((result, leftover));
        });
        let removed = Task::perform(
            async move { rx.recv().await.unwrap_or_else(|e| (Err(e.to_string()), false)) },
            move |(result, leftover)| msg(RemoveMsg::Removed { path: path.clone(), result, leftover }),
        );
        Task::batch([removed, self.after_select()])
    }

    /// git is done with the worktree at `path`: it is gone, or the Worktrees
    /// dialog says why not on its row (a banner does when the dialog is closed).
    fn removed(&mut self, path: PathBuf, result: Result<(), String>, leftover: bool) -> Task<Message> {
        let Some(i) = self.removals.iter().position(|r| r.path == path) else { return Task::none() };
        let common_dir = self.removals[i].common_dir.clone();
        match result {
            Ok(()) => {
                tracing::info!(path = %path.display(), "worktree removed");
                self.removals.remove(i);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, leftover, "removing a worktree failed");
                let banner = self.worktrees_of(&common_dir).is_none().then(|| {
                    let name =
                        path.file_name().map_or_else(|| display_dir(&path), |n| n.to_string_lossy().into_owned());
                    if leftover {
                        format!("The worktree {name} is gone from git, but part of its folder is left: {e}")
                    } else {
                        format!("git did not remove the worktree {name}: {e}")
                    }
                });
                if banner.is_some() {
                    self.error.clone_from(&banner);
                }
                let r = &mut self.removals[i];
                r.failed = Some(e);
                r.leftover = leftover;
                r.banner = banner;
            }
        }
        Task::batch([self.reload_worktrees(&common_dir), self.read_git()])
    }

    /// Brings the failed removals of a repository in line with git's list
    /// of its worktrees: one whose folder is gone meanwhile is dropped, one
    /// git no longer lists is left over, one it still lists is not.
    pub(crate) fn settle_removals(&mut self, common_dir: &Path, listed: &[PathBuf]) {
        self.removals.retain_mut(|r| {
            if r.failed.is_none() || !git::same_path(&r.common_dir, common_dir) {
                return true;
            }
            if !r.path.exists() {
                return false;
            }
            r.leftover = !listed.iter().any(|p| git::same_path(p, &r.path));
            true
        });
    }

    pub(crate) fn view_remove<'a>(&'a self, f: &'a RemoveForm) -> DialogParts<'a> {
        let s = self.core.state();
        let project = s.tab(f.back.tab).map(|t| t.project);
        let mut col = column![].spacing(12);

        let mut what = row![].spacing(8).align_y(Alignment::Center);
        if let Some(head) = &f.head {
            what = what.push(branch_label(head, 13.0, 40));
        }
        col = col.push(
            container(what.push(muted_text(display_dir(&f.path)).size(12)))
                .padding([8, 12])
                .width(Length::Fill)
                .style(theme::list_box),
        );
        col = col.push(
            text(match &f.head {
                Some(Head::Branch(b)) => {
                    format!("This deletes the folder and everything in it. The branch {b} is kept.")
                }
                _ => "This deletes the folder and everything in it.".to_owned(),
            })
            .size(13),
        );

        let names: Vec<String> = f
            .sessions
            .iter()
            .filter_map(|t| s.tab(*t))
            .map(|t| match s.project(t.project) {
                Some(p) if Some(t.project) != project => format!("{} ({})", t.title, p.name),
                _ => t.title.clone(),
            })
            .collect();
        let this_one = if f.sessions.contains(&f.back.tab) { ", this one included" } else { "" };
        col = col.push(
            text(match names.len() {
                0 => "No session works there.".to_owned(),
                1 => format!("It closes the session working there{this_one}: {}.", names[0]),
                n => format!("It closes the {n} sessions working there{this_one}: {}.", names.join(", ")),
            })
            .size(13),
        );
        if !f.running.is_empty() {
            let lines = f.running.iter().map(|t| muted_text(format!("{}: {}", t.place, t.programs)).size(12).into());
            col = col.push(column![text("These stop:").size(13), column(lines).spacing(2).padding([0, 12])].spacing(4));
        }
        for t in &f.blocking {
            col = col.push(ui::note_text(
                Tone::Danger,
                format!(
                    "{} has a terminal in this folder running {}. Stop it first, or git may not be able to delete \
                     the folder.",
                    t.place, t.programs
                ),
            ));
        }
        if let Some(others) = f.others.as_ref().filter(|o| !o.is_empty()) {
            col = col.push(ui::note(
                Tone::Warning,
                column![
                    text(format!(
                        "Other programs work in this folder, and it cannot be deleted while they run: {}.",
                        program_list(others.iter().map(|p| p.name.as_str()))
                    ))
                    .size(13),
                    ui::check(f.end_others).label("End them").on_toggle(|on| msg(RemoveMsg::EndOthers(on))),
                ]
                .spacing(8),
            ));
        }

        let changes: Element<'_, Message> = match &f.changes {
            None => muted_text("Checking for uncommitted changes…").size(12).into(),
            Some(Ok(0)) => muted_text("No uncommitted changes.").size(12).into(),
            Some(Ok(n)) => ui::note(
                Tone::Warning,
                column![
                    text(format!(
                        "It has {n} uncommitted change{}: edited files, or new files git does not ignore.",
                        if *n == 1 { "" } else { "s" }
                    ))
                    .size(13),
                    ui::check(f.discard).label("Delete them too").on_toggle(|on| msg(RemoveMsg::Discard(on))),
                ]
                .spacing(8),
            ),
            Some(Err(e)) => ui::note_text(Tone::Danger, format!("git could not check for changes: {e}")),
        };
        // Once the worktree is gone from git, what is left of it just goes.
        if !f.leftover {
            col = col.push(changes);
        }
        if let Some(e) = &f.error {
            col = col.push(ui::note_text(
                Tone::Danger,
                if f.leftover {
                    format!("The worktree is gone from git, but part of its folder is left: {e}")
                } else {
                    format!("git did not remove it: {e}")
                },
            ));
        }

        let label = if f.leftover { "Delete the rest" } else { "Remove worktree" };
        DialogParts::new("Remove worktree?", col)
            .button(ui::secondary("Back").on_press(Message::DialogCancel))
            .button(ui::danger(label).on_press_maybe(f.can_remove().then(|| msg(RemoveMsg::Confirm))))
    }
}

/// Program names with how many of each run, most first:
/// `conhost.exe ×3, cmd.exe ×2, more.com`.
fn program_list<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for name in names {
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let items: Vec<String> =
        counts.iter().map(|(name, n)| if *n == 1 { (*name).to_owned() } else { format!("{name} ×{n}") }).collect();
    items.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_lists() {
        let names = ["cmd.exe", "conhost.exe", "more.com", "conhost.exe", "cmd.exe", "conhost.exe"];
        assert_eq!(program_list(names.into_iter()), "conhost.exe ×3, cmd.exe ×2, more.com");
        assert_eq!(program_list(["node.exe"].into_iter()), "node.exe");
    }
}
