//! Git history pane: paged commit rows plus a topological lane graph.
//!
//! The pane is hosted by the current Changes surface for now, but owns its
//! data and rendering so it can move intact into the future right-panel tabs.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::DateTime;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, EventEmitter, Focusable as _, Image,
    ImageFormat, ListAlignment, ListOffset, ListState, ObjectFit, PathBuilder, Pixels, Render,
    SharedString, Subscription, Task, Window, canvas, container_query, div, img, list, point,
    prelude::*, px,
};
use zeron_engine::repos::git_history_matches;
use zeron_proto::{
    GitHistoryCommit, GitHistoryComparison, GitHistoryPage, GitHistoryRef, GitHistoryRefKind,
};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion::AnimationExt;
use crate::popover::{self, Popup};
use crate::settings::{
    self, GitHistoryAuthorDisplay, GitHistoryColumn, GitHistoryColumnOrder, GitHistoryColumnWidths,
    GitHistoryColumns, SavePolicy,
};
use crate::state::AppState;
use crate::theme::Theme;

const HISTORY_PAGE_SIZE: usize = 100;
const HISTORY_ROW_HEIGHT: f32 = 36.0;
const HISTORY_LANE_SPACING: f32 = 12.0;
const HISTORY_NODE_RADIUS: f32 = 3.0;
const HISTORY_HEAD_RING_PADDING: f32 = 2.0;
const HISTORY_STROKE_WIDTH: f32 = 1.5;
const HISTORY_GRAPH_SATURATION: f32 = 0.72;
const HISTORY_GRAPH_SIDE_PADDING: f32 = 5.0;
const HISTORY_GRAPH_TRAILING_PADDING: f32 = 20.0;
const HISTORY_GRAPH_MIN_COMPACT_WIDTH: f32 = 48.0;
const HISTORY_GRAPH_MAX_WIDTH_RATIO: f32 = 0.34;
const HISTORY_GRAPH_RESIZE_STEP: f32 = 2.0;
const HISTORY_GRAPH_COMPACT_ENTER_SUBJECT_WIDTH: f32 = 160.0;
const HISTORY_GRAPH_COMPACT_EXIT_SUBJECT_WIDTH: f32 = 184.0;
const HISTORY_GRAPH_COMPACT_ENTER_LANE_SPACING: f32 = 4.0;
const HISTORY_GRAPH_COMPACT_EXIT_LANE_SPACING: f32 = 5.0;
const HISTORY_GRAPH_ROW_OVERLAP: f32 = 0.75;
const HISTORY_GRAPH_HIT_RADIUS: f32 = 5.5;
const HISTORY_GRAPH_FOCUSED_STROKE_WIDTH: f32 = 2.25;
const HISTORY_GRAPH_UNFOCUSED_OPACITY: f32 = 0.24;
const HISTORY_ROW_UNFOCUSED_OPACITY: f32 = 0.6;
const HISTORY_COMMIT_SUBJECT_MIN_WIDTH: f32 = 80.0;
const HISTORY_REF_AREA_RATIO: f32 = 0.45;
const HISTORY_REF_BADGE_MAX_WIDTH: f32 = 112.0;
const HISTORY_REF_GAP: f32 = 5.0;
const HISTORY_SEARCH_WIDTH: f32 = 196.0;
const HISTORY_SEARCH_DEBOUNCE: Duration = Duration::from_millis(70);
const HISTORY_SEARCH_IDLE_DISMISS: Duration = Duration::from_millis(1_500);
/// The header's comparison pill is supplemental metadata. Hide it before it
/// can crowd the fixed History controls on a narrow pane.
const HISTORY_COMPARISON_MIN_WIDTH: f32 = 260.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum GitHistorySearchMode {
    Collapsed,
    Expanded,
    Collapsing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentShape {
    Through,
    Incoming,
    Outgoing,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphSegment {
    from_lane: usize,
    to_lane: usize,
    color_id: usize,
    shape: SegmentShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphRow {
    sha: String,
    node_lane: usize,
    node_color_id: usize,
    segments: Vec<GraphSegment>,
    is_head: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GraphLayout {
    rows: Vec<GraphRow>,
    max_lane_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct GraphGeometry {
    lane_count: usize,
    width: f32,
    lane_spacing: f32,
    device_scale: f32,
    compact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct GraphGeometryMorph {
    from: GraphGeometry,
    to: GraphGeometry,
    epoch: usize,
}

impl GraphGeometry {
    fn natural(lane_count: usize) -> Self {
        let count = lane_count.max(1);
        Self {
            lane_count: count,
            width: HISTORY_GRAPH_SIDE_PADDING
                + HISTORY_GRAPH_TRAILING_PADDING
                + HISTORY_NODE_RADIUS * 2.0
                + (count - 1) as f32 * HISTORY_LANE_SPACING,
            lane_spacing: HISTORY_LANE_SPACING,
            device_scale: 1.0,
            compact: false,
        }
    }

    fn fitted(lane_count: usize, width: f32) -> Self {
        let count = lane_count.max(1);
        let natural = Self::natural(count);
        if count == 1 || width >= natural.width {
            return natural;
        }
        let fixed_width =
            HISTORY_GRAPH_SIDE_PADDING + HISTORY_GRAPH_TRAILING_PADDING + HISTORY_NODE_RADIUS * 2.0;
        let width = width.clamp(fixed_width, natural.width);
        Self {
            lane_count: count,
            width,
            lane_spacing: (width - fixed_width) / (count - 1) as f32,
            device_scale: 1.0,
            compact: false,
        }
    }

    fn compact(lane_count: usize) -> Self {
        let mut geometry = Self::natural(1);
        geometry.lane_count = lane_count.max(1);
        geometry.lane_spacing = 0.0;
        geometry.compact = true;
        geometry
    }

    fn lane_x(self, lane: usize) -> f32 {
        let x = HISTORY_GRAPH_SIDE_PADDING + HISTORY_NODE_RADIUS + lane as f32 * self.lane_spacing;
        (x * self.device_scale).round() / self.device_scale
    }
}

fn interpolate_graph_geometry(
    from: GraphGeometry,
    to: GraphGeometry,
    progress: f32,
) -> GraphGeometry {
    let progress = progress.clamp(0.0, 1.0);
    let lerp = |start: f32, end: f32| start + (end - start) * progress;
    GraphGeometry {
        lane_count: to.lane_count,
        width: lerp(from.width, to.width),
        lane_spacing: lerp(from.lane_spacing, to.lane_spacing),
        device_scale: to.device_scale,
        // Keep the full topology while lanes converge; the compact rail takes
        // over only once all paths already share its x coordinate. Expanding
        // does the inverse from the rail's first frame.
        compact: if from.compact {
            progress <= 0.001
        } else {
            to.compact && progress >= 0.999
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct GraphFocus {
    color_id: usize,
    amount: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GitHistoryViewMode {
    #[default]
    AllCommits,
    BranchTips,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryRowTransition {
    Stable,
    Entering,
    Exiting,
}

struct HistoryViewTransition {
    rows: Vec<HistoryRowTransition>,
    final_commits: Vec<GitHistoryCommit>,
    final_collapsed_counts: HashMap<String, usize>,
    epoch: usize,
}

#[derive(Clone)]
struct HistoryScrollAnchor {
    sha: String,
    offset_in_item: Pixels,
}

fn resolve_history_scroll_anchor(
    anchor: Option<HistoryScrollAnchor>,
    old: &[GitHistoryCommit],
    target: &[GitHistoryCommit],
) -> Option<HistoryScrollAnchor> {
    let anchor = anchor?;
    if target.iter().any(|commit| commit.sha == anchor.sha) {
        return Some(anchor);
    }
    let old_index = old.iter().position(|commit| commit.sha == anchor.sha)?;
    let target_shas: HashSet<&str> = target.iter().map(|commit| commit.sha.as_str()).collect();
    let replacement = old
        .iter()
        .skip(old_index + 1)
        .find(|commit| target_shas.contains(commit.sha.as_str()))
        .or_else(|| {
            old[..old_index]
                .iter()
                .rev()
                .find(|commit| target_shas.contains(commit.sha.as_str()))
        })?;
    Some(HistoryScrollAnchor {
        sha: replacement.sha.clone(),
        offset_in_item: px(0.0),
    })
}

fn history_list_splice(
    old: &[GitHistoryCommit],
    old_has_load_more: bool,
    target: &[GitHistoryCommit],
    target_has_load_more: bool,
) -> Option<(std::ops::Range<usize>, usize)> {
    const LOAD_MORE_KEY: &str = "\0history-load-more";
    let mut old_keys: Vec<&str> = old.iter().map(|commit| commit.sha.as_str()).collect();
    let mut target_keys: Vec<&str> = target.iter().map(|commit| commit.sha.as_str()).collect();
    if old_has_load_more {
        old_keys.push(LOAD_MORE_KEY);
    }
    if target_has_load_more {
        target_keys.push(LOAD_MORE_KEY);
    }
    let prefix = old_keys
        .iter()
        .zip(&target_keys)
        .take_while(|(old, target)| old == target)
        .count();
    let suffix_limit = old_keys.len().min(target_keys.len()).saturating_sub(prefix);
    let suffix = old_keys
        .iter()
        .rev()
        .zip(target_keys.iter().rev())
        .take(suffix_limit)
        .take_while(|(old, target)| old == target)
        .count();
    if prefix == old_keys.len() && prefix == target_keys.len() {
        return None;
    }
    Some((
        prefix..old_keys.len() - suffix,
        target_keys.len() - prefix - suffix,
    ))
}

/// Build a temporary list that preserves every old row while introducing the
/// target rows in their final order. Old-only rows sit beside their previous
/// stable anchor, so contracting them pulls the surrounding commits together
/// instead of making the list flash to a different ordering first.
fn history_transition_rows(
    old: &[GitHistoryCommit],
    target: &[GitHistoryCommit],
) -> (Vec<GitHistoryCommit>, Vec<HistoryRowTransition>) {
    let target_shas: HashSet<&str> = target.iter().map(|commit| commit.sha.as_str()).collect();
    let old_shas: HashSet<&str> = old.iter().map(|commit| commit.sha.as_str()).collect();
    let mut before_first = Vec::new();
    let mut after_anchor: HashMap<String, Vec<GitHistoryCommit>> = HashMap::new();
    let mut anchor: Option<String> = None;

    for commit in old {
        if target_shas.contains(commit.sha.as_str()) {
            anchor = Some(commit.sha.clone());
        } else if let Some(anchor) = anchor.as_ref() {
            after_anchor
                .entry(anchor.clone())
                .or_default()
                .push(commit.clone());
        } else {
            before_first.push(commit.clone());
        }
    }

    let mut commits = Vec::with_capacity(target.len() + old.len());
    let mut rows = Vec::with_capacity(target.len() + old.len());
    for commit in before_first {
        commits.push(commit);
        rows.push(HistoryRowTransition::Exiting);
    }
    for commit in target {
        commits.push(commit.clone());
        rows.push(if old_shas.contains(commit.sha.as_str()) {
            HistoryRowTransition::Stable
        } else {
            HistoryRowTransition::Entering
        });
        if let Some(exiting) = after_anchor.remove(commit.sha.as_str()) {
            for commit in exiting {
                commits.push(commit);
                rows.push(HistoryRowTransition::Exiting);
            }
        }
    }

    // This is only reachable when old contains duplicate anchors or an old
    // sequence has no shared row. Keeping the rows is safer than dropping the
    // exit animation, and the settled target still removes them afterwards.
    for exiting in after_anchor.into_values() {
        for commit in exiting {
            commits.push(commit);
            rows.push(HistoryRowTransition::Exiting);
        }
    }
    (commits, rows)
}

fn branch_ref_key(reference: &GitHistoryRef) -> Option<String> {
    let prefix = match reference.kind {
        GitHistoryRefKind::Branch => "local",
        GitHistoryRefKind::Remote => "remote",
        GitHistoryRefKind::Tag => return None,
    };
    Some(format!("{prefix}:{}", reference.label))
}

struct HistoryColumnPreferences {
    columns: GitHistoryColumns,
    widths: GitHistoryColumnWidths,
    order: GitHistoryColumnOrder,
    author_display: GitHistoryAuthorDisplay,
}

impl gpui::Global for HistoryColumnPreferences {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryDataColumn {
    Commit,
    Author,
    Date,
    Sha,
}

#[derive(Debug, Clone, Copy)]
struct HistoryColumnDragAnchor {
    start_x: f32,
    left: HistoryDataColumn,
    right: HistoryDataColumn,
    left_width: f32,
    right_width: f32,
}

struct HistoryColumnResize;

#[derive(Clone)]
struct HistoryColumnDrag {
    column: GitHistoryColumn,
    label: SharedString,
}

#[derive(Debug, Clone, Copy)]
struct HistoryColumnDragState {
    from: usize,
    over: usize,
}

struct HistoryResizeGhost;

struct HistoryColumnGhost {
    label: SharedString,
}

impl Render for HistoryResizeGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Render for HistoryColumnGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(24.0))
            .min_w(px(64.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .opacity(0.9)
            .child(self.label.clone())
    }
}

pub fn init(
    columns: GitHistoryColumns,
    widths: GitHistoryColumnWidths,
    order: GitHistoryColumnOrder,
    author_display: GitHistoryAuthorDisplay,
    cx: &mut App,
) {
    cx.set_global(HistoryColumnPreferences {
        columns,
        widths,
        order,
        author_display,
    });
}

pub fn configured_columns(cx: &App) -> GitHistoryColumns {
    cx.global::<HistoryColumnPreferences>().columns
}

pub fn configured_column_widths(cx: &App) -> GitHistoryColumnWidths {
    cx.global::<HistoryColumnPreferences>().widths
}

pub fn configured_column_order(cx: &App) -> GitHistoryColumnOrder {
    cx.global::<HistoryColumnPreferences>().order.clone()
}

pub fn configured_author_display(cx: &App) -> GitHistoryAuthorDisplay {
    cx.global::<HistoryColumnPreferences>().author_display
}

fn history_column_label(column: GitHistoryColumn) -> &'static str {
    match column {
        GitHistoryColumn::Author => "Author",
        GitHistoryColumn::Date => "Date",
        GitHistoryColumn::Sha => "SHA",
    }
}

fn history_column_is_visible(column: GitHistoryColumn, columns: GitHistoryColumns) -> bool {
    match column {
        GitHistoryColumn::Author => columns.author,
        GitHistoryColumn::Date => columns.date,
        GitHistoryColumn::Sha => columns.sha,
    }
}

fn visible_history_columns(
    order: &GitHistoryColumnOrder,
    columns: GitHistoryColumns,
) -> Vec<GitHistoryColumn> {
    order
        .0
        .iter()
        .copied()
        .filter(|column| history_column_is_visible(*column, columns))
        .collect()
}

fn history_data_column(column: GitHistoryColumn) -> HistoryDataColumn {
    match column {
        GitHistoryColumn::Author => HistoryDataColumn::Author,
        GitHistoryColumn::Date => HistoryDataColumn::Date,
        GitHistoryColumn::Sha => HistoryDataColumn::Sha,
    }
}

fn history_optional_width(column: GitHistoryColumn, widths: GitHistoryColumnWidths) -> f32 {
    history_column_width(history_data_column(column), widths)
}

fn history_column_drop_index(
    relative_x: f32,
    rendered_width: f32,
    columns: &[GitHistoryColumn],
    widths: GitHistoryColumnWidths,
) -> usize {
    if columns.is_empty() || rendered_width <= 0.0 {
        return 0;
    }
    let desired_width = columns
        .iter()
        .map(|column| history_optional_width(*column, widths))
        .sum::<f32>();
    let x = relative_x.clamp(0.0, rendered_width) * desired_width / rendered_width;
    let mut cursor = 0.0;
    for (index, column) in columns.iter().enumerate() {
        let width = history_optional_width(*column, widths);
        if x < cursor + width / 2.0 {
            return index;
        }
        cursor += width;
    }
    columns.len() - 1
}

fn reordered_history_columns(
    order: &GitHistoryColumnOrder,
    dragged: GitHistoryColumn,
    target: GitHistoryColumn,
) -> GitHistoryColumnOrder {
    if dragged == target {
        return order.clone();
    }
    let Some(from) = order.0.iter().position(|column| *column == dragged) else {
        return order.clone();
    };
    let Some(over) = order.0.iter().position(|column| *column == target) else {
        return order.clone();
    };
    let mut columns = order.0.clone();
    columns.remove(from);
    let target_after_removal = columns
        .iter()
        .position(|column| *column == target)
        .unwrap_or(columns.len());
    let insertion = if from < over {
        target_after_removal + 1
    } else {
        target_after_removal
    };
    columns.insert(insertion.min(columns.len()), dragged);
    GitHistoryColumnOrder(columns)
}

fn history_column_width(column: HistoryDataColumn, widths: GitHistoryColumnWidths) -> f32 {
    match column {
        HistoryDataColumn::Commit => HISTORY_COMMIT_SUBJECT_MIN_WIDTH,
        HistoryDataColumn::Author => widths.author,
        HistoryDataColumn::Date => widths.date,
        HistoryDataColumn::Sha => widths.sha,
    }
}

fn history_column_limits(column: HistoryDataColumn) -> (f32, f32) {
    match column {
        HistoryDataColumn::Commit => (HISTORY_COMMIT_SUBJECT_MIN_WIDTH, f32::MAX),
        HistoryDataColumn::Author => (
            GitHistoryColumnWidths::AUTHOR_MIN,
            GitHistoryColumnWidths::AUTHOR_MAX,
        ),
        HistoryDataColumn::Date => (
            GitHistoryColumnWidths::DATE_MIN,
            GitHistoryColumnWidths::DATE_MAX,
        ),
        HistoryDataColumn::Sha => (
            GitHistoryColumnWidths::SHA_MIN,
            GitHistoryColumnWidths::SHA_MAX,
        ),
    }
}

fn set_history_column_width(
    widths: &mut GitHistoryColumnWidths,
    column: HistoryDataColumn,
    width: f32,
) {
    match column {
        HistoryDataColumn::Commit => {}
        HistoryDataColumn::Author => widths.author = width,
        HistoryDataColumn::Date => widths.date = width,
        HistoryDataColumn::Sha => widths.sha = width,
    }
}

fn resized_history_column_widths(
    mut widths: GitHistoryColumnWidths,
    anchor: HistoryColumnDragAnchor,
    requested_delta: f32,
) -> GitHistoryColumnWidths {
    if anchor.left == HistoryDataColumn::Commit {
        let (right_min, right_max) = history_column_limits(anchor.right);
        set_history_column_width(
            &mut widths,
            anchor.right,
            (anchor.right_width - requested_delta).clamp(right_min, right_max),
        );
    } else {
        let (left_min, left_max) = history_column_limits(anchor.left);
        let (right_min, right_max) = history_column_limits(anchor.right);
        let min_delta = (left_min - anchor.left_width).max(anchor.right_width - right_max);
        let max_delta = (left_max - anchor.left_width).min(anchor.right_width - right_min);
        let delta = requested_delta.clamp(min_delta, max_delta);
        set_history_column_width(&mut widths, anchor.left, anchor.left_width + delta);
        set_history_column_width(&mut widths, anchor.right, anchor.right_width - delta);
    }
    widths
}

#[derive(Debug, Clone)]
struct ActiveLane {
    id: usize,
    color_id: usize,
    target_sha: String,
}

fn lane_index(lanes: &[ActiveLane], id: usize) -> usize {
    lanes
        .iter()
        .position(|lane| lane.id == id)
        .expect("active Git history lane exists")
}

/// Commits arrive child-before-parent from `git log --topo-order`. Active
/// lanes point at the parent commit that will eventually resolve each path.
fn layout_graph(commits: &[GitHistoryCommit], head_sha: Option<&str>) -> GraphLayout {
    let mut active_lanes: Vec<ActiveLane> = Vec::new();
    let mut next_lane_id = 0usize;
    let mut next_color_id = 0usize;
    let mut max_lane_count = 0usize;
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let before = active_lanes.clone();
        let incoming: Vec<usize> = before
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| (lane.target_sha == commit.sha).then_some(index))
            .collect();
        let primary_incoming = incoming.first().copied();
        let node_lane = primary_incoming.unwrap_or(before.len());
        let primary_lane = primary_incoming.and_then(|index| before.get(index));
        let node_color_id = primary_lane.map(|lane| lane.color_id).unwrap_or_else(|| {
            let color = next_color_id;
            next_color_id += 1;
            color
        });
        let resolved_ids: HashSet<usize> = incoming.iter().map(|&index| before[index].id).collect();
        let mut next_lanes: Vec<ActiveLane> = before
            .iter()
            .filter(|lane| !resolved_ids.contains(&lane.id))
            .cloned()
            .collect();
        let mut outgoing: Vec<(usize, usize)> = Vec::new();

        let mut primary_outgoing_id = None;
        if let Some(first_parent) = commit.parent_shas.first() {
            let id = primary_lane.map(|lane| lane.id).unwrap_or_else(|| {
                let id = next_lane_id;
                next_lane_id += 1;
                id
            });
            let lane = ActiveLane {
                id,
                color_id: node_color_id,
                target_sha: first_parent.clone(),
            };
            next_lanes.insert(node_lane.min(next_lanes.len()), lane.clone());
            primary_outgoing_id = Some(id);
            outgoing.push((id, lane.color_id));
        }

        let mut parent_offset = 1usize;
        for parent_sha in commit.parent_shas.iter().skip(1) {
            if let Some(existing) = next_lanes
                .iter()
                .find(|lane| lane.target_sha == *parent_sha)
            {
                outgoing.push((existing.id, existing.color_id));
                continue;
            }
            let lane = ActiveLane {
                id: next_lane_id,
                color_id: next_color_id,
                target_sha: parent_sha.clone(),
            };
            next_lane_id += 1;
            next_color_id += 1;
            let primary_index = primary_outgoing_id
                .map(|id| lane_index(&next_lanes, id))
                .unwrap_or_else(|| node_lane.min(next_lanes.len()));
            next_lanes.insert(
                (primary_index + parent_offset).min(next_lanes.len()),
                lane.clone(),
            );
            parent_offset += 1;
            outgoing.push((lane.id, lane.color_id));
        }

        let mut segments: Vec<GraphSegment> = before
            .iter()
            .enumerate()
            .filter(|(_, lane)| !resolved_ids.contains(&lane.id))
            .map(|(from_lane, lane)| GraphSegment {
                from_lane,
                to_lane: lane_index(&next_lanes, lane.id),
                color_id: lane.color_id,
                shape: SegmentShape::Through,
            })
            .collect();
        segments.extend(incoming.iter().map(|&from_lane| GraphSegment {
            from_lane,
            to_lane: node_lane,
            color_id: before[from_lane].color_id,
            shape: SegmentShape::Incoming,
        }));
        segments.extend(outgoing.into_iter().map(|(id, color_id)| GraphSegment {
            from_lane: node_lane,
            to_lane: lane_index(&next_lanes, id),
            color_id,
            shape: SegmentShape::Outgoing,
        }));

        max_lane_count = max_lane_count
            .max(before.len())
            .max(next_lanes.len())
            .max(node_lane + 1);
        rows.push(GraphRow {
            sha: commit.sha.clone(),
            node_lane,
            node_color_id,
            segments,
            is_head: head_sha == Some(commit.sha.as_str()),
        });
        active_lanes = next_lanes;
    }

    GraphLayout {
        rows,
        max_lane_count,
    }
}

/// Remove the linear portions of selected branch lanes while retaining refs,
/// roots, merges and branch points. Parents that cross a hidden run are
/// contracted to the nearest visible ancestors so the compact graph never
/// paints a line toward a row that no longer exists.
fn collapse_branch_runs(
    commits: &[GitHistoryCommit],
    collapsed_refs: &HashSet<String>,
    head_sha: Option<&str>,
) -> (Vec<GitHistoryCommit>, HashMap<String, usize>) {
    if collapsed_refs.is_empty() || commits.is_empty() {
        return (commits.to_vec(), HashMap::new());
    }

    let source_graph = layout_graph(commits, head_sha);
    let mut colors_by_ref = HashMap::new();
    for (commit, row) in commits.iter().zip(&source_graph.rows) {
        for reference in &commit.refs {
            if let Some(key) = branch_ref_key(reference)
                && collapsed_refs.contains(&key)
            {
                colors_by_ref.insert(key, row.node_color_id);
            }
        }
    }
    if colors_by_ref.is_empty() {
        return (commits.to_vec(), HashMap::new());
    }

    let collapsed_colors: HashSet<_> = colors_by_ref.values().copied().collect();
    let mut child_counts: HashMap<&str, usize> = HashMap::new();
    for commit in commits {
        for parent in &commit.parent_shas {
            *child_counts.entry(parent.as_str()).or_default() += 1;
        }
    }

    let mut visible = HashSet::new();
    let mut hidden_counts = HashMap::new();
    for (commit, row) in commits.iter().zip(&source_graph.rows) {
        let is_selected_tip = commit.refs.iter().any(|reference| {
            branch_ref_key(reference).is_some_and(|key| collapsed_refs.contains(&key))
        });
        let is_junction = commit.parent_shas.len() != 1
            || child_counts
                .get(commit.sha.as_str())
                .copied()
                .unwrap_or_default()
                > 1;
        let hide = collapsed_colors.contains(&row.node_color_id)
            && !is_selected_tip
            && commit.refs.is_empty()
            && !is_junction;
        if hide {
            for (key, color) in &colors_by_ref {
                if *color == row.node_color_id {
                    *hidden_counts.entry(key.clone()).or_default() += 1;
                }
            }
        } else {
            visible.insert(commit.sha.clone());
        }
    }

    (compact_commits_to_visible(commits, &visible), hidden_counts)
}

/// Retain a subset in its original topological order and contract parent
/// edges across omitted commits. Search uses this to keep a coherent graph
/// without pretending that a fuzzy match is a new ancestry boundary.
fn compact_commits_to_visible(
    commits: &[GitHistoryCommit],
    visible: &HashSet<String>,
) -> Vec<GitHistoryCommit> {
    let by_sha: HashMap<_, _> = commits
        .iter()
        .map(|commit| (commit.sha.as_str(), commit))
        .collect();

    fn nearest_visible_parents(
        sha: &str,
        visible: &HashSet<String>,
        by_sha: &HashMap<&str, &GitHistoryCommit>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        if visible.contains(sha) || !by_sha.contains_key(sha) {
            return vec![sha.to_string()];
        }
        if let Some(cached) = memo.get(sha) {
            return cached.clone();
        }

        struct Frame {
            sha: String,
            next_parent: usize,
            resolved: Vec<String>,
            seen: HashSet<String>,
        }

        impl Frame {
            fn new(sha: String) -> Self {
                Self {
                    sha,
                    next_parent: 0,
                    resolved: Vec::new(),
                    seen: HashSet::new(),
                }
            }

            fn extend(&mut self, parents: &[String]) {
                for parent in parents {
                    if self.seen.insert(parent.clone()) {
                        self.resolved.push(parent.clone());
                    }
                }
            }
        }

        let mut visiting = HashSet::from([sha.to_string()]);
        let mut stack = vec![Frame::new(sha.to_string())];
        loop {
            let next_parent = {
                let frame = stack.last_mut().expect("history traversal frame");
                let parents = &by_sha[frame.sha.as_str()].parent_shas;
                (frame.next_parent < parents.len()).then(|| {
                    let parent = parents[frame.next_parent].clone();
                    frame.next_parent += 1;
                    parent
                })
            };

            let Some(parent) = next_parent else {
                let frame = stack.pop().expect("history traversal frame");
                visiting.remove(&frame.sha);
                let resolved = frame.resolved;
                memo.insert(frame.sha, resolved.clone());
                if let Some(caller) = stack.last_mut() {
                    caller.extend(&resolved);
                    continue;
                }
                return resolved;
            };

            let resolved = if visible.contains(&parent) || !by_sha.contains_key(parent.as_str()) {
                Some(vec![parent.clone()])
            } else if let Some(cached) = memo.get(&parent) {
                Some(cached.clone())
            } else if visiting.contains(&parent) {
                Some(Vec::new())
            } else {
                None
            };

            if let Some(resolved) = resolved {
                stack
                    .last_mut()
                    .expect("history traversal frame")
                    .extend(&resolved);
            } else {
                visiting.insert(parent.clone());
                stack.push(Frame::new(parent));
            }
        }
    }

    let mut memo = HashMap::new();
    commits
        .iter()
        .filter(|commit| visible.contains(&commit.sha))
        .cloned()
        .map(|mut commit| {
            let mut seen = HashSet::new();
            commit.parent_shas = commit
                .parent_shas
                .iter()
                .flat_map(|parent| nearest_visible_parents(parent, visible, &by_sha, &mut memo))
                .filter(|parent| seen.insert(parent.clone()))
                .collect();
            commit
        })
        .collect()
}

fn responsive_graph_geometry(
    lane_count: usize,
    container_width: f32,
    optional_columns_width: f32,
) -> GraphGeometry {
    let natural = GraphGeometry::natural(lane_count);
    if natural.width <= HISTORY_GRAPH_MIN_COMPACT_WIDTH {
        return natural;
    }

    // Commit subjects remain the primary content. Give the graph at most a
    // third of the surface, and never consume the subject's minimum width or
    // the configured metadata columns when the pane becomes narrow.
    let content_budget =
        (container_width - optional_columns_width - HISTORY_COMMIT_SUBJECT_MIN_WIDTH)
            .max(HISTORY_GRAPH_MIN_COMPACT_WIDTH);
    let share_budget =
        (container_width * HISTORY_GRAPH_MAX_WIDTH_RATIO).max(HISTORY_GRAPH_MIN_COMPACT_WIDTH);
    GraphGeometry::fitted(
        lane_count,
        natural.width.min(content_budget.min(share_budget)),
    )
}

fn should_use_compact_graph(
    target: GraphGeometry,
    previous: GraphGeometry,
    container_width: f32,
    optional_columns_width: f32,
) -> bool {
    if target.lane_count <= 1 {
        return false;
    }
    let subject_width = container_width - optional_columns_width - target.width;
    if previous.compact {
        subject_width < HISTORY_GRAPH_COMPACT_EXIT_SUBJECT_WIDTH
            || target.lane_spacing < HISTORY_GRAPH_COMPACT_EXIT_LANE_SPACING
    } else {
        subject_width < HISTORY_GRAPH_COMPACT_ENTER_SUBJECT_WIDTH
            || target.lane_spacing < HISTORY_GRAPH_COMPACT_ENTER_LANE_SPACING
    }
}

fn stabilized_graph_geometry(
    target: GraphGeometry,
    previous: GraphGeometry,
    device_scale: f32,
    compact: bool,
) -> GraphGeometry {
    if compact {
        let mut compact_geometry = GraphGeometry::compact(target.lane_count);
        compact_geometry.device_scale = device_scale.max(1.0);
        return compact_geometry;
    }
    let natural = GraphGeometry::natural(target.lane_count);
    let target_is_natural = (target.width - natural.width).abs() < f32::EPSILON;
    let snapped_width = if target_is_natural {
        natural.width
    } else {
        (target.width / HISTORY_GRAPH_RESIZE_STEP).floor() * HISTORY_GRAPH_RESIZE_STEP
    };
    let mut next = GraphGeometry::fitted(target.lane_count, snapped_width);
    next.device_scale = device_scale.max(1.0);

    if !target_is_natural
        && previous.lane_count == next.lane_count
        && (previous.width - next.width).abs() < HISTORY_GRAPH_RESIZE_STEP
    {
        // Small width oscillations are common while dragging a GPUI split.
        // Keep the last geometry until a complete step has accumulated.
        let mut stable = previous;
        stable.device_scale = next.device_scale;
        stable
    } else {
        next
    }
}

fn cubic_coordinate(start: f32, control_1: f32, control_2: f32, end: f32, t: f32) -> f32 {
    let inverse = 1.0 - t;
    inverse.powi(3) * start
        + 3.0 * inverse.powi(2) * t * control_1
        + 3.0 * inverse * t.powi(2) * control_2
        + t.powi(3) * end
}

fn point_to_segment_distance(
    point_x: f32,
    point_y: f32,
    start_x: f32,
    start_y: f32,
    end_x: f32,
    end_y: f32,
) -> f32 {
    let delta_x = end_x - start_x;
    let delta_y = end_y - start_y;
    let length_squared = delta_x * delta_x + delta_y * delta_y;
    if length_squared <= f32::EPSILON {
        return ((point_x - start_x).powi(2) + (point_y - start_y).powi(2)).sqrt();
    }
    let projection = (((point_x - start_x) * delta_x + (point_y - start_y) * delta_y)
        / length_squared)
        .clamp(0.0, 1.0);
    let closest_x = start_x + projection * delta_x;
    let closest_y = start_y + projection * delta_y;
    ((point_x - closest_x).powi(2) + (point_y - closest_y).powi(2)).sqrt()
}

fn segment_distance(
    segment: &GraphSegment,
    point_x: f32,
    point_y: f32,
    geometry: GraphGeometry,
) -> f32 {
    let middle = HISTORY_ROW_HEIGHT / 2.0;
    let from_x = geometry.lane_x(segment.from_lane);
    let to_x = geometry.lane_x(segment.to_lane);
    let (start_y, end_y, control_1_y, control_2_y) = match segment.shape {
        SegmentShape::Incoming => (
            -HISTORY_GRAPH_ROW_OVERLAP,
            middle,
            middle * 0.55,
            middle * 0.55,
        ),
        SegmentShape::Outgoing => (
            middle,
            HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP,
            middle * 1.45,
            middle * 1.45,
        ),
        SegmentShape::Through => (
            -HISTORY_GRAPH_ROW_OVERLAP,
            HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP,
            middle,
            middle,
        ),
    };
    if segment.shape == SegmentShape::Through && segment.from_lane == segment.to_lane {
        return point_to_segment_distance(point_x, point_y, from_x, start_y, to_x, end_y);
    }

    // A short polyline approximation is enough for pointer hit testing and
    // keeps the interactive target in lockstep with the painted Bezier.
    const SAMPLES: usize = 10;
    let mut closest = f32::MAX;
    let mut previous_x = from_x;
    let mut previous_y = start_y;
    for sample in 1..=SAMPLES {
        let t = sample as f32 / SAMPLES as f32;
        let current_x = cubic_coordinate(from_x, from_x, to_x, to_x, t);
        let current_y = cubic_coordinate(start_y, control_1_y, control_2_y, end_y, t);
        closest = closest.min(point_to_segment_distance(
            point_x, point_y, previous_x, previous_y, current_x, current_y,
        ));
        previous_x = current_x;
        previous_y = current_y;
    }
    closest
}

fn hovered_graph_path(
    row: &GraphRow,
    point_x: f32,
    point_y: f32,
    geometry: GraphGeometry,
) -> Option<usize> {
    let node_x = geometry.lane_x(row.node_lane);
    let node_y = HISTORY_ROW_HEIGHT / 2.0;
    let node_distance = ((point_x - node_x).powi(2) + (point_y - node_y).powi(2)).sqrt();
    if node_distance <= HISTORY_GRAPH_HIT_RADIUS + HISTORY_NODE_RADIUS {
        return Some(row.node_color_id);
    }

    if geometry.compact && (point_x - geometry.lane_x(0)).abs() <= HISTORY_GRAPH_HIT_RADIUS {
        return Some(row.node_color_id);
    }

    row.segments
        .iter()
        .map(|segment| {
            (
                segment.color_id,
                segment_distance(segment, point_x, point_y, geometry),
            )
        })
        .filter(|(_, distance)| *distance <= HISTORY_GRAPH_HIT_RADIUS)
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(color_id, _)| color_id)
}

fn estimated_ref_badge_width(reference: &GitHistoryRef) -> f32 {
    // 10 px icon + 2 px gap + 10 px horizontal padding + the 10 px label.
    (22.0 + reference.label.chars().count() as f32 * 5.7).min(HISTORY_REF_BADGE_MAX_WIDTH)
}

fn estimated_ref_overflow_width(hidden: usize) -> f32 {
    format!("+{hidden}").chars().count() as f32 * 5.7
}

fn ref_area_width(commit_column_width: f32) -> f32 {
    let inner = (commit_column_width - 8.0).max(0.0);
    (inner * HISTORY_REF_AREA_RATIO)
        .min((inner - HISTORY_COMMIT_SUBJECT_MIN_WIDTH - HISTORY_REF_GAP).max(0.0))
}

fn visible_ref_count(refs: &[GitHistoryRef], available_width: f32) -> usize {
    if refs.is_empty() {
        return 0;
    }

    // Start with no visible badges so the overflow target remains available
    // when even the first badge plus `+N` would exceed the ref area.
    let mut visible = 0;
    for count in 1..=refs.len() {
        let hidden = refs.len() - count;
        let item_count = count + usize::from(hidden > 0);
        let badges = refs[..count]
            .iter()
            .map(estimated_ref_badge_width)
            .sum::<f32>();
        let overflow = (hidden > 0)
            .then(|| estimated_ref_overflow_width(hidden))
            .unwrap_or_default();
        let gaps = item_count.saturating_sub(1) as f32 * HISTORY_REF_GAP;
        if badges + overflow + gaps <= available_width {
            visible = count;
        }
    }
    visible
}

fn format_date(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.format("%b %-d, %Y").to_string())
        .unwrap_or_else(|_| "—".to_string())
}

fn ref_color(reference: &GitHistoryRef, theme: &Theme) -> gpui::Hsla {
    match reference.kind {
        GitHistoryRefKind::Branch => theme.accent,
        GitHistoryRefKind::Remote => theme.busy,
        GitHistoryRefKind::Tag => theme.warning,
    }
}

fn ref_icon(kind: GitHistoryRefKind) -> &'static str {
    match kind {
        GitHistoryRefKind::Branch => crate::icons::GIT_BRANCH,
        GitHistoryRefKind::Remote => crate::icons::CLOUD,
        GitHistoryRefKind::Tag => crate::icons::TAG,
    }
}

fn ref_description(reference: &GitHistoryRef) -> SharedString {
    let kind = match reference.kind {
        GitHistoryRefKind::Branch => "Branch",
        GitHistoryRefKind::Remote => "Remote branch",
        GitHistoryRefKind::Tag => "Tag",
    };
    format!("{kind}: {}", reference.label).into()
}

fn graph_color(mut color: gpui::Hsla) -> gpui::Hsla {
    color.s *= HISTORY_GRAPH_SATURATION;
    color
}

struct HistoryRefTooltip {
    descriptions: Vec<SharedString>,
}

impl Render for HistoryRefTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let longest = self
            .descriptions
            .iter()
            .map(|description| description.chars().count())
            .max()
            .unwrap_or_default();
        let width = (longest as f32 * 6.4 + 16.0).clamp(72.0, 360.0);
        div()
            .w(px(width))
            .px(px(8.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .children(self.descriptions.iter().cloned().map(|description| {
                div()
                    .min_w_0()
                    .truncate()
                    .whitespace_nowrap()
                    .font_family(theme.font_mono.clone())
                    .child(description)
            }))
    }
}

struct HistoryAuthorTooltip {
    name: SharedString,
}

impl Render for HistoryAuthorTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let width = (self.name.chars().count() as f32 * 6.2 + 16.0).clamp(72.0, 260.0);
        div()
            .w(px(width))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .truncate()
            .whitespace_nowrap()
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child(self.name.clone())
    }
}

fn history_author_name(name: &str) -> SharedString {
    if name.trim().is_empty() {
        "Unknown".into()
    } else {
        name.to_string().into()
    }
}

fn history_author_initial(name: &str) -> SharedString {
    name.chars()
        .find(|character| !character.is_whitespace())
        .map(|character| character.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_string())
        .into()
}

fn decode_history_avatar(encoded: &str) -> Option<Arc<Image>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ImageFormat::Png
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ImageFormat::Jpeg
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        ImageFormat::Gif
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        ImageFormat::Webp
    } else {
        return None;
    };
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

pub struct GitHistory {
    state: Entity<AppState>,
    started: bool,
    target_key: Option<String>,
    commits: Vec<GitHistoryCommit>,
    visible_commits: Vec<GitHistoryCommit>,
    branch_tips: Vec<GitHistoryCommit>,
    view_mode: GitHistoryViewMode,
    view_epoch: usize,
    view_transition: Option<HistoryViewTransition>,
    view_transition_task: Option<Task<()>>,
    collapsed_branches: HashSet<String>,
    collapsed_counts: HashMap<String, usize>,
    head_sha: Option<String>,
    next_cursor: Option<usize>,
    total_count: Option<usize>,
    head_commit_count: Option<usize>,
    comparison: Option<GitHistoryComparison>,
    search_query: String,
    search_results: Option<Vec<GitHistoryCommit>>,
    search_next_cursor: Option<usize>,
    search_total_count: Option<usize>,
    search_loading: bool,
    search_error: Option<SharedString>,
    search_generation: usize,
    search_scroll_anchor: Option<HistoryScrollAnchor>,
    search_task: Option<Task<()>>,
    loading: bool,
    error: Option<SharedString>,
    graph: GraphLayout,
    graph_lane_capacity: usize,
    /// Updated by the surface-level container query before rows paint. The
    /// same geometry drives canvas paths, nodes, headers, and hit testing.
    graph_geometry: Rc<Cell<GraphGeometry>>,
    /// The settled responsive target. This stays separate from the animated
    /// geometry so resize hysteresis never reads an in-between lane layout.
    graph_target_geometry: Rc<Cell<GraphGeometry>>,
    graph_geometry_morph: Rc<Cell<Option<GraphGeometryMorph>>>,
    graph_hover_suppressed: Rc<Cell<bool>>,
    list: ListState,
    hovered_path: Option<usize>,
    hovered_graph_path: Option<usize>,
    hovered_row_path: Option<usize>,
    graph_hover_active: bool,
    graph_hover_clear_task: Option<Task<()>>,
    column_drag_anchor: Option<HistoryColumnDragAnchor>,
    column_drag: Option<HistoryColumnDragState>,
    avatar_images: HashMap<String, Arc<Image>>,
    column_menu: Popup<gpui::Point<gpui::Pixels>>,
    author_menu: Popup<gpui::Point<gpui::Pixels>>,
    copied_sha: Option<String>,
    request_task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    fetching_all: bool,
    fetch_for: Option<String>,
    fetch_error: Option<SharedString>,
    fetch_task: Option<Task<()>>,
    _observe: Subscription,
}

pub enum GitHistoryEvent {
    FetchSucceeded,
    /// A commit row was clicked — the host opens it as its own diff tab.
    OpenCommit(GitHistoryCommit),
}

impl EventEmitter<GitHistoryEvent> for GitHistory {}

pub struct GitHistoryCount {
    history: Entity<GitHistory>,
    _observe: Subscription,
}

pub struct GitHistoryFetchButton {
    history: Entity<GitHistory>,
    _observe: Subscription,
}

pub struct GitHistoryViewButton {
    history: Entity<GitHistory>,
    _observe: Subscription,
}

pub struct GitHistorySearchControl {
    history: Entity<GitHistory>,
    input: Entity<ComposerInput>,
    mode: GitHistorySearchMode,
    idle_dismiss_epoch: usize,
    transition_epoch: usize,
    idle_dismiss_task: Option<Task<()>>,
    transition_task: Option<Task<()>>,
    blur_subscription: Option<Subscription>,
    _observe: Subscription,
    _input_events: Subscription,
}

impl GitHistorySearchControl {
    pub fn new(history: Entity<GitHistory>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            ComposerInput::with_context("Search", "PaletteSearch", cx).with_text_metrics(11.0, 14.0)
        });
        let observe = cx.observe(&history, |this, history, cx| {
            let query = history.read(cx).search_query.clone();
            if this.input.read(cx).text() != query {
                this.input.update(cx, |input, cx| input.set_text(query, cx));
            }
            cx.notify();
        });
        let input_events = cx.subscribe(&input, |this: &mut Self, input, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                let query = input.read(cx).text().to_string();
                let query_is_empty = query.is_empty();
                this.history
                    .update(cx, |history, cx| history.set_search_query(query, cx));
                if query_is_empty {
                    this.schedule_idle_dismiss(cx);
                } else {
                    this.cancel_idle_dismiss();
                }
            }
        });
        Self {
            history,
            input,
            mode: GitHistorySearchMode::Collapsed,
            idle_dismiss_epoch: 0,
            transition_epoch: 0,
            idle_dismiss_task: None,
            transition_task: None,
            blur_subscription: None,
            _observe: observe,
            _input_events: input_events,
        }
    }

    fn cancel_idle_dismiss(&mut self) {
        self.idle_dismiss_epoch = self.idle_dismiss_epoch.wrapping_add(1);
        self.idle_dismiss_task = None;
    }

    fn schedule_idle_dismiss(&mut self, cx: &mut Context<Self>) {
        self.cancel_idle_dismiss();
        if self.mode != GitHistorySearchMode::Expanded || !self.input.read(cx).text().is_empty() {
            return;
        }
        let epoch = self.idle_dismiss_epoch;
        self.idle_dismiss_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(HISTORY_SEARCH_IDLE_DISMISS)
                .await;
            this.update(cx, |control, cx| {
                if control.idle_dismiss_epoch == epoch
                    && control.mode == GitHistorySearchMode::Expanded
                    && control.input.read(cx).text().is_empty()
                {
                    control.begin_collapse(cx);
                }
            })
            .ok();
        }));
    }

    fn begin_collapse(&mut self, cx: &mut Context<Self>) {
        if self.mode != GitHistorySearchMode::Expanded || !self.input.read(cx).text().is_empty() {
            return;
        }
        self.cancel_idle_dismiss();
        self.mode = GitHistorySearchMode::Collapsing;
        self.transition_epoch = self.transition_epoch.wrapping_add(1);
        let epoch = self.transition_epoch;
        let duration = crate::motion::RESIZE
            .total()
            .mul_f32(crate::motion::speed_scale());
        self.transition_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            this.update(cx, |control, cx| {
                if control.transition_epoch == epoch
                    && control.mode == GitHistorySearchMode::Collapsing
                {
                    // Unmounting the input is intentional: GPUI otherwise keeps
                    // its focused caret alive after this compact control is idle.
                    control.mode = GitHistorySearchMode::Collapsed;
                    control.transition_task = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn expand(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_idle_dismiss();
        self.transition_epoch = self.transition_epoch.wrapping_add(1);
        self.transition_task = None;
        self.mode = GitHistorySearchMode::Expanded;
        // The collapsed render does not mount `input`. Focusing its handle in
        // this click cycle leaves the next focus path empty, so Shell's
        // focus-lost fallback legitimately restores the composer. Wait until
        // the expanded state has driven a frame, then complete the handoff.
        let control = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            control
                .update(cx, |control, cx| {
                    if control.mode != GitHistorySearchMode::Expanded {
                        return;
                    }
                    let focus = control.input.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                    control.schedule_idle_dismiss(cx);
                    cx.notify();
                })
                .ok();
        });
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if !self.input.read(cx).text().is_empty() {
            self.input.update(cx, |input, cx| input.set_text("", cx));
        }
        self.schedule_idle_dismiss(cx);
        cx.notify();
    }
}

impl GitHistoryFetchButton {
    pub fn new(history: Entity<GitHistory>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&history, |_, _, cx| cx.notify());
        Self {
            history,
            _observe: observe,
        }
    }
}

