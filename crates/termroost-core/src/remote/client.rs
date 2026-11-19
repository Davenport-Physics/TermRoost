//! Terminal Rooster's side of a remote host: one SSH connection per host,
//! started when something on the host is first needed and again after it
//! broke, which carries every request and terminal of the host.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::bootstrap::{self, Launch};
use super::wire::{self, ClientHello, OpenTerminal, Opened, Request, ServerHello};
use crate::command::Event;
use crate::host::HostCommand;
use crate::host::proto::{self, FrameReader};
use crate::model::PaneId;

/// How long connecting may take, SSH included, before it counts as failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
/// After a failed attempt, requests fail at once for this long rather than
/// try again (the Files and Git tabs ask every few seconds).
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// How long a terminal's helper gets to be found or started.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// The lines of SSH's own messages kept for saying why connecting failed.
const STDERR_LINES: usize = 12;

/// How the connection to a host is made.
#[derive(Clone, Debug, Default)]
pub enum Transport {
    /// `ssh <host>`, which runs Terminal Rooster's helper there (copying it
    /// there first if needed).
    #[default]
    Ssh,
    /// Runs this command instead, which serves the protocol on its standard
    /// input and output (for tests).
    Command(HostCommand),
}

/// Where the connection to a host stands, for showing.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RemoteStatus {
    /// Not connected, and no attempt failed (nothing on the host was needed yet).
    Idle,
    Connecting,
    /// Copying Terminal Rooster's helper to the host.
    Copying,
    Connected,
    /// The connection broke; the next use connects again.
    Lost,
    /// Connecting failed, for this reason.
    Failed(String),
}

/// What all hosts share.
pub(crate) struct Shared {
    /// Identifies this Terminal Rooster to hosts (see [`ClientHello::client`]).
    pub client: String,
    pub transport: Transport,
    pub events: async_channel::Sender<Event>,
    /// The terminals each host has, for [`ClientHello::keep`].
    pub keep: Mutex<HashMap<String, Vec<PaneId>>>,
}

/// The hosts, by name, each made when first asked for.
pub(crate) struct Remotes {
    hosts: Mutex<HashMap<String, RemoteHost>>,
    shared: Arc<Shared>,
}

impl Remotes {
    pub fn new(shared: Shared) -> Remotes {
        Remotes { hosts: Mutex::new(HashMap::new()), shared: Arc::new(shared) }
    }

    pub fn host(&self, name: &str) -> RemoteHost {
        self.hosts
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_insert_with(|| {
                RemoteHost(Arc::new(Inner {
                    name: name.to_owned(),
                    shared: self.shared.clone(),
                    state: Mutex::new(State::Down { error: None, lost: false, since: Instant::now() }),
                    changed: Condvar::new(),
                    home: Mutex::new(None),
                }))
            })
            .clone()
    }

    /// The hosts asked for so far.
    pub fn all(&self) -> Vec<RemoteHost> {
        self.hosts.lock().unwrap().values().cloned().collect()
    }

    /// Records which terminals each host has.
    pub fn set_keep(&self, keep: HashMap<String, Vec<PaneId>>) {
        *self.shared.keep.lock().unwrap() = keep;
    }
}

/// A host projects can be on, reached over SSH. Cheap to clone; clones are
/// the same host and share its connection.
#[derive(Clone)]
pub struct RemoteHost(Arc<Inner>);

struct Inner {
    name: String,
    shared: Arc<Shared>,
    state: Mutex<State>,
    /// Signalled whenever `state` changes.
    changed: Condvar,
    /// The user's home folder there, as the last connection said.
    home: Mutex<Option<std::path::PathBuf>>,
}

enum State {
    Down {
        /// Why the last attempt failed.
        error: Option<String>,
        /// It was connected and the connection broke.
        lost: bool,
        since: Instant,
    },
    Connecting,
    Copying,
    Up(Arc<Conn>),
}

impl PartialEq for RemoteHost {
    fn eq(&self, other: &Self) -> bool {
        self.0.name == other.0.name
    }
}

impl Eq for RemoteHost {}

impl std::fmt::Debug for RemoteHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RemoteHost").field(&self.0.name).finish()
    }
}

