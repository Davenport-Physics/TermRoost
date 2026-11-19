//! Build automation for TerminalRooster.
//!
//! ```text
//! cargo xtask package [deb|rpm|all|windows]   build release packages into dist/
//! cargo xtask remote                           build the static helper for remote hosts
//! ```
//!
//! `remote` builds `terminalrooster-remote` for Linux with musl, linked
//! statically, so it runs on any Linux host of this processor whatever its C
//! library (`target/<processor>-unknown-linux-musl/release/`, where Terminal
//! Rooster finds it to copy to hosts). It needs the Rust target
//! (`rustup target add x86_64-unknown-linux-musl`) and a C compiler for the
//! bundled SQLite: `musl-gcc` (Debian/Ubuntu `musl-tools`), else `clang`.
//!
//! With no format, Windows builds the installer and other systems build the
//! `.deb` and `.rpm` (`all`).
//!
//! Linux packaging builds the release binary and the static helper for remote
//! hosts, stages every installed file under `target/release/packaging-root/`
//! (binary, helper, desktop entry, AppStream metadata and the hicolor icons
//! from `packaging/icons/`), then runs `cargo deb`
//! and `cargo generate-rpm`, whose file lists live in
//! `crates/termroost-iced/Cargo.toml`.
//!
//! The Windows installer is built by NSIS from `packaging/windows/terminalrooster.nsi`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const APP_ID: &str = "io.github.Davenport_Physics.TerminalRooster";
/// hicolor sizes shipped; each comes from `packaging/icons/terminalrooster-<size>.png`.
const ICON_SIZES: [u32; 10] = [16, 24, 32, 48, 64, 96, 128, 192, 256, 512];

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["package"] if cfg!(windows) => package_windows(),
        ["package"] | ["package", "all"] => package(true, true),
        ["package", "deb"] => package(true, false),
        ["package", "rpm"] => package(false, true),
        ["package", "windows"] => package_windows(),
        ["remote"] => build_remote().map(drop),
        _ => {
            eprintln!("usage: cargo xtask package [deb|rpm|all|windows] | cargo xtask remote");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("workspace root").to_path_buf()
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root());
    cmd
}

fn run(cmd: &mut Command) -> Result<()> {
    eprintln!("$ {cmd:?}");
    let status = cmd.status().map_err(|e| format!("could not run {:?}: {e}", cmd.get_program()))?;
    if status.success() { Ok(()) } else { Err(format!("{:?} failed ({status})", cmd.get_program())) }
}

/// Fails early with install instructions when a cargo subcommand is missing.
fn require_tool(subcommand: &str, crate_name: &str) -> Result<()> {
    let ok = cargo().args([subcommand, "--version"]).output().is_ok_and(|o| o.status.success());
    if ok {
        Ok(())
    } else {
        Err(format!("`cargo {subcommand}` is not installed; run: cargo install --locked {crate_name}"))
    }
}

/// The workspace version, from `[workspace.package]`.
fn version() -> Result<String> {
    let text = std::fs::read_to_string(root().join("Cargo.toml")).map_err(|e| e.to_string())?;
    let section = text.split("[workspace.package]").nth(1).ok_or("no [workspace.package] in Cargo.toml")?;
    section
        .lines()
        .find_map(|l| l.trim().strip_prefix("version = \"").and_then(|v| v.strip_suffix('"')))
        .map(str::to_owned)
        .ok_or_else(|| "no version in [workspace.package]".into())
}

fn package(deb: bool, rpm: bool) -> Result<()> {
    if deb {
        require_tool("deb", "cargo-deb")?;
    }
    if rpm {
        require_tool("generate-rpm", "cargo-generate-rpm")?;
    }
    let root = root();
    let version = version()?;
    let metainfo = root.join(format!("packaging/linux/{APP_ID}.metainfo.xml"));
    let metainfo_text = std::fs::read_to_string(&metainfo).map_err(|e| format!("{}: {e}", metainfo.display()))?;
    if !metainfo_text.contains(&format!("<release version=\"{version}\"")) {
        return Err(format!(
            "{} has no <release version=\"{version}\"> entry; add one for this version",
            metainfo.display()
        ));
    }

    run(cargo().args(["build", "--release", "--locked", "-p", "termroost-iced"]))?;
    let helper = build_remote()?;
    stage(&root, &helper)?;

    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).map_err(|e| e.to_string())?;
    if deb {
        // Already stripped by the release profile (debug info only; symbols kept).
        run(cargo().args(["deb", "-p", "termroost-iced", "--no-build", "--no-strip", "--output"]).arg(&dist))?;
    }
    if rpm {
        run(cargo().args(["generate-rpm", "-p", "crates/termroost-iced", "--output"]).arg(&dist))?;
    }
    eprintln!("\npackages in {}:", dist.display());
    let mut files: Vec<_> = std::fs::read_dir(&dist).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files.iter().filter(|f| f.to_string_lossy().contains(&version)) {
        eprintln!("  {}", f.display());
    }
    Ok(())
}

