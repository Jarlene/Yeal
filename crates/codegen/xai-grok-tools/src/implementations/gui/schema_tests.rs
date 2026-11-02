//! Schema tests for the native GUI tools.
//!
//! Ported from the `#[cfg(test)] mod tests` block of upstream
//! `codex-rs/tools/src/gui_tool.rs`, plus the schema-shape assertions of
//! upstream `codex-rs/core/src/tools/spec_tests.rs`
//! (`gui_coordinate_targeting_adds_coordinate_fields_to_gui_tools`,
//! `gui_tools_present_when_feature_enabled`) rebased onto the pure schema
//! constructors — the upstream spec tests ran the full tool-registration
//! pipeline, which is exercised by the registration slice instead.

use serde_json::{Value, json};

use super::schemas::{
    GuiToolSchemaOptions, create_gui_click_tool, create_gui_click_tool_with_options,
    create_gui_drag_tool, create_gui_drag_tool_with_options, create_gui_key_tool,
    create_gui_move_tool, create_gui_observe_tool, create_gui_scroll_tool, create_gui_type_tool,
    create_gui_wait_tool,
};
use crate::types::definition::ToolDefinition;

fn function_parameters(tool: &ToolDefinition) -> &serde_json::Map<String, Value> {
    tool.function
        .parameters
        .get("properties")
        .and_then(Value::as_object)
        .expect("expected object schema with properties")
}

/// The top-level `required` list of the object schema (sibling of
/// `properties`, matching upstream's `JsonSchema::Object { required }`).
fn function_required(tool: &ToolDefinition) -> Option<&Value> {
    tool.function.parameters.get("required")
}

#[test]
fn click_tool_exposes_semantic_target_properties() {
    let tool = create_gui_click_tool();
    let properties = function_parameters(&tool);

    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("location_hint"));
    assert!(properties.contains_key("scope"));
    assert!(properties.contains_key("grounding_mode"));
    assert!(!properties.contains_key("x"));
    assert!(!properties.contains_key("y"));
    assert_eq!(function_required(&tool), Some(&json!(["target"])));
}

#[test]
fn click_tool_optionally_exposes_coordinate_properties() {
    let tool = create_gui_click_tool_with_options(GuiToolSchemaOptions {
        coordinate_targeting: true,
    });
    let properties = function_parameters(&tool);

    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("x"));
    assert!(properties.contains_key("y"));
    assert!(properties.contains_key("coordinate_space"));
    assert!(function_required(&tool).is_none());
    assert!(
        tool.function
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("experimental direct-coordinate placeholder")
    );
}

#[test]
fn wait_tool_exposes_target_wait_controls() {
    let tool = create_gui_wait_tool();
    let properties = function_parameters(&tool);

    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("state"));
    assert!(properties.contains_key("timeout_ms"));
    assert!(properties.contains_key("interval_ms"));
    assert!(properties.contains_key("scope"));
    assert!(properties.contains_key("grounding_mode"));
    assert!(!properties.contains_key("duration_ms"));
    assert!(!properties.contains_key("return_image"));
    assert_eq!(function_required(&tool), Some(&json!(["target"])));
}

#[test]
fn drag_tool_exposes_semantic_source_and_destination_properties() {
    let tool = create_gui_drag_tool();
    let properties = function_parameters(&tool);

    assert!(properties.contains_key("grounding_mode"));
    assert!(properties.contains_key("from_target"));
    assert!(properties.contains_key("from_location_hint"));
    assert!(properties.contains_key("from_scope"));
    assert!(properties.contains_key("to_target"));
    assert!(properties.contains_key("to_location_hint"));
    assert!(properties.contains_key("to_scope"));
    assert!(!properties.contains_key("steps"));
    assert!(!properties.contains_key("from_x"));
    assert!(!properties.contains_key("from_y"));
    assert!(!properties.contains_key("to_x"));
    assert!(!properties.contains_key("to_y"));
    assert_eq!(
        function_required(&tool),
        Some(&json!(["from_target", "to_target"]))
    );
}

