//! The Git tab of the side panel: the log of the visible session's
//! repository, between the Tasks and Files tabs. It follows new commits:
//! every git poll compares the repository's refs and config, and reads the
//! log again when they changed. Commits that are on no remote are marked,
//! and the branch's distance to its upstream is shown, with Pull (rebasing
//! by default) and Push. A commit clicked shows what it changed in place of
//! the terminals (see `git_view`).

use std::path::PathBuf;

use iced::widget::{button, column, container, pick_list, row, space, text};
use iced::{Alignment, Element, Length, Task};
use termroost_core::git::{Commit, Head, Repo, Tracking};

use crate::app::{App, Message};
use crate::git_view::GitViewMsg;
use crate::icons::Icon;
use crate::remotes::RemotesMsg;
use crate::side_panel::SideTab;
use crate::sidebar::{branch_label, muted_text};
use crate::theme::{self, Tone};
use crate::ui::{self, icon_button};

/// How many commits the log shows.
const LOG_LIMIT: usize = 200;

/// What the Git tab does with the remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOp {
    Pull,
    Push,
    /// Make the checked-out branch pull from and push to this remote.
    UseRemote(String),
}

/// A pull, push or change of remote: running while `result` is `None`.
#[derive(Clone, Debug)]
pub struct GitSync {
    pub worktree: PathBuf,
    pub op: SyncOp,
    /// What happened, in a sentence, or what git said went wrong.
    pub result: Option<Result<String, String>>,
}

/// The log on screen, and the working tree it comes from.
#[derive(Clone, Debug)]
pub struct GitLog {
    pub worktree: PathBuf,
    /// Every branch, not just the one checked out.
    pub all: bool,
    /// `None` while it is read.
    pub commits: Option<Result<Vec<Commit>, String>>,
    /// [`Repo::ref_stamp`] when `commits` were read.
    pub stamp: Option<u64>,
    /// How the commits stand against the remotes; `None` until read, or
    /// when git could not tell.
    pub tracking: Option<Tracking>,
    /// How many files (or new folders) changed since the last commit;
    /// `None` until read.
    pub changes: Option<usize>,
}

/// A log read in the background.
#[derive(Clone, Debug)]
pub struct LogRead {
    pub worktree: PathBuf,
    pub all: bool,
    pub stamp: u64,
    /// `None` when the refs have not moved since the log on screen was read.
    pub commits: Option<Result<Vec<Commit>, String>>,
    pub tracking: Option<Tracking>,
    pub changes: Option<usize>,
}

