//! The nine model-facing GUI tool structs — the grok-build adaptation of
//! upstream's single multiplexing `GuiHandler` registration
//! (PR understudy-ai/codex-cua#3, `tool_registry_plan.rs` + `handlers/gui.rs`).
//!
//! Every struct is a thin shell: it declares the upstream wire name, the
//! verbatim upstream description, the hand-built JSON schema (via the
//! [`ToolMetadata::versioned_definition`] override — the `schemars`-derived
//! schema would drop the rich per-field guidance), and delegates `run` to the
//! shared engine ([`super::handler::shared_gui_engine`]) with its tool name,
//! preserving the upstream dispatch semantics.
//!
//! Registration, gating (`gui_tools` feature, `gui_batch` on/off), and the
//! grounding-sampler injection live with the integration slices; the tools
//! themselves are inert until registered.

use serde_json::Value;

use xai_tool_protocol::ToolCapabilities;
use xai_tool_protocol::ToolId;
use xai_tool_protocol::ToolScope;
use xai_tool_runtime::ListToolsContext;
use xai_tool_runtime::ToolCallContext;
use xai_tool_runtime::ToolError;
use xai_tool_types::ToolDescription;

use super::handler::GuiHandlerContext;
use super::handler::GuiToolOutput;
use super::handler::shared_gui_engine;
use super::schemas::GuiToolSchemaOptions;
use crate::types::definition::ToolDefinition;
use crate::types::template_renderer::TemplateRenderer;
use crate::types::tool::ToolKind;
use crate::types::tool::ToolNamespace;

/// Macro generating one thin GUI tool struct: `ToolMetadata` (kind `Gui`,
/// GrokBuild namespace, verbatim description, hand-built schema override) +
/// `xai_tool_runtime::Tool` (Args from the shared schemas, output
/// [`GuiToolOutput`], `run` delegating into the engine's `$method`).
macro_rules! gui_tool {
    (
        $tool:ident,
        $wire_name:literal,
        $method:ident,
        $args:ident,
        $schema_builder:path,
        $description:expr,
        $read_only:expr
    ) => {
        #[derive(Debug, Default)]
        pub struct $tool;

        impl crate::types::tool_metadata::ToolMetadata for $tool {
            fn kind(&self) -> ToolKind {
                ToolKind::Gui
            }

            fn tool_namespace(&self) -> ToolNamespace {
                ToolNamespace::GrokBuild
            }

            fn description_template(&self) -> &str {
                $description
            }

            // The `Gui` kind-class default is mutating, which is right for
            // the family; observe/wait are pure captures and must agree with
            // `Tool::capabilities().is_read_only`.
            fn is_read_only(&self) -> bool {
                $read_only
            }

            fn versioned_definition(
                &self,
                _contract_version: Option<&str>,
                client_name: &str,
                description_override: Option<&str>,
                _renderer: &TemplateRenderer,
                param_map: &std::collections::HashMap<String, String>,
                _input_schema: &Value,
                effective_params: &Value,
            ) -> ToolDefinition {
                // The hand-built schema is the feature: it carries the
                // upstream verbatim descriptions and toggles the
                // experimental direct-coordinate fields on
                // `coordinate_targeting` (upstream `GuiToolSchemaOptions`).
                let options = GuiToolSchemaOptions {
                    coordinate_targeting: effective_params
                        .get("coordinate_targeting")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                let mut definition = $schema_builder(options);
                if let Some(description_override) = description_override {
                    definition.function.description = Some(description_override.to_string());
                }
                if !param_map.is_empty() {
                    definition.function.parameters = crate::util::remap::remap_schema_properties(
                        &definition.function.parameters,
                        param_map,
                    );
                }
                definition.function.name = client_name.to_string();
                definition
            }
        }

        impl xai_tool_runtime::Tool for $tool {
            type Args = super::schemas::$args;
            type Output = GuiToolOutput;

            fn id(&self) -> ToolId {
                ToolId::new($wire_name).expect("valid tool id")
            }

            fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
                ToolDescription::new(
                    $wire_name,
                    crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
                )
            }

            fn capabilities(&self) -> ToolCapabilities {
                // Upstream registered all nine tools with
                // `supports_parallel_tool_calls = false`: GUI actions mutate
                // one physical machine, so they never run concurrently.
                ToolCapabilities {
                    max_concurrency: Some(1),
                    is_read_only: $read_only,
                    tool_scope: Some(if $read_only {
                        ToolScope::Read
                    } else {
                        ToolScope::Write
                    }),
                    ..Default::default()
                }
            }

            async fn run(
                &self,
                ctx: ToolCallContext,
                args: Self::Args,
            ) -> Result<Self::Output, ToolError> {
                let handler_context = GuiHandlerContext::from_tool_context(&ctx).await;
                // Defense-in-depth (upstream only registered the handler when
                // `config.gui_tools` was set): refuse dispatch when the
                // session carries no explicit GUI-tools enable marker.
                handler_context.ensure_enabled()?;
                shared_gui_engine().$method(&handler_context, args).await
            }
        }
    };
}

