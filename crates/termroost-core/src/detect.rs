//! Finds which coding agent runs in each terminal, and its current session id.
//!
//! Each agent keeps its own record of live sessions, keyed by process id:
//!
//! * **Claude Code** writes `~/.claude/sessions/<pid>.json` (`sessionId`,
//!   `procStart`) and rewrites it when `/clear` starts a new session.
//! * **Grok** lists `{session_id, pid, pid_start}` in `~/.grok/active_sessions.json`.
//! * **Codex** has no per-process record: since 0.169 every terminal UI talks
//!   to one shared app-server daemon, which owns all threads. A Codex terminal
//!   is matched to its thread by, in order: the id on its command line
//!   (`codex resume <id>`); the thread files the process itself holds open
//!   (Linux, older non-daemon versions); Codex's thread table — the newest
//!   user thread in the process's directory created after it started (several
//!   Codex instances in one directory are matched latest-started first); and
//!   finally the rollout files on disk.
//!
//! The detector walks each terminal's process tree (the shell and everything
//! below it) and looks every process up in those records. So agents are found
//! whether they were started from TerminalRooster or typed by hand, and a new
//! session (`/clear`, `/new`, `/resume …`) is picked up on the next scan.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::agent::{self, AgentKind};
use crate::codex_hook::HookReport;
use crate::model::PaneId;

/// Where each agent keeps its state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHomes {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub grok: PathBuf,
}

impl Default for AgentHomes {
    fn default() -> Self {
        Self::from_env()
    }
}

impl AgentHomes {
    /// `~/.claude`, `~/.codex` and `~/.grok`, honouring `CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME` and `GROK_HOME`.
    pub fn from_env() -> AgentHomes {
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()).unwrap_or_default();
        let pick = |var: &str, dir: &str| {
            std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(dir))
        };
        AgentHomes {
            claude: pick("CLAUDE_CONFIG_DIR", ".claude"),
            codex: pick("CODEX_HOME", ".codex"),
            grok: pick("GROK_HOME", ".grok"),
        }
    }

    /// Whether a saved session exists on disk and can be resumed. Answers
    /// `true` when it cannot tell (e.g. an unknown directory layout).
    pub fn session_exists(&self, kind: AgentKind, id: &str) -> bool {
        match kind {
            AgentKind::Claude => any_subdir(&self.claude.join("projects"), |d| d.join(format!("{id}.jsonl")).is_file()),
            AgentKind::Grok => any_subdir(&self.grok.join("sessions"), |d| d.join(id).is_dir()),
            AgentKind::Codex => codex_rollout_exists(&self.codex.join("sessions"), id),
        }
    }
}

fn any_subdir(dir: &Path, found: impl Fn(&Path) -> bool) -> bool {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|e| found(&e.path())),
        Err(_) => true,
    }
}

/// Looks for `sessions/YYYY/MM/DD/rollout-*-<id>.jsonl`.
fn codex_rollout_exists(root: &Path, id: &str) -> bool {
    if !root.is_dir() {
        return true;
    }
    let suffix = format!("-{id}.jsonl");
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                dirs.push(path);
            } else if e.file_name().to_string_lossy().ends_with(&suffix) {
                return true;
            }
        }
    }
    false
}

/// An agent found running in a terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedAgent {
    pub kind: AgentKind,
    /// `None` for a Codex that has not started its first thread yet.
    pub session_id: Option<String>,
    pub pid: u32,
}

/// Scans needed in a row without an agent before it counts as exited.
const MISSES_BEFORE_GONE: u8 = 2;

/// Scans terminals for running agents; see the module docs.
pub struct Detector {
    homes: AgentHomes,
    sys: Option<System>,
    last: HashMap<PaneId, ObservedAgent>,
    misses: HashMap<PaneId, u8>,
    /// Codex threads reported by the hook: pane -> (codex pid, thread id).
    pins: HashMap<PaneId, (u32, String)>,
}

impl Detector {
    pub fn new(homes: AgentHomes) -> Detector {
        let sys = System::new().map_err(|e| tracing::warn!(error = %e, "process inspection unavailable")).ok();
        Detector { homes, sys, last: HashMap::new(), misses: HashMap::new(), pins: HashMap::new() }
    }

    /// Scans the process trees below the given shells (`(pane, shell pid)`) and
    /// returns what changed since the previous scan: `Some` when an agent
    /// appeared or its session changed, `None` once a seen agent has exited.
    ///
    /// `submits` holds when Enter was last pressed in each terminal, and
    /// `reports` the Codex hook reports since the previous scan.
    pub fn scan(&mut self, shells: &[(PaneId, u32)]) -> Vec<(PaneId, Option<ObservedAgent>)> {
        self.scan_with(shells, &HashMap::new(), &[])
    }

    pub(crate) fn scan_with(
        &mut self,
        shells: &[(PaneId, u32)],
        submits: &HashMap<PaneId, Vec<SystemTime>>,
        reports: &[HookReport],
    ) -> Vec<(PaneId, Option<ObservedAgent>)> {
        let found = self.observe(shells, submits, reports);
        let mut changes = Vec::new();
        for (pane, _) in shells {
            match found.get(pane) {
                Some(obs) => {
                    self.misses.remove(pane);
                    if self.last.get(pane) != Some(obs) {
                        self.last.insert(*pane, obs.clone());
                        changes.push((*pane, Some(obs.clone())));
                    }
                }
                None if self.last.contains_key(pane) => {
                    let misses = self.misses.entry(*pane).or_default();
                    *misses += 1;
                    if *misses >= MISSES_BEFORE_GONE {
                        self.last.remove(pane);
                        self.misses.remove(pane);
                        changes.push((*pane, None));
                    }
                }
                None => {}
            }
        }
        let live: HashSet<PaneId> = shells.iter().map(|(p, _)| *p).collect();
        self.last.retain(|p, _| live.contains(p));
        self.misses.retain(|p, _| live.contains(p));
        changes
    }

