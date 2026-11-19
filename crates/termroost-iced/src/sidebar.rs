//! Project and tab columns. Both lists can be reordered by dragging, and a
//! search box filters the projects.

use std::path::Path;

use iced::widget::{MouseArea, button, column, container, hover, mouse_area, row, space, stack, text, tooltip};
use iced::{Alignment, Element, Length, Padding};
use termroost_core::git::Head;
use termroost_core::{AgentKind, Machine, PaneId, Project, ProjectGroup, RemoteStatus, Tab, TabId};

use crate::agent_icon::{agent_icon, terminal_icon};
use crate::app::{App, DropTarget, Message, PROJECT_SEARCH, RENAME_INPUT, RenameTarget};
use crate::busy::busy_mark;
use crate::groups::{self, Row};
use crate::icons::{Icon, icon};
use crate::keymap::Action;
use crate::notify::{Category, Notice};
use crate::on_escape::on_escape;
use crate::reorder::reorder;
use crate::theme::{self, BOLD, SEMIBOLD, Tone};
use crate::ui::{self, ICON_BUTTON_WIDTH, icon_button, tip};
use crate::worktree::WorktreeMsg;

/// Width of the edit/delete buttons in the selected list item.
const ACTIONS_WIDTH: f32 = 26.0;
/// Space between the items of the project and session lists.
const LIST_SPACING: f32 = 2.0;
/// How far right a group's projects are.
const GROUP_INDENT: f32 = 12.0;
/// Space between a column's edges and its list.
const COLUMN_PADDING: f32 = 8.0;
/// The padding of a column's header, which lines its text up with the list's.
const HEADER_PADDING: Padding = Padding { top: 10.0, right: COLUMN_PADDING, bottom: 6.0, left: 16.0 };

/// Powerline's branch symbol, which the bundled JetBrains Mono has.
const BRANCH_SYMBOL: &str = "\u{E0A0}";

/// A git branch (or detached commit) with the branch symbol, cut to `max` characters.
pub fn branch_label<'a>(head: &Head, size: f32, max: usize) -> Element<'a, Message> {
    let name = head.to_string();
    let name = if name.chars().count() > max {
        format!("{}…", name.chars().take(max.saturating_sub(1)).collect::<String>())
    } else {
        name
    };
    row![
        text(BRANCH_SYMBOL)
            .font(theme::MONO)
            .size(size)
            .style(|t: &iced::Theme| text::Style { color: Some(theme::accent(t)) }),
        text(name).size(size).wrapping(text::Wrapping::None),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

/// Text size of [`muted_text`].
const MUTED_SIZE: f32 = 11.0;

pub fn muted_text<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(MUTED_SIZE).style(ui::muted)
}

/// Shows a path with the home directory abbreviated to `~`.
pub fn display_dir(path: &std::path::Path) -> String {
    if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
    }
    path.display().to_string()
}

/// Like [`display_dir`], but keeps only the trailing components that fit in `max` characters.
pub fn short_dir(path: &std::path::Path, max: usize) -> String {
    shorten(display_dir(path), max)
}

/// A folder of a computer as it is shown: as [`short_dir`] on this one; on a
/// remote host after its name, scp style (`droplet:~/app`), with `~` for the
/// home folder there once it is known.
pub fn place_label(machine: &Machine, path: &Path, max: usize) -> String {
    let Machine::Remote(host) = machine else { return short_dir(path, max) };
    let shown = match host.home().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    let name = host.name();
    format!("{name}:{}", shorten(shown, max.saturating_sub(name.chars().count() + 1).max(8)))
}

/// `path` with the host in front for a remote one (`droplet:/srv/app`), as
/// `scp` and `rsync` take it.
pub fn place_path(machine: &Machine, path: &Path) -> String {
    match machine.host() {
        Some(host) => format!("{host}:{}", path.display()),
        None => path.display().to_string(),
    }
}

/// Keeps only the trailing parts of the path `full` that fit in `max` characters.
fn shorten(full: String, max: usize) -> String {
    if full.chars().count() <= max {
        return full;
    }
    let parts: Vec<&str> = full.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let mut out = String::new();
    for part in parts.iter().rev() {
        let candidate = if out.is_empty() { part.to_string() } else { format!("{part}/{out}") };
        if candidate.chars().count() + 2 > max && !out.is_empty() {
            break;
        }
        out = candidate;
    }
    format!("…/{out}")
}