gui_tool!(
    GuiObserveTool,
    "gui_observe",
    handle_observe,
    ObserveArgs,
    super::schemas::create_gui_observe_tool_with_options,
    "Capture a screenshot of the current native GUI surface for visual inspection and follow-up GUI grounding. Supports display-wide capture and focused-window capture, and can also resolve a semantic `target` within the observed GUI.",
    true
);

gui_tool!(
    GuiClickTool,
    "gui_click",
    handle_click,
    ClickArgs,
    super::schemas::create_gui_click_tool_with_options,
    "Click, right-click, double-click, hover, or click-and-hold on a semantic target in the current native GUI.",
    false
);

gui_tool!(
    GuiDragTool,
    "gui_drag",
    handle_drag,
    DragArgs,
    super::schemas::create_gui_drag_tool_with_options,
    "Drag between semantic `from_target` and `to_target` points in the current native GUI.",
    false
);

gui_tool!(
    GuiScrollTool,
    "gui_scroll",
    handle_scroll,
    ScrollArgs,
    super::schemas::create_gui_scroll_tool_with_options,
    "Scroll in the current native GUI. Defaults to a targetless scroll on the current surface, or provide `target` to scroll a semantic region. Prefer `direction` with semantic `distance`; use `amount` only when you need an explicit legacy line count.",
    false
);

gui_tool!(
    GuiTypeTool,
    "gui_type",
    handle_type,
    TypeArgs,
    super::schemas::create_gui_type_tool_with_options,
    "Type text into the currently focused native GUI control. Typically use gui_click first to focus the desired field, or provide `target` so the tool focuses the semantic input target for you. Provide exactly one of `value`, `secret_env_var`, or `secret_command_env_var`.",
    false
);

gui_tool!(
    GuiKeyTool,
    "gui_key",
    handle_key,
    KeyArgs,
    super::schemas::create_gui_key_tool_with_options,
    "Press a key or hotkey in the current native GUI.",
    false
);

gui_tool!(
    GuiMoveTool,
    "gui_move",
    handle_move,
    MoveArgs,
    super::schemas::create_gui_move_tool_with_options,
    "Move the pointer to an absolute display coordinate in logical points.",
    false
);

gui_tool!(
    GuiWaitTool,
    "gui_wait",
    handle_wait,
    WaitArgs,
    super::schemas::create_gui_wait_tool_with_options,
    "Repeatedly refresh the current native GUI screenshot until a semantic target appears or disappears. Uses consecutive confirmations for stability and reuses the previous gui_observe capture selection when no explicit capture selection is provided.",
    true
);

gui_tool!(
    GuiBatchTool,
    "gui_batch",
    handle_batch,
    BatchArgs,
    super::schemas::create_gui_batch_tool_with_options,
    "Execute a batch of independent GUI actions in a single call for faster task completion. Takes one screenshot, resolves all semantic targets at once, and executes each step sequentially. Use this when consecutive actions do not depend on each other's visual effects. For dependent actions, use individual gui_* tools with gui_wait or gui_observe between them.",
    false
);
