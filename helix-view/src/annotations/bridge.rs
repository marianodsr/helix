//! What the bb bridge shows inside a document (fork addition, see bb-bridge/):
//! review annotations as virtual lines under their code, and the reveal
//! animation of text an agent just wrote.
//!
//! The data lives on the [`Document`] so the text-annotation machinery can
//! borrow it like inlay hints and inline diagnostics. This module only holds
//! it and reserves the virtual lines; helix-term's bridge fills it, maps it
//! through edits and draws it. Nothing here is part of the text.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use helix_core::diagnostic::Severity;
use helix_core::syntax::Highlight;
use helix_core::text_annotations::{LineAnnotation, Overlay, TextAnnotations};
use helix_core::unicode::segmentation::UnicodeSegmentation;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::{line_ending, ChangeSet, Position, RopeSlice};
use helix_core::{Assoc, Rope};

use crate::Document;

/// `:bb-annotations` hides every bridge annotation for the session.
static SHOWN: AtomicBool = AtomicBool::new(true);

pub fn shown() -> bool {
    SHOWN.load(Ordering::Relaxed)
}

pub fn set_shown(shown: bool) {
    SHOWN.store(shown, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Comment,
    Reply,
    Note,
}

/// One annotation: lines `first_line..=last_line` and a text shown under them.
#[derive(Debug, Clone)]
pub struct Item {
    /// Index into [`BridgeAnnotations::namespaces`].
    pub namespace: usize,
    /// Position in the list its namespace was set with.
    pub seq: usize,
    /// Start of the first line.
    pub start: usize,
    /// Start of the last line's line ending (or the end of the text).
    pub eol: usize,
    /// Start of the line after the last one (or the end of the text).
    pub next: usize,
    pub first_line: usize,
    pub last_line: usize,
    pub text: String,
    pub kind: Kind,
    pub severity: Option<Severity>,
    pub sign: Option<String>,
    pub highlight: bool,
}

impl Item {
    /// An item for 0-based lines `first..=last` (clamped to the text).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: RopeSlice,
        first: usize,
        last: usize,
        body: String,
        kind: Kind,
        severity: Option<Severity>,
        sign: Option<String>,
        highlight: bool,
    ) -> Item {
        let max = text.len_lines() - 1;
        let first = first.min(max);
        let last = last.clamp(first, max);
        Item {
            namespace: 0,
            seq: 0,
            start: text.line_to_char(first),
            eol: line_ending::line_end_char_index(&text, last),
            next: 0,
            first_line: first,
            last_line: last,
            text: body,
            kind,
            severity,
            sign,
            highlight,
        }
        .with_next(text)
    }

    fn with_next(mut self, text: RopeSlice) -> Item {
        self.next = if self.last_line + 1 < text.len_lines() {
            text.line_to_char(self.last_line + 1)
        } else {
            text.len_chars()
        };
        self
    }
}

/// A line the reveal animation shows struck through: `text` above `line`.
#[derive(Debug, Clone)]
pub struct RemovedLine {
    /// 0-based line (in the current text) the removed text was above.
    pub line: usize,
    pub text: String,
}

impl RemovedLine {
    /// The line whose virtual lines hold it: the one above, or the first
    /// line when it was at the top.
    pub fn anchor(&self) -> usize {
        self.line.saturating_sub(1)
    }
}

/// The reveal animation's current frame for one document.
#[derive(Debug, Default)]
pub struct Reveal {
    /// Graphemes not uncovered yet, sorted.
    pub overlays: Vec<Overlay>,
    /// Style of `overlays`: the background colour as foreground, when the
    /// theme has one; otherwise the overlays are blank graphemes.
    pub highlight: Option<Highlight>,
    /// Char ranges uncovered recently, sorted, with how strongly to
    /// highlight them (255 = just now).
    pub fresh: Vec<(Range<usize>, u8)>,
    /// Sorted by line.
    pub removed: Vec<RemovedLine>,
}

#[derive(Debug, Default)]
pub struct BridgeAnnotations {
    /// Namespace names, in the order first set.
    namespaces: Vec<String>,
    /// Sorted by (`last_line`, `namespace`, `seq`): the order they stack in.
    items: Vec<Item>,
    pub reveal: Option<Reveal>,
}

impl BridgeAnnotations {
    /// Nothing to show and nothing to map: the cheap check every hook makes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.reveal.is_none()
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Replace what `namespace` shows. Returns how many items it now has.
    pub fn set(&mut self, namespace: &str, items: Vec<Item>) -> usize {
        let ns = match self.namespaces.iter().position(|n| n == namespace) {
            Some(ns) => ns,
            None => {
                self.namespaces.push(namespace.to_owned());
                self.namespaces.len() - 1
            }
        };
        self.items.retain(|item| item.namespace != ns);
        let count = items.len();
        self.items
            .extend(items.into_iter().enumerate().map(|(seq, mut item)| {
                item.namespace = ns;
                item.seq = seq;
                item
            }));
        self.sort();
        count
    }

