//! Terminal Rooster's helper on a remote host, run by SSH for one
//! connection: it answers requests (files, git, what runs in terminals) and
//! relays each terminal's channel to the terminal's own helper there (see
//! [`crate::host`]), which it starts when needed. Terminals' helpers outlive
//! the connection, so a terminal runs on while the connection is down or
//! Terminal Rooster is closed, and is found again when it comes back.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::bootstrap;
use super::wire::{self, Bytes, ClientHello, GitOp, OpenMode, OpenTerminal, Opened, Request, Resume, ServerHello};
use crate::agent::AgentSession;
use crate::detect::{AgentHomes, Detector};
use crate::git::{Repo, Status};
use crate::host::proto::{self, ConnInfo, FrameReader, Hello, Spec};
use crate::host::{HostCommand, client};
use crate::model::PaneId;
use crate::session::{GridSize, integration, shell};

/// How long a terminal's helper gets to answer.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long requests still running get to finish once the connection closed
/// (ending terminals as Terminal Rooster quits, say).
const FINISH_WAIT: Duration = Duration::from_secs(5);

struct Server {
    out: Mutex<io::Stdout>,
    /// Each open channel's connection to its terminal's helper.
    channels: Mutex<HashMap<u32, Arc<TcpStream>>>,
    /// Channels the client closed before they were open (it gave up waiting).
    abandoned: Mutex<HashSet<u32>>,
    /// Requests being answered.
    busy: AtomicUsize,
    detector: Mutex<Detector>,
    homes: AgentHomes,
    /// This client's folder: its terminals' connection files and history.
    dir: PathBuf,
    /// Where the shell integration scripts are, if they could be written.
    integration: Option<PathBuf>,
    /// How to start a terminal's helper.
    helper: HostCommand,
    home: PathBuf,
}

/// Serves one connection on standard input and output until it closes.
/// `helper` starts a terminal's helper (this program, as `host`). Returns
/// the process exit code.
pub fn serve(helper: HostCommand) -> i32 {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()).unwrap_or_else(|| "/".into());
    {
        let mut out = io::stdout();
        // On a line of its own, after whatever a login script printed.
        if write!(out, "\n{} {}\n", wire::MAGIC, wire::VERSION).and_then(|()| out.flush()).is_err() {
            return 1;
        }
    }
    let mut input = FrameReader::new(io::stdin());
    let hello: ClientHello = match input.next() {
        Ok(Some(f)) if f.kind == wire::HELLO => match proto::parse(&f.payload) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error = %e, "unreadable hello");
                return 2;
            }
        },
        _ => {
            tracing::error!("no hello");
            return 2;
        }
    };
    if hello.version != wire::VERSION {
        tracing::error!(theirs = hello.version, ours = wire::VERSION, "another version of Terminal Rooster");
    }
    let dir = bootstrap::remote_root().join("clients").join(safe_name(&hello.client)).join(safe_host(&hello.host));
    if let Err(e) = std::fs::create_dir_all(proto::hosts_dir(&dir)) {
        tracing::error!(error = %e, dir = %dir.display(), "can't make the helper's folder");
        return 1;
    }
    let integration =
        integration::install(&dir).map_err(|e| tracing::warn!(error = %e, "no shell integration scripts")).ok();
    end_strays(&dir, &hello.keep);
    if let Some(scripts) = &integration {
        integration::forget_history(scripts, |pane| hello.keep.contains(&pane));
    }
    let homes = AgentHomes::from_env();
    let server = Arc::new(Server {
        out: Mutex::new(io::stdout()),
        channels: Mutex::new(HashMap::new()),
        abandoned: Mutex::new(HashSet::new()),
        busy: AtomicUsize::new(0),
        detector: Mutex::new(Detector::new(homes.clone())),
        homes,
        dir,
        integration,
        helper,
        home: home.clone(),
    });
    server.send_json(wire::WELCOME, &ServerHello { version: wire::VERSION, home });
    tracing::info!(client = %hello.client, terminals = hello.keep.len(), "serving");

    loop {
        let frame = match input.next() {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "connection broke");
                break;
            }
        };
        match frame.kind {
            wire::REQUEST => {
                let Some((id, json)) = wire::split_numbered(&frame.payload) else { continue };
                let request: Result<Request, _> = serde_json::from_slice(json);
                let server = server.clone();
                server.busy.fetch_add(1, Ordering::AcqRel);
                // Each on a thread: git and folders can be slow, terminals mustn't wait.
                std::thread::spawn(move || {
                    let result = match request {
                        Ok(request) => server.handle(request),
                        Err(e) => Err(format!("unreadable request: {e}")),
                    };
                    let mut json = serde_json::to_vec(&result).unwrap_or_default();
                    if json.is_empty() || json.len() > wire::MAX_RESPONSE {
                        let e: Result<(), String> = Err("the answer is too large to send".into());
                        json = serde_json::to_vec(&e).unwrap_or_default();
                    }
                    server.send(wire::RESPONSE, &wire::numbered(id, &json));
                    server.busy.fetch_sub(1, Ordering::AcqRel);
                });
            }
            wire::DATA => {
                let Some((channel, bytes)) = wire::split_channel(&frame.payload) else { continue };
                let stream = server.channels.lock().unwrap().get(&channel).cloned();
                if let Some(stream) = stream
                    && (&*stream).write_all(bytes).is_err()
                {
                    server.close_channel(channel);
                }
            }
            wire::CLOSE => {
                let Some((channel, _)) = wire::split_channel(&frame.payload) else { continue };
                match server.channels.lock().unwrap().remove(&channel) {
                    Some(stream) => {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    // Still being opened: it is closed once it is.
                    None => {
                        server.abandoned.lock().unwrap().insert(channel);
                    }
                }
            }
            _ => {}
        }
    }
    // Requests still running finish first (they may be ending terminals).
    let deadline = std::time::Instant::now() + FINISH_WAIT;
    while server.busy.load(Ordering::Acquire) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // The terminals' helpers run on without this client.
    for (_, stream) in server.channels.lock().unwrap().drain() {
        let _ = stream.shutdown(Shutdown::Both);
    }
    tracing::info!("connection closed");
    0
}

