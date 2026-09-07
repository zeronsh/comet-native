//! Embedded icon assets + the gpui [`AssetSource`] that serves them.
//!
//! The set mirrors the original zeron's icon usage exactly:
//! - Most glyphs come from the **Solar Icons** set (Linear weight) by 480 Design,
//!   the same set the Electron app used via `@solar-icons/react`. Solar Icons is
//!   licensed under CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/);
//!   attribution: "Solar Icons by 480 Design".
//! - The terminal tab glyphs (`terminal`, `plus`, `close`) and the stop square
//!   are ports of the hand-drawn inline SVGs in zeron's `terminal-panel.tsx` /
//!   `composer-actions.tsx`.
//! - The harness brand marks (`claude-mark`, `openai-mark`, `cursor-mark`) are
//!   ports of zeron's `icons.tsx`. gpui tints SVGs with the text color, so the
//!   Claude mark's brand orange is applied at the call site ([`CLAUDE_BRAND`]).
//!
//! Icons render via [`icon`]: `icon(icons::PAPERCLIP).size(px(16.)).text_color(…)`.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Result, SharedString, Styled as _, Svg, svg};

macro_rules! icon_assets {
    ($(($const_name:ident, $path:literal)),+ $(,)?) => {
        $(pub const $const_name: &str = concat!("icons/", $path, ".svg");)+

        /// Serves the embedded icons to gpui's SVG renderer.
        pub struct Assets;

        impl AssetSource for Assets {
            fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
                Ok(match path {
                    $(concat!("icons/", $path, ".svg") => Some(Cow::Borrowed(
                        include_bytes!(concat!("../assets/icons/", $path, ".svg")).as_slice(),
                    )),)+
                    _ => None,
                })
            }

            fn list(&self, path: &str) -> Result<Vec<SharedString>> {
                let all = [$(concat!("icons/", $path, ".svg")),+];
                Ok(all
                    .iter()
                    .filter(|p| p.starts_with(path))
                    .map(|p| SharedString::from(*p))
                    .collect())
            }
        }
    };
}

