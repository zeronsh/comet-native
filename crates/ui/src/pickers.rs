//! Composer pickers (feature-inventory §1.7): RepoPicker (recents + search +
//! in-app folder browser + clone/create), BranchPicker (search + isolated-
//! worktree toggle), HarnessModelPicker (harness rail + model list, harness
//! locked once the chat exists), TraitsPicker (reasoning ladder + advertised
//! model options; trigger shows the non-default summary "High · 1M · Fast").
//!
//! All selections accumulate into a [`DraftConfig`] the composer threads into
//! the Run command and the `Mutate createChat` call on first send.
//!
//! Pure logic (repo ordering, folder-browser navigation, traits summary) lives
//! in free functions with unit tests; RPC results land in [`Loadable`] slots
//! rendered as skeletons / inline errors with Retry.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use comet_engine::registry::HarnessDescriptor;
use comet_proto::{
    Chat, ChatConfig, FolderListing, HarnessId, Model, ReasoningLevel, Repo, SandboxLevel,
};
use comet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable, MenuKey};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// option id → choice id (only non-defaults are meaningful).
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub repo: Option<Repo>,
    pub branch: Option<String>,
    /// Run in an isolated worktree (`CreateWorktree` on send).
    pub isolated_worktree: bool,
}

impl DraftConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known harness).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: SandboxLevel::WorkspaceWrite,
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: labels + traits summary
// ---------------------------------------------------------------------------

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

/// The TraitsPicker trigger summary: non-default reasoning + non-default model
/// option choices, joined with " · " (comet: "High · 1M · Fast"). `None` when
/// everything is at its default.
pub fn traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let Some(choice_id) = selections.get(&option.id).and_then(|v| v.as_str()) else {
                continue;
            };
            if choice_id == option.default_choice {
                continue;
            }
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

// ---------------------------------------------------------------------------
// Pure: repo ordering
// ---------------------------------------------------------------------------

/// Cwds of recent chats, most recent first, deduped — the RepoPicker "recents".
pub fn recent_cwds(chats: &[Chat]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for chat in chats {
        if let Some(cwd) = chat.cwd.as_deref()
            && !cwd.is_empty()
            && !out.iter().any(|c| c == cwd)
        {
            out.push(cwd.to_string());
        }
    }
    out
}

/// Recents-first repo ordering: repos whose path appears in `recents` (already
/// most-recent-first) lead in that order; the rest follow alphabetically.
pub fn order_repos(mut repos: Vec<Repo>, recents: &[String]) -> Vec<Repo> {
    repos.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut out: Vec<Repo> = Vec::new();
    for cwd in recents {
        if let Some(at) = repos.iter().position(|r| &r.path == cwd) {
            out.push(repos.remove(at));
        }
    }
    out.extend(repos);
    out
}

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation
// ---------------------------------------------------------------------------

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// What Enter does on the active browser row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseEnter {
    /// Descend into a plain directory.
    Descend(String),
    /// The entry is a git repo — pick it (AddRepo).
    Pick(String),
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&comet_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

/// Resolve Enter on row `active` of a listing.
pub fn browse_enter(listing: &FolderListing, active: usize) -> Option<BrowseEnter> {
    let rows = browser_rows(listing);
    let entry = rows.get(active)?;
    let full = child_path(&listing.path, &entry.name);
    if entry.is_repo {
        Some(BrowseEnter::Pick(full))
    } else {
        Some(BrowseEnter::Descend(full))
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// Which picker popover is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Repo,
    Branch,
    HarnessModel,
    Traits,
}

/// Sub-view of the repo popover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoPane {
    List,
    Browser,
    CloneUrl,
    CreateName,
}

