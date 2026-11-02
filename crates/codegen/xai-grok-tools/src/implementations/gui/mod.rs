//! Native GUI automation tools for macOS (Swift helper), Linux (in-process
//! X11 via x11rb), and Windows (in-process Win32 via windows-sys) — port of
//! upstream codex PR
//! understudy-ai/codex-cua#3 (`feat(gui): native GUI tools with builtin tool
//! call observability`).
//!
//! Layout (upstream home in parentheses):
//! - [`schemas`] — hand-built JSON schemas + typed Args
//!   (`tools/gui_tool.rs`)
//! - [`handler`] — the engine: one dispatcher for all nine tools, the
//!   observe-act-verify loop, wait probes, batch execution
//!   (`core/src/tools/handlers/gui.rs`)
//! - [`grounding`] — semantic target resolution: prompts, image prep,
//!   validation/refinement round loop, batch grounding
//!   (`handlers/gui/grounding.rs`)
//! - [`provider`] — alternate grounding-backend seam
//!   (`handlers/gui/provider.rs`)
//! - [`platform`] — OS abstraction + the macOS implementation (Swift helper
//!   lifecycle, IPC, readiness probes, screencapture, osascript), the
//!   Linux implementation (in-process x11rb X11 backend: XTEST input, RANDR
//!   geometry, GetImage capture), and the Windows implementation (in-process
//!   Win32 backend: GDI BitBlt/GetDIBits capture, SendInput input)
//!   (`handlers/gui/platform*.rs`)
//! - [`session`] — physical-resource lock + guaranteed per-action cleanup
//!   (`handlers/gui/session.rs`)
//! - [`readiness`] — environment capability matrix (30s cached)
//!   (`handlers/gui/readiness.rs`)
//! - [`tools`] — the nine thin `Tool`/`ToolMetadata` structs registered with
//!   the toolset (upstream registered one multiplexing handler)
//!
//! Harness adaptations (upstream logic otherwise preserved):
//! - Model access for grounding goes through the
//!   [`grounding::GuiGroundingSampler`] seam injected via `ToolCallContext`
//!   extensions; its presence doubles as the image-input capability gate.
//! - `FunctionCallError::RespondToModel` → `ToolError` (kind `Execution`);
//!   the detail string stays the model-facing message.
//! - `CODEX_GUI_*` env-var names are kept verbatim — they are half of the
//!   Rust↔Swift helper protocol shared with `native_helper.swift`.
//!
//! Real-machine side effects: the tools drive the mouse/keyboard and capture
//! the screen on macOS, on Linux (X11), and on Windows. Live end-to-end
//! coverage is upstream `#[ignore]`
//! territory and was not ported; the tests that ship here are pure logic.

pub mod grounding;
pub mod handler;
pub mod platform;
pub mod provider;
pub mod readiness;
pub mod schemas;
pub mod session;
pub mod tools;
pub(crate) mod wire;

#[cfg(test)]
mod schema_tests;

pub use handler::GuiToolOutput;
pub use handler::GuiToolsSettings;
pub use tools::{
    GuiBatchTool, GuiClickTool, GuiDragTool, GuiKeyTool, GuiMoveTool, GuiObserveTool,
    GuiScrollTool, GuiTypeTool, GuiWaitTool,
};

/// Shell-resolved GUI tools configuration (upstream: `ToolsConfig.gui_tools`
/// feature + `ToolsConfig.gui_*` fields from `[tools.gui]`).
///
/// `Disabled` (the default) keeps the tools unadvertised and refuses
/// dispatch; `Enabled` carries the resolved [`GuiToolsSettings`] that gate
/// the batch tool and the experimental direct-coordinate fields.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum GuiToolsConfig {
    #[default]
    Disabled,
    Enabled(GuiToolsSettings),
}

impl GuiToolsConfig {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled(_))
    }

    /// The resolved settings, or the upstream defaults when disabled.
    pub fn settings(&self) -> GuiToolsSettings {
        match self {
            Self::Disabled => GuiToolsSettings::default(),
            Self::Enabled(settings) => settings.clone(),
        }
    }
}
