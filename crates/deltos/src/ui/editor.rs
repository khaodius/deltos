//! A multi-line code editor for queries: monospace text with SQL highlighting, line
//! numbers, selection, the clipboard and IME input. Only the lines on screen are shaped
//! and painted, so a long script scrolls as fast as a short one.
//!
//! It knows where its statements are (for the language its owner sets), marks the one the
//! cursor is in, and shows a completion popup filled by the owner's completer.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementInputHandler, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, Hsla, IntoElement, KeyBinding, MouseButton, Pixels,
    Point, Render, ShapedLine, SharedString, TextRun, UTF16Selection, Window, actions, canvas, div,
    fill, point, prelude::*, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::lang::{self, Language};
use crate::theme::{Theme, theme};
use crate::ui::completion::{Completer, CompletionMenu, Request};

actions!(
    editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocStart,
        DocEnd,
        PageUp,
        PageDown,
        SelectAll,
        Newline,
        Tab,
        Copy,
        Cut,
        Paste,
        Dismiss,
        ShowCompletions,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Editor");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("ctrl-left", WordLeft, ctx),
        KeyBinding::new("ctrl-right", WordRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("ctrl-home", DocStart, ctx),
        KeyBinding::new("ctrl-end", DocEnd, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("ctrl-a", SelectAll, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("tab", Tab, ctx),
        KeyBinding::new("ctrl-c", Copy, ctx),
        KeyBinding::new("ctrl-x", Cut, ctx),
        KeyBinding::new("ctrl-v", Paste, ctx),
        KeyBinding::new("shift-insert", Paste, ctx),
        KeyBinding::new("escape", Dismiss, ctx),
    ]);
}

const KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "as",
    "asc",
    "begin",
    "between",
    "by",
    "case",
    "cast",
    "check",
    "column",
    "commit",
    "constraint",
    "create",
    "cross",
    "database",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "explain",
    "false",
    "foreign",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "ilike",
    "limit",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "returning",
    "right",
    "rollback",
    "schema",
    "select",
    "set",
    "table",
    "then",
    "true",
    "truncate",
    "union",
    "unique",
    "update",
    "using",
    "values",
    "view",
    "when",
    "where",
    "with",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "over",
    "partition",
];

#[derive(Debug, Clone)]
pub enum EditorEvent {
    Changed,
}

pub struct Editor {
    focus: FocusHandle,
    text: String,
    line_starts: Vec<usize>,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    scroll_y: f32,
    /// The x the cursor keeps while moving up and down through shorter lines.
    goal_x: Option<Pixels>,
    selecting: bool,
    placeholder: SharedString,
    /// What the last paint laid out, for mouse and IME positions.
    layout: Option<Layout>,
    /// Plain text in a field: no line numbers, no highlighting.
    plain: bool,
    language: Language,
    /// Where each statement is, kept with the text.
    statements: Vec<Range<usize>>,
    completer: Option<Completer>,
    completion: Option<CompletionMenu>,
}

struct Layout {
    bounds: Bounds<Pixels>,
    text_left: Pixels,
    line_height: Pixels,
    first_line: usize,
    lines: Vec<ShapedLine>,
}

impl EventEmitter<EditorEvent> for Editor {}

