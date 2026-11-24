//! The Worktrees dialog: switching a session to another git worktree of its
//! repository, opening a new session in one, or making a new worktree (in a
//! new session, or this one when asked).
//!
//! New worktrees go beside the repository (so on the same drive), named
//! `<repository>-<branch>`. Removing one is in [`crate::worktree_remove`].

use std::path::{Path, PathBuf};

use iced::widget::{column, container, operation, row, space, text};
use iced::{Alignment, Element, Length, Task};
use termroost_core::git::{self, Branches, Head, Plan, Repo, Worktree};
use termroost_core::{Command, Outcome, TabId};

use crate::app::{App, DIALOG_FIRST_INPUT, Dialog, Message, expand_home};
use crate::dialogs::DialogParts;
use crate::sidebar::{branch_label, display_dir, muted_text, short_dir};
use crate::theme::{self, SEMIBOLD, Tone};
use crate::ui::{self, field};
use crate::worktree_remove::{Removal, RemoveMsg};

/// State of the Worktrees dialog.
#[derive(Clone, Debug)]
pub struct WorktreeForm {
    pub tab: TabId,
    /// The session's directory when the dialog opened.
    pub from: PathBuf,
    pub repo: Repo,
    pub head: Option<Head>,
    pub branch: String,
    /// Where a new branch starts; empty: the session's current commit.
    pub base: String,
    pub path: String,
    /// The location was typed in, so it no longer follows the branch name.
    pub path_edited: bool,
    /// A new worktree takes this session along; otherwise it gets a new session.
    pub move_session: bool,
    /// Branches and worktrees, read in the background when the dialog opens.
    pub branches: Option<Result<Branches, String>>,
    /// git is making the worktree.
    pub creating: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum WorktreeMsg {
    /// Open the dialog for the visible session.
    Open,
    Loaded(TabId, Result<Branches, String>),
    /// Move the session to this existing worktree.
    Switch(PathBuf),
    /// Open a new session in this existing worktree.
    NewSession(PathBuf),
    MoveSession(bool),
    Branch(String),
    Base(String),
    Path(String),
    Submit,
    Created {
        tab: TabId,
        branch: String,
        path: PathBuf,
        result: Result<(), String>,
    },
}

impl WorktreeForm {
    /// What submitting would do; `None` until there is a branch name and the
    /// branches are known.
    fn plan(&self) -> Option<Plan> {
        let name = self.branch.trim();
        match &self.branches {
            Some(Ok(b)) if !name.is_empty() => Some(b.plan(name)),
            _ => None,
        }
    }

    /// The typed location; a relative one is taken beside the working tree.
    fn target(&self) -> Option<PathBuf> {
        let typed = self.path.trim();
        if typed.is_empty() {
            return None;
        }
        let path = expand_home(typed);
        if path.is_absolute() { Some(path) } else { self.repo.worktree.parent().map(|p| p.join(path)) }
    }

    /// The commit a new branch starts from, as shown to the user.
    fn base_label(&self) -> String {
        match (self.base.trim(), &self.head) {
            ("", Some(Head::Branch(b))) => b.clone(),
            ("", Some(Head::Detached(c))) => c.clone(),
            ("", None) => "HEAD".into(),
            (base, _) => base.to_owned(),
        }
    }

    fn can_submit(&self) -> bool {
        match self.plan() {
            _ if self.creating => false,
            None | Some(Plan::CheckedOut(_)) => false,
            Some(_) => self.target().is_some(),
        }
    }
}

fn msg(m: WorktreeMsg) -> Message {
    Message::Worktree(m)
}

/// Reads the repository's branches and worktrees on a thread, for the
/// Worktrees dialog of `tab`.
fn load_worktrees(tab: TabId, repo: Repo) -> Task<Message> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(repo.branches());
    });
    Task::perform(async move { rx.recv().await.unwrap_or_else(|e| Err(e.to_string())) }, move |r| {
        msg(WorktreeMsg::Loaded(tab, r))
    })
}

/// Width of a worktree's Remove button, kept free on the main one's row.
const REMOVE_WIDTH: f32 = 66.0;

