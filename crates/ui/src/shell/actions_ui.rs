use super::*;

use zeron_proto::{
    ProjectAction, ProjectActionDraft, ProjectActionIcon, ProjectActionRun, ProjectActionsSnapshot,
};

use crate::project_actions::{
    ACTION_ICONS, ProjectActionEditor, ProjectActionsKey, ProjectActionsStatus, action_icon,
    draft_from_action, preferred_action, show_action_label,
};

#[derive(Clone)]
struct ProjectActionContext {
    key: ProjectActionsKey,
    chat_id: String,
    target_device_id: Option<String>,
}

impl Shell {
    pub(super) fn attach_worktree_setup(
        &mut self,
        chat_id: String,
        setup_action: Option<ProjectActionRun>,
        setup_error: Option<String>,
        target_device_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(error) = setup_error {
            self.sidebar_notice = Some(format!("Setup action failed: {error}").into());
        }
        let Some(run) = setup_action else {
            cx.notify();
            return;
        };

        let panel = self.terminal_panel(cx);
        let title = format!("{} (setup)", run.action_name);
        let tab = panel.update(cx, |panel, cx| {
            panel.reserve_tab_for_chat(chat_id.clone(), title, cx)
        });
        let attached = panel.update(cx, |panel, cx| {
            panel.attach_reserved_session(&chat_id, tab, run.terminal, target_device_id, cx)
        });
        if !attached {
            self.sidebar_notice =
                Some("Setup action started, but its terminal could not be attached".into());
        }

        let selected = self.active_chat == chat_id;
        self.panels
            .update(&chat_id, |panels| panels.terminal_open = true);
        if selected {
            self.terminal_tween = None;
            self.terminal_tween_task = None;
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
            panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
        }
        cx.notify();
    }

