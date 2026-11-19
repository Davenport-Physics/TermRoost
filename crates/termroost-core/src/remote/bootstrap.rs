//! Starting Terminal Rooster's helper on a host over SSH, and copying it
//! there first when it isn't.
//!
//! The helper lives in `~/.terminalrooster/bin` on the host, named after
//! its version and contents, so each build gets a copy of its own (the few
//! used last are kept: computers on different versions don't copy theirs
//! over each other's every time). It is copied from this computer: a static
//! build for Linux when there is one (see [`local_binaries`]), else the
//! `terminalrooster-remote` program next to this one, else this program
//! itself, which can be the helper too. A host gets the one built for its
//! system and processor.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::client::StderrTail;
use super::wire;
use crate::host::HostCommand;

/// The program `ssh`, unless this names another (for tests, or an `ssh`
/// that isn't on `PATH`).
pub const SSH_ENV: &str = "TERMINALROOSTER_SSH";
/// A helper to copy to hosts instead of the one found next to this program.
pub const BINARY_ENV: &str = "TERMINALROOSTER_REMOTE_BINARY";
/// The argument that makes Terminal Rooster's programs the remote helper.
pub const REMOTE_ARG: &str = "remote";
/// The helper's name on hosts, before its version.
const NAME: &str = "terminalrooster-remote";
/// How many builds of the helper a host keeps, those used last.
const KEEP_BUILDS: usize = 4;
/// How long connecting, SSH included, may take.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);
/// How long copying the helper may take.
const COPY_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A process started to serve the protocol, its output read up to the
/// line that starts it.
pub(crate) struct Launch {
    pub child: Arc<Mutex<Option<Child>>>,
    pub stdout: BufReader<ChildStdout>,
    pub stdin: ChildStdin,
    pub stderr: StderrTail,
    pub watchdog: Watchdog,
}

/// What came of reading a launch's output.
enum Started {
    /// The helper started; its frames come next.
    Serving,
    /// It isn't on the host, which is this system and processor (`uname -sm`).
    Missing(String),
    /// The output ended first.
    Ended,
}

impl Launch {
    fn start(mut command: Command, host: &str, timeout: Duration) -> Result<Launch, String> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        no_window(&mut command);
        let mut child = command.spawn().map_err(|e| format!("could not start {:?}: {e}", command.get_program()))?;
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        let stdin = child.stdin.take().expect("piped");
        let stderr = StderrTail::follow(host, child.stderr.take().expect("piped"));
        let child = Arc::new(Mutex::new(Some(child)));
        let watchdog = Watchdog::arm(child.clone(), timeout);
        Ok(Launch { child, stdout, stdin, stderr, watchdog })
    }

    /// Reads lines until the helper says it started, or the launch script
    /// that it isn't there. Login scripts may print anything before.
    fn started(&mut self) -> Started {
        let mut line = Vec::new();
        loop {
            line.clear();
            match self.stdout.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return Started::Ended,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if text.starts_with(wire::MAGIC) {
                return Started::Serving;
            }
            if let Some(uname) = text.strip_prefix(wire::MISSING) {
                return Started::Missing(uname.trim().to_owned());
            }
        }
    }

    fn kill(&self) {
        end_child(&self.child);
    }
}

