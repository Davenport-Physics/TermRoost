//! Connects to an SSH host as Terminal Rooster does, and says what happens:
//! the connection (copying the helper there first if needed), a terminal in
//! the folder there, its files and git. Uses data of its own, not the app's.
//!
//! ```sh
//! cargo build -p termroost-remote   # the helper it copies to the host
//! cargo run -p termroost-core --example remote -- droplet:~/app
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use termroost_core::*;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let (host, folder) = match arg.split_once(':') {
        Some((h, f)) => (h.to_owned(), if f.is_empty() { "~".to_owned() } else { f.to_owned() }),
        None if !arg.is_empty() => (arg.clone(), "~".to_owned()),
        None => {
            eprintln!("usage: remote HOST[:FOLDER]");
            std::process::exit(2);
        }
    };
    let data = tempfile::tempdir().expect("a temporary folder");
    let mut core = Core::open(Config { data_dir: Some(data.path().to_path_buf()), ..Config::default() }).expect("core");
    let started = Instant::now();
    let say = |what: &str| println!("[{:>5.1}s] {what}", started.elapsed().as_secs_f32());

    say(&format!("asking {host} for {folder}"));
    let dir = match core.machine(Some(&host)).resolve_dir(&PathBuf::from(&folder)) {
        Ok(dir) => dir,
        Err(e) => {
            say(&format!("failed: {e}"));
            std::process::exit(1);
        }
    };
    say(&format!("connected; the folder is {}", dir.display()));
    let project = match core
        .dispatch(Command::CreateProject {
            name: "remote".into(),
            working_dir: dir.clone(),
            shell: None,
            host: Some(host.clone()),
        })
        .expect("a project")
    {
        Outcome::Project(p) => p,
        _ => unreachable!(),
    };
    let pane = *core.state().panes.keys().next().expect("a terminal");
    let events = core.events();
    let screen = |core: &Core| core.terminal(pane).map(|h| h.with_content(|c| c.text())).unwrap_or_default();
    let wait = |core: &mut Core, needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !screen(core).contains(needle) {
            while let Ok(e) = events.try_recv() {
                if let Event::Remote { status, .. } = &e {
                    say(&format!("connection: {status:?}"));
                }
                core.apply_event(&e);
            }
            if Instant::now() > deadline {
                say(&format!("gave up waiting for {needle:?}; the screen:\n{}", screen(core)));
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    core.terminal(pane).unwrap().write("echo rooster-$((6*7)); uname -sr; pwd\r");
    wait(&mut core, "rooster-42");
    say("the terminal runs there:");
    println!("{}", screen(&core).trim_end());

    let machine = core.project_machine(project);
    match &machine.list_dirs(vec![dir.clone()])[0].1 {
        Ok(entries) => say(&format!("{} entries in the folder", entries.len())),
        Err(e) => say(&format!("listing failed: {e}")),
    }
    match machine.git_status(&dir) {
        Some(status) => say(&format!("git: {}", status.head.map(|h| h.to_string()).unwrap_or_default())),
        None => say("git: not a repository"),
    }

    core.reconnect(&host);
    say("dropped the connection");
    // Typed while it is down, it goes out once the terminal is back.
    core.terminal(pane).unwrap().write("echo back-$((50+5))\r");
    wait(&mut core, "back-55");
    say("connected again: the same shell, which got what was typed meanwhile");
    core.shutdown();
    say("done");
}