    fn observe(
        &mut self,
        shells: &[(PaneId, u32)],
        submits: &HashMap<PaneId, Vec<SystemTime>>,
        reports: &[HookReport],
    ) -> HashMap<PaneId, ObservedAgent> {
        let homes = &self.homes;
        let Some(sys) = self.sys.as_mut() else { return HashMap::new() };
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            // Working directories are read below, only for Codex processes.
            ProcessRefreshKind::nothing().without_tasks().with_cmd(UpdateKind::OnlyIfNotSet),
        );
        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for (pid, p) in sys.processes() {
            if let Some(parent) = p.parent() {
                children.entry(parent).or_default().push(*pid);
            }
        }
        for kids in children.values_mut() {
            kids.sort();
        }
        let grok = read_grok_registry(&homes.grok);
        let mut out = HashMap::new();
        let mut codex = Vec::new();

        for (pane, shell) in shells {
            // Breadth first: the agent closest to the shell wins (an agent may
            // itself run other agents as tools, and Codex's daemon is a child
            // of the first Codex).
            let mut queue = VecDeque::from([Pid::from_u32(*shell)]);
            while let Some(pid) = queue.pop_front() {
                match sys.process(pid).and_then(|p| identify(homes, pid, p, &grok)) {
                    Some(Hit::Agent(obs)) => {
                        out.insert(*pane, obs);
                        break;
                    }
                    Some(Hit::Codex(c)) => {
                        codex.push((*pane, c));
                        break;
                    }
                    None => {}
                }
                if let Some(kids) = children.get(&pid) {
                    queue.extend(kids.iter().copied());
                }
            }
        }

        if !codex.is_empty() {
            let pids: Vec<Pid> = codex.iter().map(|(_, c)| Pid::from_u32(c.pid)).collect();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&pids),
                false,
                ProcessRefreshKind::nothing().without_tasks().with_cwd(UpdateKind::Always),
            );
            for (_, c) in &mut codex {
                c.cwd = sys.process(Pid::from_u32(c.pid)).and_then(|p| p.cwd()).map(Path::to_path_buf);
            }
        }
        // Hook reports name the thread exactly; pin it to the right terminal.
        for r in reports {
            let candidates: Vec<(PaneId, &CodexProcess)> = codex
                .iter()
                .filter(|(_, c)| c.cwd.as_deref().is_some_and(|cwd| agent::same_dir(cwd, &r.cwd)))
                .map(|(p, c)| (*p, c))
                .collect();
            if let Some(pane) = attribute(r, &candidates, submits)
                && let Some((_, c)) = candidates.iter().find(|(p, _)| *p == pane)
            {
                tracing::debug!(%pane, thread = %r.session_id, event = %r.event, "codex hook report");
                self.pins.insert(pane, (c.pid, r.session_id.clone()));
            }
        }
        // A pin lasts as long as the Codex process it was made for.
        self.pins.retain(|pane, (pid, _)| codex.iter().any(|(p, c)| p == pane && c.pid == *pid));

        // Latest-started first: a later Codex can only own threads created
        // after it started, so it has fewer candidates.
        codex.sort_by_key(|(_, c)| std::cmp::Reverse(c.started));
        let mut claimed: HashSet<String> = self.pins.values().map(|(_, id)| id.clone()).collect();
        for (pane, c) in codex {
            let pinned = self.pins.get(&pane).map(|(_, id)| id.clone());
            let id = pinned.or_else(|| codex_thread(&homes.codex, &c, &claimed));
            if let Some(id) = &id {
                claimed.insert(id.clone());
            }
            out.insert(pane, ObservedAgent { kind: AgentKind::Codex, session_id: id, pid: c.pid });
        }
        out
    }
}

/// A program running in a terminal, below its shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    /// The process name without `.exe` (`node`), or the agent's name (`Claude Code`).
    pub name: String,
    /// The coding agent this is.
    pub agent: Option<AgentKind>,
    /// The coding agent that runs it: one of the agent's commands, such as a
    /// dev server it left running in the background.
    pub started_by: Option<AgentKind>,
}

impl Detector {
    /// What runs below each shell; see [`Procs::programs_below`].
    pub(crate) fn programs(&mut self, shells: &[(PaneId, u32)]) -> HashMap<PaneId, Vec<Program>> {
        let Some(sys) = self.sys.as_mut() else { return HashMap::new() };
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().without_tasks().with_cmd(UpdateKind::OnlyIfNotSet),
        );
        self.programs_seen(shells)
    }

    /// Like [`Self::programs`], from the processes as the last refresh saw them.
    pub(crate) fn programs_seen(&self, shells: &[(PaneId, u32)]) -> HashMap<PaneId, Vec<Program>> {
        let Some(sys) = self.sys.as_ref() else { return HashMap::new() };
        let procs = Procs::below(sys, shells.iter().map(|(_, shell)| *shell));
        shells.iter().map(|(pane, shell)| (*pane, procs.programs_below(*shell))).collect()
    }
}

/// Helpers nobody started on purpose: console hosts, and the processes
/// wsl.exe always keeps (what runs inside WSL can't be seen).
const HELPERS: &[&str] = &["conhost", "openconsole", "wslhost", "wslrelay"];

/// What the walks below need to know about a process.
#[derive(Debug)]
struct Proc {
    /// Lowercased name without extension (`bash`).
    stem: String,
    /// The name as shown, without `.exe` (`PING`).
    shown: String,
    /// The command line after the program.
    args: Vec<std::ffi::OsString>,
    agent: Option<AgentKind>,
}

/// The processes below some shells, with who started whom.
#[derive(Debug, Default)]
struct Procs {
    procs: HashMap<u32, Proc>,
    children: HashMap<u32, Vec<u32>>,
}

