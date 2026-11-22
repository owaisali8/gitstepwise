//! The terminal panel: one terminal, or several split side by side and one
//! above the other, in any mix. Its header shows the active terminal's
//! title, with buttons to split, hide and close.
//!
//! Starting a shell needs the workspace (it knows which folder to start in),
//! so splitting asks it with [`TerminalPanelEvent::Split`]. Moving between
//! terminals and closing them happen here.

use std::path::Path;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::*;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::terminal::{self, Shell, TerminalEvent, TerminalView};

gpui_kit::actions!(
    terminal_panel,
    [
        /// Start a new terminal to the right of the active one.
        SplitRight,
        /// Start a new terminal below the active one.
        SplitDown,
        /// Move to the terminal on the left (and so on for the others).
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        /// Close the active terminal, ending its shell.
        CloseTerminal,
    ]
);

#[cfg(target_os = "macos")]
const CLOSE_KEYS: &str = "cmd-shift-w";
#[cfg(not(target_os = "macos"))]
const CLOSE_KEYS: &str = "ctrl-shift-w";

/// Keys for any terminal in the panel, as in Windows Terminal.
fn key_bindings() -> Vec<KeyBinding> {
    let context = Some(terminal::KEY_CONTEXT);
    vec![
        // Shift with = or -, reported with or without the Shift.
        KeyBinding::new("alt-shift-=", SplitRight, context),
        KeyBinding::new("alt-+", SplitRight, context),
        KeyBinding::new("alt-shift--", SplitDown, context),
        KeyBinding::new("alt-_", SplitDown, context),
        KeyBinding::new("alt-left", FocusLeft, context),
        KeyBinding::new("alt-right", FocusRight, context),
        KeyBinding::new("alt-up", FocusUp, context),
        KeyBinding::new("alt-down", FocusDown, context),
        KeyBinding::new(CLOSE_KEYS, CloseTerminal, context),
    ]
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(key_bindings());
}

/// The smallest a terminal in a split can be made, so even a short panel
/// can be split.
const MIN_PANE_SIZE: Pixels = px(40.);

/// What the workspace needs to hear about.
pub enum TerminalPanelEvent {
    /// Start a terminal beside the active one: to its right on the
    /// horizontal axis, below it on the vertical.
    Split(Axis),
    Hide,
    /// The last terminal was closed, or its shell ended.
    Closed,
}

/// How the panel is divided: one terminal, or two parts side by side
/// (`Axis::Horizontal`) or one above the other (`Axis::Vertical`), each
/// divided the same way. Generic so the tests can use plain numbers.
enum Tree<P, S> {
    Pane(P),
    Split {
        axis: Axis,
        parts: Box<(Tree<P, S>, Tree<P, S>)>,
        split: S,
    },
}

impl<P: Copy + PartialEq, S> Tree<P, S> {
    /// The panes from left to right and top to bottom.
    #[cfg(test)]
    fn panes(&self) -> Vec<P> {
        match self {
            Tree::Pane(pane) => vec![*pane],
            Tree::Split { parts, .. } => {
                let mut panes = parts.0.panes();
                panes.extend(parts.1.panes());
                panes
            }
        }
    }

    fn first(&self) -> P {
        match self {
            Tree::Pane(pane) => *pane,
            Tree::Split { parts, .. } => parts.0.first(),
        }
    }

    fn last(&self) -> P {
        match self {
            Tree::Pane(pane) => *pane,
            Tree::Split { parts, .. } => parts.1.last(),
        }
    }

    /// Put `new` after `target`: right of it on the horizontal axis, below
    /// it on the vertical. Hands `split` back if `target` isn't here.
    fn split(&mut self, target: P, new: P, axis: Axis, split: S) -> Result<(), S> {
        match self {
            Tree::Pane(pane) if *pane == target => {
                let old = std::mem::replace(self, Tree::Pane(new));
                *self = Tree::Split {
                    axis,
                    parts: Box::new((old, Tree::Pane(new))),
                    split,
                };
                Ok(())
            }
            Tree::Pane(_) => Err(split),
            Tree::Split { parts, .. } => match parts.0.split(target, new, axis, split) {
                Ok(()) => Ok(()),
                Err(split) => parts.1.split(target, new, axis, split),
            },
        }
    }