impl RemoteHost {
    /// The host's name, as `ssh` takes it (`droplet`, `me@example.com`).
    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn status(&self) -> RemoteStatus {
        match &*self.0.state.lock().unwrap() {
            State::Down { error: Some(e), .. } => RemoteStatus::Failed(e.clone()),
            State::Down { lost: true, .. } => RemoteStatus::Lost,
            State::Down { .. } => RemoteStatus::Idle,
            State::Connecting => RemoteStatus::Connecting,
            State::Copying => RemoteStatus::Copying,
            State::Up(c) if c.alive() => RemoteStatus::Connected,
            State::Up(_) => RemoteStatus::Lost,
        }
    }

    /// The user's home folder there, once it was connected.
    pub fn home(&self) -> Option<std::path::PathBuf> {
        self.0.home.lock().unwrap().clone()
    }

    fn announce(&self, status: RemoteStatus) {
        tracing::info!(host = %self.0.name, ?status, "remote host");
        let _ = self.0.shared.events.try_send(Event::Remote { host: self.0.name.clone(), status });
    }

    /// The connection, if there is one now.
    pub(crate) fn up(&self) -> Option<Arc<Conn>> {
        match &*self.0.state.lock().unwrap() {
            State::Up(c) if c.alive() => Some(c.clone()),
            _ => None,
        }
    }