impl Procs {
    /// The given processes and everything below them.
    fn below(sys: &System, roots: impl IntoIterator<Item = u32>) -> Procs {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, kids) in children_of(sys) {
            children.insert(pid.as_u32(), kids.iter().map(|k| k.as_u32()).collect());
        }
        for kids in children.values_mut() {
            kids.sort_unstable();
        }
        let mut procs = HashMap::new();
        let mut queue: Vec<u32> = roots.into_iter().collect();
        while let Some(pid) = queue.pop() {
            if procs.contains_key(&pid) {
                continue;
            }
            let Some(p) = sys.process(Pid::from_u32(pid)) else { continue };
            let proc = Proc {
                stem: stem(p.name()),
                shown: without_exe(&p.name().to_string_lossy()).to_owned(),
                args: p.cmd().get(1..).unwrap_or_default().to_vec(),
                agent: AgentKind::ALL.into_iter().find(|k| runs(p, k.as_str())),
            };
            procs.insert(pid, proc);
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
        Procs { procs, children }
    }

    fn kids(&self, pid: u32) -> impl Iterator<Item = u32> + '_ {
        self.children.get(&pid).into_iter().flatten().copied()
    }

    /// What runs below a terminal's shell: the programs the shell started,
    /// not their own children, except the commands of coding agents (see
    /// [`Self::agent_commands`]). Shell forks (MSYS bash starts programs from
    /// a copy of itself) are looked through.
    fn programs_below(&self, shell: u32) -> Vec<Program> {
        let Some(shell_name) = self.procs.get(&shell).map(|p| p.stem.as_str()) else { return Vec::new() };
        let mut out: Vec<Program> = Vec::new();
        let mut add = |program: Program| {
            if !out.contains(&program) {
                out.push(program);
            }
        };
        let mut seen = HashSet::from([shell]);
        let mut queue: VecDeque<u32> = self.kids(shell).collect();
        while let Some(pid) = queue.pop_front() {
            if !seen.insert(pid) {
                continue;
            }
            let Some(p) = self.procs.get(&pid) else { continue };
            if p.stem == shell_name {
                queue.extend(self.kids(pid));
                continue;
            }
            if HELPERS.contains(&p.stem.as_str()) {
                continue;
            }
            match p.agent {
                Some(kind) => {
                    add(Program { name: kind.label().to_owned(), agent: Some(kind), started_by: None });
                    for name in self.agent_commands(pid) {
                        add(Program { name, agent: None, started_by: Some(kind) });
                    }
                }
                None => add(Program { name: p.shown.clone(), agent: None, started_by: None }),
            }
        }
        out
    }

    /// The commands a coding agent runs, by the programs they run. Agents
    /// run commands through a shell (`bash -c …`, `cmd /c …`), in the
    /// foreground or left in the background; what they start directly is
    /// their own machinery (language servers, MCP servers) and is left out.
    /// A command whose program can't be seen (a builtin, or an MSYS tool,
    /// which leaves the Windows process tree) is named by its shell.
    fn agent_commands(&self, agent: u32) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen = HashSet::from([agent]);
        for command in self.kids(agent) {
            let Some(p) = self.procs.get(&command) else { continue };
            if !SHELLS.contains(&p.stem.as_str()) || interactive_shell(&p.stem, &p.args) {
                continue;
            }
            let mut shell = &p.shown;
            let mut found = Vec::new();
            let mut queue = VecDeque::from([command]);
            while let Some(pid) = queue.pop_front() {
                if !seen.insert(pid) {
                    continue;
                }
                let Some(p) = self.procs.get(&pid) else { continue };
                let stem = p.stem.as_str();
                if SHELLS.contains(&stem) || LAUNCHERS.contains(&stem) {
                    if SHELLS.contains(&stem) {
                        shell = &p.shown;
                    }
                    queue.extend(self.kids(pid));
                } else if !HELPERS.contains(&stem) {
                    found.push(p.agent.map(|k| k.label().to_owned()).unwrap_or_else(|| p.shown.clone()));
                }
            }
            if found.is_empty() {
                found.push(shell.clone());
            }
            for name in found {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }
}

/// A process name without a Windows `.exe` (any case): `PING.EXE` -> `PING`.
fn without_exe(name: &str) -> &str {
    match name.len().checked_sub(4).and_then(|i| Some((i, name.get(i..)?))) {
        Some((i, ext)) if ext.eq_ignore_ascii_case(".exe") => &name[..i],
        _ => name,
    }
}

/// Which terminal a hook report belongs to: the only Codex in that folder, or
/// the one that most recently submitted a prompt (or started) before the
/// event. The pane id the hook saw is used only to break a tie.
fn attribute(
    r: &HookReport,
    candidates: &[(PaneId, &CodexProcess)],
    submits: &HashMap<PaneId, Vec<SystemTime>>,
) -> Option<PaneId> {
    match candidates {
        [] => None,
        [(pane, _)] => Some(*pane),
        _ => {
            let latest = r.at + Duration::from_millis(500);
            let mut ranked: Vec<(SystemTime, PaneId)> = candidates
                .iter()
                .filter_map(|(pane, c)| {
                    let started = UNIX_EPOCH + Duration::from_secs(c.started);
                    let typed = submits.get(pane).into_iter().flatten().copied();
                    typed.chain([started]).filter(|t| *t <= latest).max().map(|t| (t, *pane))
                })
                .collect();
            ranked.sort();
            match ranked.as_slice() {
                [.., (a, _), (b, pane)] if b > a => Some(*pane),
                [(_, pane)] => Some(*pane),
                _ => r.env_pane.filter(|e| candidates.iter().any(|(p, _)| p == e)),
            }
        }
    }
}

/// Shells. The terminal's own shell may have any arguments; a shell started
/// inside it counts as idle only without a script or command to run (`bash`,
/// not `bash build.sh` or `pwsh -Command …`).
pub(crate) const SHELLS: &[&str] =
    &["bash", "sh", "dash", "zsh", "fish", "ksh", "mksh", "tcsh", "csh", "nu", "elvish", "pwsh", "powershell", "cmd"];

/// Programs that only start a shell and wait for it: MSYS2's launcher runs
/// `env … bash --login`, and console hosts.
const LAUNCHERS: &[&str] = &["env", "conhost", "openconsole"];