    /// Remove what `namespace` shows. Returns whether it showed anything.
    pub fn clear(&mut self, namespace: &str) -> bool {
        let Some(ns) = self.namespaces.iter().position(|n| n == namespace) else {
            return false;
        };
        let before = self.items.len();
        self.items.retain(|item| item.namespace != ns);
        before != self.items.len()
    }

    fn sort(&mut self) {
        self.items
            .sort_by_key(|item| (item.last_line, item.namespace, item.seq));
    }

    /// Follow an edit, the way diagnostics do. An item whose lines were
    /// deleted, line endings included, goes away with them.
    pub fn map(&mut self, changes: &ChangeSet, text: &Rope) {
        if self.items.is_empty() {
            return;
        }
        let spans: Vec<bool> = self.items.iter().map(|i| i.next > i.start).collect();
        changes.update_positions(self.items.iter_mut().map(|i| (&mut i.start, Assoc::After)));
        changes.update_positions(self.items.iter_mut().map(|i| (&mut i.eol, Assoc::Before)));
        changes.update_positions(self.items.iter_mut().map(|i| (&mut i.next, Assoc::Before)));
        let text = text.slice(..);
        let mut spans = spans.into_iter();
        self.items.retain_mut(|item| {
            let had_span = spans.next().unwrap_or(false);
            if had_span && item.next <= item.start {
                return false;
            }
            let len = text.len_chars();
            item.start = item.start.min(len);
            item.eol = item.eol.clamp(item.start, len);
            item.first_line = text.char_to_line(item.start);
            item.last_line = text.char_to_line(item.eol).max(item.first_line);
            // Keep the positions on line boundaries for the next edit.
            item.start = text.line_to_char(item.first_line);
            item.eol = line_ending::line_end_char_index(&text, item.last_line);
            item.next = if item.last_line + 1 < text.len_lines() {
                text.line_to_char(item.last_line + 1)
            } else {
                len
            };
            true
        });
        self.sort();
    }
}

// --- layout ------------------------------------------------------------------

/// Columns of the bar in front of an annotation's text (`▎ `).
pub const BAR_WIDTH: usize = 2;
/// Text never wraps narrower than this; the indent gives way first.
pub const MIN_TEXT_WIDTH: usize = 24;
/// Longer annotations are cut, with an ellipsis.
pub const MAX_ROWS: usize = 24;
/// Lines of code looked at to find the indent of an annotation.
const INDENT_SCAN: usize = 8;

/// What goes in front of the first row of an item's text; later rows are
/// indented by its width.
pub fn marker(kind: Kind) -> &'static str {
    match kind {
        Kind::Comment => "",
        Kind::Reply => "↳ ",
        Kind::Note => "",
    }
}

/// Where an item's rows go in a view `width` columns wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// Column of the bar (before horizontal scrolling).
    pub indent: usize,
    /// Columns available to the text after the bar and marker.
    pub text_width: usize,
}

/// Indent of the code the item marks: the leading whitespace of its first
/// non-blank line, in columns.
pub fn code_indent(text: RopeSlice, item: &Item, tab_width: usize) -> usize {
    let last = item.last_line.min(item.first_line + INDENT_SCAN);
    for line in item.first_line..=last.min(text.len_lines() - 1) {
        let mut col = 0;
        for ch in text.line(line).chars() {
            match ch {
                ' ' => col += 1,
                '\t' => col += tab_width - col % tab_width.max(1),
                '\n' | '\r' => break,
                _ => return col,
            }
        }
    }
    0
}

pub fn place(code_indent: usize, width: usize, kind: Kind) -> Option<Place> {
    let lead = BAR_WIDTH + marker(kind).width();
    if width < lead + 8 {
        return None;
    }
    let indent = code_indent.min(width.saturating_sub(lead + MIN_TEXT_WIDTH));
    Some(Place {
        indent,
        text_width: width - indent - lead,
    })
}

