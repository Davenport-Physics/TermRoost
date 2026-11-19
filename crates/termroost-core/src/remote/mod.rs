//! Projects on other computers, reached over SSH.
//!
//! A project's [`crate::Project::host`] names the SSH host its folder is on.
//! Terminal Rooster connects to it with `ssh`, which runs Terminal Rooster's
//! helper there (see [`server`]), copied there first when needed (see
//! [`bootstrap`]). One connection per host carries everything:
//!
//! * **Terminals** run under helpers of their own on the host (as terminals
//!   that keep running do here, see [`crate::host`]), so they run on while the
//!   connection is down and are shown again, as they are, once it is back.
//! * **Agents** are found by looking at the host's processes and their
//!   records there, so their exact sessions are resumed as here.
//! * **Files and git** go through [`Machine`], which does what was asked on
//!   the computer the folder is on.

pub(crate) mod bootstrap;
pub(crate) mod client;
pub(crate) mod server;
pub(crate) mod wire;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use bootstrap::{BINARY_ENV, REMOTE_ARG, SSH_ENV, check_host};

/// This program, for starting it again (as a terminal's helper): on Linux
/// through `/proc/self/exe`, which still works after a newer build of the
/// helper replaced (or deleted) the file it was started from.
pub fn this_program() -> std::io::Result<PathBuf> {
    let proc_exe = Path::new("/proc/self/exe");
    if cfg!(target_os = "linux") && proc_exe.exists() {
        return Ok(proc_exe.to_path_buf());
    }
    std::env::current_exe()
}
pub use client::{RemoteHost, RemoteStatus, Transport};
pub use server::serve;

use crate::files::{Entry, FileMeta, FsError};
use crate::git::Status;
use wire::{Bytes, Request};

/// The hosts named in `~/.ssh/config` (and files it includes), in order,
/// leaving out patterns (`*`, `?`, `!`): those to offer for a new project.
pub fn ssh_config_hosts() -> Vec<String> {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else { return Vec::new() };
    let ssh = home.join(".ssh");
    let mut found = Vec::new();
    read_ssh_config(&ssh.join("config"), &ssh, &mut found, 0);
    found
}

fn read_ssh_config(file: &Path, ssh_dir: &Path, found: &mut Vec<String>, depth: usize) {
    let Ok(text) = std::fs::read_to_string(file) else { return };
    for line in text.lines() {
        let line = line.trim();
        let (key, rest) = line.split_once(|c: char| c.is_whitespace() || c == '=').unwrap_or((line, ""));
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        if key.eq_ignore_ascii_case("host") {
            for name in rest.split_whitespace() {
                if !name.contains(['*', '?', '!']) && !found.iter().any(|f| f == name) {
                    found.push(name.to_owned());
                }
            }
        } else if key.eq_ignore_ascii_case("include") && depth < 4 {
            for inc in rest.split_whitespace().filter(|i| !i.contains(['*', '?'])) {
                // As ssh takes them: from the home folder, whole, or in ~/.ssh.
                let path = match inc.strip_prefix("~/") {
                    Some(r) => ssh_dir.parent().unwrap_or(ssh_dir).join(r),
                    None if Path::new(inc).is_absolute() => PathBuf::from(inc),
                    None => ssh_dir.join(inc),
                };
                read_ssh_config(&path, ssh_dir, found, depth + 1);
            }
        }
    }
}

/// How long a request about files may take.
const FILES_TIMEOUT: Duration = Duration::from_secs(60);

/// The computer a project's folder is on: this one, or a remote host. What
/// is asked of it is done there. Cheap to clone. A remote one is asked over
/// the connection its terminals made (or [`Machine::resolve_dir`] makes),
/// waiting while it is being made: call these off the GUI thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Machine {
    #[default]
    Local,
    Remote(RemoteHost),
}