/// Whether a nested shell was started to sit at a prompt: only flags, and
/// none that runs a command or script. Unknown flag values count as busy.
fn interactive_shell(name: &str, args: &[std::ffi::OsString]) -> bool {
    args.iter().all(|a| {
        let a = a.to_string_lossy().to_lowercase();
        match name {
            "cmd" => a.starts_with('/') && !matches!(a.as_str(), "/c" | "/k" | "/r"),
            // PowerShell accepts any unambiguous prefix of a parameter name.
            "pwsh" | "powershell" => a.strip_prefix('-').is_some_and(|f| {
                let runs = ["command", "file", "encodedcommand", "commandwithargs"].iter().any(|p| p.starts_with(f));
                !f.is_empty() && !runs && f != "ec" && f != "cwa"
            }),
            // `-c` runs a command, also inside a cluster such as `-lc`.
            _ => a.starts_with("--") || a.strip_prefix('-').is_some_and(|f| !f.is_empty() && !f.contains('c')),
        }
    })
}

/// The terminals (`(pane, shell pid)`) with only an idle shell in them, so
/// restarting them loses no work. Anything unknown counts as busy, such as
/// WSL, whose programs Windows cannot see.
pub(crate) fn idle_shells(shells: &[(PaneId, u32)]) -> HashSet<PaneId> {
    if shells.is_empty() {
        return HashSet::new();
    }
    let Ok(mut sys) = System::new() else { return HashSet::new() };
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
    let children = children_of(&sys);
    let trees: Vec<(PaneId, Vec<Pid>)> = shells.iter().map(|(pane, shell)| (*pane, tree(&children, *shell))).collect();
    // Command lines are slow to read on Windows: only for these processes.
    let pids: Vec<Pid> = trees.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        ProcessRefreshKind::nothing().without_tasks().with_cmd(UpdateKind::Always),
    );
    let idle = |tree: &[Pid]| {
        tree.iter().enumerate().all(|(i, pid)| {
            let Some(p) = sys.process(*pid) else { return false };
            let name = stem(p.name());
            let shell = SHELLS.contains(&name.as_str());
            let args = p.cmd().get(1..).unwrap_or_default();
            LAUNCHERS.contains(&name.as_str()) || (shell && (i == 0 || interactive_shell(&name, args)))
        })
    };
    trees.iter().filter(|(_, tree)| idle(tree)).map(|(pane, _)| *pane).collect()
}

/// Each process's children.
fn children_of(sys: &System) -> HashMap<Pid, Vec<Pid>> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    children
}

/// A process and everything below it, without looping on reused process ids.
fn tree(children: &HashMap<Pid, Vec<Pid>>, root: u32) -> Vec<Pid> {
    let mut seen = vec![Pid::from_u32(root)];
    let mut i = 0;
    while let Some(pid) = seen.get(i).copied() {
        for kid in children.get(&pid).into_iter().flatten() {
            if !seen.contains(kid) {
                seen.push(*kid);
            }
        }
        i += 1;
    }
    seen
}

/// The given shells and every process below them that is running now.
pub(crate) fn process_trees(shells: &[u32]) -> Vec<u32> {
    let Ok(mut sys) = System::new() else { return shells.to_vec() };
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
    let children = children_of(&sys);
    let mut out: Vec<u32> = shells.iter().flat_map(|s| tree(&children, *s)).map(|p| p.as_u32()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Waits until none of `pids` runs any more, for at most `timeout`; returns
/// whether they all exited. A process id taken over by a new process counts
/// as exited.
pub(crate) fn wait_for_exit(pids: &[u32], timeout: Duration) -> bool {
    let Ok(mut sys) = System::new() else { return false };
    let pids: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    let refresh = |sys: &mut System| {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
    };
    refresh(&mut sys);
    let started: HashMap<Pid, u64> = pids.iter().filter_map(|p| Some((*p, sys.process(*p)?.start_time()))).collect();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let alive = started.iter().any(|(pid, start)| sys.process(*pid).is_some_and(|p| p.start_time() == *start));
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
        refresh(&mut sys);
    }
}

/// A process working in a folder, see [`processes_in`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderProcess {
    pub pid: u32,
    /// Its program's file name (`cmd.exe`).
    pub name: String,
    start: u64,
}

