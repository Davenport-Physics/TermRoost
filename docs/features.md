# Features

Everything is saved in SQLite as it changes: projects, sessions, split layouts, focus, tasks, notes, window geometry, terminal scrollback and the directory each terminal's shell is in. After a crash or a normal close, the app reopens as you left it (see [Restoring](restoring.md)).

## Projects and sessions

- **Projects** on the left, each with a working directory. Hover over a project for a button that copies its path.
- **Sessions** per project: vertical, renamable (double-click). Each session holds terminals in horizontal/vertical **splits**.
- **Search** (Ctrl+Shift+P) above the projects list shows only the projects whose name or folder contains each word you type, ignoring case. **Enter** opens the first one and brings the whole list back; **Escape** empties the search and goes back to the terminal, **✕** just empties it. While you search, the next/previous project shortcuts go through the projects shown, and moving a project (by dragging or with the shortcuts) moves it among them.
- **Reordering:** drag a project or session up or down its list; a line shows where it will land. Dropping it outside the list cancels. The [move shortcuts](shortcuts.md) do the same from the keyboard.

## Files

The session's folder is in the right panel's **Files** tab (Ctrl+Shift+F). Click a folder to open or close it, and a file to show it in place of the session's terminals (a strip above them switches between the terminals and the open files). What git ignores (`.gitignore`, `.git/info/exclude`, your global excludes) is dimmed; `.git` itself is not listed. The tree shows changes on disk as they happen; a folder the system can't watch is read again every few seconds instead.

**+** at the top makes a new file in the session's folder: type its name, then Enter creates and opens it (Escape cancels). A name like `docs/notes.md` makes the folders too. Right-click an entry, or the folder name at the top, for **New file** in that folder (next to a file, for a file) and **Show in Explorer** (on Linux, your default file manager: it selects the file if the file manager supports that, else opens its folder).

### Editing

A file opened from the Files tab can be edited where it is shown. **Ctrl+S** saves, **Ctrl+Z** undoes and **Ctrl+Y** (or Ctrl+Shift+Z) redoes; **Tab** and **Shift+Tab** indent and outdent the way the file already does (tabs or its number of spaces). Code is colored by its language, known by the file's extension. Saving keeps the file's line endings (LF or CRLF) and its byte order mark. A ● on the file's tab marks unsaved changes; closing the file, deleting its session or quitting asks first. Files larger than 5 MB, binary files and files that aren't UTF-8 don't open. Open files come back after a restart (their unsaved changes don't).

**Files that change on disk** (say, an agent edits one you have open) are noticed within a few seconds, and when you come back to the window. Without unsaved changes, the file just shows the new text (undo brings back the old). With unsaved changes, a bar under the text offers **Reload** or **Keep mine**; a deleted file offers to be saved again. Saving over a file that changed since you opened it asks first.

### Pictures

PNG, JPEG, GIF, WebP, BMP, ICO, TIFF, TGA, QOI and PNM pictures, and SVGs, open in a viewer instead of the editor (also when files open in Neovim). A picture starts fit to the space, never enlarged (an SVG is drawn as large as fits). The **wheel** zooms where the pointer is, stopping at 100% and at the fit on the way; **drag** to move a zoomed picture; a **double click** zooms in, or back to the fit. **Fit** and **100%** under the picture do the same. A checkerboard shows through transparent parts, and pixels show as squares when enlarged. A picture that changes on disk is shown again at the same zoom. A GIF shows its first frame; pictures larger than 64 MB don't open.

An SVG can be edited: **Edit as text** under it opens its text (in Neovim, if files open there), and **Show picture** under the saved text shows it again.

### Neovim

In **Settings → Editor**, *Edit files with: Neovim or Vim* opens files in Neovim (or Vim, if there is no Neovim) running in a terminal in the same place, with your own configuration. Quitting it (`:q`) closes the file. Neovim hears when you look at it and when you look away, so with `autoread` it picks up changes agents make. A different editor command can be set there; the file's path is added at the end. Ctrl+Shift shortcuts and Alt+arrows stay with Terminal Rooster.

