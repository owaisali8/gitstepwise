//! Branches, as GitHub Desktop shows them: the dropdown under the title
//! bar's "Current branch" button, to see, filter, create and switch
//! branches; and the dialogs to create a branch and to merge one into the
//! current branch. The Git side is in `git_branches.rs`. The workspace runs
//! the operations, since they change its open files.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::git::Repo;
use crate::git_branches::{self, Branch, Branches, MergePreview};
use crate::projects::fuzzy_score;

const KEY_CONTEXT: &str = "Branches";

gpui_kit::actions!(
    branches,
    [
        /// Close the branch list, or a branch dialog.
        Close
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Close, Some(KEY_CONTEXT))]);
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// "just now", "5 minutes ago", "a day ago", as GitHub Desktop says it.
pub fn ago(seconds: i64) -> String {
    let unit = |count: i64, one: &str, many: &str| {
        if count <= 1 {
            one.to_string()
        } else {
            format!("{count} {many} ago")
        }
    };
    match seconds.max(0) {
        0..=44 => "just now".into(),
        45..=3_599 => unit(seconds / 60, "a minute ago", "minutes"),
        3_600..=86_399 => unit(seconds / 3_600, "an hour ago", "hours"),
        86_400..=2_591_999 => unit(seconds / 86_400, "a day ago", "days"),
        2_592_000..=31_535_999 => unit(seconds / 2_592_000, "a month ago", "months"),
        _ => unit(seconds / 31_536_000, "a year ago", "years"),
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Row {
    Header(&'static str),
    /// An index into `Branches::all`.
    Branch(usize),
}

/// Branches in sections, as GitHub Desktop lists them: the default branch,
/// recent branches, then the others. Typing narrows the list.
#[derive(Default)]
struct BranchList {
    branches: Branches,
    /// Leave out the current branch (merging it into itself makes no sense).
    without_current: bool,
    query: String,
    rows: Vec<Row>,
    /// The selected row, always a branch.
    selected: Option<usize>,
    scroll: ScrollHandle,
}

impl BranchList {
    fn set_branches(&mut self, branches: Branches) {
        let keep = self
            .selected_branch()
            .map(Branch::full_name)
            .or_else(|| branches.current.clone());
        self.branches = branches;
        self.rebuild(keep);
    }

    fn set_query(&mut self, query: &str) {
        self.query = query.trim().to_string();
        let keep = self.query.is_empty().then(|| self.branches.current.clone()).flatten();
        self.rebuild(keep);
    }

    fn rebuild(&mut self, keep: Option<String>) {
        let all = &self.branches.all;
        let current = self.branches.current.as_deref();
        let shown = |branch: &Branch| {
            let is_current = branch.remote.is_none() && Some(branch.name.as_str()) == current;
            !(self.without_current && is_current)
                && (self.query.is_empty() || fuzzy_score(&self.query, &branch.full_name()).is_some())
        };
        let default = self
            .branches
            .default
            .as_ref()
            .and_then(|name| all.iter().position(|branch| &branch.name == name));
        let recent: Vec<usize> = self
            .branches
            .recent
            .iter()
            .filter_map(|name| all.iter().position(|branch| &branch.name == name && branch.remote.is_none()))
            .filter(|ix| Some(*ix) != default)
            .collect();
        let other: Vec<usize> = (0..all.len())
            .filter(|ix| Some(*ix) != default && !recent.contains(ix))
            .collect();

        self.rows.clear();
        for (title, section) in [
            ("Default branch", default.into_iter().collect()),
            ("Recent branches", recent),
            ("Other branches", other),
        ] {
            let section: Vec<usize> = section.into_iter().filter(|ix| shown(&all[*ix])).collect();
            if section.is_empty() {
                continue;
            }
            self.rows.push(Row::Header(title));
            self.rows.extend(section.into_iter().map(Row::Branch));
        }
        let first = self.rows.iter().position(|row| matches!(row, Row::Branch(_)));
        self.selected = keep
            .and_then(|name| {
                self.rows
                    .iter()
                    .position(|row| matches!(row, Row::Branch(ix) if all[*ix].full_name() == name))
            })
            .or(first);
        self.scroll.scroll_to_item(self.selected.unwrap_or(0));
    }

    fn selected_branch(&self) -> Option<&Branch> {
        match self.rows.get(self.selected?)? {
            Row::Branch(ix) => self.branches.all.get(*ix),
            Row::Header(_) => None,
        }
    }

    fn is_empty(&self) -> bool {
        !self.rows.iter().any(|row| matches!(row, Row::Branch(_)))
    }

    /// Select the next branch down, or up.
    fn move_selection(&mut self, down: bool) {
        let branch_rows: Vec<usize> = (0..self.rows.len())
            .filter(|row| matches!(self.rows[*row], Row::Branch(_)))
            .collect();
        let at = self
            .selected
            .and_then(|selected| branch_rows.iter().position(|row| *row == selected));
        let next = match at {
            None => branch_rows.first(),
            Some(at) if down => branch_rows.get(at + 1).or(branch_rows.get(at)),
            Some(at) => branch_rows.get(at.saturating_sub(1)),
        };
        if let Some(&row) = next {
            self.selected = Some(row);
            // Show the section's heading above the first branch too.
            let first_of_section = row > 0 && matches!(self.rows[row - 1], Row::Header(_));
            self.scroll.scroll_to_item(if first_of_section { row - 1 } else { row });
        }
    }

    /// The rows, in a scrolling column. A click on a branch calls `on_click`
    /// with its row.
    fn render<V: 'static>(
        &self,
        id: &'static str,
        cx: &mut Context<V>,
        on_click: fn(&mut V, usize, &mut Window, &mut Context<V>),
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let (muted, border, hover, active) = (
            theme.muted_foreground,
            theme.border,
            theme.list_hover,
            theme.list_active,
        );
        let now = now_seconds();
        let current = self.branches.current.as_deref();
        let rows: Vec<AnyElement> = self
            .rows
            .iter()
            .enumerate()
            .map(|(row, item)| match *item {
                Row::Header(title) => div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title)
                    .into_any_element(),
                Row::Branch(ix) => {
                    let branch = &self.branches.all[ix];
                    let is_current = branch.remote.is_none() && current == Some(branch.name.as_str());
                    div()
                        .id((id, row))
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .h(px(30.))
                        .px_3()
                        .cursor_pointer()
                        .when(self.selected == Some(row), |item| item.bg(active))
                        .hover(|style| style.bg(hover))
                        .child(
                            div()
                                .w(px(16.))
                                .flex_none()
                                .when(is_current, |mark| mark.child(Icon::new(IconName::Check).small())),
                        )
                        .child(div().flex_1().min_w_0().truncate().text_sm().child(branch.name.clone()))
                        .when_some(branch.remote.clone(), |item, remote| {
                            item.child(
                                div()
                                    .flex_none()
                                    .px_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(border)
                                    .text_xs()
                                    .text_color(muted)
                                    .child(remote),
                            )
                        })
                        .child(div().flex_none().text_xs().text_color(muted).child(ago(now - branch.time)))
                        .on_click(cx.listener(move |this, _, window, cx| on_click(this, row, window, cx)))
                        .into_any_element()
                }
            })
            .collect();
        div()
            .id(id)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .pb_1()
            .children(rows)
    }
}

fn filter_input(placeholder: &'static str, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

fn render_filter(filter: &Entity<InputState>, muted: Hsla) -> Input {
    Input::new(filter)
        .small()
        .cleanable(true)
        .prefix(Icon::new(IconName::Search).small().text_color(muted))
}

/// A dialog's title row, with a close button.
fn dialog_header<V: EventEmitter<E> + 'static, E: Clone + 'static>(
    title: impl IntoElement,
    close: E,
    cx: &mut Context<V>,
) -> Div {
    h_flex()
        .h(px(44.))
        .flex_none()
        .items_center()
        .pl_4()
        .pr_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(div().flex_1().min_w_0().text_base().child(title))
        .child(
            Button::new("close-dialog")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip("Close (Esc)")
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(close.clone()))),
        )
}

