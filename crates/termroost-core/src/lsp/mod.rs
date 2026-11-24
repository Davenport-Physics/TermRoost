//! Language servers: which one serves a file, whether it is installed, and
//! installing it, so Neovim (see [`nvim`]) can complete, check and navigate
//! code without any setup of the user's own.
//!
//! Servers are installed into Terminal Rooster's own folder with what the
//! computer already has: npm for the ones written for Node, `dotnet tool`
//! for Roslyn, rustup for rust-analyzer. Node, .NET and Rust themselves are
//! never installed; without them a language says what it needs.

mod install;
pub mod nvim;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use install::install;

use crate::session::bundle;
use crate::session::shell::find_program;

/// What is installed together: one or more servers for some languages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Package {
    /// rust-analyzer, checking with Clippy; from rustup.
    Rust,
    /// Microsoft's Roslyn server, the one VS Code's C# extension runs.
    CSharp,
    /// TypeScript 7's own server, also for JavaScript and React.
    TypeScript,
    /// VS Code's servers for HTML, CSS and JSON, and its ESLint server.
    Web,
    Tailwind,
}

impl Package {
    pub const ALL: [Package; 5] =
        [Package::Rust, Package::CSharp, Package::TypeScript, Package::Web, Package::Tailwind];

    /// The languages, for the user.
    pub fn name(self) -> &'static str {
        match self {
            Package::Rust => "Rust",
            Package::CSharp => "C#",
            Package::TypeScript => "JavaScript and TypeScript",
            Package::Web => "HTML, CSS, JSON and ESLint",
            Package::Tailwind => "Tailwind CSS",
        }
    }

    /// The servers, for the user.
    pub fn servers(self) -> &'static str {
        match self {
            Package::Rust => "rust-analyzer and Clippy, with rustup",
            Package::CSharp => "Roslyn, with dotnet",
            Package::TypeScript => "TypeScript 7, with npm",
            Package::Web => "VS Code's language servers, with npm",
            Package::Tailwind => "Tailwind CSS IntelliSense, with npm",
        }
    }

    /// Its servers' names, which `servers.lua` in the Neovim setup knows.
    pub fn server_names(self) -> &'static [&'static str] {
        match self {
            Package::Rust => &["rust-analyzer"],
            Package::CSharp => &["roslyn"],
            Package::TypeScript => &["typescript"],
            Package::Web => &["html", "css", "json", "eslint"],
            Package::Tailwind => &["tailwind"],
        }
    }

    /// Its folder in [`dir`]; rust-analyzer lives with the Rust toolchain.
    fn folder(self) -> Option<&'static str> {
        match self {
            Package::Rust => None,
            Package::CSharp => Some("roslyn"),
            Package::TypeScript => Some("typescript"),
            Package::Web => Some("vscode"),
            Package::Tailwind => Some("tailwind"),
        }
    }

    /// What is installed: an npm package spec, or Roslyn's version.
    fn pinned(self) -> &'static str {
        match self {
            Package::Rust => "rustup",
            Package::CSharp => ROSLYN_VERSION,
            Package::TypeScript => "typescript@7.0.2",
            Package::Web => "vscode-langservers-extracted@4.10.0",
            Package::Tailwind => "@tailwindcss/language-server@0.16.0",
        }
    }
}

/// The `roslyn-language-server` dotnet tool's version.
const ROSLYN_VERSION: &str = "5.13.0-1.26514.10";
/// Roslyn runs on .NET 10, and its tool package needs the .NET 10 SDK.
const DOTNET_MAJOR: u32 = 10;
/// The servers that run on Node need Node 18 or newer.
const NODE_MAJOR: u32 = 18;

/// Written last into a package's folder: an install that stopped half way
/// has none. Holds what was installed ([`Package::pinned`]).
const DONE_FILE: &str = ".terminalrooster-installed";

/// Where Terminal Rooster's language servers live:
/// `%LOCALAPPDATA%\TerminalRooster\data\lsp` (or `lsp` in
/// `TERMINALROOSTER_DATA_DIR`, or the local data directory elsewhere).
pub fn dir() -> Option<PathBuf> {
    bundle::local_data_subdir("lsp")
}

fn package_dir(p: Package) -> Option<PathBuf> {
    Some(dir()?.join(p.folder()?))
}

/// What installed `p` holds, if it finished.
fn installed_spec(p: Package) -> Option<String> {
    std::fs::read_to_string(package_dir(p)?.join(DONE_FILE)).ok().map(|s| s.trim().to_owned())
}

