//! What changed, file by file and hunk by hunk: in a commit, or in a working
//! tree (staged, not staged yet, and new files git doesn't track yet). And
//! the other way round: staging and unstaging whole files, single hunks or
//! chosen lines of a hunk, and committing what is staged.
//!
//! Hunks are staged by handing `git apply --cached` a patch of just that
//! hunk. A hunk of the unstaged changes compares the index with the working
//! tree, so its old side is the index exactly as it is; a staged hunk
//! compares HEAD with the index, so its new side is, and it is unstaged by
//! applying it in reverse. Either way the patch fits the index as it stands,
//! whatever happens to the other hunks.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use super::ignored::set_aside;
use super::{Repo, command, git, let_ask, spawn_error};

/// How a file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    /// Not tracked by git yet (and not ignored).
    Untracked,
    /// Both sides of a merge (or rebase, or cherry-pick) changed it, and
    /// git left the conflict to be resolved.
    Conflicted,
}

impl FileStatus {
    /// One letter for it, as git and most tools show it.
    pub fn letter(self) -> char {
        match self {
            FileStatus::Modified => 'M',
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
            FileStatus::Untracked => 'U',
            FileStatus::Conflicted => '!',
        }
    }
}

/// One file's changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// Where the file is after the change (before it, when deleted),
    /// relative to the top of the working tree, with `/` between folders.
    pub path: String,
    /// Where it was before, when it was renamed or copied.
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// git found it binary: there are no lines to show.
    pub binary: bool,
    /// Too big to show (only new files are measured; git shows the rest).
    pub large: bool,
    /// Its mode changed, such as made executable: (old, new).
    pub mode: Option<(String, String)>,
    /// The ids of its blobs before and after, whole (git's `index` line).
    /// None for a conflict, a change of mode alone, or an untracked file.
    pub blobs: Option<(String, String)>,
    pub hunks: Vec<Hunk>,
    /// Its text is not UTF-8: it shows with replacement characters, and its
    /// hunks can't be staged on their own (the patch would not fit).
    pub lossy: bool,
}

impl FileDiff {
    fn new(path: String, status: FileStatus) -> FileDiff {
        FileDiff {
            path,
            old_path: None,
            status,
            binary: false,
            large: false,
            mode: None,
            blobs: None,
            hunks: Vec::new(),
            lossy: false,
        }
    }

    /// What the change is, exactly: both sides' blobs, and the modes when
    /// they changed. Changing the file again, or what it is compared with,
    /// makes another. `None` when git gave neither (an untracked file).
    pub fn fingerprint(&self) -> Option<String> {
        let blobs = self.blobs.as_ref().map(|(old, new)| format!("{old}..{new}"));
        let mode = self.mode.as_ref().map(|(old, new)| format!("{old}->{new}"));
        let parts: Vec<String> = [blobs, mode].into_iter().flatten().collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    /// Lines added and removed, over all hunks.
    pub fn counts(&self) -> (usize, usize) {
        let lines = self.hunks.iter().flat_map(|h| &h.lines);
        lines.fold((0, 0), |(added, removed), l| match l.kind {
            LineKind::Added => (added + 1, removed),
            LineKind::Removed => (added, removed + 1),
            LineKind::Context => (added, removed),
        })
    }

    /// Whether its hunks (and their lines) can be staged or unstaged one by
    /// one: a change to the text of a file that is there before and after.
    /// Other changes (new, deleted, binary files, submodules) go whole.
    pub fn splittable(&self) -> bool {
        matches!(self.status, FileStatus::Modified | FileStatus::Renamed | FileStatus::Copied)
            && !self.binary
            && !self.lossy
            && !self.hunks.is_empty()
            && !self.is_submodule()
    }

    /// Whether it is a submodule: its "text" is the commit it is at.
    pub fn is_submodule(&self) -> bool {
        let mut lines = self.hunks.iter().flat_map(|h| &h.lines).peekable();
        lines.peek().is_some() && lines.all(|l| l.text.starts_with("Subproject commit "))
    }

    /// Every path the change touches: both ends of a rename.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.old_path.as_deref().into_iter().chain([self.path.as_str()])
    }
}

