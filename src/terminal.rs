//! The integrated terminal: a shell running in a pseudo-terminal (ConPTY on
//! Windows), emulated by `alacritty_terminal` and painted cell by cell.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::{
    Event as TermEvent, EventListener, Notify as _, OnResize as _, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb};
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::component::input;
use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::*;

pub const KEY_CONTEXT: &str = "Terminal";

gpui_kit::actions!(
    terminal,
    [
        /// Tab completion. Bound here so the window's focus navigation
        /// doesn't take the key.
        SendTab,
        SendBackTab,
        /// Ctrl+C copies the selection if there is one, and otherwise
        /// interrupts the running program.
        CopyOrInterrupt,
        CopySelection,
    ]
);

/// Keys the terminal must receive itself, overriding the window's bindings.
pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("tab", SendTab, context),
        KeyBinding::new("shift-tab", SendBackTab, context),
        KeyBinding::new("ctrl-c", CopyOrInterrupt, context),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", CopySelection, context),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-c", CopySelection, context),
    ]);
}

/// What the workspace needs to hear about.
pub enum TerminalEvent {
    TitleChanged,
    /// The shell process ended (for example after `exit`).
    Exited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridSize {
    columns: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Forwards events from the terminal's I/O thread to the UI thread.
#[derive(Clone)]
pub struct Listener(UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

/// Where the grid was last painted, for mapping the mouse to cells.
#[derive(Clone, Copy)]
struct Layout {
    origin: Point<Pixels>,
    cell: Size<Pixels>,
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    focus_handle: FocusHandle,
    grid: GridSize,
    layout: Option<Layout>,
    title: Option<SharedString>,
    selecting: bool,
    /// Fractional scroll-wheel lines not yet applied.
    scroll_remainder: f32,
    _events: Task<()>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        // Stops the I/O thread, which closes the PTY and ends the shell.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

/// A running shell, ready to be shown by a [`TerminalView`].
pub struct Shell {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    events: UnboundedReceiver<TermEvent>,
    grid: GridSize,
}

/// Start the platform's default shell (PowerShell on Windows, `$SHELL`
/// elsewhere) in `working_directory`.
pub fn start_shell(working_directory: Option<PathBuf>) -> std::io::Result<Shell> {
    tty::setup_env();

    // A placeholder size; the first paint resizes it to fit the panel.
    let grid = GridSize {
        columns: 80,
        lines: 24,
    };
    let options = tty::Options {
        working_directory,
        drain_on_exit: true,
        ..Default::default()
    };
    let window_size = WindowSize {
        num_lines: grid.lines as u16,
        num_cols: grid.columns as u16,
        cell_width: 8,
        cell_height: 16,
    };
    let pty = tty::new(&options, window_size, 0)?;

    let (events_tx, events) = unbounded();
    let listener = Listener(events_tx);
    let term = Arc::new(FairMutex::new(Term::new(
        Config::default(),
        &grid,
        listener.clone(),
    )));
    let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)?;
    let notifier = Notifier(event_loop.channel());
    let _io_thread = event_loop.spawn();

    Ok(Shell {
        term,
        notifier,
        events,
        grid,
    })
}

impl TerminalView {
    pub fn new(shell: Shell, cx: &mut Context<Self>) -> Self {
        let Shell {
            term,
            notifier,
            events: mut events_rx,
            grid,
        } = shell;

        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = events_rx.next().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            term,
            notifier,
            focus_handle: cx.focus_handle(),
            grid,
            layout: None,
            title: None,
            selecting: false,
            scroll_remainder: 0.,
            _events: events,
        }
    }

    /// The title the shell set, if any.
    pub fn title(&self) -> Option<SharedString> {
        self.title.clone()
    }

    fn handle_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            TermEvent::Wakeup | TermEvent::CursorBlinkingChange | TermEvent::MouseCursorDirty => {
                cx.notify()
            }
            TermEvent::Title(title) => {
                self.title = Some(title.into());
                cx.emit(TerminalEvent::TitleChanged);
            }
            TermEvent::ResetTitle => {
                self.title = None;
                cx.emit(TerminalEvent::TitleChanged);
            }
            TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
            TermEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text))
            }
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                self.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(index, format) => {
                let palette = Palette::new(cx.theme());
                let color = {
                    let term = self.term.lock();
                    term.colors()[index].unwrap_or_else(|| palette.rgb_at(index))
                };
                self.write(format(color).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                self.write(format(self.window_size()).into_bytes())
            }
            TermEvent::Exit | TermEvent::ChildExit(_) => cx.emit(TerminalEvent::Exited),
            TermEvent::Bell => {}
        }
    }

    /// Send bytes to the shell.
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    /// Send typed input: jump back to the live screen and drop any selection.
    fn input(&mut self, bytes: impl Into<Cow<'static, [u8]>>, cx: &mut Context<Self>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
        cx.notify();
    }

    fn window_size(&self) -> WindowSize {
        let cell = self
            .layout
            .map_or(size(px(8.), px(16.)), |layout| layout.cell);
        WindowSize {
            num_lines: self.grid.lines as u16,
            num_cols: self.grid.columns as u16,
            cell_width: f32::from(cell.width) as u16,
            cell_height: f32::from(cell.height) as u16,
        }
    }

    /// Called while painting, once the panel's size is known.
    fn set_layout(&mut self, grid: GridSize, layout: Layout) {
        self.layout = Some(layout);
        if grid != self.grid {
            self.grid = grid;
            self.term.lock().resize(grid);
            let window_size = self.window_size();
            self.notifier.on_resize(window_size);
        }
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let text = self.term.lock().selection_to_string();
        match text {
            Some(text) if !text.is_empty() => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.term.lock().selection = None;
                cx.notify();
                true
            }
            _ => false,
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let bytes = if bracketed {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            // A terminal sends Enter as CR, so pasted line breaks must be too.
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.input(bytes.into_bytes(), cx);
    }

    fn send_tab(&mut self, _: &SendTab, _: &mut Window, cx: &mut Context<Self>) {
        self.input(&b"\t"[..], cx);
    }

    fn send_back_tab(&mut self, _: &SendBackTab, _: &mut Window, cx: &mut Context<Self>) {
        self.input(&b"\x1b[Z"[..], cx);
    }

    fn copy_or_interrupt(&mut self, _: &CopyOrInterrupt, _: &mut Window, cx: &mut Context<Self>) {
        if !self.copy_selection(cx) {
            self.input(&b"\x03"[..], cx);
        }
    }

    fn copy(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let key = keystroke.key.as_str();

        // Shift+PageUp/PageDown scroll back through the output.
        if modifiers.shift && !modifiers.control && !modifiers.alt {
            let scroll = match key {
                "pageup" => Some(Scroll::PageUp),
                "pagedown" => Some(Scroll::PageDown),
                _ => None,
            };
            if let Some(scroll) = scroll {
                self.term.lock().scroll_display(scroll);
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }

        // Ctrl+V and Ctrl+Shift+V paste, like Windows Terminal (Cmd+V on macOS).
        if key == "v" && (modifiers.control || modifiers.platform) && !modifiers.alt {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }

        let mode = *self.term.lock().mode();
        if let Some(bytes) = keystroke_to_bytes(keystroke, mode) {
            self.input(bytes, cx);
            cx.stop_propagation();
        }
    }

    /// The grid cell under a window position, and which half of it.
    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let layout = self.layout?;
        let x = ((position.x - layout.origin.x) / layout.cell.width).max(0.);
        let y = ((position.y - layout.origin.y) / layout.cell.height).max(0.);
        let column = (x.floor() as usize).min(self.grid.columns.saturating_sub(1));
        let row = (y.floor() as usize).min(self.grid.lines.saturating_sub(1));
        let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
        let display_offset = self.term.lock().grid().display_offset();
        let line = Line(row as i32 - display_offset as i32);
        Some((GridPoint::new(line, Column(column)), side))
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        // Double-click selects a word, triple-click a line.
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.term.lock().selection = Some(Selection::new(kind, point, side));
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        if let Some((point, side)) = self.grid_point(event.position) {
            if let Some(selection) = self.term.lock().selection.as_mut() {
                selection.update(point, side);
            }
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.layout else {
            return;
        };
        let delta = event.delta.pixel_delta(layout.cell.height);
        self.scroll_remainder += delta.y / layout.cell.height;
        let lines = self.scroll_remainder.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= lines as f32;

        let mode = *self.term.lock().mode();
        if mode.contains(TermMode::ALT_SCREEN) {
            // Full-screen programs (less, vim) get arrow keys instead.
            let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.write(arrow.repeat(lines.unsigned_abs() as usize));
        } else {
            self.term.lock().scroll_display(Scroll::Delta(lines));
        }
        cx.notify();
        cx.stop_propagation();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::new(cx.theme());
        let font_family = cx.theme().mono_font_family.clone();
        let font_size = cx.theme().mono_font_size;
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();
        let term = self.term.clone();
        let background = palette.background;

        div()
            .id("terminal")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::send_tab))
            .on_action(cx.listener(Self::send_back_tab))
            .on_action(cx.listener(Self::copy_or_interrupt))
            .on_action(cx.listener(Self::copy))
            // Edit › Copy and Edit › Paste from the menu bar.
            .on_action(cx.listener(|this, _: &input::Copy, _, cx| {
                this.copy_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &input::Paste, _, cx| this.paste(cx)))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        layout_frame(&entity, &term, palette, font_family, font_size, focused, bounds, window, cx)
                    },
                    |bounds, frame, window, cx| paint_frame(bounds, frame, window, cx),
                )
                .size_full(),
            )
    }
}

