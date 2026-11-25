use std::collections::BTreeSet;

use super::*;

fn line(kind: LineKind, text: &str, old: Option<u32>, new: Option<u32>) -> DiffLine {
    DiffLine { kind, text: text.into(), old, new, no_newline: false }
}

#[test]
fn parses_hunks_and_numbers_their_lines() {
    let out = "diff --git a/src/main.rs b/src/main.rs\n\
               index 1111111..2222222 100644\n\
               --- a/src/main.rs\n\
               +++ b/src/main.rs\n\
               @@ -1,3 +1,3 @@ fn main() {\n \
               one\n\
               -two\n\
               +TWO\n \
               three\n\
               @@ -10,2 +10,3 @@\n \
               ten\n\
               +ten and a half\n\
               --- a removed line that looks like a header\n";
    let files = parse_diff(out.as_bytes());
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!((f.path.as_str(), f.status, f.old_path.as_deref()), ("src/main.rs", FileStatus::Modified, None));
    assert_eq!(f.hunks.len(), 2);
    let h = &f.hunks[0];
    assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (1, 3, 1, 3));
    assert_eq!(h.context, "fn main() {");
    assert_eq!(
        h.lines,
        vec![
            line(LineKind::Context, "one", Some(1), Some(1)),
            line(LineKind::Removed, "two", Some(2), None),
            line(LineKind::Added, "TWO", None, Some(2)),
            line(LineKind::Context, "three", Some(3), Some(3)),
        ]
    );
    let h = &f.hunks[1];
    assert_eq!(h.header(), "@@ -10,2 +10,3 @@");
    assert_eq!(h.lines[2], line(LineKind::Removed, "-- a removed line that looks like a header", Some(11), None));
    assert_eq!(f.counts(), (2, 2));
    assert!(f.splittable());
    assert_eq!(f.blobs, Some(("1111111".into(), "2222222".into())));
    assert_eq!(f.fingerprint().as_deref(), Some("1111111..2222222"));
}

#[test]
fn parses_every_kind_of_file() {
    let out = "diff --git a/a b.txt b/a b.txt\n\
               new file mode 100644\n\
               index 0000000..814f4a4\n\
               --- /dev/null\n\
               +++ b/a b.txt\t\n\
               @@ -0,0 +1,2 @@\n\
               +one\n\
               +two\n\
               \\ No newline at end of file\n\
               diff --git a/gone.txt b/gone.txt\n\
               deleted file mode 100644\n\
               --- a/gone.txt\n\
               +++ /dev/null\n\
               @@ -1 +0,0 @@\n\
               -bye\n\
               diff --git a/old name.rs b/new name.rs\n\
               similarity index 90%\n\
               rename from old name.rs\n\
               rename to new name.rs\n\
               --- a/old name.rs\t\n\
               +++ b/new name.rs\t\n\
               @@ -1,2 +1,2 @@\n \
               keep\n\
               -x\n\
               +y\n\
               diff --git a/plain.txt b/moved.txt\n\
               similarity index 100%\n\
               rename from plain.txt\n\
               rename to moved.txt\n\
               diff --git a/bin.dat b/bin.dat\n\
               new file mode 100644\n\
               Binary files /dev/null and b/bin.dat differ\n\
               diff --git a/run.sh b/run.sh\n\
               old mode 100644\n\
               new mode 100755\n\
               diff --git \"a/tab\\there\" \"b/tab\\there\"\n\
               old mode 100644\n\
               new mode 100755\n\
               diff --cc conflict.txt\n\
               index 1,1..0000000\n\
               @@@ -1,1 -1,1 +1,5 @@@\n\
               ++<<<<<<< HEAD\n\
               * Unmerged path conflict.txt\n";
    let files = parse_diff(out.as_bytes());
    let summary: Vec<(&str, FileStatus)> = files.iter().map(|f| (f.path.as_str(), f.status)).collect();
    assert_eq!(
        summary,
        vec![
            ("a b.txt", FileStatus::Added),
            ("gone.txt", FileStatus::Deleted),
            ("new name.rs", FileStatus::Renamed),
            ("moved.txt", FileStatus::Renamed),
            ("bin.dat", FileStatus::Added),
            ("run.sh", FileStatus::Modified),
            ("tab\there", FileStatus::Modified),
            ("conflict.txt", FileStatus::Conflicted),
        ]
    );
    assert!(files[0].hunks[0].lines[1].no_newline);
    assert_eq!(files[0].hunks[0].lines[1].new, Some(2));
    assert_eq!(files[1].hunks[0].lines, vec![line(LineKind::Removed, "bye", Some(1), None)]);
    assert_eq!(files[2].old_path.as_deref(), Some("old name.rs"));
    assert!(files[2].splittable());
    assert_eq!(files[3].old_path.as_deref(), Some("plain.txt"));
    assert!(files[3].hunks.is_empty() && !files[3].splittable());
    assert!(files[4].binary && !files[4].splittable());
    assert_eq!(files[5].mode, Some(("100644".into(), "100755".into())));
    // A change of mode alone has no blobs: the modes tell it.
    assert_eq!(files[5].fingerprint().as_deref(), Some("100644->100755"));
    assert_eq!(files[0].blobs, Some(("0000000".into(), "814f4a4".into())));
    assert!(files[7].hunks.is_empty());
    assert_eq!(files[7].blobs, None, "not a conflict's three blobs");
    assert!(!files[0].splittable() && !files[1].splittable(), "new and deleted files go whole");
}

