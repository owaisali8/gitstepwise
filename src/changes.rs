//! The Changes view, three ways:
//!
//! - **Timeline**: every edit in the order it happened, one at a time, so
//!   editing a file, then another, then the first again is three steps.
//! - **By file**: each changed file against the last commit, one at a time.
//! - **All files**: every changed file in one list.
//!
//! Moving on to the next edit or file marks the current one reviewed, so a
//! long run of changes (say, from a coding agent) can be read through before
//! it's committed.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::PathBuf;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::*;
use gpui_kit::component::diff::{Diff, DiffFile, DiffFileStatus, DiffMode, DiffSide, DiffState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::timeline::{self, Source, Step, Timeline};

const KEY_CONTEXT: &str = "Changes";
const LIST_WIDTH: Pixels = px(240.);
const ROW_HEIGHT: Pixels = px(26.);
const EDIT_ROW_HEIGHT: Pixels = px(40.);

gpui_kit::actions!(
    changes,
    [NextFile, PreviousFile, NextChange, PreviousChange, ToggleReviewed]
);

/// Single keys, active while the Changes view has focus.
pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("n", NextFile, context),
        KeyBinding::new("p", PreviousFile, context),
        KeyBinding::new("]", NextChange, context),
        KeyBinding::new("[", PreviousChange, context),
        KeyBinding::new("r", ToggleReviewed, context),
    ]);
}

pub enum ChangesEvent {
    /// Edit a file, at a 1-based line if given.
    OpenFile { path: PathBuf, line: Option<usize> },
    Refresh,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Timeline,
    ByFile,
    AllFiles,
}

/// One changed file (or one edit's change to a file), parsed for the diff.
#[derive(Clone)]
pub struct ChangedFile {
    diff: DiffFile,
    /// Identifies this version of the change. A reviewed file whose changes
    /// change again needs reviewing again.
    fingerprint: u64,
}

impl ChangedFile {
    /// Parse a one-file patch.
    pub fn parse(patch: &str) -> Result<Self, String> {
        let diff = DiffFile::parse(patch)
            .map_err(|err| err.message().to_string())?
            .into_iter()
            .next()
            .ok_or("the patch has no files")?;
        let mut hasher = DefaultHasher::new();
        patch.hash(&mut hasher);
        Ok(Self {
            diff,
            fingerprint: hasher.finish(),
        })
    }

    fn path(&self) -> &SharedString {
        self.diff.path()
    }
}

/// One step of the timeline, with its diff.
struct Edit {
    step: Step,
    file: ChangedFile,
}

/// Which file to show once the next set of changes arrives.
pub enum Select {
    File(String),
    FirstUnreviewed,
}

/// Where the review is in one list.
#[derive(Clone, Copy, Default)]
struct Cursor {
    current: usize,
    /// Stepped past the last item: show the summary.
    finished: bool,
}

/// One row of the list in the current mode.
struct Item<'a> {
    file: &'a ChangedFile,
    reviewed: bool,
    step: Option<&'a Step>,
}

pub struct ChangesView {
    diff: Entity<DiffState>,
    mode: Mode,
    /// The repository's root; `None` when the folder isn't in a repository.
    root: Option<PathBuf>,
    branch: Option<SharedString>,
    loaded: bool,
    files: Vec<ChangedFile>,
    file_cursor: Cursor,
    /// Fingerprints of the files marked reviewed, by path.
    reviewed: HashMap<SharedString, u64>,
    timeline: Option<Entity<Timeline>>,
    edits: Vec<Edit>,
    edit_cursor: Cursor,
    /// Parsed diffs of steps, by step id and time (a merged step has a new time).
    edit_cache: HashMap<(u64, u64), ChangedFile>,
    /// The mode and fingerprints the diff shows, so a refresh that changes
    /// nothing doesn't reset its scroll position.
    shown: Option<(Mode, Vec<u64>)>,
    /// Files for the diff, and a file to scroll it to, applied when the
    /// view next renders. Giving the diff new files more than once before it
    /// is first drawn leaves it blank, so updates made while hidden wait.
    pending_files: Option<Vec<DiffFile>>,
    pending_scroll: Option<String>,
    select: Option<Select>,
    soft_wrap: bool,
    list_scroll: ScrollHandle,
    _timeline_observer: Option<Subscription>,
}

impl EventEmitter<ChangesEvent> for ChangesView {}