#[test]
fn drag_tool_optionally_exposes_coordinate_properties() {
    let tool = create_gui_drag_tool_with_options(GuiToolSchemaOptions {
        coordinate_targeting: true,
    });
    let properties = function_parameters(&tool);

    assert!(properties.contains_key("from_target"));
    assert!(properties.contains_key("to_target"));
    assert!(properties.contains_key("from_x"));
    assert!(properties.contains_key("from_y"));
    assert!(properties.contains_key("to_x"));
    assert!(properties.contains_key("to_y"));
    assert!(properties.contains_key("coordinate_space"));
    assert!(function_required(&tool).is_none());
    assert!(
        tool.function
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("experimental direct-coordinate placeholder")
    );
}

#[test]
fn observe_tool_exposes_semantic_grounding_properties() {
    let tool = create_gui_observe_tool();
    let properties = function_parameters(&tool);

    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("location_hint"));
    assert!(properties.contains_key("scope"));
    assert!(properties.contains_key("grounding_mode"));
    let description = properties["target"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(description.contains("visible screenshot evidence"));
    assert!(description.contains("actionable or editable control"));
}

#[test]
fn scroll_tool_exposes_understudy_aligned_scroll_semantics() {
    let scroll_tool = create_gui_scroll_tool();
    let properties = function_parameters(&scroll_tool);

    assert!(properties.contains_key("direction"));
    assert!(properties.contains_key("distance"));
    assert!(properties.contains_key("amount"));
    assert!(!properties.contains_key("delta_x"));
    assert!(!properties.contains_key("delta_y"));
    assert!(!properties.contains_key("unit"));
    assert!(!properties.contains_key("x"));
    assert!(!properties.contains_key("y"));
}

#[test]
fn type_tool_matches_understudy_native_contract_shape() {
    let type_tool = create_gui_type_tool();
    let properties = function_parameters(&type_tool);

    assert!(properties.contains_key("value"));
    assert!(properties.contains_key("type_strategy"));
    assert!(!properties.contains_key("text"));
    assert!(!properties.contains_key("strategy"));
    let description = properties["type_strategy"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(!description.contains("unicode"));
}

// ───────────────────────────────────────────────────────────────────────────
// Pure-logic ports of the upstream spec_tests.rs GUI tests.
// ───────────────────────────────────────────────────────────────────────────

/// Upstream asserted these names appear in the advertised toolset when the
/// GuiTools feature is enabled (`gui_batch` is gated separately). The
/// registration pipeline is out of scope here, so the pure-logic equivalent
/// pins the names produced by the schema constructors.
#[test]
fn gui_tool_creators_produce_core_feature_tool_names() {
    let names = [
        create_gui_observe_tool(),
        create_gui_wait_tool(),
        create_gui_click_tool(),
        create_gui_drag_tool(),
        create_gui_scroll_tool(),
        create_gui_type_tool(),
        create_gui_key_tool(),
        create_gui_move_tool(),
    ]
    .map(|tool| tool.function.name);

    assert_eq!(
        names,
        [
            "gui_observe",
            "gui_wait",
            "gui_click",
            "gui_drag",
            "gui_scroll",
            "gui_type",
            "gui_key",
            "gui_move",
        ]
    );
}

#[test]
fn gui_coordinate_targeting_adds_coordinate_fields_to_gui_tools() {
    let click_tool = create_gui_click_tool_with_options(GuiToolSchemaOptions {
        coordinate_targeting: true,
    });
    let drag_tool = create_gui_drag_tool_with_options(GuiToolSchemaOptions {
        coordinate_targeting: true,
    });

    let click_props = function_parameters(&click_tool);
    assert!(click_props.contains_key("x"));
    assert!(click_props.contains_key("y"));
    assert!(click_props.contains_key("coordinate_space"));
    assert!(function_required(&click_tool).is_none());

    let drag_props = function_parameters(&drag_tool);
    assert!(drag_props.contains_key("from_x"));
    assert!(drag_props.contains_key("from_y"));
    assert!(drag_props.contains_key("to_x"));
    assert!(drag_props.contains_key("to_y"));
    assert!(drag_props.contains_key("coordinate_space"));
    assert!(function_required(&drag_tool).is_none());
}