/// Whether a project matches the projects search: each word of `query` is in
/// its name, its group's name or its folder's name, ignoring case. An empty
/// query matches all.
pub fn matches_search(name: &str, group: Option<&str>, dir: &Path, query: &str) -> bool {
    let mut words = query.split_whitespace().map(str::to_lowercase).peekable();
    if words.peek().is_none() {
        return true;
    }
    let name = name.to_lowercase();
    let group = group.unwrap_or_default().to_lowercase();
    // Not the whole path: its first folders are often shared by every project.
    let folder = dir.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
    words.all(|w| name.contains(&w) || group.contains(&w) || folder.contains(&w))
}

/// Text size of a list item's name.
const NAME_SIZE: f32 = 14.0;
/// Space around the text of a list item.
const ITEM_PADDING: Padding = Padding { top: 7.0, right: 6.0, bottom: 7.0, left: 6.0 };

/// `content` centered on the first line of a list item's name, where it
/// stays when the name wraps.
fn on_first_line<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let line = text::LineHeight::default().to_absolute(NAME_SIZE.into());
    container(content).height(line).align_y(Alignment::Center).into()
}

/// A list item: the bar that marks the selected one, `body` (which takes the
/// clicks) and `actions`. The widgets are the same whether it's selected or
/// not, so the mouse area keeps its click state and a double click on an
/// unselected item still registers. `drop_target` outlines it while a task is
/// held over it.
fn list_item<'a>(
    body: MouseArea<'a, Message>,
    actions: Element<'a, Message>,
    selected: bool,
    drop_target: bool,
) -> Element<'a, Message> {
    let bar = container(space()).width(3).height(Length::Fill).style(theme::selection_bar(selected));
    let bar = container(bar).padding([9, 0]).height(Length::Fill);
    // As tall as the body, which the bar then fills. Without the explicit
    // height the bar would make the whole item fill, and a project in a group
    // (beside the group's rail, which fills too) would have no height at all.
    let line = row![bar, body, container(actions).width(ACTIONS_WIDTH)].height(Length::Shrink);
    container(line.align_y(Alignment::Center)).width(Length::Fill).style(theme::list_item(selected, drop_target)).into()
}

/// The edit and delete buttons of the selected item; room for them on the others.
fn item_actions<'a>(selected: bool, edit: (Message, &'a str), delete: (Message, &'a str)) -> Element<'a, Message> {
    if selected {
        column![icon_button(Icon::Edit, edit.0, edit.1), icon_button(Icon::Trash, delete.0, delete.1)].into()
    } else {
        space::horizontal().width(ACTIONS_WIDTH).into()
    }
}

/// A button that copies `dir`, for over the end of the folder line that ends a
/// list item. It sits on the item's own fill, which hides the end of a long
/// folder rather than letting it run under the button.
fn copy_dir_button<'a>(dir: String, selected: bool) -> Element<'a, Message> {
    let copy = icon_button(Icon::Copy, Message::CopyText(dir), "Copy path");
    let copy = container(copy).padding(Padding { left: 4.0, ..Padding::ZERO }).style(theme::list_item_cover(selected));
    // Centered on the folder's line.
    let line = text::LineHeight::default().to_absolute(MUTED_SIZE.into()).0;
    let bottom = ITEM_PADDING.bottom + (line - ICON_BUTTON_WIDTH) / 2.0;
    container(copy)
        .padding(Padding { right: ITEM_PADDING.right, bottom, ..Padding::ZERO })
        .align_right(Length::Fill)
        .align_bottom(Length::Fill)
        .into()
}

/// The line down the left of a group's projects, as wide as they are indented.
fn group_rail<'a>() -> Element<'a, Message> {
    let line = container(space::horizontal()).width(1).height(Length::Fill).style(theme::divider);
    container(line).padding(Padding { left: 6.0, ..Padding::ZERO }).width(GROUP_INDENT).height(Length::Fill).into()
}

/// "+ New …" under the last item of a list, as wide as the list.
fn add_item<'a>(msg: Message, label: &'a str) -> Element<'a, Message> {
    let content =
        row![icon(Icon::Plus, 13.0, theme::muted), text(label).size(theme::TEXT)].spacing(8).align_y(Alignment::Center);
    button(content).width(Length::Fill).padding([6, 11]).style(theme::add_item_button).on_press(msg).into()
}

