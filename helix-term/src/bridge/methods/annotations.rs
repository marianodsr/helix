//! annotations/set, annotations/clear, `:bb-annotations` (step 2d), and how
//! annotations are drawn.
//!
//! The items live on the document (`helix_view::annotations::bridge`), where
//! `View::text_annotations` reserves their virtual lines like inline
//! diagnostics do. This file fills them in, maps them through edits, keeps
//! the ones for files that are not open yet, and draws them: a sign in the
//! gutter, an optional line background, and the text under the code.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::future::BoxFuture;
use helix_core::command_line::Signature;
use helix_core::diagnostic::Severity;
use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::unicode::segmentation::UnicodeSegmentation;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::{
    char_idx_at_visual_offset, line_ending::line_end_char_index, visual_offset_from_anchor,
    Position, RopeSlice,
};
use helix_event::{register_hook, runtime_local};
use helix_view::annotations::bridge::{
    self as data, code_indent, place, shown, wrap, Item, Kind, RemovedLine, BAR_WIDTH,
};
use helix_view::editor::GutterType;
use helix_view::events::{DocumentDidChange, DocumentDidOpen};
use helix_view::graphics::{Color, Modifier, Rect, Style};
use helix_view::view::ViewPosition;
use helix_view::{doc, doc_mut, view, Document, Editor, Theme, View};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::bridge::convert::{open_path, resolve_path};
use crate::bridge::dispatch::on_editor;
use crate::bridge::protocol::{parse_params, RpcError, RpcResult};
use crate::bridge::server::Ctx;
use crate::commands::typed::{CommandCompleter, TypableCommand};
use crate::compositor;
use crate::events::PostCommand;
use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::{Decoration, DecorationManager};
use crate::ui::PromptEvent;

pub const CAPABILITY: Option<&str> = Some("annotations");

/// Most items one `annotations/set` may carry, and characters per item.
const MAX_ITEMS: usize = 500;
const MAX_CHARS: usize = 2000;

pub const COMMANDS: &[TypableCommand] = &[TypableCommand {
    name: "bb-annotations",
    aliases: &[],
    doc: "Show or hide the annotations bb put in the code.",
    fun: toggle,
    completer: CommandCompleter::none(),
    signature: Signature {
        positionals: (0, Some(0)),
        ..Signature::DEFAULT
    },
}];

fn toggle(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    data::set_shown(!shown());
    cx.editor.set_status(if shown() {
        "bb annotations shown"
    } else {
        "bb annotations hidden"
    });
    Ok(())
}

