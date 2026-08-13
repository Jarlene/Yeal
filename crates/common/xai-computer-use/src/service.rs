use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::backend::{ActOutcome, BackendError, TextPage, UiBackend};
use crate::model::{
    Action, FindRootsRequest, ObserveMode, ObserveRequest, RootInfo, UiNode, UiSnapshot,
};
use crate::runtime::{ResourceScheduler, StaleStateError, StateStore, StoredState};

const DEFAULT_STATE_LIMIT: usize = 128;
/// Hard safety bound on stored outline nodes. Backends may return larger
/// trees; the service truncates beyond this and marks the boundary node.
const MAX_STORED_NODES: usize = 20_000;
/// Fold budget applied to the observation returned to callers. The stored
/// tree remains complete so search/expand/inspect query the full outline.
const FOLD_MAX_DEPTH: usize = 2;
const FOLD_MAX_NODES: usize = 150;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("state not found: {0}")]
    StateNotFound(String),
    #[error("element reference not found: {0}")]
    ElementNotFound(String),
    #[error("action `{action}` is not supported by element {element_ref}")]
    UnsupportedAction { action: String, element_ref: String },
    #[error("invalid action: {0}")]
    InvalidAction(String),
    #[error("no browser backend configured")]
    NoBrowserBackend,
    #[error(transparent)]
    Stale(#[from] StaleStateError),
    #[error(transparent)]
    Backend(#[from] BackendError),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Observation {
    pub state_id: String,
    pub resource_key: String,
    pub epoch: u64,
    pub root: RootInfo,
    pub look_id: String,
    pub captured_at_ms: u64,
    /// Folded outline (see [`FOLD_MAX_NODES`]); the stored tree is complete.
    pub outline: UiNode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<crate::model::ImageCapture>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchMatch {
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub role: String,
    pub label: String,
    pub path: String,
    pub match_reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResponse {
    pub state_id: String,
    pub matches: Vec<SearchMatch>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExpandResponse {
    pub state_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub node: UiNode,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InspectResponse {
    pub state_id: String,
    pub node: UiNode,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExpectResult {
    pub satisfied: bool,
    pub timed_out: bool,
}

/// One node-level change in a successor diff.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OutlineChange {
    Added { element_ref: String, node: UiNode },
    Updated { element_ref: String, node: UiNode },
    Removed { element_ref: String },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OutlineDiff {
    pub changes: Vec<OutlineChange>,
    pub changed_node_count: usize,
    pub full_node_count: usize,
    pub use_full_view: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ActResponse {
    pub previous_state_id: String,
    pub state_id: String,
    pub resource_key: String,
    pub epoch: u64,
    pub executed: usize,
    #[serde(default)]
    pub outcomes: Vec<ActOutcome>,
    /// Index of the first step whose outcome was verified `didnt` (or absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<ExpectResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<OutlineDiff>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WaitResponse {
    pub state_id: String,
    pub satisfied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
}

/// Condition checked after `act_ui`; belongs to the base state of the act.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ExpectCondition {
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub element_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default = "default_present")]
    pub until: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ActRequest {
    pub state_id: String,
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<ExpectCondition>,
}

#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct WaitForRequest {
    pub state_id: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub element_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default = "default_present")]
    pub until: String,
    /// When present and non-zero, live-poll the root for up to this many ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

fn default_present() -> String {
    "present".into()
}

type StateValue = (RootInfo, UiNode, String, u64);

/// Orchestrates state-scoped computer-use operations over a platform backend.
pub struct ComputerUseService {
    backend: Arc<dyn UiBackend>,
    states: Arc<StateStore<StateValue>>,
    scheduler: Arc<ResourceScheduler>,
}

impl std::fmt::Debug for ComputerUseService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerUseService").finish_non_exhaustive()
    }
}

impl ComputerUseService {
    pub fn new(backend: Arc<dyn UiBackend>) -> Self {
        Self::with_state_limit(backend, DEFAULT_STATE_LIMIT)
    }

    /// Process-wide shared instance, built lazily from the environment
    /// (`COMPUTER_USE_CDP_PORT` / `COMPUTER_USE_BROWSER_PATH` /
    /// `COMPUTER_USE_HEADLESS`).
    ///
    /// Every computer-use tool registered through the grok-tools tool pack
    /// wraps this same instance, so captured UI state is shared across the
    /// whole tool family (and across toolset rebuilds). The service keeps its
    /// own per-session state keyed by session, so one shared instance is safe
    /// for all sessions.
    pub fn shared() -> Arc<Self> {
        static SHARED: std::sync::OnceLock<Arc<ComputerUseService>> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| {
                Arc::new(Self::new(crate::backends::native_backend(
                    &crate::backends::ComputerUseConfig::from_env(),
                )))
            })
            .clone()
    }

    pub fn with_state_limit(backend: Arc<dyn UiBackend>, state_limit: usize) -> Self {
        Self {
            backend,
            states: Arc::new(StateStore::new(state_limit)),
            scheduler: Arc::new(ResourceScheduler::default()),
        }
    }

    pub fn backend(&self) -> &Arc<dyn UiBackend> {
        &self.backend
    }

    pub async fn find_roots(
        &self,
        request: FindRootsRequest,
    ) -> Result<Vec<RootInfo>, ServiceError> {
        Ok(self.backend.find_roots(request).await?)
    }

    pub async fn observe(
        &self,
        root_ref: &str,
        mode: ObserveMode,
    ) -> Result<Observation, ServiceError> {
        let roots = self.backend.find_roots(FindRootsRequest::default()).await?;
        let root = roots
            .into_iter()
            .find(|root| root.root_ref == root_ref)
            .ok_or_else(|| ServiceError::ElementNotFound(root_ref.into()))?;
        let resource_key = root.resource_key.clone();
        let (snapshot, epoch) = self
            .scheduler
            .read(&resource_key, |_| async move {
                self.backend
                    .observe(&root, ObserveRequest { mode, ..Default::default() })
                    .await
            })
            .await;
        self.save_snapshot(snapshot?, epoch).await
    }

    pub async fn search(
        &self,
        state_id: &str,
        text: Option<&str>,
        role: Option<&str>,
        capability: Option<&str>,
    ) -> Result<SearchResponse, ServiceError> {
        let state = self.state(state_id).await?;
        let mut nodes = Vec::new();
        state.value.1.walk(&mut nodes);
        let needle = text.map(str::to_lowercase);
        let matches = nodes
            .into_iter()
            .filter(|node| {
                needle.as_ref().is_none_or(|needle| {
                    [
                        node.title.as_str(),
                        node.value.as_str(),
                        node.description.as_str(),
                    ]
                    .into_iter()
                    .any(|value| value.to_lowercase().contains(needle))
                })
            })
            .filter(|node| role.is_none_or(|wanted| node.role.eq_ignore_ascii_case(wanted)))
            .filter(|node| capability.is_none_or(|wanted| capability_matches(node, wanted)))
            .take(100)
            .map(|node| SearchMatch {
                element_ref: node.element_ref.clone(),
                role: node.role.clone(),
                label: display_label(node),
                path: node.element_ref.clone(),
                match_reason: "filter".into(),
            })
            .collect();
        Ok(SearchResponse {
            state_id: state.state_id.clone(),
            matches,
        })
    }

    pub async fn expand(
        &self,
        state_id: &str,
        element_ref: &str,
        depth: usize,
    ) -> Result<ExpandResponse, ServiceError> {
        let state = self.state(state_id).await?;
        let node = state
            .value
            .1
            .find(element_ref)
            .ok_or_else(|| ServiceError::ElementNotFound(element_ref.into()))?;
        let mut node = node.clone();
        trim_depth(&mut node, depth.min(8));
        Ok(ExpandResponse {
            state_id: state.state_id.clone(),
            element_ref: element_ref.into(),
            node,
        })
    }

    pub async fn inspect(
        &self,
        state_id: &str,
        element_ref: &str,
    ) -> Result<InspectResponse, ServiceError> {
        let state = self.state(state_id).await?;
        let node = state
            .value
            .1
            .find(element_ref)
            .ok_or_else(|| ServiceError::ElementNotFound(element_ref.into()))?;
        Ok(InspectResponse {
            state_id: state.state_id.clone(),
            node: node.clone(),
        })
    }

    /// Read a bounded page of text from an element. When the backend supports
    /// live reads this pages the element's current text; otherwise it falls
    /// back to the stored observation.
    pub async fn read_text(
        &self,
        state_id: &str,
        element_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TextPage, ServiceError> {
        let state = self.state(state_id).await?;
        let node = state
            .value
            .1
            .find(element_ref)
            .ok_or_else(|| ServiceError::ElementNotFound(element_ref.into()))?;
        let limit = limit.clamp(1, 4000);
        if let Some(wire_ref) = node.wire_ref.as_deref() {
            if let Ok(page) = self
                .backend
                .read_text(&state.value.0, wire_ref, offset, limit)
                .await
            {
                return Ok(page);
            }
        }
        let text = if !node.value.is_empty() {
            node.value.clone()
        } else if !node.title.is_empty() {
            node.title.clone()
        } else {
            node.description.clone()
        };
        let total = text.chars().count();
        let offset = offset.min(total);
        let page: String = text.chars().skip(offset).take(limit).collect();
        Ok(TextPage {
            text: page,
            offset,
            limit,
            total_chars: total,
            has_more: offset + limit < total,
        })
    }

    pub async fn wait_for(&self, request: WaitForRequest) -> Result<WaitResponse, ServiceError> {
        let base = self.state(&request.state_id).await?;
        let timeout_ms = request.timeout_ms.unwrap_or(0);
        if timeout_ms == 0 {
            let found = walk_matches(&base.value.1, &|node| condition_matches(node, &request));
            return Ok(WaitResponse {
                state_id: base.state_id.clone(),
                satisfied: present_until(&request.until, found),
                timed_out: None,
            });
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut satisfied;
        loop {
            let snapshot = self
                .backend
                .observe(&base.value.0, ObserveRequest::default())
                .await?;
            let found = walk_matches(&snapshot.outline, &|node| condition_matches(node, &request));
            satisfied = present_until(&request.until, found);
            if satisfied || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(WAIT_POLL_INTERVAL).await;
        }
        let timed_out = !satisfied;
        Ok(WaitResponse {
            state_id: base.state_id.clone(),
            satisfied,
            timed_out: Some(timed_out),
        })
    }

    pub async fn act(&self, request: ActRequest) -> Result<ActResponse, ServiceError> {
        if request.actions.is_empty() {
            return Err(ServiceError::InvalidAction(
                "actions must not be empty".into(),
            ));
        }
        let base = self.state(&request.state_id).await?;
        let prepared = prepare_actions(&base.value.1, &request.actions)?;
        let root = base.value.0.clone();
        let resource_key = base.resource_key.clone();
        let backend = Arc::clone(&self.backend);
        let act_root = root.clone();
        let act_actions = prepared.clone();
        let (result, epoch) = self
            .scheduler
            .write(&resource_key, base.epoch, move |_| async move {
                let outcomes = backend.act(&act_root, &act_actions).await?;
                let snapshot = backend
                    .observe(&act_root, ObserveRequest::default())
                    .await?;
                Ok::<_, BackendError>((outcomes, snapshot))
            })
            .await?;
        let (outcomes, snapshot) = result?;
        let successor = self.save_snapshot(snapshot, epoch).await?;

        let mut stopped_at = None;
        for (index, outcome) in outcomes.iter().enumerate() {
            if *outcome == ActOutcome::Didnt {
                stopped_at = Some(index);
                break;
            }
        }

        let expect = if let Some(condition) = request.expect {
            let timeout_ms = condition.timeout_ms.unwrap_or(10_000);
            let wait = WaitForRequest {
                state_id: successor.state_id.clone(),
                element_ref: condition.element_ref,
                scope_ref: condition.scope_ref,
                text: condition.text,
                role: condition.role,
                value: condition.value,
                until: condition.until,
                timeout_ms: Some(timeout_ms),
            };
            let result = self.wait_for(wait).await?;
            Some(ExpectResult {
                satisfied: result.satisfied,
                timed_out: result.timed_out.unwrap_or(false),
            })
        } else {
            None
        };

        let diff = diff_outlines(&base.value.1, &successor.outline);

        Ok(ActResponse {
            previous_state_id: base.state_id.clone(),
            state_id: successor.state_id,
            resource_key,
            epoch,
            executed: prepared.len(),
            outcomes,
            stopped_at,
            expect,
            diff,
        })
    }

    /// Launch a managed browser and observe its first page.
    pub async fn launch_browser(&self, url: Option<&str>) -> Result<Observation, ServiceError> {
        let roots = self.backend.launch_browser(url).await?;
        let root = roots
            .into_iter()
            .next()
            .ok_or(ServiceError::NoBrowserBackend)?;
        self.observe(&root.root_ref, ObserveMode::Fused).await
    }

    /// Navigate a browser root and return its successor observation.
    pub async fn navigate_browser(&self, state_id: &str, url: &str) -> Result<Observation, ServiceError> {
        let base = self.state(state_id).await?;
        self.backend.navigate(&base.value.0, url).await?;
        let resource_key = base.resource_key.clone();
        let root = base.value.0.clone();
        let (snapshot, epoch) = self
            .scheduler
            .read(&resource_key, |_| async move {
                self.backend
                    .observe(&root, ObserveRequest { mode: ObserveMode::Fused, ..Default::default() })
                    .await
            })
            .await;
        self.save_snapshot(snapshot?, epoch).await
    }

    /// Evaluate a bounded JavaScript expression in a browser root.
    pub async fn evaluate_browser(&self, state_id: &str, expression: &str) -> Result<String, ServiceError> {
        let base = self.state(state_id).await?;
        Ok(self.backend.evaluate(&base.value.0, expression).await?)
    }

    async fn save_snapshot(
        &self,
        snapshot: UiSnapshot,
        epoch: u64,
    ) -> Result<Observation, ServiceError> {
        let mut outline = snapshot.outline;
        cap_nodes(&mut outline, MAX_STORED_NODES);
        let look_id = uuid::Uuid::now_v7().to_string();
        let value = (
            snapshot.root.clone(),
            outline.clone(),
            look_id.clone(),
            snapshot.captured_at_ms,
        );
        let record = self
            .states
            .create(snapshot.root.resource_key.clone(), epoch, value)
            .await;
        let mut folded = outline.clone();
        fold_outline(&mut folded, FOLD_MAX_DEPTH, FOLD_MAX_NODES);
        Ok(Observation {
            state_id: record.state_id.clone(),
            resource_key: record.resource_key.clone(),
            epoch,
            root: snapshot.root,
            look_id,
            captured_at_ms: snapshot.captured_at_ms,
            outline: folded,
            image: snapshot.image,
        })
    }

    async fn state(&self, state_id: &str) -> Result<Arc<StoredState<StateValue>>, ServiceError> {
        self.states
            .get(state_id)
            .await
            .ok_or_else(|| ServiceError::StateNotFound(state_id.into()))
    }
}

/// Resolve `@eN` refs to backend wire refs and validate capabilities before
/// dispatch.
fn prepare_actions(root: &UiNode, actions: &[Action]) -> Result<Vec<Action>, ServiceError> {
    actions
        .iter()
        .map(|action| {
            let prepared = match action {
                Action::Press { element_ref, .. } => {
                    let node = require_node(root, element_ref)?;
                    if !node.can_press {
                        return Err(ServiceError::UnsupportedAction {
                            action: "press".into(),
                            element_ref: element_ref.clone(),
                        });
                    }
                    Action::Press {
                        element_ref: element_ref.clone(),
                        wire_ref: node.wire_ref.clone(),
                    }
                }
                Action::Click {
                    element_ref,
                    wire_ref: _,
                    x,
                    y,
                    button,
                    click_count,
                } => {
                    let (element_ref, wire_ref) = match element_ref {
                        Some(element_ref) if !element_ref.is_empty() => {
                            let node = require_node(root, element_ref)?;
                            if !node.can_press {
                                return Err(ServiceError::UnsupportedAction {
                                    action: "click".into(),
                                    element_ref: element_ref.clone(),
                                });
                            }
                            (Some(element_ref.clone()), node.wire_ref.clone())
                        }
                        Some(_) | None if x.is_some() && y.is_some() => (None, None),
                        _ => {
                            return Err(ServiceError::InvalidAction(
                                "click requires either ref or both x and y".into(),
                            ))
                        }
                    };
                    Action::Click {
                        element_ref,
                        wire_ref,
                        x: *x,
                        y: *y,
                        button: button.clone(),
                        click_count: *click_count,
                    }
                }
                Action::SetText { element_ref, text, .. } => {
                    let node = require_node(root, element_ref)?;
                    if !node.can_set_value && !node.is_text_input {
                        return Err(ServiceError::UnsupportedAction {
                            action: "setText".into(),
                            element_ref: element_ref.clone(),
                        });
                    }
                    Action::SetText {
                        element_ref: element_ref.clone(),
                        wire_ref: node.wire_ref.clone(),
                        text: text.clone(),
                    }
                }
                Action::TypeText {
                    element_ref,
                    text,
                    ..
                } => {
                    let wire_ref = element_ref
                        .as_ref()
                        .filter(|value| !value.is_empty())
                        .map(|element_ref| {
                            let node = require_node(root, element_ref)?;
                            if !node.can_set_value && !node.is_text_input {
                                return Err(ServiceError::UnsupportedAction {
                                    action: "typeText".into(),
                                    element_ref: element_ref.clone(),
                                });
                            }
                            Ok(node.wire_ref.clone())
                        })
                        .transpose()?
                        .flatten();
                    Action::TypeText {
                        element_ref: element_ref.clone(),
                        wire_ref,
                        text: text.clone(),
                    }
                }
                Action::Keypress { element_ref, keys, .. } => {
                    let wire_ref = element_ref
                        .as_ref()
                        .filter(|value| !value.is_empty())
                        .map(|element_ref| {
                            let node = require_node(root, element_ref)?;
                            if !node.can_focus && !node.is_text_input {
                                return Err(ServiceError::UnsupportedAction {
                                    action: "keypress".into(),
                                    element_ref: element_ref.clone(),
                                });
                            }
                            Ok(node.wire_ref.clone())
                        })
                        .transpose()?
                        .flatten();
                    Action::Keypress {
                        element_ref: element_ref.clone(),
                        wire_ref,
                        keys: keys.clone(),
                    }
                }
                Action::Scroll {
                    element_ref,
                    scroll_x,
                    scroll_y,
                    ..
                } => {
                    let wire_ref = element_ref
                        .as_ref()
                        .filter(|value| !value.is_empty())
                        .map(|element_ref| {
                            let node = require_node(root, element_ref)?;
                            if !node.can_scroll {
                                return Err(ServiceError::UnsupportedAction {
                                    action: "scroll".into(),
                                    element_ref: element_ref.clone(),
                                });
                            }
                            Ok(node.wire_ref.clone())
                        })
                        .transpose()?
                        .flatten();
                    Action::Scroll {
                        element_ref: element_ref.clone(),
                        wire_ref,
                        scroll_x: *scroll_x,
                        scroll_y: *scroll_y,
                    }
                }
                Action::Drag { path } => {
                    if path.len() < 2 {
                        return Err(ServiceError::InvalidAction(
                            "drag requires at least two points".into(),
                        ));
                    }
                    Action::Drag { path: path.clone() }
                }
                Action::MoveMouse { x, y } => Action::MoveMouse { x: *x, y: *y },
            };
            Ok(prepared)
        })
        .collect()
}

fn require_node<'a>(root: &'a UiNode, element_ref: &str) -> Result<&'a UiNode, ServiceError> {
    root.find(element_ref)
        .ok_or_else(|| ServiceError::ElementNotFound(element_ref.into()))
}

