//! The file explorer sidebar: a tree of one folder, read from disk lazily as
//! folders are expanded, and virtualized so large folders stay fast.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

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
    ]
);

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("right", ExpandSelected, context),
        KeyBinding::new("left", CollapseSelected, context),
        KeyBinding::new("enter", OpenSelected, context),
    ]);
}

pub enum ExplorerEvent {
    OpenFile(PathBuf),
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
    /// The row with the new file's name field, first in its folder.
    new_file: bool,
}

/// A file being named, before it's created.
struct NewFile {
    /// The folder it goes in.
    dir: PathBuf,
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
    new_file: Option<NewFile>,
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
            new_file: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        };
        this.set_root(root, cx);
        this
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Show a different folder.
    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = std::fs::canonicalize(&root).map_or(root, strip_verbatim_prefix);
        self.expanded.clear();
        self.listings.clear();
        self.selected = None;
        self.new_file = None;
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
        self.new_file = None;
        self.rebuild();
        cx.notify();
    }

    // ---- New file ----------------------------------------------------------

    /// Show a name field for a new file: in the selected folder, else the
    /// selected file's folder, else the root. Enter creates and opens it;
    /// Escape or clicking elsewhere cancels.
    pub fn start_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.target_folder();
        if dir != self.root {
            self.reveal(&dir);
            self.expanded.insert(dir.clone());
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("File name"));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.create_new_file(window, cx),
                InputEvent::Blur => this.cancel_new_file(cx),
                InputEvent::Change => {
                    if let Some(new_file) = &mut this.new_file {
                        if new_file.error.take().is_some() {
                            cx.notify();
                        }
                    }
                }
                InputEvent::Focus => {}
            }
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.new_file = Some(NewFile {
            dir,
            input,
            error: None,
            _subscription: subscription,
        });
        self.rebuild();
        if let Some(ix) = self.entries.iter().position(|entry| entry.new_file) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Where a new file goes, given the selection.
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

    /// Create the named file (and any folders in its name), then open it.
    fn create_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(new_file) = &self.new_file else {
            return;
        };
        let name = new_file.input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.cancel_new_file(cx);
            self.focus_handle.focus(window, cx);
            return;
        }
        match create_file(&new_file.dir, &name) {
            Ok(path) => {
                self.new_file = None;
                self.focus_handle.focus(window, cx);
                self.selected = Some(path.clone());
                // Re-read the folders, which now list the file, and show it.
                self.refresh(cx);
                self.reveal(&path);
                cx.emit(ExplorerEvent::OpenFile(path));
            }
            Err(error) => {
                if let Some(new_file) = &mut self.new_file {
                    new_file.error = Some(error.into());
                }
                cx.notify();
            }
        }
    }

    fn cancel_new_file(&mut self, cx: &mut Context<Self>) {
        if self.new_file.take().is_some() {
            self.rebuild();
            cx.notify();
        }
    }

    /// Escape in the name field cancels it; anywhere else it's not ours.
    fn escape(&mut self, _: &input::Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_file.is_none() {
            cx.propagate();
            return;
        }
        self.cancel_new_file(cx);
        self.focus_handle.focus(window, cx);
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
        if self.new_file.as_ref().is_some_and(|new_file| new_file.dir == dir) {
            entries.push(Entry {
                name: SharedString::default(),
                path: dir.to_path_buf(),
                is_dir: false,
                depth,
                new_file: true,
            });
        }
        let children = self.listing(dir).to_vec();
        for child in children {
            let expand = child.is_dir && self.expanded.contains(&child.path);
            entries.push(Entry {
                name: child.name,
                path: child.path.clone(),
                is_dir: child.is_dir,
                depth,
                new_file: false,
            });
            if expand {
                self.push_children(&child.path, depth + 1, entries);
            }
        }
    }

    fn index_of(&self, path: &Path) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| !entry.new_file && entry.path == path)
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

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index().map_or(0, |ix| ix.saturating_sub(1));
        self.select_index(ix, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self
            .selected_index()
            .map_or(0, |ix| (ix + 1).min(self.entries.len().saturating_sub(1)));
        self.select_index(ix, cx);
    }

    fn expand_selected(&mut self, _: &ExpandSelected, _: &mut Window, cx: &mut Context<Self>) {
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
        let Some(ix) = self.selected_index() else {
            return;
        };
        let entry = &self.entries[ix];
        if entry.is_dir && self.expanded.contains(&entry.path) {
            let path = entry.path.clone();
            self.toggle(&path, cx);
        } else if let Some(parent) = entry.path.parent().filter(|parent| *parent != self.root) {
            if let Some(parent_ix) = self.index_of(parent) {
                self.select_index(parent_ix, cx);
            }
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
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
        let (hover, active_bg, selected_border, muted) = (
            theme.list_hover,
            theme.list_active,
            theme.list_active_border,
            theme.muted_foreground,
        );

        range
            .filter_map(|ix| {
                let entry = self.entries.get(ix)?;
                if entry.new_file {
                    let new_file = self.new_file.as_ref()?;
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
                            .child(Icon::new(IconName::File).small().text_color(muted))
                            .child(div().flex_1().min_w_0().child(Input::new(&new_file.input).xsmall()))
                            .into_any_element(),
                    );
                }
                let is_active = self.active_file.as_deref() == Some(entry.path.as_path());
                let is_selected = self.selected.as_deref() == Some(entry.path.as_path());
                let expanded = self.expanded.contains(&entry.path);
                let (chevron, icon) = match (entry.is_dir, expanded) {
                    (true, true) => (Some(IconName::ChevronDown), IconName::FolderOpen),
                    (true, false) => (Some(IconName::ChevronRight), IconName::Folder),
                    (false, _) => (None, IconName::File),
                };

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
                        .child(
                            div()
                                .w(px(14.))
                                .flex_none()
                                .text_color(muted)
                                .when_some(chevron, |slot, chevron| {
                                    slot.child(Icon::new(chevron).xsmall())
                                }),
                        )
                        .child(Icon::new(icon).small().text_color(muted))
                        .child(div().min_w_0().truncate().child(entry.name.clone()))
                        .on_click(cx.listener(move |this, _, window, cx| this.click(ix, window, cx)))
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
                            .on_click(cx.listener(|this, _, window, cx| this.start_new_file(window, cx))),
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
                self.new_file.as_ref().and_then(|new_file| new_file.error.clone()),
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
                    .vertical_scrollbar(&self.scroll),
            )
    }
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

/// Create an empty file named `name` in `dir`, along with any folders the
/// name includes ("src/new.rs"). Returns its path, or why it couldn't.
fn create_file(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let mut path = dir.to_path_buf();
    for component in Path::new(name).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            _ => return Err(format!("\"{name}\" isn't a name inside this folder.")),
        }
    }
    if path == dir || name.ends_with(['/', '\\']) {
        return Err("Enter a file name.".into());
    }
    if path.exists() {
        return Err(format!("\"{name}\" already exists."));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("Couldn't create the folder: {err}"))?;
    }
    std::fs::File::create_new(&path).map_err(|err| format!("Couldn't create \"{name}\": {err}"))?;
    Ok(path)
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
