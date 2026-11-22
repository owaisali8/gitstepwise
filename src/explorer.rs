//! The file explorer sidebar: a tree of one folder, read from disk lazily as
//! folders are expanded, and virtualized so large folders stay fast.
//!
//! Right-click a row (or the empty space below the rows, for the folder
//! itself) for what can be done with it; most of it also has a shortcut
//! while the explorer has focus. The actions act on the selected row.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::file_icons::file_icon;
use crate::git::Repo;

pub const KEY_CONTEXT: &str = "Explorer";
const ROW_HEIGHT: Pixels = px(24.);
const INDENT: f32 = 12.;

/// Names never shown, like most editors' defaults.
const HIDDEN_NAMES: &[&str] = &[".git", ".DS_Store", "Thumbs.db"];

gpui_kit::actions!(
    explorer,
    [
        SelectPrevious,
        SelectNext,
        /// Right arrow: expand a folder, or step into an expanded one.
        ExpandSelected,
        /// Left arrow: collapse a folder, or step out to the parent folder.
        CollapseSelected,
        /// Enter: open a file, or toggle a folder.
        OpenSelected,
        NewFileHere,
        NewFolderHere,
        RevealInFileManager,
        OpenWithSystem,
        OpenPreview,
        OpenInTerminal,
        CutEntry,
        CopyEntry,
        DuplicateEntry,
        PasteEntry,
        CopyPath,
        CopyRelativePath,
        AddToGitignore,
        AddToGitExclude,
        ViewHistory,
        OpenPermalink,
        CopyPermalink,
        RenameEntry,
        TrashEntry,
        DeleteEntry,
    ]
);

/// `keys` with the platform's main modifier: Cmd on macOS, Ctrl elsewhere.
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

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    let (trash, delete) = if cfg!(target_os = "macos") {
        ("cmd-backspace", "cmd-alt-backspace")
    } else {
        ("delete", "ctrl-delete")
    };
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("right", ExpandSelected, context),
        KeyBinding::new("left", CollapseSelected, context),
        KeyBinding::new("enter", OpenSelected, context),
        KeyBinding::new(primary!("n"), NewFileHere, context),
        KeyBinding::new("alt-n", NewFolderHere, context),
        KeyBinding::new(primary!("alt-r"), RevealInFileManager, context),
        KeyBinding::new(primary!("shift-enter"), OpenWithSystem, context),
        KeyBinding::new(primary!("x"), CutEntry, context),
        KeyBinding::new(primary!("c"), CopyEntry, context),
        KeyBinding::new(primary!("d"), DuplicateEntry, context),
        KeyBinding::new(primary!("v"), PasteEntry, context),
        KeyBinding::new("alt-shift-c", CopyPath, context),
        KeyBinding::new(primary!("alt-shift-c"), CopyRelativePath, context),
        KeyBinding::new("f2", RenameEntry, context),
        KeyBinding::new(trash, TrashEntry, context),
        KeyBinding::new(delete, DeleteEntry, context),
    ]);
}

pub enum ExplorerEvent {
    OpenFile(PathBuf),
    /// A file or folder was renamed or moved; open files inside it move too.
    Renamed { from: PathBuf, to: PathBuf },
    /// A file or folder was trashed or deleted.
    Removed(PathBuf),
    /// Open this Markdown file rendered.
    OpenPreview(PathBuf),
    /// Show the terminal in this folder.
    OpenTerminal(PathBuf),
    /// Show the commits that changed this file or folder.
    ViewHistory(PathBuf),
    /// Open, or copy, the link to this file on the remote's website.
    Permalink { path: PathBuf, open: bool },
    /// Something to say in the status bar.
    Status { message: String, is_error: bool },
    /// A `.gitignore` or exclude file changed: what counts as changed may too.
    GitChanged,
}

/// Files cut or copied, waiting to be pasted.
struct Clipboard {
    paths: Vec<PathBuf>,
    cut: bool,
}

/// What a right-click menu is for, which decides what's in it.
struct MenuTarget {
    /// The explorer's folder itself (the empty space was clicked).
    is_root: bool,
    is_dir: bool,
    is_markdown: bool,
    can_paste: bool,
    in_repo: bool,
}

#[derive(Clone)]
struct Child {
    name: SharedString,
    path: PathBuf,
    is_dir: bool,
}

/// One visible row.
struct Entry {
    name: SharedString,
    path: PathBuf,
    is_dir: bool,
    depth: usize,
    /// The row with the name field for a new file or folder, first in its folder.
    new_entry: bool,
}

/// What the New File and New Folder buttons make.
#[derive(Clone, Copy, PartialEq)]
pub enum EntryKind {
    File,
    Folder,
}

/// A file or folder being named: a new one, before it's created, or one
/// being renamed.
struct NewEntry {
    kind: EntryKind,
    /// The folder it goes in.
    dir: PathBuf,
    /// The file or folder being renamed; `None` for a new one.
    rename: Option<PathBuf>,
    input: Entity<InputState>,
    /// Why the last name didn't work, shown above the tree.
    error: Option<SharedString>,
    _subscription: Subscription,
}

