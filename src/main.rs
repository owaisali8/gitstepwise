//! Gitstepwise: a small text editor built on GPUI Kit, with a file
//! explorer, tabs and a terminal, for reviewing what changed (say, what a
//! coding agent wrote) step by step before committing it to Git.
//!
//! Run with `cargo run`, or `cargo run -- file1 file2` to open files, or
//! `cargo run -- some/folder` to show that folder in the explorer.
//! A path that doesn't exist yet opens as a new, empty file at that path.

// On Windows a release build is a windowed app, without a console window
// behind it. Debug builds keep the console, for `eprintln!` output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod changes;
mod explorer;
mod file_icons;
mod git;
mod markdown;
mod projects;
mod terminal;
mod terminal_panel;
mod timeline;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::base::GlobalState;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{
    self as edit, Editor, EditorState, GutterDecoration, GutterDecorationCollection, InputEvent,
    OverviewRulerDecoration, OverviewRulerDecorationCollection, OverviewRulerLane, Position,
};
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use changes::{ChangedFile, ChangesEvent, ChangesView, Mode, Select};
use timeline::Timeline;
use explorer::{Explorer, ExplorerEvent};
use markdown::{EmbeddedImages, Links, Target};
use projects::{ProjectPicker, ProjectPickerEvent, Projects};
use terminal_panel::{SplitDown, SplitRight, TerminalPanel, TerminalPanelEvent};

gpui_kit::actions!(
    text_editor,
    [
        NewFile,
        OpenFile,
        OpenFolder,
        /// Switch to another project: the picker of recent folders.
        OpenRecent,
        ToggleSidebar,
        ToggleChanges,
        ReviewChanges,
        ShowTimeline,
        ShowHistory,
        FocusExplorer,
        SaveFile,
        SaveFileAs,
        CloseTab,
        NextTab,
        PreviousTab,
        ToggleTerminal,
        ToggleSoftWrap,
        ToggleLineNumbers,
        TogglePreview,
        TogglePreviewToSide,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        UseLightTheme,
        UseDarkTheme,
        Quit,
    ]
);

/// A link in a Markdown preview was clicked: open the file or folder it
/// leads to.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = text_editor, no_json)]
struct OpenLinkedPath(PathBuf);

const APP_NAME: &str = "Gitstepwise";
const KEY_CONTEXT: &str = "TextEditor";
const PLAIN_TEXT: &str = "text";
const MIN_TERMINAL_HEIGHT: Pixels = px(80.);
const MIN_SIDEBAR_WIDTH: Pixels = px(140.);
/// How long typing pauses before the gutter's change markers update.
const MARKER_DELAY: Duration = Duration::from_millis(150);
/// How often to check the repository for commits, checkouts and staging.
const REPO_POLL: Duration = Duration::from_millis(1500);
/// The editor's font size limits, and how much each zoom step changes it.
const MIN_FONT_SIZE: Pixels = px(8.);
const MAX_FONT_SIZE: Pixels = px(40.);
const FONT_SIZE_STEP: Pixels = px(1.);

// Icons beyond the kit's default set, embedded on top of it.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        BookOpen,
        Circle,
        Columns2,
        Database,
        FileArchive,
        FileBraces,
        FileCode,
        FileCog,
        FileImage,
        FileKey,
        FileLock,
        FilePlus,
        FileSpreadsheet,
        FileTerminal,
        FileText,
        FolderPlus,
        GitBranch,
        Package,
        Rows2,
        Scale,
    ]
);

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

fn is_markdown(path: &Path) -> bool {
    language_for(path) == "markdown"
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
    /// The active tab is a Markdown file, and how it shows.
    markdown: bool,
    preview: Preview,
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
            MenuItem::action("Open Recent…", OpenRecent),
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
            MenuItem::action("Review Changes Step by Step", ReviewChanges),
            MenuItem::action("Edit Timeline", ShowTimeline),
            MenuItem::action("Commit History", ShowHistory),
            MenuItem::action("Terminal", ToggleTerminal).checked(checks.terminal),
            MenuItem::action("Split Terminal Right", SplitRight),
            MenuItem::action("Split Terminal Down", SplitDown),
            MenuItem::separator(),
            MenuItem::action("Word Wrap", ToggleSoftWrap).checked(checks.soft_wrap),
            MenuItem::action("Line Numbers", ToggleLineNumbers).checked(checks.line_numbers),
            MenuItem::separator(),
            MenuItem::action("Markdown Preview", TogglePreview)
                .checked(checks.preview == Preview::Full)
                .disabled(!checks.markdown),
            MenuItem::action("Markdown Preview to the Side", TogglePreviewToSide)
                .checked(checks.preview == Preview::Side)
                .disabled(!checks.markdown),
            MenuItem::separator(),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::action("Reset Zoom", ResetZoom),
            MenuItem::separator(),
            MenuItem::action("Light Theme", UseLightTheme).checked(!checks.dark),
            MenuItem::action("Dark Theme", UseDarkTheme).checked(checks.dark),
        ]),
    ]
}

/// A project's terminals, kept running while another project shows.
struct ParkedTerminal {
    folder: PathBuf,
    panel: Entity<TerminalPanel>,
    visible: bool,
    _subscription: Option<Subscription>,
}

/// A step that runs once unsaved changes have been saved or discarded.
type Continuation = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// Identifies a document across tab moves and closes.
type DocId = usize;

/// How a Markdown file shows: as its source, as the rendered page, or both.
#[derive(Clone, Copy, PartialEq, Default)]
enum Preview {
    #[default]
    Off,
    /// The rendered page in place of the source.
    Full,
    /// The source on the left and the rendered page on the right.
    Side,
}

