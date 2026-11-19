//! What Terminal Rooster and its helper on a remote host say to each other,
//! over the SSH connection's standard input and output.
//!
//! Frames as in [`crate::host::proto`]: a byte saying what the frame is, the
//! length of the payload (u32, little endian), the payload. Requests and
//! their responses carry a number that pairs them, then JSON. Terminals go
//! through channels: each is a byte stream to a terminal's helper on the
//! host, which the remote helper relays as it is.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentSession;
use crate::detect::{ObservedAgent, Program};
use crate::git::{FileDiff, Plan, Repo};
use crate::model::PaneId;
use crate::session::GridSize;

/// The largest response sent: a larger one would be refused as a frame,
/// and break the connection. (Base64 makes file contents a third larger.)
pub(crate) const MAX_RESPONSE: usize = 48 << 20;

/// Changes whenever a message changes. Both ends are the same build (the
/// remote helper is copied from this one), so this only guards against an
/// old helper found on the host.
pub(crate) const VERSION: u32 = 1;

/// The line the remote helper prints before its first frame. What a login
/// script prints before it is skipped.
pub(crate) const MAGIC: &str = "TERMINALROOSTER-REMOTE";
/// The line the launch script prints when the helper isn't on the host,
/// followed by `uname -sm`.
pub(crate) const MISSING: &str = "TERMINALROOSTER-MISSING";

// From Terminal Rooster.
/// [`ClientHello`]: the first frame.
pub(crate) const HELLO: u8 = 1;
/// A request: its number (u64, little endian), then a [`Request`] in JSON.
pub(crate) const REQUEST: u8 = 2;
/// Bytes for a channel: its number (u32, little endian), then the bytes.
pub(crate) const DATA: u8 = 3;
/// Closes a channel: its number.
pub(crate) const CLOSE: u8 = 4;

// From the remote helper.
/// [`ServerHello`]: the answer to the hello.
pub(crate) const WELCOME: u8 = 0x81;
/// A response: the request's number, then a `Result<value, String>` in JSON.
pub(crate) const RESPONSE: u8 = 0x82;
/// Bytes from a channel, as [`DATA`].
pub(crate) const OUTPUT: u8 = 0x83;
/// A channel ended (its terminal's helper closed the connection).
pub(crate) const CLOSED: u8 = 0x84;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ClientHello {
    pub version: u32,
    /// Which Terminal Rooster this is, so two computers that use the same
    /// host don't meet each other's terminals (pane ids are per computer).
    pub client: String,
    /// The host's name as the client calls it: two names of one host (or
    /// hosts sharing a home folder) keep their terminals apart too.
    #[serde(default)]
    pub host: String,
    /// The terminals of this host Terminal Rooster still has: the helpers of
    /// the others are ended (their sessions were closed meanwhile).
    pub keep: Vec<PaneId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ServerHello {
    pub version: u32,
    /// The user's home folder on the host.
    pub home: PathBuf,
}

/// What Terminal Rooster asks of the remote helper.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Request {
    /// Shows a terminal over channel `channel` (see [`OpenTerminal`]).
    /// Answers [`Opened`].
    OpenTerminal { channel: u32, open: Box<OpenTerminal> },
    /// Ends the helpers of these terminals, if they run.
    EndTerminals(Vec<PaneId>),
    /// Looks at what runs in these terminals (`(pane, shell pid)`). Answers [`Scan`].
    Scan(Vec<(PaneId, u32)>),
    /// Answers `Vec<(PathBuf, Result<Vec<Entry>, String>)>`.
    ListDirs(Vec<PathBuf>),
    /// Answers `Result<FileMeta, FsError>`.
    Metadata(PathBuf),
    /// Answers `Result<(FileMeta, Bytes), FsError>`.
    ReadFile { path: PathBuf, max: u64 },
    /// Answers `Result<FileMeta, FsError>`.
    WriteFile { path: PathBuf, bytes: Bytes },
    /// Answers `Result<(), String>`.
    CreateFile(PathBuf),
    /// Answers `Result<PathBuf, String>`.
    ResolveDir(PathBuf),
    /// Answers `Vec<String>`.
    CompleteDir { partial: String, limit: usize },
    /// Answers `Vec<bool>`.
    AreDirs(Vec<PathBuf>),
    /// Answers `Option<Status>`.
    GitStatus(PathBuf),
    /// Runs a [`Repo`] method there; answers what it returns.
    Git { repo: Repo, op: GitOp },
}

