//! editor/reveal and the reveal animation (step 2d).
//!
//! The document always holds the final text. The animation is decoration
//! only: graphemes not uncovered yet are overlaid (with themselves drawn in
//! the background colour, so layout and soft wrap never move), what was just
//! uncovered gets a background that fades, and removed lines show struck
//! through above where they were. A timer advances it, one frame per tick,
//! only while something is animating. Any key press, or an edit to the
//! document, ends it at once.
//!
//! [`play`] is the entry point; `editor/reveal` and `editor/reload` (with
//! `reveal`) call it with char ranges in the current text.

use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use helix_core::chars::{char_is_line_ending, char_is_whitespace};
use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::graphemes::{
    ensure_grapheme_boundary_next, ensure_grapheme_boundary_prev, grapheme_width, Grapheme,
};
use helix_core::syntax::Highlight;
use helix_core::text_annotations::Overlay;
use helix_core::{RopeSlice, Selection, Tendril};
use helix_event::{register_hook, runtime_local};
use helix_stdx::rope::RopeSliceExt;
use helix_view::annotations::bridge::{RemovedLine, Reveal};
use helix_view::editor::Action;
use helix_view::events::DocumentDidChange;
use helix_view::graphics::{Color, Rect, Style};
use helix_view::{align_view, Align, DocumentId, Editor, Theme};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::annotations::blend;
use crate::bridge::convert::{char_from_pos, resolve_path};
use crate::bridge::dispatch::on_editor;
use crate::bridge::methods::interrupt::{self, Interrupt};
use crate::bridge::protocol::{parse_params, Range as ProtoRange, RpcError, RpcResult};
use crate::bridge::server::Ctx;
use crate::compositor::{Component, Compositor, Context, Event, EventResult};
use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::{Decoration, DecorationManager};

pub const CAPABILITY: Option<&str> = Some("reveal");

/// Time between frames while an animation runs.
const TICK: Duration = Duration::from_millis(16);
/// `durationMs` bounds and default.
const MIN_DURATION: u64 = 50;
const MAX_DURATION: u64 = 3000;
pub const DEFAULT_DURATION: Duration = Duration::from_millis(600);
/// Removed lines one request may show.
const MAX_REMOVED: usize = 1000;
/// Drawn over a grapheme not uncovered yet when the theme's background is
/// not an RGB colour (Braille blank: one column, not whitespace).
const BLANK: &str = "\u{2800}";
/// How strongly the just-uncovered highlight tints the background.
const FRESH_TINT: f32 = 0.28;

/// How long the highlight of an uncovered grapheme takes to fade.
fn fade_for(duration: Duration) -> Duration {
    (duration / 2).clamp(Duration::from_millis(120), Duration::from_millis(400))
}

/// One grapheme of the revealed text, in reveal order.
struct Step {
    char_idx: usize,
    chars: usize,
    /// What the overlay draws until it is uncovered; `None` for whitespace
    /// and line endings, which are never covered.
    cover: Option<Tendril>,
    /// Line endings and indentation: uncovered without a highlight.
    quiet: bool,
}

struct Anim {
    doc: DocumentId,
    start: Instant,
    duration: Duration,
    fade: Duration,
    steps: Vec<Step>,
    highlight: Option<Highlight>,
}

#[derive(Default)]
struct State {
    anims: Vec<Anim>,
    /// A timer task is running (it clears this when it stops, on the main
    /// thread, so `play` never races it).
    running: bool,
}

runtime_local! {
    static STATE: Mutex<State> = Mutex::new(State { anims: Vec::new(), running: false });
}

