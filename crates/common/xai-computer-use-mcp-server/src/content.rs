//! Mapping from xai-tool-runtime outputs to MCP `CallToolResult` content.
//!
//! The computer-use tools are typed through `xai_tool_runtime::ToolDyn`, so
//! every invocation arrives here as a [`TypedToolOutput`] (success) or a
//! [`ToolError`] (failure). This module renders those into MCP content
//! blocks:
//!
//! - success → one pretty-printed JSON text block, plus one native MCP
//!   image block per image the tool emitted (e.g. `observe_ui` evidence);
//! - tool-level failure → `CallToolResult::error` with a readable message,
//!   so the caller's MCP client renders it (protocol errors stay `Err`).

use rmcp::model::{CallToolResult, ContentBlock as McpContentBlock};
use xai_tool_runtime::{ToolError, TypedToolOutput};

/// Render a successful tool output as an MCP `CallToolResult`.
///
/// The structured JSON value is the primary text block. Image evidence
/// carried in `model_output` (base64 + mime type) is appended as native
/// MCP image blocks so clients can display screenshots directly.
pub fn success_result(output: TypedToolOutput) -> CallToolResult {
    let mut blocks = vec![McpContentBlock::text(serialize_value(&output.value))];
    for block in &output.model_output {
        if let xai_tool_runtime::ContentBlock::Image { mime_type, data, .. } = block {
            blocks.push(McpContentBlock::image(data.clone(), mime_type.clone()));
        }
    }
    CallToolResult::success(blocks)
}

/// Render a tool-level failure as an MCP `CallToolResult` with `isError`.
///
/// The tool ran (or tried to) and failed in a way the caller should see;
/// MCP clients render the `content` directly. This mirrors
/// `CallToolResult::error`'s documented contract — protocol-level routing
/// failures are the caller's job to return as `Err(McpError)`.
pub fn error_result(error: ToolError) -> CallToolResult {
    CallToolResult::error(vec![McpContentBlock::text(format!(
        "computer_use tool failed: {error}"
    ))])
}

/// Serialise a tool output value to a readable text block.
///
/// Strings pass through as-is; everything else is pretty-printed JSON so a
/// human (or model) can scan the structured output.
fn serialize_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| {
            serde_json::Value::Null.to_string()
        }),
    }
}
