use async_trait::async_trait;
use thiserror::Error;

use crate::model::{Action, FindRootsRequest, ObserveRequest, RootInfo, UiNode, UiSnapshot};

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("unsupported computer-use operation: {0}")]
    Unsupported(String),
    #[error("platform backend failed: {0}")]
    Failed(String),
}

/// Honest outcome of an acted-on element, mirroring the pi-computer-use
/// `worked` / `didnt` / `unknown` contract. A backend must never report
/// [`ActOutcome::Worked`] purely because input was posted; outcome must be
/// grounded in observed evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActOutcome {
    /// The action's intended effect was verified after delivery.
    Worked,
    /// Delivery completed but the intended effect was verified absent.
    Didnt,
    /// The effect could not be verified either way. Callers must re-observe.
    Unknown,
}

/// One page of live element text, paged by character offset.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TextPage {
    pub text: String,
    pub offset: usize,
    pub limit: usize,
    pub total_chars: usize,
    pub has_more: bool,
}

/// Platform boundary. Implementations must treat an action as potentially
/// having taken effect if it returns an error after dispatch begins.
///
/// Backends receive actions whose `wire_ref` fields have already been
/// resolved by the service from the owning observation. They must not attempt
/// to resolve `@eN` refs.
#[async_trait]
pub trait UiBackend: Send + Sync + 'static {
    async fn find_roots(&self, request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError>;
    async fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError>;
    /// Execute a batch of actions against one root and return one outcome per
    /// action, in order. Backends own preflight, delivery, and verification.
    async fn act(&self, root: &RootInfo, actions: &[Action]) -> Result<Vec<ActOutcome>, BackendError>;

    /// Read a bounded page of live text from an element. The default
    /// implementation is unsupported; service falls back to the stored
    /// observation.
    async fn read_text(
        &self,
        _root: &RootInfo,
        _wire_ref: &str,
        _offset: usize,
        _limit: usize,
    ) -> Result<TextPage, BackendError> {
        Err(BackendError::Unsupported("read_text".into()))
    }

    /// Navigate a browser root (CDP) to a URL. Default: unsupported.
    async fn navigate(&self, _root: &RootInfo, _url: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("navigate".into()))
    }

    /// Evaluate a bounded JavaScript expression in a browser root (CDP).
    /// Default: unsupported.
    async fn evaluate(&self, _root: &RootInfo, _expression: &str) -> Result<String, BackendError> {
        Err(BackendError::Unsupported("evaluate".into()))
    }

    /// Launch a managed browser (CDP) and return its page roots. Default:
    /// unsupported.
    async fn launch_browser(&self, _url: Option<&str>) -> Result<Vec<RootInfo>, BackendError> {
        Err(BackendError::Unsupported("launch_browser".into()))
    }
}

/// Explicit backend used until a native platform adapter is configured.
#[derive(Debug, Default)]
pub struct UnsupportedBackend;

#[async_trait]
impl UiBackend for UnsupportedBackend {
    async fn find_roots(&self, _request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
        Err(BackendError::Unsupported("find_roots".into()))
    }

    async fn observe(
        &self,
        _root: &RootInfo,
        _request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError> {
        Err(BackendError::Unsupported("observe_ui".into()))
    }

    async fn act(&self, _root: &RootInfo, _actions: &[Action]) -> Result<Vec<ActOutcome>, BackendError> {
        Err(BackendError::Unsupported("act_ui".into()))
    }
}

/// Deterministic backend for integration tests and embedders that already own
/// a UI tree. It records dispatched actions and replaces the root snapshot
/// after each successful action batch.
#[derive(Debug)]
pub struct InMemoryBackend {
    roots: tokio::sync::RwLock<Vec<(RootInfo, UiNode)>>,
    actions: tokio::sync::Mutex<Vec<Vec<Action>>>,
}

impl InMemoryBackend {
    pub fn new(roots: Vec<(RootInfo, UiNode)>) -> Self {
        Self {
            roots: tokio::sync::RwLock::new(roots),
            actions: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    pub async fn actions(&self) -> Vec<Vec<Action>> {
        self.actions.lock().await.clone()
    }
}

#[async_trait]
impl UiBackend for InMemoryBackend {
    async fn find_roots(&self, request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
        let roots = self.roots.read().await;
        Ok(roots
            .iter()
            .filter(|(root, _)| {
                request
                    .app
                    .as_ref()
                    .is_none_or(|app| root.app.as_ref() == Some(app))
                    && request
                        .bundle_id
                        .as_ref()
                        .is_none_or(|bundle| root.bundle_id.as_ref() == Some(bundle))
                    && request.pid.is_none_or(|pid| root.pid == Some(pid))
                    && request.kind.as_ref().is_none_or(|kind| &root.kind == kind)
                    && request.text.as_ref().is_none_or(|text| {
                        root.title.to_lowercase().contains(&text.to_lowercase())
                    })
            })
            .map(|(root, _)| root.clone())
            .collect())
    }

    async fn observe(
        &self,
        root: &RootInfo,
        _request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError> {
        let roots = self.roots.read().await;
        let (_, outline) = roots
            .iter()
            .find(|(candidate, _)| candidate.root_ref == root.root_ref)
            .ok_or_else(|| BackendError::Failed(format!("unknown root {}", root.root_ref)))?;
        Ok(UiSnapshot {
            root: root.clone(),
            outline: outline.clone(),
            captured_at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
            image: None,
        })
    }

    async fn act(&self, root: &RootInfo, actions: &[Action]) -> Result<Vec<ActOutcome>, BackendError> {
        let known = self
            .roots
            .read()
            .await
            .iter()
            .any(|(candidate, _)| candidate.root_ref == root.root_ref);
        if !known {
            return Err(BackendError::Failed(format!(
                "unknown root {}",
                root.root_ref
            )));
        }
        self.actions.lock().await.push(actions.to_vec());
        Ok(vec![ActOutcome::Worked; actions.len()])
    }
}