impl Editor {
    pub fn new(text: String, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus: cx.focus_handle(),
            text,
            line_starts: Vec::new(),
            selected: 0..0,
            reversed: false,
            marked: None,
            scroll_y: 0.0,
            goal_x: None,
            selecting: false,
            placeholder: placeholder.into(),
            layout: None,
            plain: false,
            language: Language::default(),
            statements: Vec::new(),
            completer: None,
            completion: None,
        };
        this.reindex();
        this
    }

    /// Makes it a plain multi-line field, for values rather than queries.
    pub fn plain(mut self) -> Self {
        self.plain = true;
        self
    }

    /// Changes what shows while the text is empty.
    pub fn set_placeholder(&mut self, placeholder: &'static str, cx: &mut Context<Self>) {
        if self.placeholder != placeholder {
            self.placeholder = placeholder.into();
            cx.notify();
        }
    }

    /// Replaces the whole text, the cursor at its end.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.reindex();
        let end = self.text.len();
        self.selected = end..end;
        self.reversed = false;
        self.marked = None;
        self.scroll_y = 0.0;
        self.goal_x = None;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Sets the language, which decides where statements end.
    pub fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        if self.language != language {
            self.language = language;
            self.reindex();
            cx.notify();
        }
    }

    pub fn set_completer(&mut self, completer: Completer) {
        self.completer = Some(completer);
    }

    /// The selected range, which may be empty.
    pub fn selection(&self) -> Range<usize> {
        self.selected.clone()
    }

    /// The selection, or the statement the cursor is in, trimmed.
    pub fn current_statement(&self) -> Option<Range<usize>> {
        if !self.selected.is_empty() {
            let piece = &self.text[self.selected.clone()];
            let lead = piece.len() - piece.trim_start().len();
            let trail = piece.len() - piece.trim_end().len();
            let r = self.selected.start + lead..self.selected.end - trail;
            return (!r.is_empty()).then_some(r);
        }
        lang::split::statement_at(&self.statements, &self.text, self.cursor())
    }

    /// The text of [`Editor::current_statement`], or nothing.
    pub fn statement_at_cursor(&self) -> String {
        self.current_statement()
            .map(|r| self.text[r].trim_end_matches(';').trim().to_owned())
            .unwrap_or_default()
    }

    /// Every statement in the selection, or in the whole text when nothing is selected.
    pub fn all_statements(&self) -> Vec<String> {
        let (base, text) = if self.selected.is_empty() {
            (0, self.text.as_str())
        } else {
            (self.selected.start, &self.text[self.selected.clone()])
        };
        let ranges = if base == 0 && self.selected.is_empty() {
            self.statements.clone()
        } else {
            lang::split::statement_ranges(text, self.language)
        };
        ranges
            .into_iter()
            .filter_map(|r| text.get(r).map(str::to_owned))
            .collect()
    }

    /// Replaces `range` with `new`, leaving the cursor after it.
    pub fn replace_range(&mut self, range: Range<usize>, new: &str, cx: &mut Context<Self>) {
        self.completion = None;
        self.replace(range, new, cx);
    }

    /// Types `new` over the selection.
    pub fn insert_at_cursor(&mut self, new: &str, cx: &mut Context<Self>) {
        let range = self.selected.clone();
        self.replace_range(range, new, cx);
    }

    fn reindex(&mut self) {
        self.line_starts.clear();
        self.line_starts.push(0);
        for (i, b) in self.text.bytes().enumerate() {
            if b == b'\n' {
                self.line_starts.push(i + 1);
            }
        }
        self.statements = lang::split::statement_ranges(&self.text, self.language);
    }

    // ---- completion -------------------------------------------------------------------

    /// Asks the completer what fits at the cursor and opens, updates or closes the popup.
    /// `explicit` is a request by key, which shows the popup even with nothing typed.
    pub fn complete(&mut self, explicit: bool, cx: &mut Context<Self>) {
        let Some(completer) = self.completer.clone() else {
            return;
        };
        if !self.selected.is_empty() {
            self.completion = None;
            cx.notify();
            return;
        }
        let request = Request {
            text: &self.text,
            cursor: self.cursor(),
            statement: lang::split::statement_at(&self.statements, &self.text, self.cursor()),
            explicit,
            editor: cx.entity().downgrade(),
        };
        let found = completer(&request, cx);
        let keep = self.completion.as_ref().map(|m| m.selected_label());
        self.completion = found.and_then(|c| {
            let mut menu = CompletionMenu::new(c)?;
            if let Some(label) = keep.flatten() {
                menu.select_label(&label);
            }
            Some(menu)
        });
        cx.notify();
    }

    /// Runs the completer again for an open popup, as when the columns it waited for
    /// arrive.
    pub fn refresh_completions(&mut self, cx: &mut Context<Self>) {
        if self.completion.is_some() {
            let explicit = self.completion.as_ref().is_some_and(|m| m.explicit);
            self.complete(explicit, cx);
        }
    }

    fn close_completions(&mut self, cx: &mut Context<Self>) {
        if self.completion.take().is_some() {
            cx.notify();
        }
    }

    /// Accepts the popup's item `index`, as a click on it does.
    pub fn pick_completion(&mut self, index: usize, cx: &mut Context<Self>) {
        self.accept_completion(Some(index), cx);
    }

    fn accept_completion(&mut self, index: Option<usize>, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        let Some((range, insert)) = menu.accept(index) else {
            return false;
        };
        self.replace(range, &insert, cx);
        true
    }

    fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    fn line_range(&self, line: usize) -> Range<usize> {
        let start = self.line_starts[line];
        let end = self
            .line_starts
            .get(line + 1)
            .map_or(self.text.len(), |n| n - 1);
        start..end
    }

    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.completion = None;
        let offset = offset.min(self.text.len());
        self.selected = offset..offset;
        self.reversed = false;
        self.reveal();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.completion = None;
        let offset = offset.min(self.text.len());
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        self.reveal();
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.text.len())
    }

    fn x_in_line(&self, offset: usize) -> Pixels {
        let line = self.line_of(offset);
        let range = self.line_range(line);
        let Some(layout) = &self.layout else {
            return px(0.);
        };
        match line
            .checked_sub(layout.first_line)
            .and_then(|i| layout.lines.get(i))
        {
            Some(shaped) => shaped.x_for_index(offset - range.start),
            None => px(0.),
        }
    }

    /// The offset in `line` closest to `x`, using the last layout or the font's advance.
    fn offset_in_line(&self, line: usize, x: Pixels) -> usize {
        let range = self.line_range(line);
        if let Some(layout) = &self.layout
            && let Some(shaped) = line
                .checked_sub(layout.first_line)
                .and_then(|i| layout.lines.get(i))
        {
            return range.start + shaped.closest_index_for_x(x).min(range.len());
        }
        range.start
    }

    fn vertical(&mut self, lines: isize, extend: bool, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let line = self.line_of(cursor);
        let x = self.goal_x.unwrap_or_else(|| self.x_in_line(cursor));
        let target = (line as isize + lines).clamp(0, self.line_count() as isize - 1) as usize;
        let offset = if target == line && lines < 0 {
            0
        } else if target == line && lines > 0 {
            self.text.len()
        } else {
            self.offset_in_line(target, x)
        };
        if extend {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.goal_x = Some(x);
    }

    /// Scrolls so the cursor's line is on screen.
    fn reveal(&mut self) {
        let Some(layout) = &self.layout else { return };
        let lh = f32::from(layout.line_height);
        let view = f32::from(layout.bounds.size.height);
        let y = self.line_of(self.cursor()) as f32 * lh;
        if y < self.scroll_y {
            self.scroll_y = y;
        } else if y + lh > self.scroll_y + view {
            self.scroll_y = y + lh - view;
        }
    }

    fn replace(&mut self, range: Range<usize>, new: &str, cx: &mut Context<Self>) {
        let range = range.start.min(self.text.len())..range.end.min(self.text.len());
        self.text.replace_range(range.clone(), new);
        let end = range.start + new.len();
        self.selected = end..end;
        self.reversed = false;
        self.marked = None;
        self.goal_x = None;
        self.reindex();
        self.reveal();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn index_for_point(&self, p: Point<Pixels>) -> usize {
        let Some(layout) = &self.layout else { return 0 };
        let y = f32::from(p.y - layout.bounds.top()) + self.scroll_y;
        let line = ((y / f32::from(layout.line_height)).floor().max(0.0) as usize)
            .min(self.line_count() - 1);
        self.offset_in_line(line, p.x - layout.text_left)
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.text.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(r.start)..self.offset_from_utf16(r.end)
    }

    fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(r.start)..self.offset_to_utf16(r.end)
    }

    fn on_mouse_down(
        &mut self,
        e: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        let at = self.index_for_point(e.position);
        if e.click_count >= 2 {
            // A double click selects the word.
            let start = self.text[..at]
                .unicode_word_indices()
                .next_back()
                .filter(|(i, w)| i + w.len() >= at)
                .map_or(at, |(i, _)| i);
            let end = self.text[at..]
                .unicode_word_indices()
                .next()
                .filter(|(i, _)| *i == 0)
                .map_or(at, |(_, w)| at + w.len());
            self.selected = start..end;
            self.reversed = false;
            cx.notify();
            return;
        }
        self.selecting = true;
        if e.modifiers.shift {
            self.select_to(at, cx);
        } else {
            self.move_to(at, cx);
        }
    }
}