/// The packages serving file `path`: by its kind, and for ESLint and
/// Tailwind also whether its project uses them.
pub fn for_file(path: &Path) -> Vec<Package> {
    let ext = path.extension().and_then(OsStr::to_str).map(str::to_ascii_lowercase).unwrap_or_default();
    let script = matches!(ext.as_str(), "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts");
    let markup = matches!(ext.as_str(), "html" | "htm" | "css" | "scss" | "less" | "vue" | "svelte" | "astro");
    let mut found = Vec::new();
    match ext.as_str() {
        "rs" => found.push(Package::Rust),
        "cs" | "csx" => found.push(Package::CSharp),
        "html" | "htm" | "css" | "scss" | "less" | "json" | "jsonc" => found.push(Package::Web),
        _ if script => found.push(Package::TypeScript),
        _ => {}
    }
    let dir = path.parent().unwrap_or(path);
    if script && project_has(dir, |d| ESLINT_CONFIGS.iter().any(|f| d.join(f).is_file())) {
        found.push(Package::Web);
    }
    if (script || markup) && project_has(dir, uses_tailwind) {
        found.push(Package::Tailwind);
    }
    found
}

/// ESLint's configuration files (as the setup's `servers.lua` knows them).
const ESLINT_CONFIGS: [&str; 12] = [
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yaml",
    ".eslintrc.yml",
];

/// A Tailwind configuration, or a `package.json` that brings Tailwind in
/// (Tailwind 4 needs no configuration file).
fn uses_tailwind(d: &Path) -> bool {
    ["tailwind.config.js", "tailwind.config.cjs", "tailwind.config.mjs", "tailwind.config.ts"]
        .iter()
        .any(|f| d.join(f).is_file())
        || std::fs::read_to_string(d.join("package.json")).is_ok_and(|s| s.contains("tailwindcss"))
}

/// Whether `has` holds for `dir` or a folder above it, up to the top of
/// its git repository.
fn project_has(dir: &Path, has: impl Fn(&Path) -> bool) -> bool {
    for d in dir.ancestors() {
        if has(d) {
            return true;
        }
        if d.join(".git").exists() {
            break;
        }
    }
    false
}

/// Where a package stands on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Ready to use; `outdated` when Terminal Rooster now installs a newer
    /// version.
    Installed { outdated: bool },
    /// Not installed, and installing it works.
    Missing,
    /// It can't be installed or run: what the computer lacks.
    Needs(&'static str),
}

/// The programs packages are installed and run with.
#[derive(Clone, Debug, Default)]
struct Runtimes {
    node: Option<PathBuf>,
    node_major: Option<u32>,
    npm: Option<PathBuf>,
    dotnet: Option<PathBuf>,
    dotnet_sdk_major: Option<u32>,
    rustup: Option<PathBuf>,
}

impl Runtimes {
    /// Finds them on PATH and asks Node and .NET for their versions, which
    /// takes a moment: run it on a thread.
    fn detect() -> Runtimes {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let node = find_program("node", &path);
        let node_major = node.as_deref().and_then(|n| output(n, &["--version"])).and_then(|v| node_major(&v));
        let dotnet = find_program("dotnet", &path);
        let dotnet_sdk_major = dotnet.as_deref().and_then(|d| output(d, &["--list-sdks"])).and_then(|s| sdk_major(&s));
        Runtimes {
            node,
            node_major,
            npm: find_npm(&path),
            dotnet,
            dotnet_sdk_major,
            rustup: find_program("rustup", &path),
        }
    }

    /// Where `p` stands, `installed` being what is installed of it
    /// ([`Package::pinned`]'s value when it was installed).
    fn state(&self, p: Package, installed: Option<&str>) -> State {
        let node_ok = self.node.is_some() && self.node_major.is_some_and(|m| m >= NODE_MAJOR);
        let can_install = match p {
            Package::Rust => self.rustup.is_some(),
            Package::CSharp => self.dotnet_sdk_major.is_some_and(|m| m >= DOTNET_MAJOR),
            Package::TypeScript => self.npm.is_some(),
            Package::Web | Package::Tailwind => self.npm.is_some() && node_ok,
        };
        let runs = match p {
            Package::Web | Package::Tailwind => node_ok,
            _ => true,
        };
        if let Some(installed) = installed.filter(|_| runs) {
            return State::Installed { outdated: installed != p.pinned() && can_install };
        }
        if can_install {
            return State::Missing;
        }
        State::Needs(match p {
            Package::Rust => "Rust, installed with rustup",
            Package::CSharp => "the .NET 10 SDK",
            Package::TypeScript => "Node.js",
            Package::Web | Package::Tailwind => "Node.js 18 or newer",
        })
    }
}

/// Where each package stands. Runs `node` and `dotnet`: call it on a thread.
pub fn status() -> BTreeMap<Package, State> {
    let rt = Runtimes::detect();
    let installed = |p: Package| match p {
        Package::Rust => rust_analyzer().map(|_| p.pinned().to_owned()),
        _ => installed_spec(p),
    };
    Package::ALL.into_iter().map(|p| (p, rt.state(p, installed(p).as_deref()))).collect()
}

