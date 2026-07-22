//! Text selection for rendered markdown (round 18).
//!
//! gpui has no built-in selection for plain text elements. Zed's markdown
//! selects continuously because its whole document is ONE element over one
//! text model; comet renders a TREE of text elements inside a virtualized
//! list, so this module rebuilds that continuity: every frame the renderer
//! registers each painted text element in paint order (= document order),
//! and a drag anchored in one element resolves against that registry into
//! per-element SPANS — partial in the anchor/head elements, whole for every
//! element between. The wash paints per element from its span; copy joins
//! the spans in order.
//!
//! This module is the pure state half (gpui-free, unit-tested); the
//! registry, geometry and mouse listeners live in `render.rs`.

use std::ops::Range;
use std::sync::{Mutex, OnceLock};

/// One element's slice of the selection, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Element key (`{row_key}:{element ix}`).
    pub key: String,
    /// Selected byte range of the element's flat text.
    pub range: Range<usize>,
    /// The element's full flat text (copy source, snapshotted at drag time
    /// so copy still works after the element scrolls out of the registry).
    pub text: String,
}

#[derive(Clone, Default)]
struct MdSelection {
    /// Element that owns the drag (where the mouse went down).
    anchor_key: String,
    /// Byte offset of the anchor within its element.
    anchor_ix: usize,
    dragging: bool,
    /// Resolved spans, document order. Empty while a click hasn't moved.
    spans: Vec<Span>,
}

fn state() -> &'static Mutex<Option<MdSelection>> {
    static STATE: OnceLock<Mutex<Option<MdSelection>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// Resolve the spans for a selection between `a` and `b`, each an
/// `(element index, byte offset)` into `elements` (document-ordered
/// `(key, text)` pairs). Handles either direction; empty slices are skipped.
pub fn resolve_spans(elements: &[(&str, &str)], a: (usize, usize), b: (usize, usize)) -> Vec<Span> {
    let (start, end) = if (a.0, a.1) <= (b.0, b.1) {
        (a, b)
    } else {
        (b, a)
    };
    let mut spans = Vec::new();
    for (ei, (key, text)) in elements.iter().enumerate().take(end.0 + 1).skip(start.0) {
        let from = if ei == start.0 { start.1 } else { 0 };
        let to = if ei == end.0 { end.1 } else { text.len() };
        let (from, to) = (from.min(text.len()), to.min(text.len()));
        if from < to {
            spans.push(Span {
                key: (*key).to_string(),
                range: from..to,
                text: (*text).to_string(),
            });
        }
    }
    spans
}

/// Begin a drag anchored at `(key, ix)`; claims the global selection.
pub fn begin(key: &str, ix: usize) {
    *state().lock().unwrap() = Some(MdSelection {
        anchor_key: key.to_string(),
        anchor_ix: ix,
        dragging: true,
        spans: Vec::new(),
    });
}

/// Begin with an immediate span (double/triple click inside one element).
pub fn begin_with_span(key: &str, text: &str, range: Range<usize>) {
    *state().lock().unwrap() = Some(MdSelection {
        anchor_key: key.to_string(),
        anchor_ix: range.start,
        dragging: true,
        spans: vec![Span {
            key: key.to_string(),
            range,
            text: text.to_string(),
        }],
    });
}

/// The live drag's anchor, if `key` owns it: `(anchor byte offset)`.
pub fn drag_anchor(key: &str) -> Option<usize> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    (sel.dragging && sel.anchor_key == key).then_some(sel.anchor_ix)
}

/// Replace the resolved spans (drag update). Returns true if they changed.
pub fn update_spans(spans: Vec<Span>) -> bool {
    let mut guard = state().lock().unwrap();
    let Some(sel) = guard.as_mut() else {
        return false;
    };
    if sel.spans == spans {
        return false;
    }
    sel.spans = spans;
    true
}

/// End the drag for `key`'s claim; returns the joined text if non-empty.
pub fn end_drag(key: &str) -> Option<String> {
    let mut guard = state().lock().unwrap();
    let sel = guard.as_mut()?;
    if sel.anchor_key != key || !sel.dragging {
        return None;
    }
    sel.dragging = false;
    if sel.spans.iter().all(|s| s.range.is_empty()) {
        *guard = None;
        return None;
    }
    Some(join_spans(&sel.spans))
}

/// Clear if `key` owns a settled selection (a mouse-down landed outside the
/// owner; the element the down landed IN claims right after). True if cleared.
pub fn clear_if_owner(key: &str) -> bool {
    let mut guard = state().lock().unwrap();
    if guard
        .as_ref()
        .is_some_and(|s| s.anchor_key == key && !s.dragging)
    {
        *guard = None;
        return true;
    }
    false
}

/// The wash range for `key` this frame (empty ⇒ nothing to paint).
pub fn wash_range(key: &str) -> Option<Range<usize>> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    sel.spans
        .iter()
        .find(|s| s.key == key && !s.range.is_empty())
        .map(|s| s.range.clone())
}

