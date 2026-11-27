//! Things happening in a terminal that the user may want to hear about:
//! failed commands, and coding agents finishing their turn.
//!
//! Each terminal has a [`Tracker`] fed with what its program sends:
//!
//! * **Failed commands** come from shell integration (`OSC 133 ; D ; <status>`),
//!   which TerminalRooster sets up for bash and PowerShell (see
//!   [`crate::session::integration`]). Other shells can send it themselves.
//! * **Claude Code** puts its state in the terminal title: `◐`/`◑` in front
//!   while it works, `✳` when it is idle or waiting for an answer.
//! * **Codex and Antigravity** report their state with `OSC 7501` (`working`,
//!   `done`, `error`, `blocked` on an approval, a question or a sign-in).
//! * **Progress indicators** (`OSC 9 ; 4`), which Grok shows while working.
//! * **Notifications** a program asks for (`OSC 9`, `OSC 777`, `OSC 99`),
//!   and the bell, which agents use at the end of a turn in terminals they
//!   have no better way to notify in.

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::session::osc::Signal;

/// Something in a terminal worth telling the user about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Alert {
    /// A command exited with an error status.
    CommandFailed {
        code: i32,
        /// The command line, when the shell integration reported it.
        command: Option<String>,
    },
    /// A coding agent finished its turn or is waiting for the user.
    Agent {
        agent: Option<AgentKind>,
        event: AgentEvent,
        /// What the agent said in its notification, if it sent one.
        message: Option<String>,
    },
    /// A program that is not a coding agent asked for a desktop notification.
    Notification { title: Option<String>, body: String },
}

/// Why an agent wants the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEvent {
    /// It finished its turn. Claude Code shows the same state when it waits
    /// for an answer, so for Claude this also means "waiting for you".
    Done,
    /// The turn ended with an error.
    Failed,
    /// It needs an approval.
    Approval,
    /// It asked a question.
    Question,
    /// It needs the user to sign in.
    SignIn,
}

/// Exit statuses that mean the user stopped the command (Ctrl+C), not that it failed.
fn interrupted(code: i32) -> bool {
    // SIGINT in POSIX shells; STATUS_CONTROL_C_EXIT on Windows.
    code == 130 || code == 0xC000_013A_u32 as i32
}

/// Claude Code's title prefix: `Some(true)` while working, `Some(false)` when
/// idle or waiting for the user, `None` for other titles.
pub(crate) fn claude_working(title: &str) -> Option<bool> {
    match title.chars().next()? {
        '◐' | '◑' => Some(true),
        '✳' => Some(false),
        _ => None,
    }
}

/// The program a command line runs: its first word that is not a variable
/// assignment or a wrapper such as `sudo`, without its folder and `.exe`.
fn program_of(command: &str) -> Option<String> {
    const WRAPPERS: &[&str] = &["builtin", "command", "env", "exec", "nice", "nohup", "sudo", "time"];
    let word = command.split_whitespace().find(|w| !w.contains('=') && !WRAPPERS.contains(w))?;
    let path = word.trim_matches(['\'', '"']);
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = match name.len().checked_sub(4).and_then(|i| Some((i, name.get(i..)?))) {
        Some((i, ext)) if ext.eq_ignore_ascii_case(".exe") => &name[..i],
        _ => name,
    };
    (!name.is_empty()).then(|| name.to_owned())
}

/// Turns one terminal's output signals into [`Alert`]s.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    /// The coding agent running in the terminal, as last detected.
    pub agent: Option<AgentKind>,
    /// Claude Code's title says it is working.
    title_busy: bool,
    /// A progress indicator is showing.
    progress_busy: bool,
    /// The program reported `OSC 7501 state=working`.
    status_busy: bool,
    /// The last command line the shell integration reported.
    command: Option<String>,
    /// The program of the command typed at the prompt, from when the shell
    /// integration reports that it started until it finished.
    running: Option<String>,
}

impl Tracker {
    /// The agent is working, as far as its own signals tell.
    pub fn busy(&self) -> bool {
        self.title_busy || self.progress_busy || self.status_busy
    }

    /// The program of the command typed at the prompt while it runs, unless
    /// it is a coding agent or a shell, which wait for the user most of the time.
    pub fn running(&self) -> Option<&str> {
        self.running.as_deref().filter(|name| {
            let name = name.to_lowercase();
            AgentKind::parse(&name).is_none() && !crate::detect::SHELLS.contains(&name.as_str())
        })
    }

    /// A new shell started: nothing from the old one is still running.
    pub fn reset(&mut self) {
        *self = Tracker { agent: self.agent, ..Tracker::default() };
    }

    fn agent_alert(&self, agent: Option<AgentKind>, event: AgentEvent) -> Alert {
        Alert::Agent { agent: agent.or(self.agent), event, message: None }
    }