impl GitHistoryViewButton {
    pub fn new(history: Entity<GitHistory>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&history, |_, _, cx| cx.notify());
        Self {
            history,
            _observe: observe,
        }
    }
}

impl Render for GitHistoryFetchButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let fetching = self.history.read(cx).fetching_all;
        let history = self.history.clone();
        div()
            .id("history-fetch-all")
            .h(px(24.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .bg(if fetching {
                crate::theme::wash(0.05)
            } else {
                crate::motion::hover_blend(
                    "history-fetch-all",
                    crate::theme::wash(0.0),
                    crate::theme::wash(0.14),
                )
            })
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .when(!fetching, |element| {
                element
                    .cursor_pointer()
                    .on_hover(crate::motion::hover_listener("history-fetch-all"))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        history.update(cx, |history, cx| history.fetch_all(cx));
                    })
            })
            .child(if fetching {
                crate::loaders::mini_glyph_spinner(
                    "history-fetch-all-spinner",
                    1.75,
                    theme.glyph,
                    cx.entity_id(),
                    cx,
                )
                .into_any_element()
            } else {
                crate::icons::icon(crate::icons::CLOUD)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.75))
                    .into_any_element()
            })
            .child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .text_color(if fetching {
                        theme.text_faint
                    } else {
                        theme.text_muted
                    })
                    .child(if fetching { "Fetching…" } else { "Fetch all" }),
            )
    }
}