/// The processes whose working directory is in `dir` or below, or whose
/// program is: Windows cannot delete a folder while they run. Leaves out
/// this process, its own children, and the processes of `shells` (the
/// terminals' shells, which are stopped another way) and below them.
pub fn processes_in(dir: &Path, shells: &[u32]) -> Vec<FolderProcess> {
    let Ok(mut sys) = System::new() else { return Vec::new() };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks().with_cwd(UpdateKind::Always).with_exe(UpdateKind::Always),
    );
    let children = children_of(&sys);
    let skip: HashSet<Pid> = shells.iter().flat_map(|s| tree(&children, *s)).collect();
    let me = Pid::from_u32(std::process::id());
    let within = |path: Option<&Path>| path.is_some_and(|p| crate::git::is_within(p, dir));
    let mut found: Vec<FolderProcess> = sys
        .processes()
        .iter()
        .filter(|(pid, p)| **pid != me && p.parent() != Some(me) && !skip.contains(pid))
        .filter(|(_, p)| within(p.cwd()) || within(p.exe()))
        .map(|(pid, p)| FolderProcess {
            pid: pid.as_u32(),
            name: p.name().to_string_lossy().into_owned(),
            start: p.start_time(),
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    found
}

/// Ends the given processes, except ones that exited meanwhile (their id
/// may belong to another process by now). Returns the ids of those ended,
/// to wait for (see [`crate::LeftDir::wait`]).
pub fn end_processes(procs: &[FolderProcess]) -> Vec<u32> {
    let Ok(mut sys) = System::new() else { return Vec::new() };
    let pids: Vec<Pid> = procs.iter().map(|p| Pid::from_u32(p.pid)).collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    procs
        .iter()
        .filter(|f| sys.process(Pid::from_u32(f.pid)).is_some_and(|p| p.start_time() == f.start && p.kill()))
        .map(|f| f.pid)
        .collect()
}

/// Waits up to `grace` for the process `pid` (which `process` is a handle
/// to) to exit. If it does not, ends it and every process below it, and
/// returns true.
#[cfg(windows)]
pub(crate) fn end_unless_exited(pid: u32, process: &std::os::windows::io::OwnedHandle, grace: Duration) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

    let handle = process.as_raw_handle();
    let millis = u32::try_from(grace.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: `process` is an open process handle.
    if unsafe { WaitForSingleObject(handle, millis) } != WAIT_TIMEOUT {
        return false;
    }
    // While the handle is open, `pid` is still this process.
    if let Ok(mut sys) = System::new() {
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
        let below = tree(&children_of(&sys), pid);
        for p in below.iter().skip(1).filter_map(|p| sys.process(*p)) {
            p.kill();
        }
    }
    // SAFETY: as above.
    unsafe { TerminateProcess(handle, 1) };
    true
}

/// Lowercased file stem of a program path (`/usr/bin/Codex.exe` -> `codex`).
fn stem(s: &std::ffi::OsStr) -> String {
    Path::new(s).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Whether the process runs `program`, directly or through a script runtime.
fn runs(p: &Process, program: &str) -> bool {
    let name = stem(p.name());
    let cmd = p.cmd();
    if name == program || cmd.first().is_some_and(|a| stem(a) == program) {
        return true;
    }
    let runtime = ["node", "bun", "deno"].contains(&name.as_str());
    runtime && cmd.get(1).is_some_and(|a| stem(a) == program || a.to_string_lossy().to_lowercase().contains(program))
}

/// A Codex terminal whose thread is resolved after all panes are scanned.
struct CodexProcess {
    pid: u32,
    /// Seconds since the epoch.
    started: u64,
    cwd: Option<PathBuf>,
    cmd: Vec<String>,
}

enum Hit {
    Agent(ObservedAgent),
    Codex(CodexProcess),
}

fn identify(homes: &AgentHomes, pid: Pid, p: &Process, grok: &HashMap<u32, (String, Option<u64>)>) -> Option<Hit> {
    let raw = pid.as_u32();
    if let Some((id, start)) = grok.get(&raw)
        && start_matches(raw, *start)
    {
        return Some(Hit::Agent(ObservedAgent { kind: AgentKind::Grok, session_id: Some(id.clone()), pid: raw }));
    }
    if runs(p, "claude")
        && let Some((id, start)) = read_claude_registry(&homes.claude, raw)
        && start_matches(raw, start)
    {
        return Some(Hit::Agent(ObservedAgent { kind: AgentKind::Claude, session_id: Some(id), pid: raw }));
    }
    if runs(p, "codex") {
        return Some(Hit::Codex(CodexProcess {
            pid: raw,
            started: p.start_time(),
            cwd: None,
            cmd: p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect(),
        }));
    }
    None
}

#[derive(Deserialize)]
struct ClaudeRegistry {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "procStart", default)]
    proc_start: Option<serde_json::Value>,
}

/// `(session id, process start)` from `~/.claude/sessions/<pid>.json`.
fn read_claude_registry(claude_home: &Path, pid: u32) -> Option<(String, Option<u64>)> {
    let text = std::fs::read_to_string(claude_home.join("sessions").join(format!("{pid}.json"))).ok()?;
    let reg: ClaudeRegistry = serde_json::from_str(&text).ok()?;
    let start = reg.proc_start.and_then(|v| match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    });
    Some((reg.session_id, start))
}

#[derive(Deserialize)]
struct GrokEntry {
    session_id: String,
    pid: u32,
    #[serde(default)]
    pid_start: Option<u64>,
}

/// `pid -> (session id, process start)` from `~/.grok/active_sessions.json`.
fn read_grok_registry(grok_home: &Path) -> HashMap<u32, (String, Option<u64>)> {
    let Ok(text) = std::fs::read_to_string(grok_home.join("active_sessions.json")) else { return HashMap::new() };
    let entries: Vec<GrokEntry> = serde_json::from_str(&text).unwrap_or_default();
    entries.into_iter().map(|e| (e.pid, (e.session_id, e.pid_start))).collect()
}

/// Guards against a reused pid: on Linux both registries record the process
/// start time in clock ticks since boot (`/proc/<pid>/stat` field 22).
fn start_matches(pid: u32, recorded: Option<u64>) -> bool {
    #[cfg(target_os = "linux")]
    if let (Some(recorded), Some(actual)) = (recorded, linux_start_ticks(pid)) {
        return recorded == actual;
    }
    let _ = (pid, recorded);
    true
}

#[cfg(target_os = "linux")]
fn linux_start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; starttime is field 22 overall.
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// The thread a Codex terminal is working on (see the module docs).
fn codex_thread(codex_home: &Path, c: &CodexProcess, claimed: &HashSet<String>) -> Option<String> {
    let resumed = resumed_thread(&c.cmd);
    #[cfg(target_os = "linux")]
    {
        let own: Vec<String> =
            linux_codex_threads(codex_home, c.pid).into_iter().filter(|id| !claimed.contains(id)).collect();
        if let Some(id) = pick_codex_thread(codex_home, own) {
            return Some(id);
        }
    }
    let cwd = c.cwd.as_deref()?;
    match codex_db_threads(codex_home, cwd, c.started, resumed.as_deref()) {
        Some(ids) => ids.into_iter().find(|id| !claimed.contains(id)).or(resumed),
        None => {
            // No readable thread table: use the rollout files.
            let since = UNIX_EPOCH + std::time::Duration::from_secs(c.started);
            agent::scan_codex_sessions_in(&codex_home.join("sessions"), since)
                .into_iter()
                .rev()
                .find(|r| !claimed.contains(&r.id) && agent::same_dir(&r.cwd, cwd))
                .map(|r| r.id)
                .or(resumed)
        }
    }
}

/// The thread named on a `codex resume [options] <id>` command line.
fn resumed_thread(cmd: &[String]) -> Option<String> {
    let at = cmd.iter().position(|a| a == "resume")?;
    cmd[at + 1..].iter().find(|a| looks_like_uuid(a)).cloned()
}