/// npm: a batch file on Windows, which only starts through `cmd.exe`
/// (Rust's `Command` does that itself).
fn find_npm(path: &OsStr) -> Option<PathBuf> {
    let name = if cfg!(windows) { "npm.cmd" } else { "npm" };
    std::env::split_paths(path).filter(|d| d.is_absolute()).map(|d| d.join(name)).find(|p| p.is_file())
}

/// `v24.15.0` -> 24.
fn node_major(version: &str) -> Option<u32> {
    version.trim().trim_start_matches('v').split('.').next()?.parse().ok()
}

/// The newest SDK's major version in `dotnet --list-sdks` (lines such as
/// `10.0.100 [C:\Program Files\dotnet\sdk]`).
fn sdk_major(list: &str) -> Option<u32> {
    list.lines().filter_map(|l| l.split('.').next()?.trim().parse().ok()).max()
}

/// rust-analyzer, when it can run: one on PATH, unless it is rustup's
/// stand-in and no toolchain has rust-analyzer.
fn rust_analyzer() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let ra = find_program("rust-analyzer", &path)?;
    let proxy = ra.parent().is_some_and(|d| find_program("rustup", d.as_os_str()).is_some());
    (!proxy || toolchain_has("rust-analyzer")).then_some(ra)
}

/// Whether a rustup toolchain has program `name` (a component's).
fn toolchain_has(name: &str) -> bool {
    let home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join(".rustup")));
    let Some(Ok(toolchains)) = home.map(|h| std::fs::read_dir(h.join("toolchains"))) else { return false };
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    toolchains.flatten().any(|t| t.path().join("bin").join(&exe).is_file())
}

/// How Neovim (and, later, the built-in editor) runs one server.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Server {
    pub cmd: Vec<String>,
    /// Settings Terminal Rooster chooses, merged into the server's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Value>,
}

/// The installed servers that can run, by the names the setup's
/// `servers.lua` gives them. Only looks at files: fine on the UI thread.
pub fn servers() -> BTreeMap<&'static str, Server> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let node = find_program("node", &path);
    let mut found = BTreeMap::new();
    let cmd = |program: &Path, args: &[&str]| -> Server {
        let cmd = std::iter::once(program.display().to_string()).chain(args.iter().map(|a| (*a).to_owned())).collect();
        Server { cmd, settings: None }
    };
    if let Some(ra) = rust_analyzer() {
        let mut server = cmd(&ra, &[]);
        if toolchain_has("cargo-clippy") {
            server.settings = Some(serde_json::json!({ "rust-analyzer": { "check": { "command": "clippy" } } }));
        }
        found.insert("rust-analyzer", server);
    }
    for p in [Package::CSharp, Package::TypeScript, Package::Web, Package::Tailwind] {
        let Some(dir) = installed_spec(p).and(package_dir(p)) else { continue };
        for (name, file) in p.server_names().iter().zip(entry_points(p, &dir)) {
            let server = match p {
                Package::CSharp => cmd(&file, &["--stdio", "--autoLoadProjects", "--telemetryLevel", "off"]),
                Package::TypeScript => cmd(&file, &["--lsp", "--stdio"]),
                _ => match &node {
                    Some(node) => cmd(node, &[&file.display().to_string(), "--stdio"]),
                    None => continue,
                },
            };
            found.insert(*name, server);
        }
    }
    found
}

/// The programs (or, for Node, the scripts) package `p` installed into
/// `dir` runs its servers with, in the order of [`Package::server_names`].
fn entry_points(p: Package, dir: &Path) -> Vec<PathBuf> {
    let modules = dir.join("node_modules");
    match p {
        Package::Rust => Vec::new(),
        Package::CSharp => vec![roslyn_exe(dir)],
        Package::TypeScript => vec![tsc_exe(dir)],
        Package::Web => ["html", "css", "json", "eslint"]
            .map(|s| {
                modules.join("vscode-langservers-extracted").join("bin").join(format!("vscode-{s}-language-server"))
            })
            .into(),
        Package::Tailwind => {
            vec![modules.join("@tailwindcss").join("language-server").join("bin").join("tailwindcss-language-server")]
        }
    }
}

/// Roslyn's program in its folder `dir` (see [`install`] for how it gets
/// there).
fn roslyn_exe(dir: &Path) -> PathBuf {
    dir.join(format!("roslyn-language-server{}", std::env::consts::EXE_SUFFIX))
}

