//! Drawing: the list on the left, the shown session's terminals on the right.
//!
//! Lines are thin and colors few: the accent marks where the keyboard is,
//! dim text is for what matters less. Everything else is the user's own
//! terminal colors.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Clear;
use termroost_core::{AgentEvent, Alert, Axis, GridSize, ProjectId, RenderContent, Tab, TabId, git};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Edit, Focus, Note, Row};
use crate::colors::{self, ui};
use crate::folders;
use crate::panes::{self, Placed};

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
/// Below this width the list only shows while it is in use.
const ROOMY: u16 = 72;

enum Item {
    Gap,
    Project(ProjectId),
    Session(TabId),
    NewProject,
}

impl App {
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.hits = Default::default();
        self.cursor_shape = None;
        self.animating = false;
        // When narrow, the list shows only while in use, over the terminals
        // rather than beside them: resizing them would make them repaint.
        let roomy = area.width >= ROOMY;
        let list_shown = roomy || self.focus == Focus::List || self.edit.is_some();
        let list = Rect { width: (area.width / 5).clamp(24, 32).min(area.width.saturating_sub(1)), ..area };
        let main = if roomy { Rect { x: list.right() + 1, width: area.width - list.width - 1, ..area } } else { area };
        let headers = self.draw_main(f, main);
        if list_shown {
            if !roomy {
                f.render_widget(Clear, Rect { width: list.width + 1, ..list });
                self.hits.list = Rect { width: list.width + 1, ..list };
            }
            self.draw_list(f, list);
            let beside: Vec<Rect> =
                headers.iter().map(|h| h.intersection(Rect { x: list.right() + 1, ..area })).collect();
            vline(f.buffer_mut(), list.right(), area.y, area.bottom(), &beside);
        }
        self.update_title();
    }

    // ---- the list ----

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        if self.hits.list.is_empty() {
            self.hits.list = area;
        }
        if area.height == 0 {
            return;
        }
        let w = area.width;
        put(f.buffer_mut(), area.x + 1, area.y, "Terminal Rooster", ui::accent().add_modifier(Modifier::BOLD), w - 1);

        let bottom = self.list_footer(w.saturating_sub(2) as usize);
        let bottom_h = (bottom.len() as u16 + 1).min(area.height.saturating_sub(3));
        for (i, line) in bottom.iter().rev().take(bottom_h as usize).enumerate() {
            let y = area.bottom() - 1 - i as u16;
            let mut x = area.x + 1;
            for (text, style) in line {
                x = f.buffer_mut().set_stringn(x, y, text, (area.right() - x) as usize, *style).0;
            }
        }

        let top = area.y + 2;
        let height = area.bottom().saturating_sub(bottom_h).saturating_sub(top) as usize;
        let items = self.items();
        let cursor = self.cursor();
        let shown = self.core.state().active_tab().map(|t| t.id);
        let at = items.iter().position(|item| match item {
            Item::NewProject => true,
            Item::Project(p) => self.focus == Focus::List && cursor == Some(Row::Project(*p)),
            Item::Session(t) => Some(*t) == shown || (self.focus == Focus::List && cursor == Some(Row::Session(*t))),
            Item::Gap => false,
        });
        if let Some(at) = at {
            if at < self.list_scroll {
                self.list_scroll = at;
            } else if at >= self.list_scroll + height {
                self.list_scroll = at + 1 - height;
            }
        }
        self.list_scroll = self.list_scroll.min(items.len().saturating_sub(height));

        for (i, item) in items.iter().enumerate().skip(self.list_scroll).take(height) {
            let y = top + (i - self.list_scroll) as u16;
            let line = Rect { y, height: 1, ..area };
            match *item {
                Item::Gap => {}
                Item::NewProject => self.draw_new_project(f, line),
                Item::Project(p) => self.draw_row(f, line, Row::Project(p), cursor, shown),
                Item::Session(t) => self.draw_row(f, line, Row::Session(t), cursor, shown),
            }
        }
    }

    fn items(&self) -> Vec<Item> {
        let s = self.core.state();
        let mut items = Vec::new();
        for p in &s.projects {
            if !items.is_empty() {
                items.push(Item::Gap);
            }
            items.push(Item::Project(p.id));
            items.extend(s.tabs_of(p.id).map(|t| Item::Session(t.id)));
        }
        if matches!(self.edit, Some(Edit::NewProject { .. })) {
            if !items.is_empty() {
                items.push(Item::Gap);
            }
            items.push(Item::NewProject);
        }
        items
    }

    fn draw_row(&mut self, f: &mut Frame, line: Rect, row: Row, cursor: Option<Row>, shown: Option<TabId>) {
        let s = self.core.state();
        let (name, indent, status) = match row {
            Row::Project(p) => {
                let Some(p) = s.project(p) else { return };
                (p.name.clone(), 2, None)
            }
            Row::Session(t) => {
                let Some(tab) = s.tab(t).cloned() else { return };
                (tab.title.clone(), 4, self.session_status(&tab))
            }
        };
        self.hits.rows.push((line.y, row));
        let pointed = self.focus == Focus::List && cursor == Some(row);
        let is_shown = matches!(row, Row::Session(t) if Some(t) == shown);
        let buf = f.buffer_mut();
        if pointed {
            put(buf, line.x, line.y, "▌", ui::accent(), 1);
        } else if is_shown {
            put(buf, line.x, line.y, "▌", ui::dim(), 1);
        }
        let x = line.x + indent;
        let room = line.right().saturating_sub(x + 3);

        match &self.edit {
            Some(Edit::Rename { row: r, field }) if *r == row => {
                let (text, caret) = field.view(room as usize + 2);
                put(buf, x, line.y, &text, ui::accent(), room + 2);
                f.set_cursor_position(Position { x: x + caret as u16, y: line.y });
                self.cursor_shape = Some(termroost_core::CursorShape::Beam);
                return;
            }
            Some(Edit::Delete(r)) if *r == row => {
                let what = match row {
                    Row::Project(_) => format!("Delete {name} and its sessions?"),
                    Row::Session(_) => format!("Delete {name}?"),
                };
                put(buf, x, line.y, &fit(&what, room as usize + 2), ui::bad().add_modifier(Modifier::BOLD), room + 2);
                return;
            }
            _ => {}
        }

        let style = match (row, pointed, is_shown) {
            (_, true, _) => ui::accent().add_modifier(Modifier::BOLD),
            (Row::Project(_), ..) | (_, _, true) => ui::strong(),
            _ => Style::new(),
        };
        put(buf, x, line.y, &fit(&name, room as usize), style, room);
        if let Some((mark, style)) = status {
            put(buf, line.right().saturating_sub(2), line.y, mark, style, 1);
        }
    }

    fn draw_new_project(&mut self, f: &mut Frame, line: Rect) {
        let Some(Edit::NewProject { field, .. }) = &self.edit else { return };
        let buf = f.buffer_mut();
        put(buf, line.x, line.y, "▌", ui::accent(), 1);
        put(buf, line.x + 2, line.y, "+", ui::accent().add_modifier(Modifier::BOLD), 1);
        let x = line.x + 4;
        let room = line.right().saturating_sub(x + 1);
        let (text, caret) = field.view(room as usize);
        put(buf, x, line.y, &text, Style::new(), room);
        f.set_cursor_position(Position { x: x + caret as u16, y: line.y });
        self.cursor_shape = Some(termroost_core::CursorShape::Beam);
    }

    /// What a session's row says at its end: an alert, a working agent, or a busy terminal.
    fn session_status(&mut self, tab: &Tab) -> Option<(&'static str, Style)> {
        let panes = tab.layout.panes();
        if let Some(alert) = panes.iter().filter_map(|p| self.alerts.get(p)).max_by_key(|a| urgency(a)) {
            return Some(match alert {
                Alert::Agent { event: AgentEvent::Done, .. } => ("✓", ui::good()),
                Alert::Agent { event: AgentEvent::Failed, .. } | Alert::CommandFailed { .. } => ("✗", ui::bad()),
                Alert::Agent { .. } | Alert::Notification { .. } => ("!", ui::warn().add_modifier(Modifier::BOLD)),
            });
        }
        let activity: Vec<_> = panes.iter().filter_map(|p| self.core.terminal(*p)).map(|h| h.activity()).collect();
        if activity.iter().any(|a| a.agent_working) {
            self.animating = true;
            let frame = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() / 120;
            return Some((SPINNER[frame as usize % SPINNER.len()], ui::accent()));
        }
        activity.iter().any(|a| !a.is_idle()).then(|| ("•", ui::dim()))
    }

    /// The lines under the list: a note, a question, or the keys that work here.
    fn list_footer(&self, width: usize) -> Vec<Vec<(String, Style)>> {
        let mut lines = Vec::new();
        if let Some((note, _)) = &self.note {
            let (text, style) = match note {
                Note::Done(t) => (t, ui::good()),
                Note::Problem(t) => (t, ui::bad()),
            };
            lines.extend(wrap(text, width).into_iter().take(3).map(|l| vec![(l, style)]));
        }
        let keys: &[(&str, &str)] = match &self.edit {
            Some(Edit::Rename { .. }) => &[("↵", "save"), ("esc", "cancel")],
            Some(Edit::NewProject { error, .. }) => {
                if let Some(e) = error {
                    lines.push(vec![(e.to_string(), ui::bad())]);
                }
                &[("↵", "open"), ("tab", "complete"), ("esc", "cancel")]
            }
            Some(Edit::Delete(_)) => &[("y", "delete"), ("n", "keep")],
            Some(Edit::Quit(names)) => {
                let text = format!("Stop {} and quit?", names.join(", "));
                lines.extend(wrap(&text, width).into_iter().map(|l| vec![(l, ui::bad().add_modifier(Modifier::BOLD))]));
                &[("y", "quit"), ("n", "stay")]
            }
            None if self.focus == Focus::List => &[
                ("↵", "open"),
                ("n", "new"),
                ("v", "split →"),
                ("s", "split ↓"),
                ("r", "rename"),
                ("x", "close"),
                ("d", "delete"),
                ("p", "project"),
                ("⇧↑↓", "move"),
                ("q", "quit"),
            ],
            None => &[("alt ↑↓", "switch"), ("alt ←", "menu")],
        };
        let entry = |(k, label): &(&str, &str)| vec![(k.to_string(), ui::strong()), (format!(" {label}"), ui::dim())];
        let col = width / 2;
        let two = keys.len() > 3 && keys.iter().all(|(k, l)| k.width() + 1 + l.width() < col);
        if two {
            for pair in keys.chunks(2) {
                let mut line = entry(&pair[0]);
                if let Some(second) = pair.get(1) {
                    let used = pair[0].0.width() + 1 + pair[0].1.width();
                    line.push((" ".repeat(col - used), Style::new()));
                    line.extend(entry(second));
                }
                lines.push(line);
            }
        } else {
            lines.extend(keys.iter().map(entry));
        }
        lines
    }

    // ---- the terminals ----

    /// Draws the shown session; returns the title rows drawn.
    fn draw_main(&mut self, f: &mut Frame, area: Rect) -> Vec<Rect> {
        if area.width == 0 || area.height == 0 {
            return Vec::new();
        }
        let Some(tab) = self.core.state().active_tab().cloned() else {
            self.draw_empty(f, area);
            return vec![Rect { height: 1, ..area }];
        };
        let layout = match &self.resizing {
            Some((t, l)) if *t == tab.id => l.clone(),
            _ => tab.layout.clone(),
        };
        let arranged = panes::arrange(&layout, area);
        let focused = self.focused_pane();
        for placed in &arranged.panes {
            self.draw_pane(f, *placed, &tab, Some(placed.pane) == focused);
        }
        let headers: Vec<Rect> = arranged.panes.iter().map(|p| p.title()).collect();
        for d in arranged.dividers.iter().filter(|d| d.axis == Axis::Vertical) {
            vline(f.buffer_mut(), d.at, d.split.y, d.split.bottom(), &headers);
        }
        self.hits.tab = Some(tab.id);
        self.hits.panes = arranged.panes;
        self.hits.dividers = arranged.dividers;
        headers
    }

    fn draw_pane(&mut self, f: &mut Frame, placed: Placed, tab: &Tab, focused: bool) {
        if placed.area.width == 0 || placed.area.height == 0 {
            return;
        }
        let handle = self.core.terminal(placed.pane);
        let active = focused && self.keyboard_pane().is_some();
        let content = placed.content();
        if let Some(h) = &handle
            && content.width > 0
            && content.height > 0
        {
            // While a divider is dragged the terminals keep their size: each
            // resize makes Windows' console repaint, so they resize once, on release.
            if self.resizing.is_none() {
                h.resize(GridSize { cols: content.width, rows: content.height, ..h.size() });
            }
            let cursor = h.with_content(|c| {
                paint(f.buffer_mut(), content, c);
                c.cursor
            });
            if active
                && let Some(cur) = cursor
                && (cur.col as u16) < content.width
                && (cur.row as u16) < content.height
            {
                f.set_cursor_position(Position { x: content.x + cur.col as u16, y: content.y + cur.row as u16 });
                self.cursor_shape = Some(cur.shape);
            }
        }

        let s = self.core.state();
        let dir: PathBuf = s.pane(placed.pane).and_then(|p| p.cwd.clone()).unwrap_or_else(|| s.tab_working_dir(tab));
        let machine = self.core.tab_machine(tab.id);
        let (place, branch) = match &machine {
            // As scp takes it; the branch there isn't asked for while drawing.
            termroost_core::Machine::Remote(host) => {
                (format!("{}:{}", host.name(), folders::pretty(&dir, host.home().as_deref())), None)
            }
            termroost_core::Machine::Local => (folders::pretty(&dir, self.home.as_deref()), self.branch(&dir)),
        };
        let title = handle.as_ref().and_then(|h| h.title()).and_then(|t| telling(&t).map(str::to_owned));
        let exited = handle.as_ref().and_then(|h| h.exit_status());

        let mut right: Vec<(String, Style)> = Vec::new();
        if let Some(code) = exited {
            let code = code.filter(|c| *c != 0).map(|c| format!(" {c}")).unwrap_or_default();
            right.push((format!("exited{code}"), ui::bad()));
            right.push((" · ↵ restart".into(), ui::dim()));
        } else {
            if title.is_some() {
                right.push((place.clone(), ui::dim()));
            }
            if let Some(b) = branch {
                let sep = if title.is_some() { "  " } else { "" };
                right.push((format!("{sep}{b}"), ui::dim()));
            }
        }
        // A folder keeps its end when cut: that's the part that tells folders apart.
        let left = match title {
            Some(t) => Left::Title(t),
            None => Left::Folder(place),
        };
        let (line_style, left_style) = if active {
            (ui::accent(), ui::accent().add_modifier(Modifier::BOLD))
        } else if focused {
            (ui::dim(), ui::strong())
        } else {
            (ui::dim(), ui::dim())
        };
        title_row(f.buffer_mut(), placed.title(), &left, left_style, &right, line_style);
    }

    fn draw_empty(&mut self, f: &mut Frame, area: Rect) {
        let buf = f.buffer_mut();
        for x in area.left()..area.right() {
            put(buf, x, area.y, "─", ui::dim(), 1);
        }
        let lines: Vec<Vec<(&str, Style)>> = if self.core.state().projects.is_empty() {
            vec![vec![("Terminal Rooster", ui::strong())], vec![], vec![("Pick a folder to start.", ui::dim())]]
        } else {
            vec![vec![("n", ui::strong()), ("  new session", ui::dim())]]
        };
        let top = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
        for (i, line) in lines.iter().enumerate() {
            let width: usize = line.iter().map(|(t, _)| t.width()).sum();
            let mut x = area.x + area.width.saturating_sub(width as u16) / 2;
            for (text, style) in line {
                x = buf.set_stringn(x, top + i as u16, text, (area.right() - x) as usize, *style).0;
            }
        }
    }

    /// The git branch of a folder, looked up again every few seconds.
    fn branch(&mut self, dir: &Path) -> Option<String> {
        if let Some((at, branch)) = self.branches.get(dir)
            && at.elapsed() < Duration::from_secs(3)
        {
            return branch.clone();
        }
        let branch = git::Status::of(dir).and_then(|s| s.head).map(|h| h.to_string());
        self.branches.insert(dir.to_path_buf(), (Instant::now(), branch.clone()));
        branch
    }

    /// The outer terminal's title: where we are, marked while something wants attention.
    fn update_title(&mut self) {
        let s = self.core.state();
        let mut title = match s.active_tab() {
            Some(t) => {
                let project = s.project(t.project).map(|p| p.name.as_str()).unwrap_or_default();
                format!("{project} · {}", t.title)
            }
            None => "Terminal Rooster".to_owned(),
        };
        if !self.alerts.is_empty() {
            title.insert_str(0, "● ");
        }
        title.retain(|c| !c.is_control());
        if title != self.title {
            self.out.push(format!("\x1b]2;{title}\x07"));
            self.title = title;
        }
    }
}

