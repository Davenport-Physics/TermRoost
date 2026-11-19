//! SQLite persistence.
//!
//! State is loaded once at startup. Afterwards the in-memory [`AppState`] is the
//! source of truth and every change is sent, in order, to a dedicated writer
//! thread as a batch of [`StoreOp`]s that is committed in one transaction. The
//! UI never waits on disk, and a crash loses at most the batch in flight.

pub mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use rusqlite::{Connection, OptionalExtension, params};

use crate::agent::{AgentKind, AgentSession};
use crate::error::Result;
use crate::layout::Layout;
use crate::model::*;

const KV_SELECTED_PROJECT: &str = "selected_project";
const KV_SETTINGS: &str = "settings";
const KV_GUI_PREFIX: &str = "gui:";

/// A single persisted change.
#[derive(Clone, Debug)]
pub enum StoreOp {
    UpsertProject(Project),
    DeleteProject(ProjectId),
    UpsertGroup(ProjectGroup),
    DeleteGroup(GroupId),
    UpsertTab(Tab),
    DeleteTab(TabId),
    UpsertPane(Pane),
    DeletePane(PaneId),
    UpsertTask(Task),
    DeleteTask(TaskId),
    UpsertNote(Note),
    DeleteNote(NoteId),
    SelectedProject(Option<ProjectId>),
    Settings(Settings),
    Gui(String, String),
    Scrollback { pane: PaneId, data: Vec<u8>, saved_at: i64 },
}

enum Msg {
    Batch(Vec<StoreOp>),
    Flush(mpsc::SyncSender<()>),
    Shutdown,
}

/// Cloneable handle for queueing writes from any thread.
#[derive(Clone)]
pub struct StoreWriter {
    tx: mpsc::Sender<Msg>,
}

impl StoreWriter {
    /// Queues a batch to be committed atomically.
    pub fn write(&self, ops: Vec<StoreOp>) {
        if !ops.is_empty() && self.tx.send(Msg::Batch(ops)).is_err() {
            tracing::error!("database writer thread is gone; change not saved");
        }
    }

    /// Blocks until every previously queued batch is committed.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self.tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }
}

/// Handle to the writer thread plus a read connection for lazy loads.
pub struct Store {
    writer_handle: StoreWriter,
    writer: Option<JoinHandle<()>>,
    reader: Connection,
}

impl Store {
    /// Opens (creating and migrating if needed) the database and loads all state.
    pub fn open(path: &Path) -> Result<(Store, AppState)> {
        let mut conn = Connection::open(path)?;
        schema::configure(&conn)?;
        schema::migrate(&mut conn)?;
        let state = load(&conn)?;

        let reader = Connection::open(path)?;
        schema::configure(&reader)?;

        let (tx, rx) = mpsc::channel();
        let writer =
            std::thread::Builder::new().name("terminalrooster-db".into()).spawn(move || writer_loop(conn, rx))?;
        Ok((Store { writer_handle: StoreWriter { tx }, writer: Some(writer), reader }, state))
    }

    pub fn writer(&self) -> StoreWriter {
        self.writer_handle.clone()
    }

    /// Queues a batch to be committed atomically.
    pub fn write(&self, ops: Vec<StoreOp>) {
        self.writer_handle.write(ops);
    }

    /// Blocks until every previously queued batch is committed.
    pub fn flush(&self) {
        self.writer_handle.flush();
    }

