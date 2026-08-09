//! Tests for the computer-use MCP server: tool discovery/name mapping,
//! and a full MCP session over an in-memory stdio-style transport.

use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, ContentBlock, PaginatedRequestParams};
use rmcp::ServerHandler;
use serde_json::json;
use xai_computer_use::{
    ComputerUseService, InMemoryBackend, RootInfo, UiNode,
};
use xai_computer_use_mcp_server::ComputerUseMcpServer;

/// A computer-use service backed by an in-memory backend with one fake
/// window root, so tests never touch the real desktop or need OS
/// accessibility permissions.
fn fixture() -> Arc<ComputerUseService> {
    let root = RootInfo {
        root_ref: "@r1".into(),
        resource_key: "window:1".into(),
        kind: "window".into(),
        title: "Test App".into(),
        app: Some("test.app".into()),
        pid: Some(42),
        ..Default::default()
    };
    let button = UiNode {
        element_ref: "@e2".into(),
        wire_ref: Some("ax:1".into()),
        role: "button".into(),
        title: "Save".into(),
        can_press: true,
        actions: vec!["press".into()],
        ..Default::default()
    };
    let root_node = UiNode {
        element_ref: "@e1".into(),
        role: "window".into(),
        title: "Test App".into(),
        children: vec![button],
        ..Default::default()
    };
    Arc::new(ComputerUseService::new(Arc::new(InMemoryBackend::new(
        vec![(root, root_node)],
    ))))
}

fn server() -> ComputerUseMcpServer {
    ComputerUseMcpServer::new(fixture())
}

fn is_mcp_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn tool_count_matches_the_computer_use_family() {
    assert_eq!(server().tool_count(), 11);
}

#[test]
fn tool_names_are_mcp_compliant_and_mapped_from_tool_ids() {
    let srv = server();
    // Names use the `computer_use__name` convention (MCP forbids `:`).
    for name in [
        "computer_use__find_roots",
        "computer_use__observe_ui",
        "computer_use__search_ui",
        "computer_use__expand_ui",
        "computer_use__inspect_ui",
        "computer_use__act_ui",
        "computer_use__read_text",
        "computer_use__wait_for",
        "computer_use__launch_browser",
        "computer_use__navigate_browser",
        "computer_use__evaluate_browser",
    ] {
        let tool = srv.get_tool(name).expect("tool should be discoverable");
        assert!(is_mcp_tool_name(tool.name.as_ref()), "bad name: {tool:?}");
        assert!(tool.description.is_some());
        assert!(
            tool.input_schema.get("type") == Some(&json!("object")),
            "schema must be an object: {:?}",
            tool.input_schema
        );
    }
    // The raw in-process id (`:` separator) is not a valid MCP name.
    assert!(srv.get_tool("computer_use:find_roots").is_none());
}

#[tokio::test]
async fn full_mcp_session_over_in_memory_transport() {
    let srv = server();
    let (client_stream, server_stream) = tokio::io::duplex(1 << 16);
    let (client_reader, client_writer) = tokio::io::split(client_stream);
    let (server_reader, server_writer) = tokio::io::split(server_stream);

    let server_task = tokio::spawn(async move {
        rmcp::serve_server(srv, (server_reader, server_writer)).await
    });

    // `()` is a no-op rmcp ClientHandler with default client info; the
    // handshake (initialize) is performed by serve_client itself.
    let mut client = rmcp::serve_client((), (client_reader, client_writer))
        .await
        .expect("client handshake should succeed");

    // tools/list
    let tools = client
        .list_all_tools()
        .await
        .expect("tools/list should succeed");
    assert_eq!(tools.len(), 11, "all computer-use tools are advertised");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"computer_use__find_roots"));
    assert!(names.contains(&"computer_use__act_ui"));
    assert!(tools.iter().all(|t| is_mcp_tool_name(t.name.as_ref())));

    // tools/call: find_roots
    let result = client
        .call_tool(CallToolRequestParams::new("computer_use__find_roots"))
        .await
        .expect("find_roots call should succeed");
    assert_ne!(result.is_error, Some(true), "find_roots must not error");
    let text = result_text(&result);
    assert!(
        text.contains("Test App"),
        "structured output should carry the fake window: {text}"
    );

    // tools/call: observe_ui on the discovered root
    let args: rmcp::model::JsonObject =
        serde_json::from_value(json!({ "root": "@r1" })).unwrap();
    let result = client
        .call_tool(
            CallToolRequestParams::new("computer_use__observe_ui").with_arguments(args),
        )
        .await
        .expect("observe_ui call should succeed");
    assert_ne!(result.is_error, Some(true), "observe_ui must not error");
    let text = result_text(&result);
    assert!(
        text.contains("state_id") && text.contains("Test App"),
        "observe_ui should return a captured state with the window outline: {text}"
    );

    // tools/call: unknown tool → protocol error on the wire
    let unknown = client
        .call_tool(CallToolRequestParams::new("computer_use__no_such_tool"))
        .await;
    assert!(unknown.is_err(), "unknown tool must be a protocol error");

    // Clean shutdown: close the client, let the server loop exit.
    let _ = client.close().await;
    let _ = server_task.await;
}

#[tokio::test]
async fn paginated_list_tools_returns_everything_in_one_page() {
    let srv = server();
    let (client_stream, server_stream) = tokio::io::duplex(1 << 16);
    let (client_reader, client_writer) = tokio::io::split(client_stream);
    let (server_reader, server_writer) = tokio::io::split(server_stream);

    let server_task = tokio::spawn(async move {
        rmcp::serve_server(srv, (server_reader, server_writer)).await
    });

    let mut client = rmcp::serve_client((), (client_reader, client_writer))
        .await
        .expect("client handshake should succeed");

    let listed = client
        .list_tools(Some(PaginatedRequestParams::default()))
        .await
        .expect("tools/list should succeed");
    assert_eq!(listed.tools.len(), 11);
    assert!(listed.next_cursor.is_none(), "no pagination needed");

    let _ = client.close().await;
    let _ = server_task.await;
}

/// Extract the concatenated text from a `CallToolResult`'s content blocks.
fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