    fn project_action_context(&self, cx: &App) -> Option<ProjectActionContext> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let space_id = chat.space_id.clone()?;
        let space = state.space_row(&space_id)?;
        if space.device_id != chat.device_id {
            return None;
        }
        let target_device_id = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        Some(ProjectActionContext {
            key: ProjectActionsKey {
                device_id: chat.device_id.clone(),
                space_id,
            },
            chat_id: chat.id.clone(),
            target_device_id,
        })
    }

    pub(super) fn ensure_project_actions(&mut self, cx: &mut Context<Self>) {
        let context = self.project_action_context(cx);
        let key = context.as_ref().map(|context| context.key.clone());
        let changed = self.project_actions.activate(key.clone());
        let needs_load = key.as_ref().is_some_and(|key| {
            !self.project_actions.cache.contains_key(key)
                || matches!(
                    self.project_actions.cache.get(key),
                    Some(ProjectActionsStatus::Idle)
                )
        });
        if (changed || needs_load)
            && let Some(context) = context
        {
            self.refresh_project_actions(context, cx);
        }
    }

    fn refresh_project_actions(&mut self, context: ProjectActionContext, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.project_actions
                .mark_unavailable(&context.key, "Engine not connected".into());
            cx.notify();
            return;
        };
        let generation = self.project_actions.begin_load(&context.key);
        let params = project_action_params(
            serde_json::json!({ "spaceId": context.key.space_id }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        self.project_actions.request_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionsSnapshot>(methods::LIST_PROJECT_ACTIONS, params)
                .await
                .map_err(|err| err.to_string());
            this.update(cx, |shell, cx| {
                if shell.project_actions.accept_load(&key, generation, result) {
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn close_project_actions_menu(&mut self, cx: &mut Context<Self>) {
        if self.project_actions.menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Shell| &mut shell.project_actions.menu);
        }
        cx.notify();
    }

    fn toggle_project_actions_menu(&mut self, cx: &mut Context<Self>) {
        if self.project_actions.menu.take_press_was_open() {
            self.close_project_actions_menu(cx);
            return;
        }
        self.project_actions.menu.open(());
        if let Some(context) = self.project_action_context(cx) {
            self.refresh_project_actions(context, cx);
        }
        cx.notify();
    }

    fn open_project_action_editor(
        &mut self,
        action: Option<ProjectAction>,
        import: Option<ProjectActionDraft>,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.project_actions.active.clone() else {
            return;
        };
        self.close_project_actions_menu(cx);
        let action_id = action.as_ref().map(|action| action.id.clone());
        let draft =
            action
                .as_ref()
                .map(draft_from_action)
                .or(import)
                .unwrap_or(ProjectActionDraft {
                    name: String::new(),
                    command: String::new(),
                    icon: ProjectActionIcon::Play,
                    run_on_worktree_create: false,
                });
        let name = cx.new(|cx| ComposerInput::new("Action name", cx));
        name.update(cx, |input, cx| input.set_text(draft.name, cx));
        let command = cx.new(|cx| ComposerInput::new("Command", cx));
        command.update(cx, |input, cx| input.set_text(draft.command, cx));
        let name_events = cx.subscribe(&name, |_: &mut Shell, _, _, cx| cx.notify());
        let command_events = cx.subscribe(&command, |_: &mut Shell, _, _, cx| cx.notify());
        self.project_actions.editor = Some(ProjectActionEditor {
            key,
            action_id,
            name,
            command,
            icon: draft.icon,
            run_on_worktree_create: draft.run_on_worktree_create,
            error: None,
            saving: false,
            focus_pending: true,
            confirm_delete: false,
            _name_events: name_events,
            _command_events: command_events,
        });
        cx.notify();
    }

    fn save_project_action(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.project_action_context(cx) else {
            if let Some(editor) = self.project_actions.editor.as_mut() {
                editor.error = Some("Project action is no longer available".into());
                cx.notify();
            }
            return;
        };
        let Some(editor) = self.project_actions.editor.as_mut() else {
            return;
        };
        if editor.saving {
            return;
        }
        let name = editor.name.read(cx).text().trim().to_string();
        let command = editor.command.read(cx).text().trim().to_string();
        let error = if name.is_empty() {
            Some("Action name is required".to_string())
        } else if name.chars().count() > 80 {
            Some("Action name must not exceed 80 characters".to_string())
        } else if command.is_empty() {
            Some("Action command is required".to_string())
        } else if command.len() > 16 * 1024 {
            Some("Action command must not exceed 16384 bytes".to_string())
        } else {
            None
        };
        if let Some(error) = error {
            editor.error = Some(error);
            cx.notify();
            return;
        }
        if context.key != editor.key {
            editor.error = Some("The selected project changed".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            editor.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let draft = ProjectActionDraft {
            name,
            command,
            icon: editor.icon,
            run_on_worktree_create: editor.run_on_worktree_create,
        };
        let action_id = editor.action_id.clone();
        editor.saving = true;
        editor.error = None;
        if let Some(snapshot) = self.project_actions.active_snapshot().cloned() {
            self.project_actions
                .cache
                .insert(context.key.clone(), ProjectActionsStatus::Saving(snapshot));
        }
        let params = project_action_params(
            serde_json::json!({
                "spaceId": context.key.space_id,
                "actionId": action_id,
                "action": draft,
            }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        let mutation_generation = self.project_actions.begin_mutation();
        self.project_actions.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionsSnapshot>(methods::UPSERT_PROJECT_ACTION, params)
                .await;
            this.update(cx, |shell, cx| {
                if !shell
                    .project_actions
                    .is_current_mutation(&key, mutation_generation)
                {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        shell
                            .project_actions
                            .cache
                            .insert(key.clone(), ProjectActionsStatus::Ready(snapshot));
                        if shell
                            .project_actions
                            .editor
                            .as_ref()
                            .is_some_and(|editor| editor.key == key && editor.saving)
                        {
                            shell.project_actions.editor = None;
                        }
                    }
                    Err(err) => {
                        let message = err.to_string();
                        shell
                            .project_actions
                            .mark_unavailable(&key, message.clone());
                        if let Some(editor) = shell
                            .project_actions
                            .editor
                            .as_mut()
                            .filter(|editor| editor.key == key && editor.saving)
                        {
                            editor.saving = false;
                            editor.error = Some(message);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn delete_project_action(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.project_action_context(cx) else {
            return;
        };
        let Some(editor) = self.project_actions.editor.as_mut() else {
            return;
        };
        let Some(action_id) = editor.action_id.clone() else {
            self.project_actions.editor = None;
            cx.notify();
            return;
        };
        if editor.saving {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        editor.saving = true;
        let params = project_action_params(
            serde_json::json!({
                "spaceId": context.key.space_id,
                "actionId": action_id,
            }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        let mutation_generation = self.project_actions.begin_mutation();
        self.project_actions.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionsSnapshot>(methods::DELETE_PROJECT_ACTION, params)
                .await;
            this.update(cx, |shell, cx| {
                if !shell
                    .project_actions
                    .is_current_mutation(&key, mutation_generation)
                {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        shell
                            .project_actions
                            .cache
                            .insert(key.clone(), ProjectActionsStatus::Ready(snapshot));
                        if shell
                            .settings
                            .last_project_action_by_space_id
                            .get(&key.space_id)
                            == Some(&action_id)
                        {
                            shell
                                .settings
                                .last_project_action_by_space_id
                                .remove(&key.space_id);
                            shell.schedule_save(cx);
                        }
                        if shell
                            .project_actions
                            .editor
                            .as_ref()
                            .is_some_and(|editor| editor.key == key && editor.saving)
                        {
                            shell.project_actions.editor = None;
                        }
                    }
                    Err(err) => {
                        let message = err.to_string();
                        shell
                            .project_actions
                            .mark_unavailable(&key, message.clone());
                        if let Some(editor) = shell
                            .project_actions
                            .editor
                            .as_mut()
                            .filter(|editor| editor.key == key && editor.saving)
                        {
                            editor.saving = false;
                            editor.confirm_delete = false;
                            editor.error = Some(message);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn run_project_action(&mut self, action: ProjectAction, cx: &mut Context<Self>) {
        let Some(context) = self.project_action_context(cx) else {
            return;
        };
        if !self
            .project_actions
            .active_status()
            .is_some_and(ProjectActionsStatus::can_run)
        {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.close_project_actions_menu(cx);

        let panel = self.terminal_panel(cx);
        let tab = panel.update(cx, |panel, cx| {
            panel.reserve_tab_for_chat(context.chat_id.clone(), action.name.clone(), cx)
        });
        self.panels
            .update(&context.chat_id, |panels| panels.terminal_open = true);
        self.terminal_tween = None;
        self.terminal_tween_task = None;
        panel.update(cx, |panel, cx| {
            panel.set_open(true, cx);
            panel.select_tab_by_key(tab, cx);
        });

        let params = project_action_params(
            serde_json::json!({
                "spaceId": context.key.space_id,
                "chatId": context.chat_id,
                "actionId": action.id,
                "cols": 80,
                "rows": 24,
            }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        let chat_id = context.chat_id.clone();
        let target = context.target_device_id.clone();
        let action_id = action.id.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionRun>(methods::RUN_PROJECT_ACTION, params)
                .await;
            match result {
                Ok(run) => {
                    let terminal_id = run.terminal.id.clone();
                    let attached = panel.update(cx, |panel, cx| {
                        panel.attach_reserved_session(
                            &chat_id,
                            tab,
                            run.terminal,
                            target.clone(),
                            cx,
                        )
                    });
                    if !attached {
                        let _ = engine
                            .client()
                            .call(
                                methods::CLOSE_TERMINAL,
                                project_action_params(
                                    serde_json::json!({ "terminalId": terminal_id }),
                                    &target,
                                ),
                            )
                            .await;
                    }
                    this.update(cx, |shell, cx| {
                        shell
                            .settings
                            .last_project_action_by_space_id
                            .insert(key.space_id, action_id);
                        shell.schedule_save(cx);
                        cx.notify();
                    })
                    .ok();
                }
                Err(err) => {
                    let message = err.to_string();
                    panel.update(cx, |panel, cx| {
                        panel.fail_reserved_tab(&chat_id, tab, &message, cx)
                    });
                    this.update(cx, |shell, cx| {
                        shell.project_actions.mark_unavailable(&key, message);
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    pub(super) fn render_project_actions_control(
        &mut self,
        available_titlebar_width: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.ensure_project_actions(cx);
        let status = self.project_actions.active_status()?.clone();
        if matches!(
            status,
            ProjectActionsStatus::Idle
                | ProjectActionsStatus::Loading
                | ProjectActionsStatus::Unsupported
        ) {
            return None;
        }
        let snapshot = self.project_actions.visible_snapshot()?;
        let can_run = status.can_run();
        let unavailable = matches!(status, ProjectActionsStatus::Unavailable { .. });
        let theme = Theme::of(cx).clone();
        let preferred = preferred_action(
            &snapshot.actions,
            self.settings
                .last_project_action_by_space_id
                .get(&snapshot.space_id)
                .map(String::as_str),
        )
        .cloned();
        let has_imports = !snapshot.importable_actions.is_empty();
        let has_actions = !snapshot.actions.is_empty();
        let show_label = show_action_label(available_titlebar_width);
        let menu_mounted = self.project_actions.menu.get().is_some();
        let menu_closing = self.project_actions.menu.closing_since();

        let mut control = div()
            .relative()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .h(px(24.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .occlude();

        if let Some(action) = preferred.clone() {
            let run_action = action.clone();
            let main = action_segment(&theme, "project-action-main")
                .when(!can_run, |el| el.opacity(0.45))
                .when(can_run, |el| {
                    el.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.run_project_action(run_action.clone(), cx)
                        }))
                })
                .child(
                    icon(action_icon(action.icon))
                        .size(px(13.0))
                        .text_color(theme.text_muted),
                )
                .when(show_label, |el| {
                    el.child(
                        div()
                            .max_w(px(150.0))
                            .truncate()
                            .child(SharedString::from(action.name)),
                    )
                });
            control = control.child(main).child(
                action_chevron(
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_project_actions_menu(cx)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.project_actions.menu.note_trigger_press();
                    }),
                ),
            );
        } else if unavailable {
            let retry = action_segment(&theme, "project-actions-unavailable")
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.project_actions.menu.note_trigger_press();
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_project_actions_menu(cx)))
                .child(
                    icon(icons::DANGER_TRIANGLE)
                        .size(px(13.0))
                        .text_color(theme.danger),
                )
                .when(show_label, |el| {
                    el.child(SharedString::from("Actions unavailable"))
                });
            control = control.child(retry);
        } else {
            let add = action_segment(&theme, "project-action-add")
                .cursor_pointer()
                .on_click(
                    cx.listener(|this, _, _, cx| this.open_project_action_editor(None, None, cx)),
                )
                .child(
                    icon(icons::PLUS)
                        .size(px(13.0))
                        .text_color(theme.text_muted),
                )
                .child(SharedString::from("Add action"));
            control = control.child(add);
            if has_imports {
                control = control.child(
                    action_chevron(
                        &theme,
                        cx.listener(|this, _, _, cx| this.toggle_project_actions_menu(cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| {
                            this.project_actions.menu.note_trigger_press();
                        }),
                    ),
                );
            }
        }

        if menu_mounted && (has_actions || has_imports || !can_run) {
            let menu = self.render_project_actions_menu(&status, &snapshot, cx);
            control = control.child(popover::anchored_menu_below(
                "project-actions-menu",
                menu,
                menu_closing,
            ));
        }
        Some(control.into_any_element())
    }

    fn render_project_actions_menu(
        &mut self,
        status: &ProjectActionsStatus,
        snapshot: &ProjectActionsSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let mut card = popover::popover_card(&theme)
            .w(px(280.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_project_actions_menu(cx)))
            .child(popover::menu_heading(&theme, "Project actions"));
        if let ProjectActionsStatus::Unavailable { message, .. } = status {
            let retry = self.project_action_context(cx);
            card = card
                .child(
                    div()
                        .px(px(8.0))
                        .py(px(6.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(message.clone())),
                )
                .when_some(retry, |card, context| {
                    card.child(
                        popover::menu_row(&theme, false, "project-actions-retry")
                            .id("project-actions-retry")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.refresh_project_actions(context.clone(), cx)
                            }))
                            .child(icon(icons::REFRESH).size(px(15.0)))
                            .child(SharedString::from("Retry")),
                    )
                });
        }
        for action in snapshot.actions.clone() {
            let run = action.clone();
            let edit = action.clone();
            let row_id = SharedString::from(format!("project-action-row-{}", action.id));
            card = card.child(
                popover::menu_row(&theme, false, row_id.clone())
                    .id(row_id)
                    .when(status.can_run(), |row| {
                        row.on_click(cx.listener(move |this, _, _, cx| {
                            this.run_project_action(run.clone(), cx)
                        }))
                    })
                    .child(
                        icon(action_icon(action.icon))
                            .size(px(15.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(if action.run_on_worktree_create {
                                format!("{} (setup)", action.name)
                            } else {
                                action.name
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "edit-project-action-{}",
                                edit.id
                            )))
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .hover(|style| style.bg(crate::theme::ink(0.08)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.open_project_action_editor(Some(edit.clone()), None, cx)
                            }))
                            .child(
                                icon(icons::SETTINGS_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            );
        }
        if !snapshot.importable_actions.is_empty() {
            card = card
                .child(popover::menu_separator())
                .child(popover::menu_heading(&theme, "Import from zeron.json"));
            for draft in snapshot.importable_actions.clone() {
                let import = draft.clone();
                let row_id = SharedString::from(format!("import-project-action-{}", draft.name));
                card = card.child(
                    popover::menu_row(&theme, false, row_id.clone())
                        .id(row_id)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_project_action_editor(None, Some(import.clone()), cx)
                        }))
                        .child(
                            icon(action_icon(draft.icon))
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(draft.name)),
                );
            }
        }
        if let Some(issue) = snapshot.project_file_issue.clone() {
            card = card.child(
                div()
                    .px(px(8.0))
                    .py(px(5.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(issue)),
            );
        }
        card.child(popover::menu_separator())
            .child(
                popover::menu_row(&theme, false, "project-actions-add-row")
                    .id("project-actions-add-row")
                    .on_click(
                        cx.listener(|this, _, _, cx| {
                            this.open_project_action_editor(None, None, cx)
                        }),
                    )
                    .child(
                        icon(icons::PLUS)
                            .size(px(15.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from("Add action")),
            )
            .into_any_element()
    }

    pub(super) fn render_project_action_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.project_actions.editor.as_mut()?;
        if std::mem::take(&mut editor.focus_pending) {
            window.focus(&editor.name.focus_handle(cx), cx);
        }
        let theme = Theme::of(cx).clone();
        if editor.confirm_delete {
            let name = editor.name.read(cx).text().trim().to_string();
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete action?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("“{name}” will be permanently deleted."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "action-delete-cancel")
                                .id("action-delete-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(editor) = this.project_actions.editor.as_mut() {
                                        editor.confirm_delete = false;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("action-delete-confirm")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.delete_project_action(cx)),
                                ),
                        ),
                )
                .into_any_element();
            return Some(popover::modal("project-action-delete", viewport, card));
        }

        let name = editor.name.clone();
        let command = editor.command.clone();
        let selected_icon = editor.icon;
        let setup = editor.run_on_worktree_create;
        let editing = editor.action_id.is_some();
        let saving = editor.saving;
        let error = editor.error.clone();
        let title = if editing { "Edit action" } else { "Add action" };
        let mut card = popover::dialog_card(&theme)
            .w(px(440.0))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.project_actions.editor = None;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(&theme, title))
            .child(action_field_label(&theme, "Name"))
            .child(popover::dialog_field(name.into_any_element()))
            .child(action_field_label(&theme, "Command"))
            .child(
                popover::dialog_field(
                    div()
                        .h(px(88.0))
                        .overflow_hidden()
                        .child(command)
                        .into_any_element(),
                )
                .font_family(theme.font_mono.clone()),
            )
            .child(action_field_label(&theme, "Icon"));
        let mut icon_row = div().flex().flex_row().gap(px(6.0));
        for (kind, label) in ACTION_ICONS {
            icon_row = icon_row.child(
                div()
                    .id(SharedString::from(format!("action-icon-{label}")))
                    .size(px(34.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(if kind == selected_icon {
                        theme.text_muted
                    } else {
                        theme.border
                    })
                    .bg(if kind == selected_icon {
                        crate::theme::ink(0.10)
                    } else {
                        crate::theme::ink(0.03)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(editor) = this.project_actions.editor.as_mut() {
                            editor.icon = kind;
                        }
                        cx.notify();
                    }))
                    .child(
                        icon(action_icon(kind))
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    ),
            );
        }
        card = card
            .child(icon_row)
            .child(
                div()
                    .id("action-setup-toggle")
                    .mt(px(14.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(editor) = this.project_actions.editor.as_mut() {
                            editor.run_on_worktree_create = !editor.run_on_worktree_create;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(16.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if setup { theme.text } else { theme.border })
                            .bg(if setup {
                                theme.text
                            } else {
                                crate::theme::ink(0.03)
                            })
                            .when(setup, |el| {
                                el.child(
                                    icon(icons::CHECK).size(px(12.0)).text_color(theme.on_solid),
                                )
                            }),
                    )
                    .child(SharedString::from("Run automatically on worktree creation")),
            )
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .mt(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(error)),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().when(editing, |el| {
                        el.child(
                            popover::btn_ghost(&theme, "Delete action", "action-delete")
                                .id("action-delete")
                                .text_color(theme.danger)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(editor) = this.project_actions.editor.as_mut() {
                                        editor.confirm_delete = true;
                                    }
                                    cx.notify();
                                })),
                        )
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "action-cancel")
                                    .id("action-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.project_actions.editor = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if saving { "Saving…" } else { "Save action" },
                                )
                                .id("action-save")
                                .when(!saving, |button| {
                                    button.on_click(
                                        cx.listener(|this, _, _, cx| this.save_project_action(cx)),
                                    )
                                }),
                            ),
                    ),
            );
        Some(popover::modal(
            "project-action-editor",
            viewport,
            card.into_any_element(),
        ))
    }
}

fn project_action_params(
    mut params: serde_json::Value,
    target_device_id: &Option<String>,
) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target_device_id, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

fn action_segment(theme: &Theme, id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h_full()
        .px(px(7.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(px(11.5))
        .text_color(theme.text.opacity(0.9))
        .hover(|style| style.bg(crate::theme::ink(0.07)))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
}

fn action_chevron(
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id("project-actions-chevron")
        .h_full()
        .w(px(23.0))
        .flex()
        .items_center()
        .justify_center()
        .border_l_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::ink(0.07)))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(
            icon(icons::ALT_ARROW_DOWN)
                .size(px(11.0))
                .text_color(theme.text_muted),
        )
}

fn action_field_label(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .mt(px(12.0))
        .mb(px(5.0))
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_muted)
        .child(SharedString::from(label.to_string()))
}
