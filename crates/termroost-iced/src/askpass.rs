//! Questions git and ssh ask while they run (passwords, key passphrases, new
//! host keys; see `termroost_core::askpass`): shown one at a time, above
//! everything else, dialogs too, until answered.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use iced::widget::{column, operation};
use iced::{Element, Task};
use termroost_core::askpass::{Prompt, PromptKind};

use crate::app::{App, Message};
use crate::dialogs::{DialogParts, modal, para};
use crate::theme::Tone;
use crate::ui;

pub const ASKPASS_INPUT: &str = "askpass-input";
/// No widget has this id: focusing it takes the keyboard from all of them.
const NO_FIELD: &str = "askpass-no-field";

/// The same question asked again this soon after it was answered means the
/// answer wasn't accepted (ssh asks for a passphrase up to three times).
const RETRY_WINDOW: Duration = Duration::from_secs(60);

/// A question on screen, or waiting behind the one that is.
pub(crate) struct Ask {
    pub prompt: Prompt,
    pub input: String,
    /// The question came back right after it was answered.
    pub retry: bool,
}

#[derive(Default)]
pub(crate) struct Asks {
    pub queue: VecDeque<Ask>,
    /// The question answered last, and when.
    last: Option<(String, Instant)>,
}

impl Asks {
    /// Whether a question is on screen.
    pub fn open(&self) -> bool {
        !self.queue.is_empty()
    }
}

/// Hashable wrapper for where git's questions arrive.
pub(crate) struct Prompts(pub async_channel::Receiver<Prompt>);

impl Hash for Prompts {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "terminalrooster-askpass".hash(state);
    }
}

impl App {
    pub(crate) fn ask(&mut self, prompt: Prompt) -> Task<Message> {
        let retry = prompt.kind == PromptKind::Secret
            && self.asks.last.as_ref().is_some_and(|(text, at)| *text == prompt.text && at.elapsed() < RETRY_WINDOW);
        self.asks.queue.push_back(Ask { prompt, input: String::new(), retry });
        if self.asks.queue.len() == 1 { self.focus_ask() } else { Task::none() }
    }

    pub(crate) fn ask_input(&mut self, input: String) {
        if let Some(a) = self.asks.queue.front_mut() {
            a.input = input;
        }
    }

    /// Answers the question on screen (`ok`), or answers no / cancels it.
    pub(crate) fn answer_ask(&mut self, ok: bool) -> Task<Message> {
        let Some(mut a) = self.asks.queue.pop_front() else { return Task::none() };
        tracing::info!(ok, kind = ?a.prompt.kind, "git's question answered");
        if ok {
            a.prompt.answer(std::mem::take(&mut a.input));
            self.asks.last = Some((a.prompt.text.clone(), Instant::now()));
        } else {
            a.prompt.cancel();
            self.asks.last = None;
        }
        if self.asks.open() {
            self.focus_ask()
        } else if self.dialog.is_some() {
            Task::none()
        } else {
            self.focus_active()
        }
    }

    /// Gives the shown question's field the keyboard; a yes-or-no question
    /// has none, and takes the keyboard from everything else.
    pub(crate) fn focus_ask(&self) -> Task<Message> {
        match self.asks.queue.front() {
            Some(a) if a.prompt.kind == PromptKind::YesNo => operation::focus(NO_FIELD),
            Some(_) => operation::focus(ASKPASS_INPUT),
            None => Task::none(),
        }
    }

    pub(crate) fn view_ask(&self) -> Option<Element<'_, Message>> {
        let a = self.asks.queue.front()?;
        let p = &a.prompt;
        let mut body = column![para(p.text.trim())].spacing(12);
        if a.retry {
            body = body.push(ui::note(Tone::Warning, para("That wasn't accepted. Try again.")));
        }
        let cancel = ui::secondary("Cancel").on_press(Message::AskPassAnswer(false));
        let parts = match p.kind {
            PromptKind::YesNo => {
                let yes = if p.is_new_host() { "Connect" } else { "Allow" };
                DialogParts::new(p.title(), body)
                    .button(cancel)
                    .button(ui::primary(yes).on_press(Message::AskPassAnswer(true)))
            }
            PromptKind::Secret | PromptKind::Text => {
                let field = ui::input("", &a.input)
                    .id(ASKPASS_INPUT)
                    .secure(p.kind == PromptKind::Secret)
                    .on_input(Message::AskPassInput)
                    .on_submit(Message::AskPassAnswer(true));
                DialogParts::new(p.title(), body.push(field))
                    .button(cancel)
                    .button(ui::primary("OK").on_press(Message::AskPassAnswer(true)))
            }
        };
        Some(modal(parts.on_cancel(Message::AskPassAnswer(false)), 480.0, false))
    }
}