// --- requests ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetParams {
    namespace: String,
    path: String,
    items: Vec<ItemParams>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemParams {
    line: usize,
    end_line: Option<usize>,
    text: String,
    #[serde(default)]
    kind: KindParam,
    severity: Option<SeverityParam>,
    sign: Option<String>,
    #[serde(default)]
    highlight: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum KindParam {
    #[default]
    Comment,
    Reply,
    Note,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum SeverityParam {
    Hint,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearParams {
    namespace: String,
    path: Option<String>,
}

/// Annotations for files that are not open, applied when they open.
/// `path -> [(namespace, items)]`.
type Pending = HashMap<PathBuf, Vec<(String, Vec<ItemParams>)>>;

runtime_local! {
    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
    /// Some annotation was set in this process: the key hook has work.
    static USED: AtomicBool = AtomicBool::new(false);
}

/// Annotation text as it is drawn: no tabs or control characters, no
/// trailing blank lines.
fn clean_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.replace("\r\n", "\n").chars() {
        match ch {
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out.trim_end().to_owned()
}

fn validate(items: &mut [ItemParams]) -> Result<(), RpcError> {
    if items.len() > MAX_ITEMS {
        return Err(RpcError::invalid_params(
            "items",
            format!("at most {MAX_ITEMS} items per file"),
        ));
    }
    for item in items {
        if item.text.chars().count() > MAX_CHARS {
            return Err(RpcError::invalid_params(
                "text",
                format!("at most {MAX_CHARS} characters per item"),
            ));
        }
        if let Some(sign) = &item.sign {
            if sign.graphemes(true).count() != 1 || sign.chars().any(char::is_control) {
                return Err(RpcError::invalid_params(
                    "sign",
                    "sign must be one grapheme",
                ));
            }
        }
        item.text = clean_text(&item.text);
    }
    Ok(())
}

fn to_item(text: RopeSlice, p: &ItemParams) -> Item {
    let first = p.line.max(1) - 1;
    let last = p.end_line.unwrap_or(p.line).max(p.line).max(1) - 1;
    Item::new(
        text,
        first,
        last,
        p.text.clone(),
        match p.kind {
            KindParam::Comment => Kind::Comment,
            KindParam::Reply => Kind::Reply,
            KindParam::Note => Kind::Note,
        },
        p.severity.map(|s| match s {
            SeverityParam::Hint => Severity::Hint,
            SeverityParam::Info => Severity::Info,
            SeverityParam::Warning => Severity::Warning,
            SeverityParam::Error => Severity::Error,
        }),
        p.sign.clone(),
        p.highlight,
    )
}

fn set_on_doc(doc: &mut Document, namespace: &str, items: &[ItemParams]) -> usize {
    if !items.is_empty() {
        USED.store(true, Ordering::Relaxed);
    }
    let text = doc.text().slice(..);
    let items = items.iter().map(|p| to_item(text, p)).collect();
    doc.bridge_annotations.set(namespace, items)
}

pub fn set(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let mut params: SetParams = parse_params(params)?;
        if params.namespace.is_empty() {
            return Err(RpcError::invalid_params("namespace", "namespace is empty"));
        }
        validate(&mut params.items)?;
        let path = resolve_path(&params.path, &cx.shared.info.workspace_root);
        let on_disk = tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file());
        let count = on_editor(move |editor, _| {
            let count = set_on_main(editor, &path, on_disk, params);
            show_last_line_rows(editor);
            count
        })
        .await??;
        Ok(json!({ "count": count }))
    })
}

fn set_on_main(
    editor: &mut Editor,
    path: &Path,
    on_disk: bool,
    params: SetParams,
) -> Result<usize, RpcError> {
    let path = &open_path(editor, path);
    if let Some(id) = editor.document_id_by_path(path) {
        let doc = doc_mut!(editor, &id);
        return Ok(set_on_doc(doc, &params.namespace, &params.items));
    }
    if !on_disk {
        return Err(RpcError::not_found(format!("{} not found", path.display())));
    }
    let mut pending = PENDING.lock();
    let files = pending.get_or_insert_with(HashMap::new);
    let path = helix_stdx::path::canonicalize(path);
    let file = files.entry(path.clone()).or_default();
    file.retain(|(ns, _)| *ns != params.namespace);
    let count = params.items.len();
    if count > 0 {
        file.push((params.namespace, params.items));
    }
    if file.is_empty() {
        files.remove(&path);
    }
    Ok(count)
}

pub fn clear(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: ClearParams = parse_params(params)?;
        let path = params.path.as_deref().map(|p| {
            helix_stdx::path::canonicalize(resolve_path(p, &cx.shared.info.workspace_root))
        });
        on_editor(move |editor, _| {
            let ns = params.namespace;
            if let Some(files) = PENDING.lock().as_mut() {
                for (file, list) in files.iter_mut() {
                    if path.as_ref().is_none_or(|p| p == file) {
                        list.retain(|(n, _)| *n != ns);
                    }
                }
                files.retain(|_, list| !list.is_empty());
            }
            let ids: Vec<_> = match &path {
                Some(path) => editor
                    .document_id_by_path(&open_path(editor, path))
                    .into_iter()
                    .collect(),
                None => editor.documents().map(Document::id).collect(),
            };
            for id in ids {
                doc_mut!(editor, &id).bridge_annotations.clear(&ns);
            }
        })
        .await?;
        Ok(json!({}))
    })
}