/// A run of changed lines and the unchanged lines around them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// What git shows after the line numbers: usually the function the hunk
    /// is in.
    pub context: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// The `@@ -1,4 +1,5 @@` line git starts it with.
    pub fn header(&self) -> String {
        format!("@@ -{},{} +{},{} @@", self.old_start, self.old_lines, self.new_start, self.new_lines)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// A line of a hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// The line without its `+`, `-` or space and its line break (a `\r`
    /// before the break is kept).
    pub text: String,
    /// Its number in the old file, when it is in the old file.
    pub old: Option<u32>,
    /// Its number in the new file, when it is in the new file.
    pub new: Option<u32>,
    /// It is the last line of its file, which doesn't end in a line break.
    pub no_newline: bool,
}

/// Someone and when, as a commit records them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Seconds since the epoch.
    pub time: i64,
}

/// A commit and what it changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDetails {
    pub id: String,
    /// The commits it was made on: none for the first commit, two or more
    /// for a merge. The changes shown are against the first.
    pub parents: Vec<String>,
    pub author: Signature,
    pub committer: Signature,
    /// The whole message: subject, a blank line, and the body.
    pub message: String,
    pub files: Vec<FileDiff>,
}

/// What is not committed yet in a working tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingChanges {
    /// Staged: the index against HEAD.
    pub staged: Vec<FileDiff>,
    /// Not staged: the working tree against the index, conflicts included,
    /// ignored changes not.
    pub unstaged: Vec<FileDiff>,
    /// Files git doesn't track yet; the first few have their text read.
    pub untracked: Vec<FileDiff>,
    /// Changes not staged that the user ignored, exactly as they are (see
    /// [`Repo::ignore_change`]).
    pub ignored: Vec<FileDiff>,
    /// Paths of staged or unstaged files whose change was ignored, and has
    /// changed since (or was staged): no longer ignored.
    pub was_ignored: BTreeSet<String>,
    /// HEAD has no commit yet (a new repository): a commit would be the first.
    pub unborn: bool,
}

/// New files whose text is read; the rest show without it. (The changes
/// are read again every few seconds while they are on screen.)
const UNTRACKED_READ: usize = 50;
/// New files are listed up to this many.
const UNTRACKED_MAX: usize = 2000;
/// A new file bigger than this isn't shown.
const UNTRACKED_MAX_BYTES: u64 = 256 << 10;

/// Options that make git's diffs look the same whatever the user set up:
/// paths as they are, `a/` and `b/` in front of them, three lines around
/// every change, no external programs, and whole blob ids (abbreviated ones
/// grow with the repository, and would change a change's fingerprint).
const DIFF_ARGS: [&str; 8] = [
    "--no-color",
    "--no-ext-diff",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "-U3",
    "--no-textconv",
    "--full-index",
];

impl Repo {
    /// The commit `id` (anything `git show` takes), its message and what it
    /// changed. A merge shows what it changed against its first parent.
    pub fn commit_details(&self, id: &str) -> Result<CommitDetails, String> {
        let format = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%B%x00";
        let mut args = vec!["-c", "core.quotePath=false", "-c", "log.showSignature=false", "show"];
        args.extend(DIFF_ARGS);
        args.extend(["-M", "--diff-merges=first-parent", format, "--end-of-options", id]);
        let out = git_bytes(&self.worktree, &args)?;
        let split = out.iter().position(|b| *b == 0).ok_or_else(|| format!("git show {id} said nothing"))?;
        let head = String::from_utf8_lossy(&out[..split]);
        let mut f = head.split('\u{1f}');
        let mut next = || f.next().unwrap_or_default().to_owned();
        let id = next();
        let parents = next().split_whitespace().map(str::to_owned).collect();
        let mut person = || Signature { name: next(), email: next(), time: next().parse().unwrap_or(0) };
        let (author, committer) = (person(), person());
        let message = next().trim_end().to_owned();
        Ok(CommitDetails { id, parents, author, committer, message, files: parse_diff(&out[split + 1..]) })
    }

    /// What is not committed yet: staged, not staged, and untracked.
    pub fn working_changes(&self) -> Result<WorkingChanges, String> {
        let diff = |extra: &[&str]| {
            let mut args = vec!["-c", "core.quotePath=false", "diff"];
            args.extend(DIFF_ARGS);
            args.extend(extra);
            git_bytes(&self.worktree, &args).map(|out| parse_diff(&out))
        };
        // Renames show among the staged changes, where `git mv` puts them;
        // the working tree against the index has none to find.
        let staged = diff(&["--cached", "-M"])?;
        let unstaged = diff(&["--no-renames"])?;
        let listed = git_bytes(&self.worktree, &["ls-files", "-z", "--others", "--exclude-standard"])?;
        let untracked = String::from_utf8_lossy(&listed)
            .split('\0')
            .filter(|p| !p.is_empty())
            .take(UNTRACKED_MAX)
            .enumerate()
            .map(|(i, path)| untracked_file(&self.worktree, path, i < UNTRACKED_READ))
            .collect();
        let unborn = self.head_commit_id().is_none();
        let mut changes = WorkingChanges { staged, unstaged, untracked, unborn, ..WorkingChanges::default() };
        set_aside(&mut changes, &self.ignored()?);
        Ok(changes)
    }