pub struct Explorer {
    root: PathBuf,
    expanded: HashSet<PathBuf>,
    /// Folder listings read so far, folders first. Cleared by `refresh`.
    listings: HashMap<PathBuf, Vec<Child>>,
    /// The visible rows in display order.
    entries: Vec<Entry>,
    /// The keyboard cursor.
    selected: Option<PathBuf>,
    /// The file in the active tab, highlighted.
    active_file: Option<PathBuf>,
    new_entry: Option<NewEntry>,
    clipboard: Option<Clipboard>,
    /// The repository the folder is in, for the Git items in the menu.
    repo: Option<Repo>,
    /// Set when a row is right-clicked, until its menu is built: the menu is
    /// for that row, not for the folder.
    menu_on_row: bool,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl EventEmitter<ExplorerEvent> for Explorer {}

impl Focusable for Explorer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Explorer {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            root: PathBuf::new(),
            expanded: HashSet::new(),
            listings: HashMap::new(),
            entries: Vec::new(),
            selected: None,
            active_file: None,
            new_entry: None,
            clipboard: None,
            repo: None,
            menu_on_row: false,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        };
        this.set_root(root, cx);
        this
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_repo(&mut self, repo: Option<Repo>, cx: &mut Context<Self>) {
        self.repo = repo;
        cx.notify();
    }