// ---- The branch list ---------------------------------------------------------

/// What the workspace needs to hear about.
#[derive(Clone)]
pub enum BranchesEvent {
    Switch(Branch),
    /// Open the dialog to create a branch, with this name typed.
    NewBranch(String),
    /// Open the dialog to merge a branch into the current one.
    ChooseMerge,
    RestoreStash(String),
    DiscardStash(String),
    AbortMerge,
    Close,
}

#[derive(Clone, Copy, PartialEq)]
enum PanelTab {
    Branches,
    PullRequests,
}

/// The dropdown under the "Current branch" button.
pub struct BranchesPanel {
    repo: Option<Repo>,
    tab: PanelTab,
    filter: Entity<InputState>,
    list: BranchList,
    loaded: bool,
    /// Changes left on the current branch, as a stash.
    stash: Option<String>,
    /// A merge stopped on conflicts.
    merging: bool,
    /// The repository's page on GitHub or GitLab.
    web: Option<String>,
    focus_handle: FocusHandle,
    _load_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BranchesEvent> for BranchesPanel {}

impl Focusable for BranchesPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.read(cx).focus_handle(cx)
    }
}

impl BranchesPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = filter_input("Filter", window, cx);
        let subscriptions = vec![cx.subscribe_in(&filter, window, |this, filter, event: &InputEvent, _, cx| {
            match event {
                InputEvent::Change => {
                    let query = filter.read(cx).value();
                    this.list.set_query(&query);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.confirm(cx),
                _ => {}
            }
        })];
        Self {
            repo: None,
            tab: PanelTab::Branches,
            filter,
            list: BranchList::default(),
            loaded: false,
            stash: None,
            merging: false,
            web: None,
            focus_handle: cx.focus_handle(),
            _load_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_repo(&mut self, repo: Option<Repo>, cx: &mut Context<Self>) {
        if self.repo != repo {
            self.repo = repo;
            self.loaded = false;
            self.list.set_branches(Branches::default());
            cx.notify();
        }
    }

    /// The list is showing: start with every branch, the current one
    /// selected, and the cursor in the filter.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = PanelTab::Branches;
        self.filter.update(cx, |filter, cx| filter.set_value("", window, cx));
        self.list.set_query("");
        self.load(cx);
        self.filter.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// Read the branches again, and any stash or merge in progress.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let (branches, stash, merging, web) = cx
                .background_spawn(async move {
                    let branches = repo.branches();
                    let stash = branches.current.as_deref().and_then(|branch| repo.stash_for(branch));
                    (branches, stash, repo.is_merging(), repo.web_page())
                })
                .await;
            this.update(cx, |this, cx| {
                this.list.set_branches(branches);
                this.stash = stash;
                this.merging = merging;
                this.web = web;
                this.loaded = true;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Enter: switch to the selected branch, or offer to create the one
    /// typed when nothing matches.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(branch) = self.list.selected_branch() {
            cx.emit(BranchesEvent::Switch(branch.clone()));
        } else if !self.list.query.is_empty() {
            cx.emit(BranchesEvent::NewBranch(self.list.query.clone()));
        }
    }

    fn click_row(&mut self, row: usize, _: &mut Window, cx: &mut Context<Self>) {
        self.list.selected = Some(row);
        self.confirm(cx);
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, primary, muted) = (theme.border, theme.primary, theme.muted_foreground);
        let tab = |id: &'static str, label: &'static str, which: PanelTab, cx: &mut Context<Self>| {
            let active = self.tab == which;
            div()
                .id(id)
                .flex_1()
                .h_flex()
                .justify_center()
                .h(px(36.))
                .text_sm()
                .cursor_pointer()
                .border_b_2()
                .border_color(if active { primary } else { gpui_kit::transparent_black() })
                .when(!active, |tab| tab.text_color(muted))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.tab = which;
                    if which == PanelTab::Branches {
                        this.filter.read(cx).focus_handle(cx).focus(window, cx);
                    }
                    cx.notify();
                }))
        };
        h_flex()
            .flex_none()
            .border_b_1()
            .border_color(border)
            .child(tab("tab-branches", "Branches", PanelTab::Branches, cx))
            .child(tab("tab-pull-requests", "Pull requests", PanelTab::PullRequests, cx))
    }

    /// A strip above the list: a merge waiting on conflicts, or changes
    /// left on this branch.
    fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (border, muted, warning) = (theme.border, theme.muted_foreground, theme.warning);
        let strip = |icon: IconName, color: Hsla, text: &'static str| {
            h_flex()
                .flex_none()
                .items_center()
                .gap_2()
                .mx_2()
                .mb_2()
                .px_2()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(border)
                .text_sm()
                .child(Icon::new(icon).small().text_color(color))
                .child(div().flex_1().min_w_0().child(text))
        };
        if self.merging {
            return Some(
                strip(
                    IconName::TriangleAlert,
                    warning,
                    "A merge is waiting on conflicts. Resolve them, then commit.",
                )
                .child(
                    Button::new("abort-merge")
                        .outline()
                        .xsmall()
                        .label("Abort Merge")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(BranchesEvent::AbortMerge))),
                )
                .into_any_element(),
            );
        }
        let stash = self.stash.clone()?;
        let discard = stash.clone();
        Some(
            strip(IconName::Info, muted, "You left changes on this branch.")
                .child(
                    Button::new("restore-stash")
                        .outline()
                        .xsmall()
                        .label("Restore")
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(BranchesEvent::RestoreStash(stash.clone())))),
                )
                .child(
                    Button::new("discard-stash")
                        .ghost()
                        .xsmall()
                        .label("Discard")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(BranchesEvent::DiscardStash(discard.clone()))
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_branches(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, hover) = (theme.muted_foreground, theme.border, theme.secondary_hover);
        let query = self.list.query.clone();
        let body = if !self.loaded {
            div().p_4().text_sm().text_color(muted).child("Reading the branches…").into_any_element()
        } else if self.list.is_empty() && !query.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .px_6()
                .text_center()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Sorry, I can't find that branch"))
                .child(div().text_sm().text_color(muted).child("Do you want to create a new branch instead?"))
                .child(
                    Button::new("create-instead")
                        .primary()
                        .small()
                        .mt_1()
                        .label("Create new branch")
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(BranchesEvent::NewBranch(query.clone())))),
                )
                .into_any_element()
        } else if self.list.is_empty() {
            div()
                .p_4()
                .text_sm()
                .text_color(muted)
                .child("There are no branches until the first commit.")
                .into_any_element()
        } else {
            self.list
                .render("branch-row", cx, Self::click_row)
                .into_any_element()
        };

        let current = self.list.branches.current.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .p_2()
                    .child(div().flex_1().min_w_0().child(render_filter(&self.filter, muted)))
                    .child(
                        Button::new("new-branch")
                            .outline()
                            .small()
                            .label("New branch")
                            .tooltip(format!("Create a branch ({})", crate::shortcut("Shift+N")))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.emit(BranchesEvent::NewBranch(this.list.query.clone()))
                            })),
                    ),
            )
            .children(self.render_banner(cx))
            .child(body)
            .when_some(current, |column, current| {
                column.child(
                    div().flex_none().p_2().border_t_1().border_color(border).child(
                        div()
                            .id("choose-merge")
                            .h_flex()
                            .justify_center()
                            .items_center()
                            .gap_2()
                            .py_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .text_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(hover))
                            .child(Icon::new(LucideIcon::GitMerge).small())
                            .child(
                                h_flex()
                                    .child("Choose a branch to merge into\u{a0}")
                                    .child(div().font_weight(FontWeight::BOLD).child(current)),
                            )
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(BranchesEvent::ChooseMerge))),
                    ),
                )
            })
    }

    fn render_pull_requests(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let column = v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .px_6()
            .text_center()
            .child(Icon::new(LucideIcon::GitPullRequest).large().text_color(muted));
        let Some(web) = self.web.clone() else {
            return column
                .child(div().font_weight(FontWeight::SEMIBOLD).child("No pull requests here"))
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("Pull requests show for repositories on GitHub or GitLab."),
                );
        };
        let gitlab = web.starts_with("https://gitlab.com/");
        let (site, list_url) = if gitlab {
            ("GitLab", format!("{web}/-/merge_requests"))
        } else {
            ("GitHub", format!("{web}/pulls"))
        };
        let branch = self
            .list
            .branches
            .current
            .clone()
            .filter(|current| Some(current) != self.list.branches.default.as_ref());
        column
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("Pull requests are on {site}")))
            .child(div().text_sm().text_color(muted).child("They open in your browser."))
            .child(
                Button::new("view-pull-requests")
                    .outline()
                    .small()
                    .mt_2()
                    .icon(IconName::ExternalLink)
                    .label(format!("View Pull Requests on {site}"))
                    .on_click(move |_, _, cx| cx.open_url(&list_url)),
            )
            .when_some(branch, |column, branch| {
                let url = if gitlab {
                    format!("{web}/-/merge_requests/new?merge_request%5Bsource_branch%5D={branch}")
                } else {
                    format!("{web}/compare/{branch}?expand=1")
                };
                column.child(
                    Button::new("create-pull-request")
                        .primary()
                        .small()
                        .icon(IconName::ExternalLink)
                        .label(format!("Create Pull Request from {branch}"))
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
    }
}

impl Render for BranchesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            PanelTab::Branches => self.render_branches(cx).into_any_element(),
            PanelTab::PullRequests => self.render_pull_requests(cx).into_any_element(),
        };
        v_flex()
            .id("branches-panel")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(BranchesEvent::Close)))
            // The arrow keys move through the list while the cursor is in the
            // filter.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.list.move_selection(false);
                cx.stop_propagation();
                cx.notify();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.list.move_selection(true);
                cx.stop_propagation();
                cx.notify();
            }))
            .size_full()
            .popover_style(cx)
            .occlude()
            .overflow_hidden()
            .child(self.render_tabs(cx))
            .child(body)
    }
}

