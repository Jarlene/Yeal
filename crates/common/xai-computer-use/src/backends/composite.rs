//! A multi-root backend that merges the desktop adapter with the CDP browser
//! backend. Root refs (`@rN`) are stable across `find_roots` calls: the
//! allocator keys on the backend-provided identity `(resource_key,
//! window_id)` and reuses previously issued refs while that identity is
//! alive.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::backend::{ActOutcome, BackendError, TextPage, UiBackend};
use crate::model::{Action, FindRootsRequest, ObserveRequest, RootInfo, UiSnapshot};

/// Stable `@rN` allocator keyed by backend identity.
#[derive(Debug, Default)]
pub struct RootRefAllocator {
    by_identity: HashMap<(String, Option<i64>), String>,
    by_ref: HashMap<String, (String, Option<i64>)>,
    next: usize,
}

impl RootRefAllocator {
    pub fn assign(&mut self, identity: (String, Option<i64>)) -> String {
        if let Some(existing) = self.by_identity.get(&identity) {
            return existing.clone();
        }
        loop {
            self.next += 1;
            let candidate = format!("@r{}", self.next);
            if !self.by_ref.contains_key(&candidate) {
                self.by_identity.insert(identity.clone(), candidate.clone());
                self.by_ref.insert(candidate.clone(), identity);
                return candidate;
            }
        }
    }

    /// Drop refs whose identity no longer appears in a `find_roots` result.
    pub fn prune(&mut self, seen: &HashSet<(String, Option<i64>)>) {
        let stale: Vec<_> = self
            .by_identity
            .iter()
            .filter(|(identity, _)| !seen.contains(*identity))
            .map(|(identity, root_ref)| (identity.clone(), root_ref.clone()))
            .collect();
        for (identity, root_ref) in stale {
            self.by_identity.remove(&identity);
            self.by_ref.remove(&root_ref);
        }
    }
}

/// Merges a desktop backend with an optional CDP browser backend.
pub struct CompositeBackend {
    desktop: Arc<dyn UiBackend>,
    cdp: Option<Arc<dyn UiBackend>>,
    allocator: Mutex<RootRefAllocator>,
}

impl std::fmt::Debug for CompositeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeBackend")
            .field("desktop", &"..")
            .field("cdp", &self.cdp.is_some())
            .finish()
    }
}

impl CompositeBackend {
    pub fn new(desktop: Arc<dyn UiBackend>, cdp: Option<Arc<dyn UiBackend>>) -> Self {
        Self {
            desktop,
            cdp,
            allocator: Mutex::new(RootRefAllocator::default()),
        }
    }

    fn dispatch<'a>(&'a self, root: &'a RootInfo) -> Result<&'a dyn UiBackend, BackendError> {
        if root.kind == "browser_page" {
            self.cdp
                .as_deref()
                .ok_or_else(|| BackendError::Unsupported("browser backend is not configured".into()))
        } else {
            Ok(self.desktop.as_ref())
        }
    }
}

#[async_trait]
impl UiBackend for CompositeBackend {
    async fn find_roots(&self, request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
        let mut merged = Vec::new();
        merged.extend(self.desktop.find_roots(request.clone()).await?);
        if let Some(cdp) = &self.cdp {
            merged.extend(cdp.find_roots(request.clone()).await?);
        }
        let mut seen: HashSet<(String, Option<i64>)> = HashSet::new();
        let mut allocator = self.allocator.lock().await;
        let mut roots = Vec::new();
        for mut root in merged {
            let identity = (root.resource_key.clone(), root.window_id);
            if !seen.insert(identity.clone()) {
                continue;
            }
            root.root_ref = allocator.assign(identity);
            roots.push(root);
        }
        allocator.prune(&seen);
        Ok(roots)
    }

    async fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError> {
        self.dispatch(root)?.observe(root, request).await
    }

    async fn act(&self, root: &RootInfo, actions: &[Action]) -> Result<Vec<ActOutcome>, BackendError> {
        self.dispatch(root)?.act(root, actions).await
    }

    async fn read_text(
        &self,
        root: &RootInfo,
        wire_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TextPage, BackendError> {
        self.dispatch(root)?.read_text(root, wire_ref, offset, limit).await
    }

    async fn navigate(&self, root: &RootInfo, url: &str) -> Result<(), BackendError> {
        self.dispatch(root)?.navigate(root, url).await
    }

    async fn evaluate(&self, root: &RootInfo, expression: &str) -> Result<String, BackendError> {
        self.dispatch(root)?.evaluate(root, expression).await
    }

    async fn launch_browser(&self, url: Option<&str>) -> Result<Vec<RootInfo>, BackendError> {
        match &self.cdp {
            Some(cdp) => {
                let roots = cdp.launch_browser(url).await?;
                // Assign stable @rN refs exactly like find_roots would, so a
                // subsequent observe() (which re-runs find_roots and matches by
                // root_ref) resolves the launched pages.
                let mut allocator = self.allocator.lock().await;
                Ok(roots
                    .into_iter()
                    .map(|mut root| {
                        let identity = (root.resource_key.clone(), root.window_id);
                        root.root_ref = allocator.assign(identity);
                        root
                    })
                    .collect())
            }
            None => Err(BackendError::Unsupported(
                "launch_browser requires a configured CDP browser backend".into(),
            )),
        }
    }
}
