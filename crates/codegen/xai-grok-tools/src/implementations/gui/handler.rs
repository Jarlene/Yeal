//! Native GUI tool engine — ported from upstream codex PR
//! understudy-ai/codex-cua#3 (`core/src/tools/handlers/gui.rs`).
//!
//! One engine multiplexes all nine GUI tools (`gui_observe`, `gui_click`,
//! `gui_drag`, `gui_scroll`, `gui_type`, `gui_key`, `gui_move`, `gui_wait`,
//! `gui_batch`), preserving the upstream observe-act-verify loop: semantic
//! target resolution → blocking platform event → post-action evidence
//! probe. The thin per-tool structs in [`super::tools`] adapt this engine
//! to the `xai_tool_runtime::Tool` contract.
//!
//! Contract adaptations from upstream (logic and semantics preserved):
//! - `ToolInvocation` → [`GuiHandlerContext`] (session id + grounding
//!   sampler + GUI settings). The codex model-client access becomes the
//!   [`GuiGroundingSampler`] seam; its presence is also the image-input
//!   capability signal (upstream: `InputModality::Image`).
//! - `FunctionCallError::RespondToModel` → [`ToolError`] with kind
//!   `Execution`; `ToolError::detail` is the model-facing message.
//! - `GuiHandler` was a single handler multiplexed by tool name; grok
//!   registers one tool struct per name. The per-conversation observe
//!   state must stay shared across all nine tools, so the engine lives in
//!   a process-global [`shared_gui_engine`] (GUI physical resources are
//!   process-global anyway) — matching upstream's single-handler shape.

use std::collections::HashMap;
use std::process::Command;
use std::sync::OnceLock;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use xai_tool_runtime::ToolCallContext;
use xai_tool_runtime::ToolError;
use xai_tool_runtime::ToolErrorKind;
use xai_tool_runtime::tool::ContentBlock;

use super::grounding::BatchGroundingRole;
use super::grounding::BatchGroundingTarget;
use super::grounding::CaptureArtifact;
use super::grounding::CaptureMode;
use super::grounding::GroundedGuiTarget;
use super::grounding::GuiTargetRequest;
use super::grounding::HelperPoint;
use super::grounding::HelperRect;
use super::grounding::HostCaptureExclusionState;
use super::grounding::ObserveState;
use super::grounding::ResolvedTarget;
use super::grounding::WindowSelector;
use super::grounding::build_target_resolution_details;
use super::grounding::data_url;
use super::grounding::grounding_sampler_from_context;
use super::grounding::normalize_coordinate_space;
use super::grounding::normalize_grounding_mode;
use super::grounding::normalize_optional_coordinate_point;
use super::grounding::resolve_batch_grounded_targets;
use super::grounding::resolve_batch_grounded_targets_unified;
use super::platform::PlatformObservation;
use super::platform::default_gui_platform;
use super::provider::GuiGroundingProvider;
use super::provider::default_gui_grounding_provider;
use super::readiness::enforce_gui_tool_capability;
use super::schemas;
use super::session::begin_gui_action_session;
use super::wire::HelperCaptureContext;

const SCREENSHOT_JPEG_QUALITY: u8 = 75;

const PLATFORM_NAME: &str = if cfg!(target_os = "macos") {
    "macOS"
} else if cfg!(target_os = "linux") {
    "Linux"
} else if cfg!(target_os = "windows") {
    "Windows"
} else {
    "this platform"
};
const GUI_IMAGE_UNSUPPORTED_MESSAGE: &str =
    "Native GUI screenshot tools are not allowed because you do not support image inputs";
const GUI_TOOLS_DISABLED_MESSAGE: &str = "GUI tools are not enabled for this session. Enable the `gui_tools` feature (`[features] gui_tools = true` in config.toml, or the GROK_GUI_TOOLS environment variable) to use them.";
const DEFAULT_DRAG_DURATION_MS: i64 = 450;
const DEFAULT_DRAG_STEPS: i64 = 24;
const DEFAULT_HOVER_SETTLE_MS: i64 = 200;
const DEFAULT_CLICK_AND_HOLD_MS: i64 = 650;
const DEFAULT_GUI_WAIT_TIMEOUT_MS: i64 = 8000;
const DEFAULT_GUI_WAIT_INTERVAL_MS: i64 = 350;
const WAIT_CONFIRMATION_COUNT: i64 = 2;
const DEFAULT_POST_ACTION_SETTLE_MS: i64 = 3000;
const DEFAULT_POST_TYPE_SETTLE_MS: i64 = 500;
const DEFAULT_TYPE_FOCUS_SETTLE_MS: i64 = 180;
const DEFAULT_TARGETED_SCROLL_DISTANCE: &str = "medium";
const DEFAULT_TARGETLESS_SCROLL_DISTANCE: &str = "page";
const GUI_DIRECT_COORDINATE_PLACEHOLDER_MESSAGE: &str = "Direct coordinate GUI targeting is currently kept as an experimental placeholder only. Semantic grounding remains the supported path because the direct-coordinate benchmark accuracy is still poor.";
const MAX_OBSERVE_STATE_ENTRIES: usize = 64;
const MAX_BATCH_STEPS: usize = 10;

/// Map a model-facing failure onto the grok tool error. `ToolError::detail`
/// is sent back to the model, mirroring upstream's
/// `FunctionCallError::RespondToModel`.
fn respond_to_model(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorKind::Execution, message)
}

/// Process-global GUI engine. Upstream registered one `GuiHandler` for all
/// nine tool names; grok has one tool struct per name, so the per-conversation
/// observe state must be shared process-wide.
pub struct GuiEngine {
    observe_state: Mutex<HashMap<String, ObserveState>>,
}

impl Default for GuiEngine {
    fn default() -> Self {
        Self {
            observe_state: Mutex::new(HashMap::new()),
        }
    }
}

impl std::fmt::Debug for GuiEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuiEngine").finish_non_exhaustive()
    }
}

/// Shared engine handle used by every GUI tool struct.
pub fn shared_gui_engine() -> &'static GuiEngine {
    static ENGINE: OnceLock<GuiEngine> = OnceLock::new();
    ENGINE.get_or_init(GuiEngine::default)
}

/// GUI-specific tool settings (upstream: `ToolsConfig.gui_*` fields).
/// Defaults mirror the upstream config resolution (`coordinate_targeting`
/// false, batch enabled, `parallel` grounding strategy, no inter-step delay).
/// Stored as the registered params type for all nine GUI tools, so
/// `ToolConfig.params` flows in as `Params<Self>` at finalize; direct
/// `ToolCallContext` extension injection also works for tests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiToolsSettings {
    pub coordinate_targeting: bool,
    pub batch_enabled: bool,
    pub batch_grounding_strategy: String,
    pub batch_action_delay_ms: u64,
}

impl crate::types::resources::ResourceType for GuiToolsSettings {
    const ID: &'static str = "grok_build.GuiToolsSettings";
}

impl Default for GuiToolsSettings {
    fn default() -> Self {
        Self {
            coordinate_targeting: false,
            batch_enabled: true,
            batch_grounding_strategy: "parallel".to_string(),
            batch_action_delay_ms: 0,
        }
    }
}

/// Per-call context handed to every engine method — the grok replacement for
/// upstream's `ToolInvocation`.
pub struct GuiHandlerContext {
    /// Stable session identifier keying the per-conversation observe state
    /// (upstream: `invocation.session.conversation_id`).
    pub session_id: String,
    sampler: Option<std::sync::Arc<dyn super::grounding::GuiGroundingSampler>>,
    /// `None` when this dispatch carried no explicit GUI-tools enable marker
    /// (no `GuiToolsSettings` extension and no `Params<GuiToolsSettings>`
    /// resource) — the feature is off for the session and dispatch must be
    /// refused (upstream never registered the handler at all when
    /// `config.gui_tools` was unset).
    settings: Option<GuiToolsSettings>,
}

impl GuiHandlerContext {
    /// Build the context from a runtime `ToolCallContext`. The grounding
    /// sampler is read from the extensions first (injected by the harness
    /// when semantic grounding is available), then from `SharedResources`
    /// (injected post-finalize); settings come from the extensions or
    /// `SharedResources` and stay `None` when the harness never enabled the
    /// feature — [`Self::ensure_enabled`] turns that into the refusal.
    pub async fn from_tool_context(ctx: &ToolCallContext) -> Self {
        let session_id = ctx
            .extensions
            .get::<xai_tool_runtime::SessionContext>()
            .map(|session| session.0.clone())
            .unwrap_or_else(|| ctx.call_id.as_str().to_string());
        let mut sampler = grounding_sampler_from_context(ctx);
        let mut settings = ctx
            .extensions
            .get::<GuiToolsSettings>()
            .map(|settings| (*settings).clone());
        if settings.is_none() || sampler.is_none() {
            if let Ok(resources) = crate::types::tool_metadata::shared_resources(ctx) {
                let res = resources.lock().await;
                if settings.is_none() {
                    settings = res
                        .get::<crate::types::resources::Params<GuiToolsSettings>>()
                        .map(|params| params.0.clone())
                        .or_else(|| res.get::<GuiToolsSettings>().cloned());
                }
                if sampler.is_none() {
                    sampler = res
                        .get::<std::sync::Arc<dyn super::grounding::GuiGroundingSampler>>()
                        .cloned();
                }
            }
        }
        Self {
            session_id,
            sampler,
            settings,
        }
    }

    /// The resolved GUI settings. Falls back to the upstream defaults for
    /// callers that only need the values (dispatch itself is gated by
    /// [`Self::ensure_enabled`]).
    pub fn settings(&self) -> GuiToolsSettings {
        self.settings.clone().unwrap_or_default()
    }

    /// Whether the harness explicitly enabled the GUI tools for this
    /// dispatch (feature flag resolved + settings on the tool config).
    pub fn is_enabled(&self) -> bool {
        self.settings.is_some()
    }

    /// Refuse dispatch when the session never enabled the GUI tools.
    /// Upstream only registered the handler when `config.gui_tools` was set;
    /// grok registers the nine tools unconditionally, so this is the
    /// defense-in-depth gate that keeps a client-crafted `ToolServerConfig`
    /// from dispatching `gui_*` while the feature is off.
    pub fn ensure_enabled(&self) -> Result<(), ToolError> {
        if self.settings.is_some() {
            return Ok(());
        }
        Err(ToolError::new(
            ToolErrorKind::NotImplemented,
            GUI_TOOLS_DISABLED_MESSAGE,
        ))
    }

    /// Upstream gated grounding and screenshot attachment on the model's
    /// image-input modality; grok-build signals the same capability through
    /// the presence of the grounding sampler.
    pub fn image_input_supported(&self) -> bool {
        self.sampler.is_some()
    }

    /// The grounding model seam. Errors with the upstream
    /// image-unsupported message when no sampler was injected — the same
    /// user-facing outcome as upstream's `supports_image_input` gate.
    pub fn sampler(&self) -> Result<&dyn super::grounding::GuiGroundingSampler, ToolError> {
        self.sampler
            .as_deref()
            .ok_or_else(|| respond_to_model(GUI_IMAGE_UNSUPPORTED_MESSAGE))
    }
}

/// Structured result of one GUI tool call. `success: false` outputs are
/// model-facing soft failures (target not found, wait timed out) — the same
/// shape as upstream's `GuiToolOutput`, whose `success_for_logging` drove
/// the Completed/Failed status.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuiToolOutput {
    pub message: String,
    pub image_url: Option<String>,
    pub code_result: JsonValue,
    pub success: bool,
}

impl xai_tool_runtime::ToolOutput for GuiToolOutput {
    fn model_output(&self) -> Vec<ContentBlock> {
        let mut blocks = vec![ContentBlock::Text {
            text: self.message.clone(),
        }];
        if let Some((mime_type, data)) = self.image_url.as_deref().and_then(parse_data_url) {
            blocks.push(ContentBlock::Image {
                mime_type: mime_type.to_string(),
                data: data.to_string(),
                media_id: None,
                filename: None,
                path: None,
                metadata: HashMap::new(),
            });
        }
        blocks
    }
}

/// Split a `data:<mime>;base64,<payload>` URL into its parts.
pub fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(";base64,")?;
    Some((mime_type, data))
}

impl GuiEngine {
    async fn set_observe_state(&self, conversation_id: &str, state: ObserveState) {
        let mut map = self.observe_state.lock().await;
        map.insert(conversation_id.to_string(), state);
        if map.len() > MAX_OBSERVE_STATE_ENTRIES {
            // Evict arbitrary excess entries to prevent unbounded growth.
            let keys_to_remove: Vec<String> = map
                .keys()
                .filter(|k| k.as_str() != conversation_id)
                .take(map.len() - MAX_OBSERVE_STATE_ENTRIES)
                .cloned()
                .collect();
            for key in keys_to_remove {
                map.remove(&key);
            }
        }
    }

