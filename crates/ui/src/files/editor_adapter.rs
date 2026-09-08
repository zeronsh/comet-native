//! Zeron-owned styling and highlighting adapters for `gpui-base`.

use std::{ops::Range, rc::Rc, sync::Arc};

use gpui::{Context, HighlightStyle, SharedString, Window};
use gpui_base::input::{
    FoldRange, HighlightStyleResolver, InputEdit, InputEditorStyle, InputHighlighter, Rope,
};
use zeron_syntax::{HighlightKind, HighlightedDocument};

use super::editor::FileEditorState;
use crate::theme::{SyntaxPalette, Theme};

#[derive(Clone)]
struct ZeronHighlightStyleResolver {
    palette: SyntaxPalette,
}

impl HighlightStyleResolver for ZeronHighlightStyleResolver {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        kind_for_name(name).map(|kind| HighlightStyle {
            color: Some(self.palette.color(kind)),
            ..Default::default()
        })
    }
}

#[derive(Clone)]
pub(super) struct ZeronInputHighlighter {
    language: SharedString,
    spans: Vec<(Range<usize>, HighlightKind)>,
}

impl ZeronInputHighlighter {
    fn new(source: &str, document: &HighlightedDocument) -> Self {
        Self {
            language: format!("{:?}", document.language).into(),
            spans: absolute_spans(source, document),
        }
    }
}

