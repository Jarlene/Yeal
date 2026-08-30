//! Rust↔Swift helper wire types shared by the platform modules and the GUI
//! engine. Upstream home: `core/src/tools/handlers/gui.rs` (the `Helper*`
//! structs, camelCase serde — the Rust side of the native helper protocol).
//!
//! `HelperPoint`/`HelperRect` live in [`super::grounding`] next to the
//! grounding coordinate math that also consumes them.

use serde::Deserialize;
use serde::Serialize;

use super::grounding::HelperPoint;
use super::grounding::HelperRect;

/// Snapshot of the capture surface returned by the helper's
/// `capture-context` command: frontmost app, cursor, display and matched
/// window metadata.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HelperCaptureContext {
    pub(crate) app_name: Option<String>,
    pub(crate) cursor: HelperPoint,
    pub(crate) display: HelperDisplayDescriptor,
    pub(crate) window_id: Option<i64>,
    pub(crate) window_title: Option<String>,
    pub(crate) window_bounds: Option<HelperRect>,
    pub(crate) window_count: Option<i64>,
    pub(crate) window_capture_strategy: Option<String>,
    pub(crate) host_self_exclude_applied: Option<bool>,
    pub(crate) host_frontmost_excluded: Option<bool>,
    pub(crate) host_frontmost_app_name: Option<String>,
    pub(crate) host_frontmost_bundle_id: Option<String>,
}

/// One macOS display descriptor (index + logical bounds) from the helper.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct HelperDisplayDescriptor {
    pub(crate) index: i64,
    pub(crate) bounds: HelperRect,
}
