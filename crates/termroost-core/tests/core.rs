//! End-to-end tests of the core with real PTYs and a real database.

use std::path::Path;
use std::time::{Duration, Instant};

use termroost_core::*;

/// Agent state directories inside the test's temp dir, never the user's real ones.
fn test_homes(dir: &Path) -> AgentHomes {
    let agents = dir.join("agent-homes");
    AgentHomes {
        claude: agents.join("claude"),
        codex: agents.join("codex"),
        grok: agents.join("grok"),
        antigravity: agents.join("antigravity"),
    }
}

fn open(dir: &Path) -> Core {
    Core::open(Config {
        data_dir: Some(dir.to_path_buf()),
        autosave_interval: Duration::from_millis(200),
        agent_homes: test_homes(dir),
        ..Default::default()
    })
    .expect("open core")
}

/// A shell command line that prints `text`, then stays alive (or exits with
/// `code`). Staying alive, it reads what is typed, so that shows on screen.
fn printer(text: &str, exit_code: Option<i32>) -> String {
    if cfg!(windows) {
        match exit_code {
            Some(c) => format!("cmd /c \"echo {text} & exit {c}\""),
            // The console echoes typed lines only to a program that reads them.
            None => format!("cmd /c \"echo {text} & more\""),
        }
    } else {
        match exit_code {
            Some(c) => format!("sh -c \"printf '{text}\\n'; exit {c}\""),
            None => format!("sh -c \"printf '{text}\\n'; exec cat\""),
        }
    }
}

fn wait_for(core: &mut Core, timeout: Duration, mut done: impl FnMut(&mut Core, Option<&Event>) -> bool) {
    let events = core.events();
    let deadline = Instant::now() + timeout;
    loop {
        let ev = events.try_recv().ok();
        if let Some(e) = &ev {
            core.apply_event(e);
        }
        if done(core, ev.as_ref()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting");
        if ev.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn screen_text(core: &Core, pane: PaneId) -> String {
    core.terminal(pane).map(|h| h.with_content(|c| c.text())).unwrap_or_default()
}

fn wait_for_text(core: &mut Core, pane: PaneId, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    wait_for(core, Duration::from_secs(20), |c, _| {
        let text = screen_text(c, pane);
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?} on the screen:\n{text}");
        text.contains(needle)
    });
}

fn only_pane(core: &Core) -> PaneId {
    core.state().active_tab().unwrap().layout.first_pane()
}

#[test]
fn terminal_runs_and_reports_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(printer("hello terminalrooster", Some(3))))).unwrap();
    core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None }).unwrap();
    let pane = only_pane(&core);

    let mut code = None;
    wait_for(&mut core, Duration::from_secs(15), |_, ev| {
        if let Some(Event::ProcessExited { pane: p, code: c }) = ev
            && *p == pane
        {
            code = Some(*c);
            return true;
        }
        false
    });
    assert_eq!(code, Some(Some(3)));
    let handle = core.terminal(pane).unwrap();
    assert!(!handle.is_running());
    assert_eq!(handle.exit_status(), Some(Some(3)));
    assert!(screen_text(&core, pane).contains("hello terminalrooster"));

    // Resizing updates the grid.
    handle.resize(GridSize { cols: 40, rows: 10, cell_width: 8, cell_height: 16 });
    handle.with_content(|c| assert_eq!((c.cols, c.rows), (40, 10)));
}

#[test]
fn state_and_scrollback_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (project, tab, panes, task) = {
        let mut core = open(dir.path());
        core.dispatch(Command::SetDefaultShell(Some(printer("marker-123", None)))).unwrap();
        let Outcome::Project(project) = core
            .dispatch(Command::CreateProject { name: "api".into(), working_dir: dir.path().into(), shell: None })
            .unwrap()
        else {
            panic!()
        };
        let tab = core.state().active_tab().unwrap().id;
        core.dispatch(Command::RenameTab { tab, title: "agents".into() }).unwrap();
        let first = only_pane(&core);
        let Outcome::Pane(second) = core.dispatch(Command::SplitPane { pane: first, axis: Axis::Vertical }).unwrap()
        else {
            panic!()
        };
        core.dispatch(Command::SetResumeCommand { pane: first, command: Some("echo resumed-ok".into()) }).unwrap();
        let Outcome::Task(task) =
            core.dispatch(Command::CreateTask { project, title: "fix auth".into(), tab: Some(tab) }).unwrap()
        else {
            panic!()
        };
        core.dispatch(Command::CreateTask { project, title: "write docs".into(), tab: None }).unwrap();
        core.dispatch(Command::SetTaskDone { task, done: true }).unwrap();
        wait_for_text(&mut core, first, "marker-123");
        wait_for_text(&mut core, second, "marker-123");
        // Only terminals someone typed in are saved; `second` stays untouched.
        core.terminal(first).unwrap().text("typed-ok\r");
        wait_for_text(&mut core, first, "typed-ok");
        // Clean shutdown saves scrollback.
        (project, tab, vec![first, second], task)
    };

    let mut core = open(dir.path());
    let s = core.state();
    assert_eq!(s.selected_project, Some(project));
    assert_eq!(s.project(project).unwrap().name, "api");
    let t = s.tab(tab).unwrap();
    assert_eq!(t.title, "agents");
    assert_eq!(t.layout.panes(), panes);
    assert_eq!(s.tasks_of(project).count(), 2);
    assert!(s.task(task).unwrap().done);
    assert_eq!(s.open_task_count(tab), 0);

    // The visible tab is started again with its old scrollback and resume command.
    wait_for_text(&mut core, panes[0], "typed-ok");
    let text = screen_text(&core, panes[0]);
    assert!(text.contains("marker-123"), "{text}");
    wait_for_text(&mut core, panes[0], "resumed-ok");
    // Nothing was typed in the other terminal, so it starts fresh: its
    // marker shows once, from its new shell. Once what is typed now shows,
    // the shell has printed.
    wait_for_text(&mut core, panes[1], "marker-123");
    core.terminal(panes[1]).unwrap().text("fresh-ok\r");
    wait_for_text(&mut core, panes[1], "fresh-ok");
    let text = screen_text(&core, panes[1]);
    assert_eq!(text.matches("marker-123").count(), 1, "{text}");
}