/// A Markdown file rendered, kept up to date with its tab's text.
struct Rendered {
    state: Entity<TextViewState>,
    images: EmbeddedImages,
}

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
    /// How a Markdown file shows; always `Off` for other files.
    preview: Preview,
    /// The rendered page, while `preview` shows it.
    rendered: Option<Rendered>,
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

    fn is_markdown(&self) -> bool {
        self.path.as_deref().is_some_and(is_markdown)
    }

    /// Hand the editor's text to the rendered page, if there is one.
    fn update_rendered(&self, cx: &mut App) {
        if let Some(rendered) = &self.rendered {
            let text = self.editor.read(cx).value();
            rendered.state.update(cx, |state, cx| state.set_text(&text, cx));
        }
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

/// Opt-in frame-rate meter. Run with `GITSTEPWISE_FPS=1` and the window
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
        std::env::var_os("GITSTEPWISE_FPS")?;
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
    /// The open files. Can be empty: closing the last tab leaves none.
    documents: Vec<Document>,
    active: usize,
    next_id: DocId,
    soft_wrap: bool,
    line_numbers: bool,
    /// The editor's font size, set by zooming; `None` uses the theme's.
    font_size: Option<Pixels>,
    /// Holds focus when no file is open, so the shortcuts keep working.
    focus_handle: FocusHandle,
    status: Option<Status>,
    explorer: Entity<Explorer>,
    sidebar_visible: bool,
    sidebar_width: Pixels,
    /// The terminals, once one has been opened.
    terminal: Option<Entity<TerminalPanel>>,
    terminal_visible: bool,
    terminal_height: Pixels,
    resizing: Option<Resize>,
    fps: Option<FpsMeter>,
    menu_bar: Entity<AppMenuBar>,
    /// What the installed menus' check marks show; `None` before the first install.
    menu_checks: Option<MenuChecks>,
    /// The Git repository of the explorer's folder, if it's in one.
    repo: Option<git::Repo>,
    /// Every edit to the repository's files, in order, since the last commit.
    timeline: Option<Entity<Timeline>>,
    /// The timeline of every repository shown so far, by its root. They keep
    /// recording while another project shows.
    timelines: HashMap<PathBuf, Entity<Timeline>>,
    /// The terminals of the other projects opened.
    parked_terminals: Vec<ParkedTerminal>,
    /// The list of projects to switch to, while it shows.
    project_picker: Option<(Entity<ProjectPicker>, Subscription)>,
    /// The project folder, its open files and the one showing, as last
    /// remembered in the list of projects.
    last_session: Option<(PathBuf, Vec<PathBuf>, Option<PathBuf>)>,
    /// Last seen modification times in `.git`, to notice commits and staging.
    repo_stamp: Option<Vec<Option<SystemTime>>>,
    branch: Option<SharedString>,
    changed_files: usize,
    /// Every changed file in the repository, shown in place of the editor.
    changes: Entity<ChangesView>,
    /// The Changes tab: whether it's open, and whether it's the tab showing.
    changes_open: bool,
    changes_active: bool,
    _repo_task: Option<Task<()>>,
    _changes_task: Option<Task<()>>,
    _repo_watch: Task<()>,
    _terminal_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The explorer shows the first folder given, else the first file's
        // folder. Given nothing, it reopens the last project, else shows the
        // current directory. A folder given or reopened is a project: it goes
        // first in the list of projects, and with no files given, the files
        // last open in it open again.
        let nothing_given = paths.is_empty();
        let (folders, files): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|path| path.is_dir());
        let last_project = || {
            cx.global::<Projects>()
                .list()
                .iter()
                .map(|project| project.path.clone())
                .find(|path| path.is_dir())
        };
        let project = match folders.first() {
            Some(folder) => Some(std::path::absolute(folder).unwrap_or_else(|_| folder.clone())),
            None if nothing_given => last_project(),
            None => None,
        };
        let root = project
            .clone()
            .or_else(|| {
                files
                    .first()
                    .and_then(|file| std::path::absolute(file).ok())
                    .and_then(|file| file.parent().map(Path::to_path_buf))
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();

        let explorer = cx.new(|cx| Explorer::new(root, cx));
        let mut subscriptions = vec![
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
        let changes = cx.new(|cx| ChangesView::new(true, cx));
        subscriptions.push(cx.subscribe_in(&changes, window, Self::on_changes_event));
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
            font_size: None,
            focus_handle: cx.focus_handle(),
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
            timeline: None,
            timelines: HashMap::new(),
            parked_terminals: Vec::new(),
            project_picker: None,
            last_session: None,
            repo_stamp: None,
            branch: None,
            changed_files: 0,
            changes,
            changes_open: false,
            changes_active: false,
            _repo_task: None,
            _changes_task: None,
            _repo_watch: repo_watch,
            _terminal_subscription: None,
            _subscriptions: subscriptions,
        };
        this.sync_menus(cx);

        if let Some(project) = &project {
            cx.update_global::<Projects, _>(|projects, _| {
                projects.opened(project.clone(), projects::now_ms());
                projects.save();
            });
            if files.is_empty() {
                this.restore_session(project, window, cx);
            }
        }
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
            ExplorerEvent::Renamed { from, to } => self.on_renamed(from, to, window, cx),
            ExplorerEvent::Removed(path) => self.on_removed(path, window, cx),
            ExplorerEvent::OpenTerminal(dir) => self.open_terminal_in(dir.clone(), window, cx),
            ExplorerEvent::ViewHistory(path) => {
                let relative = self
                    .repo
                    .as_ref()
                    .and_then(|repo| repo.relative(path))
                    .filter(|relative| !relative.is_empty());
                self.changes
                    .update(cx, |view, cx| view.show_history(relative, cx));
                self.show_changes(None, window, cx);
            }
            ExplorerEvent::OpenPreview(path) => {
                self.open_path(path.clone(), window, cx);
                let opened = self
                    .active_document()
                    .and_then(|doc| doc.path.as_deref())
                    .is_some_and(|open| same_file(open, path));
                if opened {
                    self.set_preview(Preview::Full, false, window, cx);
                }
            }
            ExplorerEvent::Permalink { path, open } => self.permalink(path, *open, cx),
            ExplorerEvent::Status { message, is_error } => self.set_status(message.clone(), *is_error, cx),
            ExplorerEvent::GitChanged => self.refresh_git(cx),
        }
    }

    /// A file or folder moved: tabs showing it, or files inside it, follow.
    fn on_renamed(&mut self, from: &Path, to: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let mut moved = Vec::new();
        for doc in &mut self.documents {
            let Some(path) = doc.path.as_deref() else {
                continue;
            };
            let Ok(rest) = path.strip_prefix(from) else {
                continue;
            };
            let new_path = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            };
            // A new extension may mean a new language.
            let language = language_for(&new_path);
            if doc.editor.read(cx).language_name() != language {
                doc.editor.update(cx, |state, cx| state.set_highlighter(language, cx));
            }
            doc.path = Some(new_path);
            if !doc.is_markdown() {
                doc.preview = Preview::Off;
                doc.rendered = None;
            }
            moved.push(doc.id);
        }
        for id in moved {
            self.load_base(id, cx);
        }
        self.update_title(window);
        self.sync_explorer(cx);
        self.refresh_changes(cx);
        cx.notify();
    }

    /// A file or folder was deleted: close its tabs, unless they have unsaved
    /// changes (those stay open, to be saved again or discarded).
    fn on_removed(&mut self, removed: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<DocId> = self
            .documents
            .iter()
            .filter(|doc| !doc.dirty && doc.path.as_deref().is_some_and(|path| path.starts_with(removed)))
            .map(|doc| doc.id)
            .collect();
        for id in ids {
            self.close_document(id, window, cx);
        }
        self.refresh_changes(cx);
    }

    /// Open, or copy, a link to a file as it is in the last commit, on the
    /// remote's website.
    fn permalink(&mut self, path: &Path, open: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(relative) = repo.relative(path) else {
            self.set_status(format!("{} isn't in the repository.", path.display()), true, cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let link = cx
                .background_spawn(async move {
                    let head = repo.head().ok_or("Nothing has been committed yet.")?;
                    let url = repo.permalink(&head, &relative)?;
                    Ok::<_, String>((url, head))
                })
                .await;
            this.update(cx, |this, cx| match link {
                Ok((url, head)) => {
                    let short = &head[..head.len().min(7)];
                    if open {
                        cx.open_url(&url);
                        this.set_status(format!("Opened the link to commit {short}: {url}"), false, cx);
                    } else {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        this.set_status(format!("Copied the link to commit {short}: {url}"), false, cx);
                    }
                }
                Err(err) => this.set_status(err, true, cx),
            })
            .ok();
        })
        .detach();
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
            this.update_in(cx, |this, window, cx| this.open_project(folder, window, cx))
                .ok();
        })
        .detach();
    }

    // ---- Projects ----------------------------------------------------------

    /// Open `folder` as the project: ask about unsaved changes, then swap
    /// the tabs for the ones last open in it.
    fn open_project(&mut self, folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let folder = std::path::absolute(&folder).unwrap_or(folder);
        if !folder.is_dir() {
            self.set_status(format!("{} isn't there any more.", folder.display()), true, cx);
            return;
        }
        if projects::same_folder(&folder, &self.default_folder(cx)) {
            // Already showing: it only becomes a project, if it wasn't one.
            self.remember_opened(folder, cx);
            self.sidebar_visible = true;
            cx.notify();
            return;
        }
        self.confirm_close_all(
            window,
            cx,
            Box::new(move |this, window, cx| this.switch_project(folder, window, cx)),
        );
    }

    /// Put `folder` first in the list of projects.
    fn remember_opened(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        cx.update_global::<Projects, _>(|projects, _| {
            projects.opened(folder, projects::now_ms());
            projects.save();
        });
    }

    /// Show `folder` in place of the project showing. Its unsaved changes
    /// have been saved or discarded by now. Its terminals keep running, to
    /// come back with it.
    fn switch_project(&mut self, folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let old = self.default_folder(cx);
        if let Some(panel) = self.terminal.take() {
            self.parked_terminals.push(ParkedTerminal {
                folder: old,
                panel,
                visible: self.terminal_visible,
                _subscription: self._terminal_subscription.take(),
            });
        }
        self.terminal_visible = false;
        let parked = self
            .parked_terminals
            .iter()
            .position(|parked| projects::same_folder(&parked.folder, &folder));
        if let Some(ix) = parked {
            let parked = self.parked_terminals.remove(ix);
            self.terminal = Some(parked.panel);
            self._terminal_subscription = parked._subscription;
            self.terminal_visible = parked.visible;
        }

        self.documents.clear();
        self.active = 0;
        self.changes_open = false;
        self.changes_active = false;
        self.explorer
            .update(cx, |explorer, cx| explorer.set_root(folder.clone(), cx));
        self.sidebar_visible = true;
        self.remember_opened(folder.clone(), cx);
        self.restore_session(&folder, window, cx);
        self.refresh_repo(cx);
        self.show_active(window, cx);
        self.set_status(format!("Opened {}", folder.display()), false, cx);
    }

    /// Open the files that were open in the project at `folder` the last
    /// time, and show the one that was showing.
    fn restore_session(&mut self, folder: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = cx.global::<Projects>().get(folder).cloned() else {
            return;
        };
        for file in project.files.iter().filter(|file| file.is_file()) {
            self.open_path(file.clone(), window, cx);
        }
        let active = project.active.and_then(|active| {
            self.documents
                .iter()
                .position(|doc| doc.path.as_deref() == Some(active.as_path()))
        });
        if let Some(ix) = active {
            self.activate(ix, window, cx);
        }
    }

    /// Keep the list of projects up to date with the files open in this
    /// one. Runs on every render, and writes only when they change.
    fn sync_project_session(&mut self, cx: &mut Context<Self>) {
        let folder = self.default_folder(cx);
        let files: Vec<PathBuf> = self.documents.iter().filter_map(|doc| doc.path.clone()).collect();
        let active = self.active_document().and_then(|doc| doc.path.clone());
        if let Some((last_folder, last_files, last_active)) = &self.last_session
            && *last_folder == folder
            && *last_files == files
            && *last_active == active
        {
            return;
        }
        let changed = cx.update_global::<Projects, _>(|projects, _| {
            projects.set_session(&folder, &files, active.as_deref())
        });
        self.last_session = Some((folder, files, active));
        if changed {
            // Not while drawing.
            cx.defer(|cx| cx.global::<Projects>().save());
        }
    }

    /// Show the list of projects to switch to, or close it.
    fn open_recent(&mut self, _: &OpenRecent, window: &mut Window, cx: &mut Context<Self>) {
        if self.project_picker.is_some() {
            self.close_picker(window, cx);
            return;
        }
        let current = self.default_folder(cx);
        let picker = cx.new(|cx| ProjectPicker::new(&current, window, cx));
        let subscription = cx.subscribe_in(&picker, window, Self::on_picker_event);
        picker.read(cx).focus_handle(cx).focus(window, cx);
        self.project_picker = Some((picker, subscription));
        cx.notify();
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.project_picker.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn on_picker_event(
        &mut self,
        _: &Entity<ProjectPicker>,
        event: &ProjectPickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_picker(window, cx);
        match event {
            ProjectPickerEvent::Open(folder) => self.open_project(folder.clone(), window, cx),
            ProjectPickerEvent::OpenFolder => self.open_folder(&OpenFolder, window, cx),
            ProjectPickerEvent::Dismissed => {}
        }
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

    /// The file showing (or last shown, under the Changes tab); `None` when
    /// no file is open.
    fn active_document(&self) -> Option<&Document> {
        self.documents.get(self.active)
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
            preview: Preview::Off,
            rendered: None,
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
        // Switching to a file leaves the Changes tab open but not showing.
        self.changes_active = false;
        self.show_active(window, cx);
    }

    /// Bring focus, the title and the explorer's highlight in line with the
    /// tab showing.
    fn show_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_active(window, cx);
        self.update_title(window);
        self.sync_explorer(cx);
        cx.notify();
    }

    /// The Changes tab's position in the tab bar, after the files, if it's open.
    fn changes_tab_index(&self) -> Option<usize> {
        self.changes_open.then_some(self.documents.len())
    }

    /// The position in the tab bar of the tab showing.
    fn active_tab_index(&self) -> usize {
        match self.changes_tab_index() {
            Some(ix) if self.changes_active => ix,
            _ => self.active,
        }
    }

    /// Show the tab at `ix`: a file, or the Changes tab.
    fn select_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if Some(ix) == self.changes_tab_index() {
            self.show_changes(None, window, cx);
        } else {
            self.activate(ix, window, cx);
        }
    }

    /// Focus what the main area shows: the active editor, the Changes view,
    /// or the empty area when no file is open.
    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.changes_active {
            self.changes.read(cx).focus_handle(cx).focus(window, cx);
            return;
        }
        match self.active_document() {
            // The page alone shows: the source can't be typed into unseen.
            Some(doc) if doc.preview == Preview::Full => self.focus_handle.focus(window, cx),
            Some(doc) => {
                let editor = doc.editor.clone();
                editor.update(cx, |state, cx| state.focus(window, cx));
            }
            None => self.focus_handle.focus(window, cx),
        }
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

    /// Close a tab, asking first if it has unsaved changes. Closing the last
    /// one leaves the Changes tab, if it's open, or else no tabs at all.
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
                if ix < this.active {
                    this.active -= 1;
                }
                this.active = this.active.min(this.documents.len().saturating_sub(1));
                if this.changes_active {
                    // A file closed behind the Changes tab: it stays showing.
                    cx.notify();
                } else if this.documents.is_empty() && this.changes_open {
                    this.show_changes(None, window, cx);
                } else {
                    this.show_active(window, cx);
                }
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
        self.documents[ix].update_rendered(cx);
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

    /// "● name — Gitstepwise", with the dot when there are unsaved changes.
    fn window_title(&self) -> String {
        if self.changes_active {
            return format!("Changes — {APP_NAME}");
        }
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
        let doc = self.active_document().filter(|_| !self.changes_active);
        let checks = MenuChecks {
            sidebar: self.sidebar_visible,
            changes: self.changes_open,
            terminal: self.terminal_visible,
            soft_wrap: self.soft_wrap,
            line_numbers: self.line_numbers,
            markdown: doc.is_some_and(Document::is_markdown),
            preview: doc.map_or(Preview::Off, |doc| doc.preview),
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
        // So the timeline marks this edit as saved here. The watcher hears
        // about the write later, on the main thread, so this comes first.
        if let Some(timeline) = &self.timeline {
            timeline.update(cx, |timeline, _| timeline.note_saved(&path, &text));
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
        // Saved as something other than Markdown: there's no page to show.
        if !doc.is_markdown() && doc.preview != Preview::Off {
            doc.preview = Preview::Off;
            doc.rendered = None;
            self.focus_active(window, cx);
        }
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
            if self.write_to(id, path, window, cx)
                && let Some(then) = then
            {
                then(self, window, cx);
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
                if this.write_to(id, path, window, cx)
                    && let Some(then) = then
                {
                    then(this, window, cx);
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
                    // A repository shown before still has its timeline, which
                    // kept recording; it may have missed a commit meanwhile.
                    this.timeline = repo.clone().map(|repo| {
                        if let Some(timeline) = this.timelines.get(&repo.root) {
                            timeline.update(cx, |timeline, cx| timeline.check_head(cx));
                            return timeline.clone();
                        }
                        let root = repo.root.clone();
                        let timeline = cx.new(|cx| Timeline::new(repo, cx));
                        this.timelines.insert(root, timeline.clone());
                        timeline
                    });
                    this.repo = repo.clone();
                    let timeline = this.timeline.clone();
                    this.changes.update(cx, |view, cx| {
                        view.set_loading(cx);
                        view.set_timeline(timeline, cx);
                        view.set_repo(repo.clone(), cx);
                    });
                    this.explorer.update(cx, |explorer, cx| explorer.set_repo(repo, cx));
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
            // A commit starts a new timeline, and adds to the history.
            if let Some(timeline) = &self.timeline {
                timeline.update(cx, |timeline, cx| timeline.check_head(cx));
            }
            self.changes.update(cx, |view, cx| view.refresh_history(cx));
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
            self.changes.update(cx, |view, cx| view.set_not_a_repository(cx));
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
        let with_diffs = self.changes_open;

        let root = repo.root.clone();
        self._changes_task = Some(cx.spawn(async move |this, cx| {
            let (branch, count, patches) = cx
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
                    (repo.branch(), count, patch)
                })
                .await;

            this.update(cx, |this, cx| {
                let branch = branch.map(SharedString::from);
                this.branch = branch.clone();
                this.changed_files = count;
                if let Some(patches) = patches {
                    let mut files = Vec::new();
                    for patch in &patches {
                        match ChangedFile::parse(patch) {
                            Ok(file) => files.push(file),
                            Err(err) => this.set_status(format!("Couldn't read a change: {err}"), true, cx),
                        }
                    }
                    this.changes
                        .update(cx, |view, cx| view.set_changes(root, branch, files, cx));
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Open the Changes tab (or switch to it); close it if it's showing.
    fn toggle_changes(&mut self, _: &ToggleChanges, window: &mut Window, cx: &mut Context<Self>) {
        if self.changes_active {
            self.close_changes(window, cx);
            return;
        }
        // Reviewing picks up at the first file not yet reviewed; showing
        // everything starts at the file being edited.
        let select = if self.changes.read(cx).mode() != Mode::AllFiles {
            Some(Select::FirstUnreviewed)
        } else {
            self.repo.as_ref().and_then(|repo| {
                let path = self.documents.get(self.active)?.path.as_deref()?;
                repo.relative(path).map(Select::File)
            })
        };
        self.show_changes(select, window, cx);
    }

    /// Step through the changes one file at a time.
    fn review_changes(&mut self, _: &ReviewChanges, window: &mut Window, cx: &mut Context<Self>) {
        self.changes
            .update(cx, |view, cx| view.set_mode(Mode::ByFile, cx));
        self.show_changes(Some(Select::FirstUnreviewed), window, cx);
    }

    /// Step through every edit in the order it happened.
    fn show_timeline(&mut self, _: &ShowTimeline, window: &mut Window, cx: &mut Context<Self>) {
        self.changes.update(cx, |view, cx| view.review_timeline(cx));
        self.show_changes(None, window, cx);
    }

    /// The commits already made, and what each one changed.
    fn show_commit_history(&mut self, _: &ShowHistory, window: &mut Window, cx: &mut Context<Self>) {
        self.changes.update(cx, |view, cx| view.show_history(None, cx));
        self.show_changes(None, window, cx);
    }

    /// Open the Changes tab, or switch to it, and bring its changes up to date.
    fn show_changes(&mut self, select: Option<Select>, window: &mut Window, cx: &mut Context<Self>) {
        self.changes_open = true;
        self.changes_active = true;
        if let Some(select) = select {
            self.changes.update(cx, |view, _| view.select_on_load(select));
        }
        self.refresh_changes(cx);
        self.focus_active(window, cx);
        self.update_title(window);
        cx.notify();
    }

    fn close_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.changes_open = false;
        self.changes_active = false;
        self.focus_active(window, cx);
        self.update_title(window);
        cx.notify();
    }

    fn on_changes_event(
        &mut self,
        _: &Entity<ChangesView>,
        event: &ChangesEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ChangesEvent::OpenFile { path, line } => self.open_at(path.clone(), *line, window, cx),
            ChangesEvent::Refresh => self.refresh_git(cx),
            ChangesEvent::Close => self.close_changes(window, cx),
        }
    }

    /// Open a file from the Changes view, at a 1-based line if given.
    fn open_at(&mut self, path: PathBuf, line: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(path.clone(), window, cx);
        let Some(line) = line else {
            return;
        };
        let Some(doc) = self.active_document() else {
            return;
        };
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
            self.hide_terminal(window, cx);
            return;
        }

        if self.terminal.is_none() && !self.start_terminal(self.terminal_folder(cx), window, cx) {
            return;
        }
        self.show_terminal(window, cx);
    }

    fn hide_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Where a new shell starts: the active file's folder, else the explorer's.
    fn terminal_folder(&self, cx: &App) -> PathBuf {
        self.active_document()
            .and_then(|doc| doc.path.as_deref())
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.default_folder(cx))
    }

    /// Start a shell in `working_directory`, or say why it couldn't start.
    fn start_shell(&mut self, working_directory: PathBuf, cx: &mut Context<Self>) -> Option<terminal::Shell> {
        match terminal::start_shell(Some(working_directory)) {
            Ok(shell) => Some(shell),
            Err(err) => {
                self.set_status(format!("Couldn't start a terminal: {err}"), true, cx);
                None
            }
        }
    }

    /// Open the terminal panel with a shell in `working_directory`. Returns
    /// whether it started.
    fn start_terminal(&mut self, working_directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(shell) = self.start_shell(working_directory, cx) else {
            return false;
        };
        let panel = cx.new(|cx| TerminalPanel::new(shell, window, cx));
        self._terminal_subscription = Some(cx.subscribe_in(&panel, window, Self::on_terminal_event));
        self.terminal = Some(panel);
        true
    }

    /// Start another shell beside the active terminal: to its right on the
    /// horizontal axis, below it on the vertical. With no terminal yet, open
    /// the first one.
    fn split_terminal(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let folder = self.terminal_folder(cx);
        match self.terminal.clone() {
            Some(panel) => {
                let Some(shell) = self.start_shell(folder, cx) else {
                    return;
                };
                panel.update(cx, |panel, cx| panel.split(axis, shell, window, cx));
            }
            None => {
                if !self.start_terminal(folder, window, cx) {
                    return;
                }
            }
        }
        self.show_terminal(window, cx);
    }

    fn split_terminal_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split_terminal(Axis::Horizontal, window, cx);
    }

    fn split_terminal_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split_terminal(Axis::Vertical, window, cx);
    }

    fn show_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = true;
        if let Some(terminal) = &self.terminal {
            terminal.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Show the terminal in `dir`: a new shell there, or the active one
    /// moved there.
    fn open_terminal_in(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match &self.terminal {
            Some(terminal) => terminal.update(cx, |panel, cx| panel.change_directory(&dir, cx)),
            None => {
                if !self.start_terminal(dir, window, cx) {
                    return;
                }
            }
        }
        self.show_terminal(window, cx);
    }

    /// End every shell and remove the panel.
    fn kill_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal = None;
        self._terminal_subscription = None;
        self.terminal_visible = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn on_terminal_event(
        &mut self,
        panel: &Entity<TerminalPanel>,
        event: &TerminalPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal.as_ref() != Some(panel) {
            // Another project's: when its last shell ends, it goes.
            if matches!(event, TerminalPanelEvent::Closed) {
                self.parked_terminals.retain(|parked| &parked.panel != panel);
            }
            return;
        }
        match event {
            TerminalPanelEvent::Split(axis) => self.split_terminal(*axis, window, cx),
            TerminalPanelEvent::Hide => self.hide_terminal(window, cx),
            TerminalPanelEvent::Closed => self.kill_terminal(window, cx),
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
        if let Some(id) = self.active_document().map(|doc| doc.id) {
            self.save_document(id, false, None, window, cx);
        }
    }

    fn save_file_as(&mut self, _: &SaveFileAs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_document().map(|doc| doc.id) {
            self.save_document(id, true, None, window, cx);
        }
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.changes_active {
            self.close_changes(window, cx);
            return;
        }
        if let Some(id) = self.active_document().map(|doc| doc.id) {
            self.close_document(id, window, cx);
        }
    }

    /// How many tabs there are: the files, and the Changes tab if it's open.
    fn tab_count(&self) -> usize {
        self.documents.len() + usize::from(self.changes_open)
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tab_count();
        if count == 0 {
            return;
        }
        let ix = (self.active_tab_index() + 1) % count;
        self.select_tab(ix, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tab_count();
        if count == 0 {
            return;
        }
        let ix = (self.active_tab_index() + count - 1) % count;
        self.select_tab(ix, window, cx);
    }

    fn toggle_soft_wrap(&mut self, _: &ToggleSoftWrap, window: &mut Window, cx: &mut Context<Self>) {
        self.soft_wrap = !self.soft_wrap;
        let soft_wrap = self.soft_wrap;
        for doc in &self.documents {
            doc.editor
                .update(cx, |state, cx| state.set_soft_wrap(soft_wrap, window, cx));
        }
        self.changes
            .update(cx, |view, cx| view.set_soft_wrap(soft_wrap, cx));
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

    // ---- Markdown preview --------------------------------------------------

    /// Show the active Markdown file as `preview`. With `toggle`, a file
    /// already showing that way goes back to its source.
    fn set_preview(&mut self, preview: Preview, toggle: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.changes_active {
            return;
        }
        let Some(doc) = self.documents.get_mut(self.active) else {
            return;
        };
        if !doc.is_markdown() {
            self.set_status("Only Markdown files have a preview.", false, cx);
            return;
        }
        doc.preview = if toggle && doc.preview == preview {
            Preview::Off
        } else {
            preview
        };
        if doc.preview == Preview::Off {
            doc.rendered = None;
        } else if doc.rendered.is_none() {
            let text = doc.editor.read(cx).value();
            doc.rendered = Some(Rendered {
                state: cx.new(|cx| TextViewState::markdown(&text, cx)),
                images: EmbeddedImages::default(),
            });
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn toggle_preview(&mut self, _: &TogglePreview, window: &mut Window, cx: &mut Context<Self>) {
        self.set_preview(Preview::Full, true, window, cx);
    }

    fn toggle_preview_to_side(&mut self, _: &TogglePreviewToSide, window: &mut Window, cx: &mut Context<Self>) {
        self.set_preview(Preview::Side, true, window, cx);
    }

    fn open_linked_path(&mut self, link: &OpenLinkedPath, window: &mut Window, cx: &mut Context<Self>) {
        let path = &link.0;
        if path.exists() {
            self.open_path(path.clone(), window, cx);
        } else {
            self.set_status(format!("The link leads to {}, which doesn't exist.", path.display()), true, cx);
        }
    }

    /// Where a Markdown file's links starting with `/` lead: the repository,
    /// or else the explorer's folder.
    fn links_root(&self, cx: &App) -> PathBuf {
        self.repo
            .as_ref()
            .map_or_else(|| self.default_folder(cx), |repo| repo.root.clone())
    }

    /// The editor's font size: the zoomed one, else the theme's code size.
    fn editor_font_size(&self, cx: &App) -> Pixels {
        self.font_size.unwrap_or(cx.theme().mono_font_size)
    }

    /// Change the editor's font size (in every tab) by `delta`, or back to
    /// the theme's with `None`.
    fn zoom(&mut self, delta: Option<Pixels>, cx: &mut Context<Self>) {
        self.font_size = delta.map(|delta| {
            (self.editor_font_size(cx) + delta).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        });
        let size = f32::from(self.editor_font_size(cx));
        self.set_status(format!("Font size {size} px"), false, cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(Some(FONT_SIZE_STEP), cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(Some(-FONT_SIZE_STEP), cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(None, cx);
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
    /// bar, a button that shows or hides the explorer, the window title, and
    /// (added by `TitleBar` on Windows and Linux) the minimize, maximize and
    /// close buttons. Drag the empty space or the title to move the window;
    /// double-click it to maximize.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let (sidebar_icon, sidebar_tooltip) = if self.sidebar_visible {
            (IconName::PanelLeftClose, "Hide explorer")
        } else {
            (IconName::PanelLeftOpen, "Show explorer")
        };

        TitleBar::new()
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .h_full()
                    .items_center()
                    .gap_1()
                    .child(div().flex_none().h_full().child(self.menu_bar.clone()))
                    .child(
                        div()
                            .h_flex()
                            .flex_none()
                            .gap_1()
                            // The title bar moves the window when pressed: claim
                            // the press so it clicks the button instead.
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                            })
                            .child(
                                Button::new("toggle-sidebar")
                                    .ghost()
                                    .xsmall()
                                    .icon(sidebar_icon)
                                    .tooltip(format!("{sidebar_tooltip} ({})", shortcut("B")))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_sidebar(&ToggleSidebar, window, cx)
                                    })),
                            )
                            // The project's name; click to switch to another.
                            .child(
                                Button::new("switch-project")
                                    .ghost()
                                    .xsmall()
                                    .label(projects::folder_name(&self.default_folder(cx)))
                                    .dropdown_caret(true)
                                    .selected(self.project_picker.is_some())
                                    .tooltip(format!("Switch project ({})", shortcut("Alt+O")))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_recent(&OpenRecent, window, cx)
                                    })),
                            ),
                    ),
            )
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

    /// A tab for each open file, then the Changes tab if it's open.
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let changes_tab = self.changes_open.then(|| {
            // The icon goes in the prefix: a tab with `.icon()` shows only the icon.
            Tab::new()
                .prefix(div().pl_2().child(Icon::new(LucideIcon::GitBranch).small()))
                .label("Changes")
                .suffix(
                    Button::new("close-changes-tab")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(format!("Close ({})", shortcut("W")))
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_changes(window, cx);
                        })),
                )
        });

        // For a Markdown file, buttons that show it rendered.
        let preview_buttons = self
            .active_document()
            .filter(|doc| !self.changes_active && doc.is_markdown())
            .map(|doc| {
                div()
                    .h_flex()
                    .h_full()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .child(
                        Button::new("toggle-preview")
                            .ghost()
                            .xsmall()
                            .icon(LucideIcon::Eye)
                            .selected(doc.preview == Preview::Full)
                            .tooltip(format!("Preview ({})", shortcut("Shift+V")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_preview(&TogglePreview, window, cx)
                            })),
                    )
                    .child(
                        Button::new("toggle-preview-to-side")
                            .ghost()
                            .xsmall()
                            .icon(LucideIcon::Columns2)
                            .selected(doc.preview == Preview::Side)
                            .tooltip(format!("Preview to the side ({}, then V)", shortcut("K")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_preview_to_side(&TogglePreviewToSide, window, cx)
                            })),
                    )
            });

        TabBar::new("documents")
            .when_some(preview_buttons, |bar, buttons| bar.suffix(buttons))
            .selected_index(self.active_tab_index())
            .on_click(cx.listener(|this, ix: &usize, window, cx| this.select_tab(*ix, window, cx)))
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
            .children(changes_tab)
    }

    /// The active file: its editor, its rendered Markdown, or both side by side.
    fn render_document(&self, doc: &Document, cx: &mut Context<Self>) -> AnyElement {
        let editor = Editor::new(&doc.editor)
            .bordered(false)
            .text_size(self.editor_font_size(cx))
            .size_full();
        let area = div().flex_1().min_h_0();
        match (&doc.rendered, doc.preview) {
            (Some(rendered), Preview::Full) => area
                // Holds focus in place of the hidden editor.
                .track_focus(&self.focus_handle)
                .child(self.render_preview(doc, rendered, cx))
                .into_any_element(),
            (Some(rendered), Preview::Side) => {
                let border = cx.theme().border;
                area.h_flex()
                    .child(div().flex_1().min_w_0().h_full().child(editor))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .border_l_1()
                            .border_color(border)
                            .child(self.render_preview(doc, rendered, cx)),
                    )
                    .into_any_element()
            }
            _ => area.child(editor).into_any_element(),
        }
    }

    /// A Markdown file as a page. It zooms with the editor, and its links and
    /// images lead to files next to it.
    fn render_preview(&self, doc: &Document, rendered: &Rendered, cx: &mut Context<Self>) -> impl IntoElement {
        let (font_size, mono_font_size) = (cx.theme().font_size, cx.theme().mono_font_size);
        let zoom = f32::from(self.editor_font_size(cx)) / f32::from(mono_font_size);
        let file = doc.path.clone().unwrap_or_default();
        let links = Links::new(&file, &self.links_root(cx), rendered.images.clone());
        let image_links = links.clone();

        div().size_full().text_size(font_size * zoom).child(
            TextView::new(&rendered.state)
                .scrollable(true)
                .selectable(true)
                .size_full()
                .px_8()
                .py_3()
                .image_source(move |uri| image_links.image(uri))
                .on_link_click(move |url, _, window, cx| match links.target(url) {
                    Target::Url => cx.open_url(url),
                    Target::Path(path) => window.dispatch_action(Box::new(OpenLinkedPath(path)), cx),
                    Target::Anchor => {}
                }),
        )
    }

    /// What the main area shows once every tab is closed. It holds focus, so
    /// the shortcuts and menus still reach the workspace.
    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let action = |id: &'static str, label: &'static str, keys: &str| {
            Button::new(id)
                .ghost()
                .small()
                .label(label)
                .tooltip(keys.to_string())
        };

        div()
            .id("empty-state")
            .track_focus(&self.focus_handle)
            .flex_1()
            .min_h_0()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_2()
            .text_sm()
            .text_color(muted)
            .child("No file is open")
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(
                        action("empty-new-file", "New File", &shortcut("N"))
                            .on_click(cx.listener(|this, _, window, cx| this.new_file(&NewFile, window, cx))),
                    )
                    .child(
                        action("empty-open-file", "Open File…", &shortcut("O"))
                            .on_click(cx.listener(|this, _, window, cx| this.open_file(&OpenFile, window, cx))),
                    )
                    .child(
                        action(
                            "empty-open-folder",
                            "Open Folder…",
                            &format!("{} {}", shortcut("K"), shortcut("O")),
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.open_folder(&OpenFolder, window, cx))),
                    ),
            )
    }

    /// The terminal panel, below a divider that sets its height.
    fn render_terminal_panel(&self, terminal: &Entity<TerminalPanel>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

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
            .child(div().flex_1().min_h_0().child(terminal.clone()))
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let doc = self.active_document();
        // The cursor, line count, language and encoding, for an open file.
        let file_info = doc.map(|doc| {
            let state = doc.editor.read(cx);
            let line_count = state.text().lines_len();
            [
                if doc.preview == Preview::Full {
                    "Preview".to_string()
                } else {
                    format!("Ln {}, Col {}", doc.cursor.0 + 1, doc.cursor.1 + 1)
                },
                format!("{line_count} {}", if line_count == 1 { "line" } else { "lines" }),
                language_label(&state.language_name()).to_string(),
                "UTF-8".to_string(),
            ]
        });

        // Shows or hides the terminal, and is lit while it shows. (A tooltip
        // stays as it was built while the pointer rests, so it says both.)
        let terminal_button = div()
            .id("terminal-status")
            .flex_none()
            .cursor_pointer()
            .when(self.terminal_visible, |button| button.text_color(theme.foreground))
            .hover(|style| style.text_color(theme.foreground))
            .child(Icon::new(LucideIcon::SquareTerminal).xsmall())
            .tooltip(|window, cx| Tooltip::new("Show or hide the terminal (Ctrl+`)").build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_terminal(&ToggleTerminal, window, cx)
            }));

        let (message, message_color) = match &self.status {
            Some(status) if status.is_error => (status.message.clone(), theme.danger),
            Some(status) => (status.message.clone(), theme.muted_foreground),
            None => {
                let location = match doc {
                    Some(doc) => doc
                        .path
                        .as_deref()
                        .map_or_else(|| "Untitled".into(), |path| path.display().to_string()),
                    None => String::new(),
                };
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
                    .children(file_info.into_iter().flatten())
                    .child(terminal_button),
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
        self.sync_project_session(cx);

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
            .on_action(cx.listener(Self::open_recent))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_changes))
            .on_action(cx.listener(Self::review_changes))
            .on_action(cx.listener(Self::show_timeline))
            .on_action(cx.listener(Self::show_commit_history))
            .on_action(cx.listener(Self::focus_explorer))
            .on_action(cx.listener(Self::save_file))
            .on_action(cx.listener(Self::save_file_as))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::split_terminal_right))
            .on_action(cx.listener(Self::split_terminal_down))
            .on_action(cx.listener(Self::toggle_soft_wrap))
            .on_action(cx.listener(Self::toggle_line_numbers))
            .on_action(cx.listener(Self::toggle_preview))
            .on_action(cx.listener(Self::toggle_preview_to_side))
            .on_action(cx.listener(Self::open_linked_path))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
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
                            .child(self.render_tab_bar(cx))
                            .map(|column| {
                                if self.changes_active {
                                    return column
                                        .child(div().flex_1().min_h_0().child(self.changes.clone()));
                                }
                                let Some(doc) = self.active_document() else {
                                    return column.child(self.render_empty_state(cx));
                                };
                                column.child(self.render_document(doc, cx))
                            })
                            .when_some(terminal, |this, terminal| {
                                this.child(self.render_terminal_panel(&terminal, cx))
                            }),
                    ),
            )
            .child(self.render_status_bar(cx))
            // The list of projects, over everything near the top, as in Zed.
            .when_some(self.project_picker.as_ref(), |root, (picker, _)| {
                root.child(
                    div()
                        .absolute()
                        .top(px(48.))
                        .left_0()
                        .right_0()
                        .h_flex()
                        .justify_center()
                        .child(picker.clone()),
                )
            })
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
        KeyBinding::new(primary!("alt-o"), OpenRecent, context),
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
        KeyBinding::new(primary!("shift-v"), TogglePreview, context),
        KeyBinding::new(concat!(primary!("k"), " v"), TogglePreviewToSide, context),
        // Ctrl+= and Ctrl++ (with or without Shift, on the main row's = key).
        // The menu shows the last binding, so plain Ctrl+= comes last.
        KeyBinding::new(primary!("+"), ZoomIn, context),
        KeyBinding::new(primary!("shift-="), ZoomIn, context),
        KeyBinding::new(primary!("="), ZoomIn, context),
        KeyBinding::new(primary!("-"), ZoomOut, context),
        KeyBinding::new(primary!("0"), ResetZoom, context),
        KeyBinding::new(primary!("q"), Quit, context),
    ]);
    terminal::bind_keys(cx);
    terminal_panel::bind_keys(cx);
    explorer::bind_keys(cx);
    changes::bind_keys(cx);
}

fn main() {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            bind_keys(cx);
            cx.set_global(Projects::load());

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