pub struct Pickers {
    state: Entity<AppState>,
    config: DraftConfig,
    open: Option<PickerKind>,
    repo_pane: RepoPane,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    models: HashMap<HarnessId, Loadable<Vec<Model>>>,
    repos: Loadable<Vec<Repo>>,
    branches: Loadable<Vec<String>>,
    /// Repo path the `branches` slot belongs to.
    branches_repo: Option<String>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = server default, i.e. home).
    browser_path: Option<String>,
    /// Highlighted row in the open list (keyboard nav).
    active: usize,
    /// Shared search / URL / name input, reused across popovers.
    search: Entity<ComposerInput>,
    form_busy: bool,
    form_error: Option<SharedString>,
    focus: FocusHandle,
    /// Re-open suppression after outside-click dismissal (the dismiss and the
    /// trigger click would otherwise toggle twice).
    suppressed: Option<(PickerKind, Instant)>,
    load_task: Option<Task<()>>,
    form_task: Option<Task<()>>,
    _search_events: Subscription,
    _state_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| ComposerInput::new("Search…", cx));
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                this.active = 0;
                cx.notify();
            }
            ComposerInputEvent::Submitted => this.on_search_submit(cx),
        });
        // Chat selection / config changes must re-render the chips (child views
        // only re-render on their own notify).
        let state_observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            config: DraftConfig::default(),
            open: None,
            repo_pane: RepoPane::List,
            harnesses: Loadable::Idle,
            models: HashMap::new(),
            repos: Loadable::Idle,
            branches: Loadable::Idle,
            branches_repo: None,
            browser: Loadable::Idle,
            browser_path: None,
            active: 0,
            search,
            form_busy: false,
            form_error: None,
            focus: cx.focus_handle(),
            suppressed: None,
            load_task: None,
            form_task: None,
            _search_events: search_events,
            _state_observe: state_observe,
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    /// Harness is locked once the chat exists (feature-inventory §1.7).
    fn harness_locked(&self, cx: &App) -> bool {
        self.state.read(cx).selected_chat.is_some()
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// Effective harness: picked, or the chat's config, or the first listed.
    fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            return Some(config.harness);
        }
        self.harnesses
            .ready()
            .and_then(|list| list.first())
            .map(|d| d.id)
    }

    fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(cx)?;
        let models = self.models.get(&harness)?.ready()?;
        match self.config.model.as_deref() {
            Some(id) => models.iter().find(|m| m.id == id),
            None => models.first(),
        }
    }

    // ---- open/close ----

    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(kind) = self.open.take() {
            self.suppressed = Some((kind, Instant::now()));
        }
        self.form_error = None;
        self.form_busy = false;
        cx.notify();
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.open == Some(kind) {
            self.open = None;
            cx.notify();
            return;
        }
        // A just-dismissed popover's trigger click must not instantly reopen.
        if let Some((suppressed, at)) = self.suppressed.take()
            && suppressed == kind
            && at.elapsed() < Duration::from_millis(400)
        {
            cx.notify();
            return;
        }
        self.open = Some(kind);
        self.repo_pane = RepoPane::List;
        self.active = 0;
        self.form_error = None;
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            input.set_text("", cx);
        });
        // Searchable pickers focus the filter input (it sits inside the frame,
        // so the frame's key handler still sees arrows/Enter); the rest focus
        // the frame itself for pure keyboard nav.
        match kind {
            PickerKind::Repo | PickerKind::Branch => {
                let handle = self.search.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            PickerKind::Repo => self.ensure_repos(false, cx),
            PickerKind::Branch => self.ensure_branches(false, cx),
            PickerKind::HarnessModel | PickerKind::Traits => {
                self.ensure_harnesses(cx);
                if let Some(harness) = self.effective_harness(cx) {
                    self.ensure_models(harness, cx);
                }
            }
        }
        cx.notify();
    }

    // ---- loads ----

    fn ensure_harnesses(&mut self, cx: &mut Context<Self>) {
        if matches!(self.harnesses, Loadable::Ready(_) | Loadable::Loading) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.harnesses = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if let Some(harness) = pickers.effective_harness(cx) {
                    pickers.ensure_models(harness, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn ensure_models(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if matches!(
            self.models.get(&harness),
            Some(Loadable::Ready(_)) | Some(Loadable::Loading)
        ) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.models.insert(harness, Loadable::Loading);
        cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "harness": harness });
            let result = engine.client().call(methods::LIST_MODELS, params).await;
            this.update(cx, |pickers, cx| {
                let loaded = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Model>>(value) {
                        Ok(models) => Loadable::Ready(models),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                pickers.models.insert(harness, loaded);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn ensure_repos(&mut self, force: bool, cx: &mut Context<Self>) {
        if !force && matches!(self.repos, Loadable::Ready(_) | Loadable::Loading) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.repos = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_REPOS, serde_json::json!({}))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.repos = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Repo>>(value) {
                        Ok(repos) => Loadable::Ready(repos),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn ensure_branches(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.config.repo.clone() else {
            return;
        };
        let fresh = self.branches_repo.as_deref() == Some(repo.path.as_str());
        if !force && fresh && matches!(self.branches, Loadable::Ready(_) | Loadable::Loading) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.branches = Loadable::Loading;
        self.branches_repo = Some(repo.path.clone());
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "repoPath": repo.path });
            let result = engine.client().call(methods::LIST_BRANCHES, params).await;
            this.update(cx, |pickers, cx| {
                pickers.branches = match result {
                    Ok(value) => match serde_json::from_value::<Vec<String>>(value) {
                        Ok(branches) => Loadable::Ready(branches),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.browser_path = path.clone();
        self.browser = Loadable::Loading;
        self.active = 0;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let params = match &path {
                Some(p) => serde_json::json!({ "path": p }),
                None => serde_json::json!({}),
            };
            let result = engine.client().call(methods::LIST_FOLDERS, params).await;
            this.update(cx, |pickers, cx| {
                pickers.browser = match result {
                    Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                        Ok(listing) => Loadable::Ready(listing),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- selections ----

    fn pick_repo(&mut self, repo: Repo, cx: &mut Context<Self>) {
        if self.config.repo.as_ref().map(|r| &r.path) != Some(&repo.path) {
            self.config.branch = None;
            self.branches = Loadable::Idle;
            self.branches_repo = None;
        }
        self.config.repo = Some(repo);
        self.open = None;
        cx.notify();
    }

    /// AddRepo for a browsed folder, then select the resulting repo.
    fn add_repo_path(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.form_busy = true;
        self.form_error = None;
        cx.notify();
        self.form_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::ADD_REPO, serde_json::json!({ "path": path }))
                .await
                .and_then(|value| {
                    serde_json::from_value::<Repo>(value)
                        .map_err(|e| comet_rpc::RpcError::Failed(e.to_string()))
                });
            this.update(cx, |pickers, cx| {
                pickers.form_busy = false;
                match result {
                    Ok(repo) => {
                        pickers.ensure_repos(true, cx);
                        pickers.pick_repo(repo, cx);
                    }
                    Err(err) => pickers.form_error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// CloneRepo / CreateRepo from the inline forms.
    fn submit_repo_form(&mut self, cx: &mut Context<Self>) {
        let text = self.search.read(cx).text().trim().to_string();
        if text.is_empty() || self.form_busy {
            return;
        }
        let (method, params) = match self.repo_pane {
            RepoPane::CloneUrl => (methods::CLONE_REPO, serde_json::json!({ "url": text })),
            RepoPane::CreateName => (methods::CREATE_REPO, serde_json::json!({ "name": text })),
            _ => return,
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.form_busy = true;
        self.form_error = None;
        cx.notify();
        self.form_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .and_then(|value| {
                    serde_json::from_value::<Repo>(value)
                        .map_err(|e| comet_rpc::RpcError::Failed(e.to_string()))
                });
            this.update(cx, |pickers, cx| {
                pickers.form_busy = false;
                match result {
                    Ok(repo) => {
                        pickers.ensure_repos(true, cx);
                        pickers
                            .search
                            .update(cx, |input, cx| input.set_text("", cx));
                        pickers.pick_repo(repo, cx);
                    }
                    Err(err) => pickers.form_error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn pick_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        self.config.branch = Some(branch);
        self.open = None;
        cx.notify();
    }

    fn pick_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.harness_locked(cx) {
            return;
        }
        if self.config.harness != Some(harness) {
            self.config.model = None;
            self.config.reasoning = None;
            self.config.model_options.clear();
        }
        self.config.harness = Some(harness);
        self.ensure_models(harness, cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        self.config.model = Some(model_id);
        self.open = None;
        cx.notify();
    }

    fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        // Clicking the active level clears back to the model default.
        if self.config.reasoning == Some(level) {
            self.config.reasoning = None;
        } else {
            self.config.reasoning = Some(level);
        }
        cx.notify();
    }

    fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if default {
            self.config.model_options.remove(&option_id);
        } else {
            self.config
                .model_options
                .insert(option_id, serde_json::Value::String(choice_id));
        }
        cx.notify();
    }

    // ---- keyboard ----

    fn filtered_repo_rows(&self, cx: &App) -> Vec<Repo> {
        let Some(repos) = self.repos.ready() else {
            return Vec::new();
        };
        let recents = recent_cwds(&self.state.read(cx).chats);
        let ordered = order_repos(repos.clone(), &recents);
        let query = self.search.read(cx).text().to_string();
        let labels: Vec<&str> = ordered.iter().map(|r| r.name.as_str()).collect();
        popover::filter_indices(&query, &labels)
            .into_iter()
            .map(|ix| ordered[ix].clone())
            .collect()
    }

    fn filtered_branch_rows(&self, cx: &App) -> Vec<String> {
        let Some(branches) = self.branches.ready() else {
            return Vec::new();
        };
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, branches)
            .into_iter()
            .map(|ix| branches[ix].clone())
            .collect()
    }

    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        match (self.open, self.repo_pane) {
            (Some(PickerKind::Repo), RepoPane::CloneUrl | RepoPane::CreateName) => {
                self.submit_repo_form(cx)
            }
            (Some(PickerKind::Repo), RepoPane::List) => {
                if let Some(repo) = self.filtered_repo_rows(cx).into_iter().nth(self.active) {
                    self.pick_repo(repo, cx);
                }
            }
            (Some(PickerKind::Branch), _) => {
                if let Some(branch) = self.filtered_branch_rows(cx).into_iter().nth(self.active) {
                    self.pick_branch(branch, cx);
                }
            }
            _ => {}
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        let search_empty = self.search.read(cx).is_empty();
        match key {
            MenuKey::Escape => {
                if self.open == Some(PickerKind::Repo) && self.repo_pane != RepoPane::List {
                    self.repo_pane = RepoPane::List;
                    self.form_error = None;
                    self.active = 0;
                    cx.notify();
                } else {
                    self.open = None;
                    cx.notify();
                }
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match (self.open, self.repo_pane) {
                    (Some(PickerKind::Repo), RepoPane::List) => self.filtered_repo_rows(cx).len(),
                    (Some(PickerKind::Repo), RepoPane::Browser) => self
                        .browser
                        .ready()
                        .map(|l| browser_rows(l).len())
                        .unwrap_or(0),
                    (Some(PickerKind::Branch), _) => self.filtered_branch_rows(cx).len(),
                    _ => 0,
                };
                self.active = popover::menu_step(Some(self.active), count, delta).unwrap_or(0);
                cx.notify();
            }
            MenuKey::Enter if !search_focused => {
                if self.open == Some(PickerKind::Repo) && self.repo_pane == RepoPane::Browser {
                    self.browser_activate(cx);
                } else {
                    self.on_search_submit(cx);
                }
            }
            MenuKey::ModEnter => {
                // Browser accelerator: pick the *current* folder as the repo.
                if self.open == Some(PickerKind::Repo)
                    && self.repo_pane == RepoPane::Browser
                    && let Some(listing) = self.browser.ready()
                {
                    let path = listing.path.clone();
                    self.add_repo_path(path, cx);
                }
            }
            MenuKey::Backspace if !search_focused || search_empty => {
                if self.open == Some(PickerKind::Repo)
                    && self.repo_pane == RepoPane::Browser
                    && let Some(listing) = self.browser.ready()
                    && let Some(parent) = parent_path(&listing.path)
                {
                    self.load_folders(Some(parent), cx);
                }
            }
            _ => {}
        }
    }

    fn browser_activate(&mut self, cx: &mut Context<Self>) {
        let Some(listing) = self.browser.ready() else {
            return;
        };
        match browse_enter(listing, self.active) {
            Some(BrowseEnter::Descend(path)) => self.load_folders(Some(path), cx),
            Some(BrowseEnter::Pick(path)) => self.add_repo_path(path, cx),
            None => {}
        }
    }

    // ---- render ----

    fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id: &'static str = match kind {
            PickerKind::Repo => "picker-repo",
            PickerKind::Branch => "picker-branch",
            PickerKind::HarnessModel => "picker-model",
            PickerKind::Traits => "picker-traits",
        };
        let open = self.open == Some(kind);
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(Theme::SPACE_SM))
            .py(px(3.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .border_color(if open {
                theme.border_strong
            } else {
                theme.border
            })
            .text_size(px(11.0))
            .text_color(if set { theme.text } else { theme.text_muted })
            .when(open, |el| el.bg(theme.element_active))
            .hover(|s| s.bg(theme.element_hover))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(label)
            .child(
                div()
                    .text_color(theme.text_faint)
                    .child(SharedString::from("▾")),
            )
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            .max_h(px(380.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn search_box(&self, theme: &Theme) -> AnyElement {
        div()
            .m(px(4.0))
            .px(px(Theme::SPACE_SM))
            .py(px(4.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .border_color(theme.border)
            .child(self.search.clone())
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Repo => {
                            if this.repo_pane == RepoPane::Browser {
                                let path = this.browser_path.clone();
                                this.load_folders(path, cx);
                            } else {
                                this.ensure_repos(true, cx);
                            }
                        }
                        PickerKind::Branch => this.ensure_branches(true, cx),
                        PickerKind::HarnessModel | PickerKind::Traits => {
                            this.harnesses = Loadable::Idle;
                            this.ensure_harnesses(cx);
                        }
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    fn render_repo_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        match self.repo_pane {
            RepoPane::List => {
                let rows = self.filtered_repo_rows(cx);
                let body: AnyElement = match &self.repos {
                    Loadable::Loading | Loadable::Idle => {
                        popover::skeleton_rows("repo-skeleton", &theme, 4)
                    }
                    Loadable::Error(message) => {
                        let message = message.clone();
                        self.retry_row("repo-retry", &message, PickerKind::Repo, &theme, cx)
                    }
                    Loadable::Ready(_) if rows.is_empty() => div()
                        .p(px(Theme::SPACE_SM))
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("No repositories"))
                        .into_any_element(),
                    Loadable::Ready(_) => {
                        let active = self.active;
                        div()
                            .id("repo-list")
                            .flex()
                            .flex_col()
                            .max_h(px(220.0))
                            .overflow_y_scroll()
                            .children(rows.into_iter().enumerate().map(|(ix, repo)| {
                                let name: SharedString = repo.name.clone().into();
                                let path: SharedString = repo.path.clone().into();
                                popover::menu_row(&theme, ix == active)
                                    .id(("repo-row", ix))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.pick_repo(repo.clone(), cx);
                                    }))
                                    .child(div().flex_none().child(name))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_faint)
                                            .child(path),
                                    )
                            }))
                            .into_any_element()
                    }
                };
                let action = |id: &'static str, label: &'static str, pane: RepoPane| {
                    popover::menu_row(&theme, false)
                        .id(id)
                        .text_color(theme.text_muted)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.repo_pane = pane;
                            this.active = 0;
                            this.form_error = None;
                            let placeholder = match pane {
                                RepoPane::CloneUrl => "https://github.com/owner/repo.git",
                                RepoPane::CreateName => "New repo name",
                                _ => "Search…",
                            };
                            this.search.update(cx, |input, cx| {
                                input.set_placeholder(placeholder, cx);
                                input.set_text("", cx);
                            });
                            if pane == RepoPane::Browser {
                                // The browser is keyboard-driven off the frame.
                                window.focus(&this.focus, cx);
                                if this.browser.ready().is_none() {
                                    this.load_folders(None, cx);
                                }
                            } else {
                                let handle = this.search.read(cx).focus_handle(cx);
                                window.focus(&handle, cx);
                            }
                            cx.notify();
                        }))
                        .child(SharedString::from(label))
                };
                div()
                    .flex()
                    .flex_col()
                    .child(self.search_box(&theme))
                    .child(body)
                    .child(div().h(px(1.0)).mx(px(4.0)).my(px(2.0)).bg(theme.border))
                    .child(action(
                        "repo-open-folder",
                        "Open folder…",
                        RepoPane::Browser,
                    ))
                    .child(action("repo-clone", "Clone from URL…", RepoPane::CloneUrl))
                    .child(action(
                        "repo-create",
                        "Create new repo…",
                        RepoPane::CreateName,
                    ))
                    .into_any_element()
            }
            RepoPane::Browser => self.render_browser(&theme, cx),
            RepoPane::CloneUrl | RepoPane::CreateName => {
                let (title, submit_label) = if self.repo_pane == RepoPane::CloneUrl {
                    ("Clone from URL", "Clone")
                } else {
                    ("Create new repo", "Create")
                };
                let busy = self.form_busy;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .p(px(4.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(title)),
                    )
                    .child(self.search_box(&theme))
                    .when_some(self.form_error.clone(), |el, message| {
                        el.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.danger)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(
                                div()
                                    .id("repo-form-back")
                                    .px(px(Theme::SPACE_SM))
                                    .py(px(3.0))
                                    .rounded(px(Theme::CONTROL_RADIUS))
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.element_hover))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.repo_pane = RepoPane::List;
                                        this.form_error = None;
                                        this.search.update(cx, |input, cx| {
                                            input.set_placeholder("Search…", cx);
                                            input.set_text("", cx);
                                        });
                                        cx.notify();
                                    }))
                                    .child(SharedString::from("Back")),
                            )
                            .child(
                                div()
                                    .id("repo-form-submit")
                                    .px(px(Theme::SPACE_MD))
                                    .py(px(3.0))
                                    .rounded(px(Theme::CONTROL_RADIUS))
                                    .bg(theme.accent_strong)
                                    .text_size(px(12.0))
                                    .text_color(gpui::white())
                                    .when(busy, |el| el.opacity(0.6))
                                    .cursor_pointer()
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.submit_repo_form(cx)),
                                    )
                                    .child(SharedString::from(if busy {
                                        "Working…"
                                    } else {
                                        submit_label
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    /// The in-app folder browser: breadcrumbs + arrow/Enter/Cmd+Enter/Backspace
    /// keys + skeleton rows + truncation notice + Retry (§1.7).
    fn render_browser(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let crumbs: AnyElement = match self.browser.ready() {
            Some(listing) => {
                let segments = breadcrumbs(&listing.path);
                let last = segments.len().saturating_sub(1);
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(2.0))
                    .px(px(6.0))
                    .pt(px(4.0))
                    .text_size(px(11.0))
                    .children(segments.into_iter().enumerate().map(|(ix, (label, full))| {
                        let color = if ix == last {
                            theme.text
                        } else {
                            theme.text_faint
                        };
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(2.0))
                            .when(ix > 0, |el| {
                                el.child(
                                    div()
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from("/")),
                                )
                            })
                            .child(
                                div()
                                    .id(("crumb", ix))
                                    .px(px(2.0))
                                    .rounded(px(3.0))
                                    .text_color(color)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.element_hover))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.load_folders(Some(full.clone()), cx);
                                    }))
                                    .child(SharedString::from(label)),
                            )
                    }))
                    .into_any_element()
            }
            None => gpui::Empty.into_any_element(),
        };

        let body: AnyElement = match &self.browser {
            Loadable::Loading | Loadable::Idle => {
                popover::skeleton_rows("browser-skeleton", theme, 6)
            }
            Loadable::Error(message) => {
                let message = message.clone();
                self.retry_row("browser-retry", &message, PickerKind::Repo, theme, cx)
            }
            Loadable::Ready(listing) => {
                let listing = listing.clone();
                let rows = browser_rows(&listing);
                let active = self.active;
                let truncated = listing.truncated;
                let list = div()
                    .id("browser-list")
                    .flex()
                    .flex_col()
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .children(rows.iter().enumerate().map(|(ix, entry)| {
                        let name: SharedString = entry.name.clone().into();
                        let is_repo = entry.is_repo;
                        popover::menu_row(theme, ix == active)
                            .id(("browser-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.active = ix;
                                this.browser_activate(cx);
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(if is_repo {
                                        theme.accent
                                    } else {
                                        theme.text_faint
                                    })
                                    .child(SharedString::from(if is_repo { "◆" } else { "▸" })),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            .when(is_repo, |el| {
                                el.child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from("repo")),
                                )
                            })
                    }));
                div()
                    .flex()
                    .flex_col()
                    .child(list)
                    .when(rows.is_empty(), |el| {
                        el.child(
                            div()
                                .p(px(Theme::SPACE_SM))
                                .text_size(px(12.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from("No folders here")),
                        )
                    })
                    .when(truncated, |el| {
                        el.child(
                            div()
                                .px(px(Theme::SPACE_SM))
                                .py(px(4.0))
                                .text_size(px(11.0))
                                .text_color(theme.warning)
                                .child(SharedString::from("Listing truncated — narrow down")),
                        )
                    })
                    .into_any_element()
            }
        };

        let use_this = self.browser.ready().map(|l| l.path.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(crumbs)
            .child(body)
            .when_some(self.form_error.clone(), |el, message| {
                el.child(
                    div()
                        .px(px(6.0))
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            })
            .child(div().h(px(1.0)).mx(px(4.0)).my(px(2.0)).bg(theme.border))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px(px(4.0))
                    .pb(px(2.0))
                    .child(
                        div()
                            .id("browser-back")
                            .px(px(Theme::SPACE_SM))
                            .py(px(3.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.repo_pane = RepoPane::List;
                                cx.notify();
                            }))
                            .child(SharedString::from("Back")),
                    )
                    .when_some(use_this, |el, path| {
                        el.child(
                            div()
                                .id("browser-use-current")
                                .px(px(Theme::SPACE_SM))
                                .py(px(3.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .border_1()
                                .border_color(theme.border)
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.element_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.add_repo_path(path.clone(), cx);
                                }))
                                .child(SharedString::from("Use this folder")),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.config.repo.is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("Pick a repository first"))
                .into_any_element();
        }
        let rows = self.filtered_branch_rows(cx);
        let body: AnyElement = match &self.branches {
            Loadable::Loading | Loadable::Idle => {
                popover::skeleton_rows("branch-skeleton", &theme, 4)
            }
            Loadable::Error(message) => {
                let message = message.clone();
                self.retry_row("branch-retry", &message, PickerKind::Branch, &theme, cx)
            }
            Loadable::Ready(_) if rows.is_empty() => div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No branches match"))
                .into_any_element(),
            Loadable::Ready(_) => {
                let active = self.active;
                let selected = self.config.branch.clone();
                div()
                    .id("branch-list")
                    .flex()
                    .flex_col()
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .children(rows.into_iter().enumerate().map(|(ix, branch)| {
                        let label: SharedString = branch.clone().into();
                        let is_selected = selected.as_deref() == Some(branch.as_str());
                        popover::menu_row(&theme, ix == active)
                            .id(("branch-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_branch(branch.clone(), cx);
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            .when(is_selected, |el| {
                                el.child(
                                    div()
                                        .text_color(theme.accent)
                                        .child(SharedString::from("✓")),
                                )
                            })
                    }))
                    .into_any_element()
            }
        };
        let isolated = self.config.isolated_worktree;
        div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body)
            .child(div().h(px(1.0)).mx(px(4.0)).my(px(2.0)).bg(theme.border))
            // Isolated-worktree toggle (~/.comet/worktrees/… on send).
            .child(
                popover::menu_row(&theme, false)
                    .id("branch-isolated")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.isolated_worktree = !this.config.isolated_worktree;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .text_color(if isolated {
                                theme.accent
                            } else {
                                theme.text_faint
                            })
                            .child(SharedString::from(if isolated { "☑" } else { "☐" })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_color(theme.text)
                                    .child(SharedString::from("Isolated worktree")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(
                                        "Runs in a fresh worktree of this branch",
                                    )),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_harness_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let locked = self.harness_locked(cx);
        let effective = self.effective_harness(cx);

        let rail: AnyElement = match &self.harnesses {
            Loadable::Loading | Loadable::Idle => {
                popover::skeleton_rows("harness-skeleton", &theme, 3)
            }
            Loadable::Error(message) => {
                let message = message.clone();
                self.retry_row(
                    "harness-retry",
                    &message,
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                )
            }
            Loadable::Ready(list) => {
                let descriptors: Vec<HarnessDescriptor> = visible_harnesses(list);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(descriptors.into_iter().enumerate().map(|(ix, descriptor)| {
                        let harness = descriptor.id;
                        let name: SharedString = descriptor.name.clone().into();
                        let is_active = effective == Some(harness);
                        popover::menu_row(&theme, is_active)
                            .id(("harness-row", ix))
                            .when(locked, |el| el.opacity(0.55))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_harness(harness, cx);
                            }))
                            .child(div().flex_1().child(name))
                    }))
                    .when(locked, |el| {
                        el.child(
                            div()
                                .px(px(Theme::SPACE_SM))
                                .pt(px(4.0))
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from("Harness is locked for this chat")),
                        )
                    })
                    .into_any_element()
            }
        };

        let models: AnyElement = match effective.map(|h| (h, self.models.get(&h))) {
            Some((_, Some(Loadable::Ready(models)))) => {
                let selected = self.config.model.clone();
                let models = models.clone();
                div()
                    .id("model-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_h(px(260.0))
                    .overflow_y_scroll()
                    .children(models.into_iter().enumerate().map(|(ix, model)| {
                        let label: SharedString = model.label.clone().into();
                        let id = model.id.clone();
                        let is_selected = selected.as_deref() == Some(model.id.as_str())
                            || (selected.is_none() && ix == 0);
                        popover::menu_row(&theme, false)
                            .id(("model-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_model(id.clone(), cx);
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            .when(is_selected, |el| {
                                el.child(
                                    div()
                                        .text_color(theme.accent)
                                        .child(SharedString::from("✓")),
                                )
                            })
                    }))
                    .into_any_element()
            }
            Some((_, Some(Loadable::Error(message)))) => {
                let message = message.clone();
                self.retry_row(
                    "model-retry",
                    &message,
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                )
            }
            _ => popover::skeleton_rows("model-skeleton", &theme, 4),
        };

        div()
            .flex()
            .flex_row()
            .gap(px(4.0))
            .child(
                div()
                    .w(px(150.0))
                    .flex_none()
                    .border_r_1()
                    .border_color(theme.border)
                    .pr(px(4.0))
                    .child(rail),
            )
            .child(div().flex_1().min_w_0().child(models))
            .into_any_element()
    }

    fn render_traits_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(model) = self.selected_model(cx).cloned() else {
            return popover::skeleton_rows("traits-skeleton", &theme, 3);
        };
        let harness_levels = self
            .effective_harness(cx)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.reasoning_levels.clone())
            })
            .unwrap_or_default();
        let levels = if model.reasoning_levels.is_empty() {
            harness_levels
        } else {
            model.reasoning_levels.clone()
        };
        let current = self.config.reasoning;

        let ladder: AnyElement = if levels.is_empty() {
            gpui::Empty.into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("Reasoning")),
                )
                .children(levels.into_iter().enumerate().map(|(ix, level)| {
                    let is_active = current == Some(level);
                    popover::menu_row(&theme, is_active)
                        .id(("reasoning-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_reasoning(level, cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .child(SharedString::from(reasoning_label(level))),
                        )
                        .when(is_active, |el| {
                            el.child(
                                div()
                                    .text_color(theme.accent)
                                    .child(SharedString::from("✓")),
                            )
                        })
                }))
                .into_any_element()
        };

        let selections = self.config.model_options.clone();
        let options =
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .children(model.options.iter().enumerate().map(|(opt_ix, option)| {
                    let selected_choice = selections
                        .get(&option.id)
                        .and_then(|v| v.as_str())
                        .unwrap_or(&option.default_choice)
                        .to_string();
                    let option_id = option.id.clone();
                    let default_choice = option.default_choice.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .px(px(Theme::SPACE_SM))
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(option.label.clone())),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(4.0))
                                .px(px(4.0))
                                .children(option.choices.iter().enumerate().map(
                                    |(choice_ix, choice)| {
                                        let is_active = selected_choice == choice.id;
                                        let choice_id = choice.id.clone();
                                        let option_id = option_id.clone();
                                        let is_default = choice.id == default_choice;
                                        div()
                                            .id(("trait-choice", opt_ix * 32 + choice_ix))
                                            .px(px(Theme::SPACE_SM))
                                            .py(px(2.0))
                                            .rounded(px(Theme::CONTROL_RADIUS))
                                            .border_1()
                                            .border_color(if is_active {
                                                theme.accent
                                            } else {
                                                theme.border
                                            })
                                            .text_size(px(11.0))
                                            .text_color(if is_active {
                                                theme.text
                                            } else {
                                                theme.text_muted
                                            })
                                            .cursor_pointer()
                                            .hover(|s| s.bg(theme.element_hover))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.pick_option(
                                                    option_id.clone(),
                                                    choice_id.clone(),
                                                    is_default,
                                                    cx,
                                                );
                                            }))
                                            .child(SharedString::from(choice.label.clone()))
                                    },
                                )),
                        )
                }));

        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .py(px(2.0))
            .child(ladder)
            .child(options)
            .into_any_element()
    }
}