/// The full selected text (Cmd+C), spans joined in document order.
pub fn selected_text() -> Option<String> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    if sel.spans.iter().all(|s| s.range.is_empty()) {
        return None;
    }
    Some(join_spans(&sel.spans))
}

fn join_spans(spans: &[Span]) -> String {
    spans
        .iter()
        .filter(|s| !s.range.is_empty())
        .map(|s| &s.text[s.range.clone()])
        .collect::<Vec<_>>()
        .join("\n")
}

/// Word range around `ix` for double-click selection: an alphanumeric/`_`
/// run, or the single non-space char under the cursor, or empty at spaces.
pub fn word_range(text: &str, ix: usize) -> Range<usize> {
    let mut ix = ix.min(text.len());
    // Snap into a char boundary (mouse indices should already be on one;
    // defensive against mid-char byte offsets).
    while ix > 0 && !text.is_char_boundary(ix) {
        ix -= 1;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let before = text[..ix].chars().next_back();
    let at = text[ix..].chars().next();
    // Off a word boundary entirely: select the single char (or nothing).
    if !at.is_some_and(is_word) && !before.is_some_and(is_word) {
        return match at {
            Some(c) if !c.is_whitespace() => ix..ix + c.len_utf8(),
            _ => ix..ix,
        };
    }
    let start = text[..ix]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(ix);
    let end = text[ix..]
        .char_indices()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map(|(i, c)| ix + i + c.len_utf8())
        .unwrap_or(ix);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elems<'a>() -> Vec<(&'a str, &'a str)> {
        vec![
            ("p1", "first paragraph"),
            ("p2", "second"),
            ("p3", "third one"),
        ]
    }

    #[test]
    fn spans_within_one_element() {
        let spans = resolve_spans(&elems(), (0, 6), (0, 15));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].key, "p1");
        assert_eq!(&spans[0].text[spans[0].range.clone()], "paragraph");
        // Reversed direction normalizes.
        assert_eq!(resolve_spans(&elems(), (0, 15), (0, 6)), spans);
    }

    #[test]
    fn spans_across_elements_cover_middles_whole() {
        let spans = resolve_spans(&elems(), (0, 6), (2, 5));
        assert_eq!(spans.len(), 3);
        assert_eq!(&spans[0].text[spans[0].range.clone()], "paragraph");
        assert_eq!(&spans[1].text[spans[1].range.clone()], "second");
        assert_eq!(&spans[2].text[spans[2].range.clone()], "third");
        // Reversed drag (bottom-up) resolves identically.
        assert_eq!(resolve_spans(&elems(), (2, 5), (0, 6)), spans);
    }

    #[test]
    fn drag_lifecycle_and_copy_joins() {
        begin("p1", 6);
        assert_eq!(drag_anchor("p1"), Some(6));
        assert_eq!(drag_anchor("p2"), None);
        let spans = resolve_spans(&elems(), (0, 6), (1, 6));
        assert!(update_spans(spans.clone()));
        assert!(!update_spans(spans)); // unchanged ⇒ no repaint
        assert_eq!(wash_range("p1"), Some(6..15));
        assert_eq!(wash_range("p2"), Some(0..6));
        assert_eq!(wash_range("p3"), None);
        assert_eq!(end_drag("p1").as_deref(), Some("paragraph\nsecond"));
        assert_eq!(selected_text().as_deref(), Some("paragraph\nsecond"));
        // Settled: a down elsewhere clears via the owner's listener.
        assert!(!clear_if_owner("p2"));
        assert!(clear_if_owner("p1"));
        assert_eq!(selected_text(), None);
    }

    #[test]
    fn empty_click_clears_on_release() {
        begin("p1", 3);
        assert_eq!(end_drag("p1"), None);
        assert_eq!(selected_text(), None);
    }

    #[test]
    fn double_click_span() {
        begin_with_span("p1", "hello world", 6..11);
        assert_eq!(wash_range("p1"), Some(6..11));
        assert_eq!(end_drag("p1").as_deref(), Some("world"));
    }

    #[test]
    fn word_ranges() {
        let t = "let foo_bar = 12;";
        assert_eq!(word_range(t, 5), 4..11); // inside foo_bar
        assert_eq!(word_range(t, 4), 4..11); // at word start
        assert_eq!(word_range(t, 11), 4..11); // at word end
        assert_eq!(word_range(t, 15), 14..16); // inside 12
        assert_eq!(&t[word_range(t, 12)], "="); // lone symbol
        assert_eq!(word_range(t, 3), 0..3); // boundary after "let"
        // Unicode-safe (mid-char byte offsets snap down).
        let u = "héllo wörld";
        assert_eq!(&u[word_range(u, 2)], "héllo");
    }
}
