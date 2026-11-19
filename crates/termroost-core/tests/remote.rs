//! Projects on a remote host. The host is this computer: the core reaches it
//! by running this test binary as the remote helper (its `remote_entry`
//! test, with `REMOTE_ENV` set) instead of over SSH, and that helper starts
//! terminals' helpers the same way (`host_entry`, with `HOST_ENV` set).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use termroost_core::*;

const REMOTE_ENV: &str = "TERMROOST_TEST_REMOTE";
const HOST_ENV: &str = "TERMROOST_TEST_REMOTE_HOST";
const HOST: &str = "testhost";

#[test]
fn remote_entry() {
    if std::env::var_os(REMOTE_ENV).is_some() {
        let helper = HostCommand {
            program: std::env::current_exe().unwrap(),
            args: ["host_entry", "--exact", "--nocapture", "--test-threads=1"].map(String::from).to_vec(),
            env: vec![(HOST_ENV.to_owned(), "1".to_owned())],
        };
        std::process::exit(remote::serve(helper));
    }
}

#[test]
fn host_entry() {
    if std::env::var_os(HOST_ENV).is_some() {
        std::process::exit(host::serve());
    }
}

/// Where the remote helper keeps its files, in the test's folder.
fn remote_root(dir: &Path) -> PathBuf {
    dir.join("remote-root")
}

fn open(dir: &Path) -> Core {
    let agents = dir.join("agent-homes");
    let daemon = HostCommand {
        program: std::env::current_exe().unwrap(),
        args: ["remote_entry", "--exact", "--nocapture", "--test-threads=1"].map(String::from).to_vec(),
        env: vec![
            (REMOTE_ENV.to_owned(), "1".to_owned()),
            ("TERMINALROOSTER_REMOTE_DIR".to_owned(), remote_root(dir).display().to_string()),
            ("CLAUDE_CONFIG_DIR".to_owned(), agents.join("claude").display().to_string()),
            ("CODEX_HOME".to_owned(), agents.join("codex").display().to_string()),
            ("GROK_HOME".to_owned(), agents.join("grok").display().to_string()),
        ],
    };
    Core::open(Config {
        data_dir: Some(dir.join("data")),
        autosave_interval: Duration::from_millis(200),
        agent_homes: AgentHomes {
            claude: agents.join("claude"),
            codex: agents.join("codex"),
            grok: agents.join("grok"),
        },
        remote: remote::Transport::Command(daemon),
        ..Default::default()
    })
    .expect("open core")
}

/// Ends the terminals' helpers a test leaves behind, failed or not.
struct Helpers(PathBuf);

