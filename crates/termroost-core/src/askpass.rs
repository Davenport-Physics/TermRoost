//! Passwords and passphrases git asks for, answered in the app.
//!
//! git (for HTTPS logins) and ssh (for key passphrases, passwords and new
//! host keys) run without a terminal, so they ask an "askpass" program
//! instead. Once a front end calls [`enable`], `GIT_ASKPASS` and
//! `SSH_ASKPASS` point at the app's own executable, which [`run_helper`]
//! turns into a small client: it hands the question to the app over a
//! loopback connection (with a token only git's environment knows), the app
//! shows it (see [`Prompt`]), and the answer goes back to git or ssh on the
//! helper's standard output.
//!
//! Only git commands that run off the UI thread may ask (see
//! `git::let_ask`): a question asked while the UI thread waits for git
//! could never be shown. The others fail instead of asking, as before.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::host::proto::{new_token, token_matches};

/// Where the helper finds the app (`127.0.0.1:<port>`).
const ADDR_ENV: &str = "TERMINALROOSTER_ASKPASS";
const TOKEN_ENV: &str = "TERMINALROOSTER_ASKPASS_TOKEN";
/// Longer requests are taken for garbage.
const MAX_REQUEST: u64 = 64 << 10;

struct Broker {
    helper: PathBuf,
    addr: SocketAddr,
    token: String,
}

static BROKER: OnceLock<Broker> = OnceLock::new();

/// A question from git or ssh, waiting for an answer. Answer it with
/// [`Prompt::answer`] or [`Prompt::cancel`]; one dropped unanswered (every
/// copy of it) counts as cancelled.
#[derive(Clone, Debug)]
pub struct Prompt {
    /// Tells prompts apart (they come one after another).
    pub id: u64,
    /// The question as git or ssh put it; may span lines (a new host key).
    pub text: String,
    pub kind: PromptKind,
    reply: async_channel::Sender<Option<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// A password or passphrase: not shown as it is typed.
    Secret,
    /// Shown as typed: a user name.
    Text,
    /// Yes or no: connecting to a host whose key is new, or allowing a key's use.
    YesNo,
}

impl Prompt {
    /// Sends `answer` back (for [`PromptKind::YesNo`], any answer means yes).
    pub fn answer(&self, answer: String) {
        let answer = if self.kind == PromptKind::YesNo { "yes".to_owned() } else { answer };
        let _ = self.reply.try_send(Some(answer));
    }

    /// Answers no, or doesn't answer: git or ssh stop asking and fail.
    pub fn cancel(&self) {
        let _ = self.reply.try_send(None);
    }

    /// Whether ssh asks before connecting to a host whose key it doesn't know.
    pub fn is_new_host(&self) -> bool {
        self.kind == PromptKind::YesNo && self.text.contains("authenticity of host")
    }

    /// A short title for the question.
    pub fn title(&self) -> &'static str {
        let text = self.text.to_ascii_lowercase();
        match self.kind {
            PromptKind::YesNo if self.is_new_host() => "Connect to a new host?",
            PromptKind::YesNo => "Allow?",
            PromptKind::Text => "Git needs a user name",
            PromptKind::Secret if text.contains("passphrase") => "SSH key passphrase",
            PromptKind::Secret => "Git needs a password",
        }
    }
}

/// What a question is, from its text and the hint ssh gives in
/// `SSH_ASKPASS_PROMPT`.
fn kind_of(text: &str, hint: Option<&str>) -> PromptKind {
    let t = text.trim();
    if hint == Some("confirm") || t.contains("(yes/no") {
        PromptKind::YesNo
    } else if t.starts_with("Username") {
        PromptKind::Text
    } else {
        // Anything else may be a secret: don't show it as typed.
        PromptKind::Secret
    }
}