/// Colored runs for one line of SQL. `in_comment` carries a block comment across lines.
fn highlight(line: &str, t: &Theme, font: &gpui::Font, in_comment: &mut bool) -> Vec<TextRun> {
    let run = |len: usize, color: Hsla| TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let bytes = line.as_bytes();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut push = |len: usize, color: Hsla| {
        if len == 0 {
            return;
        }
        match runs.last_mut() {
            Some(last) if last.color == color => last.len += len,
            _ => runs.push(run(len, color)),
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        if *in_comment {
            let end = line[i..].find("*/").map_or(bytes.len(), |e| i + e + 2);
            if end <= bytes.len() && line[i..].contains("*/") {
                *in_comment = false;
            }
            push(end - i, t.syntax_comment);
            i = end;
            continue;
        }
        let b = bytes[i];
        if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            push(bytes.len() - i, t.syntax_comment);
            break;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            *in_comment = true;
            push(2, t.syntax_comment);
            i += 2;
            continue;
        }
        if b == b'\'' || b == b'"' {
            let end = line[i + 1..]
                .find(b as char)
                .map_or(bytes.len(), |e| i + 1 + e + 1);
            push(end - i, if b == b'\'' { t.syntax_string } else { t.text });
            i = end;
            continue;
        }
        if b.is_ascii_digit() {
            let end = line[i..]
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map_or(bytes.len(), |e| i + e);
            push(end - i, t.syntax_number);
            i = end;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let end = line[i..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(bytes.len(), |e| i + e);
            let word = &line[i..end];
            let keyword = KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word));
            push(end - i, if keyword { t.syntax_keyword } else { t.text });
            i = end;
            continue;
        }
        let len = line[i..].chars().next().map_or(1, char::len_utf8);
        push(len, t.text);
        i += len;
    }
    runs
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        actual.replace(self.range_to_utf16(&range));
        Some(self.text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.replace(range, new, cx);
        // A name character keeps the popup coming; anything else closes it.
        let mut chars = new.chars();
        let typed = match (chars.next(), chars.next()) {
            (Some(c), None) => c.is_alphanumeric() || c == '_' || c == '.' || c == '$',
            _ => false,
        };
        if typed {
            let explicit = self.completion.as_ref().is_some_and(|m| m.explicit);
            self.complete(explicit, cx);
        } else {
            self.close_completions(cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.replace(range.clone(), new, cx);
        self.marked = (!new.is_empty()).then(|| range.start..range.start + new.len());
        if let Some(sel) = new_selected {
            let sel = self.range_from_utf16(&sel);
            self.selected = range.start + sel.start..range.start + sel.end;
        }
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let start = self.range_from_utf16(&range).start;
        let line = self.line_of(start);
        let x = self.x_in_line(start);
        let y = layout.bounds.top() + layout.line_height * (line as f32) - px(self.scroll_y);
        Some(Bounds {
            origin: point(layout.text_left + x, y),
            size: size(px(2.), layout.line_height),
        })
    }

    fn character_index_for_point(
        &mut self,
        p: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_point(p)))
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let entity = cx.entity();
        let line_height = (t.editor_size * 1.55).round();
        let digits = self.line_count().to_string().len().max(2) as f32;
        let plain = self.plain;
        let gutter = if plain {
            px(6.)
        } else {
            px(digits * f32::from(t.editor_size) * 0.62 + 24.)
        };
        let lines_total = self.line_count();
        let text = self.text.clone();
        let starts = self.line_starts.clone();
        let selected = self.selected.clone();
        let cursor = self.cursor();
        let scroll_y = self.scroll_y;
        let placeholder = self.placeholder.clone();
        // The statement a run would take, marked when there is more than one.
        let active = (self.statements.len() > 1 && self.selected.is_empty())
            .then(|| self.current_statement())
            .flatten();
        let popup = self.completion.as_ref().and_then(|menu| {
            let layout = self.layout.as_ref()?;
            let start = menu.replace.start;
            let line = self.line_of(start);
            let x = layout.text_left + self.x_in_line(start) - px(6.);
            let y = layout.bounds.top() + layout.line_height * (line as f32 + 1.0)
                - px(self.scroll_y)
                + px(2.);
            Some(menu.render(point(x, y), cx.entity().downgrade(), &t))
        });
        let tp = t.clone();
        let painter = canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                let t = &tp;
                let font = gpui::font(t.editor_font.clone());
                let lh = line_height;
                let first = (scroll_y / f32::from(lh)).floor().max(0.0) as usize;
                let visible = (f32::from(bounds.size.height) / f32::from(lh)).ceil() as usize + 1;
                let last = (first + visible).min(lines_total);
                let text_left = bounds.left() + gutter;
                let focused = entity.read(cx).focus.is_focused(window);
                window.handle_input(
                    &entity.read(cx).focus.clone(),
                    ElementInputHandler::new(bounds, entity.clone()),
                    cx,
                );
                // A block comment opened above the screen carries into it.
                let mut in_comment = text[..starts.get(first).copied().unwrap_or(0)]
                    .matches("/*")
                    .count()
                    > text[..starts.get(first).copied().unwrap_or(0)]
                        .matches("*/")
                        .count();
                let mut shaped = Vec::with_capacity(last.saturating_sub(first));
                if !plain {
                    window.paint_quad(fill(
                        Bounds {
                            origin: bounds.origin,
                            size: size(gutter - px(8.), bounds.size.height),
                        },
                        t.surface,
                    ));
                }
                let cursor_line = starts.partition_point(|s| *s <= cursor).saturating_sub(1);
                for line in first..last {
                    let start = starts[line];
                    let end = starts.get(line + 1).map_or(text.len(), |n| n - 1);
                    let y = bounds.top() + lh * (line as f32) - px(scroll_y);
                    if let Some(a) = &active
                        && start <= a.end
                        && a.start <= end
                    {
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(text_left - px(4.), y),
                                size: size(bounds.right() - text_left + px(4.), lh),
                            },
                            t.accent.opacity(0.035),
                        ));
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(text_left - px(8.), y),
                                size: size(px(2.), lh),
                            },
                            t.accent.opacity(0.5),
                        ));
                    }
                    let content: SharedString = text[start..end].to_owned().into();
                    let runs = if plain {
                        vec![TextRun {
                            len: content.len(),
                            font: font.clone(),
                            color: t.text,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }]
                    } else {
                        highlight(&content, t, &font, &mut in_comment)
                    };
                    let line_shaped =
                        window
                            .text_system()
                            .shape_line(content, t.editor_size, &runs, None);
                    // Selection on this line.
                    let sel_start = selected.start.max(start);
                    let sel_end = selected.end.min(end);
                    if !selected.is_empty()
                        && (sel_start < sel_end || (selected.start <= start && selected.end > end))
                    {
                        let x0 = line_shaped
                            .x_for_index(sel_start.saturating_sub(start).min(end - start));
                        let mut x1 =
                            line_shaped.x_for_index(sel_end.saturating_sub(start).min(end - start));
                        if selected.end > end {
                            x1 += px(6.);
                        }
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(text_left + x0, y),
                                size: size((x1 - x0).max(px(0.)), lh),
                            },
                            t.selection,
                        ));
                    }
                    let _ = line_shaped.paint(point(text_left, y), lh, window, cx);
                    if focused && line == cursor_line && selected.is_empty() {
                        let x = line_shaped.x_for_index(cursor - start);
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(text_left + x, y + px(2.)),
                                size: size(px(1.5), lh - px(4.)),
                            },
                            t.accent,
                        ));
                    }
                    if plain {
                        shaped.push(line_shaped);
                        continue;
                    }
                    let number: SharedString = (line + 1).to_string().into();
                    let num_run = TextRun {
                        len: number.len(),
                        font: font.clone(),
                        color: if line == cursor_line {
                            t.text_muted
                        } else {
                            t.text_faint
                        },
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let num = window.text_system().shape_line(
                        number,
                        t.editor_size * 0.9,
                        &[num_run],
                        None,
                    );
                    let _ = num.paint(
                        point(text_left - px(14.) - num.width, y + px(1.)),
                        lh,
                        window,
                        cx,
                    );
                    shaped.push(line_shaped);
                }
                if text.is_empty() {
                    let run = TextRun {
                        len: placeholder.len(),
                        font: font.clone(),
                        color: t.text_faint,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().shape_line(
                        placeholder.clone(),
                        t.editor_size,
                        &[run],
                        None,
                    );
                    let _ = line.paint(point(text_left + px(4.), bounds.top()), lh, window, cx);
                }
                entity.update(cx, |e, _| {
                    e.layout = Some(Layout {
                        bounds,
                        text_left,
                        line_height: lh,
                        first_line: first,
                        lines: shaped,
                    });
                });
            },
        )
        .size_full();
        div()
            .id("editor")
            .key_context("Editor")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(if plain { t.input } else { t.surface })
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                if this.selected.is_empty() {
                    let c = this.cursor();
                    let p = this.previous_boundary(c);
                    this.selected = p..c;
                }
                this.replace(this.selected.clone(), "", cx);
                // An open popup follows what is left of the word.
                if let Some(explicit) = this.completion.as_ref().map(|m| m.explicit) {
                    this.complete(explicit, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Delete, _, cx| {
                this.close_completions(cx);
                if this.selected.is_empty() {
                    let c = this.cursor();
                    let n = this.next_boundary(c);
                    this.selected = c..n;
                }
                this.replace(this.selected.clone(), "", cx);
            }))
            .on_action(cx.listener(|this, _: &Dismiss, _, cx| {
                if this.completion.is_some() {
                    this.close_completions(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ShowCompletions, _, cx| this.complete(true, cx)))
            .on_action(cx.listener(|this, _: &Left, _, cx| {
                this.goal_x = None;
                let to = if this.selected.is_empty() {
                    this.previous_boundary(this.cursor())
                } else {
                    this.selected.start
                };
                this.move_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &Right, _, cx| {
                this.goal_x = None;
                let to = if this.selected.is_empty() {
                    this.next_boundary(this.cursor())
                } else {
                    this.selected.end
                };
                this.move_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                this.goal_x = None;
                let to = this.previous_boundary(this.cursor());
                this.select_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                this.goal_x = None;
                let to = this.next_boundary(this.cursor());
                this.select_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &Up, _, cx| {
                if let Some(menu) = &mut this.completion {
                    menu.move_by(-1);
                    cx.notify();
                } else {
                    this.vertical(-1, false, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &Down, _, cx| {
                if let Some(menu) = &mut this.completion {
                    menu.move_by(1);
                    cx.notify();
                } else {
                    this.vertical(1, false, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.vertical(-1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.vertical(1, true, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                if let Some(menu) = &mut this.completion {
                    menu.move_by(-8);
                    cx.notify();
                } else {
                    this.vertical(-20, false, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                if let Some(menu) = &mut this.completion {
                    menu.move_by(8);
                    cx.notify();
                } else {
                    this.vertical(20, false, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &WordLeft, _, cx| {
                let c = this.cursor();
                let to = this.text[..c]
                    .unicode_word_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i);
                this.move_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &WordRight, _, cx| {
                let c = this.cursor();
                let to = this.text[c..]
                    .unicode_word_indices()
                    .next()
                    .map_or(this.text.len(), |(i, w)| c + i + w.len());
                this.move_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &Home, _, cx| {
                let to = this.line_range(this.line_of(this.cursor())).start;
                this.move_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &End, _, cx| {
                let to = this.line_range(this.line_of(this.cursor())).end;
                this.move_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectHome, _, cx| {
                let to = this.line_range(this.line_of(this.cursor())).start;
                this.select_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectEnd, _, cx| {
                let to = this.line_range(this.line_of(this.cursor())).end;
                this.select_to(to, cx);
            }))
            .on_action(cx.listener(|this, _: &DocStart, _, cx| this.move_to(0, cx)))
            .on_action(cx.listener(|this, _: &DocEnd, _, cx| {
                let end = this.text.len();
                this.move_to(end, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.selected = 0..this.text.len();
                this.reversed = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Newline, _, cx| {
                if this.accept_completion(None, cx) {
                    return;
                }
                // The new line keeps the indentation of the one it splits.
                let line = this.line_range(this.line_of(this.cursor()));
                let indent: String = this.text[line]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                this.replace(this.selected.clone(), &format!("\n{indent}"), cx);
            }))
            .on_action(cx.listener(|this, _: &Tab, _, cx| {
                if this.accept_completion(None, cx) {
                    return;
                }
                this.replace(this.selected.clone(), "    ", cx);
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| {
                if !this.selected.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        this.text[this.selected.clone()].to_owned(),
                    ));
                }
            }))
            .on_action(cx.listener(|this, _: &Cut, _, cx| {
                this.close_completions(cx);
                if !this.selected.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        this.text[this.selected.clone()].to_owned(),
                    ));
                    this.replace(this.selected.clone(), "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                this.close_completions(cx);
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    let text = text.replace("\r\n", "\n");
                    this.replace(this.selected.clone(), &text, cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if this.selecting && e.pressed_button == Some(MouseButton::Left) {
                    let at = this.index_for_point(e.position);
                    this.select_to(at, cx);
                }
            }))
            .on_scroll_wheel(cx.listener(move |this, e: &gpui::ScrollWheelEvent, _, cx| {
                let d = e.delta.pixel_delta(line_height);
                let max = (this.line_count() as f32 - 1.0) * f32::from(line_height);
                this.scroll_y = (this.scroll_y - f32::from(d.y)).clamp(0.0, max.max(0.0));
                cx.notify();
            }))
            .child(painter)
            .children(popup)
    }
}
