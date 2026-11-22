# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Gitstepwise is a small code editor in Rust on [GPUI Kit](https://crates.io/crates/gpui-kit) (`gpui-kit` 0.10, which wraps `gpui-component` and `gpui-base`) for reviewing uncommitted changes — for example a coding agent's — file by file or edit by edit before committing. It is developed and tested on Windows. On Linux it builds, its tests pass, and the app has been run briefly under X11. macOS is untried.

## Commands

```sh
cargo run -- path/to/repo              # show a folder in the explorer
cargo run -- src/main.rs README.md     # open files in tabs (a missing path opens as a new file)
cargo test                             # all unit tests
cargo test relative_links              # one test, by (part of) its name
cargo test markdown::                  # one module's tests
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`, windows-latest) runs `cargo build --locked --all-targets`, `cargo test --locked` and `cargo clippy --locked --all-targets -- -D warnings`. Warnings fail CI, so keep clippy clean. After adding a dependency, run an unlocked build once so `Cargo.lock` is updated before `--locked` runs.

- Git must be on `PATH`: all repository access shells out to `git`.
- On Linux, GPUI links against xcb, freetype and xkbcommon. Without their development packages the build compiles but fails at linking (`unable to find library -lxkbcommon`). On Debian and Ubuntu, install `libxcb1-dev libfreetype-dev libxkbcommon-dev libxkbcommon-x11-dev`.
- `GITSTEPWISE_FPS=1` shows the frame rate in the status bar.
- Release builds use `windows_subsystem = "windows"`, so there is no console and `eprintln!` output isn't visible. On Windows, a running `target\release\gitstepwise.exe` can't be overwritten by a rebuild; close it first.
- `release.yml` builds and publishes a release when a `v*` tag is pushed; the tag must match the version in `Cargo.toml`.
- `build.rs` embeds `assets/icon.ico` on Windows. Rebuild the icon from the SVGs with `powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1`.
- `examples/hello.rs` is a standalone GPUI Kit sample, not part of the app.

## Architecture

It's one binary crate. `src/main.rs` holds `Workspace`, the root view, which owns everything else:

- **Documents**: one tab per open file, each with its own `EditorState`.
- **Child views**: `Explorer`, `ChangesView` and the optional `TerminalPanel`, which holds a `TerminalView` for each split terminal.
- **Repository state**: the `git::Repo` and the `Timeline`.

**How the views talk:**
- **Upward, by events:** child views emit events (`ExplorerEvent`, `ChangesEvent`, `TerminalPanelEvent`) that `Workspace` subscribes to in `Workspace::new`. The terminal panel is created on first use, so its subscription is made in `start_terminal`. Each `TerminalView` emits `TerminalEvent` to its panel.
- **Downward, by direct calls:** `Workspace` calls into child views with `entity.update(cx, ...)`.
- **Rule:** new cross-view behavior goes through an event plus a `Workspace` handler. Child views don't hold references to each other.

**Actions, keys and menus:**
- **Actions:** each module declares its own with `gpui_kit::actions!` and binds keys in its `bind_keys`. `main.rs`'s `bind_keys` calls all of them.
- **Key contexts:** each view has its own key context: `"TextEditor"` for the workspace, and `"Explorer"`, `"Changes"` and `"Terminal"` inside it. The workspace's bindings apply in every child view too.
- **Taking a key from an outer view:** a binding in a more specific context wins. Key bindings run before `on_key_down` handlers, and a handled action stops there. So a child view that wants a key an outer context binds must bind it in its own context; handling it in `on_key_down` isn't enough. That's why the terminal binds Tab (the window uses it to move focus) and Ctrl+Shift+V (`PasteClipboard`; the workspace binds it to the Markdown preview). The terminal panel's keys are bound in `"Terminal"` too, and their actions bubble up to the panel or the workspace.
- **Keystrokes:** `KeyBinding::new` needs `&'static str` and doesn't resolve `secondary-`. Build keystrokes with the `primary!` macro (`concat!`, Cmd on macOS and Ctrl elsewhere), not `format!`. It's defined separately in `main.rs` and `explorer.rs`, so a new module that needs it gets its own copy. `KeyBinding::new` panics on a keystroke it can't parse, which stops the app at startup; `terminal_panel.rs` builds its list in a function so a test can check it.
- **Menus:** the View menu's check marks come from `MenuChecks`. `sync_menus` runs on every render and reinstalls the menus only when a check changes. A new checked or disabled menu state needs a field there.