impl Focusable for ChangesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff.read(cx).focus_handle(cx)
    }
}

impl ChangesView {
    pub fn new(soft_wrap: bool, cx: &mut Context<Self>) -> Self {
        Self {
            diff: cx.new(|cx| DiffState::new(Vec::new(), cx)),
            mode: Mode::ByFile,
            root: None,
            branch: None,
            loaded: false,
            files: Vec::new(),
            file_cursor: Cursor::default(),
            reviewed: HashMap::new(),
            timeline: None,
            edits: Vec::new(),
            edit_cursor: Cursor::default(),
            edit_cache: HashMap::new(),
            // The diff starts out empty.
            shown: Some((Mode::ByFile, Vec::new())),
            pending_files: None,
            pending_scroll: None,
            select: None,
            soft_wrap,
            list_scroll: ScrollHandle::new(),
            _timeline_observer: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.cursor_mut().finished = false;
            self.show(true, cx);
        }
    }

    /// Show the timeline from its first edit not yet reviewed.
    pub fn review_timeline(&mut self, cx: &mut Context<Self>) {
        self.mode = Mode::Timeline;
        self.edit_cursor = Cursor {
            current: self.first_unreviewed().unwrap_or(0),
            finished: false,
        };
        self.show(true, cx);
    }

    /// Pick the file to show when the changes next arrive.
    pub fn select_on_load(&mut self, select: Select) {
        self.select = Some(select);
    }

    pub fn set_soft_wrap(&mut self, soft_wrap: bool, cx: &mut Context<Self>) {
        self.soft_wrap = soft_wrap;
        cx.notify();
    }