fn small_button(label: &str) -> iced::widget::Button<'_, Message> {
    ui::small(label, theme::button_secondary)
}

/// A heading over a part of the dialog.
fn subheading(label: &str) -> text::Text<'_> {
    text(label).size(13).font(SEMIBOLD)
}

/// Small text about something that went wrong.
fn problem_text<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(12).style(|t: &iced::Theme| text::Style { color: Some(t.extended_palette().danger.base.color) })
}

/// A row of the worktree list: its branch and folder, why removing it
/// failed (if it did), then `actions`.
fn worktree_row<'a>(
    head: Option<&Head>,
    path: &Path,
    removal: Option<&Removal>,
    actions: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut about = column![].spacing(2).width(Length::Fill);
    if let Some(head) = head {
        about = about.push(branch_label(head, 13.0, 40));
    }
    about = about.push(muted_text(short_dir(path, 52)).size(12).wrapping(text::Wrapping::None));
    if let Some(r) = removal
        && let Some(e) = &r.failed
    {
        about = about.push(problem_text(if r.leftover {
            format!("Gone from git, but part of its folder is left: {e}")
        } else {
            format!("git did not remove it: {e}")
        }));
    }
    row(std::iter::once(about.into()).chain(actions)).spacing(8).align_y(Alignment::Center).padding([8, 12]).into()
}

impl App {
    pub(crate) fn update_worktree(&mut self, m: WorktreeMsg) -> Task<Message> {
        match m {
            WorktreeMsg::Open => return self.open_worktree_dialog(),
            WorktreeMsg::Loaded(tab, result) => {
                let Some(f) = self.worktrees_of_tab(tab) else { return Task::none() };
                let common_dir = f.repo.common_dir.clone();
                let listed: Option<Vec<PathBuf>> =
                    result.as_ref().ok().map(|b| b.worktrees.iter().map(|w| w.path.clone()).collect());
                f.branches = Some(result);
                if let Some(listed) = listed {
                    self.settle_removals(&common_dir, &listed);
                }
            }
            WorktreeMsg::Switch(path) => {
                let Some(f) = self.worktree_form().filter(|f| !f.creating) else { return Task::none() };
                let (tab, dir) = (f.tab, f.repo.same_place_in(&f.from, &path));
                return self.move_session(tab, dir);
            }
            WorktreeMsg::NewSession(path) => {
                let Some(f) = self.worktree_form().filter(|f| !f.creating) else { return Task::none() };
                let dir = f.repo.same_place_in(&f.from, &path);
                let title = f
                    .branches
                    .as_ref()
                    .and_then(|b| b.as_ref().ok())
                    .and_then(|b| b.worktrees.iter().find(|w| w.path == path))
                    .and_then(|w| match &w.head {
                        Some(Head::Branch(b)) => Some(b.clone()),
                        _ => None,
                    })
                    .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
                return self.open_session(title, dir);
            }
            WorktreeMsg::MoveSession(on) => {
                if let Some(f) = self.worktree_form() {
                    f.move_session = on;
                }
            }
            WorktreeMsg::Branch(v) => {
                if let Some(f) = self.worktree_form() {
                    if !f.path_edited {
                        let name = v.trim();
                        let suggested = (!name.is_empty()).then(|| f.repo.suggest_worktree_path(name)).flatten();
                        f.path = suggested.map(|p| p.display().to_string()).unwrap_or_default();
                    }
                    f.branch = v;
                    f.error = None;
                }
            }
            WorktreeMsg::Base(v) => {
                if let Some(f) = self.worktree_form() {
                    f.base = v;
                }
            }
            WorktreeMsg::Path(v) => {
                if let Some(f) = self.worktree_form() {
                    f.path_edited = !v.trim().is_empty();
                    f.path = v;
                    f.error = None;
                }
            }
            WorktreeMsg::Submit => return self.submit_worktree(),
            WorktreeMsg::Created { tab, branch, path, result } => {
                let Some(f) = self.worktree_form().filter(|f| f.tab == tab) else { return Task::none() };
                f.creating = false;
                match result {
                    Ok(()) => {
                        let dir = f.repo.same_place_in(&f.from, &path);
                        tracing::info!(%tab, path = %path.display(), "worktree created");
                        return if f.move_session {
                            self.move_session(tab, dir)
                        } else {
                            self.open_session(Some(branch), dir)
                        };
                    }
                    Err(e) => {
                        tracing::warn!(%tab, path = %path.display(), error = %e, "creating a worktree failed");
                        f.error = Some(e);
                    }
                }
            }
        }
        Task::none()
    }

