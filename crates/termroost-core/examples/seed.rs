//! Fills a data directory with demo projects, tabs, splits and tasks.
//!
//! Usage: `TERMINALROOSTER_DATA_DIR=/tmp/terminalrooster-demo cargo run -p termroost-core --example seed -- <dir>...`
//! Each given directory becomes a project (at least one is required).

use std::time::Duration;

use termroost_core::{Axis, Command, Config, Core, Outcome};

fn main() {
    let dirs: Vec<std::path::PathBuf> = std::env::args().skip(1).map(Into::into).collect();
    assert!(!dirs.is_empty(), "pass at least one project directory");
    let mut core = Core::open(Config::default()).expect("open core");
    let demo = "printf '\\033[1;31mbold red\\033[0m \\033[3mitalic\\033[0m \\033[4munderline\\033[0m \
                \\033[42;30m green bg \\033[0m \\033[38;2;255;160;60mtruecolor\\033[0m\\n\
                wide: 漢字テスト  symbols: ✻ ⏺ ⎿ ─│┌┐ → ✓\\n'; exec sh";
    if !cfg!(windows) {
        core.dispatch(Command::SetDefaultShell(Some(format!("sh -c \"{demo}\"")))).unwrap();
    }
    for (i, dir) in dirs.iter().enumerate() {
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| format!("project {i}"));
        let Outcome::Project(project) =
            core.dispatch(Command::CreateProject { name, working_dir: dir.clone(), shell: None, host: None }).unwrap()
        else {
            unreachable!()
        };
        let tab = core.state().active_tab().unwrap().id;
        core.dispatch(Command::RenameTab { tab, title: "agents".into() }).unwrap();
        let first = core.state().tab(tab).unwrap().layout.first_pane();
        if let Outcome::Pane(right) = core.dispatch(Command::SplitPane { pane: first, axis: Axis::Vertical }).unwrap() {
            core.dispatch(Command::SplitPane { pane: right, axis: Axis::Horizontal }).unwrap();
        }
        core.dispatch(Command::CreateTab { project, title: Some("build".into()), working_dir: None }).unwrap();
        core.dispatch(Command::CreateTab { project, title: Some("logs".into()), working_dir: None }).unwrap();
        core.dispatch(Command::CreateTask { project, title: "Fix auth token refresh".into(), tab: Some(tab) }).unwrap();
        core.dispatch(Command::CreateTask { project, title: "Add CI workflow".into(), tab: None }).unwrap();
        if let Outcome::Task(t) =
            core.dispatch(Command::CreateTask { project, title: "Write README".into(), tab: None }).unwrap()
        {
            core.dispatch(Command::SetTaskDone { task: t, done: true }).unwrap();
        }
        core.dispatch(Command::SelectTab(tab)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(500));
    println!("seeded {} project(s) in {}", dirs.len(), core.data_dir().display());
}