/// Candidate threads from Codex's state database, best first: user threads in
/// `cwd` created after `started` (or the resumed one), most recently updated
/// first. `None` when the database cannot be read.
fn codex_db_threads(codex_home: &Path, cwd: &Path, started: u64, resumed: Option<&str>) -> Option<Vec<String>> {
    let conn = open_codex_state(codex_home)?;
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let since_ms = started.saturating_sub(2) as i64 * 1000;
    let params = rusqlite::params![cwd.to_string_lossy(), canonical.to_string_lossy(), since_ms, resumed.unwrap_or("")];
    // Newer Codex marks terminal threads with an originator; older ones with source = 'cli'.
    const QUERIES: [&str; 2] = [
        "SELECT id FROM threads WHERE archived = 0 AND cwd IN (?1, ?2) \
           AND source NOT LIKE '{%' AND (originator = 'codex-tui' OR source = 'cli') \
           AND COALESCE(thread_source, 'user') = 'user' \
           AND (COALESCE(created_at_ms, created_at * 1000) >= ?3 OR id = ?4) \
         ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC",
        "SELECT id FROM threads WHERE archived = 0 AND cwd IN (?1, ?2) AND source NOT LIKE '{%' \
           AND (created_at * 1000 >= ?3 OR id = ?4) ORDER BY updated_at DESC",
    ];
    for sql in QUERIES {
        let Ok(mut stmt) = conn.prepare(sql) else { continue };
        if let Ok(rows) = stmt.query_map(params, |r| r.get::<_, String>(0)) {
            return Some(rows.flatten().collect());
        }
    }
    None
}

fn open_codex_state(codex_home: &Path) -> Option<rusqlite::Connection> {
    // Newest `state_<n>.sqlite`.
    let db = std::fs::read_dir(codex_home)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("state_") && n.ends_with(".sqlite"))
        })
        .max()?;
    rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// Thread ids whose writer lock or rollout file the process holds open.
#[cfg(target_os = "linux")]
fn linux_codex_threads(codex_home: &Path, pid: u32) -> Vec<String> {
    let home = std::fs::canonicalize(codex_home).unwrap_or_else(|_| codex_home.to_path_buf());
    let locks = home.join("thread-writer-locks");
    let sessions = home.join("sessions");
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    let mut ids = Vec::new();
    for fd in fds.flatten() {
        let Ok(target) = std::fs::read_link(fd.path()) else { continue };
        if let Some(id) = codex_thread_of_path(&target, &locks, &sessions)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    ids
}

/// The thread id named by a lock (`<id>.lock`) or rollout (`rollout-<time>-<id>.jsonl`) path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn codex_thread_of_path(path: &Path, locks: &Path, sessions: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if path.starts_with(locks) {
        return name.strip_suffix(".lock").filter(|id| looks_like_uuid(id)).map(str::to_owned);
    }
    if path.starts_with(sessions) && name.starts_with("rollout-") {
        let stem = name.strip_suffix(".jsonl")?;
        let id = stem.get(stem.len().checked_sub(36)?..)?;
        return looks_like_uuid(id).then(|| id.to_owned());
    }
    None
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars()
            .enumerate()
            .all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// Chooses the main thread when a process holds several (subagents, earlier
