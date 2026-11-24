//! Language support: the language servers Terminal Rooster installs for
//! Neovim (`termroost_core::lsp`). Settings lists them; a file opened in
//! Neovim offers its language's, unless the user chose to install them all.

use std::collections::{BTreeMap, HashMap, HashSet};

use iced::widget::{Row, column, container, row, space, text};
use iced::{Alignment, Element, Length, Task};
use serde::{Deserialize, Serialize};
use termroost_core::lsp::{self, Package, State};

use crate::app::{App, Message};
use crate::theme::{self, TEXT_SM, Tone};
use crate::ui;

/// Whether to ask before installing a language's support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallMode {
    /// Offer it when a file of that language opens.
    #[default]
    Ask,
    /// Install every language's support (and updates) without asking.
    All,
}

/// An install that hasn't finished well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    Running,
    Failed(String),
}

/// What the app knows of language support.
#[derive(Debug, Default)]
pub struct Languages {
    /// Where each package stands; `None` until looked at.
    pub status: Option<BTreeMap<Package, State>>,
    detecting: bool,
    pub installs: HashMap<Package, Install>,
    /// Offers the user turned down, until Terminal Rooster restarts.
    declined: HashSet<Package>,
}

#[derive(Clone, Debug)]
pub enum LangMsg {
    Detected(BTreeMap<Package, State>),
    Install(Vec<Package>),
    Installed(Package, Result<(), String>),
    Decline(Vec<Package>),
    Mode(InstallMode),
}

impl Languages {
    fn state(&self, p: Package) -> Option<&State> {
        self.status.as_ref()?.get(&p)
    }

    /// The packages to install now that everything is to be: the missing
    /// and the outdated, unless being installed.
    fn to_install_all(&self) -> Vec<Package> {
        let Some(status) = &self.status else { return Vec::new() };
        let wanted = |s: &State| matches!(s, State::Missing | State::Installed { outdated: true });
        status.iter().filter(|(p, s)| wanted(s) && !self.installs.contains_key(p)).map(|(p, _)| *p).collect()
    }
}

impl App {
    pub(crate) fn update_languages(&mut self, msg: LangMsg) -> Task<Message> {
        match msg {
            LangMsg::Detected(status) => {
                self.languages.status = Some(status);
                self.languages.detecting = false;
                if self.prefs.install_languages == InstallMode::All {
                    return self.install_languages(self.languages.to_install_all());
                }
            }
            LangMsg::Install(packages) => return self.install_languages(packages),
            LangMsg::Installed(p, Ok(())) => {
                self.languages.installs.remove(&p);
                return self.detect_languages();
            }
            LangMsg::Installed(p, Err(e)) => {
                tracing::warn!(package = ?p, error = %e, "installing language support failed");
                self.languages.installs.insert(p, Install::Failed(e));
            }
            LangMsg::Decline(packages) => self.languages.declined.extend(packages),
            LangMsg::Mode(mode) => {
                self.prefs.install_languages = mode;
                self.mark_prefs();
                if mode == InstallMode::All {
                    return self.install_languages(self.languages.to_install_all());
                }
            }
        }
        Task::none()
    }

