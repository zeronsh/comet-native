use std::collections::{HashMap, HashSet};

use zeron_proto::{
    WorkspaceDirectoryPage, WorkspaceEntry, WorkspaceEntryKind, WorkspaceFileSearchMatch,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryLoadState {
    Unloaded,
    Loading {
        cursor: Option<String>,
    },
    Loaded {
        next_cursor: Option<String>,
    },
    Error {
        message: String,
        cursor: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub entry: WorkspaceEntry,
    pub children: Vec<String>,
    pub load: DirectoryLoadState,
    pub stale: bool,
}

impl TreeNode {
    fn new(entry: WorkspaceEntry) -> Self {
        let load = if entry.kind == WorkspaceEntryKind::Directory {
            DirectoryLoadState::Unloaded
        } else {
            DirectoryLoadState::Loaded { next_cursor: None }
        };
        Self {
            entry,
            children: Vec::new(),
            load,
            stale: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleRowKind {
    Entry,
    Loading { directory: String },
    Empty { directory: String },
    Error { directory: String, message: String },
    LoadMore { directory: String, cursor: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleTreeRow {
    pub path: String,
    pub depth: usize,
    pub kind: VisibleRowKind,
}

impl VisibleTreeRow {
    pub fn selectable(&self) -> bool {
        matches!(
            self.kind,
            VisibleRowKind::Entry | VisibleRowKind::LoadMore { .. }
        )
    }
}

#[derive(Debug, Clone)]
pub struct FileTreeModel {
    nodes: HashMap<String, TreeNode>,
    expanded: HashSet<String>,
    visible_rows: Vec<VisibleTreeRow>,
    selected: Option<String>,
    include_ignored: bool,
    generation: u64,
}

impl Default for FileTreeModel {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTreeModel {
    pub fn new() -> Self {
        Self::with_include_ignored(false)
    }

    pub fn with_include_ignored(include_ignored: bool) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(String::new(), TreeNode::new(root_entry()));
        Self {
            nodes,
            expanded: HashSet::from([String::new()]),
            visible_rows: Vec::new(),
            selected: None,
            include_ignored,
            generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn include_ignored(&self) -> bool {
        self.include_ignored
    }

    pub fn visible_rows(&self) -> &[VisibleTreeRow] {
        &self.visible_rows
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn node(&self, path: &str) -> Option<&TreeNode> {
        self.nodes.get(path)
    }

    pub fn is_expanded(&self, path: &str) -> bool {
        self.expanded.contains(path)
    }

    pub fn expanded_directories(&self) -> Vec<String> {
        let mut paths = self
            .expanded
            .iter()
            .filter(|path| !path.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| (path.matches('/').count(), path.clone()));
        paths
    }

    pub fn is_directory_loaded(&self, path: &str) -> bool {
        self.nodes.get(path).is_some_and(|node| {
            node.entry.kind == WorkspaceEntryKind::Directory
                && matches!(node.load, DirectoryLoadState::Loaded { .. })
        })
    }

    pub fn reset(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.nodes.clear();
        self.nodes
            .insert(String::new(), TreeNode::new(root_entry()));
        self.expanded.clear();
        self.expanded.insert(String::new());
        self.visible_rows.clear();
        self.selected = None;
        self.generation
    }

    pub fn set_include_ignored(&mut self, include_ignored: bool) -> bool {
        if self.include_ignored == include_ignored {
            return false;
        }
        self.include_ignored = include_ignored;
        self.reset();
        true
    }

    pub fn begin_load(&mut self, directory: &str, cursor: Option<String>, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(node) = self.nodes.get_mut(directory) else {
            return false;
        };
        if matches!(node.load, DirectoryLoadState::Loading { .. }) {
            return false;
        }
        node.load = DirectoryLoadState::Loading { cursor };
        node.stale = false;
        self.rebuild_visible_rows();
        true
    }

    pub fn fail_load(
        &mut self,
        directory: &str,
        cursor: Option<String>,
        message: impl Into<String>,
        generation: u64,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(node) = self.nodes.get_mut(directory) else {
            return false;
        };
        node.load = DirectoryLoadState::Error {
            message: message.into(),
            cursor,
        };
        self.rebuild_visible_rows();
        true
    }

    pub fn apply_page(&mut self, page: WorkspaceDirectoryPage, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        let directory = page.directory;
        let Some(parent) = self.nodes.get(&directory) else {
            return false;
        };
        if parent.entry.kind != WorkspaceEntryKind::Directory {
            return false;
        }
        let parent_ignored = parent.entry.ignored;

        let append = matches!(parent.load, DirectoryLoadState::Loading { cursor: Some(_) });
        if !append {
            let previous = parent.children.clone();
            for child in previous {
                self.remove_subtree(&child);
            }
            if let Some(parent) = self.nodes.get_mut(&directory) {
                parent.children.clear();
            }
        }

        let mut known_children = self
            .nodes
            .get(&directory)
            .map(|node| node.children.iter().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();
        for mut entry in page.entries {
            if !is_direct_child(&entry.path, &directory) {
                continue;
            }
            entry.ignored |= parent_ignored;
            let path = entry.path.clone();
            self.nodes
                .entry(path.clone())
                .and_modify(|node| node.entry = entry.clone())
                .or_insert_with(|| TreeNode::new(entry));
            if let Some(parent) = self.nodes.get_mut(&directory)
                && known_children.insert(path.clone())
            {
                parent.children.push(path);
            }
        }

        let mut children = self
            .nodes
            .get(&directory)
            .map(|node| node.children.clone())
            .unwrap_or_default();
        children.sort_by(|left, right| compare_paths(&self.nodes, left, right));
        if let Some(parent) = self.nodes.get_mut(&directory) {
            parent.children = children;
            parent.load = DirectoryLoadState::Loaded {
                next_cursor: page.next_cursor,
            };
            parent.stale = false;
        }
        self.rebuild_visible_rows();
        true
    }

    pub fn toggle_expanded(&mut self, path: &str) -> bool {
        let Some(node) = self.nodes.get(path) else {
            return false;
        };
        if node.entry.kind != WorkspaceEntryKind::Directory {
            return false;
        }
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_string());
        } else if self
            .selected
            .as_deref()
            .is_some_and(|selected| is_descendant(selected, path))
        {
            self.selected = (!path.is_empty()).then(|| path.to_string());
        }
        self.rebuild_visible_rows();
        true
    }

    pub fn expand(&mut self, path: &str) -> bool {
        if self
            .nodes
            .get(path)
            .is_none_or(|node| node.entry.kind != WorkspaceEntryKind::Directory)
        {
            return false;
        }
        let changed = self.expanded.insert(path.to_string());
        if changed {
            self.rebuild_visible_rows();
        }
        changed
    }

    pub fn select(&mut self, path: impl Into<String>) -> bool {
        let path = path.into();
        if !self.nodes.contains_key(&path) {
            return false;
        }
        self.selected = Some(path);
        true
    }

    pub fn select_next(&mut self) -> Option<&str> {
        self.move_selection(1)
    }

    pub fn select_previous(&mut self) -> Option<&str> {
        self.move_selection(-1)
    }

    pub fn select_parent(&mut self) -> Option<&str> {
        let selected = self.selected.clone()?;
        let parent = parent_path(&selected)?;
        if parent.is_empty() {
            return self.selected.as_deref();
        }
        self.selected = Some(parent);
        self.selected.as_deref()
    }

    pub fn select_first_child(&mut self) -> Option<&str> {
        let selected = self.selected.clone()?;
        let first = self.nodes.get(&selected)?.children.first()?.clone();
        self.selected = Some(first);
        self.selected.as_deref()
    }

    pub fn invalidate_directory(&mut self, directory: &str) -> bool {
        let Some(node) = self.nodes.get_mut(directory) else {
            return false;
        };
        if node.entry.kind != WorkspaceEntryKind::Directory {
            return false;
        }
        node.stale = true;
        true
    }

    pub fn remove(&mut self, path: &str) -> bool {
        if path.is_empty() || !self.nodes.contains_key(path) {
            return false;
        }
        if let Some(parent) = parent_path(path).and_then(|path| self.nodes.get_mut(&path)) {
            parent.children.retain(|child| child != path);
        }
        self.remove_subtree(path);
        if self
            .selected
            .as_deref()
            .is_some_and(|selected| selected == path || is_descendant(selected, path))
        {
            self.selected = parent_path(path).filter(|parent| !parent.is_empty());
        }
        self.rebuild_visible_rows();
        true
    }

    pub fn reveal_search_match(&mut self, result: &WorkspaceFileSearchMatch) -> Vec<String> {
        let mut ancestors = Vec::new();
        let mut current = parent_path(&result.path);
        while let Some(path) = current {
            if path.is_empty() {
                break;
            }
            ancestors.push(path.clone());
            current = parent_path(&path);
        }
        ancestors.reverse();
        ancestors
    }

    fn move_selection(&mut self, delta: isize) -> Option<&str> {
        let selectable = self
            .visible_rows
            .iter()
            .filter(|row| row.selectable())
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            self.selected = None;
            return None;
        }
        let current = self
            .selected
            .as_deref()
            .and_then(|path| selectable.iter().position(|candidate| *candidate == path));
        let next = match (current, delta.is_negative()) {
            (Some(index), true) => index.saturating_sub(delta.unsigned_abs()),
            (Some(index), false) => (index + delta as usize).min(selectable.len() - 1),
            (None, true) => selectable.len() - 1,
            (None, false) => 0,
        };
        self.selected = Some(selectable[next].to_string());
        self.selected.as_deref()
    }

    fn remove_subtree(&mut self, path: &str) {
        let children = self
            .nodes
            .get(path)
            .map(|node| node.children.clone())
            .unwrap_or_default();
        for child in children {
            self.remove_subtree(&child);
        }
        self.nodes.remove(path);
        self.expanded.remove(path);
    }

    fn rebuild_visible_rows(&mut self) {
        let mut rows = Vec::new();
        let mut visited = HashSet::from([String::new()]);
        self.append_directory_rows("", 0, &mut rows, &mut visited);
        self.visible_rows = rows;
        if self
            .selected
            .as_deref()
            .is_some_and(|selected| !self.visible_rows.iter().any(|row| row.path == selected))
        {
            self.selected = None;
        }
    }

    fn append_directory_rows(
        &self,
        directory: &str,
        depth: usize,
        rows: &mut Vec<VisibleTreeRow>,
        visited: &mut HashSet<String>,
    ) {
        let Some(node) = self.nodes.get(directory) else {
            return;
        };
        for child_path in &node.children {
            if !visited.insert(child_path.clone()) {
                continue;
            }
            let Some(child) = self.nodes.get(child_path) else {
                continue;
            };
            rows.push(VisibleTreeRow {
                path: child_path.clone(),
                depth,
                kind: VisibleRowKind::Entry,
            });
            if child.entry.kind == WorkspaceEntryKind::Directory
                && self.expanded.contains(child_path)
            {
                self.append_directory_rows(child_path, depth + 1, rows, visited);
            }
        }

        if !self.expanded.contains(directory) {
            return;
        }
        match &node.load {
            DirectoryLoadState::Unloaded => {}
            DirectoryLoadState::Loading { .. } => rows.push(VisibleTreeRow {
                path: synthetic_path(directory, "loading"),
                depth,
                kind: VisibleRowKind::Loading {
                    directory: directory.to_string(),
                },
            }),
            DirectoryLoadState::Loaded { next_cursor } => {
                if node.children.is_empty() {
                    rows.push(VisibleTreeRow {
                        path: synthetic_path(directory, "empty"),
                        depth,
                        kind: VisibleRowKind::Empty {
                            directory: directory.to_string(),
                        },
                    });
                }
                if let Some(cursor) = next_cursor {
                    rows.push(VisibleTreeRow {
                        path: synthetic_path(directory, "more"),
                        depth,
                        kind: VisibleRowKind::LoadMore {
                            directory: directory.to_string(),
                            cursor: cursor.clone(),
                        },
                    });
                }
            }
            DirectoryLoadState::Error { message, .. } => rows.push(VisibleTreeRow {
                path: synthetic_path(directory, "error"),
                depth,
                kind: VisibleRowKind::Error {
                    directory: directory.to_string(),
                    message: message.clone(),
                },
            }),
        }
    }
}

fn root_entry() -> WorkspaceEntry {
    WorkspaceEntry {
        path: String::new(),
        name: String::new(),
        kind: WorkspaceEntryKind::Directory,
        size: None,
        modified_at: None,
        ignored: false,
        read_only: false,
    }
}

fn compare_paths(nodes: &HashMap<String, TreeNode>, left: &str, right: &str) -> std::cmp::Ordering {
    let left = &nodes[left].entry;
    let right = &nodes[right].entry;
    entry_rank(left.kind)
        .cmp(&entry_rank(right.kind))
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.path.cmp(&right.path))
}

fn entry_rank(kind: WorkspaceEntryKind) -> u8 {
    match kind {
        WorkspaceEntryKind::Directory => 0,
        WorkspaceEntryKind::File => 1,
        WorkspaceEntryKind::Symlink => 2,
    }
}

pub fn parent_path(path: &str) -> Option<String> {
    path.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .or_else(|| (!path.is_empty()).then(String::new))
}

fn is_descendant(candidate: &str, ancestor: &str) -> bool {
    if ancestor.is_empty() {
        return !candidate.is_empty();
    }
    candidate
        .strip_prefix(ancestor)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_direct_child(candidate: &str, directory: &str) -> bool {
    parent_path(candidate).as_deref() == Some(directory)
}

fn synthetic_path(directory: &str, kind: &str) -> String {
    format!("{directory}\0{kind}")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn entry(path: &str, kind: WorkspaceEntryKind) -> WorkspaceEntry {
        WorkspaceEntry {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            kind,
            size: (kind == WorkspaceEntryKind::File).then_some(12),
            modified_at: Some(Utc::now()),
            ignored: false,
            read_only: kind == WorkspaceEntryKind::Symlink,
        }
    }

    fn page(
        directory: &str,
        entries: Vec<WorkspaceEntry>,
        next_cursor: Option<&str>,
    ) -> WorkspaceDirectoryPage {
        WorkspaceDirectoryPage {
            directory: directory.into(),
            entries,
            next_cursor: next_cursor.map(str::to_string),
            truncated: next_cursor.is_some(),
        }
    }

    #[test]
    fn root_page_is_sorted_and_flattened() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        tree.apply_page(
            page(
                "",
                vec![
                    entry("z.rs", WorkspaceEntryKind::File),
                    entry("src", WorkspaceEntryKind::Directory),
                    entry("a.rs", WorkspaceEntryKind::File),
                    entry("link", WorkspaceEntryKind::Symlink),
                ],
                None,
            ),
            generation,
        );
        assert_eq!(
            tree.visible_rows
                .iter()
                .filter(|row| matches!(row.kind, VisibleRowKind::Entry))
                .map(|row| row.path.as_str())
                .collect::<Vec<_>>(),
            ["src", "a.rs", "z.rs", "link"]
        );
    }

    #[test]
    fn expanded_directories_add_rows_at_the_correct_depth() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        tree.apply_page(
            page("", vec![entry("src", WorkspaceEntryKind::Directory)], None),
            generation,
        );
        tree.expand("src");
        tree.begin_load("src", None, generation);
        tree.apply_page(
            page(
                "src",
                vec![entry("src/lib.rs", WorkspaceEntryKind::File)],
                None,
            ),
            generation,
        );
        let row = tree
            .visible_rows
            .iter()
            .find(|row| row.path == "src/lib.rs")
            .unwrap();
        assert_eq!(row.depth, 1);
    }

    #[test]
    fn ignored_directories_propagate_ignored_state_to_all_descendants() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        let mut ignored_directory = entry("target", WorkspaceEntryKind::Directory);
        ignored_directory.ignored = true;

        tree.begin_load("", None, generation);
        tree.apply_page(page("", vec![ignored_directory], None), generation);
        tree.begin_load("target", None, generation);
        tree.apply_page(
            page(
                "target",
                vec![
                    entry("target/cache", WorkspaceEntryKind::Directory),
                    entry("target/output.bin", WorkspaceEntryKind::File),
                ],
                None,
            ),
            generation,
        );
        tree.begin_load("target/cache", None, generation);
        tree.apply_page(
            page(
                "target/cache",
                vec![entry("target/cache/nested.bin", WorkspaceEntryKind::File)],
                None,
            ),
            generation,
        );

        assert!(tree.node("target/cache").unwrap().entry.ignored);
        assert!(tree.node("target/output.bin").unwrap().entry.ignored);
        assert!(tree.node("target/cache/nested.bin").unwrap().entry.ignored);
    }

    #[test]
    fn collapsing_an_ancestor_moves_hidden_selection_to_it() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        tree.apply_page(
            page("", vec![entry("src", WorkspaceEntryKind::Directory)], None),
            generation,
        );
        tree.expand("src");
        tree.begin_load("src", None, generation);
        tree.apply_page(
            page(
                "src",
                vec![entry("src/lib.rs", WorkspaceEntryKind::File)],
                None,
            ),
            generation,
        );
        tree.select("src/lib.rs");
        tree.toggle_expanded("src");
        assert_eq!(tree.selected(), Some("src"));
    }

    #[test]
    fn pagination_appends_and_deduplicates_entries() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        tree.apply_page(
            page(
                "",
                vec![entry("a.rs", WorkspaceEntryKind::File)],
                Some("next"),
            ),
            generation,
        );
        tree.begin_load("", Some("next".into()), generation);
        tree.apply_page(
            page(
                "",
                vec![
                    entry("a.rs", WorkspaceEntryKind::File),
                    entry("b.rs", WorkspaceEntryKind::File),
                ],
                None,
            ),
            generation,
        );
        let root = tree.node("").unwrap();
        assert_eq!(root.children, ["a.rs", "b.rs"]);
    }

    #[test]
    fn directory_pages_reject_self_parents_and_ancestors() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        tree.apply_page(
            page("", vec![entry("src", WorkspaceEntryKind::Directory)], None),
            generation,
        );
        tree.begin_load("src", None, generation);

        assert!(tree.apply_page(
            page(
                "src",
                vec![
                    entry("src", WorkspaceEntryKind::Directory),
                    entry("", WorkspaceEntryKind::Directory),
                    entry("src/lib.rs", WorkspaceEntryKind::File),
                ],
                None,
            ),
            generation,
        ));

        assert_eq!(tree.node("src").unwrap().children, ["src/lib.rs"]);
        assert_eq!(tree.node("").unwrap().children, ["src"]);
    }

    #[test]
    fn directory_pages_reject_non_direct_descendants() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);

        assert!(tree.apply_page(
            page(
                "",
                vec![
                    entry("src", WorkspaceEntryKind::Directory),
                    entry("src/nested.rs", WorkspaceEntryKind::File),
                    entry("README.md", WorkspaceEntryKind::File),
                ],
                None,
            ),
            generation,
        ));

        assert_eq!(tree.node("").unwrap().children, ["src", "README.md"]);
        assert!(tree.node("src/nested.rs").is_none());
    }

    #[test]
    fn visible_rows_stop_at_cycles_in_a_corrupted_model() {
        let mut tree = FileTreeModel::new();
        tree.nodes.insert(
            "loop".into(),
            TreeNode::new(entry("loop", WorkspaceEntryKind::Directory)),
        );
        tree.nodes.get_mut("").unwrap().children.push("loop".into());
        tree.nodes
            .get_mut("loop")
            .unwrap()
            .children
            .push("loop".into());
        tree.expanded.insert("loop".into());

        tree.rebuild_visible_rows();

        assert_eq!(
            tree.visible_rows()
                .iter()
                .filter(|row| matches!(row.kind, VisibleRowKind::Entry))
                .map(|row| row.path.as_str())
                .collect::<Vec<_>>(),
            ["loop"]
        );
    }

    #[test]
    fn stale_generation_cannot_replace_the_tree() {
        let mut tree = FileTreeModel::new();
        let stale = tree.generation();
        tree.reset();
        assert!(!tree.apply_page(
            page("", vec![entry("stale.rs", WorkspaceEntryKind::File)], None),
            stale,
        ));
        assert!(tree.node("stale.rs").is_none());
    }

    #[test]
    fn removing_a_directory_removes_all_descendants() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        tree.apply_page(
            page("", vec![entry("src", WorkspaceEntryKind::Directory)], None),
            generation,
        );
        tree.expand("src");
        tree.begin_load("src", None, generation);
        tree.apply_page(
            page(
                "src",
                vec![entry("src/lib.rs", WorkspaceEntryKind::File)],
                None,
            ),
            generation,
        );
        assert!(tree.remove("src"));
        assert!(tree.node("src").is_none());
        assert!(tree.node("src/lib.rs").is_none());
    }

    #[test]
    fn empty_error_and_load_more_rows_follow_load_state() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        assert!(matches!(
            tree.visible_rows.last().map(|row| &row.kind),
            Some(VisibleRowKind::Loading { .. })
        ));
        tree.fail_load("", None, "offline", generation);
        assert!(matches!(
            tree.visible_rows.last().map(|row| &row.kind),
            Some(VisibleRowKind::Error { .. })
        ));
        tree.begin_load("", None, generation);
        tree.apply_page(page("", Vec::new(), None), generation);
        assert!(matches!(
            tree.visible_rows.last().map(|row| &row.kind),
            Some(VisibleRowKind::Empty { .. })
        ));
    }

    #[test]
    fn keyboard_selection_follows_visible_rows() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        tree.apply_page(
            page(
                "",
                vec![
                    entry("src", WorkspaceEntryKind::Directory),
                    entry("README.md", WorkspaceEntryKind::File),
                ],
                None,
            ),
            generation,
        );
        assert_eq!(tree.select_next(), Some("src"));
        assert_eq!(tree.select_next(), Some("README.md"));
        assert_eq!(tree.select_previous(), Some("src"));
    }

    #[test]
    fn an_inflight_directory_load_is_not_duplicated() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        assert!(tree.begin_load("", None, generation));
        assert!(!tree.begin_load("", None, generation));
    }

    #[test]
    fn a_large_monorepo_page_stays_flat_and_addressable() {
        let mut tree = FileTreeModel::new();
        let generation = tree.generation();
        tree.begin_load("", None, generation);
        let entries = (0..10_000)
            .map(|index| entry(&format!("package-{index:05}.rs"), WorkspaceEntryKind::File))
            .chain([
                entry("arquitectura rápida.md", WorkspaceEntryKind::File),
                entry("packages", WorkspaceEntryKind::Directory),
            ])
            .collect();
        assert!(tree.apply_page(page("", entries, None), generation));
        assert_eq!(tree.node("").unwrap().children.len(), 10_002);
        assert_eq!(tree.visible_rows().len(), 10_002);
        assert!(tree.node("arquitectura rápida.md").is_some());
        assert_eq!(tree.visible_rows()[0].path, "packages");
    }

    #[test]
    fn reset_discards_loaded_content_and_invalidates_inflight_pages() {
        let mut tree = FileTreeModel::new();
        let stale_generation = tree.generation();
        tree.begin_load("", None, stale_generation);
        tree.apply_page(
            page(
                "",
                vec![entry("secret.env", WorkspaceEntryKind::File)],
                None,
            ),
            stale_generation,
        );
        let current_generation = tree.reset();
        assert_ne!(current_generation, stale_generation);
        assert!(tree.node("secret.env").is_none());
        assert!(tree.visible_rows().is_empty());
        assert!(!tree.apply_page(
            page("", vec![entry("stale.env", WorkspaceEntryKind::File)], None),
            stale_generation,
        ));
    }
}