/// Lets git commands ask for passwords through this app: `helper` is the
/// executable git and ssh start to ask (one that calls [`run_helper`] before
/// anything else). Returns where the questions arrive. Without a call, or
/// once the receiver is dropped, git fails when it would have to ask.
pub fn enable(helper: PathBuf) -> io::Result<async_channel::Receiver<Prompt>> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let addr = listener.local_addr()?;
    let token = new_token();
    let (tx, rx) = async_channel::unbounded();
    let broker = Broker { helper, addr, token: token.clone() };
    if BROKER.set(broker).is_err() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "asking for passwords is already set up"));
    }
    std::thread::Builder::new().name("askpass".into()).spawn(move || serve(listener, token, tx))?;
    tracing::debug!(%addr, "git and ssh ask for passwords here");
    Ok(rx)
}

/// Makes `cmd` (git) ask through the app, if [`enable`] was called.
pub(crate) fn let_ask(cmd: &mut Command) {
    let Some(b) = BROKER.get() else { return };
    cmd.env("GIT_ASKPASS", &b.helper)
        .env("SSH_ASKPASS", &b.helper)
        // Even with a terminal (the app started from one), where nobody
        // would see the question.
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(ADDR_ENV, b.addr.to_string())
        .env(TOKEN_ENV, &b.token);
}

fn serve(listener: TcpListener, token: String, prompts: async_channel::Sender<Prompt>) {
    let ids = std::sync::Arc::new(AtomicU64::new(1));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (token, prompts, ids) = (token.clone(), prompts.clone(), ids.clone());
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, &token, &prompts, &ids) {
                tracing::debug!(error = %e, "askpass connection failed");
            }
        });
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    prompt: String,
    /// `SSH_ASKPASS_PROMPT`, when ssh set it.
    hint: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    answer: Option<String>,
}

/// One question: hands it to the front end, waits for the answer and sends
/// it back. Connections without the token get nothing.
fn handle(conn: TcpStream, token: &str, prompts: &async_channel::Sender<Prompt>, ids: &AtomicU64) -> io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new((&conn).take(MAX_REQUEST)).read_line(&mut line)?;
    let req: Request = serde_json::from_str(&line)?;
    if !token_matches(&req.token, token) {
        tracing::warn!("askpass request with a wrong token");
        return Ok(());
    }
    let kind = kind_of(&req.prompt, req.hint.as_deref());
    tracing::info!(?kind, "git asks: {}", req.prompt.trim());
    let answer = if prompts.receiver_count() == 0 {
        // No front end shows questions (it quit).
        None
    } else {
        let (reply, answered) = async_channel::bounded(1);
        let id = ids.fetch_add(1, Ordering::Relaxed);
        match prompts.send_blocking(Prompt { id, text: req.prompt, kind, reply }) {
            // Every copy of the prompt dropped unanswered ends the wait too.
            Ok(()) => answered.recv_blocking().ok().flatten(),
            Err(_) => None,
        }
    };
    tracing::info!(answered = answer.is_some(), "askpass done");
    let mut out = serde_json::to_vec(&Reply { answer })?;
    out.push(b'\n');
    (&conn).write_all(&out)
}

/// When git or ssh started this process to ask something (see [`enable`]):
/// asks the app, prints the answer for them, and returns the exit code.
/// `None` for an ordinary start.
pub fn run_helper() -> Option<i32> {
    let addr = std::env::var(ADDR_ENV).ok()?;
    let token = std::env::var(TOKEN_ENV).ok()?;
    // git and ssh pass the question as the only argument.
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let hint = std::env::var("SSH_ASKPASS_PROMPT").ok().filter(|h| !h.is_empty());
    // ssh shows a note (touch a security key) and ends the helper itself; it
    // takes no answer.
    if hint.as_deref() == Some("none") {
        return Some(0);
    }
    Some(match ask(&addr, &token, &prompt, hint) {
        Ok(Some(answer)) => {
            let mut out = io::stdout().lock();
            match writeln!(out, "{answer}").and_then(|()| out.flush()) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        }
        // Cancelled: git and ssh take a failing helper for no answer.
        Ok(None) => 1,
        Err(e) => {
            eprintln!("terminalrooster: could not ask for the password: {e}");
            1
        }
    })
}

