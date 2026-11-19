# Development

```sh
cargo test --workspace          # unit tests + real-PTY/real-DB integration tests, including an abort() crash test
cargo run -p termroost-core --example seed -- /some/dir   # demo data (use with TERMINALROOSTER_DATA_DIR)
cargo run -p termroost-core --example remote -- host:~/app   # connects to an SSH host as the app does, and says what happens
```

## Architecture

Cargo workspace with four crates:

- **`termroost-core`**: the program. No GUI dependencies. It owns the state, the SQLite store (on a writer thread), the terminals (`alacritty_terminal` with PTY / ConPTY), the helper processes that keep terminals running (`host`) and the autosave + Codex detection thread. A front end uses only this API:
  - `Core::open(Config)`
  - `core.state()`: projects, tabs, panes, tasks
  - `core.dispatch(Command)`: every change
  - `core.terminal(pane)`: returns a `TerminalHandle` for keys, paste, mouse, selection, resize, and `with_content` for a GUI-neutral grid with resolved RGB colors
  - `core.events()`: an `async_channel` receiver. Pass each event to `core.apply_event`.

  No iced, alacritty or rusqlite types appear in the API, so a different GUI can replace the iced one.
- **`termroost-iced`**: the iced 0.14 GUI. It contains a custom terminal widget, the pane grid, the sidebars, the task panel, the dialogs and the settings page. It also owns the themes; it hands the terminal colors to the core with `core.set_palette`. JetBrains Mono and Noto Sans Symbols 2 are bundled (both SIL OFL 1.1; licenses in `crates/termroost-iced/fonts`).
- **`termroost-tui`**: the ratatui front end, `terminalrooster-tui`. It draws each terminal's grid into the ratatui buffer. Its palette's default and ANSI colors map back to the outer terminal's own, so programs keep the user's theme.
- **`termroost-remote`**: `terminalrooster-remote`, the helper copied to [remote hosts](remote.md). It is a thin `main` around the core's `remote::serve` (and `host::serve` for each terminal there). The app programs can be the helper too (`terminalrooster remote`), which is what gets copied when there is no static build at hand.

### Remote hosts

A project's `host` names the SSH host its folder is on. The core's `remote` module runs `ssh <host>`, which starts the helper there; the module docs say how it is found and copied. One connection per host carries frames both ways (`remote::wire`): numbered requests and their JSON answers, and channels, each a byte stream to one terminal's own helper on the host (the same `host` helper as for terminals that keep running here, reached over loopback TCP there). A terminal's thread connects again whenever the connection breaks, and the helper there shows it as it is. Agent detection runs on the host and reports back through scans.

Front ends reach a project's files and git through `Core::project_machine` / `tab_machine`, a `Machine` that is this computer or the host. `git::Repo` carries its `Machine`, so its methods run git where the repository is. Requests use the connection the terminals made; they don't connect by themselves.

### The helper for remote hosts

`cargo xtask remote` builds `terminalrooster-remote` for Linux with musl, linked statically, into `target/x86_64-unknown-linux-musl/release/`. A build of the app finds it there and copies it to hosts, so it runs on any 64-bit Linux, whatever its C library. It needs the Rust target and a C compiler for the bundled SQLite:

```sh
rustup target add x86_64-unknown-linux-musl
sudo apt install musl-tools        # or have clang: xtask then uses it with this system's headers
cargo xtask remote
```

Without it, the app copies itself (or the `terminalrooster-remote` next to it), which needs a C library at least as new as this computer's: fine for hosts like it, not for older ones. `TERMINALROOSTER_REMOTE_BINARY` picks the file to copy, and `TERMINALROOSTER_SSH` the `ssh` to run. The packages bring the static helper in `/usr/lib/terminalrooster`.

`tests/remote.rs` runs the protocol end to end without SSH (the core's `remote::Transport::Command` starts the helper directly). For SSH itself, an `sshd` of your own on a spare port, with a `~/.ssh/config`-style file passed by a small `TERMINALROOSTER_SSH` wrapper (`exec ssh -F that-file "$@"`), works with the `remote` example.

## Packaging (Linux)

The `.deb` and `.rpm` packages install the `terminalrooster` binary, the static helper for remote hosts, a desktop entry, icons and AppStream metadata.

**Locally:**

```sh
cargo install --locked cargo-deb cargo-generate-rpm   # once
rustup target add x86_64-unknown-linux-musl          # once, for the helper for remote hosts (see above)
cargo xtask package                                  # or: cargo xtask package deb / rpm
```

The packages land in `dist/`. A binary only runs on systems whose glibc is at least as new as the one it was built against, so locally built packages suit your own machine. For packages to share, use the CI build.

**On GitHub:** push a tag that matches the version, e.g. `git tag v0.2.0 && git push origin v0.2.0`. The *Release packages* workflow builds both packages on Ubuntu 22.04, so they also run on older distributions, and attaches them to the release. You can also run it from the Actions tab to get the packages as a download.

**New version:**
1. Bump `version` in `Cargo.toml`.
2. Add a `<release>` entry to `packaging/linux/io.github.Davenport_Physics.TerminalRooster.metainfo.xml` (the build checks for it).

**Icons:** they live in `packaging/icons/`. The `terminalrooster-<size>.png` files are the hicolor icons for the packages, `terminalrooster-64.png` is also the window icon, and `terminalrooster.ico` is embedded in the Windows `.exe`.

## Packaging (Windows)

The installer is built with [NSIS](https://nsis.sourceforge.io) from `packaging/windows/terminalrooster.nsi`. It installs `terminalrooster.exe` along with the license files. It also adds Start menu and desktop shortcuts and an *Apps* entry with an uninstaller.

```powershell
winget install NSIS.NSIS    # once; or set MAKENSIS to makensis.exe
cargo xtask package windows
```

The installer lands in `dist/TerminalRooster-<version>-x64-setup.exe`. On Windows, `cargo xtask package` with no format builds the installer. On other platforms it builds the `.deb` and `.rpm`. The installer is not code-signed, so SmartScreen warns the first time it runs.
