//! Database schema and migrations, tracked with `PRAGMA user_version`.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE projects (
        id            INTEGER PRIMARY KEY,
        name          TEXT NOT NULL,
        working_dir   TEXT NOT NULL,
        shell         TEXT,
        sort_order    INTEGER NOT NULL DEFAULT 0,
        active_tab_id INTEGER,
        created_at    INTEGER NOT NULL
    );
    CREATE TABLE tabs (
        id              INTEGER PRIMARY KEY,
        project_id      INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        title           TEXT NOT NULL,
        working_dir     TEXT,
        layout          TEXT NOT NULL,
        focused_pane_id INTEGER,
        sort_order      INTEGER NOT NULL DEFAULT 0,
        created_at      INTEGER NOT NULL
    );
    CREATE INDEX tabs_project ON tabs(project_id);
    CREATE TABLE panes (
        id             INTEGER PRIMARY KEY,
        tab_id         INTEGER NOT NULL REFERENCES tabs(id) ON DELETE CASCADE,
        resume_command TEXT,
        agent_kind     TEXT,
        agent_session  TEXT,
        agent_args     TEXT,
        created_at     INTEGER NOT NULL
    );
    CREATE INDEX panes_tab ON panes(tab_id);
    CREATE TABLE pane_scrollback (
        pane_id  INTEGER PRIMARY KEY REFERENCES panes(id) ON DELETE CASCADE,
        data     BLOB NOT NULL,
        saved_at INTEGER NOT NULL
    );
    CREATE TABLE tasks (
        id           INTEGER PRIMARY KEY,
        project_id   INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        tab_id       INTEGER REFERENCES tabs(id) ON DELETE SET NULL,
        title        TEXT NOT NULL,
        notes        TEXT NOT NULL DEFAULT '',
        done         INTEGER NOT NULL DEFAULT 0,
        sort_order   INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL,
        completed_at INTEGER
    );
    CREATE INDEX tasks_project ON tasks(project_id);
    CREATE INDEX tasks_tab ON tasks(tab_id);
    CREATE TABLE kv (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // 2: the directory each terminal's shell was last in
    "ALTER TABLE panes ADD COLUMN cwd TEXT;",
    // 3: project groups
    r#"
    CREATE TABLE project_groups (
        id         INTEGER PRIMARY KEY,
        name       TEXT NOT NULL,
        collapsed  INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL
    );
    ALTER TABLE projects ADD COLUMN group_id INTEGER REFERENCES project_groups(id) ON DELETE SET NULL;
    "#,
    // 4: terminals that keep running when TerminalRooster quits
    "ALTER TABLE panes ADD COLUMN keep_running INTEGER NOT NULL DEFAULT 0;",
    // 5: notes
    r#"
    CREATE TABLE notes (
        id         INTEGER PRIMARY KEY,
        title      TEXT NOT NULL,
        body       TEXT NOT NULL DEFAULT '',
        sort_order INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL
    );
    "#,
    // 6: projects on remote hosts
    "ALTER TABLE projects ADD COLUMN host TEXT;",
];

/// Applies connection pragmas used by every connection.
pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Brings the schema up to date.
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current.max(0) as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
        tracing::info!("applied database migration {}", i + 1);
    }
    Ok(())
}

pub fn version() -> usize {
    MIGRATIONS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_of_a_version_1_database_get_no_directory() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, name, working_dir, created_at) VALUES (1, 'p', '/p', 0);
             INSERT INTO tabs (id, project_id, title, layout, created_at) VALUES (2, 1, 't', '{}', 0);
             INSERT INTO panes (id, tab_id, created_at) VALUES (3, 2, 0);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let cwd: Option<String> = conn.query_row("SELECT cwd FROM panes WHERE id = 3", [], |r| r.get(0)).unwrap();
        assert_eq!(cwd, None);
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v as usize, version());
    }
}