    /// Stages these files whole, as they are in the working tree: changed,
    /// new, deleted, or with a conflict resolved.
    pub fn stage_paths<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
        with_pathspecs(&self.worktree, &["add", "-A"], paths)
    }

    /// Takes these files out of what is staged, leaving the working tree as
    /// it is. (Both paths of a rename, to unstage it.)
    pub fn unstage_paths<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
        if self.head_commit_id().is_none() {
            // Nothing to go back to: what is staged is simply dropped.
            return with_pathspecs(&self.worktree, &["rm", "--cached", "-r", "-q", "--ignore-unmatch"], paths);
        }
        with_pathspecs(&self.worktree, &["reset", "-q"], paths)
    }

    /// Stages one hunk of `file`, a file of the unstaged changes; with
    /// `lines`, only those lines of it (indexes into the hunk's lines).
    pub fn stage_hunk(&self, file: &FileDiff, hunk: usize, lines: Option<&BTreeSet<usize>>) -> Result<(), String> {
        self.apply_hunk(file, hunk, lines, false)
    }

    /// Unstages one hunk of `file`, a file of the staged changes; with
    /// `lines`, only those lines of it.
    pub fn unstage_hunk(&self, file: &FileDiff, hunk: usize, lines: Option<&BTreeSet<usize>>) -> Result<(), String> {
        self.apply_hunk(file, hunk, lines, true)
    }

    fn apply_hunk(
        &self,
        file: &FileDiff,
        hunk: usize,
        lines: Option<&BTreeSet<usize>>,
        reverse: bool,
    ) -> Result<(), String> {
        if !file.splittable() {
            return Err(format!("{} can only be staged or unstaged whole.", file.path));
        }
        let hunk = file.hunks.get(hunk).ok_or("That hunk is gone; the changes were read again.")?;
        let patch = hunk_patch(&file.path, hunk, lines, reverse).ok_or("No changed lines are chosen.")?;
        let mut args = vec!["apply", "--cached", "--recount", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        git_with_input(&self.worktree, &args, patch.as_bytes()).map(drop)
    }

    /// Commits what is staged with `message`; `amend` replaces the last
    /// commit instead (with what is staged added to it). Returns what
    /// happened, in a sentence.
    pub fn commit(&self, message: &str, amend: bool) -> Result<String, String> {
        if message.trim().is_empty() {
            return Err("Write a message for the commit first.".into());
        }
        let mut args = vec!["commit", "--file=-"];
        if amend {
            args.push("--amend");
        }
        // Signing the commit may need a key's passphrase.
        run_with_input(let_ask(command(&self.worktree, &args)), &args, message.as_bytes())?;
        let made = git(&self.worktree, &["log", "-1", "--format=%h"]).unwrap_or_default();
        let verb = if amend { "Amended" } else { "Committed" };
        Ok(format!("{verb} {}.", made.trim()))
    }

    /// The whole message of the last commit, for amending it.
    pub fn last_message(&self) -> Result<String, String> {
        git(&self.worktree, &["log", "-1", "--format=%B"]).map(|m| m.trim_end().to_owned())
    }

    /// The commit HEAD points at, asking git (which knows every way refs are
    /// kept). `None` before the first commit.
    fn head_commit_id(&self) -> Option<String> {
        git(&self.worktree, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]).ok().map(|s| s.trim().to_owned())
    }
}