/// One row of shaped-ready text plus the cell backgrounds behind it.
#[derive(Default)]
struct Row {
    text: String,
    runs: Vec<TextRun>,
    /// (first column, end column, color)
    backgrounds: Vec<(usize, usize, Hsla)>,
}

struct Frame {
    rows: Vec<Row>,
    cell: Size<Pixels>,
    font_size: Pixels,
    cursor: Option<(usize, usize, CursorShape)>,
    cursor_color: Hsla,
    focused: bool,
}

#[allow(clippy::too_many_arguments)]
fn layout_frame(
    entity: &Entity<TerminalView>,
    term: &Arc<FairMutex<Term<Listener>>>,
    palette: Palette,
    font_family: SharedString,
    font_size: Pixels,
    focused: bool,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Frame {
    let base_font = font(font_family);
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&base_font);
    let cell_width = text_system
        .advance(font_id, font_size, 'm')
        .map_or(font_size * 0.6, |advance| advance.width);
    let cell = size(cell_width, (font_size * 1.4).round());

    let grid = GridSize {
        columns: ((bounds.size.width / cell.width).floor() as usize).max(2),
        lines: ((bounds.size.height / cell.height).floor() as usize).max(1),
    };
    entity.update(cx, |terminal, _| {
        terminal.set_layout(
            grid,
            Layout {
                origin: bounds.origin,
                cell,
            },
        )
    });

    let term = term.lock();
    let content = term.renderable_content();
    let display_offset = content.display_offset as i32;
    let mut rows: Vec<Row> = (0..grid.lines).map(|_| Row::default()).collect();

    for indexed in content.display_iter {
        let row_ix = indexed.point.line.0 + display_offset;
        let Some(row) = usize::try_from(row_ix).ok().and_then(|ix| rows.get_mut(ix)) else {
            continue;
        };
        let cell_data = indexed.cell;
        let flags = cell_data.flags;
        if flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let mut fg = palette.resolve(cell_data.fg, content.colors);
        let mut bg = palette.resolve(cell_data.bg, content.colors);
        // Bold text in one of the 8 base colors shows in its bright variant.
        if flags.contains(Flags::BOLD) {
            if let AnsiColor::Named(name) = cell_data.fg {
                if (name as usize) < 8 && content.colors[name].is_none() {
                    fg = palette.ansi[name as usize + 8];
                }
            }
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::DIM) {
            fg = dim(fg);
        }
        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if content
            .selection
            .as_ref()
            .is_some_and(|selection| selection.contains(indexed.point))
        {
            bg = palette.selection;
        }

        let column = indexed.point.column.0;
        let width = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        if bg != palette.background {
            match row.backgrounds.last_mut() {
                Some((_, end, color)) if *end == column && *color == bg => *end = column + width,
                _ => row.backgrounds.push((column, column + width, bg)),
            }
        }

        let start = row.text.len();
        row.text.push(if cell_data.c == '\0' { ' ' } else { cell_data.c });
        if let Some(zero_width) = cell_data.zerowidth() {
            row.text.extend(zero_width);
        }
        let len = row.text.len() - start;

        let run_font = Font {
            weight: if flags.contains(Flags::BOLD) {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if flags.contains(Flags::ITALIC) {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
            ..base_font.clone()
        };
        let underline = flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(fg),
            wavy: flags.contains(Flags::UNDERCURL),
        });
        let strikethrough = flags.contains(Flags::STRIKEOUT).then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(fg),
        });

        match row.runs.last_mut() {
            Some(run)
                if run.color == fg
                    && run.font == run_font
                    && run.underline == underline
                    && run.strikethrough == strikethrough =>
            {
                run.len += len
            }
            _ => row.runs.push(TextRun {
                len,
                font: run_font,
                color: fg,
                background_color: None,
                underline,
                strikethrough,
            }),
        }
    }

    let cursor = (content.mode.contains(TermMode::SHOW_CURSOR)
        && !matches!(content.cursor.shape, CursorShape::Hidden))
    .then(|| {
        let row = content.cursor.point.line.0 + display_offset;
        let column = content.cursor.point.column.0;
        usize::try_from(row)
            .ok()
            .filter(|row| *row < grid.lines)
            .map(|row| (row, column, content.cursor.shape))
    })
    .flatten();

    Frame {
        rows,
        cell,
        font_size,
        cursor,
        cursor_color: palette.cursor,
        focused,
    }
}