    /// Loads the saved scrollback of a pane.
    pub fn load_scrollback(&self, pane: PaneId) -> Option<(Vec<u8>, i64)> {
        self.reader
            .query_row("SELECT data, saved_at FROM pane_scrollback WHERE pane_id = ?1", [pane.0], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .unwrap_or_else(|e| {
                tracing::warn!("loading scrollback of pane {pane}: {e}");
                None
            })
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.writer_handle.tx.send(Msg::Shutdown);
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<Msg>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Batch(ops) => {
                if let Err(e) = apply_batch(&mut conn, &ops) {
                    tracing::error!("database write failed: {e} ({} ops)", ops.len());
                }
            }
            Msg::Flush(ack) => {
                let _ = ack.send(());
            }
            Msg::Shutdown => break,
        }
    }
    // Fold the WAL back into the main file on clean exit.
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn apply_batch(conn: &mut Connection, ops: &[StoreOp]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for op in ops {
        apply(&tx, op)?;
    }
    tx.commit()
}

fn apply(c: &Connection, op: &StoreOp) -> rusqlite::Result<()> {
    match op {
        StoreOp::UpsertProject(p) => {
            c.execute(
                "INSERT INTO projects (id, name, working_dir, shell, sort_order, active_tab_id, group_id, created_at, host)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, working_dir = excluded.working_dir,
                   shell = excluded.shell, sort_order = excluded.sort_order,
                   active_tab_id = excluded.active_tab_id, group_id = excluded.group_id, host = excluded.host",
                params![
                    p.id.0,
                    p.name,
                    path_str(&p.working_dir),
                    p.shell,
                    p.sort_order,
                    p.active_tab.map(|t| t.0),
                    p.group.map(|g| g.0),
                    p.created_at,
                    p.host
                ],
            )?;
        }
        StoreOp::DeleteProject(id) => {
            c.execute("DELETE FROM projects WHERE id = ?1", [id.0])?;
        }
        StoreOp::UpsertGroup(g) => {
            c.execute(
                "INSERT INTO project_groups (id, name, collapsed, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, collapsed = excluded.collapsed",
                params![g.id.0, g.name, g.collapsed, g.created_at],
            )?;
        }
        StoreOp::DeleteGroup(id) => {
            c.execute("DELETE FROM project_groups WHERE id = ?1", [id.0])?;
        }
        StoreOp::UpsertTab(t) => {
            c.execute(
                "INSERT INTO tabs (id, project_id, title, working_dir, layout, focused_pane_id, sort_order, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET project_id = excluded.project_id, title = excluded.title,
                   working_dir = excluded.working_dir, layout = excluded.layout,
                   focused_pane_id = excluded.focused_pane_id, sort_order = excluded.sort_order",
                params![
                    t.id.0,
                    t.project.0,
                    t.title,
                    t.working_dir.as_deref().map(path_str),
                    t.layout.to_json(),
                    t.focused_pane.map(|p| p.0),
                    t.sort_order,
                    t.created_at
                ],
            )?;
        }
        StoreOp::DeleteTab(id) => {
            c.execute("DELETE FROM tabs WHERE id = ?1", [id.0])?;
        }
        StoreOp::UpsertPane(p) => {
            let agent = p.agent.as_ref();
            c.execute(
                "INSERT INTO panes (id, tab_id, resume_command, agent_kind, agent_session, agent_args, cwd, created_at, keep_running)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET tab_id = excluded.tab_id,
                   resume_command = excluded.resume_command, agent_kind = excluded.agent_kind,
                   agent_session = excluded.agent_session, agent_args = excluded.agent_args,
                   cwd = excluded.cwd, keep_running = excluded.keep_running",
                params![
                    p.id.0,
                    p.tab.0,
                    p.resume_command,
                    agent.map(|a| a.kind.as_str()),
                    agent.and_then(|a| a.session_id.clone()),
                    agent.map(|a| a.args.clone()),
                    p.cwd.as_deref().map(path_str),
                    p.created_at,
                    p.keep_running
                ],
            )?;
        }
        StoreOp::DeletePane(id) => {
            c.execute("DELETE FROM panes WHERE id = ?1", [id.0])?;
        }
        StoreOp::UpsertTask(t) => {
            c.execute(
                "INSERT INTO tasks (id, project_id, tab_id, title, notes, done, sort_order, created_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET project_id = excluded.project_id, tab_id = excluded.tab_id,
                   title = excluded.title, notes = excluded.notes, done = excluded.done,
                   sort_order = excluded.sort_order, completed_at = excluded.completed_at",
                params![
                    t.id.0,
                    t.project.0,
                    t.tab.map(|x| x.0),
                    t.title,
                    t.notes,
                    t.done,
                    t.sort_order,
                    t.created_at,
                    t.completed_at
                ],
            )?;
        }
        StoreOp::DeleteTask(id) => {
            c.execute("DELETE FROM tasks WHERE id = ?1", [id.0])?;
        }
        StoreOp::UpsertNote(n) => {
            c.execute(
                "INSERT INTO notes (id, title, body, sort_order, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET title = excluded.title, body = excluded.body,
                   sort_order = excluded.sort_order",
                params![n.id.0, n.title, n.text, n.sort_order, n.created_at],
            )?;
        }
        StoreOp::DeleteNote(id) => {
            c.execute("DELETE FROM notes WHERE id = ?1", [id.0])?;
        }
        StoreOp::SelectedProject(p) => match p {
            Some(p) => set_kv(c, KV_SELECTED_PROJECT, &p.0.to_string())?,
            None => {
                c.execute("DELETE FROM kv WHERE key = ?1", [KV_SELECTED_PROJECT])?;
            }
        },
        StoreOp::Settings(s) => set_kv(c, KV_SETTINGS, &serde_json::to_string(s).expect("settings serialize"))?,
        StoreOp::Gui(k, v) => set_kv(c, &format!("{KV_GUI_PREFIX}{k}"), v)?,
        StoreOp::Scrollback { pane, data, saved_at } => {
            // The pane may have been closed while this save was queued.
            c.execute(
                "INSERT INTO pane_scrollback (pane_id, data, saved_at)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM panes WHERE id = ?1)
                 ON CONFLICT(pane_id) DO UPDATE SET data = excluded.data, saved_at = excluded.saved_at",
                params![pane.0, data, saved_at],
            )?;
        }
    }
    Ok(())
}

fn set_kv(c: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn load(c: &Connection) -> rusqlite::Result<AppState> {
    let mut state = AppState::default();

    let mut stmt = c.prepare(
        "SELECT id, name, working_dir, shell, sort_order, active_tab_id, group_id, created_at, host FROM projects",
    )?;
    state.projects = stmt
        .query_map([], |r| {
            Ok(Project {
                id: ProjectId(r.get(0)?),
                name: r.get(1)?,
                working_dir: PathBuf::from(r.get::<_, String>(2)?),
                shell: r.get(3)?,
                sort_order: r.get(4)?,
                active_tab: r.get::<_, Option<i64>>(5)?.map(TabId),
                group: r.get::<_, Option<i64>>(6)?.map(GroupId),
                created_at: r.get(7)?,
                host: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut stmt = c.prepare("SELECT id, name, collapsed, created_at FROM project_groups")?;
    state.groups = stmt
        .query_map([], |r| {
            Ok(ProjectGroup { id: GroupId(r.get(0)?), name: r.get(1)?, collapsed: r.get(2)?, created_at: r.get(3)? })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut stmt = c.prepare(
        "SELECT id, project_id, title, working_dir, layout, focused_pane_id, sort_order, created_at FROM tabs",
    )?;
    let raw_tabs: Vec<(Tab, String)> = stmt
        .query_map([], |r| {
            let layout_json: String = r.get(4)?;
            Ok((
                Tab {
                    id: TabId(r.get(0)?),
                    project: ProjectId(r.get(1)?),
                    title: r.get(2)?,
                    working_dir: r.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    // Placeholder; parsed and validated in `repair`.
                    layout: Layout::Pane(PaneId(-1)),
                    focused_pane: r.get::<_, Option<i64>>(5)?.map(PaneId),
                    sort_order: r.get(6)?,
                    created_at: r.get(7)?,
                },
                layout_json,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    state.tabs = raw_tabs
        .into_iter()
        .map(|(mut t, json)| {
            t.layout = Layout::from_json(&json).unwrap_or(Layout::Pane(PaneId(-1)));
            t
        })
        .collect();

    let mut stmt = c.prepare(
        "SELECT id, tab_id, resume_command, agent_kind, agent_session, agent_args, created_at, cwd, keep_running FROM panes",
    )?;
    state.panes = stmt
        .query_map([], |r| {
            let kind: Option<String> = r.get(3)?;
            let agent = kind.and_then(|k| AgentKind::parse(&k)).map(|kind| AgentSession {
                kind,
                session_id: r.get(4).ok().flatten(),
                args: r.get::<_, Option<String>>(5).ok().flatten().unwrap_or_default(),
            });
            Ok(Pane {
                id: PaneId(r.get(0)?),
                tab: TabId(r.get(1)?),
                resume_command: r.get(2)?,
                agent,
                cwd: r.get::<_, Option<String>>(7)?.map(PathBuf::from),
                keep_running: r.get(8)?,
                created_at: r.get(6)?,
            })
        })?
        .map(|p| p.map(|p| (p.id, p)))
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;

    let mut stmt = c.prepare(
        "SELECT id, project_id, tab_id, title, notes, done, sort_order, created_at, completed_at FROM tasks",
    )?;
    state.tasks = stmt
        .query_map([], |r| {
            Ok(Task {
                id: TaskId(r.get(0)?),
                project: ProjectId(r.get(1)?),
                tab: r.get::<_, Option<i64>>(2)?.map(TabId),
                title: r.get(3)?,
                notes: r.get(4)?,
                done: r.get(5)?,
                sort_order: r.get(6)?,
                created_at: r.get(7)?,
                completed_at: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut stmt = c.prepare("SELECT id, title, body, sort_order, created_at FROM notes")?;
    state.notes = stmt
        .query_map([], |r| {
            Ok(Note {
                id: NoteId(r.get(0)?),
                title: r.get(1)?,
                text: r.get(2)?,
                sort_order: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut stmt = c.prepare("SELECT key, value FROM kv")?;
    let kv: Vec<(String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (k, v) in kv {
        if k == KV_SELECTED_PROJECT {
            state.selected_project = v.parse().ok().map(ProjectId);
        } else if k == KV_SETTINGS {
            state.settings = serde_json::from_str(&v).unwrap_or_default();
        } else if let Some(gk) = k.strip_prefix(KV_GUI_PREFIX) {
            state.gui.insert(gk.to_owned(), v);
        }
    }

    state.sort();
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> (Store, AppState) {
        let (store, state) = Store::open(&dir.join("t.db")).unwrap();
        assert!(state.projects.is_empty());
        let p = Project {
            id: ProjectId(1),
            name: "api".into(),
            working_dir: "/tmp/api".into(),
            shell: None,
            host: None,
            sort_order: 0,
            active_tab: Some(TabId(10)),
            group: None,
            created_at: 1,
        };
        let t = Tab {
            id: TabId(10),
            project: p.id,
            title: "agents".into(),
            working_dir: None,
            layout: Layout::Pane(PaneId(100)),
            focused_pane: Some(PaneId(100)),
            sort_order: 0,
            created_at: 1,
        };
        let pane = Pane {
            id: PaneId(100),
            tab: t.id,
            resume_command: Some("make watch".into()),
            agent: Some(AgentSession {
                kind: AgentKind::Claude,
                session_id: Some("0b7e0c4e-1d8f-4a43-9a43-1f0c3f9b2a11".into()),
                args: "--model opus".into(),
            }),
            cwd: Some("/tmp/api/src".into()),
            keep_running: true,
            created_at: 1,
        };
        let task = Task {
            id: TaskId(5),
            project: p.id,
            tab: Some(t.id),
            title: "fix auth".into(),
            notes: String::new(),
            done: false,
            sort_order: 0,
            created_at: 1,
            completed_at: None,
        };
        store.write(vec![
            StoreOp::UpsertProject(p),
            StoreOp::UpsertTab(t),
            StoreOp::UpsertPane(pane),
            StoreOp::UpsertTask(task),
            StoreOp::SelectedProject(Some(ProjectId(1))),
            StoreOp::Scrollback { pane: PaneId(100), data: b"hello".to_vec(), saved_at: 7 },
            StoreOp::Gui("window".into(), "{}".into()),
        ]);
        store.flush();
        (store, state)
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = sample(dir.path());
        assert_eq!(store.load_scrollback(PaneId(100)), Some((b"hello".to_vec(), 7)));
        drop(store);

        let (_store, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(state.projects.len(), 1);
        assert_eq!(state.projects[0].active_tab, Some(TabId(10)));
        assert_eq!(state.tabs[0].layout, Layout::Pane(PaneId(100)));
        let pane = &state.panes[&PaneId(100)];
        assert_eq!(pane.resume_command.as_deref(), Some("make watch"));
        assert_eq!(pane.cwd.as_deref(), Some(Path::new("/tmp/api/src")));
        let agent = pane.agent.as_ref().unwrap();
        assert_eq!(agent.kind, AgentKind::Claude);
        assert_eq!(agent.args, "--model opus");
        assert!(agent.session_id.is_some());
        assert_eq!(state.tasks[0].tab, Some(TabId(10)));
        assert_eq!(state.selected_project, Some(ProjectId(1)));
        assert_eq!(state.gui_value("window"), Some("{}"));
    }

    #[test]
    fn delete_tab_unlinks_tasks_and_drops_panes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = sample(dir.path());
        store.write(vec![StoreOp::DeleteTab(TabId(10))]);
        store.flush();
        assert_eq!(store.load_scrollback(PaneId(100)), None);
        drop(store);
        let (_s, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert!(state.tabs.is_empty());
        assert!(state.panes.is_empty());
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].tab, None);
    }

    #[test]
    fn delete_project_cascades() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = sample(dir.path());
        store.write(vec![StoreOp::DeleteProject(ProjectId(1))]);
        store.flush();
        drop(store);
        let (_s, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert!(state.projects.is_empty());
        assert!(state.tabs.is_empty());
        assert!(state.panes.is_empty());
        assert!(state.tasks.is_empty());
    }

    #[test]
    fn upsert_does_not_cascade() {
        // ON CONFLICT DO UPDATE must not delete children the way INSERT OR REPLACE would.
        let dir = tempfile::tempdir().unwrap();
        let (store, state0) = sample(dir.path());
        drop(state0);
        let mut p = Project {
            id: ProjectId(1),
            name: "renamed".into(),
            working_dir: "/tmp/api".into(),
            shell: None,
            host: None,
            sort_order: 0,
            active_tab: Some(TabId(10)),
            group: None,
            created_at: 1,
        };
        store.write(vec![StoreOp::UpsertProject(p.clone())]);
        p.name = "renamed2".into();
        store.write(vec![StoreOp::UpsertProject(p)]);
        store.flush();
        drop(store);
        let (_s, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(state.projects[0].name, "renamed2");
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tasks.len(), 1);
    }

    #[test]
    fn groups_round_trip_and_deleting_one_frees_its_projects() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = sample(dir.path());
        let group = ProjectGroup { id: GroupId(20), name: "work".into(), collapsed: true, created_at: 2 };
        let mut p = Project {
            id: ProjectId(1),
            name: "api".into(),
            working_dir: "/tmp/api".into(),
            shell: None,
            host: None,
            sort_order: 0,
            active_tab: Some(TabId(10)),
            group: Some(group.id),
            created_at: 1,
        };
        store.write(vec![StoreOp::UpsertGroup(group.clone()), StoreOp::UpsertProject(p.clone())]);
        store.flush();
        drop(store);
        let (store, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(state.groups, vec![group.clone()]);
        assert_eq!(state.projects[0].group, Some(group.id));

        store.write(vec![StoreOp::DeleteGroup(group.id)]);
        store.flush();
        drop(store);
        let (_s, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert!(state.groups.is_empty());
        p.group = None;
        assert_eq!(state.projects, [p], "the project stays, outside any group");
    }

    #[test]
    fn notes_round_trip_until_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = sample(dir.path());
        let note = |id, title: &str, text: &str, sort_order| Note {
            id: NoteId(id),
            title: title.into(),
            text: text.into(),
            sort_order,
            created_at: 3,
        };
        let mut kept = note(30, "Note 1", "line one\r\n\tline two\n", 0);
        store.write(vec![StoreOp::UpsertNote(kept.clone()), StoreOp::UpsertNote(note(31, "Note 2", "", 1))]);
        kept.title = "keys".into();
        kept.text.push('ü');
        store.write(vec![StoreOp::UpsertNote(kept.clone()), StoreOp::DeleteNote(NoteId(31))]);
        store.flush();
        drop(store);
        let (_s, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(state.notes, [kept], "the text exactly as it was, and no closed note");
    }

    #[test]
    fn scrollback_for_missing_pane_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = sample(dir.path());
        store.write(vec![StoreOp::Scrollback { pane: PaneId(999), data: vec![1], saved_at: 1 }]);
        store.write(vec![StoreOp::Gui("after".into(), "ok".into())]);
        store.flush();
        drop(store);
        let (_s, state) = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(state.gui_value("after"), Some("ok"));
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        drop(Store::open(&path).unwrap());
        drop(Store::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v as usize, schema::version());
    }
}
