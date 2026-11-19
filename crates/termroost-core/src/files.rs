//! Reading and writing a project's files, on whichever computer they are
//! (see [`crate::Machine`]): the Files tab's listings, the editor's reads and
//! saves. These run on this computer; a remote project's run on its host,
//! in Terminal Rooster's helper there.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// One file or folder in a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    /// Git's ignore rules leave it out.
    pub ignored: bool,
}

/// What a file is like on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub is_file: bool,
}

impl FileMeta {
    fn of(meta: &std::fs::Metadata) -> FileMeta {
        FileMeta { len: meta.len(), modified: meta.modified().ok(), is_file: meta.is_file() }
    }
}

/// Why a file could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsError {
    NotFound,
    /// It is a folder, or something else that isn't a file.
    NotAFile,
    /// Larger than the most that was asked for.
    TooLarge,
    /// Anything else, in words (permissions, a drive or a connection gone).
    Other(String),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::NotFound => f.write_str("it does not exist"),
            FsError::NotAFile => f.write_str("it is not a file"),
            FsError::TooLarge => f.write_str("it is too large"),
            FsError::Other(e) => f.write_str(e),
        }
    }
}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => FsError::NotFound,
            _ => FsError::Other(e.to_string()),
        }
    }
}

/// The files and folders in `dir`: folders first, then by name ignoring case.
/// `.git` (a folder, or a file in a linked worktree) is left out.
pub fn list_dir(dir: &Path) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    // What a walk that follows git's rules (as git does: only inside a
    // repository) still lists; the rest is ignored. Hidden files count too.
    let kept: HashSet<PathBuf> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .ignore(false)
        .parents(true)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.depth() == 1)
        .map(ignore::DirEntry::into_path)
        .collect();
    let mut entries: Vec<Entry> = read
        .filter_map(Result::ok)
        .filter(|e| e.file_name() != ".git")
        .map(|e| {
            let path = e.path();
            // The listing knows the type for free; only links need a look at their target.
            let is_dir = match e.file_type() {
                Ok(t) if t.is_symlink() => path.is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => false,
            };
            let ignored = !kept.contains(&path);
            Entry { name: e.file_name().to_string_lossy().into_owned(), path, is_dir, ignored }
        })
        .collect();
    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase(), e.name.clone()));
    Ok(entries)
}

/// Where a file named `name` goes in `dir`. The name may go through folders
/// (`docs/notes.md`), but not out of `dir`.
pub fn new_file_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    let plain = Path::new(name).components().all(|c| matches!(c, Component::Normal(_)));
    if name.is_empty() || !plain || name.ends_with(std::path::is_separator) {
        return Err("Not a file name".into());
    }
    Ok(dir.join(name))
}

/// Creates an empty file at `path`, and the folders it goes in. A file
/// already there is left alone.
pub fn create_file(path: &Path) -> Result<(), String> {
    let exists = || format!("{} already exists", path.file_name().unwrap_or_default().to_string_lossy());
    // Checked first: on Windows, a folder in the way is "access denied".
    if path.symlink_metadata().is_ok() {
        return Err(exists());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(exists()),
        Err(e) => Err(e.to_string()),
    }
}

/// What the file at `path` is like.
pub fn metadata(path: &Path) -> Result<FileMeta, FsError> {
    Ok(FileMeta::of(&std::fs::metadata(path)?))
}

/// The file at `path`, if it is one of at most `max` bytes, with what it was
/// like just before it was read (so a change in between shows up later).
pub fn read_file(path: &Path, max: u64) -> Result<(FileMeta, Vec<u8>), FsError> {
    let meta = metadata(path)?;
    if !meta.is_file {
        return Err(FsError::NotAFile);
    }
    if meta.len > max {
        return Err(FsError::TooLarge);
    }
    Ok((meta, std::fs::read(path)?))
}

/// Writes `bytes` to `path` and says what the file is like now.
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<FileMeta, FsError> {
    std::fs::write(path, bytes)?;
    metadata(path)
}

/// The folder `path` names, made absolute: `~` and a path starting with
/// `~/` are in the home folder, and so is a relative path. `None` when it
/// is not a folder.
pub fn resolve_dir(path: &Path) -> Result<PathBuf, String> {
    let home = || directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()).unwrap_or_default();
    let text = path.to_string_lossy();
    let full = if text.is_empty() || text == "~" {
        home()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home().join(rest)
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        home().join(path)
    };
    match std::fs::metadata(&full) {
        Ok(m) if m.is_dir() => Ok(normalize(&full)),
        Ok(_) => Err(format!("{} is not a folder", full.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("{} does not exist", full.display())),
        Err(e) => Err(format!("can't open {}: {e}", full.display())),
    }
}

/// `path` without `.` and `..` parts (without following links, unlike
/// `canonicalize`, so a folder keeps the name it was given).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The folders whose path starts with `partial` (`/srv/a` gives `/srv/app/`
/// and `/srv/assets/`), for completing a folder as it is typed. At most
/// `limit` of them, sorted.
pub fn complete_dir(partial: &str, limit: usize) -> Vec<String> {
    let (dir, prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => return Vec::new(),
    };
    let Ok(entries) = std::fs::read_dir(if dir.is_empty() { "/" } else { dir }) else { return Vec::new() };
    let hidden = prefix.starts_with('.');
    let mut found: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.starts_with(prefix) && (hidden || !name.starts_with('.')))
        .map(|name| format!("{dir}{name}/"))
        .collect();
    found.sort();
    found.truncate(limit);
    found
}

/// Which of `paths` are folders.
pub fn are_dirs(paths: &[PathBuf]) -> Vec<bool> {
    paths.iter().map(|p| p.is_dir()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn folders_come_first_and_git_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["b.txt", "A.txt"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        for d in ["zeta", ".git"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        assert_eq!(names(&list_dir(dir.path()).unwrap()), ["zeta", "A.txt", "b.txt"]);
    }

    #[test]
    fn reading_says_why_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        assert_eq!(read_file(&file, 10), Err(FsError::NotFound));
        std::fs::write(&file, "hello").unwrap();
        assert_eq!(read_file(&file, 4), Err(FsError::TooLarge));
        let (meta, bytes) = read_file(&file, 10).unwrap();
        assert_eq!((meta.len, bytes.as_slice()), (5, &b"hello"[..]));
        assert_eq!(read_file(dir.path(), 10), Err(FsError::NotAFile));
        let written = write_file(&file, b"hi").unwrap();
        assert_eq!(written.len, 2);
    }

    #[test]
    fn folders_resolve_from_home_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app/src")).unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("apple.txt"), "").unwrap();
        let app = dir.path().join("app");
        assert_eq!(resolve_dir(&app.join("src/..")).unwrap(), app);
        assert!(resolve_dir(&dir.path().join("apple.txt")).unwrap_err().contains("not a folder"));
        assert!(resolve_dir(&dir.path().join("nope")).unwrap_err().contains("does not exist"));
        let base = format!("{}/", dir.path().display());
        assert_eq!(complete_dir(&format!("{base}a"), 10), [format!("{base}app/"), format!("{base}assets/")]);
        assert_eq!(complete_dir(&format!("{base}."), 10), [format!("{base}.hidden/")]);
        assert_eq!(complete_dir(&format!("{base}a"), 1).len(), 1);
    }
}