    /// Looks at where each package stands, on a thread (it asks Node and
    /// .NET for their versions).
    pub(crate) fn detect_languages(&mut self) -> Task<Message> {
        if self.languages.detecting {
            return Task::none();
        }
        self.languages.detecting = true;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(lsp::status());
        });
        Task::perform(async move { rx.recv().await.unwrap_or_default() }, |s| Message::Languages(LangMsg::Detected(s)))
    }

    /// A file of `packages`' languages opened in Neovim: what they stand at
    /// is needed for its offer, and with everything to be installed, any
    /// that's missing installs now.
    pub(crate) fn languages_wanted(&mut self, packages: &[Package]) -> Task<Message> {
        if packages.is_empty() {
            return Task::none();
        }
        let missing = |p: &Package| matches!(self.languages.state(*p), None | Some(State::Missing));
        if packages.iter().any(missing) { self.detect_languages() } else { Task::none() }
    }

    fn install_languages(&mut self, packages: Vec<Package>) -> Task<Message> {
        let mut tasks = Vec::new();
        for p in packages {
            if self.languages.installs.get(&p) == Some(&Install::Running) {
                continue;
            }
            self.languages.installs.insert(p, Install::Running);
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(lsp::install(p).map_err(|e| e.to_string()));
            });
            tasks.push(Task::perform(
                async move { rx.recv().await.unwrap_or_else(|_| Err("the install stopped".into())) },
                move |r| Message::Languages(LangMsg::Installed(p, r)),
            ));
        }
        Task::batch(tasks)
    }

    /// Under a file open in Neovim: an offer to install its languages'
    /// support, or what installing it needs, or how it goes.
    pub(crate) fn language_banner<'a>(&'a self, packages: &[Package]) -> Element<'a, Message> {
        let l = &self.languages;
        let running: Vec<Package> =
            packages.iter().copied().filter(|p| l.installs.get(p) == Some(&Install::Running)).collect();
        let failed = packages.iter().find_map(|p| match l.installs.get(p) {
            Some(Install::Failed(e)) => Some((*p, e)),
            _ => None,
        });
        let open: Vec<Package> = packages
            .iter()
            .copied()
            .filter(|p| !l.declined.contains(p) && !l.installs.contains_key(p))
            .filter(|p| matches!(l.state(*p), Some(State::Missing | State::Needs(_))))
            .collect();
        let msg = |m: LangMsg| Message::Languages(m);
        if !running.is_empty() {
            return banner(format!("Installing {} support…", names(&running)), row![], Tone::Neutral);
        }
        if let Some((p, e)) = failed {
            let buttons = row![
                ui::small("Try again", theme::button_secondary).on_press(msg(LangMsg::Install(vec![p]))),
                ui::small("Not now", theme::button_ghost).on_press(msg(LangMsg::Decline(vec![p]))),
            ];
            return banner(format!("Could not install {} support: {e}", p.name()), buttons, Tone::Danger);
        }
        let installable: Vec<Package> =
            open.iter().copied().filter(|p| matches!(l.state(*p), Some(State::Missing))).collect();
        if !installable.is_empty() && self.prefs.install_languages == InstallMode::Ask {
            let buttons = row![
                ui::small("Install", theme::button_primary).on_press(msg(LangMsg::Install(installable.clone()))),
                ui::small("Not now", theme::button_ghost).on_press(msg(LangMsg::Decline(installable.clone()))),
            ];
            return banner(format!("Install {} support?", names(&installable)), buttons, Tone::Accent);
        }
        if let Some((p, needs)) = open.iter().find_map(|p| match l.state(*p) {
            Some(State::Needs(needs)) => Some((*p, *needs)),
            _ => None,
        }) {
            let ok = ui::small("OK", theme::button_ghost).on_press(msg(LangMsg::Decline(vec![p])));
            return banner(format!("{} support needs {needs}.", p.name()), row![ok], Tone::Neutral);
        }
        space::vertical().height(0).into()
    }

    /// The Languages section of the settings page.
    pub(crate) fn languages_rows(&self) -> Vec<Element<'_, Message>> {
        let mode = self.prefs.install_languages;
        let choices = [("Ask per language", InstallMode::Ask), ("Install all", InstallMode::All)]
            .map(|(label, m)| (label.to_owned(), mode == m, Message::Languages(LangMsg::Mode(m))));
        let mut rows = vec![language_row(
            "Language servers",
            "Completion, errors and go to definition in Neovim, installed into Terminal Rooster's own folder.",
            ui::segmented(choices),
        )];
        for p in Package::ALL {
            rows.push(self.package_row(p));
        }
        rows
    }

    fn package_row(&self, p: Package) -> Element<'_, Message> {
        let l = &self.languages;
        let install = |label: &'static str| -> Element<'_, Message> {
            ui::small(label, theme::button_secondary).on_press(Message::Languages(LangMsg::Install(vec![p]))).into()
        };
        let control: Element<'_, Message> = match (l.installs.get(&p), l.state(p)) {
            (Some(Install::Running), _) => text("Installing…").size(TEXT_SM).style(ui::muted).into(),
            (Some(Install::Failed(e)), _) => {
                let note = container(ui::note_text(Tone::Danger, e.clone())).max_width(360);
                row![note, install("Try again")].spacing(12).align_y(Alignment::Center).into()
            }
            (None, None) => text("…").size(TEXT_SM).style(ui::muted).into(),
            (None, Some(State::Installed { outdated: false })) => ui::badge("Installed", Tone::Success),
            (None, Some(State::Installed { outdated: true })) => install("Update"),
            (None, Some(State::Missing)) => install("Install"),
            (None, Some(State::Needs(needs))) => text(format!("Needs {needs}")).size(TEXT_SM).style(ui::muted).into(),
        };
        language_row(p.name(), p.servers(), control)
    }
}

/// A row of the Languages section: a name, a line under it, and `control`.
fn language_row<'a>(name: &'a str, about: &'a str, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let label = column![text(name).size(14), text(about).size(TEXT_SM).style(ui::muted)].spacing(3);
    row![container(label).width(Length::Fill), control.into()]
        .spacing(24)
        .align_y(Alignment::Center)
        .padding(iced::Padding { top: 14.0, right: 18.0, bottom: 14.0, left: 18.0 })
        .into()
}

/// The packages' names, for a sentence.
fn names(packages: &[Package]) -> String {
    let names: Vec<&str> = packages.iter().map(|p| p.name()).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn banner<'a>(message: String, buttons: Row<'a, Message>, tone: Tone) -> Element<'a, Message> {
    let line = row![text(message).size(13).width(Length::Fill), buttons.spacing(6)].align_y(Alignment::Center);
    container(ui::note(tone, line)).padding([6, 0]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_as_a_list() {
        assert_eq!(names(&[Package::CSharp]), "C#");
        assert_eq!(names(&[Package::TypeScript, Package::Tailwind]), "JavaScript and TypeScript, and Tailwind CSS");
    }

    #[test]
    fn installing_all_takes_the_missing_and_the_outdated() {
        let mut l = Languages::default();
        assert!(l.to_install_all().is_empty(), "nothing known yet");
        l.status = Some(BTreeMap::from([
            (Package::Rust, State::Installed { outdated: false }),
            (Package::CSharp, State::Missing),
            (Package::TypeScript, State::Installed { outdated: true }),
            (Package::Web, State::Needs("Node.js 18 or newer")),
            (Package::Tailwind, State::Missing),
        ]));
        l.installs.insert(Package::Tailwind, Install::Running);
        assert_eq!(l.to_install_all(), [Package::CSharp, Package::TypeScript]);
    }
}
