//! Notes and a to-do list for each project, in a panel at the right of the
//! window. They're kept with the list of projects, in the config folder, so
//! they never show up as changes in the repository.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::projects::{self, Projects, Todo};

/// How long typing pauses before the notes are written to disk.
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// Add a to-do with `text`, unless it's blank. Returns whether one was added.
fn add_todo(todos: &mut Vec<Todo>, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    todos.push(Todo {
        text: text.to_string(),
        done: false,
    });
    true
}

/// How many to-dos aren't done yet.
pub fn open_count(todos: &[Todo]) -> usize {
    todos.iter().filter(|todo| !todo.done).count()
}

/// What the workspace needs to hear about.
pub enum NotesEvent {
    Close,
}

pub struct NotesPanel {
    /// The project the notes belong to: the explorer's folder.
    folder: Option<PathBuf>,
    todos: Vec<Todo>,
    new_todo: Entity<InputState>,
    notes: Entity<TextareaState>,
    /// Whether there are changes not yet written to disk.
    unsaved: bool,
    _save_task: Option<Task<()>>,
    _subscriptions: [Subscription; 3],
}

impl EventEmitter<NotesEvent> for NotesPanel {}

impl Focusable for NotesPanel {
    /// The to-do field's.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.new_todo.read(cx).focus_handle(cx)
    }
}

