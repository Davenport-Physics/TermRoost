//! The settings page: appearance (themes), terminal options and shortcuts.
//!
//! Each section is a heading over a card of rows: what a setting is on the
//! left, its control on the right, with a hairline between rows.

use std::path::PathBuf;

use iced::widget::{Column, button, column, container, pick_list, row, slider, space, text, text_input, tooltip};
use iced::{Alignment, Border, Color, Element, Length, Padding};
use termroost_core::session::shell::{self, ShellOption};
use termroost_core::session::{bundled_bash, bundled_nvim};

use crate::app::{App, Message, SettingsState};
use crate::color_picker;
use crate::editor::FileEditor;
use crate::icons::{Icon, icon};
use crate::keymap::{Action, Combo};
use crate::notify::{Category, Level};
use crate::sidebar::muted_text;
use crate::theme::{self, MONO, SEMIBOLD, TEXT, TEXT_SM, Tone};
use crate::themes::{ANSI_NAMES, ColorField, Hex, ThemeDef};
use crate::ui::{self, tip};

/// How wide the sections get on a wide window.
const MAX_WIDTH: f32 = 860.0;
/// Padding of a row in a section's card.
const ROW_PADDING: Padding = Padding { top: 14.0, right: 18.0, bottom: 14.0, left: 18.0 };

fn swatch<'a>(hex: Hex, size: f32) -> Element<'a, Message> {
    container(space())
        .width(size)
        .height(size)
        .style(move |t: &iced::Theme| container::Style {
            background: Some(hex.color().into()),
            border: Border { width: 1.0, color: theme::ink(t).scale_alpha(0.2), radius: 4.0.into() },
            ..container::Style::default()
        })
        .into()
}

/// A section: its heading, over a card holding `rows` with a hairline between each.
fn section<'a>(title: &'a str, rows: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let heading = container(text(title).size(TEXT).font(SEMIBOLD).style(ui::muted))
        .padding(Padding { left: 4.0, ..Padding::ZERO });
    let mut card = column![];
    for (i, r) in rows.into_iter().enumerate() {
        if i > 0 {
            card = card.push(ui::divider());
        }
        card = card.push(r);
    }
    column![heading, container(card).width(Length::Fill).style(theme::card)].spacing(10).into()
}

/// What a setting is: its name, and a line or two about it.
fn about<'a>(label: impl text::IntoFragment<'a>, help: Option<String>) -> Element<'a, Message> {
    let mut col = column![text(label).size(14)].spacing(3);
    if let Some(help) = help.filter(|h| !h.is_empty()) {
        col = col.push(text(help).size(TEXT_SM).style(ui::muted));
    }
    col.into()
}

/// A row of a section: what the setting is on the left, `control` on the right.
fn setting<'a>(
    label: impl text::IntoFragment<'a>,
    help: Option<String>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![container(about(label, help)).width(Length::Fill), control.into()]
        .spacing(24)
        .align_y(Alignment::Center)
        .padding(ROW_PADDING)
        .into()
}

/// A row of a section whose control needs the whole width: it goes under the label.
fn stacked<'a>(
    label: impl text::IntoFragment<'a>,
    help: Option<String>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![about(label, help), control.into()].spacing(10).padding(ROW_PADDING).into()
}

/// A row of a section holding anything else.
fn block<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content).padding(ROW_PADDING).width(Length::Fill).into()
}

/// A heading inside a section's card.
fn subheading<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(14).font(SEMIBOLD)
}

/// A row of small swatches summarizing a theme.
fn theme_strip<'a>(t: &ThemeDef) -> Element<'a, Message> {
    let colors = [
        t.ui.background,
        t.ui.text,
        t.ui.primary,
        t.terminal.background,
        t.terminal.foreground,
        t.terminal.ansi[1],
        t.terminal.ansi[2],
        t.terminal.ansi[3],
        t.terminal.ansi[4],
        t.terminal.ansi[5],
    ];
    row(colors.into_iter().map(|c| swatch(c, 14.0))).spacing(3).into()
}

