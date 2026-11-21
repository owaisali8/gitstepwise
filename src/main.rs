//! A small text editor built on GPUI Kit, with a file explorer, tabs and a
//! terminal.
//!
//! Run with `cargo run`, or `cargo run -- file1 file2` to open files, or
//! `cargo run -- some/folder` to show that folder in the explorer.
//! A path that doesn't exist yet opens as a new, empty file at that path.

mod explorer;
mod git;
mod terminal;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::base::GlobalState;
use gpui_kit::component::button::*;
use gpui_kit::component::diff::{Diff, DiffFile, DiffFileStatus, DiffMode, DiffSide, DiffState};
use gpui_kit::component::input::{
    self as edit, Editor, EditorState, GutterDecoration, GutterDecorationCollection, InputEvent,
    OverviewRulerDecoration, OverviewRulerDecorationCollection, OverviewRulerLane, Position,
};
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use explorer::{Explorer, ExplorerEvent};
use terminal::{TerminalEvent, TerminalView};

gpui_kit::actions!(
    text_editor,
    [
        NewFile,
        OpenFile,
        OpenFolder,
        ToggleSidebar,
        ToggleChanges,
        FocusExplorer,
        SaveFile,
        SaveFileAs,
        CloseTab,
        NextTab,
        PreviousTab,
        ToggleTerminal,
        ToggleSoftWrap,
        ToggleLineNumbers,
        UseLightTheme,
        UseDarkTheme,
        Quit,
    ]
);

const APP_NAME: &str = "GPUI Editor";
const KEY_CONTEXT: &str = "TextEditor";
const PLAIN_TEXT: &str = "text";
const MIN_TERMINAL_HEIGHT: Pixels = px(80.);
const MIN_SIDEBAR_WIDTH: Pixels = px(140.);
/// How long typing pauses before the gutter's change markers update.
const MARKER_DELAY: Duration = Duration::from_millis(150);
/// How often to check the repository for commits, checkouts and staging.
const REPO_POLL: Duration = Duration::from_millis(1500);

// Icons beyond the kit's default set, embedded on top of it.
gpui_kit::assets::icon_assets!(ExtraIcons, [GitBranch]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// File extensions, the highlighter's language id, and the name shown in the
/// status bar. Each language needs its `tree-sitter-*` feature in Cargo.toml.
const LANGUAGES: &[(&[&str], &str, &str)] = &[
    (&["rs"], "rust", "Rust"),
    (&["toml"], "toml", "TOML"),
    (&["md", "markdown"], "markdown", "Markdown"),
    (&["py", "pyw"], "python", "Python"),
    (&["js", "mjs", "cjs", "jsx"], "javascript", "JavaScript"),
    (&["ts", "mts", "cts"], "typescript", "TypeScript"),
    (&["html", "htm"], "html", "HTML"),
    (&["css", "scss"], "css", "CSS"),
    (&["json", "jsonc"], "json", "JSON"),
    (&["yaml", "yml"], "yaml", "YAML"),
    (&["sh", "bash", "zsh"], "bash", "Shell"),
];

fn language_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|(exts, _, _)| exts.contains(&ext.as_str()))
        .map_or(PLAIN_TEXT, |(_, id, _)| id)
}

fn language_label(id: &str) -> &'static str {
    LANGUAGES
        .iter()
        .find(|(_, lang, _)| *lang == id)
        .map_or("Plain Text", |(_, _, label)| label)
}

/// "Ctrl+S" on Windows and Linux, "Cmd+S" on macOS.
fn shortcut(keys: &str) -> String {
    let modifier = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };
    format!("{modifier}+{keys}")
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// The settings the View menu shows check marks for.
#[derive(Clone, Copy, PartialEq)]
struct MenuChecks {
    sidebar: bool,
    changes: bool,
    terminal: bool,
    soft_wrap: bool,
    line_numbers: bool,
    dark: bool,
}