/// An untracked file as a diff: all of its lines added (when `read`, and it
/// is text and not too big).
fn untracked_file(worktree: &Path, path: &str, read: bool) -> FileDiff {
    let mut file = FileDiff::new(path.to_owned(), FileStatus::Untracked);
    if !read {
        return file;
    }
    let full = worktree.join(path);
    match std::fs::metadata(&full) {
        Ok(m) if m.len() > UNTRACKED_MAX_BYTES => file.large = true,
        Ok(m) if m.is_file() => {
            let Ok(bytes) = std::fs::read(&full) else { return file };
            // As git decides: a NUL near the start makes it binary.
            if bytes[..bytes.len().min(8000)].contains(&0) {
                file.binary = true;
                return file;
            }
            let (text, lossy) = match String::from_utf8(bytes) {
                Ok(s) => (s, false),
                Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
            };
            file.lossy = lossy;
            if text.is_empty() {
                return file;
            }
            let no_newline = !text.ends_with('\n');
            let body = text.strip_suffix('\n').unwrap_or(&text);
            let mut lines: Vec<DiffLine> = body
                .split('\n')
                .zip(1..)
                .map(|(l, n)| DiffLine {
                    kind: LineKind::Added,
                    text: l.to_owned(),
                    old: None,
                    new: Some(n),
                    no_newline: false,
                })
                .collect();
            if let Some(last) = lines.last_mut() {
                last.no_newline = no_newline;
            }
            let count = lines.len() as u32;
            file.hunks.push(Hunk {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: count,
                context: String::new(),
                lines,
            });
        }
        // A link to a folder, or gone already.
        _ => {}
    }
    file
}

/// A patch of one hunk of `path`, or of the lines of it in `chosen`. The
/// lines left out stay as they are in the index: for a patch applied
/// forward (staging) that is the old side, so a removed line left out
/// becomes context and an added one is dropped; for one applied in
/// `reverse` (unstaging) it is the new side, the other way round. `None`
/// when no changed line is left in.
pub(crate) fn hunk_patch(path: &str, hunk: &Hunk, chosen: Option<&BTreeSet<usize>>, reverse: bool) -> Option<String> {
    let mut body = String::new();
    let (mut old_lines, mut new_lines, mut changes) = (0u32, 0u32, 0usize);
    for (i, line) in hunk.lines.iter().enumerate() {
        let picked = chosen.is_none_or(|c| c.contains(&i));
        let prefix = match (line.kind, picked) {
            (LineKind::Context, _) => ' ',
            (LineKind::Added, true) => '+',
            (LineKind::Removed, true) => '-',
            // In the index already: stays, as context.
            (LineKind::Added, false) if reverse => ' ',
            (LineKind::Removed, false) if !reverse => ' ',
            // Not in the index: stays out.
            (LineKind::Added | LineKind::Removed, false) => continue,
        };
        match prefix {
            ' ' => {
                old_lines += 1;
                new_lines += 1;
            }
            '+' => {
                new_lines += 1;
                changes += 1;
            }
            _ => {
                old_lines += 1;
                changes += 1;
            }
        }
        body.push(prefix);
        body.push_str(&line.text);
        body.push('\n');
        if line.no_newline {
            body.push_str("\\ No newline at end of file\n");
        }
    }
    if changes == 0 {
        return None;
    }
    // A side with no lines names the line before where they would be.
    let start = |start: u32, lines: u32, now: u32| {
        let first = if lines == 0 { start + 1 } else { start };
        if now == 0 { first.saturating_sub(1) } else { first }
    };
    let old_start = start(hunk.old_start, hunk.old_lines, old_lines);
    let new_start = start(hunk.new_start, hunk.new_lines, new_lines);
    let name = |prefix: &str| quote_path(&format!("{prefix}{path}"));
    let (a, b) = (name("a/"), name("b/"));
    // git ends a name with a space in it with a tab, so the end is clear.
    let tab = if path.contains(' ') { "\t" } else { "" };
    Some(format!(
        "diff --git {a} {b}\n--- {a}{tab}\n+++ {b}{tab}\n@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n{body}"
    ))
}