    /// The connection, connecting first if needed. Blocks while connecting
    /// (here or on another thread). Unless `retry`, an attempt that failed
    /// a moment ago is not made again yet: its error comes back at once.
    pub(crate) fn conn(&self, retry: bool) -> Result<Arc<Conn>, String> {
        let mut state = self.0.state.lock().unwrap();
        let deadline = Instant::now() + CONNECT_TIMEOUT * 2;
        // Another thread's attempt was waited for: its failure is this one's too.
        let mut waited = false;
        loop {
            match &*state {
                State::Up(c) if c.alive() => return Ok(c.clone()),
                State::Up(_) => {
                    // Broken, and its reading thread hasn't said so yet.
                    *state = State::Down { error: None, lost: true, since: Instant::now() };
                    self.announce(RemoteStatus::Lost);
                }
                State::Connecting | State::Copying => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(format!("still connecting to {}", self.0.name));
                    }
                    waited = true;
                    state = self.0.changed.wait_timeout(state, left).unwrap().0;
                }
                State::Down { error: Some(e), since, .. } if waited || (!retry && since.elapsed() < RETRY_AFTER) => {
                    return Err(e.clone());
                }
                State::Down { .. } => break,
            }
        }
        *state = State::Connecting;
        drop(state);
        self.announce(RemoteStatus::Connecting);
        let result = self.dial();
        let mut state = self.0.state.lock().unwrap();
        let out = match result {
            Ok(conn) => {
                *state = State::Up(conn.clone());
                Ok(conn)
            }
            Err(e) => {
                *state = State::Down { error: Some(e.clone()), lost: false, since: Instant::now() };
                Err(e)
            }
        };
        drop(state);
        self.0.changed.notify_all();
        match &out {
            Ok(_) => self.announce(RemoteStatus::Connected),
            Err(e) => self.announce(RemoteStatus::Failed(e.clone())),
        }
        out
    }

    /// Sets the state shown while connecting.
    fn set_copying(&self) {
        *self.0.state.lock().unwrap() = State::Copying;
        self.announce(RemoteStatus::Copying);
    }

    /// The connection `conn` broke.
    fn lost(&self, conn: &Arc<Conn>) {
        let mut state = self.0.state.lock().unwrap();
        if matches!(&*state, State::Up(c) if Arc::ptr_eq(c, conn)) {
            *state = State::Down { error: None, lost: true, since: Instant::now() };
            drop(state);
            self.0.changed.notify_all();
            self.announce(RemoteStatus::Lost);
        }
    }

    /// Ends the connection (the remote helper ends with it; terminals'
    /// helpers there run on).
    pub(crate) fn disconnect(&self) {
        let conn = match &*self.0.state.lock().unwrap() {
            State::Up(c) => Some(c.clone()),
            _ => None,
        };
        if let Some(conn) = conn {
            conn.close();
        }
    }

    /// The connection, once one being made is: requests don't connect by
    /// themselves (terminals do, see [`Self::open_terminal`]), so a host that
    /// is down isn't dialled again by every look at its files.
    fn ready(&self) -> Result<Arc<Conn>, String> {
        let mut state = self.0.state.lock().unwrap();
        let deadline = Instant::now() + CONNECT_TIMEOUT * 2;
        loop {
            match &*state {
                State::Up(c) if c.alive() => return Ok(c.clone()),
                State::Connecting | State::Copying if Instant::now() < deadline => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    state = self.0.changed.wait_timeout(state, left).unwrap().0;
                }
                State::Down { error: Some(e), .. } => return Err(e.clone()),
                _ => return Err(format!("not connected to {}", self.0.name)),
            }
        }
    }

    /// Asks the remote helper something and waits for the answer, over the
    /// connection there is (see [`Self::ready`]).
    pub(crate) fn call<T: DeserializeOwned>(&self, request: Request, timeout: Duration) -> Result<T, String> {
        let conn = self.ready()?;
        let value = conn.call(&request, timeout).map_err(String::from)?;
        serde_json::from_value(value).map_err(|e| format!("unexpected answer from {}: {e}", self.0.name))
    }

    /// Like [`Self::call`], only if connected now.
    pub(crate) fn call_if_up<T: DeserializeOwned>(&self, request: Request, timeout: Duration) -> Option<T> {
        let value = self.up()?.call(&request, timeout).ok()?;
        serde_json::from_value(value).ok()
    }

    /// Starts the connection: SSH (copying the helper over first if it
    /// isn't there), the hello, and the thread that reads what comes.
    fn dial(&self) -> Result<Arc<Conn>, String> {
        let name = &self.0.name;
        let shared = &self.0.shared;
        let launched = match &shared.transport {
            Transport::Ssh => bootstrap::launch(name, || self.set_copying())?,
            Transport::Command(cmd) => bootstrap::launch_command(cmd, name)?,
        };
        let Launch { child, stdout, mut stdin, stderr, watchdog } = launched;
        let keep = shared.keep.lock().unwrap().get(name).cloned().unwrap_or_default();
        let hello = ClientHello { version: wire::VERSION, client: shared.client.clone(), host: name.clone(), keep };
        let mut reader = FrameReader::new(stdout);
        let welcome = proto::write_json(&mut stdin, wire::HELLO, &hello).and_then(|()| reader.next());
        let welcome: ServerHello = match welcome {
            Ok(Some(f)) if f.kind == wire::WELCOME => {
                proto::parse(&f.payload).map_err(|e| format!("{name} answered oddly: {e}"))?
            }
            Ok(_) | Err(_) => {
                bootstrap::end_child(&child);
                return Err(bootstrap::explain(name, &stderr.lines(), "the helper there stopped"));
            }
        };
        watchdog.finish();
        if welcome.version != wire::VERSION {
            bootstrap::end_child(&child);
            return Err(format!(
                "{name} has a different version of Terminal Rooster's helper, which can't be used (remove it from \
                 ~/.terminalrooster/bin there)"
            ));
        }
        let conn = Arc::new(Conn {
            host: name.clone(),
            writer: Mutex::new(Box::new(stdin)),
            pending: Mutex::new(HashMap::new()),
            channels: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_channel: AtomicU32::new(1),
            alive: AtomicBool::new(true),
            child,
            hello: welcome,
        });
        let host = self.clone();
        let reading = conn.clone();
        std::thread::Builder::new()
            .name(format!("remote-{name}"))
            .spawn(move || {
                reading.read(reader);
                host.lost(&reading);
            })
            .map_err(|e| e.to_string())?;
        tracing::info!(host = %name, home = %conn.hello.home.display(), "connected");
        *self.0.home.lock().unwrap() = Some(conn.hello.home.clone());
        Ok(conn)
    }

    /// Opens a channel to a terminal's helper on the host (see
    /// [`Request::OpenTerminal`]), connecting first if needed.
    pub(crate) fn open_terminal(
        &self,
        open: OpenTerminal,
    ) -> Result<(Opened, ChannelReader, ChannelWriter), OpenError> {
        let conn = self.conn(true).map_err(OpenError::Connect)?;
        conn.open_terminal(open)
    }
}