fn activity_dot<'a>(on: bool) -> Element<'a, Message> {
    if on {
        text("●").size(10).style(|t: &iced::Theme| text::Style { color: Some(theme::accent(t)) }).into()
    } else {
        space::horizontal().width(0).into()
    }
}

/// What a list item shows next to its name: the mark of an unseen alert
/// (✗ a failure, ✓ an agent), else a dot for new output.
fn marker<'a>(notice: Option<&Notice>, activity: bool) -> Element<'a, Message> {
    let Some(n) = notice else { return activity_dot(activity) };
    let category = n.category;
    tip(
        text(n.glyph())
            .size(13)
            .font(BOLD)
            .style(move |t: &iced::Theme| text::Style { color: Some(theme::notice_color(t, category)) }),
        n.headline.clone(),
        tooltip::Position::Bottom,
    )
}

impl App {
    /// For a remote project whose host isn't connected: a line saying so,
    /// with why in its tooltip when connecting failed.
    fn connection_note<'a>(&self, p: &Project) -> Option<Element<'a, Message>> {
        let host = p.host.as_deref()?;
        let (line, tone, why) = match self.core.remote_status(host) {
            RemoteStatus::Connecting => ("connecting…", Tone::Warning, None),
            RemoteStatus::Copying => ("copying Terminal Rooster's helper there…", Tone::Warning, None),
            RemoteStatus::Lost => ("connection lost", Tone::Warning, None),
            RemoteStatus::Failed(e) => ("can't connect", Tone::Danger, Some(e)),
            RemoteStatus::Idle | RemoteStatus::Connected => return None,
        };
        let note = text(line).size(MUTED_SIZE).wrapping(text::Wrapping::None).style(theme::toned(tone));
        Some(match why {
            Some(why) => tip(note, why, tooltip::Position::Bottom),
            None => note.into(),
        })
    }

    /// The alert to show for a set of terminals: a failure before an agent.
    fn notice_of(&self, panes: impl IntoIterator<Item = PaneId>) -> Option<&Notice> {
        let found: Vec<&Notice> = panes.into_iter().filter_map(|p| self.notices.get(&p)?.first()).collect();
        found.iter().find(|n| n.category == Category::Error).or(found.first()).copied()
    }

    fn tab_notice(&self, tab: &Tab) -> Option<&Notice> {
        self.notice_of(tab.layout.panes())
    }