impl Render for GitHistoryViewButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let showing_tips = self.history.read(cx).view_mode == GitHistoryViewMode::BranchTips;
        let history = self.history.clone();
        let tooltip = if showing_tips {
            "Show all commits"
        } else {
            "Show branch tips"
        };

        div()
            .id("history-view-trigger")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(if showing_tips {
                theme.accent.opacity(0.12)
            } else {
                crate::motion::hover_blend(
                    "history-view-trigger",
                    crate::theme::wash(0.0),
                    crate::theme::wash(0.14),
                )
            })
            .on_hover(crate::motion::hover_listener("history-view-trigger"))
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                history.update(cx, |history, cx| {
                    let mode = if history.view_mode == GitHistoryViewMode::BranchTips {
                        GitHistoryViewMode::AllCommits
                    } else {
                        GitHistoryViewMode::BranchTips
                    };
                    history.set_view_mode(mode, cx);
                });
            })
            .child(
                crate::icons::icon(crate::icons::FOLD_VERTICAL)
                    .size(px(12.0))
                    .text_color(if showing_tips {
                        theme.accent
                    } else {
                        theme.text_muted
                    }),
            )
            .tooltip(move |_, cx| {
                cx.new(|_| HistoryRefTooltip {
                    descriptions: vec![tooltip.into()],
                })
                .into()
            })
            .tooltip_show_delay(Duration::from_millis(350))
    }
}

impl Render for GitHistorySearchControl {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if self.mode == GitHistorySearchMode::Collapsed {
            let control = cx.entity().downgrade();
            return div()
                .id("history-search-trigger")
                .size(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .bg(crate::motion::hover_blend(
                    "history-search-trigger",
                    crate::theme::wash(0.0),
                    crate::theme::wash(0.14),
                ))
                .on_hover(crate::motion::hover_listener("history-search-trigger"))
                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    control
                        .update(cx, |control, cx| control.expand(window, cx))
                        .ok();
                })
                .child(
                    crate::icons::icon(crate::icons::MAGNIFER)
                        .size(px(12.0))
                        .text_color(theme.text_muted),
                )
                .tooltip(|_, cx| {
                    cx.new(|_| HistoryRefTooltip {
                        descriptions: vec!["Search commits".into()],
                    })
                    .into()
                })
                .tooltip_show_delay(Duration::from_millis(350))
                .into_any_element();
        }
        if self.blur_subscription.is_none() {
            let focus = self.input.read(cx).focus_handle(cx);
            self.blur_subscription = Some(cx.on_blur(&focus, window, |control, _, cx| {
                control.clear(cx);
            }));
        }
        let control = cx.entity().downgrade();
        let search_loading = self.history.read(cx).search_loading;
        let closing = self.mode == GitHistorySearchMode::Collapsing;
        let transition_epoch = self.transition_epoch;
        let status_icon = if search_loading {
            crate::loaders::mini_glyph_spinner(
                "history-search-spinner",
                1.5,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element()
        } else {
            crate::icons::icon(crate::icons::MAGNIFER)
                .size(px(11.0))
                .flex_none()
                .text_color(theme.text_faint)
                .into_any_element()
        };
        div()
            .id("history-search-expanded")
            .h(px(22.0))
            .w(px(HISTORY_SEARCH_WIDTH))
            .flex_none()
            .overflow_hidden()
            .flex()
            .items_center()
            .gap(px(4.0))
            .pl(px(6.0))
            .pr(px(2.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.border.opacity(0.8))
            .bg(theme.surface_raised.opacity(0.86))
            .child(
                div()
                    .size(px(14.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(status_icon),
            )
            .child(
                div()
                    .h(px(14.0))
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .relative()
                    // The shaped text's ascent sits above its line box;
                    // nudge the compact input down to the icon's visual
                    // center rather than relying on flex-box geometry alone.
                    .top(px(3.0))
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .id("history-search-close")
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.5))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::ink(0.08)))
                    .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                        window.prevent_default()
                    })
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        control.update(cx, |control, cx| control.clear(cx)).ok();
                    })
                    .child(
                        crate::icons::icon(crate::icons::CLOSE)
                            .size(px(9.0))
                            .text_color(theme.text_faint),
                    ),
            )
            .with_animation(
                SharedString::from(format!(
                    "history-search-morph-{transition_epoch}-{}",
                    if closing { "out" } else { "in" }
                )),
                crate::motion::RESIZE.animation(),
                move |element, progress| {
                    let amount = if closing { 1.0 - progress } else { progress };
                    element
                        .w(px(24.0 + (HISTORY_SEARCH_WIDTH - 24.0) * amount))
                        .opacity(0.45 + 0.55 * amount)
                },
            )
            .into_any_element()
    }
}

impl GitHistoryCount {
    pub fn new(history: Entity<GitHistory>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&history, |_, _, cx| cx.notify());
        Self {
            history,
            _observe: observe,
        }
    }
}

impl Render for GitHistoryCount {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let history = self.history.read(cx);
        let count = history.commit_count();
        let comparison = history
            .comparison
            .clone()
            .filter(|comparison| comparison.ahead > 0 || comparison.behind > 0);
        div()
            .h_full()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .overflow_hidden()
            .child(
                container_query(move |size, _, _| {
                    let comparison_fits = f32::from(size.width) >= HISTORY_COMPARISON_MIN_WIDTH;
                    div()
                        .size_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .when_some(count, |element, count| {
                            element.child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_size(px(11.0))
                                    .line_height(px(14.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(format!(
                                        "{count} commit{}",
                                        if count == 1 { "" } else { "s" }
                                    ))),
                            )
                        })
                        .when_some(
                            comparison_fits.then(|| comparison.clone()).flatten(),
                            |element, comparison| {
                                let base = comparison.base.clone();
                                let ahead = comparison.ahead;
                                let behind = comparison.behind;
                                element.child(
                                    div()
                                        .id("history-comparison")
                                        .relative()
                                        .top(px(1.0))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap(px(4.0))
                                        .when(ahead > 0, |comparison| {
                                            comparison.child(
                                                div()
                                                    .whitespace_nowrap()
                                                    .text_size(px(10.5))
                                                    .line_height(px(13.0))
                                                    .text_color(theme.accent.opacity(0.88))
                                                    .child(SharedString::from(format!(
                                                        "{ahead} ahead"
                                                    ))),
                                            )
                                        })
                                        .when(ahead > 0 && behind > 0, |comparison| {
                                            comparison.child(
                                                div()
                                                    .text_size(px(10.0))
                                                    .text_color(theme.text_faint)
                                                    .child("·"),
                                            )
                                        })
                                        .when(behind > 0, |comparison| {
                                            comparison.child(
                                                div()
                                                    .whitespace_nowrap()
                                                    .text_size(px(10.5))
                                                    .line_height(px(13.0))
                                                    .text_color(theme.warning.opacity(0.82))
                                                    .child(SharedString::from(format!(
                                                        "{behind} behind"
                                                    ))),
                                            )
                                        })
                                        .tooltip(move |_, cx| {
                                            cx.new(|_| HistoryRefTooltip {
                                                descriptions: vec![
                                                    format!(
                                                        "Compared with {base}: {ahead} ahead, {behind} behind"
                                                    )
                                                    .into(),
                                                ],
                                            })
                                            .into()
                                        }),
                                )
                            },
                        )
                })
                .size_full(),
            )
    }
}

