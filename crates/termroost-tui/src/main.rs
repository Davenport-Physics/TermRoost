//! Terminal Rooster inside a terminal: the desktop app's projects, sessions
//! and coding agents, drawn with ratatui on the same core and the same data.
//! One of the two runs at a time.

mod app;
mod clipboard;
mod colors;
mod folders;
mod input;
mod logging;
mod panes;
mod text_field;
mod view;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{SetCursorStyle, Show};
use ratatui::crossterm::{event, execute, terminal};
use termroost_core::{Config, Core, CursorShape, Error};

use crate::app::{App, Msg};

const HELP: &str = "\
Usage: terminalrooster-tui [FOLDER]

Projects, sessions and coding agents, side by side, in this terminal.
With FOLDER, opens its project (adding it if there is none).
";

fn main() -> ExitCode {
    let launch_dir = std::env::current_dir().unwrap_or_default();
    let mut open = None;
    for arg in std::env::args_os().skip(1) {
        match arg.to_str() {
            Some("-h" | "--help") => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            Some("-V" | "--version") => {
                println!("terminalrooster-tui {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            Some(flag) if flag.starts_with('-') => {
                eprint!("terminalrooster-tui: unknown option {flag}\n\n{HELP}");
                return ExitCode::from(2);
            }
            _ if open.is_none() => open = Some(PathBuf::from(arg)),
            _ => {
                eprint!("terminalrooster-tui: one folder at a time\n\n{HELP}");
                return ExitCode::from(2);
            }
        }
    }
    let open = match open.map(|p| launch_dir.join(p)) {
        Some(dir) if !dir.is_dir() => {
            eprintln!("terminalrooster-tui: {} is not a folder", dir.display());
            return ExitCode::FAILURE;
        }
        Some(dir) => Some(plain(dir.canonicalize().unwrap_or(dir))),
        None => None,
    };

    // Terminals started from here must not believe they run inside another
    // agent session (when this was itself started from one).
    // SAFETY: no other threads exist yet.
    unsafe {
        std::env::remove_var("CLAUDECODE");
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        std::env::remove_var("ANTIGRAVITY_AGENT");
        std::env::remove_var("ANTIGRAVITY_CONVERSATION_ID");
        std::env::remove_var("ANTIGRAVITY_TRAJECTORY_ID");
        std::env::remove_var("ANTIGRAVITY_LS_ADDRESS");
        std::env::remove_var("ANTIGRAVITY_CSRF_TOKEN");
        std::env::remove_var("ANTIGRAVITY_SOURCE_METADATA");
        std::env::remove_var("ANTIGRAVITY_PROJECT_ID");
    }
    let _log = logging::init(termroost_core::paths::default_data_dir().ok().as_deref());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "terminalrooster-tui starting");

    let core = match Core::open(Config { palette: colors::palette(), ..Config::default() }) {
        Ok(core) => core,
        Err(e) => {
            tracing::error!(error = %e, "could not open the core");
            eprintln!("terminalrooster-tui: {e}");
            if matches!(e, Error::AlreadyRunning(_)) {
                eprintln!("Close the other one first, or set TERMINALROOSTER_DATA_DIR to use other data.");
            }
            return ExitCode::FAILURE;
        }
    };
    match run(core, launch_dir, open) {
        Ok(()) => {
            tracing::info!("terminalrooster-tui exited");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, "terminal error");
            eprintln!("terminalrooster-tui: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `canonicalize` gives `\\?\D:\…` on Windows; the plain form reads better and
/// matches what shells report.
fn plain(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

fn run(core: Core, launch_dir: PathBuf, open: Option<PathBuf>) -> io::Result<()> {
    let screen = Screen::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let (tx, rx) = mpsc::channel();
    let keys = tx.clone();
    std::thread::Builder::new().name("input".into()).spawn(move || {
        loop {
            match event::read() {
                Ok(ev) => {
                    if keys.send(Msg::Input(ev)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "cannot read the keyboard");
                    let _ = keys.send(Msg::InputClosed);
                    break;
                }
            }
        }
    })?;
    let events = core.events();
    std::thread::Builder::new().name("core-events".into()).spawn(move || {
        while let Ok(ev) = events.recv_blocking() {
            if tx.send(Msg::Core(ev)).is_err() {
                break;
            }
        }
    })?;

    let mut app = App::new(core, launch_dir, open);
    let mut shape = None;
    loop {
        if app.dirty {
            app.dirty = false;
            terminal.draw(|f| app.draw(f))?;
            if app.cursor_shape != shape {
                shape = app.cursor_shape;
                execute!(terminal.backend_mut(), cursor_style(shape))?;
            }
        }
        if !app.out.is_empty() {
            let out = terminal.backend_mut();
            for seq in app.out.drain(..) {
                out.write_all(seq.as_bytes())?;
            }
            out.flush()?;
        }
        if app.quit {
            break;
        }
        match rx.recv_timeout(app.timeout()) {
            Ok(msg) => {
                app.handle(msg);
                // Take everything that queued up meanwhile, then draw once.
                for msg in rx.try_iter().take(1024) {
                    app.handle(msg);
                }
            }
            Err(RecvTimeoutError::Timeout) => app.tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Saves scrollback and stops the shells while the screen still shows the app.
    app.core.shutdown();
    drop(screen);
    Ok(())
}

fn cursor_style(shape: Option<CursorShape>) -> SetCursorStyle {
    match shape {
        Some(CursorShape::Beam) => SetCursorStyle::SteadyBar,
        Some(CursorShape::Underline) => SetCursorStyle::SteadyUnderScore,
        _ => SetCursorStyle::DefaultUserShape,
    }
}

/// Whether the kitty keyboard protocol was turned on, for putting things back.
static KITTY: AtomicBool = AtomicBool::new(false);

/// The terminal set up for the app; dropping it (or a panic) puts it back.
struct Screen;

impl Screen {
    fn enter() -> io::Result<Screen> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        // Saves the window title, to put it back when done.
        out.write_all(b"\x1b[22;0t")?;
        execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture)?;
        // On Windows the console reports focus by itself, and crossterm reads
        // no bracketed paste there: turning these on would turn their escape
        // sequences into keys.
        #[cfg(not(windows))]
        {
            execute!(out, event::EnableFocusChange, event::EnableBracketedPaste)?;
            // Tells Shift+Enter from Enter, for agents' newlines.
            if terminal::supports_keyboard_enhancement().unwrap_or(false) {
                execute!(
                    out,
                    event::PushKeyboardEnhancementFlags(event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
                )?;
                KITTY.store(true, Ordering::Relaxed);
            }
        }
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            tracing::error!(%info, backtrace = %std::backtrace::Backtrace::force_capture(), "panic");
            default_hook(info);
        }));
        Ok(Screen)
    }
}

fn restore() {
    let mut out = io::stdout();
    if KITTY.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, event::PopKeyboardEnhancementFlags);
    }
    #[cfg(not(windows))]
    let _ = execute!(out, event::DisableBracketedPaste, event::DisableFocusChange);
    let _ = execute!(out, event::DisableMouseCapture, SetCursorStyle::DefaultUserShape, Show);
    let _ = execute!(out, terminal::LeaveAlternateScreen);
    let _ = out.write_all(b"\x1b[23;0t");
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}
