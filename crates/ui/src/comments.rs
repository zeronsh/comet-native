//! Review comments pinned to lines in a diff or editable workspace file,
//! staged on the composer and folded into the next prompt as plain text.
//!
//! [`with_comments`] appends them; [`extract_badge`] reads the same block back
//! out for the transcript. There is no second data model.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommentSide {
    Old,
    New,
}

impl CommentSide {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Old => "L",
            Self::New => "R",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentSource {
    Diff {
        side: CommentSide,
        old_path: Option<String>,
    },
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: String,
    /// The current workspace path. For an old-side diff comment on a renamed
    /// file, [`Self::cite_path`] returns the pre-rename path instead.
    pub path: String,
    pub line: u32,
    pub body: String,
    pub source: CommentSource,
}

impl ReviewComment {
    /// Create a comment anchored to one side of a diff.
    pub fn new(
        path: impl Into<String>,
        side: CommentSide,
        line: u32,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            path: path.into(),
            line,
            body: body.into(),
            source: CommentSource::Diff {
                side,
                old_path: None,
            },
        }
    }

    /// Create a comment anchored to the current contents of an editable file.
    pub fn file(path: impl Into<String>, line: u32, body: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            path: path.into(),
            line,
            body: body.into(),
            source: CommentSource::File,
        }
    }

    /// Tag the comment with the file's pre-rename path so an `Old`-side line
    /// cites where that line actually lives.
    pub fn renamed_from(mut self, old_path: Option<impl Into<String>>) -> Self {
        if let CommentSource::Diff {
            old_path: current, ..
        } = &mut self.source
        {
            *current = old_path.map(Into::into);
        }
        self
    }

    pub fn diff_anchor(&self) -> Option<(CommentSide, u32)> {
        match self.source {
            CommentSource::Diff { side, .. } => Some((side, self.line)),
            CommentSource::File => None,
        }
    }

    pub fn is_file(&self) -> bool {
        matches!(self.source, CommentSource::File)
    }

    pub fn side(&self) -> Option<CommentSide> {
        match self.source {
            CommentSource::Diff { side, .. } => Some(side),
            CommentSource::File => None,
        }
    }

    /// The path the line number is valid in. An `Old`-side line only exists in
    /// the pre-rename file, so citing `path` there points the agent at a line
    /// of a file that never held it.
    pub fn cite_path(&self) -> &str {
        match &self.source {
            CommentSource::Diff {
                side: CommentSide::Old,
                old_path,
            } => old_path.as_deref().unwrap_or(&self.path),
            CommentSource::Diff { .. } | CommentSource::File => &self.path,
        }
    }

    pub fn location(&self) -> String {
        format!("{}:{}", self.cite_path(), self.line)
    }
}

pub const COMMENT_ONLY_TEXT: &str = "Address the review comments below.";

pub const COMMENT_BLOCK_HEADER: &str = "Comments on the diff (each cites the file and line it belongs to; L = line number in the original file, R = in the changed file):";
pub const REVIEW_COMMENT_BLOCK_HEADER: &str =
    "Review comments (each cites the workspace file and line it belongs to):";

fn side_marker(side: CommentSide) -> String {
    format!(" ({}): ", side.tag())
}

pub fn with_comments(text: &str, comments: &[ReviewComment]) -> String {
    if comments.is_empty() {
        return text.to_string();
    }
    let bullets: Vec<String> = comments
        .iter()
        .map(|comment| {
            let body = comment.body.trim().replace('\n', "\n  ");
            let separator = comment
                .side()
                .map(side_marker)
                .unwrap_or_else(|| ": ".to_string());
            format!("- {}{separator}{body}", comment.location())
        })
        .collect();
    let body = if text.is_empty() {
        COMMENT_ONLY_TEXT
    } else {
        text
    };
    let header = if comments.iter().all(|comment| !comment.is_file()) {
        COMMENT_BLOCK_HEADER
    } else {
        REVIEW_COMMENT_BLOCK_HEADER
    };
    format!("{body}\n\n{header}\n{}", bullets.join("\n"))
}

/// [`crate::badges::Extractor`] for the comment block. Matched only as a whole
/// trailing block, so a prompt quoting the header mid-body is left alone.
pub fn extract_badge(text: &str) -> Option<(String, crate::badges::MessageBadge)> {
    let markers = [
        format!("\n\n{COMMENT_BLOCK_HEADER}\n"),
        format!("\n\n{REVIEW_COMMENT_BLOCK_HEADER}\n"),
    ];
    let (at, marker) = markers
        .iter()
        .filter_map(|marker| text.rfind(marker).map(|at| (at, marker)))
        .max_by_key(|(at, _)| *at)?;
    let block = &text[at + marker.len()..];
    if block.is_empty()
        || !block
            .lines()
            .all(|line| line.starts_with("- ") || line.starts_with("  "))
    {
        return None;
    }
    let details = parse_bullets(block);
    if details.is_empty() {
        return None;
    }
    Some((
        text[..at].to_string(),
        crate::badges::MessageBadge {
            icon: crate::icons::CHAT_ROUND_LINE,
            label: chip_label(details.len()).into(),
            details,
        },
    ))
}

