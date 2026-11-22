// Release builds on Windows should not open a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agent_icon;
mod app;
mod busy;
mod color_picker;
mod context_menu;
#[cfg(target_os = "linux")]
mod desktop_entry;
mod dialogs;
mod diff_lines;
mod editor;
mod file_icons;
mod files;
mod folder_watch;
mod git_panel;
mod git_view;
mod gone_folders;
mod groups;
mod host_exe;
mod icons;
mod keymap;
mod layout_conv;
mod leave_room;
mod logging;
mod notes;
mod notify;
mod on_escape;
mod panes;
mod picture_view;
mod remotes;
mod reorder;
mod reveal;
mod settings;
mod side_panel;
mod sidebar;
mod tasks;
mod terminal_view;
mod theme;
mod themes;
mod toasts;
mod ui;
mod worktree;
mod worktree_remove;

use std::sync::Mutex;

use iced::{Point, Size, window};
use termroost_core::{Config, Core};

use crate::app::{App, Prefs};

/// Desktop application id (also the .desktop and AppStream file names).
pub const APP_ID: &str = "io.github.Davenport_Physics.TerminalRooster";
/// The app's name as people read it: window title, dialogs, notifications.
/// Folders, ids and environment variables keep `TerminalRooster`.
pub const DISPLAY_NAME: &str = "Terminal Rooster";

/// The desktop id this build runs under. On Linux a build that wasn't
/// installed from a package uses a separate id (see `desktop_entry`).
#[cfg(target_os = "linux")]
pub use desktop_entry::app_id;
#[cfg(not(target_os = "linux"))]
pub fn app_id() -> &'static str {
    APP_ID
}

fn main() -> iced::Result {
    let data_dir = termroost_core::paths::default_data_dir().ok();
    // A terminal's helper process, or a window of its own (see `host_exe`),
    // not the app.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == host_exe::HOST_ARG) {
        let log_guard = logging::init_as(data_dir.as_deref(), "terminalrooster-host");
        let code = termroost_core::host::serve();
        drop(log_guard);
        std::process::exit(code);
    }
    if args.first().is_some_and(|a| a == host_exe::ATTACH_ARG) {
        let log_guard = logging::init_as(data_dir.as_deref(), "terminalrooster-window");
        let code = match (data_dir.as_deref(), args.get(1).and_then(|p| p.parse().ok())) {
            (Some(dir), Some(pane)) => {
                termroost_core::host::attach_console(dir, termroost_core::PaneId(pane), args.get(2).map(String::as_str))
            }
            _ => {
                eprintln!("usage: terminalrooster {} <terminal id> [window title]", host_exe::ATTACH_ARG);
                2
            }
        };
        drop(log_guard);
        std::process::exit(code);
    }
    // Flushes the log file when main returns.
    let _log_guard = logging::init(data_dir.as_deref());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "TerminalRooster starting");
    // Lets desktops show the icon for a build run without installing a package.
    #[cfg(target_os = "linux")]
    desktop_entry::ensure();
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        tracing::error!("no display: neither WAYLAND_DISPLAY nor DISPLAY is set");
        eprintln!("terminalrooster: no graphical session found (neither WAYLAND_DISPLAY nor DISPLAY is set)");
        std::process::exit(1);
    }

    // Terminals started from here must not believe they run inside another
    // Claude Code session (e.g. when TerminalRooster itself was launched from one).
    // SAFETY: no other threads exist yet.
    unsafe {
        std::env::remove_var("CLAUDECODE");
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
    }

    let config = Config { host: host_exe::command(data_dir.as_deref()), ..Config::default() };
    let core = match Core::open(config) {
        Ok(core) => core,
        Err(e) => {
            tracing::error!(error = %e, "could not open the core");
            let _ = rfd::MessageDialog::new()
                .set_title(DISPLAY_NAME)
                .set_description(e.to_string())
                .set_level(rfd::MessageLevel::Error)
                .show();
            std::process::exit(1);
        }
    };

    let prefs = Prefs::load(&core);
    let geom = prefs.window.unwrap_or_default();
    let size = if geom.width >= 640.0 && geom.height >= 400.0 {
        Size::new(geom.width, geom.height)
    } else {
        Size::new(1400.0, 860.0)
    };
    let position = match (geom.x, geom.y) {
        (Some(x), Some(y)) if x > -10_000.0 && y > -10_000.0 => window::Position::Specific(Point::new(x, y)),
        _ => window::Position::Default,
    };
    let window = window::Settings {
        size,
        position,
        min_size: Some(Size::new(640.0, 400.0)),
        exit_on_close_request: false,
        icon: window::icon::from_rgba(include_bytes!(concat!(env!("OUT_DIR"), "/icon-64.rgba")).to_vec(), 64, 64).ok(),
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            // Matches the .desktop file name, so desktops show the right icon.
            application_id: app_id().to_owned(),
            ..Default::default()
        },
        ..window::Settings::default()
    };

    // `boot` is a `Fn`; hand the already-open core over exactly once.
    let core = Mutex::new(Some(core));
    let mut application = iced::application(
        move || App::new(core.lock().unwrap().take().expect("boot runs once")),
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(App::theme)
    .window(window)
    .settings(iced::Settings { default_text_size: theme::TEXT.into(), ..iced::Settings::default() })
    .default_font(theme::UI)
    .antialiasing(true);
    for font in theme::FONTS {
        application = application.font(font);
    }
    let result = application.run();
    tracing::info!("TerminalRooster exited");
    result
}