    pub async fn handle_observe(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::ObserveArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        let semantic_target = normalize_optional_string(args.target.as_deref());
        let location_hint = normalize_optional_string(args.location_hint.as_deref());
        let scope = normalize_optional_string(args.scope.as_deref());
        let attach_image =
            prepare_gui_observe_request(ctx, semantic_target.is_some(), args.return_image).await?;
        let observation = observe_platform(
            args.app.as_deref(),
            /*activate_app*/ true,
            args.capture_mode.as_deref(),
            window_selection.as_ref(),
            args.app.as_deref().is_some(),
        )
        .await?;
        let state = observation.state;
        self.set_observe_state(&ctx.session_id, state.clone()).await;
        let image_output = attach_image.then(|| screenshot_data_url(&observation.image_bytes));
        let app_label = state
            .app_name
            .as_ref()
            .map(|app| format!(" for app `{app}`"))
            .unwrap_or_default();
        let subject = if state.capture.capture_mode == CaptureMode::Window {
            state
                .capture
                .window_title
                .as_ref()
                .map(|title| format!("window `{title}`"))
                .unwrap_or_else(|| "window".to_string())
        } else {
            format!("display {}", state.capture.display_index)
        };
        if let Some(target) = semantic_target.as_deref() {
            let grounded = self
                .resolve_gui_target(
                    ctx,
                    GuiTargetRequest {
                        app: args.app.as_deref(),
                        capture_mode: args.capture_mode.as_deref(),
                        window_selection: window_selection.as_ref(),
                        target,
                        location_hint: location_hint.as_deref(),
                        scope: scope.as_deref(),
                        grounding_mode: args.grounding_mode.as_deref(),
                        action: "observe",
                        related_target: None,
                        related_scope: None,
                        related_location_hint: None,
                        related_point: None,
                    },
                )
                .await?;
            let Some(grounded) = grounded else {
                let summary = format!(
                    "Captured {platform} {subject}{app_label}, but could not resolve semantic GUI target `{target}` in the observed surface.",
                    platform = PLATFORM_NAME
                );
                return Ok(self.build_gui_output(
                    summary,
                    state,
                    image_output,
                    false,
                    Some(serde_json::json!({
                        "error": format!("No confident semantic GUI target `{target}` was found."),
                        "target": target,
                        "grounding_method": "grounding",
                        "grounding_mode_requested": normalize_grounding_mode(args.grounding_mode.as_deref(), "observe")?,
                        "grounding_mode_effective": normalize_grounding_mode(args.grounding_mode.as_deref(), "observe")?,
                        "scope": scope,
                        "confidence": 0.0,
                    })),
                ));
            };
            let summary = format!(
                "Captured {platform} {subject}{app_label} and resolved semantic GUI target `{target}` in the observed surface.",
                platform = PLATFORM_NAME
            );
            let mut extra = serde_json::Map::new();
            extra.insert(
                "target_resolution".to_string(),
                build_target_resolution_details(target, &grounded),
            );
            return Ok(self.build_gui_output(
                summary,
                state,
                image_output,
                true,
                Some(JsonValue::Object(extra)),
            ));
        }

        let summary = format!(
            "Captured {platform} {subject}{app_label} at origin ({origin_x}, {origin_y}) with size {width}x{height} for visual inspection and follow-up GUI grounding.",
            platform = PLATFORM_NAME,
            origin_x = state.capture.origin_x.round(),
            origin_y = state.capture.origin_y.round(),
            width = state.capture.width,
            height = state.capture.height
        );
        Ok(self.build_gui_output(summary, state, image_output, true, None))
    }

    pub async fn handle_wait(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::WaitArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let mut window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        let mut app = normalize_optional_string(args.app.as_deref());
        let mut capture_mode = normalize_optional_string(args.capture_mode.as_deref());
        let target = normalize_optional_string(Some(args.target.as_str())).ok_or_else(|| {
            respond_to_model("gui_wait requires a semantic `target`.".to_string())
        })?;
        let location_hint = normalize_optional_string(args.location_hint.as_deref());
        let scope = normalize_optional_string(args.scope.as_deref());
        enforce_gui_tool_capability(ctx.image_input_supported(), "gui_wait", true).await?;
        if app.is_none()
            && capture_mode.is_none()
            && window_selection.is_none()
            && let Some(previous_state) = self.get_observe_state(ctx).await
        {
            app = previous_state.app_name.clone();
            capture_mode = Some(previous_state.capture.capture_mode.to_string());
            if previous_state.capture.capture_mode == CaptureMode::Window {
                window_selection =
                    previous_state
                        .capture
                        .window_title
                        .as_ref()
                        .map(|title| WindowSelector {
                            title: Some(title.clone()),
                            title_contains: None,
                            index: None,
                        });
            }
        }

        let target_state = normalize_wait_target_state(args.state.as_deref())?;
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_GUI_WAIT_TIMEOUT_MS);
        let interval_ms = args.interval_ms.unwrap_or(DEFAULT_GUI_WAIT_INTERVAL_MS);
        if timeout_ms <= 0 {
            return Err(respond_to_model(
                "gui_wait.timeout_ms must be a positive integer".to_string(),
            ));
        }
        if interval_ms <= 0 {
            return Err(respond_to_model(
                "gui_wait.interval_ms must be a positive integer".to_string(),
            ));
        }

        let probe = self
            .probe_for_target(
                ctx,
                GuiTargetRequest {
                    app: app.as_deref(),
                    capture_mode: capture_mode.as_deref(),
                    window_selection: window_selection.as_ref(),
                    target: &target,
                    location_hint: location_hint.as_deref(),
                    scope: scope.as_deref(),
                    grounding_mode: args.grounding_mode.as_deref(),
                    action: "wait",
                    related_target: None,
                    related_scope: None,
                    related_location_hint: None,
                    related_point: None,
                },
                target_state,
                timeout_ms,
                interval_ms,
            )
            .await?;

        let summary = if probe.matched {
            match (target_state, probe.grounded.as_ref()) {
                ("appear", Some(resolved)) => format!(
                    "Confirmed GUI target `{target}` appeared after {} visual checks and {} consecutive confirmations at global ({}, {}).{}",
                    probe.attempts,
                    WAIT_CONFIRMATION_COUNT,
                    resolved.resolved.point.x.round(),
                    resolved.resolved.point.y.round(),
                    if probe.image_url.is_some() {
                        " Attached a refreshed GUI evidence screenshot."
                    } else {
                        ""
                    }
                ),
                ("disappear", _) => format!(
                    "Confirmed GUI target `{target}` disappeared after {} visual checks and {} consecutive confirmations.{}",
                    probe.attempts,
                    WAIT_CONFIRMATION_COUNT,
                    if probe.image_url.is_some() {
                        " Attached a refreshed GUI evidence screenshot."
                    } else {
                        ""
                    }
                ),
                _ => unreachable!("validated wait target state"),
            }
        } else {
            format!(
                "Timed out after {timeout_ms}ms waiting for GUI target `{target}` to {target_state}.{}",
                if probe.image_url.is_some() {
                    " Attached a refreshed GUI evidence screenshot."
                } else {
                    ""
                }
            )
        };

