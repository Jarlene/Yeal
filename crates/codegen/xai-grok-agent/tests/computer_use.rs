//! End-to-end verification that the xai-computer-use tool family is usable
//! from a grok-build agent toolset.
//!
//! Regression coverage for the fix that made computer-use visible to the
//! model. Three things must all hold:
//!
//! 1. The out-of-tree tool pack makes `computer_use:*` ids resolvable in
//!    every [`ToolRegistryBuilder`], so `finalize` does not panic on an
//!    unknown id.
//! 2. The default grok-build toolset lists the computer-use tools, so they
//!    appear in the model-facing `tool_definitions()`.
//! 3. In-process dispatch through the finalized toolset resolves and
//!    executes (no "Tool not found"), meaning the tools are actually
//!    callable by the model.

use std::collections::HashMap;
use std::sync::Arc;

use tempfile::TempDir;
use xai_grok_agent::config::workspace_grok_build_toolset;
use xai_grok_tools::registry::types::{
    SessionContext, ToolConfig, ToolRegistryBuilder, ToolServerConfig,
};
use xai_tool_protocol::ToolId;

/// Fully-qualified ids of the computer-use tool family. Must match the ids
/// registered by `xai_computer_use::register_computer_use_tool_pack` and
/// listed by `default_grok_build_toolset`.
const COMPUTER_USE_IDS: [&str; 11] = [
    "computer_use:find_roots",
    "computer_use:observe_ui",
    "computer_use:search_ui",
    "computer_use:expand_ui",
    "computer_use:inspect_ui",
    "computer_use:act_ui",
    "computer_use:read_text",
    "computer_use:wait_for",
    "computer_use:launch_browser",
    "computer_use:navigate_browser",
    "computer_use:evaluate_browser",
];

/// Build a `SessionContext` for finalization using a temp dir and the real
/// local filesystem/terminal backends (same shape as the registry's own
/// unit-test helper).
fn session_context(tmp: &TempDir) -> SessionContext {
    SessionContext {
        backend: Arc::new(xai_grok_tools::computer::local::LocalTerminalBackend::new()),
        fs: Arc::new(xai_grok_tools::computer::local::LocalFs),
        cwd: tmp.path().to_path_buf(),
        session_folder: tmp.path().join("session"),
        session_env: Arc::new(HashMap::new()),
        notification_handle: xai_grok_tools::notification::ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: tmp.path().join("state.json"),
        memory_backend: None,
        web_search_config: xai_grok_tools::implementations::web_search::WebSearchConfig::default(),
        web_fetch_config: xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig::default(),
        lsp: None,
        image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig::default(),
        video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig::default(),
        app_builder_deployer_config: xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    }
}

/// The default grok-build toolset must list every computer-use tool, so the
/// agent's model-facing toolset includes them.
#[test]
fn default_grok_build_toolset_lists_computer_use_tools() {
    let toolset = workspace_grok_build_toolset();
    let ids: Vec<&str> = toolset.tools.iter().map(|t| t.id.as_str()).collect();
    for id in COMPUTER_USE_IDS {
        assert!(
            ids.contains(&id),
            "default grok-build toolset is missing {id}"
        );
    }
}

/// After `ensure_computer_use_tool_pack_registered`, every builder can
/// resolve the computer-use ids (so `finalize` will not panic on them).
#[test]
fn tool_pack_makes_computer_use_ids_resolvable() {
    xai_computer_use::ensure_computer_use_tool_pack_registered();
    let builder = ToolRegistryBuilder::new();
    for id in COMPUTER_USE_IDS {
        assert!(
            builder.has_tool_id(id),
            "tool pack did not register {id} in ToolRegistryBuilder"
        );
    }
}

/// Finalize a toolset containing only the computer-use tools and prove the
/// full runtime path works: the tools show up in `tool_definitions()`, are
/// present in the toolset's `LocalRegistry`, and dispatch executes rather
/// than failing with "Tool not found".
#[tokio::test]
async fn finalized_toolset_exposes_and_dispatches_computer_use_tools() {
    xai_computer_use::ensure_computer_use_tool_pack_registered();
    let tmp = TempDir::new().expect("temp dir");

    let config = ToolServerConfig {
        tools: COMPUTER_USE_IDS
            .iter()
            .map(|id| ToolConfig::from_id(*id))
            .collect(),
        behavior_preset: None,
    };
    let finalized = ToolRegistryBuilder::new()
        .finalize(config, session_context(&tmp))
        .expect("finalize with only computer-use tools");
    let toolset = Arc::new(finalized);

    // Model-facing definitions: the client must see the tools.
    let defs = toolset.tool_definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    for id in COMPUTER_USE_IDS {
        assert!(
            names.contains(&id),
            "model-facing tool_definitions() is missing {id}; got {names:?}"
        );
    }

    // Local registry: dispatch resolves to the shared computer-use service.
    let tid = ToolId::new("computer_use:find_roots").expect("valid tool id");
    assert!(
        toolset.local_registry().find(&tid).is_some(),
        "computer_use:find_roots missing from finalized LocalRegistry"
    );

    // Dispatch: must reach the tool (may error at runtime on this machine —
    // e.g. missing Accessibility permission — but must NOT be a resolution
    // failure).
    let result = toolset
        .call_with_cancellation(
            "computer_use:find_roots",
            serde_json::json!({}),
            "call-1",
            None,
            None,
        )
        .await;
    match &result {
        Ok(run) => {
            println!("dispatch: Ok — computer_use:find_roots executed; output={:?}", run.output);
            assert!(
                matches!(
                    run.output,
                    xai_grok_tools::types::output::ToolOutput::Dynamic(_)
                ),
                "computer_use:find_roots returned unexpected output: {:?}",
                run.output
            );
        }
        Err(e) => {
            println!("dispatch: Err — tool ran but returned a runtime error: {e}");
            let msg = e.to_string().to_lowercase();
            assert!(
                !msg.contains("not found"),
                "dispatch failed to resolve computer_use:find_roots: {e}"
            );
        }
    }
}