/// Why a terminal could not be shown.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The host could not be reached, or was too slow to answer: worth
    /// trying again later.
    Connect(String),
    /// The connection broke meanwhile: worth trying again.
    Lost,
    /// The host refused (no such shell, say).
    Failed(String),
}

/// Why a request got no answer.
#[derive(Debug)]
pub(crate) enum CallError {
    /// None came in time; the connection may just be slow.
    Timeout(String),
    /// The connection broke.
    Lost(String),
    Failed(String),
}

impl From<CallError> for String {
    fn from(e: CallError) -> String {
        match e {
            CallError::Timeout(e) | CallError::Lost(e) | CallError::Failed(e) => e,
        }
    }
}

/// One SSH connection to the remote helper.
pub(crate) struct Conn {
    host: String,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Requests waiting for their response, by number.
    pending: Mutex<HashMap<u64, Sender<Result<serde_json::Value, String>>>>,
    /// Where each open channel's bytes go.
    channels: Mutex<HashMap<u32, Sender<ChanMsg>>>,
    next_id: AtomicU64,
    next_channel: AtomicU32,
    alive: AtomicBool,
    child: Arc<Mutex<Option<Child>>>,
    pub hello: ServerHello,
}

/// What a channel's reader gets.
pub(crate) enum ChanMsg {
    Data(Vec<u8>),
    /// The terminal's helper there ended the channel.
    Closed,
    /// The connection broke.
    Lost,
}

impl Conn {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        if !self.alive() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let mut w = self.writer.lock().unwrap();
        proto::write_frame(&mut *w, kind, payload).and_then(|()| w.flush())
    }

    fn call(&self, request: &impl Serialize, timeout: Duration) -> Result<serde_json::Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let json = serde_json::to_vec(request).map_err(|e| CallError::Failed(e.to_string()))?;
        if let Err(e) = self.send(wire::REQUEST, &wire::numbered(id, &json)) {
            self.pending.lock().unwrap().remove(&id);
            return Err(CallError::Lost(format!("lost the connection to {}: {e}", self.host)));
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => result.map_err(CallError::Failed),
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                Err(CallError::Timeout(format!("{} took too long to answer", self.host)))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(CallError::Lost(format!("lost the connection to {}", self.host)))
            }
        }
    }

    fn open_terminal(
        self: &Arc<Self>,
        open: OpenTerminal,
    ) -> Result<(Opened, ChannelReader, ChannelWriter), OpenError> {
        let channel = self.next_channel.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.channels.lock().unwrap().insert(channel, tx);
        let result = self.call(&Request::OpenTerminal { channel, open: Box::new(open) }, OPEN_TIMEOUT).and_then(|v| {
            serde_json::from_value::<Result<Opened, String>>(v).map_err(|e| CallError::Failed(e.to_string()))
        });
        let opened = match result {
            Ok(Ok(opened)) => opened,
            failed => {
                self.channels.lock().unwrap().remove(&channel);
                return Err(match failed {
                    _ if !self.alive() => OpenError::Lost,
                    Err(CallError::Lost(_)) => OpenError::Lost,
                    Err(CallError::Timeout(e)) => {
                        // It may still open there: it is closed then, as given up.
                        let _ = self.send(wire::CLOSE, &wire::channel(channel, &[]));
                        OpenError::Connect(e)
                    }
                    Ok(Err(e)) | Err(CallError::Failed(e)) => OpenError::Failed(e),
                    Ok(Ok(_)) => unreachable!("handled above"),
                });
            }
        };
        let reader = ChannelReader { rx, buf: Vec::new(), pos: 0, timeout: std::cell::Cell::new(None) };
        let writer = ChannelWriter { conn: self.clone(), channel, closed: false };
        Ok((opened, reader, writer))
    }

    /// Reads what comes until the connection ends, then fails what still waits.
    fn read(&self, mut reader: FrameReader<BufReader<ChildStdout>>) {
        loop {
            let frame = match reader.next() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(host = %self.host, error = %e, "remote connection broke");
                    break;
                }
            };
            match frame.kind {
                wire::RESPONSE => {
                    let Some((id, json)) = wire::split_numbered(&frame.payload) else { continue };
                    let result = serde_json::from_slice::<Result<serde_json::Value, String>>(json)
                        .unwrap_or_else(|e| Err(format!("unreadable answer: {e}")));
                    if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(result);
                    }
                }
                wire::OUTPUT => {
                    let Some((channel, bytes)) = wire::split_channel(&frame.payload) else { continue };
                    if let Some(tx) = self.channels.lock().unwrap().get(&channel) {
                        let _ = tx.send(ChanMsg::Data(bytes.to_vec()));
                    }
                }
                wire::CLOSED => {
                    if let Some((channel, _)) = wire::split_channel(&frame.payload)
                        && let Some(tx) = self.channels.lock().unwrap().remove(&channel)
                    {
                        let _ = tx.send(ChanMsg::Closed);
                    }
                }
                _ => {}
            }
        }
        self.alive.store(false, Ordering::Release);
        for (_, tx) in self.channels.lock().unwrap().drain() {
            let _ = tx.send(ChanMsg::Lost);
        }
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(format!("lost the connection to {}", self.host)));
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Ends the connection: closing SSH's input ends the remote helper,
    /// which the reading thread then sees.
    fn close(&self) {
        // From now on what is sent fails, rather than goes nowhere.
        self.alive.store(false, Ordering::Release);
        // SSH first: a write stuck on a full pipe then fails and lets go of the writer.
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        *self.writer.lock().unwrap() = Box::new(io::sink());
    }
}

