//! Installing a [`Package`] with the computer's own npm, dotnet or rustup.

use std::path::Path;
use std::process::{Command, Stdio};

use super::{DONE_FILE, Package, Runtimes, entry_points, package_dir, servers};
use crate::Error;

/// Installs `p` (again, if it was), blocking until it is done: run it on a
/// thread.
pub fn install(p: Package) -> crate::Result<()> {
    let rt = Runtimes::detect();
    match p {
        Package::Rust => {
            let rustup = rt.rustup.as_deref().ok_or(Error::NotFound("rustup"))?;
            run(quiet(rustup).args(["component", "add", "rust-analyzer", "clippy"]), "rustup")?;
            if !servers().contains_key("rust-analyzer") {
                return Err(Error::Invalid("rustup added rust-analyzer, but it doesn't run".into()));
            }
        }
        Package::CSharp => {
            let dotnet = rt.dotnet.as_deref().ok_or(Error::NotFound("dotnet"))?;
            let dir = package_dir(p).ok_or(Error::NoDataDir)?;
            let tool = dir.with_file_name("roslyn-tool");
            let result = install_roslyn(dotnet, &tool, &dir);
            let _ = std::fs::remove_dir_all(&tool);
            result?;
            finish(p, &dir)?;
        }
        Package::TypeScript | Package::Web | Package::Tailwind => {
            let npm = rt.npm.as_deref().ok_or(Error::NotFound("npm"))?;
            let dir = fresh_dir(p)?;
            // No install scripts: none of these packages needs one.
            let mut cmd = quiet(npm);
            cmd.args(["install", "--no-fund", "--no-audit", "--ignore-scripts", "--loglevel=error", "--prefix"])
                .arg(&dir)
                .arg(p.pinned())
                .current_dir(&dir);
            run(&mut cmd, "npm")?;
            finish(p, &dir)?;
        }
    }
    tracing::info!(package = ?p, "installed language support");
    Ok(())
}

/// Installs the `roslyn-language-server` dotnet tool into `tool`, then
/// moves the server's own folder to `dir`. In the tool's store it is so
/// deep (`.store/roslyn-language-server/<version>/roslyn-language-server.<rid>/<version>/tools/<tfm>/<rid>`)
/// that on Windows its path can pass the 260 characters a program may be
/// started from, and its batch file stand-in needs a shell to start.
fn install_roslyn(dotnet: &Path, tool: &Path, dir: &Path) -> crate::Result<()> {
    for d in [tool, dir] {
        if d.exists() {
            std::fs::remove_dir_all(d)?;
        }
    }
    std::fs::create_dir_all(tool)?;
    let mut cmd = quiet(dotnet);
    cmd.args(["tool", "install", "roslyn-language-server", "--version", Package::CSharp.pinned(), "--tool-path"])
        .arg(tool)
        // Not in a project, whose global.json could ask for an older SDK.
        .current_dir(tool)
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1");
    run(&mut cmd, "dotnet")?;
    let exe = format!("roslyn-language-server{}", std::env::consts::EXE_SUFFIX);
    let store = tool.join(".store").join("roslyn-language-server");
    let server = super::find_file(&store, &exe, 7).and_then(|f| f.parent().map(Path::to_path_buf));
    let server = server.ok_or_else(|| Error::Invalid(format!("the dotnet tool has no {exe}")))?;
    std::fs::rename(server, dir)?;
    Ok(())
}

/// `p`'s folder, emptied of an earlier install.
fn fresh_dir(p: Package) -> crate::Result<std::path::PathBuf> {
    let dir = package_dir(p).ok_or(Error::NoDataDir)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Marks the install done, once its servers are where they should be.
fn finish(p: Package, dir: &Path) -> crate::Result<()> {
    if let Some(missing) = entry_points(p, dir).into_iter().find(|f| !f.is_file()) {
        let name = missing.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        return Err(Error::Invalid(format!("the install has no {name}")));
    }
    std::fs::write(dir.join(DONE_FILE), p.pinned())?;
    Ok(())
}

/// Runs `cmd`; an error carries the end of what `tool` said.
fn run(cmd: &mut Command, tool: &str) -> crate::Result<()> {
    let out = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output()?;
    if out.status.success() {
        return Ok(());
    }
    let said = [&out.stderr, &out.stdout].map(|b| String::from_utf8_lossy(b).trim().to_owned());
    let said = said.iter().find(|s| !s.is_empty()).map_or("", String::as_str);
    tracing::warn!(tool, status = %out.status, output = said, "installing language support failed");
    let last = said.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    Err(Error::Invalid(if last.is_empty() {
        format!("{tool} failed ({})", out.status)
    } else {
        format!("{tool}: {last}")
    }))
}

/// `program`, started without a console window or input.
pub(super) fn quiet(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::session::shell::CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::super::find_file;

    #[test]
    fn roslyn_is_found_in_the_tool_store() {
        let dir = tempfile::tempdir().unwrap();
        let exe = format!("roslyn-language-server{}", std::env::consts::EXE_SUFFIX);
        // The tool's own package holds only its settings; the server is in
        // the one for this platform.
        let store = dir.path().join(".store").join("roslyn-language-server");
        let version = store.join("5.13.0");
        std::fs::create_dir_all(version.join("roslyn-language-server").join("5.13.0").join("tools")).unwrap();
        let rid = version.join("roslyn-language-server.win-x64").join("5.13.0");
        let bin = rid.join("tools").join("net10.0").join("win-x64");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(&exe), "").unwrap();
        assert_eq!(find_file(&store, &exe, 7), Some(bin.join(&exe)));
        assert_eq!(find_file(&store, &exe, 6), None, "not that deep");
    }
}