    /// Show a different folder.
    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = std::fs::canonicalize(&root).map_or(root, strip_verbatim_prefix);
        self.expanded.clear();
        self.listings.clear();
        self.selected = None;
        self.new_entry = None;
        self.rebuild();
        if let Some(active) = self.active_file.clone() {
            self.reveal(&active);
        }
        cx.notify();
    }

    /// Re-read every expanded folder from disk.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.listings.clear();
        self.expanded.retain(|dir| dir.is_dir());
        self.rebuild();
        cx.notify();
    }

    /// Highlight the file in the active tab, expanding folders to show it.
    pub fn set_active_file(&mut self, path: Option<&Path>, cx: &mut Context<Self>) {
        let path = path.map(|path| std::fs::canonicalize(path).map_or(path.to_path_buf(), strip_verbatim_prefix));
        if path == self.active_file {
            return;
        }
        self.active_file = path.clone();
        if let Some(path) = path {
            self.selected = Some(path.clone());
            self.reveal(&path);
        }
        cx.notify();
    }

    /// Expand the folders between the root and `path`, and scroll to it.
    fn reveal(&mut self, path: &Path) {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return;
        };
        let mut dir = self.root.clone();
        let components: Vec<_> = relative.components().collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            dir.push(component);
            self.expanded.insert(dir.clone());
        }
        self.rebuild();
        if let Some(ix) = self.index_of(path) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.new_entry = None;
        self.rebuild();
        cx.notify();
    }

    // ---- New file or folder ------------------------------------------------

    /// Show a name field for a new file or folder: in the selected folder,
    /// else the selected file's folder, else the root. Enter creates it (and
    /// opens a file); Escape or clicking elsewhere cancels.
    pub fn start_new_entry(&mut self, kind: EntryKind, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.target_folder();
        if dir != self.root {
            self.reveal(&dir);
            self.expanded.insert(dir.clone());
        }
        self.start_naming(kind, dir, None, window, cx);
    }

    /// Show a name field in place of the selected row, to rename it.
    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.target() else {
            return;
        };
        let Some(dir) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        let kind = if path.is_dir() { EntryKind::Folder } else { EntryKind::File };
        self.start_naming(kind, dir, Some(path), window, cx);
    }

    /// Show the name field: a new row at the top of `dir`, or in place of the
    /// row being renamed, with its name filled in and selected.
    fn start_naming(
        &mut self,
        kind: EntryKind,
        dir: PathBuf,
        rename: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match kind {
            EntryKind::File => "File name",
            EntryKind::Folder => "Folder name",
        };
        let current_name = rename
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned());
        let input = cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(placeholder);
            match &current_name {
                Some(name) => state.default_value(name.clone()),
                None => state,
            }
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.create_new_entry(window, cx),
                InputEvent::Blur => this.cancel_new_entry(cx),
                InputEvent::Change => {
                    if let Some(new_entry) = &mut this.new_entry
                        && new_entry.error.take().is_some()
                    {
                        cx.notify();
                    }
                }
                InputEvent::Focus => {}
            }
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            if current_name.is_some() {
                state.select_all(window, cx);
            }
        });
        self.new_entry = Some(NewEntry {
            kind,
            dir,
            rename,
            input,
            error: None,
            _subscription: subscription,
        });
        self.rebuild();
        if let Some(ix) = self.entries.iter().position(|entry| entry.new_entry) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Where a new file or folder goes, given the selection.
    fn target_folder(&self) -> PathBuf {
        let in_tree = |path: &Path| path.starts_with(&self.root);
        match self.selected.as_deref() {
            Some(path) if path.is_dir() && in_tree(path) => path.to_path_buf(),
            Some(path) => path
                .parent()
                .filter(|parent| in_tree(parent))
                .map_or_else(|| self.root.clone(), Path::to_path_buf),
            None => self.root.clone(),
        }
    }

    /// Create the named file or folder (and any folders in its name), or
    /// rename the one being renamed. A new file opens in a tab; a new folder
    /// is selected, ready for what goes in it.
    fn create_new_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(new_entry) = &self.new_entry else {
            return;
        };
        let kind = new_entry.kind;
        let name = new_entry.input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.cancel_new_entry(cx);
            self.focus_handle.focus(window, cx);
            return;
        }
        let result = match &new_entry.rename {
            Some(from) => rename_entry(from, &new_entry.dir, &name),
            None => create_entry(&new_entry.dir, &name, kind),
        };
        match result {
            Ok(path) => {
                let renamed = self.new_entry.take().and_then(|new_entry| new_entry.rename);
                self.focus_handle.focus(window, cx);
                self.selected = Some(path.clone());
                if let Some(from) = &renamed {
                    self.moved(from, &path);
                }
                // Re-read the folders, which now list it, and show it.
                self.refresh(cx);
                self.reveal(&path);
                match renamed {
                    Some(from) if from != path => cx.emit(ExplorerEvent::Renamed { from, to: path }),
                    Some(_) => {}
                    None if kind == EntryKind::File => cx.emit(ExplorerEvent::OpenFile(path)),
                    None => {}
                }
            }
            Err(error) => {
                if let Some(new_entry) = &mut self.new_entry {
                    new_entry.error = Some(error.into());
                }
                cx.notify();
            }
        }
    }

    fn cancel_new_entry(&mut self, cx: &mut Context<Self>) {
        if self.new_entry.take().is_some() {
            self.rebuild();
            cx.notify();
        }
    }

    /// Escape in the name field cancels it; anywhere else it's not ours.
    fn escape(&mut self, _: &input::Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_entry.is_none() {
            cx.propagate();
            return;
        }
        self.cancel_new_entry(cx);
        self.focus_handle.focus(window, cx);
    }

    // ---- The right-click menu and its actions ------------------------------

    /// The selected file or folder; `None` means the explorer's folder itself.
    fn target(&self) -> Option<PathBuf> {
        self.selected
            .clone()
            .filter(|path| path.starts_with(&self.root) && *path != self.root && path.exists())
    }

    fn target_or_root(&self) -> PathBuf {
        self.target().unwrap_or_else(|| self.root.clone())
    }

    fn status(&self, message: impl Into<String>, is_error: bool, cx: &mut Context<Self>) {
        cx.emit(ExplorerEvent::Status {
            message: message.into(),
            is_error,
        });
    }

    /// A right-click menu is opening: it's for the row that was clicked, or
    /// for the folder when the empty space was.
    fn menu_target(&mut self, cx: &mut Context<Self>) -> MenuTarget {
        if !std::mem::take(&mut self.menu_on_row) {
            self.selected = None;
            cx.notify();
        }
        let target = self.target();
        MenuTarget {
            is_root: target.is_none(),
            is_dir: target.as_deref().is_none_or(Path::is_dir),
            is_markdown: target.as_deref().is_some_and(|path| !path.is_dir() && crate::is_markdown(path)),
            can_paste: self.clipboard.is_some(),
            in_repo: self.repo.is_some(),
        }
    }

    /// Keep the tree pointing at a file or folder that moved from `from` to `to`.
    fn moved(&mut self, from: &Path, to: &Path) {
        let moved = |path: &Path| {
            let rest = path.strip_prefix(from).ok()?;
            Some(if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) })
        };
        self.expanded = self
            .expanded
            .drain()
            .map(|dir| moved(&dir).unwrap_or(dir))
            .collect();
        if let Some(active) = self.active_file.as_deref().and_then(moved) {
            self.active_file = Some(active);
        }
        self.forget(from);
    }

    /// Drop a path that's gone (or moved) from what's waiting to be pasted.
    fn forget(&mut self, path: &Path) {
        if let Some(clipboard) = &mut self.clipboard {
            clipboard.paths.retain(|pasting| !pasting.starts_with(path));
            if clipboard.paths.is_empty() {
                self.clipboard = None;
            }
        }
    }

    fn new_file_here(&mut self, _: &NewFileHere, window: &mut Window, cx: &mut Context<Self>) {
        if !self.naming() {
            self.start_new_entry(EntryKind::File, window, cx);
        }
    }

    fn new_folder_here(&mut self, _: &NewFolderHere, window: &mut Window, cx: &mut Context<Self>) {
        if !self.naming() {
            self.start_new_entry(EntryKind::Folder, window, cx);
        }
    }

    fn reveal_in_file_manager(&mut self, _: &RevealInFileManager, _: &mut Window, cx: &mut Context<Self>) {
        if !self.naming() {
            cx.reveal_path(&self.target_or_root());
        }
    }

    fn open_with_system(&mut self, _: &OpenWithSystem, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        if let Some(path) = self.target().filter(|path| path.is_file()) {
            cx.open_with_system(&path);
        }
    }

    fn open_in_terminal(&mut self, _: &OpenInTerminal, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let path = self.target_or_root();
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().map_or_else(|| self.root.clone(), Path::to_path_buf)
        };
        cx.emit(ExplorerEvent::OpenTerminal(dir));
    }

    fn cut_entry(&mut self, _: &CutEntry, _: &mut Window, cx: &mut Context<Self>) {
        self.set_clipboard(true, cx);
    }

    fn copy_entry(&mut self, _: &CopyEntry, _: &mut Window, cx: &mut Context<Self>) {
        self.set_clipboard(false, cx);
    }

    fn set_clipboard(&mut self, cut: bool, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let Some(path) = self.target() else {
            return;
        };
        let name = display_name(&path);
        self.clipboard = Some(Clipboard { paths: vec![path], cut });
        let verb = if cut { "Cut" } else { "Copied" };
        self.status(format!("{verb} \"{name}\". Paste it into a folder from its menu."), false, cx);
        cx.notify();
    }

    /// Paste what was cut or copied into the selected folder (or the selected
    /// file's folder). Copies of something already there get a new name.
    fn paste_entry(&mut self, _: &PasteEntry, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let Some(clipboard) = &self.clipboard else {
            return;
        };
        let (paths, cut) = (clipboard.paths.clone(), clipboard.cut);
        let dest = self.target_folder();
        let mut last = None;
        for from in &paths {
            match paste_one(from, &dest, cut) {
                Ok(to) => {
                    if cut && *from != to {
                        self.moved(from, &to);
                        cx.emit(ExplorerEvent::Renamed {
                            from: from.clone(),
                            to: to.clone(),
                        });
                    }
                    last = Some(to);
                }
                Err(error) => {
                    self.status(error, true, cx);
                    break;
                }
            }
        }
        if cut {
            // A move can't be repeated.
            self.clipboard = None;
        }
        self.refresh(cx);
        if let Some(to) = last {
            self.selected = Some(to.clone());
            self.reveal(&to);
        }
    }

    fn duplicate_entry(&mut self, _: &DuplicateEntry, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let Some(from) = self.target() else {
            return;
        };
        let (Some(dir), Some(name)) = (from.parent(), from.file_name()) else {
            return;
        };
        let to = unique_copy_path(dir, name);
        match copy_recursive(&from, &to) {
            Ok(()) => {
                self.refresh(cx);
                self.selected = Some(to.clone());
                self.reveal(&to);
                self.status(format!("Duplicated as \"{}\".", display_name(&to)), false, cx);
            }
            Err(err) => self.status(format!("Couldn't duplicate \"{}\": {err}", display_name(&from)), true, cx),
        }
    }

    fn copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if !self.naming() {
            let path = self.target_or_root().display().to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
            self.status(format!("Copied {path}"), false, cx);
        }
    }

    fn copy_relative_path(&mut self, _: &CopyRelativePath, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let path = self.target_or_root();
        let relative = match path.strip_prefix(&self.root) {
            Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
            _ => ".".to_string(),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(relative.clone()));
        self.status(format!("Copied {relative}"), false, cx);
    }

    fn add_to_gitignore(&mut self, _: &AddToGitignore, _: &mut Window, cx: &mut Context<Self>) {
        self.add_to_ignore_file(false, cx);
    }

    fn add_to_git_exclude(&mut self, _: &AddToGitExclude, _: &mut Window, cx: &mut Context<Self>) {
        self.add_to_ignore_file(true, cx);
    }

    /// Ignore the selected file or folder: for everyone, in the repository's
    /// `.gitignore`, or only in this clone, in `.git/info/exclude`.
    fn add_to_ignore_file(&mut self, exclude: bool, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let (Some(repo), Some(path)) = (self.repo.clone(), self.target()) else {
            return;
        };
        let Some(relative) = repo.relative(&path).filter(|relative| !relative.is_empty()) else {
            self.status(format!("\"{}\" isn't in the repository.", display_name(&path)), true, cx);
            return;
        };
        let pattern = format!(
            "/{}{}",
            escape_ignore_pattern(&relative),
            if path.is_dir() { "/" } else { "" }
        );
        let (file, file_name) = if exclude {
            (repo.git_dir.join("info").join("exclude"), ".git/info/exclude")
        } else {
            (repo.root.join(".gitignore"), ".gitignore")
        };
        match append_line(&file, &pattern) {
            Ok(true) => {
                self.status(format!("Added {pattern} to {file_name}."), false, cx);
                self.refresh(cx);
                cx.emit(ExplorerEvent::GitChanged);
            }
            Ok(false) => self.status(format!("{pattern} is already in {file_name}."), false, cx),
            Err(err) => self.status(format!("Couldn't change {file_name}: {err}"), true, cx),
        }
    }

    fn open_preview(&mut self, _: &OpenPreview, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        if let Some(path) = self.target().filter(|path| !path.is_dir() && crate::is_markdown(path)) {
            cx.emit(ExplorerEvent::OpenPreview(path));
        }
    }

    fn view_history(&mut self, _: &ViewHistory, _: &mut Window, cx: &mut Context<Self>) {
        if !self.naming() && self.repo.is_some() {
            cx.emit(ExplorerEvent::ViewHistory(self.target_or_root()));
        }
    }

    fn open_permalink(&mut self, _: &OpenPermalink, _: &mut Window, cx: &mut Context<Self>) {
        self.permalink(true, cx);
    }

    fn copy_permalink(&mut self, _: &CopyPermalink, _: &mut Window, cx: &mut Context<Self>) {
        self.permalink(false, cx);
    }

    fn permalink(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.naming() || self.repo.is_none() {
            return;
        }
        if let Some(path) = self.target().filter(|path| path.is_file()) {
            cx.emit(ExplorerEvent::Permalink { path, open });
        }
    }

    fn rename_entry(&mut self, _: &RenameEntry, window: &mut Window, cx: &mut Context<Self>) {
        if !self.naming() {
            self.start_rename(window, cx);
        }
    }

    fn trash_entry(&mut self, _: &TrashEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.remove(true, window, cx);
    }

    fn delete_entry(&mut self, _: &DeleteEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.remove(false, window, cx);
    }

    /// Move the selected file or folder to the Recycle Bin (or Trash), or
    /// delete it for good, after asking.
    fn remove(&mut self, to_trash: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let Some(path) = self.target() else {
            return;
        };
        let name = display_name(&path);
        let (message, detail, button) = if to_trash {
            (
                format!("Move \"{name}\" to the {TRASH_NAME}?"),
                None,
                format!("Move to {TRASH_NAME}"),
            )
        } else {
            (
                format!("Permanently delete \"{name}\"?"),
                Some("This can't be undone."),
                "Delete".to_string(),
            )
        };
        let answer = window.prompt(PromptLevel::Warning, &message, detail, &[button.as_str(), "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let removing = path.clone();
            let result = cx
                .background_spawn(async move {
                    if to_trash {
                        trash::delete(&removing).map_err(|err| err.to_string())
                    } else if removing.is_dir() {
                        std::fs::remove_dir_all(&removing).map_err(|err| err.to_string())
                    } else {
                        std::fs::remove_file(&removing).map_err(|err| err.to_string())
                    }
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    if this.selected.as_deref().is_some_and(|selected| selected.starts_with(&path)) {
                        this.selected = path.parent().map(Path::to_path_buf);
                    }
                    this.forget(&path);
                    this.refresh(cx);
                    let done = if to_trash {
                        format!("Moved \"{name}\" to the {TRASH_NAME}.")
                    } else {
                        format!("Deleted \"{name}\".")
                    };
                    this.status(done, false, cx);
                    cx.emit(ExplorerEvent::Removed(path));
                }
                Err(err) => this.status(format!("Couldn't remove \"{name}\": {err}"), true, cx),
            })
            .ok();
        })
        .detach();
    }

    fn listing(&mut self, dir: &Path) -> &[Child] {
        self.listings
            .entry(dir.to_path_buf())
            .or_insert_with(|| read_folder(dir))
    }

    /// Flatten the expanded tree into the visible rows.
    fn rebuild(&mut self) {
        let mut entries = Vec::new();
        let root = self.root.clone();
        self.push_children(&root, 0, &mut entries);
        self.entries = entries;
    }

    fn push_children(&mut self, dir: &Path, depth: usize, entries: &mut Vec<Entry>) {
        // A new entry's name field goes first in its folder; a renamed one's
        // takes the place of its row.
        let renaming = self.new_entry.as_ref().and_then(|new_entry| new_entry.rename.clone());
        if self
            .new_entry
            .as_ref()
            .is_some_and(|new_entry| new_entry.rename.is_none() && new_entry.dir == dir)
        {
            entries.push(Entry {
                name: SharedString::default(),
                path: dir.to_path_buf(),
                is_dir: false,
                depth,
                new_entry: true,
            });
        }
        let children = self.listing(dir).to_vec();
        for child in children {
            let expand = child.is_dir && self.expanded.contains(&child.path);
            let is_renaming = renaming.as_deref() == Some(child.path.as_path());
            entries.push(Entry {
                name: child.name,
                path: child.path.clone(),
                is_dir: child.is_dir,
                depth,
                new_entry: is_renaming,
            });
            if expand {
                self.push_children(&child.path, depth + 1, entries);
            }
        }
    }

    fn index_of(&self, path: &Path) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| !entry.new_entry && entry.path == path)
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected.as_deref().and_then(|path| self.index_of(path))
    }

    fn select_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get(ix) {
            self.selected = Some(entry.path.clone());
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn toggle(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
        self.rebuild();
        cx.notify();
    }

    fn click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        let (path, is_dir) = (entry.path.clone(), entry.is_dir);
        self.selected = Some(path.clone());
        self.focus_handle.focus(window, cx);
        if is_dir {
            self.toggle(&path, cx);
        } else {
            cx.emit(ExplorerEvent::OpenFile(path));
            cx.notify();
        }
    }

    /// Whether a new file or folder is being named. The name field passes on
    /// keys it doesn't use (Enter after it's handled them, the arrow keys),
    /// and the tree's own shortcuts would act on them: Enter would open or
    /// collapse the selected row, taking the field (and its error) away.
    fn naming(&self) -> bool {
        self.new_entry.is_some()
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let ix = self.selected_index().map_or(0, |ix| ix.saturating_sub(1));
        self.select_index(ix, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let ix = self
            .selected_index()
            .map_or(0, |ix| (ix + 1).min(self.entries.len().saturating_sub(1)));
        self.select_index(ix, cx);
    }

    fn expand_selected(&mut self, _: &ExpandSelected, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let Some(ix) = self.selected_index() else {
            return;
        };
        let entry = &self.entries[ix];
        if !entry.is_dir {
            return;
        }
        if self.expanded.contains(&entry.path) {
            // Step onto the first child, if there is one.
            if self.entries.get(ix + 1).is_some_and(|next| next.depth > entry.depth) {
                self.select_index(ix + 1, cx);
            }
        } else {
            let path = entry.path.clone();
            self.toggle(&path, cx);
        }
    }

    fn collapse_selected(&mut self, _: &CollapseSelected, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let Some(ix) = self.selected_index() else {
            return;
        };
        let entry = &self.entries[ix];
        if entry.is_dir && self.expanded.contains(&entry.path) {
            let path = entry.path.clone();
            self.toggle(&path, cx);
        } else if let Some(parent) = entry.path.parent().filter(|parent| *parent != self.root)
            && let Some(parent_ix) = self.index_of(parent)
        {
            self.select_index(parent_ix, cx);
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if self.naming() {
            return;
        }
        let Some(ix) = self.selected_index() else {
            return;
        };
        let entry = &self.entries[ix];
        if entry.is_dir {
            let path = entry.path.clone();
            self.toggle(&path, cx);
        } else {
            cx.emit(ExplorerEvent::OpenFile(entry.path.clone()));
        }
    }

    fn render_rows(&mut self, range: Range<usize>, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let focused = self.focus_handle.is_focused(window);
        let (hover, active_bg, selected_border, muted, dark) = (
            theme.list_hover,
            theme.list_active,
            theme.list_active_border,
            theme.muted_foreground,
            theme.is_dark(),
        );
        let cut: Vec<PathBuf> = self
            .clipboard
            .as_ref()
            .filter(|clipboard| clipboard.cut)
            .map(|clipboard| clipboard.paths.clone())
            .unwrap_or_default();

        range
            .filter_map(|ix| {
                let entry = self.entries.get(ix)?;
                if entry.new_entry {
                    let new_entry = self.new_entry.as_ref()?;
                    let icon = match new_entry.kind {
                        EntryKind::File => IconName::File,
                        EntryKind::Folder => IconName::Folder,
                    };
                    return Some(
                        div()
                            .id(("explorer-entry", ix))
                            .h(ROW_HEIGHT)
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl(px(8. + INDENT * entry.depth as f32))
                            .pr_2()
                            .text_sm()
                            .child(div().w(px(14.)).flex_none())
                            .child(Icon::new(icon).small().text_color(muted))
                            .child(div().flex_1().min_w_0().child(Input::new(&new_entry.input).xsmall()))
                            .into_any_element(),
                    );
                }
                let is_active = self.active_file.as_deref() == Some(entry.path.as_path());
                let is_selected = self.selected.as_deref() == Some(entry.path.as_path());
                let is_cut = cut.iter().any(|path| entry.path.starts_with(path));
                let expanded = self.expanded.contains(&entry.path);
                let (chevron, icon) = match (entry.is_dir, expanded) {
                    (true, true) => (
                        Some(IconName::ChevronDown),
                        Icon::new(IconName::FolderOpen).text_color(muted),
                    ),
                    (true, false) => (
                        Some(IconName::ChevronRight),
                        Icon::new(IconName::Folder).text_color(muted),
                    ),
                    (false, _) => (
                        None,
                        match file_icon(&entry.name) {
                            Some((icon, (on_dark, on_light))) => {
                                Icon::new(icon).text_color(rgb(if dark { on_dark } else { on_light }))
                            }
                            None => Icon::new(IconName::File).text_color(muted),
                        },
                    ),
                };
                let path = entry.path.clone();

                Some(
                    div()
                        .id(("explorer-entry", ix))
                        .h(ROW_HEIGHT)
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl(px(8. + INDENT * entry.depth as f32))
                        .pr_2()
                        .text_sm()
                        .border_1()
                        .border_color(transparent_black())
                        .hover(|style| style.bg(hover))
                        .when(is_active, |row| row.bg(active_bg))
                        .when(is_selected && focused, |row| row.border_color(selected_border))
                        // Cut, waiting to be pasted somewhere else.
                        .when(is_cut, |row| row.opacity(0.5))
                        .child(
                            div()
                                .w(px(14.))
                                .flex_none()
                                .text_color(muted)
                                .when_some(chevron, |slot, chevron| {
                                    slot.child(Icon::new(chevron).xsmall())
                                }),
                        )
                        .child(icon.small())
                        .child(div().min_w_0().truncate().child(entry.name.clone()))
                        .on_click(cx.listener(move |this, _, window, cx| this.click(ix, window, cx)))
                        // The right-click menu (on the list) is for this row.
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _, window, cx| {
                                this.selected = Some(path.clone());
                                this.menu_on_row = true;
                                this.focus_handle.focus(window, cx);
                                cx.notify();
                            }),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for Explorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let folder_name = self
            .root
            .file_name()
            .map_or_else(|| self.root.display().to_string(), |name| name.to_string_lossy().into_owned())
            .to_uppercase();
        let (muted, danger) = (theme.muted_foreground, theme.danger);

        div()
            .v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::expand_selected))
            .on_action(cx.listener(Self::collapse_selected))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::new_file_here))
            .on_action(cx.listener(Self::new_folder_here))
            .on_action(cx.listener(Self::reveal_in_file_manager))
            .on_action(cx.listener(Self::open_with_system))
            .on_action(cx.listener(Self::open_in_terminal))
            .on_action(cx.listener(Self::cut_entry))
            .on_action(cx.listener(Self::copy_entry))
            .on_action(cx.listener(Self::duplicate_entry))
            .on_action(cx.listener(Self::paste_entry))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::add_to_gitignore))
            .on_action(cx.listener(Self::add_to_git_exclude))
            .on_action(cx.listener(Self::view_history))
            .on_action(cx.listener(Self::open_preview))
            .on_action(cx.listener(Self::open_permalink))
            .on_action(cx.listener(Self::copy_permalink))
            .on_action(cx.listener(Self::rename_entry))
            .on_action(cx.listener(Self::trash_entry))
            .on_action(cx.listener(Self::delete_entry))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .pl_3()
                    .pr_1()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(folder_name),
                    )
                    .child(
                        Button::new("explorer-new-file")
                            .ghost()
                            .xsmall()
                            .icon(LucideIcon::FilePlus)
                            .tooltip("New File…")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_new_entry(EntryKind::File, window, cx)
                            })),
                    )
                    .child(
                        Button::new("explorer-new-folder")
                            .ghost()
                            .xsmall()
                            .icon(LucideIcon::FolderPlus)
                            .tooltip("New Folder…")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_new_entry(EntryKind::Folder, window, cx)
                            })),
                    )
                    .child(
                        Button::new("explorer-refresh")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        Button::new("explorer-collapse")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronsUpDown)
                            .tooltip("Collapse folders")
                            .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
                    ),
            )
            .when_some(
                self.new_entry.as_ref().and_then(|new_entry| new_entry.error.clone()),
                |column, error| {
                    column.child(
                        div()
                            .px_3()
                            .pb_1()
                            .text_xs()
                            .text_color(danger)
                            .child(error),
                    )
                },
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .when(self.entries.is_empty(), |empty| {
                        empty.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_sm()
                                .text_color(muted)
                                .child("This folder is empty."),
                        )
                    })
                    .child(
                        uniform_list(
                            "explorer-entries",
                            self.entries.len(),
                            cx.processor(Self::render_rows),
                        )
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    .vertical_scrollbar(&self.scroll)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _, window, cx| this.focus_handle.focus(window, cx)),
                    )
                    .context_menu({
                        let explorer = cx.weak_entity();
                        let focus = self.focus_handle.clone();
                        move |menu, _, cx| {
                            match explorer.update(cx, |explorer, cx| explorer.menu_target(cx)) {
                                Ok(target) => build_menu(menu.action_context(focus.clone()), &target),
                                Err(_) => menu,
                            }
                        }
                    }),
            )
    }
}

