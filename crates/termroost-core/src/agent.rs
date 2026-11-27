//! Coding-agent launchers with exact session tracking.
//!
//! `--continue` / `resume --last` resume the most recent session in a
//! directory, which is wrong when several agents work in the same project. So
//! TerminalRooster owns the session id instead:
//!
//! * Claude Code and Grok accept `--session-id <uuid>` at launch, so the id is
//!   generated up front and resumed with `--resume <uuid>`.
//! * Codex and Antigravity cannot be given an id; they are found by
//!   [`crate::detect`], which also tracks agents started by hand and new
//!   sessions started from inside an agent (`/clear`, `/new`).

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentKind {
    Claude,
    Codex,
    Grok,
    Antigravity,
}

impl AgentKind {
    pub const ALL: [AgentKind; 4] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Grok, AgentKind::Antigravity];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Grok => "grok",
            AgentKind::Antigravity => "agy",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex",
            AgentKind::Grok => "Grok",
            AgentKind::Antigravity => "Antigravity",
        }
    }

    pub fn parse(s: &str) -> Option<AgentKind> {
        match s {
            "antigravity" => Some(AgentKind::Antigravity),
            _ => AgentKind::ALL.into_iter().find(|k| k.as_str() == s),
        }
    }

    /// Whether the session id can be chosen at launch.
    pub fn assigns_id(self) -> bool {
        matches!(self, AgentKind::Claude | AgentKind::Grok)
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The agent running (or last run) in a pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    pub kind: AgentKind,
    /// Known session id; `None` while a Codex or Antigravity session is still being discovered.
    pub session_id: Option<String>,
    /// Extra command-line arguments, reused when resuming (e.g. `--model opus`).
    pub args: String,
}

impl AgentSession {
    /// Starts tracking a new session, generating its id when the agent allows it.
    pub fn new(kind: AgentKind, args: String) -> Self {
        let session_id = kind.assigns_id().then(|| uuid::Uuid::new_v4().to_string());
        Self { kind, session_id, args: args.trim().to_owned() }
    }

    /// Command line that starts this session.
    pub fn launch_command(&self) -> String {
        let base = match (self.kind, &self.session_id) {
            (AgentKind::Claude, Some(id)) => format!("claude --session-id {id}"),
            (AgentKind::Grok, Some(id)) => format!("grok --session-id {id}"),
            (kind, _) => kind.as_str().to_owned(),
        };
        join_args(base, &self.args)
    }

    /// Command line that resumes exactly this session, if its id is known.
    pub fn resume_command(&self) -> Option<String> {
        let id = self.session_id.as_deref()?;
        let base = match self.kind {
            AgentKind::Claude => format!("claude --resume {id}"),
            AgentKind::Grok => format!("grok --resume {id}"),
            AgentKind::Antigravity => format!("agy --resume {id}"),
            AgentKind::Codex => {
                // `codex resume [OPTIONS] [SESSION_ID]`: options go before the id.
                return Some(join_args("codex resume".into(), &self.args) + " " + id);
            }
        };
        Some(join_args(base, &self.args))
    }
}

fn join_args(base: String, args: &str) -> String {
    if args.trim().is_empty() { base } else { format!("{base} {}", args.trim()) }
}

// Fallback for Codex detection when its thread table cannot be read.
#[derive(Deserialize)]
struct RolloutHead {
    #[serde(rename = "type")]
    kind: String,
    payload: RolloutMeta,
}

// Fallback for Codex detection when its thread table cannot be read.
#[derive(Deserialize)]
struct RolloutMeta {
    id: String,
    cwd: PathBuf,
}

// Fallback for Codex detection when its thread table cannot be read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CodexCandidate {
    pub id: String,
    pub cwd: PathBuf,
    pub created: SystemTime,
}