impl NotesPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let new_todo = cx.new(|cx| InputState::new(window, cx).placeholder("Add a to-do, then Enter"));
        let notes = cx.new(|cx| TextareaState::new(window, cx).placeholder("Notes about this project…"));
        let subscriptions = [
            cx.subscribe_in(&new_todo, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add(window, cx);
                }
            }),
            cx.subscribe_in(&notes, window, |this, _, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    this.store(cx);
                }
            }),
            // Notes typed just before quitting are written anyway.
            cx.on_app_quit(|this, cx| {
                if this.unsaved {
                    cx.global::<Projects>().save();
                }
                async {}
            }),
        ];
        Self {
            folder: None,
            todos: Vec::new(),
            new_todo,
            notes,
            unsaved: false,
            _save_task: None,
            _subscriptions: subscriptions,
        }
    }

    /// Show the notes and to-dos of the project at `folder`.
    pub fn set_project(&mut self, folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .folder
            .as_deref()
            .is_some_and(|showing| projects::same_folder(showing, &folder))
        {
            return;
        }
        let (notes, todos) = cx
            .global::<Projects>()
            .get(&folder)
            .map(|project| (project.notes.clone(), project.todos.clone()))
            .unwrap_or_default();
        self.todos = todos;
        // Setting a value sends no change event, so nothing is stored here.
        self.notes.update(cx, |state, cx| state.set_value(notes, window, cx));
        self.new_todo.update(cx, |state, cx| state.set_value("", window, cx));
        self.folder = Some(folder);
        cx.notify();
    }

    /// How many to-dos aren't done yet, for the status bar.
    pub fn open_count(&self) -> usize {
        open_count(&self.todos)
    }

    /// Whether the cursor is in the to-do field or the notes.
    pub fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.new_todo.read(cx).focus_handle(cx).is_focused(window)
            || self.notes.read(cx).focus_handle(cx).is_focused(window)
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.new_todo.read(cx).value();
        if add_todo(&mut self.todos, &text) {
            self.new_todo.update(cx, |state, cx| state.set_value("", window, cx));
            self.store(cx);
            cx.notify();
        }
    }

    fn set_done(&mut self, ix: usize, done: bool, cx: &mut Context<Self>) {
        if let Some(todo) = self.todos.get_mut(ix) {
            todo.done = done;
            self.store(cx);
            cx.notify();
        }
    }

    fn remove(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.todos.len() {
            self.todos.remove(ix);
            self.store(cx);
            cx.notify();
        }
    }

    fn clear_done(&mut self, cx: &mut Context<Self>) {
        self.todos.retain(|todo| !todo.done);
        self.store(cx);
        cx.notify();
    }

    /// Keep the notes and to-dos in the list of projects, and write it to
    /// disk once typing pauses.
    fn store(&mut self, cx: &mut Context<Self>) {
        let Some(folder) = self.folder.clone() else {
            return;
        };
        let notes = self.notes.read(cx).value().to_string();
        let todos = &self.todos;
        let changed = cx.update_global::<Projects, _>(|projects, _| {
            projects.set_notes(&folder, &notes, todos, projects::now_ms())
        });
        if !changed {
            return;
        }
        self.unsaved = true;
        self._save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |this, cx| {
                cx.global::<Projects>().save();
                this.unsaved = false;
            })
            .ok();
        }));
    }

    fn render_todo(&self, ix: usize, todo: &Todo, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id(("todo", ix))
            .group("todo")
            .h_flex()
            .items_start()
            .gap_2()
            .py_0p5()
            .child(
                Checkbox::new(("todo-done", ix))
                    .checked(todo.done)
                    .on_click(cx.listener(move |this, done: &bool, _, cx| this.set_done(ix, *done, cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .when(todo.done, |text| text.line_through().text_color(theme.muted_foreground))
                    .child(todo.text.clone()),
            )
            .child(
                // Shows while the pointer is over the to-do.
                div().opacity(0.).group_hover("todo", |style| style.opacity(1.)).child(
                    Button::new(("todo-remove", ix))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Remove")
                        .on_click(cx.listener(move |this, _, _, cx| this.remove(ix, cx))),
                ),
            )
    }
}

impl Render for NotesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let done = self.todos.len() - self.open_count();
        let heading = |text: &'static str| div().font_weight(FontWeight::SEMIBOLD).child(text);
        let rows: Vec<AnyElement> = self
            .todos
            .iter()
            .enumerate()
            .map(|(ix, todo)| self.render_todo(ix, todo, cx).into_any_element())
            .collect();

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_1()
                    .h(px(28.))
                    .flex_none()
                    .text_xs()
                    .text_color(muted)
                    .child(heading("TO-DO"))
                    .when(!self.todos.is_empty(), |row| {
                        row.child(format!("{done} of {} done", self.todos.len()))
                    })
                    .child(div().flex_1())
                    .when(done > 0, |row| {
                        row.child(
                            Button::new("clear-done")
                                .ghost()
                                .xsmall()
                                .label("Clear done")
                                .on_click(cx.listener(|this, _, _, cx| this.clear_done(cx))),
                        )
                    })
                    .child(
                        Button::new("close-notes")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(format!("Hide notes ({})", crate::shortcut("Alt+B")))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(NotesEvent::Close))),
                    ),
            )
            .child(div().flex_none().px_3().child(Input::new(&self.new_todo).small()))
            .child(
                // Shrinks and scrolls when the to-dos outgrow the panel.
                div()
                    .id("todos")
                    .flex_shrink(1.)
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .py_1()
                    .children(rows),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .pl_3()
                    .h(px(28.))
                    .border_t_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(heading("NOTES")),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(120.))
                    .px_3()
                    .pb_3()
                    .child(Textarea::new(&self.notes).h(relative(1.))),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob of `gpui_kit` would replace `#[test]`.
    use super::{add_todo, open_count};
    use crate::projects::Todo;

    #[test]
    fn blank_to_dos_are_not_added() {
        let mut todos = Vec::new();
        assert!(add_todo(&mut todos, "  Check the migration  "));
        assert!(!add_todo(&mut todos, "   "));
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].text, "Check the migration");
        assert!(!todos[0].done);
    }

    #[test]
    fn only_unfinished_to_dos_are_counted() {
        let todo = |done| Todo {
            text: "x".into(),
            done,
        };
        assert_eq!(open_count(&[todo(false), todo(true), todo(false)]), 2);
        assert_eq!(open_count(&[]), 0);
    }
}