// ---- Merging ---------------------------------------------------------------------

#[derive(Clone)]
pub enum MergeEvent {
    /// Merge this branch (as Git names it) into the current one.
    Merge(String),
    Close,
}

/// "Merge into main": choose a branch, see what merging it would do, merge.
pub struct MergeDialog {
    repo: Repo,
    into: String,
    filter: Entity<InputState>,
    list: BranchList,
    /// The branch looked at, and what merging it would do once that's known.
    preview: Option<(String, Option<MergePreview>)>,
    focus_handle: FocusHandle,
    _load_task: Task<()>,
    _preview_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MergeEvent> for MergeDialog {}

impl Focusable for MergeDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.read(cx).focus_handle(cx)
    }
}

impl MergeDialog {
    pub fn new(repo: Repo, into: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = filter_input("Filter", window, cx);
        let subscriptions = vec![cx.subscribe_in(&filter, window, |this, filter, event: &InputEvent, _, cx| {
            match event {
                InputEvent::Change => {
                    let query = filter.read(cx).value();
                    this.list.set_query(&query);
                    this.preview_selected(cx);
                }
                InputEvent::PressEnter { .. } => this.merge(cx),
                _ => {}
            }
        })];
        let load = repo.clone();
        let load_task = cx.spawn(async move |this, cx| {
            let branches = cx.background_spawn(async move { load.branches() }).await;
            this.update(cx, |this, cx| {
                this.list.set_branches(branches);
                // Nothing chosen yet: GitHub Desktop starts with none.
                this.list.selected = None;
                cx.notify();
            })
            .ok();
        });
        Self {
            repo,
            into,
            filter,
            list: BranchList {
                without_current: true,
                ..BranchList::default()
            },
            preview: None,
            focus_handle: cx.focus_handle(),
            _load_task: load_task,
            _preview_task: None,
            _subscriptions: subscriptions,
        }
    }