    pub fn title(&mut self, title: &str) -> Option<Alert> {
        match claude_working(title) {
            Some(true) => {
                self.title_busy = true;
                None
            }
            Some(false) => std::mem::take(&mut self.title_busy)
                .then(|| self.agent_alert(Some(AgentKind::Claude), AgentEvent::Done)),
            // Claude Code exited (or something else set the title).
            None => {
                self.title_busy = false;
                None
            }
        }
    }

    pub fn signal(&mut self, signal: Signal) -> Option<Alert> {
        match signal {
            Signal::CommandLine(command) => {
                self.command = Some(command).filter(|c| !c.trim().is_empty());
                None
            }
            Signal::CommandStart => {
                self.running = self.command.as_deref().and_then(program_of);
                None
            }
            Signal::CommandDone(code) => {
                self.running = None;
                let command = self.command.take();
                code.filter(|c| *c != 0 && !interrupted(*c)).map(|code| Alert::CommandFailed { code, command })
            }
            Signal::Progress(0) => {
                // Other programs show progress too (installers, downloads); only an
                // agent's counts as finishing a turn.
                (std::mem::take(&mut self.progress_busy) && self.agent.is_some())
                    .then(|| self.agent_alert(None, AgentEvent::Done))
            }
            Signal::Progress(_) => {
                self.progress_busy = true;
                None
            }
            Signal::Cwd(_) | Signal::Cleared | Signal::StatusQuery => None,
            Signal::Status { state, app, kind, message } => {
                let agent = app.as_deref().and_then(AgentKind::parse);
                let event = match state.as_str() {
                    "working" => {
                        self.status_busy = true;
                        return None;
                    }
                    "done" => AgentEvent::Done,
                    "error" => AgentEvent::Failed,
                    "blocked" => match kind.as_deref() {
                        Some("question") => AgentEvent::Question,
                        Some("auth") => AgentEvent::SignIn,
                        _ => AgentEvent::Approval,
                    },
                    // `idle`, `clear` and anything newer.
                    _ => {
                        self.status_busy = false;
                        return None;
                    }
                };
                self.status_busy = false;
                Some(Alert::Agent { agent: agent.or(self.agent), event, message })
            }
            Signal::Notify { title, body } => Some(match self.agent {
                Some(agent) => {
                    let message = match title {
                        Some(t) if !body.is_empty() && t != agent.label() => format!("{t}: {body}"),
                        Some(t) if body.is_empty() => t,
                        _ => body,
                    };
                    Alert::Agent {
                        agent: Some(agent),
                        event: AgentEvent::Done,
                        message: Some(message).filter(|m| !m.is_empty()),
                    }
                }
                None => Alert::Notification { title, body },
            }),
        }
    }

