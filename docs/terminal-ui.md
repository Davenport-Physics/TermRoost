# In a terminal

`terminalrooster-tui` is Terminal Rooster inside a terminal you already have open, over SSH too. It works on the same projects, sessions and data as the window, so only one of the two runs at a time.

```sh
cargo run --release -p termroost-tui            # or: terminalrooster-tui [FOLDER | HOST:FOLDER]
```

With a folder, it opens that folder's project, adding the project if there is none. The first start offers the current folder. A folder of another computer is typed as `scp` takes it, `droplet:~/app`, there and in **p** (Tab completes its folder names once connected): see [Projects on other computers](remote.md).

The list of projects and their sessions is on the left; the session you pick shows on the right. **Alt+Arrows** move between them from anywhere: left from the leftmost terminal reaches the list, and up or down past the edge goes to the previous or next session. Terminals get every other key.

| Keys in the list | Action |
|---|---|
| ↑ ↓ (or j k) | Pick a session; it shows at once |
| Enter, →, Esc | Go to its terminals |
| n | New session |
| p | New project (Tab completes folder names) |
| v / s | Split the terminal right / down |
| x | Close the terminal |
| r | Rename |
| d | Delete (asks first) |
| Shift+↑ ↓ (or J K) | Move up / down |
| q, Ctrl+C | Quit (asks first while programs run) |

In a terminal, **Shift+PgUp / PgDn** scroll its history, and **Enter** restarts a shell that ended. A shell ended with `exit` closes its terminal when the session has others.

- **Mouse:** click a session or a terminal to go there; drag to select text, which is copied when you let go (through the outer terminal, with `OSC 52`); the wheel scrolls; drag a divider to resize. Programs that use the mouse get it; hold Shift to select in them anyway.
- **Colors:** the interface and the programs in it use your terminal's own theme, light or dark.
- **Marks** in the list, as in the window: a turning `◐` while an agent works, `✓` when it is done, `!` when it needs you, `✗` for a failed command, `•` while a program runs. The outer terminal's title gets a `●` while one waits for you.
- Below 72 columns the list stays out of the way until you go to it.

Logs go to `<data dir>/logs/terminalrooster-tui.*.log`.
