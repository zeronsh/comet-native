use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use gpui::{
    AnimationExt, AnyElement, Context, Entity, IntoElement, Render, ScrollHandle, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::{
    ChangeRequestListItem, ChangeRequestMergeability, ChangeRequestReviewDecision, Device,
};
use zeron_rpc::{RpcError, capability_errors, methods};

use crate::icons::{self, icon};
use crate::motion;
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const SNAPSHOT_TTL: Duration = Duration::from_secs(60);
const PR_PAGE_MAX_WIDTH: f32 = 1120.0;
const PR_PAGE_HORIZONTAL_PADDING: f32 = 24.0;
const PR_TABLE_HEADER_HEIGHT: f32 = 24.0;
const PR_TABLE_ROW_HEIGHT: f32 = 52.0;
const PR_TABLE_CHANGES_WIDTH: f32 = 92.0;
const PR_TABLE_OPENED_WIDTH: f32 = 92.0;
const PR_TABLE_UPDATED_WIDTH: f32 = 92.0;
const PR_TABLE_ACTION_WIDTH: f32 = 22.0;
const PR_ROW_HOVER_TEXT_SCALE: f32 = 0.06;
const PR_SORT_OFFSET_PER_ROW: f32 = 8.0;
const PR_SORT_MAX_OFFSET: f32 = 24.0;
const PR_SCROLL_FADE_BAND: f32 = 24.0;
const PR_SORT_ANIMATION: motion::MotionSpec = motion::MotionSpec::new(180, motion::EASE_RESORT);

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullRequestsPageError {
    CliUnavailable,
    Authentication,
    RateLimited,
    Network,
    RemoteOffline(String),
    UpdateRequired(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullRequestsLoadState {
    Idle,
    Loading,
    Ready,
    Failed(PullRequestsPageError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestSortField {
    Changes,
    Opened,
    Updated,
}

impl PullRequestSortField {
    fn key(self) -> &'static str {
        match self {
            Self::Changes => "changes",
            Self::Opened => "opened",
            Self::Updated => "updated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PullRequestSort {
    field: PullRequestSortField,
    direction: SortDirection,
}

struct MergeConflictTooltip;

impl Render for MergeConflictTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(px(11.0))
            .text_color(theme.danger_muted)
            .child("Merge conflicts")
    }
}

impl PullRequestSort {
    const DEFAULT: Self = Self {
        field: PullRequestSortField::Updated,
        direction: SortDirection::Descending,
    };

    fn select(self, field: PullRequestSortField) -> Self {
        if self.field != field {
            return Self {
                field,
                direction: SortDirection::Descending,
            };
        }
        Self {
            field,
            direction: match self.direction {
                SortDirection::Ascending => SortDirection::Descending,
                SortDirection::Descending => SortDirection::Ascending,
            },
        }
    }
}

/// Ephemeral dashboard for open pull requests authored by the active GitHub CLI account.
pub struct PullRequestsPage {
    state: Entity<AppState>,
    /// `None` keeps local calls direct; a value is forwarded by the relay.
    target_device: Option<String>,
    items: Vec<ChangeRequestListItem>,
    sort: PullRequestSort,
    sort_epoch: u64,
    sort_offsets: HashMap<String, f32>,
    load_state: PullRequestsLoadState,
    last_loaded_at: Option<Instant>,
    generation: u64,
    request_task: Option<Task<()>>,
    visible: bool,
    scroll: ScrollHandle,
    content_width: Option<f32>,
    device_menu: popover::Popup<()>,
    _observe: Subscription,
}

impl PullRequestsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |page, _, cx| {
            let target_changed = page.reconcile_target_device(cx);
            if target_changed && page.visible {
                page.load(cx);
            } else {
                cx.notify();
            }
        });
        let mut page = Self {
            state,
            target_device: None,
            items: Vec::new(),
            sort: PullRequestSort::DEFAULT,
            sort_epoch: 0,
            sort_offsets: HashMap::new(),
            load_state: PullRequestsLoadState::Idle,
            last_loaded_at: None,
            generation: 0,
            request_task: None,
            // The entity is created lazily only while this route is active.
            visible: true,
            scroll: ScrollHandle::new(),
            content_width: None,
            device_menu: popover::Popup::default(),
            _observe: observe,
        };
        page.load(cx);
        page
    }

    /// Called whenever shell navigation makes the already-owned entity visible again.
    pub fn on_visible(&mut self, cx: &mut Context<Self>) {
        self.visible = true;
        // Resort offsets describe one click transition, not durable page state.
        // Dropping them here prevents a completed animation from replaying after navigation.
        self.sort_offsets.clear();
        let target_changed = self.reconcile_target_device(cx);
        let stale = self
            .last_loaded_at
            .is_none_or(|loaded| loaded.elapsed() >= SNAPSHOT_TTL);
        if target_changed || (!matches!(self.load_state, PullRequestsLoadState::Loading) && stale) {
            self.load(cx);
        }
    }

    /// Keep the retained route entity dormant while another outlet is active.
    pub fn on_hidden(&mut self) {
        self.visible = false;
    }

    fn close_device_menu(&mut self, cx: &mut Context<Self>) {
        if self.device_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.device_menu);
            cx.notify();
        }
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.close_device_menu(cx);
        if self.target_device == target {
            return;
        }

        self.reset_for_target(target);
        self.load(cx);
    }

    fn reset_for_target(&mut self, target: Option<String>) {
        self.generation = self.generation.wrapping_add(1);
        self.request_task = None;
        self.target_device = target;
        self.items.clear();
        self.sort_offsets.clear();
        self.load_state = PullRequestsLoadState::Idle;
        self.last_loaded_at = None;
        self.scroll.set_offset(gpui::Point::default());
    }

    /// Heal a selected remote that is no longer present in an authoritative
    /// device frame. The local row proves the frame has landed, so the empty
    /// pre-sync list cannot accidentally discard a valid remote selection.
    fn reconcile_target_device(&mut self, cx: &mut Context<Self>) -> bool {
        let normalized = {
            let state = self.state.read(cx);
            normalized_target_device(
                self.target_device.as_deref(),
                &state.devices,
                state.local_device_id.as_deref(),
            )
        };
        if normalized == self.target_device {
            return false;
        }
        self.close_device_menu(cx);
        self.reset_for_target(normalized);
        true
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.load_state, PullRequestsLoadState::Loading) {
            self.load(cx);
        }
    }

    fn select_sort(&mut self, field: PullRequestSortField, cx: &mut Context<Self>) {
        let mut previous = self.items.clone();
        sort_pull_requests(&mut previous, self.sort);
        let previous: Vec<_> = previous.iter().map(pull_request_key).collect();

        let next_sort = self.sort.select(field);
        let mut next = self.items.clone();
        sort_pull_requests(&mut next, next_sort);
        let next: Vec<_> = next.iter().map(pull_request_key).collect();

        self.sort = next_sort;
        self.sort_offsets.clear();
        if !motion::reduced_motion(cx) {
            self.sort_offsets = sort_row_offsets(&previous, &next);
            if !self.sort_offsets.is_empty() {
                self.sort_epoch = self.sort_epoch.wrapping_add(1);
            }
        }
        self.scroll.set_offset(gpui::Point::default());
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.reconcile_target_device(cx);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.load_state = PullRequestsLoadState::Failed(PullRequestsPageError::Network);
            cx.notify();
            return;
        };

        let target_name = selected_device_name(&self.state.read(cx), self.target_device.as_deref());
        if let Some(target) = self.target_device.as_deref()
            && !self.state.read(cx).device_online(target, Utc::now())
        {
            self.load_state =
                PullRequestsLoadState::Failed(PullRequestsPageError::RemoteOffline(target_name));
            cx.notify();
            return;
        }

        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let params = params_for_target(self.target_device.as_deref());
        self.load_state = PullRequestsLoadState::Loading;
        self.request_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_OPEN_CHANGE_REQUESTS, params)
                .await;
            this.update(cx, |page, cx| {
                if !response_is_current(page.generation, generation) {
                    return;
                }

                page.request_task = None;
                let loaded = match result {
                    Ok(value) => serde_json::from_value::<Vec<ChangeRequestListItem>>(value)
                        .map_err(|_| PullRequestsPageError::Network),
                    Err(error) => Err(map_rpc_error(&error, &target_name)),
                };
                let succeeded = loaded.is_ok();
                page.load_state = settle_snapshot(&mut page.items, loaded);
                if succeeded {
                    page.last_loaded_at = Some(Instant::now());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (devices, local_id) = {
            let state = self.state.read(cx);
            (
                eligible_desktop_devices(&state.devices),
                state.local_device_id.clone(),
            )
        };
        if devices.len() <= 1 {
            return div().into_any_element();
        }

        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|device| Some(device.id.as_str()) == effective.as_deref());
        let label: SharedString = selected
            .map(|device| device.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let glyph = selected
            .map(|device| platform_icon(&device.platform))
            .unwrap_or(icons::LAPTOP);
        let open = self.device_menu.is_open();

        let mut trigger = div()
            .id("pull-requests-device-switcher")
            .flex_none()
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .bg(if open {
                crate::theme::ink(0.06)
            } else {
                gpui::transparent_black()
            })
            .when(!open, |element| {
                element.hover(|style| style.bg(crate::theme::ink(0.04)))
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|page, _, _, _| page.device_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|page, _, _, cx| {
                if page.device_menu.take_press_was_open() {
                    page.close_device_menu(cx);
                } else {
                    page.device_menu.open(());
                }
                cx.notify();
            }))
            .child(icon(glyph).size(px(16.0)).text_color(theme.text_muted))
            .child(
                div()
                    .max_w(px(130.0))
                    .truncate()
                    .text_size(px(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(label),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            );

        if self.device_menu.get().is_some() {
            let closing = self.device_menu.closing_since();
            let menu = popover::popover_card(theme)
                .w(px(240.0))
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_device_menu(cx)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Desktop devices"))
                .children(devices.into_iter().enumerate().map(|(index, device)| {
                    let active = effective.as_deref() == Some(device.id.as_str());
                    let local = local_id.as_deref() == Some(device.id.as_str());
                    let device_id = device.id.clone();
                    let name: SharedString = device.name.clone().into();
                    let online = self.state.read(cx).device_online(&device.id, Utc::now());
                    popover::menu_row(theme, active, format!("pull-requests-device-row-{index}"))
                        .id(("pull-requests-device-row", index))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.set_target_device((!local).then(|| device_id.clone()), cx);
                        }))
                        .child(
                            icon(platform_icon(&device.platform))
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(local, |element| {
                            element.child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_muted.opacity(0.45))
                                    .child("You"),
                            )
                        })
                        .child(div().size(px(6.0)).rounded_full().bg(if online {
                            theme.success
                        } else {
                            crate::theme::ink(0.2)
                        }))
                }));
            trigger = trigger.child(popover::anchored_menu(
                "pull-requests-device-menu",
                menu.into_any_element(),
                closing,
            ));
        }

        trigger.into_any_element()
    }

    fn render_empty_or_error(&self, theme: &Theme) -> AnyElement {
        let (title, body) = match &self.load_state {
            PullRequestsLoadState::Failed(error) => error_copy(error),
            _ => (
                "No open pull requests".to_string(),
                "Pull requests authored by you will appear here.".to_string(),
            ),
        };
        div()
            .mt(px(72.0))
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .max_w(px(420.0))
                    .text_size(px(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(body)),
            )
            .into_any_element()
    }
}