        Ok(self.build_gui_output(
            summary,
            probe.state,
            probe.image_url,
            probe.matched,
            Some(serde_json::json!({
                "timeout_ms": timeout_ms,
                "interval_ms": interval_ms,
                "target": target,
                "target_state": target_state,
                "attempts": probe.attempts,
                "wait_confirmations_required": WAIT_CONFIRMATION_COUNT,
                "target_found": probe.grounded.is_some(),
                "grounding_method": probe.grounded.as_ref().map(|grounded| grounded.grounding_method),
                "target_resolution": probe.grounded.as_ref().map(|grounded| build_target_resolution_details(&target, grounded)),
            })),
        ))
    }

    pub async fn handle_click(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::ClickArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let mut action_session = begin_gui_action_session(&ctx.session_id, "gui_click", true)?;
        action_session.hide_other_apps(args.app.as_deref());
        let window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        let semantic_target = normalize_optional_string(args.target.as_deref());
        let location_hint = normalize_optional_string(args.location_hint.as_deref());
        let scope = normalize_optional_string(args.scope.as_deref());
        let coordinate_point = normalize_optional_coordinate_point(args.x, args.y, "x", "y")?;
        if coordinate_point.is_none()
            && normalize_optional_string(args.coordinate_space.as_deref()).is_some()
        {
            return Err(respond_to_model(
                "gui_click.coordinate_space requires both `x` and `y`.".to_string(),
            ));
        }
        if semantic_target.is_some() && coordinate_point.is_some() {
            return Err(respond_to_model(
                "gui_click accepts either semantic `target` fields or direct coordinates, not both in the same call."
                    .to_string(),
            ));
        }
        if coordinate_point.is_some() {
            normalize_coordinate_space(args.coordinate_space.as_deref())?;

            if !ctx.settings().coordinate_targeting {
                return Err(respond_to_model(
                    "Direct coordinate GUI clicking is disabled by default. Enable `[tools.gui] coordinate_targeting = true` only if you intentionally want to keep the experimental placeholder path visible."
                        .to_string(),
                ));
            }
            return Err(respond_to_model(
                GUI_DIRECT_COORDINATE_PLACEHOLDER_MESSAGE.to_string(),
            ));
        }
        let target = semantic_target.as_deref().ok_or_else(|| {
            respond_to_model("gui_click requires a semantic `target`.".to_string())
        })?;
        enforce_gui_tool_capability(ctx.image_input_supported(), "gui_click", true).await?;
        let grounded = self
            .resolve_gui_target(
                ctx,
                GuiTargetRequest {
                    app: args.app.as_deref(),
                    capture_mode: args.capture_mode.as_deref(),
                    window_selection: window_selection.as_ref(),
                    target,
                    location_hint: location_hint.as_deref(),
                    scope: scope.as_deref(),
                    grounding_mode: args.grounding_mode.as_deref(),
                    action: "click",
                    related_target: None,
                    related_scope: None,
                    related_location_hint: None,
                    related_point: None,
                },
            )
            .await?;
        let grounded = grounded.ok_or_else(|| {
            respond_to_model(format!("Could not resolve semantic GUI target `{target}`."))
        })?;
        let target_details = build_target_resolution_details(target, &grounded);
        let resolved = grounded.resolved;
        let coordinate_summary = resolved
            .local_point
            .as_ref()
            .map(|point| {
                format!(
                    "target `{target}` at image coordinate ({}, {})",
                    point.x.round(),
                    point.y.round()
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "target `{target}` at global coordinate ({}, {})",
                    resolved.point.x.round(),
                    resolved.point.y.round()
                )
            });
        let global_x = resolved.point.x;
        let global_y = resolved.point.y;
        let state = resolved.capture_state;
        let target_details = Some(target_details);
        let button = args.button.as_deref().unwrap_or("left");
        let clicks = args.clicks.unwrap_or(1);
        let hold_ms = args.hold_ms.unwrap_or(DEFAULT_CLICK_AND_HOLD_MS).max(1);
        let settle_ms = args.settle_ms.unwrap_or(DEFAULT_HOVER_SETTLE_MS).max(1);
        let event_mode = match (button, clicks, args.hold_ms) {
            ("none", 1, None) => "move_cursor",
            ("left", 1, None) => "click",
            ("left", 1, Some(_)) => "click_and_hold",
            ("left", 2, None) => "double_click",
            ("right", 1, None) => "right_click",
            ("none", _, Some(_)) => {
                return Err(respond_to_model(
                    "gui_click cannot combine `button: none` with `hold_ms`".to_string(),
                ));
            }
            ("none", other, None) => {
                return Err(respond_to_model(format!(
                    "gui_click with `button: none` only supports a single hover action, got `{other}`"
                )));
            }
            ("left", 2, Some(_)) => {
                return Err(respond_to_model(
                    "gui_click cannot combine `clicks: 2` with `hold_ms`".to_string(),
                ));
            }
            ("left", other, _) => {
                return Err(respond_to_model(format!(
                    "gui_click only supports 1 or 2 left clicks, got `{other}`"
                )));
            }
            ("right", other, _) => {
                return Err(respond_to_model(format!(
                    "gui_click only supports a single right click, got `{other}`"
                )));
            }
            (other, _, _) => {
                return Err(respond_to_model(format!(
                    "gui_click.button only supports `left`, `right`, or `none`, got `{other}`"
                )));
            }
        };
        action_session.throw_if_emergency_stopped()?;

        run_gui_event(
            event_mode,
            args.app.as_deref(),
            &[("CODEX_GUI_X", global_x), ("CODEX_GUI_Y", global_y)],
            &[
                ("CODEX_GUI_HOLD_MS", hold_ms.to_string()),
                ("CODEX_GUI_SETTLE_MS", settle_ms.to_string()),
            ],
        )
        .await?;
        action_session.throw_if_emergency_stopped()?;

        let evidence = self
            .capture_evidence_image(
                ctx,
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
                DEFAULT_POST_ACTION_SETTLE_MS,
            )
            .await?;

        let summary = format!(
            "{action} at {coordinate_summary} on {platform} {mode} {subject} (global {gx}, {gy}).{evidence_note} Use gui_wait or gui_observe to verify the resulting UI state before the next risky action.",
            action = describe_click_action(button, clicks, args.hold_ms.is_some()),
            mode = state.capture.capture_mode.as_str(),
            platform = PLATFORM_NAME,
            subject = describe_capture_subject(&state),
            gx = global_x.round(),
            gy = global_y.round(),
            evidence_note = if evidence.image_url.is_some() {
                " Attached a refreshed GUI evidence screenshot."
            } else {
                ""
            }
        );
        let mut extra_details = serde_json::Map::new();
        extra_details.insert(
            "action_kind".to_string(),
            JsonValue::String(event_mode.to_string()),
        );
        extra_details.insert("executed_point".to_string(), point_json(global_x, global_y));
        if let Some(target_details) = target_details {
            extend_object_fields(&mut extra_details, target_details);
        }
        extra_details.insert(
            "pre_action_capture".to_string(),
            build_capture_details_from_state(&state),
        );
        Ok(self.build_gui_output(
            summary,
            evidence.state,
            evidence.image_url,
            true,
            Some(JsonValue::Object(extra_details)),
        ))
    }

    pub async fn handle_drag(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::DragArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let mut action_session = begin_gui_action_session(&ctx.session_id, "gui_drag", true)?;
        action_session.hide_other_apps(args.app.as_deref());
        let window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        let from_target = normalize_optional_string(args.from_target.as_deref());
        let from_point =
            normalize_optional_coordinate_point(args.from_x, args.from_y, "from_x", "from_y")?;
        let from_location_hint = normalize_optional_string(args.from_location_hint.as_deref());
        let from_scope = normalize_optional_string(args.from_scope.as_deref());
        let to_target = normalize_optional_string(args.to_target.as_deref());
        let to_point = normalize_optional_coordinate_point(args.to_x, args.to_y, "to_x", "to_y")?;
        let to_location_hint = normalize_optional_string(args.to_location_hint.as_deref());
        let to_scope = normalize_optional_string(args.to_scope.as_deref());
        if from_target.is_some() && from_point.is_some() {
            return Err(respond_to_model(
                "gui_drag source accepts either `from_target` fields or `from_x`/`from_y`, not both in the same call."
                    .to_string(),
            ));
        }
        if to_target.is_some() && to_point.is_some() {
            return Err(respond_to_model(
                "gui_drag destination accepts either `to_target` fields or `to_x`/`to_y`, not both in the same call."
                    .to_string(),
            ));
        }
        let uses_direct_coordinates = from_point.is_some() || to_point.is_some();
        if !uses_direct_coordinates
            && normalize_optional_string(args.coordinate_space.as_deref()).is_some()
        {
            return Err(respond_to_model(
                "gui_drag.coordinate_space requires at least one coordinate endpoint.".to_string(),
            ));
        }
        if uses_direct_coordinates {
            normalize_coordinate_space(args.coordinate_space.as_deref())?;

            if !ctx.settings().coordinate_targeting {
                return Err(respond_to_model(
                    "Direct coordinate GUI dragging is disabled by default. Enable `[tools.gui] coordinate_targeting = true` only if you intentionally want to keep the experimental placeholder path visible."
                        .to_string(),
                ));
            }
            return Err(respond_to_model(
                GUI_DIRECT_COORDINATE_PLACEHOLDER_MESSAGE.to_string(),
            ));
        }
        enforce_gui_tool_capability(ctx.image_input_supported(), "gui_drag", true).await?;
        let source_endpoint = normalize_drag_endpoint(
            "source",
            "from_target",
            from_target.as_deref(),
            from_location_hint.as_deref(),
            from_scope.as_deref(),
        )?;
        let destination_endpoint = normalize_drag_endpoint(
            "destination",
            "to_target",
            to_target.as_deref(),
            to_location_hint.as_deref(),
            to_scope.as_deref(),
        )?;
        // Ground both drag endpoints in parallel for ~2x speed.
        let source_req = match &source_endpoint {
            DragEndpoint::Target {
                target,
                location_hint,
                scope,
            } => GuiTargetRequest {
                app: args.app.as_deref(),
                capture_mode: args.capture_mode.as_deref(),
                window_selection: window_selection.as_ref(),
                target,
                location_hint: *location_hint,
                scope: *scope,
                grounding_mode: args.grounding_mode.as_deref(),
                action: "drag_source",
                related_target: to_target.as_deref(),
                related_scope: to_scope.as_deref(),
                related_location_hint: to_location_hint.as_deref(),
                related_point: None,
            },
        };
        let dest_req = match &destination_endpoint {
            DragEndpoint::Target {
                target,
                location_hint,
                scope,
            } => GuiTargetRequest {
                app: args.app.as_deref(),
                capture_mode: args.capture_mode.as_deref(),
                window_selection: window_selection.as_ref(),
                target,
                location_hint: *location_hint,
                scope: *scope,
                grounding_mode: args.grounding_mode.as_deref(),
                action: "drag_destination",
                related_target: from_target.as_deref(),
                related_scope: from_scope.as_deref(),
                related_location_hint: from_location_hint.as_deref(),
                related_point: None,
            },
        };
        let (source_result, dest_result) = tokio::join!(
            self.resolve_gui_target(ctx, source_req),
            self.resolve_gui_target(ctx, dest_req),
        );
        let (from_global_x, from_global_y, state, from_summary, source_target_details) = {
            let DragEndpoint::Target { target, .. } = source_endpoint;
            let grounded = source_result?.ok_or_else(|| {
                respond_to_model(format!(
                    "Could not resolve semantic GUI drag source `{target}`."
                ))
            })?;
            let target_details = build_target_resolution_details(target, &grounded);
            let resolved = grounded.resolved;
            let summary = resolved
                .local_point
                .as_ref()
                .map(|point| {
                    format!(
                        "target `{target}` at image coordinate ({}, {})",
                        point.x.round(),
                        point.y.round()
                    )
                })
                .unwrap_or_else(|| {
                    format!(
                        "target `{target}` at global coordinate ({}, {})",
                        resolved.point.x.round(),
                        resolved.point.y.round()
                    )
                });
            (
                resolved.point.x,
                resolved.point.y,
                resolved.capture_state,
                summary,
                Some(target_details),
            )
        };
        let (to_global_x, to_global_y, to_summary, destination_target_details) = {
            let DragEndpoint::Target { target, .. } = destination_endpoint;
            let grounded = dest_result?.ok_or_else(|| {
                respond_to_model(format!(
                    "Could not resolve semantic GUI drag destination `{target}`."
                ))
            })?;
            let target_details = build_target_resolution_details(target, &grounded);
            let resolved = grounded.resolved;
            let summary = resolved
                .local_point
                .as_ref()
                .map(|point| {
                    format!(
                        "target `{target}` at image coordinate ({}, {})",
                        point.x.round(),
                        point.y.round()
                    )
                })
                .unwrap_or_else(|| {
                    format!(
                        "target `{target}` at global coordinate ({}, {})",
                        resolved.point.x.round(),
                        resolved.point.y.round()
                    )
                });
            (
                resolved.point.x,
                resolved.point.y,
                summary,
                Some(target_details),
            )
        };
        let duration_ms = args.duration_ms.unwrap_or(DEFAULT_DRAG_DURATION_MS).max(1);
        let steps = DEFAULT_DRAG_STEPS;
        action_session.throw_if_emergency_stopped()?;

        run_gui_event(
            "drag",
            args.app.as_deref(),
            &[
                ("CODEX_GUI_FROM_X", from_global_x),
                ("CODEX_GUI_FROM_Y", from_global_y),
                ("CODEX_GUI_TO_X", to_global_x),
                ("CODEX_GUI_TO_Y", to_global_y),
            ],
            &[
                ("CODEX_GUI_DURATION_MS", duration_ms.to_string()),
                ("CODEX_GUI_STEPS", steps.to_string()),
            ],
        )
        .await?;
        action_session.throw_if_emergency_stopped()?;

        let evidence = self
            .capture_evidence_image(
                ctx,
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
                DEFAULT_POST_ACTION_SETTLE_MS,
            )
            .await?;
        let summary = format!(
            "Dragged from {from_summary} to {to_summary} on {platform} {mode} {subject} (global {fx}, {fy} -> {tx}, {ty}).{evidence_note} Use gui_wait or gui_observe to confirm the drop landed where you expected.",
            mode = state.capture.capture_mode.as_str(),
            platform = PLATFORM_NAME,
            subject = describe_capture_subject(&state),
            fx = from_global_x.round(),
            fy = from_global_y.round(),
            tx = to_global_x.round(),
            ty = to_global_y.round(),
            evidence_note = if evidence.image_url.is_some() {
                " Attached a refreshed GUI evidence screenshot."
            } else {
                ""
            }
        );
        let mut extra_details = serde_json::Map::new();
        extra_details.insert(
            "action_kind".to_string(),
            JsonValue::String("drag".to_string()),
        );
        extra_details.insert(
            "executed_from_point".to_string(),
            point_json(from_global_x, from_global_y),
        );
        extra_details.insert(
            "executed_to_point".to_string(),
            point_json(to_global_x, to_global_y),
        );
        extra_details.insert(
            "pre_action_capture".to_string(),
            build_capture_details_from_state(&state),
        );
        if let Some(source_target_details) = source_target_details {
            extend_object_fields(&mut extra_details, source_target_details);
        }
        if let Some(destination_target_details) = destination_target_details {
            extra_details.insert(
                "destination_target_resolution".to_string(),
                destination_target_details,
            );
        }
        Ok(self.build_gui_output(
            summary,
            evidence.state,
            evidence.image_url,
            true,
            (!extra_details.is_empty()).then_some(JsonValue::Object(extra_details)),
        ))
    }

    pub async fn handle_scroll(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::ScrollArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let mut action_session = begin_gui_action_session(&ctx.session_id, "gui_scroll", true)?;
        action_session.hide_other_apps(args.app.as_deref());
        let window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        let direction = normalize_scroll_direction(args.direction.as_deref())?;
        let distance = normalize_scroll_distance(args.distance.as_deref())?;
        let semantic_target = normalize_optional_string(args.target.as_deref());
        let location_hint = normalize_optional_string(args.location_hint.as_deref());
        let scope = normalize_optional_string(args.scope.as_deref());
        enforce_gui_tool_capability(
            ctx.image_input_supported(),
            "gui_scroll",
            semantic_target.is_some(),
        )
        .await?;

        let mut float_env = Vec::new();
        let mut state_for_summary = None;
        let mut target_details = None;
        let mut executed_point = None;
        let mut target_bounds = None;
        if let Some(target) = semantic_target.as_deref() {
            let grounded = self
                .resolve_gui_target(
                    ctx,
                    GuiTargetRequest {
                        app: args.app.as_deref(),
                        capture_mode: args.capture_mode.as_deref(),
                        window_selection: window_selection.as_ref(),
                        target,
                        location_hint: location_hint.as_deref(),
                        scope: scope.as_deref(),
                        grounding_mode: args.grounding_mode.as_deref(),
                        action: "scroll",
                        related_target: None,
                        related_scope: None,
                        related_location_hint: None,
                        related_point: None,
                    },
                )
                .await?;
            let grounded = grounded.ok_or_else(|| {
                respond_to_model(format!(
                    "Could not resolve semantic GUI target `{target}` for scrolling."
                ))
            })?;
            let details = build_target_resolution_details(target, &grounded);
            let resolved = grounded.resolved;
            float_env.push(("CODEX_GUI_X", resolved.point.x));
            float_env.push(("CODEX_GUI_Y", resolved.point.y));
            executed_point = Some((resolved.point.x, resolved.point.y));
            target_bounds = Some(resolved.bounds);
            state_for_summary = Some(resolved.capture_state);
            target_details = Some(details);
        } else if args.app.is_some() || args.capture_mode.is_some() || window_selection.is_some() {
            let context =
                capture_context(args.app.as_deref(), false, window_selection.as_ref()).await?;
            let capture = resolve_capture_target(
                &context,
                args.capture_mode.as_deref(),
                window_selection.is_some(),
                args.app.as_deref().is_some(),
            )?;
            state_for_summary = Some(ObserveState {
                capture: CaptureArtifact {
                    origin_x: capture.bounds.x,
                    origin_y: capture.bounds.y,
                    width: capture.width,
                    height: capture.height,
                    image_width: capture.width,
                    image_height: capture.height,
                    display_index: context.display.index,
                    capture_mode: capture.mode,
                    window_title: capture.window_title,
                    window_count: capture.window_count,
                    window_capture_strategy: capture.window_capture_strategy,
                    host_exclusion: HostCaptureExclusionState {
                        applied: context.host_self_exclude_applied.unwrap_or(false),
                        frontmost_excluded: context.host_frontmost_excluded.unwrap_or(false),
                        adjusted: capture.host_self_exclude_adjusted,
                        frontmost_app_name: context.host_frontmost_app_name.clone(),
                        frontmost_bundle_id: context.host_frontmost_bundle_id.clone(),
                        redaction_count: 0,
                    },
                },
                app_name: context.app_name,
            });
        } else if let Some(previous_state) = self.get_observe_state(ctx).await {
            state_for_summary = Some(previous_state);
        }

        let capture_bounds = state_for_summary.as_ref().map(ObserveState::capture_bounds);
        let scroll_plan = resolve_scroll_plan(
            args.amount,
            distance,
            semantic_target.is_some(),
            direction,
            target_bounds.as_ref(),
            capture_bounds.as_ref(),
        );
        let (delta_x, delta_y) = scroll_delta_components(direction, scroll_plan.amount);
        action_session.throw_if_emergency_stopped()?;

        run_gui_event(
            "scroll",
            args.app.as_deref(),
            &float_env,
            &[
                ("CODEX_GUI_SCROLL_X", delta_x.to_string()),
                ("CODEX_GUI_SCROLL_Y", delta_y.to_string()),
                ("CODEX_GUI_SCROLL_UNIT", scroll_plan.unit.to_string()),
            ],
        )
        .await?;
        action_session.throw_if_emergency_stopped()?;
        let evidence = self
            .capture_evidence_image(
                ctx,
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
                DEFAULT_POST_ACTION_SETTLE_MS,
            )
            .await?;
        let summary = format!(
            "Scrolled {platform} GUI {dir} using `{preset}` distance ({amount} {unit}).{context}.{evidence_note} Refresh with gui_wait or gui_observe before grounding the next GUI action.",
            platform = PLATFORM_NAME,
            dir = scroll_direction_label(direction),
            preset = scroll_plan.distance_preset,
            amount = scroll_plan.amount,
            unit = scroll_plan.unit,
            context = state_for_summary
                .as_ref()
                .map(|state| format!(
                    " on {} {}",
                    state.capture.capture_mode,
                    describe_capture_subject(state)
                ))
                .unwrap_or_default(),
            evidence_note = if evidence.image_url.is_some() {
                " Attached a refreshed GUI evidence screenshot."
            } else {
                ""
            }
        );
        let mut extra_details = serde_json::Map::new();
        extra_details.insert(
            "action_kind".to_string(),
            JsonValue::String("scroll".to_string()),
        );
        extra_details.insert(
            "scroll_direction".to_string(),
            JsonValue::String(scroll_direction_label(direction).to_string()),
        );
        extra_details.insert(
            "scroll_distance".to_string(),
            JsonValue::String(scroll_plan.distance_preset.to_string()),
        );
        extra_details.insert(
            "scroll_amount".to_string(),
            JsonValue::from(scroll_plan.amount),
        );
        extra_details.insert(
            "scroll_unit".to_string(),
            JsonValue::String(scroll_plan.unit.to_string()),
        );
        if let Some(viewport_dimension) = scroll_plan.viewport_dimension {
            extra_details.insert(
                "scroll_viewport_dimension".to_string(),
                JsonValue::from(viewport_dimension),
            );
        }
        if let Some(viewport_source) = scroll_plan.viewport_source {
            extra_details.insert(
                "scroll_viewport_source".to_string(),
                JsonValue::String(viewport_source.to_string()),
            );
        }
        if let Some(travel_fraction) = scroll_plan.travel_fraction {
            extra_details.insert(
                "scroll_travel_fraction".to_string(),
                JsonValue::from(travel_fraction),
            );
        }
        if let Some((x, y)) = executed_point {
            extra_details.insert("executed_point".to_string(), point_json(x, y));
        }
        if let Some(target_details) = target_details {
            extend_object_fields(&mut extra_details, target_details);
            if let Some(state) = state_for_summary.as_ref() {
                extra_details.insert(
                    "pre_action_capture".to_string(),
                    build_capture_details_from_state(state),
                );
            }
        } else {
            extra_details.insert(
                "grounding_method".to_string(),
                JsonValue::String("targetless".to_string()),
            );
            extra_details.insert("confidence".to_string(), JsonValue::from(1.0));
            if let Some(state) = state_for_summary.as_ref() {
                extra_details.insert(
                    "pre_action_capture".to_string(),
                    build_capture_details_from_state(state),
                );
            }
        }
        Ok(self.build_gui_output(
            summary,
            evidence.state,
            evidence.image_url,
            true,
            Some(JsonValue::Object(extra_details)),
        ))
    }

    pub async fn handle_type(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::TypeArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let mut action_session = begin_gui_action_session(&ctx.session_id, "gui_type", true)?;
        action_session.hide_other_apps(args.app.as_deref());
        let window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        let text = resolve_type_value(&args)?;
        let semantic_target = normalize_optional_string(args.target.as_deref());
        let location_hint = normalize_optional_string(args.location_hint.as_deref());
        let scope = normalize_optional_string(args.scope.as_deref());
        enforce_gui_tool_capability(
            ctx.image_input_supported(),
            "gui_type",
            semantic_target.is_some(),
        )
        .await?;
        let strategy = normalize_optional_string(args.type_strategy.as_deref());
        if let Some(strategy) = strategy.as_deref()
            && !matches!(
                strategy,
                "clipboard_paste"
                    | "physical_keys"
                    | "system_events_paste"
                    | "system_events_keystroke"
                    | "system_events_keystroke_chars"
            )
        {
            return Err(respond_to_model(format!(
                "gui_type.type_strategy only supports `clipboard_paste`, `physical_keys`, `system_events_paste`, `system_events_keystroke`, or `system_events_keystroke_chars`, got `{strategy}`"
            )));
        }
        action_session.throw_if_emergency_stopped()?;

        let mut target_details = None;
        let mut executed_point = None;
        let mut pre_action_capture = None;
        if let Some(target) = semantic_target.as_deref() {
            let grounded = self
                .resolve_gui_target(
                    ctx,
                    GuiTargetRequest {
                        app: args.app.as_deref(),
                        capture_mode: args.capture_mode.as_deref(),
                        window_selection: window_selection.as_ref(),
                        target,
                        location_hint: location_hint.as_deref(),
                        scope: scope.as_deref(),
                        grounding_mode: args.grounding_mode.as_deref(),
                        action: "type",
                        related_target: None,
                        related_scope: None,
                        related_location_hint: None,
                        related_point: None,
                    },
                )
                .await?;
            let grounded = grounded.ok_or_else(|| {
                respond_to_model(format!(
                    "Could not resolve semantic input target `{target}`."
                ))
            })?;
            let details = build_target_resolution_details(target, &grounded);
            let resolved = grounded.resolved;
            let focus_point = targeted_type_focus_point(&resolved);
            run_gui_event(
                "click",
                args.app.as_deref(),
                &[
                    ("CODEX_GUI_X", focus_point.x),
                    ("CODEX_GUI_Y", focus_point.y),
                ],
                &[],
            )
            .await?;
            action_session.throw_if_emergency_stopped()?;
            sleep(Duration::from_millis(DEFAULT_TYPE_FOCUS_SETTLE_MS as u64)).await;
            action_session.throw_if_emergency_stopped()?;
            executed_point = Some((focus_point.x, focus_point.y));
            pre_action_capture = Some(build_capture_details_from_state(&resolved.capture_state));
            target_details = Some(details);
        } else {
            prepare_targeted_gui_action(
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
            )
            .await?;
            action_session.throw_if_emergency_stopped()?;
        }

        let replace = args.replace.unwrap_or(true);
        let submit = args.submit.unwrap_or(false);
        let effective_strategy = if matches!(
            strategy.as_deref(),
            Some("system_events_paste")
                | Some("system_events_keystroke")
                | Some("system_events_keystroke_chars")
        ) {
            run_system_events_type(
                args.app.as_deref(),
                window_selection.as_ref(),
                &text,
                replace,
                submit,
                strategy
                    .as_deref()
                    .expect("system events strategy should be present"),
            )
            .await?;
            strategy.clone()
        } else if let Some(native_strategy) = strategy.as_deref() {
            run_gui_event(
                "type_text",
                args.app.as_deref(),
                &[],
                &[
                    ("CODEX_GUI_TEXT", text.clone()),
                    (
                        "CODEX_GUI_REPLACE",
                        if replace { "1" } else { "0" }.to_string(),
                    ),
                    (
                        "CODEX_GUI_SUBMIT",
                        if submit { "1" } else { "0" }.to_string(),
                    ),
                    ("CODEX_GUI_TYPE_STRATEGY", native_strategy.to_string()),
                ],
            )
            .await?;
            Some(native_strategy.to_string())
        } else {
            // Prefer the paste-based macOS typing path first, then fall back
            // to native unicode injection if System Events is unavailable.
            if run_system_events_type(
                args.app.as_deref(),
                window_selection.as_ref(),
                &text,
                replace,
                submit,
                "system_events_paste",
            )
            .await
            .is_err()
            {
                let native_strategy = "unicode";
                run_gui_event(
                    "type_text",
                    args.app.as_deref(),
                    &[],
                    &[
                        ("CODEX_GUI_TEXT", text.clone()),
                        (
                            "CODEX_GUI_REPLACE",
                            if replace { "1" } else { "0" }.to_string(),
                        ),
                        (
                            "CODEX_GUI_SUBMIT",
                            if submit { "1" } else { "0" }.to_string(),
                        ),
                        ("CODEX_GUI_TYPE_STRATEGY", native_strategy.to_string()),
                    ],
                )
                .await?;
                Some("unicode".to_string())
            } else {
                Some("system_events_paste".to_string())
            }
        };
        action_session.throw_if_emergency_stopped()?;

        let evidence = self
            .capture_evidence_image(
                ctx,
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
                DEFAULT_POST_TYPE_SETTLE_MS,
            )
            .await?;
        let summary = format!(
            "Typed {} character(s){}{}.{} Use gui_wait or gui_observe to verify the field contents and any follow-on UI changes.",
            text.chars().count(),
            strategy
                .as_ref()
                .map(|value| format!(" with strategy `{value}`"))
                .unwrap_or_default(),
            semantic_target
                .as_ref()
                .map(|target| format!(" into semantic target `{target}`"))
                .unwrap_or_default(),
            if evidence.image_url.is_some() {
                " Attached a refreshed GUI evidence screenshot."
            } else {
                ""
            }
        );
        let mut extra_details = serde_json::Map::new();
        extra_details.insert(
            "action_kind".to_string(),
            JsonValue::String("type_text".to_string()),
        );
        if let Some(strategy) = strategy {
            extra_details.insert(
                "type_strategy_requested".to_string(),
                JsonValue::String(strategy),
            );
        }
        if let Some(effective_strategy) = effective_strategy {
            extra_details.insert(
                "type_strategy_effective".to_string(),
                JsonValue::String(effective_strategy),
            );
        }
        if let Some((x, y)) = executed_point {
            extra_details.insert("executed_point".to_string(), point_json(x, y));
        }
        if let Some(target_details) = target_details {
            extend_object_fields(&mut extra_details, target_details);
            if let Some(pre_action_capture) = pre_action_capture {
                extra_details.insert("pre_action_capture".to_string(), pre_action_capture);
            }
        } else {
            extra_details.insert(
                "grounding_method".to_string(),
                JsonValue::String("targetless".to_string()),
            );
            extra_details.insert("confidence".to_string(), JsonValue::from(1.0));
        }
        Ok(self.build_gui_output(
            summary,
            evidence.state,
            evidence.image_url,
            true,
            Some(JsonValue::Object(extra_details)),
        ))
    }

    pub async fn handle_key(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::KeyArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let mut action_session = begin_gui_action_session(&ctx.session_id, "gui_key", true)?;
        action_session.hide_other_apps(args.app.as_deref());
        let window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        let repeat = args.repeat.unwrap_or(1).max(1);
        let mut modifiers = args.modifiers.unwrap_or_default();
        enforce_gui_tool_capability(ctx.image_input_supported(), "gui_key", false).await?;
        let key_code = resolve_key_code(&args.key, &mut modifiers)?;
        let modifiers_env = modifiers.join(",");
        prepare_targeted_gui_action(
            args.app.as_deref(),
            args.capture_mode.as_deref(),
            window_selection.as_ref(),
        )
        .await?;
        action_session.throw_if_emergency_stopped()?;

        // Escape keycode 53: tell the emergency stop monitor to treat the
        // upcoming Escape detection as a programmatic event, not a user abort.
        if key_code == 53 {
            action_session.expect_escape();
        }

        run_gui_event(
            "key_press",
            args.app.as_deref(),
            &[],
            &[
                ("CODEX_GUI_KEY_CODE", key_code.to_string()),
                ("CODEX_GUI_REPEAT", repeat.to_string()),
                ("CODEX_GUI_MODIFIERS", modifiers_env.clone()),
            ],
        )
        .await?;
        action_session.throw_if_emergency_stopped()?;

        let evidence = self
            .capture_evidence_image(
                ctx,
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
                DEFAULT_POST_TYPE_SETTLE_MS,
            )
            .await?;
        let summary = format!(
            "Pressed key `{}`{} {} time(s).{} Use gui_wait or gui_observe if this shortcut should change the visible UI.",
            args.key,
            if modifiers_env.is_empty() {
                String::new()
            } else {
                format!(" with modifiers [{}]", modifiers_env)
            },
            repeat,
            if evidence.image_url.is_some() {
                " Attached a refreshed GUI evidence screenshot."
            } else {
                ""
            }
        );
        Ok(self.build_gui_output(
            summary,
            evidence.state,
            evidence.image_url,
            true,
            Some(serde_json::json!({
                "action_kind": "key_press",
                "grounding_method": "targetless",
                "confidence": 1.0,
                "repeat": repeat,
            })),
        ))
    }

    pub async fn handle_move(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::MoveArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        let mut action_session = begin_gui_action_session(&ctx.session_id, "gui_move", true)?;
        action_session.hide_other_apps(args.app.as_deref());
        enforce_gui_tool_capability(ctx.image_input_supported(), "gui_move", false).await?;
        action_session.throw_if_emergency_stopped()?;
        run_gui_event(
            "move_cursor",
            args.app.as_deref(),
            &[("CODEX_GUI_X", args.x), ("CODEX_GUI_Y", args.y)],
            &[("CODEX_GUI_SETTLE_MS", DEFAULT_HOVER_SETTLE_MS.to_string())],
        )
        .await?;
        action_session.throw_if_emergency_stopped()?;

        let summary = format!(
            "Moved the {platform} pointer to absolute display coordinate ({x}, {y}).",
            platform = PLATFORM_NAME,
            x = args.x.round(),
            y = args.y.round()
        );
        Ok(GuiToolOutput {
            message: summary.clone(),
            image_url: None,
            code_result: serde_json::json!({
                "message": summary,
                "action_kind": "move_cursor",
                "grounding_method": "absolute_coordinates",
                "confidence": 1.0,
                "executed_point": {
                    "x": args.x,
                    "y": args.y,
                },
                "app": args.app,
            }),
            success: true,
        })
    }

    pub async fn handle_batch(
        &self,
        ctx: &GuiHandlerContext,
        args: schemas::BatchArgs,
    ) -> Result<GuiToolOutput, ToolError> {
        if !ctx.settings().batch_enabled {
            return Err(respond_to_model(
                "gui_batch is disabled. Enable it with `[tools.gui] batch = true`.".to_string(),
            ));
        }
        let mut action_session = begin_gui_action_session(&ctx.session_id, "gui_batch", true)?;
        if args.steps.is_empty() {
            return Err(respond_to_model(
                "gui_batch requires at least one step.".to_string(),
            ));
        }
        if args.steps.len() > MAX_BATCH_STEPS {
            return Err(respond_to_model(format!(
                "gui_batch supports at most {MAX_BATCH_STEPS} steps, got {}.",
                args.steps.len()
            )));
        }
        for (i, step) in args.steps.iter().enumerate() {
            if !matches!(
                step.action.as_str(),
                "click" | "type" | "key" | "scroll" | "drag"
            ) {
                return Err(respond_to_model(format!(
                    "gui_batch step {i}: unsupported action `{}`. Supported: click, type, key, scroll, drag.",
                    step.action
                )));
            }
        }

        action_session.hide_other_apps(args.app.as_deref());
        let window_selection = normalize_window_selection(
            args.window_title.as_deref(),
            args.window_selector.as_ref(),
        )?;
        enforce_gui_tool_capability(ctx.image_input_supported(), "gui_batch", true).await?;

        // ── Step 1: Collect grounding targets ──────────────────────────
        let mut grounding_targets: Vec<BatchGroundingTarget> = Vec::new();
        for (i, step) in args.steps.iter().enumerate() {
            if step.action == "drag" {
                // Drag produces two grounding targets: source + destination.
                if let Some(from) = normalize_optional_string(step.from_target.as_deref()) {
                    grounding_targets.push(BatchGroundingTarget {
                        step_index: i,
                        role: BatchGroundingRole::Primary,
                        target: from,
                        action: "drag_source".to_string(),
                        location_hint: normalize_optional_string(
                            step.from_location_hint.as_deref(),
                        ),
                        scope: normalize_optional_string(step.from_scope.as_deref()),
                    });
                }
                if let Some(to) = normalize_optional_string(step.to_target.as_deref()) {
                    grounding_targets.push(BatchGroundingTarget {
                        step_index: i,
                        role: BatchGroundingRole::DragDestination,
                        target: to,
                        action: "drag_destination".to_string(),
                        location_hint: normalize_optional_string(step.to_location_hint.as_deref()),
                        scope: normalize_optional_string(step.to_scope.as_deref()),
                    });
                }
            } else {
                let semantic_target = normalize_optional_string(step.target.as_deref());
                if let Some(target) = semantic_target {
                    grounding_targets.push(BatchGroundingTarget {
                        step_index: i,
                        role: BatchGroundingRole::Primary,
                        target,
                        action: step.action.clone(),
                        location_hint: normalize_optional_string(step.location_hint.as_deref()),
                        scope: normalize_optional_string(step.scope.as_deref()),
                    });
                }
            }
        }

        // ── Step 2 & 3: Screenshot + grounding (only if needed) ───────
        let mut batch_capture_state: Option<ObserveState> = None;
        let grounded_results = if !grounding_targets.is_empty() {
            // Take ONE screenshot for all grounding targets.
            let observation = observe_platform(
                args.app.as_deref(),
                /*activate_app*/ true,
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
                args.app.as_deref().is_some(),
            )
            .await?;
            let capture_state = observation.state;
            self.set_observe_state(&ctx.session_id, capture_state.clone())
                .await;

            let image_bytes = if observation.image_bytes.is_empty() {
                let bounds = capture_state.capture_bounds();
                capture_region(
                    &bounds,
                    capture_state.capture.image_width,
                    capture_state.capture.image_height,
                )
                .await?
            } else {
                observation.image_bytes
            };
            // Dispatch grounding based on configured strategy.
            let settings = ctx.settings();
            let strategy = settings.batch_grounding_strategy.as_str();
            let results = if strategy == "unified" {
                resolve_batch_grounded_targets_unified(
                    ctx.sampler()?,
                    &grounding_targets,
                    &capture_state,
                    &image_bytes,
                )
                .await?
            } else {
                resolve_batch_grounded_targets(
                    ctx.sampler()?,
                    &grounding_targets,
                    &capture_state,
                    &image_bytes,
                )
                .await?
            };
            batch_capture_state = Some(capture_state);
            results
        } else {
            // No semantic targets — just activate the app without screenshotting.
            prepare_targeted_gui_action(
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
            )
            .await?;
            Vec::new()
        };

        // Build a map from (step_index, role) → resolved target.
        let mut resolved_map: HashMap<(usize, BatchGroundingRole), ResolvedTarget> = HashMap::new();
        for (i, result) in grounded_results.into_iter().enumerate() {
            let gt = &grounding_targets[i];
            if let Some(resolved) = result {
                resolved_map.insert((gt.step_index, gt.role.clone()), resolved);
            } else {
                return Err(respond_to_model(format!(
                    "gui_batch: could not resolve target `{}` for step {} ({} action).",
                    gt.target, gt.step_index, gt.action
                )));
            }
        }

        // ── Step 4: Execute each action sequentially ───────────────────
        let mut step_summaries: Vec<String> = Vec::new();
        let mut step_details: Vec<JsonValue> = Vec::new();

        let batch_action_delay_ms = ctx.settings().batch_action_delay_ms;
        for (i, step) in args.steps.iter().enumerate() {
            action_session.throw_if_emergency_stopped()?;

            // Delay between steps to let the UI settle.
            if i > 0 && batch_action_delay_ms > 0 {
                sleep(Duration::from_millis(batch_action_delay_ms)).await;
            }

            match step.action.as_str() {
                "click" => {
                    let resolved = resolved_map
                        .get(&(i, BatchGroundingRole::Primary))
                        .ok_or_else(|| {
                            respond_to_model(format!(
                                "gui_batch step {i}: click requires a `target`."
                            ))
                        })?;
                    let button = step.button.as_deref().unwrap_or("left");
                    let clicks = step.clicks.unwrap_or(1);
                    let hold_ms = step.hold_ms.unwrap_or(DEFAULT_CLICK_AND_HOLD_MS).max(1);
                    let settle_ms = step.settle_ms.unwrap_or(DEFAULT_HOVER_SETTLE_MS).max(1);
                    let event_mode = match (button, clicks, step.hold_ms) {
                        ("none", 1, None) => "move_cursor",
                        ("left", 1, None) => "click",
                        ("left", 1, Some(_)) => "click_and_hold",
                        ("left", 2, None) => "double_click",
                        ("right", 1, None) => "right_click",
                        _ => {
                            return Err(respond_to_model(format!(
                                "gui_batch step {i}: unsupported click variant (button={button}, clicks={clicks})"
                            )));
                        }
                    };
                    run_gui_event(
                        event_mode,
                        args.app.as_deref(),
                        &[
                            ("CODEX_GUI_X", resolved.point.x),
                            ("CODEX_GUI_Y", resolved.point.y),
                        ],
                        &[
                            ("CODEX_GUI_HOLD_MS", hold_ms.to_string()),
                            ("CODEX_GUI_SETTLE_MS", settle_ms.to_string()),
                        ],
                    )
                    .await?;
                    let target_label = step.target.as_deref().unwrap_or("unknown");
                    step_summaries.push(format!(
                        "step {i}: {action} `{target_label}` at ({x}, {y})",
                        action = describe_click_action(button, clicks, step.hold_ms.is_some()),
                        x = resolved.point.x.round(),
                        y = resolved.point.y.round(),
                    ));
                    step_details.push(serde_json::json!({
                        "step": i,
                        "action": "click",
                        "event_mode": event_mode,
                        "target": target_label,
                        "point": { "x": resolved.point.x.round(), "y": resolved.point.y.round() },
                        "confidence": resolved.confidence,
                    }));
                }
                "type" => {
                    // Resolve text value
                    let text = if let Some(value) = &step.value {
                        value.clone()
                    } else if let Some(env_var) = &step.secret_env_var {
                        std::env::var(env_var).map_err(|_| {
                            respond_to_model(format!(
                                "gui_batch step {i}: secret_env_var `{env_var}` is not set."
                            ))
                        })?
                    } else if let Some(cmd_var) = &step.secret_command_env_var {
                        std::env::var(cmd_var).map_err(|_| {
                            respond_to_model(format!(
                                "gui_batch step {i}: secret_command_env_var `{cmd_var}` is not set."
                            ))
                        })?
                    } else {
                        return Err(respond_to_model(format!(
                            "gui_batch step {i}: type action requires `value`, `secret_env_var`, or `secret_command_env_var`."
                        )));
                    };

                    // If target was specified, click to focus first
                    if let Some(resolved) = resolved_map.get(&(i, BatchGroundingRole::Primary)) {
                        let focus_point = targeted_type_focus_point(resolved);
                        run_gui_event(
                            "click",
                            args.app.as_deref(),
                            &[
                                ("CODEX_GUI_X", focus_point.x),
                                ("CODEX_GUI_Y", focus_point.y),
                            ],
                            &[],
                        )
                        .await?;
                        action_session.throw_if_emergency_stopped()?;
                        sleep(Duration::from_millis(DEFAULT_TYPE_FOCUS_SETTLE_MS as u64)).await;
                        action_session.throw_if_emergency_stopped()?;
                    }

                    let replace = step.replace.unwrap_or(true);
                    let submit = step.submit.unwrap_or(false);
                    let strategy = normalize_optional_string(step.type_strategy.as_deref());

                    if matches!(
                        strategy.as_deref(),
                        Some("system_events_paste")
                            | Some("system_events_keystroke")
                            | Some("system_events_keystroke_chars")
                    ) {
                        run_system_events_type(
                            args.app.as_deref(),
                            window_selection.as_ref(),
                            &text,
                            replace,
                            submit,
                            strategy.as_deref().unwrap(),
                        )
                        .await?;
                    } else if let Some(native_strategy) = strategy.as_deref() {
                        run_gui_event(
                            "type_text",
                            args.app.as_deref(),
                            &[],
                            &[
                                ("CODEX_GUI_TEXT", text.clone()),
                                (
                                    "CODEX_GUI_REPLACE",
                                    if replace { "1" } else { "0" }.to_string(),
                                ),
                                (
                                    "CODEX_GUI_SUBMIT",
                                    if submit { "1" } else { "0" }.to_string(),
                                ),
                                ("CODEX_GUI_TYPE_STRATEGY", native_strategy.to_string()),
                            ],
                        )
                        .await?;
                    } else {
                        // Default: try system_events_paste, fallback to unicode
                        if run_system_events_type(
                            args.app.as_deref(),
                            window_selection.as_ref(),
                            &text,
                            replace,
                            submit,
                            "system_events_paste",
                        )
                        .await
                        .is_err()
                        {
                            run_gui_event(
                                "type_text",
                                args.app.as_deref(),
                                &[],
                                &[
                                    ("CODEX_GUI_TEXT", text.clone()),
                                    (
                                        "CODEX_GUI_REPLACE",
                                        if replace { "1" } else { "0" }.to_string(),
                                    ),
                                    (
                                        "CODEX_GUI_SUBMIT",
                                        if submit { "1" } else { "0" }.to_string(),
                                    ),
                                    ("CODEX_GUI_TYPE_STRATEGY", "unicode".to_string()),
                                ],
                            )
                            .await?;
                        }
                    }

                    let target_label = step.target.as_deref().unwrap_or("focused field");
                    step_summaries.push(format!(
                        "step {i}: typed {} chars into `{target_label}`",
                        text.chars().count(),
                    ));
                    step_details.push(serde_json::json!({
                        "step": i,
                        "action": "type",
                        "chars_typed": text.chars().count(),
                        "target": target_label,
                    }));
                }
                "key" => {
                    let key = step.key.as_deref().ok_or_else(|| {
                        respond_to_model(format!("gui_batch step {i}: key action requires `key`."))
                    })?;
                    let mut modifiers = step.modifiers.clone().unwrap_or_default();
                    let key_code = resolve_key_code(key, &mut modifiers)?;
                    let repeat = step.repeat.unwrap_or(1).max(1);
                    let modifiers_env = modifiers.join(",");

                    if key_code == 53 {
                        action_session.expect_escape();
                    }

                    run_gui_event(
                        "key_press",
                        args.app.as_deref(),
                        &[],
                        &[
                            ("CODEX_GUI_KEY_CODE", key_code.to_string()),
                            ("CODEX_GUI_REPEAT", repeat.to_string()),
                            ("CODEX_GUI_MODIFIERS", modifiers_env.clone()),
                        ],
                    )
                    .await?;

                    step_summaries.push(format!(
                        "step {i}: pressed key `{key}`{}",
                        if modifiers_env.is_empty() {
                            String::new()
                        } else {
                            format!(" with [{modifiers_env}]")
                        },
                    ));
                    step_details.push(serde_json::json!({
                        "step": i,
                        "action": "key",
                        "key": key,
                        "repeat": repeat,
                    }));
                }
                "scroll" => {
                    let direction = normalize_scroll_direction(step.direction.as_deref())?;
                    let distance = normalize_scroll_distance(step.distance.as_deref())?;

                    let mut float_env: Vec<(&str, f64)> = Vec::new();
                    let mut target_bounds = None;
                    if let Some(resolved) = resolved_map.get(&(i, BatchGroundingRole::Primary)) {
                        float_env.push(("CODEX_GUI_X", resolved.point.x));
                        float_env.push(("CODEX_GUI_Y", resolved.point.y));
                        target_bounds = Some(resolved.bounds);
                    }

                    let capture_bounds = batch_capture_state
                        .as_ref()
                        .map(ObserveState::capture_bounds);
                    let scroll_plan = resolve_scroll_plan(
                        step.amount,
                        distance,
                        resolved_map.contains_key(&(i, BatchGroundingRole::Primary)),
                        direction,
                        target_bounds.as_ref(),
                        capture_bounds.as_ref(),
                    );
                    let (delta_x, delta_y) = scroll_delta_components(direction, scroll_plan.amount);

                    run_gui_event(
                        "scroll",
                        args.app.as_deref(),
                        &float_env,
                        &[
                            ("CODEX_GUI_SCROLL_X", delta_x.to_string()),
                            ("CODEX_GUI_SCROLL_Y", delta_y.to_string()),
                            ("CODEX_GUI_SCROLL_UNIT", scroll_plan.unit.to_string()),
                        ],
                    )
                    .await?;

                    let target_label = step.target.as_deref().unwrap_or("current surface");
                    step_summaries.push(format!(
                        "step {i}: scrolled {dir} on `{target_label}`",
                        dir = scroll_direction_label(direction),
                    ));
                    step_details.push(serde_json::json!({
                        "step": i,
                        "action": "scroll",
                        "direction": scroll_direction_label(direction),
                        "target": target_label,
                    }));
                }
                "drag" => {
                    let from_target_str = step.from_target.as_deref().ok_or_else(|| {
                        respond_to_model(format!(
                            "gui_batch step {i}: drag requires `from_target`."
                        ))
                    })?;
                    let to_target_str = step.to_target.as_deref().ok_or_else(|| {
                        respond_to_model(format!("gui_batch step {i}: drag requires `to_target`."))
                    })?;
                    let from_resolved = resolved_map
                        .get(&(i, BatchGroundingRole::Primary))
                        .ok_or_else(|| {
                            respond_to_model(format!(
                                "gui_batch step {i}: could not resolve drag source `{from_target_str}`."
                            ))
                        })?;
                    let to_resolved = resolved_map
                        .get(&(i, BatchGroundingRole::DragDestination))
                        .ok_or_else(|| {
                            respond_to_model(format!(
                                "gui_batch step {i}: could not resolve drag destination `{to_target_str}`."
                            ))
                        })?;

                    let duration_ms = step.duration_ms.unwrap_or(DEFAULT_DRAG_DURATION_MS).max(1);
                    run_gui_event(
                        "drag",
                        args.app.as_deref(),
                        &[
                            ("CODEX_GUI_FROM_X", from_resolved.point.x),
                            ("CODEX_GUI_FROM_Y", from_resolved.point.y),
                            ("CODEX_GUI_TO_X", to_resolved.point.x),
                            ("CODEX_GUI_TO_Y", to_resolved.point.y),
                        ],
                        &[
                            ("CODEX_GUI_DURATION_MS", duration_ms.to_string()),
                            ("CODEX_GUI_STEPS", DEFAULT_DRAG_STEPS.to_string()),
                        ],
                    )
                    .await?;

                    step_summaries.push(format!(
                        "step {i}: dragged `{from_target_str}` → `{to_target_str}`",
                    ));
                    step_details.push(serde_json::json!({
                        "step": i,
                        "action": "drag",
                        "from_target": from_target_str,
                        "to_target": to_target_str,
                        "from_point": { "x": from_resolved.point.x.round(), "y": from_resolved.point.y.round() },
                        "to_point": { "x": to_resolved.point.x.round(), "y": to_resolved.point.y.round() },
                    }));
                }
                _ => unreachable!("action validated above"),
            }
        }

        action_session.throw_if_emergency_stopped()?;

        // ── Step 5: ONE evidence screenshot ────────────────────────────
        // Use shorter settle for batches that only contain typing and key
        // presses; use the full settle for click/scroll actions that may
        // trigger larger UI changes.
        let has_click_or_scroll = args
            .steps
            .iter()
            .any(|s| s.action == "click" || s.action == "scroll" || s.action == "drag");
        let settle_ms = if has_click_or_scroll {
            DEFAULT_POST_ACTION_SETTLE_MS
        } else {
            DEFAULT_POST_TYPE_SETTLE_MS
        };
        let evidence = self
            .capture_evidence_image(
                ctx,
                args.app.as_deref(),
                args.capture_mode.as_deref(),
                window_selection.as_ref(),
                settle_ms,
            )
            .await?;

        // ── Step 6: Build combined result ──────────────────────────────
        let summary = format!(
            "Executed {} GUI actions in batch on {platform}:\n{steps}{evidence_note}\nUse gui_wait or gui_observe to verify the resulting UI state.",
            args.steps.len(),
            platform = PLATFORM_NAME,
            steps = step_summaries
                .iter()
                .map(|s| format!("  - {s}"))
                .collect::<Vec<_>>()
                .join("\n"),
            evidence_note = if evidence.image_url.is_some() {
                "\nAttached a refreshed GUI evidence screenshot."
            } else {
                ""
            }
        );

        let extra_details = serde_json::json!({
            "action_kind": "batch",
            "steps_count": args.steps.len(),
            "grounding_targets_count": grounding_targets.len(),
            "grounding_method": "batch_grounding",
            "steps": step_details,
        });

        Ok(self.build_gui_output(
            summary,
            evidence.state,
            evidence.image_url,
            true,
            Some(extra_details),
        ))
    }

    async fn get_observe_state(&self, ctx: &GuiHandlerContext) -> Option<ObserveState> {
        self.observe_state
            .lock()
            .await
            .get(&ctx.session_id)
            .cloned()
    }

    async fn capture_post_action_evidence(
        &self,
        ctx: &GuiHandlerContext,
        app: Option<&str>,
        capture_mode: Option<&str>,
        window_selection: Option<&WindowSelector>,
        default_settle_ms: i64,
    ) -> Result<ActionEvidence, ToolError> {
        let attach_image = ctx.image_input_supported();
        let mut app = normalize_optional_string(app);
        let mut capture_mode = normalize_optional_string(capture_mode);
        let mut window_selection = window_selection.cloned();

        if app.is_none()
            && capture_mode.is_none()
            && window_selection.is_none()
            && let Some(previous_state) = self.get_observe_state(ctx).await
        {
            app = previous_state.app_name.clone();
            capture_mode = Some(previous_state.capture.capture_mode.to_string());
            if previous_state.capture.capture_mode == CaptureMode::Window {
                window_selection =
                    previous_state
                        .capture
                        .window_title
                        .as_ref()
                        .map(|title| WindowSelector {
                            title: Some(title.clone()),
                            title_contains: None,
                            index: None,
                        });
            }
        }

        sleep(Duration::from_millis(default_settle_ms.max(0) as u64)).await;

        // Do not re-activate the app when capturing post-action evidence.
        // The action (click/drag/type/etc.) already targeted the active app,
        // and re-activation can reset transient UI state such as selected
        // items, hover highlights, or chess piece selection.
        let observation = observe_platform(
            app.as_deref(),
            /*activate_app*/ false,
            capture_mode.as_deref(),
            window_selection.as_ref(),
            app.as_deref().is_some(),
        )
        .await?;
        let image_bytes = if attach_image {
            Some(observation.image_bytes.clone())
        } else {
            None
        };
        let image_url = image_bytes.as_deref().map(screenshot_data_url);
        let state = observation.state;
        self.set_observe_state(&ctx.session_id, state.clone()).await;

        Ok(ActionEvidence { image_url, state })
    }

    fn build_gui_output(
        &self,
        summary: String,
        state: ObserveState,
        image_url: Option<String>,
        success: bool,
        extra_details: Option<JsonValue>,
    ) -> GuiToolOutput {
        let mut code_result = serde_json::json!({
            "message": summary,
            "image_url": image_url,
            "display_index": state.capture.display_index,
            "capture_mode": state.capture.capture_mode,
            "origin_x": state.capture.origin_x,
            "origin_y": state.capture.origin_y,
            "width": state.capture.width,
            "height": state.capture.height,
            "image_width": state.capture.image_width,
            "image_height": state.capture.image_height,
            "capture_scale_x": state.capture.scale_x(),
            "capture_scale_y": state.capture.scale_y(),
            "app": state.app_name,
            "window_title": state.capture.window_title,
            "window_count": state.capture.window_count,
            "window_capture_strategy": state.capture.window_capture_strategy,
            "capture_host_self_exclude_applied": state.capture.host_exclusion.applied,
            "capture_host_frontmost_excluded": state.capture.host_exclusion.frontmost_excluded,
            "capture_host_self_exclude_adjusted": state.capture.host_exclusion.adjusted,
            "capture_host_frontmost_app": state.capture.host_exclusion.frontmost_app_name,
            "capture_host_frontmost_bundle_id": state.capture.host_exclusion.frontmost_bundle_id,
            "capture_host_self_exclude_redaction_count": state.capture.host_exclusion.redaction_count,
        });
        if let Some(JsonValue::Object(extra)) = extra_details
            && let Some(base) = code_result.as_object_mut()
        {
            for (key, value) in extra {
                base.insert(key, value);
            }
        }

        GuiToolOutput {
            message: code_result
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            image_url,
            code_result,
            success,
        }
    }

    async fn probe_semantic_target(
        &self,
        ctx: &GuiHandlerContext,
        request: GuiTargetRequest<'_>,
    ) -> Result<TargetProbe, ToolError> {
        let observation = observe_platform(
            request.app,
            true,
            request.capture_mode,
            request.window_selection,
            request.app.is_some(),
        )
        .await?;
        let capture_state = observation.state;
        let target = self
            .ground_target(ctx, request, &capture_state, &observation.image_bytes)
            .await?;
        Ok(TargetProbe {
            capture_state,
            target,
            timed_out: false,
        })
    }

    async fn probe_semantic_target_before_deadline(
        &self,
        ctx: &GuiHandlerContext,
        request: GuiTargetRequest<'_>,
        deadline: Instant,
    ) -> Result<TargetProbe, ToolError> {
        // Do not activate the app during polling probes.  The caller
        // (gui_wait) may be checking whether a transient state persists
        // and re-activation could dismiss it.
        let observation = observe_platform(
            request.app,
            /*activate_app*/ false,
            request.capture_mode,
            request.window_selection,
            request.app.is_some(),
        )
        .await?;
        let capture_state = observation.state;
        let Some(remaining) = remaining_wait_budget_duration(deadline) else {
            return Ok(TargetProbe {
                capture_state,
                target: None,
                timed_out: true,
            });
        };
        let target = match timeout(
            remaining,
            self.ground_target(ctx, request, &capture_state, &observation.image_bytes),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Ok(TargetProbe {
                    capture_state,
                    target: None,
                    timed_out: true,
                });
            }
        };
        Ok(TargetProbe {
            capture_state,
            target,
            timed_out: false,
        })
    }

    async fn resolve_gui_target(
        &self,
        ctx: &GuiHandlerContext,
        request: GuiTargetRequest<'_>,
    ) -> Result<Option<GroundedGuiTarget>, ToolError> {
        let initial_probe = self
            .probe_semantic_target(
                ctx,
                GuiTargetRequest {
                    capture_mode: fallback_probe_capture_mode(
                        request.capture_mode,
                        /*attempt*/ 1,
                        request.app,
                    ),
                    ..request
                },
            )
            .await?;
        if let Some(resolved) = initial_probe.target {
            return Ok(Some(GroundedGuiTarget {
                grounding_method: "grounding",
                resolved,
            }));
        }

        let should_retry_with_display = request.capture_mode.is_none()
            && request.app.is_some()
            && initial_probe.capture_state.capture.capture_mode == CaptureMode::Window;
        if !should_retry_with_display {
            return Ok(None);
        }

        let fallback_probe = self
            .probe_semantic_target(
                ctx,
                GuiTargetRequest {
                    capture_mode: fallback_probe_capture_mode(
                        request.capture_mode,
                        /*attempt*/ 2,
                        request.app,
                    ),
                    ..request
                },
            )
            .await?;
        Ok(fallback_probe.target.map(|resolved| GroundedGuiTarget {
            grounding_method: "grounding_display_fallback",
            resolved,
        }))
    }

    async fn ground_target(
        &self,
        ctx: &GuiHandlerContext,
        request: GuiTargetRequest<'_>,
        capture_state: &ObserveState,
        image_bytes: &[u8],
    ) -> Result<Option<ResolvedTarget>, ToolError> {
        if !ctx.image_input_supported() {
            return Err(respond_to_model(GUI_IMAGE_UNSUPPORTED_MESSAGE.to_string()));
        }

        let image_bytes = if image_bytes.is_empty() {
            let bounds = capture_state.capture_bounds();
            capture_region(
                &bounds,
                capture_state.capture.image_width,
                capture_state.capture.image_height,
            )
            .await?
        } else {
            image_bytes.to_vec()
        };
        default_gui_grounding_provider()
            .ground(ctx, request, capture_state, &image_bytes)
            .await
    }

    async fn capture_evidence_image(
        &self,
        ctx: &GuiHandlerContext,
        app: Option<&str>,
        capture_mode: Option<&str>,
        window_selection: Option<&WindowSelector>,
        default_settle_ms: i64,
    ) -> Result<ActionEvidence, ToolError> {
        self.capture_post_action_evidence(
            ctx,
            app,
            capture_mode,
            window_selection,
            default_settle_ms,
        )
        .await
    }

    async fn probe_for_target(
        &self,
        ctx: &GuiHandlerContext,
        request: GuiTargetRequest<'_>,
        state: &'static str,
        timeout_ms: i64,
        interval_ms: i64,
    ) -> Result<GuiTargetProbeResult, ToolError> {
        let attach_image = ctx.image_input_supported();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
        let mut attempts = 0;
        let initial_probe = self
            .probe_semantic_target_before_deadline(
                ctx,
                GuiTargetRequest {
                    capture_mode: fallback_probe_capture_mode(request.capture_mode, 1, request.app),
                    ..request
                },
                deadline,
            )
            .await?;
        let mut last_grounded = initial_probe.target.map(|resolved| GroundedGuiTarget {
            grounding_method: "grounding",
            resolved,
        });
        attempts += 1;
        let mut current_state = initial_probe.capture_state;
        let mut budget_exhausted = initial_probe.timed_out;
        let initial_satisfied = match state {
            "appear" => last_grounded.is_some(),
            "disappear" => last_grounded.is_none(),
            _ => false,
        };
        let mut consecutive_satisfied = if initial_satisfied { 1 } else { 0 };
        let mut matched = consecutive_satisfied >= WAIT_CONFIRMATION_COUNT;

        while !matched && !budget_exhausted {
            let Some(remaining_ms) = remaining_wait_budget_ms(deadline) else {
                break;
            };
            let sleep_ms = remaining_ms.min(interval_ms as u64);
            sleep(Duration::from_millis(sleep_ms)).await;
            let Some(_) = remaining_wait_budget_ms(deadline) else {
                break;
            };
            let probe = self
                .probe_semantic_target_before_deadline(
                    ctx,
                    GuiTargetRequest {
                        capture_mode: fallback_probe_capture_mode(
                            request.capture_mode,
                            attempts + 1,
                            request.app,
                        ),
                        ..request
                    },
                    deadline,
                )
                .await?;
            current_state = probe.capture_state.clone();
            last_grounded = probe.target.map(|resolved| GroundedGuiTarget {
                grounding_method: "grounding",
                resolved,
            });
            attempts += 1;
            budget_exhausted = probe.timed_out;
            if budget_exhausted {
                break;
            }
            let satisfied = match state {
                "appear" => last_grounded.is_some(),
                "disappear" => last_grounded.is_none(),
                _ => false,
            };
            consecutive_satisfied = if satisfied {
                consecutive_satisfied + 1
            } else {
                0
            };
            matched = consecutive_satisfied >= WAIT_CONFIRMATION_COUNT;
        }

        self.set_observe_state(&ctx.session_id, current_state.clone())
            .await;

        let image_url = if attach_image {
            Some(capture_image_url_for_state(&current_state).await?)
        } else {
            None
        };

        Ok(GuiTargetProbeResult {
            matched,
            attempts,
            grounded: last_grounded,
            state: current_state,
            image_url,
        })
    }
}

