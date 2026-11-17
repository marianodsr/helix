//! Conversions between protocol positions/ranges and Helix char offsets,
//! document ids, paths and timestamps.
//!
//! Protocol positions are 1-based `{line, col}` with `col` counted in chars.
//! Helix selections are char-offset ranges that always cover at least one
//! grapheme; a range covering exactly one grapheme (or none) is what Helix
//! shows as a cursor, so it is reported as a bare cursor (`start == end`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use helix_core::{
    graphemes::next_grapheme_boundary, line_ending::line_end_char_index, Range as HxRange, Rope,
    RopeSlice, Selection,
};
use helix_view::DocumentId;

use super::protocol::{Position, Range, TextRange};

/// Char offset -> 1-based position.
pub fn pos_from_char(text: RopeSlice, char_idx: usize) -> Position {
    let char_idx = char_idx.min(text.len_chars());
    let line = text.char_to_line(char_idx);
    let col = char_idx - text.line_to_char(line);
    Position {
        line: line + 1,
        col: col + 1,
    }
}

/// 1-based position -> char offset, clamped into the document: lines past the
/// end go to the end of the text, columns past the end of a line go to the end
/// of that line (before its line ending).
pub fn char_from_pos(text: RopeSlice, pos: Position) -> usize {
    let len_lines = text.len_lines();
    let line = pos.line.max(1) - 1;
    if line >= len_lines {
        return text.len_chars();
    }
    let start = text.line_to_char(line);
    let end = line_end_char_index(&text, line);
    start.saturating_add(pos.col.max(1) - 1).min(end)
}

/// True when Helix would draw this range as a plain cursor.
pub fn is_cursor(text: RopeSlice, range: &HxRange) -> bool {
    range.from() == range.to() || next_grapheme_boundary(text, range.from()) >= range.to()
}

pub fn range_to_proto(text: RopeSlice, range: &HxRange) -> Range {
    if is_cursor(text, range) {
        let p = pos_from_char(text, range.cursor(text));
        Range { start: p, end: p }
    } else {
        Range {
            start: pos_from_char(text, range.from()),
            end: pos_from_char(text, range.to()),
        }
    }
}

pub fn selection_to_proto(text: RopeSlice, selection: &Selection) -> Vec<Range> {
    selection.iter().map(|r| range_to_proto(text, r)).collect()
}

/// Ranges with their text, sharing one `budget` of bytes. When every range is
/// a cursor, each `text` is the cursor's line without its line ending.
/// Returns the ranges and whether any text was cut.
pub fn selection_with_text(
    text: RopeSlice,
    selection: &Selection,
    budget: usize,
) -> (Vec<TextRange>, bool) {
    let all_cursors = selection.iter().all(|r| is_cursor(text, r));
    let mut left = budget;
    let mut truncated = false;
    let ranges = selection
        .iter()
        .map(|r| {
            let range = range_to_proto(text, r);
            let slice = if all_cursors {
                let line = text.char_to_line(r.cursor(text).min(text.len_chars()));
                text.slice(text.line_to_char(line)..line_end_char_index(&text, line))
            } else {
                text.slice(r.from()..r.to())
            };
            let (s, cut) = slice_to_string(slice, left);
            left -= s.len();
            truncated |= cut;
            TextRange {
                start: range.start,
                end: range.end,
                text: s,
            }
        })
        .collect();
    (ranges, truncated)
}

/// Copy at most `max_bytes` of the slice, cutting on a char boundary.
pub fn slice_to_string(slice: RopeSlice, max_bytes: usize) -> (String, bool) {
    if slice.len_bytes() <= max_bytes {
        return (slice.to_string(), false);
    }
    let mut out = String::with_capacity(max_bytes);
    for chunk in slice.chunks() {
        if out.len() + chunk.len() <= max_bytes {
            out.push_str(chunk);
            continue;
        }
        let mut end = max_bytes - out.len();
        while !chunk.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str(&chunk[..end]);
        break;
    }
    (out, true)
}

/// Text of a protocol range (or the whole text), limited to `max_bytes`.
pub fn text_in_range(text: &Rope, range: Option<Range>, max_bytes: usize) -> (String, bool) {
    let slice = text.slice(..);
    match range {
        None => slice_to_string(slice, max_bytes),
        Some(r) => {
            let start = char_from_pos(slice, r.start);
            let end = char_from_pos(slice, r.end).max(start);
            slice_to_string(slice.slice(start..end), max_bytes)
        }
    }
}

/// The number behind a `DocumentId`, without allocating.
pub fn doc_num(id: DocumentId) -> usize {
    struct Buf {
        bytes: [u8; 24],
        len: usize,
    }
    impl std::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let end = self.len + s.len();
            if end > self.bytes.len() {
                return Err(std::fmt::Error);
            }
            self.bytes[self.len..end].copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }
    let mut buf = Buf {
        bytes: [0; 24],
        len: 0,
    };
    let _ = write!(buf, "{id}");
    buf.bytes[..buf.len]
        .iter()
        .fold(0usize, |n, b| n * 10 + (b - b'0') as usize)
}

pub fn path_string(path: Option<&Path>) -> Option<String> {
    path.map(|p| p.to_string_lossy().into_owned())
}

/// Resolve a request path: absolute as given, relative against `root`.
pub fn resolve_path(path: &str, root: &Path) -> PathBuf {
    let p = Path::new(path);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    helix_stdx::path::canonicalize(p)
}

