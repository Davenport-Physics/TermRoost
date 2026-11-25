//! Changes the user ignored: a file's change not staged, such as a setting
//! pointed at a local server, that is never meant to be committed. While it
//! is exactly the change ignored it is kept apart from the unstaged changes
//! (so staging all of them leaves it), by its fingerprint: the blobs on both
//! sides. Changing the file again, or what it is compared with (staging some
//! of it, a pull), makes another change, which is not ignored, and is marked
//! as having been until it is ignored again or forgotten.
//!
//! What is ignored is kept per working tree, in its git directory, as JSON
//! (path to fingerprint): git leaves files it doesn't know there alone, and
//! a linked worktree's goes with it when it is removed.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::PathBuf;

use super::Repo;
use super::diff::{FileDiff, FileStatus, WorkingChanges};

/// The file in a working tree's git directory.
const FILE: &str = "terminalrooster-ignored.json";

/// The fingerprint of each ignored change, by path.
pub(super) type Ignored = BTreeMap<String, String>;

impl Repo {
    /// Ignores the change of `file`, a file of the unstaged changes, as
    /// `file` has it: what the user saw, even if it changed since.
    pub fn ignore_change(&self, file: &FileDiff) -> Result<(), String> {
        let fingerprint = match file.status {
            FileStatus::Conflicted | FileStatus::Untracked => None,
            _ => file.fingerprint(),
        };
        let fingerprint = fingerprint.ok_or_else(|| format!("The change to {} can't be ignored.", file.path))?;
        let mut ignored = self.ignored()?;
        ignored.insert(file.path.clone(), fingerprint);
        self.save_ignored(&ignored)
    }

    /// Stops ignoring these files' changes, or marking them as ignored before.
    pub fn stop_ignoring<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
        let mut ignored = self.ignored()?;
        let before = ignored.len();
        for path in paths {
            ignored.remove(path);
        }
        if ignored.len() == before {
            return Ok(());
        }
        self.save_ignored(&ignored)
    }

    fn ignored_file(&self) -> PathBuf {
        self.git_dir.join(FILE)
    }

    /// What is ignored in this working tree.
    pub(super) fn ignored(&self) -> Result<Ignored, String> {
        let path = self.ignored_file();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("Could not read the ignored changes in {}: {e}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Ignored::new()),
            Err(e) => Err(format!("Could not read the ignored changes in {}: {e}", path.display())),
        }
    }

    fn save_ignored(&self, ignored: &Ignored) -> Result<(), String> {
        let path = self.ignored_file();
        let failed = |e: std::io::Error| format!("Could not save the ignored changes in {}: {e}", path.display());
        if ignored.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != ErrorKind::NotFound => Err(failed(e)),
                _ => Ok(()),
            };
        }
        let json = serde_json::to_vec_pretty(ignored).map_err(|e| e.to_string())?;
        // Written whole beside it, then put in its place: a crash leaves the
        // old list or the new one, never half of one.
        let fresh = self.git_dir.join(format!("{FILE}.new"));
        std::fs::write(&fresh, json).and_then(|()| std::fs::rename(&fresh, &path)).map_err(failed)
    }
}

/// Moves the unstaged changes that are ignored, exactly as they are, into
/// `changes.ignored`, and marks the other staged or unstaged files that
/// were ignored.
pub(super) fn set_aside(changes: &mut WorkingChanges, ignored: &Ignored) {
    if ignored.is_empty() {
        return;
    }
    let holds = |f: &FileDiff| {
        f.status != FileStatus::Conflicted && f.fingerprint().is_some_and(|p| ignored.get(&f.path) == Some(&p))
    };
    let (aside, kept) = std::mem::take(&mut changes.unstaged).into_iter().partition(holds);
    changes.unstaged = kept;
    changes.ignored = aside;
    let still = |path: &String| changes.ignored.iter().any(|f| &f.path == path);
    let was = changes.staged.iter().chain(&changes.unstaged).map(|f| &f.path);
    changes.was_ignored = was.filter(|p| ignored.contains_key(*p) && !still(p)).cloned().collect();
}

#[cfg(test)]
mod tests {
    use super::super::diff::tests::{repo, run, write};
    use super::*;

