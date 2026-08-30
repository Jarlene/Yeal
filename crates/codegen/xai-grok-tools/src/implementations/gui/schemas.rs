//! JSON schemas and typed parameters for the native GUI tools (macOS,
//! Linux/X11, Windows; `gui_observe`, `gui_click`, `gui_drag`, `gui_scroll`,
//! `gui_type`, `gui_key`, `gui_move`, `gui_wait`, `gui_batch`).
//!
//! Ported from upstream codex PR understudy-ai/codex-cua#3
//! (`codex-rs/tools/src/gui_tool.rs`). Schemas are hand-built
//! [`serde_json::Value`] trees in standard JSON Schema shape so they can be
//! consumed by [`ToolDefinition::function`]. The hand-built descriptions must
//! be used for the model-facing definitions (e.g. via a
//! `ToolMetadata::versioned_definition` override); the `schemars`-derived
//! schema on the arg structs below is only a compatibility fallback and drops
//! the rich per-field guidance.
//!
//! Upstream wrapped `JsonSchema` trees in
//! `ToolSpec::Function(ResponsesApiTool { strict: false, .. })`; the grok
//! `ToolDefinition` equivalent has no `strict`/`output_schema`/`defer_loading`
//! fields, so those upstream settings are implicit here. Enum-like parameters
//! (`button`, `direction`, `distance`, `coordinate_space`, ...) are plain
//! strings in both the schema and the typed args, matching upstream — value
//! validation lives in the engine, not the schema.

use serde::Deserialize;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::types::definition::ToolDefinition;

/// Options controlling the shape of the GUI tool schemas.
///
/// `coordinate_targeting` toggles the experimental direct-coordinate fields
/// (`x`/`y`/`coordinate_space` on click, `from_x`/`from_y`/`to_x`/`to_y`/
/// `coordinate_space` on drag) and relaxes the `required` list accordingly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuiToolSchemaOptions {
    pub coordinate_targeting: bool,
}

// ───────────────────────────────────────────────────────────────────────────
// JSON Schema node builders (equivalents of the upstream codex JsonSchema
// variants; `tag = "type"` + lowercase + camelCase serialization shape).
// ───────────────────────────────────────────────────────────────────────────

fn string_schema(description: impl Into<String>) -> Value {
    serde_json::json!({ "type": "string", "description": description.into() })
}

fn number_schema(description: impl Into<String>) -> Value {
    serde_json::json!({ "type": "number", "description": description.into() })
}

fn boolean_schema(description: impl Into<String>) -> Value {
    serde_json::json!({ "type": "boolean", "description": description.into() })
}

fn array_schema(items: Value, description: impl Into<String>) -> Value {
    serde_json::json!({ "type": "array", "items": items, "description": description.into() })
}

