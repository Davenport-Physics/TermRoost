//! How the uncommitted changes look: a section for each kind of change,
//! its title folding it, with a button to stage (or unstage) all of it,
//! then its files, each with a button for the whole file and one for each
//! hunk. Lines chosen in a hunk are marked, and its button then stages just
//! those. Above it all (not scrolled away) is the message of the next
//! commit with its button, and one that folds or unfolds every file.

use iced::keyboard::Key;
use iced::keyboard::key::Named;
use iced::widget::text_editor::{Binding, KeyPress, Status};
use iced::widget::{Row, button, center, column, container, row, space, text, text_editor, tooltip};
use iced::{Alignment, Element, Length, Padding};
use termroost_core::git::{FileDiff, FileStatus, WorkingChanges};

use super::changes::{ChangesDoc, Pick, Section};
use super::view::{file_body_with, file_digits, file_header};
use super::{CHANGES_SCROLL, GitViewMsg};
use crate::app::{App, Message};
use crate::diff_lines::DiffLines;
use crate::editor::DocId;
use crate::files::FilesMsg;
use crate::icons::{Icon, icon};
use crate::keymap::Keymap;
use crate::sidebar::muted_text;
use crate::theme::{self, MONO, SEMIBOLD, Tone};
use crate::ui::{self, icon_button, tip};

/// Files shown per section; the rest are counted.
const MAX_FILES: usize = 200;

/// What the buttons of a section do to its files.
fn verb(section: Section) -> &'static str {
    match section {
        Section::Conflicts => "Mark resolved",
        Section::Staged => "Unstage",
        Section::Unstaged | Section::Untracked => "Stage",
    }
}