/// Production pickers hide the mock harness unless it's all there is.
pub fn visible_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    let real: Vec<HarnessDescriptor> = list
        .iter()
        .filter(|d| d.id != HarnessId::Mock)
        .cloned()
        .collect();
    if real.is_empty() { list.to_vec() } else { real }
}

/// Attach the (single) open popover overlay to its trigger chip.
fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu(id, element));
    }
    chip
}

impl Render for Pickers {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let new_chat = self.state.read(cx).selected_chat.is_none();

        let repo_label: SharedString = self
            .config
            .repo
            .as_ref()
            .map(|r| SharedString::from(r.name.clone()))
            .unwrap_or_else(|| SharedString::from("Repo"));
        let branch_label: SharedString = self
            .config
            .branch
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from("Branch"));
        let model_label: SharedString = {
            let model = self.selected_model(cx).map(|m| m.label.clone());
            let harness = self.effective_harness(cx).and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.name.clone())
            });
            match (harness, model) {
                (Some(h), Some(m)) => format!("{h} · {m}").into(),
                (Some(h), None) => h.into(),
                _ => "Model".into(),
            }
        };
        let traits_set = traits_summary(
            self.selected_model(cx),
            self.config.reasoning,
            &self.config.model_options,
        );
        let traits_label: SharedString = traits_set
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from("Traits"));

        // Render the open popover's body first (mutable borrow), then the chips.
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.open {
            Some(PickerKind::Repo) => {
                let content = self.render_repo_popover(cx);
                Some((PickerKind::Repo, self.popover_frame(300.0, content, cx)))
            }
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(260.0, content, cx)))
            }
            Some(PickerKind::HarnessModel) => {
                let content = self.render_harness_model_popover(cx);
                Some((
                    PickerKind::HarnessModel,
                    self.popover_frame(380.0, content, cx),
                ))
            }
            Some(PickerKind::Traits) => {
                let content = self.render_traits_popover(cx);
                Some((PickerKind::Traits, self.popover_frame(280.0, content, cx)))
            }
            None => None,
        };

        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_wrap()
            .gap(px(6.0));
        if new_chat {
            let repo_set = self.config.repo.is_some();
            let branch_set = self.config.branch.is_some();
            let repo_chip = self.trigger_chip(PickerKind::Repo, repo_label, repo_set, &theme, cx);
            row = row.child(attach_overlay(
                repo_chip,
                &mut overlay,
                PickerKind::Repo,
                "repo-popover",
            ));
            let branch_chip =
                self.trigger_chip(PickerKind::Branch, branch_label, branch_set, &theme, cx);
            row = row.child(attach_overlay(
                branch_chip,
                &mut overlay,
                PickerKind::Branch,
                "branch-popover",
            ));
        }
        let model_chip = self.trigger_chip(
            PickerKind::HarnessModel,
            model_label,
            self.config.model.is_some(),
            &theme,
            cx,
        );
        row = row.child(attach_overlay(
            model_chip,
            &mut overlay,
            PickerKind::HarnessModel,
            "model-popover",
        ));
        let traits_chip = self.trigger_chip(
            PickerKind::Traits,
            traits_label,
            traits_set.is_some(),
            &theme,
            cx,
        );
        row.child(attach_overlay(
            traits_chip,
            &mut overlay,
            PickerKind::Traits,
            "traits-popover",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{FolderEntry, Model, ModelOption, ModelOptionChoice};

    fn repo(name: &str, path: &str) -> Repo {
        Repo {
            path: path.into(),
            name: name.into(),
            default_branch: Some("main".into()),
        }
    }

    #[test]
    fn repos_order_recents_first_then_alphabetical() {
        let repos = vec![
            repo("zebra", "/r/zebra"),
            repo("Alpha", "/r/alpha"),
            repo("mango", "/r/mango"),
        ];
        let recents = vec!["/r/mango".to_string(), "/r/missing".to_string()];
        let ordered = order_repos(repos, &recents);
        let names: Vec<&str> = ordered.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["mango", "Alpha", "zebra"]);
        // No recents → purely alphabetical (case-insensitive).
        let ordered = order_repos(vec![repo("b", "/b"), repo("A", "/a")], &[]);
        assert_eq!(ordered[0].name, "A");
    }

    #[test]
    fn recent_cwds_dedupe_in_recency_order() {
        use chrono::TimeDelta;
        let base = chrono::DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        let chat = |id: &str, cwd: Option<&str>, min: i64| Chat {
            id: id.into(),
            device_id: "d".into(),
            title: None,
            archived: false,
            cwd: cwd.map(str::to_string),
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: base + TimeDelta::minutes(min),
        };
        // Input is already sidebar-sorted; recent_cwds just walks it.
        let chats = vec![
            chat("a", Some("/dev/comet"), 3),
            chat("b", Some("/dev/zed"), 2),
            chat("c", Some("/dev/comet"), 1),
            chat("d", None, 0),
            chat("e", Some(""), 0),
        ];
        assert_eq!(
            recent_cwds(&chats),
            vec!["/dev/comet".to_string(), "/dev/zed".to_string()]
        );
    }

    #[test]
    fn traits_summary_formats_non_defaults() {
        let model = Model {
            id: "opus".into(),
            label: "Opus".into(),
            reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            options: vec![
                ModelOption {
                    id: "context".into(),
                    label: "Context window".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "standard".into(),
                            label: "Standard".into(),
                        },
                        ModelOptionChoice {
                            id: "1m".into(),
                            label: "1M".into(),
                        },
                    ],
                    default_choice: "standard".into(),
                },
                ModelOption {
                    id: "speed".into(),
                    label: "Speed".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "normal".into(),
                            label: "Normal".into(),
                        },
                        ModelOptionChoice {
                            id: "fast".into(),
                            label: "Fast".into(),
                        },
                    ],
                    default_choice: "normal".into(),
                },
            ],
        };
        let mut selections = serde_json::Map::new();
        selections.insert("context".into(), serde_json::Value::String("1m".into()));
        selections.insert("speed".into(), serde_json::Value::String("fast".into()));
        assert_eq!(
            traits_summary(Some(&model), Some(ReasoningLevel::High), &selections),
            Some("High · 1M · Fast".to_string())
        );
        // All defaults → no summary.
        assert_eq!(
            traits_summary(Some(&model), None, &serde_json::Map::new()),
            None
        );
        // Default-choice selections don't count as non-default.
        let mut defaults = serde_json::Map::new();
        defaults.insert("speed".into(), serde_json::Value::String("normal".into()));
        assert_eq!(traits_summary(Some(&model), None, &defaults), None);
        // Reasoning shows without a model too.
        assert_eq!(
            traits_summary(
                None,
                Some(ReasoningLevel::Ultrathink),
                &serde_json::Map::new()
            ),
            Some("Ultrathink".to_string())
        );
    }

    #[test]
    fn folder_paths_and_breadcrumbs() {
        assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/"), Some("/".to_string()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(child_path("/home", "w"), "/home/w");
        assert_eq!(child_path("/", "home"), "/home");
        let crumbs = breadcrumbs("/home/w/dev");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                FolderEntry {
                    name: "comet".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        // Files never show as rows.
        assert_eq!(browser_rows(&listing).len(), 2);
        // Enter on a plain dir descends; on a repo picks.
        assert_eq!(
            browse_enter(&listing, 0),
            Some(BrowseEnter::Descend("/home/w/dev".into()))
        );
        assert_eq!(
            browse_enter(&listing, 1),
            Some(BrowseEnter::Pick("/home/w/comet".into()))
        );
        // Out-of-range → None.
        assert_eq!(browse_enter(&listing, 5), None);
    }

    #[test]
    fn draft_chat_config_requires_harness() {
        let mut draft = DraftConfig::default();
        assert!(draft.chat_config().is_none());
        draft.harness = Some(HarnessId::ClaudeCode);
        draft.model = Some("opus".into());
        draft.reasoning = Some(ReasoningLevel::High);
        let config = draft.chat_config().expect("harness set");
        assert_eq!(config.harness, HarnessId::ClaudeCode);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn mock_harness_hidden_unless_alone() {
        let descriptor = |id: HarnessId, name: &str| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: comet_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
        };
        let mixed = vec![
            descriptor(HarnessId::Mock, "Mock"),
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
        ];
        let visible = visible_harnesses(&mixed);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, HarnessId::ClaudeCode);
        let only_mock = vec![descriptor(HarnessId::Mock, "Mock")];
        assert_eq!(visible_harnesses(&only_mock).len(), 1);
    }
}
