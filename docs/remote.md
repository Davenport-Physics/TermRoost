# Projects on other computers

A project can be on another computer that you reach with SSH, such as a server or a cloud machine. Its terminals, coding agents, files and git run there, and the project shows in the list like any other.

## Adding one

In **New project**, type the host in **Computer** the way `ssh` takes it: a name from `~/.ssh/config` (such as `droplet`) or `user@address`. The list beside the field offers the hosts in your `~/.ssh/config`. Then type the folder on that computer: `~/app`, `/srv/app`, or a path relative to the home folder there. Terminal Rooster connects to check that the folder exists, and says why if it can't.

In the projects list, the folder shows after the host, as `scp` writes it (`droplet:~/app`). **Copy path** copies it in that form too.

In the terminal UI, type `droplet:~/app` when it asks for a folder (**p**), or start it with that: `terminalrooster-tui droplet:~/app`.

A project stays on the computer it was made on: to move it, make a new one.

## What it needs

- **SSH that logs in without asking**, with a key (from `ssh-agent`, or one without a passphrase). Terminal Rooster runs your own `ssh` with your `~/.ssh/config`, so users, ports, keys and jump hosts set there apply. It never types a password. Connect once with `ssh <host>` in a terminal first, to accept the host's key.
- **A Linux computer.** Terminal Rooster copies a small helper program to `~/.terminalrooster/bin` there the first time; the sidebar says *copying Terminal Rooster's helper there…* while it does. Nothing needs installing, and no port needs opening: everything goes through the one SSH connection.

The helper it copies comes with Terminal Rooster: the Linux packages bring one built for any 64-bit Intel or AMD Linux. A build of your own uses the one `cargo xtask remote` makes (see [Development](development.md#the-helper-for-remote-hosts)); without it, Terminal Rooster copies itself, which only runs on a Linux like the one it was built on. On Windows there is none to copy yet: put `terminalrooster-remote` on the remote computer's `PATH` (from a Linux package, or built there with `cargo build --release -p termroost-remote`). If a computer can't run what it gets, the sidebar says so.

## While you work

- **Terminals keep running when the connection drops**, such as when the laptop sleeps or changes networks. Each runs under a helper of its own on that computer. The terminal says *lost the connection; reconnecting*, connects again by itself, and shows the program as it is now. What you typed meanwhile is sent once it is back.
- **Quitting Terminal Rooster** ends the remote terminals too, unless they are set to **Keep running when Terminal Rooster quits** (⚙ in the terminal's title bar). Such terminals are shown again, as they are, the next time; agents in the others resume their conversation, as for [terminals on this computer](restoring.md).
- **Agents are found** on that computer, the same way as here. Their status marks, notifications and exact resume work as for local ones.
- **Files, the editor and git** work in the right panel as for a local project. The Files tab reads the folders again every few seconds rather than watching them. With *Edit files with: Neovim or Vim*, files open in Neovim (or Vim) on that computer, over SSH.
- **The sidebar** says when a host is connecting, can't be reached (hover for why), or has lost its connection.
- **Not there yet:** opening a remote folder in your file manager, and opening a terminal in a window of its own (Windows).

## On the remote computer

Terminal Rooster keeps everything under `~/.terminalrooster`:

| Folder | What it holds |
|---|---|
| `bin/` | The helper, one file per build. The four used last are kept. |
| `clients/<id>/<host>/` | The terminals of one Terminal Rooster, for one host name: how to reach their helpers, shell integration, each terminal's command history. |

Deleting the folder removes it all; the next connection copies the helper again.

## Troubleshooting

- **Logs:** what the helper says ends up in Terminal Rooster's own log (see [Logs](installing.md#logs)), marked with the host.
- **"Permission denied (publickey)"**, **"Could not resolve hostname"** and other SSH errors show as SSH reported them. Check with `ssh <host>` in a terminal.
- **"built for a newer Linux"**: the helper copied there needs a newer C library than that computer has. Build the static one with `cargo xtask remote`, or use a release package.
- **A different SSH program:** set `TERMINALROOSTER_SSH` to its path.
