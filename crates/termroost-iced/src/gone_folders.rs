//! Session folders deleted outside Terminal Rooster: by hand, or by a
//! program in a terminal (`git worktree remove`). Their sessions stay, marked
//! in the sidebar and the header, and a banner says so once. Removing a
//! worktree from the Worktrees dialog closes its sessions first, so it is
//! never reported here.

use std::collections::HashSet;

use iced::Element;
use iced::widget::tooltip;
use termroost_core::Tab;

use crate::app::{App, Message, Place};
use crate::sidebar::display_dir;
use crate::theme::Tone;
use crate::ui::{self, tip};

/// A folder found deleted, for the banner.
struct Gone {
    name: String,
    /// It was a linked git worktree, when last seen.
    worktree: bool,
    /// Its sessions, with their project when it isn't the one on screen.
    sessions: Vec<String>,
}

impl App {
    /// Takes the session folders the last read found missing; the ones newly
    /// missing get a banner.
    pub(crate) fn folders_gone(&mut self, found: Vec<Place>) {
        let s = self.core.state();
        let places: Vec<(Place, &Tab)> = s.tabs.iter().map(|t| (self.tab_place(t), t)).collect();
        let sessions_in = |dir: &Place| places.iter().filter(|(d, _)| d == dir).map(|(_, t)| *t).collect::<Vec<_>>();
        // Only folders sessions still work in: one closed meanwhile (such as
        // by removing its worktree in the app) doesn't count.
        let found: HashSet<Place> = found.into_iter().filter(|d| places.iter().any(|(p, _)| p == d)).collect();
        let mut new: Vec<&Place> = found.iter().filter(|d| !self.gone.contains(*d)).collect();
        new.sort();
        let new: Vec<Gone> = new
            .into_iter()
            .map(|place| Gone {
                name: place.1.file_name().map_or_else(|| display_dir(&place.1), |n| n.to_string_lossy().into_owned()),
                // As the last read before it went saw it.
                worktree: self
                    .git
                    .get(place)
                    .and_then(Option::as_ref)
                    .is_some_and(|g| g.repo.is_linked() && g.repo.worktree == place.1),
                sessions: sessions_in(place)
                    .into_iter()
                    .map(|t| match s.project(t.project) {
                        Some(p) if s.selected_project != Some(t.project) => format!("{} ({})", t.title, p.name),
                        _ => t.title.clone(),
                    })
                    .collect(),
            })
            .collect();
        if !new.is_empty() {
            for g in &new {
                tracing::info!(folder = %g.name, sessions = g.sessions.len(), "a session folder was deleted");
            }
            self.notice = Some(notice(&new));
        }
        self.gone = found;
    }

    /// A warning that `tab`'s folder was deleted, if it was.
    pub(crate) fn gone_badge(&self, tab: &Tab) -> Option<Element<'_, Message>> {
        let place = self.tab_place(tab);
        let dir = place.1.clone();
        self.gone.contains(&place).then(|| {
            tip(
                ui::badge("Folder deleted", Tone::Warning),
                format!("{} was deleted outside {}", display_dir(&dir), crate::DISPLAY_NAME),
                tooltip::Position::Bottom,
            )
        })
    }
}

/// The banner for folders found deleted.
fn notice(gone: &[Gone]) -> String {
    let app = crate::DISPLAY_NAME;
    match gone {
        [g] => {
            let what = if g.worktree { "worktree" } else { "folder" };
            let stays = match g.sessions.as_slice() {
                [one] => format!("The session {one} stays open without it."),
                many => format!("The sessions {} stay open without it.", many.join(", ")),
            };
            format!("The {what} {} was deleted outside {app}. {stays}", g.name)
        }
        many => {
            let what = if many.iter().all(|g| g.worktree) { "worktrees" } else { "folders" };
            let names: Vec<&str> = many.iter().map(|g| g.name.as_str()).collect();
            format!(
                "The {what} {} were deleted outside {app}. Their sessions stay open without them.",
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gone(name: &str, worktree: bool, sessions: &[&str]) -> Gone {
        Gone { name: name.into(), worktree, sessions: sessions.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn notices() {
        assert_eq!(
            notice(&[gone("TermRoost-remotes", true, &["agents"])]),
            "The worktree TermRoost-remotes was deleted outside Terminal Rooster. The session agents stays open \
             without it."
        );
        assert_eq!(
            notice(&[gone("scratch", false, &["agents", "logs (api)"])]),
            "The folder scratch was deleted outside Terminal Rooster. The sessions agents, logs (api) stay open \
             without it."
        );
        assert_eq!(
            notice(&[gone("a", true, &["x"]), gone("b", false, &["y"])]),
            "The folders a, b were deleted outside Terminal Rooster. Their sessions stay open without them."
        );
        assert_eq!(
            notice(&[gone("a", true, &["x"]), gone("b", true, &["y"])]),
            "The worktrees a, b were deleted outside Terminal Rooster. Their sessions stay open without them."
        );
    }
}
