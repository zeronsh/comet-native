//! Viewport-local state for host-owned project Actions.

use std::collections::HashMap;

use gpui::{Entity, Subscription, Task};
use zeron_proto::{ProjectAction, ProjectActionDraft, ProjectActionIcon, ProjectActionsSnapshot};

use crate::composer::ComposerInput;
use crate::popover;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectActionsKey {
    pub device_id: String,
    pub space_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectActionsStatus {
    Idle,
    Loading,
    Ready(ProjectActionsSnapshot),
    Saving(ProjectActionsSnapshot),
    Unavailable {
        snapshot: Option<ProjectActionsSnapshot>,
        message: String,
    },
    Unsupported,
}

impl ProjectActionsStatus {
    pub fn snapshot(&self) -> Option<&ProjectActionsSnapshot> {
        match self {
            Self::Ready(snapshot) | Self::Saving(snapshot) => Some(snapshot),
            Self::Unavailable {
                snapshot: Some(snapshot),
                ..
            } => Some(snapshot),
            Self::Idle
            | Self::Loading
            | Self::Unavailable { snapshot: None, .. }
            | Self::Unsupported => None,
        }
    }

    pub fn can_run(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

pub struct ProjectActionEditor {
    pub key: ProjectActionsKey,
    pub action_id: Option<String>,
    pub name: Entity<ComposerInput>,
    pub command: Entity<ComposerInput>,
    pub icon: ProjectActionIcon,
    pub run_on_worktree_create: bool,
    pub error: Option<String>,
    pub saving: bool,
    pub focus_pending: bool,
    pub confirm_delete: bool,
    pub _name_events: Subscription,
    pub _command_events: Subscription,
}

pub struct ProjectActionsController {
    pub active: Option<ProjectActionsKey>,
    pub generation: u64,
    mutation_generation: u64,
    pub cache: HashMap<ProjectActionsKey, ProjectActionsStatus>,
    pub menu: popover::Popup<()>,
    pub editor: Option<ProjectActionEditor>,
    pub request_task: Option<Task<()>>,
    pub mutation_task: Option<Task<()>>,
}

impl Default for ProjectActionsController {
    fn default() -> Self {
        Self {
            active: None,
            generation: 0,
            mutation_generation: 0,
            cache: HashMap::new(),
            menu: popover::Popup::default(),
            editor: None,
            request_task: None,
            mutation_task: None,
        }
    }
}

impl ProjectActionsController {
    pub fn activate(&mut self, key: Option<ProjectActionsKey>) -> bool {
        if self.active == key {
            return false;
        }
        self.active = key;
        self.generation = self.generation.wrapping_add(1);
        self.menu = popover::Popup::default();
        self.editor = None;
        self.request_task = None;
        self.invalidate_mutation();
        true
    }

    fn invalidate_mutation(&mut self) {
        self.mutation_generation = self.mutation_generation.wrapping_add(1);
        self.mutation_task = None;
    }

    pub fn begin_mutation(&mut self) -> u64 {
        self.invalidate_mutation();
        self.mutation_generation
    }

    pub fn is_current_mutation(&self, key: &ProjectActionsKey, generation: u64) -> bool {
        self.active.as_ref() == Some(key) && self.mutation_generation == generation
    }

    pub fn active_status(&self) -> Option<&ProjectActionsStatus> {
        self.active.as_ref().and_then(|key| self.cache.get(key))
    }

    pub fn active_snapshot(&self) -> Option<&ProjectActionsSnapshot> {
        self.active_status()
            .and_then(ProjectActionsStatus::snapshot)
    }

    /// Snapshot used to keep the control recoverable when the first request
    /// fails before any host state has been cached.
    pub fn visible_snapshot(&self) -> Option<ProjectActionsSnapshot> {
        let key = self.active.as_ref()?;
        match self.active_status()? {
            ProjectActionsStatus::Ready(snapshot)
            | ProjectActionsStatus::Saving(snapshot)
            | ProjectActionsStatus::Unavailable {
                snapshot: Some(snapshot),
                ..
            } => Some(snapshot.clone()),
            ProjectActionsStatus::Unavailable { snapshot: None, .. } => {
                Some(ProjectActionsSnapshot {
                    space_id: key.space_id.clone(),
                    actions: Vec::new(),
                    importable_actions: Vec::new(),
                    project_file_issue: None,
                })
            }
            ProjectActionsStatus::Idle
            | ProjectActionsStatus::Loading
            | ProjectActionsStatus::Unsupported => None,
        }
    }

    pub fn begin_load(&mut self, key: &ProjectActionsKey) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if self
            .cache
            .get(key)
            .and_then(ProjectActionsStatus::snapshot)
            .is_none()
        {
            self.cache
                .insert(key.clone(), ProjectActionsStatus::Loading);
        }
        generation
    }

    pub fn accept_load(
        &mut self,
        key: &ProjectActionsKey,
        generation: u64,
        result: Result<ProjectActionsSnapshot, String>,
    ) -> bool {
        if self.active.as_ref() != Some(key) || self.generation != generation {
            return false;
        }
        let next = match result {
            Ok(snapshot) => ProjectActionsStatus::Ready(snapshot),
            Err(message) if unknown_method(&message) => ProjectActionsStatus::Unsupported,
            Err(message) => ProjectActionsStatus::Unavailable {
                snapshot: self
                    .cache
                    .get(key)
                    .and_then(|state| state.snapshot().cloned()),
                message,
            },
        };
        self.cache.insert(key.clone(), next);
        true
    }

    pub fn mark_unavailable(&mut self, key: &ProjectActionsKey, message: String) {
        let snapshot = self
            .cache
            .get(key)
            .and_then(|state| state.snapshot().cloned());
        let state = if unknown_method(&message) {
            ProjectActionsStatus::Unsupported
        } else {
            ProjectActionsStatus::Unavailable { snapshot, message }
        };
        self.cache.insert(key.clone(), state);
    }
}

pub fn preferred_action<'a>(
    actions: &'a [ProjectAction],
    preferred_id: Option<&str>,
) -> Option<&'a ProjectAction> {
    preferred_id
        .and_then(|id| actions.iter().find(|action| action.id == id))
        .or_else(|| actions.iter().find(|action| !action.run_on_worktree_create))
        .or_else(|| actions.first())
}