    /// Work out, in the background, what merging the selected branch would do.
    fn preview_selected(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.list.selected_branch().map(Branch::full_name) else {
            self.preview = None;
            self._preview_task = None;
            cx.notify();
            return;
        };
        if self.preview.as_ref().is_some_and(|(previewed, _)| *previewed == from) {
            return;
        }
        self.preview = Some((from.clone(), None));
        let repo = self.repo.clone();
        self._preview_task = Some(cx.spawn(async move |this, cx| {
            let branch = from.clone();
            let preview = cx.background_spawn(async move { repo.merge_preview(&branch) }).await;
            this.update(cx, |this, cx| {
                if this.preview.as_ref().is_some_and(|(previewed, _)| *previewed == from) {
                    this.preview = Some((from, Some(preview)));
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn can_merge(&self) -> bool {
        matches!(
            self.preview,
            Some((
                _,
                Some(MergePreview::Clean { .. } | MergePreview::Conflicts { .. } | MergePreview::Unknown { .. })
            ))
        )
    }

    fn merge(&mut self, cx: &mut Context<Self>) {
        if !self.can_merge() {
            return;
        }
        if let Some((from, _)) = &self.preview {
            cx.emit(MergeEvent::Merge(from.clone()));
        }
    }

    fn click_row(&mut self, row: usize, _: &mut Window, cx: &mut Context<Self>) {
        self.list.selected = Some(row);
        self.preview_selected(cx);
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, success, warning) = (theme.muted_foreground, theme.success, theme.warning);
        let into = self.into.clone();
        let line = |icon: Option<(IconName, Hsla)>, text: String| {
            h_flex()
                .items_start()
                .gap_2()
                .text_sm()
                .children(icon.map(|(icon, color)| div().pt_0p5().child(Icon::new(icon).small().text_color(color))))
                .child(div().flex_1().min_w_0().child(text))
        };
        match &self.preview {
            None => line(None, format!("Choose a branch to merge into {into}.")).text_color(muted),
            Some((_, None)) => line(None, "Checking for conflicts…".into()).text_color(muted),
            Some((from, Some(preview))) => match *preview {
                MergePreview::UpToDate => line(
                    Some((IconName::CircleCheck, muted)),
                    format!("This branch is up to date with {from}."),
                ),
                MergePreview::Clean { commits } => line(
                    Some((IconName::CircleCheck, success)),
                    format!(
                        "This will merge {} from {from} into {into}.",
                        plural(commits, "commit", "commits")
                    ),
                ),
                MergePreview::Unknown { commits } => line(
                    Some((IconName::Info, muted)),
                    format!(
                        "This will merge {} from {from} into {into}.",
                        plural(commits, "commit", "commits")
                    ),
                ),
                MergePreview::Conflicts { files, .. } => line(
                    Some((IconName::TriangleAlert, warning)),
                    format!(
                        "There will be {} when merging {from} into {into}.",
                        plural(files, "conflicted file", "conflicted files")
                    ),
                ),
            },
        }
    }
}

impl Render for MergeDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let title = h_flex()
            .child("Merge into\u{a0}")
            .child(div().font_weight(FontWeight::BOLD).child(self.into.clone()));
        let body = if self.list.is_empty() {
            div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(muted)
                .child(if self.list.query.is_empty() {
                    "There are no other branches to merge."
                } else {
                    "No branch matches."
                })
                .into_any_element()
        } else {
            self.list.render("merge-row", cx, Self::click_row).into_any_element()
        };
        v_flex()
            .id("merge-dialog")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(MergeEvent::Close)))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.list.move_selection(false);
                this.preview_selected(cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.list.move_selection(true);
                this.preview_selected(cx);
                cx.stop_propagation();
            }))
            .size_full()
            .popover_style(cx)
            .occlude()
            .overflow_hidden()
            .child(dialog_header(title, MergeEvent::Close, cx))
            .child(div().flex_none().p_2().child(render_filter(&self.filter, muted)))
            .child(body)
            .child(
                v_flex()
                    .flex_none()
                    .gap_3()
                    .p_3()
                    .border_t_1()
                    .border_color(border)
                    .child(self.render_preview(cx))
                    .child(
                        Button::new("create-merge-commit")
                            .primary()
                            .w_full()
                            .label("Create a merge commit")
                            .disabled(!self.can_merge())
                            .on_click(cx.listener(|this, _, _, cx| this.merge(cx))),
                    ),
            )
    }
}