#[test]
fn submodules_go_whole() {
    let out = "diff --git a/lib b/lib\n\
               index 1111111..2222222 160000\n\
               --- a/lib\n\
               +++ b/lib\n\
               @@ -1 +1 @@\n\
               -Subproject commit 1111111111111111111111111111111111111111\n\
               +Subproject commit 2222222222222222222222222222222222222222\n";
    let f = &parse_diff(out.as_bytes())[0];
    assert!(f.is_submodule() && !f.splittable());
    assert_eq!(f.status, FileStatus::Modified);
}

#[test]
fn empty_context_lines_may_be_bare() {
    // `diff.suppressBlankEmpty` leaves out the space of an empty context line.
    let out = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
    let lines = &parse_diff(out.as_bytes())[0].hunks[0].lines;
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[1], line(LineKind::Context, "", Some(2), Some(2)));
    assert_eq!(lines[3], line(LineKind::Added, "c", None, Some(3)));
}

#[test]
fn text_that_is_not_utf8_is_marked() {
    let mut out = b"diff --git a/l1.txt b/l1.txt\n--- a/l1.txt\n+++ b/l1.txt\n@@ -1 +1 @@\n-caf".to_vec();
    out.extend([0xe9, b'\n']);
    out.extend(b"+cafe\n");
    let f = &parse_diff(&out)[0];
    assert!(f.lossy && !f.splittable());
    assert_eq!(f.hunks[0].lines[0].text, "caf\u{fffd}");
}

