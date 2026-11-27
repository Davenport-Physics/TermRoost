# Notifications

Terminal Rooster tells you when something happens in a terminal you aren't looking at:

- **Coding agents:** Claude Code, Codex, Grok or Antigravity finishes its turn, needs an approval, or asks a question.
- **Failed commands:** a command exits with an error status, or a terminal's shell does.

In **Settings → Notifications**, each of the two can be **Off**, **Subtle** or **Prominent**. By default, agents are Prominent and failed commands are Subtle.

- **Subtle:** a ✓ (agent) or ✗ (failure) next to the session and the project, and a label in the terminal's title bar. These stay until you focus that terminal.
- **Prominent:** the same marks. If the terminal is out of view, a note also pops up in the top right corner; **Show** takes you to it. While Terminal Rooster is in the background, you get a desktop notification instead and the taskbar entry flashes. Clicking the notification brings up the terminal.

The terminal you are looking at never notifies. **Send a test notification** in Settings checks that desktop notifications get through. On Windows, Terminal Rooster registers its name and icon for them under `HKCU\Software\Classes\AppUserModelId`, and the uninstaller removes that again.

## How it knows

No agent needs any setup.

| Source | What Terminal Rooster reads |
|---|---|
| Claude Code | Its terminal title: `◐`/`◑` while it works, `✳` once it is done or waiting for you |
| Codex, Antigravity | The state they report to the terminal (`OSC 7501`: working, done, error, waiting for an approval or question) |
| Grok and other programs | Desktop notification requests (`OSC 9`, `OSC 777`, `OSC 99`), progress indicators (`OSC 9;4`), and the bell in a terminal running an agent |
| Failed commands | The exit status the shell reports after each command (`OSC 133;D`) |

## Shell integration

bash and PowerShell (`pwsh`, `powershell`) don't report exit statuses or their working directory by themselves, so Terminal Rooster loads a small script into them when a terminal starts. The scripts live in the data directory, in `shell-integration/`, and your startup files are not changed:

- **bash** gets `PROMPT_COMMAND` in its environment. That sources the script at the first prompt, after `~/.bashrc`. If your `~/.bashrc` replaces `PROMPT_COMMAND` instead of adding to it, there is no integration.
- **PowerShell** runs the script after your profile, with `-NoExit -Command`. This is skipped when the shell's command line already runs a command or a file.

The scripts also give each terminal its own command history, so Up in a restored terminal brings back what was typed there. A new terminal's history starts as a copy of the shell's usual one (`~/.bash_history`, or PSReadLine's file), and commands typed in Terminal Rooster no longer go into that file.

Other shells (zsh, fish, cmd, WSL) start unchanged. They work too if something in them sends `OSC 133;D;<status>`, and `OSC 7` or `OSC 9;9` for the directory (fish and many prompt themes do). Turn the integration off in Settings → Notifications.

## Quitting with programs running

Closing the window while terminals run programs (an agent, a build, a server) asks first and lists them. Agents come back in the same conversation on the next start, but a reply they are writing is cut off. Choose **Don't ask again** there, or switch it off in Settings → Notifications. Programs inside WSL can't be seen from Windows, so WSL terminals never count, and neither do terminals that [keep running](restoring.md#terminals-that-keep-running).