/// Show `ranges` (char ranges in the current text of `doc`) as if typed,
/// and `removed` struck through above their lines, over `duration` (clamped
/// to 50..3000 ms). With `follow`, the focused view first moves to the first
/// change. Only what is on screen in some view of the document animates.
/// Returns how many ranges animate. Does nothing when
/// `[editor.bridge] reveal` is off.
pub fn play(
    editor: &mut Editor,
    doc: DocumentId,
    ranges: &[Range<usize>],
    removed: Vec<RemovedLine>,
    duration: Duration,
    follow: bool,
) -> usize {
    if !editor.config().bridge.reveal || editor.document(doc).is_none() {
        return 0;
    }
    let duration = duration.clamp(
        Duration::from_millis(MIN_DURATION),
        Duration::from_millis(MAX_DURATION),
    );
    if follow {
        follow_first(editor, doc, ranges, &removed);
    }

    let theme_bg = editor.theme.get("ui.background").bg;
    let highlight = match theme_bg {
        Some(Color::Rgb(r, g, b)) => Some(Theme::rgb_highlight(r, g, b)),
        _ => None,
    };
    let windows = visible_lines(editor, doc);
    let document = doc!(editor, &doc);
    let text = document.text().slice(..);
    let visible_chars: Vec<Range<usize>> = windows
        .iter()
        .map(|lines| text.line_to_char(lines.start)..text.line_to_char(lines.end))
        .collect();

    let mut steps = Vec::new();
    let mut seen = HashSet::new();
    let mut animated = 0;
    for range in ranges {
        let len = text.len_chars();
        let (from, to) = (range.start.min(len), range.end.min(len));
        let mut any = false;
        for window in &visible_chars {
            let start = ensure_grapheme_boundary_prev(text, from.max(window.start));
            let end = ensure_grapheme_boundary_next(text, to.min(window.end));
            if start >= end {
                continue;
            }
            let mut char_idx = start;
            let line_start = text.line_to_char(text.char_to_line(start));
            let mut in_indent = text
                .slice(line_start..start)
                .chars()
                .all(char_is_whitespace);
            for g in text.slice(start..end).graphemes() {
                let chars = g.len_chars();
                if seen.insert(char_idx) {
                    any = true;
                    let first = g.chars().next().unwrap_or(' ');
                    let s: Tendril = Tendril::from(std::borrow::Cow::<str>::from(g).as_ref());
                    let width = grapheme_width(&s);
                    let line_ending = helix_core::chars::char_is_line_ending(first);
                    let blank = line_ending || char_is_whitespace(first);
                    let cover = if blank {
                        None
                    } else if highlight.is_some() {
                        Some(s)
                    } else if width == 1 {
                        Some(BLANK.into())
                    } else {
                        None
                    };
                    steps.push(Step {
                        char_idx,
                        chars,
                        cover,
                        quiet: line_ending || (blank && in_indent),
                    });
                }
                in_indent = helix_core::chars::char_is_line_ending(g.chars().next().unwrap_or(' '))
                    || (in_indent && g.chars().all(char_is_whitespace));
                char_idx += chars;
            }
        }
        animated += any as usize;
    }
    let mut removed: Vec<RemovedLine> = removed
        .into_iter()
        .filter(|r| windows.iter().any(|w| w.contains(&r.anchor())))
        .take(MAX_REMOVED)
        .collect();
    removed.sort_by_key(|r| r.line);
    if steps.is_empty() && removed.is_empty() {
        return 0;
    }

    let anim = Anim {
        doc,
        start: Instant::now(),
        duration,
        fade: fade_for(duration),
        steps,
        highlight,
    };
    let mut reveal = Reveal {
        removed,
        ..Reveal::default()
    };
    frame(&anim, Duration::ZERO, &mut reveal);
    doc_mut!(editor, &doc).bridge_annotations.reveal = Some(reveal);

    let mut state = STATE.lock();
    state.anims.retain(|a| a.doc != doc);
    state.anims.push(anim);
    if !state.running {
        state.running = true;
        tokio::spawn(timer());
    }
    animated
}

