//! Modal dialogs: a title with a ✕, the content, and the buttons along the bottom.

use std::sync::Arc;

use iced::widget::{Column, button, center, column, container, mouse_area, opaque, row, space, text};
use iced::{Alignment, Element, Length, Padding};
use termroost_core::AgentKind;
use termroost_core::session::shell;

use crate::app::{App, DIALOG_FIRST_INPUT, Dialog, Message};
use crate::editor::EditorMsg;
use crate::icons::{Icon, icon};
use crate::keymap::Keymap;
use crate::sidebar::muted_text;
use crate::terminal_view::{KeyboardFocus, TerminalView};
use crate::theme::{self, SEMIBOLD, TEXT, TEXT_TITLE, Tone};
use crate::ui::{self, field};

/// What a dialog shows.
pub(crate) struct DialogParts<'a> {
    pub title: String,
    pub body: Element<'a, Message>,
    /// Along the bottom, right-aligned; the main action last.
    pub buttons: Vec<Element<'a, Message>>,
    /// Whether the ✕ can be pressed; not while git is busy, for example.
    pub closable: bool,
    /// What the ✕ (and a click outside, where allowed) does.
    pub cancel: Message,
}

impl<'a> DialogParts<'a> {
    pub fn new(title: impl Into<String>, body: impl Into<Element<'a, Message>>) -> Self {
        DialogParts {
            title: title.into(),
            body: body.into(),
            buttons: Vec::new(),
            closable: true,
            cancel: Message::DialogCancel,
        }
    }

    /// Makes the ✕ send `cancel` instead of closing `App::dialog`.
    pub fn on_cancel(mut self, cancel: Message) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn button(mut self, button: impl Into<Element<'a, Message>>) -> Self {
        self.buttons.push(button.into());
        self
    }

    /// Cancel, then `confirm` (in the danger color if it deletes or ends something).
    fn confirm(self, confirm: &'a str, danger: bool) -> Self {
        let confirm = if danger { ui::danger(confirm) } else { ui::primary(confirm) };
        self.button(ui::secondary("Cancel").on_press(Message::DialogCancel))
            .button(confirm.on_press(Message::DialogSubmit))
    }
}

/// Said when deleting a session or project closes files with unsaved changes.
fn unsaved_note(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => " A file with unsaved changes is closed without saving.".into(),
        n => format!(" {n} files with unsaved changes are closed without saving."),
    }
}

/// Body text of a dialog.
/// The terminal settings' switch for keeping a terminal running when
/// Terminal Rooster quits. `was`: the setting as saved.
fn keep_running<'a>(keep: bool, was: bool) -> Element<'a, Message> {
    let switch = row![
        para("Keep running when Terminal Rooster quits").width(Length::Fill),
        ui::switch(keep).on_toggle(Message::DialogKeepRunning)
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    if keep == was {
        return switch.into();
    }
    column![switch, ui::note(Tone::Warning, para("Saving restarts this terminal."))].spacing(8).into()
}

pub(crate) fn para<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(TEXT)
}

/// A list with a dot before each item.
fn bullets<'a>(items: impl Iterator<Item = Element<'a, Message>>) -> Element<'a, Message> {
    let dot = || {
        container(text("•").size(TEXT).style(ui::muted))
            .height(text::LineHeight::default().to_absolute(TEXT.into()))
            .center_y(Length::Shrink)
    };
    Column::with_children(items.map(|item| row![dot(), item].spacing(8).into())).spacing(6).into()
}

/// A dialog's card: the title and ✕, the content, then the buttons on a
/// tinted strip. `dismiss_outside` lets a click outside it cancel it.
pub(crate) fn modal<'a>(parts: DialogParts<'a>, width: f32, dismiss_outside: bool) -> Element<'a, Message> {
    let close = button(icon(Icon::Close, 14.0, theme::muted))
        .padding(5)
        .style(theme::button_ghost)
        .on_press_maybe(parts.closable.then(|| parts.cancel.clone()));
    let header = row![text(parts.title).size(TEXT_TITLE).font(SEMIBOLD).width(Length::Fill), close]
        .spacing(12)
        .align_y(Alignment::Center)
        .padding(Padding { top: 14.0, right: 14.0, bottom: 14.0, left: 22.0 });
    let body = container(parts.body).padding([18, 22]).width(Length::Fill);
    let mut card = column![header, ui::divider(), body];
    if !parts.buttons.is_empty() {
        let buttons =
            row(std::iter::once(space::horizontal().into()).chain(parts.buttons)).spacing(8).align_y(Alignment::Center);
        card = card
            .push(ui::divider())
            .push(container(buttons).padding([12, 22]).width(Length::Fill).style(theme::modal_footer));
    }
    let card = opaque(container(card).width(width).style(theme::modal_card));
    let backdrop = center(card).padding(24).style(theme::modal_backdrop);
    if dismiss_outside { opaque(mouse_area(backdrop).on_press(parts.cancel)) } else { opaque(backdrop) }
}