#[derive(Clone, Debug)]
struct ActionEvidence {
    image_url: Option<String>,
    state: ObserveState,
}

#[derive(Clone, Debug)]
struct TargetProbe {
    capture_state: ObserveState,
    target: Option<ResolvedTarget>,
    timed_out: bool,
}

#[derive(Clone, Debug)]
struct GuiTargetProbeResult {
    matched: bool,
    attempts: i64,
    grounded: Option<GroundedGuiTarget>,
    state: ObserveState,
    image_url: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum DragEndpoint<'a> {
    Target {
        target: &'a str,
        location_hint: Option<&'a str>,
        scope: Option<&'a str>,
    },
}

#[derive(Clone, Copy, Debug)]
enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug)]
struct ResolvedGuiScrollPlan {
    amount: i64,
    distance_preset: &'static str,
    unit: &'static str,
    viewport_dimension: Option<i64>,
    viewport_source: Option<&'static str>,
    travel_fraction: Option<f64>,
}

async fn prepare_gui_observe_request(
    ctx: &GuiHandlerContext,
    targeted: bool,
    return_image: Option<bool>,
) -> Result<bool, ToolError> {
    enforce_gui_tool_capability(ctx.image_input_supported(), "gui_observe", targeted).await?;
    Ok(return_image.unwrap_or(true) && ctx.image_input_supported())
}