fn capability_matches(node: &UiNode, capability: &str) -> bool {
    match capability {
        "press" | "click" => node.can_press,
        "focus" => node.can_focus,
        "setText" | "set_value" => node.can_set_value,
        "scroll" => node.can_scroll,
        "increment" => node.can_increment,
        "decrement" => node.can_decrement,
        "text_input" => node.is_text_input,
        _ => false,
    }
}

fn display_label(node: &UiNode) -> String {
    [
        node.title.as_str(),
        node.value.as_str(),
        node.description.as_str(),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or(&node.role)
    .into()
}

fn condition_matches(node: &UiNode, request: &WaitForRequest) -> bool {
    let within_scope = request
        .scope_ref
        .as_deref()
        .is_none_or(|scope| node.element_ref == scope || node_has_ref(node, scope));
    within_scope
        && request
            .element_ref
            .as_deref()
            .is_none_or(|value| node.element_ref == value)
        && request
            .text
            .as_deref()
            .is_none_or(|value| display_label(node).contains(value))
        && request
            .role
            .as_deref()
            .is_none_or(|value| node.role.eq_ignore_ascii_case(value))
        && request
            .value
            .as_deref()
            .is_none_or(|value| node.value == value)
}

fn node_has_ref(node: &UiNode, scope: &str) -> bool {
    node.element_ref == scope || node.children.iter().any(|child| node_has_ref(child, scope))
}

fn present_until(until: &str, found: bool) -> bool {
    if until == "absent" {
        !found
    } else {
        found
    }
}

fn walk_matches(node: &UiNode, predicate: &dyn Fn(&UiNode) -> bool) -> bool {
    if predicate(node) {
        return true;
    }
    node.children
        .iter()
        .any(|child| walk_matches(child, predicate))
}

fn trim_depth(node: &mut UiNode, depth: usize) {
    if depth == 0 {
        node.children.clear();
        return;
    }
    for child in &mut node.children {
        trim_depth(child, depth - 1);
    }
}

/// Hard cap on stored tree size. Marks the boundary node `truncated`.
fn cap_nodes(node: &mut UiNode, limit: usize) {
    fn trim(node: &mut UiNode, remaining: &mut usize) {
        if *remaining == 0 {
            node.children.clear();
            node.truncated = true;
            return;
        }
        *remaining -= 1;
        let mut kept = Vec::new();
        for mut child in node.children.drain(..) {
            if *remaining == 0 {
                node.truncated = true;
                break;
            }
            trim(&mut child, remaining);
            kept.push(child);
        }
        node.children = kept;
    }
    let mut remaining = limit;
    trim(node, &mut remaining);
}

/// Fold the returned observation to a bounded view while the stored tree
/// stays complete. Nodes whose children were dropped are marked `truncated`.
fn fold_outline(node: &mut UiNode, max_depth: usize, max_nodes: usize) {
    fn fold(node: &mut UiNode, depth: usize, max_depth: usize, remaining: &mut usize) {
        if *remaining == 0 {
            node.children.clear();
            node.truncated = true;
            return;
        }
        *remaining -= 1;
        let unfold = depth < max_depth;
        if !unfold {
            let had_children = !node.children.is_empty();
            node.children.clear();
            node.truncated = had_children;
            return;
        }
        let mut kept = Vec::new();
        for mut child in node.children.drain(..) {
            if *remaining == 0 {
                node.truncated = true;
                break;
            }
            fold(&mut child, depth + 1, max_depth, remaining);
            kept.push(child);
        }
        node.children = kept;
    }
    let mut remaining = max_nodes;
    fold(node, 0, max_depth, &mut remaining);
}

/// Identity-based successor diff. Matches nodes by wire ref (falling back to
/// element ref when the backend provides no wire identity).
fn diff_outlines(before: &UiNode, after: &UiNode) -> Option<OutlineDiff> {
    let mut before_nodes = Vec::new();
    let mut after_nodes = Vec::new();
    before.walk(&mut before_nodes);
    after.walk(&mut after_nodes);

    let identity = |node: &UiNode| -> String {
        node.wire_ref
            .clone()
            .unwrap_or_else(|| node.element_ref.clone())
    };

    let before_ids: std::collections::HashSet<String> =
        before_nodes.iter().map(|node| identity(node)).collect();
    let after_ids: std::collections::HashSet<String> =
        after_nodes.iter().map(|node| identity(node)).collect();

    let removed: Vec<_> = before_nodes
        .iter()
        .filter(|node| !after_ids.contains(&identity(node)))
        .map(|node| OutlineChange::Removed {
            element_ref: node.element_ref.clone(),
        })
        .collect();

    let mut added = Vec::new();
    let mut updated = Vec::new();
    for node in &after_nodes {
        let id = identity(node);
        if !before_ids.contains(&id) {
            added.push(OutlineChange::Added {
                element_ref: node.element_ref.clone(),
                node: (*node).clone(),
            });
        } else if let Some(before) = before_nodes.iter().find(|candidate| identity(candidate) == id)
        {
            if node_fields_changed(before, node) {
                updated.push(OutlineChange::Updated {
                    element_ref: node.element_ref.clone(),
                    node: (*node).clone(),
                });
            }
        }
    }

    let mut changes: Vec<OutlineChange> = Vec::new();
    changes.extend(added);
    changes.extend(updated);
    changes.extend(removed);

    let root_changed = identity(before) != identity(after);
    let change_budget_exceeded = changes.len() > 40;
    let use_full_view = root_changed || change_budget_exceeded;
    let reason = if root_changed {
        Some("root_replaced".into())
    } else if change_budget_exceeded {
        Some("change_budget_exceeded".into())
    } else if changes.is_empty() {
        None
    } else {
        None
    };
    if use_full_view {
        return None;
    }
    if changes.is_empty() {
        return None;
    }
    Some(OutlineDiff {
        changed_node_count: changes.len(),
        full_node_count: after_nodes.len(),
        use_full_view: false,
        reason,
        changes,
    })
}

fn node_fields_changed(before: &UiNode, after: &UiNode) -> bool {
    before.title != after.title
        || before.value != after.value
        || before.description != after.description
        || before.focused != after.focused
        || before.bounds != after.bounds
        || before.actions != after.actions
}