/// `text` wrapped at word boundaries to `width` columns: one byte range per
/// row (trailing spaces excluded), at most [`MAX_ROWS`]; `true` when cut.
pub fn wrap(text: &str, width: usize) -> (Vec<Range<usize>>, bool) {
    let mut rows = Vec::new();
    let width = width.max(1);
    let mut start = 0; // start of the row
    let mut row_width = 0;
    // Byte index after the last space in the row, and the end of the text before it.
    let mut brk: Option<(usize, usize)> = None;
    let push = |rows: &mut Vec<Range<usize>>, range: Range<usize>| {
        let end = range.start + text[range.clone()].trim_end().len();
        rows.push(range.start..end);
    };
    for (i, g) in text.grapheme_indices(true) {
        if rows.len() > MAX_ROWS {
            break;
        }
        if g == "\n" || g == "\r\n" {
            push(&mut rows, start..i);
            start = i + g.len();
            row_width = 0;
            brk = None;
            continue;
        }
        let w = g.width();
        // Spaces may hang past the edge; the row breaks before the next word.
        if g != " " && row_width + w > width && row_width > 0 {
            match brk {
                Some((after, before)) if after > start && before > start => {
                    push(&mut rows, start..before);
                    start = after;
                    row_width = text[start..i].width();
                }
                _ => {
                    push(&mut rows, start..i);
                    start = i;
                    row_width = 0;
                }
            }
            brk = None;
        }
        if g == " " && row_width == 0 && !rows.is_empty() && start == i {
            // no leading space on a wrapped row
            start = i + 1;
            continue;
        }
        row_width += w;
        if g == " " {
            brk = Some((i + 1, i));
        }
    }
    if start < text.len() || rows.is_empty() {
        push(&mut rows, start..text.len());
    }
    let cut = rows.len() > MAX_ROWS;
    rows.truncate(MAX_ROWS);
    (rows, cut)
}

/// Rows an item takes in a view `width` columns wide. The renderer draws
/// exactly this many.
pub fn item_rows(text: RopeSlice, item: &Item, width: usize, tab_width: usize) -> usize {
    match place(code_indent(text, item, tab_width), width, item.kind) {
        Some(place) => wrap(&item.text, place.text_width).0.len(),
        None => 0,
    }
}

// --- text annotations --------------------------------------------------------

/// Reserves the virtual lines under each annotated line and above each
/// removed line.
struct Rows<'a> {
    text: RopeSlice<'a>,
    items: &'a [Item],
    removed: &'a [RemovedLine],
    item: usize,
    gone: usize,
    width: usize,
    tab_width: usize,
}

impl LineAnnotation for Rows<'_> {
    fn reset_pos(&mut self, char_idx: usize) -> usize {
        let line = self.text.char_to_line(char_idx.min(self.text.len_chars()));
        self.item = self.items.partition_point(|i| i.last_line < line);
        self.gone = self.removed.partition_point(|r| r.anchor() < line);
        usize::MAX
    }

    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        // Called at the end of every visual line; only the end of a document
        // line (just after its line ending) gets rows.
        if doc_line + 1 >= self.text.len_lines()
            || self.text.line_to_char(doc_line + 1) != line_end_char_idx
        {
            return Position::new(0, 0);
        }
        let mut rows = 0;
        while let Some(item) = self.items.get(self.item) {
            if item.last_line > doc_line {
                break;
            }
            if item.last_line == doc_line {
                rows += item_rows(self.text, item, self.width, self.tab_width);
            }
            self.item += 1;
        }
        while let Some(removed) = self.removed.get(self.gone) {
            if removed.anchor() > doc_line {
                break;
            }
            if removed.anchor() == doc_line {
                rows += 1;
            }
            self.gone += 1;
        }
        Position::new(rows, 0)
    }
}