/// Move the focused view to the first change, at once (callers go through
/// `interrupt::jump` first).
fn follow_first(
    editor: &mut Editor,
    doc: DocumentId,
    ranges: &[Range<usize>],
    removed: &[RemovedLine],
) {
    let text = doc!(editor, &doc).text().slice(..);
    let target = match (ranges.first(), removed.iter().map(|r| r.line).min()) {
        (Some(range), _) => range.start.min(text.len_chars()),
        (None, Some(line)) => text.line_to_char(line.min(text.len_lines() - 1)),
        (None, None) => return,
    };
    if view!(editor).doc != doc {
        editor.switch(doc, Action::Replace);
    }
    let (view, document) = current!(editor);
    document.set_selection(view.id, Selection::point(target));
    align_view(document, view, Align::Center);
}

/// The document lines (0-based, end exclusive) on screen in each view of
/// `doc`.
fn visible_lines(editor: &Editor, doc: DocumentId) -> Vec<Range<usize>> {
    let document = doc!(editor, &doc);
    let text = document.text().slice(..);
    editor
        .tree
        .views()
        .filter(|(view, _)| view.doc == doc)
        .map(|(view, _)| {
            let anchor = document.view_offset(view.id).anchor.min(text.len_chars());
            let first = text.char_to_line(anchor);
            first..view.estimate_last_doc_line(document) + 1
        })
        .collect()
}

/// Fill `reveal` with the frame `elapsed` after the start.
fn frame(anim: &Anim, elapsed: Duration, reveal: &mut Reveal) {
    let total = anim.steps.len();
    let shown = if elapsed >= anim.duration {
        total
    } else {
        ((total as u128 * elapsed.as_micros()).div_ceil(anim.duration.as_micros().max(1))) as usize
    };
    reveal.highlight = anim.highlight;
    reveal.overlays.clear();
    reveal
        .overlays
        .extend(anim.steps[shown..].iter().filter_map(|step| {
            let cover = step.cover.as_ref()?;
            Some(Overlay::new(step.char_idx, cover.clone()))
        }));
    reveal.overlays.sort_by_key(|o| o.char_idx);

    reveal.fresh.clear();
    let fade = anim.fade.as_secs_f32();
    for (i, step) in anim.steps[..shown].iter().enumerate().rev() {
        let at = anim.duration.as_secs_f32() * i as f32 / total as f32;
        let age = elapsed.as_secs_f32() - at;
        if age >= fade {
            break;
        }
        let strength = (255.0 * (1.0 - age.max(0.0) / fade)).round() as u8;
        if !step.quiet {
            reveal
                .fresh
                .push((step.char_idx..step.char_idx + step.chars, strength));
        }
    }
    reveal.fresh.sort_by_key(|(range, _)| range.start);
}

/// Advance every animation by one frame. Returns whether any is left; when
/// none is, the timer stops.
fn tick(editor: &mut Editor, compositor: &mut Compositor) -> bool {
    let now = Instant::now();
    let mut state = STATE.lock();
    state.anims.retain(|anim| {
        let Some(doc) = editor.document_mut(anim.doc) else {
            return false;
        };
        // Cleared by a key press or an edit.
        let Some(reveal) = doc.bridge_annotations.reveal.as_mut() else {
            return false;
        };
        let elapsed = now.saturating_duration_since(anim.start);
        if elapsed >= anim.duration + anim.fade {
            doc.bridge_annotations.reveal = None;
            return false;
        }
        frame(anim, elapsed, reveal);
        true
    });
    let more = !state.anims.is_empty();
    if more {
        if compositor.find_id::<EndOnKey>(END_ON_KEY).is_none() {
            compositor.push(Box::new(EndOnKey));
        }
    } else {
        state.running = false;
        compositor.remove(END_ON_KEY);
    }
    more
}

async fn timer() {
    loop {
        let (tx, rx) = oneshot::channel();
        crate::job::dispatch(move |editor, compositor| {
            let _ = tx.send(tick(editor, compositor));
        })
        .await;
        match rx.await {
            Ok(true) => tokio::time::sleep(TICK).await,
            Ok(false) => return,
            Err(_) => break,
        }
    }
    // The editor dropped the frame (it is closing).
    STATE.lock().running = false;
}

