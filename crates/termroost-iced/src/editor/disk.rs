//! Open files and the disk: what a file was like when it was read or
//! written, and whether it changed since (agents edit the same files). The
//! disk is that of the computer the session's folder is on (see [`Machine`]).

use std::hash::{DefaultHasher, Hasher};
use std::path::Path;
use std::time::{Duration, SystemTime};

use termroost_core::Machine;
use termroost_core::files::{FileMeta, FsError};

use super::text::{self, Decoded};

/// A file written this soon before it was looked at may be written again
/// within the same tick of its modification time, which then doesn't
/// change (the "racy git" problem): its content is checked every time.
const RACY: Duration = Duration::from_secs(2);
/// The most of a file read to compare it with the editor's copy: a file
/// grown past it (a log, say) counts as changed without a look.
const CHECK_MAX: u64 = 4 * text::MAX_SIZE;

/// What a file on disk was like when it was read or written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    hash: u64,
    /// The modification time can't be trusted to show the next change.
    racy: bool,
}

impl Stamp {
    fn new(meta: &FileMeta, bytes: &[u8]) -> Stamp {
        let mut h = DefaultHasher::new();
        h.write(bytes);
        let modified = meta.modified;
        // A time ahead of this computer's clock (another computer's) can't be trusted either.
        let racy = modified.is_none_or(|m| SystemTime::now().duration_since(m).map_or(true, |age| age < RACY));
        Stamp { modified, len: bytes.len() as u64, hash: h.finish(), racy }
    }

    /// The metadata says nothing changed, so the content needn't be read.
    fn unchanged(&self, meta: &FileMeta) -> bool {
        !self.racy && self.len == meta.len && self.modified == meta.modified
    }
}

/// A file read for the editor.
#[derive(Clone, Debug)]
pub struct FileText {
    pub decoded: Decoded,
    pub stamp: Stamp,
}

/// Reads `path` as text, or says why it can't be edited.
pub fn read(machine: &Machine, path: &Path) -> Result<FileText, String> {
    // The metadata is from before reading: a change in between then shows up
    // as a change later.
    let (meta, bytes) = machine.read_file(path, text::MAX_SIZE).map_err(|e| match e {
        FsError::TooLarge => text::too_large(),
        e => e.to_string(),
    })?;
    let stamp = Stamp::new(&meta, &bytes);
    Ok(FileText { decoded: text::decode(bytes)?, stamp })
}

/// How a file on disk compares to what was read or written last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnDisk {
    /// The same content (maybe with a new modification time).
    Same(Stamp),
    Changed(Stamp),
    Deleted,
    /// It can't be looked at now (permissions, a drive gone): try again later.
    Unreadable,
}

/// Compares the file at `path` with `known`. The content is only read when
/// the size or modification time changed, and a file saved again unchanged
/// still counts as the same.
pub fn check(machine: &Machine, path: &Path, known: &Stamp) -> OnDisk {
    let meta = match machine.metadata(path) {
        Ok(m) if m.is_file => m,
        Ok(_) | Err(FsError::NotFound) => return OnDisk::Deleted,
        Err(_) => return OnDisk::Unreadable,
    };
    if known.unchanged(&meta) {
        return OnDisk::Same(*known);
    }
    let bytes = match machine.read_file(path, CHECK_MAX) {
        Ok((_, bytes)) => bytes,
        Err(FsError::TooLarge) => {
            return OnDisk::Changed(Stamp { modified: meta.modified, len: meta.len, hash: 0, racy: false });
        }
        Err(_) => return OnDisk::Unreadable,
    };
    let now = Stamp::new(&meta, &bytes);
    if now.len == known.len && now.hash == known.hash { OnDisk::Same(now) } else { OnDisk::Changed(now) }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The file changed on disk since it was read or saved.
    Conflict,
    Io(String),
}

/// Writes `bytes` to `path`, unless the file changed since `known` and
/// `force` isn't set. A deleted file is written again.
pub fn write(
    machine: &Machine,
    path: &Path,
    bytes: &[u8],
    known: Option<&Stamp>,
    force: bool,
) -> Result<Stamp, SaveError> {
    if !force
        && let Some(known) = known
        && matches!(check(machine, path, known), OnDisk::Changed(_))
    {
        return Err(SaveError::Conflict);
    }
    let meta = machine.write_file(path, bytes).map_err(|e| SaveError::Io(e.to_string()))?;
    Ok(Stamp::new(&meta, bytes))
}

