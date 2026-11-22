//! Projects: folders opened with Open Folder (or from the command line),
//! remembered with the files that were open in each, and a picker to
//! switch between them, like Zed's Open Recent.
//!
//! The list is kept in `projects.json` in the app's config folder, shared
//! by every window.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::button::*;
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

/// How many projects are remembered; the longest unopened go first.
const MAX_PROJECTS: usize = 50;
const FILE_VERSION: u32 = 1;
const FILE_NAME: &str = "projects.json";
#[cfg(target_os = "linux")]
const CONFIG_FOLDER: &str = "gitstepwise";
#[cfg(not(target_os = "linux"))]
const CONFIG_FOLDER: &str = "Gitstepwise";

/// A folder opened as a project, and what was open in it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub path: PathBuf,
    /// When it was last opened, in milliseconds since the Unix epoch.
    pub opened: u64,
    /// The files open in tabs, in tab order.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// The file that was showing.
    #[serde(default)]
    pub active: Option<PathBuf>,
}

/// Every remembered project, the most recently opened first.
#[derive(Default, Serialize, Deserialize)]
pub struct Projects {
    version: u32,
    projects: Vec<Project>,
    /// Where the list is kept; `None` when there's no config folder.
    #[serde(skip)]
    file: Option<PathBuf>,
}

impl Global for Projects {}

impl Projects {
    /// The list as last saved, or an empty one.
    pub fn load() -> Self {
        let file = dirs::config_dir().map(|dir| dir.join(CONFIG_FOLDER).join(FILE_NAME));
        let mut projects = file
            .as_deref()
            .and_then(|file| std::fs::read(file).ok())
            .map(|bytes| Self::from_json(&bytes))
            .unwrap_or_default();
        projects.file = file;
        projects
    }

    /// The list in `bytes`; empty if they can't be read as one.
    fn from_json(bytes: &[u8]) -> Self {
        serde_json::from_slice::<Self>(bytes)
            .ok()
            .filter(|projects| projects.version == FILE_VERSION)
            .unwrap_or_default()
    }

    /// Write the list to its file. Failing to is no reason to stop
    /// working, so it's only reported in a debug build's console.
    pub fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let saved = Self {
            version: FILE_VERSION,
            projects: self.projects.clone(),
            file: None,
        };
        let result = serde_json::to_vec_pretty(&saved)
            .map_err(std::io::Error::other)
            .and_then(|json| write_whole(file, &json));
        if let Err(err) = result {
            eprintln!("Couldn't save the list of projects to {}: {err}", file.display());
        }
    }

    pub fn list(&self) -> &[Project] {
        &self.projects
    }

    pub fn get(&self, path: &Path) -> Option<&Project> {
        self.projects.iter().find(|project| same_folder(&project.path, path))
    }

    /// `path` was opened as a project: put it first.
    pub fn opened(&mut self, path: PathBuf, now: u64) {
        let project = match self.projects.iter().position(|project| same_folder(&project.path, &path)) {
            Some(ix) => Project {
                opened: now,
                ..self.projects.remove(ix)
            },
            None => Project {
                path,
                opened: now,
                files: Vec::new(),
                active: None,
            },
        };
        self.projects.insert(0, project);
        self.projects.truncate(MAX_PROJECTS);
    }

    /// Remember which files a project has open. Returns whether that
    /// changed anything; a folder that isn't a project is left alone.
    pub fn set_session(&mut self, path: &Path, files: &[PathBuf], active: Option<&Path>) -> bool {
        let Some(project) = self.projects.iter_mut().find(|project| same_folder(&project.path, path)) else {
            return false;
        };
        if project.files == files && project.active.as_deref() == active {
            return false;
        }
        project.files = files.to_vec();
        project.active = active.map(Path::to_path_buf);
        true
    }

    pub fn remove(&mut self, path: &Path) {
        self.projects.retain(|project| !same_folder(&project.path, path));
    }
}