impl Render for PullRequestsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let initial_loading =
            self.items.is_empty() && matches!(self.load_state, PullRequestsLoadState::Loading);
        let refreshing =
            !self.items.is_empty() && matches!(self.load_state, PullRequestsLoadState::Loading);
        let refresh_error =
            !self.items.is_empty() && matches!(self.load_state, PullRequestsLoadState::Failed(_));
        let count = (!initial_loading
            && !matches!(self.load_state, PullRequestsLoadState::Failed(_))
            || !self.items.is_empty())
        .then_some(self.items.len());
        let mut items = self.items.clone();
        sort_pull_requests(&mut items, self.sort);
        let layout = self
            .content_width
            .map(table_layout)
            .unwrap_or(PullRequestTableLayout::Narrow);
        let scroll = self.scroll.clone();
        let width_probe = {
            let page = cx.weak_entity();
            gpui::canvas(
                move |bounds, _, cx| {
                    let width = table_content_width(f32::from(bounds.size.width));
                    page.update(cx, |page, cx| {
                        if page
                            .content_width
                            .is_none_or(|current| (current - width).abs() > 0.5)
                        {
                            page.content_width = Some(width);
                            cx.notify();
                        }
                    })
                    .ok();
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
        };

        div().id("pull-requests-page").size_full().child(
            crate::edge_fade::edge_faded(
                PR_SCROLL_FADE_BAND,
                true,
                false,
                div()
                    .id("pull-requests-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .child(
                        div()
                            .w_full()
                            .max_w(px(PR_PAGE_MAX_WIDTH))
                            .mx_auto()
                            .relative()
                            .px(px(PR_PAGE_HORIZONTAL_PADDING))
                            .pt(px(Theme::TITLEBAR_HEIGHT + Theme::SPACE_SM))
                            .pb(px(64.0))
                            .flex()
                            .flex_col()
                            .child(width_probe)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(widgets::page_header(&theme, "Pull requests", count))
                                    .child(div().flex_1())
                                    .child(self.render_device_switcher(&theme, cx))
                                    .child(
                                        widgets::ghost_action(&theme)
                                            .id("pull-requests-refresh")
                                            .flex_none()
                                            .hover(|style| widgets::ghost_hover(&theme, style))
                                            .when(
                                                matches!(
                                                    self.load_state,
                                                    PullRequestsLoadState::Loading
                                                ),
                                                |element| element.opacity(0.5),
                                            )
                                            .on_click(
                                                cx.listener(|page, _, _, cx| page.refresh(cx)),
                                            )
                                            .child(if refreshing || initial_loading {
                                                crate::loaders::mini_glyph_spinner(
                                                    "pull-requests-refresh-spinner",
                                                    1.75,
                                                    theme.glyph,
                                                    cx.entity_id(),
                                                    cx,
                                                )
                                                .into_any_element()
                                            } else {
                                                icon(icons::REFRESH)
                                                    .size(px(16.0))
                                                    .text_color(theme.text_muted)
                                                    .into_any_element()
                                            })
                                            .child("Refresh"),
                                    ),
                            )
                            .child(widgets::page_subtitle(
                                &theme,
                                "Open pull requests authored by you on GitHub.",
                            ))
                            .when(refresh_error, |element| {
                                element.child(widgets::error_strip(
                                    &theme,
                                    "Refresh failed. Try again.",
                                ))
                            })
                            .child(if initial_loading {
                                div()
                                    .mt(px(72.0))
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap(px(12.0))
                                    .text_size(px(13.0))
                                    .text_color(theme.text_muted)
                                    .child(crate::loaders::gradient_spinner(
                                        "pull-requests-loading",
                                        &theme,
                                        3.0,
                                        cx.entity_id(),
                                        cx,
                                    ))
                                    .child("Loading pull requests…")
                                    .into_any_element()
                            } else if items.is_empty() {
                                self.render_empty_or_error(&theme)
                            } else {
                                div()
                                    .mt(px(24.0))
                                    .child(render_pull_request_table(
                                        &items,
                                        layout,
                                        self.sort,
                                        self.sort_epoch,
                                        &self.sort_offsets,
                                        &theme,
                                        cx,
                                    ))
                                    .into_any_element()
                            }),
                    ),
            )
            .inset_top(Theme::TITLEBAR_HEIGHT)
            .band_top(PR_SCROLL_FADE_BAND)
            .fade_overflow_y(&scroll),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestTableLayout {
    Narrow,
    Compact,
    Wide,
}

fn table_layout(width: f32) -> PullRequestTableLayout {
    if width < 640.0 {
        PullRequestTableLayout::Narrow
    } else if width < 900.0 {
        PullRequestTableLayout::Compact
    } else {
        PullRequestTableLayout::Wide
    }
}

fn table_content_width(container_width: f32) -> f32 {
    (container_width - PR_PAGE_HORIZONTAL_PADDING * 2.0)
        .max(0.0)
        .min(PR_PAGE_MAX_WIDTH - PR_PAGE_HORIZONTAL_PADDING * 2.0)
}

fn sort_pull_requests(items: &mut [ChangeRequestListItem], sort: PullRequestSort) {
    items.sort_by(|left, right| {
        let primary = match sort.field {
            PullRequestSortField::Changes => left
                .additions
                .saturating_add(left.deletions)
                .cmp(&right.additions.saturating_add(right.deletions)),
            PullRequestSortField::Opened => left.created_at.cmp(&right.created_at),
            PullRequestSortField::Updated => left.updated_at.cmp(&right.updated_at),
        };
        let primary = match sort.direction {
            SortDirection::Ascending => primary,
            SortDirection::Descending => primary.reverse(),
        };
        primary
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.repository.cmp(&right.repository))
            .then_with(|| left.number.cmp(&right.number))
    });
}

fn pull_request_key(item: &ChangeRequestListItem) -> String {
    format!("{}#{}", item.repository, item.number)
}

fn sort_row_offsets(previous: &[String], next: &[String]) -> HashMap<String, f32> {
    let previous_positions: HashMap<&str, usize> = previous
        .iter()
        .enumerate()
        .map(|(index, key)| (key.as_str(), index))
        .collect();
    next.iter()
        .enumerate()
        .filter_map(|(next_index, key)| {
            let previous_index = *previous_positions.get(key.as_str())?;
            let delta = previous_index as f32 - next_index as f32;
            (delta.abs() > f32::EPSILON).then(|| {
                (
                    key.clone(),
                    (delta * PR_SORT_OFFSET_PER_ROW).clamp(-PR_SORT_MAX_OFFSET, PR_SORT_MAX_OFFSET),
                )
            })
        })
        .collect()
}

fn render_pull_request_table(
    items: &[ChangeRequestListItem],
    layout: PullRequestTableLayout,
    sort: PullRequestSort,
    sort_epoch: u64,
    sort_offsets: &HashMap<String, f32>,
    theme: &Theme,
    cx: &mut Context<PullRequestsPage>,
) -> AnyElement {
    let show_header = layout != PullRequestTableLayout::Narrow;
    div()
        .w_full()
        .when(show_header, |element| {
            element.child(render_table_header(layout, sort, theme, cx))
        })
        .children(items.iter().map(|item| {
            let row = render_table_row(item, layout, theme);
            let key = pull_request_key(item);
            let Some(offset) = sort_offsets.get(&key).copied() else {
                return row;
            };
            let animation_id = SharedString::from(format!("pull-request-sort-{sort_epoch}-{key}"));
            div()
                .w_full()
                .flex_none()
                .child(row)
                .with_animation(animation_id, PR_SORT_ANIMATION.animation(), move |el, t| {
                    el.relative()
                        .top(px(offset * (1.0 - t)))
                        .opacity(0.82 + 0.18 * t)
                })
                .into_any_element()
        }))
        .into_any_element()
}

fn render_table_header(
    layout: PullRequestTableLayout,
    sort: PullRequestSort,
    theme: &Theme,
    cx: &mut Context<PullRequestsPage>,
) -> AnyElement {
    let label = |copy: &'static str| {
        div()
            .text_size(px(12.0))
            .text_color(theme.text_faint)
            .child(copy)
    };
    div()
        .h(px(PR_TABLE_HEADER_HEIGHT))
        .px(px(8.0))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.0))
        .border_b_1()
        .border_color(crate::theme::hairline(0.06))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pl(px(23.0))
                .child(label("Pull request")),
        )
        .child(render_sort_header(
            "Changes",
            PR_TABLE_CHANGES_WIDTH,
            PullRequestSortField::Changes,
            sort,
            theme,
            cx,
        ))
        .when(layout == PullRequestTableLayout::Wide, |element| {
            element.child(render_sort_header(
                "Opened",
                PR_TABLE_OPENED_WIDTH,
                PullRequestSortField::Opened,
                sort,
                theme,
                cx,
            ))
        })
        .child(render_sort_header(
            "Updated",
            PR_TABLE_UPDATED_WIDTH,
            PullRequestSortField::Updated,
            sort,
            theme,
            cx,
        ))
        .child(div().w(px(PR_TABLE_ACTION_WIDTH)).flex_none())
        .into_any_element()
}