/// The menu bar's menus. Each item dispatches its action to whatever had
/// focus when the menu opened, so File and View reach the workspace and Edit
/// reaches the editor (or the terminal, for Copy and Paste).
fn app_menus(checks: MenuChecks) -> Vec<Menu> {
    vec![
        Menu::new("File").items([
            MenuItem::action("New File", NewFile),
            MenuItem::action("Open File…", OpenFile),
            MenuItem::action("Open Folder…", OpenFolder),
            MenuItem::separator(),
            MenuItem::action("Save", SaveFile),
            MenuItem::action("Save As…", SaveFileAs),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::separator(),
            MenuItem::action("Exit", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::action("Undo", edit::Undo),
            MenuItem::action("Redo", edit::Redo),
            MenuItem::separator(),
            MenuItem::action("Cut", edit::Cut),
            MenuItem::action("Copy", edit::Copy),
            MenuItem::action("Paste", edit::Paste),
            MenuItem::separator(),
            MenuItem::action("Select All", edit::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find", edit::Search),
            MenuItem::action("Replace", edit::Replace),
        ]),
        Menu::new("View").items([
            MenuItem::action("Explorer", ToggleSidebar).checked(checks.sidebar),
            MenuItem::action("Changes", ToggleChanges).checked(checks.changes),
            MenuItem::action("Terminal", ToggleTerminal).checked(checks.terminal),
            MenuItem::separator(),
            MenuItem::action("Word Wrap", ToggleSoftWrap).checked(checks.soft_wrap),
            MenuItem::action("Line Numbers", ToggleLineNumbers).checked(checks.line_numbers),
            MenuItem::separator(),
            MenuItem::action("Light Theme", UseLightTheme).checked(!checks.dark),
            MenuItem::action("Dark Theme", UseDarkTheme).checked(checks.dark),
        ]),
    ]
}

/// A step that runs once unsaved changes have been saved or discarded.
type Continuation = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// Identifies a document across tab moves and closes.
type DocId = usize;

/// One open file (or untitled buffer) and its tab.
struct Document {
    id: DocId,
    editor: Entity<EditorState>,
    path: Option<PathBuf>,
    dirty: bool,
    /// Cursor as 0-based (line, column), mirrored for the status bar.
    cursor: (u32, u32),
    /// The file's text in the last commit, when it's committed to Git. The
    /// gutter marks where the buffer differs from it.
    base: Option<Arc<str>>,
    markers: Vec<git::Marker>,
    /// The gutter strip and scrollbar marks, created once the file has a base.
    decorations: Option<(GutterDecorationCollection, OverviewRulerDecorationCollection)>,
    _base_task: Option<Task<()>>,
    _marker_task: Option<Task<()>>,
    _subscriptions: [Subscription; 2],
}

impl Document {
    fn name(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(|| "Untitled".into(), |name| name.to_string_lossy().into_owned())
    }

    /// An untitled tab nobody has typed into, which opening a file reuses.
    fn is_pristine(&self, cx: &App) -> bool {
        self.path.is_none() && !self.dirty && self.editor.read(cx).text().len() == 0
    }
}

struct Status {
    message: SharedString,
    is_error: bool,
}

/// Opt-in frame-rate meter. Run with `GPUI_EDITOR_FPS=1` and the window
/// redraws every frame, showing the measured rate in the status bar (and
/// printing it to stderr once a second).
struct FpsMeter {
    frames: u32,
    second_started: Instant,
    last_frame: Instant,
    slowest: Duration,
    /// The last full second: frames per second and the longest gap.
    reading: Option<(f32, Duration)>,
}

impl FpsMeter {
    fn from_env() -> Option<Self> {
        std::env::var_os("GPUI_EDITOR_FPS")?;
        let now = Instant::now();
        Some(Self {
            frames: 0,
            second_started: now,
            last_frame: now,
            slowest: Duration::ZERO,
            reading: None,
        })
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.slowest = self.slowest.max(now - self.last_frame);
        self.last_frame = now;
        self.frames += 1;
        let elapsed = now - self.second_started;
        if elapsed >= Duration::from_secs(1) {
            let fps = self.frames as f32 / elapsed.as_secs_f32();
            eprintln!(
                "fps: {fps:.1}  slowest frame: {:.2} ms",
                self.slowest.as_secs_f64() * 1000.
            );
            self.reading = Some((fps, self.slowest));
            self.frames = 0;
            self.second_started = now;
            self.slowest = Duration::ZERO;
        }
    }

    fn label(&self) -> String {
        match self.reading {
            Some((fps, slowest)) => format!(
                "{fps:.0} fps (slowest {:.1} ms)",
                slowest.as_secs_f64() * 1000.
            ),
            None => "measuring fps…".into(),
        }
    }
}

/// A divider being dragged, with the pointer position and panel size at the
/// start of the drag.
#[derive(Clone, Copy)]
enum Resize {
    Terminal { start_y: Pixels, start_height: Pixels },
    Sidebar { start_x: Pixels, start_width: Pixels },
}

struct Workspace {
    documents: Vec<Document>,
    active: usize,
    next_id: DocId,
    soft_wrap: bool,
    line_numbers: bool,
    status: Option<Status>,
    explorer: Entity<Explorer>,
    sidebar_visible: bool,
    sidebar_width: Pixels,
    terminal: Option<Entity<TerminalView>>,
    terminal_visible: bool,
    terminal_height: Pixels,
    resizing: Option<Resize>,
    fps: Option<FpsMeter>,
    menu_bar: Entity<AppMenuBar>,
    /// What the installed menus' check marks show; `None` before the first install.
    menu_checks: Option<MenuChecks>,
    /// The Git repository of the explorer's folder, if it's in one.
    repo: Option<git::Repo>,
    /// Last seen modification times in `.git`, to notice commits and staging.
    repo_stamp: Option<Vec<Option<SystemTime>>>,
    branch: Option<SharedString>,
    changed_files: usize,
    /// Every changed file in the repository, shown in place of the editor.
    changes: Entity<DiffState>,
    changes_visible: bool,
    changes_loaded: bool,
    /// The file to scroll the Changes view to once it has loaded.
    changes_scroll_to: Option<String>,
    _repo_task: Option<Task<()>>,
    _changes_task: Option<Task<()>>,
    _repo_watch: Task<()>,
    _terminal_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The explorer shows the first folder given, else the first file's
        // folder, else the current directory.
        let (folders, files): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|path| path.is_dir());
        let root = folders
            .first()
            .cloned()
            .or_else(|| {
                files
                    .first()
                    .and_then(|file| std::path::absolute(file).ok())
                    .and_then(|file| file.parent().map(Path::to_path_buf))
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();

        let explorer = cx.new(|cx| Explorer::new(root, cx));
        let subscriptions = vec![
            cx.subscribe_in(&explorer, window, Self::on_explorer_event),
            // Pick up files created elsewhere when the user comes back.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.explorer.update(cx, |explorer, cx| explorer.refresh(cx));
                    // Files may have been committed or changed elsewhere too.
                    this.refresh_git(cx);
                }
            }),
            // The gutter's marker colors come from the theme.
            cx.observe_global::<Theme>(|this, cx| {
                for ix in 0..this.documents.len() {
                    this.paint_markers(ix, cx);
                }
            }),
        ];
        let changes = cx.new(|cx| DiffState::new(Vec::new(), cx));
        // Notice commits, checkouts and staging done outside the editor, or
        // in its terminal.
        let repo_watch = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REPO_POLL).await;
                if this.update(cx, |this, cx| this.check_repo(cx)).is_err() {
                    break;
                }
            }
        });

        let mut this = Self {
            documents: Vec::new(),
            active: 0,
            next_id: 0,
            soft_wrap: true,
            line_numbers: true,
            status: None,
            explorer,
            sidebar_visible: true,
            sidebar_width: px(240.),
            terminal: None,
            terminal_visible: false,
            terminal_height: px(260.),
            resizing: None,
            fps: FpsMeter::from_env(),
            menu_bar: AppMenuBar::new(cx),
            menu_checks: None,
            repo: None,
            repo_stamp: None,
            branch: None,
            changed_files: 0,
            changes,
            changes_visible: false,
            changes_loaded: false,
            changes_scroll_to: None,
            _repo_task: None,
            _changes_task: None,
            _repo_watch: repo_watch,
            _terminal_subscription: None,
            _subscriptions: subscriptions,
        };
        this.sync_menus(cx);

        for path in files {
            this.open_path(path, window, cx);
        }
        if this.documents.is_empty() {
            this.add_document(None, String::new(), window, cx);
        }

        this.install_close_guard(window, cx);
        this.activate(this.active, window, cx);
        this.refresh_repo(cx);
        this
    }

    // ---- Explorer ----------------------------------------------------------

    fn on_explorer_event(
        &mut self,
        _: &Entity<Explorer>,
        event: &ExplorerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ExplorerEvent::OpenFile(path) => self.open_path(path.clone(), window, cx),
        }
    }

    /// Tell the explorer which file the active tab shows.
    fn sync_explorer(&self, cx: &mut Context<Self>) {
        let path = self.documents.get(self.active).and_then(|doc| doc.path.clone());
        self.explorer
            .update(cx, |explorer, cx| explorer.set_active_file(path.as_deref(), cx));
    }

    fn show_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.explorer
            .update(cx, |explorer, cx| explorer.set_root(folder, cx));
        self.sidebar_visible = true;
        self.refresh_repo(cx);
        cx.notify();
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| this.show_folder(folder, cx)).ok();
        })
        .detach();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        if !self.sidebar_visible && self.explorer.read(cx).focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    fn focus_explorer(&mut self, _: &FocusExplorer, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.explorer.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// The folder to use when there's no better one: the explorer's.
    fn default_folder(&self, cx: &App) -> PathBuf {
        self.explorer.read(cx).root().to_path_buf()
    }

    // ---- Documents and tabs ------------------------------------------------

    fn index_of(&self, id: DocId) -> Option<usize> {
        self.documents.iter().position(|doc| doc.id == id)
    }

    fn active_document(&self) -> &Document {
        &self.documents[self.active]
    }

    /// Add a tab for `text` and return its index (it isn't activated).
    fn add_document(
        &mut self,
        path: Option<PathBuf>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let language = path.as_deref().map_or(PLAIN_TEXT, language_for);
        let (soft_wrap, line_numbers) = (self.soft_wrap, self.line_numbers);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .line_number(line_numbers)
                .soft_wrap(soft_wrap)
        });
        editor.update(cx, |state, cx| state.set_value(text, window, cx));

        let subscriptions = [
            cx.subscribe_in(&editor, window, Self::on_editor_event),
            // Moving the cursor emits no event, so watch for redraws.
            cx.observe(&editor, Self::on_editor_changed),
        ];
        let id = self.next_id;
        self.next_id += 1;
        self.documents.push(Document {
            id,
            editor,
            path,
            dirty: false,
            cursor: (0, 0),
            base: None,
            markers: Vec::new(),
            decorations: None,
            _base_task: None,
            _marker_task: None,
            _subscriptions: subscriptions,
        });
        self.load_base(id, cx);
        self.documents.len() - 1
    }

    /// Replace a tab's contents with a file's.
    fn load_into(
        &mut self,
        ix: usize,
        path: PathBuf,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let language = language_for(&path);
        let doc = &mut self.documents[ix];
        doc.editor.update(cx, |state, cx| {
            state.set_highlighter(language, cx);
            state.set_value(text, window, cx);
        });
        doc.path = Some(path);
        doc.dirty = false;
        doc.cursor = (0, 0);
        let id = doc.id;
        self.load_base(id, cx);
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.documents.len() {
            return;
        }
        self.active = ix;
        // Switching to a file leaves the Changes view.
        self.changes_visible = false;
        self.focus_active(window, cx);
        self.update_title(window);
        self.sync_explorer(cx);
        cx.notify();
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.active_document().editor.clone();
        editor.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Open a file in a tab, or switch to it if it's already open. A folder
    /// is shown in the explorer instead.
    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.show_folder(path, cx);
            return;
        }

        let already_open = self.documents.iter().position(|doc| {
            doc.path
                .as_deref()
                .is_some_and(|open| same_file(open, &path))
        });
        if let Some(ix) = already_open {
            self.activate(ix, window, cx);
            return;
        }

        if !path.exists() {
            let ix = self.add_document(Some(path), String::new(), window, cx);
            self.activate(ix, window, cx);
            return;
        }

        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let message = format!("Opened {}", path.display());
                let reuse_active = self
                    .documents
                    .get(self.active)
                    .is_some_and(|doc| doc.is_pristine(cx));
                let ix = if reuse_active {
                    self.load_into(self.active, path, text, window, cx);
                    self.active
                } else {
                    self.add_document(Some(path), text, window, cx)
                };
                self.activate(ix, window, cx);
                self.set_status(message, false, cx);
            }
            Err(err) => {
                self.set_status(format!("Couldn't open {}: {err}", path.display()), true, cx);
            }
        }
    }

    /// Close a tab, asking first if it has unsaved changes.
    fn close_document(&mut self, id: DocId, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_discard(
            id,
            window,
            cx,
            Box::new(move |this, window, cx| {
                let Some(ix) = this.index_of(id) else {
                    return;
                };
                this.documents.remove(ix);
                if this.documents.is_empty() {
                    this.add_document(None, String::new(), window, cx);
                }
                let active = if ix < this.active {
                    this.active - 1
                } else {
                    this.active.min(this.documents.len() - 1)
                };
                this.activate(active, window, cx);
            }),
        );
    }

    fn on_editor_event(
        &mut self,
        editor: &Entity<EditorState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let Some(ix) = self.documents.iter().position(|doc| &doc.editor == editor) else {
            return;
        };
        self.status = None;
        if !self.documents[ix].dirty {
            self.documents[ix].dirty = true;
            if ix == self.active {
                self.update_title(window);
            }
        }
        let id = self.documents[ix].id;
        self.schedule_markers(id, MARKER_DELAY, cx);
        cx.notify();
    }

    fn on_editor_changed(&mut self, editor: Entity<EditorState>, cx: &mut Context<Self>) {
        let Some(ix) = self.documents.iter().position(|doc| doc.editor == editor) else {
            return;
        };
        let position = editor.read(cx).cursor_position();
        let cursor = (position.line, position.character);
        if self.documents[ix].cursor != cursor {
            self.documents[ix].cursor = cursor;
            if ix == self.active {
                cx.notify();
            }
        }
    }

    /// "● name — GPUI Editor", with the dot when there are unsaved changes.
    fn window_title(&self) -> String {
        let Some(doc) = self.documents.get(self.active) else {
            return APP_NAME.into();
        };
        let marker = if doc.dirty { "● " } else { "" };
        format!("{marker}{} — {APP_NAME}", doc.name())
    }

    /// The title bar draws the title itself; this keeps the taskbar's in step.
    fn update_title(&self, window: &mut Window) {
        window.set_window_title(&self.window_title());
    }

    /// Reinstall the menus when one of their check marks would change.
    fn sync_menus(&mut self, cx: &mut Context<Self>) {
        let checks = MenuChecks {
            sidebar: self.sidebar_visible,
            changes: self.changes_visible,
            terminal: self.terminal_visible,
            soft_wrap: self.soft_wrap,
            line_numbers: self.line_numbers,
            dark: cx.theme().is_dark(),
        };
        if self.menu_checks == Some(checks) {
            return;
        }
        self.menu_checks = Some(checks);
        let menus = app_menus(checks).into_iter().map(Menu::owned).collect();
        GlobalState::global_mut(cx).set_app_menus(menus);
        self.menu_bar.update(cx, |menu_bar, cx| menu_bar.reload(cx));
    }

    fn set_status(&mut self, message: impl Into<SharedString>, is_error: bool, cx: &mut Context<Self>) {
        self.status = Some(Status {
            message: message.into(),
            is_error,
        });
        cx.notify();
    }

    // ---- Saving and unsaved changes ---------------------------------------

    /// Write a document to `path`. Returns whether it was saved.
    fn write_to(&mut self, id: DocId, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.index_of(id) else {
            return false;
        };
        let editor = self.documents[ix].editor.clone();
        let text = editor.read(cx).value();
        if let Err(err) = std::fs::write(&path, text.as_bytes()) {
            self.set_status(format!("Couldn't save {}: {err}", path.display()), true, cx);
            return false;
        }

        // "Save As" with a new extension switches the highlighting.
        let language = language_for(&path);
        if editor.read(cx).language_name() != language {
            editor.update(cx, |state, cx| state.set_highlighter(language, cx));
        }

        let message = format!("Saved {}", path.display());
        let doc = &mut self.documents[ix];
        doc.path = Some(path);
        doc.dirty = false;
        self.update_title(window);
        self.set_status(message, false, cx);
        // A "Save As" may have created a file the explorer should list.
        self.explorer.update(cx, |explorer, cx| explorer.refresh(cx));
        self.sync_explorer(cx);
        // A "Save As" may also have moved the file in or out of Git.
        self.load_base(id, cx);
        self.refresh_changes(cx);
        true
    }

    /// Save to the document's path, or ask for one when it's untitled or
    /// `save_as` is set. `then` runs only if the file was saved.
    fn save_document(
        &mut self,
        id: DocId,
        save_as: bool,
        then: Option<Continuation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.index_of(id) else {
            return;
        };
        let current = self.documents[ix].path.clone();
        if let (Some(path), false) = (current.clone(), save_as) {
            if self.write_to(id, path, window, cx) {
                if let Some(then) = then {
                    then(self, window, cx);
                }
            }
            return;
        }

        let directory = current
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.default_folder(cx));
        let suggested_name = match current {
            Some(_) => self.documents[ix].name(),
            None => "untitled.txt".to_string(),
        };
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested_name));

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                if this.write_to(id, path, window, cx) {
                    if let Some(then) = then {
                        then(this, window, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Run `then` right away if the document has no unsaved changes.
    /// Otherwise ask whether to save them first, and run it unless the user
    /// cancels.
    fn confirm_discard(
        &mut self,
        id: DocId,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: Continuation,
    ) {
        let Some(ix) = self.index_of(id) else {
            return;
        };
        if !self.documents[ix].dirty {
            then(self, window, cx);
            return;
        }

        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Save changes to {}?", self.documents[ix].name()),
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );

        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| match answer {
                0 => this.save_document(id, false, Some(then), window, cx),
                1 => then(this, window, cx),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Go through every tab with unsaved changes, one at a time, then run
    /// `then`. Cancelling any of the prompts stops the whole thing.
    fn confirm_close_all(&mut self, window: &mut Window, cx: &mut Context<Self>, then: Continuation) {
        let Some(ix) = self.documents.iter().position(|doc| doc.dirty) else {
            then(self, window, cx);
            return;
        };
        let id = self.documents[ix].id;
        self.activate(ix, window, cx);
        self.confirm_discard(
            id,
            window,
            cx,
            Box::new(move |this, window, cx| {
                // Saved or discarded: either way it no longer holds up closing.
                if let Some(ix) = this.index_of(id) {
                    this.documents[ix].dirty = false;
                }
                this.confirm_close_all(window, cx, then);
            }),
        );
    }

    /// Ask about unsaved changes when the window's close button is clicked.
    fn install_close_guard(&self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let Some(this) = this.upgrade() else {
                return true;
            };
            this.update(cx, |this, cx| {
                if !this.documents.iter().any(|doc| doc.dirty) {
                    return true;
                }
                this.confirm_close_all(window, cx, Box::new(|_, window, _| window.remove_window()));
                false
            })
        });
    }

    // ---- Git ---------------------------------------------------------------

    /// Find the repository of the explorer's folder (or else the active
    /// file's), then load its changes.
    fn refresh_repo(&mut self, cx: &mut Context<Self>) {
        let mut folders = vec![self.default_folder(cx)];
        if let Some(folder) = self
            .documents
            .get(self.active)
            .and_then(|doc| doc.path.as_deref())
            .and_then(Path::parent)
        {
            folders.push(folder.to_path_buf());
        }
        self._repo_task = Some(cx.spawn(async move |this, cx| {
            let repo = cx
                .background_spawn(async move { folders.iter().find_map(|folder| git::Repo::find(folder)) })
                .await;
            this.update(cx, |this, cx| {
                if this.repo != repo {
                    this.repo_stamp = repo.as_ref().map(git::Repo::stamp);
                    this.repo = repo;
                    this.changes_loaded = false;
                }
                this.refresh_changes(cx);
            })
            .ok();
        }));
    }

    /// Reload everything when `.git` shows a commit, checkout or staging.
    fn check_repo(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = &self.repo else {
            return;
        };
        let stamp = Some(repo.stamp());
        if self.repo_stamp != stamp {
            self.repo_stamp = stamp;
            self.refresh_git(cx);
        }
    }

    /// Re-read every open file's committed version, and the changed files.
    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<DocId> = self.documents.iter().map(|doc| doc.id).collect();
        for id in ids {
            self.load_base(id, cx);
        }
        self.refresh_changes(cx);
    }

    /// Read a document's committed version in the background, then mark
    /// where the buffer differs from it.
    fn load_base(&mut self, id: DocId, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(id) else {
            return;
        };
        let doc = &mut self.documents[ix];
        let Some(path) = doc.path.clone() else {
            self.set_base(id, None, cx);
            return;
        };
        doc._base_task = Some(cx.spawn(async move |this, cx| {
            let base = cx
                .background_spawn(async move { git::committed_text(&path) })
                .await;
            this.update(cx, |this, cx| this.set_base(id, base.map(Arc::from), cx))
                .ok();
        }));
    }

    fn set_base(&mut self, id: DocId, base: Option<Arc<str>>, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(id) else {
            return;
        };
        let doc = &mut self.documents[ix];
        if base.is_none() {
            // Not committed: no gutter strip at all.
            doc.base = None;
            doc.markers.clear();
            doc._marker_task = None;
            if let Some((gutter, ruler)) = doc.decorations.take() {
                gutter.dispose(cx);
                ruler.dispose(cx);
            }
            return;
        }
        doc.base = base;
        if doc.decorations.is_none() {
            doc.decorations = Some(doc.editor.update(cx, |state, cx| {
                (
                    state.create_gutter_decorations_collection(Vec::new(), cx),
                    state.create_overview_ruler_decorations_collection(Vec::new(), cx),
                )
            }));
        }
        self.schedule_markers(id, Duration::ZERO, cx);
    }

    /// Diff the buffer against its committed version after `delay`, off the
    /// main thread. A newer call replaces a pending one.
    fn schedule_markers(&mut self, id: DocId, delay: Duration, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(id) else {
            return;
        };
        let doc = &mut self.documents[ix];
        let Some(base) = doc.base.clone() else {
            return;
        };
        doc._marker_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let Ok(Some(text)) = this.update(cx, |this, cx| {
                let ix = this.index_of(id)?;
                Some(this.documents[ix].editor.read(cx).value().to_string())
            }) else {
                return;
            };
            let markers = cx
                .background_spawn(async move { git::markers(&base, &text) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(ix) = this.index_of(id) {
                    this.documents[ix].markers = markers;
                    this.paint_markers(ix, cx);
                }
            })
            .ok();
        }));
    }

    /// Draw a document's markers in its gutter and on its scrollbar: green for
    /// added lines, blue for modified ones, red where lines were deleted.
    fn paint_markers(&self, ix: usize, cx: &mut Context<Self>) {
        let doc = &self.documents[ix];
        let Some((gutter, ruler)) = &doc.decorations else {
            return;
        };
        let theme = cx.theme();
        let (added, modified, deleted) = (theme.success, theme.info, theme.danger);
        let color = |kind| match kind {
            git::MarkerKind::Added => added,
            git::MarkerKind::Modified => modified,
            git::MarkerKind::Deleted => deleted,
        };
        let gutter_marks = doc
            .markers
            .iter()
            .map(|marker| GutterDecoration::new(marker.range.clone()).with_color(color(marker.kind)))
            .collect();
        let ruler_marks = doc
            .markers
            .iter()
            .map(|marker| {
                OverviewRulerDecoration::new(marker.range.clone())
                    .with_lane(OverviewRulerLane::Left)
                    .with_color(color(marker.kind))
            })
            .collect();
        gutter.set(gutter_marks, cx);
        ruler.set(ruler_marks, cx);
    }

    /// Reload the branch and the list of changed files. The diffs themselves
    /// are only built while the Changes view is showing.
    fn refresh_changes(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.branch = None;
            self.changed_files = 0;
            self.changes_loaded = true;
            self.changes.update(cx, |state, cx| state.set_files(Vec::new(), cx));
            cx.notify();
            return;
        };
        // Unsaved edits count as changes, as they do in the gutter.
        let unsaved: Vec<(PathBuf, String)> = self
            .documents
            .iter()
            .filter(|doc| doc.dirty)
            .filter_map(|doc| Some((doc.path.clone()?, doc.editor.read(cx).value().to_string())))
            .collect();
        let with_diffs = self.changes_visible;

        self._changes_task = Some(cx.spawn(async move |this, cx| {
            let (branch, count, patch) = cx
                .background_spawn(async move {
                    let unsaved: Vec<(String, String)> = unsaved
                        .into_iter()
                        .filter_map(|(path, text)| Some((repo.relative(&path)?, text)))
                        .collect();
                    let mut paths = repo.changed_paths();
                    paths.extend(unsaved.iter().map(|(path, _)| path.clone()));
                    paths.sort();
                    paths.dedup();
                    let patch = with_diffs.then(|| {
                        paths
                            .iter()
                            .filter_map(|path| {
                                let current = match unsaved.iter().find(|(unsaved, _)| unsaved == path) {
                                    Some((_, text)) => Some(text.clone().into_bytes()),
                                    None => std::fs::read(repo.root.join(path)).ok(),
                                };
                                git::file_patch(path, repo.committed_bytes(path), current)
                            })
                            .collect::<Vec<_>>()
                    });
                    let count = patch.as_ref().map_or(paths.len(), Vec::len);
                    (repo.branch(), count, patch.map(|patches| patches.concat()))
                })
                .await;

            this.update(cx, |this, cx| {
                this.branch = branch.map(SharedString::from);
                this.changed_files = count;
                if let Some(patch) = patch {
                    let files = match DiffFile::parse(&patch) {
                        Ok(files) => files,
                        Err(err) => {
                            this.set_status(format!("Couldn't read the changes: {}", err.message()), true, cx);
                            Vec::new()
                        }
                    };
                    let scroll_to = this.changes_scroll_to.take();
                    this.changes.update(cx, |state, cx| {
                        state.set_files(files, cx);
                        if let Some(path) = scroll_to {
                            state.scroll_to_file(&path, cx);
                        }
                    });
                    this.changes_loaded = true;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Show or hide every change since the last commit, in place of the editor.
    fn toggle_changes(&mut self, _: &ToggleChanges, window: &mut Window, cx: &mut Context<Self>) {
        if self.changes_visible {
            self.changes_visible = false;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        self.changes_visible = true;
        // Start at the file being edited.
        self.changes_scroll_to = self.repo.as_ref().and_then(|repo| {
            let path = self.documents.get(self.active)?.path.as_deref()?;
            repo.relative(path)
        });
        self.refresh_changes(cx);
        self.changes.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn set_diff_mode(&mut self, mode: DiffMode, cx: &mut Context<Self>) {
        self.changes.update(cx, |state, cx| state.set_mode(mode, cx));
    }

    /// Open a file from the Changes view, at a 1-based line if given.
    fn open_at(&mut self, path: PathBuf, line: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(path.clone(), window, cx);
        let Some(line) = line else {
            return;
        };
        let doc = self.active_document();
        if doc.path.as_deref().is_some_and(|open| same_file(open, &path)) {
            let position = Position::new(line.saturating_sub(1) as u32, 0);
            doc.editor
                .clone()
                .update(cx, |state, cx| state.set_cursor_position(position, window, cx));
        }
    }

    // ---- Terminal ----------------------------------------------------------

    fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_visible {
            self.terminal_visible = false;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }

        if self.terminal.is_none() {
            // Start in the active file's folder, else the explorer's.
            let working_directory = self
                .active_document()
                .path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.default_folder(cx));
            let working_directory = Some(working_directory);
            match terminal::start_shell(working_directory) {
                Ok(shell) => {
                    let view = cx.new(|cx| TerminalView::new(shell, cx));
                    self._terminal_subscription =
                        Some(cx.subscribe_in(&view, window, Self::on_terminal_event));
                    self.terminal = Some(view);
                }
                Err(err) => {
                    self.set_status(format!("Couldn't start a terminal: {err}"), true, cx);
                    return;
                }
            }
        }

        self.terminal_visible = true;
        if let Some(terminal) = &self.terminal {
            terminal.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// End the shell and remove the panel.
    fn kill_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal = None;
        self._terminal_subscription = None;
        self.terminal_visible = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn on_terminal_event(
        &mut self,
        _: &Entity<TerminalView>,
        event: &TerminalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::TitleChanged => cx.notify(),
            TerminalEvent::Exited => self.kill_terminal(window, cx),
        }
    }

    // ---- Actions -----------------------------------------------------------

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.add_document(None, String::new(), window, cx);
        self.activate(ix, window, cx);
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                for path in paths {
                    this.open_path(path, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_file(&mut self, _: &SaveFile, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_document().id;
        self.save_document(id, false, None, window, cx);
    }

    fn save_file_as(&mut self, _: &SaveFileAs, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_document().id;
        self.save_document(id, true, None, window, cx);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_document().id;
        self.close_document(id, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let ix = (self.active + 1) % self.documents.len();
        self.activate(ix, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.documents.len();
        let ix = (self.active + count - 1) % count;
        self.activate(ix, window, cx);
    }

    fn toggle_soft_wrap(&mut self, _: &ToggleSoftWrap, window: &mut Window, cx: &mut Context<Self>) {
        self.soft_wrap = !self.soft_wrap;
        let soft_wrap = self.soft_wrap;
        for doc in &self.documents {
            doc.editor
                .update(cx, |state, cx| state.set_soft_wrap(soft_wrap, window, cx));
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn toggle_line_numbers(
        &mut self,
        _: &ToggleLineNumbers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.line_numbers = !self.line_numbers;
        let line_numbers = self.line_numbers;
        for doc in &self.documents {
            doc.editor
                .update(cx, |state, cx| state.set_line_number(line_numbers, window, cx));
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn use_light_theme(&mut self, _: &UseLightTheme, window: &mut Window, cx: &mut Context<Self>) {
        Theme::change(ThemeMode::Light, Some(window), cx);
    }

    fn use_dark_theme(&mut self, _: &UseDarkTheme, window: &mut Window, cx: &mut Context<Self>) {
        Theme::change(ThemeMode::Dark, Some(window), cx);
    }

    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close_all(window, cx, Box::new(|_, _, cx| cx.quit()));
    }

    fn on_drop_paths(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        for path in paths.paths() {
            self.open_path(path.clone(), window, cx);
        }
    }

    // ---- Rendering ---------------------------------------------------------

    /// The title bar, drawn by the app in place of the system one: the menu
    /// bar, the window title, and (added by `TitleBar` on Windows and Linux)
    /// the minimize, maximize and close buttons. Drag the empty space or the
    /// title to move the window; double-click it to maximize.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;

        TitleBar::new()
            .child(div().flex_none().h_full().child(self.menu_bar.clone()))
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_center()
                    .px_4()
                    .text_xs()
                    .text_color(muted)
                    .child(div().truncate().child(self.window_title())),
            )
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TabBar::new("documents")
            .selected_index(self.active)
            .on_click(cx.listener(|this, ix: &usize, window, cx| this.activate(*ix, window, cx)))
            .children(self.documents.iter().map(|doc| {
                let id = doc.id;
                let label = if doc.dirty {
                    format!("● {}", doc.name())
                } else {
                    doc.name()
                };
                Tab::new().label(label).suffix(
                    Button::new(("close-tab", id))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(format!("Close ({})", shortcut("W")))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_document(id, window, cx);
                        })),
                )
            }))
    }

    /// Every change since the last commit, one file after another, in place
    /// of the tabs and the editor.
    fn render_changes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, success, danger) = (
            theme.muted_foreground,
            theme.border,
            theme.success,
            theme.danger,
        );
        let state = self.changes.read(cx);
        let files = state.files();
        let mode = state.mode();
        let (additions, deletions) = files
            .iter()
            .fold((0, 0), |(a, d), file| (a + file.additions(), d + file.deletions()));
        let summary = match files.len() {
            0 => String::new(),
            1 => "1 changed file".into(),
            n => format!("{n} changed files"),
        };
        let has_files = !files.is_empty();

        let message = |text: &str| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(text.to_string())
                .into_any_element()
        };
        let body = match &self.repo {
            None => message("The folder in the explorer isn't in a Git repository."),
            Some(_) if !self.changes_loaded => message("Loading changes…"),
            Some(_) if !has_files => message("No changes since the last commit."),
            Some(repo) => {
                let workspace = cx.weak_entity();
                let root = repo.root.clone();
                let open_button = {
                    let (workspace, root) = (workspace.clone(), root.clone());
                    move |file: &DiffFile, _: &mut Window, _: &mut App| {
                        let path = root.join(file.path().to_string());
                        let workspace = workspace.clone();
                        div().when(file.status() != DiffFileStatus::Deleted, |this| {
                            this.child(
                                Button::new(SharedString::from(format!("open:{}", file.path())))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::ExternalLink)
                                    .label("Open")
                                    .on_click(move |_, window, cx| {
                                        workspace
                                            .update(cx, |this, cx| this.open_path(path.clone(), window, cx))
                                            .ok();
                                    }),
                            )
                        })
                    }
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        Diff::new(&self.changes)
                            .size_full()
                            .soft_wrap(self.soft_wrap)
                            .header_suffix(open_button)
                            // Double-click a line to edit the file there.
                            .on_line_click(move |position, event, window, cx| {
                                if event.click_count() < 2 {
                                    return;
                                }
                                let path = root.join(position.path().to_string());
                                let line = (position.side() == DiffSide::Modified).then(|| position.line());
                                workspace
                                    .update(cx, |this, cx| this.open_at(path, line, window, cx))
                                    .ok();
                            }),
                    )
                    .into_any_element()
            }
        };

        let mode_button = |id: &'static str, label: &'static str, value: DiffMode| {
            Button::new(id)
                .ghost()
                .xsmall()
                .label(label)
                .selected(mode == value)
                .on_click(cx.listener(move |this, _, _, cx| this.set_diff_mode(value, cx)))
        };

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_1()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("CHANGES"))
                    .when_some(self.branch.clone(), |row, branch| {
                        row.child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_1()
                                .child(Icon::new(LucideIcon::GitBranch).xsmall())
                                .child(branch),
                        )
                    })
                    .child(summary)
                    .when(has_files, |row| {
                        row.child(div().text_color(success).child(format!("+{additions}")))
                            .child(div().text_color(danger).child(format!("−{deletions}")))
                    })
                    .child(div().flex_1())
                    .child(mode_button("diff-unified", "Unified", DiffMode::Unified))
                    .child(mode_button("diff-split", "Split", DiffMode::Split))
                    .child(
                        Button::new("previous-change")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowUp)
                            .tooltip("Previous change")
                            .disabled(!has_files)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.changes.update(cx, |state, cx| state.previous_change(cx))
                            })),
                    )
                    .child(
                        Button::new("next-change")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowDown)
                            .tooltip("Next change")
                            .disabled(!has_files)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.changes.update(cx, |state, cx| state.next_change(cx))
                            })),
                    )
                    .child(
                        Button::new("refresh-changes")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_git(cx))),
                    )
                    .child(
                        Button::new("close-changes")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(format!("Close ({})", shortcut("Shift+G")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_changes(&ToggleChanges, window, cx)
                            })),
                    ),
            )
            .child(body)
    }

    fn render_terminal_panel(&self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let title = terminal.read(cx).title().unwrap_or_else(|| "Shell".into());

        div()
            .v_flex()
            .flex_none()
            .h(self.terminal_height)
            .child(
                // Drag to resize the panel.
                div()
                    .id("terminal-divider")
                    .h(px(5.))
                    .flex_none()
                    .border_t_1()
                    .border_color(theme.border)
                    .cursor(CursorStyle::ResizeRow)
                    .hover(|style| style.bg(theme.accent))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.resizing = Some(Resize::Terminal {
                                start_y: event.position.y,
                                start_height: this.terminal_height,
                            });
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("TERMINAL"))
                    .child(div().min_w_0().truncate().child(title))
                    .child(div().flex_1())
                    .child(
                        Button::new("hide-terminal")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronDown)
                            .tooltip(format!("Hide terminal ({})", shortcut("`")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_terminal(&ToggleTerminal, window, cx)
                            })),
                    )
                    .child(
                        Button::new("kill-terminal")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close terminal (ends the shell)")
                            .on_click(cx.listener(|this, _, window, cx| this.kill_terminal(window, cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .pl_3()
                    .pr_1()
                    .pb_1()
                    .child(terminal.clone()),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let doc = self.active_document();
        let state = doc.editor.read(cx);
        let line_count = state.text().lines_len();
        let language = language_label(&state.language_name());

        let (message, message_color) = match &self.status {
            Some(status) if status.is_error => (status.message.clone(), theme.danger),
            Some(status) => (status.message.clone(), theme.muted_foreground),
            None => {
                let location = doc
                    .path
                    .as_deref()
                    .map_or_else(|| "Untitled".into(), |path| path.display().to_string());
                (location.into(), theme.muted_foreground)
            }
        };

        div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_4()
            .px_3()
            .py_1()
            .text_xs()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .h_flex()
                    .min_w_0()
                    .items_center()
                    .gap_4()
                    // The branch, and how many files changed; click for the Changes view.
                    .when_some(self.branch.clone(), |row, branch| {
                        let changed = match self.changed_files {
                            0 => String::new(),
                            1 => " · 1 change".into(),
                            n => format!(" · {n} changes"),
                        };
                        row.child(
                            div()
                                .id("git-status")
                                .h_flex()
                                .flex_none()
                                .items_center()
                                .gap_1()
                                .cursor_pointer()
                                .hover(|style| style.text_color(theme.foreground))
                                .child(Icon::new(LucideIcon::GitBranch).xsmall())
                                .child(format!("{branch}{changed}"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_changes(&ToggleChanges, window, cx)
                                })),
                        )
                    })
                    .child(div().min_w_0().truncate().text_color(message_color).child(message)),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .gap_4()
                    .when_some(self.fps.as_ref(), |row, meter| row.child(meter.label()))
                    .child(format!("Ln {}, Col {}", doc.cursor.0 + 1, doc.cursor.1 + 1))
                    .child(format!(
                        "{line_count} {}",
                        if line_count == 1 { "line" } else { "lines" }
                    ))
                    .child(language)
                    .child("UTF-8"),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(meter) = &mut self.fps {
            meter.tick();
            // Keep drawing every frame so the meter sees the full rate.
            window.request_animation_frame();
        }
        // Covers every way a checked setting changes, the system theme included.
        self.sync_menus(cx);

        let theme = cx.theme();
        let (background, foreground, border, sidebar_bg, accent) = (
            theme.background,
            theme.foreground,
            theme.border,
            theme.sidebar,
            theme.accent,
        );
        let viewport = window.viewport_size();
        let max_terminal_height = (viewport.height - px(160.)).max(MIN_TERMINAL_HEIGHT);
        let max_sidebar_width = (viewport.width - px(300.)).max(MIN_SIDEBAR_WIDTH);
        let terminal = self
            .terminal
            .clone()
            .filter(|_| self.terminal_visible);

        let sidebar = self.sidebar_visible.then(|| {
            div()
                .h_flex()
                .flex_none()
                .h_full()
                .child(
                    div()
                        .w(self.sidebar_width)
                        .h_full()
                        .bg(sidebar_bg)
                        .child(self.explorer.clone()),
                )
                .child(
                    // Drag to resize the sidebar.
                    div()
                        .id("sidebar-divider")
                        .w(px(5.))
                        .h_full()
                        .flex_none()
                        .border_l_1()
                        .border_color(border)
                        .cursor(CursorStyle::ResizeColumn)
                        .hover(|style| style.bg(accent))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                this.resizing = Some(Resize::Sidebar {
                                    start_x: event.position.x,
                                    start_width: this.sidebar_width,
                                });
                                cx.stop_propagation();
                            }),
                        ),
                )
        });

        div()
            .v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_changes))
            .on_action(cx.listener(Self::focus_explorer))
            .on_action(cx.listener(Self::save_file))
            .on_action(cx.listener(Self::save_file_as))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::toggle_soft_wrap))
            .on_action(cx.listener(Self::toggle_line_numbers))
            .on_action(cx.listener(Self::use_light_theme))
            .on_action(cx.listener(Self::use_dark_theme))
            .on_action(cx.listener(Self::quit))
            .on_drop(cx.listener(Self::on_drop_paths))
            // Dragging a divider.
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let Some(resize) = this.resizing else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    this.resizing = None;
                    return;
                }
                match resize {
                    Resize::Terminal {
                        start_y,
                        start_height,
                    } => {
                        this.terminal_height = (start_height + (start_y - event.position.y))
                            .clamp(MIN_TERMINAL_HEIGHT, max_terminal_height);
                    }
                    Resize::Sidebar {
                        start_x,
                        start_width,
                    } => {
                        this.sidebar_width = (start_width + (event.position.x - start_x))
                            .clamp(MIN_SIDEBAR_WIDTH, max_sidebar_width);
                    }
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.resizing = None),
            )
            .bg(background)
            .text_color(foreground)
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_h_0()
                    .children(sidebar)
                    .child(
                        // Tabs, editor and terminal, right of the sidebar.
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .map(|column| {
                                if self.changes_visible {
                                    return column.child(self.render_changes(cx));
                                }
                                column.child(self.render_tab_bar(cx)).child(
                                    div().flex_1().min_h_0().child(
                                        Editor::new(&self.active_document().editor)
                                            .bordered(false)
                                            .size_full(),
                                    ),
                                )
                            })
                            .when_some(terminal, |this, terminal| {
                                this.child(self.render_terminal_panel(&terminal, cx))
                            }),
                    ),
            )
            .child(self.render_status_bar(cx))
    }
}

