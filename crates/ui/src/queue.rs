//! The pending-message queue, docked above the composer.
//!
//! Everything you typed while the agent was busy, in the order it will be sent.
//! The rows live on the session doc ([`zeron_doc::QueuedMessage`]), so the phone
//! shows the same queue and either device can reorder it.
//!
//! Each row exposes a uniform `Steer` control, with delivery semantics
//! explained in its tooltip. Editing moves
//! the message into the composer while its leased row reserves its position.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div, prelude::*, px,
};

use zeron_doc::{QueueDeliveryGate, QueuedMessage};
use zeron_rpc::methods;

use crate::composer::{Composer, QUEUE_COMPOSER_OVERLAP};
use crate::icons::{self, icon};
use crate::motion::{self, AnimationExt as _, TAB_SLIDE};
use crate::settings::shortcuts::modifier_send_label;
use crate::terminal::panel::{drop_index, slide_offset};
use crate::theme::Theme;

/// Queue rows are replicated CRDT state, so ordinary mutations deliberately
/// land on the local engine. Delivery and cancellation must execute on the
/// chat's owning device because they race over the same row.
fn queue_action_needs_host(method: &str) -> bool {
    matches!(
        method,
        methods::SEND_QUEUED_MESSAGE_NOW
            | methods::STEER_QUEUED_MESSAGE_NOW
            | methods::REMOVE_QUEUED_MESSAGE
            | methods::BEGIN_QUEUED_MESSAGE_EDIT
            | methods::RENEW_QUEUED_MESSAGE_EDIT
            | methods::FINISH_QUEUED_MESSAGE_EDIT
    )
}

/// Queue mutation replies are deliberately explicit. A false or malformed
/// acknowledgement means an optimistic local edit may not match the document
/// (for example, another device removed the same row first).
fn queue_mutation_acknowledged(method: &str, reply: &serde_json::Value) -> bool {
    let field = match method {
        methods::UPDATE_QUEUED_MESSAGE | methods::MOVE_QUEUED_MESSAGE => "changed",
        methods::REMOVE_QUEUED_MESSAGE => "removed",
        methods::SEND_QUEUED_MESSAGE_NOW | methods::STEER_QUEUED_MESSAGE_NOW => "sent",
        _ => return true,
    };
    reply.get(field).and_then(serde_json::Value::as_bool) == Some(true)
}

struct QueueActionTooltip {
    label: SharedString,
}

impl Render for QueueActionTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_overlay)
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .child(self.label.clone())
    }
}

/// Compact, borderless rows inside the queue's single glass surface.
const ROW_HEIGHT: f32 = 36.0;
const QUEUE_TEXT_SIZE: f32 = 12.5;
const ROW_GAP: f32 = 2.0;
const ROW_SLOT: f32 = ROW_HEIGHT + ROW_GAP;
const ROW_PAD_X: f32 = 8.0;
const ROW_RADIUS: f32 = 8.0;
const PANEL_PAD_X: f32 = 8.0;
const PANEL_RADIUS: f32 = ROW_RADIUS + PANEL_PAD_X;
const PANEL_PAD_TOP: f32 = PANEL_PAD_X;
/// The custom 24px queue glyphs have quieter geometry than the legacy set, so
/// render them slightly larger to preserve the previous optical weight.
const QUEUE_ICON_SIZE: f32 = 13.0;

/// The single trailing action a queue row advertises and executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePrimaryAction {
    Steer,
    SendNow,
}

impl QueuePrimaryAction {
    fn tooltip(self) -> &'static str {
        match self {
            Self::Steer => "Steer without interrupting",
            Self::SendNow => "Send now (interrupt)",
        }
    }
}

/// Attachments cannot travel through the text-only steering channel. Unknown
/// catalogs are conservative too: never promise a non-interrupting steer until
/// the selected provider has advertised it.
fn queue_primary_action(
    resolved_mid_turn_steering: Option<bool>,
    has_attachments: bool,
) -> Option<QueuePrimaryAction> {
    match resolved_mid_turn_steering {
        Some(true) if !has_attachments => Some(QueuePrimaryAction::Steer),
        Some(_) => Some(QueuePrimaryAction::SendNow),
        None => None,
    }
}

/// The primary action is executable only when both the selected provider and
/// the chat host can honor it, and nobody currently owns the row for editing
/// or review. The row button, keyboard accelerator and its hint all use this
/// decision so they cannot disagree about availability.
fn available_queue_primary_action(
    resolved_mid_turn_steering: Option<bool>,
    has_attachments: bool,
    delivery_blocked: bool,
    host_supports_actions: bool,
) -> Option<QueuePrimaryAction> {
    if delivery_blocked || !host_supports_actions {
        return None;
    }
    queue_primary_action(resolved_mid_turn_steering, has_attachments)
}

fn queue_head_shortcut_visible(
    index: usize,
    reveal_requested: bool,
    action_available: bool,
) -> bool {
    index == 0 && reveal_requested && action_available
}

/// Translate a pointer inside the whole panel into a row slot. The top pad
/// belongs to slot zero; the bottom pad clamps to the final row.
fn queue_drop_index(panel_y: f32, count: usize) -> usize {
    drop_index(panel_y - PANEL_PAD_TOP, ROW_SLOT, count)
}

/// Paint-only start and target positions for the PR #90 reorder treatment.
/// The dragged row travels to the hovered slot while every row in its path
/// slides into the space it leaves behind.
fn queue_drag_offsets(ix: usize, from: usize, prev_over: usize, over: usize) -> (f32, f32) {
    if ix == from {
        (
            (prev_over as f32 - from as f32) * ROW_SLOT,
            (over as f32 - from as f32) * ROW_SLOT,
        )
    } else {
        (
            slide_offset(ix, from, prev_over) * ROW_SLOT,
            slide_offset(ix, from, over) * ROW_SLOT,
        )
    }
}

/// A queue row being dragged (gpui drag-and-drop). Scoped to its chat so a
/// drag can't land in a queue it didn't come from.
pub struct QueueDragPayload {
    chat: String,
    from: usize,
}

/// Where the dragged row would land, including the previous slot needed to
/// restart the short PR #90-style slide from its current visual position.
pub struct QueueDragState {
    pub from: usize,
    pub over: usize,
    pub prev_over: usize,
    pub epoch: usize,
}