fn render_sort_header(
    label: &'static str,
    width: f32,
    field: PullRequestSortField,
    sort: PullRequestSort,
    theme: &Theme,
    cx: &mut Context<PullRequestsPage>,
) -> AnyElement {
    let active = sort.field == field;
    let arrow = match sort.direction {
        SortDirection::Ascending => icons::ARROW_UP,
        SortDirection::Descending => icons::ARROW_DOWN,
    };
    div()
        .id(SharedString::from(format!(
            "pull-requests-sort-{}",
            field.key()
        )))
        .w(px(width))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.0))
        .cursor_pointer()
        .text_size(px(12.0))
        .text_color(if active {
            theme.text_muted
        } else {
            theme.text_faint
        })
        .hover(|style| style.text_color(theme.text))
        .on_click(cx.listener(move |page, _, _, cx| page.select_sort(field, cx)))
        .child(label)
        .child(
            div()
                .w(px(10.0))
                .h(px(10.0))
                .flex_none()
                .when(active, |element| {
                    element.child(
                        icon(arrow)
                            .size(px(9.0))
                            .text_color(theme.text_muted.opacity(0.8)),
                    )
                }),
        )
        .into_any_element()
}

fn render_table_row(
    item: &ChangeRequestListItem,
    layout: PullRequestTableLayout,
    theme: &Theme,
) -> AnyElement {
    let url = item.url.clone();
    let group: SharedString =
        format!("pull-request-row-hover-{}-{}", item.repository, item.number).into();
    let hover_t = motion::hover_t(&group);
    let row = div()
        .id(SharedString::from(format!(
            "pull-request-row-{}-{}",
            item.repository, item.number
        )))
        .group(group.clone())
        .relative()
        .top(px(-0.75 * hover_t))
        .w_full()
        .flex_none()
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::ink(0.025)))
        .on_hover(motion::hover_listener(group.clone()))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.open_url(&url);
        });

    match layout {
        PullRequestTableLayout::Narrow => row
            .px(px(8.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(render_pr_identity(item, theme, hover_t))
            .child(
                div()
                    .pl(px(23.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(render_diff_stats(item, true, theme, hover_t))
                    .child(div().flex_1())
                    .child(
                        div()
                            .line_height(px(14.0))
                            .text_size(hover_text_size(11.0, hover_t))
                            .text_color(theme.text_muted.opacity(0.65))
                            .child(SharedString::from(compact_relative_time(
                                item.updated_at,
                                Utc::now(),
                            ))),
                    )
                    .child(
                        icon(icons::ARROW_UP_RIGHT)
                            .size(px(12.0))
                            .text_color(theme.accent.opacity(0.7)),
                    ),
            )
            .into_any_element(),
        PullRequestTableLayout::Compact | PullRequestTableLayout::Wide => row
            .h(px(PR_TABLE_ROW_HEIGHT))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(render_pr_identity(item, theme, hover_t))
            .child(
                div()
                    .w(px(PR_TABLE_CHANGES_WIDTH))
                    .flex_none()
                    .child(render_diff_stats(item, false, theme, hover_t)),
            )
            .when(layout == PullRequestTableLayout::Wide, |element| {
                element.child(render_relative_time_cell(
                    item.created_at,
                    PR_TABLE_OPENED_WIDTH,
                    hover_t,
                    theme,
                ))
            })
            .child(render_relative_time_cell(
                item.updated_at,
                PR_TABLE_UPDATED_WIDTH,
                hover_t,
                theme,
            ))
            .child(
                div()
                    .w(px(PR_TABLE_ACTION_WIDTH))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .opacity(0.0)
                    .group_hover(group, |style| style.opacity(1.0))
                    .child(
                        icon(icons::ARROW_UP_RIGHT)
                            .size(px(12.0))
                            .text_color(theme.accent.opacity(0.7)),
                    ),
            )
            .into_any_element(),
    }
}

fn render_pr_identity(item: &ChangeRequestListItem, theme: &Theme, hover_t: f32) -> AnyElement {
    let conflicting = item.mergeability == ChangeRequestMergeability::Conflicting;
    let status_icon = div()
        .id(SharedString::from(format!(
            "pull-request-status-{}-{}",
            item.repository, item.number
        )))
        .size(px(15.0))
        .flex_none()
        .mt(px(3.0))
        .flex()
        .items_center()
        .justify_center()
        .child(
            icon(pull_request_status_icon(item.mergeability))
                .size(px(15.0))
                .text_color(if conflicting {
                    theme.danger_muted
                } else {
                    theme.success_muted
                }),
        )
        .when(conflicting, |element| {
            element
                .tooltip(|_, cx| cx.new(|_| MergeConflictTooltip).into())
                .tooltip_show_delay(Duration::from_millis(350))
        });
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_start()
        .gap(px(8.0))
        .child(status_icon)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .min_w_0()
                        .line_height(px(15.0))
                        .text_size(hover_text_size(12.0, hover_t))
                        .text_color(theme.text)
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(truncate_title(&item.title, 120))),
                        )
                        .when(item.is_draft, |element| {
                            element.child(render_pr_badge("DRAFT", theme.warning))
                        })
                        .when(
                            item.review_decision == ChangeRequestReviewDecision::ChangesRequested,
                            |element| {
                                element.child(render_pr_badge("CHANGES REQUESTED", theme.warning))
                            },
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .min_w_0()
                        .line_height(px(13.0))
                        .text_size(hover_text_size(10.0, hover_t))
                        .text_color(theme.text_faint)
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(item.repository.clone())),
                        )
                        .child(SharedString::from(format!("#{}", item.number))),
                ),
        )
        .into_any_element()
}