/// A program's title, unless it only names a program file (Windows' console
/// titles itself `C:\…\powershell.exe`), which says less than the folder.
fn telling(title: &str) -> Option<&str> {
    let t = title.trim();
    let exe = t.len() >= 4 && t.get(t.len() - 4..).is_some_and(|ext| ext.eq_ignore_ascii_case(".exe"));
    (!t.is_empty() && !exe).then_some(t)
}

/// How much an alert asks for the user: needing them beats failing beats being done.
fn urgency(alert: &Alert) -> u8 {
    match alert {
        Alert::Agent { event: AgentEvent::Done, .. } => 1,
        Alert::Agent { event: AgentEvent::Failed, .. } | Alert::CommandFailed { .. } => 2,
        Alert::Agent { .. } | Alert::Notification { .. } => 3,
    }
}

/// Copies a terminal's cells into the buffer.
fn paint(buf: &mut Buffer, area: Rect, c: &RenderContent) {
    let rows = c.rows.min(area.height as usize);
    let cols = c.cols.min(area.width as usize);
    for row in 0..rows {
        for (col, cell) in c.row(row).iter().enumerate().take(cols) {
            if cell.spacer {
                continue;
            }
            let Some(out) = buf.cell_mut((area.x + col as u16, area.y + row as u16)) else { continue };
            let mut style = colors::cell_style(cell.fg, cell.bg);
            // Hidden text, or a wide character cut by the edge.
            if cell.fg == cell.bg || (cell.wide && col + 1 >= cols) {
                out.set_char(' ');
            } else if let Some(zw) = &cell.zerowidth {
                let mut symbol = String::from(cell.ch);
                symbol.extend(zw.iter());
                out.set_symbol(&symbol);
            } else {
                out.set_char(cell.ch);
            }
            let flags = [
                (cell.style.bold, Modifier::BOLD),
                (cell.style.italic, Modifier::ITALIC),
                (cell.style.underline, Modifier::UNDERLINED),
                (cell.style.strikeout, Modifier::CROSSED_OUT),
            ];
            for (on, m) in flags {
                if on {
                    style = style.add_modifier(m);
                }
            }
            if cell.selected {
                style = if style.add_modifier.contains(Modifier::REVERSED) {
                    style.remove_modifier(Modifier::REVERSED)
                } else {
                    style.add_modifier(Modifier::REVERSED)
                };
            }
            out.set_style(style);
        }
    }
}