fn normalize_wait_target_state(state: Option<&str>) -> Result<&'static str, ToolError> {
    match state.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok("appear"),
        Some("appear") => Ok("appear"),
        Some("disappear") => Ok("disappear"),
        Some(other) => Err(respond_to_model(format!(
            "gui_wait.state only supports `appear` or `disappear`, got `{other}`"
        ))),
    }
}

fn remaining_wait_budget_ms(deadline: Instant) -> Option<u64> {
    remaining_wait_budget_duration(deadline).map(|duration| duration.as_millis() as u64)
}

fn remaining_wait_budget_duration(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    if now >= deadline {
        None
    } else {
        Some(deadline.duration_since(now))
    }
}

fn normalize_scroll_direction(direction: Option<&str>) -> Result<ScrollDirection, ToolError> {
    match direction
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("down")
    {
        "up" => Ok(ScrollDirection::Up),
        "down" => Ok(ScrollDirection::Down),
        "left" => Ok(ScrollDirection::Left),
        "right" => Ok(ScrollDirection::Right),
        other => Err(respond_to_model(format!(
            "gui_scroll.direction only supports `up`, `down`, `left`, or `right`, got `{other}`"
        ))),
    }
}

fn normalize_scroll_distance(distance: Option<&str>) -> Result<Option<&'static str>, ToolError> {
    match distance.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some("small") => Ok(Some("small")),
        Some("medium") => Ok(Some("medium")),
        Some("page") => Ok(Some("page")),
        Some(other) => Err(respond_to_model(format!(
            "gui_scroll.distance only supports `small`, `medium`, or `page`, got `{other}`"
        ))),
    }
}