impl App {
    pub(crate) fn view_settings(&self) -> Element<'_, Message> {
        let Some(st) = &self.settings else { return space().into() };
        let back = button(
            row![icon(Icon::ChevronLeft, 14.0, theme::ink), text("Back").size(TEXT)]
                .spacing(4)
                .align_y(Alignment::Center),
        )
        .padding([6, 10])
        .style(theme::button_ghost)
        .on_press(Message::CloseSettings);
        let header = container(
            row![
                back,
                column![
                    text("Settings").size(theme::TEXT_PAGE).font(SEMIBOLD),
                    text("How Terminal Rooster looks, and how its terminals, editor and alerts behave")
                        .size(TEXT)
                        .style(ui::muted),
                ]
                .spacing(2),
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        )
        .padding([16, 24])
        .width(Length::Fill)
        .style(theme::side_column);

        let body = column![
            self.appearance_section(st),
            self.terminal_section(st),
            self.editor_section(st),
            section("Languages", self.languages_rows()),
            self.notifications_section(),
            self.integrations_section(),
            self.shortcuts_section(st),
            self.folder_buttons(st),
        ]
        .spacing(32)
        .max_width(MAX_WIDTH);

        column![
            header,
            ui::divider(),
            ui::scroll(container(body).padding([28, 32]).center_x(Length::Fill)).height(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// Buttons that open the folders Terminal Rooster keeps its files in,
    /// each folder's path in its tooltip.
    fn folder_buttons<'a>(&'a self, st: &'a SettingsState) -> Element<'a, Message> {
        let data = self.core.data_dir().to_path_buf();
        let logs = crate::logging::log_dir(&data);
        let open = |label: &'a str, about: String, dir: PathBuf| {
            let button =
                ui::small_with_icon(Icon::Open, label, theme::button_secondary).on_press(Message::OpenFolder(dir));
            tip(button, about, tooltip::Position::Top)
        };
        let data_about = format!("{}\n{} in it holds everything", data.display(), termroost_core::paths::DB_FILE);
        let buttons =
            row![open("Data folder", data_about, data), open("Logs folder", logs.display().to_string(), logs)]
                .spacing(8);
        let mut col = column![buttons].spacing(8);
        if let Some(e) = &st.folder_error {
            col = col.push(ui::note_text(Tone::Danger, e));
        }
        container(col).padding(Padding { left: 4.0, ..Padding::ZERO }).into()
    }

    fn appearance_section<'a>(&'a self, st: &'a SettingsState) -> Element<'a, Message> {
        let follow = self.prefs.follow_system;
        let tone = if self.system_is_dark() { "dark" } else { "light" };
        let note = if follow {
            format!(
                "Your system is {tone}. Light mode uses “{}” and dark mode uses “{}”. Using a theme makes it the \
                 choice for its mode.",
                self.prefs.light_theme, self.prefs.dark_theme
            )
        } else {
            "The theme you use stays on regardless of the system setting.".to_owned()
        };
        let active = self.active_theme().name;
        let list = Column::with_children(self.themes.all().map(|t| self.theme_row(t, st, &active))).spacing(8);

        let mut themes = column![
            subheading("Interface theme"),
            list,
            muted_text("Built-in themes can't be changed. Duplicate one to make your own, then edit its colors.")
                .size(TEXT_SM),
        ]
        .spacing(12);
        if let Some(t) = st.editing.as_deref().and_then(|n| self.themes.find(n)) {
            themes = themes.push(self.theme_editor(st, t));
        }
        section(
            "Appearance",
            vec![
                setting(
                    "Match the system's light/dark setting",
                    Some(note),
                    ui::switch(follow).on_toggle(Message::FollowSystem),
                ),
                block(themes),
                block(self.terminal_colors()),
            ],
        )
    }

    /// Terminal colors can follow the interface theme or come from any theme,
    /// e.g. dark terminals under a light interface.
    fn terminal_colors(&self) -> Element<'_, Message> {
        let chosen = self.prefs.terminal_theme.as_deref();
        let matching = self.active_theme();
        let mut cards = vec![terminal_card(
            "Match interface".to_owned(),
            &matching,
            chosen.is_none(),
            Message::TerminalTheme(None),
        )];
        cards.extend(self.themes.all().map(|t| {
            terminal_card(
                t.name.clone(),
                t,
                chosen == Some(t.name.as_str()),
                Message::TerminalTheme(Some(t.name.clone())),
            )
        }));
        column![
            column![
                subheading("Terminal colors"),
                muted_text(
                    "Terminals can use another theme's colors, for example dark terminals with a light interface."
                )
                .size(TEXT_SM),
            ]
            .spacing(3),
            row(cards).spacing(10).wrap().vertical_spacing(10),
        ]
        .spacing(12)
        .into()
    }

    fn theme_row<'a>(&'a self, t: &'a ThemeDef, st: &'a SettingsState, active: &str) -> Element<'a, Message> {
        let builtin = self.themes.is_builtin(&t.name);
        let in_use = t.name == active;
        let mut tags = vec![if builtin { "built-in" } else { "custom" }, if t.is_dark() { "dark" } else { "light" }];
        if self.prefs.follow_system {
            if t.name == self.prefs.light_theme {
                tags.push("light mode");
            }
            if t.name == self.prefs.dark_theme {
                tags.push("dark mode");
            }
        }
        let editing = st.editing.as_deref() == Some(t.name.as_str());
        let mut actions = row![].spacing(6).align_y(Alignment::Center);
        actions = actions.push(if in_use {
            ui::badge("In use", Tone::Accent)
        } else {
            ui::small("Use", theme::button_primary).on_press(Message::UseTheme(t.name.clone())).into()
        });
        if !builtin {
            actions = actions.push(
                ui::small(if editing { "Editing" } else { "Edit" }, theme::button_secondary)
                    .on_press_maybe((!editing).then(|| Message::EditTheme(t.name.clone()))),
            );
        }
        actions = actions
            .push(ui::small("Duplicate", theme::button_secondary).on_press(Message::DuplicateTheme(t.name.clone())));
        if !builtin {
            actions = actions.push(
                ui::small("Delete", theme::button_link(Tone::Danger)).on_press(Message::DeleteTheme(t.name.clone())),
            );
        }
        container(
            row![
                theme_strip(t),
                column![text(&t.name).size(14), muted_text(tags.join(" · "))].spacing(2).width(Length::Fill),
                actions,
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        )
        .padding([10, 12])
        .style(theme::choice(in_use || editing))
        .into()
    }