/// Write `bytes` to a file next to `file`, then move it into place, so a
/// crash never leaves half a file.
fn write_whole(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = file.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, file)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Whether two paths name the same folder, written the same way apart
/// from a trailing separator (and, on Windows, letter case).
pub fn same_folder(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        let lower = |path: &Path| path.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
        lower(a) == lower(b)
    } else {
        a.components().eq(b.components())
    }
}

/// The name a folder goes by: its last part.
pub fn folder_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// `path` with the home folder written as `~`, as it's shown in the list.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// How well `query` matches `text`: its characters, in order, ignoring
/// case and spaces. Higher is better; `None` if they aren't all there.
/// Characters next to each other and at the start of a word count most.
fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut next = 0;
    let mut previous: Option<usize> = None;
    for wanted in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let at = (next..text.len()).find(|&ix| text[ix] == wanted)?;
        score += 1;
        if previous.is_some_and(|previous| previous + 1 == at) {
            score += 5;
        }
        if at == 0 || matches!(text[at - 1], '/' | '\\' | '-' | '_' | '.' | ' ') {
            score += 3;
        }
        previous = Some(at);
        next = at + 1;
    }
    Some(score)
}

/// The projects that match `query`, best first, as indices into
/// `projects`. A match in the folder's name beats one elsewhere in its
/// path; ties keep the most recently opened first.
fn ranked(projects: &[Project], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..projects.len()).collect();
    }
    let mut scored: Vec<(usize, i32)> = projects
        .iter()
        .enumerate()
        .filter_map(|(ix, project)| {
            let in_name = fuzzy_score(query, &folder_name(&project.path)).map(|score| score + 100);
            let in_path = fuzzy_score(query, &project.path.to_string_lossy());
            Some((ix, in_name.max(in_path)?))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(ix, _)| ix).collect()
}

/// The picker's rows: the projects that match what's typed.
struct ProjectList {
    projects: Vec<Project>,
    /// Whether each project's folder is still there.
    present: Vec<bool>,
    home: Option<PathBuf>,
    query: String,
    matches: Vec<usize>,
    selected: Option<usize>,
}

impl ProjectList {
    fn new(projects: Vec<Project>) -> Self {
        let present = projects.iter().map(|project| project.path.is_dir()).collect();
        let matches = (0..projects.len()).collect();
        Self {
            projects,
            present,
            home: dirs::home_dir(),
            query: String::new(),
            matches,
            selected: None,
        }
    }

    fn path_at(&self, row: usize) -> Option<PathBuf> {
        let ix = *self.matches.get(row)?;
        Some(self.projects[ix].path.clone())
    }

    fn remove(&mut self, path: &Path) {
        if let Some(ix) = self.projects.iter().position(|project| same_folder(&project.path, path)) {
            self.projects.remove(ix);
            self.present.remove(ix);
        }
        self.matches = ranked(&self.projects, &self.query);
    }
}

impl ListDelegate for ProjectList {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.query = query.to_string();
        self.matches = ranked(&self.projects, query);
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.matches.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        let row = ix.row;
        let project_ix = *self.matches.get(row)?;
        let project = &self.projects[project_ix];
        let present = self.present[project_ix];
        let name = folder_name(&project.path);
        let mut detail = display_path(&project.path, self.home.as_deref());
        if !present {
            // First, so a long path cut short doesn't hide it.
            detail.insert_str(0, "Not found: ");
        }
        let muted = cx.theme().muted_foreground;
        let path = project.path.clone();
        let list = cx.entity().downgrade();

        Some(
            ListItem::new(("project", row))
                .selected(self.selected == Some(row))
                .accessibility_label(name.clone())
                .child(
                    // A set width, so a long path is cut short (the picker
                    // is 560 wide) and leaves room for the remove button.
                    v_flex()
                        .w(px(480.))
                        .overflow_hidden()
                        .when(!present, |column| column.opacity(0.5))
                        .child(div().text_sm().truncate().child(name))
                        .child(div().text_xs().text_color(muted).truncate().child(detail)),
                )
                .suffix(move |_, _| {
                    let path = path.clone();
                    let list = list.clone();
                    Button::new(("forget-project", row))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Remove from the list")
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            forget(&path, &list, window, cx);
                        })
                }),
        )
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<ListState<Self>>) -> impl IntoElement {
        let message = if self.projects.is_empty() {
            "No other projects yet. Open a folder to add one."
        } else {
            "No project matches."
        };
        div()
            .p_4()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(message)
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|ix| ix.row);
    }
}

