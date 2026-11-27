# Restoring after a restart or crash

A terminal's programs end when Terminal Rooster closes or crashes, unless the terminal is set to [keep running](#terminals-that-keep-running). On reopen, every terminal gets the following:

- Its **saved scrollback**, with colors. It is saved every 5 seconds and on close, for terminals you typed in, and stays on screen above the new shell's prompt. Terminals that were running a coding agent skip this: the agent shows its own conversation again when it resumes.
- A **fresh shell** in the directory the last one was in, if the shell reported it (see [Shell integration](notifications.md#shell-integration)). Otherwise, or if that directory is gone, it starts in the session's working directory.
- Its **resume command**, which runs automatically.

Restored sessions start their shells the first time you open them, so dozens of agents don't all launch at once.

## Terminals that keep running

In a terminal's settings (⚙ in its title bar), **Keep running when Terminal Rooster quits** runs it under a helper process of its own, so a dev server or an agent goes on while Terminal Rooster is closed or restarting. The next start shows the terminal again as the program left it, with what it printed meanwhile. Switching the setting restarts the terminal.

Such terminals carry an ∞ mark, and quitting doesn't count them as programs it would stop. On Windows, **Open in its own window** in their title bar shows them in a console window that stays open when Terminal Rooster quits; closing that window brings the terminal back.

## Exact agent sessions, not "continue last"

`--continue` and `resume --last` reopen the most recent conversation in a folder, which is wrong when several agents work in one project. Terminal Rooster watches each terminal's processes instead (every 5 seconds, and once more on shutdown). It finds the coding agent running there and reads the agent's session id from the agent's own records:

| Agent | Where the session id comes from | Restore command |
|---|---|---|
| Claude Code | `~/.claude/sessions/<pid>.json`, rewritten on `/clear` | `claude --resume <id>` |
| Grok | `~/.grok/active_sessions.json` (`pid` → `session_id`) | `grok --resume <id>` |
| Codex | Codex's thread table (`~/.codex/state_*.sqlite`): the newest user thread in the process's folder started after the process, or the id in `codex resume <id>` | `codex resume <id>` |
| Antigravity | The conversation file held open by `agy` on Linux (`~/.gemini/antigravity-cli/presence/<id>.lock`, `conversations/<id>.db`), `~/.gemini/antigravity-cli/conversation_summaries.db`, or the id in `agy --resume <id>` / `--conversation <id>` | `agy --resume <id>` |

- **How agents are found:** it works whether you start the agent with **Start agent** or type `claude` yourself. It follows `/clear`, `/new` and `/resume`, and it forgets the agent when you quit it.
- **Missing sessions:** if a session never got a message, so nothing is saved to resume, the agent is started fresh instead.
- **Extra arguments:** arguments set in terminal settings (e.g. `--model opus`) are reused on resume.
- **Codex accuracy:** Codex runs every terminal through one shared daemon and keeps no per-process record, so by default its session is inferred. That is exact with one Codex per folder. For exact tracking, add the optional **Codex hook**. Terminal Rooster offers it once at startup, and you can also add it from Settings → Integrations. Codex only runs hooks you trust, so Terminal Rooster then opens a small Codex window: type `/hooks` there and trust it. The hook goes in `~/.codex/hooks.json`, your other hooks are kept, and it only writes while Terminal Rooster runs.
- **Debugging detection:** `cargo run -p termroost-core --example detect_agents -- <shell pid>…` prints what is detected under any shell.