    fn color_input<'a>(&self, st: &'a SettingsState, t: &ThemeDef, field: ColorField) -> Element<'a, Message> {
        let value = st.inputs.get(&field).cloned().unwrap_or_else(|| t.get(field).to_string());
        let valid = Hex::parse(&value).is_some();
        let open = st.picker.is_some_and(|p| p.field == field);
        row![
            color_picker::field_swatch(t.get(field), open, Message::PickColor((!open).then_some(field))),
            text_input("#rrggbb", &value)
                .on_input(move |v| Message::ThemeColor(field, v))
                .font(MONO)
                .size(12)
                .padding([4, 8])
                .width(92)
                .style(move |th: &iced::Theme, status| {
                    let mut s = theme::input(th, status);
                    if !valid {
                        s.border.color = th.extended_palette().danger.base.color;
                    }
                    s
                }),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    }

    fn theme_editor<'a>(&'a self, st: &'a SettingsState, t: &'a ThemeDef) -> Element<'a, Message> {
        let labeled = |label: &'a str, field: ColorField| -> Element<'a, Message> {
            row![text(label).size(TEXT).width(100), self.color_input(st, t, field)].align_y(Alignment::Center).into()
        };
        let ui_colors = Column::with_children(ColorField::UI.iter().map(|(f, l)| labeled(l, *f))).spacing(6);
        let term = Column::with_children(ColorField::TERMINAL.iter().map(|(f, l)| labeled(l, *f))).spacing(6);
        let ansi_row = |bright: bool| -> Element<'a, Message> {
            row((0..8u8).map(|i| {
                let idx = i + if bright { 8 } else { 0 };
                column![muted_text(ANSI_NAMES[i as usize]), self.color_input(st, t, ColorField::Ansi(idx))]
                    .spacing(3)
                    .into()
            }))
            .spacing(8)
            .wrap()
            .vertical_spacing(8)
            .into()
        };

        let mut name_col = column![
            row![
                text("Name").size(TEXT).width(100),
                ui::input("Theme name", &st.name_input).on_input(Message::ThemeNameInput).width(260),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(6);
        if let Some(err) = &st.name_error {
            name_col = name_col.push(ui::note_text(Tone::Danger, err));
        }

        // The picker opens under the group holding the color being picked.
        let picker_after = |group: usize| -> Option<Element<'a, Message>> {
            let p = st.picker.as_ref()?;
            let at = match p.field {
                ColorField::Ansi(i) if i < 8 => 1,
                ColorField::Ansi(_) => 2,
                _ => 0,
            };
            (at == group).then(|| color_picker::panel(p, t))
        };
        let group_title = |label: &'a str| text(label).size(TEXT).font(SEMIBOLD);

        let editor = column![
            column![
                subheading(format!("Editing “{}”", t.name)).size(15),
                muted_text(
                    "Changes apply immediately and are saved automatically. Type a #rrggbb color, or click a color's \
                     square to choose from a palette."
                )
                .size(TEXT_SM),
            ]
            .spacing(3),
            name_col,
            row![
                column![group_title("Interface"), ui_colors].spacing(8),
                column![group_title("Terminal"), term].spacing(8),
            ]
            .spacing(48),
        ]
        .push(picker_after(0))
        .push(group_title("Terminal colors (ANSI)"))
        .push(muted_text("Normal"))
        .push(ansi_row(false))
        .push(picker_after(1))
        .push(muted_text("Bright"))
        .push(ansi_row(true))
        .push(picker_after(2))
        .push(group_title("Preview"))
        .push(preview(t))
        .spacing(12);
        container(editor).padding(16).width(Length::Fill).style(theme::list_box).into()
    }

    fn notifications_section(&self) -> Element<'_, Message> {
        let level_row = |category: Category, label: &'static str, help: &'static str| -> Element<'_, Message> {
            let current = self.prefs.level(category);
            let choices = Level::ALL
                .into_iter()
                .map(|level| (level.label().to_owned(), level == current, Message::NotifyLevel(category, level)));
            setting(label, Some(help.to_owned()), ui::segmented(choices))
        };
        let integration = self.core.state().settings.shell_integration;
        section(
            "Notifications",
            vec![
                level_row(
                    Category::Agent,
                    "Coding agents",
                    "Claude Code, Codex, Grok or Antigravity finishes its turn or waits for you. Programs that ask for \
                     a notification count too.",
                ),
                level_row(
                    Category::Error,
                    "Failed commands",
                    "A command exits with an error status, or a terminal's shell does.",
                ),
                setting(
                    "How alerts show",
                    Some(
                        "Subtle marks the terminal, its session and its project until you look at the terminal. \
                         Prominent also pops up a note when the terminal is out of view, and a desktop notification \
                         with a flashing taskbar entry while Terminal Rooster is in the background. The terminal you \
                         are looking at never notifies."
                            .to_owned(),
                    ),
                    ui::small("Send a test notification", theme::button_secondary).on_press(Message::TestNotification),
                ),
                setting(
                    "Shell integration for bash and PowerShell",
                    Some(
                        "Gives each terminal its own command history, and lets Terminal Rooster see how each \
                         command ends, which failed commands need. It is loaded \
                         when a terminal starts, from Terminal Rooster's own folder; your startup files are not \
                         changed. Other shells can report a command's status themselves with OSC 133;D. Applies to \
                         terminals started from now on."
                            .to_owned(),
                    ),
                    ui::switch(integration).on_toggle(Message::ShellIntegration),
                ),
                setting(
                    "Ask before quitting while terminals run programs",
                    None,
                    ui::switch(self.prefs.confirm_quit).on_toggle(Message::ConfirmQuitPref),
                ),
            ],
        )
    }

    /// What files from the Files tab open in, and the terminal editor's command.
    fn editor_section<'a>(&'a self, st: &'a SettingsState) -> Element<'a, Message> {
        let current = self.prefs.file_editor;
        let choices = [("Built-in editor", FileEditor::Builtin), ("Neovim or Vim", FileEditor::Terminal)]
            .map(|(label, value)| (label.to_owned(), current == value, Message::FileEditor(value)));
        let placeholder = match &st.editor_found {
            Some(p) => format!("Empty: {}", p.display()),
            None => "Empty: Neovim or Vim (none found)".to_owned(),
        };
        let mut rows = vec![
            setting(
                "Edit files with",
                Some("Files you open from the Files tab show in place of the session's terminals.".to_owned()),
                ui::segmented(choices),
            ),
            stacked(
                "Terminal editor command",
                Some(
                    "Neovim or Vim runs in a terminal; quitting it (:q) closes the file. A command set here runs with \
                     your own configuration. The file's path is added to the command. Leave it empty to use Neovim, \
                     or Vim when there is no Neovim. Ctrl+Shift shortcuts and Alt+arrows stay with Terminal Rooster; \
                     every other key goes to the editor."
                        .to_owned(),
                ),
                ui::input(&placeholder, &self.prefs.editor_command).on_input(Message::EditorCommand),
            ),
        ];
        rows.push(self.neovim_row(st));
        rows.push(setting(
            "Terminal Rooster's Neovim setup",
            Some(
                "Language servers (see Languages), completion and diagnostics. Off: Neovim runs your own \
                 configuration."
                    .to_owned(),
            ),
            ui::switch(st.nvim_setup).on_toggle(Message::NvimSetup),
        ));
        section("Editor", rows)
    }

    /// Getting Neovim: on Windows Terminal Rooster can install it for itself;
    /// elsewhere the system's package manager does. Nothing to do when the
    /// user's own Neovim is on PATH.
    fn neovim_row<'a>(&'a self, st: &'a SettingsState) -> Element<'a, Message> {
        if let Some(path) = &st.nvim_on_path {
            return setting("Neovim", Some(format!("Found at {}.", path.display())), space::horizontal().width(0));
        }
        if !bundled_nvim::SUPPORTED {
            let how = if cfg!(target_os = "macos") {
                "brew install neovim"
            } else {
                "your package manager, e.g. sudo apt install neovim, sudo dnf install neovim or sudo pacman -S neovim"
            };
            return setting(
                "Neovim",
                Some(format!("Neovim isn't installed. Install it with {how}, then reopen Settings.")),
                space::horizontal().width(0),
            );
        }
        let mb = bundled_nvim::download_size().div_ceil(1_000_000);
        let version = bundled_nvim::NVIM_VERSION;
        install_row(
            "Neovim",
            &self.nvim_install,
            bundled_nvim::installed().map(|exe| format!("Neovim {version} is installed in {}.", exe.display())),
            format!(
                "Neovim {version}: a {mb} MB download, unpacked into Terminal Rooster's own folder. Nothing else on the \
                 system changes."
            ),
            "Could not install Neovim",
            Message::InstallNvim,
        )
    }

    fn integrations_section(&self) -> Element<'_, Message> {
        let h = &self.codex_hook;
        let (status, tone, actions): (String, Tone, Vec<Element<'_, Message>>) = if !h.codex_found {
            ("Codex isn't set up on this computer".to_owned(), Tone::Neutral, vec![])
        } else if h.installed_in.is_none() {
            (
                "Not added".to_owned(),
                Tone::Neutral,
                vec![ui::small("Add hook", theme::button_primary).on_press(Message::AddCodexHook).into()],
            )
        } else {
            let remove = ui::small("Remove hook", theme::button_secondary).on_press(Message::RemoveCodexHook).into();
            match (h.trusted, self.core.codex_hook_last_report()) {
                (_, Some(at)) => {
                    let at: chrono::DateTime<chrono::Local> = at.into();
                    (format!("Active · last report at {}", at.format("%H:%M:%S")), Tone::Success, vec![remove])
                }
                // The trust entry may be for an earlier version of the hook (e.g.
                // TermRoost's), so offer to trust it again until a report shows it runs.
                (true, None) => (
                    "Added and trusted".to_owned(),
                    Tone::Success,
                    vec![
                        ui::small("Trust again…", theme::button_secondary).on_press(Message::TrustCodexHook).into(),
                        remove,
                    ],
                ),
                (false, None) => (
                    "Added, but not trusted in Codex yet".to_owned(),
                    Tone::Warning,
                    vec![
                        ui::small("Trust in Codex…", theme::button_primary).on_press(Message::TrustCodexHook).into(),
                        remove,
                    ],
                ),
            }
        };
        let about = column![
            row![text("Codex session hook").size(14), ui::badge(status, tone)].spacing(8).align_y(Alignment::Center),
            text("Lets Terminal Rooster resume the right Codex conversation in each terminal.")
                .size(TEXT_SM)
                .style(ui::muted),
        ]
        .spacing(4);
        let line = row![container(about).width(Length::Fill), row(actions).spacing(8)]
            .spacing(24)
            .align_y(Alignment::Center)
            .padding(ROW_PADDING);
        section("Integrations", vec![line.into()])
    }

    fn terminal_section<'a>(&'a self, st: &'a SettingsState) -> Element<'a, Message> {
        let platform = {
            let sh = shell::default_shell();
            std::iter::once(sh.program).chain(sh.args).collect::<Vec<_>>().join(" ")
        };
        let font_size = row![
            slider(8.0_f32..=32.0, self.prefs.font_size, Message::FontSize)
                .step(1.0_f32)
                .width(220)
                .style(theme::slider_track),
            text(format!("{} px", self.prefs.font_size)).size(TEXT).font(MONO).width(48),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        let shell_input = row![
            ui::input(&format!("platform default: {platform}"), &st.shell_input)
                .on_input(Message::ShellInput)
                .on_submit(Message::ShellCommit)
                .width(Length::Fill),
            self.shell_picker(&st.shell_input, &platform, Message::PickDefaultShell),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let mut rows = vec![
            setting("Font size", None, font_size),
            stacked(
                "Default shell",
                Some(
                    "Pick an installed shell, or type any command line. Used for new terminals in projects without \
                     their own shell. Typed changes are saved when you press Enter or leave settings."
                        .to_owned(),
                ),
                shell_input,
            ),
        ];
        if bundled_bash::SUPPORTED {
            rows.push(self.bundled_bash_row(&st.shell_input, &platform));
        }
        section("Terminal", rows)
    }

    /// Dropdown of the installed shells, showing the one `input` (or, when it
    /// is empty, `fallback`) runs. Hidden until detection has found some.
    pub(crate) fn shell_picker<'a>(
        &'a self,
        input: &str,
        fallback: &str,
        on_pick: fn(ShellOption) -> Message,
    ) -> Option<Element<'a, Message>> {
        if self.shells.is_empty() {
            return None;
        }
        let current = if input.trim().is_empty() { fallback } else { input };
        let picker = pick_list(self.shells.as_slice(), crate::app::shell_matching(&self.shells, current), on_pick)
            .placeholder("Installed shells…")
            .text_size(TEXT)
            .padding([6, 10])
            .width(200)
            .style(theme::dropdown)
            .menu_style(theme::dropdown_menu);
        Some(picker.into())
    }

    /// Windows: download Terminal Rooster's own bash, or say where it is.
    fn bundled_bash_row<'a>(&'a self, input: &str, platform: &str) -> Element<'a, Message> {
        use crate::app::shell_matching;

        let mb = bundled_bash::download_size() / 1_000_000;
        let version = bundled_bash::GIT_VERSION;
        let installed = bundled_bash::installed().map(|_| {
            let current = if input.trim().is_empty() { platform } else { input };
            let in_use = shell_matching(&self.shells, current).is_some_and(|o| o.name == bundled_bash::NAME);
            let dir = bundled_bash::install_dir().map(|d| d.display().to_string()).unwrap_or_default();
            if in_use {
                format!("Installed in {dir} and used for new terminals.")
            } else {
                format!("Installed in {dir}. Pick “{}” above to use it.", bundled_bash::NAME)
            }
        });
        install_row(
            bundled_bash::NAME,
            &self.bash_install,
            installed,
            format!(
                "Bash with coreutils, vim, less, ssh and git, from Git for Windows {version}: a {mb} MB download, a \
                 few hundred MB unpacked into Terminal Rooster's own folder. Nothing else on the system changes."
            ),
            "Could not install bash",
            Message::InstallBash,
        )
    }
}