#[cfg(target_os = "windows")]
const REVEAL_LABEL: &str = "Reveal in File Explorer";
#[cfg(target_os = "macos")]
const REVEAL_LABEL: &str = "Reveal in Finder";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const REVEAL_LABEL: &str = "Reveal in File Manager";

/// What deleted files go to, by its name on this platform.
const TRASH_NAME: &str = if cfg!(target_os = "windows") { "Recycle Bin" } else { "Trash" };

/// The right-click menu. Each item dispatches an action to the explorer,
/// which acts on the selected row (or the folder); the shortcuts shown are
/// the explorer's.
fn build_menu(menu: PopupMenu, target: &MenuTarget) -> PopupMenu {
    let is_file = !target.is_root && !target.is_dir;
    let no_git = !target.in_repo;
    let menu = menu
        .menu("New File", Box::new(NewFileHere))
        .menu("New Folder", Box::new(NewFolderHere))
        .separator()
        .menu(REVEAL_LABEL, Box::new(RevealInFileManager))
        .menu_with_disabled("Open in Default App", Box::new(OpenWithSystem), !is_file);
    let menu = if target.is_markdown {
        menu.menu("Open Preview", Box::new(OpenPreview))
    } else {
        menu
    };
    menu.menu("Open in Terminal", Box::new(OpenInTerminal))
        .separator()
        .menu_with_disabled("Cut", Box::new(CutEntry), target.is_root)
        .menu_with_disabled("Copy", Box::new(CopyEntry), target.is_root)
        .menu_with_disabled("Duplicate", Box::new(DuplicateEntry), target.is_root)
        .menu_with_disabled("Paste", Box::new(PasteEntry), !target.can_paste)
        .separator()
        .menu("Copy Path", Box::new(CopyPath))
        .menu("Copy Relative Path", Box::new(CopyRelativePath))
        .separator()
        .menu_with_disabled("Add to .gitignore", Box::new(AddToGitignore), no_git || target.is_root)
        .menu_with_disabled("Add to .git/info/exclude", Box::new(AddToGitExclude), no_git || target.is_root)
        .menu_with_disabled("View History", Box::new(ViewHistory), no_git)
        .menu_with_disabled("Open File Permalink", Box::new(OpenPermalink), no_git || !is_file)
        .menu_with_disabled("Copy File Permalink", Box::new(CopyPermalink), no_git || !is_file)
        .separator()
        .menu_with_disabled("Rename", Box::new(RenameEntry), target.is_root)
        .menu_with_disabled(format!("Move to {TRASH_NAME}"), Box::new(TrashEntry), target.is_root)
        .menu_with_disabled("Delete", Box::new(DeleteEntry), target.is_root)
}