/// A host's name made safe for a folder name: what isn't a letter or a
/// digit becomes `_`.
fn safe_host(host: &str) -> String {
    let name: String =
        host.chars().take(100).map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    if name.is_empty() { "default".into() } else { name }
}

/// A client id made safe for a folder name.
fn safe_name(id: &str) -> String {
    let name: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
    if name.is_empty() { "default".into() } else { name }
}

/// Ends the helpers of terminals the client no longer has.
fn end_strays(dir: &Path, keep: &[PaneId]) {
    let Ok(entries) = std::fs::read_dir(proto::hosts_dir(dir)) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".tmp") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let Some(pane) = name.strip_suffix(".json").and_then(|id| id.parse().ok()).map(PaneId) else { continue };
        if !keep.contains(&pane) {
            tracing::info!(%pane, "ending the helper of a terminal that was closed");
            end_helper(&path);
        }
    }
}

/// Ends the helper whose connection file is `file`, if it runs.
fn end_helper(file: &Path) {
    let Some(info) = proto::read_conn(file) else { return };
    if client::end(&info).is_err() {
        proto::remove_conn_of(file, info.pid);
    }
}

impl Server {
    fn send(&self, kind: u8, payload: &[u8]) {
        let mut out = self.out.lock().unwrap();
        let _ = proto::write_frame(&mut *out, kind, payload).and_then(|()| out.flush());
    }

    fn send_json(&self, kind: u8, msg: &impl serde::Serialize) {
        if let Ok(json) = serde_json::to_vec(msg) {
            self.send(kind, &json);
        }
    }

    fn close_channel(&self, channel: u32) {
        if let Some(stream) = self.channels.lock().unwrap().remove(&channel) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.send(wire::CLOSED, &wire::channel(channel, &[]));
    }

    fn handle(self: &Arc<Self>, request: Request) -> Result<serde_json::Value, String> {
        let value = match request {
            Request::OpenTerminal { channel, open } => to_value(self.open_terminal(channel, *open)),
            Request::EndTerminals(panes) => {
                for pane in panes {
                    end_helper(&proto::conn_file(&self.dir, pane));
                }
                to_value(())
            }
            Request::Scan(shells) => {
                let mut detector = self.detector.lock().unwrap();
                let changes = detector.scan_with(&shells, &HashMap::new(), &[]);
                let programs = detector.programs_seen(&shells);
                to_value(wire::Scan { changes, programs })
            }
            Request::ListDirs(dirs) => to_value(
                dirs.into_iter().map(|d| (crate::files::list_dir(&d), d)).map(|(l, d)| (d, l)).collect::<Vec<_>>(),
            ),
            Request::Metadata(path) => to_value(crate::files::metadata(&path)),
            Request::ReadFile { path, max } => {
                to_value(crate::files::read_file(&path, max).map(|(m, b)| (m, Bytes(b))))
            }
            Request::WriteFile { path, bytes } => to_value(crate::files::write_file(&path, &bytes.0)),
            Request::CreateFile(path) => to_value(crate::files::create_file(&path)),
            Request::ResolveDir(path) => to_value(crate::files::resolve_dir(&path)),
            Request::CompleteDir { partial, limit } => to_value(crate::files::complete_dir(&partial, limit)),
            Request::AreDirs(paths) => to_value(crate::files::are_dirs(&paths)),
            Request::GitStatus(dir) => to_value(Status::of(&dir)),
            Request::Git { repo, op } => git(&repo, op),
        };
        value.map_err(|e| e.to_string())
    }