/// Asks the app at `addr` and waits for the answer (`None`: cancelled).
fn ask(addr: &str, token: &str, prompt: &str, hint: Option<String>) -> io::Result<Option<String>> {
    let addr: SocketAddr = addr.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad address"))?;
    let conn = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    let mut req = serde_json::to_vec(&Request { token: token.to_owned(), prompt: prompt.to_owned(), hint })?;
    req.push(b'\n');
    (&conn).write_all(&req)?;
    // The person may take a while to answer: no timeout.
    let mut line = String::new();
    BufReader::new((&conn).take(MAX_REQUEST)).read_line(&mut line)?;
    if line.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the app closed the connection"));
    }
    Ok(serde_json::from_str::<Reply>(&line)?.answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of("Username for 'https://github.com': ", None), PromptKind::Text);
        assert_eq!(kind_of("Password for 'https://me@github.com': ", None), PromptKind::Secret);
        assert_eq!(kind_of("Enter passphrase for key '/home/me/.ssh/id_ed25519': ", None), PromptKind::Secret);
        assert_eq!(kind_of("me@example.com's password: ", None), PromptKind::Secret);
        let host = "The authenticity of host 'example.com (1.2.3.4)' can't be established.\n\
                    ED25519 key fingerprint is SHA256:abc.\n\
                    Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        assert_eq!(kind_of(host, None), PromptKind::YesNo);
        assert_eq!(kind_of("Allow use of key id_ed25519?", Some("confirm")), PromptKind::YesNo);
        assert_eq!(kind_of("Something else: ", None), PromptKind::Secret);
    }

    /// The front end's side of a broker (one per process, as in the app).
    fn broker() -> (&'static Broker, async_channel::Receiver<Prompt>) {
        static PROMPTS: OnceLock<async_channel::Receiver<Prompt>> = OnceLock::new();
        let rx = PROMPTS.get_or_init(|| enable(PathBuf::from("/nonexistent/helper")).unwrap()).clone();
        (BROKER.get().unwrap(), rx)
    }

    #[test]
    fn questions_reach_the_front_end_and_answers_come_back() {
        let (b, prompts) = broker();
        let (addr, token) = (b.addr.to_string(), b.token.clone());
        let asking = {
            let (addr, token) = (addr.clone(), token.clone());
            std::thread::spawn(move || ask(&addr, &token, "Password for 'https://me@example.com': ", None))
        };
        let p = prompts.recv_blocking().unwrap();
        assert_eq!((p.kind, p.title()), (PromptKind::Secret, "Git needs a password"));
        assert!(p.text.starts_with("Password for"));
        p.answer("hunter2 with spaces".into());
        assert_eq!(asking.join().unwrap().unwrap().as_deref(), Some("hunter2 with spaces"));

        // Cancelled, and dropped unanswered: no answer.
        let asking = {
            let (addr, token) = (addr.clone(), token.clone());
            std::thread::spawn(move || ask(&addr, &token, "Enter passphrase for key 'k': ", None))
        };
        prompts.recv_blocking().unwrap().cancel();
        assert_eq!(asking.join().unwrap().unwrap(), None);
        let asking = std::thread::spawn(move || ask(&addr, &token, "Username for 'https://x': ", None));
        drop(prompts.recv_blocking().unwrap());
        assert_eq!(asking.join().unwrap().unwrap(), None);
    }

    #[test]
    fn a_wrong_token_gets_nothing() {
        let (b, _prompts) = broker();
        assert!(ask(&b.addr.to_string(), "not-the-token", "Password: ", None).is_err());
    }

    #[test]
    fn git_commands_get_the_helper() {
        let (b, _prompts) = broker();
        let mut cmd = Command::new("git");
        let_ask(&mut cmd);
        let env: std::collections::HashMap<_, _> =
            cmd.get_envs().map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_owned()))).collect();
        assert_eq!(env["GIT_ASKPASS"].as_deref(), Some(b.helper.as_os_str()));
        assert_eq!(env["SSH_ASKPASS"].as_deref(), Some(b.helper.as_os_str()));
        assert_eq!(env["SSH_ASKPASS_REQUIRE"].as_deref(), Some(std::ffi::OsStr::new("force")));
        assert_eq!(env[TOKEN_ENV].as_deref(), Some(std::ffi::OsStr::new(&b.token)));
    }
}