impl App {
    /// The uncommitted changes, in place of the terminals.
    pub(crate) fn view_changes<'a>(&'a self, doc: DocId, c: &'a ChangesDoc) -> Element<'a, Message> {
        let composer = self.composer(doc, c);
        // Under the message, so it doesn't move the box (which would lose the
        // keyboard); always there, so the changes keep their place (and scroll).
        let note: Element<'a, Message> = match (&c.error, &c.done) {
            (Some(message), _) | (None, Some(message)) => {
                let tone = if c.error.is_some() { Tone::Danger } else { Tone::Success };
                let dismiss = Message::GitView(GitViewMsg::DismissNote(doc));
                let line = row![
                    text(message).size(theme::TEXT).width(Length::Fill),
                    icon_button(Icon::Close, dismiss, "Dismiss")
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                container(ui::note(tone, line))
                    .padding(Padding { top: 0.0, right: 14.0, bottom: 10.0, left: 4.0 })
                    .into()
            }
            (None, None) => space::vertical().height(0).into(),
        };
        let body: Element<'a, Message> = match &c.changes {
            None => center(muted_text("Reading the changes…").size(theme::TEXT)).into(),
            Some(Err(e)) => container(ui::note_text(Tone::Danger, e)).padding(12).into(),
            Some(Ok(changes)) => self.view_sections(doc, c, changes),
        };
        column![composer, note, body].into()
    }

    /// The message of the next commit, whether it amends the last one, and
    /// the button that commits.
    fn composer<'a>(&'a self, doc: DocId, c: &'a ChangesDoc) -> Element<'a, Message> {
        let staged = match &c.changes {
            Some(Ok(changes)) => Some(changes.staged.len()),
            _ => None,
        };
        let unborn = matches!(&c.changes, Some(Ok(changes)) if changes.unborn);
        let keymap = self.keymap.clone();
        let mut message = text_editor(&c.message)
            .placeholder(if c.amend { "Message of the amended commit" } else { "Commit message" })
            .font(MONO)
            .size(theme::TEXT)
            .padding([8, 10])
            .height(96)
            .wrapping(text::Wrapping::WordOrGlyph)
            .key_binding(move |press| message_keys(&keymap, doc, press))
            .style(theme::text_box);
        if !c.committing {
            message = message.on_action(move |a| Message::GitView(GitViewMsg::MessageEdit(doc, a)));
        }
        let amend = ui::check(c.amend)
            .label("Amend last commit")
            .text_size(theme::TEXT_SM)
            .on_toggle_maybe((!c.busy && !unborn).then_some(move |on| Message::GitView(GitViewMsg::Amend(doc, on))));
        let count = match staged {
            None => String::new(),
            Some(0) if c.amend => "Nothing staged: only the message changes".to_owned(),
            Some(0) => "Nothing staged yet".to_owned(),
            Some(1) => "1 file staged".to_owned(),
            Some(n) => format!("{n} files staged"),
        };
        let label = match (c.committing, c.amend) {
            (true, _) => "Committing…",
            (false, true) => "Amend",
            (false, false) => "Commit",
        };
        let commit =
            ui::primary(label).on_press_maybe(c.can_commit().then_some(Message::GitView(GitViewMsg::Commit(doc))));
        let commit =
            tip(commit, format!("{label} what is staged\nCtrl+Enter in the message"), tooltip::Position::Bottom);
        let footer = row![amend, space::horizontal(), muted_text(count).size(theme::TEXT_SM), commit]
            .spacing(12)
            .align_y(Alignment::Center);
        container(column![message, footer].spacing(10))
            .padding(Padding { top: 6.0, right: 14.0, bottom: 12.0, left: 4.0 })
            .into()
    }

    /// The sections of the changes and their files, scrolled, under a bar
    /// that folds or unfolds every file.
    fn view_sections<'a>(&'a self, doc: DocId, c: &'a ChangesDoc, changes: &'a WorkingChanges) -> Element<'a, Message> {
        let mut page = column![].spacing(12);
        let mut empty = true;
        for section in Section::ALL {
            let files: Vec<&FileDiff> = section.files(changes).collect();
            if files.is_empty() {
                continue;
            }
            empty = false;
            let folded = c.section_folded(section);
            page = page.push(section_header(doc, section, files.len(), folded, c.busy));
            if folded {
                continue;
            }
            for file in files.iter().take(MAX_FILES) {
                page = page.push(self.changed_file(doc, c, section, file, files.len()));
            }
            if files.len() > MAX_FILES {
                let more = format!("{} more files are not shown.", files.len() - MAX_FILES);
                page = page.push(container(muted_text(more).size(theme::TEXT_SM)).padding([0, 4]));
            }
        }
        if empty {
            let what = if changes.unborn {
                "Nothing to commit yet: add some files to the folder."
            } else {
                "Nothing to commit: the files are as the last commit left them."
            };
            page = page.push(container(text(what).size(theme::TEXT).style(ui::muted)).padding([8, 4]));
        }
        let list = ui::scroll(container(page).padding(Padding { top: 0.0, right: 14.0, bottom: 12.0, left: 4.0 }))
            .id(CHANGES_SCROLL)
            .height(Length::Fill);
        // Always there, so the list keeps its place (and scroll) in the tree.
        let bar: Element<'a, Message> =
            if empty { space::vertical().height(0).into() } else { fold_bar(doc, c, changes) };
        column![bar, list].into()
    }

    /// A file of a section: its header with a button for the whole file,
    /// and unless folded its hunks, each with a button of its own (when
    /// hunks can be staged one by one).
    fn changed_file<'a>(
        &'a self,
        doc: DocId,
        c: &'a ChangesDoc,
        section: Section,
        file: &'a FileDiff,
        in_section: usize,
    ) -> Element<'a, Message> {
        let busy = c.busy;
        let pick = |p: Pick| (!busy).then_some(Message::GitView(GitViewMsg::Move(doc, p)));
        let mut actions = Row::new();
        if file.status != FileStatus::Deleted {
            let path = c.repo.worktree.join(&file.path);
            actions = actions.push(icon_button(Icon::Open, Message::Files(FilesMsg::Open(path)), "Open the file"));
        }
        let whole = ui::small(verb(section), theme::button_secondary)
            .padding([2, 10])
            .on_press_maybe(pick(Pick::File(section, file.path.clone())));
        actions = actions.push(whole);
        let toggle = Message::GitView(GitViewMsg::ToggleChange(doc, section, file.path.clone()));
        let folded = c.folded(section, file, in_section);
        let mut block = column![file_header(file, folded, toggle, actions)];
        if !folded {
            let digits = file_digits(file);
            let font_size = self.prefs.font_size;
            let splittable = file.splittable();
            let chosen = |i: usize| c.chosen.as_ref().filter(|ch| ch.is(section, &file.path, i)).map(|ch| &ch.lines);
            let lines = |i: usize, hunk: &'a termroost_core::git::Hunk| {
                let lines = DiffLines::new(&hunk.lines, digits, font_size);
                if !splittable {
                    return lines;
                }
                let path = file.path.clone();
                let press = move |line, mods| {
                    Message::GitView(GitViewMsg::ChooseLine(doc, section, path.clone(), i, line, mods))
                };
                let drag = move |line| Message::GitView(GitViewMsg::DragLine(doc, line));
                lines.chosen(chosen(i)).on_choose(press, drag)
            };
            let actions = |i: usize| {
                if !splittable {
                    return Row::new();
                }
                let Some(lines) = chosen(i) else {
                    let press = pick(Pick::Hunk(section, file.path.clone(), i));
                    let hint = format!(
                        "Or press lines to {} only those: drag over them, or hold Shift for a range and Ctrl for \
                         one more",
                        verb(section).to_lowercase()
                    );
                    return row![tip(
                        hunk_button(format!("{} hunk", verb(section)), press),
                        hint,
                        tooltip::Position::Top
                    )];
                };
                let n = lines.len();
                let label = format!("{} {n} line{}", verb(section), if n == 1 { "" } else { "s" });
                let press = pick(Pick::Lines(section, file.path.clone(), i, lines.clone()));
                let clear = Message::GitView(GitViewMsg::ClearChosen(doc));
                row![hunk_button(label, press), icon_button(Icon::Close, clear, "Choose no lines")]
            };
            block = block.push(file_body_with(file, lines, actions));
        }
        container(block).width(Length::Fill).style(theme::diff_file).into()
    }
}