pub fn action_icon(icon: ProjectActionIcon) -> &'static str {
    match icon {
        ProjectActionIcon::Play => crate::icons::ACTION_PLAY,
        ProjectActionIcon::Test => crate::icons::ACTION_TEST,
        ProjectActionIcon::Lint => crate::icons::ACTION_LINT,
        ProjectActionIcon::Configure => crate::icons::ACTION_CONFIGURE,
        ProjectActionIcon::Build => crate::icons::ACTION_BUILD,
        ProjectActionIcon::Debug => crate::icons::ACTION_DEBUG,
    }
}

pub const ACTION_ICONS: [(ProjectActionIcon, &str); 6] = [
    (ProjectActionIcon::Play, "Play"),
    (ProjectActionIcon::Test, "Test"),
    (ProjectActionIcon::Lint, "Lint"),
    (ProjectActionIcon::Configure, "Configure"),
    (ProjectActionIcon::Build, "Build"),
    (ProjectActionIcon::Debug, "Debug"),
];

const ACTION_LABEL_MIN_TITLEBAR_WIDTH: f32 = 420.0;

pub fn show_action_label(available_titlebar_width: f32) -> bool {
    available_titlebar_width >= ACTION_LABEL_MIN_TITLEBAR_WIDTH
}

pub fn draft_from_action(action: &ProjectAction) -> ProjectActionDraft {
    ProjectActionDraft {
        name: action.name.clone(),
        command: action.command.clone(),
        icon: action.icon,
        run_on_worktree_create: action.run_on_worktree_create,
    }
}

