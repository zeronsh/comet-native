use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use gpui::{
    AnyElement, Context, ListSizingBehavior, SharedString, Task, Window, div, list, prelude::*, px,
};
use zeron_proto::{
    ListWorkspaceDirectoryRequest, SearchWorkspaceFilesRequest, WorkspaceEntryKind,
    WorkspaceFileSearchMatch,
};

use super::{
    FilesSurface, WorkspacePathDrag, client::WorkspaceFilesClient, model::parent_path,
    workspace_path_drag_ghost,
};
use crate::{
    icons::{self, icon},
    theme::Theme,
};

pub const SEARCH_ROW_HEIGHT: f32 = 27.0;
const SEARCH_TREE_INDENT: f32 = 14.0;
const SEARCH_RESULT_LIMIT: usize = 200;

#[derive(Debug, Clone)]
struct SearchTreeNode {
    path: String,
    name: String,
    kind: WorkspaceEntryKind,
    score: Option<i64>,
    best_score: i64,
    children: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchTreeRow {
    path: String,
    name: String,
    kind: WorkspaceEntryKind,
    depth: usize,
    has_children: bool,
    score: i64,
}

impl SearchTreeRow {
    fn as_match(&self) -> WorkspaceFileSearchMatch {
        WorkspaceFileSearchMatch {
            path: self.path.clone(),
            name: self.name.clone(),
            kind: self.kind,
            score: self.score,
        }
    }
}

#[derive(Debug, Default)]
struct SearchTreeModel {
    nodes: HashMap<String, SearchTreeNode>,
    roots: Vec<String>,
    collapsed: HashSet<String>,
    rows: Vec<SearchTreeRow>,
}

impl SearchTreeModel {
    fn clear(&mut self) {
        self.nodes.clear();
        self.roots.clear();
        self.collapsed.clear();
        self.rows.clear();
    }

    fn rebuild(&mut self, results: &[WorkspaceFileSearchMatch]) {
        self.clear();
        for result in results {
            let mut parent = None;
            let mut path = String::new();
            for component in result
                .path
                .split('/')
                .filter(|component| !component.is_empty())
            {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(component);
                let is_match = path == result.path;
                self.nodes
                    .entry(path.clone())
                    .and_modify(|node| {
                        if is_match {
                            node.name = result.name.clone();
                            node.kind = result.kind;
                            node.score = Some(node.score.unwrap_or(i64::MIN).max(result.score));
                        }
                    })
                    .or_insert_with(|| SearchTreeNode {
                        path: path.clone(),
                        name: if is_match {
                            result.name.clone()
                        } else {
                            component.to_string()
                        },
                        kind: if is_match {
                            result.kind
                        } else {
                            WorkspaceEntryKind::Directory
                        },
                        score: is_match.then_some(result.score),
                        best_score: result.score,
                        children: Vec::new(),
                    });
                if let Some(parent) = parent.as_ref() {
                    let children = &mut self
                        .nodes
                        .get_mut(parent)
                        .expect("search ancestor must exist")
                        .children;
                    if !children.contains(&path) {
                        children.push(path.clone());
                    }
                } else if !self.roots.contains(&path) {
                    self.roots.push(path.clone());
                }
                parent = Some(path.clone());
            }
        }
        let roots = self.roots.clone();
        for root in &roots {
            update_best_score(root, &mut self.nodes);
        }
        sort_search_paths(&mut self.roots, &self.nodes);
        for root in roots {
            sort_search_branch(&root, &mut self.nodes);
        }
        self.rebuild_rows();
    }

    fn rows(&self) -> &[SearchTreeRow] {
        &self.rows
    }

    fn toggle(&mut self, path: &str) -> bool {
        if !self
            .nodes
            .get(path)
            .is_some_and(|node| !node.children.is_empty())
        {
            return false;
        }
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_string());
        }
        self.rebuild_rows();
        true
    }

    fn is_expanded(&self, path: &str) -> bool {
        !self.collapsed.contains(path)
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for root in self.roots.clone() {
            append_search_rows(&root, 0, &self.nodes, &self.collapsed, &mut rows);
        }
        self.rows = rows;
    }
}

fn update_best_score(path: &str, nodes: &mut HashMap<String, SearchTreeNode>) -> i64 {
    let Some(node) = nodes.get(path) else {
        return i64::MIN;
    };
    let children = node.children.clone();
    let mut best_score = node.score.unwrap_or(i64::MIN);
    for child in children {
        best_score = best_score.max(update_best_score(&child, nodes));
    }
    if let Some(node) = nodes.get_mut(path) {
        node.best_score = best_score;
    }
    best_score
}

