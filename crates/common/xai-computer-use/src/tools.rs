use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xai_computer_hub_sdk::LocalRegistry;
use xai_tool_protocol::{ToolCapabilities, ToolId, ToolScope};
use xai_tool_runtime::{ArcTool, ListToolsContext, Tool, ToolCallContext, ToolError, ToolOutput};
use xai_tool_types::ToolDescription;

use crate::model::{FindRootsRequest, ObserveMode};
use crate::service::{ActRequest, ComputerUseService};

const NAMESPACE: &str = "computer_use";

fn id(name: &str) -> ToolId {
    ToolId::new(format!("{NAMESPACE}:{name}")).expect("computer-use tool id is valid")
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    })
}

fn description<T: JsonSchema>(name: &str, text: &str) -> ToolDescription {
    ToolDescription::new(name, text)
        .with_namespace(NAMESPACE)
        .with_kind("computer_use")
        .with_arguments_schema(schema::<T>())
}

fn read_capabilities() -> ToolCapabilities {
    ToolCapabilities {
        is_read_only: true,
        max_concurrency: Some(8),
        ..Default::default()
    }
}

fn write_capabilities() -> ToolCapabilities {
    ToolCapabilities {
        max_concurrency: Some(1),
        tool_scope: Some(ToolScope::Write),
        supports_cancel: false,
        ..Default::default()
    }
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct FindRootsArgs {
    #[serde(flatten)]
    pub request: FindRootsRequest,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ObserveArgs {
    #[serde(rename = "root")]
    pub root_ref: String,
    #[serde(default)]
    pub mode: ObserveMode,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub state_id: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub capability: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct RefArgs {
    pub state_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ExpandArgs {
    pub state_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    #[serde(default = "default_depth")]
    pub depth: usize,
}

fn default_depth() -> usize {
    3
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ReadTextArgs {
    pub state_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_text_limit")]
    pub limit: usize,
}

fn default_text_limit() -> usize {
    2000
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct WaitArgs {
    #[serde(flatten)]
    pub request: crate::service::WaitForRequest,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct NavigateBrowserArgs {
    pub state_id: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct EvaluateBrowserArgs {
    pub state_id: String,
    pub expression: String,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct LaunchBrowserArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextOutput {
    pub text: String,
    pub offset: usize,
    pub limit: usize,
    pub total_chars: usize,
    pub has_more: bool,
}

impl ToolOutput for TextOutput {}

macro_rules! output_impl {
    ($name:ident) => {
        impl ToolOutput for $name {}
    };
}

#[derive(Debug, Clone, Serialize)]
pub struct RootsOutput {
    pub roots: Vec<crate::model::RootInfo>,
}
output_impl!(RootsOutput);

#[derive(Debug, Clone, Serialize)]
pub struct ObserveOutput {
    pub state_id: String,
    pub resource_key: String,
    pub epoch: u64,
    pub root: crate::model::RootInfo,
    pub look_id: String,
    pub captured_at_ms: u64,
    pub outline: crate::model::UiNode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<crate::model::ImageCapture>,
}
output_impl!(ObserveOutput);

#[derive(Debug, Clone, Serialize)]
pub struct SearchOutput {
    pub state_id: String,
    pub matches: Vec<crate::service::SearchMatch>,
}
output_impl!(SearchOutput);

#[derive(Debug, Clone, Serialize)]
pub struct InspectOutput {
    pub state_id: String,
    pub node: crate::model::UiNode,
}
output_impl!(InspectOutput);

#[derive(Debug, Clone, Serialize)]
pub struct ExpandOutput {
    pub state_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub node: crate::model::UiNode,
}
output_impl!(ExpandOutput);

#[derive(Debug, Clone, Serialize)]
pub struct ActToolOutput {
    pub previous_state_id: String,
    pub state_id: String,
    pub resource_key: String,
    pub epoch: u64,
    pub executed: usize,
    #[serde(default)]
    pub outcomes: Vec<crate::backend::ActOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<crate::service::ExpectResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<crate::service::OutlineDiff>,
}
output_impl!(ActToolOutput);

#[derive(Debug, Clone, Serialize)]
pub struct WaitOutput {
    pub state_id: String,
    pub satisfied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
}
output_impl!(WaitOutput);

#[derive(Debug, Clone, Serialize)]
pub struct EvaluateOutput {
    pub value: String,
}
output_impl!(EvaluateOutput);

#[derive(Debug, Clone)]
pub struct FindRootsTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct ObserveTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct SearchTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct ExpandTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct InspectTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct ActTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct ReadTextTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct WaitForTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct LaunchBrowserTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct NavigateBrowserTool {
    service: Arc<ComputerUseService>,
}
#[derive(Debug, Clone)]
pub struct EvaluateBrowserTool {
    service: Arc<ComputerUseService>,
}

impl FindRootsTool {
    async fn call(&self, args: FindRootsArgs) -> Result<RootsOutput, ToolError> {
        self.service
            .find_roots(args.request)
            .await
            .map(|roots| RootsOutput { roots })
            .map_err(tool_error)
    }
}
impl ObserveTool {
    async fn call(&self, args: ObserveArgs) -> Result<ObserveOutput, ToolError> {
        self.service
            .observe(&args.root_ref, args.mode)
            .await
            .map(|out| ObserveOutput {
                state_id: out.state_id,
                resource_key: out.resource_key,
                epoch: out.epoch,
                root: out.root,
                look_id: out.look_id,
                captured_at_ms: out.captured_at_ms,
                outline: out.outline,
                image: out.image,
            })
            .map_err(tool_error)
    }
}
impl SearchTool {
    async fn call(&self, args: SearchArgs) -> Result<SearchOutput, ToolError> {
        self.service
            .search(
                &args.state_id,
                args.text.as_deref(),
                args.role.as_deref(),
                args.capability.as_deref(),
            )
            .await
            .map(|out| SearchOutput {
                state_id: out.state_id,
                matches: out.matches,
            })
            .map_err(tool_error)
    }
}
impl ExpandTool {
    async fn call(&self, args: ExpandArgs) -> Result<ExpandOutput, ToolError> {
        self.service
            .expand(&args.state_id, &args.element_ref, args.depth)
            .await
            .map(|out| ExpandOutput {
                state_id: out.state_id,
                element_ref: out.element_ref,
                node: out.node,
            })
            .map_err(tool_error)
    }
}
impl InspectTool {
    async fn call(&self, args: RefArgs) -> Result<InspectOutput, ToolError> {
        self.service
            .inspect(&args.state_id, &args.element_ref)
            .await
            .map(|out| InspectOutput {
                state_id: out.state_id,
                node: out.node,
            })
            .map_err(tool_error)
    }
}
impl ActTool {
    async fn call(&self, args: ActRequest) -> Result<ActToolOutput, ToolError> {
        self.service
            .act(args)
            .await
            .map(|out| ActToolOutput {
                previous_state_id: out.previous_state_id,
                state_id: out.state_id,
                resource_key: out.resource_key,
                epoch: out.epoch,
                executed: out.executed,
                outcomes: out.outcomes,
                stopped_at: out.stopped_at,
                expect: out.expect,
                diff: out.diff,
            })
            .map_err(tool_error)
    }
}
impl ReadTextTool {
    async fn call(&self, args: ReadTextArgs) -> Result<TextOutput, ToolError> {
        self.service
            .read_text(&args.state_id, &args.element_ref, args.offset, args.limit)
            .await
            .map(|page| TextOutput {
                text: page.text,
                offset: page.offset,
                limit: page.limit,
                total_chars: page.total_chars,
                has_more: page.has_more,
            })
            .map_err(tool_error)
    }
}
impl WaitForTool {
    async fn call(&self, args: WaitArgs) -> Result<WaitOutput, ToolError> {
        self.service
            .wait_for(args.request)
            .await
            .map(|out| WaitOutput {
                state_id: out.state_id,
                satisfied: out.satisfied,
                timed_out: out.timed_out,
            })
            .map_err(tool_error)
    }
}
impl LaunchBrowserTool {
    async fn call(&self, args: LaunchBrowserArgs) -> Result<ObserveOutput, ToolError> {
        self.service
            .launch_browser(args.url.as_deref())
            .await
            .map(|out| ObserveOutput {
                state_id: out.state_id,
                resource_key: out.resource_key,
                epoch: out.epoch,
                root: out.root,
                look_id: out.look_id,
                captured_at_ms: out.captured_at_ms,
                outline: out.outline,
                image: out.image,
            })
            .map_err(tool_error)
    }
}
impl NavigateBrowserTool {
    async fn call(&self, args: NavigateBrowserArgs) -> Result<ObserveOutput, ToolError> {
        self.service
            .navigate_browser(&args.state_id, &args.url)
            .await
            .map(|out| ObserveOutput {
                state_id: out.state_id,
                resource_key: out.resource_key,
                epoch: out.epoch,
                root: out.root,
                look_id: out.look_id,
                captured_at_ms: out.captured_at_ms,
                outline: out.outline,
                image: out.image,
            })
            .map_err(tool_error)
    }
}
impl EvaluateBrowserTool {
    async fn call(&self, args: EvaluateBrowserArgs) -> Result<EvaluateOutput, ToolError> {
        self.service
            .evaluate_browser(&args.state_id, &args.expression)
            .await
            .map(|value| EvaluateOutput { value })
            .map_err(tool_error)
    }
}

fn tool_error(error: crate::service::ServiceError) -> ToolError {
    ToolError::custom("computer_use", error.to_string())
}

macro_rules! explicit_tool {
    ($ty:ident, $args:ty, $out:ty, $name:literal, $desc:literal, $caps:expr) => {
        impl Tool for $ty {
            type Args = $args;
            type Output = $out;
            fn id(&self) -> ToolId {
                id($name)
            }
            fn description(&self, _: &ListToolsContext) -> ToolDescription {
                description::<$args>($name, $desc)
            }
            fn capabilities(&self) -> ToolCapabilities {
                $caps
            }
            async fn run(
                &self,
                _: ToolCallContext,
                args: Self::Args,
            ) -> Result<Self::Output, ToolError> {
                self.call(args).await
            }
        }
    };
}

explicit_tool!(
    FindRootsTool,
    FindRootsArgs,
    RootsOutput,
    "find_roots",
    "Find a bounded, ranked set of controllable UI roots (desktop windows, transient surfaces, and browser pages) with refs, geometry, and focus state.",
    read_capabilities()
);
explicit_tool!(
    ObserveTool,
    ObserveArgs,
    ObserveOutput,
    "observe_ui",
    "Capture a state-scoped semantic outline (and optional image evidence) of one UI root.",
    read_capabilities()
);
explicit_tool!(
    SearchTool,
    SearchArgs,
    SearchOutput,
    "search_ui",
    "Search elements in a previously captured UI state.",
    read_capabilities()
);
explicit_tool!(
    ExpandTool,
    ExpandArgs,
    ExpandOutput,
    "expand_ui",
    "Expand one element's cached children from a captured state.",
    read_capabilities()
);
explicit_tool!(
    InspectTool,
    RefArgs,
    InspectOutput,
    "inspect_ui",
    "Inspect one element from a captured UI state.",
    read_capabilities()
);
explicit_tool!(
    ActTool,
    ActRequest,
    ActToolOutput,
    "act_ui",
    "Perform checked UI actions against a captured state and return its successor state.",
    write_capabilities()
);
explicit_tool!(
    ReadTextTool,
    ReadTextArgs,
    TextOutput,
    "read_text",
    "Read a fixed-size page of text from one element in a captured UI state.",
    read_capabilities()
);
explicit_tool!(
    WaitForTool,
    WaitArgs,
    WaitOutput,
    "wait_for",
    "Wait for a scoped UI condition and return whether it was satisfied.",
    read_capabilities()
);
explicit_tool!(
    LaunchBrowserTool,
    LaunchBrowserArgs,
    ObserveOutput,
    "launch_browser",
    "Launch the configured managed CDP browser and return an observed browser-page state.",
    write_capabilities()
);
explicit_tool!(
    NavigateBrowserTool,
    NavigateBrowserArgs,
    ObserveOutput,
    "navigate_browser",
    "Navigate an observed CDP browser-page state to an HTTP(S) URL.",
    write_capabilities()
);
explicit_tool!(
    EvaluateBrowserTool,
    EvaluateBrowserArgs,
    EvaluateOutput,
    "evaluate_browser",
    "Evaluate targeted JavaScript in a CDP browser-page state; returned output is strictly bounded.",
    write_capabilities()
);

/// Construct all computer-use tools backed by one shared service.
pub fn computer_use_tools(service: Arc<ComputerUseService>) -> Vec<ArcTool> {
    vec![
        Arc::new(FindRootsTool {
            service: Arc::clone(&service),
        }),
        Arc::new(ObserveTool {
            service: Arc::clone(&service),
        }),
        Arc::new(SearchTool {
            service: Arc::clone(&service),
        }),
        Arc::new(ExpandTool {
            service: Arc::clone(&service),
        }),
        Arc::new(InspectTool {
            service: Arc::clone(&service),
        }),
        Arc::new(ActTool {
            service: Arc::clone(&service),
        }),
        Arc::new(ReadTextTool {
            service: Arc::clone(&service),
        }),
        Arc::new(WaitForTool {
            service: Arc::clone(&service),
        }),
        Arc::new(LaunchBrowserTool {
            service: Arc::clone(&service),
        }),
        Arc::new(NavigateBrowserTool {
            service: Arc::clone(&service),
        }),
        Arc::new(EvaluateBrowserTool { service }),
    ]
}

/// Register all computer-use tools into an in-process Computer Hub registry.
pub fn register_computer_use_tools(registry: &LocalRegistry, service: Arc<ComputerUseService>) {
    for tool in computer_use_tools(service) {
        registry.register_dyn(tool);
    }
}

/// Register the computer-use tool family into a grok-tools registry builder
/// as an out-of-tree tool pack.
///
/// The tools are enabled for a session only when the session's tool config
/// lists their fully-qualified ids (`computer_use:*`); the pack just makes
/// the ids resolvable by every [`ToolRegistryBuilder`] constructed after
/// [`ensure_computer_use_tool_pack_registered`] has run. All tools share the
/// process-wide [`ComputerUseService::shared`] instance.
pub fn register_computer_use_tool_pack(
    b: &mut xai_grok_tools::registry::types::ToolRegistryBuilder,
) {
    use xai_grok_tools::types::tool::ToolKind;
    for tool in computer_use_tools(ComputerUseService::shared()) {
        let fqid = tool.id().as_str().to_owned();
        b.register_dyn_tool(&fqid, tool, ToolKind::Other);
    }
}

/// Register the computer-use tool pack into the process-wide grok-tools pack
/// list (idempotent). Must run before the first `ToolRegistryBuilder::new()`
/// in the process — the agent builder calls this before constructing any
/// toolset.
pub fn ensure_computer_use_tool_pack_registered() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        xai_grok_tools::registry::types::register_tool_pack(register_computer_use_tool_pack);
    });
}