/// What to do about an open file after looking at the disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reaction {
    /// Nothing changed; remember the new stamp.
    Keep(Stamp),
    /// It changed and has no unsaved changes here: show the new text.
    Reload,
    /// It changed and has unsaved changes here: ask which to keep.
    Ask(Stamp),
    Deleted,
    Nothing,
}

pub fn react(modified: bool, on_disk: OnDisk) -> Reaction {
    match on_disk {
        OnDisk::Same(stamp) => Reaction::Keep(stamp),
        OnDisk::Changed(_) if !modified => Reaction::Reload,
        OnDisk::Changed(stamp) => Reaction::Ask(stamp),
        OnDisk::Deleted => Reaction::Deleted,
        OnDisk::Unreadable => Reaction::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_checks_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&Machine::Local, dir.path()).unwrap_err().contains("not a file"));
        assert!(read(&Machine::Local, &dir.path().join("gone")).is_err());
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hi\n").unwrap();
        assert_eq!(read(&Machine::Local, &file).unwrap().decoded.text, "hi\n");
        std::fs::write(&file, b"\0bin").unwrap();
        assert!(read(&Machine::Local, &file).unwrap_err().contains("binary"));
    }

    #[test]
    fn changes_on_disk_are_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let read_at = read(&Machine::Local, &file).unwrap().stamp;
        assert_eq!(check(&Machine::Local, &file, &read_at), OnDisk::Same(read_at));

        // Saved again unchanged, later: the same, with the new time remembered.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "one\n").unwrap();
        let OnDisk::Same(touched) = check(&Machine::Local, &file, &read_at) else { panic!("rewritten unchanged") };
        assert_eq!(check(&Machine::Local, &file, &touched), OnDisk::Same(touched));

        std::fs::write(&file, "two\n").unwrap();
        assert!(matches!(check(&Machine::Local, &file, &touched), OnDisk::Changed(_)));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(check(&Machine::Local, &file, &touched), OnDisk::Deleted);
    }

    #[test]
    fn only_files_written_just_before_are_read_every_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        assert!(read(&Machine::Local, &file).unwrap().stamp.racy, "just written");

        let old = SystemTime::now() - Duration::from_secs(3600);
        let set_time = |t| std::fs::File::options().write(true).open(&file).unwrap().set_modified(t).unwrap();
        set_time(old);
        let stamp = read(&Machine::Local, &file).unwrap().stamp;
        assert!(!stamp.racy);
        // Same size and time as before: taken as unchanged without reading it.
        std::fs::write(&file, "ONE\n").unwrap();
        set_time(old);
        assert_eq!(check(&Machine::Local, &file, &stamp), OnDisk::Same(stamp));
        // Any newer time is a reason to look.
        set_time(old + Duration::from_secs(1));
        assert!(matches!(check(&Machine::Local, &file, &stamp), OnDisk::Changed(_)));
    }

    #[test]
    fn saving_over_a_change_on_disk_asks_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let known = read(&Machine::Local, &file).unwrap().stamp;
        let saved = write(&Machine::Local, &file, b"mine\n", Some(&known), false).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"mine\n");
        assert_eq!(check(&Machine::Local, &file, &saved), OnDisk::Same(saved), "what was written is what is known");

        std::fs::write(&file, "theirs\n").unwrap();
        assert_eq!(write(&Machine::Local, &file, b"mine 2\n", Some(&saved), false), Err(SaveError::Conflict));
        assert_eq!(std::fs::read(&file).unwrap(), b"theirs\n", "not overwritten");
        write(&Machine::Local, &file, b"mine 2\n", Some(&saved), true).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"mine 2\n", "forced");

        std::fs::remove_file(&file).unwrap();
        write(&Machine::Local, &file, b"back\n", Some(&saved), false).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"back\n", "a deleted file is written again");
    }

    #[test]
    fn only_changed_files_with_changes_here_ask() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let s = read(&Machine::Local, &file).unwrap().stamp;
        assert_eq!(react(true, OnDisk::Same(s)), Reaction::Keep(s));
        assert_eq!(react(false, OnDisk::Changed(s)), Reaction::Reload);
        assert_eq!(react(true, OnDisk::Changed(s)), Reaction::Ask(s));
        assert_eq!(react(false, OnDisk::Deleted), Reaction::Deleted);
        assert_eq!(react(true, OnDisk::Unreadable), Reaction::Nothing);
    }
}