/// The last line of a document gets no virtual lines (Helix lays them out
/// after a line ending), so its annotation is drawn in the empty rows below
/// the end of the text, and was cut off when that line sat at the bottom of
/// the view. The rows of any other line come into view when the cursor moves
/// down past them; here, while the cursor is on the last line, the view
/// scrolls just enough for them to fit, never pushing the cursor line out.
fn show_last_line_rows(editor: &mut Editor) {
    let view = view!(editor);
    let doc = doc!(editor, &view.doc);
    let items = doc.bridge_annotations.items();
    if items.is_empty() || !shown() {
        return;
    }
    let text = doc.text().slice(..);
    let last = text.len_lines() - 1;
    let cursor = doc.selection(view.id).primary().cursor(text);
    if text.char_to_line(cursor) != last {
        return;
    }
    let viewport = view.inner_area(doc);
    let width = viewport.width as usize;
    let rows: usize = items
        .iter()
        .filter(|item| item.last_line == last)
        .map(|item| data::item_rows(text, item, width, doc.tab_width()))
        .sum();
    if rows == 0 {
        return;
    }
    // Helix brings the cursor into view after the hooks; do it first (it
    // changes nothing the second time).
    let view_id = view.id;
    editor.ensure_cursor_in_view(view_id);
    let view = view!(editor);
    let doc = doc!(editor, &view.doc);
    let text = doc.text().slice(..);
    let height = viewport.height as usize;
    let offset = doc.view_offset(view.id);
    let text_fmt = doc.text_format(viewport.width, None);
    let annotations = view.text_annotations(doc, None);
    let row_of = |pos| {
        visual_offset_from_anchor(
            text,
            offset.anchor,
            pos,
            &text_fmt,
            &annotations,
            offset.vertical_offset + height,
        )
        .ok()
        .map(|(p, _)| p.row.saturating_sub(offset.vertical_offset))
    };
    let (Some(end_row), Some(cursor_row)) = (row_of(text.len_chars()), row_of(cursor)) else {
        return;
    };
    let shift = (end_row + 1 + rows).saturating_sub(height).min(cursor_row);
    if shift == 0 {
        return;
    }
    let (anchor, vertical_offset) = char_idx_at_visual_offset(
        text,
        offset.anchor,
        (offset.vertical_offset + shift) as isize,
        0,
        &text_fmt,
        &annotations,
    );
    drop(annotations);
    doc_mut!(editor, &view.doc).set_view_offset(
        view_id,
        ViewPosition {
            anchor,
            vertical_offset,
            ..offset
        },
    );
}

/// Hooks this module needs (registered when the bridge starts).
pub fn register_hooks() {
    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        if USED.load(Ordering::Relaxed) {
            show_last_line_rows(event.cx.editor);
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let doc = &mut *event.doc;
        if !doc.bridge_annotations.items().is_empty() {
            let text = doc.text().clone();
            doc.bridge_annotations.map(event.changes, &text);
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let mut pending = PENDING.lock();
        let Some(files) = pending.as_mut().filter(|f| !f.is_empty()) else {
            return Ok(());
        };
        let Some(doc) = event.editor.document_mut(event.doc) else {
            return Ok(());
        };
        let Some(list) = doc.path().and_then(|p| files.remove(p)) else {
            return Ok(());
        };
        for (ns, items) in list {
            set_on_doc(doc, &ns, &items);
        }
        Ok(())
    });
}

// --- drawing -------------------------------------------------------------------

/// The gutter sign of a marked line when the item has none of its own.
const SIGN: &str = "┃";
/// In front of each row of an annotation's text.
const BAR: &str = "▎";
const ELLIPSIS: &str = "…";

/// How strongly an accent tints a background, out of 1.
const BAND_TINT: f32 = 0.13;
const LINE_TINT: f32 = 0.09;

pub(crate) fn blend(fg: Option<Color>, bg: Option<Color>, amount: f32) -> Option<Color> {
    match (fg?, bg?) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |a: u8, b: u8| (a as f32 * amount + b as f32 * (1.0 - amount)).round() as u8;
            Some(Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2)))
        }
        _ => None,
    }
}

/// Styles for one kind (or severity) of annotation.
#[derive(Debug, Clone, Copy)]
struct Look {
    /// Bar, marker and gutter sign.
    accent: Style,
    text: Style,
    /// Background of the rows under the code.
    band: Style,
    /// Background of the marked lines with `highlight`.
    line: Style,
}

struct Looks {
    comment: Look,
    reply: Look,
    note: Look,
    severities: [Look; 4],
    removed: Style,
    removed_band: Style,
}

