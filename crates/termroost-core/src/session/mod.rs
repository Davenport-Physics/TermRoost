//! Terminal sessions and the background autosave/agent-detection thread.

mod bundle;
pub mod bundled_bash;
pub mod bundled_nvim;
pub mod input;
pub mod integration;
pub(crate) mod osc;
pub mod render;
pub mod shell;
pub(crate) mod snapshot;
mod tap;
mod terminal;

pub(crate) use snapshot::{just_a_prompt, without_restore_banners};
#[cfg(windows)]
pub(crate) use terminal::quote_program;
pub use terminal::{Activity, GridSize, SelectionKind, TerminalHandle};
pub(crate) use terminal::{HostPlan, NewHost, RemotePlan, SpawnSpec, resize_like_conpty};

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::command::Event;
use crate::detect::{Detector, ObservedAgent};
use crate::model::{PaneId, now};
use crate::store::{StoreOp, StoreWriter};

pub(crate) type SessionMap = Arc<Mutex<HashMap<PaneId, TerminalHandle>>>;

/// State shared with the background thread.
#[derive(Clone)]
pub(crate) struct Background {
    pub sessions: SessionMap,
    pub detector: Arc<Mutex<Detector>>,
    /// Where the Codex hook drops its reports.
    pub data_dir: std::path::PathBuf,
    /// Hook reports written before this are stale.
    pub started: std::time::SystemTime,
    pub last_codex_report: Arc<Mutex<Option<std::time::SystemTime>>>,
    pub store: StoreWriter,
    pub events: async_channel::Sender<Event>,
    pub saved_lines: Arc<std::sync::atomic::AtomicUsize>,
    /// PTYs not done closing, of all terminals; see `tap::EndShell`.
    pub ptys: Arc<std::sync::atomic::AtomicUsize>,
}

impl Background {
    /// Saves the scrollback of every terminal that changed since its last save.
    pub fn save_dirty(&self, force: bool) {
        let handles: Vec<TerminalHandle> = self.sessions.lock().unwrap().values().cloned().collect();
        let max = self.saved_lines.load(Ordering::Relaxed);
        let mut ops = Vec::new();
        for h in handles {
            // Nothing typed: there's only what the shell printed by itself (a
            // prompt), not worth restoring. What an earlier session saved for
            // this pane stays as it was.
            if !h.typed() {
                continue;
            }
            let dirty = h.0.shared.save_dirty.swap(false, Ordering::AcqRel);
            if !(dirty || force) {
                continue;
            }
            if let Some(data) = h.snapshot(max) {
                ops.push(StoreOp::Scrollback { pane: h.pane(), data, saved_at: now() });
            }
        }
        if !ops.is_empty() {
            tracing::debug!(terminals = ops.len(), "saving scrollback");
        }
        for op in ops {
            // One batch per pane: a pane closed meanwhile cannot fail the others.
            self.store.write(vec![op]);
        }
    }

    /// Looks at what runs in every terminal: records the programs below each
    /// shell (see [`TerminalHandle::activity`]), and returns what changed
    /// about their agents.
    pub fn scan_terminals(&self) -> Vec<(PaneId, Option<ObservedAgent>)> {
        let reports = crate::codex_hook::drain(&self.data_dir, self.started);
        if let Some(last) = reports.last() {
            *self.last_codex_report.lock().unwrap() = Some(last.at);
            let _ = self.events.try_send(Event::CodexHookReported);
        }
        let handles: Vec<TerminalHandle> = self.sessions.lock().unwrap().values().cloned().collect();
        let mut shells = Vec::new();
        let mut submits = HashMap::new();
        // Remote terminals, by host: their hosts look.
        let mut remote: HashMap<String, (crate::RemoteHost, Vec<(PaneId, u32)>)> = HashMap::new();
        for h in &handles {
            if let Some(host) = h.remote() {
                if let Some(pid) = h.remote_shell_pid() {
                    remote
                        .entry(host.name().to_owned())
                        .or_insert_with(|| (host.clone(), Vec::new()))
                        .1
                        .push((h.pane(), pid));
                }
            } else if let Some(pid) = h.shell_pid() {
                shells.push((h.pane(), pid));
                submits.insert(h.pane(), h.submits());
            }
        }
        let mut detector = self.detector.lock().unwrap();
        let mut changes = detector.scan_with(&shells, &submits, &reports);
        // The same look at the processes.
        let mut programs = detector.programs_seen(&shells);
        drop(detector);
        for h in handles.iter().filter(|h| h.remote().is_none()) {
            h.set_programs(programs.remove(&h.pane()).unwrap_or_default());
        }
        for (host, shells) in remote.into_values() {
            // A host that isn't connected now isn't asked; what was seen last stays.
            let request = crate::remote::wire::Request::Scan(shells);
            let Some(scan) = host.call_if_up::<crate::remote::wire::Scan>(request, Duration::from_secs(10)) else {
                continue;
            };
            let mut found = scan.programs;
            for h in handles.iter().filter(|h| h.remote() == Some(&host)) {
                h.set_programs(found.remove(&h.pane()).unwrap_or_default());
            }
            changes.extend(scan.changes);
        }
        for (pane, agent) in &changes {
            match agent {
                Some(a) => {
                    tracing::info!(%pane, agent = a.kind.as_str(), session = ?a.session_id, pid = a.pid, "agent detected")
                }
                None => tracing::info!(%pane, "agent exited"),
            }
        }
        changes
    }

    pub fn spawn(self, stop: mpsc::Receiver<()>, interval: Duration) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("terminalrooster-autosave".into())
            .spawn(move || {
                // Runs until the core sends a stop signal (or is dropped).
                while let Err(mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(interval) {
                    self.save_dirty(false);
                    for (pane, agent) in self.scan_terminals() {
                        let _ = self.events.try_send(Event::AgentObserved { pane, agent });
                    }
                }
            })
            .expect("spawn autosave thread")
    }
}
