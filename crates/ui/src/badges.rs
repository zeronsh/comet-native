//! Message badges: pills standing in for structured context a user message
//! carries.
//!
//! A feature stages context on the composer, folds it into the prompt as plain
//! text, and registers an [`Extractor`]. [`split`] lifts that block back out of
//! the sent message so the transcript draws a pill instead of the bullets the
//! agent reads. Nothing here knows what a diff comment is.

use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, IntoElement, Render, SharedString, Window, div, prelude::*, px};

use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBadge {
    /// An `icons::*` asset name.
    pub icon: &'static str,
    pub label: SharedString,
    /// Empty means the label says everything and the pill carries no card.
    pub details: Vec<BadgeDetail>,
}

/// One row of a hover card. Three generic slots, so a new badge kind fills them
/// without this module learning what it cites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadgeDetail {
    /// `src/main.rs:42`, a URL, a commit subject.
    pub location: SharedString,
    /// `L`/`R` for a diff side, a status, a count.
    pub tag: Option<SharedString>,
    pub body: SharedString,
}

/// Returns the text with this feature's block removed, plus the pill replacing
/// it. `None` when the message carries nothing of that kind.
pub type Extractor = fn(&str) -> Option<(String, MessageBadge)>;

const EXTRACTORS: &[Extractor] = &[crate::comments::extract_badge];

/// Each extractor sees what the previous ones left behind, so two features can
/// ride the same prompt.
pub fn split(text: &str) -> (String, Vec<MessageBadge>) {
    let mut rest = text.to_string();
    let mut badges = Vec::new();
    for extract in EXTRACTORS {
        if let Some((stripped, badge)) = extract(&rest) {
            rest = stripped;
            badges.push(badge);
        }
    }
    (rest, badges)
}

/// The composer sizes its strip arithmetically, so this cannot be a
/// measurement.
pub const BADGE_HEIGHT: f32 = 24.0;

const PILL_RADIUS: f32 = 8.0;
const ICON_SIZE: f32 = 12.0;
const TEXT_SIZE: f32 = 12.0;
const CARD_WIDTH: f32 = 320.0;
const HOVER_DELAY: Duration = Duration::from_millis(280);

/// `id` scopes the hover card and must be stable per rendered pill.
pub fn render(
    id: impl Into<gpui::ElementId>,
    badge: &MessageBadge,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(BADGE_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .rounded(px(PILL_RADIUS))
        .bg(crate::theme::ink(0.06))
        .text_size(px(TEXT_SIZE))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_muted)
        .child(
            crate::icons::icon(badge.icon)
                .size(px(ICON_SIZE))
                .text_color(theme.text_muted.opacity(0.7)),
        )
        .child(badge.label.clone())
        .when(!badge.details.is_empty(), |el| {
            let details = Arc::new(badge.details.clone());
            el.tooltip(move |_, cx| {
                cx.new(|_| BadgeCard {
                    details: details.clone(),
                })
                .into()
            })
            .tooltip_show_delay(HOVER_DELAY)
        })
}

struct BadgeCard {
    details: Arc<Vec<BadgeDetail>>,
}

impl BadgeCard {
    fn row(detail: &BadgeDetail, theme: &Theme) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .p(px(8.0))
            .rounded(px(6.0))
            .bg(crate::theme::ink(0.05))
            .child(
                div()
                    .flex_none()
                    .w(px(2.0))
                    .rounded(px(1.0))
                    .bg(theme.solid.opacity(0.35)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .font_family(theme.font_mono.clone())
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(detail.location.clone()),
                            )
                            .children(detail.tag.clone().map(|tag| {
                                div()
                                    .flex_none()
                                    .px(px(4.0))
                                    .rounded(px(3.0))
                                    .bg(crate::theme::ink(0.10))
                                    .child(tag)
                            })),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(TEXT_SIZE))
                            .line_height(px(16.0))
                            .text_color(theme.text)
                            .child(detail.body.clone()),
                    ),
            )
    }
}

impl Render for BadgeCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let card = crate::popover::popover_card(theme)
            .w(px(CARD_WIDTH))
            .p(px(6.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(self.details.iter().map(|detail| Self::row(detail, theme)));
        // `frosted` is load-bearing beyond the blur: it gives the card its own
        // scene layer. Sharing the transcript's layer let the bubble's text
        // paint over the card, since equal draw orders render quads before
        // text.
        crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{CommentSide, ReviewComment, with_comments};

    #[test]
    fn a_plain_message_carries_no_badges() {
        let (text, badges) = split("just a prompt");
        assert_eq!(text, "just a prompt");
        assert!(badges.is_empty());
    }

    #[test]
    fn a_sent_comment_block_becomes_one_pill() {
        let staged = vec![
            ReviewComment::new("a.rs", CommentSide::New, 3, "fix"),
            ReviewComment::new("b.rs", CommentSide::Old, 9, "why"),
        ];
        let (text, badges) = split(&with_comments("do", &staged));
        assert_eq!(text, "do");
        assert_eq!(badges.len(), 1);
        assert_eq!(badges[0].label.as_ref(), "2 comments");
    }

    #[test]
    fn the_card_carries_one_row_per_comment() {
        let staged = vec![
            ReviewComment::new("src/main.rs", CommentSide::New, 42, "early-return here"),
            ReviewComment::new("src/lib.rs", CommentSide::Old, 7, "why was this dropped?"),
        ];
        let (_, badges) = split(&with_comments("look", &staged));
        let details = &badges[0].details;
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].location.as_ref(), "src/main.rs:42");
        assert_eq!(details[0].tag.as_deref(), Some("R"));
        assert_eq!(details[0].body.as_ref(), "early-return here");
        assert_eq!(details[1].location.as_ref(), "src/lib.rs:7");
        assert_eq!(details[1].tag.as_deref(), Some("L"));
    }

    #[test]
    fn a_multiline_body_rejoins_its_continuation_lines() {
        let staged = vec![ReviewComment::new(
            "a.rs",
            CommentSide::New,
            3,
            "first\nsecond",
        )];
        let (_, badges) = split(&with_comments("x", &staged));
        assert_eq!(badges[0].details[0].body.as_ref(), "first\nsecond");
    }

    #[test]
    fn a_path_with_a_colon_still_splits_on_the_side_marker() {
        let staged = vec![ReviewComment::new("odd:name.rs", CommentSide::New, 5, "hm")];
        let (_, badges) = split(&with_comments("x", &staged));
        assert_eq!(badges[0].details[0].location.as_ref(), "odd:name.rs:5");
        assert_eq!(badges[0].details[0].body.as_ref(), "hm");
    }

    #[test]
    fn a_comment_only_send_keeps_its_stand_in_body() {
        let staged = vec![ReviewComment::new("a.rs", CommentSide::New, 3, "fix")];
        let (text, badges) = split(&with_comments("", &staged));
        assert_eq!(text, crate::comments::COMMENT_ONLY_TEXT);
        assert_eq!(badges[0].label.as_ref(), "1 comment");
    }
}
