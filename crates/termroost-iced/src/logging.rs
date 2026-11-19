//! `tracing` setup: logs go to stderr and to a daily-rotated file in the data
//! directory (`<data dir>/logs/terminalrooster.YYYY-MM-DD.log`, last 7 days kept).
//! The file matters on Windows, where release builds have no console.
//!
//! The level is taken from `TERMINALROOSTER_LOG`, then `RUST_LOG`, e.g.
//! `TERMINALROOSTER_LOG=debug` or `TERMINALROOSTER_LOG=termroost_core=trace`.

use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

pub const LOG_ENV: &str = "TERMINALROOSTER_LOG";
/// Our binary's target is `terminalrooster`; the core's is `termroost_core`.
const DEFAULT_FILTER: &str = "warn,wgpu_hal=error,wgpu_core=error,naga=error,terminalrooster=info,termroost_core=info";

fn filter() -> EnvFilter {
    EnvFilter::try_from_env(LOG_ENV)
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// Installs the global subscriber and a panic hook that logs panics.
/// Keep the returned guard alive until exit: dropping it flushes the file.
pub fn init(data_dir: Option<&Path>) -> Option<WorkerGuard> {
    init_as(data_dir, "terminalrooster")
}

/// As [`init`], logging to files named after `prefix`: each kind of process
/// writes (and rotates) files of its own.
pub fn init_as(data_dir: Option<&Path>, prefix: &str) -> Option<WorkerGuard> {
    let stderr = fmt::layer().with_writer(std::io::stderr).with_filter(filter());

    let mut guard = None;
    let file = data_dir.and_then(|dir| {
        let dir = log_dir(dir);
        let appender = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|()| {
            RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix(prefix)
                .filename_suffix("log")
                .max_log_files(7)
                .build(&dir)
                .map_err(|e| e.to_string())
        });
        match appender {
            Ok(appender) => {
                let (writer, g) = NonBlockingBuilder::default().lossy(false).finish(appender);
                guard = Some(g);
                Some(fmt::layer().with_ansi(false).with_writer(writer).with_filter(filter()))
            }
            Err(e) => {
                eprintln!("terminalrooster: cannot write logs to {}: {e}", dir.display());
                None
            }
        }
    });

    tracing_subscriber::registry().with(stderr).with(file).init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%info, %backtrace, "panic");
        default_hook(info);
    }));
    guard
}

/// Logs to standard error only: for this program as Terminal Rooster's
/// helper on a remote host, whose standard error SSH carries back.
pub fn init_stderr() {
    let layer = fmt::layer().with_ansi(false).with_writer(std::io::stderr).with_filter(remote_filter());
    let _ = tracing_subscriber::registry().with(layer).try_init();
}

fn remote_filter() -> EnvFilter {
    EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new("warn,termroost_core=info"))
}