fn parse_bullets(block: &str) -> Vec<crate::badges::BadgeDetail> {
    let mut details: Vec<crate::badges::BadgeDetail> = Vec::new();
    for line in block.lines() {
        let Some(bullet) = line.strip_prefix("- ") else {
            if let (Some(indented), Some(last)) = (line.strip_prefix("  "), details.last_mut()) {
                last.body = format!("{}\n{indented}", last.body).into();
            }
            continue;
        };
        // Earliest marker wins: a body may contain "(L): " itself, and matching
        // that would swallow the body into the location.
        let split = [CommentSide::Old, CommentSide::New]
            .into_iter()
            .filter_map(|side| {
                let marker = side_marker(side);
                bullet.find(&marker).map(|at| (at, marker.len(), side))
            })
            .min_by_key(|(at, _, _)| *at);
        let file_split = split_file_bullet(bullet);
        if let Some((at, location, body)) = file_split
            && split.is_none_or(|(side_at, _, _)| at < side_at)
        {
            details.push(crate::badges::BadgeDetail {
                location: location.into(),
                tag: None,
                body: body.into(),
            });
            continue;
        }
        if let Some((at, marker_len, side)) = split {
            details.push(crate::badges::BadgeDetail {
                location: bullet[..at].into(),
                tag: Some(side.tag().into()),
                body: bullet[at + marker_len..].into(),
            });
            continue;
        }
        if let Some((_, location, body)) = file_split {
            details.push(crate::badges::BadgeDetail {
                location: location.into(),
                tag: None,
                body: body.into(),
            });
        }
    }
    details
}

fn split_file_bullet(bullet: &str) -> Option<(usize, &str, &str)> {
    bullet.match_indices(": ").find_map(|(at, separator)| {
        let location = &bullet[..at];
        let (_, line) = location.rsplit_once(':')?;
        line.parse::<u32>().ok()?;
        Some((at, location, &bullet[at + separator.len()..]))
    })
}

pub fn chip_label(count: usize) -> String {
    if count == 1 {
        "1 comment".to_string()
    } else {
        format!("{count} comments")
    }
}

pub const CARD_PAD_V: f32 = 20.0;
pub const CARD_HEADER_HEIGHT: f32 = 22.0;
pub const CARD_LINE_HEIGHT: f32 = 18.0;
pub const DRAFT_CARD_HEIGHT: f32 = 116.0;
pub const COMMENT_ADDER_SIZE: f32 = 16.0;
const CARD_GAP: f32 = 6.0;
const CARD_WRAP_COLUMNS: usize = 64;
const CARD_MAX_LINES: usize = 8;

/// Wraps are guessed, not measured: the changes pane sizes bodies by arithmetic
/// to drive the fold tween, and a measured card would desync it.
pub fn card_body_lines(body: &str) -> usize {
    body.lines()
        .map(|line| line.chars().count().div_ceil(CARD_WRAP_COLUMNS).max(1))
        .sum::<usize>()
        .clamp(1, CARD_MAX_LINES)
}

