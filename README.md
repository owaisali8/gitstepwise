<p align="center">
  <img src="assets/logo.png" alt="Gitstepwise logo" width="112">
</p>

<h1 align="center">Gitstepwise</h1>

<p align="center">
  A small code editor for reading what changed, one step at a time, before you commit it.
</p>

---

A coding agent can rewrite a dozen files in a minute. Gitstepwise is a lightweight
editor, built in Rust on [GPUI Kit](https://crates.io/crates/gpui-kit), that lets you
review that work the way it happened: file by file, or edit by edit in the order it
was made. Step through until you've seen everything, then commit.

## Features

**Step-by-step review**

- A **Changes** tab with three ways to read your uncommitted work:
  - **Timeline**: every edit in the order it happened. If a file is edited, then
    another, then the first one again, that's three steps.
  - **By file**: each changed file against the last commit, one at a time.
  - **All files**: every changed file in one list.
- **Next file** (or **Next edit**) moves on and marks what you just read as
  reviewed, so you can stop and pick up where you left off.
- Unified or side-by-side diffs. Jump between changes inside a file, and open a
  file at the changed line with a double-click.

**Edit timeline**

- A file watcher records each write to the repository, whoever made it: you, an
  agent, a formatter or a script.
- Quick bursts of writes to the same file (within 2 seconds) become one step.
- Steps say when they were saved in the editor, or happened while it was closed.
- The timeline survives restarts and starts afresh after each commit.

**Git in the editor**

- Gutter marks for added (green), modified (blue) and deleted (red) lines, also
  shown on the scrollbar. They update as you type.
- The branch and number of changed files in the status bar.
- **Commit history** (View → Commit History, or History in the Changes tab): the
  commits on the branch, newest first, and what each one changed. Right-click a
  file or folder and choose **View History** for only the commits that touched it.

**Branches, as in GitHub Desktop**

- **Current branch** in the toolbar opens the list of branches: the default
  branch, recent ones, then the rest, with how long ago each was last
  committed to. Type to filter it, and click a branch, or press Enter, to
  switch to it. A branch that's only on the remote gets a local branch that
  tracks it.
- **New branch** (or Ctrl+Shift+N) creates a branch from the current one, or
  from the default branch, and switches to it. Spaces and other characters Git
  won't take become hyphens.
- Switching with changes not committed asks whether to bring them along, or
  leave them on the branch they were made on. Changes left behind are stashed,
  and the list offers to restore them when you come back.
- **Choose a branch to merge into…** at the bottom of the list (or
  Ctrl+Shift+M) shows how many commits a merge would bring and whether files
  would conflict, then merges. After conflicts, resolve the files and commit,
  or use **Abort Merge** in the list.
- Next to it, **Fetch origin** checks the remote for new commits and says when
  it last did. It becomes **Pull origin** when the remote has commits the
  branch doesn't, **Push origin** when the branch has commits the remote
  doesn't, and **Publish branch** for a branch that isn't on the remote yet.
  Pulling merges, unless Git is set to rebase.
- The **Pull requests** tab opens the repository's pull requests, or a new
  one for the current branch, on GitHub or GitLab, in your browser.
- The **Branch** menu has all of these too.

**Projects**

- Open a folder and it becomes a project. Switch to another, as in Zed, with
  **Current project** in the toolbar, File → Open Recent…, or Ctrl+Alt+O:
  type to narrow the list, then Enter.
- Each project remembers the files open in it. They open again when you come
  back to it, or when Gitstepwise starts without a folder or file to open.
- A project's terminals keep running while another project shows, and its edit
  timeline keeps recording.
- Each project has its own **notes and to-do list**, in a panel at the right
  (View → Notes and To-do, the to-do icon in the status bar, or Ctrl+Alt+B).
  Type a to-do and press Enter; tick it off when it's done. The status bar
  counts the to-dos left. They're kept with the list of projects, not in the
  project, so they never show up as changes.
- The list is kept in `projects.json` in your config folder
  (`%APPDATA%\Gitstepwise` on Windows).

**Credentials**

- A vault for API keys, passwords, server logins and any other secret, in
  place of a text file (View → Credentials, the key icon in the status bar, or
  Ctrl+Alt+K). It belongs to no project: every project sees the same one.
- The first time, choose a master password. The vault is encrypted with it, and
  locks again after 15 minutes without use, or with **Lock**.
- Type to find a credential by its name, username, host or notes. Enter copies
  its password and closes the vault; Ctrl+Enter copies the username. A copied
  password is cleared from the clipboard after 30 seconds.
- Each credential has a name, a username, a password or key, a host or URL, and
  notes. Only the name is needed.
- The vault is `credentials.vault`, next to `projects.json`. Back it up like any
  file: without the master password it can't be read. If you forget the master
  password, there's no way to get the credentials back.

**The explorer**

- Icons for each kind of file, in their own colors.
- Right-click a file or folder for New File and New Folder, Rename, Duplicate,
  Cut, Copy and Paste, Move to Recycle Bin (or Trash) and Delete, Reveal in File
  Explorer, Open in Default App, Open Preview (for Markdown files), Open in
  Terminal, Copy Path and Copy Relative Path, Add to `.gitignore` or `.git/info/exclude`, View History, and Open or
  Copy File Permalink (for GitHub and GitLab remotes).

**The editor itself**

- Tabs, the file explorer, and an integrated terminal. The terminal icon at the
  right end of the status bar shows or hides it.
- Split the terminal side by side or one above the other, as many times as you
  like, and drag the dividers to resize. Use the buttons in the terminal's
  header, View → Split Terminal Right or Down, or the keys below.
- Syntax highlighting for Rust, TOML, Markdown, Python, JavaScript, TypeScript,
  HTML, CSS, JSON, YAML and shell scripts.
- A **Markdown preview** (View → Markdown Preview, or the eye button at the
  right of the tabs): the file rendered, in place of its source or beside it,
  updating as you type. Links to other files open them in a tab, and images
  stored with the file are shown (images on the web aren't yet).
- Word wrap, line numbers, zoom, and light and dark themes.
- Its own title bar with File, Edit, Branch, View and Help menus, and a
  toolbar under it, as in GitHub Desktop: the current project, the current
  branch, and Fetch, Pull or Push.

**Updates**

- The Windows app keeps itself up to date. A few seconds after it starts, and
  every six hours after that, it looks for a newer release on GitHub. If there
  is one, it downloads it in the background and checks it against the size and
  SHA-256 digest GitHub lists for it.
- **Restart to update** then shows in the status bar. Click it, or choose Help →
  Restart to Update, to start the new version; it asks about unsaved changes
  first. Otherwise the new version runs from the next start.
- **Help → Check for Updates…** looks straight away, and asks before installing.
- Turn off **Help → Update Automatically** to only check when you ask. The
  setting is kept in `updates.json` in the app's config folder.
- Copies built with `cargo build` (debug builds) never replace themselves. They
  only say when there's a newer release.

## Getting started

On Windows, the quickest way is to download the latest
`gitstepwise-vX.Y.Z-windows-x64.exe` from the
[Releases page](https://github.com/owaisali8/gitstepwise/releases) and run it. It
needs [Git](https://git-scm.com/) on your `PATH`, and updates itself from then on.
Put it in a folder you can write to (not `Program Files`), so it can replace
itself.

To build it yourself, you need:

- A recent stable [Rust toolchain](https://rustup.rs/) (Gitstepwise uses the 2024
  edition).
- [Git](https://git-scm.com/) on your `PATH`. Gitstepwise runs the `git` command
  to read your repository.
- On Windows, the MSVC build tools (installed with Visual Studio or the Build Tools
  for Visual Studio), which Rust uses there anyway.
- On Linux, the development libraries for xcb, FreeType and xkbcommon. On Debian
  and Ubuntu: `sudo apt install libxcb1-dev libfreetype-dev libxkbcommon-dev libxkbcommon-x11-dev`.

Then:

```sh
git clone https://github.com/owaisali8/gitstepwise.git
cd gitstepwise
cargo run --release
```

Open a folder or files from the command line:

```sh
cargo run --release -- path/to/your/repo        # show a folder in the explorer
cargo run --release -- src/main.rs README.md    # open files in tabs
```

A path that doesn't exist yet opens as a new, empty file at that path. With
nothing given, the last project opens again.

Gitstepwise is developed and tested on Windows. GPUI also runs on macOS and Linux,
and the code handles both (Cmd shortcuts on macOS, your `$SHELL` in the terminal).
On Linux it builds, its tests pass and it has been run briefly under X11. It hasn't
been tried on macOS yet.

## Reviewing an agent's changes

1. Open the repository the agent works in, and leave Gitstepwise running while it
   works. Every write is added to the timeline.
2. Open **View → Edit Timeline** to go through the edits in order, or
   **View → Review Changes Step by Step** to go through them file by file.
3. Press **N**, or **Next edit** / **Next file**, to move on. Each step you leave
   is marked reviewed, and the last one says **Finish**.
4. Double-click a line, or press **Open**, to fix something in the editor.
5. Commit when you're happy. The timeline clears and starts again.

## Keyboard shortcuts

On macOS, use Cmd in place of Ctrl.

| Action | Shortcut |
| --- | --- |
| New file / Open file | Ctrl+N / Ctrl+O |
| Open folder | Ctrl+K, then Ctrl+O |
| Switch project | Ctrl+Alt+O |
| Show or hide notes and to-dos | Ctrl+Alt+B |
| Show or hide credentials | Ctrl+Alt+K |
| Branches / new branch / merge into the current branch | Ctrl+Shift+B / Ctrl+Shift+N / Ctrl+Shift+M |
| Pull / push | Ctrl+Shift+P / Ctrl+P |
| Save / Save as | Ctrl+S / Ctrl+Shift+S |
| Close tab | Ctrl+W |
| Next / previous tab | Ctrl+Tab / Ctrl+Shift+Tab (or Ctrl+PageDown / Ctrl+PageUp) |
| Show or hide the explorer | Ctrl+B |
| Focus the explorer | Ctrl+Shift+E |
| Open or close Changes | Ctrl+Shift+G |
| Show or hide the terminal | Ctrl+\` |
| Word wrap | Alt+Z |
| Markdown preview / preview to the side | Ctrl+Shift+V / Ctrl+K, then V |
| Zoom in / out / reset | Ctrl+= / Ctrl+- / Ctrl+0 |
| Quit | Ctrl+Q |

In the **Changes** tab:

| Action | Key |
| --- | --- |
| Next / previous file or step | N / P |
| Next / previous change in the diff | ] / [ |
| Mark reviewed, or not | R |

In the **explorer**, the arrow keys move and expand or collapse folders, and Enter
opens. These act on the selected file or folder:

| Action | Shortcut |
| --- | --- |
| New file / new folder here | Ctrl+N / Alt+N |
| Rename | F2 |
| Cut / copy / paste | Ctrl+X / Ctrl+C / Ctrl+V |
| Duplicate | Ctrl+D |
| Move to Recycle Bin / delete | Delete / Ctrl+Delete |
| Copy path / relative path | Alt+Shift+C / Ctrl+Alt+Shift+C |
| Reveal in File Explorer | Ctrl+Alt+R |
| Open in default app | Ctrl+Shift+Enter |

In the **History** view, N and P go to older and newer commits.

In the **list of branches** and the merge dialog, the arrow keys move through
the branches while you type, Enter switches (or merges), and Escape closes.

In **Credentials**, the arrow keys move through the list, Enter copies the
password (and closes), Ctrl+Enter copies the username, and Escape closes (or
leaves a form without saving).

In the **terminal** (as in Windows Terminal):

| Action | Shortcut |
| --- | --- |
| Split right / split down | Alt+Shift+= / Alt+Shift+- |
| Move to the terminal on the left, right, above or below | Alt+arrow keys |
| Close this terminal (ends its shell) | Ctrl+Shift+W |

## How the timeline is stored

The timeline is kept in `.git/gitstepwise/timeline.json` inside your repository, so
it never shows up as a change and is never committed. It records:

- Files Git tracks or would track. Files in `.gitignore` are skipped.
- The text of each step, for text files up to 4 MB. Larger and binary files are
  recorded as changed, without their contents.

When Gitstepwise starts, it compares the repository with the saved timeline and adds
a step for anything that changed while it was closed. After a commit, the timeline
starts again from the new commit.

## Project layout

| Path | What it holds |
| --- | --- |
| `src/main.rs` | The window: title bar and menus, tabs, editor, status bar and shortcuts |
| `src/branches.rs` | The list of branches, and the dialogs to create a branch and to merge one |
| `src/changes.rs` | The Changes tab: timeline, by-file and all-files review, and commit history |
| `src/credentials.rs` | The credentials panel: searching, copying and editing credentials |
| `src/timeline.rs` | The file watcher and the edit timeline |
| `src/vault.rs` | The encrypted credentials vault and its search |
| `src/git.rs` | Reading the repository through the `git` command, and line diffs |
| `src/git_branches.rs` | Branches through `git`: listing, switching, merging, stashing, fetch, pull and push |
| `src/explorer.rs` | The file explorer |
| `src/file_icons.rs` | The explorer's icon and color for each kind of file |
| `src/markdown.rs` | Where the Markdown preview's links and images lead |
| `src/notes.rs` | Each project's notes and to-do list, in a panel at the right |
| `src/projects.rs` | The list of projects and the picker to switch between them |
| `src/terminal.rs` | The integrated terminal |
| `src/terminal_panel.rs` | The terminal panel: split terminals and their header |
| `src/updater.rs` | Checking GitHub for a newer release, and installing it |
| `assets/` | The logo, the app icon, and `make-icon.ps1`, which rebuilds the icon |

## Development

```sh
cargo test     # unit tests
cargo build    # debug build
```

- Set `GITSTEPWISE_FPS=1` to show the frame rate in the status bar.
- To publish a release, set the new `version` in `Cargo.toml`, commit, then tag
  and push the tag: `git tag v0.2.0` and `git push origin v0.2.0`. The Release
  workflow builds the Windows app and publishes it, and the apps already out
  there update to it. The tag must match the version in `Cargo.toml`.
- After editing `assets/logo.svg` or `assets/logo-small.svg`, rebuild the Windows
  icon with `powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1` (it uses
  Microsoft Edge to render the SVGs).