/// Ends a process and waits for it, so it doesn't linger as a zombie.
pub(crate) fn end_child(child: &Mutex<Option<Child>>) {
    if let Some(mut child) = child.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Kills a process that takes too long, unless [`Watchdog::finish`]ed first.
pub(crate) struct Watchdog(Arc<AtomicBool>);

impl Watchdog {
    fn arm(child: Arc<Mutex<Option<Child>>>, timeout: Duration) -> Watchdog {
        let done = Arc::new(AtomicBool::new(false));
        let watching = done.clone();
        let _ = std::thread::Builder::new().name("remote-watchdog".into()).spawn(move || {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if watching.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if !watching.load(Ordering::Acquire)
                && let Some(child) = child.lock().unwrap().as_mut()
            {
                tracing::warn!("a remote connection took too long to start");
                let _ = child.kill();
            }
        });
        Watchdog(done)
    }

    pub fn finish(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Starts a command that serves the protocol itself (see [`super::Transport::Command`]).
pub(crate) fn launch_command(cmd: &HostCommand, host: &str) -> Result<Launch, String> {
    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args).envs(cmd.env.iter().map(|(k, v)| (k, v)));
    let mut launch = Launch::start(command, host, LAUNCH_TIMEOUT)?;
    match launch.started() {
        Started::Serving => Ok(launch),
        _ => {
            launch.kill();
            Err(explain(host, &launch.stderr.lines(), "the helper did not start"))
        }
    }
}

/// Starts the helper on `host` over SSH, copying it there first if it
/// isn't. `copying` is called before copying.
pub(crate) fn launch(host: &str, copying: impl FnOnce()) -> Result<Launch, String> {
    check_host(host)?;
    let binaries = local_binaries();
    let mut launch = Launch::start(ssh(host, &launch_script(binaries)), host, LAUNCH_TIMEOUT)?;
    let uname = match launch.started() {
        Started::Serving => return Ok(launch),
        Started::Missing(uname) => uname,
        Started::Ended => {
            launch.kill();
            return Err(explain(host, &launch.stderr.lines(), "the connection closed"));
        }
    };
    launch.kill();
    if binaries.is_empty() {
        return Err(format!(
            "Terminal Rooster's helper isn't on {host}, and there is none here to copy there. Install \
             terminalrooster-remote there (on PATH)."
        ));
    }
    let Some(binary) = binaries.iter().find(|b| b.runs_on(&uname)) else {
        return Err(format!(
            "{host} is {uname}, and this computer has no Terminal Rooster helper for it. Put a \
             terminalrooster-remote built for it on PATH there."
        ));
    };
    copying();
    copy(host, binary)?;
    let mut launch = Launch::start(ssh(host, &launch_script(std::slice::from_ref(binary))), host, LAUNCH_TIMEOUT)?;
    match launch.started() {
        Started::Serving => Ok(launch),
        _ => {
            launch.kill();
            Err(explain(host, &launch.stderr.lines(), "the helper copied there did not start"))
        }
    }
}

/// Copies the helper to the host, whole or not at all (its size is checked
/// before it gets its name), and deletes builds there not used for longest
/// (see [`KEEP_BUILDS`]; the launch script touches the one it starts).
fn copy(host: &str, binary: &Binary) -> Result<(), String> {
    let name = &binary.name;
    let mut file =
        std::fs::File::open(&binary.path).map_err(|e| format!("can't read {}: {e}", binary.path.display()))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let keep = KEEP_BUILDS + 1;
    let script = format!(
        r#"exec sh -c 'set -e; d="$HOME/.terminalrooster/bin"; mkdir -p "$d"; t="$d/.upload-$$"; trap "rm -f \"$t\"" EXIT; cat > "$t"; [ "$(wc -c < "$t" | tr -d " ")" = "{size}" ]; chmod 755 "$t"; mv -f "$t" "$d/{name}"; for f in $(ls -t "$d"/{NAME}-* | tail -n +{keep}); do rm -f "$f"; done'"#
    );
    tracing::info!(host, binary = %binary.path.display(), name, "copying the remote helper");
    let mut launch = Launch::start(ssh(host, &script), host, COPY_TIMEOUT)?;
    let sent = std::io::copy(&mut file, &mut launch.stdin).and_then(|_| launch.stdin.flush());
    drop(launch.stdin);
    let status = launch.child.lock().unwrap().take().map(|mut c| c.wait());
    launch.watchdog.finish();
    match (sent, status) {
        (Ok(()), Some(Ok(s))) if s.success() => Ok(()),
        _ => Err(explain(host, &launch.stderr.lines(), "copying Terminal Rooster's helper there failed")),
    }
}

/// `ssh` running `script` on `host`, with what SSH needs to carry binary
/// data and to notice a dead connection.
fn ssh(host: &str, script: &str) -> Command {
    let program = std::env::var_os(SSH_ENV).filter(|p| !p.is_empty()).unwrap_or_else(|| "ssh".into());
    let mut command = Command::new(program);
    command.args([
        "-T",
        "-e",
        "none",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=20",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
        "--",
        host,
        script,
    ]);
    command
}

/// What the host runs: the helper copied from one of `binaries`, if it is
/// there (it is touched, so it counts as used, see [`copy`]); else, with no
/// helper here to copy, one on its `PATH`. Otherwise it says what the host
/// is. Run by the user's login shell, whichever it is, so all of it goes to
/// `sh`.
fn launch_script(binaries: &[Binary]) -> String {
    let names: Vec<&str> = binaries.iter().map(|b| b.name.as_str()).collect();
    let fallback = match binaries {
        [] => format!("command -v {NAME} >/dev/null 2>&1 && exec {NAME} {REMOTE_ARG}; "),
        _ => String::new(),
    };
    let names = if names.is_empty() { NAME.to_owned() } else { names.join(" ") };
    format!(
        r#"exec sh -c 'for n in {names}; do b="$HOME/.terminalrooster/bin/$n"; if [ -x "$b" ]; then touch "$b" 2>/dev/null; exec "$b" {REMOTE_ARG}; fi; done; {fallback}echo; echo "{} $(uname -sm)"'"#,
        wire::MISSING
    )
}

/// Refuses a host name `ssh` would take for an option, or that holds spaces.
pub fn check_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("The host is empty.".into());
    }
    if host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{host:?} is not a host name ssh can take."));
    }
    Ok(())
}