#[test]
fn agents_resume_their_exact_session() {
    let dir = tempfile::tempdir().unwrap();
    let pane;
    let session_id;
    {
        let mut core = open(dir.path());
        // `cat` echoes what is typed, so the launch line shows without running an agent.
        core.dispatch(Command::SetDefaultShell(Some(printer("ready", None)))).unwrap();
        core.dispatch(Command::CreateProject { name: "a".into(), working_dir: dir.path().into(), shell: None })
            .unwrap();
        pane = only_pane(&core);
        wait_for_text(&mut core, pane, "ready");
        core.dispatch(Command::StartAgent { pane, kind: AgentKind::Claude, args: "--model opus".into() }).unwrap();
        let agent = core.state().pane(pane).unwrap().agent.clone().unwrap();
        session_id = agent.session_id.clone().unwrap();
        wait_for_text(&mut core, pane, &format!("claude --session-id {session_id}"));
    }
    // No conversation was saved for that session yet, so Claude starts fresh.
    let projects = test_homes(dir.path()).claude.join("projects/-tmp-a");
    std::fs::create_dir_all(&projects).unwrap();
    {
        let mut core = open(dir.path());
        wait_for_text(&mut core, pane, "\nclaude --model opus");
    }
    // Once the conversation exists on disk, the exact session is resumed.
    std::fs::write(projects.join(format!("{session_id}.jsonl")), "{}").unwrap();
    let mut core = open(dir.path());
    wait_for_text(&mut core, pane, &format!("claude --resume {session_id} --model opus"));
    // The agent shows its own conversation; the old screen is not replayed.
    let text = screen_text(&core, pane);
    assert!(!text.contains("--session-id"), "{text}");
    core.dispatch(Command::ClearAgent(pane)).unwrap();
    assert!(core.state().pane(pane).unwrap().agent.is_none());
}

#[test]
fn second_instance_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _core = open(dir.path());
    let config =
        Config { data_dir: Some(dir.path().into()), agent_homes: test_homes(dir.path()), ..Default::default() };
    let err = Core::open(config).err().unwrap();
    assert!(matches!(err, Error::AlreadyRunning(_)), "{err}");
}

#[test]
fn delete_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(printer("x", None)))).unwrap();
    let Outcome::Project(p) = core
        .dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None })
        .unwrap()
    else {
        panic!()
    };
    let tab1 = core.state().active_tab().unwrap().id;
    let Outcome::Tab(tab2) = core.dispatch(Command::CreateTab { project: p, title: None, working_dir: None }).unwrap()
    else {
        panic!()
    };
    assert_eq!(core.state().tab(tab2).unwrap().title, "Session 2");
    let Outcome::Task(task) =
        core.dispatch(Command::CreateTask { project: p, title: "t".into(), tab: Some(tab2) }).unwrap()
    else {
        panic!()
    };
    let pane2 = core.state().tab(tab2).unwrap().layout.first_pane();
    assert!(core.terminal(pane2).is_some());

    // Last pane of a tab cannot be closed.
    assert!(core.dispatch(Command::ClosePane(pane2)).is_err());

    core.dispatch(Command::DeleteTab(tab2)).unwrap();
    assert!(core.terminal(pane2).is_none());
    assert_eq!(core.state().task(task).unwrap().tab, None, "task unlinked, not deleted");
    assert_eq!(core.state().active_tab().unwrap().id, tab1);

    core.dispatch(Command::DeleteTask(task)).unwrap();
    assert!(core.state().task(task).is_none());

    core.dispatch(Command::DeleteProject(p)).unwrap();
    assert!(core.state().projects.is_empty());
    assert!(core.state().tabs.is_empty());
    assert_eq!(core.state().selected_project, None);
    drop(core);
    let core = open(dir.path());
    assert!(core.state().projects.is_empty());
}

#[test]
fn notes_stay_until_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    let create = |core: &mut Core, title: Option<&str>| match core
        .dispatch(Command::CreateNote { title: title.map(Into::into) })
        .unwrap()
    {
        Outcome::Note(id) => id,
        other => panic!("{other:?}"),
    };
    let first = create(&mut core, None);
    let second = create(&mut core, None);
    let named = create(&mut core, Some("  links  "));
    let titles = |core: &Core| core.state().notes.iter().map(|n| n.title.clone()).collect::<Vec<_>>();
    assert_eq!(titles(&core), ["Note 1", "Note 2", "links"]);

    core.dispatch(Command::SetNoteText { note: second, text: "token: abc\n  indented\n".into() }).unwrap();
    core.dispatch(Command::RenameNote { note: second, title: "keep".into() }).unwrap();
    assert!(core.dispatch(Command::RenameNote { note: second, title: " ".into() }).is_err());
    core.dispatch(Command::CloseNote(first)).unwrap();
    assert!(core.dispatch(Command::CloseNote(first)).is_err(), "already closed");
    let third = create(&mut core, None);
    assert_eq!(core.state().note(third).unwrap().title, "Note 1", "a closed note's name is free again");
    assert_eq!(titles(&core), ["keep", "links", "Note 1"], "new notes go last");
    drop(core);

    let mut core = open(dir.path());
    assert_eq!(titles(&core), ["keep", "links", "Note 1"]);
    assert_eq!(core.state().note(second).unwrap().text, "token: abc\n  indented\n");
    assert!(core.state().note(named).unwrap().text.is_empty());
    let after = create(&mut core, None);
    assert!(after > third, "ids of notes are not reused");

    core.dispatch(Command::CloseAllNotes).unwrap();
    assert!(core.state().notes.is_empty());
    drop(core);
    assert!(open(dir.path()).state().notes.is_empty());
}