fn sort_search_branch(path: &str, nodes: &mut HashMap<String, SearchTreeNode>) {
    let children = nodes
        .get(path)
        .map(|node| node.children.clone())
        .unwrap_or_default();
    for child in &children {
        sort_search_branch(child, nodes);
    }
    let mut children = children;
    sort_search_paths(&mut children, nodes);
    if let Some(node) = nodes.get_mut(path) {
        node.children = children;
    }
}

fn sort_search_paths(paths: &mut [String], nodes: &HashMap<String, SearchTreeNode>) {
    paths.sort_by(|left, right| {
        let left = nodes.get(left).expect("search node must exist");
        let right = nodes.get(right).expect("search node must exist");
        right
            .best_score
            .cmp(&left.best_score)
            .then_with(|| {
                let left_directory = left.kind == WorkspaceEntryKind::Directory;
                let right_directory = right.kind == WorkspaceEntryKind::Directory;
                right_directory.cmp(&left_directory)
            })
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn append_search_rows(
    path: &str,
    depth: usize,
    nodes: &HashMap<String, SearchTreeNode>,
    collapsed: &HashSet<String>,
    rows: &mut Vec<SearchTreeRow>,
) {
    let Some(node) = nodes.get(path) else {
        return;
    };
    let has_children = !node.children.is_empty();
    rows.push(SearchTreeRow {
        path: node.path.clone(),
        name: node.name.clone(),
        kind: node.kind,
        depth,
        has_children,
        score: node.score.unwrap_or(node.best_score),
    });
    if has_children && !collapsed.contains(path) {
        for child in &node.children {
            append_search_rows(child, depth + 1, nodes, collapsed, rows);
        }
    }
}

#[derive(Default)]
pub(super) struct FileSearchState {
    pub query: String,
    pub results: Vec<WorkspaceFileSearchMatch>,
    pub loading: bool,
    pub error: Option<SharedString>,
    pub generation: u64,
    pub active: usize,
    pub task: Option<Task<()>>,
    pub reveal_task: Option<Task<()>>,
    tree: SearchTreeModel,
}

impl FileSearchState {
    fn accepts(&self, generation: u64, query: &str) -> bool {
        self.generation == generation && self.query == query
    }

    pub(super) fn visible_len(&self) -> usize {
        self.tree.rows().len()
    }
}

impl FilesSurface {
    pub(super) fn on_search_edited(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_string();
        if self.search_state.query == query {
            return;
        }
        self.search_state.generation = self.search_state.generation.wrapping_add(1);
        self.search_state.query = query.clone();
        self.search_state.active = 0;
        self.search_state.error = None;
        self.search_state.task = None;
        if query.is_empty() {
            self.search_state.loading = false;
            self.search_state.results.clear();
            self.search_state.tree.clear();
            self.search_list.reset(0);
            self.search.update(cx, |search, cx| {
                search.set_mention_controls(false, false, cx)
            });
            cx.notify();
            return;
        }
        self.search.update(cx, |search, cx| {
            search.set_mention_controls(true, false, cx)
        });
        let Some(context) = self.request_context.clone() else {
            self.search_state.loading = false;
            self.search_state.error = Some("No workspace available for this chat.".into());
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.search_state.loading = false;
            self.search_state.error = Some("Workspace service is still starting.".into());
            cx.notify();
            return;
        };
        self.search_state.loading = true;
        let generation = self.search_state.generation;
        let request = SearchWorkspaceFilesRequest {
            target: context.target.clone(),
            query: query.clone(),
            include_ignored: self.tree.include_ignored(),
            limit: Some(SEARCH_RESULT_LIMIT as u16),
        };
        let client = WorkspaceFilesClient::new(engine, context);
        self.search_state.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let result = client.search(request).await;
            let _ = this.update(cx, |surface, cx| {
                if !surface.search_state.accepts(generation, &query) {
                    return;
                }
                surface.search_state.loading = false;
                match result {
                    Ok(results) => {
                        surface.search_state.error = None;
                        surface.search_state.results = results;
                        surface
                            .search_state
                            .tree
                            .rebuild(&surface.search_state.results);
                        surface.search_state.active = 0;
                    }
                    Err(error) => {
                        surface.search_state.error = Some(error.to_string().into());
                    }
                }
                surface.search_list.reset_with_uniform_height(
                    surface.search_state.tree.rows().len(),
                    px(SEARCH_ROW_HEIGHT),
                );
                let has_results = !surface.search_state.tree.rows().is_empty();
                surface.search.update(cx, |search, cx| {
                    search.set_mention_controls(true, has_results, cx)
                });
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search.update(cx, |search, cx| search.set_text("", cx));
    }

    pub(super) fn activate_search_result(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self
            .search_state
            .tree
            .rows()
            .get(self.search_state.active)
            .cloned()
        else {
            return;
        };
        if row.kind == WorkspaceEntryKind::Directory
            && row.has_children
            && self.search_state.tree.toggle(&row.path)
        {
            self.search_list.reset_with_uniform_height(
                self.search_state.tree.rows().len(),
                px(SEARCH_ROW_HEIGHT),
            );
            self.search_state.active = self
                .search_state
                .tree
                .rows()
                .iter()
                .position(|candidate| candidate.path == row.path)
                .unwrap_or(0);
            self.search_list
                .scroll_to_reveal_item(self.search_state.active);
            cx.notify();
            return;
        }
        self.reveal_search_result(row.as_match(), cx);
    }

    pub(super) fn reveal_search_result(
        &mut self,
        result: WorkspaceFileSearchMatch,
        cx: &mut Context<Self>,
    ) {
        let Some(context) = self.request_context.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut directories = vec![String::new()];
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
        directories.extend(ancestors.clone());
        let generation = self.tree.generation();
        let include_ignored = self.tree.include_ignored();
        let client = WorkspaceFilesClient::new(engine, context.clone());
        self.search_state.reveal_task = Some(cx.spawn(async move |this, cx| {
            let mut pages = Vec::with_capacity(directories.len());
            for directory in directories {
                match client
                    .list_directory(ListWorkspaceDirectoryRequest {
                        target: context.target.clone(),
                        directory,
                        include_ignored,
                        cursor: None,
                    })
                    .await
                {
                    Ok(page) => pages.push(page),
                    Err(error) => {
                        let _ = this.update(cx, |surface, cx| {
                            if surface.tree.generation() == generation {
                                surface.search_state.error = Some(error.to_string().into());
                                cx.notify();
                            }
                        });
                        return;
                    }
                }
            }
            let _ = this.update(cx, |surface, cx| {
                if surface.tree.generation() != generation {
                    return;
                }
                for (index, page) in pages.into_iter().enumerate() {
                    surface.tree.apply_page(page, generation);
                    if let Some(next) = ancestors.get(index) {
                        surface.tree.expand(next);
                    }
                }
                surface.tree.select(result.path.clone());
                surface.sync_tree_list();
                surface
                    .search
                    .update(cx, |search, cx| search.set_text("", cx));
                surface.reveal_tree_selection();
                if result.kind == WorkspaceEntryKind::Directory {
                    surface.show_tree_sidebar(cx);
                } else {
                    surface.open_tree_file(result.path.clone(), cx);
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn render_search_results(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if let Some(error) = self.search_state.error.clone() {
            return centered_search_message(error, theme.danger.opacity(0.82));
        }
        if self.search_state.results.is_empty() {
            let label = if self.search_state.loading {
                "Searching…"
            } else {
                "No files found."
            };
            return centered_search_message(label.into(), theme.text_faint);
        }
        div()
            .id("files-search-results")
            .role(gpui::Role::Tree)
            .aria_label("Fuzzy workspace file results")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .when(
                self.search_state.results.len() >= SEARCH_RESULT_LIMIT,
                |element| {
                    element.child(
                        div()
                            .h(px(24.0))
                            .flex_none()
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .font_family(theme.font_sans.clone())
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child("Showing the first 200 matches"),
                    )
                },
            )
            .child(
                list(
                    self.search_list.clone(),
                    cx.processor(Self::render_search_row),
                )
                .flex_1()
                .min_h_0()
                .with_sizing_behavior(ListSizingBehavior::Auto),
            )
            .into_any_element()
    }

    fn render_search_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.search_state.tree.rows().get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let selected = self.search_state.active == index;
        let expanded = row.has_children && self.search_state.tree.is_expanded(&row.path);
        let is_directory = row.kind == WorkspaceEntryKind::Directory;
        let padding = 8.0 + row.depth as f32 * SEARCH_TREE_INDENT;
        let drag_payload = WorkspacePathDrag::new(row.path.clone(), is_directory);
        div()
            .id(("files-search-result", index))
            .role(gpui::Role::TreeItem)
            .aria_label(row.name.clone())
            .aria_selected(selected)
            .when(row.has_children, |element| element.aria_expanded(expanded))
            .h(px(SEARCH_ROW_HEIGHT))
            .w_full()
            .flex_none()
            .pl(px(padding))
            .pr(px(8.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .when(selected, |element| element.bg(crate::theme::wash(0.1)))
            .when(!selected, |element| {
                element.hover(|style| style.bg(crate::theme::wash(0.055)))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.search_state.active = index;
                this.activate_search_result(cx);
            }))
            .on_drag(drag_payload, |payload, _, _, cx| {
                cx.stop_propagation();
                workspace_path_drag_ghost(payload, cx)
            })
            .child(
                div()
                    .size(px(14.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(row.has_children, |element| {
                        element.child(
                            icon(if expanded {
                                icons::ALT_ARROW_DOWN
                            } else {
                                icons::ALT_ARROW_RIGHT
                            })
                            .size(px(11.0))
                            .text_color(theme.text_faint),
                        )
                    }),
            )
            .child(
                icon(if is_directory {
                    icons::FOLDER
                } else {
                    icons::DOCUMENT
                })
                .size(px(13.0))
                .flex_none()
                .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_sans.clone())
                    .text_size(px(11.5))
                    .text_color(if is_directory {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .child(row.name),
            )
            .into_any_element()
    }
}

fn centered_search_message(message: SharedString, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .px(px(24.0))
        .text_center()
        .text_size(px(11.5))
        .text_color(color)
        .child(message)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_match(path: &str, kind: WorkspaceEntryKind, score: i64) -> WorkspaceFileSearchMatch {
        WorkspaceFileSearchMatch {
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            kind,
            score,
        }
    }

    #[test]
    fn search_state_rejects_stale_queries() {
        let state = FileSearchState {
            query: "shell".into(),
            generation: 4,
            ..Default::default()
        };
        assert!(state.accepts(4, "shell"));
        assert!(!state.accepts(3, "shell"));
        assert!(!state.accepts(4, "other"));
    }

    #[test]
    fn search_tree_groups_matches_under_real_ancestors() {
        let mut tree = SearchTreeModel::default();
        tree.rebuild(&[
            search_match(
                "archive-renamed/docs/docs-1.txt",
                WorkspaceEntryKind::File,
                100,
            ),
            search_match(
                "archive-renamed/logs/file-2.txt",
                WorkspaceEntryKind::File,
                90,
            ),
            search_match("README.md", WorkspaceEntryKind::File, 80),
        ]);

        let rows = tree.rows();
        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            vec![
                "archive-renamed",
                "archive-renamed/docs",
                "archive-renamed/docs/docs-1.txt",
                "archive-renamed/logs",
                "archive-renamed/logs/file-2.txt",
                "README.md",
            ]
        );
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            vec![0, 1, 2, 1, 2, 0]
        );
    }

    #[test]
    fn search_tree_uses_path_components_instead_of_name_prefixes() {
        let mut tree = SearchTreeModel::default();
        tree.rebuild(&[search_match(
            "THIRD_PARTY_NOTICES/THIRD_PARTY_NOTICES.md",
            WorkspaceEntryKind::File,
            100,
        )]);

        assert_eq!(
            tree.rows()
                .iter()
                .map(|row| row.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "THIRD_PARTY_NOTICES",
                "THIRD_PARTY_NOTICES/THIRD_PARTY_NOTICES.md"
            ]
        );
    }

    #[test]
    fn search_tree_collapses_and_restores_matching_branches() {
        let mut tree = SearchTreeModel::default();
        tree.rebuild(&[
            search_match("src/main.rs", WorkspaceEntryKind::File, 100),
            search_match("src/lib.rs", WorkspaceEntryKind::File, 90),
        ]);

        assert!(tree.toggle("src"));
        assert_eq!(tree.rows().len(), 1);
        assert!(!tree.is_expanded("src"));
        assert!(tree.toggle("src"));
        assert_eq!(tree.rows().len(), 3);
        assert!(tree.is_expanded("src"));
    }
}