icon_assets![
    // Solar Icons (Linear), CC BY 4.0 — 480 Design.
    (MONITOR, "monitor"),
    (LAPTOP, "laptop"),
    (PEN_NEW_SQUARE, "pen-new-square"),
    (SORT, "sort"),
    (SORT_VERTICAL, "sort-vertical"),
    (CLOCK_CIRCLE, "clock-circle"),
    (CALENDAR, "calendar"),
    (LIST, "list"),
    (FOLDER_WITH_FILES, "folder-with-files"),
    (FOLDER, "folder"),
    // Hand-drawn git-branch glyph in the Solar Linear style (like the
    // terminal/plus/return ports) — the set has no branch icon.
    (GIT_BRANCH, "git-branch"),
    // Provider-neutral pull-request glyph, drawn in the same linear family.
    (PULL_REQUEST, "pull-request"),
    // Compact history-ref glyphs, drawn in the same linear style.
    (CLOUD, "cloud"),
    (TAG, "tag"),
    (SIDEBAR_MINIMALISTIC, "sidebar-minimalistic"),
    // Mirrored variant (zeron window-controls.tsx `-scale-x-100`): the LEFT
    // sidebar toggle shows the panel line on the left; gpui divs have no
    // scale transform at the pinned rev, so the flip is baked into the asset.
    (SIDEBAR_MINIMALISTIC_LEFT, "sidebar-minimalistic-left"),
    (KEY_MINIMALISTIC, "key-minimalistic"),
    (KEYBOARD, "keyboard"),
    (ARROW_LEFT, "arrow-left"),
    (ARROW_RIGHT, "arrow-right"),
    (ARROW_UP, "arrow-up"),
    // arrow-up mirrored (like the sidebar flip) — the Solar Linear set here
    // has no plain arrow-down.
    (ARROW_DOWN, "arrow-down"),
    // arrow-up rotated 45° — the "opens elsewhere" glyph on spawn chips;
    // the set has no diagonal arrow.
    (ARROW_UP_RIGHT, "arrow-up-right"),
    // Hand-drawn return/enter arrow in the Solar Linear style (like the
    // terminal/plus/close ports) — the set has no return glyph.
    (RETURN, "return"),
    (ALT_ARROW_DOWN, "alt-arrow-down"),
    // alt-arrow-down mirrored (drawn as its twin: same stroke, caps, joints) —
    // the embedded Solar Linear set ships no up chevron. The collapse half of
    // the long-user-message expander.
    (ALT_ARROW_UP, "alt-arrow-up"),
    // Hand-drawn expand/maximize arrows in the Solar Linear style (like the
    // terminal/plus/return ports) — the set has no expand glyph.
    (EXPAND_ARROWS, "expand-arrows"),
    // Inward-pointing companion used to restore an expanded pane.
    (COLLAPSE_ARROWS, "collapse-arrows"),
    // Hand-drawn fold-all chevrons, drawn as a family with EXPAND_ARROWS
    // (same stroke, caps, 90° joints) — Solar has no unfold-less either.
    (FOLD_VERTICAL, "fold-vertical"),
    // The changes pane's unified/split toggle: a rounded frame halved by a
    // centre rule (Solar Linear weight).
    (SPLIT_COLUMNS, "split-columns"),
    // Long-line wrapping toggle shared by changes and agent Markdown fences.
    (WRAP_TEXT, "wrap-text"),
    (ALT_ARROW_LEFT, "alt-arrow-left"),
    (ALT_ARROW_RIGHT, "alt-arrow-right"),
    (SMARTPHONE, "smartphone"),
    (ARCHIVE_UP_MINIMALISTIC, "archive-up-minimalistic"),
    (REFRESH, "refresh"),
    (RESTART, "restart"),
    (ADD_CIRCLE, "add-circle"),
    (TUNING, "tuning"),
    (PAPERCLIP, "paperclip"),
    (PEN, "pen"),
    (ARCHIVE_MINIMALISTIC, "archive-minimalistic"),
    (TRASH_BIN_MINIMALISTIC, "trash-bin-minimalistic"),
    (SETTINGS_MINIMALISTIC, "settings-minimalistic"),
    (LOGOUT_2, "logout-2"),
    (MAGNIFER, "magnifer"),
    (COMMAND, "command"),
    (DOCUMENT, "document"),
    (DOCUMENT_ADD, "document-add"),
    // File-kind glyphs, drawn in the same linear family for transcript badges.
    (FILE_CODE, "file-code"),
    (FILE_STYLE, "file-style"),
    (FILE_DATA, "file-data"),
    (FILE_MARKDOWN, "file-markdown"),
    (FILE_IMAGE, "file-image"),
    (GLOBAL, "global"),
    (CHECKLIST, "checklist"),
    (WIDGET, "widget"),
    (WIFI_OFF, "wifi-off"),
    (CLOSE_CIRCLE, "close-circle"),
    // Hand-drawn info glyph in the Solar Linear style (like the terminal/
    // plus/return ports) — the embedded set has no info-circle.
    (INFO_CIRCLE, "info-circle"),
    (DANGER_TRIANGLE, "danger-triangle"),
    (CHAT_ROUND_LINE, "chat-round-line"),
    // Hand-drawn bot head (antenna + eyes + ears) in the Solar Linear style
    // — the embedded set has no bot/robot glyph. Subagent tabs.
    (BOT, "bot"),
    // Hand-drawn bell + speaker in the Solar Linear style (like the terminal/
    // plus/return ports) — the embedded set has neither.
    (BELL, "bell"),
    (VOLUME_LOUD, "volume-loud"),
    // Hand-drawn zeron glyphs (terminal-panel.tsx / composer-actions.tsx /
    // menu-check.tsx / logo.tsx).
    (TERMINAL, "terminal"),
    (PLUS, "plus"),
    (CLOSE, "close"),
    // Hand-drawn Linux caption glyphs (minimize dash, maximize square,
    // restore stacked squares) in the same style as `close` — drawn for the
    // client-side-decoration window controls; no system glyph font exists on
    // Linux the way Segoe Fluent Icons does on Windows.
    (WINDOW_MINIMIZE, "window-minimize"),
    (WINDOW_MAXIMIZE, "window-maximize"),
    (WINDOW_RESTORE, "window-restore"),
    // Hand-drawn hard-drive + home glyphs in the Solar Linear style (like the
    // terminal/plus/return ports) — drawn for the add-space palette's
    // Locations rail; the set has neither.
    (HARD_DRIVE, "hard-drive"),
    (HOME, "home"),
    (STOP, "stop"),
    (CHECK, "check"),
    (COPY, "copy"),
    // Hand-drawn star pair in the Solar Linear style (like the terminal/
    // plus/return ports) — outline for the favorite affordance, bold for the
    // favorited state and the picker's favorites rail tab.
    (STAR, "star"),
    (STAR_BOLD, "star-bold"),
    (ZERON_LOGO, "zeron-logo"),
    // Harness brand marks (icons.tsx).
    (CLAUDE_MARK, "claude-mark"),
    (OPENAI_MARK, "openai-mark"),
    (CURSOR_MARK, "cursor-mark"),
    (DEVIN_MARK, "devin-mark"),
    (GROK_MARK, "grok-mark"),
    (HERMES_MARK, "hermes-mark"),
    (PI_MARK, "pi-mark"),
    (OPENCODE_MARK, "opencode-mark"),
];