/// A row for something Terminal Rooster can download for itself: where it
/// is once installed, how the download goes, or what it would be and a
/// button to get it.
fn install_row<'a>(
    name: &'a str,
    state: &crate::app::InstallState,
    installed: Option<String>,
    offer: String,
    failed: &str,
    install: Message,
) -> Element<'a, Message> {
    use crate::app::InstallState;

    match (state, installed) {
        (InstallState::Running(progress), _) => setting(name, Some(progress.to_string()), space::horizontal().width(0)),
        (_, Some(installed)) => setting(name, Some(installed), ui::badge("Installed", Tone::Success)),
        (InstallState::Failed(e), None) => stacked(
            name,
            Some(offer),
            row![
                container(ui::note_text(Tone::Danger, format!("{failed}: {e}"))).width(Length::Fill),
                ui::small("Try again", theme::button_secondary).on_press(install),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        ),
        (InstallState::Idle, None) => {
            setting(name, Some(offer), ui::small("Install and use", theme::button_primary).on_press(install))
        }
    }
}

/// A clickable miniature terminal in a theme's colors.
fn terminal_card<'a>(label: String, t: &ThemeDef, selected: bool, msg: Message) -> Element<'a, Message> {
    let term = t.terminal.clone();
    let line = |s: &'a str, c: Hex| text(s).font(MONO).size(12).color(c.color());
    let dots = row(term.ansi[..8].iter().map(|c| line("●", *c).into())).spacing(1);
    let screen = container(
        column![row![line("~ $ ", term.ansi[2]), line("claude", term.foreground)], line("✓ ready", term.ansi[4]), dots]
            .spacing(1),
    )
    .padding(8)
    .width(150)
    .style(move |_: &iced::Theme| container::Style {
        background: Some(term.background.color().into()),
        border: Border::default().rounded(theme::RADIUS_SM),
        ..container::Style::default()
    });
    let font = if selected { SEMIBOLD } else { theme::UI };
    button(column![screen, text(label).size(TEXT_SM).font(font)].spacing(6).align_x(Alignment::Center))
        .padding(5)
        .on_press(msg)
        .style(theme::choice_button(selected))
        .into()
}