    /// A different repository is coming: show "Loading" until it arrives.
    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        self.loaded = false;
        cx.notify();
    }

    pub fn set_not_a_repository(&mut self, cx: &mut Context<Self>) {
        self.root = None;
        self.branch = None;
        self.loaded = true;
        self.files.clear();
        self.show(false, cx);
    }

    pub fn set_timeline(&mut self, timeline: Option<Entity<Timeline>>, cx: &mut Context<Self>) {
        self._timeline_observer = timeline
            .as_ref()
            .map(|timeline| cx.observe(timeline, |this, _, cx| this.sync_edits(cx)));
        self.timeline = timeline;
        self.edits.clear();
        self.edit_cache.clear();
        self.edit_cursor = Cursor::default();
        self.sync_edits(cx);
    }

    pub fn set_changes(
        &mut self,
        root: PathBuf,
        branch: Option<SharedString>,
        files: Vec<ChangedFile>,
        cx: &mut Context<Self>,
    ) {
        let previous = self.files.get(self.file_cursor.current).map(|file| file.path().clone());
        self.root = Some(root);
        self.branch = branch;
        self.files = files;
        self.loaded = true;
        // Reviews of files that are no longer changed don't matter any more.
        let files = &self.files;
        self.reviewed
            .retain(|path, _| files.iter().any(|file| file.path() == path));

        let position = |path: &str| self.files.iter().position(|file| file.path().as_ref() == path);
        let first_unreviewed = self.files.iter().position(|file| !self.is_file_reviewed(file));
        let (current, scroll) = match self.select.take() {
            Some(Select::File(path)) => (position(&path), true),
            Some(Select::FirstUnreviewed) => (first_unreviewed.or(Some(0)), true),
            None => (previous.as_deref().and_then(position), false),
        };
        self.file_cursor.current = current
            .unwrap_or(self.file_cursor.current)
            .min(self.files.len().saturating_sub(1));
        // New or changed files bring back the review.
        if first_unreviewed.is_some() {
            self.file_cursor.finished = false;
        }
        self.show(scroll && self.mode != Mode::Timeline, cx);
    }

    /// Bring the list of edits up to date with the timeline.
    fn sync_edits(&mut self, cx: &mut Context<Self>) {
        let previous = self.edits.get(self.edit_cursor.current).map(|edit| edit.step.id);
        let steps: Vec<Step> = self
            .timeline
            .as_ref()
            .map(|timeline| timeline.read(cx).steps().to_vec())
            .unwrap_or_default();

        let mut cache = std::mem::take(&mut self.edit_cache);
        self.edits = steps
            .into_iter()
            .filter_map(|step| {
                let key = (step.id, step.time);
                let file = match cache.remove(&key) {
                    Some(file) => file,
                    None => ChangedFile::parse(&step.patch()?).ok()?,
                };
                Some(Edit { step, file })
            })
            .collect();
        self.edit_cache = self
            .edits
            .iter()
            .map(|edit| ((edit.step.id, edit.step.time), edit.file.clone()))
            .collect();

        let current = previous
            .and_then(|id| self.edits.iter().position(|edit| edit.step.id == id))
            .unwrap_or(self.edit_cursor.current);
        self.edit_cursor.current = current.min(self.edits.len().saturating_sub(1));
        if self.edits.iter().any(|edit| !edit.step.reviewed) {
            self.edit_cursor.finished = false;
        }
        self.show(false, cx);
    }

    fn is_file_reviewed(&self, file: &ChangedFile) -> bool {
        self.reviewed.get(file.path()) == Some(&file.fingerprint)
    }

    /// The list for the current mode.
    fn items(&self) -> Vec<Item<'_>> {
        match self.mode {
            Mode::Timeline => self
                .edits
                .iter()
                .map(|edit| Item {
                    file: &edit.file,
                    reviewed: edit.step.reviewed,
                    step: Some(&edit.step),
                })
                .collect(),
            Mode::ByFile | Mode::AllFiles => self
                .files
                .iter()
                .map(|file| Item {
                    file,
                    reviewed: self.is_file_reviewed(file),
                    step: None,
                })
                .collect(),
        }
    }

    fn item_count(&self) -> usize {
        match self.mode {
            Mode::Timeline => self.edits.len(),
            Mode::ByFile | Mode::AllFiles => self.files.len(),
        }
    }

    fn cursor(&self) -> Cursor {
        match self.mode {
            Mode::Timeline => self.edit_cursor,
            Mode::ByFile | Mode::AllFiles => self.file_cursor,
        }
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        match self.mode {
            Mode::Timeline => &mut self.edit_cursor,
            Mode::ByFile | Mode::AllFiles => &mut self.file_cursor,
        }
    }

    fn first_unreviewed(&self) -> Option<usize> {
        self.items().iter().position(|item| !item.reviewed)
    }

    /// Put the right files in the diff for the mode.
    fn show(&mut self, scroll: bool, cx: &mut Context<Self>) {
        let current = self.cursor().current;
        let shown: Vec<&ChangedFile> = match self.mode {
            Mode::AllFiles => self.files.iter().collect(),
            Mode::ByFile => self.files.get(current).into_iter().collect(),
            Mode::Timeline => self.edits.get(current).map(|edit| &edit.file).into_iter().collect(),
        };
        let key = (
            self.mode,
            shown.iter().map(|file| file.fingerprint).collect::<Vec<_>>(),
        );
        let path = match self.mode {
            Mode::Timeline => self.edits.get(current).map(|edit| edit.file.path().to_string()),
            Mode::ByFile | Mode::AllFiles => self.files.get(current).map(|file| file.path().to_string()),
        };
        let replaced = self.shown.as_ref() != Some(&key);
        if replaced {
            self.pending_files = Some(shown.iter().map(|file| file.diff.clone()).collect());
            self.shown = Some(key);
        }
        if scroll || (replaced && self.mode != Mode::AllFiles) {
            self.pending_scroll = path;
            self.list_scroll.scroll_to_item(current);
        }
        cx.notify();
    }

    /// Give the diff the files `show` chose, now that it's about to be drawn.
    fn apply_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(files) = self.pending_files.take() {
            self.diff.update(cx, |state, cx| state.set_files(files, cx));
        }
        if let Some(path) = self.pending_scroll.take() {
            self.diff.update(cx, |state, cx| state.scroll_to_file(&path, cx));
        }
    }

    fn go_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.item_count() {
            *self.cursor_mut() = Cursor {
                current: ix,
                finished: false,
            };
            self.show(true, cx);
        }
    }

    fn set_reviewed(&mut self, ix: usize, reviewed: bool, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Timeline => {
                let Some(edit) = self.edits.get_mut(ix) else {
                    return;
                };
                // Updated here at once; the timeline's copy follows.
                edit.step.reviewed = reviewed;
                let id = edit.step.id;
                if let Some(timeline) = &self.timeline {
                    timeline.update(cx, |timeline, cx| timeline.set_reviewed(id, reviewed, cx));
                }
            }
            Mode::ByFile | Mode::AllFiles => {
                let Some(file) = self.files.get(ix) else {
                    return;
                };
                if reviewed {
                    self.reviewed.insert(file.path().clone(), file.fingerprint);
                } else {
                    self.reviewed.remove(file.path());
                }
            }
        }
        cx.notify();
    }

    /// Mark the current item reviewed and go on to the next; past the last
    /// one, show the summary.
    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let count = self.item_count();
        if cursor.finished || count == 0 {
            return;
        }
        self.set_reviewed(cursor.current, true, cx);
        if cursor.current + 1 < count {
            self.go_to(cursor.current + 1, cx);
        } else if self.mode != Mode::AllFiles {
            self.cursor_mut().finished = true;
            cx.notify();
        }
    }

    fn previous_file(&mut self, _: &PreviousFile, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        if cursor.finished {
            self.cursor_mut().finished = false;
            cx.notify();
        } else if cursor.current > 0 {
            self.go_to(cursor.current - 1, cx);
        }
    }

    fn next_change(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.diff.update(cx, |state, cx| state.next_change(cx));
    }

    fn previous_change(&mut self, _: &PreviousChange, _: &mut Window, cx: &mut Context<Self>) {
        self.diff.update(cx, |state, cx| state.previous_change(cx));
    }

    fn toggle_reviewed(&mut self, _: &ToggleReviewed, _: &mut Window, cx: &mut Context<Self>) {
        let current = self.cursor().current;
        if let Some(reviewed) = self.items().get(current).map(|item| item.reviewed) {
            self.set_reviewed(current, !reviewed, cx);
        }
    }

    fn start_over(&mut self, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Timeline => {
                for edit in &mut self.edits {
                    edit.step.reviewed = false;
                }
                if let Some(timeline) = &self.timeline {
                    timeline.update(cx, |timeline, cx| timeline.clear_reviewed(cx));
                }
            }
            Mode::ByFile | Mode::AllFiles => self.reviewed.clear(),
        }
        self.go_to(0, cx);
    }

    fn clear_timeline(&mut self, cx: &mut Context<Self>) {
        if let Some(timeline) = &self.timeline {
            timeline.update(cx, |timeline, cx| timeline.clear(cx));
        }
    }

    fn set_diff_mode(&mut self, mode: DiffMode, cx: &mut Context<Self>) {
        self.diff.update(cx, |state, cx| state.set_mode(mode, cx));
    }

    /// The repository root, for opening files.
    fn root(&self, cx: &App) -> Option<PathBuf> {
        self.root.clone().or_else(|| {
            self.timeline
                .as_ref()
                .map(|timeline| timeline.read(cx).root().to_path_buf())
        })
    }

    // ---- Rendering ---------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, success, danger) = (
            theme.muted_foreground,
            theme.border,
            theme.success,
            theme.danger,
        );
        let diff_mode = self.diff.read(cx).mode();
        let has_items = self.item_count() > 0;
        let (summary, totals) = match self.mode {
            Mode::Timeline => (
                match self.edits.len() {
                    0 => String::new(),
                    1 => "1 edit".into(),
                    n => format!("{n} edits"),
                },
                None,
            ),
            Mode::ByFile | Mode::AllFiles => (
                match self.files.len() {
                    0 => String::new(),
                    1 => "1 changed file".into(),
                    n => format!("{n} changed files"),
                },
                Some(self.files.iter().fold((0, 0), |(a, d), file| {
                    (a + file.diff.additions(), d + file.diff.deletions())
                })),
            ),
        };
        let toggle = |id: &'static str, label: &'static str, selected: bool| {
            Button::new(id).ghost().xsmall().label(label).selected(selected)
        };
        let divider = || div().w(px(1.)).h(px(14.)).bg(border);

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
            .when_some(totals.filter(|_| has_items), |row, (additions, deletions)| {
                row.child(div().text_color(success).child(format!("+{additions}")))
                    .child(div().text_color(danger).child(format!("−{deletions}")))
            })
            .child(div().flex_1())
            .child(
                toggle("changes-timeline", "Timeline", self.mode == Mode::Timeline)
                    .tooltip("Every edit, in the order it happened")
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::Timeline, cx))),
            )
            .child(
                toggle("changes-by-file", "By file", self.mode == Mode::ByFile)
                    .tooltip("One changed file at a time")
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::ByFile, cx))),
            )
            .child(
                toggle("changes-all", "All files", self.mode == Mode::AllFiles)
                    .tooltip("Every changed file in one list")
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::AllFiles, cx))),
            )
            .child(divider())
            .child(
                toggle("diff-unified", "Unified", diff_mode == DiffMode::Unified)
                    .on_click(cx.listener(|this, _, _, cx| this.set_diff_mode(DiffMode::Unified, cx))),
            )
            .child(
                toggle("diff-split", "Split", diff_mode == DiffMode::Split)
                    .on_click(cx.listener(|this, _, _, cx| this.set_diff_mode(DiffMode::Split, cx))),
            )
            .child(divider())
            .child(
                Button::new("previous-change")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .tooltip("Previous change ([)")
                    .disabled(!has_items)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.previous_change(&PreviousChange, window, cx)
                    })),
            )
            .child(
                Button::new("next-change")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowDown)
                    .tooltip("Next change (])")
                    .disabled(!has_items)
                    .on_click(cx.listener(|this, _, window, cx| this.next_change(&NextChange, window, cx))),
            )
            .when(self.mode == Mode::Timeline, |row| {
                row.child(
                    Button::new("clear-timeline")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Delete)
                        .tooltip("Clear the timeline and start again from the files as they are now")
                        .disabled(self.timeline.is_none())
                        .on_click(cx.listener(|this, _, _, cx| this.clear_timeline(cx))),
                )
            })
            .when(self.mode != Mode::Timeline, |row| {
                row.child(
                    Button::new("refresh-changes")
                        .ghost()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .tooltip("Refresh")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ChangesEvent::Refresh))),
                )
            })
    }

    /// The files or edits, with their reviewed marks and the progress so far.
    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, success, danger, info, hover, active) = (
            theme.muted_foreground,
            theme.border,
            theme.success,
            theme.danger,
            theme.info,
            theme.list_hover,
            theme.list_active,
        );
        let items = self.items();
        let cursor = self.cursor();
        let total = items.len();
        let reviewed = items.iter().filter(|item| item.reviewed).count();
        let progress = if total == 0 { 0. } else { reviewed as f32 / total as f32 };
        let title = if self.mode == Mode::Timeline { "EDITS" } else { "FILES" };

        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                let is_reviewed = item.reviewed;
                let path = item.file.path();
                let (folder, name) = match path.rsplit_once('/') {
                    Some((folder, name)) => (folder.to_string(), name.to_string()),
                    None => (String::new(), path.to_string()),
                };
                let (letter, color) = match item.file.diff.status() {
                    DiffFileStatus::Added => ("A", success),
                    DiffFileStatus::Deleted => ("D", danger),
                    DiffFileStatus::Renamed => ("R", info),
                    _ => ("M", info),
                };
                let mark = div()
                    .id(("reviewed-mark", ix))
                    .flex_none()
                    .text_color(if is_reviewed { success } else { muted })
                    .child(
                        Icon::new(if is_reviewed {
                            LucideIcon::CircleCheck
                        } else {
                            LucideIcon::Circle
                        })
                        .small(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.set_reviewed(ix, !is_reviewed, cx);
                    }));
                let name_line = div()
                    .h_flex()
                    .min_w_0()
                    .items_baseline()
                    .gap_1()
                    .overflow_hidden()
                    .when(item.step.is_some(), |line| {
                        line.child(div().flex_none().text_xs().text_color(muted).child(format!("{}", ix + 1)))
                    })
                    .child(div().flex_shrink_0().max_w(px(140.)).truncate().child(name))
                    .child(div().min_w_0().truncate().text_xs().text_color(muted).child(folder));
                let counts = div()
                    .flex_none()
                    .h_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().text_color(success).child(format!("+{}", item.file.diff.additions())))
                    .child(div().text_color(danger).child(format!("−{}", item.file.diff.deletions())))
                    .child(div().w(px(10.)).text_color(color).child(letter));

                let row = div()
                    .id(("change-row", ix))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .text_sm()
                    .hover(|style| style.bg(hover))
                    .when(ix == cursor.current && !cursor.finished, |row| row.bg(active))
                    .child(mark)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.go_to(ix, cx);
                        this.focus_handle(cx).focus(window, cx);
                    }));
                match item.step {
                    // Edits get a second line: when, and who made the change.
                    Some(step) => row
                        .h(EDIT_ROW_HEIGHT)
                        .child(
                            div()
                                .v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(name_line)
                                .child(
                                    div()
                                        .h_flex()
                                        .gap_2()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(timeline::ago(step.time))
                                        .when_some(source_label(step.source), |line, label| line.child(label)),
                                ),
                        )
                        .child(counts)
                        .into_any_element(),
                    None => row
                        .h(ROW_HEIGHT)
                        .child(div().flex_1().min_w_0().child(name_line))
                        .child(counts)
                        .into_any_element(),
                }
            })
            .collect();

        div()
            .v_flex()
            .w(LIST_WIDTH)
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(border)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(format!("{reviewed} of {total} reviewed")),
            )
            .child(
                // Progress through the review.
                div()
                    .h(px(2.))
                    .mx_3()
                    .mb_1()
                    .bg(border)
                    .child(div().h_full().w(relative(progress)).bg(success)),
            )
            .child(
                div()
                    .id("change-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .children(rows),
            )
    }

    /// "Edit 3 of 7" or "File 3 of 7", and the buttons to move through the
    /// review. The file's name, status and size are in the diff's own header.
    fn render_step_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let items = self.items();
        let cursor = self.cursor();
        let item = &items[cursor.current];
        let is_reviewed = item.reviewed;
        let is_last = cursor.current + 1 == items.len();
        let (noun, next_label) = match self.mode {
            Mode::Timeline => ("Edit", "Next edit"),
            Mode::ByFile | Mode::AllFiles => ("File", "Next file"),
        };
        let mut position = format!("{noun} {} of {}", cursor.current + 1, items.len());
        if let Some(step) = item.step {
            position.push_str(&format!(" · {}", timeline::ago(step.time)));
            if let Some(label) = source_label(step.source) {
                position.push_str(&format!(" · {label}"));
            }
        }

        div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(border)
            .child(div().min_w_0().truncate().text_sm().text_color(muted).child(position))
            .child(div().flex_1())
            .child(
                Button::new("toggle-reviewed")
                    .ghost()
                    .small()
                    .icon(Icon::new(if is_reviewed {
                        LucideIcon::CircleCheck
                    } else {
                        LucideIcon::Circle
                    }))
                    .label(if is_reviewed { "Reviewed" } else { "Mark reviewed" })
                    .selected(is_reviewed)
                    .tooltip("Mark this reviewed, or not (R)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_reviewed(&ToggleReviewed, window, cx)
                    })),
            )
            .child(
                Button::new("previous-file")
                    .small()
                    .icon(IconName::ChevronLeft)
                    .label("Previous")
                    .tooltip("Previous (P)")
                    .disabled(cursor.current == 0)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.previous_file(&PreviousFile, window, cx)
                    })),
            )
            .child(
                Button::new("next-file")
                    .primary()
                    .small()
                    .icon(if is_last { IconName::Check } else { IconName::ChevronRight })
                    .label(if is_last { "Finish" } else { next_label })
                    .tooltip("Mark reviewed and go on (N)")
                    .on_click(cx.listener(|this, _, window, cx| this.next_file(&NextFile, window, cx))),
            )
    }

    /// Shown after stepping past the last item.
    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, success) = (theme.muted_foreground, theme.success);
        let items = self.items();
        let total = items.len();
        let reviewed = items.iter().filter(|item| item.reviewed).count();
        let all_reviewed = reviewed == total;
        let noun = if self.mode == Mode::Timeline { "edits" } else { "files" };
        let (additions, deletions) = items.iter().fold((0, 0), |(a, d), item| {
            (a + item.file.diff.additions(), d + item.file.diff.deletions())
        });
        let first_unreviewed = items.iter().position(|item| !item.reviewed);

        div()
            .flex_1()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .text_color(if all_reviewed { success } else { muted })
                    .child(Icon::new(LucideIcon::CircleCheck).large()),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(match (all_reviewed, self.mode) {
                        (true, Mode::Timeline) => "All edits reviewed",
                        (true, _) => "All changes reviewed",
                        (false, Mode::Timeline) => "That was the last edit",
                        (false, _) => "That was the last file",
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("{reviewed} of {total} {noun} reviewed · +{additions} −{deletions}")),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .when_some(first_unreviewed, |row, ix| {
                        row.child(
                            Button::new("next-unreviewed")
                                .primary()
                                .small()
                                .label("Next unreviewed")
                                .on_click(cx.listener(move |this, _, _, cx| this.go_to(ix, cx))),
                        )
                    })
                    .child(
                        Button::new("back-to-last")
                            .small()
                            .icon(IconName::ChevronLeft)
                            .label("Back")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.previous_file(&PreviousFile, window, cx)
                            })),
                    )
                    .child(
                        Button::new("start-over")
                            .small()
                            .label("Start over")
                            .tooltip("Clear the reviewed marks and go back to the first")
                            .on_click(cx.listener(|this, _, _, cx| this.start_over(cx))),
                    )
                    .child(
                        Button::new("close-review")
                            .small()
                            .label("Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ChangesEvent::Close))),
                    ),
            )
    }

    fn render_diff(&self, root: PathBuf, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let open_button = {
            let (view, root) = (view.clone(), root.clone());
            move |file: &DiffFile, _: &mut Window, _: &mut App| {
                let path = root.join(file.path().to_string());
                let view = view.clone();
                div().when(file.status() != DiffFileStatus::Deleted, |this| {
                    this.child(
                        Button::new(SharedString::from(format!("open:{}", file.path())))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .label("Open")
                            .on_click(move |_, _, cx| {
                                let path = path.clone();
                                view.update(cx, |_, cx| cx.emit(ChangesEvent::OpenFile { path, line: None }))
                                    .ok();
                            }),
                    )
                })
            }
        };

        div().flex_1().min_h_0().child(
            Diff::new(&self.diff)
                .size_full()
                .soft_wrap(self.soft_wrap)
                .header_suffix(open_button)
                // Double-click a line to edit the file there.
                .on_line_click(move |position, event, _, cx| {
                    if event.click_count() < 2 {
                        return;
                    }
                    let path = root.join(position.path().to_string());
                    let line = (position.side() == DiffSide::Modified).then(|| position.line());
                    view.update(cx, |_, cx| cx.emit(ChangesEvent::OpenFile { path, line }))
                        .ok();
                }),
        )
    }

    /// The message shown in place of the list and diff, if any.
    fn empty_message(&self, cx: &App) -> Option<String> {
        match self.mode {
            Mode::Timeline => {
                let Some(timeline) = &self.timeline else {
                    return Some(if self.loaded {
                        "The folder in the explorer isn't in a Git repository.".into()
                    } else {
                        "Loading the timeline…".into()
                    });
                };
                let timeline = timeline.read(cx);
                if let Some(err) = timeline.watch_error() {
                    Some(format!("Couldn't watch the files for edits: {err}"))
                } else if !timeline.is_loaded() {
                    Some("Loading the timeline…".into())
                } else if self.edits.is_empty() {
                    Some(
                        "No edits yet. While the editor is open, each change to a file in this \
                         repository shows up here as a step, in order, until the next commit."
                            .into(),
                    )
                } else {
                    None
                }
            }
            Mode::ByFile | Mode::AllFiles => match &self.root {
                None if self.loaded => Some("The folder in the explorer isn't in a Git repository.".into()),
                _ if !self.loaded => Some("Loading changes…".into()),
                _ if self.files.is_empty() => Some("No changes since the last commit.".into()),
                _ => None,
            },
        }
    }
}