    /// Finds or starts the terminal's helper, says hello to it for the
    /// client, and relays what it sends over the channel from then on.
    fn open_terminal(self: &Arc<Self>, channel: u32, open: OpenTerminal) -> Result<Opened, String> {
        let file = proto::conn_file(&self.dir, open.pane);
        let mut opened = Opened::default();
        let mut stream = None;
        if open.mode == OpenMode::New {
            // One that runs it ends: its program is restarted.
            end_helper(&file);
        }
        if open.mode == OpenMode::ExistingOrNew
            && let Some(info) = proto::read_conn(&file)
        {
            match hello(&info, open.size) {
                Ok(s) => stream = Some(s),
                Err(e) => {
                    tracing::info!(pane = %open.pane, error = %e, "the terminal's helper is gone");
                    proto::remove_conn_of(&file, info.pid);
                }
            }
        }
        let stream = match stream {
            Some(s) => s,
            None => {
                let (spec, notice) = self.spec(&open);
                let program = spec.program.clone();
                tracing::info!(pane = %open.pane, %program, cwd = %spec.cwd.display(), "starting a terminal");
                let info = client::start(&self.helper, &spec, &proto::hosts_dir(&self.dir))
                    .map_err(|e| format!("failed to start {program}: {e}"))?;
                opened.fresh = true;
                opened.notice = notice;
                opened.then_type = resume_line(&open.start.resume, &self.homes);
                hello(&info, open.size).map_err(|e| format!("failed to start {program}: {e}"))?
            }
        };
        let reading = stream.try_clone().map_err(|e| e.to_string())?;
        if self.abandoned.lock().unwrap().remove(&channel) {
            // The client stopped waiting for it; it opens another.
            let _ = stream.shutdown(Shutdown::Both);
            return Err("given up".into());
        }
        self.channels.lock().unwrap().insert(channel, Arc::new(stream));
        let server = self.clone();
        std::thread::Builder::new()
            .name(format!("channel-{channel}"))
            .spawn(move || server.relay(channel, reading))
            .map_err(|e| e.to_string())?;
        Ok(opened)
    }

    /// Sends what the terminal's helper says over the channel until it closes.
    fn relay(&self, channel: u32, mut from: TcpStream) {
        let mut buf = vec![0; 64 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => self.send(wire::OUTPUT, &wire::channel(channel, &buf[..n])),
            }
        }
        // Unless the client closed it already.
        if self.channels.lock().unwrap().remove(&channel).is_some() {
            self.send(wire::CLOSED, &wire::channel(channel, &[]));
        }
    }

    /// What a new helper runs, and a notice when it can't start where it was asked to.
    fn spec(&self, open: &OpenTerminal) -> (Spec, Option<String>) {
        let start = &open.start;
        let mut command = start.shell.as_deref().and_then(shell::split_command_line).unwrap_or_else(login_shell);
        let mut env = start.env.clone();
        if start.shell_integration
            && let Some(dir) = &self.integration
        {
            integration::apply(&mut command, &mut env, dir, open.pane);
        }
        let mut notice = None;
        let cwd = match start.cwd.iter().find(|d| d.is_dir()) {
            Some(d) => d.clone(),
            None => {
                if let Some(first) = start.cwd.first() {
                    notice = Some(format!(
                        "working directory {} does not exist; starting in {}",
                        first.display(),
                        self.home.display()
                    ));
                }
                self.home.clone()
            }
        };
        let spec = Spec {
            pane: open.pane,
            conn_file: proto::conn_file(&self.dir, open.pane),
            token: proto::new_token(),
            program: command.program,
            args: command.args,
            cwd,
            env,
            size: open.size,
            scrollback_lines: start.scrollback_lines,
            preload: start.preload.clone(),
        };
        (spec, notice)
    }
}

/// The user's shell as a login shell, as `ssh` itself starts it: so the
/// profile runs (and puts `~/.local/bin`, where agents install, on `PATH`).
fn login_shell() -> shell::ShellCommand {
    let program = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    shell::ShellCommand { program, args: vec!["-l".into()] }
}

/// Connects to a terminal's helper and says hello for the client, which
/// reads the answer from the channel.
fn hello(info: &ConnInfo, size: GridSize) -> io::Result<TcpStream> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
    let mut stream = TcpStream::connect_timeout(&addr, HELPER_TIMEOUT)?;
    stream.set_nodelay(true)?;
    // The client that connects last shows the terminal: one that went away
    // with a broken connection may not have been noticed yet.
    let hello = Hello { token: info.token.clone(), version: proto::VERSION, steal: true, stay: true, size };
    proto::write_json(&mut stream, proto::HELLO, &hello)?;
    Ok(stream)
}

