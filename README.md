<p align="center">
  <img src="packaging/icons/terminalrooster-256.png" alt="Terminal Rooster's icon: a rooster's head in a terminal window" width="128">
</p>

# Terminal Rooster

A project-based terminal manager for running coding agents (Claude Code, Codex, Grok, …) side by side.

*TR, TermRoost or Rooster for short.*

- **Projects and sessions:** each project has a folder and its own sessions of split terminals.
- **Picks up where you left off:** after a restart or a crash, terminals come back with their scrollback and folder, and agents resume their exact conversation. A terminal can also keep running while Terminal Rooster is closed.
- **Notifications** when an agent finishes or needs you, or a command fails, in a terminal you aren't looking at.
- **A side panel** for the session's files (with an editor, or Neovim, and a picture viewer), tasks, notes and git: the log, staging, commits and worktrees.
- **Themes:** light and dark, following the system, or your own.
- **In a terminal too:** `terminalrooster-tui` shows the same projects inside a terminal you already have open, over SSH too.

## Running

On Linux:

```sh
cargo run --release -p termroost-iced
```

On Windows, run the installer (`TerminalRooster-<version>-x64-setup.exe`), or build it yourself with `cargo build --release`. Windows 10 1809 or newer is required.

See [Installing and running](docs/installing.md) for what a build needs, bash on Windows, and where data and logs live.

## Documentation

- [Installing and running](docs/installing.md): building, bash on Windows, data and logs
- [Features](docs/features.md): projects, sessions, files and the editor, tasks, notes, git
- [Restoring](docs/restoring.md): what comes back after a restart, terminals that keep running, exact agent sessions
- [Notifications](docs/notifications.md): what notifies, how agents are detected, shell integration
- [Themes](docs/themes.md)
- [Keyboard shortcuts](docs/shortcuts.md)
- [In a terminal](docs/terminal-ui.md): `terminalrooster-tui`
- [Development](docs/development.md): architecture, tests, packaging

## License

Terminal Rooster is released under the [MIT License](LICENSE).

The bundled fonts keep their own license, the SIL Open Font License 1.1: JetBrains Mono and Noto Sans Symbols 2. Their license texts are in `crates/termroost-iced/fonts/`.
