//! Tells the Files tab when the folders it shows change, so it reads them
//! again then rather than every few seconds.
//!
//! On Windows one watch covers the session's whole folder. A watch holds its
//! folder open, and Windows won't rename a folder while something inside it
//! is held open, so watching each folder shown would stop agents from
//! renaming the folders around it. Elsewhere each folder shown is watched on
//! its own: a watch of a whole tree there is one per folder in it, `target/`
//! and `node_modules/` included.
//!
//! Changes are gathered for a moment and passed on as the folders they
//! happened in.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

// `::notify` is the crate; `crate::notify` is the app's desktop notifications.
use ::notify::event::ModifyKind;
use ::notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// One watch of the session's folder covers everything shown in it.
pub const WHOLE_TREE: bool = cfg!(windows);
/// How long changes are gathered before they are passed on.
const GATHER: Duration = Duration::from_millis(150);

/// What changed in the folders watched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Folders whose entries may have changed.
    pub dirs: HashSet<PathBuf>,
    /// Folders whose `.gitignore` changed, which can dim or undim anything below them.
    pub rules: HashSet<PathBuf>,
    /// Changes were lost (the system's queue overflowed): everything shown may be out of date.
    pub all: bool,
}

impl Changes {
    /// Whether the listing of `dir` may be out of date.
    pub fn touches(&self, dir: &Path) -> bool {
        self.all || self.dirs.contains(dir) || self.rules.iter().any(|r| dir.starts_with(r))
    }

    fn is_empty(&self) -> bool {
        !self.all && self.dirs.is_empty() && self.rules.is_empty()
    }

    fn add(&mut self, event: ::notify::Result<::notify::Event>) {
        let Ok(event) = event else {
            self.all = true;
            return;
        };
        if event.need_rescan() {
            self.all = true;
        }
        // A file being read or written to leaves its folder's listing as it was.
        let lists = !matches!(
            event.kind,
            EventKind::Access(_) | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_) | ModifyKind::Any)
        );
        let rules = !matches!(event.kind, EventKind::Access(_));
        for path in event.paths {
            let Some(parent) = path.parent() else { continue };
            if rules && path.file_name() == Some(OsStr::new(".gitignore")) {
                self.rules.insert(parent.to_path_buf());
            }
            if lists {
                self.dirs.insert(parent.to_path_buf());
                // The path itself, when it is a folder that was removed or came back.
                self.dirs.insert(path);
            }
        }
    }
}

/// The watches of the folders shown.
pub struct FolderWatch {
    watcher: Option<RecommendedWatcher>,
    /// The folders are on a remote host: none is watched, all are read again
    /// now and then (see [`FolderWatch::poll_only`]).
    remote: bool,
    /// The folders asked for, watched or not.
    wanted: HashSet<PathBuf>,
    /// Those of them that could not be watched.
    failed: HashSet<PathBuf>,
    changes: (async_channel::Sender<Changes>, async_channel::Receiver<Changes>),
}

impl Default for FolderWatch {
    fn default() -> Self {
        FolderWatch {
            watcher: None,
            remote: false,
            wanted: HashSet::new(),
            failed: HashSet::new(),
            changes: async_channel::unbounded(),
        }
    }
}

impl std::fmt::Debug for FolderWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderWatch")
            .field("watching", &self.watcher.is_some())
            .field("wanted", &self.wanted.len())
            .field("failed", &self.failed.len())
            .finish()
    }
}

/// Hashable wrapper so the changes can back a subscription.
pub struct Subscribed(pub async_channel::Receiver<Changes>);

impl Hash for Subscribed {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "terminalrooster-folder-changes".hash(state);
    }
}