    fn worktree_form(&mut self) -> Option<&mut WorktreeForm> {
        match &mut self.dialog {
            Some(Dialog::Worktree(f)) => Some(f),
            _ => None,
        }
    }

    /// The Worktrees dialog, shown or behind the question whether to remove one.
    fn worktrees_behind(&mut self) -> Option<&mut WorktreeForm> {
        match &mut self.dialog {
            Some(Dialog::Worktree(f)) => Some(f),
            Some(Dialog::RemoveWorktree(f)) => Some(&mut f.back),
            _ => None,
        }
    }

    fn worktrees_of_tab(&mut self, tab: TabId) -> Option<&mut WorktreeForm> {
        self.worktrees_behind().filter(|f| f.tab == tab)
    }

    /// The Worktrees dialog (shown or behind) when it lists the worktrees of
    /// the repository whose shared git directory is `common_dir`.
    pub(crate) fn worktrees_of(&mut self, common_dir: &Path) -> Option<&mut WorktreeForm> {
        self.worktrees_behind().filter(|f| git::same_path(&f.repo.common_dir, common_dir))
    }

    /// Reads the worktrees again for the Worktrees dialog, if it lists the
    /// repository's; the old list stays until then.
    pub(crate) fn reload_worktrees(&mut self, common_dir: &Path) -> Task<Message> {
        match self.worktrees_of(common_dir) {
            Some(f) => load_worktrees(f.tab, f.repo.clone()),
            None => Task::none(),
        }
    }

    fn open_worktree_dialog(&mut self) -> Task<Message> {
        let s = self.core.state();
        let Some(tab) = s.active_tab() else { return Task::none() };
        let from = s.tab_working_dir(tab);
        let Some(status) = git::Status::of(&from) else {
            self.error = Some(format!("{} is not in a git repository.", display_dir(&from)));
            return Task::none();
        };
        let tab = tab.id;
        let loaded = load_worktrees(tab, status.repo.clone());
        self.dialog = Some(Dialog::Worktree(Box::new(WorktreeForm {
            tab,
            from,
            repo: status.repo,
            head: status.head,
            branch: String::new(),
            base: String::new(),
            path: String::new(),
            path_edited: false,
            move_session: false,
            branches: None,
            creating: false,
            error: None,
        })));
        Task::batch([loaded, operation::focus(DIALOG_FIRST_INPUT)])
    }

    fn submit_worktree(&mut self) -> Task<Message> {
        let Some(f) = self.worktree_form() else { return Task::none() };
        if !f.can_submit() {
            return Task::none();
        }
        let tab = f.tab;
        let branch = f.branch.trim().to_owned();
        let plan = f.plan().expect("checked by can_submit");
        let path = f.target().expect("checked by can_submit");
        let base = (plan == Plan::NewBranch).then(|| f.base.trim().to_owned()).filter(|b| !b.is_empty());
        f.creating = true;
        f.error = None;
        let repo = f.repo.clone();
        let (tx, rx) = async_channel::bounded(1);
        let (target, name) = (path.clone(), branch.clone());
        std::thread::spawn(move || {
            let _ = tx.send_blocking(repo.add_worktree(&target, &name, &plan, base.as_deref()));
        });
        Task::perform(async move { rx.recv().await.unwrap_or_else(|e| Err(e.to_string())) }, move |result| {
            msg(WorktreeMsg::Created { tab, branch: branch.clone(), path: path.clone(), result })
        })
    }

    /// Opens a new session of this project in `dir`, closing the dialog.
    fn open_session(&mut self, title: Option<String>, dir: PathBuf) -> Task<Message> {
        let Some(Dialog::Worktree(f)) = self.dialog.take() else { return Task::none() };
        let Some(project) = self.core.state().tab(f.tab).map(|t| t.project) else { return Task::none() };
        self.dispatch(Command::CreateTab { project, title, working_dir: Some(dir) });
        self.after_select()
    }

