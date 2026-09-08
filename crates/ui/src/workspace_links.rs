//! Safe resolution of agent-authored Markdown links into workspace files.

use std::path::{Component, Path};

const FILE_MENTION_SCHEME: &str = "zeron-file:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceFileLink {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

pub(crate) fn resolve_workspace_file_link(
    target: &str,
    workspace_root: &str,
) -> Option<WorkspaceFileLink> {
    let target = target.trim();
    if target.is_empty() || target.contains(['?', '\0']) {
        return None;
    }

    let decoded;
    let is_file_mention = target.starts_with(FILE_MENTION_SCHEME);
    let mut target = if let Some(encoded) = target.strip_prefix(FILE_MENTION_SCHEME) {
        decoded = percent_decode_path(encoded)?;
        if percent_encode_path(&decoded) != encoded || decoded.ends_with('/') {
            return None;
        }
        decoded.as_str()
    } else if let Some(path) = target.strip_prefix("file://") {
        path
    } else if target.contains("://") || target.starts_with("mailto:") {
        return None;
    } else {
        target
    };

    let (without_fragment, fragment_line) = if is_file_mention {
        (target, None)
    } else {
        split_line_fragment(target)
    };
    target = without_fragment;
    let (target, suffix_line, column) = if is_file_mention {
        (target, None, None)
    } else {
        split_line_suffix(target)
    };
    if target.contains(['\\', '\n', '\r'])
        || target
            .split('/')
            .enumerate()
            .any(|(index, part)| (part.is_empty() && index != 0) || matches!(part, "." | ".."))
    {
        return None;
    }
    if target.contains(':') {
        return None;
    }
    let line = fragment_line.or(suffix_line);

    let root = Path::new(workspace_root);
    let relative = if Path::new(target).is_absolute() {
        Path::new(target).strip_prefix(root).ok()?
    } else {
        Path::new(target)
    };
    let path = safe_relative_path(relative)?;
    Some(WorkspaceFileLink { path, line, column })
}

fn safe_relative_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn split_line_fragment(target: &str) -> (&str, Option<u32>) {
    let Some((path, fragment)) = target.rsplit_once('#') else {
        return (target, None);
    };
    let line = fragment
        .strip_prefix('L')
        .and_then(|line| line.parse::<u32>().ok())
        .filter(|line| *line > 0);
    if line.is_some() {
        (path, line)
    } else {
        (target, None)
    }
}

fn split_line_suffix(target: &str) -> (&str, Option<u32>, Option<u32>) {
    let mut pieces = target.rsplitn(3, ':');
    let last = pieces.next().unwrap_or_default();
    let Some(before_last) = pieces.next() else {
        return (target, None, None);
    };
    let Some(last_number) = positive_number(last) else {
        return (target, None, None);
    };
    if let Some(path) = pieces.next()
        && let Some(line) = positive_number(before_last)
    {
        return (path, Some(line), Some(last_number));
    }
    let path_len = target.len() - last.len() - 1;
    (&target[..path_len], Some(last_number), None)
}

fn positive_number(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|number| *number > 0)
}

fn percent_decode_path(encoded: &str) -> Option<String> {
    let raw = encoded.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_absolute_and_location_links() {
        let root = "/work/comet";
        assert_eq!(
            resolve_workspace_file_link("crates/ui/src/lib.rs", root),
            Some(WorkspaceFileLink {
                path: "crates/ui/src/lib.rs".into(),
                line: None,
                column: None,
            })
        );
        assert_eq!(
            resolve_workspace_file_link("/work/comet/crates/ui/src/lib.rs:42:7", root),
            Some(WorkspaceFileLink {
                path: "crates/ui/src/lib.rs".into(),
                line: Some(42),
                column: Some(7),
            })
        );
        assert_eq!(
            resolve_workspace_file_link("file:///work/comet/README.md#L12", root),
            Some(WorkspaceFileLink {
                path: "README.md".into(),
                line: Some(12),
                column: None,
            })
        );
    }

    #[test]
    fn resolves_canonical_file_mentions() {
        assert_eq!(
            resolve_workspace_file_link("zeron-file:src/a%20file.rs", "/work/comet"),
            Some(WorkspaceFileLink {
                path: "src/a file.rs".into(),
                line: None,
                column: None,
            })
        );
        assert!(resolve_workspace_file_link("zeron-file:src/%61.rs", "/work/comet").is_none());
        assert!(resolve_workspace_file_link("zeron-file:src/", "/work/comet").is_none());
    }

    #[test]
    fn rejects_external_and_unsafe_targets() {
        let root = "/work/comet";
        for target in [
            "https://example.com/file.rs",
            "mailto:dev@example.com",
            "/work/comet-other/src/lib.rs",
            "/tmp/file.rs",
            "../secret.rs",
            "src/../../secret.rs",
            "src/./lib.rs",
            "src//lib.rs",
        ] {
            assert!(
                resolve_workspace_file_link(target, root).is_none(),
                "accepted {target}"
            );
        }
    }
}
