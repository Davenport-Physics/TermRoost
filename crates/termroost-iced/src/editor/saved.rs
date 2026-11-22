//! The files each session has open, kept across restarts (not their unsaved
//! changes). They come back unread and are read, or their terminal editor
//! started, when they are first shown.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use termroost_core::TabId;

use super::{DocId, DocKind, OpenAs, OpenFile, SessionFiles, picture};

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
    /// It was shown as a picture (an SVG can be shown as text too).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    picture: bool,
}

impl SavedFile {
    fn of(doc: &OpenFile) -> SavedFile {
        let how = match doc.kind {
            DocKind::Terminal { .. } => OpenAs::Terminal,
            DocKind::Picture(_) => OpenAs::Picture,
            DocKind::Restored(how) => how,
            // Still being read: a raster file can only be a picture.
            DocKind::Loading if picture::kind_of(&doc.path) == Some(picture::Kind::Raster) => OpenAs::Picture,
            _ => OpenAs::Text,
        };
        SavedFile { path: doc.path.clone(), terminal: how == OpenAs::Terminal, picture: how == OpenAs::Picture }
    }

    fn open_as(&self) -> OpenAs {
        match (self.terminal, self.picture) {
            (true, _) => OpenAs::Terminal,
            (_, true) => OpenAs::Picture,
            _ => OpenAs::Text,
        }
    }
}

impl Saved {
    /// The files open in each session; commits shown among them are left out.
    pub fn of<'a>(editors: impl IntoIterator<Item = (&'a TabId, &'a SessionFiles)>) -> Saved {
        let sessions = editors.into_iter().filter_map(|(tab, files)| {
            let kept: Vec<&OpenFile> = files.open.iter().filter(|d| d.is_file()).collect();
            if kept.is_empty() {
                return None;
            }
            let saved = kept.iter().map(|d| SavedFile::of(d)).collect();
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
                    .map(|f| OpenFile { id: next_id(), kind: DocKind::Restored(f.open_as()), path: f.path })
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

    use OpenAs::{Picture, Terminal, Text};

    fn files(paths: &[(&str, OpenAs)], shown: Option<usize>) -> SessionFiles {
        let open: Vec<OpenFile> = paths
            .iter()
            .enumerate()
            .map(|(i, (p, how))| OpenFile {
                id: DocId(i as u64 + 1),
                path: PathBuf::from(p),
                kind: if *how == Text { DocKind::Loading } else { DocKind::Restored(*how) },
            })
            .collect();
        let shown = shown.map(|i| open[i].id);
        SessionFiles { open, shown }
    }

    fn restore_all(saved: Saved) -> Vec<(TabId, SessionFiles)> {
        let mut n = 100;
        saved.restore(
            |_| true,
            || {
                n += 1;
                DocId(n)
            },
        )
    }

    #[test]
    fn open_files_survive_a_restart() {
        let editors = BTreeMap::from([
            (TabId(1), files(&[("/w/a.rs", Text), ("/w/b.md", Terminal)], Some(1))),
            (TabId(2), files(&[], None)),
            (TabId(3), files(&[("/x/c.txt", Text)], None)),
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
        assert!(matches!(files.open[0].kind, DocKind::Restored(Text)));
        assert!(matches!(files.open[1].kind, DocKind::Restored(Terminal)));
        assert_eq!(files.shown, Some(files.open[1].id));
    }

    #[test]
    fn pictures_come_back_as_pictures() {
        let editors = BTreeMap::from([(
            TabId(1),
            files(&[("/w/logo.svg", Picture), ("/w/icon.svg", Text), ("/w/shot.png", Text)], None),
        )]);
        let json = serde_json::to_string(&Saved::of(editors.iter())).unwrap();
        let restored = restore_all(serde_json::from_str(&json).unwrap());
        let kinds: Vec<_> = restored[0].1.open.iter().map(|d| &d.kind).collect();
        assert!(matches!(kinds[0], DocKind::Restored(Picture)));
        assert!(matches!(kinds[1], DocKind::Restored(Text)), "an SVG shown as text stays text");
        assert!(matches!(kinds[2], DocKind::Restored(Picture)), "a PNG still being read is a picture");
    }

    #[test]
    fn unreadable_records_are_nothing() {
        assert!(serde_json::from_str::<Saved>("{\"1\":{\"files\":[{\"path\":\"/a\"}]}}").is_ok(), "old fields default");
        assert!(serde_json::from_str::<Saved>("[1,2]").is_err());
    }

    #[test]
    fn files_kept_before_pictures_open_as_text() {
        let old = "{\"1\":{\"files\":[{\"path\":\"/w/logo.svg\",\"terminal\":false}]}}";
        let restored = restore_all(serde_json::from_str(old).unwrap());
        assert!(matches!(restored[0].1.open[0].kind, DocKind::Restored(Text)));
    }

    #[test]
    fn commits_are_left_out() {
        let commit = |id: u64| {
            let repo = termroost_core::git::Repo {
                worktree: "/w".into(),
                git_dir: "/w/.git".into(),
                common_dir: "/w/.git".into(),
            };
            let doc = crate::git_view::CommitDoc { repo, id: "abc".into(), details: None, toggled: Default::default() };
            OpenFile { id: DocId(id), path: "/w".into(), kind: DocKind::Commit(Box::new(doc)) }
        };
        let mut with_files = files(&[("/w/a.rs", Text), ("/w/b.rs", Text)], Some(1));
        with_files.open.insert(0, commit(9));
        let only_a_commit = SessionFiles { open: vec![commit(10)], shown: Some(DocId(10)) };
        let editors = BTreeMap::from([(TabId(1), with_files), (TabId(2), only_a_commit)]);
        let restored = restore_all(Saved::of(editors.iter()));
        assert_eq!(restored.len(), 1, "a session with only a commit open has nothing to keep");
        let (_, files) = &restored[0];
        assert_eq!(files.open.len(), 2);
        assert_eq!(files.open[1].path, PathBuf::from("/w/b.rs"));
        assert_eq!(files.shown, Some(files.open[1].id), "still the file that was shown");
    }
}