#[test]
fn reordering_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(printer("x", None)))).unwrap();
    let mut project = |name: &str| match core
        .dispatch(Command::CreateProject { name: name.into(), working_dir: dir.path().into(), shell: None })
        .unwrap()
    {
        Outcome::Project(p) => p,
        _ => panic!(),
    };
    let (a, b, c) = (project("a"), project("b"), project("c"));
    let tab1 = core.state().active_tab().unwrap().id;
    let Outcome::Tab(tab2) = core.dispatch(Command::CreateTab { project: c, title: None, working_dir: None }).unwrap()
    else {
        panic!()
    };
    let Outcome::Tab(tab3) = core.dispatch(Command::CreateTab { project: c, title: None, working_dir: None }).unwrap()
    else {
        panic!()
    };
    let projects = |core: &Core| core.state().projects.iter().map(|p| p.id).collect::<Vec<_>>();
    let tabs = |core: &Core| core.state().tabs_of(c).map(|t| t.id).collect::<Vec<_>>();

    // `to_index` is the place in the list without the moved item.
    core.dispatch(Command::MoveProject { project: c, to_index: 0, group: None }).unwrap();
    assert_eq!(projects(&core), [c, a, b]);
    core.dispatch(Command::MoveProject { project: a, to_index: 2, group: None }).unwrap();
    assert_eq!(projects(&core), [c, b, a]);
    core.dispatch(Command::MoveProject { project: b, to_index: 99, group: None }).unwrap();
    assert_eq!(projects(&core), [c, a, b], "past the end means last");

    core.dispatch(Command::MoveTab { tab: tab3, to_index: 0 }).unwrap();
    core.dispatch(Command::MoveTab { tab: tab1, to_index: 2 }).unwrap();
    assert_eq!(tabs(&core), [tab3, tab2, tab1]);
    assert_eq!(core.state().active_tab().unwrap().id, tab3, "moving doesn't change what is shown");

    drop(core);
    let core = open(dir.path());
    assert_eq!(projects(&core), [c, a, b]);
    assert_eq!(tabs(&core), [tab3, tab2, tab1]);
}

#[test]
fn groups_keep_their_projects_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(printer("x", None)))).unwrap();
    let mut project = |name: &str| match core
        .dispatch(Command::CreateProject { name: name.into(), working_dir: dir.path().into(), shell: None })
        .unwrap()
    {
        Outcome::Project(p) => p,
        _ => panic!(),
    };
    let (a, b, c, d) = (project("a"), project("b"), project("c"), project("d"));
    // Each project with its group, in list order.
    let list = |core: &Core| core.state().projects.iter().map(|p| (p.id, p.group)).collect::<Vec<_>>();

    // Dropped on a project without a group: the two make a new group where that project was.
    let Outcome::Group(g) = core.dispatch(Command::GroupProjects { project: c, with: a }).unwrap() else {
        panic!("expected a new group")
    };
    assert_eq!(list(&core), [(a, Some(g)), (c, Some(g)), (b, None), (d, None)]);
    assert_eq!(core.state().group(g).unwrap().name, NEW_GROUP_NAME);
    // Dropped on a project in a group: it joins that group, just after it.
    assert_eq!(core.dispatch(Command::GroupProjects { project: d, with: a }).unwrap(), Outcome::None);
    assert_eq!(list(&core), [(a, Some(g)), (d, Some(g)), (c, Some(g)), (b, None)]);
    assert!(core.dispatch(Command::GroupProjects { project: d, with: d }).is_err());

    // A group's projects can't be split up.
    let before = list(&core);
    assert!(core.dispatch(Command::MoveProject { project: b, to_index: 1, group: None }).is_err());
    assert!(core.dispatch(Command::MoveProject { project: a, to_index: 3, group: Some(g) }).is_err());
    assert_eq!(list(&core), before, "refused moves change nothing");
    // But a project can move in among them, or out next to them.
    core.dispatch(Command::MoveProject { project: b, to_index: 1, group: Some(g) }).unwrap();
    assert_eq!(list(&core), [(a, Some(g)), (b, Some(g)), (d, Some(g)), (c, Some(g))]);
    core.dispatch(Command::MoveProject { project: c, to_index: 3, group: None }).unwrap();
    assert_eq!(list(&core), [(a, Some(g)), (b, Some(g)), (d, Some(g)), (c, None)]);

    // A group moves with all its projects, and not into another group.
    core.dispatch(Command::MoveGroup { group: g, to_index: 1 }).unwrap();
    assert_eq!(list(&core), [(c, None), (a, Some(g)), (b, Some(g)), (d, Some(g))]);
    let Outcome::Group(h) = core.dispatch(Command::GroupProjects { project: d, with: c }).unwrap() else { panic!() };
    assert_eq!(list(&core), [(c, Some(h)), (d, Some(h)), (a, Some(g)), (b, Some(g))]);
    assert!(core.dispatch(Command::MoveGroup { group: g, to_index: 1 }).is_err());
    core.dispatch(Command::MoveGroup { group: g, to_index: 0 }).unwrap();
    assert_eq!(list(&core), [(a, Some(g)), (b, Some(g)), (c, Some(h)), (d, Some(h))]);

    core.dispatch(Command::RenameGroup { group: g, name: " work ".into() }).unwrap();
    assert!(core.dispatch(Command::RenameGroup { group: g, name: "  ".into() }).is_err());
    core.dispatch(Command::SetGroupCollapsed { group: g, collapsed: true }).unwrap();
    drop(core);
    let mut core = open(dir.path());
    assert_eq!(list(&core), [(a, Some(g)), (b, Some(g)), (c, Some(h)), (d, Some(h))]);
    let group = core.state().group(g).unwrap();
    assert_eq!((group.name.as_str(), group.collapsed), ("work", true));

    // A group goes away with its last project, whether moved out or deleted.
    core.dispatch(Command::MoveProject { project: c, to_index: 0, group: None }).unwrap();
    assert!(core.state().group(h).is_some());
    core.dispatch(Command::MoveProject { project: d, to_index: 0, group: None }).unwrap();
    assert!(core.state().group(h).is_none());
    core.dispatch(Command::DeleteProject(a)).unwrap();
    core.dispatch(Command::DeleteProject(b)).unwrap();
    assert!(core.state().groups.is_empty());
    assert_eq!(list(&core), [(d, None), (c, None)]);
    drop(core);
    let core = open(dir.path());
    assert!(core.state().groups.is_empty());
    assert_eq!(list(&core), [(d, None), (c, None)]);
}