// ---- Creating a branch -------------------------------------------------------------

#[derive(Clone)]
pub enum NewBranchEvent {
    /// Create a branch named `name` from `base` (else the current branch).
    Create { name: String, base: Option<String> },
    Close,
}

/// "Create a branch": a name, and what to base it on.
pub struct NewBranchDialog {
    name: Entity<InputState>,
    current: Option<String>,
    default: Option<String>,
    existing: HashSet<String>,
    /// Base it on the default branch, rather than the current one.
    from_default: bool,
    focus_handle: FocusHandle,
    _load_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<NewBranchEvent> for NewBranchDialog {}

impl Focusable for NewBranchDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.read(cx).focus_handle(cx)
    }
}

impl NewBranchDialog {
    pub fn new(repo: Repo, typed: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Branch name");
            state.set_value(typed.to_string(), window, cx);
            state
        });
        let subscriptions = vec![cx.subscribe_in(&name, window, |this, _, event: &InputEvent, _, cx| {
            match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.create(cx),
                _ => {}
            }
        })];
        let load_task = cx.spawn(async move |this, cx| {
            let branches = cx.background_spawn(async move { repo.branches() }).await;
            this.update(cx, |this, cx| {
                this.existing = branches
                    .all
                    .iter()
                    .filter(|branch| branch.remote.is_none())
                    .map(|branch| branch.name.clone())
                    .collect();
                this.current = branches.current;
                this.default = branches.default;
                cx.notify();
            })
            .ok();
        });
        Self {
            name,
            current: None,
            default: None,
            existing: HashSet::new(),
            from_default: false,
            focus_handle: cx.focus_handle(),
            _load_task: load_task,
            _subscriptions: subscriptions,
        }
    }

    /// The branch name to create, and whether it's taken.
    fn chosen(&self, cx: &App) -> (String, bool) {
        let name = git_branches::sanitize_branch_name(&self.name.read(cx).value());
        let taken = self.existing.contains(&name);
        (name, taken)
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        let (name, taken) = self.chosen(cx);
        if name.is_empty() || taken {
            return;
        }
        let base = if self.from_default {
            self.default.clone()
        } else {
            None
        };
        cx.emit(NewBranchEvent::Create { name, base });
    }
}