    /// Take `target` out, and let the part beside it fill its place.
    /// Returns the pane nearest to where it was; `None` if `target` isn't
    /// here, or is the only pane.
    fn remove(&mut self, target: P) -> Option<P> {
        let Tree::Split { parts, .. } = self else {
            return None;
        };
        let is_target = |tree: &Tree<P, S>| matches!(tree, Tree::Pane(pane) if *pane == target);
        let keep_first = if is_target(&parts.1) {
            true
        } else if is_target(&parts.0) {
            false
        } else {
            return parts.0.remove(target).or_else(|| parts.1.remove(target));
        };
        let Tree::Split { parts, .. } = std::mem::replace(self, Tree::Pane(target)) else {
            unreachable!("matched a split above");
        };
        let (first, second) = *parts;
        let (rest, nearest) = if keep_first {
            let nearest = first.last();
            (first, nearest)
        } else {
            let nearest = second.first();
            (second, nearest)
        };
        *self = rest;
        Some(nearest)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// The pane beside `from` in `direction`: the nearest one past that edge
/// that lines up with it, and of those, the one that lines up the most.
fn neighbor<P: Copy + PartialEq>(
    panes: &[(P, Bounds<Pixels>)],
    from: P,
    direction: Direction,
) -> Option<P> {
    // Panes touch, or have a divider between them.
    const SLACK: f32 = 4.;
    let overlap = |a_start: Pixels, a_end: Pixels, b_start: Pixels, b_end: Pixels| {
        f32::from(a_end.min(b_end) - a_start.max(b_start))
    };
    let (_, a) = panes.iter().find(|(pane, _)| *pane == from)?;
    panes
        .iter()
        .filter(|(pane, _)| *pane != from)
        .filter_map(|(pane, b)| {
            let (gap, overlap) = match direction {
                Direction::Left => (a.left() - b.right(), overlap(a.top(), a.bottom(), b.top(), b.bottom())),
                Direction::Right => (b.left() - a.right(), overlap(a.top(), a.bottom(), b.top(), b.bottom())),
                Direction::Up => (a.top() - b.bottom(), overlap(a.left(), a.right(), b.left(), b.right())),
                Direction::Down => (b.top() - a.bottom(), overlap(a.left(), a.right(), b.left(), b.right())),
            };
            let gap = f32::from(gap);
            (gap > -SLACK && overlap > 0.).then_some((*pane, gap, overlap))
        })
        .min_by(|x, y| x.1.total_cmp(&y.1).then(y.2.total_cmp(&x.2)))
        .map(|(pane, _, _)| pane)
}

/// One terminal in the panel.
struct Pane {
    view: Entity<TerminalView>,
    _subscriptions: [Subscription; 2],
}

/// One split: an id for its element, and the sizes of its two parts.
struct Split {
    id: usize,
    sizes: Entity<ResizableState>,
}

pub struct TerminalPanel {
    /// Never empty: closing the last terminal closes the panel instead.
    panes: Vec<Pane>,
    tree: Tree<EntityId, Split>,
    /// The terminal that has focus, or had it last.
    active: EntityId,
    next_split_id: usize,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    /// The active terminal's.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane().view.read(cx).focus_handle(cx)
    }
}

impl TerminalPanel {
    pub fn new(shell: Shell, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = Self::new_pane(shell, window, cx);
        let id = pane.view.entity_id();
        Self {
            panes: vec![pane],
            tree: Tree::Pane(id),
            active: id,
            next_split_id: 0,
        }
    }

    fn new_pane(shell: Shell, window: &mut Window, cx: &mut Context<Self>) -> Pane {
        let view = cx.new(|cx| TerminalView::new(shell, cx));
        let id = view.entity_id();
        let focus_handle = view.read(cx).focus_handle(cx);
        let subscriptions = [
            cx.subscribe_in(&view, window, Self::on_terminal_event),
            cx.on_focus_in(&focus_handle, window, move |this, _, cx| {
                this.active = id;
                cx.notify();
            }),
        ];
        Pane {
            view,
            _subscriptions: subscriptions,
        }
    }