impl InputHighlighter for ZeronInputHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        edit: Option<InputEdit>,
        _text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<FileEditorState>,
    ) {
        let Some(edit) = edit else {
            return;
        };
        let removed = edit.old_end_byte.saturating_sub(edit.start_byte);
        let inserted = edit.new_end_byte.saturating_sub(edit.start_byte);
        self.spans.retain_mut(|(range, _)| {
            if range.end <= edit.start_byte {
                return true;
            }
            if range.start >= edit.old_end_byte {
                if inserted >= removed {
                    let delta = inserted - removed;
                    range.start = range.start.saturating_add(delta);
                    range.end = range.end.saturating_add(delta);
                } else {
                    let delta = removed - inserted;
                    range.start = range.start.saturating_sub(delta);
                    range.end = range.end.saturating_sub(delta);
                }
                return true;
            }
            false
        });
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if range.is_empty() {
            return Vec::new();
        }
        let mut runs = Vec::new();
        let mut cursor = range.start;
        for (span, kind) in &self.spans {
            if span.end <= range.start {
                continue;
            }
            if span.start >= range.end {
                break;
            }
            let start = span.start.max(range.start).max(cursor);
            let end = span.end.min(range.end);
            if start > cursor {
                runs.push((cursor..start, HighlightStyle::default()));
            }
            if start < end {
                runs.push((
                    start..end,
                    resolver.style(name_for_kind(*kind)).unwrap_or_default(),
                ));
                cursor = end;
            }
        }
        if cursor < range.end {
            runs.push((cursor..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

pub(super) fn editor_style(theme: &Theme) -> InputEditorStyle {
    InputEditorStyle {
        foreground: theme.text.opacity(0.93),
        muted_foreground: theme.text_faint,
        background: gpui::transparent_black(),
        border: theme.border,
        selection: theme.accent.opacity(0.22),
        caret: theme.caret,
        highlight_styles: Arc::new(ZeronHighlightStyleResolver {
            palette: theme.syntax.clone(),
        }),
        editor_active_line: Some(crate::theme::wash(0.025)),
        editor_gutter_background: Some(gpui::transparent_black()),
        ..Default::default()
    }
}

pub(super) fn install_highlighter(
    editor: &gpui::Entity<FileEditorState>,
    source: String,
    document: Arc<HighlightedDocument>,
    cx: &mut gpui::App,
) {
    let editor_source = editor.read(cx).value();
    if !highlight_source_matches_editor(&source, &editor_source) {
        return;
    }
    editor.update(cx, |state, cx| {
        state.set_highlighter_factory(
            Rc::new(move |_| {
                Some(Box::new(ZeronInputHighlighter::new(&source, &document)) as Box<_>)
            }),
            cx,
        );
    });
}

fn highlight_source_matches_editor(highlight_source: &str, editor_source: &str) -> bool {
    highlight_source == editor_source
}

fn absolute_spans(
    source: &str,
    document: &HighlightedDocument,
) -> Vec<(Range<usize>, HighlightKind)> {
    let mut starts = vec![0];
    starts.extend(
        source
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    let mut spans = Vec::new();
    for (line_index, line) in document.lines.iter().enumerate() {
        let Some(line_start) = starts.get(line_index).copied() else {
            break;
        };
        for span in line {
            let range = line_start.saturating_add(span.range.start)
                ..line_start.saturating_add(span.range.end);
            if range.start <= range.end
                && range.end <= source.len()
                && source.is_char_boundary(range.start)
                && source.is_char_boundary(range.end)
            {
                spans.push((range, span.kind));
            }
        }
    }
    spans.sort_unstable_by_key(|(range, _)| (range.start, range.end));
    spans
}

fn name_for_kind(kind: HighlightKind) -> &'static str {
    match kind {
        HighlightKind::Comment => "comment",
        HighlightKind::Keyword => "keyword",
        HighlightKind::String => "string",
        HighlightKind::StringSpecial => "string_special",
        HighlightKind::Escape => "escape",
        HighlightKind::Number => "number",
        HighlightKind::Boolean => "boolean",
        HighlightKind::Type => "type",
        HighlightKind::TypeBuiltin => "type_builtin",
        HighlightKind::Constructor => "constructor",
        HighlightKind::Function => "function",
        HighlightKind::FunctionBuiltin => "function_builtin",
        HighlightKind::Macro => "macro",
        HighlightKind::Property => "property",
        HighlightKind::Constant => "constant",
        HighlightKind::Variable => "variable",
        HighlightKind::VariableSpecial => "variable_special",
        HighlightKind::Parameter => "parameter",
        HighlightKind::Operator => "operator",
        HighlightKind::Punctuation => "punctuation",
        HighlightKind::Tag => "tag",
        HighlightKind::Attribute => "attribute",
        HighlightKind::Label => "label",
        HighlightKind::MarkupHeading => "markup_heading",
        HighlightKind::MarkupRaw => "markup_raw",
        HighlightKind::MarkupLink => "markup_link",
        HighlightKind::MarkupReference => "markup_reference",
        HighlightKind::MarkupEmphasis => "markup_emphasis",
        HighlightKind::MarkupStrong => "markup_strong",
        HighlightKind::Embedded => "embedded",
        HighlightKind::Invalid => "invalid",
    }
}

fn kind_for_name(name: &str) -> Option<HighlightKind> {
    const KINDS: [HighlightKind; 31] = [
        HighlightKind::Comment,
        HighlightKind::Keyword,
        HighlightKind::String,
        HighlightKind::StringSpecial,
        HighlightKind::Escape,
        HighlightKind::Number,
        HighlightKind::Boolean,
        HighlightKind::Type,
        HighlightKind::TypeBuiltin,
        HighlightKind::Constructor,
        HighlightKind::Function,
        HighlightKind::FunctionBuiltin,
        HighlightKind::Macro,
        HighlightKind::Property,
        HighlightKind::Constant,
        HighlightKind::Variable,
        HighlightKind::VariableSpecial,
        HighlightKind::Parameter,
        HighlightKind::Operator,
        HighlightKind::Punctuation,
        HighlightKind::Tag,
        HighlightKind::Attribute,
        HighlightKind::Label,
        HighlightKind::MarkupHeading,
        HighlightKind::MarkupRaw,
        HighlightKind::MarkupLink,
        HighlightKind::MarkupReference,
        HighlightKind::MarkupEmphasis,
        HighlightKind::MarkupStrong,
        HighlightKind::Embedded,
        HighlightKind::Invalid,
    ];
    KINDS.into_iter().find(|kind| name_for_kind(*kind) == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_syntax::{HighlightSpan, LanguageId};

    fn highlighted(source: &str, spans: Vec<HighlightSpan>) -> HighlightedDocument {
        HighlightedDocument::from_absolute_spans(LanguageId::Rust, source, spans).unwrap()
    }

    #[test]
    fn line_relative_spans_become_absolute_utf8_ranges() {
        let source = "let café = \"😀\";\nnext";
        let string_start = source.find('"').unwrap();
        let document = highlighted(
            source,
            vec![HighlightSpan {
                range: string_start..string_start + "\"😀\"".len(),
                kind: HighlightKind::String,
            }],
        );
        let spans = absolute_spans(source, &document);
        assert_eq!(spans[0].0, string_start..string_start + "\"😀\"".len());
    }

    #[test]
    fn style_runs_are_ordered_non_overlapping_and_cover_the_request() {
        let source = "let value = 42;";
        let document = highlighted(
            source,
            vec![
                HighlightSpan {
                    range: 0..3,
                    kind: HighlightKind::Keyword,
                },
                HighlightSpan {
                    range: 12..14,
                    kind: HighlightKind::Number,
                },
            ],
        );
        let highlighter = ZeronInputHighlighter::new(source, &document);
        let resolver = ZeronHighlightStyleResolver {
            palette: Theme::dark().syntax,
        };
        let runs = highlighter.styles(&(0..source.len()), &resolver);
        assert_eq!(runs.first().unwrap().0.start, 0);
        assert_eq!(runs.last().unwrap().0.end, source.len());
        assert!(runs.windows(2).all(|pair| pair[0].0.end == pair[1].0.start));
    }

    #[test]
    fn stale_highlight_ranges_are_rejected_before_updated_unicode_is_shaped() {
        let stale_source = "ab";
        let stale_document = highlighted(
            stale_source,
            vec![HighlightSpan {
                range: 1..2,
                kind: HighlightKind::Variable,
            }],
        );
        let highlighter = ZeronInputHighlighter::new(stale_source, &stale_document);
        let resolver = ZeronHighlightStyleResolver {
            palette: Theme::dark().syntax,
        };
        let updated_text = "é";

        let stale_runs = highlighter.styles(&(0..updated_text.len()), &resolver);

        assert!(stale_runs.iter().any(|(range, _)| {
            !updated_text.is_char_boundary(range.start) || !updated_text.is_char_boundary(range.end)
        }));
        assert!(!highlight_source_matches_editor(stale_source, updated_text));
    }

    #[test]
    fn editor_caret_uses_the_same_theme_token_as_other_inputs() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_eq!(editor_style(&theme).caret, theme.caret);
        }
    }
}