impl Render for NewBranchDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);
        let typed = self.name.read(cx).value().trim().to_string();
        let (name, taken) = self.chosen(cx);
        let hint = if taken {
            Some((format!("A branch named {name} already exists."), danger))
        } else if !name.is_empty() && name != typed {
            Some((format!("Will be created as {name}."), muted))
        } else {
            None
        };

        let base = match (&self.current, &self.default) {
            (Some(current), Some(default)) if current != default => {
                let entity = cx.entity().downgrade();
                v_flex()
                    .gap_2()
                    .child(div().text_sm().child("Create branch based on…"))
                    .child(
                        RadioGroup::vertical("branch-base")
                            .selected_index(Some(usize::from(!self.from_default)))
                            .on_click(move |ix: &usize, _, cx| {
                                let ix = *ix;
                                entity
                                    .update(cx, |this, cx| {
                                        this.from_default = ix == 0;
                                        cx.notify();
                                    })
                                    .ok();
                            })
                            .child(Radio::new("base-default").label(format!("{default} (the default branch)")))
                            .child(Radio::new("base-current").label(format!("{current} (the current branch)"))),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Changes not committed come along, if they can."),
                    )
                    .into_any_element()
            }
            (Some(current), _) => div()
                .text_sm()
                .text_color(muted)
                .child(format!(
                    "Your new branch will be based on {current}, the branch checked out. Changes not \
                     committed come along."
                ))
                .into_any_element(),
            _ => div().into_any_element(),
        };

        v_flex()
            .id("new-branch-dialog")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(NewBranchEvent::Close)))
            .size_full()
            .popover_style(cx)
            .occlude()
            .overflow_hidden()
            .child(dialog_header("Create a branch", NewBranchEvent::Close, cx))
            .child(
                v_flex()
                    .flex_1()
                    .gap_3()
                    .p_4()
                    .child(div().text_sm().child("Name"))
                    .child(Input::new(&self.name))
                    .when_some(hint, |column, (hint, color)| {
                        column.child(div().text_sm().text_color(color).child(hint))
                    })
                    .child(base),
            )
            .child(
                h_flex()
                    .flex_none()
                    .justify_end()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Button::new("cancel-new-branch")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(NewBranchEvent::Close))),
                    )
                    .child(
                        Button::new("create-branch")
                            .primary()
                            .small()
                            .label("Create branch")
                            .disabled(name.is_empty() || taken)
                            .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                    ),
            )
    }
}