    fn rename_input(&self, target: RenameTarget) -> Option<Element<'_, Message>> {
        let r = self.rename.as_ref().filter(|r| r.target == target)?;
        Some(
            ui::input("Name", &r.value)
                .id(RENAME_INPUT)
                .on_input(Message::RenameInput)
                .on_submit(Message::RenameCommit)
                .into(),
        )
    }

    pub(crate) fn view_projects(&self) -> Element<'_, Message> {
        let s = self.core.state();
        let header = row![
            ui::heading("Projects"),
            space::horizontal(),
            icon_button(Icon::Settings, Message::OpenSettings, "Settings"),
            icon_button(Icon::Plus, Message::NewProject, self.keymap.tip("New project", Action::NewProject)),
        ]
        .spacing(2)
        .align_y(Alignment::Center)
        .padding(HEADER_PADDING);

        let shown: Vec<&Project> = self.listed_projects().collect();
        // While searching, groups show the projects found in them.
        let searching = self.searching_projects();
        let open = |g| searching || s.group(g).is_some_and(|g| !g.collapsed);
        let (rows, depths): (Vec<Row>, Vec<u8>) =
            groups::rows(&groups::entries(shown.iter().copied()), open).into_iter().unzip();
        let rows_view = rows.iter().zip(&depths).map(|(row, depth)| {
            let item = match *row {
                Row::Group(g) => s.group(g).map(|g| self.group_item(g, searching)),
                Row::Project(p) => s.project(p).map(|p| self.project_item(p, *depth > 0)),
            };
            item.unwrap_or_else(|| space::horizontal().width(0).into())
        });
        // While searching, a place in the list isn't the same place among all projects.
        let all = groups::entries(&s.projects);
        let list = reorder(rows_view)
            .spacing(LIST_SPACING)
            .nested(depths, GROUP_INDENT)
            .on_drop_if(self.rename.is_none(), move |from, target| {
                groups::drop_command(&all, &rows, from, target).map(Message::ArrangeProjects)
            });
        let mut items = column![list].spacing(LIST_SPACING).padding([0.0, COLUMN_PADDING]);
        if shown.is_empty() && !s.projects.is_empty() {
            items = items.push(container(muted_text("No projects match")).padding([6, 10]));
        }
        let items = items.push(add_item(Message::NewProject, "New project"));
        container(column![header, self.project_search_box(), ui::scroll(items).height(Length::Fill)])
            .width(190)
            .height(Length::Fill)
            .style(theme::side_column)
            .into()
    }

    /// The projects the list shows: those matching the search, in their order.
    pub(crate) fn listed_projects(&self) -> impl Iterator<Item = &Project> {
        let s = self.core.state();
        let query = self.project_search.as_str();
        s.projects.iter().filter(move |p| {
            let group = p.group.and_then(|g| s.group(g)).map(|g| g.name.as_str());
            matches_search(&p.name, group, &p.working_dir, query)
        })
    }

    pub(crate) fn searching_projects(&self) -> bool {
        self.project_search.split_whitespace().next().is_some()
    }

    /// The box that filters the projects list, with a ✕ to empty it.
    fn project_search_box(&self) -> Element<'_, Message> {
        // Room on the left for the magnifier, and on the right for the ✕ while
        // there is one, so the text doesn't run under them.
        let right = if self.project_search.is_empty() { 8.0 } else { 28.0 };
        let input = ui::input(&self.keymap.hint("Search", Action::SearchProjects), &self.project_search)
            .id(PROJECT_SEARCH)
            .on_input(Message::ProjectSearch)
            .on_submit(Message::ProjectSearchSubmit)
            .padding(Padding { top: 5.0, right, bottom: 5.0, left: 29.0 });
        let magnifier = container(icon(Icon::Search, 13.0, theme::faint))
            .padding(Padding { left: 11.0, ..Padding::ZERO })
            .center_y(Length::Fill);
        // The input stays the first layer, so it keeps its focus when the ✕ comes and goes.
        let mut field = stack![on_escape(input, Message::ProjectSearchEscape), magnifier];
        if !self.project_search.is_empty() {
            let clear = icon_button(Icon::Close, Message::ProjectSearchClear, "Clear search");
            field = field.push(container(clear).padding([0, 3]).align_right(Length::Fill).center_y(Length::Fill));
        }
        container(field).padding(Padding { top: 0.0, right: COLUMN_PADDING, bottom: 8.0, left: COLUMN_PADDING }).into()
    }

    /// A group's row: a click shows or hides its projects.
    fn group_item<'a>(&'a self, group: &'a ProjectGroup, searching: bool) -> Element<'a, Message> {
        if let Some(input) = self.rename_input(RenameTarget::Group(group.id)) {
            return input;
        }
        let s = self.core.state();
        let projects: Vec<&Project> = s.projects_in(group.id).collect();
        let open = searching || !group.collapsed;
        let chevron = icon(if open { Icon::ChevronDown } else { Icon::ChevronRight }, 12.0, theme::muted);
        let mut label = row![chevron, text(&group.name).size(13).font(SEMIBOLD).width(Length::Fill)]
            .spacing(6)
            .align_y(Alignment::Center);
        let mut selected = false;
        if !open {
            // Closed, it stands for its projects: their alerts, and the selection.
            selected = projects.iter().any(|p| s.selected_project == Some(p.id));
            let tabs = || projects.iter().flat_map(|p| s.tabs_of(p.id));
            let notice = self.notice_of(tabs().flat_map(|t| t.layout.panes()));
            label = label.push(busy_mark(self.busy_in(tabs().flat_map(|t| t.layout.panes())), theme::accent));
            label = label.push(marker(notice, tabs().any(|t| self.activity.contains(&t.id))));
        }
        let label = label.push(ui::badge(projects.len().to_string(), Tone::Neutral));
        let mut body = mouse_area(container(label).padding(ITEM_PADDING).width(Length::Fill))
            .on_double_click(Message::GroupDoubleClicked(group.id));
        if !searching {
            // On release, so pressing it to drag it doesn't open or close it.
            body = body.on_release(Message::ToggleGroup(group.id));
        }
        let rename = icon_button(Icon::Edit, Message::StartRename(RenameTarget::Group(group.id)), "Rename group");
        list_item(body, rename, selected, false)
    }

    /// A project's row; `nested` in a group, it is drawn further right.
    fn project_item<'a>(&'a self, p: &'a Project, nested: bool) -> Element<'a, Message> {
        let item = self.project_row(p);
        if nested { row![group_rail(), item].into() } else { item }
    }

    fn project_row<'a>(&'a self, p: &'a Project) -> Element<'a, Message> {
        if let Some(input) = self.rename_input(RenameTarget::Project(p.id)) {
            return input;
        }
        let s = self.core.state();
        let machine = self.core.project_machine(p.id);
        let selected = s.selected_project == Some(p.id);
        let new_output = s.tabs_of(p.id).any(|t| self.activity.contains(&t.id));
        let notice = self.notice_of(s.tabs_of(p.id).flat_map(|t| t.layout.panes()));
        let busy = self.busy_in(s.tabs_of(p.id).flat_map(|t| t.layout.panes()));
        let label = column![
            row![
                text(&p.name).size(NAME_SIZE).width(Length::Fill),
                busy_mark(busy, theme::accent),
                marker(notice, new_output)
            ]
            .spacing(4)
            .align_y(Alignment::Center),
            // Cut at the item's edge, where the copy button covers its end:
            // a long folder name would otherwise run on past the button.
            container(muted_text(place_label(&machine, &p.working_dir, 24)).wrapping(text::Wrapping::None)).clip(true),
        ]
        .spacing(1);
        let label = match self.connection_note(p) {
            Some(note) => label.push(note),
            None => label,
        };
        // The folder can be copied without opening the project, from a button
        // that shows while the pointer is over it.
        let label = container(label).padding(ITEM_PADDING).width(Length::Fill);
        let body = mouse_area(hover(label, copy_dir_button(place_path(&machine, &p.working_dir), selected)))
            .on_press(Message::SelectProject(p.id))
            .on_double_click(Message::StartRename(RenameTarget::Project(p.id)));
        let actions = item_actions(
            selected,
            (Message::EditProject(p.id), "Edit project"),
            (Message::AskDeleteProject(p.id), "Delete project"),
        );
        list_item(body, actions, selected, false)
    }

    pub(crate) fn view_tabs(&self) -> Element<'_, Message> {
        let s = self.core.state();
        let Some(project) = s.selected_project.and_then(|p| s.project(p)) else {
            return container(space::horizontal()).width(210).height(Length::Fill).style(theme::side_column).into();
        };
        let mut header = row![
            column![
                text(&project.name).size(15).font(SEMIBOLD),
                muted_text(place_label(&self.core.project_machine(project.id), &project.working_dir, 26))
                    .wrapping(text::Wrapping::None)
            ]
            .spacing(1)
            .width(Length::Fill),
            icon_button(Icon::Terminal, Message::NewTab, self.keymap.tip("New session", Action::NewSession)),
        ];
        // In a repository, the Worktrees dialog too: a new worktree gets a
        // session of its own.
        if self.in_repo() {
            header = header.push(icon_button(
                Icon::Branch,
                Message::Worktree(WorktreeMsg::Open),
                "New worktree\nIn a session of its own, or open an existing one",
            ));
        }
        let header = header.spacing(2).align_y(Alignment::Center).padding(HEADER_PADDING);
        let ids: Vec<TabId> = s.tabs_of(project.id).map(|t| t.id).collect();
        let list = reorder(s.tabs_of(project.id).map(|t| self.tab_item(project, t)))
            .spacing(LIST_SPACING)
            .on_move_if(self.rename.is_none(), move |from, to| Message::MoveTab(ids[from], to));
        let items = column![list].spacing(LIST_SPACING).padding([0.0, COLUMN_PADDING]);
        let heading =
            container(ui::heading("Sessions")).padding(Padding { left: HEADER_PADDING.left, ..Padding::ZERO });
        container(column![header, heading, ui::scroll(items).height(Length::Fill)].spacing(6))
            .width(210)
            .height(Length::Fill)
            .style(theme::side_column)
            .into()
    }

    fn tab_item<'a>(&'a self, project: &'a Project, tab: &'a Tab) -> Element<'a, Message> {
        if let Some(input) = self.rename_input(RenameTarget::Tab(tab.id)) {
            return input;
        }
        let s = self.core.state();
        let selected = project.active_tab == Some(tab.id);
        let panes = tab.layout.panes();
        let agents: Vec<&str> =
            panes.iter().filter_map(|p| s.pane(*p)?.agent.as_ref()).map(|a| a.kind.label()).collect();
        let mut subtitle = format!("{} terminal{}", panes.len(), if panes.len() == 1 { "" } else { "s" });
        if !agents.is_empty() {
            subtitle.push_str(" · ");
            subtitle.push_str(&agents.join(", "));
        }
        let open = s.open_task_count(tab.id);
        let badge: Element<'_, Message> = if open > 0 {
            tip(ui::badge(open.to_string(), Tone::Neutral), "Open tasks", tooltip::Position::Bottom)
        } else {
            space::horizontal().width(0).into()
        };
        let mut marks =
            row![marker(self.tab_notice(tab), self.activity.contains(&tab.id))].spacing(4).align_y(Alignment::Center);
        // One logo per kind of agent, in terminal order.
        let mut kinds: Vec<AgentKind> = Vec::new();
        for kind in panes.iter().filter_map(|p| Some(s.pane(*p)?.agent.as_ref()?.kind)) {
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        // A session without an agent is plain terminals: TerminalRooster's own icon.
        if kinds.is_empty() {
            marks = marks.push(tip(terminal_icon(NAME_SIZE), "Terminal", tooltip::Position::Bottom));
        }
        for kind in kinds {
            marks = marks.push(tip(agent_icon(kind, NAME_SIZE, theme::ink), kind.label(), tooltip::Position::Bottom));
        }
        // A long name wraps, breaking inside a word too when one doesn't fit.
        let name = text(&tab.title).size(NAME_SIZE).width(Length::Fill).wrapping(text::Wrapping::WordOrGlyph);
        let busy = on_first_line(busy_mark(self.busy_in(panes.iter().copied()), theme::accent));
        let mut label = column![row![on_first_line(marks), name, busy, on_first_line(badge)].spacing(6)].spacing(2);
        if let Some(badge) = self.gone_badge(tab) {
            label = label.push(badge);
        } else if let Some(head) = self.git_of(tab).and_then(|g| g.head.as_ref()) {
            label = label.push(branch_label(head, 11.0, 24));
        }
        let label = label.push(muted_text(subtitle));
        let target = DropTarget::Session(tab.id);
        let body = mouse_area(container(label).padding(ITEM_PADDING).width(Length::Fill))
            .on_press(Message::SelectTab(tab.id))
            .on_double_click(Message::StartRename(RenameTarget::Tab(tab.id)));
        let actions = item_actions(
            selected,
            (Message::StartRename(RenameTarget::Tab(tab.id)), "Rename session"),
            (Message::AskDeleteTab(tab.id), "Delete session"),
        );
        // A task dragged from the task panel can be dropped here to link it to this session.
        let mut item = mouse_area(list_item(body, actions, selected, self.is_drop_target(target)));
        if self.task_drag.is_some() {
            item = item
                .on_enter(Message::TaskDragOver(target, true))
                .on_exit(Message::TaskDragOver(target, false))
                .on_release(Message::TaskDrop(target));
        }
        item.into()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn dir(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn search_matches_each_word_in_the_name_or_folder() {
        let worktree = dir(&["projects", "TermRoost-feature-search"]);
        let m = |query| matches_search("TerminalRooster", None, &worktree, query);
        assert!(m(""), "no search: every project");
        assert!(m("   "));
        assert!(m("rooster"), "in the name, any case");
        assert!(m("FEATURE"), "in the folder");
        assert!(m("search rooster"), "each word on its own, in any order");
        assert!(!m("rooster blog"), "every word has to match");
        assert!(!m("projects"), "the folders above the project's own don't count");
        assert!(!m("roosterterm"));
    }

    #[test]
    fn search_copes_with_folders_without_a_name() {
        assert!(matches_search("Root", None, Path::new("/"), "root"));
        assert!(!matches_search("Root", None, Path::new("/"), "home"));
    }

    #[test]
    fn search_finds_a_group_by_its_name() {
        let api = dir(&["work", "api"]);
        assert!(matches_search("api", Some("Client work"), &api, "client"), "every project of the group");
        assert!(matches_search("api", Some("Client work"), &api, "client api"), "each word anywhere");
        assert!(!matches_search("api", Some("Client work"), &api, "client web"));
        assert!(!matches_search("api", None, &api, "client"));
    }
}