impl Machine {
    /// The host's name, for a remote one.
    pub fn host(&self) -> Option<&str> {
        match self {
            Machine::Local => None,
            Machine::Remote(h) => Some(h.name()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Machine::Remote(_))
    }

    pub(crate) fn remote(&self) -> Option<&RemoteHost> {
        match self {
            Machine::Local => None,
            Machine::Remote(h) => Some(h),
        }
    }

    /// The contents of each folder (see [`crate::files::list_dir`]).
    pub fn list_dirs(&self, dirs: Vec<PathBuf>) -> Vec<(PathBuf, Result<Vec<Entry>, String>)> {
        match self {
            Machine::Local => dirs.into_iter().map(|d| (crate::files::list_dir(&d), d)).map(|(l, d)| (d, l)).collect(),
            Machine::Remote(h) => match h.call(Request::ListDirs(dirs.clone()), FILES_TIMEOUT) {
                Ok(listed) => listed,
                Err(e) => dirs.into_iter().map(|d| (d, Err(e.clone()))).collect(),
            },
        }
    }

    pub fn metadata(&self, path: &Path) -> Result<FileMeta, FsError> {
        match self {
            Machine::Local => crate::files::metadata(path),
            Machine::Remote(h) => {
                h.call(Request::Metadata(path.into()), FILES_TIMEOUT).unwrap_or_else(|e| Err(FsError::Other(e)))
            }
        }
    }

    /// See [`crate::files::read_file`].
    pub fn read_file(&self, path: &Path, max: u64) -> Result<(FileMeta, Vec<u8>), FsError> {
        match self {
            Machine::Local => crate::files::read_file(path, max),
            Machine::Remote(h) => h
                .call::<Result<(FileMeta, Bytes), FsError>>(Request::ReadFile { path: path.into(), max }, FILES_TIMEOUT)
                .unwrap_or_else(|e| Err(FsError::Other(e)))
                .map(|(meta, bytes)| (meta, bytes.0)),
        }
    }

    pub fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<FileMeta, FsError> {
        match self {
            Machine::Local => crate::files::write_file(path, bytes),
            Machine::Remote(h) => h
                .call(Request::WriteFile { path: path.into(), bytes: Bytes(bytes.to_vec()) }, FILES_TIMEOUT)
                .unwrap_or_else(|e| Err(FsError::Other(e))),
        }
    }

    /// See [`crate::files::create_file`].
    pub fn create_file(&self, path: &Path) -> Result<(), String> {
        match self {
            Machine::Local => crate::files::create_file(path),
            Machine::Remote(h) => h.call(Request::CreateFile(path.into()), FILES_TIMEOUT).and_then(|r| r),
        }
    }

    /// See [`crate::files::resolve_dir`]. Connecting to a host that can't
    /// be reached fails with why.
    pub fn resolve_dir(&self, path: &Path) -> Result<PathBuf, String> {
        match self {
            Machine::Local => crate::files::resolve_dir(path),
            Machine::Remote(h) => {
                // Asked for on purpose: worth trying again at once.
                h.conn(true)?;
                h.call(Request::ResolveDir(path.into()), FILES_TIMEOUT).and_then(|r| r)
            }
        }
    }

    /// See [`crate::files::complete_dir`].
    pub fn complete_dir(&self, partial: &str, limit: usize) -> Vec<String> {
        match self {
            Machine::Local => crate::files::complete_dir(partial, limit),
            Machine::Remote(h) => {
                h.call(Request::CompleteDir { partial: partial.to_owned(), limit }, FILES_TIMEOUT).unwrap_or_default()
            }
        }
    }

    /// Which of `paths` are folders, or `None` when that can't be told now
    /// (a host that isn't connected is not asked: a folder there doesn't
    /// count as gone just because the connection is).
    pub fn are_dirs(&self, paths: &[PathBuf]) -> Option<Vec<bool>> {
        match self {
            Machine::Local => Some(crate::files::are_dirs(paths)),
            Machine::Remote(h) => h.call_if_up(Request::AreDirs(paths.to_vec()), FILES_TIMEOUT),
        }
    }

    /// Where `dir` stands in git (see [`Status::of`]); `None` outside a
    /// repository, or when the host can't be asked.
    pub fn git_status(&self, dir: &Path) -> Option<Status> {
        match self {
            Machine::Local => Status::of(dir),
            Machine::Remote(h) => {
                let mut status: Status =
                    h.call::<Option<Status>>(Request::GitStatus(dir.into()), FILES_TIMEOUT).ok()??;
                status.repo.machine = self.clone();
                Some(status)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_config_hosts_leave_out_patterns_and_follow_includes() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("config"), "Host *\n  User me\nHost droplet web-1\nInclude extra\nhost=db !bad\n")
            .unwrap();
        std::fs::write(ssh.join("extra"), "Host  staging\n  HostName 10.0.0.2\nHost droplet\n").unwrap();
        let mut found = Vec::new();
        read_ssh_config(&ssh.join("config"), &ssh, &mut found, 0);
        assert_eq!(found, ["droplet", "web-1", "staging", "db"]);
    }
}
