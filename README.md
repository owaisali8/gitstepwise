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

**The explorer**

- Icons for each kind of file, in their own colors.
- Right-click a file or folder for New File and New Folder, Rename, Duplicate,
  Cut, Copy and Paste, Move to Recycle Bin (or Trash) and Delete, Reveal in File
  Explorer, Open in Default App, Open Preview (for Markdown files), Open in
  Terminal, Copy Path and Copy Relative Path, Add to `.gitignore` or `.git/info/exclude`, View History, and Open or
  Copy File Permalink (for GitHub and GitLab remotes).

**The editor itself**

- Tabs, the file explorer, and an integrated terminal.
- Syntax highlighting for Rust, TOML, Markdown, Python, JavaScript, TypeScript,
  HTML, CSS, JSON, YAML and shell scripts.
- A **Markdown preview** (View → Markdown Preview, or the eye button at the
  right of the tabs): the file rendered, in place of its source or beside it,
  updating as you type. Links to other files open them in a tab, and images
  stored with the file are shown (images on the web aren't yet).
- Word wrap, line numbers, zoom, and light and dark themes.
- Its own title bar with File, Edit and View menus.

## Getting started

You need:

- A recent stable [Rust toolchain](https://rustup.rs/) (Gitstepwise uses the 2024
  edition).
- [Git](https://git-scm.com/) on your `PATH`. Gitstepwise runs the `git` command
  to read your repository.
- On Windows, the MSVC build tools (installed with Visual Studio or the Build Tools
  for Visual Studio), which Rust uses there anyway.

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

A path that doesn't exist yet opens as a new, empty file at that path.

Gitstepwise is developed and tested on Windows. GPUI also runs on macOS and Linux,
and the code handles both (Cmd shortcuts on macOS, your `$SHELL` in the terminal),
but they haven't been tried yet.

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
| `src/changes.rs` | The Changes tab: timeline, by-file and all-files review |
| `src/timeline.rs` | The file watcher and the edit timeline |
| `src/git.rs` | Reading the repository through the `git` command, and line diffs |
| `src/explorer.rs` | The file explorer |
| `src/markdown.rs` | Where the Markdown preview's links and images lead |
| `src/terminal.rs` | The integrated terminal |
| `assets/` | The logo, the app icon, and `make-icon.ps1`, which rebuilds the icon |

## Development

```sh
cargo test     # unit tests
cargo build    # debug build
```

- Set `GITSTEPWISE_FPS=1` to show the frame rate in the status bar.
- After editing `assets/logo.svg` or `assets/logo-small.svg`, rebuild the Windows
  icon with `powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1` (it uses
  Microsoft Edge to render the SVGs).