**Git (`git.rs`):**
- Every call blocks on the `git` CLI, so callers run it with `cx.background_spawn` and apply the result in `this.update(...)`.
- The repository is found from the explorer's folder or from the active file's folder.
- `check_repo` polls `.git` modification times every 1.5 s (`REPO_POLL`). That's how it notices commits, checkouts and staging made outside the app or in its terminal. It then refreshes the gutter, the Changes view, the commit history and the timeline.

**Timeline (`timeline.rs`):**
- A `notify` watcher records every write to the repository's tracked or trackable files as a step. Quick bursts of writes are merged.
- The timeline is stored in `.git/gitstepwise/timeline.json`; the legacy `.git/gpui-editor/` location is still read.
- It starts afresh when HEAD changes. Reviewed flags live on the steps.

**Changes view (`changes.rs`):**
- Modes: Timeline, ByFile, AllFiles and History.
- `Workspace::refresh_changes` computes the changed files and their diffs in the background. Unsaved buffer text counts as a change.
- History lists commits (optionally for one path) and loads each commit's patches lazily, with a small cache.

**Gutter marks:**
- Each `Document` keeps `base`, the file's committed text.
- Marks come from diffing the buffer against it with `similar`, debounced by `MARKER_DELAY`.

**Explorer (`explorer.rs`):**
- The tree is read lazily and virtualized.
- File operations are in free functions at the bottom of the file; trash uses the `trash` crate.
- While the inline name field (new, rename) is open, every explorer action must return early via `naming()`. Keys like Enter propagate from the input to the explorer's own bindings.

**Terminal panel (`terminal_panel.rs`):**
- The terminals are a binary `Tree` of splits. Each split is the kit's `h_resizable` or `v_resizable` group. Its `ResizableState` is kept in the tree, so sizes survive when the tree changes shape.
- A new shell needs the workspace's starting folder. So splitting goes through the workspace: the header's buttons emit `TerminalPanelEvent::Split`, and the `SplitRight` and `SplitDown` actions bubble up to `Workspace`. Moving between terminals and closing them happen in the panel.
- The tree logic and `neighbor` (moving by direction) are generic, so the tests use plain numbers.
- Closing the last terminal emits `Closed`, and the workspace drops the panel. Dropping a `TerminalView` ends its shell.

**Markdown preview:**
- Each `Document` has a `Preview` mode and a `TextViewState`, updated from `InputEvent::Change`.
- `markdown.rs` resolves links and images relative to the file: links starting with `/` start from the repository root (or the explorer's folder outside a repository), and `data:` images are cached.
- The `TextView` callbacks must be `Send + Sync`, so they can't capture the workspace. Link clicks reach it by dispatching the `OpenLinkedPath` action instead.

**Icons:** only part of the Lucide set ships by default. Any other `LucideIcon` must be added to the `icon_assets!(ExtraIcons, [...])` list in `main.rs`, or it renders blank. The kit's defaults are listed in `default-icons.txt` in the `gpui-kit-assets` crate.

## Working with GPUI Kit

The API differs from upstream GPUI and from older gpui-component releases. Check the crate sources in `~/.cargo/registry/src/*/gpui-component-0.10.*`, `gpui-base-0.10.*` and `gpui-pre-*` rather than relying on memory.

## Conventions

- Doc comments, UI text and README are written in short, plain sentences that describe what the user sees.
- Keep the README's feature list, shortcut tables and project layout table in step with changes.
- Unit tests live in a `tests` module at the end of each file. Clippy rejects items after the test module.
- In a file with `use gpui_kit::*`, the glob brings in GPUI's `test` attribute, which replaces `#[test]`. The test module there must import names one by one, not `use super::*` (see `explorer.rs`).
- Some tests are platform-specific (`#[cfg(windows)]` / `#[cfg(not(windows))]`), so the test count differs between Windows and other systems.