/// Take a project off the list, and out of the picker showing it.
fn forget(
    path: &Path,
    list: &WeakEntity<ListState<ProjectList>>,
    window: &mut Window,
    cx: &mut App,
) {
    cx.update_global::<Projects, _>(|projects, _| {
        projects.remove(path);
        projects.save();
    });
    list.update(cx, |list, cx| {
        list.delegate_mut().remove(path);
        let count = list.delegate().matches.len();
        let selected = list.selected_index().map(|ix| ix.row.min(count.saturating_sub(1)));
        list.set_selected_index(selected.filter(|_| count > 0).map(IndexPath::new), window, cx);
        cx.notify();
    })
    .ok();
}

/// What the workspace needs to hear about.
pub enum ProjectPickerEvent {
    Open(PathBuf),
    /// Choose a folder that isn't on the list yet.
    OpenFolder,
    Dismissed,
}

/// The list of projects to switch to, with a search box: type to narrow
/// it, Enter to open the selected one, Escape to close.
pub struct ProjectPicker {
    list: Entity<ListState<ProjectList>>,
    _subscription: Subscription,
}

impl EventEmitter<ProjectPickerEvent> for ProjectPicker {}

impl Focusable for ProjectPicker {
    /// The search box's.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.list.read(cx).focus_handle(cx)
    }
}

impl ProjectPicker {
    /// The remembered projects, leaving out `current`, the one showing.
    pub fn new(current: &Path, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let projects: Vec<Project> = cx
            .global::<Projects>()
            .list()
            .iter()
            .filter(|project| !same_folder(&project.path, current))
            .cloned()
            .collect();
        let any = !projects.is_empty();
        let list = cx.new(|cx| ListState::new(ProjectList::new(projects), window, cx).searchable(true));
        if any {
            // The last project opened before this one, for Enter to go back to.
            list.update(cx, |list, cx| list.set_selected_index(Some(IndexPath::default()), window, cx));
        }
        let subscription = cx.subscribe_in(&list, window, |_, list, event: &ListEvent, _, cx| match event {
            ListEvent::Confirm(ix) => {
                if let Some(path) = list.read(cx).delegate().path_at(ix.row) {
                    cx.emit(ProjectPickerEvent::Open(path));
                }
            }
            ListEvent::Cancel => cx.emit(ProjectPickerEvent::Dismissed),
            ListEvent::Select(_) => {}
        });
        Self {
            list,
            _subscription: subscription,
        }
    }
}