/// What a title row starts with.
enum Left {
    /// The program's title, cut at its end.
    Title(String),
    /// A folder, cut at its start.
    Folder(String),
}

/// A terminal's title row: `── left ───────── right ─`.
fn title_row(buf: &mut Buffer, row: Rect, left: &Left, left_style: Style, right: &[(String, Style)], line: Style) {
    if row.width == 0 {
        return;
    }
    for x in row.left()..row.right() {
        put(buf, x, row.y, "─", line, 1);
    }
    let w = row.width as usize;
    let right_w = right_width(right) as usize + 2;
    let (Left::Title(text) | Left::Folder(text)) = left;
    // The left side comes first; the right side goes when both don't fit.
    let show_right = right_w > 2 && w >= right_w + 6 + text.width().min(12);
    let left_room = w.saturating_sub(4 + if show_right { right_w + 1 } else { 0 });
    if left_room > 0 {
        let cut = match left {
            Left::Title(t) => fit(t, left_room),
            Left::Folder(f) => fit_start(f, left_room),
        };
        put(buf, row.x + 1, row.y, &format!(" {cut} "), left_style, (w - 1) as u16);
    }
    if show_right {
        let mut x = row.right() - 1 - right_w as u16;
        x = buf.set_stringn(x, row.y, " ", 1, line).0;
        for (text, style) in right {
            x = buf.set_stringn(x, row.y, text, (row.right() - x) as usize, *style).0;
        }
        put(buf, x, row.y, " ", line, 1);
    }
}