/// List a folder: folders first, then files, each sorted case-insensitively.
/// An unreadable folder shows as empty.
fn read_folder(dir: &Path) -> Vec<Child> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut children: Vec<Child> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if HIDDEN_NAMES.contains(&name.as_str()) {
                return None;
            }
            // Follows symlinks, so a link to a folder expands like one.
            let is_dir = entry.path().is_dir();
            Some(Child {
                name: name.into(),
                path: entry.path(),
                is_dir,
            })
        })
        .collect();
    children.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    children
}

/// Create an empty file or folder named `name` in `dir`, along with any
/// folders the name includes ("src/new.rs", "docs/guides"). Returns its
/// path, or why it couldn't.
fn create_entry(dir: &Path, name: &str, kind: EntryKind) -> Result<PathBuf, String> {
    let path = join_name(dir, name, kind)?;
    if path.exists() {
        return Err(format!("\"{name}\" already exists."));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("Couldn't create the folder: {err}"))?;
    }
    match kind {
        EntryKind::File => std::fs::File::create_new(&path).map(drop),
        EntryKind::Folder => std::fs::create_dir(&path),
    }
    .map_err(|err| format!("Couldn't create \"{name}\": {err}"))?;
    Ok(path)
}

/// `dir` joined with a typed name, which may include folders but must stay
/// inside `dir`.
fn join_name(dir: &Path, name: &str, kind: EntryKind) -> Result<PathBuf, String> {
    let mut path = dir.to_path_buf();
    for component in Path::new(name).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            _ => return Err(format!("\"{name}\" isn't a name inside this folder.")),
        }
    }
    // "notes/" names a folder, so it can't be a file's name.
    if path == dir || (kind == EntryKind::File && name.ends_with(['/', '\\'])) {
        return Err(match kind {
            EntryKind::File => "Enter a file name.".into(),
            EntryKind::Folder => "Enter a folder name.".into(),
        });
    }
    Ok(path)
}