#[test]
fn paths_are_quoted_and_unquoted() {
    for p in ["plain.txt", "with space.txt", "tab\there", "quote\"d", "back\\slash", "new\nline", "ünï.txt"] {
        assert_eq!(unquote_path(&quote_path(p)), p);
    }
    assert_eq!(quote_path("a b"), "a b");
    assert_eq!(unquote_path(r#""\303\274""#), "ü");
    assert_eq!(git_line_path("a/x y b/x y"), Some("x y".into()));
    assert_eq!(git_line_path("a/x b/y"), None);
    assert_eq!(git_line_path(r#""a/t\tx" "b/t\tx""#), Some("t\tx".into()));
}

fn hunk_of(lines: &[(char, &str)], old_start: u32, new_start: u32) -> Hunk {
    let mut hunk = Hunk { old_start, old_lines: 0, new_start, new_lines: 0, context: String::new(), lines: vec![] };
    for (prefix, text) in lines {
        let kind = match prefix {
            '+' => LineKind::Added,
            '-' => LineKind::Removed,
            _ => LineKind::Context,
        };
        hunk.old_lines += u32::from(kind != LineKind::Added);
        hunk.new_lines += u32::from(kind != LineKind::Removed);
        hunk.lines.push(line(kind, text, None, None));
    }
    hunk
}

#[test]
fn patches_of_chosen_lines() {
    let hunk = hunk_of(&[(' ', "a"), ('-', "b"), ('+', "B"), ('+', "C"), (' ', "d")], 4, 4);
    let whole = hunk_patch("f.txt", &hunk, None, false).unwrap();
    assert_eq!(whole, "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -4,3 +4,4 @@\n a\n-b\n+B\n+C\n d\n");

    // Staging only "C": "b" stays (as context), "B" stays out.
    let c = BTreeSet::from([3]);
    let patch = hunk_patch("f.txt", &hunk, Some(&c), false).unwrap();
    assert!(patch.ends_with("@@ -4,3 +4,4 @@\n a\n b\n+C\n d\n"), "{patch}");
    // Unstaging only "C": "B" stays (it is staged), "b" stays out.
    let patch = hunk_patch("f.txt", &hunk, Some(&c), true).unwrap();
    assert!(patch.ends_with("@@ -4,3 +4,4 @@\n a\n B\n+C\n d\n"), "{patch}");

    assert_eq!(hunk_patch("f.txt", &hunk, Some(&BTreeSet::from([0, 4])), false), None, "only context");
    assert_eq!(hunk_patch("f.txt", &hunk, Some(&BTreeSet::new()), true), None);

    // A name with a space ends with a tab, as git writes it.
    let patch = hunk_patch("a b.txt", &hunk, None, false).unwrap();
    assert!(patch.starts_with("diff --git a/a b.txt b/a b.txt\n--- a/a b.txt\t\n+++ b/a b.txt\t\n"), "{patch}");
}

#[test]
fn a_side_left_empty_names_the_line_before() {
    // The whole file was one line, replaced.
    let hunk = hunk_of(&[('-', "a"), ('+', "b")], 1, 1);
    let only_removal = hunk_patch("f", &hunk, Some(&BTreeSet::from([0])), false).unwrap();
    assert!(only_removal.ends_with("@@ -1,1 +0,0 @@\n-a\n"), "{only_removal}");
    // The removal left out (in reverse): only the added line remains.
    let only_addition = hunk_patch("f", &hunk, Some(&BTreeSet::from([1])), true).unwrap();
    assert!(only_addition.ends_with("@@ -0,0 +1,1 @@\n+b\n"), "{only_addition}");
}

#[test]
fn the_no_newline_mark_goes_with_its_line() {
    let mut hunk = hunk_of(&[('-', "a"), ('+', "b")], 1, 1);
    hunk.lines[0].no_newline = true;
    hunk.lines[1].no_newline = true;
    let patch = hunk_patch("f", &hunk, None, false).unwrap();
    assert!(patch.ends_with("-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n"), "{patch}");
    let patch = hunk_patch("f", &hunk, Some(&BTreeSet::from([0])), false).unwrap();
    assert!(patch.ends_with("-a\n\\ No newline at end of file\n"), "{patch}");
}

// With git installed.

/// A new repository in a temporary folder with its own identity; `None`
/// without git.
fn repo() -> Option<(tempfile::TempDir, Repo)> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("app");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]).ok()?;
    for (key, value) in [
        ("user.name", "Tess"),
        ("user.email", "t@t"),
        ("commit.gpgsign", "false"),
        // Files are compared as written, whatever the machine's setting.
        ("core.autocrlf", "false"),
    ] {
        git(&root, &["config", key, value]).unwrap();
    }
    let repo = Repo::discover(&root).unwrap();
    Some((dir, repo))
}

fn write(repo: &Repo, path: &str, text: &str) {
    let full = repo.worktree.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

fn run(repo: &Repo, args: &[&str]) -> String {
    git(&repo.worktree, args).unwrap()
}

/// What the index holds for `path`.
fn staged_text(repo: &Repo, path: &str) -> String {
    run(repo, &["show", &format!(":{path}")])
}

fn numbered(n: u32) -> String {
    (1..=n).map(|i| format!("{i}\n")).collect()
}

fn paths(files: &[FileDiff]) -> Vec<&str> {
    files.iter().map(|f| f.path.as_str()).collect()
}

#[test]
fn stages_and_unstages_hunks_with_git() {
    let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
    write(&repo, "a b.txt", &numbered(20));
    repo.stage_paths(["a b.txt"]).unwrap();
    assert_eq!(
        repo.commit("first", false).unwrap(),
        format!("Committed {}.", run(&repo, &["log", "-1", "--format=%h"]).trim())
    );
    write(&repo, "a b.txt", &numbered(20).replace("\n2\n", "\ntwo\n").replace("19\n", "nineteen\n"));

    let changes = repo.working_changes().unwrap();
    assert!(changes.staged.is_empty() && changes.untracked.is_empty() && !changes.unborn);
    let file = &changes.unstaged[0];
    assert_eq!((file.path.as_str(), file.hunks.len()), ("a b.txt", 2));

    // The second hunk only.
    repo.stage_hunk(file, 1, None).unwrap();
    assert_eq!(staged_text(&repo, "a b.txt"), numbered(20).replace("19\n", "nineteen\n"));
    let changes = repo.working_changes().unwrap();
    assert_eq!(changes.staged[0].hunks.len(), 1);
    assert_eq!(changes.staged[0].hunks[0].lines.iter().find(|l| l.kind == LineKind::Added).unwrap().text, "nineteen");
    assert_eq!(changes.unstaged[0].hunks.len(), 1);
    assert_eq!(changes.unstaged[0].hunks[0].new_start, 1);

    // And back.
    repo.unstage_hunk(&changes.staged[0], 0, None).unwrap();
    let changes = repo.working_changes().unwrap();
    assert!(changes.staged.is_empty());
    assert_eq!(changes.unstaged[0].hunks.len(), 2);

    // A hunk that no longer fits the index is refused, and nothing changes.
    let mut stale = changes.unstaged[0].clone();
    stale.hunks[0].lines[0].text = "not there".into();
    assert!(repo.stage_hunk(&stale, 0, None).is_err());
    assert!(repo.working_changes().unwrap().staged.is_empty());
}

#[test]
fn stages_and_unstages_chosen_lines_with_git() {
    let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
    write(&repo, "f.txt", &numbered(10));
    repo.stage_paths(["f.txt"]).unwrap();
    repo.commit("first", false).unwrap();
    write(&repo, "f.txt", &numbered(10).replace("5\n", "five\n5.5\n"));

    let file = &repo.working_changes().unwrap().unstaged[0];
    let hunk = &file.hunks[0];
    let at = |text: &str| hunk.lines.iter().position(|l| l.text == text && l.kind != LineKind::Context).unwrap();
    // Only the new line 5.5: 5 stays as it was.
    repo.stage_hunk(file, 0, Some(&BTreeSet::from([at("5.5")]))).unwrap();
    assert_eq!(staged_text(&repo, "f.txt"), numbered(10).replace("5\n", "5\n5.5\n"));
    let changes = repo.working_changes().unwrap();
    let left: Vec<(LineKind, &str)> = changes.unstaged[0].hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| (l.kind, l.text.as_str()))
        .collect();
    assert_eq!(left, vec![(LineKind::Removed, "5"), (LineKind::Added, "five")]);

    // Stage the rest, then unstage only the removal of 5: 5 comes back
    // into the index beside five.
    repo.stage_paths(["f.txt"]).unwrap();
    let staged = &repo.working_changes().unwrap().staged[0];
    let removal = staged.hunks[0].lines.iter().position(|l| l.kind == LineKind::Removed).unwrap();
    repo.unstage_hunk(staged, 0, Some(&BTreeSet::from([removal]))).unwrap();
    assert_eq!(staged_text(&repo, "f.txt"), numbered(10).replace("5\n", "5\nfive\n5.5\n"));
}

#[test]
fn a_last_line_without_a_line_break_with_git() {
    let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
    write(&repo, "f.txt", "a\nb");
    repo.stage_paths(["f.txt"]).unwrap();
    repo.commit("first", false).unwrap();
    write(&repo, "f.txt", "a\nc");
    let file = &repo.working_changes().unwrap().unstaged[0];
    assert!(file.hunks[0].lines.iter().filter(|l| l.kind != LineKind::Context).all(|l| l.no_newline));
    repo.stage_hunk(file, 0, None).unwrap();
    assert_eq!(staged_text(&repo, "f.txt"), "a\nc");
    let staged = &repo.working_changes().unwrap().staged[0];
    repo.unstage_hunk(staged, 0, None).unwrap();
    assert_eq!(staged_text(&repo, "f.txt"), "a\nb");
}

#[test]
fn whole_files_and_new_repositories_with_git() {
    let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
    write(&repo, "src/new.rs", "fn main() {}\n");
    write(&repo, "notes.txt", "x");
    write(&repo, "bin.dat", "bin\0ary");
    let changes = repo.working_changes().unwrap();
    assert!(changes.unborn);
    assert_eq!(paths(&changes.untracked), vec!["bin.dat", "notes.txt", "src/new.rs"]);
    let new = &changes.untracked[2];
    assert_eq!(new.hunks[0].lines, vec![line(LineKind::Added, "fn main() {}", None, Some(1))]);
    assert!(changes.untracked[0].binary);
    assert!(changes.untracked[1].hunks[0].lines[0].no_newline);
    assert!(repo.commit("nothing staged", false).is_err());

    repo.stage_paths(["src/new.rs", "notes.txt"]).unwrap();
    let changes = repo.working_changes().unwrap();
    assert_eq!(paths(&changes.staged), vec!["notes.txt", "src/new.rs"]);
    assert_eq!(changes.staged[0].status, FileStatus::Added);
    assert_eq!(paths(&changes.untracked), vec!["bin.dat"]);

    // Before the first commit, unstaging drops it from the index.
    repo.unstage_paths(["notes.txt"]).unwrap();
    assert_eq!(paths(&repo.working_changes().unwrap().staged), vec!["src/new.rs"]);
    assert!(repo.commit("  \n", false).is_err(), "an empty message");
    repo.commit("Add: main\n\nThe body.", false).unwrap();
    assert_eq!(repo.last_message().unwrap(), "Add: main\n\nThe body.");

    // Deleted, and renamed: renames show among the staged changes.
    std::fs::remove_file(repo.worktree.join("src/new.rs")).unwrap();
    let changes = repo.working_changes().unwrap();
    assert!(!changes.unborn);
    assert_eq!(changes.unstaged[0].status, FileStatus::Deleted);
    run(&repo, &["checkout", "--", "src/new.rs"]);
    run(&repo, &["mv", "src/new.rs", "src/main.rs"]);
    let staged = &repo.working_changes().unwrap().staged;
    assert_eq!((staged[0].path.as_str(), staged[0].old_path.as_deref()), ("src/main.rs", Some("src/new.rs")));
    repo.unstage_paths(staged[0].paths()).unwrap();
    let changes = repo.working_changes().unwrap();
    assert!(changes.staged.is_empty());
    assert_eq!(changes.unstaged[0].status, FileStatus::Deleted);
    assert!(paths(&changes.untracked).contains(&"src/main.rs"));

    // Amending: the new message replaces the old one.
    repo.stage_paths(["notes.txt"]).unwrap();
    assert!(repo.commit("Add: main and notes", true).unwrap().starts_with("Amended "));
    assert_eq!(run(&repo, &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert_eq!(repo.last_message().unwrap(), "Add: main and notes");
}

#[test]
fn conflicts_with_git() {
    let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
    write(&repo, "c.txt", "base\n");
    repo.stage_paths(["c.txt"]).unwrap();
    repo.commit("base", false).unwrap();
    run(&repo, &["switch", "-q", "-c", "side"]);
    write(&repo, "c.txt", "side\n");
    repo.stage_paths(["c.txt"]).unwrap();
    repo.commit("side", false).unwrap();
    run(&repo, &["switch", "-q", "main"]);
    write(&repo, "c.txt", "main\n");
    repo.stage_paths(["c.txt"]).unwrap();
    repo.commit("main", false).unwrap();
    assert!(git(&repo.worktree, &["merge", "-q", "side"]).is_err());

    let changes = repo.working_changes().unwrap();
    assert_eq!(changes.unstaged.len(), 1);
    assert_eq!((changes.unstaged[0].path.as_str(), changes.unstaged[0].status), ("c.txt", FileStatus::Conflicted));
    assert!(changes.staged.iter().all(|f| f.path != "c.txt" || f.hunks.is_empty()));
    write(&repo, "c.txt", "both\n");
    repo.stage_paths(["c.txt"]).unwrap();
    let changes = repo.working_changes().unwrap();
    assert!(changes.unstaged.is_empty());
    assert_eq!(changes.staged[0].status, FileStatus::Modified);
    repo.commit("Merge side", false).unwrap();
}

#[test]
fn commit_details_with_git() {
    let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
    write(&repo, "a.txt", "one\n");
    repo.stage_paths(["a.txt"]).unwrap();
    repo.commit("Add: a\n\nWith a body\nof two lines.", false).unwrap();
    let first = repo.commit_details("HEAD").unwrap();
    assert!(first.parents.is_empty());
    assert_eq!(first.message, "Add: a\n\nWith a body\nof two lines.");
    assert_eq!((first.author.name.as_str(), first.author.email.as_str()), ("Tess", "t@t"));
    assert!(first.author.time > 1_600_000_000 && first.committer.time >= first.author.time);
    assert_eq!(paths(&first.files), vec!["a.txt"]);
    assert_eq!(first.files[0].status, FileStatus::Added);

    run(&repo, &["switch", "-q", "-c", "side"]);
    write(&repo, "side.txt", "s\n");
    repo.stage_paths(["side.txt"]).unwrap();
    repo.commit("side", false).unwrap();
    run(&repo, &["switch", "-q", "main"]);
    write(&repo, "a.txt", "one\ntwo\n");
    repo.stage_paths(["a.txt"]).unwrap();
    repo.commit("Change: a", false).unwrap();
    let second = repo.commit_details("HEAD").unwrap();
    assert_eq!(second.parents, vec![first.id.clone()]);
    assert_eq!(second.files[0].hunks[0].lines[1], line(LineKind::Added, "two", None, Some(2)));

    run(&repo, &["merge", "-q", "--no-edit", "side"]);
    let merge = repo.commit_details("HEAD").unwrap();
    assert_eq!(merge.parents.len(), 2);
    assert_eq!(paths(&merge.files), vec!["side.txt"], "against the first parent");
    assert!(repo.commit_details("no-such-thing").is_err());
}

#[test]
fn many_paths_fit_with_git() {
    let Some((_dir, repo)) = repo() else { return eprintln!("git not available; skipping") };
    // More than a command line holds on Windows.
    let names: Vec<String> = (0..600).map(|i| format!("{}/{i:04}.txt", "a-long-folder-name".repeat(3))).collect();
    for name in &names {
        write(&repo, name, "x\n");
    }
    repo.stage_paths(names.iter().map(String::as_str)).unwrap();
    assert_eq!(repo.working_changes().unwrap().staged.len(), 600);

    // Paths are taken as they are, not as patterns.
    write(&repo, "a.txt", "x\n");
    write(&repo, "[ab].txt", "x\n");
    repo.stage_paths(["[ab].txt"]).unwrap();
    assert_eq!(paths(&repo.working_changes().unwrap().untracked), vec!["a.txt"]);
}