/// End every animation now, showing the final text.
fn stop_all(editor: &mut Editor) {
    let mut state = STATE.lock();
    for anim in state.anims.drain(..) {
        if let Some(doc) = editor.document_mut(anim.doc) {
            doc.bridge_annotations.reveal = None;
        }
    }
}

const END_ON_KEY: &str = "bb-reveal";

/// An invisible layer on top while an animation runs: the first key press
/// (or paste) ends every animation and goes on to the editor as usual.
struct EndOnKey;

impl Component for EndOnKey {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        match event {
            Event::Key(_) | Event::Paste(_) => {
                stop_all(cx.editor);
                EventResult::Ignored(Some(Box::new(|compositor: &mut Compositor, _| {
                    compositor.remove(END_ON_KEY);
                })))
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn render(&mut self, _area: Rect, _surface: &mut tui::buffer::Buffer, _cx: &mut Context) {}

    fn id(&self) -> Option<&'static str> {
        Some(END_ON_KEY)
    }
}

/// Hooks this module needs (registered when the bridge starts).
pub fn register_hooks() {
    // An edit moves text under the overlays: end that document's animation.
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.doc.bridge_annotations.reveal.is_some() {
            event.doc.bridge_annotations.reveal = None;
        }
        Ok(())
    });
}

// --- drawing -------------------------------------------------------------------

/// The fading background of what was just uncovered.
struct Fresh<'a> {
    fresh: &'a [(Range<usize>, u8)],
    idx: usize,
    tint: Option<Color>,
    bg: Option<Color>,
    fallback: Style,
}

impl Fresh<'_> {
    fn next_anchor(&mut self, from: usize) -> usize {
        while self.fresh.get(self.idx).is_some_and(|(r, _)| r.end <= from) {
            self.idx += 1;
        }
        match self.fresh.get(self.idx) {
            Some((range, _)) => range.start.max(from),
            None => usize::MAX,
        }
    }

    fn style(&self, strength: u8) -> Style {
        let amount = FRESH_TINT * strength as f32 / 255.0;
        match blend(self.tint, self.bg, amount) {
            Some(color) => Style::default().bg(color),
            None if strength > 128 => self.fallback,
            None => Style::default(),
        }
    }
}

impl Decoration for Fresh<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.idx = self.fresh.partition_point(|(r, _)| r.end <= pos);
        self.next_anchor(pos)
    }

    fn skip_concealed_anchor(&mut self, conceal_end_char_idx: usize) -> usize {
        self.next_anchor(conceal_end_char_idx)
    }

    fn decorate_grapheme(
        &mut self,
        renderer: &mut TextRenderer,
        grapheme: &FormattedGrapheme,
    ) -> usize {
        let next = grapheme.char_idx + grapheme.doc_chars().max(1);
        let Some(&(ref range, strength)) = self.fresh.get(self.idx) else {
            return usize::MAX;
        };
        let visible_col = grapheme.visual_pos.col;
        if range.contains(&grapheme.char_idx)
            && !matches!(grapheme.raw, Grapheme::Newline)
            && !grapheme.source.is_eof()
            && renderer.column_in_bounds(visible_col, grapheme.width())
        {
            let viewport = renderer.viewport;
            let x = viewport.x + (visible_col - renderer.offset.col) as u16;
            let area = Rect::new(
                x,
                grapheme.visual_pos.row as u16,
                grapheme.width() as u16,
                1,
            );
            renderer.set_style(area, self.style(strength));
        }
        self.next_anchor(next)
    }
}

/// Helix draws indent guides and soft-wrap indicators on every visual line,
/// and they are not part of the text the overlays cover, so on a line still
/// covered they would show before its text. After each visual line whose
/// text is all still covered, this blanks the row (a plain space, no style,
/// so cursor, selection and cursorline backgrounds stay).
struct Hide<'a> {
    text: RopeSlice<'a>,
    /// Sorted by `char_idx`.
    overlays: &'a [Overlay],
    /// The lines the overlays are on.
    range: Range<usize>,
    row: Option<u16>,
    covered: bool,
    shown: bool,
}

