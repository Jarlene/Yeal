use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A point in screen points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Rectangle in screen points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A root window, transient surface, or browser page exposed by a platform
/// backend. `resource_key` is the physical-resource identity used by the
/// scheduler (e.g. `desktop-pid:123` or `cdp:<targetId>`); `root_ref` is the
/// stable agent-facing ref (`@rN`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RootInfo {
    #[serde(rename = "ref")]
    pub root_ref: String,
    pub resource_key: String,
    /// `window`, `menu`, `sheet`, `popover`, `dialog`, or `browser_page`.
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subrole: Option<String>,
    #[serde(default)]
    pub z_order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Bounds>,
    #[serde(default)]
    pub scale_factor: f64,
    #[serde(default)]
    pub is_onscreen: bool,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub is_minimized: bool,
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    pub is_modal: bool,
}

/// Scroll extent hint (screen-space pixels already consumed vs. total).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScrollExtent {
    pub seen: u64,
    pub total: u64,
}

/// A rendered text run attached to a node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TextChunk {
    pub string: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Bounds>,
}

/// A semantic UI node. Agent-facing `element_ref`s (`@eN`) are allocated per
/// observation and are invalid outside that observation's state id. The
/// backend-facing `wire_ref` is the native identity used to re-resolve the
/// element during `act` / `read_text`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiNode {
    #[serde(rename = "ref")]
    pub element_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_ref: Option<String>,
    pub role: String,
    #[serde(default)]
    pub subrole: String,
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub can_press: bool,
    #[serde(default)]
    pub can_focus: bool,
    #[serde(default)]
    pub can_set_value: bool,
    #[serde(default)]
    pub can_scroll: bool,
    #[serde(default)]
    pub can_increment: bool,
    #[serde(default)]
    pub can_decrement: bool,
    #[serde(default)]
    pub is_text_input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub offscreen: bool,
    #[serde(default)]
    pub picture_only: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_extent: Option<ScrollExtent>,
    #[serde(default)]
    pub text: Vec<TextChunk>,
    #[serde(default)]
    pub children: Vec<UiNode>,
}

impl UiNode {
    pub fn walk<'a>(&'a self, out: &mut Vec<&'a UiNode>) {
        out.push(self);
        for child in &self.children {
            child.walk(out);
        }
    }

    pub fn find(&self, element_ref: &str) -> Option<&UiNode> {
        if self.element_ref == element_ref {
            return Some(self);
        }
        self.children
            .iter()
            .find_map(|child| child.find(element_ref))
    }
}

/// A captured window image attached to an observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImageCapture {
    /// MIME type of `base64` payload (`image/png` or `image/jpeg`).
    pub mime_type: String,
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

/// Complete backend observation before service-owned state metadata is added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiSnapshot {
    pub root: RootInfo,
    pub outline: UiNode,
    #[serde(default)]
    pub captured_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageCapture>,
}

/// Observation representation selected by the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObserveMode {
    /// Accessibility structure only; never captures an image.
    #[default]
    Semantic,
    /// Forces image evidence in addition to the semantic outline.
    Visual,
    /// Automatic selection: semantic with image when the backend can provide
    /// it cheaply.
    Fused,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FindRootsRequest {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ObserveRequest {
    #[serde(default)]
    pub mode: ObserveMode,
    /// Force image capture on/off regardless of mode. `None` lets `mode`
    /// decide (semantic => off, visual/fused => on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_image: Option<bool>,
    /// Downscale the captured image so its longest edge is at most this many
    /// pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_dimension: Option<u32>,
}

/// A checked action submitted against an element in a captured state.
///
/// `element_ref` values are observation-scoped `@eN` refs. `wire_ref` is
/// populated by the service from the stored outline before backend dispatch;
/// backends must not resolve `@eN` refs themselves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    Press {
        #[serde(rename = "ref")]
        element_ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire_ref: Option<String>,
    },
    /// Click an element ref or a raw screen point. Exactly one of (`ref`) or
    /// (`x`, `y`) must be present.
    Click {
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        element_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        click_count: Option<u8>,
    },
    SetText {
        #[serde(rename = "ref")]
        element_ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire_ref: Option<String>,
        text: String,
    },
    /// Types into the element ref, or into the focus established by a
    /// previous click when `ref` is omitted.
    TypeText {
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        element_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire_ref: Option<String>,
        text: String,
    },
    Keypress {
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        element_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire_ref: Option<String>,
        keys: Vec<String>,
    },
    Scroll {
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        element_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire_ref: Option<String>,
        #[serde(default)]
        scroll_x: f64,
        #[serde(default)]
        scroll_y: f64,
    },
    /// Drag through a series of screen points (at least two).
    Drag {
        path: Vec<Point>,
    },
    MoveMouse {
        x: f64,
        y: f64,
    },
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Press { .. } => ActionKind::Press,
            Self::Click { .. } => ActionKind::Click,
            Self::SetText { .. } => ActionKind::SetText,
            Self::TypeText { .. } => ActionKind::TypeText,
            Self::Keypress { .. } => ActionKind::Keypress,
            Self::Scroll { .. } => ActionKind::Scroll,
            Self::Drag { .. } => ActionKind::Drag,
            Self::MoveMouse { .. } => ActionKind::MoveMouse,
        }
    }

    pub fn element_ref(&self) -> Option<&str> {
        match self {
            Self::Press { element_ref, .. } => Some(element_ref),
            Self::Click { element_ref, .. }
            | Self::TypeText { element_ref, .. }
            | Self::Keypress { element_ref, .. }
            | Self::Scroll { element_ref, .. } => element_ref.as_deref(),
            Self::SetText { element_ref, .. } => Some(element_ref),
            Self::Drag { .. } | Self::MoveMouse { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Press,
    Click,
    SetText,
    TypeText,
    Keypress,
    Scroll,
    Drag,
    MoveMouse,
}