fn scroll_direction_uses_horizontal_axis(direction: ScrollDirection) -> bool {
    matches!(direction, ScrollDirection::Left | ScrollDirection::Right)
}

fn scroll_viewport_dimension_for_direction(rect: &HelperRect, direction: ScrollDirection) -> i64 {
    if scroll_direction_uses_horizontal_axis(direction) {
        rect.width.round().max(1.0) as i64
    } else {
        rect.height.round().max(1.0) as i64
    }
}

fn scroll_distance_fraction(distance: &str) -> f64 {
    match distance {
        "small" => 0.25,
        "medium" => 0.5,
        "page" => 0.75,
        _ => 0.5,
    }
}

fn scroll_distance_line_amount(distance: &str) -> i64 {
    match distance {
        "small" => 3,
        "medium" => 5,
        "page" => 12,
        _ => 5,
    }
}

fn resolve_scroll_plan(
    amount: Option<i64>,
    distance: Option<&'static str>,
    has_target: bool,
    direction: ScrollDirection,
    target_bounds: Option<&HelperRect>,
    capture_bounds: Option<&HelperRect>,
) -> ResolvedGuiScrollPlan {
    if let Some(amount) = amount {
        return ResolvedGuiScrollPlan {
            amount: amount.clamp(1, 50),
            distance_preset: "custom",
            unit: "line",
            viewport_dimension: None,
            viewport_source: None,
            travel_fraction: None,
        };
    }

    let distance_preset = distance.unwrap_or(if has_target {
        DEFAULT_TARGETED_SCROLL_DISTANCE
    } else {
        DEFAULT_TARGETLESS_SCROLL_DISTANCE
    });

    if let Some(bounds) = target_bounds {
        let viewport_dimension = scroll_viewport_dimension_for_direction(bounds, direction);
        let travel_fraction = scroll_distance_fraction(distance_preset);
        return ResolvedGuiScrollPlan {
            amount: (viewport_dimension as f64 * travel_fraction)
                .round()
                .clamp(1.0, 4000.0) as i64,
            distance_preset,
            unit: "pixel",
            viewport_dimension: Some(viewport_dimension),
            viewport_source: Some("target_box"),
            travel_fraction: Some(travel_fraction),
        };
    }

    if let Some(bounds) = capture_bounds {
        let viewport_dimension = scroll_viewport_dimension_for_direction(bounds, direction);
        let travel_fraction = scroll_distance_fraction(distance_preset);
        return ResolvedGuiScrollPlan {
            amount: (viewport_dimension as f64 * travel_fraction)
                .round()
                .clamp(1.0, 4000.0) as i64,
            distance_preset,
            unit: "pixel",
            viewport_dimension: Some(viewport_dimension),
            viewport_source: Some("capture_rect"),
            travel_fraction: Some(travel_fraction),
        };
    }

    ResolvedGuiScrollPlan {
        amount: scroll_distance_line_amount(distance_preset),
        distance_preset,
        unit: "line",
        viewport_dimension: None,
        viewport_source: None,
        travel_fraction: None,
    }
}