pub fn card_height(body: &str) -> f32 {
    CARD_PAD_V + CARD_HEADER_HEIGHT + card_body_lines(body) as f32 * CARD_LINE_HEIGHT + CARD_GAP
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(path: &str, side: CommentSide, line: u32, body: &str) -> ReviewComment {
        ReviewComment::new(path, side, line, body)
    }

    #[test]
    fn empty_set_leaves_the_prompt_untouched() {
        assert_eq!(with_comments("ship it", &[]), "ship it");
    }

    #[test]
    fn comments_append_as_located_bullets() {
        let staged = vec![
            comment("src/main.rs", CommentSide::New, 42, "early-return here"),
            comment("src/lib.rs", CommentSide::Old, 7, "why was this dropped?"),
        ];
        let out = with_comments("look at these", &staged);
        assert!(out.starts_with("look at these\n\n"));
        assert!(out.contains("- src/main.rs:42 (R): early-return here"));
        assert!(out.contains("- src/lib.rs:7 (L): why was this dropped?"));
    }

    #[test]
    fn comment_only_send_gets_a_body() {
        let staged = vec![comment("a.rs", CommentSide::New, 1, "fix")];
        assert!(with_comments("", &staged).starts_with(COMMENT_ONLY_TEXT));
    }

    #[test]
    fn multiline_bodies_indent_under_their_bullet() {
        let staged = vec![comment("a.rs", CommentSide::New, 3, "first\nsecond")];
        assert!(with_comments("x", &staged).contains("- a.rs:3 (R): first\n  second"));
    }

    #[test]
    fn file_comments_use_current_workspace_locations_without_diff_tags() {
        let staged = vec![ReviewComment::file(
            "crates/ui/src/files/preview.rs",
            1494,
            "Keep this branch explicit",
        )];
        let out = with_comments("review this", &staged);
        assert!(out.contains(REVIEW_COMMENT_BLOCK_HEADER));
        assert!(out.contains("- crates/ui/src/files/preview.rs:1494: Keep this branch explicit"));
        assert!(!out.contains("(R)"));

        let (text, badge) = extract_badge(&out).unwrap();
        assert_eq!(text, "review this");
        assert_eq!(
            badge.details[0].location.as_ref(),
            "crates/ui/src/files/preview.rs:1494"
        );
        assert_eq!(badge.details[0].tag, None);
        assert_eq!(badge.details[0].body.as_ref(), "Keep this branch explicit");
    }

    #[test]
    fn mixed_editor_and_diff_comments_share_one_review_block() {
        let staged = vec![
            comment("a.rs", CommentSide::Old, 3, "why removed?"),
            ReviewComment::file("odd:path.rs", 8, "change this: please"),
        ];
        let (text, badge) = extract_badge(&with_comments("x", &staged)).unwrap();
        assert_eq!(text, "x");
        assert_eq!(badge.details.len(), 2);
        assert_eq!(badge.details[0].tag.as_deref(), Some("L"));
        assert_eq!(badge.details[1].location.as_ref(), "odd:path.rs:8");
        assert_eq!(badge.details[1].body.as_ref(), "change this: please");
    }

    #[test]
    fn file_comment_body_may_quote_a_diff_side_marker() {
        let staged = vec![ReviewComment::file("a.rs", 2, "compare with (L): old code")];
        let (_, badge) = extract_badge(&with_comments("x", &staged)).unwrap();
        assert_eq!(badge.details[0].location.as_ref(), "a.rs:2");
        assert_eq!(badge.details[0].tag, None);
        assert_eq!(badge.details[0].body.as_ref(), "compare with (L): old code");
    }

    #[test]
    fn chip_label_pluralizes() {
        assert_eq!(chip_label(1), "1 comment");
        assert_eq!(chip_label(2), "2 comments");
        assert_eq!(chip_label(0), "0 comments");
    }

    #[test]
    fn a_body_quoting_a_side_marker_survives_the_round_trip() {
        let staged = vec![comment(
            "a.rs",
            CommentSide::New,
            5,
            "see (L): the other one",
        )];
        let (text, badge) = extract_badge(&with_comments("x", &staged)).unwrap();
        assert_eq!(text, "x");
        assert_eq!(badge.details.len(), 1);
        assert_eq!(badge.details[0].location.as_ref(), "a.rs:5");
        assert_eq!(badge.details[0].tag.as_deref(), Some("R"));
        assert_eq!(badge.details[0].body.as_ref(), "see (L): the other one");
    }

    #[test]
    fn a_renamed_file_cites_the_side_the_line_lives_in() {
        let old = comment("new_name.rs", CommentSide::Old, 7, "why dropped?")
            .renamed_from(Some("old_name.rs"));
        let new =
            comment("new_name.rs", CommentSide::New, 12, "nit").renamed_from(Some("old_name.rs"));
        // The grouping key stays the diff's own path either way.
        assert_eq!(old.path, "new_name.rs");
        assert_eq!(new.path, "new_name.rs");
        let out = with_comments("x", &[old, new]);
        assert!(out.contains("- old_name.rs:7 (L): why dropped?"));
        assert!(out.contains("- new_name.rs:12 (R): nit"));
    }

    #[test]
    fn an_unrenamed_file_cites_its_only_path_on_both_sides() {
        let staged = vec![
            comment("a.rs", CommentSide::Old, 3, "gone"),
            comment("a.rs", CommentSide::New, 4, "here"),
        ];
        let out = with_comments("x", &staged);
        assert!(out.contains("- a.rs:3 (L): gone"));
        assert!(out.contains("- a.rs:4 (R): here"));
    }

    #[test]
    fn card_height_grows_with_body_lines() {
        assert!(card_height("two\nlines") > card_height("one"));
        assert_eq!(card_height(""), card_height("one"));
    }

    #[test]
    fn long_lines_are_charged_for_their_soft_wraps() {
        assert_eq!(card_body_lines("short"), 1);
        assert_eq!(card_body_lines(&"x".repeat(CARD_WRAP_COLUMNS)), 1);
        assert_eq!(card_body_lines(&"x".repeat(CARD_WRAP_COLUMNS + 1)), 2);
        assert_eq!(card_body_lines(&"line\n".repeat(200)), CARD_MAX_LINES);
    }
}