fn render_pr_badge(label: &'static str, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_none()
        .px(px(6.0))
        .h(px(15.0))
        .flex()
        .items_center()
        .rounded_full()
        .bg(color.opacity(0.1))
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(label)
        .into_any_element()
}

fn pull_request_status_icon(mergeability: ChangeRequestMergeability) -> &'static str {
    match mergeability {
        ChangeRequestMergeability::Conflicting => icons::DANGER_TRIANGLE,
        ChangeRequestMergeability::Mergeable | ChangeRequestMergeability::Unknown => {
            icons::PULL_REQUEST
        }
    }
}

fn render_diff_stats(
    item: &ChangeRequestListItem,
    compact: bool,
    theme: &Theme,
    hover_t: f32,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(if compact { 6.0 } else { 8.0 }))
        .font_family(theme.font_mono.clone())
        .line_height(px(14.0))
        .text_size(hover_text_size(10.5, hover_t))
        .child(
            div()
                .text_color(theme.success_muted)
                .child(SharedString::from(format!(
                    "+{}",
                    format_compact_count(item.additions)
                ))),
        )
        .child(
            div()
                .text_color(theme.danger_muted)
                .child(SharedString::from(format!(
                    "−{}",
                    format_compact_count(item.deletions)
                ))),
        )
        .into_any_element()
}