fn scroll_delta_components(direction: ScrollDirection, amount: i64) -> (i64, i64) {
    match direction {
        ScrollDirection::Up => (0, amount),
        ScrollDirection::Down => (0, -amount),
        ScrollDirection::Left => (-amount, 0),
        ScrollDirection::Right => (amount, 0),
    }
}

fn targeted_type_focus_point(resolved: &ResolvedTarget) -> HelperPoint {
    let bounds = &resolved.bounds;
    if bounds.width.is_finite()
        && bounds.height.is_finite()
        && bounds.width > 0.0
        && bounds.height > 0.0
    {
        return HelperPoint {
            x: bounds.x + (bounds.width / 2.0),
            y: bounds.y + (bounds.height / 2.0),
        };
    }

    resolved.point
}

fn scroll_direction_label(direction: ScrollDirection) -> &'static str {
    match direction {
        ScrollDirection::Up => "up",
        ScrollDirection::Down => "down",
        ScrollDirection::Left => "left",
        ScrollDirection::Right => "right",
    }
}

fn normalize_drag_endpoint<'a>(
    endpoint_label: &str,
    target_field: &str,
    target: Option<&'a str>,
    location_hint: Option<&'a str>,
    scope: Option<&'a str>,
) -> Result<DragEndpoint<'a>, ToolError> {
    let Some(target) = target else {
        return Err(respond_to_model(format!(
            "gui_drag requires `{target_field}` for the {endpoint_label}."
        )));
    };
    Ok(DragEndpoint::Target {
        target,
        location_hint,
        scope,
    })
}

fn build_capture_details_from_state(state: &ObserveState) -> JsonValue {
    serde_json::json!({
        "capture_mode": state.capture.capture_mode,
        "origin_x": state.capture.origin_x,
        "origin_y": state.capture.origin_y,
        "width": state.capture.width,
        "height": state.capture.height,
        "image_width": state.capture.image_width,
        "image_height": state.capture.image_height,
        "capture_scale_x": state.capture.scale_x(),
        "capture_scale_y": state.capture.scale_y(),
        "app": state.app_name,
        "window_title": state.capture.window_title,
        "window_count": state.capture.window_count,
        "window_capture_strategy": state.capture.window_capture_strategy,
        "capture_host_self_exclude_applied": state.capture.host_exclusion.applied,
        "capture_host_frontmost_excluded": state.capture.host_exclusion.frontmost_excluded,
        "capture_host_self_exclude_adjusted": state.capture.host_exclusion.adjusted,
        "capture_host_frontmost_app": state.capture.host_exclusion.frontmost_app_name,
        "capture_host_frontmost_bundle_id": state.capture.host_exclusion.frontmost_bundle_id,
        "capture_host_self_exclude_redaction_count": state.capture.host_exclusion.redaction_count,
    })
}

fn point_json(x: f64, y: f64) -> JsonValue {
    serde_json::json!({
        "x": x,
        "y": y,
    })
}

fn extend_object_fields(target: &mut serde_json::Map<String, JsonValue>, value: JsonValue) {
    if let JsonValue::Object(fields) = value {
        for (key, value) in fields {
            target.insert(key, value);
        }
    }
}

async fn capture_image_url_for_state(state: &ObserveState) -> Result<String, ToolError> {
    let window_selection = if state.capture.capture_mode == CaptureMode::Window {
        state
            .capture
            .window_title
            .as_ref()
            .map(|title| WindowSelector {
                title: Some(title.clone()),
                title_contains: None,
                index: None,
            })
    } else {
        None
    };
    let observation = observe_platform(
        state.app_name.as_deref(),
        false,
        Some(state.capture.capture_mode.as_str()),
        window_selection.as_ref(),
        false,
    )
    .await?;
    Ok(screenshot_data_url(&observation.image_bytes))
}

fn rounded_dimension(value: f64, label: &str) -> Result<u32, ToolError> {
    let rounded = value.round();
    if !rounded.is_finite() || rounded <= 0.0 || rounded > u32::MAX as f64 {
        return Err(respond_to_model(format!(
            "invalid {label} from native GUI runtime: {value}"
        )));
    }
    Ok(rounded as u32)
}

#[derive(Clone, Debug)]
struct CaptureTarget {
    mode: CaptureMode,
    bounds: HelperRect,
    width: u32,
    height: u32,
    host_self_exclude_adjusted: bool,
    window_title: Option<String>,
    window_count: Option<i64>,
    window_capture_strategy: Option<String>,
}

fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_window_selection(
    window_title: Option<&str>,
    selector: Option<&WindowSelector>,
) -> Result<Option<WindowSelector>, ToolError> {
    let title = normalize_optional_string(window_title).or_else(|| {
        selector
            .and_then(|selector| selector.title.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });
    let title_contains = selector
        .and_then(|selector| selector.title_contains.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let index = selector.and_then(|selector| selector.index);
    if let Some(index) = index
        && index <= 0
    {
        return Err(respond_to_model(
            "gui window_selector.index must be a positive integer".to_string(),
        ));
    }
    if title.is_none() && title_contains.is_none() && index.is_none() {
        return Ok(None);
    }
    Ok(Some(WindowSelector {
        title,
        title_contains,
        index,
    }))
}

fn normalize_capture_mode(capture_mode: Option<&str>) -> Result<Option<CaptureMode>, ToolError> {
    match capture_mode
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => Ok(None),
        Some("display") => Ok(Some(CaptureMode::Display)),
        Some("window") => Ok(Some(CaptureMode::Window)),
        Some(other) => Err(respond_to_model(format!(
            "gui capture_mode only supports `display` or `window`, got `{other}`"
        ))),
    }
}

fn fallback_probe_capture_mode<'a>(
    requested_capture_mode: Option<&'a str>,
    attempt: i64,
    app: Option<&str>,
) -> Option<&'a str> {
    if requested_capture_mode.is_some() {
        return requested_capture_mode;
    }
    if attempt > 1 && app.is_some() {
        Some("display")
    } else {
        None
    }
}