/// Why connecting failed, from what SSH (or the helper) said last.
pub(crate) fn explain(host: &str, stderr: &[String], otherwise: &str) -> String {
    let said: Vec<&str> = stderr.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if said.iter().any(|l| l.contains("GLIBC")) {
        return format!(
            "Terminal Rooster's helper can't run on {host}: it was built for a newer Linux. Build a static one \
             (cargo xtask remote) or install a release build."
        );
    }
    if said.iter().any(|l| l.contains("Host key verification failed")) {
        return format!("{host}'s host key isn't known yet. Connect once with ssh {host} in a terminal to accept it.");
    }
    match said.last() {
        Some(last) => format!("Could not connect to {host}: {last}"),
        None => format!("Could not connect to {host}: {otherwise}."),
    }
}

/// A helper to copy to hosts.
pub(crate) struct Binary {
    pub path: PathBuf,
    /// Its name on hosts: the version and a hash of its contents.
    pub name: String,
    /// The system and processor it is for (`linux`, `x86_64`).
    os: String,
    arch: String,
}

impl Binary {
    fn new(path: PathBuf, os: &str, arch: &str) -> Option<Binary> {
        use std::hash::Hasher;
        let bytes = std::fs::read(&path).ok()?;
        let mut h = std::hash::DefaultHasher::new();
        h.write(&bytes);
        let name = format!("{NAME}-{}-{:016x}", env!("CARGO_PKG_VERSION"), h.finish());
        Some(Binary { path, name, os: os.to_owned(), arch: arch.to_owned() })
    }

    /// Whether a host whose `uname -sm` is `uname` can run it.
    fn runs_on(&self, uname: &str) -> bool {
        let mut parts = uname.split_whitespace();
        let os = match parts.next().unwrap_or("").to_lowercase().as_str() {
            "darwin" => "macos".to_owned(),
            other => other.to_owned(),
        };
        let arch = match parts.next().unwrap_or("") {
            "arm64" => "aarch64",
            "amd64" => "x86_64",
            other => other,
        };
        os == self.os && arch == self.arch
    }
}

/// The helpers this computer has to copy to hosts, one per system and
/// processor, found once: a static Linux build next to this program (as
/// `terminalrooster-remote-linux-<processor>`) or in `../lib/terminalrooster`
/// (where packages put it), or in this build's target folder (`cargo xtask
/// remote`); else `terminalrooster-remote` next to this program, else this
/// program itself, for a host like this computer.
pub(crate) fn local_binaries() -> &'static [Binary] {
    static FOUND: OnceLock<Vec<Binary>> = OnceLock::new();
    FOUND.get_or_init(|| {
        let found = find_binaries();
        for b in &found {
            tracing::info!(binary = %b.path.display(), name = %b.name, os = %b.os, arch = %b.arch, "remote helper to copy");
        }
        if found.is_empty() {
            tracing::info!("no remote helper to copy to hosts");
        }
        found
    })
}