/// Keys in the commit message: Ctrl+Enter commits, the app's shortcuts
/// reach the app, and the rest type.
fn message_keys(keymap: &Keymap, doc: DocId, press: KeyPress) -> Option<Binding<Message>> {
    // iced's own bindings check this; a replacement has to as well.
    if !matches!(press.status, Status::Focused { .. }) {
        return None;
    }
    if press.modifiers.command() && matches!(press.key.as_ref(), Key::Named(Named::Enter)) {
        return Some(Binding::Custom(Message::GitView(GitViewMsg::Commit(doc))));
    }
    if keymap.action(&press.key, press.physical_key, press.modifiers).is_some_and(|a| !a.in_terminal()) {
        return None;
    }
    Binding::from_key_press(press)
}

/// A small button on a hunk's header line.
fn hunk_button<'a>(label: String, press: Option<Message>) -> Element<'a, Message> {
    button(text(label).size(11).wrapping(text::Wrapping::None))
        .padding([1, 8])
        .style(theme::button_secondary)
        .on_press_maybe(press)
        .into()
}

/// Over the list, not scrolled away: Collapse all while a file shows its
/// changes, else Expand all.
fn fold_bar<'a>(doc: DocId, c: &ChangesDoc, changes: &WorkingChanges) -> Element<'a, Message> {
    let unfolded = c.any_unfolded(changes, MAX_FILES);
    let label = if unfolded { "Collapse all" } else { "Expand all" };
    // With every section folded, no file would show either way.
    let open = Section::ALL.into_iter().any(|s| !c.section_folded(s) && s.files(changes).next().is_some());
    let press = open.then_some(Message::GitView(GitViewMsg::FoldAll(doc, unfolded)));
    let toggle = ui::small(label, theme::button_ghost).on_press_maybe(press);
    row![space::horizontal(), toggle].padding(Padding { top: 0.0, right: 14.0, bottom: 4.0, left: 4.0 }).into()
}

/// A section's title and how many files it has, with a button for all of
/// them. Pressed, the title folds or unfolds the section.
fn section_header<'a>(doc: DocId, section: Section, files: usize, folded: bool, busy: bool) -> Element<'a, Message> {
    let all = match section {
        Section::Staged => "Unstage all",
        Section::Conflicts => "Mark all resolved",
        Section::Unstaged | Section::Untracked => "Stage all",
    };
    let press = (!busy).then_some(Message::GitView(GitViewMsg::Move(doc, Pick::All(section))));
    let tone = if section == Section::Conflicts { Tone::Warning } else { Tone::Neutral };
    let chevron = icon(if folded { Icon::ChevronRight } else { Icon::ChevronDown }, 13.0, theme::muted);
    let title =
        row![chevron, text(section.title()).size(theme::TEXT).font(SEMIBOLD), ui::badge(files.to_string(), tone)]
            .spacing(8)
            .align_y(Alignment::Center);
    let toggle = button(title)
        .padding(0)
        .style(theme::button_bare)
        .on_press(Message::GitView(GitViewMsg::ToggleSection(doc, section)))
        .width(Length::Fill);
    row![toggle, ui::small(all, theme::button_secondary).on_press_maybe(press)]
        .spacing(8)
        .padding(Padding { top: 6.0, right: 0.0, bottom: 0.0, left: 4.0 })
        .align_y(Alignment::Center)
        .into()
}