/// How a step's source is described, if it isn't the usual "written by
/// another program".
fn source_label(source: Source) -> Option<&'static str> {
    match source {
        Source::Disk => None,
        Source::Editor => Some("saved in this editor"),
        Source::WhileClosed => Some("while the editor was closed"),
    }
}

impl Render for ChangesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_pending(cx);
        let muted = cx.theme().muted_foreground;
        let message = self.empty_message(cx);
        let has_items = message.is_none() && self.item_count() > 0;
        let finished = self.cursor().finished && self.mode != Mode::AllFiles;

        let body = match (message, self.root(cx)) {
            (Some(text), _) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_8()
                .text_sm()
                .text_color(muted)
                .child(div().max_w(px(460.)).text_center().child(text))
                .into_any_element(),
            (None, _) if finished => self.render_summary(cx).into_any_element(),
            (None, Some(root)) => div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .when(self.mode != Mode::AllFiles, |column| {
                    column.child(self.render_step_bar(cx))
                })
                .child(self.render_diff(root, cx))
                .into_any_element(),
            (None, None) => div().flex_1().into_any_element(),
        };

        div()
            .v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::previous_file))
            .on_action(cx.listener(Self::next_change))
            .on_action(cx.listener(Self::previous_change))
            .on_action(cx.listener(Self::toggle_reviewed))
            .child(self.render_header(cx))
            .child(
                div()
                    .h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .when(has_items, |row| row.child(self.render_list(cx)))
                    .child(body),
            )
    }
}