impl FolderWatch {
    /// Watches exactly the folders in `wanted` (each with what is inside it,
    /// if [`WHOLE_TREE`]). Nothing wanted lets the watcher go.
    pub fn set(&mut self, wanted: HashSet<PathBuf>) {
        if std::mem::take(&mut self.remote) {
            self.wanted.clear();
            self.failed.clear();
        }
        if wanted == self.wanted {
            return;
        }
        if wanted.is_empty() {
            self.watcher = None;
            self.wanted.clear();
            self.failed.clear();
            return;
        }
        if self.watcher.is_none() {
            match self.start() {
                Ok(w) => self.watcher = Some(w),
                Err(e) => tracing::warn!(error = ?e.kind, "could not watch folders"),
            }
        }
        for gone in self.wanted.difference(&wanted) {
            if !self.failed.remove(gone)
                && let Some(w) = &mut self.watcher
            {
                let _ = w.unwatch(gone);
            }
        }
        let mode = if WHOLE_TREE { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
        for new in wanted.difference(&self.wanted) {
            let watched = match &mut self.watcher {
                Some(w) => w.watch(new, mode).map_err(|e| tracing::debug!(error = ?e.kind, "could not watch a folder")),
                None => Err(()),
            };
            if watched.is_err() {
                self.failed.insert(new.clone());
            }
        }
        self.wanted = wanted;
    }

    /// Takes folders that can't be watched from here (those of a remote
    /// host): they are all read again now and then instead.
    pub fn poll_only(&mut self, wanted: HashSet<PathBuf>) {
        self.watcher = None;
        self.remote = true;
        self.failed = wanted.clone();
        self.wanted = wanted;
    }

    /// Some folder could not be watched: the folders shown have to be
    /// read now and then instead.
    pub fn needs_poll(&self) -> bool {
        !self.failed.is_empty()
    }

    /// The folders that could not be watched are tried again on the next [`set`](Self::set).
    pub fn retry(&mut self) {
        for dir in self.failed.drain() {
            self.wanted.remove(&dir);
        }
    }

    /// The changes, as they come.
    pub fn changes(&self) -> Subscribed {
        Subscribed(self.changes.1.clone())
    }

    /// A watcher whose events are gathered on a thread of its own. The
    /// thread ends when the watcher is dropped.
    fn start(&self) -> ::notify::Result<RecommendedWatcher> {
        let (tx, rx) = mpsc::channel();
        let watcher = ::notify::recommended_watcher(tx)?;
        let out = self.changes.0.clone();
        std::thread::Builder::new()
            .name("folder-watch".into())
            .spawn(move || gather(&rx, &out))
            .map_err(::notify::Error::io)?;
        Ok(watcher)
    }
}

/// Passes on the events from `rx` in batches: what comes within [`GATHER`]
/// of the first one goes with it.
fn gather(rx: &mpsc::Receiver<::notify::Result<::notify::Event>>, out: &async_channel::Sender<Changes>) {
    while let Ok(first) = rx.recv() {
        let mut changes = Changes::default();
        changes.add(first);
        let until = Instant::now() + GATHER;
        let mut open = true;
        // Checked first, so a flood of events can't hold the batch back.
        while Instant::now() < until {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(event) => changes.add(event),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    open = false;
                    break;
                }
            }
        }
        if !changes.is_empty() && out.send_blocking(changes).is_err() {
            return;
        }
        if !open {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use ::notify::event::{AccessKind, CreateKind, DataChange, Flag, RemoveKind, RenameMode};
    use ::notify::{Event, EventKind};

    use super::*;

    fn changes_of(events: Vec<Event>) -> Changes {
        let mut c = Changes::default();
        for e in events {
            c.add(Ok(e));
        }
        c
    }

    fn set(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn entries_coming_and_going_change_their_folder() {
        let c = changes_of(vec![
            Event::new(EventKind::Create(CreateKind::File)).add_path("/r/src/new.rs".into()),
            Event::new(EventKind::Remove(RemoveKind::Any)).add_path("/r/old".into()),
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path("/r/a".into())
                .add_path("/r/docs/a".into()),
        ]);
        assert_eq!(c.dirs, set(&["/r/src", "/r/src/new.rs", "/r", "/r/old", "/r/a", "/r/docs", "/r/docs/a"]));
        assert!(c.rules.is_empty() && !c.all);
        assert!(c.touches(Path::new("/r/docs")) && !c.touches(Path::new("/r/lib")));
    }

    #[test]
    fn writing_to_a_file_changes_no_listing() {
        let c = changes_of(vec![
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path("/r/log.txt".into()),
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path("/r/log.txt".into()),
            Event::new(EventKind::Access(AccessKind::Any)).add_path("/r/main.rs".into()),
        ]);
        assert!(c.is_empty(), "{c:?}");
    }

    #[test]
    fn a_changed_gitignore_touches_everything_below_it() {
        let c = changes_of(vec![
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path("/r/src/.gitignore".into()),
        ]);
        assert_eq!(c.rules, set(&["/r/src"]));
        assert!(c.dirs.is_empty());
        assert!(c.touches(Path::new("/r/src")) && c.touches(Path::new("/r/src/gen/x")));
        assert!(!c.touches(Path::new("/r")));
    }

    #[test]
    fn lost_events_touch_everything() {
        let c = changes_of(vec![Event::new(EventKind::Other).set_flag(Flag::Rescan)]);
        assert!(c.all && c.touches(Path::new("/anywhere")));
        let mut c = Changes::default();
        c.add(Err(::notify::Error::generic("gone wrong")));
        assert!(c.all);
    }

    /// Waits for changes that touch `dir`.
    fn wait_for(rx: &async_channel::Receiver<Changes>, dir: &Path) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            match rx.try_recv() {
                Ok(c) if c.touches(dir) => return true,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        false
    }

    #[test]
    fn a_new_file_in_a_watched_folder_is_heard_of() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("sub")).unwrap();
        let mut watch = FolderWatch::default();
        let mut wanted = HashSet::from([root.clone()]);
        if !WHOLE_TREE {
            wanted.insert(root.join("sub"));
        }
        watch.set(wanted);
        assert!(!watch.needs_poll());
        let rx = watch.changes().0;

        std::fs::write(root.join("a.txt"), "").unwrap();
        assert!(wait_for(&rx, &root), "a file in the folder");
        std::fs::write(root.join("sub/b.txt"), "").unwrap();
        assert!(wait_for(&rx, &root.join("sub")), "a file in a folder shown inside it");

        // Letting go of the folders ends the watch (and its thread).
        watch.set(HashSet::new());
        std::fs::write(root.join("c.txt"), "").unwrap();
        std::thread::sleep(GATHER * 3);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_folder_that_cannot_be_watched_is_polled_until_retried() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let mut watch = FolderWatch::default();
        watch.set(HashSet::from([gone.clone()]));
        assert!(watch.needs_poll());
        // Not tried again on every look...
        std::fs::create_dir(&gone).unwrap();
        watch.set(HashSet::from([gone.clone()]));
        assert!(watch.needs_poll());
        // ...only when asked to.
        watch.retry();
        watch.set(HashSet::from([gone]));
        assert!(!watch.needs_poll());
    }
}