impl GitHistory {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this, _, cx| {
            if this.started {
                this.ensure_loaded(cx);
            }
        });
        Self {
            state,
            started: false,
            target_key: None,
            commits: Vec::new(),
            visible_commits: Vec::new(),
            branch_tips: Vec::new(),
            view_mode: GitHistoryViewMode::default(),
            view_epoch: 0,
            view_transition: None,
            view_transition_task: None,
            collapsed_branches: HashSet::new(),
            collapsed_counts: HashMap::new(),
            head_sha: None,
            next_cursor: None,
            total_count: None,
            head_commit_count: None,
            comparison: None,
            search_query: String::new(),
            search_results: None,
            search_next_cursor: None,
            search_total_count: None,
            search_loading: false,
            search_error: None,
            search_generation: 0,
            search_scroll_anchor: None,
            search_task: None,
            loading: false,
            error: None,
            graph: GraphLayout::default(),
            graph_lane_capacity: 0,
            graph_geometry: Rc::new(Cell::new(GraphGeometry::natural(1))),
            graph_target_geometry: Rc::new(Cell::new(GraphGeometry::natural(1))),
            graph_geometry_morph: Rc::new(Cell::new(None)),
            graph_hover_suppressed: Rc::new(Cell::new(false)),
            list: ListState::new(0, ListAlignment::Top, px(HISTORY_ROW_HEIGHT * 5.0)),
            hovered_path: None,
            hovered_graph_path: None,
            hovered_row_path: None,
            graph_hover_active: false,
            graph_hover_clear_task: None,
            column_drag_anchor: None,
            column_drag: None,
            avatar_images: HashMap::new(),
            column_menu: Popup::default(),
            author_menu: Popup::default(),
            copied_sha: None,
            request_task: None,
            copy_task: None,
            fetching_all: false,
            fetch_for: None,
            fetch_error: None,
            fetch_task: None,
            _observe: observe,
        }
    }

    fn context(&self, cx: &App) -> Option<(String, String, Option<String>)> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let cwd = chat.cwd.clone()?;
        let target = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        let key = format!("{}|{cwd}", target.as_deref().unwrap_or("local"));
        Some((key, cwd, target))
    }

    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        self.started = true;
        let Some((key, cwd, target)) = self.context(cx) else {
            self.fetch_task = None;
            self.fetching_all = false;
            self.fetch_for = None;
            self.fetch_error = None;
            self.target_key = None;
            self.commits.clear();
            self.visible_commits.clear();
            self.branch_tips.clear();
            self.collapsed_branches.clear();
            self.collapsed_counts.clear();
            self.total_count = None;
            self.head_commit_count = None;
            self.comparison = None;
            self.search_query.clear();
            self.search_results = None;
            self.search_next_cursor = None;
            self.search_total_count = None;
            self.search_loading = false;
            self.search_error = None;
            self.search_generation = self.search_generation.wrapping_add(1);
            self.search_scroll_anchor = None;
            self.search_task = None;
            self.graph = GraphLayout::default();
            self.graph_lane_capacity = 0;
            self.list.reset(0);
            self.hovered_path = None;
            self.hovered_graph_path = None;
            self.hovered_row_path = None;
            self.graph_hover_active = false;
            self.graph_hover_clear_task = None;
            self.avatar_images.clear();
            self.loading = false;
            return;
        };
        if self.target_key.as_deref() == Some(key.as_str()) {
            if !self.loading && self.commits.is_empty() && self.error.is_none() {
                self.fetch_page(key, cwd, target, 0, true, cx);
            }
            return;
        }
        self.request_task = None;
        self.fetch_task = None;
        self.fetching_all = false;
        self.fetch_for = None;
        self.fetch_error = None;
        self.loading = false;
        self.target_key = Some(key.clone());
        self.commits.clear();
        self.visible_commits.clear();
        self.branch_tips.clear();
        self.collapsed_branches.clear();
        self.collapsed_counts.clear();
        self.head_sha = None;
        self.next_cursor = None;
        self.total_count = None;
        self.head_commit_count = None;
        self.comparison = None;
        self.search_query.clear();
        self.search_results = None;
        self.search_next_cursor = None;
        self.search_total_count = None;
        self.search_loading = false;
        self.search_error = None;
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_scroll_anchor = None;
        self.search_task = None;
        self.error = None;
        self.graph = GraphLayout::default();
        self.graph_lane_capacity = 0;
        self.list.reset(0);
        self.hovered_path = None;
        self.hovered_graph_path = None;
        self.hovered_row_path = None;
        self.graph_hover_active = false;
        self.graph_hover_clear_task = None;
        self.avatar_images.clear();
        self.fetch_page(key, cwd, target, 0, true, cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        self.target_key = Some(key.clone());
        self.fetch_page(key, cwd, target, 0, true, cx);
    }

    pub fn fetch_all(&mut self, cx: &mut Context<Self>) {
        if self.fetching_all {
            return;
        }
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.fetching_all = true;
        self.fetch_for = Some(key.clone());
        self.fetch_error = None;
        cx.notify();
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), serde_json::Value::String(cwd.clone()));
            if let Some(target) = target.clone() {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(methods::FETCH_ALL, serde_json::Value::Object(params))
                .await;
            this.update(cx, |history, cx| {
                if history.fetch_for.as_deref() != Some(key.as_str()) {
                    return;
                }
                history.fetching_all = false;
                history.fetch_for = None;
                match result {
                    Ok(_) => {
                        history.fetch_error = None;
                        // Cancel a pre-fetch history request so the next page
                        // is guaranteed to observe the updated remote refs.
                        history.request_task = None;
                        history.loading = false;
                        history.fetch_page(key, cwd, target, 0, true, cx);
                        cx.emit(GitHistoryEvent::FetchSucceeded);
                    }
                    Err(error) => history.fetch_error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn commit_count(&self) -> Option<usize> {
        if self.search_active() {
            self.search_total_count.or(Some(self.visible_commits.len()))
        } else {
            self.head_commit_count
        }
    }

    fn recompute_view(&mut self) {
        if self.search_active() {
            let source = self.search_results.as_ref().unwrap_or(&self.commits);
            let visible: HashSet<_> = source
                .iter()
                .filter(|commit| git_history_matches(&self.search_query, commit))
                .map(|commit| commit.sha.clone())
                .collect();
            self.visible_commits = compact_commits_to_visible(source, &visible);
            self.collapsed_counts.clear();
            self.update_graph_layout();
            return;
        }
        match self.view_mode {
            GitHistoryViewMode::AllCommits => {
                let (visible, counts) = collapse_branch_runs(
                    &self.commits,
                    &self.collapsed_branches,
                    self.head_sha.as_deref(),
                );
                self.visible_commits = visible;
                self.collapsed_counts = counts;
            }
            GitHistoryViewMode::BranchTips => {
                // Immediate parents generally are not tips themselves. Clear
                // them rather than painting false dangling connections; the
                // branch badges carry the stable identity in this overview.
                self.visible_commits = self
                    .branch_tips
                    .iter()
                    .cloned()
                    .map(|mut commit| {
                        commit.parent_shas.clear();
                        commit
                    })
                    .collect();
                self.collapsed_counts.clear();
            }
        }
        self.update_graph_layout();
    }

    fn update_graph_layout(&mut self) {
        self.graph = layout_graph(&self.visible_commits, self.head_sha.as_deref());
        // The commit subject starts immediately after the graph column. Keep
        // that column at the widest lane count seen for this repository so a
        // fold cannot move every title sideways when its compact graph settles.
        let loaded_lane_count =
            layout_graph(&self.commits, self.head_sha.as_deref()).max_lane_count;
        self.graph_lane_capacity = self
            .graph_lane_capacity
            .max(self.graph.max_lane_count)
            .max(loaded_lane_count);
    }

    fn rebuild_view(&mut self, cx: &mut Context<Self>) {
        self.view_transition = None;
        self.view_transition_task = None;
        self.recompute_view();
        let item_count = self.visible_commits.len() + usize::from(self.has_load_more());
        self.list
            .reset_with_uniform_height(item_count, px(HISTORY_ROW_HEIGHT));
        self.hovered_path = None;
        self.hovered_graph_path = None;
        self.hovered_row_path = None;
        self.graph_hover_active = false;
        self.graph_hover_clear_task = None;
        cx.notify();
    }

    fn search_active(&self) -> bool {
        !self.search_query.trim().is_empty()
    }

    fn has_load_more(&self) -> bool {
        if self.search_active() {
            self.search_next_cursor.is_some()
        } else {
            self.view_mode == GitHistoryViewMode::AllCommits && self.next_cursor.is_some()
        }
    }

    fn current_scroll_anchor(&self) -> Option<HistoryScrollAnchor> {
        let scroll_top = self.list.logical_scroll_top();
        let commit = self
            .visible_commits
            .get(scroll_top.item_ix)
            .or_else(|| self.visible_commits.last())?;
        Some(HistoryScrollAnchor {
            sha: commit.sha.clone(),
            offset_in_item: if scroll_top.item_ix < self.visible_commits.len() {
                scroll_top.offset_in_item
            } else {
                px(0.0)
            },
        })
    }

    fn restore_scroll_anchor(&self, anchor: Option<&HistoryScrollAnchor>) {
        let Some(anchor) = anchor else {
            return;
        };
        let Some(item_ix) = self
            .visible_commits
            .iter()
            .position(|commit| commit.sha == anchor.sha)
        else {
            return;
        };
        self.list.scroll_to(ListOffset {
            item_ix,
            offset_in_item: anchor.offset_in_item,
        });
    }

    fn reconcile_list_items(
        &self,
        old: &[GitHistoryCommit],
        old_has_load_more: bool,
        target: &[GitHistoryCommit],
        target_has_load_more: bool,
    ) {
        if let Some((range, count)) =
            history_list_splice(old, old_has_load_more, target, target_has_load_more)
        {
            self.list.splice(range, count);
        }
    }

    fn settle_view_transition(&mut self, cx: &mut Context<Self>) {
        let Some(transition) = self.view_transition.take() else {
            return;
        };
        // Resolve at settle time rather than reusing the click-time anchor: if
        // the user scrolls during the tween, that newer position must win.
        let final_scroll_anchor = resolve_history_scroll_anchor(
            self.current_scroll_anchor(),
            &self.visible_commits,
            &transition.final_commits,
        );
        let old_has_load_more = self.list.item_count() > self.visible_commits.len();
        let final_has_load_more = self.has_load_more();
        self.reconcile_list_items(
            &self.visible_commits,
            old_has_load_more,
            &transition.final_commits,
            final_has_load_more,
        );
        self.visible_commits = transition.final_commits;
        self.collapsed_counts = transition.final_collapsed_counts;
        self.update_graph_layout();
        self.restore_scroll_anchor(final_scroll_anchor.as_ref());
        self.view_transition_task = None;
        cx.notify();
    }

    fn apply_view_change(&mut self, animate_rows: bool, cx: &mut Context<Self>) {
        // A second click during the short tween starts from the previous
        // destination, preventing zero-height transitional rows from leaking
        // into the next merge.
        if self.view_transition.is_some() {
            self.settle_view_transition(cx);
        }
        let scroll_anchor = self.current_scroll_anchor();
        let old_commits = self.visible_commits.clone();
        let old_has_load_more = self.list.item_count() > old_commits.len();
        self.recompute_view();
        let final_commits = std::mem::take(&mut self.visible_commits);
        let final_collapsed_counts = std::mem::take(&mut self.collapsed_counts);
        let final_scroll_anchor =
            resolve_history_scroll_anchor(scroll_anchor.clone(), &old_commits, &final_commits);
        let final_has_load_more = self.has_load_more();

        let unchanged = old_commits.len() == final_commits.len()
            && old_commits
                .iter()
                .zip(&final_commits)
                .all(|(old, new)| old.sha == new.sha);
        if unchanged || !animate_rows || crate::motion::reduced_motion(cx) {
            self.visible_commits = final_commits;
            self.collapsed_counts = final_collapsed_counts;
            self.update_graph_layout();
            self.reconcile_list_items(
                &old_commits,
                old_has_load_more,
                &self.visible_commits,
                final_has_load_more,
            );
            self.restore_scroll_anchor(final_scroll_anchor.as_ref());
            cx.notify();
            return;
        }

        let (transition_commits, rows) = history_transition_rows(&old_commits, &final_commits);
        self.reconcile_list_items(
            &old_commits,
            old_has_load_more,
            &transition_commits,
            final_has_load_more,
        );
        self.visible_commits = transition_commits;
        self.collapsed_counts = final_collapsed_counts.clone();
        self.update_graph_layout();
        self.restore_scroll_anchor(scroll_anchor.as_ref());
        let epoch = self.view_epoch;
        self.view_transition = Some(HistoryViewTransition {
            rows,
            final_commits,
            final_collapsed_counts,
            epoch,
        });
        let duration = crate::motion::COLLAPSE
            .total()
            .mul_f32(crate::motion::speed_scale());
        self.view_transition_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            this.update(cx, |history, cx| {
                if history
                    .view_transition
                    .as_ref()
                    .is_some_and(|transition| transition.epoch == epoch)
                {
                    history.settle_view_transition(cx);
                }
            })
            .ok();
        }));
        self.hovered_path = None;
        self.hovered_graph_path = None;
        self.hovered_row_path = None;
        self.graph_hover_active = false;
        self.graph_hover_clear_task = None;
        cx.notify();
    }

    fn set_view_mode(&mut self, mode: GitHistoryViewMode, cx: &mut Context<Self>) {
        let cleared_individual =
            mode == GitHistoryViewMode::AllCommits && !self.collapsed_branches.is_empty();
        if cleared_individual {
            self.collapsed_branches.clear();
        }
        if self.view_mode == mode && !cleared_individual {
            return;
        }
        self.view_mode = mode;
        self.view_epoch = self.view_epoch.wrapping_add(1);
        self.apply_view_change(false, cx);
    }

    fn toggle_branch_ref(&mut self, reference: GitHistoryRef, cx: &mut Context<Self>) {
        let Some(key) = branch_ref_key(&reference) else {
            return;
        };
        self.view_mode = GitHistoryViewMode::AllCommits;
        self.view_epoch = self.view_epoch.wrapping_add(1);
        if !self.collapsed_branches.remove(&key) {
            self.collapsed_branches.insert(key);
        }
        self.apply_view_change(true, cx);
    }

    fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.trim_start().to_string();
        if self.search_query == query {
            return;
        }
        let was_active = self.search_active();
        if !was_active && !query.trim().is_empty() {
            self.search_scroll_anchor = self.current_scroll_anchor();
        }
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_task = None;
        self.search_query = query;
        self.search_results = None;
        self.search_next_cursor = None;
        self.search_total_count = None;
        self.search_loading = false;
        self.search_error = None;

        if !self.search_active() {
            self.rebuild_view(cx);
            let anchor = self.search_scroll_anchor.take();
            self.restore_scroll_anchor(anchor.as_ref());
            cx.notify();
            return;
        }

        // The loaded page filters synchronously on the keystroke. The complete
        // repository search replaces it after a tiny debounce without changing
        // topological ordering.
        self.rebuild_view(cx);
        self.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        });
        let query = self.search_query.clone();
        self.request_search_page(query, 0, true, HISTORY_SEARCH_DEBOUNCE, cx);
    }

    fn request_search_page(
        &mut self,
        query: String,
        cursor: usize,
        reset: bool,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let generation = self.search_generation;
        self.search_loading = true;
        self.search_error = None;
        cx.notify();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
            params.insert("query".into(), serde_json::Value::String(query.clone()));
            params.insert("cursor".into(), serde_json::json!(cursor));
            params.insert("limit".into(), serde_json::json!(HISTORY_PAGE_SIZE));
            if let Some(target) = target.clone() {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(
                    methods::SEARCH_GIT_HISTORY,
                    serde_json::Value::Object(params),
                )
                .await;
            this.update(cx, |history, cx| {
                if history.target_key.as_deref() != Some(key.as_str())
                    || history.search_generation != generation
                    || history.search_query != query
                {
                    return;
                }
                history.search_loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<GitHistoryPage>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(page) => {
                        let anchor = history.current_scroll_anchor();
                        if reset {
                            history.search_results = Some(page.commits);
                        } else {
                            let results = history.search_results.get_or_insert_default();
                            let mut seen: HashSet<_> =
                                results.iter().map(|commit| commit.sha.clone()).collect();
                            results.extend(
                                page.commits
                                    .into_iter()
                                    .filter(|commit| seen.insert(commit.sha.clone())),
                            );
                        }
                        history.search_next_cursor = page.next_cursor;
                        history.search_total_count = page.total_count;
                        history.rebuild_view(cx);
                        history.restore_scroll_anchor(anchor.as_ref());
                        history.resolve_avatars(key, cwd, target, cursor, cx);
                    }
                    Err(error) => {
                        // Keep the instantaneous local matches useful when a
                        // remote/device search is temporarily unavailable.
                        history.search_error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        if self.search_active() {
            if self.search_loading {
                return;
            }
            let Some(cursor) = self.search_next_cursor else {
                return;
            };
            self.request_search_page(self.search_query.clone(), cursor, false, Duration::ZERO, cx);
            return;
        }
        let Some(cursor) = self.next_cursor else {
            return;
        };
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        self.fetch_page(key, cwd, target, cursor, false, cx);
    }

    fn resolve_avatars(
        &mut self,
        key: String,
        cwd: String,
        target: Option<String>,
        cursor: usize,
        cx: &mut Context<Self>,
    ) {
        if configured_author_display(cx) != GitHistoryAuthorDisplay::Avatar {
            return;
        }
        let mut unique_authors = HashMap::new();
        for commit in self
            .commits
            .iter()
            .chain(self.search_results.iter().flatten())
        {
            let email = commit.author_email.trim().to_ascii_lowercase();
            if !email.is_empty() {
                unique_authors
                    .entry(email)
                    .or_insert_with(|| (commit.sha.clone(), commit.author_email.clone()));
            }
        }
        let authors: Vec<_> = unique_authors
            .into_values()
            .map(|(sha, email)| serde_json::json!({ "sha": sha, "email": email }))
            .collect();
        if authors.is_empty() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut params = serde_json::Map::new();
        params.insert("cwd".into(), serde_json::Value::String(cwd));
        params.insert("authors".into(), serde_json::Value::Array(authors));
        params.insert("cursor".into(), serde_json::json!(cursor));
        params.insert("limit".into(), serde_json::json!(HISTORY_PAGE_SIZE));
        if let Some(target) = target {
            params.insert("targetDeviceId".into(), serde_json::Value::String(target));
        }
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::RESOLVE_GIT_AVATARS,
                    serde_json::Value::Object(params),
                )
                .await;
            this.update(cx, |history, cx| {
                if history.target_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                if let Ok(avatars) = result.and_then(|value| {
                    serde_json::from_value::<HashMap<String, String>>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    history.avatar_images.extend(avatars.into_iter().filter_map(
                        |(email, encoded)| {
                            decode_history_avatar(&encoded).map(|image| (email, image))
                        },
                    ));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn resolve_loaded_avatars(&mut self, cx: &mut Context<Self>) {
        if self.commits.is_empty() {
            return;
        }
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        for cursor in (0..self.commits.len()).step_by(HISTORY_PAGE_SIZE) {
            self.resolve_avatars(key.clone(), cwd.clone(), target.clone(), cursor, cx);
        }
    }

    fn fetch_page(
        &mut self,
        key: String,
        cwd: String,
        target: Option<String>,
        cursor: usize,
        reset: bool,
        cx: &mut Context<Self>,
    ) {
        if self.loading {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.loading = true;
        self.error = None;
        cx.notify();
        self.request_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
            params.insert("cursor".into(), serde_json::json!(cursor));
            params.insert("limit".into(), serde_json::json!(HISTORY_PAGE_SIZE));
            if let Some(target) = target.clone() {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(methods::LIST_GIT_HISTORY, serde_json::Value::Object(params))
                .await;
            this.update(cx, |history, cx| {
                if history.target_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                history.loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<GitHistoryPage>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(page) => {
                        let restart_search = (reset && history.search_active())
                            .then(|| history.search_query.clone());
                        if restart_search.is_some() {
                            history.search_generation = history.search_generation.wrapping_add(1);
                            history.search_task = None;
                            history.search_results = None;
                            history.search_next_cursor = None;
                            history.search_total_count = None;
                            history.search_loading = false;
                            history.search_error = None;
                        }
                        let old_visible_count = history.visible_commits.len();
                        let old_item_count =
                            old_visible_count + usize::from(history.has_load_more());
                        if reset {
                            history.commits = page.commits;
                            history.branch_tips = page.branch_tips;
                            history.total_count = page.total_count;
                            history.head_commit_count = page.head_commit_count;
                            history.comparison = page.comparison;
                        } else {
                            let mut seen: HashSet<String> = history
                                .commits
                                .iter()
                                .map(|commit| commit.sha.clone())
                                .collect();
                            history.commits.extend(
                                page.commits
                                    .into_iter()
                                    .filter(|commit| seen.insert(commit.sha.clone())),
                            );
                            if page.total_count.is_some() {
                                history.total_count = page.total_count;
                            }
                            if page.head_commit_count.is_some() {
                                history.head_commit_count = page.head_commit_count;
                            }
                        }
                        history.head_sha = page.head_sha;
                        history.next_cursor = page.next_cursor;
                        let incremental_all = !history.search_active()
                            && !reset
                            && history.view_mode == GitHistoryViewMode::AllCommits
                            && history.collapsed_branches.is_empty();
                        if incremental_all {
                            history.recompute_view();
                            let new_item_count = history.visible_commits.len()
                                + usize::from(history.next_cursor.is_some());
                            history.list.splice(
                                old_visible_count..old_item_count,
                                new_item_count - old_visible_count,
                            );
                        } else {
                            history.rebuild_view(cx);
                        }
                        history.resolve_avatars(
                            key.clone(),
                            cwd.clone(),
                            target.clone(),
                            cursor,
                            cx,
                        );
                        if let Some(query) = restart_search {
                            history.request_search_page(query, 0, true, Duration::ZERO, cx);
                        }
                    }
                    Err(error) => history.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn copy_sha(&mut self, sha: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()));
        self.copied_sha = Some(sha);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1_200))
                .await;
            this.update(cx, |history, cx| {
                history.copied_sha = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn graph_hover_key(cx: &Context<Self>) -> String {
        format!("history-graph-focus-{}", cx.entity_id())
    }

    fn graph_focus(&self, cx: &Context<Self>) -> Option<GraphFocus> {
        if self.graph_hover_suppressed.get() {
            return None;
        }
        self.hovered_path.map(|color_id| GraphFocus {
            color_id,
            amount: crate::motion::hover_t(&Self::graph_hover_key(cx)),
        })
    }

    fn set_history_hover(&mut self, path: Option<usize>, cx: &mut Context<Self>) {
        if let Some(path) = path {
            if self.graph_hover_active && self.hovered_path == Some(path) {
                return;
            }
            self.graph_hover_clear_task = None;
            self.graph_hover_active = true;
            self.hovered_path = Some(path);
            crate::motion::set_hover(
                &Self::graph_hover_key(cx),
                true,
                crate::motion::reduced_motion(cx),
            );
            cx.notify();
            return;
        }

        if !self.graph_hover_active {
            return;
        }
        self.graph_hover_active = false;
        let reduced_motion = crate::motion::reduced_motion(cx);
        crate::motion::set_hover(&Self::graph_hover_key(cx), false, reduced_motion);
        if reduced_motion {
            self.hovered_path = None;
            self.graph_hover_clear_task = None;
        } else {
            let fading_path = self.hovered_path;
            let duration = crate::motion::HOVER_FADE
                .total()
                .mul_f32(crate::motion::speed_scale());
            self.graph_hover_clear_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(duration).await;
                this.update(cx, |history, cx| {
                    if !history.graph_hover_active && history.hovered_path == fading_path {
                        history.hovered_path = None;
                        history.graph_hover_clear_task = None;
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
        cx.notify();
    }

    fn set_graph_hover(&mut self, path: Option<usize>, cx: &mut Context<Self>) {
        self.hovered_graph_path = path;
        self.set_history_hover(path.or(self.hovered_row_path), cx);
    }

    fn set_row_hover(&mut self, path: Option<usize>, cx: &mut Context<Self>) {
        if path.is_some() && self.graph_hover_suppressed.replace(false) {
            cx.notify();
        }
        self.hovered_row_path = path;
        self.set_history_hover(self.hovered_graph_path.or(path), cx);
    }

    fn update_graph_hover(
        &mut self,
        row_index: usize,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.graph_hover_suppressed.replace(false) {
            cx.notify();
        }
        let Some(bounds) = self.list.bounds_for_item(row_index) else {
            self.set_graph_hover(None, cx);
            return;
        };
        let Some(row) = self.graph.rows.get(row_index) else {
            self.set_graph_hover(None, cx);
            return;
        };
        let local_x = f32::from(position.x - bounds.left());
        let local_y = f32::from(position.y - bounds.top());
        self.set_graph_hover(
            hovered_graph_path(row, local_x, local_y, self.graph_geometry.get()),
            cx,
        );
    }

    fn toggle_column(&mut self, column: GitHistoryColumn, cx: &mut Context<Self>) {
        let (columns, widths, order) = {
            let preferences = cx.global_mut::<HistoryColumnPreferences>();
            match column {
                GitHistoryColumn::Author => {
                    preferences.columns.author = !preferences.columns.author
                }
                GitHistoryColumn::Date => preferences.columns.date = !preferences.columns.date,
                GitHistoryColumn::Sha => preferences.columns.sha = !preferences.columns.sha,
            }
            (
                preferences.columns,
                preferences.widths,
                preferences.order.clone(),
            )
        };
        Self::persist_column_layout(columns, widths, &order, SavePolicy::Immediate, cx);
        cx.refresh_windows();
        cx.notify();
    }

    fn reset_columns(&mut self, cx: &mut Context<Self>) {
        let columns = GitHistoryColumns::default();
        let widths = GitHistoryColumnWidths::default();
        let order = GitHistoryColumnOrder::default();
        {
            let preferences = cx.global_mut::<HistoryColumnPreferences>();
            preferences.columns = columns;
            preferences.widths = widths;
            preferences.order = order.clone();
        }
        Self::persist_column_layout(columns, widths, &order, SavePolicy::Immediate, cx);
        cx.refresh_windows();
        cx.notify();
    }

    fn persist_column_layout(
        columns: GitHistoryColumns,
        widths: GitHistoryColumnWidths,
        order: &GitHistoryColumnOrder,
        policy: SavePolicy,
        cx: &mut Context<Self>,
    ) {
        let order = order.clone();
        settings::update(policy, cx, move |settings| {
            settings.git_history_columns = columns;
            settings.git_history_column_widths = widths;
            settings.git_history_column_order = order;
        });
    }

    fn schedule_column_layout_save(&mut self, cx: &mut Context<Self>) {
        let (columns, widths, order) = {
            let preferences = cx.global::<HistoryColumnPreferences>();
            (
                preferences.columns,
                preferences.widths,
                preferences.order.clone(),
            )
        };
        Self::persist_column_layout(columns, widths, &order, SavePolicy::Debounced, cx);
    }

    fn begin_column_resize(
        &mut self,
        left: HistoryDataColumn,
        right: HistoryDataColumn,
        start_x: f32,
        cx: &mut Context<Self>,
    ) {
        let widths = configured_column_widths(cx);
        self.column_drag_anchor = Some(HistoryColumnDragAnchor {
            start_x,
            left,
            right,
            left_width: history_column_width(left, widths),
            right_width: history_column_width(right, widths),
        });
    }

    fn on_column_resize(
        &mut self,
        event: &gpui::DragMoveEvent<HistoryColumnResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(anchor) = self.column_drag_anchor else {
            return;
        };
        let requested_delta = f32::from(event.event.position.x) - anchor.start_x;
        // Commit owns the flexible remainder. Interior dividers instead
        // preserve their pair's total width, so no drag creates overflow.
        let widths =
            resized_history_column_widths(configured_column_widths(cx), anchor, requested_delta);

        cx.global_mut::<HistoryColumnPreferences>().widths = widths;
        self.schedule_column_layout_save(cx);
        cx.refresh_windows();
        cx.notify();
    }

    fn reset_column_widths(&mut self, cx: &mut Context<Self>) {
        cx.global_mut::<HistoryColumnPreferences>().widths = GitHistoryColumnWidths::default();
        self.column_drag_anchor = None;
        self.schedule_column_layout_save(cx);
        cx.refresh_windows();
        cx.notify();
    }

    fn column_resize_handle(
        &self,
        left: HistoryDataColumn,
        right: HistoryDataColumn,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover = theme.border_strong;
        div()
            .id(SharedString::from(format!(
                "history-resize-{left:?}-{right:?}"
            )))
            .absolute()
            .left(px(-3.0))
            .top_0()
            .bottom_0()
            .w(px(6.0))
            .cursor_col_resize()
            .hover(move |style| style.bg(hover.opacity(0.7)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.begin_column_resize(left, right, f32::from(event.position.x), cx);
                }),
            )
            .on_drag(
                HistoryColumnResize,
                |_, _point: gpui::Point<gpui::Pixels>, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| HistoryResizeGhost)
                },
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.reset_column_widths(cx);
                    } else {
                        this.column_drag_anchor = None;
                    }
                }),
            )
            .into_any_element()
    }

    fn render_column_header_cell(
        &self,
        column: GitHistoryColumn,
        left: HistoryDataColumn,
        index: usize,
        drag: Option<HistoryColumnDragState>,
        widths: GitHistoryColumnWidths,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let data_column = history_data_column(column);
        let width = history_optional_width(column, widths);
        let (min_width, _) = history_column_limits(data_column);
        let label = history_column_label(column);
        let id = match column {
            GitHistoryColumn::Author => "history-author-header",
            GitHistoryColumn::Date => "history-date-header",
            GitHistoryColumn::Sha => "history-sha-header",
        };
        let resize = self.column_resize_handle(left, data_column, theme, cx);
        let indicator = drag
            .filter(|state| state.over == index && state.from != state.over)
            .map(|state| {
                let place_after = state.from < state.over;
                div()
                    .absolute()
                    .top(px(3.0))
                    .bottom(px(3.0))
                    .w(px(2.0))
                    .rounded_full()
                    .bg(theme.accent)
                    .when(place_after, |line| line.right_0())
                    .when(!place_after, |line| line.left_0())
            });
        let ghost_label: SharedString = label.into();
        div()
            .id(id)
            .relative()
            .w(px(width))
            .min_w(px(min_width))
            .h_full()
            .flex_shrink(1.0)
            .flex()
            .items_center()
            .cursor_pointer()
            .when(column == GitHistoryColumn::Author, |header| {
                header.justify_center().on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                        this.open_author_menu(event.position, cx);
                    }),
                )
            })
            .on_drag(
                HistoryColumnDrag {
                    column,
                    label: ghost_label,
                },
                |payload, _point, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| HistoryColumnGhost {
                        label: payload.label.clone(),
                    })
                },
            )
            .children(indicator)
            .child(label)
            // Paint last so this narrow hitbox wins over the header drag.
            .child(resize)
            .into_any_element()
    }

    fn on_column_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<HistoryColumnDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let payload = event.drag(cx);
        let columns = configured_columns(cx);
        let order = configured_column_order(cx);
        let visible = visible_history_columns(&order, columns);
        let Some(from) = visible.iter().position(|column| *column == payload.column) else {
            return;
        };
        let relative_x = f32::from(event.event.position.x) - f32::from(event.bounds.left());
        let over = history_column_drop_index(
            relative_x,
            f32::from(event.bounds.size.width),
            &visible,
            configured_column_widths(cx),
        );
        if self
            .column_drag
            .is_some_and(|state| state.from == from && state.over == over)
        {
            return;
        }
        self.column_drag = Some(HistoryColumnDragState { from, over });
        cx.notify();
    }

    fn commit_column_reorder(&mut self, dragged: GitHistoryColumn, cx: &mut Context<Self>) {
        let columns = configured_columns(cx);
        let current = configured_column_order(cx);
        let visible = visible_history_columns(&current, columns);
        let target = self
            .column_drag
            .and_then(|state| visible.get(state.over).copied())
            .unwrap_or(dragged);
        let reordered = reordered_history_columns(&current, dragged, target);
        self.column_drag = None;
        if reordered == current {
            cx.notify();
            return;
        }
        cx.global_mut::<HistoryColumnPreferences>().order = reordered;
        self.schedule_column_layout_save(cx);
        cx.refresh_windows();
        cx.notify();
    }

    fn open_column_menu(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.close_author_menu(cx);
        self.column_menu.open(position);
        cx.notify();
    }

    fn close_column_menu(&mut self, cx: &mut Context<Self>) {
        if self.column_menu.begin_close() {
            popover::reap_popup(cx, |history: &mut Self| &mut history.column_menu);
        }
    }

    fn open_author_menu(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.close_column_menu(cx);
        self.author_menu.open(position);
        cx.notify();
    }

    fn close_author_menu(&mut self, cx: &mut Context<Self>) {
        if self.author_menu.begin_close() {
            popover::reap_popup(cx, |history: &mut Self| &mut history.author_menu);
        }
    }

    fn toggle_author_display(&mut self, cx: &mut Context<Self>) {
        let display = {
            let preferences = cx.global_mut::<HistoryColumnPreferences>();
            preferences.author_display = match preferences.author_display {
                GitHistoryAuthorDisplay::Avatar => GitHistoryAuthorDisplay::Name,
                GitHistoryAuthorDisplay::Name => GitHistoryAuthorDisplay::Avatar,
            };
            preferences.author_display
        };
        settings::update(SavePolicy::Immediate, cx, |settings| {
            settings.git_history_author_display = display;
        });
        if display == GitHistoryAuthorDisplay::Avatar {
            self.resolve_loaded_avatars(cx);
        }
        cx.refresh_windows();
        cx.notify();
    }

    fn render_author_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let show_name = configured_author_display(cx) == GitHistoryAuthorDisplay::Name;
        popover::popover_card(theme)
            .w(px(116.0))
            .p(px(3.0))
            .rounded(px(9.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_author_menu(cx)))
            .child(
                popover::menu_row(theme, false, "history-author-display-name")
                    .id("history-author-display-name")
                    .gap(px(0.0))
                    .px(px(7.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .text_size(px(11.5))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_author_display(cx);
                        this.close_author_menu(cx);
                    }))
                    .child(div().flex_1().child("Name"))
                    .child(div().w(px(12.0)).flex_none().flex().justify_end().when(
                        show_name,
                        |element| {
                            element.child(
                                crate::icons::icon(crate::icons::CHECK)
                                    .size(px(10.0))
                                    .text_color(theme.text_muted),
                            )
                        },
                    )),
            )
            .into_any_element()
    }

    fn render_column_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let columns = configured_columns(cx);
        let widths = configured_column_widths(cx);
        let order = configured_column_order(cx);
        let option =
            |label: &'static str,
             checked: bool,
             column: GitHistoryColumn,
             index: usize,
             cx: &mut Context<Self>| {
                popover::menu_row(
                    theme,
                    false,
                    SharedString::from(format!("history-column-option-{index}")),
                )
                .id(("history-column-option", index))
                .gap(px(0.0))
                .px(px(7.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .text_size(px(11.5))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_column(column, cx);
                }))
                .child(div().flex_1().child(label))
                .child(div().w(px(12.0)).flex_none().flex().justify_end().when(
                    checked,
                    |element| {
                        element.child(
                            crate::icons::icon(crate::icons::CHECK)
                                .size(px(10.0))
                                .text_color(theme.text_muted),
                        )
                    },
                ))
            };
        let defaults = GitHistoryColumns::default();
        let can_reset = columns != defaults
            || widths != GitHistoryColumnWidths::default()
            || order != GitHistoryColumnOrder::default();

        popover::popover_card(theme)
            .w(px(132.0))
            .p(px(3.0))
            .rounded(px(9.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_column_menu(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(option(
                        "Author",
                        columns.author,
                        GitHistoryColumn::Author,
                        0,
                        cx,
                    ))
                    .child(option("Date", columns.date, GitHistoryColumn::Date, 1, cx))
                    .child(option("SHA", columns.sha, GitHistoryColumn::Sha, 2, cx))
                    .when(can_reset, |menu| {
                        menu.child(
                            div()
                                .h(px(1.0))
                                .mx(px(5.0))
                                .my(px(2.0))
                                .bg(crate::theme::hairline(0.08)),
                        )
                        .child(
                            popover::menu_row(theme, false, "history-columns-reset")
                                .id("history-columns-reset")
                                .px(px(7.0))
                                .py(px(4.0))
                                .rounded(px(6.0))
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.reset_columns(cx);
                                    this.close_column_menu(cx);
                                }))
                                .child("Reset"),
                        )
                    }),
            )
            .into_any_element()
    }

    fn graph_paths(&self, theme: &Theme, focus: Option<GraphFocus>) -> AnyElement {
        let palette = [
            graph_color(theme.accent),
            graph_color(theme.busy),
            graph_color(theme.success),
            graph_color(theme.warning),
            graph_color(theme.danger),
            graph_color(theme.text_muted),
        ];
        let graph_bg = theme.bg;
        let rows = self.graph.rows.clone();
        let list = self.list.clone();
        // Branch tips are independent overview entries: their parents are
        // intentionally removed in `recompute_view`. Do not restore a false
        // relationship between adjacent tips just because the graph has
        // collapsed into its narrow rail.
        let show_compact_rail = self.view_mode == GitHistoryViewMode::AllCommits;
        let graph_geometry = self.graph_geometry.clone();
        let graph_hover_suppressed = self.graph_hover_suppressed.clone();
        canvas(
            |_, _, _| (),
            move |viewport_bounds, _, window, _| {
                let geometry = graph_geometry.get();
                let focus = if graph_hover_suppressed.get() {
                    None
                } else {
                    focus
                };
                let pass_count = if focus.is_some() { 2 } else { 1 };
                if geometry.compact && show_compact_rail {
                    let rail_x = geometry.lane_x(0);
                    for pass in 0..pass_count {
                        let selected_pass = focus.is_some() && pass == 1;
                        for (color_index, color) in palette.iter().enumerate() {
                            let stroke_width = focus
                                .filter(|_| selected_pass)
                                .map(|focus| {
                                    HISTORY_STROKE_WIDTH
                                        + (HISTORY_GRAPH_FOCUSED_STROKE_WIDTH
                                            - HISTORY_STROKE_WIDTH)
                                            * focus.amount
                                })
                                .unwrap_or(HISTORY_STROKE_WIDTH);
                            let mut builder = PathBuilder::stroke(px(stroke_width));
                            let mut has_segments = false;
                            for (index, row) in rows.iter().enumerate() {
                                if row.node_color_id % palette.len() != color_index
                                    || focus.is_some_and(|focus| {
                                        (row.node_color_id == focus.color_id) != selected_pass
                                    })
                                {
                                    continue;
                                }
                                let Some(row_bounds) = list.bounds_for_item(index) else {
                                    continue;
                                };
                                if row_bounds.bottom() < viewport_bounds.top()
                                    || row_bounds.top() > viewport_bounds.bottom()
                                {
                                    continue;
                                }
                                let row_height = f32::from(row_bounds.size.height);
                                if row_height <= 0.5 {
                                    continue;
                                }
                                let start = point(
                                    row_bounds.origin.x + px(rail_x),
                                    row_bounds.origin.y + px(row_height / 2.0),
                                );
                                let end_y = list
                                    .bounds_for_item(index + 1)
                                    .map(|next| {
                                        next.origin.y + px(f32::from(next.size.height) / 2.0)
                                    })
                                    .unwrap_or_else(|| row_bounds.bottom());
                                builder.move_to(start);
                                builder.line_to(point(row_bounds.origin.x + px(rail_x), end_y));
                                has_segments = true;
                            }
                            if has_segments && let Ok(path) = builder.build() {
                                let mut paint_color = *color;
                                if let Some(focus) = focus
                                    && !selected_pass
                                {
                                    // Keep the dimmed stroke opaque: overlapping row
                                    // segments otherwise stack alpha into tiny dark dots.
                                    paint_color = crate::motion::mix(
                                        paint_color,
                                        graph_bg,
                                        (1.0 - HISTORY_GRAPH_UNFOCUSED_OPACITY) * focus.amount,
                                    );
                                }
                                window.paint_path(path, paint_color);
                            }
                        }
                    }
                    return;
                }
                for pass in 0..pass_count {
                    let selected_pass = focus.is_some() && pass == 1;
                    for (color_index, color) in palette.iter().enumerate() {
                        let stroke_width = focus
                            .filter(|_| selected_pass)
                            .map(|focus| {
                                HISTORY_STROKE_WIDTH
                                    + (HISTORY_GRAPH_FOCUSED_STROKE_WIDTH - HISTORY_STROKE_WIDTH)
                                        * focus.amount
                            })
                            .unwrap_or(HISTORY_STROKE_WIDTH);
                        let mut builder = PathBuilder::stroke(px(stroke_width));
                        let mut has_segments = false;

                        for (index, row) in rows.iter().enumerate() {
                            let Some(row_bounds) = list.bounds_for_item(index) else {
                                continue;
                            };
                            if row_bounds.bottom() < viewport_bounds.top()
                                || row_bounds.top() > viewport_bounds.bottom()
                            {
                                continue;
                            }
                            let row_height = f32::from(row_bounds.size.height);
                            if row_height <= 0.5 {
                                continue;
                            }
                            let middle = row_height / 2.0;
                            let overlap = HISTORY_GRAPH_ROW_OVERLAP
                                * (row_height / HISTORY_ROW_HEIGHT).clamp(0.0, 1.0);
                            for segment in row.segments.iter().filter(|segment| {
                                segment.color_id % palette.len() == color_index
                                    && focus.is_none_or(|focus| {
                                        (segment.color_id == focus.color_id) == selected_pass
                                    })
                            }) {
                                has_segments = true;
                                let from_x = geometry.lane_x(segment.from_lane);
                                let to_x = geometry.lane_x(segment.to_lane);
                                let origin = row_bounds.origin;
                                match segment.shape {
                                    SegmentShape::Incoming => {
                                        builder.move_to(point(
                                            origin.x + px(from_x),
                                            origin.y - px(overlap),
                                        ));
                                        builder.cubic_bezier_to(
                                            point(origin.x + px(to_x), origin.y + px(middle)),
                                            point(
                                                origin.x + px(from_x),
                                                origin.y + px(middle * 0.55),
                                            ),
                                            point(
                                                origin.x + px(to_x),
                                                origin.y + px(middle * 0.55),
                                            ),
                                        );
                                    }
                                    SegmentShape::Outgoing => {
                                        builder.move_to(point(
                                            origin.x + px(from_x),
                                            origin.y + px(middle),
                                        ));
                                        builder.cubic_bezier_to(
                                            point(
                                                origin.x + px(to_x),
                                                origin.y + px(row_height + overlap),
                                            ),
                                            point(
                                                origin.x + px(from_x),
                                                origin.y + px(middle * 1.45),
                                            ),
                                            point(
                                                origin.x + px(to_x),
                                                origin.y + px(middle * 1.45),
                                            ),
                                        );
                                    }
                                    SegmentShape::Through => {
                                        builder.move_to(point(
                                            origin.x + px(from_x),
                                            origin.y - px(overlap),
                                        ));
                                        let end = point(
                                            origin.x + px(to_x),
                                            origin.y + px(row_height + overlap),
                                        );
                                        if segment.from_lane == segment.to_lane {
                                            builder.line_to(end);
                                        } else {
                                            builder.cubic_bezier_to(
                                                end,
                                                point(origin.x + px(from_x), origin.y + px(middle)),
                                                point(origin.x + px(to_x), origin.y + px(middle)),
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        if has_segments && let Ok(path) = builder.build() {
                            let mut paint_color = *color;
                            if let Some(focus) = focus
                                && !selected_pass
                            {
                                // Keep the dimmed stroke opaque: overlapping row
                                // segments otherwise stack alpha into tiny dark dots.
                                paint_color = crate::motion::mix(
                                    paint_color,
                                    graph_bg,
                                    (1.0 - HISTORY_GRAPH_UNFOCUSED_OPACITY) * focus.amount,
                                );
                            }
                            window.paint_path(path, paint_color);
                        }
                    }
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    fn graph_cell(
        &mut self,
        row_index: usize,
        row: GraphRow,
        focus: Option<GraphFocus>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let geometry = self.graph_geometry.get();
        let width = geometry.width;
        let palette = [
            graph_color(theme.accent),
            graph_color(theme.busy),
            graph_color(theme.success),
            graph_color(theme.warning),
            graph_color(theme.danger),
            graph_color(theme.text_muted),
        ];
        let mut color = palette[row.node_color_id % palette.len()];
        let selected = focus.is_some_and(|focus| focus.color_id == row.node_color_id);
        if let Some(focus) = focus
            && !selected
        {
            color.a *= 1.0 - (1.0 - HISTORY_GRAPH_UNFOCUSED_OPACITY) * focus.amount;
        }
        let node_radius = HISTORY_NODE_RADIUS
            + focus
                .filter(|_| selected)
                .map(|focus| focus.amount * 0.75)
                .unwrap_or_default();
        let node_x = geometry.lane_x(row.node_lane);
        let fold_reference = (self.view_mode == GitHistoryViewMode::AllCommits)
            .then(|| {
                self.visible_commits.get(row_index).and_then(|commit| {
                    commit
                        .refs
                        .iter()
                        .find(|reference| branch_ref_key(reference).is_some())
                        .cloned()
                })
            })
            .flatten();
        let fold_control = fold_reference.map(|reference| {
            let key = branch_ref_key(&reference).unwrap_or_default();
            let collapsed = self.collapsed_branches.contains(&key);
            let hidden_count = self.collapsed_counts.get(&key).copied().unwrap_or_default();
            let tooltip = if collapsed {
                format!(
                    "Expand {}{}",
                    reference.label,
                    if hidden_count == 0 {
                        String::new()
                    } else {
                        format!(" ({hidden_count} hidden)")
                    }
                )
            } else {
                format!("Collapse {}", reference.label)
            };
            let history = cx.entity();
            div()
                .id(("history-graph-fold", row_index))
                .absolute()
                .left(px(node_x + HISTORY_NODE_RADIUS + 3.0))
                .top(px((HISTORY_ROW_HEIGHT - 16.0) / 2.0))
                .size(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(color.opacity(0.32))
                .bg(theme.bg.opacity(0.96))
                .cursor_pointer()
                .opacity(if collapsed { 1.0 } else { 0.0 })
                .group_hover("history-graph-tip", |style| style.opacity(1.0))
                .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    history.update(cx, |history, cx| {
                        history.toggle_branch_ref(reference.clone(), cx)
                    });
                })
                .child(
                    crate::icons::icon(if collapsed {
                        crate::icons::EXPAND_ARROWS
                    } else {
                        crate::icons::FOLD_VERTICAL
                    })
                    .size(px(9.0))
                    .text_color(color.opacity(0.9)),
                )
                .tooltip(move |_, cx| {
                    cx.new(|_| HistoryRefTooltip {
                        descriptions: vec![tooltip.clone().into()],
                    })
                    .into()
                })
                .tooltip_show_delay(Duration::from_millis(250))
        });
        let node = div()
            .absolute()
            .left(px(node_x - node_radius))
            .top(px(HISTORY_ROW_HEIGHT / 2.0 - node_radius))
            .size(px(node_radius * 2.0))
            .rounded_full()
            .bg(color);
        div()
            .id(("history-graph-cell", row_index))
            .relative()
            .group("history-graph-tip")
            .w(px(width))
            .h(px(HISTORY_ROW_HEIGHT))
            .flex_none()
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    this.update_graph_hover(row_index, event.position, cx);
                }),
            )
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.set_graph_hover(None, cx);
                }
            }))
            .when(row.is_head, |element| {
                element.child(
                    div()
                        .absolute()
                        .left(px(node_x - node_radius - HISTORY_HEAD_RING_PADDING))
                        .top(px(HISTORY_ROW_HEIGHT / 2.0
                            - node_radius
                            - HISTORY_HEAD_RING_PADDING))
                        .size(px((node_radius + HISTORY_HEAD_RING_PADDING) * 2.0))
                        .rounded_full()
                        .border_1()
                        .border_color(color)
                        .bg(theme.bg),
                )
            })
            .child(node)
            .children(fold_control)
            .into_any_element()
    }

    fn render_ref(
        reference: GitHistoryRef,
        row_index: usize,
        ref_index: usize,
        theme: &Theme,
    ) -> AnyElement {
        let color = ref_color(&reference, theme);
        let icon = ref_icon(reference.kind);
        let description = ref_description(&reference);
        div()
            .id(SharedString::from(format!(
                "history-ref-{row_index}-{ref_index}"
            )))
            .h(px(16.0))
            .max_w(px(112.0))
            .px(px(5.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .rounded(px(4.0))
            .bg(color.opacity(0.07))
            .text_size(px(10.0))
            .text_color(color.opacity(0.9))
            .child(
                crate::icons::icon(icon)
                    .size(px(10.0))
                    .mt(px(1.0))
                    .text_color(color.opacity(0.78)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(reference.label)),
            )
            .tooltip(move |_, cx| {
                cx.new(|_| HistoryRefTooltip {
                    descriptions: vec![description.clone()],
                })
                .into()
            })
            .tooltip_show_delay(Duration::from_millis(350))
            .into_any_element()
    }

    fn render_ref_area(
        refs: Vec<GitHistoryRef>,
        row_index: usize,
        available_width: f32,
        theme: &Theme,
    ) -> AnyElement {
        let visible_count = visible_ref_count(&refs, available_width);
        let hidden_refs: Vec<_> = refs.iter().skip(visible_count).cloned().collect();
        let hidden_count = hidden_refs.len();
        let hidden_descriptions: Vec<_> = hidden_refs.iter().map(ref_description).collect();

        div()
            .max_w(px(available_width))
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .gap(px(HISTORY_REF_GAP))
            .children(refs.into_iter().take(visible_count).enumerate().map(
                |(ref_index, reference)| Self::render_ref(reference, row_index, ref_index, theme),
            ))
            .when(hidden_count > 0, |element| {
                element.child(
                    div()
                        .id(("history-ref-overflow", row_index))
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("+{hidden_count}")))
                        .tooltip(move |_, cx| {
                            cx.new(|_| HistoryRefTooltip {
                                descriptions: hidden_descriptions.clone(),
                            })
                            .into()
                        })
                        .tooltip_show_delay(Duration::from_millis(350)),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_author_cell(
        index: usize,
        width: f32,
        display: GitHistoryAuthorDisplay,
        name: SharedString,
        initial: SharedString,
        avatar_image: Option<Arc<Image>>,
        opacity: f32,
        theme: &Theme,
    ) -> AnyElement {
        let has_avatar = avatar_image.is_some();
        div()
            .w(px(width))
            .min_w(px(GitHistoryColumnWidths::AUTHOR_MIN))
            .h_full()
            .flex_shrink(1.0)
            .flex()
            .items_center()
            .opacity(opacity)
            .when(display == GitHistoryAuthorDisplay::Avatar, |author| {
                let tooltip_name = name.clone();
                author.justify_center().child(
                    div()
                        .id(("history-author-avatar", index))
                        .size(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .overflow_hidden()
                        .rounded_full()
                        .border_1()
                        .border_color(crate::theme::hairline(0.12))
                        .bg(crate::theme::wash(0.08))
                        .when_some(avatar_image, |avatar, image| {
                            avatar.child(
                                img(image)
                                    .size_full()
                                    .rounded_full()
                                    .object_fit(ObjectFit::Cover),
                            )
                        })
                        .when(!has_avatar, |avatar| {
                            avatar
                                .text_size(px(9.0))
                                .text_color(theme.text_faint)
                                .child(initial)
                        })
                        .tooltip(move |_, cx| {
                            cx.new(|_| HistoryAuthorTooltip {
                                name: tooltip_name.clone(),
                            })
                            .into()
                        })
                        .tooltip_show_delay(Duration::from_millis(300)),
                )
            })
            .when(display == GitHistoryAuthorDisplay::Name, |author| {
                author
                    .pr(px(8.0))
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(name)
            })
            .into_any_element()
    }

    fn render_date_cell(width: f32, authored_at: &str, opacity: f32, theme: &Theme) -> AnyElement {
        div()
            .w(px(width))
            .min_w(px(GitHistoryColumnWidths::DATE_MIN))
            .h_full()
            .flex_shrink(1.0)
            .flex()
            .items_center()
            .truncate()
            .pr(px(8.0))
            .text_size(px(10.5))
            .opacity(opacity)
            .text_color(theme.text_muted)
            .child(SharedString::from(format_date(authored_at)))
            .into_any_element()
    }

    fn render_sha_cell(
        index: usize,
        width: f32,
        sha: String,
        copied: bool,
        opacity: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = if copied {
            "Copied".to_string()
        } else {
            sha.chars().take(7).collect()
        };
        div()
            .w(px(width))
            .min_w(px(GitHistoryColumnWidths::SHA_MIN))
            .h_full()
            .pr(px(6.0))
            .flex_shrink(1.0)
            .flex()
            .items_center()
            .opacity(opacity)
            .child(
                div()
                    .id(SharedString::from(format!("history-sha-{index}")))
                    .w_full()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::ink(0.07)))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.5))
                    .text_color(if copied {
                        theme.accent
                    } else {
                        theme.text_muted
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.copy_sha(sha.clone(), cx)
                    }))
                    .child(SharedString::from(label)),
            )
            .into_any_element()
    }

    fn render_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if index >= self.visible_commits.len() {
            let theme = Theme::of(cx).clone();
            let searching = self.search_active();
            let pending = if searching {
                self.search_loading
            } else {
                self.loading
            };
            let has_error = if searching {
                self.search_error.is_some()
            } else {
                self.error.is_some()
            };
            let label = if pending {
                "Loading…"
            } else if has_error {
                "Retry"
            } else {
                "Load more"
            };
            let button = div()
                .id("history-load-older")
                .h(px(28.0))
                .px(px(11.0))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.border.opacity(0.85))
                .bg(theme.surface_raised.opacity(0.72))
                .text_size(px(11.0))
                .text_color(if pending {
                    theme.text_faint
                } else {
                    theme.text_muted
                })
                .when(!pending, |element| {
                    element
                        .cursor_pointer()
                        .hover(|style| {
                            style
                                .bg(theme.element_hover)
                                .border_color(theme.border_strong.opacity(0.75))
                                .text_color(theme.text)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.load_older(cx)))
                })
                .when(!pending, |element| {
                    element.child(
                        crate::icons::icon(if has_error {
                            crate::icons::REFRESH
                        } else {
                            crate::icons::ALT_ARROW_DOWN
                        })
                        .size(px(11.0))
                        .flex_none()
                        .text_color(theme.text_faint),
                    )
                })
                .child(SharedString::from(label));
            return div()
                .w_full()
                .h(px(48.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(button)
                .into_any_element();
        }
        let Some(commit) = self.visible_commits.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let Some(graph_row) = self.graph.rows.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let sha = commit.sha.clone();
        let open_commit = commit.clone();
        let copied = self.copied_sha.as_deref() == Some(sha.as_str());
        let commit_subject = if commit.subject.is_empty() {
            "(no subject)".to_string()
        } else {
            commit.subject
        };
        let commit_refs = commit.refs;
        let commit_theme = theme.clone();
        let columns = configured_columns(cx);
        let column_widths = configured_column_widths(cx);
        let column_order = configured_column_order(cx);
        let author_display = configured_author_display(cx);
        let author_name = history_author_name(&commit.author_name);
        let author_initial = history_author_initial(&author_name);
        let avatar_image = self
            .avatar_images
            .get(&commit.author_email.trim().to_ascii_lowercase())
            .cloned();
        let graph_focus = self.graph_focus(cx);
        let row_is_focused =
            graph_focus.is_some_and(|focus| focus.color_id == graph_row.node_color_id);
        let row_content_opacity = graph_focus
            .filter(|_| !row_is_focused)
            .map(|focus| 1.0 - (1.0 - HISTORY_ROW_UNFOCUSED_OPACITY) * focus.amount)
            .unwrap_or(1.0);
        let focused_row_wash = graph_focus
            .filter(|_| row_is_focused)
            .map(|focus| crate::theme::ink(0.018 * focus.amount));
        let row_hover_path = graph_row.node_color_id;
        let optional_cells = visible_history_columns(&column_order, columns)
            .into_iter()
            .map(|column| match column {
                GitHistoryColumn::Author => Self::render_author_cell(
                    index,
                    column_widths.author,
                    author_display,
                    author_name.clone(),
                    author_initial.clone(),
                    avatar_image.clone(),
                    row_content_opacity,
                    &theme,
                ),
                GitHistoryColumn::Date => Self::render_date_cell(
                    column_widths.date,
                    &commit.authored_at,
                    row_content_opacity,
                    &theme,
                ),
                GitHistoryColumn::Sha => Self::render_sha_cell(
                    index,
                    column_widths.sha,
                    sha.clone(),
                    copied,
                    row_content_opacity,
                    &theme,
                    cx,
                ),
            })
            .collect::<Vec<_>>();

        let row = div()
            .id(("history-row", index))
            .h(px(HISTORY_ROW_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .text_size(px(11.0))
            .cursor_pointer()
            .when_some(focused_row_wash, |element, wash| element.bg(wash))
            .hover(|style| style.bg(crate::theme::ink(0.025)))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_row_hover((*hovered).then_some(row_hover_path), cx);
            }))
            // A commit row click opens the commit as its own diff tab (the
            // host — the right pane's surface strip — listens; user request).
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(GitHistoryEvent::OpenCommit(open_commit.clone()));
            }))
            .child(self.graph_cell(index, graph_row, graph_focus, &theme, cx))
            .child(
                container_query(move |size, _, _| {
                    let refs_width = ref_area_width(f32::from(size.width));
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .gap(px(HISTORY_REF_GAP))
                        .pr(px(8.0))
                        .opacity(row_content_opacity)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(commit_theme.text)
                                .child(SharedString::from(commit_subject.clone())),
                        )
                        .when(!commit_refs.is_empty(), |element| {
                            element.child(Self::render_ref_area(
                                commit_refs.clone(),
                                index,
                                refs_width,
                                &commit_theme,
                            ))
                        })
                })
                .flex_1()
                .min_w(px(HISTORY_COMMIT_SUBJECT_MIN_WIDTH))
                .overflow_hidden(),
            )
            .child(
                div()
                    .h_full()
                    .flex()
                    .flex_row()
                    .flex_shrink(1.0)
                    .children(optional_cells),
            );

        let transition = self
            .view_transition
            .as_ref()
            .and_then(|transition| transition.rows.get(index).copied());
        match transition {
            Some(HistoryRowTransition::Entering | HistoryRowTransition::Exiting) => {
                let entering = transition == Some(HistoryRowTransition::Entering);
                let epoch = self.view_epoch;
                let sha = commit.sha;
                div()
                    .w_full()
                    .flex_none()
                    .overflow_hidden()
                    .child(row)
                    .with_animation(
                        SharedString::from(format!(
                            "history-row-fold-{epoch}-{sha}-{}",
                            if entering { "in" } else { "out" }
                        )),
                        crate::motion::COLLAPSE.animation(),
                        move |element, progress| {
                            let amount = if entering { progress } else { 1.0 - progress };
                            element
                                .h(px(HISTORY_ROW_HEIGHT * amount))
                                .opacity(0.35 + 0.65 * amount)
                        },
                    )
                    .into_any_element()
            }
            _ => row.into_any_element(),
        }
    }
}