/// The small Codex window for trusting the hook. Clicking outside does not
/// close it (it would end Codex); Done does.
fn codex_window<'a>(
    handle: &'a termroost_core::TerminalHandle,
    keymap: &Arc<Keymap>,
    focus: &KeyboardFocus,
    font_size: f32,
    active: bool,
) -> Element<'a, Message> {
    let term = TerminalView::new(
        handle.clone(),
        keymap.clone(),
        focus.clone(),
        active,
        font_size,
        Message::Noop,
        Message::Noop,
    );
    let body = column![
        para("Type /hooks and trust the Terminal Rooster hook."),
        container(term).padding(1).height(420).style(theme::terminal_frame),
    ]
    .spacing(12);
    let parts =
        DialogParts::new("Trust the Codex hook", body).button(ui::primary("Done").on_press(Message::DialogCancel));
    modal(parts, 820.0, false)
}

impl App {
    pub(crate) fn view_dialog<'a>(&'a self, dialog: &'a Dialog) -> Element<'a, Message> {
        let s = self.core.state();
        let platform_shell = {
            let sh = shell::default_shell();
            std::iter::once(sh.program).chain(sh.args).collect::<Vec<_>>().join(" ")
        };
        let parts: DialogParts<'_> = match dialog {
            Dialog::Project { editing, name, dir, shell, error } => {
                let default_shell = s.settings.default_shell.clone().unwrap_or_else(|| platform_shell.clone());
                let mut col = column![
                    field(
                        "Name",
                        ui::input("my-service", name)
                            .id(DIALOG_FIRST_INPUT)
                            .on_input(Message::DialogName)
                            .on_submit(Message::DialogSubmit),
                    ),
                    field(
                        "Working directory",
                        row![
                            ui::input("/path/to/project", dir)
                                .on_input(Message::DialogDir)
                                .on_submit(Message::DialogSubmit),
                            ui::secondary("Browse…").on_press(Message::BrowseDir),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    ),
                    field(
                        "Shell (optional)",
                        row![
                            ui::input(&format!("default: {default_shell}"), shell)
                                .on_input(Message::DialogShell)
                                .on_submit(Message::DialogSubmit),
                            // Empty means "the default", so nothing is shown as picked.
                            self.shell_picker(shell, "", |o| Message::DialogShell(o.command)),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    ),
                ]
                .spacing(16);
                if let Some(e) = error {
                    col = col.push(ui::note_text(Tone::Danger, e));
                }
                let title = if editing.is_some() { "Edit project" } else { "New project" };
                DialogParts::new(title, col).confirm(if editing.is_some() { "Save" } else { "Create" }, false)
            }
            Dialog::ConfirmDeleteProject(p) => {
                let name = s.project(*p).map(|p| p.name.as_str()).unwrap_or("project");
                let tabs = s.tabs_of(*p).count();
                let terms: usize = s.tabs_of(*p).map(|t| t.layout.panes().len()).sum();
                let tasks = s.tasks_of(*p).count();
                let unsaved: usize = s.tabs_of(*p).map(|t| self.unsaved_in(t.id)).sum();
                let body = para(format!(
                    "This closes {terms} terminal(s) in {tabs} session(s) and deletes {tasks} task(s) and all saved \
                     scrollback. The directory on disk is not touched. This cannot be undone.{}",
                    unsaved_note(unsaved)
                ));
                DialogParts::new(format!("Delete project “{name}”?"), body).confirm("Delete project", true)
            }
            Dialog::ConfirmDeleteTab(t) => {
                let tab = s.tab(*t);
                let name = tab.map(|t| t.title.as_str()).unwrap_or("session");
                let terms = tab.map(|t| t.layout.panes().len()).unwrap_or(0);
                let linked = s.tasks.iter().filter(|x| x.tab == Some(*t)).count();
                let body = para(format!(
                    "This closes {terms} terminal(s) and their saved scrollback. {linked} linked task(s) are kept and \
                     unlinked.{}",
                    unsaved_note(self.unsaved_in(*t))
                ));
                DialogParts::new(format!("Delete session “{name}”?"), body).confirm("Delete session", true)
            }
            Dialog::ConfirmQuit { running, unsaved, dont_ask } => {
                let mut col = column![].spacing(14);
                let removing = self.removing_worktrees();
                if running.is_empty() && unsaved.is_empty() && removing.is_empty() {
                    // It was asked only for worktrees, and git is done with them.
                    col = col.push(para("Nothing is being removed any more."));
                }
                if !unsaved.is_empty() {
                    let n = unsaved.len();
                    let list = bullets(unsaved.iter().map(|f| para(f).into()));
                    col = col
                        .push(para(if n == 1 {
                            "A file has unsaved changes. Quitting loses them.".to_owned()
                        } else {
                            format!("{n} files have unsaved changes. Quitting loses them.")
                        }))
                        .push(container(ui::scroll(list)).max_height(200));
                }
                if !removing.is_empty() {
                    let n = removing.len();
                    let list = bullets(removing.into_iter().map(|d| para(d).into()));
                    col = col
                        .push(para(if n == 1 {
                            "A worktree is still being removed. Quitting now can leave it, or part of its folder."
                                .to_owned()
                        } else {
                            format!(
                                "{n} worktrees are still being removed. Quitting now can leave them, or parts of \
                                 their folders."
                            )
                        }))
                        .push(container(ui::scroll(list)).max_height(120));
                }
                if !running.is_empty() {
                    let n = running.len();
                    let list = bullets(running.iter().map(|r| {
                        column![para(&r.programs), muted_text(&r.place)].spacing(1).width(Length::Fill).into()
                    }));
                    col = col
                        .push(para(if n == 1 {
                            "A terminal is still running a program. Quitting stops it.".to_owned()
                        } else {
                            format!("{n} terminals are still running programs. Quitting stops them.")
                        }))
                        .push(container(ui::scroll(list)).max_height(320));
                    if running.iter().any(|r| r.agent) {
                        col = col.push(ui::note_text(
                            Tone::Neutral,
                            "Coding agents come back in the same conversation when Terminal Rooster starts again, but \
                             a reply they are writing now is cut off.",
                        ));
                    }
                    // Unsaved files are asked about either way.
                    col = col.push(
                        ui::check(*dont_ask)
                            .label("Don't ask again about running programs (Settings → Notifications)")
                            .on_toggle(Message::DialogDontAsk)
                            .text_size(12),
                    );
                }
                DialogParts::new("Quit Terminal Rooster?", col).confirm("Quit", true)
            }
            Dialog::SaveConflict { doc, name } => DialogParts::new(
                format!("{name} changed on disk"),
                para(
                    "It was changed outside Terminal Rooster since you opened or saved it, maybe by an agent. \
                     Overwrite replaces that version with yours; Reload shows it and drops your changes (undo \
                     brings them back).",
                ),
            )
            .button(ui::secondary("Cancel").on_press(Message::DialogCancel))
            .button(ui::secondary("Reload").on_press(Message::Editor(EditorMsg::Reload(*doc))))
            .button(ui::danger("Overwrite").on_press(Message::DialogSubmit)),
            Dialog::CloseFiles { docs, names } => {
                let (title, body, save): (_, Element<'_, Message>, _) = match names.as_slice() {
                    [name] => (
                        format!("Save the changes to {name}?"),
                        para("If you don't save them, they are lost.").into(),
                        "Save",
                    ),
                    _ => (
                        format!("Save the changes to {} files?", names.len()),
                        column![
                            container(ui::scroll(bullets(names.iter().map(|n| para(n).into())))).max_height(200),
                            para("If you don't save them, they are lost."),
                        ]
                        .spacing(12)
                        .into(),
                        "Save all",
                    ),
                };
                DialogParts::new(title, body)
                    .button(ui::secondary("Cancel").on_press(Message::DialogCancel))
                    .button(ui::danger("Don't save").on_press(Message::Editor(EditorMsg::Discard(docs.clone()))))
                    .button(ui::primary(save).on_press(Message::DialogSubmit))
            }
            Dialog::CloseAllNotes => {
                let n = s.notes.len();
                let title = if n == 1 { "Close the note?".to_owned() } else { format!("Close all {n} notes?") };
                DialogParts::new(title, para("Their text is gone.")).confirm("Close all", true)
            }
            Dialog::CodexTrust(handle) => {
                return codex_window(
                    handle,
                    &self.keymap,
                    &self.keyboard_focus,
                    self.prefs.font_size,
                    !self.asks.open(),
                );
            }
            Dialog::Worktree(form) => self.view_worktree(form),
            Dialog::RemoveWorktree(form) => self.view_remove(form),
            Dialog::Remotes(form) => self.view_remotes(form),
            Dialog::CodexHook => DialogParts::new(
                "Track Codex sessions?",
                column![
                    para(
                        "Add a small hook to Codex so Terminal Rooster always resumes the right conversation in each \
                         terminal."
                    ),
                    muted_text("You can change this later in Settings.").size(12),
                ]
                .spacing(8),
            )
            .button(ui::secondary("Not now").on_press(Message::DialogCancel))
            .button(ui::primary("Add hook").on_press(Message::DialogSubmit)),
            Dialog::Renamed(renamed) => {
                let mut body = column![].spacing(10);
                if let Some(from) = &renamed.moved_from {
                    body = body.push(para(format!(
                        "Your projects, sessions, tasks, scrollback and settings were moved from {} to {}.",
                        from.display(),
                        self.core.data_dir().display()
                    )));
                }
                if renamed.codex_hook_updated {
                    body = body
                        .push(para(
                            "The Codex hook now reports to the new folder. Codex only runs hooks you trust, so \
                             trust it again: type /hooks in the Codex window that opens.",
                        ))
                        .push(muted_text("You can also do this later in Settings → Integrations.").size(12));
                    DialogParts::new("TermRoost is now Terminal Rooster", body)
                        .button(ui::secondary("Later").on_press(Message::DialogCancel))
                        .button(ui::primary("Trust in Codex…").on_press(Message::DialogSubmit))
                } else {
                    DialogParts::new("TermRoost is now Terminal Rooster", body)
                        .button(ui::primary("OK").on_press(Message::DialogSubmit))
                }
            }
            Dialog::Pane { pane, resume, args, keep } => {
                let agent = s.pane(*pane).and_then(|p| p.agent.as_ref());
                let status: Element<'_, Message> = match agent {
                    Some(a) => {
                        let session = a.session_id.clone().unwrap_or_else(|| "(no conversation yet)".into());
                        let resume_line =
                            a.resume_command().unwrap_or_else(|| format!("{} (no conversation yet)", a.kind.as_str()));
                        ui::note(
                            Tone::Neutral,
                            row![
                                column![
                                    para(format!("{} is running here · session {session}", a.kind.label())),
                                    muted_text(format!("On restore: {resume_line}")),
                                ]
                                .spacing(2)
                                .width(Length::Fill),
                                ui::small("Don't resume it", theme::button_secondary)
                                    .on_press(Message::DialogClearAgent),
                            ]
                            .spacing(12)
                            .align_y(Alignment::Center),
                        )
                    }
                    None => muted_text("No coding agent is running in this terminal.").size(12).into(),
                };
                let start = row(AgentKind::ALL.iter().map(|k| {
                    ui::small(format!("Start {}", k.label()), theme::button_secondary)
                        .on_press(Message::DialogStartAgent(*k))
                        .into()
                }))
                .spacing(6)
                .wrap()
                .vertical_spacing(6);
                let body = column![
                    status,
                    field(
                        "Agent arguments",
                        ui::input("e.g. --model opus", args).id(DIALOG_FIRST_INPUT).on_input(Message::DialogArgs),
                    ),
                    start,
                    muted_text(
                        "Claude Code, Codex, Grok and Antigravity are detected automatically, also when you start \
                         them yourself, and Terminal Rooster follows the session they are in (including /clear). \
                         After a restart each terminal resumes its own conversation, not just the most recent one.",
                    )
                    .size(12),
                    field(
                        "Resume command (used when no agent is running)",
                        ui::input("e.g. npm run dev", resume)
                            .on_input(Message::DialogResume)
                            .on_submit(Message::DialogSubmit),
                    ),
                    keep_running(*keep, s.pane(*pane).is_some_and(|p| p.keep_running)),
                ]
                .spacing(14);
                DialogParts::new("Terminal settings", body).confirm("Save", false)
            }
        };
        // The worktree and remote lists need room for a path or URL and its buttons.
        let width = if matches!(dialog, Dialog::Worktree(_) | Dialog::Remotes(_)) { 620.0 } else { 500.0 };
        modal(parts, width, true)
    }
}