fn paint_frame(bounds: Bounds<Pixels>, frame: Frame, window: &mut Window, cx: &mut App) {
    let Frame {
        rows,
        cell,
        font_size,
        cursor,
        cursor_color,
        focused,
    } = frame;
    let cell_origin = |row: usize, column: usize| {
        point(
            bounds.origin.x + cell.width * column as f32,
            bounds.origin.y + cell.height * row as f32,
        )
    };

    for (row_ix, row) in rows.iter().enumerate() {
        for &(start, end, color) in &row.backgrounds {
            window.paint_quad(fill(
                Bounds::new(
                    cell_origin(row_ix, start),
                    size(cell.width * (end - start) as f32, cell.height),
                ),
                color,
            ));
        }
        if row.text.trim_end().is_empty() {
            continue;
        }
        // Forcing every glyph to the cell width keeps columns aligned.
        let line = window.text_system().shape_line(
            row.text.clone().into(),
            font_size,
            &row.runs,
            Some(cell.width),
        );
        line.paint(
            cell_origin(row_ix, 0),
            cell.height,
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
    }

    if let Some((row, column, shape)) = cursor {
        let origin = cell_origin(row, column);
        let quad = match shape {
            _ if !focused => outline(Bounds::new(origin, cell), cursor_color, BorderStyle::Solid),
            CursorShape::Beam => fill(Bounds::new(origin, size(px(2.), cell.height)), cursor_color),
            CursorShape::Underline => fill(
                Bounds::new(
                    point(origin.x, origin.y + cell.height - px(2.)),
                    size(cell.width, px(2.)),
                ),
                cursor_color,
            ),
            CursorShape::HollowBlock => {
                outline(Bounds::new(origin, cell), cursor_color, BorderStyle::Solid)
            }
            _ => fill(Bounds::new(origin, cell), cursor_color.opacity(0.6)),
        };
        window.paint_quad(quad);
    }
}

/// Translate a keystroke into the bytes a terminal sends for it.
fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = keystroke.modifiers;
    let key = keystroke.key.as_str();
    if m.platform {
        return None;
    }

    // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modifier = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor_key = |code: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{code}")
        } else if app_cursor {
            format!("\x1bO{code}")
        } else {
            format!("\x1b[{code}")
        }
        .into_bytes()
    };
    let tilde_key = |code: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{code};{modifier}~")
        } else {
            format!("\x1b[{code}~")
        }
        .into_bytes()
    };
    let function_key = |code: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{code}")
        } else {
            format!("\x1bO{code}")
        }
        .into_bytes()
    };

    let bytes = match key {
        "enter" if m.alt => b"\x1b\r".to_vec(),
        "enter" => b"\r".to_vec(),
        "backspace" if m.control => b"\x08".to_vec(),
        "backspace" if m.alt => b"\x1b\x7f".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => cursor_key('A'),
        "down" => cursor_key('B'),
        "right" => cursor_key('C'),
        "left" => cursor_key('D'),
        "home" => cursor_key('H'),
        "end" => cursor_key('F'),
        "insert" => tilde_key(2),
        "delete" => tilde_key(3),
        "pageup" => tilde_key(5),
        "pagedown" => tilde_key(6),
        "f1" => function_key('P'),
        "f2" => function_key('Q'),
        "f3" => function_key('R'),
        "f4" => function_key('S'),
        "f5" => tilde_key(15),
        "f6" => tilde_key(17),
        "f7" => tilde_key(18),
        "f8" => tilde_key(19),
        "f9" => tilde_key(20),
        "f10" => tilde_key(21),
        "f11" => tilde_key(23),
        "f12" => tilde_key(24),
        "space" if m.control => vec![0],
        _ => {
            // AltGr arrives as Ctrl+Alt on Windows; it types a character.
            if m.control && m.alt {
                if let Some(text) = &keystroke.key_char {
                    return Some(text.clone().into_bytes());
                }
            }
            if m.control {
                let mut chars = key.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                let code = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    '@' | '2' | ' ' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '/' | '7' => 0x1f,
                    '?' | '8' => 0x7f,
                    _ => return None,
                };
                return Some(if m.alt { vec![0x1b, code] } else { vec![code] });
            }
            let text = keystroke.key_char.clone().or_else(|| {
                let mut chars = key.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(c.to_string()),
                    _ => None,
                }
            })?;
            let mut bytes = if m.alt { vec![0x1b] } else { Vec::new() };
            bytes.extend_from_slice(text.as_bytes());
            bytes
        }
    };
    Some(bytes)
}

