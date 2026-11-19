//! The terminal area: header plus the split pane grid of the visible tab.

use iced::widget::{center, column, container, pane_grid, pick_list, row, space, text, tooltip};
use iced::{Alignment, Element, Length, Padding};
use termroost_core::{AgentKind, Axis, PaneId, Tab};

use crate::agent_icon::{agent_icon, terminal_icon};
use crate::app::{App, Message};
use crate::icons::Icon;
use crate::keymap::Action;
use crate::leave_room::leave_room;
use crate::notify::Category;
use crate::side_panel::SideTab;
use crate::sidebar::{branch_label, muted_text, place_label, place_path};
use crate::terminal_view::TerminalView;
use crate::theme::{self, SEMIBOLD, Tone};
use crate::ui::{self, ICON_BUTTON_WIDTH, icon_button, tip};
use crate::worktree::WorktreeMsg;

/// Space between the buttons of a terminal's title bar.
const CONTROL_SPACING: f32 = 2.0;
/// Width of the two buttons narrow terminals get, which the title leaves room for.
const COMPACT_CONTROLS_WIDTH: f32 = 2.0 * ICON_BUTTON_WIDTH + CONTROL_SPACING;
/// Space between a terminal's title and its buttons.
const TITLE_GAP: f32 = 8.0;
/// Height of the bar over the terminals.
const HEADER_HEIGHT: f32 = 40.0;

/// A message in the middle of the terminal area, with what to do about it.
fn empty_state<'a>(title: String, about: &'a str, action: &'a str, msg: Message) -> Element<'a, Message> {
    center(
        column![
            terminal_icon(48.0),
            column![text(title).size(theme::TEXT_PAGE).font(SEMIBOLD), text(about).size(theme::TEXT).style(ui::muted)]
                .spacing(4)
                .align_x(Alignment::Center),
            ui::primary(action).on_press(msg),
        ]
        .spacing(18)
        .align_x(Alignment::Center),
    )
    .into()
}

impl App {
    pub(crate) fn view_main(&self) -> Element<'_, Message> {
        let s = self.core.state();
        let Some(project) = s.selected_project.and_then(|p| s.project(p)) else {
            return empty_state(
                crate::DISPLAY_NAME.to_owned(),
                "Projects keep their terminals, splits and tasks, even across restarts and crashes.",
                "New project",
                Message::NewProject,
            );
        };
        let Some(tab) = s.active_tab() else {
            return empty_state(
                format!("{} has no sessions", project.name),
                "A session is a set of terminals, split side by side, that comes back after a restart.",
                "New session",
                Message::NewTab,
            );
        };