/// `s` cut to `max` characters, ending in "…" when cut.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// "5m ago", or the date for anything older than a week.
pub(crate) fn ago(time: i64, now: i64) -> String {
    let secs = (now - time).max(0);
    match secs {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{}m ago", secs / 60),
        3_600..86_400 => format!("{}h ago", secs / 3_600),
        86_400..604_800 => format!("{}d ago", secs / 86_400),
        _ => chrono::DateTime::from_timestamp(time, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
    }
}

impl App {
    /// The repository of the session on screen, when it is in one.
    pub(crate) fn active_repo(&self) -> Option<&Repo> {
        let tab = self.core.state().active_tab()?;
        Some(&self.git_of(tab)?.repo)
    }

    /// Whether the session on screen is in a repository, which the Git tab
    /// needs.
    pub(crate) fn in_repo(&self) -> bool {
        self.active_repo().is_some()
    }

    /// Whether the side panel shows the Git tab now. It needs a session in a
    /// repository; otherwise the panel shows the tasks.
    pub(crate) fn git_tab_shown(&self) -> bool {
        self.prefs.panel_open && self.shown_side_tab() == SideTab::Git
    }

    /// Reads the log when the Git tab shows a repository it has not read yet
    /// (such as after switching sessions), or when its refs moved since.
    pub(crate) fn refresh_log(&mut self) -> Task<Message> {
        if !self.git_tab_shown() {
            return Task::none();
        }
        let Some(repo) = self.active_repo().cloned() else { return Task::none() };
        let all = self.prefs.log_all_branches;
        let known = match &self.git_log {
            Some(log) if log.worktree == repo.worktree && log.all == all => log.stamp,
            _ => {
                // A read still running is for the old log: dropped when it
                // arrives, which starts this one.
                self.git_log = Some(GitLog {
                    worktree: repo.worktree.clone(),
                    all,
                    commits: None,
                    stamp: None,
                    tracking: None,
                    changes: None,
                });
                None
            }
        };
        if self.log_reading {
            return Task::none();
        }
        self.log_reading = true;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            // The stamp first: a commit landing in between is caught next time.
            let stamp = repo.ref_stamp();
            let changed = known != Some(stamp);
            let commits = changed.then(|| repo.log(all, LOG_LIMIT));
            let tracking = changed
                .then(|| repo.tracking(all, LOG_LIMIT))
                .and_then(|t| t.map_err(|e| tracing::warn!(error = %e, "reading the git remotes failed")).ok());
            // Files change without refs moving: counted every time.
            let changes = repo.changes(&repo.worktree).ok();
            let _ = tx.send_blocking(LogRead { worktree: repo.worktree, all, stamp, commits, tracking, changes });
        });
        Task::perform(async move { rx.recv().await.ok() }, |read| match read {
            Some(read) => Message::LogRead(read),
            None => Message::Noop,
        })
    }

    /// Takes in a log read in the background, unless the panel moved on to
    /// another repository (or the other branch setting) meanwhile.
    pub(crate) fn log_read(&mut self, read: LogRead) -> Task<Message> {
        self.log_reading = false;
        match &mut self.git_log {
            Some(log) if log.worktree == read.worktree && log.all == read.all => {
                log.changes = read.changes;
                match read.commits {
                    None => {}
                    Some(Ok(commits)) => {
                        log.commits = Some(Ok(commits));
                        log.stamp = Some(read.stamp);
                        log.tracking = read.tracking;
                    }
                    // Without a stamp, the next poll tries again.
                    Some(Err(e)) => {
                        tracing::warn!(repo = %read.worktree.display(), error = %e, "reading the git log failed");
                        log.commits = Some(Err(e));
                        log.stamp = None;
                    }
                }
                Task::none()
            }
            _ => self.refresh_log(),
        }
    }

    /// The Git tab: the branch on screen and its latest commits.
    pub(crate) fn view_git_log(&self) -> Element<'_, Message> {
        let s = self.core.state();
        let head = s.active_tab().and_then(|t| self.git_of(t)).and_then(|g| g.head.as_ref());
        let folder = self.active_repo().and_then(|r| r.worktree.file_name()).map(|n| n.to_string_lossy().into_owned());
        let mut title = column![].spacing(4);
        if let Some(head) = head {
            title = title.push(branch_label(head, 13.0, 36));
        }
        let mut options = row![].spacing(8).align_y(Alignment::Center);
        if let Some(folder) = folder {
            options = options.push(muted_text(format!("in {}", shorten(&folder, 24))).wrapping(text::Wrapping::None));
        }
        let title = title.push(
            options.push(space::horizontal()).push(
                ui::check(self.prefs.log_all_branches)
                    .label("All branches")
                    .on_toggle(Message::LogAllBranches)
                    .text_size(12),
            ),
        );

        let tracking = self.git_log.as_ref().and_then(|l| l.tracking.as_ref());
        let mut title = title;
        if let Some(t) = tracking {
            title = title.push(self.remote_row(t, head));
        }
        if let Some(status) = tracking.and_then(|t| remote_status(t, head)) {
            title = title.push(muted_text(status).size(12));
        }
        if let Some(t) = tracking.filter(|t| t.remote.is_some()) {
            title = title.push(self.sync_buttons(t, head));
        }
        if let Some(note) = self.sync_note() {
            title = title.push(note);
        }

        let changes = changes_item(self.git_log.as_ref().and_then(|l| l.changes), self.changes_shown());
        let body: Element<'_, Message> = match self.git_log.as_ref().and_then(|l| l.commits.as_ref()) {
            None => muted_text("Reading the log…").size(12).into(),
            Some(Err(e)) => ui::note_text(Tone::Danger, e),
            Some(Ok(commits)) if commits.is_empty() => {
                column![changes, container(muted_text("No commits yet.").size(12)).padding([0, 8])].spacing(8).into()
            }
            Some(Ok(commits)) => {
                let now = chrono::Utc::now().timestamp();
                let unpushed = |c: &Commit| tracking.is_some_and(|t| t.unpushed.contains(&c.id));
                let shown = self.shown_commit();
                let items = commits.iter().map(|c| commit_item(c, unpushed(c), shown == Some(&c.id), now));
                let list = column(std::iter::once(changes).chain(items))
                    .spacing(4)
                    .padding(iced::Padding { right: 10.0, ..iced::Padding::ZERO });
                ui::scroll(list).height(Length::Fill).into()
            }
        };
        column![title, body].spacing(10).into()
    }

    /// The pull, push or change of remote that is running, if any.
    fn running_sync(&self) -> Option<&SyncOp> {
        self.git_sync.as_ref().filter(|s| s.result.is_none()).map(|s| &s.op)
    }

    /// The remote the branch pulls from and pushes to, and its URLs. With
    /// more than one remote, a menu picks another for the branch; a button
    /// opens the Remotes dialog.
    fn remote_row<'a>(&'a self, t: &'a Tracking, head: Option<&Head>) -> Element<'a, Message> {
        let open = Message::Remotes(RemotesMsg::Open);
        let Some(current) = t.remote.as_deref() else {
            let add = ui::small_with_icon(Icon::Plus, "Add remote", theme::button_secondary).on_press(open);
            return row![muted_text("No remote yet").size(12), space::horizontal(), add]
                .align_y(Alignment::Center)
                .into();
        };
        let switching = match self.running_sync() {
            Some(SyncOp::UseRemote(r)) => Some(r.as_str()),
            _ => None,
        };
        let shown = switching.unwrap_or(current);
        let can_pick = self.running_sync().is_none() && matches!(head, Some(Head::Branch(_))) && t.remotes.len() > 1;
        let name: Element<'_, Message> = if can_pick {
            let names: Vec<String> = t.remotes.iter().map(|r| r.name.clone()).collect();
            let chosen = current.to_owned();
            pick_list(names, Some(chosen.clone()), move |r: String| {
                if r == chosen { Message::Noop } else { Message::GitSync(SyncOp::UseRemote(r)) }
            })
            .text_size(12)
            .padding([2, 8])
            .style(theme::dropdown)
            .menu_style(theme::dropdown_menu)
            .into()
        } else {
            text(shown).size(12).font(theme::SEMIBOLD).into()
        };
        let remote = t.remotes.iter().find(|r| r.name == shown);
        // URLs have no spaces to break at.
        let url = |s: String| muted_text(s).wrapping(text::Wrapping::WordOrGlyph);
        let manage = icon_button(Icon::Edit, open, "Add or change remotes");
        let mut col = column![
            row![muted_text("Remote").size(12), name, space::horizontal(), manage]
                .spacing(8)
                .align_y(Alignment::Center)
        ];
        col = col.push(match remote.map(|r| r.url.as_str()).filter(|u| !u.is_empty()) {
            Some(u) => url(u.to_owned()),
            None => muted_text("No URL"),
        });
        if let Some(push) = remote.and_then(|r| r.push_url.as_deref()) {
            col = col.push(url(format!("Pushes to {push}")));
        }
        if let Some(r) = switching {
            col = col.push(muted_text(format!("Fetching {r}…")));
        }
        col.spacing(2).into()
    }

    /// Pull (rebasing or not) and Push, for a repository with a remote.
    fn sync_buttons<'a>(&'a self, t: &Tracking, head: Option<&Head>) -> Element<'a, Message> {
        let running = self.running_sync();
        let on_branch = matches!(head, Some(Head::Branch(_)));
        let idle = running.is_none() && on_branch;
        let action = |glyph: Icon, label: &'a str, busy: &'a str, op: SyncOp, enabled: bool| {
            let label = if running == Some(&op) { busy } else { label };
            ui::small_with_icon(glyph, label, theme::button_secondary)
                .on_press_maybe((idle && enabled).then_some(Message::GitSync(op)))
        };
        row![
            // Pulling needs a remote branch to pull from.
            action(Icon::ArrowDown, "Pull", "Pulling…", SyncOp::Pull, t.upstream.is_some()),
            ui::check(self.prefs.pull_rebase).label("Rebase").on_toggle(Message::PullRebase).text_size(12),
            space::horizontal(),
            action(Icon::ArrowUp, "Push", "Pushing…", SyncOp::Push, true),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }

    /// What the last pull or push of this repository did, or why changing
    /// its remote failed, until dismissed.
    fn sync_note(&self) -> Option<Element<'_, Message>> {
        let sync = self.git_sync.as_ref()?;
        let shown = self.git_log.as_ref().is_some_and(|l| l.worktree == sync.worktree);
        let result = sync.result.as_ref().filter(|_| shown)?;
        let (message, tone) = match result {
            Ok(done) => (done, Tone::Neutral),
            Err(e) => (e, Tone::Danger),
        };
        let line =
            row![text(message).size(12).width(Length::Fill), icon_button(Icon::Close, Message::DismissSync, "Dismiss")]
                .spacing(4)
                .align_y(Alignment::Start);
        Some(ui::note(tone, line))
    }

    /// Pulls, pushes or changes the remote of the Git tab's repository on a
    /// thread.
    pub(crate) fn start_sync(&mut self, op: SyncOp) -> Task<Message> {
        if self.running_sync().is_some() {
            return Task::none();
        }
        let Some(repo) = self.active_repo().cloned() else { return Task::none() };
        let rebase = self.prefs.pull_rebase;
        tracing::info!(repo = %repo.worktree.display(), ?op, rebase, "git sync started");
        self.git_sync = Some(GitSync { worktree: repo.worktree.clone(), op: op.clone(), result: None });
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = match op {
                SyncOp::Pull => repo.pull(rebase),
                SyncOp::Push => repo.push(),
                SyncOp::UseRemote(remote) => repo.use_remote(&remote),
            };
            let _ = tx.send_blocking((repo.worktree, result));
        });
        Task::perform(async move { rx.recv().await.ok() }, |done| match done {
            Some((worktree, result)) => Message::GitSynced(worktree, result),
            None => Message::Noop,
        })
    }

    /// Shows how a pull or push went, or why a change of remote failed, and
    /// reads the log again right away.
    pub(crate) fn sync_done(&mut self, worktree: PathBuf, result: Result<String, String>) -> Task<Message> {
        match &result {
            Ok(done) => tracing::info!(repo = %worktree.display(), "{done}"),
            Err(e) => tracing::warn!(repo = %worktree.display(), error = %e, "git sync failed"),
        }
        if let Some(sync) = self.git_sync.as_mut().filter(|s| s.worktree == worktree) {
            sync.result = Some(result);
        }
        // The remote line shows the new remote; only a failure needs a note.
        if matches!(&self.git_sync, Some(GitSync { op: SyncOp::UseRemote(_), result: Some(Ok(_)), .. })) {
            self.git_sync = None;
        }
        // A pull that brought nothing leaves the refs as they were; read anyway.
        if let Some(log) = self.git_log.as_mut().filter(|l| l.worktree == worktree) {
            log.stamp = None;
        }
        Task::batch([self.refresh_log(), self.read_git()])
    }
}