// ---- Questions and messages ------------------------------------------------------

/// How a dialog button looks.
#[derive(Clone, Copy, PartialEq)]
pub enum Choice {
    Plain,
    Primary,
    Danger,
}

#[derive(Clone)]
pub enum MessageEvent {
    /// The button at this index was pressed.
    Chose(usize),
    Close,
}

/// A question or a message, with buttons: in the window, rather than the
/// system's dialog, so Git's long messages wrap on every system.
pub struct MessageDialog {
    title: SharedString,
    body: SharedString,
    buttons: Vec<(SharedString, Choice)>,
    focus_handle: FocusHandle,
}

impl EventEmitter<MessageEvent> for MessageDialog {}

impl Focusable for MessageDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl MessageDialog {
    pub fn new(
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
        buttons: Vec<(SharedString, Choice)>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            buttons,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for MessageDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        // Enter presses the primary button, if there's one: never a
        // dangerous one.
        let main = self.buttons.iter().position(|(_, choice)| *choice == Choice::Primary);
        let buttons: Vec<AnyElement> = self
            .buttons
            .iter()
            .enumerate()
            .map(|(ix, (label, choice))| {
                let button = Button::new(("dialog-button", ix)).small().label(label.clone());
                match choice {
                    Choice::Plain => button.ghost(),
                    Choice::Primary => button.primary(),
                    Choice::Danger => button.danger(),
                }
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(MessageEvent::Chose(ix))))
                .into_any_element()
            })
            .collect();
        v_flex()
            .id("message-dialog")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(MessageEvent::Close)))
            .on_key_down(cx.listener(move |_, event: &KeyDownEvent, _, cx| {
                if let Some(main) = main
                    && event.keystroke.key == "enter"
                    && event.keystroke.modifiers == Modifiers::default()
                {
                    cx.emit(MessageEvent::Chose(main));
                }
            }))
            .w_full()
            .popover_style(cx)
            .occlude()
            .overflow_hidden()
            .child(dialog_header(
                div().font_weight(FontWeight::SEMIBOLD).child(self.title.clone()),
                MessageEvent::Close,
                cx,
            ))
            .child(
                div()
                    .id("message-body")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .p_4()
                    .text_sm()
                    .child(self.body.clone()),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(border)
                    .children(buttons),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob of `gpui_kit` would replace `#[test]`.
    use super::{BranchList, Row, ago};
    use crate::git_branches::{Branch, Branches};

    fn branch(name: &str, remote: Option<&str>) -> Branch {
        Branch {
            name: name.into(),
            remote: remote.map(str::to_owned),
            time: 0,
        }
    }

    fn list() -> BranchList {
        let mut list = BranchList::default();
        list.set_branches(Branches {
            current: Some("feature".into()),
            default: Some("main".into()),
            recent: vec!["feature".into(), "fix".into()],
            all: vec![
                branch("feature", None),
                branch("fix", None),
                branch("main", None),
                branch("old", None),
                branch("shared", Some("origin")),
            ],
        });
        list
    }

    fn names(list: &BranchList) -> Vec<String> {
        list.rows
            .iter()
            .map(|row| match row {
                Row::Header(title) => format!("# {title}"),
                Row::Branch(ix) => list.branches.all[*ix].full_name(),
            })
            .collect()
    }

    #[test]
    fn branches_are_listed_in_sections_with_the_current_one_selected() {
        let list = list();
        assert_eq!(
            names(&list),
            [
                "# Default branch",
                "main",
                "# Recent branches",
                "feature",
                "fix",
                "# Other branches",
                "old",
                "origin/shared",
            ]
        );
        assert_eq!(list.selected_branch().unwrap().name, "feature");
    }

    #[test]
    fn typing_narrows_the_list_and_selects_the_first_match() {
        let mut list = list();
        list.set_query("sh");
        assert_eq!(names(&list), ["# Other branches", "origin/shared"]);
        assert_eq!(list.selected_branch().unwrap().name, "shared");
        list.set_query("nothing like it");
        assert!(list.is_empty());
        assert!(list.selected_branch().is_none());
    }

    #[test]
    fn the_arrow_keys_skip_headings() {
        let mut list = list();
        list.move_selection(true);
        assert_eq!(list.selected_branch().unwrap().name, "fix");
        list.move_selection(true);
        assert_eq!(list.selected_branch().unwrap().name, "old");
        list.move_selection(false);
        list.move_selection(false);
        list.move_selection(false);
        assert_eq!(list.selected_branch().unwrap().name, "main");
        list.move_selection(false);
        assert_eq!(list.selected_branch().unwrap().name, "main");
    }

    #[test]
    fn the_merge_list_leaves_out_the_current_branch() {
        let mut merge = BranchList {
            without_current: true,
            ..BranchList::default()
        };
        merge.set_branches(list().branches.clone());
        assert!(!names(&merge).contains(&"feature".to_string()));
    }

    #[test]
    fn times_read_like_github_desktop() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "a minute ago");
        assert_eq!(ago(5 * 60), "5 minutes ago");
        assert_eq!(ago(3 * 3600), "3 hours ago");
        assert_eq!(ago(86_400), "a day ago");
        assert_eq!(ago(7 * 86_400), "7 days ago");
        assert_eq!(ago(400 * 86_400), "a year ago");
    }
}