/// The Claude mark's brand orange (`#D97757`) — zeron keeps it even on the
/// monochrome surface.
pub fn claude_brand() -> Hsla {
    gpui::rgb(0xD97757).into()
}

/// An icon element for an embedded asset path. Size and colour are set by the
/// caller (`.size(..)`, `.text_color(..)`), matching the web app's
/// `[&_svg]:size-4` idiom.
pub fn icon(path: &'static str) -> Svg {
    svg().path(path).flex_none()
}

/// File badges use the basename so dotted directories cannot affect the icon.
/// Accept either path separator: transcripts can come from a different OS.
pub fn for_file(path: &str) -> &'static str {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    match name.as_str() {
        "dockerfile" | "makefile" | "justfile" | ".bashrc" | ".zshrc" => return TERMINAL,
        ".gitignore" | ".gitattributes" | ".editorconfig" | ".env" => return SETTINGS_MINIMALISTIC,
        "readme" | "license" | "changelog" => return FILE_MARKDOWN,
        _ if name.starts_with(".env.") => return SETTINGS_MINIMALISTIC,
        _ => {}
    }
    match name.rsplit_once('.').map(|(_, extension)| extension) {
        Some("css" | "scss" | "sass" | "less") => FILE_STYLE,
        Some("json" | "jsonc" | "jsonl" | "csv" | "tsv" | "sql") => FILE_DATA,
        Some("toml" | "yaml" | "yml" | "ini" | "conf" | "config" | "properties") => {
            SETTINGS_MINIMALISTIC
        }
        Some("md" | "mdx" | "markdown" | "rst") => FILE_MARKDOWN,
        Some("svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "avif" | "bmp" | "tiff") => {
            FILE_IMAGE
        }
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd") => TERMINAL,
        Some("zip" | "gz" | "tar" | "tgz" | "7z" | "rar" | "bz2" | "xz") => ARCHIVE_MINIMALISTIC,
        Some(
            "rs" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "py" | "pyi"
            | "go" | "rb" | "java" | "kt" | "kts" | "swift" | "c" | "h" | "cc" | "cpp" | "hpp"
            | "cs" | "fs" | "php" | "lua" | "ex" | "exs" | "erl" | "hs" | "scala" | "dart" | "vue"
            | "svelte" | "html" | "htm" | "xml" | "graphql" | "gql" | "proto" | "zig",
        ) => FILE_CODE,
        _ => DOCUMENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_icons_handle_paths_case_and_special_names() {
        for (path, expected) in [
            ("src/styles/globals.css", FILE_STYLE),
            ("C:\\project\\README.MD", FILE_MARKDOWN),
            ("src/component.test.tsx", FILE_CODE),
            ("config/.env.local", SETTINGS_MINIMALISTIC),
            ("Dockerfile", TERMINAL),
            ("assets/LOGO.SVG", FILE_IMAGE),
            ("package.json", FILE_DATA),
            ("folder.css/unknown", DOCUMENT),
            ("file.unrecognized", DOCUMENT),
            ("", DOCUMENT),
        ] {
            assert_eq!(for_file(path), expected, "{path}");
        }
    }

    #[test]
    fn every_registered_icon_loads_and_parses() {
        let assets = Assets;
        for path in assets.list("icons/").unwrap() {
            let bytes = assets
                .load(&path)
                .unwrap()
                .unwrap_or_else(|| panic!("missing asset {path}"));
            let text = std::str::from_utf8(&bytes).expect("icon svg is utf-8");
            assert!(text.contains("<svg"), "{path} is not an svg");
            assert!(text.contains("viewBox"), "{path} lacks a viewBox");
        }
    }

    #[test]
    fn unknown_paths_are_none() {
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }

    #[test]
    fn list_filters_by_prefix() {
        assert!(!Assets.list("icons/").unwrap().is_empty());
        assert!(Assets.list("fonts/").unwrap().is_empty());
    }
}
