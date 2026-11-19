//! Editing files in Neovim or Vim, running in a terminal in place of the
//! session's terminals (the "Vi mode" of the editor settings).

use std::path::{Path, PathBuf};

use termroost_core::session::shell::{self, EditorProgram, ShellCommand};

/// What to run to edit a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub command: ShellCommand,
    pub env: Vec<(String, String)>,
    /// The program's name for the user, e.g. `nvim`.
    pub name: String,
}

/// The command that edits `file`: the editor command from the settings
/// (`setting`, its program looked up with `lookup` unless it is a full
/// path), or the editor `detect` finds when it is empty; with the file's
/// path last, relative to `root` (the session's folder, where the editor
/// starts) when it is inside it.
pub fn launch(
    setting: &str,
    file: &Path,
    root: &Path,
    detect: impl FnOnce() -> Option<EditorProgram>,
    lookup: impl Fn(&str) -> Option<PathBuf>,
) -> Result<Launch, String> {
    let (editor, mut args) = match shell::split_command_line(setting) {
        None => {
            let found = detect().ok_or("no Neovim or Vim was found: install Neovim, or set the editor command")?;
            (found, Vec::new())
        }
        Some(ShellCommand { program, args }) => {
            let path = if Path::new(&program).is_absolute() {
                PathBuf::from(&program)
            } else {
                lookup(&program).ok_or_else(|| format!("{program} was not found on PATH"))?
            };
            (EditorProgram::new(path), args)
        }
    };
    args.push(file_arg(file, root));
    let name = editor.program.file_stem().map_or_else(|| "editor".to_owned(), |s| s.to_string_lossy().into_owned());
    Ok(Launch { command: ShellCommand { program: editor.program.display().to_string(), args }, env: editor.env, name })
}

/// `file` as the editor is given it: inside `root` relative to it, with `/`
/// (which Windows programs and MSYS Vim both take), else whole.
fn file_arg(file: &Path, root: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
        }
        _ => file.display().to_string(),
    }
}

/// What to type into the running editor `name` to have it quit as `:qa`
/// does, from whatever mode it is in (Ctrl+\ Ctrl+N leaves Insert and any
/// other mode). Neovim and Vim refuse while changes are not saved, and say
/// so. `None` for other editors.
pub fn quit_keys(name: &str) -> Option<&'static [u8]> {
    let vim = ["nvim", "vim"].iter().any(|v| name.eq_ignore_ascii_case(v));
    vim.then_some(b"\x1c\x0e:qa\r".as_slice())
}

/// Edits `file` on SSH host `host`, in a terminal here running `ssh`:
/// Neovim there, else Vim, started in `root` (the session's folder there).
pub fn launch_over_ssh(host: &str, file: &Path, root: &Path) -> Launch {
    let (dir, file) = (sh_quote(&root.display().to_string()), sh_quote(&file_arg(file, root)));
    let script = format!(
        "cd {dir} 2>/dev/null; if command -v nvim >/dev/null 2>&1; then exec nvim {file}; else exec vim {file}; fi"
    );
    let ssh = std::env::var(termroost_core::remote::SSH_ENV).ok().filter(|p| !p.is_empty()).unwrap_or("ssh".into());
    // Run by the login shell there, whichever it is: all of it goes to sh.
    let args = vec!["-t".into(), "--".into(), host.to_owned(), format!("exec sh -c {}", sh_quote(&script))];
    Launch { command: ShellCommand { program: ssh, args }, env: Vec::new(), name: "nvim".into() }
}

/// `s` in single quotes for `sh`, as one word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// [`launch`] with the editor and `PATH` of this machine.
pub fn launch_here(setting: &str, file: &Path, root: &Path) -> Result<Launch, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    launch(setting, file, root, shell::find_terminal_editor, |name| shell::find_program(name, &path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_files_open_in_the_hosts_editor() {
        let l = launch_over_ssh("droplet", Path::new("/srv/app/src/it's.rs"), Path::new("/srv/app"));
        assert_eq!(l.command.args[..3], ["-t", "--", "droplet"]);
        let script = &l.command.args[3];
        assert!(script.starts_with("exec sh -c '"), "{script}");
        assert!(script.contains("nvim"), "{script}");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(quit_keys(&l.name), quit_keys("nvim"));
    }

    fn nvim() -> Option<EditorProgram> {
        Some(EditorProgram { program: PathBuf::from("/opt/nvim/bin/nvim"), env: Vec::new() })
    }

    fn lookup(name: &str) -> Option<PathBuf> {
        (name == "hx").then(|| PathBuf::from("/usr/bin/hx"))
    }

    #[test]
    fn the_detected_editor_opens_the_file_by_its_path_in_the_session() {
        let root = Path::new("/w/app");
        let l = launch("", &root.join("src").join("main.rs"), root, nvim, lookup).unwrap();
        assert_eq!(l.command.program, PathBuf::from("/opt/nvim/bin/nvim").display().to_string());
        assert_eq!(l.command.args, ["src/main.rs"]);
        assert_eq!(l.name, "nvim");
        let outside = launch("", Path::new("/etc/hosts"), root, nvim, lookup).unwrap();
        assert_eq!(outside.command.args, [Path::new("/etc/hosts").display().to_string()]);
    }

    #[test]
    fn a_set_command_keeps_its_arguments() {
        let root = Path::new("/w");
        let l = launch("hx --vsplit", &root.join("a b.txt"), root, || None, lookup).unwrap();
        assert_eq!(l.command.program, PathBuf::from("/usr/bin/hx").display().to_string());
        assert_eq!(l.command.args, ["--vsplit", "a b.txt"]);
        assert!(launch("emacs", &root.join("x"), root, nvim, lookup).unwrap_err().contains("emacs"));
        assert!(launch("", &root.join("x"), root, || None, lookup).unwrap_err().contains("Neovim"));
    }

    #[test]
    fn only_vim_and_neovim_are_asked_to_quit() {
        assert_eq!(quit_keys("nvim"), Some(&b"\x1c\x0e:qa\r"[..]));
        assert!(quit_keys("vim").is_some() && quit_keys("NVIM").is_some());
        assert!(quit_keys("hx").is_none() && quit_keys("nano").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn a_full_path_is_used_as_it_is() {
        let vim = r"C:\Program Files\Git\usr\bin\vim.exe";
        let l = launch(&format!("\"{vim}\""), Path::new(r"C:\w\x.txt"), Path::new(r"C:\w"), || None, |_| None).unwrap();
        assert_eq!(l.command.program, vim);
        assert_eq!(l.command.args, ["x.txt"]);
        assert_eq!(l.env, [("TERM".to_owned(), "xterm-256color".to_owned())], "MSYS Vim");
    }
}