/// threads): Codex's state database knows which threads were spawned by
/// others and which was updated last.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pick_codex_thread(codex_home: &Path, ids: Vec<String>) -> Option<String> {
    if ids.len() <= 1 {
        return ids.into_iter().next();
    }
    if let Some(id) = newest_main_thread(codex_home, &ids) {
        return Some(id);
    }
    // Fall back to the most recently touched lock file.
    let mtime = |id: &String| {
        std::fs::metadata(codex_home.join("thread-writer-locks").join(format!("{id}.lock")))
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH)
    };
    ids.into_iter().max_by_key(mtime)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn newest_main_thread(codex_home: &Path, ids: &[String]) -> Option<String> {
    let conn = open_codex_state(codex_home)?;
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id FROM threads WHERE id IN ({placeholders}) \
         AND id NOT IN (SELECT child_thread_id FROM thread_spawn_edges) \
         ORDER BY updated_at DESC LIMIT 1"
    );
    conn.query_row(&sql, rusqlite::params_from_iter(ids.iter()), |r| r.get(0)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_shells_with_a_command_are_busy() {
        let idle = |name: &str, args: &[&str]| {
            let args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
            interactive_shell(name, &args)
        };
        assert!(idle("bash", &[]));
        assert!(idle("bash", &["-i", "-l"]));
        assert!(idle("bash", &["--login"]));
        assert!(!idle("bash", &["build.sh"]));
        assert!(!idle("bash", &["-lc", "make"]));
        assert!(!idle("sh", &["-"]));
        assert!(idle("pwsh", &["-NoLogo", "-NoProfile"]));
        assert!(!idle("pwsh", &["-Comm", "Get-Date"]));
        assert!(!idle("pwsh", &["-f", "x.ps1"]));
        assert!(!idle("powershell", &["-EC", "AAAA"]));
        assert!(!idle("powershell", &["Get-Date"]));
        assert!(idle("cmd", &["/d", "/q"]));
        assert!(!idle("cmd", &["/C", "dir"]));
        assert!(!idle("cmd", &["/k", "vcvars.bat"]));
    }

    /// A process table from `(pid, parent, command line, agent)`.
    fn procs(entries: &[(u32, u32, &str, Option<AgentKind>)]) -> Procs {
        let mut t = Procs::default();
        for (pid, parent, line, agent) in entries {
            let mut words = line.split_whitespace();
            let program = words.next().unwrap();
            let proc = Proc {
                stem: stem(program.as_ref()),
                shown: without_exe(program).to_owned(),
                args: words.map(Into::into).collect(),
                agent: *agent,
            };
            t.procs.insert(*pid, proc);
            t.children.entry(*parent).or_default().push(*pid);
        }
        t
    }

    /// Laid out as seen on Windows 11 with Git Bash and Claude Code (2026-11-16).
    #[test]
    fn programs_below_a_shell() {
        use AgentKind::Claude;
        let t = procs(&[
            // The terminal's Git Bash, and the copy of itself it starts programs from.
            (10, 1, "bash.exe -i -l", None),
            (11, 10, "bash.exe -i -l", None),
            (12, 11, "conhost.exe 0x4", None),
            (13, 11, "node.exe vite", None),
            (14, 13, "node.exe esbuild", None),
            (20, 11, "claude.exe", Some(Claude)),
            // Claude's own machinery, which it starts directly.
            (21, 20, "rustup.exe", None),
            (22, 21, "rust-analyzer.exe", None),
            (23, 20, "node.exe mcp-server.js", None),
            (24, 20, "bash.exe -i", None),
            // Its commands: a dev server left running, a PowerShell command,
            // and an MSYS tool whose process has left the tree.
            (30, 20, "bash.exe -c npm run dev", None),
            (31, 30, "bash.exe -c npm run dev", None),
            (32, 31, "node.exe vite", None),
            (40, 20, "cmd.exe /d /s /c powershell", None),
            (41, 40, "conhost.exe 0x4", None),
            (42, 40, "powershell.exe -NoProfile -Command Start-Sleep 9", None),
            (50, 20, "bash.exe -c sleep 90", None),
            (51, 50, "bash.exe -c sleep 90", None),
        ]);
        let p = |name: &str, agent, started_by| Program { name: name.into(), agent, started_by };
        assert_eq!(
            t.programs_below(10),
            [
                p("node", None, None),
                p("Claude Code", Some(Claude), None),
                p("node", None, Some(Claude)),
                p("powershell", None, Some(Claude)),
                p("bash", None, Some(Claude)),
            ]
        );
        let idle = procs(&[(10, 1, "bash.exe -i", None), (11, 10, "bash.exe -i", None), (12, 11, "conhost.exe", None)]);
        assert_eq!(idle.programs_below(10), []);
        assert_eq!(t.programs_below(99), [], "a shell that is gone");
        // Process ids reused into a loop.
        let looped = procs(&[(10, 11, "bash.exe -i", None), (11, 10, "bash.exe -i", None)]);
        assert_eq!(looped.programs_below(10), []);
    }

    fn homes(dir: &Path) -> AgentHomes {
        AgentHomes { claude: dir.join("claude"), codex: dir.join("codex"), grok: dir.join("grok") }
    }

    #[test]
    fn reads_registries() {
        let tmp = tempfile::tempdir().unwrap();
        let h = homes(tmp.path());
        std::fs::create_dir_all(h.claude.join("sessions")).unwrap();
        std::fs::write(
            h.claude.join("sessions/4242.json"),
            r#"{"pid":4242,"sessionId":"e60a95a6-bfb3-4082-aa6e-20bebb3377ff","procStart":"1830166","kind":"interactive"}"#,
        )
        .unwrap();
        assert_eq!(
            read_claude_registry(&h.claude, 4242),
            Some(("e60a95a6-bfb3-4082-aa6e-20bebb3377ff".into(), Some(1830166)))
        );
        assert_eq!(read_claude_registry(&h.claude, 1), None);

        std::fs::create_dir_all(&h.grok).unwrap();
        std::fs::write(
            h.grok.join("active_sessions.json"),
            r#"[{"session_id":"01a1e22a-903b-72b0-96f5-568fd03ad5a4","pid":77,"pid_start":1832929,"cwd":"/x","opened_at":"t"}]"#,
        )
        .unwrap();
        let grok = read_grok_registry(&h.grok);
        assert_eq!(grok.get(&77), Some(&("01a1e22a-903b-72b0-96f5-568fd03ad5a4".to_owned(), Some(1832929))));
        std::fs::write(h.grok.join("active_sessions.json"), "not json").unwrap();
        assert!(read_grok_registry(&h.grok).is_empty());
    }

    #[test]
    fn session_existence() {
        let tmp = tempfile::tempdir().unwrap();
        let h = homes(tmp.path());
        // Unknown layout: assume it exists and let the agent report errors.
        assert!(h.session_exists(AgentKind::Claude, "abc"));
        std::fs::create_dir_all(h.claude.join("projects/-home-me-proj")).unwrap();
        assert!(!h.session_exists(AgentKind::Claude, "abc"));
        std::fs::write(h.claude.join("projects/-home-me-proj/abc.jsonl"), "{}").unwrap();
        assert!(h.session_exists(AgentKind::Claude, "abc"));

        std::fs::create_dir_all(h.grok.join("sessions/%2Fhome%2Fme/s1")).unwrap();
        assert!(h.session_exists(AgentKind::Grok, "s1"));
        assert!(!h.session_exists(AgentKind::Grok, "s2"));

        let id = "01a1e121-e618-7503-83d2-6767497fb5d7";
        std::fs::create_dir_all(h.codex.join("sessions/2026/11/15")).unwrap();
        assert!(!h.session_exists(AgentKind::Codex, id));
        std::fs::write(h.codex.join(format!("sessions/2026/11/15/rollout-2026-11-15T16-34-17-{id}.jsonl")), "")
            .unwrap();
        assert!(h.session_exists(AgentKind::Codex, id));
    }

    #[test]
    fn codex_paths() {
        let locks = Path::new("/h/.codex/thread-writer-locks");
        let sessions = Path::new("/h/.codex/sessions");
        let id = "01a1e20f-eb7a-7971-a81c-602f1b0f42f0";
        assert_eq!(codex_thread_of_path(&locks.join(format!("{id}.lock")), locks, sessions), Some(id.into()));
        let rollout = sessions.join(format!("2026/11/15/rollout-2026-11-15T20-54-00-{id}.jsonl"));
        assert_eq!(codex_thread_of_path(&rollout, locks, sessions), Some(id.into()));
        assert_eq!(codex_thread_of_path(Path::new("/h/.codex/logs_2.sqlite"), locks, sessions), None);
        assert_eq!(codex_thread_of_path(&locks.join("junk.lock"), locks, sessions), None);
    }

    fn codex_state(home: &Path, rows: &[(&str, &str, i64, i64, &str, &str, i64)]) {
        std::fs::create_dir_all(home).unwrap();
        let conn = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, created_at INTEGER, created_at_ms INTEGER,
               updated_at INTEGER, updated_at_ms INTEGER, source TEXT, thread_source TEXT,
               originator TEXT, cwd TEXT, archived INTEGER);
             CREATE TABLE thread_spawn_edges (parent_thread_id TEXT, child_thread_id TEXT, status TEXT);",
        )
        .unwrap();
        for (id, cwd, created, updated, source, thread_source, archived) in rows {
            conn.execute(
                "INSERT INTO threads VALUES (?1, ?2, ?2 * 1000, ?3, ?3 * 1000, ?4, ?5, 'codex-tui', ?6, ?7)",
                rusqlite::params![id, created, updated, source, thread_source, cwd, archived],
            )
            .unwrap();
        }
    }

    #[test]
    fn codex_threads_from_state_db() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex");
        let proj = tmp.path().join("proj");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let (p, o) = (proj.to_str().unwrap(), other.to_str().unwrap());
        let id = |n: u8| format!("01a1e20f-eb7a-7971-a81c-602f1b0f42{n:02x}");
        let (old, new_a, new_b, sub, arch, elsewhere) = (id(1), id(2), id(3), id(4), id(5), id(6));
        codex_state(
            &home,
            &[
                (&old, p, 100, 500, "vscode", "user", 0),
                (&new_a, p, 1_000, 1_200, "vscode", "user", 0),
                (&new_b, p, 1_100, 1_300, "vscode", "user", 0),
                (&sub, p, 1_150, 1_400, r#"{"subagent":{"other":"guardian"}}"#, "guardian_review", 0),
                (&arch, p, 1_050, 1_500, "vscode", "user", 1),
                (&elsewhere, o, 1_050, 1_600, "vscode", "user", 0),
            ],
        );
        // Started at t=1000: only threads created since then, in its folder,
        // excluding subagents and archived ones, newest update first.
        assert_eq!(codex_db_threads(&home, &proj, 1_000, None), Some(vec![new_b.clone(), new_a.clone()]));
        // A resumed thread is a candidate even though it is older.
        assert_eq!(
            codex_db_threads(&home, &proj, 1_000, Some(&old)),
            Some(vec![new_b.clone(), new_a.clone(), old.clone()])
        );
        // Claimed threads go to the next candidate.
        let c = CodexProcess { pid: 0, started: 1_000, cwd: Some(proj.clone()), cmd: vec!["codex".into()] };
        let claimed: HashSet<String> = [new_b.clone()].into();
        assert_eq!(codex_thread(&home, &c, &claimed), Some(new_a.clone()));
        // No database at all: nothing found, but a resumed id still counts.
        let empty = tmp.path().join("no-codex");
        let r = CodexProcess { cmd: vec!["codex".into(), "resume".into(), "--yolo".into(), old.clone()], ..c };
        assert_eq!(codex_thread(&empty, &r, &HashSet::new()), Some(old.clone()));
    }

    #[test]
    fn resume_argument() {
        let id = "01a1e20f-eb7a-7971-a81c-602f1b0f42f0";
        let cmd = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(resumed_thread(&cmd(&["codex", "resume", "-c", "x=1", id])), Some(id.into()));
        assert_eq!(resumed_thread(&cmd(&["codex", "--yolo"])), None);
        assert_eq!(resumed_thread(&cmd(&["codex", "resume", "--last"])), None);
    }

    #[test]
    fn hook_reports_go_to_the_terminal_that_just_submitted() {
        let t = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
        let proc_at = |started: u64| CodexProcess { pid: 1, started, cwd: None, cmd: vec![] };
        let (a, b) = (proc_at(1_000), proc_at(1_010));
        let candidates = [(PaneId(1), &a), (PaneId(2), &b)];
        let report = |at: SystemTime, env_pane: Option<PaneId>| HookReport {
            session_id: "s".into(),
            cwd: "/w".into(),
            event: "UserPromptSubmit".into(),
            at,
            env_pane,
        };
        let mut submits = HashMap::new();
        submits.insert(PaneId(1), vec![t(1_100), t(1_200)]);
        submits.insert(PaneId(2), vec![t(1_150), t(1_300)]);

        // Pane 1 pressed Enter at 1200, just before the event at 1200.2; pane 2's
        // Enter at 1300 came after it and does not count.
        let at = t(1_200) + Duration::from_millis(200);
        assert_eq!(attribute(&report(at, None), &candidates, &submits), Some(PaneId(1)));
        assert_eq!(attribute(&report(t(1_301), None), &candidates, &submits), Some(PaneId(2)));
        // A SessionStart right after a Codex started belongs to that one.
        assert_eq!(attribute(&report(t(1_011), None), &candidates, &HashMap::new()), Some(PaneId(2)));
        // A single Codex in the folder needs no timing.
        assert_eq!(attribute(&report(t(5), None), &candidates[..1], &submits), Some(PaneId(1)));
        // Nothing to go on: the pane id the hook saw breaks the tie.
        let same = [(PaneId(1), &a), (PaneId(2), &a)];
        assert_eq!(attribute(&report(t(999), Some(PaneId(2))), &same, &HashMap::new()), Some(PaneId(2)));
        assert_eq!(attribute(&report(t(999), None), &same, &HashMap::new()), None);
    }

    #[test]
    fn exit_needs_consecutive_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let mut d = Detector::new(homes(tmp.path()));
        let pane = PaneId(1);
        let obs = ObservedAgent { kind: AgentKind::Claude, session_id: Some("x".into()), pid: 9 };
        d.last.insert(pane, obs);
        // Our own pid has no agent below it.
        let me = std::process::id();
        assert!(d.scan(&[(pane, me)]).is_empty(), "first miss is tolerated");
        assert_eq!(d.scan(&[(pane, me)]), vec![(pane, None)]);
        assert!(d.scan(&[(pane, me)]).is_empty(), "reported once");
    }
}