    /// The bell counts only in terminals running an agent; elsewhere it is
    /// mostly a shell complaining about a key.
    pub fn bell(&mut self) -> Option<Alert> {
        self.agent.is_some().then(|| self.agent_alert(None, AgentEvent::Done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kind: AgentKind, event: AgentEvent) -> Option<Alert> {
        Some(Alert::Agent { agent: Some(kind), event, message: None })
    }

    #[test]
    fn claude_title_turns() {
        let mut t = Tracker::default();
        assert_eq!(t.title("✳ Claude Code"), None, "idle at start");
        assert_eq!(t.title("◐ Fix the parser"), None);
        assert!(t.busy());
        assert_eq!(t.title("◑ Fix the parser"), None);
        assert_eq!(t.title("✳ Fix the parser"), agent(AgentKind::Claude, AgentEvent::Done));
        assert!(!t.busy());
        assert_eq!(t.title("✳ Fix the parser"), None, "only once");
        // Quitting mid-turn is not finishing it.
        t.title("◐ Fix the parser");
        assert_eq!(t.title("pwsh"), None);
        assert_eq!(t.title("✳ Claude Code"), None);
    }

    #[test]
    fn codex_status() {
        let mut t = Tracker::default();
        let status = |state: &str, kind: Option<&str>| Signal::Status {
            state: state.into(),
            app: Some("codex".into()),
            kind: kind.map(Into::into),
            message: None,
        };
        assert_eq!(t.signal(status("idle", None)), None);
        assert_eq!(t.signal(status("working", None)), None);
        assert!(t.busy());
        assert_eq!(t.signal(status("blocked", Some("permission"))), agent(AgentKind::Codex, AgentEvent::Approval));
        assert!(!t.busy());
        t.signal(status("working", None));
        assert_eq!(t.signal(status("done", None)), agent(AgentKind::Codex, AgentEvent::Done));
        assert_eq!(t.signal(status("idle", None)), None);
        t.signal(status("working", None));
        assert_eq!(t.signal(status("error", None)), agent(AgentKind::Codex, AgentEvent::Failed));
        t.signal(status("working", None));
        assert_eq!(t.signal(Signal::Status { state: "clear".into(), app: None, kind: None, message: None }), None);
        assert!(!t.busy(), "Codex quit");
    }

    #[test]
    fn antigravity_status() {
        let mut t = Tracker::default();
        let status = |state: &str, kind: Option<&str>, message: Option<&str>| Signal::Status {
            state: state.into(),
            app: Some("agy".into()),
            kind: kind.map(Into::into),
            message: message.map(Into::into),
        };
        assert_eq!(t.signal(status("working", None, None)), None);
        assert!(t.busy());
        assert_eq!(
            t.signal(status("blocked", Some("permission"), Some("Run cargo test?"))),
            Some(Alert::Agent {
                agent: Some(AgentKind::Antigravity),
                event: AgentEvent::Approval,
                message: Some("Run cargo test?".into()),
            })
        );
        assert!(!t.busy());
        t.signal(status("working", None, None));
        assert_eq!(t.signal(status("done", None, None)), agent(AgentKind::Antigravity, AgentEvent::Done));
        assert_eq!(t.signal(Signal::StatusQuery), None);
    }

    #[test]
    fn failed_commands() {
        let mut t = Tracker::default();
        assert_eq!(t.signal(Signal::CommandDone(Some(0))), None);
        assert_eq!(t.signal(Signal::CommandDone(None)), None);
        assert_eq!(t.signal(Signal::CommandDone(Some(130))), None, "Ctrl+C");
        t.signal(Signal::CommandLine("cargo test".into()));
        assert_eq!(
            t.signal(Signal::CommandDone(Some(101))),
            Some(Alert::CommandFailed { code: 101, command: Some("cargo test".into()) })
        );
        assert_eq!(
            t.signal(Signal::CommandDone(Some(1))),
            Some(Alert::CommandFailed { code: 1, command: None }),
            "the command line belongs to one status only"
        );
    }

    #[test]
    fn typed_commands_run_until_they_finish() {
        let mut t = Tracker::default();
        let start = |t: &mut Tracker, line: &str| {
            t.signal(Signal::CommandLine(line.into()));
            t.signal(Signal::CommandStart);
        };
        start(&mut t, "vim notes.md");
        assert_eq!(t.running(), Some("vim"));
        t.signal(Signal::CommandLine(String::new()));
        assert_eq!(t.signal(Signal::CommandDone(Some(0))), None);
        assert_eq!(t.running(), None);

        start(&mut t, "FOO=1 sudo /usr/bin/ssh -p 22 host");
        assert_eq!(t.running(), Some("ssh"));
        // A failure is still reported with the command line sent at the end.
        t.signal(Signal::CommandLine("ssh -p 22 host".into()));
        assert_eq!(
            t.signal(Signal::CommandDone(Some(255))),
            Some(Alert::CommandFailed { code: 255, command: Some("ssh -p 22 host".into()) })
        );
        assert_eq!(t.running(), None);

        start(&mut t, r"C:\Tools\Watcher.EXE --poll");
        assert_eq!(t.running(), Some("Watcher"));
        t.reset();
        assert_eq!(t.running(), None, "a new shell runs nothing yet");
        // Agents and nested shells wait for the user most of the time.
        for line in ["claude --resume x", "bash", "Codex", "agy --resume x"] {
            start(&mut t, line);
            assert_eq!(t.running(), None, "{line}");
        }
        // A shell that names no command: nothing to show.
        t.signal(Signal::CommandDone(Some(0)));
        t.signal(Signal::CommandStart);
        assert_eq!(t.running(), None);
    }

    #[test]
    fn progress_counts_only_for_agents() {
        let mut t = Tracker::default();
        t.signal(Signal::Progress(3));
        assert_eq!(t.signal(Signal::Progress(0)), None, "an installer's progress bar");
        t.agent = Some(AgentKind::Grok);
        t.signal(Signal::Progress(3));
        assert!(t.busy());
        assert_eq!(t.signal(Signal::Progress(0)), agent(AgentKind::Grok, AgentEvent::Done));
        assert_eq!(t.signal(Signal::Progress(0)), None);
    }

    #[test]
    fn notifications_and_bell() {
        let mut t = Tracker::default();
        assert_eq!(t.bell(), None);
        assert_eq!(
            t.signal(Signal::Notify { title: None, body: "Build done".into() }),
            Some(Alert::Notification { title: None, body: "Build done".into() })
        );
        t.agent = Some(AgentKind::Grok);
        assert_eq!(t.bell(), agent(AgentKind::Grok, AgentEvent::Done));
        assert_eq!(
            t.signal(Signal::Notify { title: Some("Grok".into()), body: "Turn complete".into() }),
            Some(Alert::Agent {
                agent: Some(AgentKind::Grok),
                event: AgentEvent::Done,
                message: Some("Turn complete".into())
            })
        );
    }
}