/// A [`Repo`] method, with its arguments.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum GitOp {
    Head,
    Branches,
    SamePlaceIn { dir: PathBuf, other: PathBuf },
    AddWorktree { path: PathBuf, branch: String, plan: Plan, base: Option<String> },
    Changes(PathBuf),
    RemoveWorktree { path: PathBuf, force: bool },
    HasWorktree(PathBuf),
    RemoveLeftover(PathBuf),
    Tracking { all: bool, limit: usize },
    Remotes,
    Push,
    Pull { rebase: bool },
    UseRemote(String),
    AddRemote { name: String, url: String },
    SetRemoteUrl { name: String, url: String },
    Log { all: bool, limit: usize },
    RefStamp,
    CommitDetails(String),
    WorkingChanges,
    StagePaths(Vec<String>),
    UnstagePaths(Vec<String>),
    StageHunk { file: Box<FileDiff>, hunk: usize, lines: Option<std::collections::BTreeSet<usize>> },
    UnstageHunk { file: Box<FileDiff>, hunk: usize, lines: Option<std::collections::BTreeSet<usize>> },
    Commit { message: String, amend: bool },
    LastMessage,
}

/// Which helper a terminal gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum OpenMode {
    /// The one that runs it, else a new one (a terminal shown again after a
    /// restart, or after the connection came back).
    ExistingOrNew,
    /// A new one, in place of any that runs it (a restart).
    New,
}

/// A terminal to show over a channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OpenTerminal {
    pub pane: PaneId,
    pub mode: OpenMode,
    pub size: GridSize,
    pub start: StartSpec,
}

/// How to start a terminal's program on the host, where the shell, the
/// folder and what to type are worked out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StartSpec {
    /// The project's shell command line; `None`: the user's login shell.
    pub shell: Option<String>,
    /// Where to start: the first of these that is a folder, else the home folder.
    pub cwd: Vec<PathBuf>,
    pub env: HashMap<String, String>,
    pub shell_integration: bool,
    pub scrollback_lines: usize,
    /// Output to show above the program's (a restored terminal's scrollback).
    pub preload: Option<String>,
    /// What to type once a new program runs.
    pub resume: Resume,
}

/// What a restored terminal types: its agent's session if that is saved on
/// the host (else the agent afresh), or else its own resume command.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Resume {
    pub agent: Option<AgentSession>,
    pub command: Option<String>,
}

/// The answer to [`Request::OpenTerminal`]. The helper's answer to the
/// hello (welcome, paused or refused) comes first on the channel.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Opened {
    /// A new helper was started.
    pub fresh: bool,
    /// What to type into its program, once it runs.
    pub then_type: Option<String>,
    /// Something to show in the terminal first (the folder was gone, say).
    pub notice: Option<String>,
}

/// The answer to [`Request::Scan`], as [`crate::session::Background::scan_terminals`] gathers it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Scan {
    pub changes: Vec<(PaneId, Option<ObservedAgent>)>,
    pub programs: HashMap<PaneId, Vec<Program>>,
}

/// Bytes, as base64 in JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use base64::Engine;
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use base64::Engine;
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD.decode(s).map(Bytes).map_err(serde::de::Error::custom)
    }
}

/// A request or response payload: its number, then JSON.
pub(crate) fn numbered(id: u64, json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + json.len());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(json);
    out
}

/// Splits what [`numbered`] made.
pub(crate) fn split_numbered(payload: &[u8]) -> Option<(u64, &[u8])> {
    let id = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
    Some((id, &payload[8..]))
}

/// A channel payload: its number, then the bytes.
pub(crate) fn channel(channel: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&channel.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Splits what [`channel`] made.
pub(crate) fn split_channel(payload: &[u8]) -> Option<(u32, &[u8])> {
    let id = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
    Some((id, &payload[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_go_as_base64() {
        let json = serde_json::to_string(&Bytes(b"hi\0".to_vec())).unwrap();
        assert_eq!(json, "\"aGkA\"");
        assert_eq!(serde_json::from_str::<Bytes>(&json).unwrap().0, b"hi\0");
    }

    #[test]
    fn numbers_lead_payloads() {
        assert_eq!(split_numbered(&numbered(7, b"{}")), Some((7, &b"{}"[..])));
        assert_eq!(split_channel(&channel(3, b"abc")), Some((3, &b"abc"[..])));
        assert_eq!(split_channel(b"ab"), None);
    }
}