/// Invisible cursor ghost: the real row stays in the queue and moves between
/// slots, instead of following the pointer as a detached tooltip.
struct QueueGhost;

impl Render for QueueGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// One line of a queued message: the newlines that make it a paragraph in the
/// composer make it three rows here, and the row is one line tall.
fn one_line(text: &str) -> SharedString {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    SharedString::from(flat)
}

/// New queue rows contain only editable user text. During a rolling upgrade,
/// an older client may still have stored the attachment trailer in `text`.
/// Hide it only when the parsed paths exactly match the row's attachment field.
fn queue_visible_text(text: &str, attachments: &[String]) -> String {
    if attachments.is_empty() {
        return text.to_string();
    }
    let parsed = crate::attachments::parse_user_message_images(text);
    let paths_match = parsed.attachments.len() == attachments.len()
        && parsed
            .attachments
            .iter()
            .zip(attachments)
            .all(|(parsed, stored)| parsed.path == *stored);
    if !paths_match {
        return text.to_string();
    }
    if parsed.text.trim().is_empty() {
        crate::attachments::ATTACHMENT_ONLY_TEXT.to_string()
    } else {
        parsed.text
    }
}

fn queue_panel_surface(theme: &Theme) -> gpui::Div {
    div()
        .occlude()
        .rounded_t(px(PANEL_RADIUS))
        .bg(theme.input_glass_bg())
        .border_1()
        .border_color(theme.border)
        .when(!theme.is_frost(), |el| el.shadow_lg())
        .px(px(PANEL_PAD_X))
        .pt(px(PANEL_PAD_TOP))
        .pb(px(QUEUE_COMPOSER_OVERLAP))
        .flex()
        .flex_col()
}

fn queue_rows(
    scroll: &gpui::ScrollHandle,
    max_height: gpui::Pixels,
    rows: impl IntoIterator<Item = AnyElement>,
) -> crate::edge_fade::EdgeFaded {
    crate::edge_fade::edge_faded(
        Theme::TRANSCRIPT_FADE_BAND,
        true,
        true,
        div()
            .id("message-queue-rows")
            .max_h(max_height)
            .overflow_y_scroll()
            .track_scroll(scroll)
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            .children(rows),
    )
    .fade_overflow_y(scroll)
    // GPUI samples glyph fades at baseline + font size.
    .outset_bottom(QUEUE_TEXT_SIZE)
}

impl Composer {
    /// The queue panel, or `None` when nothing is waiting. Like the composer,
    /// it is one frosted surface; rows use spacing and hover wash rather than
    /// nesting raised cards inside it.
    pub(crate) fn render_queue_panel(
        &mut self,
        show_head_shortcut: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // A drop outside the panel ends GPUI's active drag without invoking our
        // `on_drop`. Never leave the source row replaced by a stale gap.
        if self.queue_drag.is_some() && !cx.has_active_drag() {
            self.queue_drag = None;
        }
        let (items, chat_id, host_supports_actions) = {
            let state = self.state.read(cx);
            let chat_id = state.selected_chat.clone()?;
            let host_supports_actions = state.chat_host_supports(
                &chat_id,
                zeron_proto::capabilities::MESSAGE_QUEUE_ACTIONS_V1,
            );
            (state.queue.clone(), chat_id, host_supports_actions)
        };
        if items.is_empty() {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let count = items.len();
        let drag = self
            .queue_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.prev_over, d.epoch));
        let editing = self.editing_queued.clone();
        let mid_turn_steering = self.pickers().read(cx).resolved_mid_turn_steering(cx);

        let list_chat = chat_id.clone();
        let drop_chat = chat_id.clone();
        let rows = queue_rows(
            &self.queue_scroll,
            window.viewport_size().height * 0.3,
            items.iter().enumerate().map(|(ix, item)| {
                self.queue_row(
                    &chat_id,
                    ix,
                    item,
                    drag,
                    &editing,
                    mid_turn_steering,
                    host_supports_actions,
                    show_head_shortcut,
                    &theme,
                    cx,
                )
            }),
        );