/// Where the branch stands against its remote: "2 commits to push · 1 to
/// pull (origin/main)". `None` without a remote.
fn remote_status(t: &Tracking, head: Option<&Head>) -> Option<String> {
    let remote = t.remote.as_ref()?;
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    Some(match (&t.upstream, head) {
        (Some(upstream), _) if t.ahead == 0 && t.behind == 0 => format!("Up to date with {upstream}"),
        (Some(upstream), _) => {
            let mut parts = Vec::new();
            if t.ahead > 0 {
                parts.push(format!("{} commit{} to push", t.ahead, plural(t.ahead)));
            }
            if t.behind > 0 {
                parts.push(format!("{} to pull", t.behind));
            }
            format!("{} ({upstream})", parts.join(" · "))
        }
        (None, Some(Head::Branch(branch))) => format!("{branch} is not on {remote} yet"),
        (None, _) => format!("Not on a branch; {remote} is the remote"),
    })
}

/// The first item of the log: what is not committed yet, and how much.
/// Pressed, it shows the changes, to stage and commit them.
fn changes_item<'a>(count: Option<usize>, shown: bool) -> Element<'a, Message> {
    let mut item = column![text("Uncommitted changes").size(13)].spacing(3);
    if let Some(n) = count {
        item = item.push(muted_text(match n {
            0 => "Nothing to commit".to_owned(),
            1 => "1 change".to_owned(),
            n => format!("{n} changes"),
        }));
    }
    button(item)
        .padding([6, 8])
        .width(Length::Fill)
        .style(theme::list_button(shown))
        .on_press(Message::GitView(GitViewMsg::OpenChanges))
        .into()
}