/// Builds `dist/TerminalRooster-<version>-<arch>-setup.exe` with NSIS.
fn package_windows() -> Result<()> {
    if !cfg!(windows) {
        return Err("the Windows installer packages the native terminalrooster.exe; build it on Windows".into());
    }
    let makensis = find_makensis()?;
    let root = root();
    let version = version()?;
    let quad = version_quad(&version)?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };

    run(cargo().args(["build", "--release", "--locked", "-p", "termroost-iced"]))?;

    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).map_err(|e| e.to_string())?;
    let out = dist.join(format!("TerminalRooster-{version}-{arch}-setup.exe"));
    run(Command::new(&makensis)
        .arg("/V2")
        .arg(format!("/DVERSION={version}"))
        .arg(format!("/DVERSION_QUAD={quad}"))
        .arg(format!("/DROOT={}", root.display()))
        .arg(format!("/DOUTFILE={}", out.display()))
        .arg(root.join("packaging/windows/terminalrooster.nsi")))?;
    eprintln!("\ninstaller: {}", out.display());
    Ok(())
}

/// `makensis` from `MAKENSIS`, `PATH` or the default NSIS install folders.
fn find_makensis() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MAKENSIS") {
        return Ok(path.into());
    }
    if Command::new("makensis").arg("/VERSION").output().is_ok_and(|o| o.status.success()) {
        return Ok("makensis".into());
    }
    ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|dir| Path::new(&dir).join("NSIS").join("makensis.exe"))
        .find(|p| p.is_file())
        .ok_or_else(|| "NSIS is not installed; run `winget install NSIS.NSIS` or set MAKENSIS to makensis.exe".into())
}

/// Windows version resources need four numbers: `0.2.0-beta.1` becomes `0.2.0.0`.
fn version_quad(version: &str) -> Result<String> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.parse::<u16>().is_err()) {
        return Err(format!("version {version} is not MAJOR.MINOR.PATCH"));
    }
    Ok(format!("{core}.0"))
}

/// Builds the helper for remote hosts statically for Linux with musl (see the
/// module docs) and returns where it is.
fn build_remote() -> Result<PathBuf> {
    let arch = std::env::consts::ARCH;
    let triple = format!("{arch}-unknown-linux-musl");
    let installed = Command::new("rustup").args(["target", "list", "--installed"]).output();
    if installed.is_ok_and(|o| !String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim() == triple)) {
        return Err(format!("the Rust target {triple} is missing; run: rustup target add {triple}"));
    }
    let on_path = |tool: &str| Command::new(tool).arg("--version").output().is_ok_and(|o| o.status.success());
    let env_key = triple.replace('-', "_");
    let mut build = cargo();
    build.args(["build", "--release", "--locked", "-p", "termroost-remote", "--target", &triple]);
    if !on_path("musl-gcc") {
        if !on_path("clang") {
            return Err("no C compiler for musl: install musl-gcc (musl-tools) or clang".into());
        }
        // clang builds SQLite against this system's headers: without asking
        // for the `open64`-style names, which musl doesn't have (and 64-bit
        // Linux doesn't need).
        build.env(format!("CC_{env_key}"), "clang");
        build.env(format!("CFLAGS_{env_key}"), "-DSQLITE_DISABLE_LFS");
        if on_path("llvm-ar") {
            build.env(format!("AR_{env_key}"), "llvm-ar");
        }
    }
    run(&mut build)?;
    let built = root().join("target").join(&triple).join("release/terminalrooster-remote");
    eprintln!("static helper for remote hosts: {}", built.display());
    Ok(built)
}

/// Lays out every installed file under `target/release/packaging-root`.
/// `helper`: the static helper for remote hosts.
fn stage(root: &Path, helper: &Path) -> Result<()> {
    let stage = root.join("target/release/packaging-root");
    if stage.exists() {
        std::fs::remove_dir_all(&stage).map_err(|e| e.to_string())?;
    }
    let copy = |from: PathBuf, to: &str| -> Result<()> {
        let to = stage.join(to);
        std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::copy(&from, &to).map_err(|e| format!("copying {}: {e}", from.display()))?;
        Ok(())
    };
    let exe = if cfg!(windows) { "terminalrooster.exe" } else { "terminalrooster" };
    copy(root.join("target/release").join(exe), "usr/bin/terminalrooster")?;
    // Where Terminal Rooster looks for it (see termroost_core::remote::bootstrap).
    let arch = std::env::consts::ARCH;
    copy(helper.to_path_buf(), &format!("usr/lib/terminalrooster/terminalrooster-remote-linux-{arch}"))?;
    copy(root.join(format!("packaging/linux/{APP_ID}.desktop")), &format!("usr/share/applications/{APP_ID}.desktop"))?;
    copy(
        root.join(format!("packaging/linux/{APP_ID}.metainfo.xml")),
        &format!("usr/share/metainfo/{APP_ID}.metainfo.xml"),
    )?;

    for size in ICON_SIZES {
        let src = root.join(format!("packaging/icons/terminalrooster-{size}.png"));
        let icon = image::open(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        if (icon.width(), icon.height()) != (size, size) {
            return Err(format!("{} is {}x{}, expected {size}x{size}", src.display(), icon.width(), icon.height()));
        }
        copy(src, &format!("usr/share/icons/hicolor/{size}x{size}/apps/{APP_ID}.png"))?;
    }
    eprintln!("staged files in {}", stage.display());
    Ok(())
}
