//! Terminals that keep running when TerminalRooster quits, under helper
//! processes. This test binary is its own helper: started with `HOST_ENV`
//! set, its `helper_entry` test serves a terminal instead of testing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use termroost_core::*;

const HOST_ENV: &str = "TERMROOST_TEST_HOST";

#[test]
fn helper_entry() {
    if std::env::var_os(HOST_ENV).is_some() {
        std::process::exit(host::serve());
    }
}

fn host_command() -> HostCommand {
    HostCommand {
        program: std::env::current_exe().unwrap(),
        args: ["helper_entry", "--exact", "--nocapture", "--test-threads=1"].map(String::from).to_vec(),
        env: vec![(HOST_ENV.to_owned(), "1".to_owned())],
    }
}

fn open(dir: &Path) -> Core {
    let agents = dir.join("agent-homes");
    Core::open(Config {
        data_dir: Some(dir.to_path_buf()),
        autosave_interval: Duration::from_millis(200),
        agent_homes: AgentHomes {
            claude: agents.join("claude"),
            codex: agents.join("codex"),
            grok: agents.join("grok"),
        },
        host: Some(host_command()),
        ..Default::default()
    })
    .expect("open core")
}

/// Ends the helpers a test leaves behind, failed or not, so none outlive it
/// (and the temp dir, which is their working directory, can go).
struct Helpers(PathBuf);

impl Drop for Helpers {
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(self.0.join("hosts")) else { return };
        for entry in entries.flatten() {
            let Ok(info) = std::fs::read(entry.path()) else { continue };
            let Some(pid) = serde_json::from_slice::<serde_json::Value>(&info).ok().and_then(|v| v["pid"].as_u64())
            else {
                continue;
            };
            let pid = pid.to_string();
            #[cfg(windows)]
            let _ = std::process::Command::new("taskkill").args(["/F", "/T", "/PID", &pid]).output();
            #[cfg(unix)]
            let _ = std::process::Command::new("kill").args(["-9", &pid]).output();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Prints `start`, then after a while `late-output`, then echoes what is typed.
fn slow_printer() -> String {
    if cfg!(windows) {
        "cmd /c \"echo start & ping -n 4 127.0.0.1 >nul & echo late-output & more\"".to_owned()
    } else {
        "sh -c \"printf 'start\\n'; sleep 3; printf 'late-output\\n'; exec cat\"".to_owned()
    }
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn pump(core: &mut Core) {
    let events = core.events();
    while let Ok(e) = events.try_recv() {
        core.apply_event(&e);
    }
}

fn screen_text(core: &Core, pane: PaneId) -> String {
    core.terminal(pane).map(|h| h.with_content(|c| c.text())).unwrap_or_default()
}

fn wait_for_text(core: &mut Core, pane: PaneId, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
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

fn shell_pid(core: &mut Core, pane: PaneId) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        pump(core);
        if let Some(pid) = core.terminal(pane).and_then(|h| h.shell_pid()) {
            return pid;
        }
        let text = screen_text(core, pane);
        assert!(Instant::now() < deadline, "timed out waiting for the shell's pid; the screen:\n{text}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn conn_file(dir: &Path, pane: PaneId) -> PathBuf {
    dir.join("hosts").join(format!("{}.json", pane.0))
}

#[test]
fn a_terminal_keeps_running_while_terminalrooster_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let _helpers = Helpers(dir.path().to_path_buf());
    let (project, pane, pid) = {
        let mut core = open(dir.path());
        core.dispatch(Command::SetDefaultShell(Some(slow_printer()))).unwrap();
        let Outcome::Project(project) = core
            .dispatch(Command::CreateProject {
                name: "p".into(),
                working_dir: dir.path().into(),
                shell: None,
                host: None,
            })
            .unwrap()
        else {
            panic!()
        };
        let pane = core.state().active_tab().unwrap().layout.first_pane();
        core.dispatch(Command::SetKeepRunning { pane, on: true }).unwrap();
        wait_for_text(&mut core, pane, "start");
        let pid = shell_pid(&mut core, pane);
        assert!(conn_file(dir.path(), pane).exists(), "a helper runs the terminal");
        assert!(!screen_text(&core, pane).contains("late-output"));
        core.shutdown();
        (project, pane, pid)
    };

    // TerminalRooster is closed; the program goes on printing.
    std::thread::sleep(Duration::from_secs(4));
    assert!(conn_file(dir.path(), pane).exists(), "the helper outlived the core");

    let mut core = open(dir.path());
    wait_for_text(&mut core, pane, "late-output");
    assert_eq!(shell_pid(&mut core, pane), pid, "the same program, not a new one");
    assert!(core.state().pane(pane).unwrap().keep_running);
    let handle = core.terminal(pane).unwrap();
    assert!(handle.is_running());
    handle.write("typed-after\r");
    wait_for_text(&mut core, pane, "typed-after");

    // Closing the terminal ends its helper.
    core.dispatch(Command::DeleteProject(project)).unwrap();
    wait_until("the helper to end", Duration::from_secs(15), || !conn_file(dir.path(), pane).exists());
}

#[test]
fn switching_keep_running_off_ends_the_helper() {
    let dir = tempfile::tempdir().unwrap();
    let _helpers = Helpers(dir.path().to_path_buf());
    let mut core = open(dir.path());
    let printer = if cfg!(windows) { "cmd /c \"echo hi & more\"" } else { "sh -c \"printf 'hi\\n'; exec cat\"" };
    core.dispatch(Command::SetDefaultShell(Some(printer.to_owned()))).unwrap();
    core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None, host: None })
        .unwrap();
    let pane = core.state().active_tab().unwrap().layout.first_pane();
    let local_pid = shell_pid(&mut core, pane);
    assert!(!conn_file(dir.path(), pane).exists(), "terminals run in TerminalRooster by default");

    core.dispatch(Command::SetKeepRunning { pane, on: true }).unwrap();
    wait_until("a helper", Duration::from_secs(20), || conn_file(dir.path(), pane).exists());
    let mut hosted_pid = local_pid;
    wait_until("the helper's shell", Duration::from_secs(20), || {
        pump(&mut core);
        hosted_pid = core.terminal(pane).and_then(|h| h.shell_pid()).unwrap_or(local_pid);
        hosted_pid != local_pid
    });
    // What was on the screen before stays above the new shell's output.
    wait_for_text(&mut core, pane, "keeps running when Terminal Rooster quits");

    core.dispatch(Command::SetKeepRunning { pane, on: false }).unwrap();
    wait_until("the helper to end", Duration::from_secs(15), || !conn_file(dir.path(), pane).exists());
    let pid = shell_pid(&mut core, pane);
    assert_ne!(pid, hosted_pid);
    assert!(screen_text(&core, pane).contains("stops when Terminal Rooster quits"));
    core.terminal(pane).unwrap().write("still-typing\r");
    wait_for_text(&mut core, pane, "still-typing");
}