fn resolve_capture_target(
    context: &HelperCaptureContext,
    capture_mode: Option<&str>,
    window_selection_requested: bool,
    prefer_window_when_available: bool,
) -> Result<CaptureTarget, ToolError> {
    let requested_mode = normalize_capture_mode(capture_mode)?;
    if window_selection_requested && context.window_bounds.is_none() {
        return Err(respond_to_model(
            "requested window could not be found; check `window_title`/`window_selector` or switch to `capture_mode: \"display\"`"
                .to_string(),
        ));
    }

    let host_self_exclude_adjusted = requested_mode.is_none()
        && !window_selection_requested
        && !prefer_window_when_available
        && context.host_self_exclude_applied.unwrap_or(false)
        && context.host_frontmost_excluded.unwrap_or(false)
        && context.window_bounds.is_some();

    let use_window = match requested_mode {
        Some(CaptureMode::Window) => context.window_bounds.is_some(),
        Some(CaptureMode::Display) => false,
        None => {
            window_selection_requested
                || host_self_exclude_adjusted
                || (prefer_window_when_available && context.window_bounds.is_some())
        }
    };

    let (mode, bounds) = if use_window {
        let Some(bounds) = context.window_bounds else {
            return Err(respond_to_model(
                "window capture requested but no matching window bounds were available".to_string(),
            ));
        };
        (CaptureMode::Window, bounds)
    } else {
        (CaptureMode::Display, context.display.bounds)
    };
    let width = rounded_dimension(bounds.width, "capture width")?;
    let height = rounded_dimension(bounds.height, "capture height")?;

    Ok(CaptureTarget {
        mode,
        bounds,
        width,
        height,
        host_self_exclude_adjusted,
        window_title: if mode == CaptureMode::Window {
            context.window_title.clone()
        } else {
            None
        },
        window_count: if mode == CaptureMode::Window {
            context.window_count
        } else {
            None
        },
        window_capture_strategy: if mode == CaptureMode::Window {
            context.window_capture_strategy.clone()
        } else {
            None
        },
    })
}

async fn capture_context(
    app: Option<&str>,
    activate_app: bool,
    window_selection: Option<&WindowSelector>,
) -> Result<HelperCaptureContext, ToolError> {
    let app = app.map(String::from);
    let window_selection = window_selection.cloned();
    tokio::task::spawn_blocking(move || {
        default_gui_platform().capture_context(
            app.as_deref(),
            activate_app,
            window_selection.as_ref(),
        )
    })
    .await
    .map_err(|e| respond_to_model(format!("gui platform task panicked: {e}")))?
}

async fn run_gui_event(
    event_mode: &str,
    app: Option<&str>,
    float_env: &[(&str, f64)],
    string_env: &[(&str, String)],
) -> Result<(), ToolError> {
    let event_mode = event_mode.to_string();
    let app = app.map(String::from);
    let float_env: Vec<(String, f64)> =
        float_env.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    let string_env: Vec<(String, String)> = string_env
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let float_refs: Vec<(&str, f64)> =
            float_env.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let string_refs: Vec<(&str, String)> = string_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        default_gui_platform().run_event(&event_mode, app.as_deref(), &float_refs, &string_refs)
    })
    .await
    .map_err(|e| respond_to_model(format!("gui platform task panicked: {e}")))?
}

async fn prepare_targeted_gui_action(
    app: Option<&str>,
    capture_mode: Option<&str>,
    window_selection: Option<&WindowSelector>,
) -> Result<(), ToolError> {
    if app.is_none() && capture_mode.is_none() && window_selection.is_none() {
        return Ok(());
    }

    let context = capture_context(app, true, window_selection).await?;
    if capture_mode.is_some() || window_selection.is_some() {
        let _ = resolve_capture_target(
            &context,
            capture_mode,
            window_selection.is_some(),
            app.is_some(),
        )?;
    }
    Ok(())
}

fn describe_capture_subject(state: &ObserveState) -> String {
    if state.capture.capture_mode == CaptureMode::Window {
        state
            .capture
            .window_title
            .clone()
            .unwrap_or_else(|| "current window".to_string())
    } else {
        format!("display {}", state.capture.display_index)
    }
}

fn describe_click_action(button: &str, clicks: i64, hold: bool) -> String {
    match (button, clicks, hold) {
        ("none", _, _) => "Hovered pointer".to_string(),
        ("left", 1, true) => "Click-and-held".to_string(),
        ("left", 2, _) => "Double-clicked".to_string(),
        ("right", 1, _) => "Right-clicked".to_string(),
        ("left", _, _) => "Clicked".to_string(),
        (other, _, _) => format!("Interacted with button `{other}`"),
    }
}

async fn capture_region(
    bounds: &HelperRect,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, ToolError> {
    let bounds = *bounds;
    tokio::task::spawn_blocking(move || {
        default_gui_platform().capture_region(&bounds, target_width, target_height)
    })
    .await
    .map_err(|e| respond_to_model(format!("gui platform task panicked: {e}")))?
}

async fn observe_platform(
    app: Option<&str>,
    activate_app: bool,
    capture_mode: Option<&str>,
    window_selection: Option<&WindowSelector>,
    prefer_window_when_available: bool,
) -> Result<PlatformObservation, ToolError> {
    let app = app.map(String::from);
    let capture_mode = capture_mode.map(String::from);
    let window_selection = window_selection.cloned();
    tokio::task::spawn_blocking(move || {
        default_gui_platform().observe(
            app.as_deref(),
            activate_app,
            capture_mode.as_deref(),
            window_selection.as_ref(),
            prefer_window_when_available,
        )
    })
    .await
    .map_err(|e| respond_to_model(format!("gui platform task panicked: {e}")))?
}

/// Convert raw PNG screenshot bytes to a JPEG data URL for the model.
/// JPEG at quality 75 is typically 3-5x smaller than PNG, significantly
/// reducing payload size and token consumption.  Falls back to PNG if
/// JPEG encoding fails (e.g. corrupt image data).
fn screenshot_data_url(png_bytes: &[u8]) -> String {
    if let Ok(img) = image::load_from_memory(png_bytes) {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, SCREENSHOT_JPEG_QUALITY);
        if encoder.encode_image(&img).is_ok() {
            return data_url(buf.get_ref(), "image/jpeg");
        }
    }
    // Fallback: serve the original PNG.
    data_url(png_bytes, "image/png")
}

fn resolve_type_value(args: &schemas::TypeArgs) -> Result<String, ToolError> {
    let literal_text = args.value.clone();
    let secret_env_var = normalize_optional_string(args.secret_env_var.as_deref());
    let secret_command_env_var = normalize_optional_string(args.secret_command_env_var.as_deref());
    let configured_source_count = [
        literal_text.is_some(),
        secret_env_var.is_some(),
        secret_command_env_var.is_some(),
    ]
    .into_iter()
    .filter(|configured| *configured)
    .count();
    if configured_source_count == 0 {
        return Err(respond_to_model(
            "gui_type requires a text source: provide exactly one of `value`, `secret_env_var`, or `secret_command_env_var`"
                .to_string(),
        ));
    }
    if configured_source_count > 1 {
        return Err(respond_to_model(
            "gui_type accepts only one text source: provide exactly one of `value`, `secret_env_var`, or `secret_command_env_var`"
                .to_string(),
        ));
    }
    if let Some(text) = literal_text {
        return Ok(text);
    }
    if let Some(secret_env_var) = secret_env_var {
        return std::env::var(&secret_env_var).map_err(|_| {
            respond_to_model(format!(
                "gui_type secret env var `{secret_env_var}` is missing or empty"
            ))
        });
    }
    let Some(secret_command_env_var) = secret_command_env_var else {
        return Err(respond_to_model(
            "gui_type input source could not be resolved".to_string(),
        ));
    };
    let command = std::env::var(&secret_command_env_var).map_err(|_| {
        respond_to_model(format!(
            "gui_type secret command env var `{secret_command_env_var}` is missing or empty"
        ))
    })?;
    let output = Command::new("/bin/sh")
        .args(["-c", &command])
        .output()
        .map_err(|error| {
            respond_to_model(format!(
                "failed to resolve gui_type secret command `{secret_command_env_var}`: {error}"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(respond_to_model(format!(
            "gui_type secret command `{secret_command_env_var}` failed: {}",
            stderr.trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(&['\r', '\n'][..])
        .to_string();
    if text.is_empty() {
        return Err(respond_to_model(format!(
            "gui_type secret command `{secret_command_env_var}` produced empty output"
        )));
    }
    Ok(text)
}

async fn run_system_events_type(
    app: Option<&str>,
    window_selection: Option<&WindowSelector>,
    text: &str,
    replace: bool,
    submit: bool,
    strategy: &str,
) -> Result<(), ToolError> {
    let app = app.map(String::from);
    let window_selection = window_selection.cloned();
    let text = text.to_string();
    let strategy = strategy.to_string();
    tokio::task::spawn_blocking(move || {
        default_gui_platform().run_system_events_type(
            app.as_deref(),
            window_selection.as_ref(),
            &text,
            replace,
            submit,
            &strategy,
        )
    })
    .await
    .map_err(|e| respond_to_model(format!("gui platform task panicked: {e}")))?
}

fn resolve_key_code(key: &str, modifiers: &mut Vec<String>) -> Result<i64, ToolError> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(respond_to_model(
            "gui_key.key must not be empty".to_string(),
        ));
    }

    let normalized = trimmed.to_lowercase();
    let named = match normalized.as_str() {
        "enter" | "return" => Some(36),
        "tab" => Some(48),
        "escape" | "esc" => Some(53),
        "delete" | "backspace" => Some(51),
        "home" => Some(115),
        "pageup" => Some(116),
        "pagedown" => Some(121),
        "end" => Some(119),
        "up" | "arrowup" => Some(126),
        "down" | "arrowdown" => Some(125),
        "left" | "arrowleft" => Some(123),
        "right" | "arrowright" => Some(124),
        "space" | "spacebar" => Some(49),
        _ => None,
    };
    if let Some(code) = named {
        return Ok(code);
    }

    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return Err(respond_to_model(
            "gui_key.key must not be empty".to_string(),
        ));
    };
    if chars.next().is_some() {
        return Err(respond_to_model(format!(
            "unsupported gui_key.key `{trimmed}`; use a named key like `Enter` or a single printable character"
        )));
    }

    let (code, needs_shift) = match first {
        'a' | 'A' => (0, first.is_uppercase()),
        's' | 'S' => (1, first.is_uppercase()),
        'd' | 'D' => (2, first.is_uppercase()),
        'f' | 'F' => (3, first.is_uppercase()),
        'h' | 'H' => (4, first.is_uppercase()),
        'g' | 'G' => (5, first.is_uppercase()),
        'z' | 'Z' => (6, first.is_uppercase()),
        'x' | 'X' => (7, first.is_uppercase()),
        'c' | 'C' => (8, first.is_uppercase()),
        'v' | 'V' => (9, first.is_uppercase()),
        'b' | 'B' => (11, first.is_uppercase()),
        'q' | 'Q' => (12, first.is_uppercase()),
        'w' | 'W' => (13, first.is_uppercase()),
        'e' | 'E' => (14, first.is_uppercase()),
        'r' | 'R' => (15, first.is_uppercase()),
        'y' | 'Y' => (16, first.is_uppercase()),
        't' | 'T' => (17, first.is_uppercase()),
        '1' => (18, false),
        '2' => (19, false),
        '3' => (20, false),
        '4' => (21, false),
        '6' => (22, false),
        '5' => (23, false),
        '=' => (24, false),
        '9' => (25, false),
        '7' => (26, false),
        '-' => (27, false),
        '8' => (28, false),
        '0' => (29, false),
        ']' => (30, false),
        'o' | 'O' => (31, first.is_uppercase()),
        'u' | 'U' => (32, first.is_uppercase()),
        '[' => (33, false),
        'i' | 'I' => (34, first.is_uppercase()),
        'p' | 'P' => (35, first.is_uppercase()),
        'l' | 'L' => (37, first.is_uppercase()),
        'j' | 'J' => (38, first.is_uppercase()),
        '\'' => (39, false),
        'k' | 'K' => (40, first.is_uppercase()),
        ';' => (41, false),
        '\\' => (42, false),
        ',' => (43, false),
        '/' => (44, false),
        'n' | 'N' => (45, first.is_uppercase()),
        'm' | 'M' => (46, first.is_uppercase()),
        '.' => (47, false),
        ' ' => (49, false),
        '!' => (18, true),
        '@' => (19, true),
        '#' => (20, true),
        '$' => (21, true),
        '^' => (22, true),
        '%' => (23, true),
        '+' => (24, true),
        '(' => (25, true),
        '&' => (26, true),
        '_' => (27, true),
        '*' => (28, true),
        ')' => (29, true),
        '}' => (30, true),
        '{' => (33, true),
        '"' => (39, true),
        ':' => (41, true),
        '|' => (42, true),
        '<' => (43, true),
        '?' => (44, true),
        '>' => (47, true),
        _ => {
            return Err(respond_to_model(format!(
                "unsupported gui_key.key `{trimmed}`"
            )));
        }
    };

    if needs_shift
        && !modifiers
            .iter()
            .any(|modifier| modifier.eq_ignore_ascii_case("shift"))
    {
        modifiers.push("shift".to_string());
    }

    Ok(code)
}

#[cfg(test)]
#[path = "handler_tests.rs"]
mod handler_tests;