/// A keystroke with the platform's primary modifier: `cmd-` on macOS,
/// `ctrl-` elsewhere. (`KeyBinding::new` doesn't resolve `secondary-`.)
#[cfg(target_os = "macos")]
macro_rules! primary {
    ($keys:literal) => {
        concat!("cmd-", $keys)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! primary {
    ($keys:literal) => {
        concat!("ctrl-", $keys)
    };
}

fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new(primary!("n"), NewFile, context),
        KeyBinding::new(primary!("o"), OpenFile, context),
        KeyBinding::new(concat!(primary!("k"), " ", primary!("o")), OpenFolder, context),
        KeyBinding::new(primary!("b"), ToggleSidebar, context),
        KeyBinding::new(primary!("shift-g"), ToggleChanges, context),
        KeyBinding::new(primary!("shift-e"), FocusExplorer, context),
        KeyBinding::new(primary!("s"), SaveFile, context),
        KeyBinding::new(primary!("shift-s"), SaveFileAs, context),
        KeyBinding::new(primary!("w"), CloseTab, context),
        KeyBinding::new("ctrl-tab", NextTab, context),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, context),
        KeyBinding::new(primary!("pagedown"), NextTab, context),
        KeyBinding::new(primary!("pageup"), PreviousTab, context),
        KeyBinding::new("ctrl-`", ToggleTerminal, context),
        KeyBinding::new("alt-z", ToggleSoftWrap, context),
        KeyBinding::new(primary!("q"), Quit, context),
    ]);
    terminal::bind_keys(cx);
    explorer::bind_keys(cx);
}

fn main() {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            bind_keys(cx);

            // Closing the last window ends the app.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, gpui_kit::size(px(1040.), px(760.)), cx);
            // No system title bar: the workspace draws its own, with the menus
            // and the window buttons.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(APP_NAME.into()),
                    ..TitleBar::title_bar_options()
                }),
                ..TitleBar::window_options()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
                cx.new(|cx| Workspace::new(paths, window, cx))
            })
            .expect("failed to open window");

            cx.activate(true);
        });
}