impl Looks {
    fn new(theme: &Theme) -> Looks {
        let bg = theme.get("ui.background").bg;
        let text = theme.get("ui.text");
        let look = |accent: Style, text: Style| {
            let accent_fg = accent.fg.or(text.fg);
            let band = blend(accent_fg, bg, BAND_TINT)
                .map(|c| Style::default().bg(c))
                .unwrap_or_default();
            let line = blend(accent_fg, bg, LINE_TINT)
                .map(|c| Style::default().bg(c))
                .or_else(|| theme.try_get("ui.cursorline.secondary"))
                .unwrap_or_default();
            Look {
                accent: Style {
                    fg: accent_fg,
                    ..Style::default()
                },
                text,
                band,
                line,
            }
        };
        let fg = |scope: &str| Style {
            fg: theme.get(scope).fg,
            ..Style::default()
        };
        let comment_text = theme.get("comment");
        let strike = theme
            .try_get("markup.strikethrough")
            .is_some_and(|s| s.add_modifier.contains(Modifier::CROSSED_OUT));
        let minus = fg("diff.minus");
        let removed = if strike {
            minus.add_modifier(Modifier::CROSSED_OUT)
        } else {
            minus.add_modifier(Modifier::DIM)
        };
        Looks {
            comment: look(fg("function"), text),
            reply: look(fg("string"), text),
            note: look(fg("keyword"), comment_text),
            severities: [
                look(fg("hint"), text),
                look(fg("info"), text),
                look(fg("warning"), text),
                look(fg("error"), text),
            ],
            removed,
            removed_band: blend(minus.fg, bg, BAND_TINT)
                .map(|c| Style::default().bg(c))
                .unwrap_or_default(),
        }
    }

    fn of(&self, item: &Item) -> Look {
        let mut look = match item.kind {
            Kind::Comment => self.comment,
            Kind::Reply => self.reply,
            Kind::Note => self.note,
        };
        if let Some(severity) = item.severity {
            let by_severity = self.severities[match severity {
                Severity::Hint => 0,
                Severity::Info => 1,
                Severity::Warning => 2,
                Severity::Error => 3,
            }];
            look.accent = by_severity.accent;
            look.band = by_severity.band;
            look.line = by_severity.line;
        }
        look
    }
}

/// Column (absolute x) for gutter signs: the spacer right of the
/// diagnostics column in the default layout, else the diagnostics column,
/// else any spacer. `None` when the view shows no gutter.
fn sign_column(view: &View, doc: &Document) -> Option<u16> {
    if view.gutter_offset(doc) == 0 {
        return None;
    }
    let mut x = view.area.x;
    let mut columns = Vec::new();
    for gutter in view.gutters() {
        columns.push((*gutter, x));
        x += gutter.width(view, doc) as u16;
    }
    let diagnostics = columns
        .iter()
        .position(|(g, _)| *g == GutterType::Diagnostics);
    if let Some(i) = diagnostics {
        if let Some((GutterType::Spacer, x)) = columns.get(i + 1) {
            return Some(*x);
        }
        return Some(columns[i].1);
    }
    columns
        .iter()
        .find(|(g, _)| *g == GutterType::Spacer)
        .map(|(_, x)| *x)
}

/// A marked line range on screen, for gutter signs and line backgrounds.
struct Mark<'a> {
    lines: std::ops::RangeInclusive<usize>,
    look: Look,
    sign: Option<&'a str>,
    highlight: bool,
}

struct Annotations<'a> {
    text: RopeSlice<'a>,
    items: &'a [Item],
    removed: &'a [RemovedLine],
    /// Next item and removed line to look for.
    item: usize,
    gone: usize,
    /// The line whose line ending was just passed, with its items and
    /// removed lines, drawn when its visual line ends.
    pending: Option<(Range<usize>, Range<usize>)>,
    marks: Vec<Mark<'a>>,
    looks: Looks,
    width: usize,
    tab_width: usize,
    horizontal_offset: usize,
    sign_x: Option<u16>,
}

