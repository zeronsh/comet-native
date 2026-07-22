//! Text selection for rendered markdown (round 18).
//!
//! gpui has no built-in selection for plain text elements, so this is the
//! Zed-markdown mechanic adapted to comet's per-element renderer: ONE global
//! selection owned by whichever flat-text element the drag started in
//! (starting a drag elsewhere reclaims it), position↔index mapping through
//! the element's own [`gpui::TextLayout`], an accent wash painted under the
//! glyphs, and frame-scoped window-level mouse listeners so a drag keeps
//! tracking outside the element's bounds.
//!
//! Copy surfaces: a completed drag writes the X11 PRIMARY buffer
//! (middle-click paste, Linux only — same as Zed), and Cmd+C in the composer
//! falls back to the markdown selection when the input has no selection of
//! its own (the composer keeps focus while reading, so this is the natural
//! Cmd+C the user actually presses).

use std::ops::Range;
use std::sync::{Mutex, OnceLock};

/// The one live markdown selection.
#[derive(Clone, Default)]
pub struct MdSelection {
    /// Owning element key (`{row_key}:{element ix}`).
    pub key: String,
    /// Selected byte range of the owner's flat text.
    pub range: Range<usize>,
    /// Drag anchor (the mouse-down index).
    pub anchor: usize,
    /// Mid-drag: mouse is down and moves update the head.
    pub dragging: bool,
    /// The owner's full flat text (copy source).
    pub text: String,
}

fn state() -> &'static Mutex<Option<MdSelection>> {
    static STATE: OnceLock<Mutex<Option<MdSelection>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// The current selection, if any.
pub fn get() -> Option<MdSelection> {
    state().lock().unwrap().clone()
}

/// The selected text, if a non-empty selection exists (Cmd+C fallback).
pub fn selected_text() -> Option<String> {
    let sel = get()?;
    (!sel.range.is_empty()).then(|| sel.text[sel.range.clone()].to_string())
}

/// Begin a drag: claim the global selection for `key`.
pub fn begin(key: &str, text: &str, range: Range<usize>, anchor: usize) {
    *state().lock().unwrap() = Some(MdSelection {
        key: key.to_string(),
        range,
        anchor,
        dragging: true,
        text: text.to_string(),
    });
}

/// Update the drag head for `key` (no-op if another element owns the drag).
/// Returns true if the selection changed.
pub fn drag_to(key: &str, head: usize) -> bool {
    let mut guard = state().lock().unwrap();
    let Some(sel) = guard.as_mut() else {
        return false;
    };
    if sel.key != key || !sel.dragging {
        return false;
    }
    let range = sel.anchor.min(head)..sel.anchor.max(head);
    if sel.range == range {
        return false;
    }
    sel.range = range;
    true
}

/// End the drag for `key`; returns the selected text if non-empty.
pub fn end_drag(key: &str) -> Option<String> {
    let mut guard = state().lock().unwrap();
    let sel = guard.as_mut()?;
    if sel.key != key || !sel.dragging {
        return None;
    }
    sel.dragging = false;
    if sel.range.is_empty() {
        *guard = None;
        return None;
    }
    Some(sel.text[sel.range.clone()].to_string())
}

/// Clear if `key` currently owns the selection (a mouse-down landed outside
/// the owner — each element's listener clears only its own claim, and the
/// element the down landed IN claims right after). Returns true if cleared.
pub fn clear_if_owner(key: &str) -> bool {
    let mut guard = state().lock().unwrap();
    if guard.as_ref().is_some_and(|s| s.key == key && !s.dragging) {
        *guard = None;
        return true;
    }
    false
}

/// The wash range to paint for `key` this frame (empty ⇒ nothing).
pub fn wash_range(key: &str) -> Option<Range<usize>> {
    let guard = state().lock().unwrap();
    let sel = guard.as_ref()?;
    (sel.key == key && !sel.range.is_empty()).then(|| sel.range.clone())
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

    #[test]
    fn drag_lifecycle_owns_and_normalizes() {
        begin("k1", "hello world", 6..6, 6);
        assert!(drag_to("k1", 11));
        assert_eq!(get().unwrap().range, 6..11);
        // Reversed drag normalizes.
        assert!(drag_to("k1", 0));
        assert_eq!(get().unwrap().range, 0..6);
        // Another key can't move this drag.
        assert!(!drag_to("k2", 3));
        assert_eq!(end_drag("k1").as_deref(), Some("hello "));
        assert_eq!(selected_text().as_deref(), Some("hello "));
        // A new claim replaces the old owner.
        begin("k2", "abc", 0..3, 0);
        assert_eq!(get().unwrap().key, "k2");
        end_drag("k2");
        clear_if_owner("k2");
        assert!(get().is_none());
    }

    #[test]
    fn empty_drag_clears() {
        begin("k1", "hello", 2..2, 2);
        assert_eq!(end_drag("k1"), None);
        assert!(get().is_none());
        assert_eq!(selected_text(), None);
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
        // Unicode-safe.
        let u = "héllo wörld";
        assert_eq!(&u[word_range(u, 2)], "héllo");
    }
}
