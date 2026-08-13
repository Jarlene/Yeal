//! State-scoped computer-use primitives and model-facing tools.
//!
//! The crate deliberately separates the safety-critical orchestration from
//! platform access. A platform adapter implements [`UiBackend`], while this
//! crate owns state identity, bounded retention, progressive disclosure,
//! capability checks, and per-resource serialization.
//!
//! Platform adapters live in [`backends`]: native AX (macOS), UIA (Windows),
//! AT-SPI (Linux), and browser CDP. The orchestrating code in this module is
//! `unsafe`-free; the FFI boundary is confined to the platform modules.

mod backend;
mod model;
mod runtime;
mod service;
mod tools;

pub mod backends;

pub use backend::{ActOutcome, BackendError, InMemoryBackend, TextPage, UiBackend, UnsupportedBackend};
pub use model::{
    Action, ActionKind, Bounds, FindRootsRequest, ImageCapture, ObserveMode, ObserveRequest,
    Point, RootInfo, ScrollExtent, TextChunk, UiNode, UiSnapshot,
};
pub use runtime::{ResourceScheduler, StaleStateError, StateStore, StoredState};
pub use service::{
    ActRequest, ActResponse, ComputerUseService, ExpectCondition, ExpandResponse, ExpectResult,
    InspectResponse, Observation, OutlineChange, OutlineDiff, SearchMatch, SearchResponse,
    ServiceError, WaitForRequest, WaitResponse,
};
pub use tools::{
    computer_use_tools, ensure_computer_use_tool_pack_registered, register_computer_use_tool_pack,
    register_computer_use_tools,
};