impl Hide<'_> {
    fn anchor(&self, pos: usize) -> usize {
        if pos >= self.range.end {
            usize::MAX
        } else {
            pos.max(self.range.start)
        }
    }
}

impl Decoration for Hide<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.anchor(pos)
    }

    fn skip_concealed_anchor(&mut self, conceal_end_char_idx: usize) -> usize {
        self.anchor(conceal_end_char_idx)
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        grapheme: &FormattedGrapheme,
    ) -> usize {
        let row = grapheme.visual_pos.row as u16;
        if self.row != Some(row) {
            self.row = Some(row);
            self.covered = false;
            self.shown = false;
        }
        // A virtual grapheme (a wrap indicator, an inlay hint) carries the
        // index of the document grapheme after it, which the next call skips:
        // judge that one now. It is on the same row.
        let at = grapheme.char_idx;
        if at < self.text.len_chars() {
            if self
                .overlays
                .binary_search_by_key(&at, |o| o.char_idx)
                .is_ok()
            {
                self.covered = true;
            } else {
                let ch = self.text.char(at);
                self.shown |= !(char_is_whitespace(ch) || char_is_line_ending(ch));
            }
        }
        self.anchor(at + grapheme.doc_chars().max(1))
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        _virt_off: helix_core::Position,
    ) -> helix_core::Position {
        if self.row == Some(pos.visual_line) && self.covered && !self.shown {
            let viewport = renderer.viewport;
            let blank = " ".repeat(viewport.width as usize);
            renderer.set_string(viewport.x, pos.visual_line, &blank, Style::default());
        }
        helix_core::Position::new(0, 0)
    }
}

pub(super) fn decorate<'d>(
    reveal: &'d Reveal,
    text: RopeSlice<'d>,
    theme: &Theme,
    decorations: &mut DecorationManager<'d>,
) {
    if let (Some(first), Some(last)) = (reveal.overlays.first(), reveal.overlays.last()) {
        let len = text.len_chars();
        let start = text.line_to_char(text.char_to_line(first.char_idx.min(len)));
        let last_line = text.char_to_line(last.char_idx.min(len));
        let end = if last_line + 1 < text.len_lines() {
            text.line_to_char(last_line + 1)
        } else {
            len
        };
        decorations.add_decoration(Hide {
            text,
            overlays: &reveal.overlays,
            range: start..end,
            row: None,
            covered: false,
            shown: false,
        });
    }
    if reveal.fresh.is_empty() {
        return;
    }
    decorations.add_decoration(Fresh {
        fresh: &reveal.fresh,
        idx: 0,
        tint: theme.get("diff.plus").fg,
        bg: theme.get("ui.background").bg,
        fallback: theme.get("ui.selection"),
    });
}

// --- editor/reveal -------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevealParams {
    path: String,
    ranges: Vec<ProtoRange>,
    #[serde(default)]
    removed: Vec<RemovedParams>,
    duration_ms: Option<u64>,
    #[serde(default)]
    follow: bool,
    interrupt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RemovedParams {
    line: usize,
    text: String,
}

pub fn reveal(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: RevealParams = parse_params(params)?;
        let interrupt = Interrupt::parse(params.interrupt.as_deref())?;
        let duration = params
            .duration_ms
            .unwrap_or(DEFAULT_DURATION.as_millis() as u64);
        if !(MIN_DURATION..=MAX_DURATION).contains(&duration) {
            return Err(RpcError::invalid_params(
                "durationMs",
                format!("durationMs must be {MIN_DURATION} to {MAX_DURATION}"),
            ));
        }
        let path = resolve_path(&params.path, &cx.shared.info.workspace_root);
        let on_disk = tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file());
        let duration = Duration::from_millis(duration);
        let (animated, deferred) = on_editor(move |editor, compositor| {
            if !params.follow {
                return reveal_on_main(editor, &path, on_disk, params, duration, false)
                    .map(|n| (n, false));
            }
            if !on_disk && editor.document_id_by_path(&path).is_none() {
                return Err(RpcError::not_found(format!("{} not found", path.display())));
            }
            let line = params
                .ranges
                .iter()
                .map(|r| r.start.line)
                .chain(params.removed.iter().map(|r| r.line))
                .min();
            let what = format!("show {}", interrupt::location(&path, line));
            let later = (path.clone(), params.clone());
            let moved = interrupt::jump(editor, compositor, interrupt, what, move |editor, _| {
                let (path, params) = later;
                reveal_on_main(editor, &path, on_disk, params, duration, true)
            })?;
            match moved {
                Some(animated) => Ok((animated, false)),
                // `:bb-go` moves there and plays it again; what is on
                // screen now animates without moving
                None => reveal_on_main(editor, &path, on_disk, params, duration, false)
                    .map(|n| (n, true)),
            }
        })
        .await??;
        Ok(json!({ "animated": animated, "deferred": deferred }))
    })
}