    fn pane(&self, id: EntityId) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.view.entity_id() == id)
    }

    fn active_pane(&self) -> &Pane {
        self.pane(self.active).unwrap_or(&self.panes[0])
    }

    /// Show `shell` beside the active terminal, and move to it.
    pub fn split(&mut self, axis: Axis, shell: Shell, window: &mut Window, cx: &mut Context<Self>) {
        let pane = Self::new_pane(shell, window, cx);
        let id = pane.view.entity_id();
        let split = Split {
            id: self.next_split_id,
            sizes: cx.new(|_| ResizableState::default()),
        };
        self.next_split_id += 1;
        if self.tree.split(self.active, id, axis, split).is_ok() {
            self.panes.push(pane);
            self.focus_pane(id, window, cx);
        }
    }

    /// Move the active terminal's shell to `dir`.
    pub fn change_directory(&self, dir: &Path, cx: &mut App) {
        self.active_pane()
            .view
            .update(cx, |terminal, cx| terminal.change_directory(dir, cx));
    }

    fn focus_pane(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        self.active = id;
        if let Some(pane) = self.pane(id) {
            pane.view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn move_focus(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let panes: Vec<(EntityId, Bounds<Pixels>)> = self
            .panes
            .iter()
            .filter_map(|pane| Some((pane.view.entity_id(), pane.view.read(cx).bounds()?)))
            .collect();
        if let Some(id) = neighbor(&panes, self.active, direction) {
            self.focus_pane(id, window, cx);
        }
    }

    /// Close a terminal, ending its shell. Closing the last one closes the
    /// panel.
    fn close(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() <= 1 {
            cx.emit(TerminalPanelEvent::Closed);
            return;
        }
        let Some(ix) = self.panes.iter().position(|pane| pane.view.entity_id() == id) else {
            return;
        };
        let Some(nearest) = self.tree.remove(id) else {
            return;
        };
        let had_focus = self.panes[ix].view.read(cx).focus_handle(cx).is_focused(window);
        // Dropping the view ends its shell.
        self.panes.remove(ix);
        if had_focus {
            self.focus_pane(nearest, window, cx);
        } else {
            if self.active == id {
                self.active = nearest;
            }
            cx.notify();
        }
    }

    fn close_active(&mut self, _: &CloseTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active;
        self.close(id, window, cx);
    }

    fn on_terminal_event(
        &mut self,
        view: &Entity<TerminalView>,
        event: &TerminalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::TitleChanged => cx.notify(),
            TerminalEvent::Exited => self.close(view.entity_id(), window, cx),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let title = self
            .active_pane()
            .view
            .read(cx)
            .title()
            .unwrap_or_else(|| "Shell".into());
        let close_tooltip = if self.panes.len() > 1 {
            "Close this terminal (ends its shell)"
        } else {
            "Close terminal (ends the shell)"
        };

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
                Button::new("split-terminal-right")
                    .ghost()
                    .xsmall()
                    .icon(LucideIcon::Columns2)
                    .tooltip("Split right (Alt+Shift+=)")
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(TerminalPanelEvent::Split(Axis::Horizontal))
                    })),
            )
            .child(
                Button::new("split-terminal-down")
                    .ghost()
                    .xsmall()
                    .icon(LucideIcon::Rows2)
                    .tooltip("Split down (Alt+Shift+-)")
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(TerminalPanelEvent::Split(Axis::Vertical))
                    })),
            )
            .child(
                Button::new("hide-terminal")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip("Hide terminal (Ctrl+`)")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(TerminalPanelEvent::Hide))),
            )
            .child(
                Button::new("kill-terminal")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(close_tooltip)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_active(&CloseTerminal, window, cx)
                    })),
            )
    }

    fn render_tree(&self, tree: &Tree<EntityId, Split>, cx: &App) -> AnyElement {
        match tree {
            Tree::Pane(id) => self.render_pane(*id, cx),
            Tree::Split { axis, parts, split } => {
                let id = ("terminal-split", split.id);
                let group = match axis {
                    Axis::Horizontal => h_resizable(id),
                    Axis::Vertical => v_resizable(id),
                };
                group
                    .with_state(&split.sizes)
                    .child(
                        resizable_panel()
                            .size_range(MIN_PANE_SIZE..Pixels::MAX)
                            .child(self.render_tree(&parts.0, cx)),
                    )
                    .child(
                        resizable_panel()
                            .size_range(MIN_PANE_SIZE..Pixels::MAX)
                            .child(self.render_tree(&parts.1, cx)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_pane(&self, id: EntityId, cx: &App) -> AnyElement {
        let Some(pane) = self.pane(id) else {
            return Empty.into_any_element();
        };
        let several = self.panes.len() > 1;
        // With more than one terminal, a line over the active one.
        let marker = if id == self.active {
            cx.theme().primary
        } else {
            transparent_black()
        };
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .pl_3()
            .pr_1()
            .pb_1()
            .when(several, |pane| pane.pt_1().border_t_2().border_color(marker))
            .child(pane.view.clone())
            .into_any_element()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &FocusLeft, window, cx| {
                this.move_focus(Direction::Left, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusRight, window, cx| {
                this.move_focus(Direction::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusUp, window, cx| {
                this.move_focus(Direction::Up, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusDown, window, cx| {
                this.move_focus(Direction::Down, window, cx)
            }))
            .on_action(cx.listener(Self::close_active))
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.render_tree(&self.tree, cx)))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob of `gpui_kit` would replace `#[test]`.
    use super::{Direction, Tree, key_bindings, neighbor};
    use gpui_kit::{Axis, Bounds, point, px, size};

    /// Panes 1 | (2 above 3), the way splitting right then down makes them.
    fn three_panes() -> Tree<u32, ()> {
        let mut tree = Tree::Pane(1);
        tree.split(1, 2, Axis::Horizontal, ()).unwrap();
        tree.split(2, 3, Axis::Vertical, ()).unwrap();
        tree
    }

    #[test]
    fn splitting_puts_the_new_pane_after_the_active_one() {
        let mut tree = three_panes();
        assert_eq!(tree.panes(), [1, 2, 3]);
        tree.split(1, 4, Axis::Vertical, ()).unwrap();
        assert_eq!(tree.panes(), [1, 4, 2, 3]);
        assert!(tree.split(9, 5, Axis::Vertical, ()).is_err());
    }

    #[test]
    fn closing_a_pane_gives_its_space_to_the_one_beside_it() {
        let mut tree = three_panes();
        // 3 shared the right half with 2, so 2 takes it all and is nearest.
        assert_eq!(tree.remove(3), Some(2));
        assert_eq!(tree.panes(), [1, 2]);
        assert_eq!(tree.remove(1), Some(2));
        assert_eq!(tree.panes(), [2]);
        // The last pane stays, and an unknown one changes nothing.
        assert_eq!(tree.remove(2), None);
        assert_eq!(tree.remove(7), None);
        assert_eq!(tree.panes(), [2]);
    }

    #[test]
    fn the_nearest_pane_is_the_one_closest_to_where_the_closed_one_was() {
        // 1 | (2 above (3 | 4)): closing 2 leaves 3 | 4, and 3 was nearest.
        let mut tree = three_panes();
        tree.split(3, 4, Axis::Horizontal, ()).unwrap();
        assert_eq!(tree.remove(2), Some(3));
        assert_eq!(tree.panes(), [1, 3, 4]);
        // Closing 1 gives the left half to 3 | 4, starting with 3.
        assert_eq!(tree.remove(1), Some(3));
    }

    #[test]
    fn moving_between_panes_follows_where_they_are() {
        // 1 on the left; 2 above 3 on the right, with a divider's gap.
        let bounds = |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        let panes = [
            (1, bounds(0., 0., 100., 100.)),
            (2, bounds(101., 0., 100., 50.)),
            (3, bounds(101., 51., 100., 49.)),
        ];
        // Both are beside 1; 2 lines up with more of it.
        assert_eq!(neighbor(&panes, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&panes, 2, Direction::Down), Some(3));
        assert_eq!(neighbor(&panes, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&panes, 3, Direction::Left), Some(1));
        assert_eq!(neighbor(&panes, 1, Direction::Left), None);
        assert_eq!(neighbor(&panes, 2, Direction::Right), None);
        assert_eq!(neighbor(&panes, 1, Direction::Down), None);
    }

    #[test]
    fn the_keys_parse() {
        // `KeyBinding::new` panics on a keystroke it can't read, which would
        // stop the app at startup.
        assert_eq!(key_bindings().len(), 9);
    }
}
