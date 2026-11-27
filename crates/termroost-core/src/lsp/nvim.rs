//! Terminal Rooster's setup for Neovim: an `init.lua` of its own, given
//! with `-u` so the user's configuration is left alone, which starts the
//! language servers Terminal Rooster installed and turns on completion and
//! diagnostics. The files come from this program, so a newer Terminal
//! Rooster brings a newer setup.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::Error;
use crate::session::bundle;

const INIT: &str = include_str!("nvim/init.lua");
const SERVERS: &str = include_str!("nvim/servers.lua");

/// How to run each installed server ([`super::servers`]), which `init.lua`
/// reads at the start and again when it changes, so servers installed
/// while Neovim runs start in it.
const INSTALLED: &str = "installed.json";

/// Where the setup lives: `nvim-setup` in the local data directory.
pub fn dir() -> Option<PathBuf> {
    bundle::local_data_subdir("nvim-setup")
}

/// Writes the setup where it isn't current and returns its `init.lua`, for
/// `nvim -u`.
pub fn prepare() -> crate::Result<PathBuf> {
    let dir = dir().ok_or(Error::NoDataDir)?;
    std::fs::create_dir_all(&dir)?;
    write_if_changed(&dir.join("init.lua"), INIT)?;
    write_if_changed(&dir.join("servers.lua"), SERVERS)?;
    write_servers()?;
    Ok(dir.join("init.lua"))
}

/// Brings the setup's list of installed servers up to date. Nothing to do
/// before Neovim first ran with the setup.
pub fn write_servers() -> crate::Result<()> {
    // One at a time, so the list of an install that finished earlier
    // can't land after a newer one.
    static WRITING: Mutex<()> = Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(dir) = dir().filter(|d| d.is_dir()) else { return Ok(()) };
    let json = serde_json::to_string_pretty(&super::servers()).map_err(|e| Error::Invalid(e.to_string()))?;
    write_if_changed(&dir.join(INSTALLED), &json)
}

/// Writes `content` to `path` unless it holds that already. Whole: a
/// Neovim reading the file meanwhile sees the old or the new one.
fn write_if_changed(path: &Path, content: &str) -> crate::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|s| s == content) {
        return Ok(());
    }
    let part = path.with_extension("part");
    std::fs::write(&part, content)?;
    std::fs::rename(&part, path)?;
    Ok(())
}

/// The user's own Neovim configuration, if they have one: `init.lua` or
/// `init.vim` where Neovim looks for it.
pub fn own_config() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| {
        let dirs = directories::BaseDirs::new()?;
        Some(if cfg!(windows) { dirs.data_local_dir().to_path_buf() } else { dirs.home_dir().join(".config") })
    })?;
    let app = std::env::var("NVIM_APPNAME").ok().filter(|a| !a.is_empty()).unwrap_or_else(|| "nvim".into());
    ["init.lua", "init.vim"].into_iter().map(|f| base.join(&app).join(f)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::super::Package;
    use super::*;

    #[test]
    fn every_server_has_a_setup() {
        let configured: Vec<&str> = SERVERS.split("servers['").skip(1).filter_map(|s| s.split("']").next()).collect();
        let named: Vec<&str> = Package::ALL.iter().flat_map(|p| p.server_names()).copied().collect();
        for name in &named {
            assert!(configured.contains(name), "servers.lua has no {name}");
        }
        for name in &configured {
            assert!(named.contains(name), "servers.lua sets up {name}, which no package installs");
        }
    }

    #[test]
    fn files_are_rewritten_only_when_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("init.lua");
        write_if_changed(&file, "a").unwrap();
        let first = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_if_changed(&file, "a").unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), first);
        write_if_changed(&file, "b").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "b");
        assert!(!dir.path().join("init.part").exists());
    }
}