        let open: usize = s.tasks_of(project.id).filter(|t| !t.done).count();
        let git = self.git_of(tab);
        // While the right panel is hidden, buttons bring it back on any of its
        // tabs (Git only for a session in a repository).
        let panel_buttons: Element<'_, Message> = if self.prefs.panel_open {
            space::horizontal().width(0).into()
        } else {
            let mut choices = vec![(format!("Tasks ({open})"), false, Message::ShowPanel(SideTab::Tasks))];
            if git.is_some() {
                choices.push(("Git".into(), false, Message::ShowPanel(SideTab::Git)));
            }
            choices.push(("Files".into(), false, Message::ShowPanel(SideTab::Files)));
            choices.push(("Notes".into(), false, Message::ShowPanel(SideTab::Notes)));
            ui::segmented(choices)
        };
        let mut header = row![text(&tab.title).size(14).font(SEMIBOLD)];
        if let Some(head) = git.and_then(|g| g.head.as_ref()) {
            header = header.push(branch_label(head, 12.0, 40));
        }
        let dir = s.tab_working_dir(tab);
        let machine = self.core.tab_machine(tab.id);
        let mut place = row![
            muted_text(place_label(&machine, &dir, 70)).wrapping(text::Wrapping::None),
            icon_button(Icon::Copy, Message::CopyText(place_path(&machine, &dir)), "Copy path"),
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        if let Some(badge) = self.gone_badge(tab) {
            place = place.push(badge);
        }
        header = header.push(place);
        header = header.push(space::horizontal());
        if git.is_some() {
            header = header.push(tip(
                ui::small("Worktrees", theme::button_tonal).on_press(Message::Worktree(WorktreeMsg::Open)),
                "Switch this session to another git worktree, or make a new one",
                tooltip::Position::Bottom,
            ));
        }
        let header = container(header.push(panel_buttons).spacing(10).align_y(Alignment::Center))
            .padding([0, 12])
            .height(HEADER_HEIGHT)
            .center_y(HEADER_HEIGHT);

        let mut banners = column![].spacing(6).padding(Padding { top: 8.0, right: 8.0, bottom: 0.0, left: 8.0 });
        let messages =
            [(&self.error, Message::DismissError, Tone::Danger), (&self.notice, Message::DismissNotice, Tone::Accent)];
        for (message, dismiss, tone) in messages {
            if let Some(message) = message {
                let line = row![
                    text(message).size(theme::TEXT).width(Length::Fill),
                    icon_button(Icon::Close, dismiss, "Dismiss")
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                banners = banners.push(ui::note(tone, line));
            }
        }
        let body: Element<'_, Message> = match (self.shown_doc(), self.grids.get(&tab.id)) {
            (Some(doc), _) => self.view_doc(doc),
            (None, Some(grid)) => pane_grid(grid, move |_, pane, _| self.pane_content(*pane, tab))
                .spacing(4)
                .on_click(Message::PaneClicked)
                .on_drag(Message::PaneDragged)
                .on_resize(8, Message::PaneResized)
                .style(theme::pane_grid_style)
                .into(),
            (None, None) => center(text("…")).into(),
        };
        // Each part keeps its place in the column whether it shows anything or
        // not: widgets are matched to their state by position, so a banner
        // coming and going would otherwise reset the editor and the terminals.
        column![header, ui::divider(), self.view_file_strip(), banners, container(body).padding(6).height(Length::Fill)]
            .into()
    }

    fn pane_content<'a>(&'a self, pane: PaneId, tab: &'a Tab) -> pane_grid::Content<'a, Message> {
        let s = self.core.state();
        let active = tab.focused_pane == Some(pane);
        let handle = self.core.terminal(pane);

        let keeps_running = s.pane(pane).is_some_and(|p| p.keep_running);
        let elsewhere = handle.as_ref().is_some_and(|h| h.is_shown_elsewhere());
        let body: Element<'_, Message> = match handle.clone() {
            Some(_) if elsewhere => center(
                column![
                    text("In its own window").style(ui::muted),
                    ui::small("Bring back", theme::button_secondary).on_press(Message::BringBack(pane)),
                ]
                .spacing(10)
                .align_x(Alignment::Center),
            )
            .into(),
            Some(h) => TerminalView::new(
                h,
                self.keymap.clone(),
                self.keyboard_focus.clone(),
                active && self.dialog.is_none(),
                self.prefs.font_size,
                Message::FocusPane(pane),
                Message::RestartPane(pane),
            )
            .into(),
            None => center(text("Starting…").style(ui::muted)).into(),
        };

        let title = handle.as_ref().and_then(|h| h.title()).unwrap_or_else(|| "Terminal".to_owned());
        let agent = s.pane(pane).and_then(|p| p.agent.as_ref()).map(|a| {
            let label = match &a.session_id {
                Some(id) => format!("{} · {}", a.kind.label(), &id[..id.len().min(8)]),
                None => format!("{} · new session", a.kind.label()),
            };
            (a.kind, label)
        });
        // Nothing wraps; when space runs out the last item is cut off first, so
        // they go in order of importance. The gap on the right keeps the title
        // off the buttons.
        let mut title_row = row![].spacing(8).align_y(Alignment::Center).padding(Padding::ZERO.right(TITLE_GAP));
        // An alert the user hasn't seen. It goes away when the user focuses this terminal.
        let notice = self.notices.get(&pane).and_then(|n| n.first());
        if let Some(n) = notice {
            let label = match &n.detail {
                Some(d) if n.category == Category::Error => format!("{} {} · {d}", n.glyph(), n.headline),
                _ => format!("{} {}", n.glyph(), n.headline),
            };
            title_row = title_row.push(
                container(text(label).size(11).wrapping(text::Wrapping::None))
                    .padding([1, 7])
                    .style(theme::badge(theme::notice_tone(n.category)))
                    .clip(true),
            );
        }
        if let Some((kind, label)) = agent {
            let icon = agent_icon(kind, 12.0, theme::ink);
            // The label is cut inside the badge's padding, so a short badge keeps its rounded end.
            let label = container(text(label).size(11).wrapping(text::Wrapping::None)).clip(true);
            let badge = row![icon, label].spacing(4).align_y(Alignment::Center);
            title_row = title_row.push(container(badge).padding([1, 7]).style(theme::badge(Tone::Neutral)).clip(true));
        }
        if keeps_running {
            let mark = text("∞").size(13).style(ui::muted);
            title_row =
                title_row.push(tip(mark, "Keeps running when Terminal Rooster quits", tooltip::Position::Bottom));
        }
        title_row = title_row.push(container(text(title).size(12).wrapping(text::Wrapping::None)).clip(true));

        let k = &self.keymap;
        let remote = handle.as_ref().is_some_and(|h| h.remote().is_some());
        let pop_out = (keeps_running && !elsewhere && !remote && crate::host_exe::WINDOWS_OF_THEIR_OWN)
            .then(|| icon_button(Icon::Open, Message::OpenInWindow(pane), "Open in its own window"));
        let full = row![
            pick_list(&AgentKind::ALL[..], None::<AgentKind>, move |k| Message::StartAgent(pane, k))
                .placeholder("Start agent")
                .text_size(12)
                .padding([2, 8])
                .style(theme::dropdown)
                .menu_style(theme::dropdown_menu),
        ]
        .push(pop_out)
        .push(icon_button(Icon::Settings, Message::PaneSettings(pane), "Terminal settings & resume command"))
        .extend([
            icon_button(
                Icon::SplitRight,
                Message::Split(pane, Axis::Vertical),
                k.tip("Split right", Action::SplitRight),
            ),
            icon_button(
                Icon::SplitDown,
                Message::Split(pane, Axis::Horizontal),
                k.tip("Split down", Action::SplitDown),
            ),
            icon_button(Icon::Close, Message::ClosePane(pane), k.tip("Close terminal", Action::CloseTerminal)),
        ])
        .spacing(CONTROL_SPACING)
        .align_y(Alignment::Center);
        // Narrow panes get only the essentials. Keep `COMPACT_CONTROLS_WIDTH` in step.
        let compact = row![
            icon_button(Icon::Settings, Message::PaneSettings(pane), "Terminal settings, agents & resume command"),
            icon_button(Icon::Close, Message::ClosePane(pane), k.tip("Close terminal", Action::CloseTerminal)),
        ]
        .spacing(CONTROL_SPACING)
        .align_y(Alignment::Center);

        let title_bar = pane_grid::TitleBar::new(leave_room(title_row, COMPACT_CONTROLS_WIDTH))
            .controls(pane_grid::Controls::dynamic(full, compact))
            .always_show_controls()
            .padding(Padding { top: 3.0, right: 4.0, bottom: 3.0, left: 10.0 })
            .style(theme::title_bar(active));

        // The frame's fill shows behind the title bar; the terminal sits inside its edges.
        let body = container(body)
            .padding(Padding { top: 0.0, right: 1.0, bottom: 1.0, left: 1.0 })
            .width(Length::Fill)
            .height(Length::Fill);
        pane_grid::Content::new(body).title_bar(title_bar).style(theme::pane_frame(active))
    }
}