fn hover_text_size(base: f32, hover_t: f32) -> gpui::Pixels {
    px(base * (1.0 + PR_ROW_HOVER_TEXT_SCALE * hover_t))
}

fn render_relative_time_cell(
    timestamp: DateTime<Utc>,
    width: f32,
    hover_t: f32,
    theme: &Theme,
) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .truncate()
        .line_height(px(14.0))
        .text_size(hover_text_size(10.5, hover_t))
        .text_color(theme.text_muted)
        .child(SharedString::from(relative_time(timestamp, Utc::now())))
        .into_any_element()
}

fn eligible_desktop_devices(devices: &[Device]) -> Vec<Device> {
    let mut devices: Vec<_> = devices
        .iter()
        .filter(|device| !matches!(device.platform.as_str(), "ios" | "android"))
        .cloned()
        .collect();
    devices.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    devices
}

fn normalized_target_device(
    target: Option<&str>,
    devices: &[Device],
    local_id: Option<&str>,
) -> Option<String> {
    let target = target?;
    if local_id == Some(target) {
        return None;
    }

    // The local row is inserted before the first device snapshot is emitted.
    // Until it arrives, keep the selection instead of judging an empty or
    // partial pre-sync list as authoritative.
    let local_is_present = local_id
        .is_some_and(|local_id| devices.iter().any(|device| device.id.as_str() == local_id));
    if !local_is_present
        || devices.iter().any(|device| {
            device.id == target && !matches!(device.platform.as_str(), "ios" | "android")
        })
    {
        Some(target.to_string())
    } else {
        None
    }
}