/// RFC 3339 UTC timestamp, seconds precision.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::{Range as HxRange, Rope};
    use smallvec::smallvec;
    use std::time::Duration;

    fn p(line: usize, col: usize) -> Position {
        Position { line, col }
    }

    #[test]
    fn positions_multibyte() {
        // 'é' is 2 bytes, '🦀' is 4 bytes; both are one char.
        let text = Rope::from("aé🦀b\nxyz");
        let s = text.slice(..);
        assert_eq!(pos_from_char(s, 0), p(1, 1));
        assert_eq!(pos_from_char(s, 3), p(1, 4)); // 'b'
        assert_eq!(pos_from_char(s, 5), p(2, 1)); // 'x'
        assert_eq!(char_from_pos(s, p(1, 4)), 3);
        assert_eq!(char_from_pos(s, p(2, 3)), 7);
    }

    #[test]
    fn positions_crlf() {
        let text = Rope::from("ab\r\ncd\r\n");
        let s = text.slice(..);
        assert_eq!(pos_from_char(s, 4), p(2, 1));
        // col past the end of line 1 clamps before "\r\n"
        assert_eq!(char_from_pos(s, p(1, 99)), 2);
        assert_eq!(char_from_pos(s, p(2, 2)), 5);
        // "to the end of the line" is usize::MAX; it must not overflow
        assert_eq!(char_from_pos(s, p(2, usize::MAX)), 6);
    }

    #[test]
    fn positions_end_of_file() {
        let text = Rope::from("ab\ncd\n");
        let s = text.slice(..);
        // the empty line after the final newline
        assert_eq!(pos_from_char(s, 6), p(3, 1));
        assert_eq!(char_from_pos(s, p(3, 1)), 6);
        assert_eq!(char_from_pos(s, p(50, 1)), 6);
        assert_eq!(pos_from_char(s, 600), p(3, 1));
        let empty = Rope::from("");
        assert_eq!(pos_from_char(empty.slice(..), 0), p(1, 1));
        assert_eq!(char_from_pos(empty.slice(..), p(4, 4)), 0);
    }

    #[test]
    fn cursor_vs_selection() {
        let text = Rope::from("hello\nworld\n");
        let s = text.slice(..);
        // one-char range: a cursor
        let r = range_to_proto(s, &HxRange::new(1, 2));
        assert_eq!(r.start, p(1, 2));
        assert_eq!(r.end, p(1, 2));
        // backwards one-char range: still a cursor at that char
        let r = range_to_proto(s, &HxRange::new(2, 1));
        assert_eq!((r.start, r.end), (p(1, 2), p(1, 2)));
        // a whole line selected with `x`: ends at the start of the next line
        let r = range_to_proto(s, &HxRange::new(0, 6));
        assert_eq!((r.start, r.end), (p(1, 1), p(2, 1)));
        // backwards selection reports start <= end
        let r = range_to_proto(s, &HxRange::new(9, 7));
        assert_eq!((r.start, r.end), (p(2, 2), p(2, 4)));
    }

    #[test]
    fn grapheme_cluster_is_a_cursor() {
        // "e" + combining acute accent: two chars, one grapheme.
        let text = Rope::from("e\u{301}x");
        let s = text.slice(..);
        assert!(is_cursor(s, &HxRange::new(0, 2)));
        assert!(!is_cursor(s, &HxRange::new(0, 3)));
    }

    #[test]
    fn text_for_cursors_is_the_line() {
        let text = Rope::from("one\r\ntwo\nthree");
        let s = text.slice(..);
        let sel = Selection::new(smallvec![HxRange::new(1, 2), HxRange::new(5, 6)], 1);
        let (ranges, truncated) = selection_with_text(s, &sel, 1000);
        assert!(!truncated);
        assert_eq!(ranges[0].text, "one");
        assert_eq!(ranges[1].text, "two");

        // a real selection mixed in: every range gets its own text
        let sel = Selection::new(smallvec![HxRange::new(1, 2), HxRange::new(5, 9)], 0);
        let (ranges, _) = selection_with_text(s, &sel, 1000);
        assert_eq!(ranges[0].text, "n");
        assert_eq!(ranges[1].text, "two\n");
    }

    #[test]
    fn truncation_on_char_boundary() {
        let text = Rope::from("aé🦀b");
        let (s, cut) = slice_to_string(text.slice(..), 4);
        assert!(cut);
        assert_eq!(s, "aé"); // 3 bytes; the crab does not fit
        let (s, cut) = slice_to_string(text.slice(..), 100);
        assert!(!cut);
        assert_eq!(s, "aé🦀b");

        let sel = Selection::new(smallvec![HxRange::new(0, 2), HxRange::new(2, 4)], 0);
        let (ranges, cut) = selection_with_text(text.slice(..), &sel, 4);
        assert!(cut);
        assert_eq!(ranges[0].text, "aé");
        assert_eq!(ranges[1].text, "");
    }

    #[test]
    fn text_range_extraction() {
        let text = Rope::from("ab\ncd\nef");
        let (s, _) = text_in_range(
            &text,
            Some(Range {
                start: p(1, 2),
                end: p(2, 2),
            }),
            100,
        );
        assert_eq!(s, "b\nc");
        let (s, _) = text_in_range(&text, None, 100);
        assert_eq!(s, "ab\ncd\nef");
    }

    #[test]
    fn doc_numbers() {
        assert_eq!(doc_num(DocumentId::default()), 1);
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_794_867_003); // 2026-11-16T22:10:03Z
        assert_eq!(rfc3339(t), "2026-11-16T22:10:03Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(rfc3339(leap), "2000-02-29T00:00:00Z");
    }
}