/// Rename (or move, if the name includes folders) `from` to `name` in `dir`.
fn rename_entry(from: &Path, dir: &Path, name: &str) -> Result<PathBuf, String> {
    let kind = if from.is_dir() { EntryKind::Folder } else { EntryKind::File };
    let to = join_name(dir, name, kind)?;
    if to == from {
        return Ok(to);
    }
    // On a case-insensitive disk, "readme.md" to "README.md" is the same file.
    let same_file = matches!(
        (std::fs::canonicalize(from), std::fs::canonicalize(&to)),
        (Ok(a), Ok(b)) if a == b
    );
    if to.exists() && !same_file {
        return Err(format!("\"{name}\" already exists."));
    }
    if from.is_dir() && to.starts_with(from) {
        return Err("A folder can't go inside itself.".into());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("Couldn't create the folder: {err}"))?;
    }
    std::fs::rename(from, &to).map_err(|err| format!("Couldn't rename \"{}\": {err}", display_name(from)))?;
    Ok(to)
}

/// Paste one cut or copied file or folder into `dest`. A copy of something
/// already there gets a new name; a move onto something already there fails.
fn paste_one(from: &Path, dest: &Path, cut: bool) -> Result<PathBuf, String> {
    let name = from.file_name().ok_or("That can't be pasted.")?;
    if !from.exists() {
        return Err(format!("\"{}\" isn't there any more.", display_name(from)));
    }
    if from.is_dir() && dest.starts_with(from) {
        return Err("A folder can't go inside itself.".into());
    }
    if !cut {
        let to = unique_copy_path(dest, name);
        copy_recursive(from, &to).map_err(|err| format!("Couldn't copy \"{}\": {err}", display_name(from)))?;
        return Ok(to);
    }
    if from.parent() == Some(dest) {
        return Ok(from.to_path_buf());
    }
    let to = dest.join(name);
    if to.exists() {
        return Err(format!("There's already a \"{}\" in that folder.", display_name(&to)));
    }
    // A rename can't cross drives; copy and remove instead.
    if std::fs::rename(from, &to).is_err() {
        copy_recursive(from, &to).map_err(|err| format!("Couldn't move \"{}\": {err}", display_name(from)))?;
        let removed = if from.is_dir() {
            std::fs::remove_dir_all(from)
        } else {
            std::fs::remove_file(from)
        };
        removed.map_err(|err| format!("Copied \"{}\", but couldn't remove the original: {err}", display_name(from)))?;
    }
    Ok(to)
}