fn right_width(right: &[(String, Style)]) -> u16 {
    right.iter().map(|(t, _)| t.width()).sum::<usize>() as u16
}

/// A vertical line, joined to the title rows beside it.
fn vline(buf: &mut Buffer, x: u16, top: u16, bottom: u16, headers: &[Rect]) {
    let at = |x: Option<u16>, y: u16| x.is_some_and(|x| headers.iter().any(|h| h.contains(Position { x, y })));
    for y in top..bottom {
        let symbol = match (at(x.checked_sub(1), y), at(x.checked_add(1), y)) {
            (true, true) if y == top => "┬",
            (true, true) => "┼",
            (true, false) => "┤",
            (false, true) => "├",
            (false, false) => "│",
        };
        put(buf, x, y, symbol, ui::dim(), 1);
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max: u16) {
    if buf.area.contains(Position { x, y }) {
        buf.set_stringn(x, y, text, max as usize, style);
    }
}

/// `text` cut to `width` columns, with an ellipsis if it was cut.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw + 1 > width {
            break;
        }
        out.push(c);
        used += cw;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// `text` cut to `width` columns from the left, with an ellipsis if it was cut.
fn fit_start(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut tail = Vec::new();
    let mut used = 1;
    for c in text.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > width {
            break;
        }
        tail.push(c);
        used += cw;
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// Greedy word wrap; words longer than a line are cut.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let needed = if line.is_empty() { word.width() } else { line.width() + 1 + word.width() };
        if needed > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&fit(word, width));
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_and_wrapping() {
        assert_eq!(fit("Session 1", 20), "Session 1");
        assert_eq!(fit("a long session name", 8), "a long …");
        assert_eq!(fit_start(r"~\projects\docs", 8), r"…ts\docs");
        assert_eq!(fit_start("docs", 8), "docs");
        assert_eq!(wrap("Stop claude, cargo and quit?", 14), vec!["Stop claude,", "cargo and", "quit?"]);
    }

    fn row_text(width: u16, left: Left, right: &[&str]) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        let right: Vec<(String, Style)> = right.iter().map(|t| (t.to_string(), Style::new())).collect();
        title_row(&mut buf, Rect::new(0, 0, width, 1), &left, Style::new(), &right, Style::new());
        (0..width).map(|x| buf[(x, 0)].symbol().to_owned()).collect()
    }

    #[test]
    fn title_rows() {
        let claude = || Left::Title("claude".into());
        assert_eq!(row_text(40, claude(), &["~/projects/api", "  main"]), "─ claude ──────── ~/projects/api  main ─");
        // Short of room, the title stays and the rest goes.
        assert_eq!(row_text(30, claude(), &["~/projects/api", "  main"]), format!("─ claude {}", "─".repeat(21)));
        // A folder loses its start.
        assert_eq!(row_text(14, Left::Folder("~/projects/api".into()), &["main"]), "─ …jects/api ─");
    }
}
