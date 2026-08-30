//! Grounding-provider seam — ported from upstream codex PR
//! understudy-ai/codex-cua#3 (`core/src/tools/handlers/gui/provider.rs`).
//!
//! The provider indirection is kept so alternate grounding backends can be
//! slotted in; the default implementation delegates to the ported semantic
//! grounding round loop in [`super::grounding::resolve_grounded_target`],
//! driven by the harness-injected [`super::grounding::GuiGroundingSampler`].

use async_trait::async_trait;
use xai_tool_runtime::ToolError;

use super::grounding::GuiTargetRequest;
use super::grounding::ObserveState;
use super::grounding::ResolvedTarget;
use super::grounding::resolve_grounded_target;
use super::handler::GuiHandlerContext;

#[async_trait]
pub(crate) trait GuiGroundingProvider {
    async fn ground(
        &self,
        ctx: &GuiHandlerContext,
        request: GuiTargetRequest<'_>,
        capture_state: &ObserveState,
        image_bytes: &[u8],
    ) -> Result<Option<ResolvedTarget>, ToolError>;
}

pub(crate) struct ModelGuiGroundingProvider;

#[async_trait]
impl GuiGroundingProvider for ModelGuiGroundingProvider {
    async fn ground(
        &self,
        ctx: &GuiHandlerContext,
        request: GuiTargetRequest<'_>,
        capture_state: &ObserveState,
        image_bytes: &[u8],
    ) -> Result<Option<ResolvedTarget>, ToolError> {
        resolve_grounded_target(ctx.sampler()?, request, capture_state, image_bytes).await
    }
}

pub(crate) fn default_gui_grounding_provider() -> &'static ModelGuiGroundingProvider {
    static PROVIDER: ModelGuiGroundingProvider = ModelGuiGroundingProvider;
    &PROVIDER
}