/// Terminal colors, following the app's light or dark theme.
#[derive(Clone, Copy)]
struct Palette {
    ansi: [Hsla; 16],
    foreground: Hsla,
    background: Hsla,
    cursor: Hsla,
    selection: Hsla,
}

/// The 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan,
/// white, then their bright variants.
const DARK_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, //
    0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];
const LIGHT_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, //
    0x666666, 0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

impl Palette {
    fn new(theme: &Theme) -> Self {
        let table = if theme.is_dark() { DARK_ANSI } else { LIGHT_ANSI };
        Self {
            ansi: table.map(|hex| rgb(hex).into()),
            foreground: theme.foreground,
            background: theme.background,
            cursor: theme.caret,
            selection: theme.selection,
        }
    }

    fn resolve(&self, color: AnsiColor, overrides: &Colors) -> Hsla {
        match color {
            AnsiColor::Spec(rgb) => rgb_to_hsla(rgb),
            AnsiColor::Indexed(index) => overrides[index as usize]
                .map(rgb_to_hsla)
                .unwrap_or_else(|| self.indexed(index)),
            AnsiColor::Named(name) => overrides[name]
                .map(rgb_to_hsla)
                .unwrap_or_else(|| self.named(name)),
        }
    }

    fn named(&self, name: NamedColor) -> Hsla {
        let dim_range = NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize;
        match name {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => dim(self.foreground),
            name if dim_range.contains(&(name as usize)) => {
                dim(self.ansi[name as usize - NamedColor::DimBlack as usize])
            }
            name => self.ansi[(name as usize).min(15)],
        }
    }