// Fallback for Codex detection when its thread table cannot be read.
pub(crate) fn scan_codex_sessions_in(root: &Path, since: SystemTime) -> Vec<CodexCandidate> {
    // Layout is sessions/YYYY/MM/DD; only the newest few day directories matter.
    let mut day_dirs = Vec::new();
    for year in sorted_dirs(root).into_iter().rev().take(2) {
        for month in sorted_dirs(&year).into_iter().rev().take(2) {
            for day in sorted_dirs(&month).into_iter().rev().take(3) {
                day_dirs.push(day);
            }
        }
    }
    let threshold = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    let mut out = Vec::new();
    for dir in day_dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let created = meta.created().or_else(|_| meta.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            if created < threshold {
                continue;
            }
            if let Some((id, cwd)) = read_rollout_head(&path) {
                out.push(CodexCandidate { id, cwd, created });
            }
        }
    }
    out.sort_by_key(|c| c.created);
    out
}

// Fallback for Codex detection when its thread table cannot be read.
/// Numeric (date component) subdirectories, sorted.
fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let numeric = |p: &Path| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.path()).filter(|p| p.is_dir() && numeric(p)).collect())
        .unwrap_or_default();
    v.sort();
    v
}

// Fallback for Codex detection when its thread table cannot be read.
fn read_rollout_head(path: &Path) -> Option<(String, PathBuf)> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    // The first line embeds the base instructions, so it can be large.
    BufReader::new(file).take(4 << 20).read_line(&mut line).ok()?;
    let head: RolloutHead = serde_json::from_str(&line).ok()?;
    (head.kind == "session_meta").then_some((head.payload.id, head.payload.cwd))
}

// Fallback for Codex detection when its thread table cannot be read.
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        let s = AgentSession::new(AgentKind::Claude, " --model opus ".into());
        let id = s.session_id.clone().unwrap();
        assert_eq!(s.launch_command(), format!("claude --session-id {id} --model opus"));
        assert_eq!(s.resume_command().unwrap(), format!("claude --resume {id} --model opus"));

        let g = AgentSession::new(AgentKind::Grok, String::new());
        assert!(g.launch_command().starts_with("grok --session-id "));

        let mut c = AgentSession::new(AgentKind::Codex, "--yolo".into());
        assert_eq!(c.session_id, None);
        assert_eq!(c.launch_command(), "codex --yolo");
        assert_eq!(c.resume_command(), None);
        c.session_id = Some("abc".into());
        assert_eq!(c.resume_command().unwrap(), "codex resume --yolo abc");

        let mut a = AgentSession::new(AgentKind::Antigravity, "--model flash".into());
        assert_eq!(a.session_id, None);
        assert_eq!(a.launch_command(), "agy --model flash");
        assert_eq!(a.resume_command(), None);
        a.session_id = Some("abc".into());
        assert_eq!(a.resume_command().unwrap(), "agy --resume abc --model flash");
        assert_eq!(AgentKind::parse("agy"), Some(AgentKind::Antigravity));
        assert_eq!(AgentKind::parse("antigravity"), Some(AgentKind::Antigravity));
    }

    #[test]
    fn codex_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026/11/15");
        std::fs::create_dir_all(&day).unwrap();
        let proj_a = tmp.path().join("a");
        let proj_b = tmp.path().join("b");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        let start = SystemTime::now() - Duration::from_secs(1);
        let write = |name: &str, id: &str, cwd: &Path| {
            let line = serde_json::json!({
                "type": "session_meta",
                "payload": { "id": id, "cwd": cwd, "base_instructions": "x".repeat(10_000) }
            });
            std::fs::write(day.join(name), format!("{line}\n{{}}\n")).unwrap();
        };
        write("rollout-1-aaa.jsonl", "aaa", &proj_a);
        write("rollout-2-bbb.jsonl", "bbb", &proj_b);
        std::fs::write(day.join("rollout-3-bad.jsonl"), "not json\n").unwrap();

        let found = scan_codex_sessions_in(tmp.path(), start);
        assert_eq!(found.len(), 2);

        assert!(same_dir(&found[0].cwd, &proj_a) || same_dir(&found[0].cwd, &proj_b));
    }
}
