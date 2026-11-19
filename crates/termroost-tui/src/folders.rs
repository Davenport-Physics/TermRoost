//! Folder paths as typed and shown: `~` for the home folder, Tab
//! completion of folder names, and folders of SSH hosts as scp takes them
//! (`droplet:~/app`).

use std::path::{MAIN_SEPARATOR, Path, PathBuf};

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// The folder a typed path names: `~` is the home folder, and relative
/// paths are taken from `base`.
pub fn resolve(input: &str, base: &Path, home: Option<&Path>) -> PathBuf {
    let input = input.trim();
    let expanded = match (input.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(is_sep) => {
            home.join(rest.trim_start_matches(is_sep))
        }
        _ => PathBuf::from(input),
    };
    if expanded.is_absolute() { expanded } else { base.join(expanded) }
}

/// A path for showing, with the home folder as `~`.
pub fn pretty(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = path.strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~{MAIN_SEPARATOR}{}", rest.display());
    }
    path.display().to_string()
}

/// A folder of an SSH host, typed as scp takes it: `host:path`, the path
/// relative to the home folder there unless it starts with `/` (an empty one
/// is the home folder). `None` for a folder of this computer, such as
/// `C:\src` on Windows.
pub fn split_remote(input: &str) -> Option<(String, String)> {
    let (host, path) = input.trim().split_once(':')?;
    let drive = cfg!(windows) && host.len() == 1;
    let plain = !host.is_empty() && !host.contains(is_sep) && !host.contains('\\') && !host.starts_with('-');
    (plain && !drive && !host.contains(char::is_whitespace)).then(|| {
        let path = if path.is_empty() { "~".to_owned() } else { path.to_owned() };
        (host.to_owned(), path)
    })
}

/// `input` (`head` then a partial name) completed with what `names` (the
/// folders there that start with it) share, or `None` when that adds
/// nothing. A unique match gets a `/`.
pub fn complete_from(input: &str, mut names: Vec<String>) -> Option<String> {
    let cut = input.rfind('/').map(|i| i + 1).unwrap_or(0);
    let head = &input[..cut];
    names.sort();
    let first = names.first()?;
    let common = names.iter().skip(1).fold(first.chars().count(), |n, other| {
        first.chars().zip(other.chars()).take(n).take_while(|(a, b)| a == b).count()
    });
    let stem: String = first.chars().take(common).collect();
    let done = if names.len() == 1 { format!("{head}{stem}/") } else { format!("{head}{stem}") };
    (done != input).then_some(done)
}

/// `input` with its last folder name completed as far as the folders there
/// agree, or `None` when that adds nothing. A unique match gets a separator.
pub fn complete(input: &str, base: &Path, home: Option<&Path>) -> Option<String> {
    let cut = input.rfind(is_sep).map(|i| i + 1).unwrap_or(0);
    let (head, partial) = input.split_at(cut);
    let dir = if head.is_empty() { base.to_path_buf() } else { resolve(head, base, home) };
    let lower = partial.to_lowercase();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.to_lowercase().starts_with(&lower))
        .filter(|n| !n.starts_with('.') || partial.starts_with('.'))
        .collect();
    names.sort();
    let first = names.first()?;
    let common = names.iter().skip(1).fold(first.chars().count(), |n, other| {
        first.chars().zip(other.chars()).take(n).take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase())).count()
    });
    let stem: String = first.chars().take(common).collect();
    let sep = head.chars().rev().find(|c| is_sep(*c)).unwrap_or(MAIN_SEPARATOR);
    let done = if names.len() == 1 { format!("{head}{stem}{sep}") } else { format!("{head}{stem}") };
    (done != input).then_some(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_folders_are_typed_as_for_scp() {
        assert_eq!(split_remote("droplet:/srv/app"), Some(("droplet".into(), "/srv/app".into())));
        assert_eq!(split_remote("me@10.0.0.2:app"), Some(("me@10.0.0.2".into(), "app".into())));
        assert_eq!(split_remote("droplet:"), Some(("droplet".into(), "~".into())));
        assert_eq!(split_remote("/srv/app"), None);
        assert_eq!(split_remote("./a:b"), None);
        assert_eq!(split_remote(":x"), None);
        #[cfg(windows)]
        assert_eq!(split_remote(r"C:\src"), None);
    }

    #[test]
    fn remote_names_complete_as_far_as_they_agree() {
        let found = ["/srv/apple".to_owned(), "/srv/apps".to_owned()];
        let names = found.iter().map(|f| f.trim_end_matches('/').rsplit('/').next().unwrap().to_owned()).collect();
        assert_eq!(complete_from("/srv/a", names), Some("/srv/app".into()));
        assert_eq!(complete_from("/srv/a", vec!["apple".into()]), Some("/srv/apple/".into()));
        assert_eq!(complete_from("/srv/x", Vec::new()), None);
    }

    fn tree() -> (PathBuf, impl Drop) {
        struct Gone(PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!("tr-tui-folders-{}", std::process::id()));
        for d in ["projects/api", "projects/app", "projects/web", "projects/.hidden"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("projects/apple.txt"), "").unwrap();
        (root.clone(), Gone(root))
    }

    #[test]
    fn completes_folder_names() {
        let (root, _gone) = tree();
        let s = MAIN_SEPARATOR;
        assert_eq!(complete("proj", &root, None), Some(format!("projects{s}")));
        assert_eq!(complete("projects/a", &root, None), Some("projects/ap".into()), "api and app share 'ap'");
        assert_eq!(complete("projects/ap", &root, None), None, "ambiguous, nothing to add");
        assert_eq!(complete("projects/W", &root, None), Some("projects/web/".into()), "case-insensitive");
        assert_eq!(complete("projects/.h", &root, None), Some("projects/.hidden/".into()));
        assert_eq!(complete("projects/x", &root, None), None);
    }

    #[test]
    fn home_and_relative_paths() {
        let home = Path::new(if cfg!(windows) { r"C:\Users\me" } else { "/home/me" });
        let base = Path::new(if cfg!(windows) { r"D:\work" } else { "/work" });
        assert_eq!(resolve("~", base, Some(home)), home);
        assert_eq!(resolve("~/code", base, Some(home)), home.join("code"));
        assert_eq!(resolve("api", base, Some(home)), base.join("api"));
        assert_eq!(resolve("~x", base, Some(home)), base.join("~x"));
        assert_eq!(pretty(&home.join("code"), Some(home)), format!("~{MAIN_SEPARATOR}code"));
        assert_eq!(pretty(base, Some(home)), base.display().to_string());
    }
}