fn reveal_on_main(
    editor: &mut Editor,
    path: &Path,
    on_disk: bool,
    params: RevealParams,
    duration: Duration,
    follow: bool,
) -> Result<usize, RpcError> {
    let doc = match editor.document_id_by_path(path) {
        Some(doc) => doc,
        None if !on_disk => {
            return Err(RpcError::not_found(format!("{} not found", path.display())))
        }
        // Not open: nothing of it is on screen unless `follow` shows it.
        None if !follow => return Ok(0),
        None => editor
            .open(path, Action::Replace)
            .map_err(|err| RpcError::not_found(format!("{}: {err}", path.display())))?,
    };
    let text = doc!(editor, &doc).text().slice(..);
    let ranges: Vec<Range<usize>> = params
        .ranges
        .iter()
        .map(|r| {
            let (a, b) = (char_from_pos(text, r.start), char_from_pos(text, r.end));
            a.min(b)..a.max(b)
        })
        .collect();
    let removed = params
        .removed
        .into_iter()
        .flat_map(|r| {
            let line = r.line.max(1) - 1;
            r.text
                .lines()
                .map(|text| RemovedLine {
                    line,
                    text: text.to_owned(),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    Ok(play(editor, doc, &ranges, removed, duration, follow))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anim(n: usize) -> Anim {
        Anim {
            doc: DocumentId::default(),
            start: Instant::now(),
            duration: Duration::from_millis(1000),
            fade: Duration::from_millis(250),
            steps: (0..n)
                .map(|i| Step {
                    char_idx: i,
                    chars: 1,
                    cover: Some("x".into()),
                    quiet: false,
                })
                .collect(),
            highlight: None,
        }
    }

    #[test]
    fn frames_uncover_in_order() {
        let anim = anim(10);
        let mut reveal = Reveal::default();
        frame(&anim, Duration::ZERO, &mut reveal);
        assert_eq!(reveal.overlays.len(), 10);
        assert!(reveal.fresh.is_empty());

        frame(&anim, Duration::from_millis(500), &mut reveal);
        assert_eq!(reveal.overlays.len(), 5);
        assert_eq!(reveal.overlays[0].char_idx, 5);
        // uncovered in the last 250 ms: 4 just now, 3 a bit earlier
        let fresh: Vec<_> = reveal.fresh.iter().map(|(r, _)| r.start).collect();
        assert_eq!(fresh, [3, 4]);
        assert!(reveal.fresh[1].1 > reveal.fresh[0].1);

        frame(&anim, Duration::from_millis(1100), &mut reveal);
        assert!(reveal.overlays.is_empty());
        assert!(reveal.fresh.iter().all(|(r, _)| r.start >= 9));
    }

    #[test]
    fn fade_scales_with_duration() {
        assert_eq!(
            fade_for(Duration::from_millis(50)),
            Duration::from_millis(120)
        );
        assert_eq!(
            fade_for(Duration::from_millis(600)),
            Duration::from_millis(300)
        );
        assert_eq!(
            fade_for(Duration::from_millis(3000)),
            Duration::from_millis(400)
        );
    }
}