    /// The xterm 256-color table: 16 ANSI colors, a 6×6×6 cube, then grays.
    fn indexed(&self, index: u8) -> Hsla {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let i = index - 16;
                let level = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
                rgb_to_hsla(Rgb {
                    r: level(i / 36),
                    g: level((i / 6) % 6),
                    b: level(i % 6),
                })
            }
            232..=255 => {
                let v = 8 + (index - 232) * 10;
                rgb_to_hsla(Rgb { r: v, g: v, b: v })
            }
        }
    }

    /// The color at a palette index, for programs that ask (OSC 4/10/11).
    fn rgb_at(&self, index: usize) -> Rgb {
        let color = match index {
            0..=255 => self.indexed(index as u8),
            256 => self.foreground,
            257 => self.background,
            _ => self.cursor,
        };
        let rgba = Rgba::from(color);
        Rgb {
            r: (rgba.r * 255.).round() as u8,
            g: (rgba.g * 255.).round() as u8,
            b: (rgba.b * 255.).round() as u8,
        }
    }
}

fn rgb_to_hsla(color: Rgb) -> Hsla {
    Rgba {
        r: color.r as f32 / 255.,
        g: color.g as f32 / 255.,
        b: color.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn dim(color: Hsla) -> Hsla {
    color.opacity(0.7)
}