        let panel = queue_panel_surface(&theme)
            // The complete glass surface is a drop target, including its
            // padding.
            .on_drag_move::<QueueDragPayload>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<QueueDragPayload>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.chat != list_chat {
                        return;
                    }
                    let from = payload.from;
                    let rel_y = f32::from(event.event.position.y)
                        - f32::from(event.bounds.top())
                        - f32::from(this.queue_scroll.offset().y);
                    let over = queue_drop_index(rel_y, count);
                    this.update_queue_drag_over(from, over, cx);
                },
            ))
            .on_drop::<QueueDragPayload>(cx.listener(
                move |this, payload: &QueueDragPayload, _, cx| {
                    if payload.chat != drop_chat {
                        this.queue_drag = None;
                        cx.notify();
                        return;
                    }
                    let to = this
                        .queue_drag
                        .as_ref()
                        .map(|d| d.over)
                        .unwrap_or(payload.from);
                    this.queue_drag = None;
                    this.move_queued(payload.from, to, cx);
                },
            ))
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel_queue_drag(cx)),
            )
            .child(rows);
        Some(crate::frost::frosted(PANEL_RADIUS, 16.0, panel).into_any_element())
    }

    /// One queued message: a quiet queue marker, the text, edit controls, and
    /// one explicit primary delivery action.
    #[allow(clippy::too_many_arguments)]
    fn queue_row(
        &self,
        chat_id: &str,
        ix: usize,
        item: &QueuedMessage,
        drag: Option<(usize, usize, usize, usize)>,
        editing: &Option<String>,
        mid_turn_steering: Option<bool>,
        host_supports_actions: bool,
        show_head_shortcut: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = SharedString::from(format!("queue-{}", item.id));
        let being_edited = editing.as_deref() == Some(item.id.as_str());
        let being_removed = self.queue_removing.contains(&item.id);
        let delivery_blocked = item.delivery_gate.is_some();
        let interaction_blocked = delivery_blocked || being_removed;
        let text = match &item.delivery_gate {
            Some(QueueDeliveryGate::Editing {
                owner_device_id, ..
            }) if !being_edited => SharedString::from(format!("Editing on {owner_device_id}")),
            Some(QueueDeliveryGate::ReviewRequired { .. }) if !being_edited => {
                SharedString::from("Needs review")
            }
            _ => one_line(&queue_visible_text(&item.text, &item.attachments)),
        };

        let edit_id = item.id.clone();
        let edit = self.queue_action(
            &key,
            "edit",
            "Edit",
            icons::PEN,
            !being_removed,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.begin_queue_edit(edit_id.clone(), cx);
            }),
        );
        let drop_id = item.id.clone();
        let discard = self.queue_action(
            &key,
            "drop",
            if being_removed {
                "Removing…"
            } else {
                "Remove"
            },
            icons::TRASH_BIN_MINIMALISTIC,
            !being_removed,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.remove_queued(drop_id.clone(), cx);
            }),
        );
        let resolved_primary = available_queue_primary_action(
            mid_turn_steering,
            !item.attachments.is_empty(),
            interaction_blocked,
            host_supports_actions,
        );
        let primary_action = resolved_primary.unwrap_or(QueuePrimaryAction::SendNow);
        let primary_id = item.id.clone();
        let primary = self.queue_primary_action_button(
            &key,
            primary_action,
            resolved_primary.is_some(),
            theme,
            cx.listener(move |this, _, _, cx| {
                this.activate_queued_primary(primary_id.clone(), primary_action, cx);
            }),
        );
        let save = self.queue_action(
            &key,
            "save",
            "Save to queue",
            icons::QUEUE_CHECK,
            !self.queue_edit_finishing,
            theme,
            cx.listener(|this, _, _, cx| {
                this.commit_queue_edit(cx);
            }),
        );
        let cancel = self.queue_action(
            &key,
            "cancel",
            "Cancel",
            icons::QUEUE_CLOSE,
            !self.queue_edit_finishing,
            theme,
            cx.listener(|this, _, _, cx| {
                this.cancel_queue_edit(cx);
            }),
        );

        let drag_chat = chat_id.to_string();
        let queue_marker = div()
            .id(SharedString::from(format!("{key}-drag")))
            .w(px(14.0))
            .h(px(22.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .cursor_pointer()
            .when(interaction_blocked, |el| {
                el.cursor(gpui::CursorStyle::Arrow).opacity(0.35)
            })
            .child(
                icon(icons::QUEUE_DRAG_HANDLE)
                    .size(px(QUEUE_ICON_SIZE))
                    .text_color(theme.text_muted.opacity(0.5)),
            );

        let row = div()
            .id(SharedString::from(format!("{key}-row")))
            .h(px(ROW_HEIGHT))
            .flex_none()
            .px(px(ROW_PAD_X))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(ROW_RADIUS))
            .when(being_edited, |el| el.bg(crate::theme::ink(0.06)))
            .when(!being_edited && !being_removed, |el| {
                el.hover(|s| s.bg(crate::theme::ink(0.04)))
            })
            .when(being_removed, |el| el.opacity(0.55))
            .cursor(gpui::CursorStyle::Arrow)
            // The marker hints that the row belongs to the queue, while the
            // proven full-row drag hitbox keeps reordering easy. Editing
            // disables it so selection cannot become a reorder gesture.
            .when(!being_edited && !interaction_blocked, |el| {
                el.on_drag(
                    QueueDragPayload {
                        chat: drag_chat,
                        from: ix,
                    },
                    move |_payload, _point, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| QueueGhost)
                    },
                )
            })
            .when(!being_edited, |el| el.child(queue_marker))
            // Preserve text alignment while removing the disabled drag glyph
            // from the editing state.
            .when(being_edited, |el| el.child(div().w(px(14.0)).flex_none()))
            .when(!being_edited, |el| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(QUEUE_TEXT_SIZE))
                        .text_color(theme.text.opacity(0.9))
                        .child(text),
                )
            })
            .when(being_edited, |el| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(QUEUE_TEXT_SIZE))
                        .text_color(theme.text_muted)
                        .child(if self.queue_edit_finishing {
                            "Saving…"
                        } else {
                            "Editing in composer"
                        }),
                )
            })
            // Files are why a row can sit through a steerable turn, so say so.
            .when(!item.attachments.is_empty(), |el| {
                el.child(
                    crate::icons::icon(crate::icons::QUEUE_PAPERCLIP)
                        .size(px(QUEUE_ICON_SIZE))
                        .text_color(theme.text_muted.opacity(0.7)),
                )
            })
            .when(being_edited, |el| {
                el.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.0))
                        .child(save)
                        .child(cancel),
                )
            })
            .when(!being_edited, |el| {
                el.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.0))
                        .child(discard)
                        .child(edit)
                        .when(
                            queue_head_shortcut_visible(
                                ix,
                                show_head_shortcut,
                                resolved_primary.is_some() && !being_removed,
                            ),
                            |el| {
                                el.child(crate::popover::kbd_hint(
                                    theme,
                                    modifier_send_label(cfg!(target_os = "macos")),
                                ))
                            },
                        )
                        .child(primary),
                )
            });

        let Some((from, over, prev_over, epoch)) = drag else {
            return row.into_any_element();
        };
        let (start, target) = queue_drag_offsets(ix, from, prev_over, over);
        if cx.reduce_motion() {
            return div()
                .relative()
                .top(px(target))
                .child(row)
                .into_any_element();
        }
        div()
            .child(row)
            .with_animation(
                ("queue-row-slide", (ix as u64) | ((epoch as u64) << 32)),
                TAB_SLIDE.animation(),
                move |el, t| el.relative().top(px(motion::lerp(start, target, t))),
            )
            .into_any_element()
    }

    /// A permanently-visible trailing glyph button. The queue reference keeps
    /// edit and remove present instead of revealing them only on hover.
    fn queue_action(
        &self,
        key: &SharedString,
        slot: &str,
        label: &'static str,
        glyph: &'static str,
        enabled: bool,
        theme: &Theme,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let own = SharedString::from(format!("{key}-{slot}-grp"));
        div()
            .id(SharedString::from(format!("{key}-{slot}")))
            .group(own.clone())
            .size(px(18.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .opacity(0.72)
            .when(enabled, |el| {
                el.cursor_pointer()
                    .hover(|s| s.opacity(1.0).bg(crate::theme::ink(0.07)))
                    .on_click(on_click)
            })
            .when(!enabled, |el| {
                el.cursor(gpui::CursorStyle::Arrow).opacity(0.45)
            })
            .tooltip(move |_, cx| {
                cx.new(|_| QueueActionTooltip {
                    label: label.into(),
                })
                .into()
            })
            .tooltip_show_delay(std::time::Duration::from_millis(350))
            .child(
                icon(glyph)
                    .size(px(QUEUE_ICON_SIZE))
                    .text_color(theme.text_muted.opacity(0.8))
                    .group_hover(own, |s| s.text_color(theme.text)),
            )
            .into_any_element()
    }

    /// Uniform Steer control; the tooltip explains the resolved delivery behavior.
    fn queue_primary_action_button(
        &self,
        key: &SharedString,
        action: QueuePrimaryAction,
        enabled: bool,
        theme: &Theme,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let tooltip = if enabled {
            action.tooltip()
        } else {
            "Waiting for provider capabilities"
        };
        div()
            .id(SharedString::from(format!("{key}-primary")))
            .h(px(22.0))
            .flex_none()
            .px(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .rounded(px(6.0))
            .text_size(px(11.5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.82))
            .when(enabled, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.07)).text_color(theme.text))
                    .on_click(on_click)
            })
            .when(!enabled, |el| {
                el.cursor(gpui::CursorStyle::Arrow).opacity(0.5)
            })
            .tooltip(move |_, cx| {
                cx.new(|_| QueueActionTooltip {
                    label: tooltip.into(),
                })
                .into()
            })
            .tooltip_show_delay(std::time::Duration::from_millis(350))
            .child("Steer")
            .into_any_element()
    }

    /// Track the drop slot while a row is dragged over the list.
    fn update_queue_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.queue_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch = drag.epoch.wrapping_add(1);
                    cx.notify();
                }
            }
            _ => {
                self.queue_drag = Some(QueueDragState {
                    from,
                    over,
                    prev_over: from,
                    epoch: 0,
                });
                cx.notify();
            }
        }
    }

    /// Restore a row whose pointer was released outside the queue's drop zone.
    fn cancel_queue_drag(&mut self, cx: &mut Context<Self>) {
        if self.queue_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Move the row at `from` to `to`, optimistically here and for real on the
    /// doc (the watch frame is what everyone else sees).
    pub(crate) fn move_queued(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to {
            cx.notify();
            return;
        }
        let Some(id) = self
            .state
            .read(cx)
            .queue
            .get(from)
            .map(|item| item.id.clone())
        else {
            return;
        };
        self.state.update(cx, |state, cx| {
            if from < state.queue.len() {
                let item = state.queue.remove(from);
                state.queue.insert(to.min(state.queue.len()), item);
                cx.notify();
            }
        });
        self.queue_rpc(
            methods::MOVE_QUEUED_MESSAGE,
            serde_json::json!({ "id": id, "toIndex": to }),
            "Couldn't reorder the queue",
            cx,
        );
    }

    /// Cancel a queued message at its host. The row remains visible and inert
    /// until the host acknowledges winning the race against automatic drain.
    pub(crate) fn remove_queued(&mut self, id: String, cx: &mut Context<Self>) {
        if self.queue_removing.contains(&id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (chat_id, host_device_id, supported) = {
            let state = self.state.read(cx);
            let Some(chat_id) = state.selected_chat.clone() else {
                return;
            };
            let Some(host_device_id) = state.selected_chat_row().map(|chat| chat.device_id.clone())
            else {
                return;
            };
            let supported = state.chat_host_supports(
                &chat_id,
                zeron_proto::capabilities::MESSAGE_QUEUE_ACTIONS_V1,
            );
            (chat_id, host_device_id, supported)
        };
        if !supported {
            self.failure = Some("The chat host does not support safe queue removal".into());
            cx.notify();
            return;
        }
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.clear_queue_edit(cx);
        }
        self.queue_removing.insert(id.clone());
        self.queue_drag = None;
        cx.notify();

        let params = serde_json::json!({
            "chatId": chat_id,
            "id": id,
            "targetDeviceId": host_device_id,
        });
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::REMOVE_QUEUED_MESSAGE, params)
                .await;
            this.update(cx, |composer, cx| {
                composer.queue_removing.remove(&id);
                let selected_matches =
                    composer.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str());
                match result {
                    Ok(reply)
                        if queue_mutation_acknowledged(methods::REMOVE_QUEUED_MESSAGE, &reply) =>
                    {
                        if selected_matches {
                            composer.state.update(cx, |state, cx| {
                                state.queue.retain(|item| item.id != id);
                                cx.notify();
                            });
                        }
                    }
                    Ok(reply) => {
                        tracing::debug!(
                            ?reply,
                            "queued message had already left the queue before removal"
                        );
                        if selected_matches {
                            composer.failure =
                                Some("That message had already left the queue".into());
                            composer
                                .state
                                .update(cx, |state, cx| state.refresh_selected_queue(cx));
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "host-authoritative queue removal failed");
                        if selected_matches {
                            composer.failure = Some("Couldn't remove the message".into());
                            composer
                                .state
                                .update(cx, |state, cx| state.refresh_selected_queue(cx));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Send one now: the host stops the turn and hands this message over. Not
    /// optimistic — the row leaves the queue when the host has actually taken
    /// it, so a failed interrupt doesn't lose the text.
    pub(crate) fn send_queued_now(&mut self, id: String, cx: &mut Context<Self>) {
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.clear_queue_edit(cx);
        }
        self.queue_rpc(
            methods::SEND_QUEUED_MESSAGE_NOW,
            serde_json::json!({ "id": id }),
            "Couldn't send that message",
            cx,
        );
    }

    /// Steer one row into the current turn without stopping it. If the turn
    /// ends during the click, the engine sends it as the next turn instead.
    pub(crate) fn steer_queued_now(&mut self, id: String, cx: &mut Context<Self>) {
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.clear_queue_edit(cx);
        }
        self.queue_rpc(
            methods::STEER_QUEUED_MESSAGE_NOW,
            serde_json::json!({ "id": id }),
            "Couldn't steer that message",
            cx,
        );
    }

    /// Execute the same resolved action advertised on the row. Both pointer
    /// clicks and the empty-composer Enter gesture come through here.
    fn activate_queued_primary(
        &mut self,
        id: String,
        action: QueuePrimaryAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            QueuePrimaryAction::Steer => self.steer_queued_now(id, cx),
            QueuePrimaryAction::SendNow => self.send_queued_now(id, cx),
        }
    }

    /// Cmd/Ctrl+Enter on an empty composer activates the same action shown on
    /// the first queued row: non-interrupting Steer when possible, Send now
    /// otherwise. An edit/review gate or an old chat host makes it a no-op.
    pub(crate) fn queue_pop_head(&mut self, cx: &mut Context<Self>) {
        if self.editing_queued.is_some() {
            return;
        }
        let (id, has_attachments, delivery_blocked, host_supports_actions) = {
            let state = self.state.read(cx);
            let Some(chat_id) = state.selected_chat.as_deref() else {
                return;
            };
            let Some(item) = state.queue.first() else {
                return;
            };
            (
                item.id.clone(),
                !item.attachments.is_empty(),
                item.delivery_gate.is_some(),
                state.chat_host_supports(
                    chat_id,
                    zeron_proto::capabilities::MESSAGE_QUEUE_ACTIONS_V1,
                ),
            )
        };
        let Some(action) = available_queue_primary_action(
            self.pickers().read(cx).resolved_mid_turn_steering(cx),
            has_attachments,
            delivery_blocked,
            host_supports_actions,
        ) else {
            return;
        };
        self.activate_queued_primary(id, action, cx);
    }

    /// Borrow the composer while the leased row reserves its queue position.
    pub(crate) fn begin_queue_edit(&mut self, id: String, cx: &mut Context<Self>) {
        if self.queue_edit_pending_id.is_some()
            || self.queue_edit_finishing
            || self.editing_queued.is_some()
            || !self.can_edit_queue_in_composer()
        {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (chat_id, host_device_id, supported) = {
            let state = self.state.read(cx);
            let Some(chat_id) = state.selected_chat.clone() else {
                return;
            };
            let Some(host_device_id) = state.selected_chat_row().map(|chat| chat.device_id.clone())
            else {
                return;
            };
            let capability = zeron_proto::capabilities::MESSAGE_QUEUE_EDIT_LEASE_V1;
            let supported = engine.engine_info().supports(capability)
                && state.chat_host_supports(&chat_id, capability);
            (chat_id, host_device_id, supported)
        };
        if !supported {
            self.failure = Some("Update the chat host to edit queued messages safely".into());
            cx.notify();
            return;
        }
        if !self.state.read(cx).queue.iter().any(|item| item.id == id) {
            return;
        }
        let owner_device_id = engine.engine_info().device_id.clone();
        let instance_id = self.queue_edit_instance_id.clone();
        self.queue_edit_pending_id = Some(id.clone());
        self.queue_drag = None;
        cx.notify();
        let params = serde_json::json!({
            "chatId": chat_id,
            "id": id,
            "editorDeviceId": owner_device_id,
            "editorInstanceId": instance_id,
            "targetDeviceId": host_device_id,
        });
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::BEGIN_QUEUED_MESSAGE_EDIT, params)
                .await;
            let mut loaded_attachments = Vec::new();
            if let Ok(reply) = &result
                && reply.get("outcome").and_then(|v| v.as_str()) == Some("acquired")
            {
                let paths = reply.get("attachments")
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok());
                let mut load_failed = paths.is_none();
                for path in paths.unwrap_or_default() {
                    let loaded = crate::attachments::read_attachment_image(
                        &engine, cx.background_executor(), Some(&host_device_id), &path,
                    ).await;
                    match loaded {
                        Some(loaded) => loaded_attachments.push(crate::attachments::StagedAttachment {
                            id: uuid::Uuid::new_v4().to_string(), name: loaded.name, image: loaded.image,
                        }),
                        None => { load_failed = true; break; }
                    }
                }
                if load_failed {
                    let _ = engine.client().call(methods::FINISH_QUEUED_MESSAGE_EDIT, serde_json::json!({
                        "chatId": chat_id, "id": id, "leaseId": reply.get("leaseId"),
                        "action": "cancel", "targetDeviceId": host_device_id,
                    })).await;
                    this.update(cx, |composer, cx| {
                        composer.queue_edit_pending_id = None;
                        composer.failure = Some("Couldn't load the queued attachments. Check the connection and update the chat host.".into());
                        cx.notify();
                    }).ok();
                    return;
                }
            }
            this.update(cx, |composer, cx| {
                composer.queue_edit_pending_id = None;
                match result {
                    Ok(reply)
                        if reply.get("outcome").and_then(|v| v.as_str()) == Some("acquired") =>
                    {
                        let Some(lease_id) = reply.get("leaseId").and_then(|v| v.as_str()) else {
                            composer.failure =
                                Some("The chat host returned an invalid edit lease".into());
                            cx.notify();
                            return;
                        };
                        let Some(base_text_hash) =
                            reply.get("baseTextHash").and_then(|v| v.as_str())
                        else {
                            composer.failure =
                                Some("The chat host returned an invalid edit lease".into());
                            cx.notify();
                            return;
                        };
                        let raw_text = reply
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let attachments: Vec<String> = serde_json::from_value(reply["attachments"].clone()).unwrap_or_default();
                        let text = queue_visible_text(&raw_text, &attachments);
                        let text = if !attachments.is_empty() && text == crate::attachments::ATTACHMENT_ONLY_TEXT {
                            String::new()
                        } else { text };
                        let selected_matches = composer.state.read(cx).selected_chat.as_deref()
                            == Some(chat_id.as_str());
                        if !selected_matches || !composer.can_edit_queue_in_composer() {
                            // Navigation or another composer action won acquisition. Release
                            // immediately; the expiry/review path is the backup.
                            let params = serde_json::json!({
                                "chatId": chat_id,
                                "id": id,
                                "leaseId": lease_id,
                                "action": "cancel",
                                "targetDeviceId": host_device_id,
                            });
                            let engine = engine.clone();
                            cx.spawn(async move |_, _| {
                                let _ = engine
                                    .client()
                                    .call(methods::FINISH_QUEUED_MESSAGE_EDIT, params)
                                    .await;
                            })
                            .detach();
                            return;
                        }
                        composer.editing_queued = Some(id.clone());
                        composer.queue_edit_lease_id = Some(lease_id.to_string());
                        composer.queue_edit_base_text_hash = Some(base_text_hash.to_string());
                        composer.queue_edit_chat_id = Some(chat_id.clone());
                        composer.queue_edit_host_device_id = Some(host_device_id.clone());
                        composer.queue_edit_draft = Some((
                            composer.input.read(cx).text().to_string(),
                            composer.attachments.remove(&composer.current_key).unwrap_or_default(),
                        ));
                        composer.attachments.insert(composer.current_key.clone(), loaded_attachments);
                        composer.queue_edit_focus_pending = true;
                        composer.input.update(cx, |input, cx| input.set_text(text, cx));
                        composer.start_queue_edit_renewal(engine.clone(), cx);
                    }
                    Ok(reply)
                        if reply.get("outcome").and_then(|v| v.as_str()) == Some("locked") =>
                    {
                        composer.failure =
                            Some("That queued message is being edited on another device".into());
                    }
                    Ok(_) => {
                        composer.failure =
                            Some("That queued message is no longer available".into());
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "begin queue edit failed");
                        composer.failure =
                            Some("Connect to the chat host to edit this message".into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.queue_edit_task = Some(task);
    }

    /// Save the composer into the existing row, including its attachments.
    /// An entirely empty composer removes the row.
    pub(crate) fn commit_queue_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.editing_queued.is_none() {
            return false;
        }
        let text = self.input.read(cx).text().trim().to_string();
        if text.is_empty() && self.staged().is_empty() {
            self.finish_queue_edit("discard", None, cx);
        } else {
            self.finish_queue_edit("commit", Some(text), cx);
        }
        true
    }

    /// Escape out of an edit, leaving the row as it was.
    pub(crate) fn cancel_queue_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.editing_queued.is_none() {
            return false;
        }
        self.finish_queue_edit("cancel", None, cx);
        true
    }

    pub(crate) fn clear_queue_edit(&mut self, cx: &mut Context<Self>) {
        self.release_queue_edit_best_effort(cx);
        self.clear_queue_edit_local(cx);
    }

    fn clear_queue_edit_local(&mut self, cx: &mut Context<Self>) {
        self.editing_queued = None;
        self.queue_edit_lease_id = None;
        self.queue_edit_base_text_hash = None;
        self.queue_edit_chat_id = None;
        self.queue_edit_host_device_id = None;
        self.queue_edit_pending_id = None;
        self.queue_edit_finishing = false;
        self.input.update(cx, |input, cx| {
            input.read_only = false;
            cx.notify();
        });
        self.queue_edit_task = None;
        self.queue_edit_renew_task = None;
        if let Some((text, attachments)) = self.queue_edit_draft.take() {
            self.input.update(cx, |input, cx| input.set_text(text, cx));
            self.attachments
                .insert(self.current_key.clone(), attachments);
        }
        self.queue_edit_focus_pending = true;
        cx.notify();
    }

    fn finish_queue_edit(
        &mut self,
        action: &'static str,
        text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.queue_edit_finishing {
            return;
        }
        let (Some(id), Some(lease_id), Some(chat_id), Some(host_device_id), Some(engine)) = (
            self.editing_queued.clone(),
            self.queue_edit_lease_id.clone(),
            self.queue_edit_chat_id.clone(),
            self.queue_edit_host_device_id.clone(),
            self.state.read(cx).engine().cloned(),
        ) else {
            self.failure = Some("The edit lease was lost; your text is still in the editor".into());
            cx.notify();
            return;
        };
        let expected = self.queue_edit_base_text_hash.clone();
        let staged = self.staged().to_vec();
        let mut params = serde_json::json!({
            "chatId": chat_id,
            "id": id,
            "leaseId": lease_id,
            "action": action,
            "text": text,
            "expectedTextHash": expected,
            "targetDeviceId": host_device_id,
        });
        self.queue_edit_finishing = true;
        self.input.update(cx, |input, cx| {
            input.read_only = true;
            cx.notify();
        });
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = async {
                if action == "commit" {
                    let mut paths = Vec::new();
                    for attachment in &staged {
                        let path = crate::attachments::upload_attachment(
                            &engine, cx.background_executor(), Some(&host_device_id),
                            &uuid::Uuid::new_v4().to_string(), attachment, None,
                        ).await.map_err(|err| err.to_string())?;
                        paths.push(path);
                    }
                    if params["text"].as_str().is_some_and(|text| text.trim().is_empty()) && !paths.is_empty() {
                        params["text"] = crate::attachments::ATTACHMENT_ONLY_TEXT.into();
                    }
                    params["attachments"] = serde_json::json!(paths);
                }
                crate::attachments::call_with_timeout(
                    &engine, cx.background_executor(), methods::FINISH_QUEUED_MESSAGE_EDIT,
                    params, std::time::Duration::from_secs(30),
                ).await.map_err(|err| err.to_string())
            }.await;
            this.update(cx, |composer, cx| {
                composer.queue_edit_finishing = false;
                composer.input.update(cx, |input, cx| { input.read_only = false; cx.notify(); });
                match result {
                    Ok(reply) => match reply.get("outcome").and_then(|v| v.as_str()) {
                        Some("committed" | "cancelled" | "discarded" | "released") => {
                            composer.clear_queue_edit_local(cx);
                            return;
                        }
                        Some("conflict") => {
                            composer.failure = Some(
                                "This message changed on another device; your edit was kept locally".into(),
                            );
                        }
                        Some("missing") => {
                            composer.failure = Some(
                                "The queued message was removed; your edit was kept locally".into(),
                            );
                        }
                        _ => {
                            composer.failure = Some(
                                "The edit lease changed; your text is still in the editor".into(),
                            );
                        }
                    },
                    Err(err) => {
                        tracing::warn!(error = %err, "finish queue edit failed");
                        composer.failure = Some(
                            "Couldn't reach the chat host; your edit is still in the editor".into(),
                        );
                    }
                }
                cx.notify();
            }).ok();
        });
        self.queue_edit_task = Some(task);
    }

    fn start_queue_edit_renewal(
        &mut self,
        engine: crate::state::EngineHandle,
        cx: &mut Context<Self>,
    ) {
        let (Some(id), Some(lease_id), Some(chat_id), Some(host_device_id)) = (
            self.editing_queued.clone(),
            self.queue_edit_lease_id.clone(),
            self.queue_edit_chat_id.clone(),
            self.queue_edit_host_device_id.clone(),
        ) else {
            return;
        };
        self.queue_edit_renew_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(20))
                    .await;
                let params = serde_json::json!({
                    "chatId": chat_id,
                    "id": id,
                    "leaseId": lease_id,
                    "targetDeviceId": host_device_id,
                });
                match engine
                    .client()
                    .call(methods::RENEW_QUEUED_MESSAGE_EDIT, params)
                    .await
                {
                    Ok(reply)
                        if reply.get("outcome").and_then(|v| v.as_str()) == Some("renewed") => {}
                    Ok(_) => {
                        this.update(cx, |composer, cx| {
                            composer.failure = Some(
                                "Edit protection expired; review this message before sending"
                                    .into(),
                            );
                            cx.notify();
                        })
                        .ok();
                        break;
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "queue edit heartbeat failed");
                        // A transient miss is tolerated by the 60s lease. Keep
                        // trying; the host fails closed if all attempts miss.
                    }
                }
            }
        }));
    }

    fn release_queue_edit_best_effort(&self, cx: &mut Context<Self>) {
        let (Some(id), Some(lease_id), Some(chat_id), Some(host_device_id), Some(engine)) = (
            self.editing_queued.clone(),
            self.queue_edit_lease_id.clone(),
            self.queue_edit_chat_id.clone(),
            self.queue_edit_host_device_id.clone(),
            self.state.read(cx).engine().cloned(),
        ) else {
            return;
        };
        let params = serde_json::json!({
            "chatId": chat_id,
            "id": id,
            "leaseId": lease_id,
            "action": "cancel",
            "targetDeviceId": host_device_id,
        });
        cx.spawn(async move |_, _| {
            let _ = engine
                .client()
                .call(methods::FINISH_QUEUED_MESSAGE_EDIT, params)
                .await;
        })
        .detach();
    }

    /// Fire one queue mutation at the chat's doc host.
    fn queue_rpc(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        failure: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (chat_id, host_device_id, host_supports_action) = {
            let state = self.state.read(cx);
            let Some(chat_id) = state.selected_chat.clone() else {
                return;
            };
            let host = queue_action_needs_host(method)
                .then(|| state.selected_chat_row().map(|chat| chat.device_id.clone()))
                .flatten();
            let supported = !queue_action_needs_host(method)
                || state.chat_host_supports(
                    &chat_id,
                    zeron_proto::capabilities::MESSAGE_QUEUE_ACTIONS_V1,
                );
            (chat_id, host, supported)
        };
        if !host_supports_action {
            self.failure = Some("The chat host does not support queue actions".into());
            cx.notify();
            return;
        }
        let mut params = params;
        if let Some(object) = params.as_object_mut() {
            object.insert("chatId".into(), serde_json::Value::String(chat_id));
            if let Some(host) = host_device_id {
                object.insert("targetDeviceId".into(), serde_json::Value::String(host));
            }
        }
        // Detached, not held: these are independent one-shot mutations, and
        // parking them in a single slot meant the next arrow tap dropped — and
        // so cancelled — the move still in flight, leaving the optimistic list
        // showing an order the doc never got.
        cx.spawn(
            async move |this, cx| match engine.client().call(method, params).await {
                Ok(reply) if queue_mutation_acknowledged(method, &reply) => {}
                Ok(reply) => {
                    tracing::debug!(
                        method,
                        ?reply,
                        "queue mutation was not applied; reconciling"
                    );
                    this.update(cx, |composer, cx| {
                        composer
                            .state
                            .update(cx, |state, cx| state.refresh_selected_queue(cx));
                    })
                    .ok();
                }
                Err(err) => {
                    tracing::warn!(method, error = %err, "queue mutation failed");
                    this.update(cx, |composer, cx| {
                        composer.failure = Some(failure.into());
                        composer
                            .state
                            .update(cx, |state, cx| state.refresh_selected_queue(cx));
                        cx.notify();
                    })
                    .ok();
                }
            },
        )
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use zeron_rpc::methods;

    use super::{
        PANEL_PAD_TOP, QueuePrimaryAction, ROW_SLOT, available_queue_primary_action, one_line,
        queue_action_needs_host, queue_drag_offsets, queue_drop_index, queue_head_shortcut_visible,
        queue_mutation_acknowledged, queue_primary_action, queue_visible_text,
    };

    #[test]
    fn primary_action_only_promises_steer_when_the_row_can_use_it() {
        assert_eq!(
            queue_primary_action(Some(true), false),
            Some(QueuePrimaryAction::Steer)
        );
        assert_eq!(
            queue_primary_action(Some(false), false),
            Some(QueuePrimaryAction::SendNow)
        );
        assert_eq!(queue_primary_action(None, false), None);
        assert_eq!(
            queue_primary_action(Some(true), true),
            Some(QueuePrimaryAction::SendNow)
        );
    }

    #[test]
    fn available_primary_action_obeys_row_and_host_gates() {
        assert_eq!(
            available_queue_primary_action(Some(true), false, false, true),
            Some(QueuePrimaryAction::Steer)
        );
        assert_eq!(
            available_queue_primary_action(Some(false), false, false, true),
            Some(QueuePrimaryAction::SendNow)
        );
        assert_eq!(
            available_queue_primary_action(Some(true), false, true, true),
            None
        );
        assert_eq!(
            available_queue_primary_action(Some(true), false, false, false),
            None
        );
    }

    #[test]
    fn queue_shortcut_only_appears_on_an_actionable_head_when_revealed() {
        assert!(queue_head_shortcut_visible(0, true, true));
        assert!(!queue_head_shortcut_visible(1, true, true));
        assert!(!queue_head_shortcut_visible(0, false, true));
        assert!(!queue_head_shortcut_visible(0, true, false));
    }

    #[test]
    fn host_authoritative_queue_actions_route_to_the_host() {
        assert!(queue_action_needs_host(methods::SEND_QUEUED_MESSAGE_NOW));
        assert!(queue_action_needs_host(methods::STEER_QUEUED_MESSAGE_NOW));
        assert!(queue_action_needs_host(methods::REMOVE_QUEUED_MESSAGE));
        assert!(queue_action_needs_host(methods::BEGIN_QUEUED_MESSAGE_EDIT));
        assert!(queue_action_needs_host(methods::RENEW_QUEUED_MESSAGE_EDIT));
        assert!(queue_action_needs_host(methods::FINISH_QUEUED_MESSAGE_EDIT));
        assert!(!queue_action_needs_host(methods::QUEUE_MESSAGE));
        assert!(!queue_action_needs_host(methods::UPDATE_QUEUED_MESSAGE));
        assert!(!queue_action_needs_host(methods::MOVE_QUEUED_MESSAGE));
    }

    #[test]
    fn mutation_acknowledgements_detect_conflicts_and_malformed_replies() {
        assert!(queue_mutation_acknowledged(
            methods::MOVE_QUEUED_MESSAGE,
            &serde_json::json!({ "changed": true })
        ));
        assert!(!queue_mutation_acknowledged(
            methods::MOVE_QUEUED_MESSAGE,
            &serde_json::json!({ "changed": false })
        ));
        assert!(!queue_mutation_acknowledged(
            methods::REMOVE_QUEUED_MESSAGE,
            &serde_json::json!({})
        ));
        assert!(queue_mutation_acknowledged(
            methods::SEND_QUEUED_MESSAGE_NOW,
            &serde_json::json!({ "sent": true })
        ));
    }

    #[test]
    fn the_whole_panel_maps_to_a_clamped_queue_drop_slot() {
        assert_eq!(queue_drop_index(0.0, 2), 0, "top pad targets the head");
        assert_eq!(queue_drop_index(PANEL_PAD_TOP + ROW_SLOT - 0.1, 2), 0);
        assert_eq!(queue_drop_index(PANEL_PAD_TOP + ROW_SLOT, 2), 1);
        assert_eq!(queue_drop_index(10_000.0, 2), 1);
    }

    #[test]
    fn drag_offsets_move_the_real_row_and_open_its_destination() {
        assert_eq!(queue_drag_offsets(0, 0, 0, 2), (0.0, 2.0 * ROW_SLOT));
        assert_eq!(queue_drag_offsets(1, 0, 0, 2), (0.0, -ROW_SLOT));
        assert_eq!(queue_drag_offsets(2, 0, 0, 2), (0.0, -ROW_SLOT));

        // Moving the pointer back one slot restarts only the rows whose
        // visual destination actually changed.
        assert_eq!(queue_drag_offsets(0, 0, 2, 1), (2.0 * ROW_SLOT, ROW_SLOT));
        assert_eq!(queue_drag_offsets(1, 0, 2, 1), (-ROW_SLOT, -ROW_SLOT));
        assert_eq!(queue_drag_offsets(2, 0, 2, 1), (-ROW_SLOT, 0.0));
    }

    /// A row is one line tall, so a multi-line message has to read as one line
    /// — otherwise the panel's rows stop lining up.
    #[test]
    fn rows_flatten_multi_line_messages() {
        assert_eq!(
            one_line("fix the test\n\nthen ship it").as_ref(),
            "fix the test then ship it"
        );
        assert_eq!(one_line("  spaced   out  ").as_ref(), "spaced out");
    }

    #[test]
    fn legacy_attachment_trailers_are_hidden_from_queue_text() {
        let paths = vec!["/tmp/image.png".to_string()];
        let legacy = crate::attachments::with_attachments("inspect this", &paths);
        assert_eq!(queue_visible_text(&legacy, &paths), "inspect this");

        let image_only = crate::attachments::with_attachments("", &paths);
        assert_eq!(
            queue_visible_text(&image_only, &paths),
            crate::attachments::ATTACHMENT_ONLY_TEXT
        );
        assert_eq!(
            queue_visible_text("literal user text", &paths),
            "literal user text"
        );
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;
    use gpui::{AppContext, ScrollHandle, TestAppContext, point};

    struct QueueScrollTestView {
        queue: ScrollHandle,
        transcript: ScrollHandle,
        count: usize,
    }

    impl Render for QueueScrollTestView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(
                    div()
                        .id("transcript-underlay")
                        .absolute()
                        .inset_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.transcript)
                        .child(div().h(px(2000.0))),
                )
                .child(
                    div().absolute().bottom_0().w_full().child(
                        queue_panel_surface(Theme::of(cx)).child(queue_rows(
                            &self.queue,
                            px(180.0),
                            (0..self.count)
                                .map(|_| div().h(px(ROW_HEIGHT)).flex_none().into_any_element()),
                        )),
                    ),
                )
        }
    }

    #[gpui::test]
    fn queue_wheel_does_not_scroll_the_transcript_even_at_its_boundaries(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let (view, cx) = cx.add_window_view(|_, _| QueueScrollTestView {
            queue: ScrollHandle::new(),
            transcript: ScrollHandle::new(),
            count: 24,
        });
        cx.simulate_resize(gpui::size(px(400.0), px(400.0)));
        cx.run_until_parked();
        let (queue, transcript) =
            view.read_with(cx, |view, _| (view.queue.clone(), view.transcript.clone()));
        for delta in [-80.0, -10_000.0, -80.0, 10_000.0, 80.0] {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: point(px(200.0), px(300.0)),
                delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(delta))),
                ..Default::default()
            });
            cx.run_until_parked();
            assert_eq!(
                transcript.offset().y,
                px(0.0),
                "queue wheel leaked to transcript"
            );
            if delta == -80.0 {
                assert!(queue.offset().y < px(0.0), "the queue must still scroll");
            }
        }
        view.update(cx, |view, cx| {
            view.count = 2;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: point(px(200.0), px(370.0)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-80.0))),
            ..Default::default()
        });
        cx.run_until_parked();
        assert_eq!(transcript.offset().y, px(0.0));
        assert_eq!(queue.max_offset().y, px(0.0));
    }
}