impl<'a> Annotations<'a> {
    fn new(doc: &'a Document, view: &View, theme: &Theme, removed: &'a [RemovedLine]) -> Self {
        let text = doc.text().slice(..);
        let items = if shown() {
            doc.bridge_annotations.items()
        } else {
            &[]
        };
        let looks = Looks::new(theme);
        let first = text.char_to_line(doc.view_offset(view.id).anchor.min(text.len_chars()));
        let last = view.estimate_last_doc_line(doc);
        let start = items.partition_point(|i| i.last_line < first);
        let marks = items[start..]
            .iter()
            .filter(|i| i.first_line <= last)
            .map(|i| Mark {
                lines: i.first_line..=i.last_line,
                look: looks.of(i),
                sign: i.sign.as_deref(),
                highlight: i.highlight,
            })
            .collect();
        Annotations {
            text,
            items,
            removed,
            item: 0,
            gone: 0,
            pending: None,
            marks,
            looks,
            width: view.inner_width(doc) as usize,
            tab_width: doc.tab_width(),
            horizontal_offset: doc.view_offset(view.id).horizontal_offset,
            sign_x: sign_column(view, doc),
        }
    }

    /// Char index of the next line ending to stop at.
    fn next_anchor(&self) -> usize {
        let item = self.items.get(self.item).map(|i| i.last_line);
        let gone = self.removed.get(self.gone).map(RemovedLine::anchor);
        match item.into_iter().chain(gone).min() {
            Some(line) => line_end_char_index(&self.text, line),
            None => usize::MAX,
        }
    }

    fn row_visible(renderer: &TextRenderer, row: usize) -> bool {
        row >= renderer.offset.row && row - renderer.offset.row < renderer.viewport.height as usize
    }

    fn draw_item(&self, renderer: &mut TextRenderer, item: &Item, first_row: usize) -> usize {
        let Some(place) = place(
            code_indent(self.text, item, self.tab_width),
            self.width,
            item.kind,
        ) else {
            return 0;
        };
        let (rows, cut) = wrap(&item.text, place.text_width);
        let look = self.looks.of(item);
        let marker = data::marker(item.kind);
        let viewport = renderer.viewport;
        let x0 = viewport.x + place.indent.saturating_sub(self.horizontal_offset) as u16;
        let text_x = x0 + (BAR_WIDTH + marker.width()) as u16;
        let last = rows.len() - 1;
        for (i, range) in rows.iter().enumerate() {
            let row = first_row + i;
            if !Self::row_visible(renderer, row) {
                continue;
            }
            let y = row as u16;
            let band = Rect::new(x0, y, viewport.right().saturating_sub(x0), 1);
            renderer.set_style(band, look.band);
            renderer.set_string(x0, y, BAR, look.accent.patch(look.band));
            if i == 0 && !marker.is_empty() {
                renderer.set_string(
                    x0 + BAR_WIDTH as u16,
                    y,
                    marker,
                    look.accent.patch(look.band),
                );
            }
            let style = look.text.patch(look.band);
            let row_text = &item.text[range.clone()];
            renderer.set_stringn(text_x, y, row_text, place.text_width, style);
            if cut && i == last {
                let end = text_x + row_text.width().min(place.text_width.saturating_sub(1)) as u16;
                renderer.set_string(end, y, ELLIPSIS, style);
            }
        }
        rows.len()
    }

    fn draw_removed(&self, renderer: &mut TextRenderer, removed: &RemovedLine, row: usize) {
        if !Self::row_visible(renderer, row) {
            return;
        }
        let viewport = renderer.viewport;
        let y = row as u16;
        let band = self.looks.removed_band;
        renderer.set_style(Rect::new(viewport.x, y, viewport.width, 1), band);
        let mut line = String::with_capacity(removed.text.len());
        for ch in removed.text.chars() {
            match ch {
                '\t' => {
                    let col = line.width();
                    let tab = self.tab_width.max(1);
                    line.extend(std::iter::repeat_n(' ', tab - col % tab));
                }
                ch if ch.is_control() => {}
                ch => line.push(ch),
            }
        }
        let mut start = 0;
        let mut skip = self.horizontal_offset;
        for (i, g) in line.grapheme_indices(true) {
            if skip == 0 {
                break;
            }
            skip = skip.saturating_sub(g.width());
            start = i + g.len();
        }
        let style = self.looks.removed.patch(band);
        renderer.set_stringn(
            viewport.x,
            y,
            &line[start..],
            viewport.width as usize,
            style,
        );
    }
}

