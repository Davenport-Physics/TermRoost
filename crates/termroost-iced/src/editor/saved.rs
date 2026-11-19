//! The files each session has open, kept across restarts (not their unsaved
//! changes). They come back unread and are read, or their terminal editor
//! started, when they are first shown.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use termroost_core::TabId;

use super::{DocId, DocKind, OpenFile, SessionFiles};

/// Key in the core's front-end storage. Not in the preferences: those are
/// written again on every window move.
pub const KEY: &str = "iced.open_files";

/// Sessions (by id) and their open files.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Saved(BTreeMap<i64, SavedSession>);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedSession {
    files: Vec<SavedFile>,
    /// Which of them is shown in place of the terminals.
    #[serde(default)]
    shown: Option<usize>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedFile {
    path: PathBuf,
    /// It was open in a terminal editor.
    #[serde(default)]
    terminal: bool,
}

impl Saved {
    /// The files open in each session; commits shown among them are left out.
    pub fn of<'a>(editors: impl IntoIterator<Item = (&'a TabId, &'a SessionFiles)>) -> Saved {
        let sessions = editors.into_iter().filter_map(|(tab, files)| {
            let kept: Vec<&OpenFile> = files.open.iter().filter(|d| d.is_file()).collect();
            if kept.is_empty() {
                return None;
            }
            let saved = kept
                .iter()
                .map(|d| SavedFile {
                    path: d.path.clone(),
                    terminal: matches!(d.kind, DocKind::Terminal { .. } | DocKind::Restored { terminal: true }),
                })
                .collect();
            let shown = files.shown.and_then(|id| kept.iter().position(|d| d.id == id));
            Some((tab.0, SavedSession { files: saved, shown }))
        });
        Saved(sessions.collect())
    }

    /// The open files of the sessions in `exists`, unread, with ids from `next_id`.
    pub fn restore(
        self,
        exists: impl Fn(TabId) -> bool,
        mut next_id: impl FnMut() -> DocId,
    ) -> Vec<(TabId, SessionFiles)> {
        self.0
            .into_iter()
            .map(|(tab, session)| (TabId(tab), session))
            .filter(|(tab, _)| exists(*tab))
            .map(|(tab, session)| {
                let open: Vec<OpenFile> = session
                    .files
                    .into_iter()
                    .map(|f| OpenFile { id: next_id(), path: f.path, kind: DocKind::Restored { terminal: f.terminal } })
                    .collect();
                let shown = session.shown.and_then(|i| open.get(i)).map(|d| d.id);
                (tab, SessionFiles { open, shown })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[(&str, bool)], shown: Option<usize>) -> SessionFiles {
        let open: Vec<OpenFile> = paths
            .iter()
            .enumerate()
            .map(|(i, (p, terminal))| OpenFile {
                id: DocId(i as u64 + 1),
                path: PathBuf::from(p),
                kind: if *terminal { DocKind::Restored { terminal: true } } else { DocKind::Loading },
            })
            .collect();
        let shown = shown.map(|i| open[i].id);
        SessionFiles { open, shown }
    }

    #[test]
    fn open_files_survive_a_restart() {
        let editors = BTreeMap::from([
            (TabId(1), files(&[("/w/a.rs", false), ("/w/b.md", true)], Some(1))),
            (TabId(2), files(&[], None)),
            (TabId(3), files(&[("/x/c.txt", false)], None)),
        ]);
        let json = serde_json::to_string(&Saved::of(editors.iter())).unwrap();
        assert!(!json.contains("\"2\""), "sessions without open files aren't kept: {json}");

        let saved: Saved = serde_json::from_str(&json).unwrap();
        let mut n = 100;
        let restored = saved.restore(
            |t| t != TabId(3),
            || {
                n += 1;
                DocId(n)
            },
        );
        assert_eq!(restored.len(), 1, "session 3 is gone");
        let (tab, files) = &restored[0];
        assert_eq!(*tab, TabId(1));
        let paths: Vec<_> = files.open.iter().map(|d| d.path.to_string_lossy().into_owned()).collect();
        assert_eq!(paths, ["/w/a.rs", "/w/b.md"]);
        assert!(matches!(files.open[0].kind, DocKind::Restored { terminal: false }));
        assert!(matches!(files.open[1].kind, DocKind::Restored { terminal: true }));
        assert_eq!(files.shown, Some(files.open[1].id));
    }

    #[test]
    fn unreadable_records_are_nothing() {
        assert!(serde_json::from_str::<Saved>("{\"1\":{\"files\":[{\"path\":\"/a\"}]}}").is_ok(), "old fields default");
        assert!(serde_json::from_str::<Saved>("[1,2]").is_err());
    }

    #[test]
    fn commits_are_left_out() {
        let commit = |id: u64| {
            let repo = termroost_core::git::Repo {
                machine: Default::default(),
                worktree: "/w".into(),
                git_dir: "/w/.git".into(),
                common_dir: "/w/.git".into(),
            };
            let doc = crate::git_view::CommitDoc { repo, id: "abc".into(), details: None, toggled: Default::default() };
            OpenFile { id: DocId(id), path: "/w".into(), kind: DocKind::Commit(Box::new(doc)) }
        };
        let mut with_files = files(&[("/w/a.rs", false), ("/w/b.rs", false)], Some(1));
        with_files.open.insert(0, commit(9));
        let only_a_commit = SessionFiles { open: vec![commit(10)], shown: Some(DocId(10)) };
        let editors = BTreeMap::from([(TabId(1), with_files), (TabId(2), only_a_commit)]);
        let mut n = 100;
        let restored = Saved::of(editors.iter()).restore(
            |_| true,
            || {
                n += 1;
                DocId(n)
            },
        );
        assert_eq!(restored.len(), 1, "a session with only a commit open has nothing to keep");
        let (_, files) = &restored[0];
        assert_eq!(files.open.len(), 2);
        assert_eq!(files.open[1].path, PathBuf::from("/w/b.rs"));
        assert_eq!(files.shown, Some(files.open[1].id), "still the file that was shown");
    }
}