/// Reads a channel's bytes. A read after the channel ended there returns 0;
/// one after the connection broke, or the channel was closed here (its
/// writer failed, so the connection likely broke), fails with
/// `ConnectionAborted`.
pub(crate) struct ChannelReader {
    rx: Receiver<ChanMsg>,
    buf: Vec<u8>,
    pos: usize,
    timeout: std::cell::Cell<Option<Duration>>,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            let msg = match self.timeout.get() {
                Some(t) => match self.rx.recv_timeout(t) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::WouldBlock.into()),
                    Err(RecvTimeoutError::Disconnected) => ChanMsg::Lost,
                },
                None => self.rx.recv().unwrap_or(ChanMsg::Lost),
            };
            match msg {
                ChanMsg::Data(bytes) => {
                    self.buf = bytes;
                    self.pos = 0;
                }
                ChanMsg::Closed => return Ok(0),
                ChanMsg::Lost => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "connection lost")),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl crate::host::client::Duplex for ChannelReader {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.timeout.set(timeout);
        Ok(())
    }
}

/// Writes to a channel; closing it (or dropping it) ends the channel.
pub(crate) struct ChannelWriter {
    conn: Arc<Conn>,
    channel: u32,
    closed: bool,
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.conn.send(wire::DATA, &wire::channel(self.channel, buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl crate::host::client::LinkWriter for ChannelWriter {
    /// Ends the channel. Its reader hears of a broken connection (see
    /// [`ChannelReader`]): when the link let go on purpose, that is no news.
    fn close(&mut self) {
        if !std::mem::replace(&mut self.closed, true) {
            self.conn.channels.lock().unwrap().remove(&self.channel);
            if self.conn.alive() {
                let _ = self.conn.send(wire::CLOSE, &wire::channel(self.channel, &[]));
            }
        }
    }
}

impl Drop for ChannelWriter {
    fn drop(&mut self) {
        crate::host::client::LinkWriter::close(self);
    }
}

/// The last lines a process wrote to its standard error, which are logged
/// as they come.
#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    /// Reads `from` on a thread of its own until it ends.
    pub fn follow(host: &str, from: impl Read + Send + 'static) -> StderrTail {
        let tail = StderrTail::default();
        let lines = tail.0.clone();
        let host = host.to_owned();
        let _ = std::thread::Builder::new().name(format!("remote-{host}-stderr")).spawn(move || {
            for line in BufReader::new(from).lines() {
                let Ok(line) = line else { break };
                tracing::info!(host = %host, "remote: {line}");
                let mut lines = lines.lock().unwrap();
                if lines.len() == STDERR_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
        });
        tail
    }

    pub fn lines(&self) -> Vec<String> {
        // What came last may still be on its way.
        std::thread::sleep(Duration::from_millis(100));
        self.0.lock().unwrap().iter().cloned().collect()
    }
}