    fn paths(files: &[FileDiff]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn ignored_changes_stay_apart_until_they_change_with_git() {
        let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
        write(&repo, "app.cfg", "url = prod\nretries = 3\n");
        write(&repo, "main.rs", "fn main() {}\n");
        repo.stage_paths(["app.cfg", "main.rs"]).unwrap();
        repo.commit("first", false).unwrap();
        write(&repo, "app.cfg", "url = local\nretries = 3\n");
        write(&repo, "main.rs", "fn main() { run() }\n");

        let changes = repo.working_changes().unwrap();
        let cfg = changes.unstaged.iter().find(|f| f.path == "app.cfg").unwrap();
        repo.ignore_change(cfg).unwrap();
        let changes = repo.working_changes().unwrap();
        assert_eq!(paths(&changes.unstaged), vec!["main.rs"]);
        assert_eq!(paths(&changes.ignored), vec!["app.cfg"]);
        assert!(changes.was_ignored.is_empty());

        // Changed again: no longer ignored, and marked.
        write(&repo, "app.cfg", "url = local\nretries = 9\n");
        let changes = repo.working_changes().unwrap();
        assert_eq!(paths(&changes.unstaged), vec!["app.cfg", "main.rs"]);
        assert!(changes.ignored.is_empty());
        assert_eq!(changes.was_ignored.iter().collect::<Vec<_>>(), vec!["app.cfg"]);

        // Back as it was ignored: ignored again.
        write(&repo, "app.cfg", "url = local\nretries = 3\n");
        assert_eq!(paths(&repo.working_changes().unwrap().ignored), vec!["app.cfg"]);

        // Staged (from a terminal): marked among the staged changes.
        repo.stage_paths(["app.cfg"]).unwrap();
        let changes = repo.working_changes().unwrap();
        assert_eq!(paths(&changes.staged), vec!["app.cfg"]);
        assert!(changes.ignored.is_empty());
        assert!(changes.was_ignored.contains("app.cfg"));
        repo.unstage_paths(["app.cfg"]).unwrap();

        // What is ignored holds while other files are committed.
        repo.stage_paths(["main.rs"]).unwrap();
        repo.commit("second", false).unwrap();
        assert_eq!(paths(&repo.working_changes().unwrap().ignored), vec!["app.cfg"]);

        // Forgotten, it is an unstaged change like any other.
        repo.stop_ignoring(["app.cfg"]).unwrap();
        let changes = repo.working_changes().unwrap();
        assert_eq!(paths(&changes.unstaged), vec!["app.cfg"]);
        assert!(changes.was_ignored.is_empty());
        assert!(!repo.ignored_file().exists(), "an empty list leaves no file");
    }

    #[test]
    fn a_change_ignored_as_it_was_shown_with_git() {
        let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
        write(&repo, "app.cfg", "url = prod\n");
        repo.stage_paths(["app.cfg"]).unwrap();
        repo.commit("first", false).unwrap();
        write(&repo, "app.cfg", "url = local\n");
        let shown = repo.working_changes().unwrap().unstaged[0].clone();
        // It changed after it was read, before it was ignored.
        write(&repo, "app.cfg", "url = local2\n");
        repo.ignore_change(&shown).unwrap();
        let changes = repo.working_changes().unwrap();
        assert_eq!(paths(&changes.unstaged), vec!["app.cfg"]);
        assert!(changes.was_ignored.contains("app.cfg"));

        // Ignored, then staged and changed further: another change.
        write(&repo, "app.cfg", "url = local\nnew = 1\n");
        let file = repo.working_changes().unwrap().unstaged[0].clone();
        repo.ignore_change(&file).unwrap();
        assert_eq!(paths(&repo.working_changes().unwrap().ignored), vec!["app.cfg"]);
        run(&repo, &["add", "app.cfg"]);
        write(&repo, "app.cfg", "url = local\nnew = 1\nlocal = 1\n");
        let changes = repo.working_changes().unwrap();
        assert!(changes.ignored.is_empty());
        assert!(changes.was_ignored.contains("app.cfg"));
    }

    #[test]
    fn untracked_files_and_conflicts_are_not_ignored() {
        let file = FileDiff {
            path: "new.txt".into(),
            old_path: None,
            status: FileStatus::Untracked,
            binary: false,
            large: false,
            mode: None,
            blobs: Some(("0000000".into(), "1111111".into())),
            hunks: Vec::new(),
            lossy: false,
        };
        let repo = Repo { worktree: "/w".into(), git_dir: "/nowhere".into(), common_dir: "/nowhere".into() };
        assert!(repo.ignore_change(&file).is_err());
        let file = FileDiff { status: FileStatus::Conflicted, ..file };
        assert!(repo.ignore_change(&file).is_err());
    }
}