/// Copy a file, or a folder and everything in it.
fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(drop)
    }
}

/// A free name in `dir` for a copy of `name`: "notes copy.md", then
/// "notes copy 2.md", and so on.
fn unique_copy_path(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let path = Path::new(name);
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let stem = path.file_stem().unwrap_or(name).to_string_lossy();
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| {
            let suffix = if n == 1 { " copy".to_string() } else { format!(" copy {n}") };
            dir.join(format!("{stem}{suffix}{extension}"))
        })
        .find(|candidate| !candidate.exists())
        .expect("some copy name is free")
}

/// A file or folder's name, for messages.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Escape the characters `.gitignore` patterns treat specially.
fn escape_ignore_pattern(relative: &str) -> String {
    relative
        .chars()
        .flat_map(|c| match c {
            '\\' | '*' | '?' | '[' => vec!['\\', c],
            c => vec![c],
        })
        .collect()
}

/// Add `line` to the end of a text file (creating it, and its folder, if
/// needed), keeping its line endings. `Ok(false)` if it's already there.
fn append_line(file: &Path, line: &str) -> std::io::Result<bool> {
    let existing = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    if existing.lines().any(|existing| existing.trim() == line) {
        return Ok(false);
    }
    let newline = if existing.contains("\r\n") { "\r\n" } else { "\n" };
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push_str(newline);
    }
    text.push_str(line);
    text.push_str(newline);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, text)?;
    Ok(true)
}