impl Drop for Helpers {
    fn drop(&mut self) {
        let Ok(clients) = std::fs::read_dir(remote_root(&self.0).join("clients")) else { return };
        for client in clients.flatten() {
            let Ok(entries) = std::fs::read_dir(client.path().join("hosts")) else { continue };
            for entry in entries.flatten() {
                let Ok(info) = std::fs::read(entry.path()) else { continue };
                let pid = serde_json::from_slice::<serde_json::Value>(&info).ok().and_then(|v| v["pid"].as_u64());
                if let Some(pid) = pid {
                    let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn pump(core: &mut Core) -> Vec<Event> {
    let events = core.events();
    let mut seen = Vec::new();
    while let Ok(e) = events.try_recv() {
        core.apply_event(&e);
        seen.push(e);
    }
    seen
}

fn screen_text(core: &Core, pane: PaneId) -> String {
    core.terminal(pane).map(|h| h.with_content(|c| c.text())).unwrap_or_default()
}

fn wait_for_text(core: &mut Core, pane: PaneId, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        pump(core);
        let text = screen_text(core, pane);
        if text.contains(needle) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?} on the screen:\n{text}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn wait_until(core: &mut Core, what: &str, mut done: impl FnMut(&mut Core) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(core) {
        pump(core);
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A remote project in `work`, with `sh` as its shell; its first terminal.
fn remote_project(core: &mut Core, work: &Path) -> (ProjectId, PaneId) {
    let project = match core
        .dispatch(Command::CreateProject {
            name: "remote".into(),
            working_dir: work.to_path_buf(),
            shell: Some("sh".into()),
            host: Some(HOST.into()),
        })
        .unwrap()
    {
        Outcome::Project(p) => p,
        other => panic!("{other:?}"),
    };
    let pane = *core.state().panes.keys().next().unwrap();
    (project, pane)
}

fn type_line(core: &Core, pane: PaneId, line: &str) {
    core.terminal(pane).unwrap().write(format!("{line}\r"));
}

#[test]
fn a_remote_project_runs_its_terminals_and_files_on_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let _helpers = Helpers(dir.path().to_path_buf());
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("notes.txt"), "from the host").unwrap();
    let mut core = open(dir.path());
    let (project, pane) = remote_project(&mut core, &work);
    assert_eq!(core.state().project(project).unwrap().host.as_deref(), Some(HOST));

    type_line(&core, pane, "echo \"pane=$TERMINALROOSTER_PANE_ID\"; pwd");
    wait_for_text(&mut core, pane, &format!("pane={pane}"));
    wait_for_text(&mut core, pane, &work.display().to_string());
    assert_eq!(core.remote_status(HOST), RemoteStatus::Connected);
    let handle = core.terminal(pane).unwrap();
    assert_eq!(handle.remote().map(|h| h.name()), Some(HOST));
    assert_eq!(handle.shell_pid(), None, "its shell is not a process of this computer");

    // What runs in it is seen by the host.
    type_line(&core, pane, "sleep 30");
    wait_until(&mut core, "the host to see sleep running", |core| {
        core.terminal(pane).unwrap().activity().programs.iter().any(|p| p.name == "sleep")
    });

    // Its files, through the host.
    let machine = core.project_machine(project);
    assert_eq!(machine.host(), Some(HOST));
    let listed = machine.list_dirs(vec![work.clone()]);
    let names: Vec<String> = listed[0].1.as_ref().unwrap().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["notes.txt"]);
    let (meta, bytes) = machine.read_file(&work.join("notes.txt"), 1000).unwrap();
    assert_eq!((meta.len, bytes.as_slice()), (13, &b"from the host"[..]));
    machine.write_file(&work.join("new.txt"), b"written there").unwrap();
    assert_eq!(std::fs::read(work.join("new.txt")).unwrap(), b"written there");
    assert_eq!(machine.read_file(&work.join("gone.txt"), 10), Err(files::FsError::NotFound));
    assert_eq!(machine.resolve_dir(&work.join(".")).unwrap(), work);
    assert_eq!(machine.are_dirs(&[work.clone(), work.join("notes.txt")]), Some(vec![true, false]));
    machine.create_file(&work.join("sub/made.txt")).unwrap();
    assert!(work.join("sub/made.txt").is_file());

    // Its git, through the host.
    let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&work).args(args).output();
    if git(&["init", "-q", "-b", "main"]).is_ok_and(|o| o.status.success()) {
        let status = machine.git_status(&work).expect("a repository");
        assert_eq!(status.head, Some(git::Head::Branch("main".into())));
        assert_eq!(status.repo.machine, machine);
        let changes = status.repo.working_changes().unwrap();
        let mut untracked: Vec<&str> = changes.untracked.iter().map(|f| f.path.as_str()).collect();
        untracked.sort();
        assert_eq!(untracked, ["new.txt", "notes.txt", "sub/made.txt"]);
        status.repo.stage_paths(["notes.txt"]).unwrap();
        let who = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"];
        assert!(git(&[&who[..], &["commit", "-q", "-m", "first"]].concat()).unwrap().status.success());
        let log = status.repo.log(false, 10).unwrap();
        assert_eq!(log.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["first"]);
    }
}

#[test]
fn a_remote_terminal_runs_on_through_a_broken_connection() {
    let dir = tempfile::tempdir().unwrap();
    let _helpers = Helpers(dir.path().to_path_buf());
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let mut core = open(dir.path());
    let (_, pane) = remote_project(&mut core, &work);
    type_line(&core, pane, "KEPT=still-here; echo set-$((40+2))");
    wait_for_text(&mut core, pane, "set-42");

    pump(&mut core);
    core.reconnect(HOST);
    // Typed while it is down, it goes out once the terminal is back (as
    // with SSH, which sends what was typed while it stalled).
    type_line(&core, pane, "echo typed-$((3+4))-meanwhile");
    // The terminal connects again by itself (a notice says so meanwhile,
    // which the screen from the host then replaces).
    let mut seen = Vec::new();
    wait_until(&mut core, "the connection to break and come back", |core| {
        seen.extend(pump(core).into_iter().filter_map(|e| match e {
            Event::Remote { status, .. } => Some(status),
            _ => None,
        }));
        seen.ends_with(&[RemoteStatus::Lost, RemoteStatus::Connecting, RemoteStatus::Connected])
    });
    wait_for_text(&mut core, pane, "typed-7-meanwhile");
    // It is the same shell, with what it had.
    type_line(&core, pane, "echo got-$KEPT");
    wait_for_text(&mut core, pane, "got-still-here");
    assert_eq!(screen_text(&core, pane).matches("set-42").count(), 1, "the screen is the host's, once");
}

#[test]
fn remote_terminals_that_keep_running_outlive_terminalrooster() {
    let dir = tempfile::tempdir().unwrap();
    let _helpers = Helpers(dir.path().to_path_buf());
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let (kept, ended) = {
        let mut core = open(dir.path());
        let (_, kept) = remote_project(&mut core, &work);
        let ended = match core.dispatch(Command::SplitPane { pane: kept, axis: Axis::Vertical }).unwrap() {
            Outcome::Pane(p) => p,
            other => panic!("{other:?}"),
        };
        core.dispatch(Command::SetKeepRunning { pane: kept, on: true }).unwrap();
        type_line(&core, kept, "MINE=kept; echo before-$((1+1))");
        type_line(&core, ended, "MINE=lost; echo other-$((2+2))");
        wait_for_text(&mut core, kept, "before-2");
        wait_for_text(&mut core, ended, "other-4");
        core.shutdown();
        (kept, ended)
    };

    let mut core = open(dir.path());
    // The one that keeps running is shown as it was, and is the same shell.
    wait_for_text(&mut core, kept, "before-2");
    type_line(&core, kept, "echo again-$MINE");
    wait_for_text(&mut core, kept, "again-kept");
    // The other one ended with Terminal Rooster: a new shell.
    type_line(&core, ended, "echo now-${MINE:-unset}");
    wait_for_text(&mut core, ended, "now-unset");
}