/// Mock interface and terminal drawn with a theme's colors.
fn preview<'a>(t: &ThemeDef) -> Element<'a, Message> {
    let ui_colors = t.ui.clone();
    let chip = |label: &'a str, bg: Hex, fg: Hex| -> Element<'a, Message> {
        container(text(label).size(12).color(fg.color()))
            .padding([3, 8])
            .style(move |_: &iced::Theme| container::Style {
                background: Some(bg.color().into()),
                border: Border::default().rounded(theme::RADIUS_SM),
                ..container::Style::default()
            })
            .into()
    };
    let on = |c: Hex| if c.luminance() > 0.4 { Hex(0, 0, 0) } else { Hex(255, 255, 255) };
    let ui_mock = container(
        column![
            text("Interface").size(13).color(ui_colors.text.color()),
            text("Projects, sessions and tasks use these colors.").size(12).color(ui_colors.text.color()),
            row![
                chip("Accent", ui_colors.primary, on(ui_colors.primary)),
                chip("Success", ui_colors.success, on(ui_colors.success)),
                chip("Warning", ui_colors.warning, on(ui_colors.warning)),
                chip("Danger", ui_colors.danger, on(ui_colors.danger)),
            ]
            .spacing(6),
        ]
        .spacing(8),
    )
    .padding(12)
    .width(Length::FillPortion(2))
    .style(move |_: &iced::Theme| container::Style {
        background: Some(ui_colors.background.color().into()),
        border: Border { width: 1.0, color: Color { a: 0.2, ..ui_colors.text.color() }, radius: theme::RADIUS.into() },
        ..container::Style::default()
    });

    let term = t.terminal.clone();
    let line = |s: &'a str, c: Hex| text(s).font(MONO).size(13).color(c.color());
    let ansi_dots = row(term.ansi.iter().map(|c| line("██", *c).into())).spacing(0);
    let selection = container(line("selected text", term.foreground)).style(move |_: &iced::Theme| container::Style {
        background: Some(term.selection.color().into()),
        ..container::Style::default()
    });
    let cursor = container(line(" ", term.background)).style(move |_: &iced::Theme| container::Style {
        background: Some(term.cursor.color().into()),
        ..container::Style::default()
    });
    let term_mock = container(
        column![
            row![line("~/project $ ", term.ansi[2]), line("cargo test", term.foreground)],
            line("test result: ok. 27 passed", term.ansi[10]),
            line("warning: unused variable `x`", term.ansi[3]),
            line("error[E0425]: cannot find value", term.ansi[1]),
            row![line("● ", term.ansi[4]), line("Reading src/main.rs", term.foreground)],
            row![selection, line(" ", term.foreground), cursor],
            ansi_dots,
        ]
        .spacing(2),
    )
    .padding(12)
    .width(Length::FillPortion(3))
    .style(move |_: &iced::Theme| container::Style {
        background: Some(term.background.color().into()),
        border: Border { width: 1.0, color: Color { a: 0.2, ..term.foreground.color() }, radius: theme::RADIUS.into() },
        ..container::Style::default()
    });
    row![ui_mock, term_mock].spacing(12).into()
}