/// Called last in `View::text_annotations`.
pub fn add_text_annotations<'a>(
    annotations: &mut TextAnnotations<'a>,
    doc: &'a Document,
    width: u16,
) {
    let this = &doc.bridge_annotations;
    if this.is_empty() {
        return;
    }
    let removed = match &this.reveal {
        Some(reveal) => {
            annotations.add_overlay(&reveal.overlays, reveal.highlight);
            &reveal.removed[..]
        }
        None => &[],
    };
    let items = if shown() { &this.items[..] } else { &[] };
    if items.is_empty() && removed.is_empty() {
        return;
    }
    annotations.add_line_annotation(Box::new(Rows {
        text: doc.text().slice(..),
        items,
        removed,
        item: 0,
        gone: 0,
        width: width as usize,
        tab_width: doc.tab_width(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::Transaction;

    fn texts(text: &str, width: usize) -> Vec<&str> {
        wrap(text, width).0.into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn wraps_at_words() {
        assert_eq!(texts("one two three", 7), ["one two", "three"]);
        assert_eq!(texts("one two three", 8), ["one two", "three"]);
        assert_eq!(texts("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(texts("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(texts("", 10), [""]);
        assert_eq!(texts("short", 10), ["short"]);
        assert_eq!(texts("aa bb  cc", 5), ["aa bb", "cc"]);
        let long = "word ".repeat(200);
        let (rows, cut) = wrap(&long, 10);
        assert_eq!(rows.len(), MAX_ROWS);
        assert!(cut);
    }

    #[test]
    fn wide_graphemes_count_twice() {
        assert_eq!(texts("日本語の文", 4), ["日本", "語の", "文"]);
    }

    #[test]
    fn indent_gives_way_to_text() {
        assert_eq!(
            place(8, 100, Kind::Comment),
            Some(Place {
                indent: 8,
                text_width: 90
            })
        );
        let narrow = place(20, 30, Kind::Comment).unwrap();
        assert_eq!(narrow.indent, 4);
        assert_eq!(narrow.text_width, MIN_TEXT_WIDTH);
        assert_eq!(place(0, 5, Kind::Reply), None);
    }

    #[test]
    fn code_indent_expands_tabs() {
        let rope = Rope::from("\n\t\tx := 1\n  y\n");
        let text = rope.slice(..);
        let item = Item::new(text, 0, 1, String::new(), Kind::Comment, None, None, false);
        assert_eq!(code_indent(text, &item, 4), 8);
        let item = Item::new(text, 2, 2, String::new(), Kind::Comment, None, None, false);
        assert_eq!(code_indent(text, &item, 4), 2);
    }

    fn set(text: &Rope, lines: &[(usize, usize)]) -> BridgeAnnotations {
        let mut this = BridgeAnnotations::default();
        let items = lines
            .iter()
            .map(|&(a, b)| {
                Item::new(
                    text.slice(..),
                    a,
                    b,
                    format!("{a}-{b}"),
                    Kind::Comment,
                    None,
                    None,
                    false,
                )
            })
            .collect();
        this.set("ns", items);
        this
    }

    fn lines(this: &BridgeAnnotations) -> Vec<(usize, usize)> {
        this.items
            .iter()
            .map(|i| (i.first_line, i.last_line))
            .collect()
    }

    fn apply(text: &mut Rope, this: &mut BridgeAnnotations, edits: Vec<(usize, usize, &str)>) {
        let tx = Transaction::change(
            text,
            edits
                .into_iter()
                .map(|(a, b, s)| (a, b, (!s.is_empty()).then(|| s.into()))),
        );
        tx.apply(text);
        this.map(tx.changes(), text);
    }

    #[test]
    fn follows_inserted_and_deleted_lines() {
        let mut text = Rope::from("a\nb\nc\nd\n");
        let mut this = set(&text, &[(1, 2), (3, 3)]);
        // a new line above moves both down
        apply(&mut text, &mut this, vec![(0, 0, "new\n")]);
        assert_eq!(lines(&this), [(2, 3), (4, 4)]);
        // a line opened under the last marked line stays outside
        let eol = line_ending::line_end_char_index(&text.slice(..), 3);
        apply(&mut text, &mut this, vec![(eol, eol, "\nopened")]);
        assert_eq!(lines(&this), [(2, 3), (5, 5)]);
        // deleting one line of a range shrinks it
        let (a, b) = (text.line_to_char(2), text.line_to_char(3));
        apply(&mut text, &mut this, vec![(a, b, "")]);
        assert_eq!(lines(&this), [(2, 2), (4, 4)]);
        // emptying a line keeps the annotation
        let (a, b) = (
            text.line_to_char(4),
            line_ending::line_end_char_index(&text.slice(..), 4),
        );
        apply(&mut text, &mut this, vec![(a, b, "")]);
        assert_eq!(lines(&this), [(2, 2), (4, 4)]);
        // deleting the whole line removes it
        let (a, b) = (text.line_to_char(4), text.line_to_char(5));
        apply(&mut text, &mut this, vec![(a, b, "")]);
        assert_eq!(lines(&this), [(2, 2)]);
    }

    #[test]
    fn last_line_without_newline() {
        let mut text = Rope::from("a\nb");
        let mut this = set(&text, &[(1, 1), (5, 9)]);
        assert_eq!(lines(&this), [(1, 1), (1, 1)]);
        apply(&mut text, &mut this, vec![(3, 3, "c")]);
        assert_eq!(lines(&this), [(1, 1), (1, 1)]);
        apply(&mut text, &mut this, vec![(1, 4, "")]);
        assert_eq!(lines(&this), []);
    }

    #[test]
    fn namespaces_stack_in_order() {
        let text = Rope::from("a\nb\n");
        let mut this = BridgeAnnotations::default();
        let item = |s: &str| {
            Item::new(
                text.slice(..),
                0,
                0,
                s.into(),
                Kind::Comment,
                None,
                None,
                false,
            )
        };
        this.set("one", vec![item("1a"), item("1b")]);
        this.set("two", vec![item("2a")]);
        this.set("one", vec![item("1c")]);
        let order: Vec<_> = this.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(order, ["1c", "2a"]);
        assert!(this.clear("one"));
        assert!(!this.clear("one"));
        assert!(!this.clear("nope"));
        assert_eq!(this.items.len(), 1);
    }
}