    /// Moves the session to `dir`, closes the dialog and says which terminals
    /// stayed behind.
    fn move_session(&mut self, tab: TabId, dir: PathBuf) -> Task<Message> {
        self.dialog = None;
        if let Some(Outcome::Relocated { left_running }) =
            self.dispatch(Command::RelocateTab { tab, working_dir: dir.clone() })
        {
            self.notice = match left_running {
                0 => None,
                1 => Some(format!(
                    "Moved to {}. One terminal is still running a program, so it stays in the old directory; restart \
                     it to move it.",
                    display_dir(&dir)
                )),
                n => Some(format!(
                    "Moved to {}. {n} terminals are still running programs, so they stay in the old directory; \
                     restart them to move them.",
                    display_dir(&dir)
                )),
            };
        }
        Task::batch([self.read_git(), self.focus_active()])
    }

    /// The repository's worktrees, each with what can be done with it. One
    /// being removed says so; one git failed to remove says why.
    fn view_worktree_list<'a>(&'a self, f: &'a WorktreeForm) -> Element<'a, Message> {
        let b = match &f.branches {
            None => return muted_text("Looking up worktrees…").size(12).into(),
            Some(Err(e)) => return ui::note_text(Tone::Danger, e),
            Some(Ok(b)) => b,
        };
        // git lists the main working tree first.
        let main = b.worktrees.first().map(|w| &w.path);
        let listed: Vec<&Worktree> = b.worktrees.iter().filter(|w| w.usable()).collect();
        // Removals git no longer lists (being deleted, or left over) stay until done.
        let unlisted = self.removals.iter().filter(|r| {
            git::same_path(&r.common_dir, &f.repo.common_dir)
                && !listed.iter().any(|w| git::same_path(&w.path, &r.path))
        });
        let idle = !f.creating;
        let remove = |path: &Path| {
            ui::small("Remove", theme::button_link(Tone::Danger))
                .width(REMOVE_WIDTH)
                .on_press_maybe(idle.then(|| Message::RemoveWorktree(RemoveMsg::Ask(path.to_path_buf()))))
                .into()
        };
        let removing = || muted_text("Removing…").size(12).into();
        let rows = listed
            .iter()
            .map(|w| {
                let removal = self.removal(&w.path);
                let actions = if removal.is_some_and(|r| r.failed.is_none()) {
                    vec![removing()]
                } else {
                    let switch: Element<'_, Message> = if git::same_path(&w.path, &f.repo.worktree) {
                        muted_text("this session").size(12).into()
                    } else {
                        small_button("Switch")
                            .on_press_maybe(idle.then(|| msg(WorktreeMsg::Switch(w.path.clone()))))
                            .into()
                    };
                    let open = small_button("New session")
                        .on_press_maybe(idle.then(|| msg(WorktreeMsg::NewSession(w.path.clone()))));
                    // The main working tree holds the repository; git won't remove it.
                    let remove = if main == Some(&w.path) {
                        space::horizontal().width(REMOVE_WIDTH).into()
                    } else {
                        remove(&w.path)
                    };
                    vec![switch, open.into(), remove]
                };
                worktree_row(w.head.as_ref(), &w.path, removal, actions)
            })
            .chain(unlisted.map(|r| {
                let actions = if r.failed.is_none() { vec![removing()] } else { vec![remove(&r.path)] };
                worktree_row(r.head.as_ref(), &r.path, Some(r), actions)
            }));
        // A hairline between each worktree and the next.
        let mut list = column![];
        for (i, r) in rows.enumerate() {
            if i > 0 {
                list = list.push(ui::divider());
            }
            list = list.push(r);
        }
        container(ui::scroll(list).height(Length::Shrink)).max_height(240).style(theme::list_box).into()
    }

    pub(crate) fn view_worktree<'a>(&'a self, f: &'a WorktreeForm) -> DialogParts<'a> {
        let s = self.core.state();
        let session = s.tab(f.tab).map(|t| t.title.as_str()).unwrap_or("This session");
        let on = f.head.as_ref().map(|h| h.to_string()).unwrap_or_else(|| "an unknown commit".into());
        let name = f.branch.trim();
        let plan = f.plan();

        let (hint, problem) = match (&f.branches, &plan) {
            (None, _) => ("Looking up branches…".to_owned(), false),
            (Some(Err(_)), _) => ("git could not list the branches.".to_owned(), true),
            (_, None) => ("A new branch, or one that already exists.".to_owned(), false),
            (_, Some(Plan::NewBranch)) => (format!("Creates the branch {name} from {}.", f.base_label()), false),
            (_, Some(Plan::Existing)) => (format!("Checks out the existing branch {name}."), false),
            (_, Some(Plan::Track(remote))) => (format!("Creates {name} tracking {remote}."), false),
            (Some(Ok(b)), Some(Plan::CheckedOut(dir))) if b.worktrees.iter().any(|w| w.path == *dir && w.prunable) => (
                format!("{name} is checked out in {}, which is gone. Run git worktree prune first.", display_dir(dir)),
                true,
            ),
            (_, Some(Plan::CheckedOut(_))) => (format!("{name} has a worktree already; switch to it above."), true),
        };
        let hint: Element<'_, Message> =
            if problem { problem_text(hint).into() } else { muted_text(hint).size(12).into() };

        let mut col = column![
            text(format!("{session} is on {on} in {}.", display_dir(&f.repo.worktree))).size(13),
            column![subheading("Existing worktrees"), self.view_worktree_list(f)].spacing(8),
            subheading("New worktree"),
            column![
                field(
                    "Branch",
                    ui::input("feature/my-change", &f.branch)
                        .id(DIALOG_FIRST_INPUT)
                        .on_input(|v| msg(WorktreeMsg::Branch(v)))
                        .on_submit(msg(WorktreeMsg::Submit)),
                ),
                hint,
            ]
            .spacing(6),
        ]
        .spacing(16);

        if plan == Some(Plan::NewBranch) {
            col = col.push(field(
                "Start from",
                ui::input(&f.base_label(), &f.base)
                    .on_input(|v| msg(WorktreeMsg::Base(v)))
                    .on_submit(msg(WorktreeMsg::Submit)),
            ));
        }
        if !matches!(plan, Some(Plan::CheckedOut(_))) {
            let beside = f.repo.worktree.parent().map(display_dir).unwrap_or_default();
            col = col.push(
                column![
                    field(
                        "Location",
                        ui::input(&format!("{beside}/…"), &f.path)
                            .on_input(|v| msg(WorktreeMsg::Path(v)))
                            .on_submit(msg(WorktreeMsg::Submit)),
                    ),
                    muted_text("Beside the repository, so on the same drive. A relative path is taken from there.")
                        .size(12),
                ]
                .spacing(6),
            );
        }
        if !matches!(plan, Some(Plan::CheckedOut(_))) {
            col = col.push(
                column![
                    ui::check(f.move_session)
                        .label(format!("Move “{session}” into it"))
                        .on_toggle(|on| msg(WorktreeMsg::MoveSession(on))),
                    container(muted_text("Otherwise it opens in a new session named after the branch.").size(12))
                        .padding(iced::Padding { left: 23.0, ..iced::Padding::ZERO }),
                ]
                .spacing(4),
            );
        }
        col = col.push(
            muted_text(
                "Moving a session restarts its idle terminals there (keeping their scrollback); terminals running a \
                 program or an agent are left as they are.",
            )
            .size(12),
        );
        if let Some(e) = &f.error {
            col = col.push(ui::note_text(Tone::Danger, e));
        }

        let confirm = if f.creating { "Creating…" } else { "Create worktree" };
        let cancel = ui::secondary("Cancel");
        let mut parts = DialogParts::new("Worktrees", col)
            .button(if f.creating { cancel } else { cancel.on_press(Message::DialogCancel) })
            .button(ui::primary(confirm).on_press_maybe(f.can_submit().then(|| msg(WorktreeMsg::Submit))));
        parts.closable = !f.creating;
        parts
    }
}