fn params_for_target(target: Option<&str>) -> serde_json::Value {
    match target {
        Some(target) => serde_json::json!({ "targetDeviceId": target }),
        None => serde_json::json!({}),
    }
}

fn selected_device_name(state: &AppState, target: Option<&str>) -> String {
    target
        .or(state.local_device_id.as_deref())
        .and_then(|id| state.device_name(id))
        .map(single_line)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "This device".to_string())
}

fn map_rpc_error(error: &RpcError, target_name: &str) -> PullRequestsPageError {
    match error {
        RpcError::Capability(code) if code == capability_errors::PULL_REQUESTS_CLI_UNAVAILABLE => {
            PullRequestsPageError::CliUnavailable
        }
        RpcError::Capability(code) if code == capability_errors::PULL_REQUESTS_AUTHENTICATION => {
            PullRequestsPageError::Authentication
        }
        RpcError::Capability(code) if code == capability_errors::PULL_REQUESTS_RATE_LIMITED => {
            PullRequestsPageError::RateLimited
        }
        RpcError::UnknownMethod(_) => {
            PullRequestsPageError::UpdateRequired(target_name.to_string())
        }
        RpcError::Capability(_)
        | RpcError::BadParams(_)
        | RpcError::Failed(_)
        | RpcError::Transport(_)
        | RpcError::Closed => PullRequestsPageError::Network,
    }
}

fn error_copy(error: &PullRequestsPageError) -> (String, String) {
    match error {
        PullRequestsPageError::CliUnavailable => (
            "GitHub CLI isn’t available on this device".into(),
            "Install gh and sign in to view your pull requests.".into(),
        ),
        PullRequestsPageError::Authentication => (
            "Sign in to GitHub on this device".into(),
            "Run gh auth login, then refresh this page.".into(),
        ),
        PullRequestsPageError::RateLimited => (
            "GitHub’s rate limit was reached".into(),
            "Try again later.".into(),
        ),
        PullRequestsPageError::Network => (
            "Couldn’t load pull requests".into(),
            "Check the connection and try again.".into(),
        ),
        PullRequestsPageError::RemoteOffline(name) => (
            format!("{name} is offline"),
            "Reconnect the device and try again.".into(),
        ),
        PullRequestsPageError::UpdateRequired(name) => (
            format!("Update Zeron on {name}"),
            "This device doesn’t support the pull request dashboard yet.".into(),
        ),
    }
}

fn platform_icon(platform: &str) -> &'static str {
    match platform {
        "macos" | "darwin" => icons::LAPTOP,
        _ => icons::MONITOR,
    }
}

fn relative_time(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(timestamp).num_seconds().max(0);
    let (amount, unit) = if seconds < 60 {
        return "just now".into();
    } else if seconds < 3_600 {
        (seconds / 60, "minute")
    } else if seconds < 86_400 {
        (seconds / 3_600, "hour")
    } else if seconds < 604_800 {
        (seconds / 86_400, "day")
    } else if seconds < 2_592_000 {
        (seconds / 604_800, "week")
    } else if seconds < 31_536_000 {
        (seconds / 2_592_000, "month")
    } else {
        (seconds / 31_536_000, "year")
    };
    format!("{amount} {unit}{} ago", if amount == 1 { "" } else { "s" })
}