Closing the file quits Neovim or Vim as `:qa` would, without asking: with changes not saved it stays open and says so (in the background, it comes to the front). Another editor is ended. Quitting Terminal Rooster asks first.

- **Getting Neovim on Windows:** without a Neovim on PATH, **Settings → Editor → Neovim → Install and use** downloads a pinned Neovim release (the official zip), checks its SHA-256 and unpacks it with Windows' own `tar.exe` into `%LOCALAPPDATA%\TerminalRooster\data\nvim`, like [the bash download](installing.md#bash-on-windows). Nothing outside that folder changes; delete it to remove it. A Neovim of your own on PATH is used first.
- **On Linux**, install Neovim with your package manager (`sudo apt install neovim`, `sudo dnf install neovim`, `sudo pacman -S neovim`); Settings says so when it finds none.

## Tasks

Tasks per project are in the right panel's **Tasks** tab (Ctrl+Shift+K). You can mark them done and link each one to a session, with its dropdown or by dragging the task onto a session in the list or onto the terminals on screen. Sessions show a badge with their open-task count.

## Notes

The right panel's **Notes** tab (Ctrl+Shift+J) holds text to keep at hand, such as something pasted to use again later. Every project shows the same notes. **+** adds a note, a double-click on its tab renames it, **✕** closes it and **Close all** closes every note, after asking. A note keeps its text, across restarts too, until it is closed; then the text is gone. With no notes, typing or pasting into the empty page starts one. **Ctrl+Z** undoes and **Ctrl+Y** (or Ctrl+Shift+Z) redoes.

## Git

Each session shows the branch its directory is on.

### Worktrees

**Worktrees** (in the bar over a session's terminals, or the branch icon in the sessions header) lists the repository's worktrees; **Switch** moves the session to one, **New session** opens another session in it, and **Remove** deletes one (after asking).

Removing closes the sessions working in the worktree, waits for their programs to exit (Windows cannot delete a folder a program is still in), and keeps the branch; uncommitted changes are only deleted when you tick **Delete them too**. Other programs working in the folder are listed, and it is only deleted once you tick **End them**. If something still keeps part of the folder, git drops the worktree anyway, and **Delete the rest** deletes what is left.

Its **New worktree** part runs `git worktree add` beside the repository, so on the same drive (`app` → `app-feature-x`), and opens the worktree in a new session named after the branch, or moves this session there when **Move … into it** is checked. When a session moves, idle terminals restart in the worktree with their scrollback kept. Terminals running a program or an agent are left alone.

### Log

For a session in a repository, the right panel has a **Git** tab between **Tasks** and **Files**. It lists the latest 200 commits of the session's branch, or of every branch with **All branches**: subject, id, author, age, and the branches and tags on each. New commits show up on their own within about two seconds. Between commits it only reads the files under `.git` that say where branches and tags point; it runs git only when one of them moved.

Commits that are on no remote yet are marked **not pushed**, and a line says how far the branch is from its upstream ("2 commits to push · 1 to pull (origin/main)", as of the last fetch). **Pull** (with **Rebase**, on by default, which replays your commits on top of what came in instead of merging) and **Push** run git for you; a branch without an upstream is pushed to `origin` and tracks it from then on. git gets 5 minutes for either, so a login prompt it cannot show won't leave it waiting forever.

### Commits and changes

Click a commit in the log to see what it changed, in place of the terminals: its message, author, date and id, then each file it changed with its hunks.

**Uncommitted changes** at the top of the log opens what isn't committed yet, as git sees it: merge conflicts, staged changes, unstaged changes and untracked files. Staged files start folded, so they read as a list of what the next commit takes, right under its message. Click a file's or a section's title to fold or unfold it; **Collapse all** folds every file (those that change later too) and **Expand all** unfolds them. Stage or unstage a whole file, a hunk, or chosen lines of a hunk (click or drag over them, Shift for a range, Ctrl to add or take out one). Then write the message above the changes and **Commit** (or Ctrl+Enter). **Amend last commit** puts the last commit's message in the box, and the button then amends.