impl Render for GitHistory {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        if !cx.has_active_drag() {
            self.column_drag = None;
        }
        let theme = Theme::of(cx).clone();
        let graph_focus = self.graph_focus(cx);
        let columns = configured_columns(cx);
        let column_widths = configured_column_widths(cx);
        let column_order = configured_column_order(cx);
        let visible_columns = visible_history_columns(&column_order, columns);
        let optional_columns_width = visible_columns
            .iter()
            .copied()
            .map(|column| history_optional_width(column, column_widths))
            .sum::<f32>();
        let mut previous = HistoryDataColumn::Commit;
        let mut header_cells = Vec::with_capacity(visible_columns.len());
        for (index, column) in visible_columns.iter().copied().enumerate() {
            header_cells.push(self.render_column_header_cell(
                column,
                previous,
                index,
                self.column_drag,
                column_widths,
                &theme,
                cx,
            ));
            previous = history_data_column(column);
        }
        let optional_headers = div()
            .id("history-optional-column-headers")
            .h_full()
            .flex()
            .flex_row()
            .flex_shrink(1.0)
            .on_drag_move::<HistoryColumnDrag>(cx.listener(Self::on_column_drag_move))
            .on_drop::<HistoryColumnDrag>(cx.listener(
                |this, payload: &HistoryColumnDrag, _, cx| {
                    this.commit_column_reorder(payload.column, cx);
                },
            ))
            .children(header_cells);
        let column_button = div()
            .id("history-columns-button")
            .absolute()
            .right(px(3.0))
            .top(px(2.0))
            .size(px(20.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .opacity(0.0)
            .group_hover("history-column-header", |style| style.opacity(1.0))
            .when(self.column_menu.is_open(), |button| button.opacity(1.0))
            .hover(|style| style.bg(crate::theme::ink(0.08)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.open_column_menu(event.position, cx);
                }),
            )
            .child(
                crate::icons::icon(crate::icons::CHECKLIST)
                    .size(px(12.0))
                    .text_color(theme.text_muted),
            );
        let column_menu_position = self.column_menu.get().copied();
        let column_menu = column_menu_position.map(|position| {
            let closing = self.column_menu.closing_since();
            let menu = self.render_column_menu(&theme, cx);
            (position, menu, closing)
        });
        let author_menu_position = self.author_menu.get().copied();
        let author_menu = author_menu_position.map(|position| {
            let closing = self.author_menu.closing_since();
            let menu = self.render_author_menu(&theme, cx);
            (position, menu, closing)
        });
        let graph_geometry = self.graph_geometry.clone();
        let graph_target_geometry = self.graph_target_geometry.clone();
        let graph_geometry_morph = self.graph_geometry_morph.clone();
        let graph_hover_suppressed = self.graph_hover_suppressed.clone();
        let graph_lane_capacity = self.graph_lane_capacity;
        let show_header = !self.visible_commits.is_empty();
        let header_theme = theme.clone();

        let body: AnyElement = if self.target_key.is_none() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child("No repository selected")
                .into_any_element()
        } else if self.loading && self.commits.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(crate::loaders::gradient_spinner(
                    "history-loading",
                    &theme,
                    3.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("Loading history…"),
                )
                .into_any_element()
        } else if self.visible_commits.is_empty() {
            let active_error = if self.search_active() {
                self.search_error.clone()
            } else {
                self.error.clone()
            };
            let message = active_error.clone().unwrap_or_else(|| {
                SharedString::from(if self.search_active() {
                    "No matching commits"
                } else if self.view_mode == GitHistoryViewMode::BranchTips {
                    "No branch tips found"
                } else {
                    "No commits found"
                })
            });
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.0))
                .text_size(px(12.0))
                .text_color(if active_error.is_some() {
                    theme.warning
                } else {
                    theme.text_faint
                })
                .child(message)
                .into_any_element()
        } else {
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(self.graph_paths(&theme, graph_focus))
                .child(
                    list(self.list.clone(), cx.processor(Self::render_row))
                        .size_full()
                        .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                )
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .on_drag_move(cx.listener(Self::on_column_resize))
            .when_some(self.fetch_error.clone(), |element, error| {
                element.child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .px(px(8.0))
                        .border_b_1()
                        .border_color(theme.danger.opacity(0.16))
                        .bg(theme.danger.opacity(0.05))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger_muted)
                        .child(SharedString::from(format!("Fetch failed: {error}"))),
                )
            })
            .when_some(
                if self.search_active() {
                    self.search_error.clone()
                } else {
                    self.error.clone()
                }
                .filter(|_| !self.visible_commits.is_empty()),
                |element, error| {
                    element.child(
                        div()
                            .h(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(px(8.0))
                            .border_b_1()
                            .border_color(theme.danger.opacity(0.16))
                            .bg(theme.danger.opacity(0.05))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.danger_muted)
                            .child(error),
                    )
                },
            )
            .child(
                container_query(move |size, window, cx| {
                    let responsive_target = responsive_graph_geometry(
                        graph_lane_capacity,
                        f32::from(size.width),
                        optional_columns_width,
                    );
                    let previous_target = graph_target_geometry.get();
                    let compact = should_use_compact_graph(
                        responsive_target,
                        previous_target,
                        f32::from(size.width),
                        optional_columns_width,
                    );
                    let target = stabilized_graph_geometry(
                        responsive_target,
                        previous_target,
                        window.scale_factor(),
                        compact,
                    );
                    let active_morph = graph_geometry_morph.get().filter(|morph| {
                        if graph_geometry.get() == morph.to {
                            graph_geometry_morph.set(None);
                            false
                        } else {
                            true
                        }
                    });
                    let morph = match active_morph {
                        Some(active) if active.to.compact == target.compact => Some(active),
                        _ if target.compact != previous_target.compact && !cx.reduce_motion() => {
                            let epoch = graph_geometry_morph
                                .get()
                                .map_or(0, |morph| morph.epoch.wrapping_add(1));
                            let morph = GraphGeometryMorph {
                                from: graph_geometry.get(),
                                to: target,
                                epoch,
                            };
                            graph_target_geometry.set(target);
                            graph_geometry_morph.set(Some(morph));
                            graph_hover_suppressed.set(true);
                            Some(morph)
                        }
                        _ => {
                            if graph_geometry.get() != target {
                                graph_hover_suppressed.set(true);
                            }
                            graph_target_geometry.set(target);
                            graph_geometry_morph.set(None);
                            graph_geometry.set(target);
                            None
                        }
                    };
                    let graph_spacer: AnyElement = match morph {
                        Some(morph) => {
                            let graph_geometry = graph_geometry.clone();
                            div()
                                .id("history-graph-morph-spacer")
                                .w(px(morph.from.width))
                                .flex_none()
                                .with_animation(
                                    SharedString::from(format!(
                                        "history-graph-morph-{}",
                                        morph.epoch
                                    )),
                                    crate::motion::COLLAPSE.animation(),
                                    move |element, progress| {
                                        let geometry = interpolate_graph_geometry(
                                            morph.from, morph.to, progress,
                                        );
                                        graph_geometry.set(geometry);
                                        element.w(px(geometry.width))
                                    },
                                )
                                .into_any_element()
                        }
                        None => div()
                            .w(px(graph_geometry.get().width))
                            .flex_none()
                            .into_any_element(),
                    };
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .when(show_header, |element| {
                            element.child(
                                div()
                                    .id("history-column-header")
                                    .group("history-column-header")
                                    .relative()
                                    .h(px(24.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(crate::theme::hairline(0.06))
                                    .text_size(px(9.5))
                                    .text_color(header_theme.text_faint)
                                    .child(graph_spacer)
                                    .child(div().flex_1().min_w(px(80.0)).child("Commit"))
                                    .child(optional_headers)
                                    .child(column_button),
                            )
                        })
                        .child(body)
                })
                .w_full()
                .flex_1()
                .min_h_0(),
            )
            .when_some(column_menu, |element, (position, menu, closing)| {
                element.child(popover::menu_at(
                    "history-columns-menu",
                    position,
                    menu,
                    closing,
                ))
            })
            .when_some(author_menu, |element, (position, menu, closing)| {
                element.child(popover::menu_at(
                    "history-author-menu",
                    position,
                    menu,
                    closing,
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HistorySearchFocusHarness {
        composer: Entity<ComposerInput>,
        search: Entity<GitHistorySearchControl>,
        _focus_lost: Subscription,
    }

    impl HistorySearchFocusHarness {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let state = cx.new(|_| AppState::new());
            let history = cx.new(|cx| GitHistory::new(state, cx));
            let search = cx.new(|cx| GitHistorySearchControl::new(history, cx));
            let composer = cx.new(|cx| ComposerInput::new("Message", cx));
            let focus_lost = cx.on_focus_lost(window, |this, window, cx| {
                crate::shell::restore_focus_if_empty_on_next_frame(
                    this.composer.read(cx).focus_handle(cx),
                    window,
                    cx,
                );
            });
            Self {
                composer,
                search,
                _focus_lost: focus_lost,
            }
        }
    }

    impl Render for HistorySearchFocusHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(div().h(px(40.0)).child(self.composer.clone()))
                .child(self.search.clone())
        }
    }

    fn commit(sha: &str, parents: &[&str]) -> GitHistoryCommit {
        GitHistoryCommit {
            sha: sha.into(),
            parent_shas: parents.iter().map(|value| (*value).into()).collect(),
            subject: sha.into(),
            author_name: "Test".into(),
            author_email: "test@example.com".into(),
            authored_at: "2026-08-12T12:00:00Z".into(),
            refs: Vec::new(),
        }
    }

    fn with_branch(
        mut commit: GitHistoryCommit,
        kind: GitHistoryRefKind,
        label: &str,
    ) -> GitHistoryCommit {
        commit.refs.push(GitHistoryRef {
            kind,
            label: label.into(),
        });
        commit
    }

    #[test]
    fn history_search_matches_fuzzy_subject_terms_and_sha_prefix() {
        let mut candidate = commit("a1b2c3d4", &[]);
        candidate.subject = "Polish the history graph".into();

        assert!(git_history_matches("plsh grph", &candidate));
        assert!(git_history_matches("A1B2", &candidate));
        assert!(!git_history_matches("terminal", &candidate));
    }

    #[test]
    fn history_search_keeps_unicode_engine_result_visible() {
        let mut app = gpui::TestApp::new();

        app.update(|cx| {
            let state = cx.new(|_| AppState::new());
            let history = cx.new(|cx| GitHistory::new(state, cx));
            let mut candidate = commit("a1b2c3d4", &[]);
            candidate.subject = "RÉPARER la recherche".into();

            // Simulate the page returned by the engine for this query. The UI
            // must not discard a result that the shared matcher accepted.
            assert!(git_history_matches("réparer", &candidate));
            history.update(cx, |history, _cx| {
                history.search_query = "réparer".into();
                history.search_results = Some(vec![candidate]);
                history.recompute_view();

                assert_eq!(history.visible_commits.len(), 1);
                assert_eq!(history.visible_commits[0].subject, "RÉPARER la recherche");
            });
        });
    }

    #[test]
    fn history_search_focus_survives_the_composer_fallback() {
        let mut app = gpui::TestApp::new();
        app.update(|cx| {
            Theme::install(crate::theme::Appearance::Dark, cx);
            crate::composer::init(cx, crate::settings::ComposerSendBehavior::Enter);
        });
        let mut window = app.open_window(HistorySearchFocusHarness::new);
        window.draw();

        window.update(|harness, window, cx| {
            window.focus(&harness.composer.read(cx).focus_handle(cx), cx);
        });
        window.draw();
        window.update(|harness, window, cx| {
            assert!(
                harness
                    .composer
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            harness
                .search
                .update(cx, |search, cx| search.expand(window, cx));
        });

        // TestApp has no platform frame loop, so deliver the callback that a
        // headed window runs immediately before painting the expanded input.
        window.update(|_, window, cx| {
            window.simulate_next_frame(cx);
        });
        window.draw();
        window.update(|harness, window, cx| {
            let search = harness.search.read(cx);
            assert!(search.input.read(cx).focus_handle(cx).is_focused(window));
            assert!(
                !harness
                    .composer
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });

        window.simulate_input("fix");
        window.read(|harness, cx| {
            let search = harness.search.read(cx);
            assert_eq!(search.input.read(cx).text(), "fix");
            assert_eq!(search.history.read(cx).search_query, "fix");
            assert!(search.mode == GitHistorySearchMode::Expanded);
        });

        app.advance_clock(HISTORY_SEARCH_IDLE_DISMISS + Duration::from_millis(1));
        app.run_until_parked();
        window.read(|harness, cx| {
            assert!(harness.search.read(cx).mode == GitHistorySearchMode::Expanded);
        });
    }

    #[test]
    fn search_compaction_connects_matches_across_hidden_commits() {
        let commits = vec![
            commit("tip", &["middle"]),
            commit("middle", &["base"]),
            commit("base", &["root"]),
            commit("root", &[]),
        ];
        let visible = HashSet::from(["tip".to_string(), "base".to_string()]);
        let compact = compact_commits_to_visible(&commits, &visible);

        assert_eq!(
            compact
                .iter()
                .map(|commit| commit.sha.as_str())
                .collect::<Vec<_>>(),
            vec!["tip", "base"]
        );
        assert_eq!(compact[0].parent_shas, vec!["base"]);
        assert!(compact[1].parent_shas.is_empty());
    }

    #[test]
    fn search_compaction_handles_a_twenty_thousand_commit_gap() {
        const DEPTH: usize = 20_000;
        let commits = (0..DEPTH)
            .rev()
            .map(|index| {
                let sha = format!("c{index:05}");
                if index == 0 {
                    commit(&sha, &[])
                } else {
                    let parent = format!("c{:05}", index - 1);
                    commit(&sha, &[parent.as_str()])
                }
            })
            .collect::<Vec<_>>();
        let newest = format!("c{:05}", DEPTH - 1);
        let oldest = "c00000".to_string();
        let visible = HashSet::from([newest.clone(), oldest.clone()]);

        let compact = compact_commits_to_visible(&commits, &visible);

        assert_eq!(compact.len(), 2);
        assert_eq!(compact[0].sha, newest);
        assert_eq!(compact[0].parent_shas, vec![oldest.clone()]);
        assert_eq!(compact[1].sha, oldest);
        assert!(compact[1].parent_shas.is_empty());
    }

    #[test]
    fn collapsing_a_branch_contracts_linear_parents_and_keeps_junctions() {
        let commits = vec![
            with_branch(
                commit("feature", &["middle"]),
                GitHistoryRefKind::Branch,
                "feature",
            ),
            commit("middle", &["base"]),
            with_branch(commit("main", &["base"]), GitHistoryRefKind::Branch, "main"),
            commit("base", &["root"]),
            commit("root", &[]),
        ];
        let collapsed = HashSet::from(["local:feature".to_string()]);
        let (visible, counts) = collapse_branch_runs(&commits, &collapsed, Some("main"));

        assert_eq!(
            visible
                .iter()
                .map(|commit| commit.sha.as_str())
                .collect::<Vec<_>>(),
            vec!["feature", "main", "base", "root"]
        );
        assert_eq!(visible[0].parent_shas, vec!["base"]);
        assert_eq!(counts.get("local:feature"), Some(&1));
    }

    #[test]
    fn transition_rows_fold_old_commits_beside_their_stable_anchor() {
        let old = vec![
            commit("tip", &["one"]),
            commit("one", &["two"]),
            commit("two", &["base"]),
            commit("base", &[]),
        ];
        let target = vec![commit("tip", &["base"]), commit("base", &[])];
        let (rows, transitions) = history_transition_rows(&old, &target);

        assert_eq!(
            rows.iter()
                .map(|commit| commit.sha.as_str())
                .collect::<Vec<_>>(),
            vec!["tip", "one", "two", "base"]
        );
        assert_eq!(
            transitions,
            vec![
                HistoryRowTransition::Stable,
                HistoryRowTransition::Exiting,
                HistoryRowTransition::Exiting,
                HistoryRowTransition::Stable,
            ]
        );
    }

    #[test]
    fn transition_rows_expand_new_commits_in_their_final_order() {
        let old = vec![commit("tip", &["base"]), commit("base", &[])];
        let target = vec![
            commit("tip", &["one"]),
            commit("one", &["two"]),
            commit("two", &["base"]),
            commit("base", &[]),
        ];
        let (rows, transitions) = history_transition_rows(&old, &target);

        assert_eq!(
            rows.iter()
                .map(|commit| commit.sha.as_str())
                .collect::<Vec<_>>(),
            vec!["tip", "one", "two", "base"]
        );
        assert_eq!(
            transitions,
            vec![
                HistoryRowTransition::Stable,
                HistoryRowTransition::Entering,
                HistoryRowTransition::Entering,
                HistoryRowTransition::Stable,
            ]
        );
    }

    #[test]
    fn list_splice_preserves_the_unchanged_prefix_around_a_fold() {
        let old = vec![
            commit("tip", &["one"]),
            commit("one", &["two"]),
            commit("two", &["base"]),
            commit("base", &[]),
        ];
        let target = vec![commit("tip", &["base"]), commit("base", &[])];

        assert_eq!(
            history_list_splice(&old, true, &target, true),
            Some((1..3, 0))
        );
    }

    #[test]
    fn removed_scroll_anchor_moves_to_the_next_surviving_commit() {
        let old = vec![
            commit("tip", &["one"]),
            commit("one", &["two"]),
            commit("two", &["base"]),
            commit("base", &[]),
        ];
        let target = vec![commit("tip", &["base"]), commit("base", &[])];
        let anchor = HistoryScrollAnchor {
            sha: "one".into(),
            offset_in_item: px(12.0),
        };

        let resolved = resolve_history_scroll_anchor(Some(anchor), &old, &target).unwrap();
        assert_eq!(resolved.sha, "base");
        assert_eq!(resolved.offset_in_item, px(0.0));
    }

    #[test]
    fn branch_fold_keys_keep_local_and_remote_identity() {
        let local = GitHistoryRef {
            kind: GitHistoryRefKind::Branch,
            label: "main".into(),
        };
        let remote = GitHistoryRef {
            kind: GitHistoryRefKind::Remote,
            label: "origin/main".into(),
        };
        assert_eq!(branch_ref_key(&local).as_deref(), Some("local:main"));
        assert_eq!(
            branch_ref_key(&remote).as_deref(),
            Some("remote:origin/main")
        );
    }

    #[test]
    fn author_avatar_fallback_uses_the_first_visible_initial() {
        assert_eq!(history_author_name(""), SharedString::from("Unknown"));
        assert_eq!(history_author_initial("  josé"), SharedString::from("J"));
        assert_eq!(history_author_initial("   "), SharedString::from("?"));
    }

    #[test]
    fn github_avatar_payload_decodes_into_a_gpui_image() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"\xff\xd8\xffpayload");
        let image = decode_history_avatar(&encoded).expect("jpeg payload");
        assert_eq!(image.format, ImageFormat::Jpeg);
        assert!(decode_history_avatar("not base64").is_none());
    }

    #[test]
    fn graph_splits_and_rejoins_merge_lanes() {
        let commits = vec![
            commit("merge", &["main", "feature"]),
            commit("main", &["base"]),
            commit("feature", &["base"]),
            commit("base", &[]),
        ];
        let graph = layout_graph(&commits, Some("merge"));
        assert_eq!(graph.rows.len(), commits.len());
        assert!(graph.max_lane_count >= 2);
        assert!(graph.rows[0].is_head);
        assert_eq!(graph.rows[0].segments.len(), 2);
        assert_eq!(graph.rows[3].segments.len(), 2);
        assert_eq!(graph.rows[3].node_lane, 0);
    }

    #[test]
    fn appending_older_commits_preserves_the_loaded_prefix_layout() {
        let prefix = vec![commit("tip", &["parent"]), commit("parent", &["root"])];
        let before = layout_graph(&prefix, Some("tip"));
        let mut all = prefix.clone();
        all.push(commit("root", &[]));
        let after = layout_graph(&all, Some("tip"));
        assert_eq!(before.rows, after.rows[..before.rows.len()]);
    }

    #[test]
    fn graph_palette_only_reduces_saturation() {
        let source = gpui::hsla(0.62, 0.8, 0.55, 0.9);
        let muted = graph_color(source);
        assert_eq!(muted.h, source.h);
        assert_eq!(muted.l, source.l);
        assert_eq!(muted.a, source.a);
        assert!((muted.s - source.s * HISTORY_GRAPH_SATURATION).abs() < f32::EPSILON);
    }

    #[test]
    fn graph_hover_detects_vertical_and_curved_paths() {
        let geometry = GraphGeometry::natural(2);
        let vertical = GraphRow {
            sha: "vertical".into(),
            node_lane: 0,
            node_color_id: 1,
            segments: vec![GraphSegment {
                from_lane: 1,
                to_lane: 1,
                color_id: 7,
                shape: SegmentShape::Through,
            }],
            is_head: false,
        };
        assert_eq!(
            hovered_graph_path(&vertical, geometry.lane_x(1), 5.0, geometry),
            Some(7)
        );

        let curved_segment = GraphSegment {
            from_lane: 0,
            to_lane: 1,
            color_id: 9,
            shape: SegmentShape::Outgoing,
        };
        let middle = HISTORY_ROW_HEIGHT / 2.0;
        let curve_x = cubic_coordinate(
            geometry.lane_x(0),
            geometry.lane_x(0),
            geometry.lane_x(1),
            geometry.lane_x(1),
            0.5,
        );
        let curve_y = cubic_coordinate(
            middle,
            middle * 1.45,
            middle * 1.45,
            HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP,
            0.5,
        );
        let curved = GraphRow {
            sha: "curved".into(),
            node_lane: 0,
            node_color_id: 1,
            segments: vec![curved_segment],
            is_head: false,
        };
        assert_eq!(
            hovered_graph_path(&curved, curve_x, curve_y, geometry),
            Some(9)
        );
    }

    #[test]
    fn graph_hover_prefers_the_node_and_ignores_empty_space() {
        let geometry = GraphGeometry::natural(4);
        let row = GraphRow {
            sha: "node".into(),
            node_lane: 1,
            node_color_id: 11,
            segments: vec![GraphSegment {
                from_lane: 0,
                to_lane: 1,
                color_id: 4,
                shape: SegmentShape::Incoming,
            }],
            is_head: false,
        };
        assert_eq!(
            hovered_graph_path(&row, geometry.lane_x(1), HISTORY_ROW_HEIGHT / 2.0, geometry,),
            Some(11)
        );
        assert_eq!(
            hovered_graph_path(&row, geometry.width - 1.0, 2.0, geometry),
            None
        );
    }

    #[test]
    fn responsive_graph_keeps_natural_spacing_when_it_fits() {
        let geometry = responsive_graph_geometry(8, 900.0, 240.0);
        assert_eq!(geometry, GraphGeometry::natural(8));
    }

    #[test]
    fn responsive_graph_compresses_lanes_to_preserve_commit_space() {
        let geometry = responsive_graph_geometry(20, 400.0, 240.0);
        assert_eq!(geometry.width, 80.0);
        assert!(geometry.lane_spacing < HISTORY_LANE_SPACING);
        assert_eq!(geometry.lane_x(0), GraphGeometry::natural(20).lane_x(0));
        assert!(geometry.lane_x(19) < GraphGeometry::natural(20).lane_x(19));
    }

    #[test]
    fn narrow_commit_space_switches_to_a_compact_rail_with_hysteresis() {
        let target = responsive_graph_geometry(10, 500.0, 240.0);
        assert!(should_use_compact_graph(
            target,
            GraphGeometry::natural(10),
            500.0,
            240.0,
        ));

        let compact = GraphGeometry::compact(10);
        let still_narrow = responsive_graph_geometry(10, 550.0, 240.0);
        assert!(should_use_compact_graph(
            still_narrow,
            compact,
            550.0,
            240.0,
        ));

        let wide_again = responsive_graph_geometry(10, 570.0, 240.0);
        assert!(!should_use_compact_graph(wide_again, compact, 570.0, 240.0,));
    }

    #[test]
    fn compact_graph_keeps_each_rows_color_as_its_hover_identity() {
        let geometry = GraphGeometry::compact(20);
        let row = GraphRow {
            sha: "compact".into(),
            node_lane: 14,
            node_color_id: 9,
            segments: Vec::new(),
            is_head: false,
        };
        assert_eq!(geometry.lane_x(0), geometry.lane_x(14));
        assert_eq!(
            hovered_graph_path(&row, geometry.lane_x(0), 2.0, geometry),
            Some(9)
        );
    }

    #[test]
    fn graph_geometry_morph_converges_lanes_before_entering_the_compact_rail() {
        let full = GraphGeometry::natural(8);
        let compact = GraphGeometry::compact(8);
        let halfway = interpolate_graph_geometry(full, compact, 0.5);
        assert!(!halfway.compact);
        assert!(halfway.width < full.width);
        assert!(halfway.width > compact.width);
        assert!(halfway.lane_spacing < full.lane_spacing);
        assert!(halfway.lane_spacing > compact.lane_spacing);

        let settled = interpolate_graph_geometry(full, compact, 1.0);
        assert!(settled.compact);
        assert_eq!(settled.width, compact.width);
        assert_eq!(settled.lane_spacing, 0.0);
    }

    #[test]
    fn graph_geometry_morph_expands_the_rail_from_its_compact_start() {
        let compact = GraphGeometry::compact(8);
        let full = GraphGeometry::natural(8);
        assert!(interpolate_graph_geometry(compact, full, 0.0).compact);
        let halfway = interpolate_graph_geometry(compact, full, 0.5);
        assert!(!halfway.compact);
        assert!(halfway.lane_spacing > 0.0);
        assert!(halfway.lane_spacing < full.lane_spacing);
    }

    #[test]
    fn compressed_graph_hit_testing_uses_the_fitted_lane_positions() {
        let geometry = GraphGeometry::fitted(20, 80.0);
        let row = GraphRow {
            sha: "compact".into(),
            node_lane: 19,
            node_color_id: 7,
            segments: Vec::new(),
            is_head: false,
        };
        assert_eq!(
            hovered_graph_path(
                &row,
                geometry.lane_x(19),
                HISTORY_ROW_HEIGHT / 2.0,
                geometry,
            ),
            Some(7)
        );
    }

    #[test]
    fn responsive_graph_geometry_ignores_sub_step_resize_jitter() {
        let previous = GraphGeometry::fitted(20, 80.0);
        let target = GraphGeometry::fitted(20, 81.9);
        let stable = stabilized_graph_geometry(target, previous, 2.0, false);
        assert_eq!(stable.width, previous.width);
        assert_eq!(stable.lane_spacing, previous.lane_spacing);
        assert_eq!(stable.device_scale, 2.0);

        let next = stabilized_graph_geometry(GraphGeometry::fitted(20, 82.1), stable, 2.0, false);
        assert_eq!(next.width, 82.0);
        assert_ne!(next.lane_spacing, stable.lane_spacing);
    }

    #[test]
    fn responsive_graph_lanes_snap_to_device_pixels() {
        let geometry = stabilized_graph_geometry(
            GraphGeometry::fitted(20, 82.0),
            GraphGeometry::natural(1),
            2.0,
            false,
        );
        for lane in 0..20 {
            assert!((geometry.lane_x(lane) * 2.0).fract().abs() < f32::EPSILON);
        }
    }

    #[test]
    fn ref_badges_expand_with_the_available_width() {
        let reference = |label: &str| GitHistoryRef {
            kind: GitHistoryRefKind::Branch,
            label: label.into(),
        };
        let refs = vec![reference("main"), reference("tag"), reference("origin")];
        assert_eq!(visible_ref_count(&[], 100.0), 0);
        assert_eq!(visible_ref_count(&refs, 70.0), 1);
        assert_eq!(visible_ref_count(&refs, 115.0), 2);
        assert_eq!(visible_ref_count(&refs, 200.0), 3);
    }

    #[test]
    fn ref_badges_preserve_overflow_when_the_first_badge_is_too_wide() {
        let reference = |label: &str| GitHistoryRef {
            kind: GitHistoryRefKind::Branch,
            label: label.into(),
        };
        let refs = vec![
            reference("feature/accessibility-polish"),
            reference("main"),
            reference("origin/main"),
            reference("upstream/main"),
            reference("v0.1.53"),
            reference("HEAD"),
        ];

        assert_eq!(visible_ref_count(&refs, 120.0), 0);
    }

    #[test]
    fn ref_area_preserves_subject_space_and_caps_at_forty_five_percent() {
        assert_eq!(ref_area_width(80.0), 0.0);
        assert!((ref_area_width(200.0) - 86.4).abs() < 0.001);
        assert!((ref_area_width(400.0) - 176.4).abs() < 0.001);
    }

    #[test]
    fn commit_divider_resizes_the_first_visible_fixed_column() {
        let widths = GitHistoryColumnWidths::default();
        let resized = resized_history_column_widths(
            widths,
            HistoryColumnDragAnchor {
                start_x: 0.0,
                left: HistoryDataColumn::Commit,
                right: HistoryDataColumn::Author,
                left_width: HISTORY_COMMIT_SUBJECT_MIN_WIDTH,
                right_width: widths.author,
            },
            20.0,
        );
        assert_eq!(resized.author, 68.0);
        assert_eq!(resized.date, widths.date);
        assert_eq!(resized.sha, widths.sha);
    }

    #[test]
    fn interior_column_divider_preserves_width_and_clamps_both_sides() {
        let widths = GitHistoryColumnWidths::default();
        let anchor = HistoryColumnDragAnchor {
            start_x: 0.0,
            left: HistoryDataColumn::Author,
            right: HistoryDataColumn::Date,
            left_width: widths.author,
            right_width: widths.date,
        };
        let resized = resized_history_column_widths(widths, anchor, 10.0);
        assert_eq!(resized.author, 98.0);
        assert_eq!(resized.date, 78.0);
        assert_eq!(resized.author + resized.date, widths.author + widths.date);

        let clamped = resized_history_column_widths(widths, anchor, 1_000.0);
        assert_eq!(clamped.date, GitHistoryColumnWidths::DATE_MIN);
        assert_eq!(clamped.author, 108.0);
    }

    #[test]
    fn visible_columns_follow_persisted_order_and_skip_hidden_entries() {
        let order = GitHistoryColumnOrder(vec![
            GitHistoryColumn::Sha,
            GitHistoryColumn::Author,
            GitHistoryColumn::Date,
        ]);
        let columns = GitHistoryColumns {
            author: false,
            date: true,
            sha: true,
        };
        assert_eq!(
            visible_history_columns(&order, columns),
            vec![GitHistoryColumn::Sha, GitHistoryColumn::Date]
        );
    }

    #[test]
    fn reordering_visible_columns_preserves_hidden_columns() {
        let order = GitHistoryColumnOrder::default();
        let reordered =
            reordered_history_columns(&order, GitHistoryColumn::Sha, GitHistoryColumn::Date);
        assert_eq!(
            reordered,
            GitHistoryColumnOrder(vec![
                GitHistoryColumn::Author,
                GitHistoryColumn::Sha,
                GitHistoryColumn::Date,
            ])
        );
    }

    #[test]
    fn reorder_drop_index_respects_uneven_column_widths() {
        let columns = [
            GitHistoryColumn::Author,
            GitHistoryColumn::Date,
            GitHistoryColumn::Sha,
        ];
        let widths = GitHistoryColumnWidths::default();
        let rendered = widths.author + widths.date + widths.sha;
        assert_eq!(
            history_column_drop_index(10.0, rendered, &columns, widths),
            0
        );
        assert_eq!(
            history_column_drop_index(100.0, rendered, &columns, widths),
            1
        );
        assert_eq!(
            history_column_drop_index(240.0, rendered, &columns, widths),
            2
        );
    }

    #[test]
    fn ref_tooltip_describes_each_reference_kind() {
        let reference = |kind: GitHistoryRefKind, label: &str| GitHistoryRef {
            kind,
            label: label.into(),
        };
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Branch, "main")),
            "Branch: main"
        );
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Remote, "origin/main")),
            "Remote branch: origin/main"
        );
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Tag, "v0.1.52")),
            "Tag: v0.1.52"
        );
    }
}