/// One commit: subject, then id, author and age, then branches and tags,
/// and whether it is on no remote yet. Pressed, it shows what it changed;
/// the one `shown` is raised.
fn commit_item(c: &Commit, unpushed: bool, shown: bool, now: i64) -> Element<'_, Message> {
    let meta = row![
        text(&c.short)
            .font(theme::MONO)
            .size(11)
            .style(|t: &iced::Theme| text::Style { color: Some(theme::accent(t)) }),
        muted_text(&c.author).wrapping(text::Wrapping::None),
        muted_text(ago(c.time, now)).wrapping(text::Wrapping::None),
    ]
    .spacing(6);
    let mut item = column![text(&c.subject).size(13), meta].spacing(3);
    if !c.refs.is_empty() || unpushed {
        let head_branch = c.head.then(|| c.refs.first()).flatten();
        let mut badges: Vec<Element<'_, Message>> = Vec::new();
        if unpushed {
            badges.push(ui::badge("not pushed", Tone::Warning));
        }
        badges.extend(
            c.refs
                .iter()
                .map(|r| ui::badge(r.as_str(), if Some(r) == head_branch { Tone::Accent } else { Tone::Neutral })),
        );
        item = item.push(row(badges).spacing(4).wrap().vertical_spacing(4));
    }
    button(item)
        .padding([6, 8])
        .width(Length::Fill)
        .style(theme::list_button(shown))
        .on_press(Message::GitView(GitViewMsg::OpenCommit(c.id.clone())))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_statuses() {
        let main = Head::Branch("main".into());
        let t = |upstream: Option<&str>, ahead, behind| Tracking {
            remote: Some("origin".into()),
            upstream: upstream.map(Into::into),
            ahead,
            behind,
            ..Tracking::default()
        };
        let status = |t: Tracking| remote_status(&t, Some(&main));
        assert_eq!(status(t(Some("origin/main"), 0, 0)).unwrap(), "Up to date with origin/main");
        assert_eq!(status(t(Some("origin/main"), 1, 0)).unwrap(), "1 commit to push (origin/main)");
        assert_eq!(status(t(Some("origin/main"), 2, 3)).unwrap(), "2 commits to push · 3 to pull (origin/main)");
        assert_eq!(status(t(None, 0, 0)).unwrap(), "main is not on origin yet");
        assert_eq!(status(Tracking::default()), None, "no remote");
    }

    #[test]
    fn ages() {
        let now = 1_700_000_000;
        assert_eq!(ago(now - 5, now), "just now");
        assert_eq!(ago(now + 30, now), "just now");
        assert_eq!(ago(now - 125, now), "2m ago");
        assert_eq!(ago(now - 3 * 3_600, now), "3h ago");
        assert_eq!(ago(now - 2 * 86_400, now), "2d ago");
        assert_eq!(ago(now - 30 * 86_400, now).len(), "2023-10-15".len());
    }
}