fn compact_relative_time(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(timestamp).num_seconds().max(0);
    if seconds < 60 {
        "now".into()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else if seconds < 604_800 {
        format!("{}d ago", seconds / 86_400)
    } else if seconds < 2_592_000 {
        format!("{}w ago", seconds / 604_800)
    } else {
        format!("{}mo ago", seconds / 2_592_000)
    }
}

fn format_compact_count(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 1_000_000 {
        let tenths = value / 100;
        if tenths % 10 == 0 {
            format!("{}k", value / 1_000)
        } else {
            format!("{}.{:01}k", value / 1_000, tenths % 10)
        }
    } else {
        let tenths = value / 100_000;
        if tenths % 10 == 0 {
            format!("{}m", value / 1_000_000)
        } else {
            format!("{}.{:01}m", value / 1_000_000, tenths % 10)
        }
    }
}

fn truncate_title(title: &str, max_chars: usize) -> String {
    let title = single_line(title);
    if title.chars().count() <= max_chars {
        return title;
    }
    let mut truncated: String = title.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn response_is_current(current: u64, response: u64) -> bool {
    current == response
}

fn settle_snapshot<T>(
    snapshot: &mut Vec<T>,
    result: Result<Vec<T>, PullRequestsPageError>,
) -> PullRequestsLoadState {
    match result {
        Ok(items) => {
            *snapshot = items;
            PullRequestsLoadState::Ready
        }
        Err(error) => PullRequestsLoadState::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, TimeZone};

    fn device(id: &str, platform: &str) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            platform: platform.into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    fn pull_request(
        repository: &str,
        number: u64,
        changes: u64,
        opened_day: u32,
        updated_hour: u32,
    ) -> ChangeRequestListItem {
        ChangeRequestListItem {
            provider: "github".into(),
            repository: repository.into(),
            number,
            title: format!("Pull request {number}"),
            url: format!("https://github.com/{repository}/pull/{number}"),
            state: zeron_proto::ChangeRequestState::Open,
            is_draft: false,
            review_decision: ChangeRequestReviewDecision::Unknown,
            additions: changes,
            deletions: 0,
            mergeability: ChangeRequestMergeability::Mergeable,
            created_at: Utc.with_ymd_and_hms(2026, 8, opened_day, 8, 0, 0).unwrap(),
            updated_at: Utc
                .with_ymd_and_hms(2026, 8, 19, updated_hour, 0, 0)
                .unwrap(),
        }
    }

    fn numbers(items: &[ChangeRequestListItem]) -> Vec<u64> {
        items.iter().map(|item| item.number).collect()
    }

    #[test]
    fn only_desktop_devices_are_eligible_targets() {
        let eligible = eligible_desktop_devices(&[
            device("mac", "macos"),
            device("linux", "linux"),
            device("phone", "ios"),
            device("tablet", "android"),
        ]);
        assert_eq!(
            eligible.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["linux", "mac"]
        );
    }

    #[test]
    fn target_params_keep_local_calls_direct() {
        assert_eq!(params_for_target(None), serde_json::json!({}));
        assert_eq!(
            params_for_target(Some("host")),
            serde_json::json!({ "targetDeviceId": "host" })
        );
    }

    #[test]
    fn table_breakpoints_follow_the_measured_content_width() {
        assert_eq!(table_layout(639.0), PullRequestTableLayout::Narrow);
        assert_eq!(table_layout(640.0), PullRequestTableLayout::Compact);
        assert_eq!(table_layout(899.0), PullRequestTableLayout::Compact);
        assert_eq!(table_layout(900.0), PullRequestTableLayout::Wide);
        assert_eq!(
            table_layout(table_content_width(687.0)),
            PullRequestTableLayout::Narrow
        );
        assert_eq!(
            table_layout(table_content_width(688.0)),
            PullRequestTableLayout::Compact
        );
        assert_eq!(
            table_layout(table_content_width(948.0)),
            PullRequestTableLayout::Wide
        );
    }

    #[test]
    fn missing_remote_target_falls_back_only_after_an_authoritative_device_frame() {
        let local = device("local", "macos");
        let remote = device("remote", "linux");

        assert_eq!(
            normalized_target_device(Some("remote"), &[local.clone(), remote], Some("local")),
            Some("remote".into())
        );
        assert_eq!(
            normalized_target_device(Some("remote"), &[local], Some("local")),
            None
        );
        assert_eq!(
            normalized_target_device(Some("remote"), &[], Some("local")),
            Some("remote".into()),
            "the empty pre-sync list must not discard the selection"
        );
        assert_eq!(
            normalized_target_device(Some("local"), &[], Some("local")),
            None,
            "local calls stay direct even before the device frame"
        );
    }

    #[test]
    fn sorting_defaults_to_recent_updates_and_toggles_each_column() {
        assert_eq!(
            PullRequestSort::DEFAULT,
            PullRequestSort {
                field: PullRequestSortField::Updated,
                direction: SortDirection::Descending,
            }
        );
        assert_eq!(
            PullRequestSort::DEFAULT.select(PullRequestSortField::Updated),
            PullRequestSort {
                field: PullRequestSortField::Updated,
                direction: SortDirection::Ascending,
            }
        );
        assert_eq!(
            PullRequestSort::DEFAULT.select(PullRequestSortField::Changes),
            PullRequestSort {
                field: PullRequestSortField::Changes,
                direction: SortDirection::Descending,
            }
        );
    }

    #[test]
    fn table_sorting_uses_changes_opened_and_updated_values() {
        let original = vec![
            pull_request("beta/repo", 2, 30, 3, 9),
            pull_request("alpha/repo", 1, 10, 1, 10),
            pull_request("alpha/repo", 3, 30, 2, 11),
        ];

        let mut items = original.clone();
        sort_pull_requests(&mut items, PullRequestSort::DEFAULT);
        assert_eq!(numbers(&items), [3, 1, 2]);

        let mut items = original.clone();
        sort_pull_requests(
            &mut items,
            PullRequestSort {
                field: PullRequestSortField::Changes,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(numbers(&items), [3, 2, 1]);

        let mut items = original.clone();
        sort_pull_requests(
            &mut items,
            PullRequestSort {
                field: PullRequestSortField::Opened,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(numbers(&items), [1, 3, 2]);

        let mut items = original;
        sort_pull_requests(
            &mut items,
            PullRequestSort {
                field: PullRequestSortField::Updated,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(numbers(&items), [2, 1, 3]);
    }

    #[test]
    fn table_sorting_breaks_ties_by_repository_and_number() {
        let mut items = vec![
            pull_request("zeta/repo", 2, 10, 1, 10),
            pull_request("alpha/repo", 3, 10, 1, 10),
            pull_request("alpha/repo", 1, 10, 1, 10),
        ];
        sort_pull_requests(
            &mut items,
            PullRequestSort {
                field: PullRequestSortField::Changes,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(numbers(&items), [1, 3, 2]);
    }

    #[test]
    fn sort_animation_offsets_preserve_direction_and_limit_distance() {
        let previous = ["a", "b", "c", "d", "e"].map(str::to_owned).to_vec();
        let next = ["e", "b", "c", "d", "a"].map(str::to_owned).to_vec();
        let offsets = sort_row_offsets(&previous, &next);

        assert_eq!(offsets.get("e"), Some(&PR_SORT_MAX_OFFSET));
        assert_eq!(offsets.get("a"), Some(&-PR_SORT_MAX_OFFSET));
        assert!(!offsets.contains_key("b"));
        assert!(!offsets.contains_key("c"));
        assert!(!offsets.contains_key("d"));

        let adjacent = sort_row_offsets(&["a".into(), "b".into()], &["b".into(), "a".into()]);
        assert_eq!(adjacent.get("b"), Some(&PR_SORT_OFFSET_PER_ROW));
        assert_eq!(adjacent.get("a"), Some(&-PR_SORT_OFFSET_PER_ROW));
    }

    #[test]
    fn relative_dates_are_readable_and_pluralized() {
        let now = Utc::now();
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - TimeDelta::hours(1), now), "1 hour ago");
        assert_eq!(relative_time(now - TimeDelta::days(2), now), "2 days ago");
        assert_eq!(
            compact_relative_time(now - TimeDelta::hours(2), now),
            "2h ago"
        );
        assert_eq!(compact_relative_time(now + TimeDelta::hours(2), now), "now");
    }

    #[test]
    fn only_conflicting_requests_use_the_warning_icon() {
        assert_eq!(
            pull_request_status_icon(ChangeRequestMergeability::Conflicting),
            icons::DANGER_TRIANGLE
        );
        for mergeability in [
            ChangeRequestMergeability::Mergeable,
            ChangeRequestMergeability::Unknown,
        ] {
            assert_eq!(pull_request_status_icon(mergeability), icons::PULL_REQUEST);
        }
    }

    #[test]
    fn diff_counts_stay_compact_without_losing_scale() {
        assert_eq!(format_compact_count(999), "999");
        assert_eq!(format_compact_count(1_000), "1k");
        assert_eq!(format_compact_count(13_223), "13.2k");
        assert_eq!(format_compact_count(1_250_000), "1.2m");
    }

    #[test]
    fn stale_responses_cannot_replace_a_new_target_snapshot() {
        assert!(response_is_current(7, 7));
        assert!(!response_is_current(8, 7));
    }

    #[test]
    fn successful_empty_refresh_replaces_the_previous_snapshot() {
        let mut snapshot = vec![1, 2];
        let state = settle_snapshot(&mut snapshot, Ok(Vec::new()));
        assert!(snapshot.is_empty());
        assert_eq!(state, PullRequestsLoadState::Ready);
    }

    #[test]
    fn failed_refresh_preserves_the_previous_snapshot() {
        let mut snapshot = vec![1, 2];
        let state = settle_snapshot(&mut snapshot, Err(PullRequestsPageError::Authentication));
        assert_eq!(snapshot, [1, 2]);
        assert_eq!(
            state,
            PullRequestsLoadState::Failed(PullRequestsPageError::Authentication)
        );
    }

    #[test]
    fn error_mapping_uses_stable_codes_and_hides_transport_details() {
        assert_eq!(
            map_rpc_error(
                &RpcError::Capability(capability_errors::PULL_REQUESTS_AUTHENTICATION.into()),
                "Studio Mac",
            ),
            PullRequestsPageError::Authentication
        );
        assert_eq!(
            map_rpc_error(
                &RpcError::UnknownMethod("ListOpenChangeRequests".into()),
                "Studio Mac"
            ),
            PullRequestsPageError::UpdateRequired("Studio Mac".into())
        );
        assert_eq!(
            map_rpc_error(&RpcError::Transport("secret detail".into()), "Studio Mac"),
            PullRequestsPageError::Network
        );
    }

    #[test]
    fn visible_device_names_and_titles_are_sanitized() {
        assert_eq!(single_line("MacBook\n Pro"), "MacBook Pro");
        assert!(truncate_title(&"a".repeat(200), 120).ends_with('…'));
        assert_eq!(truncate_title("Short title", 120), "Short title");
    }
}