const CRASH_CHILD_ENV: &str = "TERMINALROOSTER_TEST_CRASH_CHILD";

/// Runs in a child process: builds some state, then aborts without cleanup.
fn crash_child(dir: &Path) -> ! {
    let mut core = open(dir);
    core.dispatch(Command::SetDefaultShell(Some(printer("crash-marker", None)))).unwrap();
    let Outcome::Project(p) = core
        .dispatch(Command::CreateProject { name: "survivor".into(), working_dir: dir.into(), shell: None })
        .unwrap()
    else {
        panic!()
    };
    let tab = core.state().active_tab().unwrap().id;
    core.dispatch(Command::CreateTask { project: p, title: "persist me".into(), tab: Some(tab) }).unwrap();
    let pane = only_pane(&core);
    core.dispatch(Command::SplitPane { pane, axis: Axis::Horizontal }).unwrap();
    wait_for_text(&mut core, pane, "crash-marker");
    // Only terminals someone typed in are saved.
    core.terminal(pane).unwrap().text("typed-before-crash\r");
    wait_for_text(&mut core, pane, "typed-before-crash");
    // Wait for the periodic autosave rather than forcing a save.
    std::thread::sleep(Duration::from_millis(800));
    core.dispatch(Command::RenameTab { tab, title: "after-save".into() }).unwrap();
    core.flush();
    std::process::abort();
}

#[test]
fn state_survives_a_crash() {
    if let Some(dir) = std::env::var_os(CRASH_CHILD_ENV) {
        crash_child(Path::new(&dir));
    }
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "state_survives_a_crash", "--nocapture", "--test-threads=1"])
        .env(CRASH_CHILD_ENV, dir.path())
        .status()
        .unwrap();
    assert!(!status.success(), "child should have aborted");

    // The crashed process's lock is gone, and everything it committed is back.
    let mut core = open(dir.path());
    let s = core.state();
    assert_eq!(s.projects.len(), 1);
    assert_eq!(s.projects[0].name, "survivor");
    let tab = s.active_tab().unwrap();
    assert_eq!(tab.title, "after-save");
    assert_eq!(tab.layout.panes().len(), 2);
    assert!(matches!(tab.layout, Layout::Split { axis: Axis::Horizontal, .. }));
    assert_eq!(s.tasks[0].title, "persist me");
    assert_eq!(s.open_task_count(tab.id), 1);
    let pane = tab.layout.first_pane();
    wait_for_text(&mut core, pane, "typed-before-crash");
    let text = screen_text(&core, pane);
    assert!(text.contains("crash-marker"), "{text}");
}

#[test]
fn closed_terminals_end_their_shells() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(printer("x", None)))).unwrap();
    let Outcome::Project(p) = core
        .dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None })
        .unwrap()
    else {
        panic!()
    };
    // Closed as soon as they start: on Windows, closing a console before
    // its shell is up does not end the shell, which runs on without a
    // window (and keeps its folder from being deleted).
    let mut shells = Vec::new();
    for _ in 0..3 {
        let Outcome::Tab(tab) =
            core.dispatch(Command::CreateTab { project: p, title: None, working_dir: None }).unwrap()
        else {
            panic!()
        };
        let pane = core.state().tab(tab).unwrap().layout.first_pane();
        shells.extend(core.terminal(pane).and_then(|h| h.shell_pid()));
        core.dispatch(Command::DeleteTab(tab)).unwrap();
    }
    assert_eq!(shells.len(), 3);
    // Dropping the core waits for them, as the process may exit right after.
    drop(core);
    let shells = LeftDir { pids: shells, ..Default::default() };
    assert!(shells.wait(Duration::from_secs(1)), "shells outlived their terminals");
}

#[test]
fn palette_can_change_at_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(printer("colors", None)))).unwrap();
    core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None }).unwrap();
    let pane = only_pane(&core);
    wait_for_text(&mut core, pane, "colors");
    let handle = core.terminal(pane).unwrap();
    let dark_bg = Palette::default().get(257);
    handle.with_content(|c| assert_eq!(c.default_bg, dark_bg));

    let white = Rgb::new(255, 255, 255);
    let black = Rgb::new(0, 0, 0);
    core.set_palette(Palette::new([black; 16], black, white, black, Rgb::new(200, 220, 255)));
    handle.with_content(|c| {
        assert_eq!(c.default_bg, white);
        assert_eq!(c.default_fg, black);
        assert!(c.cells.iter().all(|cell| cell.bg == white || cell.selected));
    });
}