/// A path as git writes it in a patch: in quotes, with C escapes, when it
/// has characters that would be ambiguous otherwise.
fn quote_path(p: &str) -> String {
    if !p.chars().any(|c| matches!(c, '"' | '\\') || c.is_control()) {
        return p.to_owned();
    }
    let mut out = String::from("\"");
    for c in p.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let mut buf = [0; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("\\{b:03o}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A path git put in quotes (`"a\tb"`), as it is; anything else as it is.
fn unquote_path(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else { return s.to_owned() };
    let mut bytes = Vec::new();
    let mut chars = inner.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(7),
            Some(b'b') => bytes.push(8),
            Some(b'f') => bytes.push(12),
            Some(b'v') => bytes.push(11),
            Some(d @ b'0'..=b'7') => {
                let mut n = u32::from(d - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(d @ b'0'..=b'7') => {
                            n = n * 8 + u32::from(d - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(n as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The path on a `---` or `+++` line: `None` for `/dev/null`.
fn side_path(s: &str, prefix: &str) -> Option<String> {
    // git ends names with spaces in them with a tab.
    let s = s.strip_suffix('\t').unwrap_or(s);
    if s == "/dev/null" {
        return None;
    }
    let s = unquote_path(s);
    Some(s.strip_prefix(prefix).map(str::to_owned).unwrap_or(s))
}

/// The path of a `diff --git a/<path> b/<path>` line, when both are the
/// same (the other lines name renamed files).
fn git_line_path(rest: &str) -> Option<String> {
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find("\" ").map(|i| i + 2)?;
        let a = unquote_path(&rest[..end]);
        return a.strip_prefix("a/").map(str::to_owned);
    }
    let half = rest.len().checked_sub(1)? / 2;
    let (a, b) = (rest.get(..half)?, rest.get(half + 1..)?);
    let (a, b) = (a.strip_prefix("a/")?, b.strip_prefix("b/")?);
    (a == b).then(|| a.to_owned())
}

/// Splits git's diff output into files and hunks.
pub(crate) fn parse_diff(out: &[u8]) -> Vec<FileDiff> {
    let mut starts: Vec<usize> = Vec::new();
    let mut at = 0;
    for line in out.split_inclusive(|b| *b == b'\n') {
        if line.starts_with(b"diff --git ") || line.starts_with(b"diff --cc ") || line.starts_with(b"diff --combined ")
        {
            starts.push(at);
        }
        at += line.len();
    }
    starts.push(out.len());
    starts.windows(2).filter_map(|w| parse_file(&out[w[0]..w[1]])).collect()
}

/// One file of a diff, from its `diff` line to the next.
fn parse_file(chunk: &[u8]) -> Option<FileDiff> {
    let (text, lossy) = match std::str::from_utf8(chunk) {
        Ok(s) => (std::borrow::Cow::Borrowed(s), false),
        Err(_) => (String::from_utf8_lossy(chunk), true),
    };
    let mut lines = text.split('\n');
    let first = lines.next()?;
    let mut file = FileDiff::new(String::new(), FileStatus::Modified);
    file.lossy = lossy;
    if let Some(path) = first.strip_prefix("diff --cc ").or_else(|| first.strip_prefix("diff --combined ")) {
        file.path = unquote_path(path);
        file.status = FileStatus::Conflicted;
        return Some(file);
    }
    let guess = first.strip_prefix("diff --git ").and_then(git_line_path);
    let (mut old_path, mut new_path) = (None, None);
    let (mut minus, mut plus) = (None, None);
    let mut old_mode = None;
    let mut in_hunks = false;
    // The next line numbers, and how many lines of each side are still to come.
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let (mut old_left, mut new_left) = (0u32, 0u32);
    for line in lines {
        if let Some(rest) = line.strip_prefix("@@ ") {
            let Some(hunk) = parse_hunk_header(rest) else { continue };
            (old_no, new_no) = (hunk.old_start, hunk.new_start);
            (old_left, new_left) = (hunk.old_lines, hunk.new_lines);
            file.hunks.push(hunk);
            in_hunks = true;
            continue;
        }
        if in_hunks {
            let Some(hunk) = file.hunks.last_mut() else { continue };
            let (kind, text) = match line.as_bytes().first() {
                Some(b'\\') => {
                    if let Some(last) = hunk.lines.last_mut() {
                        last.no_newline = true;
                    }
                    continue;
                }
                // Past the hunk's lines: what git adds after a file, such as
                // "* Unmerged path", or the end of the output.
                _ if old_left == 0 && new_left == 0 => continue,
                Some(b'+') => (LineKind::Added, &line[1..]),
                Some(b'-') => (LineKind::Removed, &line[1..]),
                Some(b' ') => (LineKind::Context, &line[1..]),
                // `diff.suppressBlankEmpty` writes empty context lines bare.
                None => (LineKind::Context, ""),
                _ => continue,
            };
            let (old, new) = match kind {
                LineKind::Context => (Some(old_no), Some(new_no)),
                LineKind::Added => (None, Some(new_no)),
                LineKind::Removed => (Some(old_no), None),
            };
            if old.is_some() {
                old_no += 1;
                old_left = old_left.saturating_sub(1);
            }
            if new.is_some() {
                new_no += 1;
                new_left = new_left.saturating_sub(1);
            }
            hunk.lines.push(DiffLine { kind, text: text.to_owned(), old, new, no_newline: false });
            continue;
        }
        if let Some(p) = line.strip_prefix("--- ") {
            minus = Some(side_path(p, "a/"));
        } else if let Some(p) = line.strip_prefix("+++ ") {
            plus = Some(side_path(p, "b/"));
        } else if line.starts_with("new file mode ") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode ") {
            file.status = FileStatus::Deleted;
        } else if let Some(m) = line.strip_prefix("old mode ") {
            old_mode = Some(m.to_owned());
        } else if let Some(m) = line.strip_prefix("new mode ") {
            file.mode = Some((old_mode.take().unwrap_or_default(), m.to_owned()));
        } else if let Some(rest) = line.strip_prefix("index ") {
            // `<old>..<new>`, then the mode when it is the same on both sides.
            let ids = rest.split_whitespace().next().unwrap_or_default();
            file.blobs = ids.split_once("..").map(|(old, new)| (old.to_owned(), new.to_owned()));
        } else if let Some(p) = line.strip_prefix("rename from ") {
            old_path = Some(unquote_path(p));
            file.status = FileStatus::Renamed;
        } else if let Some(p) = line.strip_prefix("rename to ") {
            new_path = Some(unquote_path(p));
        } else if let Some(p) = line.strip_prefix("copy from ") {
            old_path = Some(unquote_path(p));
            file.status = FileStatus::Copied;
        } else if let Some(p) = line.strip_prefix("copy to ") {
            new_path = Some(unquote_path(p));
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
    }
    // The new name, else the old one (a deleted file), else the `diff` line.
    file.path = new_path.or(plus.flatten()).or(minus.clone().flatten()).or(guess)?;
    if matches!(file.status, FileStatus::Renamed | FileStatus::Copied) {
        file.old_path = old_path.or(minus.flatten());
    }
    Some(file)
}

/// `-1,4 +1,5 @@ fn main()`, after the `@@ ` that starts it.
fn parse_hunk_header(rest: &str) -> Option<Hunk> {
    let (ranges, context) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((start, lines)) => Some((start.parse().ok()?, lines.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = range(old.strip_prefix('-')?)?;
    let (new_start, new_lines) = range(new.strip_prefix('+')?)?;
    Some(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        context: context.strip_prefix(' ').unwrap_or(context).trim_end_matches('\r').to_owned(),
        lines: Vec::new(),
    })
}

/// Runs git in `dir` with `args` and then the paths as pathspecs, read
/// from its input (so any number of them fit, and no path is taken for an
/// option).
fn with_pathspecs<'a>(dir: &Path, args: &[&str], paths: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    let mut input = Vec::new();
    for p in paths {
        // A literal path, not a pattern.
        input.extend_from_slice(b":(literal)");
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    if input.is_empty() {
        return Ok(());
    }
    let mut args = args.to_vec();
    args.extend(["--pathspec-from-file=-", "--pathspec-file-nul"]);
    git_with_input(dir, &args, &input).map(drop)
}

/// Runs git in `dir`, its output as it is (the text of files may be in any
/// encoding).
fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = command(dir, args).output().map_err(spawn_error)?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(failure(args, out.status, &out.stdout, &out.stderr))
}

/// Runs git in `dir` with `input` on its standard input.
fn git_with_input(dir: &Path, args: &[&str], input: &[u8]) -> Result<String, String> {
    run_with_input(command(dir, args), args, input)
}

/// Runs `cmd` (git with `args`) with `input` on its standard input.
fn run_with_input(mut cmd: std::process::Command, args: &[&str], input: &[u8]) -> Result<String, String> {
    let mut child =
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(spawn_error)?;
    let mut stdin = child.stdin.take().ok_or("could not write to git")?;
    // Written on a thread: git may answer before it read all of it, and a
    // full pipe would then stall both.
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().map_err(|e| format!("could not run git: {e}"))?;
    let _ = writer.join();
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(failure(args, out.status, &out.stdout, &out.stderr))
}

/// What git said went wrong; some commands (`commit` with nothing staged)
/// say it on the standard output.
fn failure(args: &[&str], status: std::process::ExitStatus, stdout: &[u8], stderr: &[u8]) -> String {
    let message = super::error_message(&String::from_utf8_lossy(stderr));
    let message = if message.is_empty() { super::error_message(&String::from_utf8_lossy(stdout)) } else { message };
    if message.is_empty() { format!("git {} failed ({status})", args.join(" ")) } else { message }
}

#[cfg(test)]
pub(super) mod tests;