impl Decoration for Annotations<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        let line = self.text.char_to_line(pos.min(self.text.len_chars()));
        self.item = self.items.partition_point(|i| i.last_line < line);
        self.gone = self.removed.partition_point(|r| r.anchor() < line);
        self.pending = None;
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        let item = self.items.get(self.item).map(|i| i.last_line);
        let gone = self.removed.get(self.gone).map(RemovedLine::anchor);
        let Some(line) = item.into_iter().chain(gone).min() else {
            return usize::MAX;
        };
        let items_start = self.item;
        while self
            .items
            .get(self.item)
            .is_some_and(|i| i.last_line == line)
        {
            self.item += 1;
        }
        let gone_start = self.gone;
        while self
            .removed
            .get(self.gone)
            .is_some_and(|r| r.anchor() == line)
        {
            self.gone += 1;
        }
        self.pending = Some((items_start..self.item, gone_start..self.gone));
        self.next_anchor()
    }

    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        let mut signed = false;
        for mark in &self.marks {
            if !mark.lines.contains(&pos.doc_line) {
                continue;
            }
            if mark.highlight {
                let viewport = renderer.viewport;
                renderer.set_style(
                    Rect::new(viewport.x, pos.visual_line, viewport.width, 1),
                    mark.look.line,
                );
            }
            if signed {
                continue;
            }
            signed = true;
            if let Some(x) = self.sign_x {
                let sign = match mark.sign {
                    Some(sign) if pos.first_visual_line && pos.doc_line == *mark.lines.start() => {
                        sign
                    }
                    _ => SIGN,
                };
                renderer.set_string(x, pos.visual_line, sign, mark.look.accent);
            }
        }
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some((items, removed)) = self.pending.take() else {
            return Position::new(0, 0);
        };
        let mut row = pos.visual_line as usize + virt_off.row;
        let start = row;
        for item in &self.items[items] {
            row += self.draw_item(renderer, item, row);
        }
        for removed in &self.removed[removed] {
            self.draw_removed(renderer, removed, row);
            row += 1;
        }
        Position::new(row - start, 0)
    }
}

/// Called from `EditorView::render_view` just before the document is drawn,
/// after every other decoration, so the virtual lines stack in the same
/// order as `View::text_annotations` reserved them.
pub fn decorate<'d>(
    doc: &'d Document,
    view: &View,
    theme: &Theme,
    decorations: &mut DecorationManager<'d>,
) {
    #[cfg(feature = "integration")]
    crate::bridge::count_view_drawn();
    // the inline diff's line backgrounds go under everything; its removed
    // lines come after the annotations' rows, as `View::text_annotations`
    // reserved them
    let diff = super::review::decorate_lines(doc, theme, decorations);
    let this = &doc.bridge_annotations;
    if !this.is_empty() {
        let removed = this.reveal.as_ref().map_or(&[][..], |r| &r.removed[..]);
        if (shown() && !this.items().is_empty()) || !removed.is_empty() {
            decorations.add_decoration(Annotations::new(doc, view, theme, removed));
        }
    }
    if let Some(diff) = diff {
        super::review::decorate_removed(doc, view, theme, diff, decorations);
    }
    if let Some(reveal) = &this.reveal {
        super::reveal::decorate(reveal, doc.text().slice(..), theme, decorations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_text() {
        assert_eq!(clean_text("a\tb\r\nc\u{7}\n\n"), "a    b\nc");
    }

    #[test]
    fn blends_rgb_only() {
        let a = Some(Color::Rgb(200, 100, 0));
        let b = Some(Color::Rgb(0, 100, 200));
        assert_eq!(blend(a, b, 0.5), Some(Color::Rgb(100, 100, 100)));
        assert_eq!(blend(a, Some(Color::Red), 0.5), None);
        assert_eq!(blend(None, b, 0.5), None);
    }

    #[test]
    fn rejects_bad_items() {
        let item = |text: &str, sign: Option<&str>| ItemParams {
            line: 1,
            end_line: None,
            text: text.into(),
            kind: KindParam::Comment,
            severity: None,
            sign: sign.map(Into::into),
            highlight: false,
        };
        assert!(validate(&mut [item("ok", Some("●"))]).is_ok());
        assert!(validate(&mut [item("ok", Some("ab"))]).is_err());
        assert!(validate(&mut [item(&"x".repeat(MAX_CHARS + 1), None)]).is_err());
        let mut many = vec![item("x", None); MAX_ITEMS + 1];
        assert!(validate(&mut many).is_err());
    }
}
