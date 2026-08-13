//! MCP server exposing the [`xai_computer_use`] tool family.
//!
//! This crate wraps the state-scoped computer-use tools (desktop and
//! browser UI automation) behind the Model Context Protocol, so any MCP
//! client can drive them over stdio:
//!
//! ```text
//! MCP client  <──stdio JSON-RPC──>  ComputerUseMcpServer  ──>  ComputerUseService
//!                                    (rmcp ServerHandler)         (native backend)
//! ```
//!
//! Tools are discovered from [`xai_computer_use::tools::computer_use_tools`]
//! at construction and exposed under MCP-compliant names:
//! `computer_use:find_roots` → `computer_use_find_roots` (the `:` separator
//! of the in-process [`ToolId`] is not valid in an MCP tool name, so it maps
//! to `_`. Keeping `__` out of the exposed name preserves the workspace's
//! unambiguous `server__tool` qualified-name convention for MCP clients).
//!
//! # Usage
//!
//! ```rust,ignore
//! let service = xai_computer_use::ComputerUseService::shared();
//! let server = ComputerUseMcpServer::new(service);
//! let running = rmcp::serve_server(server, (tokio::io::stdin(), tokio::io::stdout())).await?;
//! running.waiting().await?;
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use futures::StreamExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorData as McpError, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
    Tool as McpTool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use xai_computer_use::computer_use_tools;
use xai_computer_use::ComputerUseService;
use xai_tool_protocol::{ToolId, ToolScope};
use xai_tool_runtime::{ArcTool, ListToolsContext, ToolCallContext, ToolStreamItem};

mod content;

/// A state-scoped computer-use server ready to serve MCP requests.
pub struct ComputerUseMcpServer {
    service: Arc<ComputerUseService>,
    tools: Vec<ArcTool>,
    by_name: HashMap<String, ArcTool>,
}

impl std::fmt::Debug for ComputerUseMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerUseMcpServer")
            .field("tool_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}

impl ComputerUseMcpServer {
    /// Build a server over one shared [`ComputerUseService`].
    ///
    /// All tools share the same service instance, so captured UI state is
    /// shared across the whole tool family (and across connections to this
    /// server process).
    pub fn new(service: Arc<ComputerUseService>) -> Self {
        let tools = computer_use_tools(Arc::clone(&service));
        let by_name = tools
            .iter()
            .map(|tool: &ArcTool| (mcp_tool_name(&tool.id()), tool.clone()))
            .collect();
        Self {
            service,
            tools,
            by_name,
        }
    }

    /// The number of registered tools (11 for the full computer-use family).
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// The shared [`ComputerUseService`] backing every tool in this server.
    pub fn service(&self) -> &Arc<ComputerUseService> {
        &self.service
    }
}

/// Map an in-process [`ToolId`] (`computer_use:find_roots`) to the
/// MCP-compliant tool name (`computer_use_find_roots`).
///
/// `:` is replaced with a single `_` rather than `__`: an MCP tool name
/// containing `__` collides with the `server__tool` qualified-name boundary
/// used by MCP clients, so e.g. `computer-use__computer_use__find_roots`
/// parses as ambiguous and every tool in the family gets rejected at
/// discovery.
fn mcp_tool_name(id: &ToolId) -> String {
    id.as_str().replace(':', "_")
}

/// Convert one [`ArcTool`] into an rmcp `Tool` definition for `tools/list`.
fn to_mcp_tool(tool: &ArcTool) -> McpTool {
    let description = tool.description(&ListToolsContext::new());
    let capabilities = tool.capabilities();
    let annotations = ToolAnnotations::new()
        .read_only(capabilities.is_read_only)
        .destructive(capabilities.tool_scope == Some(ToolScope::Write));

    let name = mcp_tool_name(&tool.id());
    let schema = Arc::new(to_json_object(description.to_input_schema()));

    McpTool::new(name, description.description, schema).with_annotations(annotations)
}

/// Coerce a JSON schema `Value` into rmcp's `JsonObject` (object map),
/// falling back to an empty object schema when the value is not an object.
fn to_json_object(schema: serde_json::Value) -> JsonObject {
    serde_json::from_value::<JsonObject>(schema).unwrap_or_default()
}

impl ServerHandler for ComputerUseMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("xai-computer-use-mcp-server", env!("CARGO_PKG_VERSION")),
        )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools = self.tools.iter().map(to_mcp_tool).collect::<Vec<_>>();
        async move {
            Ok(ListToolsResult {
                tools,
                next_cursor: None,
                meta: None,
            })
        }
    }

    fn get_tool(&self, name: &str) -> Option<McpTool> {
        self.by_name.get(name).map(to_mcp_tool)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool = self.by_name.get(request.name.as_ref()).ok_or_else(|| {
            McpError::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            )
        })?;

        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let stream = tool.execute(ToolCallContext::default(), args).await;

        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            match item {
                ToolStreamItem::Progress(_) => {}
                ToolStreamItem::Terminal(result) => {
                    return match result {
                        Ok(output) => Ok(content::success_result(output)),
                        Err(error) => Ok(content::error_result(error)),
                    };
                }
            }
        }

        Err(McpError::internal_error(
            "tool stream ended without a terminal result",
            None,
        ))
    }
}