/// Object schema node. Upstream sets `additionalProperties: false` on every
/// object schema in this file and omits `required` entirely when absent.
fn object_schema(properties: Map<String, Value>, required: Option<Vec<String>>) -> Value {
    let mut object = Map::new();
    object.insert("type".to_string(), Value::String("object".to_string()));
    object.insert("properties".to_string(), Value::Object(properties));
    if let Some(required) = required {
        object.insert(
            "required".to_string(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    object.insert("additionalProperties".to_string(), Value::Bool(false));
    Value::Object(object)
}

fn property_map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// Shared description / property builders (ported verbatim from upstream).
// ───────────────────────────────────────────────────────────────────────────

fn string_enum_description(values: &[&str], extra: &str) -> String {
    format!("Supported values: {}. {extra}", values.join(", "))
}

fn coordinate_space_description() -> String {
    string_enum_description(
        &["image_pixels", "display_points"],
        "`image_pixels` means coordinates relative to the latest `gui_observe` screenshot for this conversation. `display_points` means absolute display coordinates in the platform's logical units (logical points on macOS, pixels on Linux/X11 and Windows).",
    )
}

fn window_selector_schema() -> Value {
    object_schema(
        property_map([
            (
                "title",
                string_schema(
                    "Optional exact visible window title. Use this when the current app has multiple similarly named windows and you want an exact match.",
                ),
            ),
            (
                "title_contains",
                string_schema(
                    "Optional visible substring of the target window title. Prefer this when only part of the window title is stable or visible.",
                ),
            ),
            (
                "index",
                number_schema(
                    "Optional 1-based index among matching visible windows after applying title filters. Use this as a last disambiguator when multiple windows still match.",
                ),
            ),
        ]),
        None,
    )
}

fn with_capture_selection_properties(
    mut properties: Map<String, Value>,
    include_window_title: bool,
) -> Map<String, Value> {
    properties.insert(
        "capture_mode".to_string(),
        string_schema(string_enum_description(
            &["display", "window"],
            "Use `window` for app-local work where a single window is the stable frame of reference. Use `display` for desktop-wide UI such as the Dock, menu bar, permission prompts, or cross-window drags. When omitted, GUI tools prefer `window` for in-app or window-targeted work and fall back to `display` otherwise.",
        )),
    );
    if include_window_title {
        properties.insert(
            "window_title".to_string(),
            string_schema(
                "Optional exact visible window title to capture or focus. Reuse the same `window_title` across related GUI steps when you want the runtime to stay on the same surface.",
            ),
        );
    }
    properties.insert("window_selector".to_string(), window_selector_schema());
    properties
}

fn with_target_properties(
    mut properties: Map<String, Value>,
    action_description: &str,
) -> Map<String, Value> {
    properties.insert(
        "target".to_string(),
        string_schema(format!(
            "Optional semantic GUI target to resolve before {action_description}. Describe the actionable or editable control itself using visible screenshot evidence: prefer the exact on-screen text, icon, state, nearby context, and coarse location that make it unique, such as `Save button`, `Search field`, or `Muted Notifications toggle in the top-right toolbar`."
        )),
    );
    properties.insert(
        "location_hint".to_string(),
        string_schema(
            "Optional coarse position hint such as `top right`, `left sidebar`, or `near the bottom`. Use this when multiple similar controls are visible.",
        ),
    );
    properties.insert(
        "scope".to_string(),
        string_schema(
            "Optional semantic region that narrows the search area, such as `left sidebar`, `toolbar`, or `composer pane`. Prefer naming the real surrounding region instead of generic container chrome.",
        ),
    );
    properties.insert(
        "grounding_mode".to_string(),
        string_schema(string_enum_description(
            &["single", "complex"],
            "Optional grounding hint. `single` suits simple isolated controls. `complex` enables the heavier validation and retry path for dense, ambiguous, or visually noisy layouts, and is a good retry mode after an initial miss.",
        )),
    );
    properties
}

fn with_drag_target_properties(mut properties: Map<String, Value>) -> Map<String, Value> {
    properties.insert(
        "grounding_mode".to_string(),
        string_schema(string_enum_description(
            &["single", "complex"],
            "Optional grounding hint shared by drag source and destination. `single` suits simple isolated controls. `complex` enables the heavier validation and retry path for dense or ambiguous layouts and is a good retry mode after an initial miss.",
        )),
    );
    properties.insert(
        "from_target".to_string(),
        string_schema(
            "Optional semantic GUI drag source to resolve before dragging. Describe the draggable surface itself using visible screenshot evidence, such as `Selected tab`, `Message row`, or `Resize handle near the lower-right corner`.",
        ),
    );
    properties.insert(
        "from_location_hint".to_string(),
        string_schema(
            "Optional coarse position hint for `from_target`, such as `left sidebar` or `near the top`.",
        ),
    );
    properties.insert(
        "from_scope".to_string(),
        string_schema(
            "Optional semantic region that narrows `from_target`, such as `left sidebar` or `active tab strip`.",
        ),
    );
    properties.insert(
        "to_target".to_string(),
        string_schema(
            "Optional semantic GUI drag destination to resolve before dragging. Describe the drop surface itself using visible screenshot evidence, such as `Trash`, `Calendar column`, or `right pane drop zone`.",
        ),
    );
    properties.insert(
        "to_location_hint".to_string(),
        string_schema(
            "Optional coarse position hint for `to_target`, such as `bottom right` or `in the center`.",
        ),
    );
    properties.insert(
        "to_scope".to_string(),
        string_schema(
            "Optional semantic region that narrows `to_target`, such as `timeline`, `calendar grid`, or `drop zone`.",
        ),
    );
    properties
}

fn with_coordinate_click_properties(mut properties: Map<String, Value>) -> Map<String, Value> {
    properties.insert(
        "x".to_string(),
        number_schema(
            "Direct click X coordinate. When `coordinate_space` is `image_pixels`, this is relative to the latest `gui_observe` screenshot. When `coordinate_space` is `display_points`, this is an absolute display X coordinate in the platform's logical units (logical points on macOS, pixels on Linux/X11 and Windows).",
        ),
    );
    properties.insert(
        "y".to_string(),
        number_schema(
            "Direct click Y coordinate. When `coordinate_space` is `image_pixels`, this is relative to the latest `gui_observe` screenshot. When `coordinate_space` is `display_points`, this is an absolute display Y coordinate in the platform's logical units (logical points on macOS, pixels on Linux/X11 and Windows).",
        ),
    );
    properties.insert(
        "coordinate_space".to_string(),
        string_schema(coordinate_space_description()),
    );
    properties
}

fn with_coordinate_drag_properties(mut properties: Map<String, Value>) -> Map<String, Value> {
    for (name, description) in [
        (
            "from_x",
            "Direct drag source X coordinate in the selected `coordinate_space`.",
        ),
        (
            "from_y",
            "Direct drag source Y coordinate in the selected `coordinate_space`.",
        ),
        (
            "to_x",
            "Direct drag destination X coordinate in the selected `coordinate_space`.",
        ),
        (
            "to_y",
            "Direct drag destination Y coordinate in the selected `coordinate_space`.",
        ),
    ] {
        properties.insert(name.to_string(), number_schema(description));
    }
    properties.insert(
        "coordinate_space".to_string(),
        string_schema(coordinate_space_description()),
    );
    properties
}

// ───────────────────────────────────────────────────────────────────────────
// Tool schema constructors (upstream `create_gui_*_tool[_with_options]`,
// returning grok `ToolDefinition`s instead of codex `ToolSpec`s).
// ───────────────────────────────────────────────────────────────────────────

pub fn create_gui_observe_tool() -> ToolDefinition {
    create_gui_observe_tool_with_options(GuiToolSchemaOptions::default())
}

pub fn create_gui_observe_tool_with_options(_options: GuiToolSchemaOptions) -> ToolDefinition {
    let properties = with_target_properties(
        with_capture_selection_properties(
            property_map([
                (
                    "app",
                    string_schema(
                        "Optional application name to activate before capturing. Defaults to the current frontmost app.",
                    ),
                ),
                (
                    "return_image",
                    boolean_schema(
                        "Whether to attach the captured screenshot image. Defaults to true.",
                    ),
                ),
            ]),
            true,
        ),
        "observing a semantic GUI target",
    );

    ToolDefinition::function(
        "gui_observe",
        Some(
            "Capture a screenshot of the current native GUI surface for visual inspection and follow-up GUI grounding. Supports display-wide capture and focused-window capture, and can also resolve a semantic `target` within the observed GUI.",
        ),
        object_schema(properties, None),
    )
}

pub fn create_gui_wait_tool() -> ToolDefinition {
    let properties = with_target_properties(
        with_capture_selection_properties(
            property_map([
                (
                    "state",
                    string_schema(string_enum_description(
                        &["appear", "disappear"],
                        "Defaults to `appear`.",
                    )),
                ),
                (
                    "timeout_ms",
                    number_schema(
                        "Maximum time to wait for `target` to satisfy `state`. Defaults to 8000.",
                    ),
                ),
                (
                    "interval_ms",
                    number_schema(
                        "Polling interval between semantic target checks. Defaults to 350.",
                    ),
                ),
                (
                    "app",
                    string_schema(
                        "Optional application name to activate before refreshing the screenshot. Defaults to the current frontmost app.",
                    ),
                ),
            ]),
            true,
        ),
        "waiting for a semantic GUI target",
    );

    ToolDefinition::function(
        "gui_wait",
        Some(
            "Repeatedly refresh the current native GUI screenshot until a semantic target appears or disappears. Uses consecutive confirmations for stability and reuses the previous gui_observe capture selection when no explicit capture selection is provided.",
        ),
        object_schema(properties, Some(vec!["target".to_string()])),
    )
}

pub fn create_gui_click_tool() -> ToolDefinition {
    create_gui_click_tool_with_options(GuiToolSchemaOptions::default())
}

pub fn create_gui_click_tool_with_options(options: GuiToolSchemaOptions) -> ToolDefinition {
    let base_properties = with_target_properties(
        with_capture_selection_properties(
            property_map([
                (
                    "button",
                    string_schema(string_enum_description(
                        &["left", "right", "none"],
                        "Use `none` for hover-only pointer movement. Defaults to `left`.",
                    )),
                ),
                (
                    "clicks",
                    number_schema(
                        "Number of clicks to send. Defaults to 1. Use 2 for a double-click.",
                    ),
                ),
                (
                    "hold_ms",
                    number_schema(
                        "Optional press-and-hold duration in milliseconds before releasing. Use this for long-press interactions.",
                    ),
                ),
                (
                    "settle_ms",
                    number_schema(
                        "Optional hover settle time in milliseconds when `button` is `none`. Defaults to 200.",
                    ),
                ),
                (
                    "app",
                    string_schema("Optional application name to activate before clicking."),
                ),
            ]),
            true,
        ),
        "clicking or hovering",
    );
    let properties = if options.coordinate_targeting {
        with_coordinate_click_properties(base_properties)
    } else {
        base_properties
    };
    let required = if options.coordinate_targeting {
        None
    } else {
        Some(vec!["target".to_string()])
    };
    let description = if options.coordinate_targeting {
        "Click, right-click, double-click, hover, or click-and-hold in the current native GUI. Use semantic `target` fields for the supported grounding workflow. The optional `x`, `y`, and `coordinate_space` fields are kept as an experimental direct-coordinate placeholder and are disabled by default."
    } else {
        "Click, right-click, double-click, hover, or click-and-hold on a semantic target in the current native GUI."
    };

    ToolDefinition::function(
        "gui_click",
        Some(description),
        object_schema(properties, required),
    )
}

pub fn create_gui_drag_tool() -> ToolDefinition {
    create_gui_drag_tool_with_options(GuiToolSchemaOptions::default())
}

pub fn create_gui_drag_tool_with_options(options: GuiToolSchemaOptions) -> ToolDefinition {
    let base_properties = with_drag_target_properties(with_capture_selection_properties(
        property_map([
            (
                "duration_ms",
                number_schema("Optional drag duration in milliseconds. Defaults to 450."),
            ),
            (
                "app",
                string_schema("Optional application name to activate before dragging."),
            ),
        ]),
        true,
    ));
    let properties = if options.coordinate_targeting {
        with_coordinate_drag_properties(base_properties)
    } else {
        base_properties
    };
    let required = if options.coordinate_targeting {
        None
    } else {
        Some(vec!["from_target".to_string(), "to_target".to_string()])
    };
    let description = if options.coordinate_targeting {
        "Drag in the current native GUI. Use semantic `from_target` and `to_target` for the supported grounding workflow. The optional `from_x`, `from_y`, `to_x`, `to_y`, and `coordinate_space` fields are kept as an experimental direct-coordinate placeholder and are disabled by default."
    } else {
        "Drag between semantic `from_target` and `to_target` points in the current native GUI."
    };

    ToolDefinition::function(
        "gui_drag",
        Some(description),
        object_schema(properties, required),
    )
}

pub fn create_gui_scroll_tool() -> ToolDefinition {
    let properties = with_target_properties(
        with_capture_selection_properties(
            property_map([
                (
                    "direction",
                    string_schema(string_enum_description(
                        &["up", "down", "left", "right"],
                        "Scroll direction. Defaults to `down`.",
                    )),
                ),
                (
                    "distance",
                    string_schema(string_enum_description(
                        &["small", "medium", "page"],
                        "Semantic scroll distance. Defaults to `page` for targetless scrolls and `medium` for grounded scrolls.",
                    )),
                ),
                (
                    "amount",
                    number_schema(
                        "Optional explicit legacy line-count override. When provided, it overrides `distance`.",
                    ),
                ),
                (
                    "app",
                    string_schema("Optional application name to activate before scrolling."),
                ),
            ]),
            true,
        ),
        "scrolling",
    );

    ToolDefinition::function(
        "gui_scroll",
        Some(
            "Scroll in the current native GUI. Defaults to a targetless scroll on the current surface, or provide `target` to scroll a semantic region. Prefer `direction` with semantic `distance`; use `amount` only when you need an explicit legacy line count.",
        ),
        object_schema(properties, None),
    )
}

pub fn create_gui_type_tool() -> ToolDefinition {
    let properties = with_target_properties(
        with_capture_selection_properties(
            property_map([
                (
                    "value",
                    string_schema("Literal text to type into the currently focused control."),
                ),
                (
                    "secret_env_var",
                    string_schema(
                        "Environment variable name whose value should be typed without exposing the literal secret in the tool call.",
                    ),
                ),
                (
                    "secret_command_env_var",
                    string_schema(
                        "Environment variable name containing a local shell command whose stdout should be typed without exposing the secret in the tool call.",
                    ),
                ),
                (
                    "replace",
                    boolean_schema(
                        "Whether to replace the current field contents before typing. Defaults to true.",
                    ),
                ),
                (
                    "submit",
                    boolean_schema("Whether to press Return after typing. Defaults to false."),
                ),
                (
                    "type_strategy",
                    string_schema(string_enum_description(
                        &[
                            "clipboard_paste",
                            "physical_keys",
                            "system_events_paste",
                            "system_events_keystroke",
                            "system_events_keystroke_chars",
                        ],
                        "When omitted, the runtime chooses the default native typing path.",
                    )),
                ),
                (
                    "app",
                    string_schema("Optional application name to activate before typing."),
                ),
            ]),
            true,
        ),
        "typing into a semantic input target",
    );

    ToolDefinition::function(
        "gui_type",
        Some(
            "Type text into the currently focused native GUI control. Typically use gui_click first to focus the desired field, or provide `target` so the tool focuses the semantic input target for you. Provide exactly one of `value`, `secret_env_var`, or `secret_command_env_var`.",
        ),
        object_schema(properties, None),
    )
}

pub fn create_gui_key_tool() -> ToolDefinition {
    let properties = with_capture_selection_properties(
        property_map([
            (
                "key",
                string_schema(
                    "Key to press, such as `Enter`, `Tab`, `Escape`, `ArrowDown`, or `s`.",
                ),
            ),
            (
                "modifiers",
                array_schema(
                    serde_json::json!({
                        "type": "string",
                        "description": "Modifier name such as `command`, `shift`, `option`, or `control`."
                    }),
                    "Optional modifier list.",
                ),
            ),
            (
                "repeat",
                number_schema("How many times to press the key. Defaults to 1."),
            ),
            (
                "app",
                string_schema("Optional application name to activate before pressing the key."),
            ),
        ]),
        true,
    );

    ToolDefinition::function(
        "gui_key",
        Some("Press a key or hotkey in the current native GUI."),
        object_schema(properties, Some(vec!["key".to_string()])),
    )
}

pub fn create_gui_batch_tool() -> ToolDefinition {
    let step_properties = property_map([
        (
            "action",
            string_schema(string_enum_description(
                &["click", "type", "key", "scroll", "drag"],
                "The GUI action to perform in this step.",
            )),
        ),
        // Semantic targeting (click, type, scroll)
        (
            "target",
            string_schema(
                "Semantic GUI target for this step. Required for `click`. Optional for `type` (to focus a field first) and `scroll` (to target a scrollable region). Describe the control using visible screenshot evidence.",
            ),
        ),
        (
            "location_hint",
            string_schema("Optional coarse position hint such as `top right` or `left sidebar`."),
        ),
        (
            "scope",
            string_schema(
                "Optional semantic region that narrows the search area, such as `toolbar` or `dialog`.",
            ),
        ),
        // Click-specific
        (
            "button",
            string_schema(string_enum_description(
                &["left", "right", "none"],
                "For `click` action. Defaults to `left`.",
            )),
        ),
        (
            "clicks",
            number_schema(
                "For `click` action. Number of clicks. Defaults to 1. Use 2 for double-click.",
            ),
        ),
        (
            "hold_ms",
            number_schema("For `click` action. Press-and-hold duration in milliseconds."),
        ),
        (
            "settle_ms",
            number_schema(
                "For `click` action with `button: none`. Hover settle time in ms. Defaults to 200.",
            ),
        ),
        // Type-specific
        (
            "value",
            string_schema("For `type` action. The text to type into the focused control."),
        ),
        (
            "replace",
            boolean_schema(
                "For `type` action. If true (default), selects all existing text before typing.",
            ),
        ),
        (
            "submit",
            boolean_schema(
                "For `type` action. If true, presses Enter after typing. Defaults to false.",
            ),
        ),
        (
            "type_strategy",
            string_schema(string_enum_description(
                &[
                    "clipboard_paste",
                    "physical_keys",
                    "system_events_paste",
                    "system_events_keystroke",
                    "system_events_keystroke_chars",
                ],
                "For `type` action. Defaults to `clipboard_paste`.",
            )),
        ),
        // Key-specific
        (
            "key",
            string_schema(
                "For `key` action. Key name such as `Enter`, `Escape`, or a single character.",
            ),
        ),
        (
            "modifiers",
            array_schema(
                serde_json::json!({ "type": "string" }),
                "For `key` action. Modifier keys such as `command`, `shift`, `option`, `control`.",
            ),
        ),
        (
            "repeat",
            number_schema("For `key` action. Number of times to press the key. Defaults to 1."),
        ),
        // Scroll-specific
        (
            "direction",
            string_schema(string_enum_description(
                &["up", "down", "left", "right"],
                "For `scroll` action. Defaults to `down`.",
            )),
        ),
        (
            "distance",
            string_schema(string_enum_description(
                &["small", "medium", "page"],
                "For `scroll` action. Semantic scroll distance.",
            )),
        ),
        (
            "amount",
            number_schema("For `scroll` action. Explicit line-count override for `distance`."),
        ),
        // Drag-specific
        (
            "from_target",
            string_schema(
                "For `drag` action. Semantic drag source to resolve. Describe the draggable surface using visible screenshot evidence.",
            ),
        ),
        (
            "from_location_hint",
            string_schema("For `drag` action. Coarse position hint for the drag source."),
        ),
        (
            "from_scope",
            string_schema("For `drag` action. Semantic region for the drag source."),
        ),
        (
            "to_target",
            string_schema(
                "For `drag` action. Semantic drag destination to resolve. Describe the drop surface using visible screenshot evidence.",
            ),
        ),
        (
            "to_location_hint",
            string_schema("For `drag` action. Coarse position hint for the drag destination."),
        ),
        (
            "to_scope",
            string_schema("For `drag` action. Semantic region for the drag destination."),
        ),
        (
            "duration_ms",
            number_schema("For `drag` action. Drag duration in milliseconds. Defaults to 450."),
        ),
    ]);

    let step_schema = object_schema(step_properties, Some(vec!["action".to_string()]));

    let mut properties = with_capture_selection_properties(
        property_map([(
            "app",
            string_schema("Optional application name to activate before executing the batch."),
        )]),
        true,
    );
    properties.insert(
        "steps".to_string(),
        array_schema(
            step_schema,
            "Array of independent GUI actions to execute in order. All steps share a single screenshot for grounding and are executed sequentially without re-observing between them. Only include steps that do NOT depend on the visual effects of earlier steps.",
        ),
    );

    ToolDefinition::function(
        "gui_batch",
        Some(
            "Execute a batch of independent GUI actions in a single call for faster task completion. Takes one screenshot, resolves all semantic targets at once, and executes each step sequentially. Use this when consecutive actions do not depend on each other's visual effects. For dependent actions, use individual gui_* tools with gui_wait or gui_observe between them.",
        ),
        object_schema(properties, Some(vec!["steps".to_string()])),
    )
}

pub fn create_gui_move_tool() -> ToolDefinition {
    let properties = property_map([
        (
            "x",
            number_schema("Absolute display X coordinate in logical points."),
        ),
        (
            "y",
            number_schema("Absolute display Y coordinate in logical points."),
        ),
        (
            "app",
            string_schema("Optional application name to activate before moving the pointer."),
        ),
    ]);

    ToolDefinition::function(
        "gui_move",
        Some("Move the pointer to an absolute display coordinate in logical points."),
        object_schema(properties, Some(vec!["x".to_string(), "y".to_string()])),
    )
}

// Uniform `_with_options` constructors for the tools whose schemas carry no
// coordinate fields (upstream only ships `_with_options` for observe/click/
// drag; the flag is unused there, as in upstream's observe constructor).
// Keeping the same signature for all nine lets the tool structs share one
// schema-construction code path.

pub fn create_gui_wait_tool_with_options(_options: GuiToolSchemaOptions) -> ToolDefinition {
    create_gui_wait_tool()
}

pub fn create_gui_scroll_tool_with_options(_options: GuiToolSchemaOptions) -> ToolDefinition {
    create_gui_scroll_tool()
}

pub fn create_gui_type_tool_with_options(_options: GuiToolSchemaOptions) -> ToolDefinition {
    create_gui_type_tool()
}

pub fn create_gui_key_tool_with_options(_options: GuiToolSchemaOptions) -> ToolDefinition {
    create_gui_key_tool()
}

pub fn create_gui_batch_tool_with_options(_options: GuiToolSchemaOptions) -> ToolDefinition {
    create_gui_batch_tool()
}

pub fn create_gui_move_tool_with_options(_options: GuiToolSchemaOptions) -> ToolDefinition {
    create_gui_move_tool()
}

// ───────────────────────────────────────────────────────────────────────────
// Typed parameter structs (ported from upstream
// `codex-rs/core/src/tools/handlers/gui.rs`). Fields and types match the
// schemas above exactly; unknown fields are ignored (upstream behavior), and
// enum-like values stay `String` for the same reason as in the schemas.
// `schemars::JsonSchema` is derived so these can serve as
// `xai_tool_runtime::Tool::Args` (the model-facing schema should still come
// from the hand-built constructors above).
// ───────────────────────────────────────────────────────────────────────────

/// Window selection criteria shared by the capture-selection parameters.
/// Single definition shared with the engine/grounding slices (upstream
/// carried identical copies in `tools/gui_tool.rs` and `handlers/gui.rs`).
pub use super::grounding::WindowSelector;

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ObserveArgs {
    pub app: Option<String>,
    pub target: Option<String>,
    pub location_hint: Option<String>,
    pub scope: Option<String>,
    pub grounding_mode: Option<String>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
    pub return_image: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ClickArgs {
    pub target: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub coordinate_space: Option<String>,
    pub location_hint: Option<String>,
    pub scope: Option<String>,
    pub grounding_mode: Option<String>,
    pub button: Option<String>,
    pub clicks: Option<i64>,
    pub hold_ms: Option<i64>,
    pub settle_ms: Option<i64>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WaitArgs {
    pub target: String,
    pub location_hint: Option<String>,
    pub scope: Option<String>,
    pub grounding_mode: Option<String>,
    pub state: Option<String>,
    pub timeout_ms: Option<i64>,
    pub interval_ms: Option<i64>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DragArgs {
    pub from_target: Option<String>,
    pub from_x: Option<f64>,
    pub from_y: Option<f64>,
    pub from_location_hint: Option<String>,
    pub from_scope: Option<String>,
    pub to_target: Option<String>,
    pub to_x: Option<f64>,
    pub to_y: Option<f64>,
    pub to_location_hint: Option<String>,
    pub to_scope: Option<String>,
    pub coordinate_space: Option<String>,
    pub grounding_mode: Option<String>,
    pub duration_ms: Option<i64>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ScrollArgs {
    pub direction: Option<String>,
    pub distance: Option<String>,
    pub amount: Option<i64>,
    pub target: Option<String>,
    pub location_hint: Option<String>,
    pub scope: Option<String>,
    pub grounding_mode: Option<String>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TypeArgs {
    pub value: Option<String>,
    pub secret_env_var: Option<String>,
    pub secret_command_env_var: Option<String>,
    pub target: Option<String>,
    pub location_hint: Option<String>,
    pub scope: Option<String>,
    pub grounding_mode: Option<String>,
    pub replace: Option<bool>,
    pub submit: Option<bool>,
    pub type_strategy: Option<String>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct KeyArgs {
    pub key: String,
    pub modifiers: Option<Vec<String>>,
    pub repeat: Option<i64>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MoveArgs {
    pub x: f64,
    pub y: f64,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BatchArgs {
    pub steps: Vec<BatchStep>,
    pub app: Option<String>,
    pub capture_mode: Option<String>,
    pub window_title: Option<String>,
    pub window_selector: Option<WindowSelector>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BatchStep {
    pub action: String,
    // Semantic targeting (click, type, scroll)
    pub target: Option<String>,
    pub location_hint: Option<String>,
    pub scope: Option<String>,
    // Click-specific
    pub button: Option<String>,
    pub clicks: Option<i64>,
    pub hold_ms: Option<i64>,
    pub settle_ms: Option<i64>,
    // Type-specific
    pub value: Option<String>,
    pub secret_env_var: Option<String>,
    pub secret_command_env_var: Option<String>,
    pub replace: Option<bool>,
    pub submit: Option<bool>,
    pub type_strategy: Option<String>,
    // Key-specific
    pub key: Option<String>,
    pub modifiers: Option<Vec<String>>,
    pub repeat: Option<i64>,
    // Scroll-specific
    pub direction: Option<String>,
    pub distance: Option<String>,
    pub amount: Option<i64>,
    // Drag-specific
    pub from_target: Option<String>,
    pub from_location_hint: Option<String>,
    pub from_scope: Option<String>,
    pub to_target: Option<String>,
    pub to_location_hint: Option<String>,
    pub to_scope: Option<String>,
    pub duration_ms: Option<i64>,
}