fn unknown_method(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("unknown method") || message.contains("unknownmethod")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(id: &str, setup: bool) -> ProjectAction {
        ProjectAction {
            id: id.into(),
            name: id.into(),
            command: id.into(),
            icon: ProjectActionIcon::Play,
            run_on_worktree_create: setup,
        }
    }

    #[test]
    fn preferred_selection_and_responsive_cutoff() {
        let actions = [action("setup", true), action("dev", false)];
        assert_eq!(
            preferred_action(&actions, Some("setup")).unwrap().id,
            "setup"
        );
        assert_eq!(preferred_action(&actions, Some("gone")).unwrap().id, "dev");
        assert_eq!(preferred_action(&actions, None).unwrap().id, "dev");
        assert!(!show_action_label(419.0));
        assert!(show_action_label(420.0));
    }

    #[test]
    fn late_response_cannot_replace_the_active_project() {
        let mut controller = ProjectActionsController::default();
        let first = ProjectActionsKey {
            device_id: "a".into(),
            space_id: "one".into(),
        };
        let second = ProjectActionsKey {
            device_id: "b".into(),
            space_id: "two".into(),
        };
        controller.activate(Some(first.clone()));
        let generation = controller.begin_load(&first);
        controller.activate(Some(second.clone()));
        controller.begin_load(&second);
        let stale = ProjectActionsSnapshot {
            space_id: first.space_id.clone(),
            actions: vec![action("stale", false)],
            importable_actions: Vec::new(),
            project_file_issue: None,
        };
        assert!(!controller.accept_load(&first, generation, Ok(stale)));
        assert!(matches!(
            controller.active_status(),
            Some(ProjectActionsStatus::Loading)
        ));
    }

    #[test]
    fn unknown_method_is_hidden_and_transport_errors_keep_snapshot() {
        let mut controller = ProjectActionsController::default();
        let key = ProjectActionsKey {
            device_id: "a".into(),
            space_id: "one".into(),
        };
        controller.activate(Some(key.clone()));
        let generation = controller.begin_load(&key);
        assert!(controller.accept_load(
            &key,
            generation,
            Err("Unknown method ListProjectActions".into())
        ));
        assert!(matches!(
            controller.active_status(),
            Some(ProjectActionsStatus::Unsupported)
        ));

        let snapshot = ProjectActionsSnapshot {
            space_id: key.space_id.clone(),
            actions: vec![action("dev", false)],
            importable_actions: Vec::new(),
            project_file_issue: None,
        };
        controller
            .cache
            .insert(key.clone(), ProjectActionsStatus::Ready(snapshot));
        controller.mark_unavailable(&key, "offline".into());
        assert_eq!(controller.active_snapshot().unwrap().actions[0].id, "dev");
        assert!(!controller.active_status().unwrap().can_run());
    }

    #[test]
    fn initial_transport_error_keeps_a_visible_retry_surface() {
        let mut controller = ProjectActionsController::default();
        let key = ProjectActionsKey {
            device_id: "remote".into(),
            space_id: "project".into(),
        };
        controller.activate(Some(key.clone()));
        let generation = controller.begin_load(&key);
        assert!(controller.accept_load(&key, generation, Err("remote routing unavailable".into())));

        assert!(controller.active_snapshot().is_none());
        let visible = controller
            .visible_snapshot()
            .expect("unavailable control remains visible");
        assert_eq!(visible.space_id, key.space_id);
        assert!(visible.actions.is_empty());
        assert!(visible.importable_actions.is_empty());
    }

    #[test]
    fn mutations_are_invalidated_by_project_changes_and_newer_mutations() {
        let mut controller = ProjectActionsController::default();
        let first = ProjectActionsKey {
            device_id: "a".into(),
            space_id: "one".into(),
        };
        let second = ProjectActionsKey {
            device_id: "b".into(),
            space_id: "two".into(),
        };

        controller.activate(Some(first.clone()));
        let first_generation = controller.begin_mutation();
        assert!(controller.is_current_mutation(&first, first_generation));

        controller.activate(Some(second.clone()));
        assert!(!controller.is_current_mutation(&first, first_generation));

        let superseded = controller.begin_mutation();
        let current = controller.begin_mutation();
        assert!(!controller.is_current_mutation(&second, superseded));
        assert!(controller.is_current_mutation(&second, current));
    }
}