/// A stand-in for Claude Code: registers `sessions/<pid>.json` the way the
/// real CLI does, then runs until interrupted.
#[cfg(unix)]
fn fake_claude(dir: &Path, homes: &AgentHomes) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let sessions = homes.claude.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("claude");
    let script = format!(
        "#!/bin/sh\nprintf '{{\"pid\": %s, \"sessionId\": \"%s\", \"kind\": \"interactive\"}}' $$ \"$1\" > \"{}/$$.json\"\n\
         echo \"FAKE-CLAUDE running $1\"\nwhile :; do sleep 1; done\n",
        sessions.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn agents_started_by_hand_are_detected_and_resumed_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let homes = AgentHomes {
        claude: dir.path().join("claude-home"),
        codex: dir.path().join("codex-home"),
        grok: dir.path().join("grok-home"),
        antigravity: dir.path().join("antigravity-home"),
    };
    let fake = fake_claude(dir.path(), &homes);
    let open_with_homes = || {
        Core::open(Config {
            data_dir: Some(dir.path().join("data")),
            autosave_interval: Duration::from_millis(200),
            agent_homes: homes.clone(),
            ..Default::default()
        })
        .unwrap()
    };
    let agent_of = |c: &Core, pane: PaneId| c.state().pane(pane).and_then(|p| p.agent.clone());
    let wait_agent = |c: &mut Core, pane: PaneId, want: Option<&str>| {
        wait_for(c, Duration::from_secs(20), |c, _| {
            agent_of(c, pane).and_then(|a| a.session_id).as_deref() == want
                && (want.is_none() || agent_of(c, pane).is_some_and(|a| a.kind == AgentKind::Claude))
        });
    };
    const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    const C: &str = "cccccccc-0000-4000-8000-000000000003";

    let pane;
    {
        let mut core = open_with_homes();
        core.dispatch(Command::SetDefaultShell(Some("sh".into()))).unwrap();
        core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None })
            .unwrap();
        pane = only_pane(&core);
        let term = core.terminal(pane).unwrap();
        std::thread::sleep(Duration::from_millis(500));

        // Typed by hand, not through "Start agent".
        term.text(&format!("{} {A}\r", fake.display()));
        wait_for_text(&mut core, pane, &format!("FAKE-CLAUDE running {A}"));
        wait_agent(&mut core, pane, Some(A));

        // `/clear` rewrites the registry with a new session id.
        let reg = std::fs::read_dir(homes.claude.join("sessions")).unwrap().next().unwrap().unwrap().path();
        let text = std::fs::read_to_string(&reg).unwrap().replace(A, B);
        std::fs::write(&reg, text).unwrap();
        wait_agent(&mut core, pane, Some(B));

        // Quitting the agent clears it.
        term.write(vec![3u8]);
        wait_for(&mut core, Duration::from_secs(20), |c, _| agent_of(c, pane).is_none());

        term.text(&format!("{} {C}\r", fake.display()));
        wait_agent(&mut core, pane, Some(C));
        // C has a saved conversation, so it can be resumed.
        std::fs::create_dir_all(homes.claude.join("projects/p")).unwrap();
        std::fs::write(homes.claude.join(format!("projects/p/{C}.jsonl")), "{}").unwrap();
    }

    let mut core = open_with_homes();
    assert_eq!(agent_of(&core, pane).and_then(|a| a.session_id).as_deref(), Some(C));
    wait_for_text(&mut core, pane, &format!("claude --resume {C}"));
}

#[cfg(unix)]
#[test]
fn codex_hook_reports_pin_threads_to_the_right_terminal() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let homes = test_homes(dir.path());
    std::fs::create_dir_all(&homes.codex).unwrap();
    std::fs::write(homes.codex.join("config.toml"), "").unwrap();
    // A stand-in `codex` that just keeps running.
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("codex");
    std::fs::write(&fake, "#!/bin/sh\necho FAKE-CODEX\nwhile :; do sleep 1; done\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut core = open(dir.path());
    assert!(core.codex_hook_status().codex_found);
    assert_eq!(core.codex_hook_status().installed_in, None);
    let hooks_file = core.install_codex_hook().unwrap();
    let hooks: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&hooks_file).unwrap()).unwrap();
    let command = hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap().to_owned();

    core.dispatch(Command::SetDefaultShell(Some("sh".into()))).unwrap();
    core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None }).unwrap();
    let one = only_pane(&core);
    let Outcome::Pane(two) = core.dispatch(Command::SplitPane { pane: one, axis: Axis::Vertical }).unwrap() else {
        panic!()
    };
    std::thread::sleep(Duration::from_millis(500));
    for pane in [one, two] {
        core.terminal(pane).unwrap().text(&format!("{}\r", fake.display()));
        wait_for_text(&mut core, pane, "FAKE-CODEX");
    }
    let codex_session = |c: &Core, pane: PaneId| {
        c.state().pane(pane).and_then(|p| p.agent.clone()).filter(|a| a.kind == AgentKind::Codex).map(|a| a.session_id)
    };
    wait_for(&mut core, Duration::from_secs(20), |c, _| {
        codex_session(c, one) == Some(None) && codex_session(c, two) == Some(None)
    });

    // What Codex does on a prompt: run the hook with the event on stdin.
    let enter = KeyInput { key: Key::Named(NamedKey::Enter), mods: Modifiers::NONE, text: None };
    let report = |thread: &str| {
        let event = serde_json::json!({
            "session_id": thread, "cwd": dir.path(), "hook_event_name": "UserPromptSubmit", "prompt": "hi"
        });
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    };
    const X: &str = "01a1e20f-eb7a-7971-a81c-602f1b0f4201";
    const Y: &str = "01a1e20f-eb7a-7971-a81c-602f1b0f4202";
    core.terminal(two).unwrap().key(&enter);
    report(X);
    wait_for(&mut core, Duration::from_secs(20), |c, _| codex_session(c, two) == Some(Some(X.into())));
    std::thread::sleep(Duration::from_millis(600));
    core.terminal(one).unwrap().key(&enter);
    report(Y);
    wait_for(&mut core, Duration::from_secs(20), |c, _| codex_session(c, one) == Some(Some(Y.into())));
    assert_eq!(codex_session(&core, two), Some(Some(X.into())), "pinned threads stay put");
    assert!(core.codex_hook_last_report().is_some());

    core.remove_codex_hook().unwrap();
    assert!(!hooks_file.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn antigravity_started_by_hand_is_detected_and_resumed_exactly() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let homes = test_homes(dir.path());
    let presence = homes.antigravity.join("presence");
    let conversations = homes.antigravity.join("conversations");
    std::fs::create_dir_all(&presence).unwrap();
    std::fs::create_dir_all(&conversations).unwrap();

    // A stand-in `agy` that holds its presence lock open, just like the real CLI.
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("agy");
    let script = format!(
        "#!/bin/sh\nexec 9>\"{}/$1.lock\"\necho \"FAKE-AGY running $1\"\nwhile :; do sleep 1; done\n",
        presence.display()
    );
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let agent_of = |c: &Core, pane: PaneId| c.state().pane(pane).and_then(|p| p.agent.clone());
    const A: &str = "5b161d7d-f369-4bf2-a5f5-37aced5dee01";
    const B: &str = "5b161d7d-f369-4bf2-a5f5-37aced5dee02";

    let pane;
    {
        let mut core = open(dir.path());
        core.dispatch(Command::SetDefaultShell(Some("sh".into()))).unwrap();
        core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None })
            .unwrap();
        pane = only_pane(&core);
        let term = core.terminal(pane).unwrap();
        std::thread::sleep(Duration::from_millis(500));

        term.text(&format!("{} {A}\r", fake.display()));
        wait_for_text(&mut core, pane, &format!("FAKE-AGY running {A}"));
        wait_for(&mut core, Duration::from_secs(20), |c, _| {
            agent_of(c, pane).is_some_and(|a| a.kind == AgentKind::Antigravity && a.session_id.as_deref() == Some(A))
        });

        // Quitting `agy` clears it.
        term.write(vec![3u8]);
        wait_for(&mut core, Duration::from_secs(20), |c, _| agent_of(c, pane).is_none());

        // Start a second conversation with a saved `.db` file so it resumes.
        std::fs::write(conversations.join(format!("{B}.db")), "").unwrap();
        term.text(&format!("{} {B}\r", fake.display()));
        wait_for_text(&mut core, pane, &format!("FAKE-AGY running {B}"));
        wait_for(&mut core, Duration::from_secs(20), |c, _| {
            agent_of(c, pane).is_some_and(|a| a.kind == AgentKind::Antigravity && a.session_id.as_deref() == Some(B))
        });
    }

    let mut core = open(dir.path());
    assert_eq!(agent_of(&core, pane).and_then(|a| a.session_id).as_deref(), Some(B));
    wait_for_text(&mut core, pane, &format!("agy --resume {B}"));
}