/// A shortcut as a key cap with a remove button.
fn key_chip<'a>(combo: Combo, remove: Message) -> Element<'a, Message> {
    let remove = button(icon(Icon::Close, 10.0, theme::muted)).padding(2).style(theme::button_ghost).on_press(remove);
    key_cap(row![text(combo.to_string()).font(MONO).size(12), remove].spacing(4).align_y(Alignment::Center), 3.0)
}

/// The key-cap frame around a shortcut.
fn key_cap<'a>(content: impl Into<Element<'a, Message>>, right: f32) -> Element<'a, Message> {
    container(content).padding(Padding { top: 2.0, bottom: 2.0, left: 8.0, right }).style(theme::key_cap).into()
}

impl App {
    fn shortcuts_section<'a>(&'a self, st: &'a SettingsState) -> Element<'a, Message> {
        let changed = !self.prefs.shortcuts.is_empty();
        let mut intro = column![
            row![
                text(
                    "Click + and press the keys for a new shortcut, or ✕ to remove one. Programs in terminals use \
                     plain Ctrl+letter keys such as Ctrl+C, so Ctrl+Shift, Alt and the function keys work best."
                )
                .size(TEXT_SM)
                .style(ui::muted)
                .width(Length::Fill),
                ui::small("Reset all", theme::button_secondary)
                    .on_press_maybe(changed.then_some(Message::ResetShortcuts(None))),
            ]
            .spacing(24)
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if let Some(note) = &st.shortcut_note {
            intro = intro.push(ui::note(Tone::Accent, text(note).size(TEXT_SM)));
        }
        let mut rows = vec![block(intro)];
        let mut group = "";
        let mut list = column![].spacing(2);
        for action in Action::ALL {
            if action.group() != group {
                if !group.is_empty() {
                    rows.push(block(list));
                    list = column![].spacing(2);
                }
                group = action.group();
                list = list.push(container(subheading(group)).padding(Padding { bottom: 6.0, ..Padding::ZERO }));
            }
            list = list.push(self.shortcut_row(st, action));
        }
        rows.push(block(list));
        let mut fixed =
            column![container(subheading("Fixed")).padding(Padding { bottom: 6.0, ..Padding::ZERO })].spacing(2);
        for (keys, what) in
            [("Right click", "Copy the selection, or paste"), ("Shift+Enter", "New line in Claude Code / Codex")]
        {
            fixed = fixed.push(
                row![text(what).size(TEXT).width(260), key_cap(text(keys).font(MONO).size(12), 8.0)]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .height(30),
            );
        }
        rows.push(block(fixed));
        section("Keyboard shortcuts", rows)
    }

    fn shortcut_row<'a>(&'a self, st: &'a SettingsState, action: Action) -> Element<'a, Message> {
        let mut keys = row(self.keymap.combos(action).iter().map(|&c| key_chip(c, Message::RemoveShortcut(action, c))))
            .spacing(6)
            .align_y(Alignment::Center);
        keys = keys.push(if st.recording == Some(action) {
            ui::small("Press the keys… (Esc cancels)", theme::button_primary)
                .on_press(Message::RecordShortcut(None))
                .into()
        } else {
            tip(
                button(icon(Icon::Plus, 12.0, theme::muted))
                    .padding(4)
                    .style(theme::button_ghost)
                    .on_press(Message::RecordShortcut(Some(action))),
                "Add a shortcut",
                tooltip::Position::Bottom,
            )
        });
        let reset: Element<'a, Message> = if self.prefs.shortcuts.contains_key(action.id()) {
            ui::small("Reset", theme::button_link(Tone::Neutral)).on_press(Message::ResetShortcuts(Some(action))).into()
        } else {
            space().into()
        };
        row![text(action.label()).size(TEXT).width(260), container(keys).width(Length::Fill), reset]
            .spacing(8)
            .align_y(Alignment::Center)
            .height(30)
            .into()
    }
}