fn find_binaries() -> Vec<Binary> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    if let Some(path) = std::env::var_os(BINARY_ENV).filter(|p| !p.is_empty()) {
        return Binary::new(PathBuf::from(path), os, arch).into_iter().collect();
    }
    let Some(exe) = std::env::current_exe().ok() else { return Vec::new() };
    let Some(dir) = exe.parent() else { return Vec::new() };
    let mut candidates: Vec<(PathBuf, &str, &str)> = Vec::new();
    for linux_arch in ["x86_64", "aarch64"] {
        let file = format!("{NAME}-linux-{linux_arch}");
        candidates.push((dir.join(&file), "linux", linux_arch));
        candidates.push((dir.join("../lib/terminalrooster").join(&file), "linux", linux_arch));
        if let Some(target) = dir.parent() {
            for profile in ["release", "debug"] {
                let built = target.join(format!("{linux_arch}-unknown-linux-musl")).join(profile).join(NAME);
                candidates.push((built, "linux", linux_arch));
            }
        }
    }
    if cfg!(unix) {
        candidates.push((dir.join(NAME), os, arch));
        candidates.push((exe.clone(), os, arch));
    }
    let mut found: Vec<Binary> = Vec::new();
    for (path, os, arch) in candidates {
        // The first for each system and processor.
        if path.is_file() && !found.iter().any(|b| b.os == os && b.arch == arch) {
            found.extend(Binary::new(path, os, arch));
        }
    }
    found
}

/// Keeps a console program from opening a window of its own on Windows.
fn no_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Where the remote helper keeps its files on its host: `~/.terminalrooster`,
/// unless `TERMINALROOSTER_REMOTE_DIR` says otherwise.
pub(crate) fn remote_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("TERMINALROOSTER_REMOTE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()).unwrap_or_default();
    home.join(".terminalrooster")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_that_look_like_options_are_refused() {
        assert!(check_host("droplet").is_ok());
        assert!(check_host("me@10.0.0.2").is_ok());
        assert!(check_host("ssh://me@example.com:2222").is_ok());
        assert!(check_host("-oProxyCommand=x").is_err());
        assert!(check_host("a b").is_err());
        assert!(check_host("").is_err());
    }

    #[test]
    fn binaries_match_hosts() {
        let b = Binary { path: "x".into(), name: "n".into(), os: "linux".into(), arch: "x86_64".into() };
        assert!(b.runs_on("Linux x86_64"));
        assert!(b.runs_on("Linux amd64"));
        assert!(!b.runs_on("Linux aarch64"));
        assert!(!b.runs_on("Darwin arm64"));
        let mac = Binary { path: "x".into(), name: "n".into(), os: "macos".into(), arch: "aarch64".into() };
        assert!(mac.runs_on("Darwin arm64"));
    }

    #[test]
    fn the_launch_script_has_no_single_quotes_inside() {
        let b = |n: &str| Binary { path: "x".into(), name: n.into(), os: "".into(), arch: "".into() };
        let two = [b("terminalrooster-remote-1-2"), b("terminalrooster-remote-1-3")];
        for script in [launch_script(&two), launch_script(&[])] {
            let inner = script.strip_prefix("exec sh -c '").and_then(|s| s.strip_suffix('\'')).unwrap();
            assert!(!inner.contains('\''), "{script}");
        }
        assert!(launch_script(&[]).contains("command -v terminalrooster-remote"));
        assert!(!launch_script(&two).contains("command -v"));
        assert!(launch_script(&two).contains("for n in terminalrooster-remote-1-2 terminalrooster-remote-1-3;"));
    }

    #[test]
    fn errors_say_what_ssh_said() {
        let said = vec!["".into(), "me@droplet: Permission denied (publickey).".into()];
        assert_eq!(
            explain("droplet", &said, "x"),
            "Could not connect to droplet: me@droplet: Permission denied (publickey)."
        );
        assert!(
            explain("d", &["/lib64/libc.so.6: version `GLIBC_2.40' not found".into()], "x").contains("newer Linux")
        );
        assert_eq!(explain("d", &[], "it closed"), "Could not connect to d: it closed.");
    }
}