/// Windows builds the command line by joining the program and its arguments:
/// a program under `C:\Program Files` and an argument with spaces must both
/// arrive intact. Uses Git Bash, so it is skipped where that is not installed.
#[cfg(windows)]
#[test]
fn windows_shell_paths_and_arguments_keep_their_spaces() {
    use termroost_core::session::shell;
    let Some(git_bash) = shell::detect_shells().into_iter().find(|o| o.name == "Git Bash") else {
        eprintln!("Git Bash is not installed; skipping");
        return;
    };
    let bash = shell::split_command_line(&git_bash.command).unwrap().program;
    // Before arguments were quoted, bash got `-c echo` and printed an empty line.
    let command = shell::join_command_line(&[bash.as_str(), "-c", "echo spaced  args-ok; exec cat"]);
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(command))).unwrap();
    core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None }).unwrap();
    let pane = only_pane(&core);
    wait_for_text(&mut core, pane, "spaced args-ok");
}

/// Moving a session (as after making a git worktree) restarts its idle shells
/// in the new directory and leaves a terminal that runs a program alone.
#[test]
fn relocating_a_session_moves_idle_shells_only() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("app");
    let moved = dir.path().join("moved-here");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::create_dir_all(&moved).unwrap();
    // A plain interactive shell. The busy terminal runs a nested shell with a
    // command and no child process, so only its command line shows it is busy.
    // The typed commands print text that differs from the command line
    // itself, so echoed input never matches.
    let (shell, busy_line, show_dir) = if cfg!(windows) {
        ("cmd.exe", "powershell -NoProfile -Command \"Write-Output ('busy'+'-now'); Start-Sleep 60\"", "echo [%CD%]")
    } else {
        ("sh", "sh -c 'echo bu\"\"sy-now; read x'", "echo \"[$PWD]\"")
    };
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(shell.into()))).unwrap();
    core.dispatch(Command::CreateProject { name: "p".into(), working_dir: project_dir.clone(), shell: None }).unwrap();
    let tab = core.state().active_tab().unwrap().id;
    let idle = only_pane(&core);
    let Outcome::Pane(busy) = core.dispatch(Command::SplitPane { pane: idle, axis: Axis::Vertical }).unwrap() else {
        panic!()
    };
    core.terminal(busy).unwrap().write(format!("{busy_line}\r"));
    wait_for_text(&mut core, busy, "busy-now");
    std::thread::sleep(Duration::from_millis(500));
    // As if both shells had reported a directory of their own.
    for pane in [idle, busy] {
        core.apply_event(&Event::CwdChanged { pane, dir: dir.path().into() });
    }

    let outcome = core.dispatch(Command::RelocateTab { tab, working_dir: moved.clone() }).unwrap();
    assert_eq!(outcome, Outcome::Relocated { left_running: 1 });
    assert_eq!(core.state().tab(tab).unwrap().working_dir.as_deref(), Some(moved.as_path()));
    // The moved shell forgets where its old one was; the busy one is still there.
    assert_eq!(core.state().pane(idle).unwrap().cwd, None);
    assert_eq!(core.state().pane(busy).unwrap().cwd.as_deref(), Some(dir.path()));
    wait_for_text(&mut core, idle, "moved to");
    std::thread::sleep(Duration::from_millis(500));
    core.terminal(idle).unwrap().write(format!("{show_dir}\r"));
    wait_for_text(&mut core, idle, "moved-here]");
    assert!(!screen_text(&core, busy).contains("moved to"), "{}", screen_text(&core, busy));

    // Back in the project's directory, the session follows the project again.
    core.dispatch(Command::RelocateTab { tab, working_dir: project_dir }).unwrap();
    assert_eq!(core.state().tab(tab).unwrap().working_dir, None);
}

/// A restored terminal starts in the directory its shell last reported.
#[test]
fn restored_terminals_start_where_their_shell_was() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("app");
    let inner = project_dir.join("restored-here");
    std::fs::create_dir_all(&inner).unwrap();
    let (shell, show_dir) = if cfg!(windows) { ("cmd.exe", "echo [%CD%]") } else { ("sh", "echo \"[$PWD]\"") };
    let pane = {
        let mut core = open(dir.path());
        core.dispatch(Command::SetDefaultShell(Some(shell.into()))).unwrap();
        core.dispatch(Command::CreateProject { name: "p".into(), working_dir: project_dir.clone(), shell: None })
            .unwrap();
        let pane = only_pane(&core);
        // What the shell integration sends after a `cd`.
        core.apply_event(&Event::CwdChanged { pane, dir: inner.clone() });
        pane
    };

    let restart = || {
        let core = open(dir.path());
        std::thread::sleep(Duration::from_millis(500));
        core.terminal(pane).unwrap().write(format!("{show_dir}\r"));
        core
    };
    let mut core = restart();
    assert_eq!(core.state().pane(pane).unwrap().cwd.as_deref(), Some(inner.as_path()));
    wait_for_text(&mut core, pane, "restored-here]");
    drop(core);

    // Once that directory is gone, it starts in the project's directory.
    std::fs::remove_dir(&inner).unwrap();
    let mut core = restart();
    wait_for_text(&mut core, pane, "app]");
}