impl Render for ProjectPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .w(px(560.))
            .popover_style(cx)
            .occlude()
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(ProjectPickerEvent::Dismissed)))
            .child(
                List::new(&self.list)
                    .search_placeholder("Switch to a project…")
                    .max_h(px(360.)),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Enter to open · Esc to close")
                    .child(
                        Button::new("picker-open-folder")
                            .ghost()
                            .xsmall()
                            .label("Open Folder…")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ProjectPickerEvent::OpenFolder))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob of `gpui_kit` would replace `#[test]`.
    use super::{Project, Projects, display_path, fuzzy_score, ranked, same_folder, MAX_PROJECTS};
    use std::path::{Path, PathBuf};

    fn project(path: &str) -> Project {
        Project {
            path: PathBuf::from(path),
            opened: 0,
            files: Vec::new(),
            active: None,
        }
    }

    #[test]
    fn opening_a_project_puts_it_first_once() {
        let mut projects = Projects::default();
        projects.opened("/code/a".into(), 1);
        projects.opened("/code/b".into(), 2);
        projects.opened("/code/a/".into(), 3);
        let paths: Vec<&Path> = projects.list().iter().map(|project| project.path.as_path()).collect();
        assert_eq!(paths, [Path::new("/code/a"), Path::new("/code/b")]);
        assert_eq!(projects.list()[0].opened, 3);
    }

    #[test]
    fn the_oldest_projects_are_forgotten_past_the_limit() {
        let mut projects = Projects::default();
        for n in 0..MAX_PROJECTS + 5 {
            projects.opened(format!("/code/{n}").into(), n as u64);
        }
        assert_eq!(projects.list().len(), MAX_PROJECTS);
        assert_eq!(projects.list()[0].path, Path::new(&format!("/code/{}", MAX_PROJECTS + 4)));
    }

    #[test]
    fn a_project_remembers_its_open_files() {
        let mut projects = Projects::default();
        projects.opened("/code/a".into(), 1);
        let files = [PathBuf::from("/code/a/x.rs"), PathBuf::from("/code/a/y.rs")];
        assert!(projects.set_session(Path::new("/code/a"), &files, Some(Path::new("/code/a/y.rs"))));
        // The same again changes nothing, so there's nothing to save.
        assert!(!projects.set_session(Path::new("/code/a"), &files, Some(Path::new("/code/a/y.rs"))));
        // A folder that isn't a project isn't added.
        assert!(!projects.set_session(Path::new("/elsewhere"), &files, None));
        assert_eq!(projects.list().len(), 1);

        // Opening it again keeps its files.
        projects.opened("/code/b".into(), 2);
        projects.opened("/code/a".into(), 3);
        let a = projects.get(Path::new("/code/a")).unwrap();
        assert_eq!(a.files, files);
        assert_eq!(a.active.as_deref(), Some(Path::new("/code/a/y.rs")));

        projects.remove(Path::new("/code/a"));
        assert!(projects.get(Path::new("/code/a")).is_none());
    }

    #[test]
    fn the_list_survives_a_save_and_a_bad_file_is_ignored() {
        let mut projects = Projects::default();
        projects.opened("/code/a".into(), 1);
        projects.version = super::FILE_VERSION;
        let json = serde_json::to_vec(&projects).unwrap();
        assert_eq!(Projects::from_json(&json).list(), projects.list());
        assert!(Projects::from_json(b"not json").list().is_empty());
        assert!(Projects::from_json(br#"{"version": 99, "projects": []}"#).list().is_empty());
    }

    #[test]
    fn folders_match_with_or_without_a_trailing_separator() {
        assert!(same_folder(Path::new("/code/a"), Path::new("/code/a/")));
        assert!(!same_folder(Path::new("/code/a"), Path::new("/code/ab")));
    }

    #[test]
    fn the_home_folder_shows_as_a_tilde() {
        let home = Path::new("/home/me");
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(display_path(Path::new("/home/me/code/a"), Some(home)), format!("~{sep}code/a"));
        assert_eq!(display_path(home, Some(home)), "~");
        assert_eq!(display_path(Path::new("/srv/a"), Some(home)), "/srv/a");
    }

    #[test]
    fn typing_finds_letters_in_order() {
        assert!(fuzzy_score("gsw", "gitstepwise").is_some());
        assert!(fuzzy_score("GIT", "gitstepwise").is_some());
        assert!(fuzzy_score("wsg", "gitstepwise").is_none());
        // Letters next to each other beat scattered ones.
        assert!(fuzzy_score("step", "gitstepwise") > fuzzy_score("step", "s-t-e-p"));
    }

    #[test]
    fn a_match_in_the_name_ranks_first() {
        let projects = [project("/web/api"), project("/api-tools/web"), project("/other/thing")];
        // "web" is the name of the second, but only in the path of the first.
        assert_eq!(ranked(&projects, "web"), [1, 0]);
        assert_eq!(ranked(&projects, ""), [0, 1, 2]);
        assert!(ranked(&projects, "zzz").is_empty());
    }
}