/// What a restored terminal types, as [`crate::Core`] works it out for one
/// of this computer, with the agent's sessions looked up here.
fn resume_line(resume: &Resume, homes: &AgentHomes) -> Option<String> {
    match &resume.agent {
        Some(a) => Some(match &a.session_id {
            Some(id) if homes.session_exists(a.kind, id) => a.resume_command()?,
            _ => AgentSession { session_id: None, ..a.clone() }.launch_command(),
        }),
        None => resume.command.clone(),
    }
}

fn to_value(v: impl serde::Serialize) -> serde_json::Result<serde_json::Value> {
    serde_json::to_value(v)
}

fn git(repo: &Repo, op: GitOp) -> serde_json::Result<serde_json::Value> {
    match op {
        GitOp::Head => to_value(repo.head()),
        GitOp::Branches => to_value(repo.branches()),
        GitOp::SamePlaceIn { dir, other } => to_value(repo.same_place_in(&dir, &other)),
        GitOp::AddWorktree { path, branch, plan, base } => {
            to_value(repo.add_worktree(&path, &branch, &plan, base.as_deref()))
        }
        GitOp::Changes(path) => to_value(repo.changes(&path)),
        GitOp::RemoveWorktree { path, force } => to_value(repo.remove_worktree(&path, force)),
        GitOp::HasWorktree(path) => to_value(repo.has_worktree(&path)),
        GitOp::RemoveLeftover(path) => to_value(repo.remove_leftover(&path)),
        GitOp::Tracking { all, limit } => to_value(repo.tracking(all, limit)),
        GitOp::Remotes => to_value(repo.remotes()),
        GitOp::Push => to_value(repo.push()),
        GitOp::Pull { rebase } => to_value(repo.pull(rebase)),
        GitOp::UseRemote(remote) => to_value(repo.use_remote(&remote)),
        GitOp::AddRemote { name, url } => to_value(repo.add_remote(&name, &url)),
        GitOp::SetRemoteUrl { name, url } => to_value(repo.set_remote_url(&name, &url)),
        GitOp::Log { all, limit } => to_value(repo.log(all, limit)),
        GitOp::RefStamp => to_value(repo.ref_stamp()),
        GitOp::CommitDetails(id) => to_value(repo.commit_details(&id)),
        GitOp::WorkingChanges => to_value(repo.working_changes()),
        GitOp::StagePaths(paths) => to_value(repo.stage_paths(paths.iter().map(String::as_str))),
        GitOp::UnstagePaths(paths) => to_value(repo.unstage_paths(paths.iter().map(String::as_str))),
        GitOp::StageHunk { file, hunk, lines } => to_value(repo.stage_hunk(&file, hunk, lines.as_ref())),
        GitOp::UnstageHunk { file, hunk, lines } => to_value(repo.unstage_hunk(&file, hunk, lines.as_ref())),
        GitOp::Commit { message, amend } => to_value(repo.commit(&message, amend)),
        GitOp::LastMessage => to_value(repo.last_message()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_make_safe_folder_names() {
        assert_eq!(safe_name("3f2a-bc"), "3f2a-bc");
        assert_eq!(safe_name("../../etc"), "etc");
        assert_eq!(safe_name("/"), "default");
        assert_eq!(safe_host("me@10.0.0.2"), "me_10_0_0_2");
        assert_eq!(safe_host("../x"), "___x");
    }

    #[test]
    fn restored_agents_resume_or_start_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let homes = AgentHomes {
            claude: dir.path().join("claude"),
            codex: dir.path().join("codex"),
            grok: dir.path().join("grok"),
        };
        std::fs::create_dir_all(dir.path().join("claude/projects/p")).unwrap();
        std::fs::write(dir.path().join("claude/projects/p/abc.jsonl"), "").unwrap();
        let mut agent = AgentSession::new(crate::AgentKind::Claude, String::new());
        agent.session_id = Some("abc".into());
        let saved = Resume { agent: Some(agent.clone()), command: None };
        assert_eq!(resume_line(&saved, &homes), agent.resume_command());
        agent.session_id = Some("gone".into());
        let lost = Resume { agent: Some(agent.clone()), command: None };
        assert_eq!(resume_line(&lost, &homes), Some(AgentSession { session_id: None, ..agent }.launch_command()));
        let plain = Resume { agent: None, command: Some("npm run dev".into()) };
        assert_eq!(resume_line(&plain, &homes).as_deref(), Some("npm run dev"));
    }
}