/// Waits until `needle` shows `count` times in the terminal.
fn wait_for_count(core: &mut Core, pane: PaneId, needle: &str, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    wait_for(core, Duration::from_secs(20), |c, _| {
        let text = screen_text(c, pane);
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?} {count} times:\n{text}");
        text.matches(needle).count() >= count
    });
}

/// Each terminal has its own shell history: after a restart, Up brings back
/// the last command typed in that terminal, not the one typed in another.
/// `shell` starts the shell; `history` is the pane's history file;
/// `command(x)` prints `<x>-42`. Returns the two terminals' panes.
fn restored_terminals_keep_their_own_history(
    dir: &Path,
    shell: &str,
    history: impl Fn(PaneId) -> std::path::PathBuf,
    command: impl Fn(&str) -> String,
) -> [PaneId; 2] {
    let wait = Duration::from_secs(15);
    let (a, b) = {
        let mut core = open(dir);
        core.dispatch(Command::SetDefaultShell(Some(shell.to_owned()))).unwrap();
        core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.into(), shell: None }).unwrap();
        let a = only_pane(&core);
        let Outcome::Pane(b) = core.dispatch(Command::SplitPane { pane: a, axis: Axis::Vertical }).unwrap() else {
            panic!()
        };
        for (pane, x) in [(a, "a"), (b, "b")] {
            // The file is there before the shell reads its first line: what
            // is typed can't end up in the user's own history.
            wait_for(&mut core, wait, |_, _| history(pane).is_file());
            core.terminal(pane).unwrap().write(format!("{}\r", command(x)));
            wait_for_text(&mut core, pane, &format!("{x}-42"));
            let saved = |_: &mut Core, _: Option<&Event>| {
                std::fs::read_to_string(history(pane)).is_ok_and(|t| t.trim_end().ends_with(&command(x)))
            };
            wait_for(&mut core, wait, saved);
        }
        (a, b)
    };

    let mut core = open(dir);
    for (pane, x) in [(a, "a"), (b, "b")] {
        // Typed before the shell reads lines, it waits for it.
        let before = screen_text(&core, pane).matches(&format!("{x}-42")).count();
        core.terminal(pane).unwrap().write("\x1b[A\r");
        wait_for_count(&mut core, pane, &format!("{x}-42"), before + 1);
    }
    [a, b]
}

#[test]
fn restored_terminals_keep_their_own_bash_history() {
    use termroost_core::session::shell;
    let Some(bash) = shell::detect_shells().into_iter().find(|o| o.name.to_lowercase().contains("bash")) else {
        eprintln!("no bash installed; skipping");
        return;
    };
    let bash = shell::split_command_line(&bash.command).unwrap().program;
    eprintln!("using {bash}");
    let dir = tempfile::tempdir().unwrap();
    let slashes = |p: &Path| p.display().to_string().replace('\\', "/");
    // bash's usual history, in the test's folder instead of the user's.
    let shared = dir.path().join("bash_history");
    std::fs::write(&shared, "echo from-shared\n").unwrap();
    let rc = dir.path().join("bashrc");
    std::fs::write(&rc, format!("HISTFILE='{}'\n", slashes(&shared))).unwrap();
    let shell = shell::join_command_line(&[bash.as_str(), "--rcfile", &slashes(&rc), "-i"]);
    let history = |pane: PaneId| dir.path().join(format!("shell-integration/history/{pane}.bash_history"));
    let panes =
        restored_terminals_keep_their_own_history(dir.path(), &shell, history, |x| format!("echo {x}-$((6*7))"));
    // A new terminal's history starts as a copy of the usual one, which
    // stays as it was.
    for pane in panes {
        assert!(std::fs::read_to_string(history(pane)).unwrap().starts_with("echo from-shared\n"));
    }
    assert_eq!(std::fs::read_to_string(&shared).unwrap(), "echo from-shared\n");
}

/// The same for Windows PowerShell, and PowerShell 7 where installed. Their
/// usual history (PSReadLine's) can't be moved to the test's folder; it is
/// only read.
#[cfg(windows)]
#[test]
fn restored_terminals_keep_their_own_powershell_history() {
    use termroost_core::session::shell;
    let path = std::env::var_os("PATH").unwrap_or_default();
    for exe in ["powershell", "pwsh"] {
        let Some(program) = shell::find_program(exe, &path) else {
            eprintln!("{exe} not installed; skipping it");
            continue;
        };
        eprintln!("using {}", program.display());
        let dir = tempfile::tempdir().unwrap();
        let shell = shell::join_command_line(&[program.display().to_string().as_str(), "-NoLogo", "-NoProfile"]);
        let history = |pane: PaneId| dir.path().join(format!("shell-integration/history/{pane}.ps_history"));
        restored_terminals_keep_their_own_history(dir.path(), &shell, history, |x| format!("echo \"{x}-$(6*7)\""));
    }
}

/// Git Bash's `bin\bash.exe` runs the real bash as a child; an idle Git Bash
/// must still count as idle when its session moves. Skipped without Git Bash.
#[cfg(windows)]
#[test]
fn relocating_moves_an_idle_git_bash() {
    use termroost_core::session::shell;
    let shells = shell::detect_shells();
    let Some(bash) = shells.iter().find(|o| o.name == "Git Bash" || o.name.contains("bash")) else {
        eprintln!("no bash installed; skipping");
        return;
    };
    eprintln!("using {}", bash.command);
    let dir = tempfile::tempdir().unwrap();
    let moved = dir.path().join("moved-here");
    std::fs::create_dir_all(&moved).unwrap();
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(bash.command.clone()))).unwrap();
    core.dispatch(Command::CreateProject { name: "p".into(), working_dir: dir.path().into(), shell: None }).unwrap();
    let tab = core.state().active_tab().unwrap().id;
    let pane = only_pane(&core);
    core.terminal(pane).unwrap().write("echo re\"\"ady\r");
    wait_for_text(&mut core, pane, "ready");
    // Git Bash's prompt runs git after each command; under load that child
    // can still be alive just after the output, which is busy, not idle.
    std::thread::sleep(Duration::from_millis(1500));

    let outcome = core.dispatch(Command::RelocateTab { tab, working_dir: moved }).unwrap();
    assert_eq!(outcome, Outcome::Relocated { left_running: 0 });
    wait_for_text(&mut core, pane, "moved to");
    std::thread::sleep(Duration::from_millis(1500));
    core.terminal(pane).unwrap().write("echo \"[$PWD]\"\r");
    wait_for_text(&mut core, pane, "moved-here]");
}

