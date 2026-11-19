//! Logs go to a daily file in the data directory only
//! (`<data dir>/logs/terminalrooster-tui.YYYY-MM-DD.log`, last 7 days):
//! anything written to the screen would garble it.
//!
//! The level comes from `TERMINALROOSTER_LOG`, then `RUST_LOG`.

use std::path::Path;

use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

const LOG_ENV: &str = "TERMINALROOSTER_LOG";
const DEFAULT_FILTER: &str = "warn,terminalrooster_tui=info,termroost_core=info";

/// Installs the subscriber. Keep the guard alive until exit: dropping it
/// flushes the file.
pub fn init(data_dir: Option<&Path>) -> Option<WorkerGuard> {
    let dir = data_dir?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("terminalrooster-tui")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = NonBlockingBuilder::default().lossy(false).finish(appender);
    let filter = EnvFilter::try_from_env(LOG_ENV)
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry().with(fmt::layer().with_ansi(false).with_writer(writer).with_filter(filter)).init();
    Some(guard)
}

/// Logs to standard error only: for this program as Terminal Rooster's
/// helper on a remote host, whose standard error SSH carries back.
pub fn init_stderr() {
    let filter = EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new("warn,termroost_core=info"));
    let layer = fmt::layer().with_ansi(false).with_writer(std::io::stderr).with_filter(filter);
    let _ = tracing_subscriber::registry().with(layer).try_init();
}