/// The first file named `name` in `dir` or its folders, `depth` deep.
fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in &entries {
        let path = entry.path();
        if entry.file_name() == name && path.is_file() {
            return Some(path);
        }
    }
    let folders = entries.iter().filter(|e| e.file_type().is_ok_and(|t| t.is_dir()));
    folders.filter(|_| depth > 1).find_map(|e| find_file(&e.path(), name, depth - 1))
}

/// TypeScript 7's compiler, which is also its language server: a native
/// program in the npm package for this platform.
fn tsc_exe(dir: &Path) -> PathBuf {
    let os = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    dir.join("node_modules")
        .join("@typescript")
        .join(format!("typescript-{os}-{arch}"))
        .join("lib")
        .join(format!("tsc{}", std::env::consts::EXE_SUFFIX))
}

/// What `program args` prints, when it succeeds.
fn output(program: &Path, args: &[&str]) -> Option<String> {
    let out = install::quiet(program).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_served_by_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let at = |name: &str| for_file(&dir.path().join(name));
        assert_eq!(at("main.rs"), [Package::Rust]);
        assert_eq!(at("Program.cs"), [Package::CSharp]);
        assert_eq!(at("App.tsx"), [Package::TypeScript]);
        assert_eq!(at("index.HTML"), [Package::Web]);
        assert_eq!(at("package.json"), [Package::Web]);
        assert!(at("notes.txt").is_empty() && at("Makefile").is_empty());
    }

    #[test]
    fn eslint_and_tailwind_come_with_projects_that_use_them() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("app");
        let src = repo.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        // Above the repository: not this project's.
        std::fs::write(dir.path().join("eslint.config.js"), "").unwrap();
        assert_eq!(for_file(&src.join("App.tsx")), [Package::TypeScript]);

        std::fs::write(repo.join("eslint.config.mjs"), "").unwrap();
        std::fs::write(repo.join("package.json"), r#"{"devDependencies":{"tailwindcss":"^4"}}"#).unwrap();
        assert_eq!(for_file(&src.join("App.tsx")), [Package::TypeScript, Package::Web, Package::Tailwind]);
        assert_eq!(for_file(&src.join("app.css")), [Package::Web, Package::Tailwind]);
        assert_eq!(for_file(&src.join("data.json")), [Package::Web], "no Tailwind in JSON");
    }

    #[test]
    fn versions_are_read_from_node_and_dotnet() {
        assert_eq!(node_major("v24.15.0\n"), Some(24));
        assert_eq!(node_major("garbage"), None);
        let sdks = "8.0.405 [C:\\Program Files\\dotnet\\sdk]\n10.0.100 [C:\\Program Files\\dotnet\\sdk]\n9.0.311 [x]\n";
        assert_eq!(sdk_major(sdks), Some(10));
        assert_eq!(sdk_major(""), None);
    }

    #[test]
    fn what_a_computer_lacks_is_named() {
        let none = Runtimes::default();
        assert_eq!(none.state(Package::CSharp, None), State::Needs("the .NET 10 SDK"));
        assert_eq!(none.state(Package::Rust, None), State::Needs("Rust, installed with rustup"));
        let old = Runtimes { dotnet_sdk_major: Some(9), node_major: Some(16), ..Runtimes::default() };
        assert_eq!(old.state(Package::CSharp, None), State::Needs("the .NET 10 SDK"));
        let npm = Runtimes { npm: Some("npm".into()), node: Some("node".into()), node_major: Some(16), ..old };
        assert_eq!(npm.state(Package::Web, None), State::Needs("Node.js 18 or newer"));
        assert_eq!(npm.state(Package::TypeScript, None), State::Missing, "TypeScript 7 doesn't run on Node");
        let web = Package::Web.pinned();
        assert_eq!(npm.state(Package::Web, Some(web)), State::Needs("Node.js 18 or newer"), "installed, can't run");
    }

    #[test]
    fn an_older_install_can_be_updated() {
        let rt = Runtimes { dotnet_sdk_major: Some(10), ..Runtimes::default() };
        let now = Package::CSharp.pinned();
        assert_eq!(rt.state(Package::CSharp, Some(now)), State::Installed { outdated: false });
        assert_eq!(rt.state(Package::CSharp, Some("5.0.0")), State::Installed { outdated: true });
        let no_sdk = Runtimes::default();
        assert_eq!(no_sdk.state(Package::CSharp, Some("5.0.0")), State::Installed { outdated: false }, "can't update");
        assert_eq!(rt.state(Package::CSharp, None), State::Missing);
    }

    #[test]
    fn typescript_runs_its_native_compiler() {
        let exe = tsc_exe(Path::new("ts"));
        let rel: Vec<_> = exe.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        assert_eq!(rel[..3], ["ts", "node_modules", "@typescript"]);
        assert!(rel[3].starts_with("typescript-") && rel[4] == "lib", "{rel:?}");
    }
}
