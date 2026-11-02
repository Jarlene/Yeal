//! Native GUI tool calls (upstream PR understudy-ai/codex-cua#3) flowing
//! through the headless mapping: `map_session_update` keeps the canonical
//! `gui_*` tool name/kind from the `x.ai/tool` meta, and the typed
//! `ToolOutput::Gui` result rides `rawOutput` into the reducer output.

use super::*;
use crate::headless::reducer::map_session_update;
use pretty_assertions::assert_eq;
// `acp` binds to the protocol crate here to avoid resolving to the sibling module through `use super::*`.
use agent_client_protocol as acp;
use xai_grok_tools::types::output::ToolOutput;

fn gui_tool_call() -> acp::ToolCall {
    acp::ToolCall::new(
        acp::ToolCallId::from("g1".to_string()),
        "gui_click".to_string(),
    )
    .kind(acp::ToolKind::Other)
    .status(acp::ToolCallStatus::Pending)
    .raw_input(Some(json!({"target": "Send button", "x": 120, "y": 240})))
    .meta(
        json!({"x.ai/tool": {"name": "gui_click", "kind": "gui"}})
            .as_object()
            .cloned(),
    )
}

fn gui_tool_call_update(success: bool) -> acp::ToolCallUpdate {
    let output = ToolOutput::Gui(xai_grok_tools::implementations::gui::GuiToolOutput {
        message: "Clicked Send button.".into(),
        image_url: None,
        code_result: json!({"tool": "gui_click"}),
        success,
    });
    acp::ToolCallUpdate::new(
        acp::ToolCallId::from("g1".to_string()),
        acp::ToolCallUpdateFields::new()
            .status(Some(if success {
                acp::ToolCallStatus::Completed
            } else {
                acp::ToolCallStatus::Failed
            }))
            .raw_output(serde_json::to_value(output).ok()),
    )
}

#[test]
fn gui_tool_call_maps_to_headless_event_with_canonical_name() {
    let update = acp::SessionUpdate::ToolCall(gui_tool_call());
    let Some(StreamEvent::ToolCall(ev)) = map_session_update(&update) else {
        panic!("expected a ToolCall event");
    };
    assert_eq!(ev.tool_name, "gui_click");
    assert_eq!(ev.tool_kind.as_deref(), Some("gui"));
    assert_eq!(ev.title, "gui_click");
    assert_eq!(ev.raw_input["target"], "Send button");
}

#[test]
fn gui_typed_result_flows_to_acp_reducer_native_shape() {
    let started =
        map_session_update(&acp::SessionUpdate::ToolCall(gui_tool_call())).expect("tool_call");
    let done = map_session_update(&acp::SessionUpdate::ToolCallUpdate(gui_tool_call_update(
        true,
    )))
    .expect("tool_call_update");

    let mut r = AcpReducer;
    let lines = r.reduce(started);
    assert_eq!(lines[0]["type"], "tool_call");
    assert_eq!(lines[0]["toolName"], "gui_click");
    assert_eq!(lines[0]["kind"], "gui");
    assert_eq!(lines[0]["status"], "pending");

    let lines = r.reduce(done);
    assert_eq!(lines[0]["type"], "tool_call_update");
    assert_eq!(lines[0]["status"], "completed");
    assert_eq!(lines[0]["rawOutput"]["type"], "Gui");
    assert_eq!(lines[0]["rawOutput"]["message"], "Clicked Send button.");
    assert_eq!(lines[0]["rawOutput"]["success"], true);
}

#[test]
fn gui_soft_failure_flows_to_acp_reducer_as_failed() {
    let done = map_session_update(&acp::SessionUpdate::ToolCallUpdate(gui_tool_call_update(
        false,
    )))
    .expect("tool_call_update");
    let mut r = AcpReducer;
    let lines = r.reduce(done);
    assert_eq!(lines[0]["type"], "tool_call_update");
    assert_eq!(lines[0]["status"], "failed");
    assert_eq!(lines[0]["rawOutput"]["success"], false);
}