/// `canonicalize` on Windows returns `\\?\C:\...` paths; strip the prefix so
/// they compare equal to, and display like, ordinary paths.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob of `gpui_kit` would replace `#[test]`.
    use super::{append_line, escape_ignore_pattern, paste_one, unique_copy_path};
    use std::path::PathBuf;

    /// An empty folder of its own for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gitstepwise-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_get_free_names() {
        let dir = scratch("copies");
        std::fs::write(dir.join("notes.md"), "").unwrap();
        assert_eq!(unique_copy_path(&dir, "notes.md".as_ref()), dir.join("notes copy.md"));
        std::fs::write(dir.join("notes copy.md"), "").unwrap();
        assert_eq!(unique_copy_path(&dir, "notes.md".as_ref()), dir.join("notes copy 2.md"));
        assert_eq!(unique_copy_path(&dir, "new.md".as_ref()), dir.join("new.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pasting_copies_and_moves() {
        let dir = scratch("paste");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(a.join("inner")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("inner").join("f.txt"), "hi").unwrap();

        let copied = paste_one(&a.join("inner"), &b, false).unwrap();
        assert_eq!(std::fs::read_to_string(copied.join("f.txt")).unwrap(), "hi");
        assert!(a.join("inner").exists());

        let moved = paste_one(&a.join("inner"), &dir, true).unwrap();
        assert!(moved.join("f.txt").exists() && !a.join("inner").exists());
        assert!(paste_one(&dir.join("b"), &dir.join("b").join("inner"), true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignore_lines_are_added_once() {
        let dir = scratch("ignore");
        let file = dir.join(".gitignore");
        std::fs::write(&file, "/target\r\n*.log").unwrap();
        assert!(append_line(&file, "/notes.md").unwrap());
        assert!(!append_line(&file, "/notes.md").unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "/target\r\n*.log\r\n/notes.md\r\n");
        assert_eq!(escape_ignore_pattern("a [b]*.md"), r"a \[b]\*.md");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