/// A new session can start in another directory, such as a git worktree.
#[test]
fn new_sessions_can_start_in_another_directory() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("app");
    let other = dir.path().join("app-other");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let (shell, show_dir) = if cfg!(windows) { ("cmd.exe", "echo [%CD%]") } else { ("sh", "echo \"[$PWD]\"") };
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(shell.into()))).unwrap();
    let Outcome::Project(project) = core
        .dispatch(Command::CreateProject { name: "p".into(), working_dir: project_dir.clone(), shell: None })
        .unwrap()
    else {
        panic!()
    };
    let new = |core: &mut Core, working_dir: &Path| {
        let cmd = Command::CreateTab { project, title: Some("x".into()), working_dir: Some(working_dir.into()) };
        let Outcome::Tab(tab) = core.dispatch(cmd).unwrap() else { panic!() };
        tab
    };
    // The project's own directory means "follow the project".
    let same = new(&mut core, &project_dir);
    assert_eq!(core.state().tab(same).unwrap().working_dir, None);

    let tab = new(&mut core, &other);
    assert_eq!(core.state().tab(tab).unwrap().working_dir.as_deref(), Some(other.as_path()));
    assert_eq!(core.state().active_tab().map(|t| t.id), Some(tab));
    let pane = core.state().tab(tab).unwrap().layout.first_pane();
    std::thread::sleep(Duration::from_millis(500));
    core.terminal(pane).unwrap().write(format!("{show_dir}\r"));
    wait_for_text(&mut core, pane, "app-other]");
}

/// Leaving a folder (before deleting a git worktree) closes the sessions in
/// it and moves idle shells of other sessions out, so that once their
/// processes have exited the folder can be deleted, even on Windows.
#[test]
fn leaving_a_folder_frees_it_for_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("app");
    let worktree = dir.path().join("app-wt");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::create_dir_all(worktree.join("sub")).unwrap();
    let (shell, busy_line) = if cfg!(windows) {
        ("cmd.exe", "powershell -NoProfile -Command \"Write-Output ('busy'+'-now'); Start-Sleep 60\"")
    } else {
        ("sh", "sh -c 'echo bu\"\"sy-now; read x'")
    };
    let mut core = open(dir.path());
    core.dispatch(Command::SetDefaultShell(Some(shell.into()))).unwrap();
    let Outcome::Project(project) = core
        .dispatch(Command::CreateProject { name: "p".into(), working_dir: project_dir.clone(), shell: None })
        .unwrap()
    else {
        panic!()
    };
    let home = core.state().active_tab().unwrap().id;
    let idle = only_pane(&core);
    let Outcome::Pane(busy) = core.dispatch(Command::SplitPane { pane: idle, axis: Axis::Vertical }).unwrap() else {
        panic!()
    };
    core.terminal(busy).unwrap().write(format!("{busy_line}\r"));
    wait_for_text(&mut core, busy, "busy-now");
    let mut new = |working_dir: std::path::PathBuf| {
        let cmd = Command::CreateTab { project, title: None, working_dir: Some(working_dir) };
        let Outcome::Tab(tab) = core.dispatch(cmd).unwrap() else { panic!() };
        tab
    };
    let in_worktree = new(worktree.clone());
    let in_sub = new(worktree.join("sub"));
    // Both home terminals say they `cd`ed into the worktree (they did not:
    // only the closed sessions' shells really work there).
    for pane in [idle, busy] {
        core.apply_event(&Event::CwdChanged { pane, dir: worktree.join("sub") });
    }
    std::thread::sleep(Duration::from_millis(500));

    let left = core.leave_dir(&worktree);
    assert_eq!(left.closed, vec![in_worktree, in_sub]);
    assert_eq!(left.busy, vec![busy]);
    assert!(!left.pids.is_empty());
    let s = core.state();
    assert!(s.tab(in_worktree).is_none() && s.tab(in_sub).is_none());
    assert!(s.tab(home).is_some());
    assert_eq!(s.pane(idle).unwrap().cwd, None, "the idle terminal left");
    assert!(s.pane(busy).unwrap().cwd.is_some(), "the busy one stays");
    wait_for_text(&mut core, idle, "being removed");

    assert!(left.wait(Duration::from_secs(10)), "the stopped shells exit");
    std::fs::remove_dir_all(&worktree).unwrap();
}

#[test]
fn programs_run_without_a_shell_and_end_their_terminal() {
    use termroost_core::session::shell::ShellCommand;
    let dir = tempfile::tempdir().unwrap();
    let mut core = open(dir.path());
    let (program, args) = if cfg!(windows) { ("cmd.exe", ["/c", "exit 4"]) } else { ("sh", ["-c", "exit 4"]) };
    let command = ShellCommand { program: program.into(), args: args.map(Into::into).to_vec() };
    let handle = core.open_program_terminal(dir.path(), command, Vec::new(), GridSize::default()).unwrap();
    let pane = handle.pane();
    assert!(pane.0 < 0, "outside the projects");

    let mut code = None;
    wait_for(&mut core, Duration::from_secs(15), |_, ev| {
        if let Some(Event::ProcessExited { pane: p, code: c }) = ev
            && *p == pane
        {
            code = *c;
            return true;
        }
        false
    });
    assert_eq!(code, Some(4), "its own exit status, no shell around it");
    core.close_scratch_terminal(pane);

    let missing = ShellCommand { program: "no-such-program-terminalrooster".into(), args: Vec::new() };
    assert!(core.open_program_terminal(dir.path(), missing, Vec::new(), GridSize::default()).is_err());
}
