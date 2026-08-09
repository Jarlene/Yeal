//! Native macOS backend over the Accessibility (AX) API.
//!
//! Implements [`UiBackend`] using:
//! - **AX tree**: `objc2-application-services` bindings for
//!   `AXUIElement` attribute/action traversal (`AXUIElementCopyAttributeValue`,
//!   `AXUIElementPerformAction`, `AXUIElementSetAttributeValue`).
//! - **Input**: `objc2-core-graphics` `CGEvent` posting
//!   (`CGEventCreateMouseEvent`, `CGEventCreateKeyboardEvent`,
//!   `CGEventPost`) so keystrokes and pointer events reach the target app
//!   regardless of AX action support.
//! - **Capture**: window image via `xcap` (CGWindowListCreateImage under the
//!   hood), encoded as base64 PNG, downscaled to `max_dimension`.
//!
//! Wire refs are `ax:<seq>`: [`MacosBackend::ref_store`] retains `AXUIElement`
//! values across observations so `act` / `read_text` can re-resolve them.
//! The store is bounded (evict oldest beyond 4096 entries).
//!
//! Reference implementation: `native/macos/bridge.swift` in the
//! pi-computer-use repo (see `/tmp/pi-computer-use` on this machine) — the
//! `look`, `act`, `axReadText`, and `listRoots` command handlers' semantics
//! (role/capability mapping, text ownership, coordinate fallback, keycode
//! mapping) are ported here.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::os::raw::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use image::imageops::FilterType;
use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFEqual, CFNumber, CFRange, CFRetained, CFString, CFType, CGPoint, CGRect,
    CGSize, Type,
};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType,
    CGMouseButton, CGMainDisplayID, CGScrollEventUnit,
};

use crate::backend::{ActOutcome, BackendError, TextPage, UiBackend};
use crate::model::{
    Action, Bounds, FindRootsRequest, ImageCapture, ObserveMode, ObserveRequest, Point, RootInfo,
    ScrollExtent, TextChunk, UiNode, UiSnapshot,
};

/// Upper bound on the wire-ref store; oldest entries are evicted beyond this.
const MAX_REF_STORE: usize = 4096;
/// Total AX-tree nodes kept in one outline.
const MAX_OUTLINE_NODES: usize = 2000;
/// Sibling cap applied to each node's children list.
const MAX_CHILDREN: usize = 30;
/// AX messaging timeout in seconds per attribute call.
const AX_TIMEOUT: f32 = 1.0;
/// Whole-traversal budget; large web views are cut off after this.
const TRAVERSE_DEADLINE: Duration = Duration::from_secs(20);
/// Cap on a single node's string attributes.
const MAX_STRING_CHARS: usize = 32 * 1024;
/// Delay used when re-reading evidence after posting input.
const POST_DELAY: Duration = Duration::from_millis(120);

/// Attribute names (the crate ships no `kAX*` constants).
const KAX_ROLE: &str = "AXRole";
const KAX_SUBROLE: &str = "AXSubrole";
const KAX_TITLE: &str = "AXTitle";
const KAX_DESCRIPTION: &str = "AXDescription";
const KAX_VALUE: &str = "AXValue";
const KAX_IDENTIFIER: &str = "AXIdentifier";
const KAX_CHILDREN: &str = "AXChildren";
const KAX_POSITION: &str = "AXPosition";
const KAX_SIZE: &str = "AXSize";
const KAX_FRAME: &str = "AXFrame";
const KAX_FOCUSED: &str = "AXFocused";
const KAX_MINIMIZED: &str = "AXMinimized";
const KAX_MAIN: &str = "AXMain";
const KAX_MODAL: &str = "AXModal";
const KAX_WINDOWS: &str = "AXWindows";
const KAX_FOCUSED_WINDOW: &str = "AXFocusedWindow";
const KAX_CG_WINDOW_ID: &str = "AXCGWindowID";
const KAX_SELECTED_TEXT: &str = "AXSelectedText";
const KAX_SELECTED_TEXT_RANGE: &str = "AXSelectedTextRange";
const KAX_VISIBLE_CHILDREN: &str = "AXVisibleChildren";
const KAX_VISIBLE_ROWS: &str = "AXVisibleRows";
const KAX_VISIBLE_COLUMNS: &str = "AXVisibleColumns";
const KAX_VISIBLE_CELLS: &str = "AXVisibleCells";
const KAX_ROWS: &str = "AXRows";
const KAX_COLUMNS: &str = "AXColumns";
const KAX_CELLS: &str = "AXCells";
const KAX_VERTICAL_SCROLL_BAR: &str = "AXVerticalScrollBar";
const KAX_HORIZONTAL_SCROLL_BAR: &str = "AXHorizontalScrollBar";
const KAX_PARENT: &str = "AXParent";
const KAX_ENHANCED_USER_INTERFACE: &str = "AXEnhancedUserInterface";
const KAX_MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";

/// Actions used for capability mapping and dispatch.
const AX_PRESS: &str = "AXPress";
const AX_RAISE: &str = "AXRaise";
const AX_SCROLL_DOWN: &str = "AXScrollDown";
const AX_SCROLL_UP: &str = "AXScrollUp";
const AX_SCROLL_LEFT: &str = "AXScrollLeft";
const AX_SCROLL_RIGHT: &str = "AXScrollRight";

/// Roles that own editable text (mirrors pi bridge.swift's `textRoles`).
const TEXT_INPUT_ROLES: &[&str] = &[
    "AXTextField",
    "AXTextArea",
    "AXTextView",
    "AXSearchField",
    "AXComboBox",
    "AXEditableText",
    "AXSecureTextField",
];

/// Roles that can receive keyboard focus (AXFocused settable).
const FOCUSABLE_ROLES: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXMenuItem",
    "AXSlider",
    "AXCell",
    "AXComboBox",
    "AXDisclosureTriangle",
    "AXTabGroup",
    "AXLink",
    "AXColorWell",
];

/// Modifier keycodes used by CGEventCreateKeyboardEvent.
const MOD_SHIFT: u16 = 56;
const MOD_CTRL: u16 = 59;
const MOD_OPTION: u16 = 58;
const MOD_COMMAND: u16 = 55;

/// Options controlling foreground activation and physical input.
#[derive(Debug, Clone, Default)]
pub struct MacosOptions {
    /// When true, never raise apps or post physical input; semantic AX
    /// actions only.
    pub headless: bool,
}

/// macOS Accessibility backend.
pub struct MacosBackend {
    options: MacosOptions,
    // Session-scoped native ref store: "ax:<seq>" -> retained AXUIElement.
    ref_store: tokio::sync::Mutex<std::collections::VecDeque<(String, i64)>>,
    // Monotonic counter for wire-ref / element-ref allocation.
    next_seq: AtomicU64,
}

impl std::fmt::Debug for MacosBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacosBackend")
            .field("headless", &self.options.headless)
            .finish()
    }
}

impl MacosBackend {
    pub fn new(options: MacosOptions) -> Self {
        Self {
            options,
            ref_store: tokio::sync::Mutex::new(std::collections::VecDeque::new()),
            next_seq: AtomicU64::new(1),
        }
    }

    /// AX access must be granted in System Settings → Privacy & Security →
    /// Accessibility; otherwise every entry point fails with a clear message.
    fn check_trusted(&self) -> Result<(), BackendError> {
        // SAFETY: AXIsProcessTrusted is a plain C function with no arguments.
        let trusted = unsafe { AXIsProcessTrusted() };
        if trusted {
            Ok(())
        } else {
            Err(BackendError::Failed(
                "macOS 辅助功能权限未授予：请在“系统设置 → 隐私与安全性 → 辅助功能”中为本进程/终端授权后重试".into(),
            ))
        }
    }

    /// Bump +1 retain on every element referenced by the action batch so the
    /// batch owns its own references even if the store is evicted meanwhile.
    async fn resolve_action_refs(&self, actions: &[Action]) -> HashMap<String, i64> {
        let mut needed: HashSet<String> = HashSet::new();
        for action in actions {
            let wire_ref = match action {
                Action::Press { wire_ref, .. }
                | Action::Click { wire_ref, .. }
                | Action::SetText { wire_ref, .. }
                | Action::TypeText { wire_ref, .. }
                | Action::Keypress { wire_ref, .. }
                | Action::Scroll { wire_ref, .. } => wire_ref,
                _ => &None,
            };
            if let Some(wire_ref) = wire_ref {
                needed.insert(wire_ref.clone());
            }
        }
        let mut resolved: HashMap<String, i64> = HashMap::new();
        if needed.is_empty() {
            return resolved;
        }
        {
            let store = self.ref_store.lock().await;
            for (wire_ref, ptr) in store.iter() {
                if needed.contains(wire_ref) {
                    // SAFETY: the store holds a +1 retain on `ptr`; retaining
                    // again hands the batch its own reference.
                    let retained = unsafe {
                        CFRetained::<AXUIElement>::retain(NonNull::new(*ptr as *mut AXUIElement).unwrap())
                    };
                    let owned = CFRetained::into_raw(retained).as_ptr() as i64;
                    resolved.insert(wire_ref.clone(), owned);
                    if resolved.len() == needed.len() {
                        break;
                    }
                }
            }
        }
        resolved
    }

    /// Register one RawNode (and its descendants) in the ref store and convert
    /// it into an agent-facing [`UiNode`].
    async fn register_node(&self, raw: &RawNode) -> UiNode {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let wire_ref = format!("ax:{seq}");
        let element_ref = format!("@e{seq}");
        {
            let mut store = self.ref_store.lock().await;
            store.push_back((wire_ref.clone(), raw.ptr));
            while store.len() > MAX_REF_STORE {
                if let Some((_, evicted)) = store.pop_front() {
                    release_element(evicted);
                }
            }
        }
        let mut node = UiNode {
            element_ref,
            wire_ref: Some(wire_ref),
            role: raw.role.clone(),
            subrole: raw.subrole.clone(),
            identifier: raw.identifier.clone(),
            title: raw.title.clone(),
            description: raw.description.clone(),
            value: raw.value.clone(),
            actions: raw.actions.clone(),
            can_press: raw.can_press,
            can_focus: raw.can_focus,
            can_set_value: raw.can_set_value,
            can_scroll: raw.can_scroll,
            can_increment: raw.can_increment,
            can_decrement: raw.can_decrement,
            is_text_input: raw.is_text_input,
            bounds: raw.bounds,
            focused: raw.focused,
            offscreen: raw.offscreen,
            picture_only: raw.picture_only,
            truncated: raw.truncated,
            scroll_extent: raw.scroll_extent,
            text: raw
                .text
                .iter()
                .map(|(string, rect)| TextChunk {
                    string: string.clone(),
                    confidence: 1.0,
                    rect: *rect,
                })
                .collect(),
            children: Vec::with_capacity(raw.children.len()),
        };
        for child in &raw.children {
            let child_node = Box::pin(self.register_node(child)).await;
            node.children.push(child_node);
        }
        node
    }
}

#[async_trait]
impl UiBackend for MacosBackend {
    async fn find_roots(&self, request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
        self.check_trusted()?;
        let roots = tokio::task::spawn_blocking(move || enumerate_roots(&request))
            .await
            .map_err(|error| BackendError::Failed(format!("find_roots 任务失败: {error}")))??;
        Ok(roots)
    }

    async fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError> {
        self.check_trusted()?;
        let pid = root
            .pid
            .unwrap_or_else(|| parse_pid_from_resource_key(&root.resource_key));
        let window_id = root.window_id;
        let include_image = request
            .include_image
            .unwrap_or(matches!(request.mode, ObserveMode::Visual | ObserveMode::Fused));
        let max_dimension = request.max_dimension;
        let (raw_root, window_frame) = tokio::task::spawn_blocking(move || {
            build_ax_outline_raw(pid, window_id)
        })
        .await
        .map_err(|error| BackendError::Failed(format!("observe 任务失败: {error}")))??;
        let outline = self.register_node(&raw_root).await;
        let mut snapshot_root = root.clone();
        if let Some(frame) = window_frame {
            snapshot_root.frame = Some(frame);
        }
        let image = if include_image {
            tokio::task::spawn_blocking(move || {
                capture_window_image_sync(window_id, max_dimension)
            })
            .await
            .map_err(|error| BackendError::Failed(format!("截图任务失败: {error}")))?
        } else {
            None
        };
        Ok(UiSnapshot {
            root: snapshot_root,
            outline,
            captured_at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
            image,
        })
    }

    async fn act(
        &self,
        root: &RootInfo,
        actions: &[Action],
    ) -> Result<Vec<ActOutcome>, BackendError> {
        self.check_trusted()?;
        let resolved = self.resolve_action_refs(actions).await;
        let pid = root
            .pid
            .unwrap_or_else(|| parse_pid_from_resource_key(&root.resource_key));
        let window_id = root.window_id;
        let headless = self.options.headless;
        let actions = actions.to_vec();
        let outcomes = tokio::task::spawn_blocking(move || {
            let mut elements: HashMap<String, CFRetained<AXUIElement>> = HashMap::new();
            for (wire_ref, ptr) in resolved {
                // SAFETY: `ptr` carries a +1 retain transferred from
                // resolve_action_refs; the CFRetained owns it and releases it
                // when this closure (and the map) is dropped.
                let element = unsafe {
                    CFRetained::<AXUIElement>::from_raw(
                        NonNull::new(ptr as *mut AXUIElement).unwrap(),
                    )
                };
                elements.insert(wire_ref, element);
            }
            let mut outcomes = Vec::with_capacity(actions.len());
            for action in &actions {
                outcomes.push(dispatch_action(action, &elements, pid, window_id, headless));
            }
            outcomes
        })
        .await
        .map_err(|error| BackendError::Failed(format!("act 任务失败: {error}")))?;
        Ok(outcomes)
    }

    async fn read_text(
        &self,
        _root: &RootInfo,
        wire_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TextPage, BackendError> {
        self.check_trusted()?;
        let retained = {
            let store = self.ref_store.lock().await;
            store
                .iter()
                .find(|(key, _)| key == wire_ref)
                .map(|(_, ptr)| {
                    // SAFETY: the store holds a +1 retain on `ptr`; retaining
                    // again keeps the element alive for the duration of the read.
                    unsafe {
                        CFRetained::<AXUIElement>::retain(
                            NonNull::new(*ptr as *mut AXUIElement).unwrap(),
                        )
                    }
                })
        };
        let Some(retained) = retained else {
            return Err(BackendError::Failed(format!(
                "未知的 wire_ref: {wire_ref}（元素可能已被驱逐）"
            )));
        };
        // No awaits below: the retained element must not cross an await point.
        let element: &AXUIElement = &retained;
        let text = match ax_string(element, KAX_VALUE) {
            Some(text) if !text.is_empty() => text,
            _ => ax_string(element, KAX_TITLE).unwrap_or_default(),
        };
        let total_chars = text.chars().count();
        let offset = offset.min(total_chars);
        let page: String = text.chars().skip(offset).take(limit).collect();
        Ok(TextPage {
            text: page,
            offset,
            limit,
            total_chars,
            has_more: offset + limit < total_chars,
        })
    }
}

/// ---------------------------------------------------------------------------
/// AX attribute helpers
/// ---------------------------------------------------------------------------

/// Read one attribute; returns `None` when unsupported or on AX error.
fn ax_attr(element: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    let attribute = CFString::from_str(name);
    let mut value: *const CFType = std::ptr::null();
    // SAFETY: `value` is a valid out-parameter; on success AX returns a +1
    // retained CFType that we hand to CFRetained::from_raw below.
    let error = unsafe {
        element.copy_attribute_value(
            &attribute,
            NonNull::new(&mut value as *mut *const CFType).unwrap(),
        )
    };
    if error != AXError::Success {
        return None;
    }
    if value.is_null() {
        return None;
    }
    // SAFETY: the +1 retain from the copy rule is transferred into the
    // CFRetained, which releases it when dropped.
    Some(unsafe {
        CFRetained::<CFType>::from_raw(NonNull::new(value as *mut CFType).unwrap())
    })
}

fn ax_string(element: &AXUIElement, name: &str) -> Option<String> {
    let value = ax_attr(element, name)?;
    let string = value.downcast_ref::<CFString>()?;
    Some(string.to_string())
}

fn ax_bool(element: &AXUIElement, name: &str) -> Option<bool> {
    let value = ax_attr(element, name)?;
    if let Some(boolean) = value.downcast_ref::<CFBoolean>() {
        return Some(boolean.as_bool());
    }
    if let Some(number) = value.downcast_ref::<CFNumber>() {
        return Some(number.as_i64()? != 0);
    }
    if let Some(string) = value.downcast_ref::<CFString>() {
        let string = string.to_string();
        if string.eq_ignore_ascii_case("true") {
            return Some(true);
        }
        if string.eq_ignore_ascii_case("false") {
            return Some(false);
        }
    }
    None
}

fn ax_i64(element: &AXUIElement, name: &str) -> Option<i64> {
    let value = ax_attr(element, name)?;
    let number = value.downcast_ref::<CFNumber>()?;
    number.as_i64()
}

fn ax_number(element: &AXUIElement, name: &str) -> Option<f64> {
    let value = ax_attr(element, name)?;
    let number = value.downcast_ref::<CFNumber>()?;
    number.as_f64()
}

fn ax_is_settable(element: &AXUIElement, name: &str) -> bool {
    let attribute = CFString::from_str(name);
    let mut settable: u8 = 0;
    // SAFETY: `settable` is a valid out-parameter.
    let error = unsafe {
        element.is_attribute_settable(&attribute, NonNull::new(&mut settable as *mut u8).unwrap())
    };
    error == AXError::Success && settable != 0
}

fn ax_element(element: &AXUIElement, name: &str) -> Option<CFRetained<AXUIElement>> {
    let value = ax_attr(element, name)?;
    let ax = value.downcast_ref::<AXUIElement>()?;
    Some(ax.retain())
}

fn ax_element_array(element: &AXUIElement, name: &str) -> Option<CFRetained<CFArray>> {
    let value = ax_attr(element, name)?;
    let array = value.downcast_ref::<CFArray>()?;
    Some(array.retain())
}

/// Extract every element of an array attribute (e.g. AXWindows) as owned,
/// retained AXUIElement values.
fn ax_elements(element: &AXUIElement, name: &str) -> Vec<CFRetained<AXUIElement>> {
    let Some(array) = ax_element_array(element, name) else {
        return Vec::new();
    };
    let count = array.count();
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for index in 0..count.max(0) {
        // SAFETY: `index` is within bounds; the element pointer is retained by
        // the array, and CFRetained::retain gives each returned value its own
        // reference.
        let ptr = unsafe { array.value_at_index(index) };
        if ptr.is_null() {
            continue;
        }
        let element = unsafe {
            CFRetained::<AXUIElement>::retain(NonNull::new(ptr as *mut AXUIElement).unwrap())
        };
        out.push(element);
    }
    out
}

fn ax_action_names(element: &AXUIElement) -> Vec<String> {
    let mut names: *const CFArray = std::ptr::null();
    // SAFETY: `names` is a valid out-parameter; on success the array is +1
    // retained and transferred to CFRetained below.
    let error = unsafe {
        element.copy_action_names(NonNull::new(&mut names as *mut *const CFArray).unwrap())
    };
    if error != AXError::Success || names.is_null() {
        return Vec::new();
    }
    // SAFETY: the +1 retain from the copy rule is owned by the CFRetained.
    let array = unsafe { CFRetained::<CFArray>::from_raw(NonNull::new(names as *mut CFArray).unwrap()) };
    let count = array.count();
    let mut out = Vec::new();
    for index in 0..count.max(0) {
        // SAFETY: `index` is within bounds (we iterate over `count`); the
        // returned pointer is retained by the array and only borrowed here.
        let ptr = unsafe { array.value_at_index(index) };
        if ptr.is_null() {
            continue;
        }
        // SAFETY: array elements are CFStringRef action names retained by the
        // array; CFRetained::retain keeps each one alive for `out`.
        let string = unsafe {
            CFRetained::<CFString>::retain(NonNull::new(ptr as *mut CFString).unwrap())
        };
        out.push(string.to_string());
    }
    out
}

fn ax_point(element: &AXUIElement) -> Option<CGPoint> {
    let value = ax_attr(element, KAX_POSITION)?;
    let ax = value.downcast_ref::<AXValue>()?;
    let mut point = CGPoint::default();
    // SAFETY: `point` is a valid out-parameter for AXValueGetValue(CGPoint).
    let ok = unsafe {
        ax.value(
            AXValueType::CGPoint,
            NonNull::new(&mut point as *mut CGPoint as *mut c_void).unwrap(),
        )
    };
    ok.then_some(point)
}

fn ax_size(element: &AXUIElement) -> Option<CGSize> {
    let value = ax_attr(element, KAX_SIZE)?;
    let ax = value.downcast_ref::<AXValue>()?;
    let mut size = CGSize::default();
    // SAFETY: `size` is a valid out-parameter for AXValueGetValue(CGSize).
    let ok = unsafe {
        ax.value(
            AXValueType::CGSize,
            NonNull::new(&mut size as *mut CGSize as *mut c_void).unwrap(),
        )
    };
    ok.then_some(size)
}

fn ax_frame_rect(element: &AXUIElement) -> Option<CGRect> {
    let value = ax_attr(element, KAX_FRAME)?;
    let ax = value.downcast_ref::<AXValue>()?;
    let mut frame = CGRect::default();
    // SAFETY: `frame` is a valid out-parameter for AXValueGetValue(CGRect).
    let ok = unsafe {
        ax.value(
            AXValueType::CGRect,
            NonNull::new(&mut frame as *mut CGRect as *mut c_void).unwrap(),
        )
    };
    ok.then_some(frame)
}

/// Element frame in top-left-origin screen coordinates; prefers AXFrame and
/// falls back to AXPosition + AXSize (mirrors bridge.swift).
fn element_frame(element: &AXUIElement) -> Option<CGRect> {
    if let Some(frame) = ax_frame_rect(element) {
        return Some(frame);
    }
    let point = ax_point(element)?;
    let size = ax_size(element)?;
    Some(CGRect {
        origin: point,
        size,
    })
}

fn element_center(element: &AXUIElement) -> Option<CGPoint> {
    let frame = element_frame(element)?;
    Some(CGPoint {
        x: frame.origin.x + frame.size.width / 2.0,
        y: frame.origin.y + frame.size.height / 2.0,
    })
}

fn same_element(left: &AXUIElement, right: &AXUIElement) -> bool {
    CFEqual(Some(&**left), Some(&**right))
}

/// Release a retained element owned by the ref store.
fn release_element(ptr: i64) {
    // SAFETY: the store held a +1 retain on `ptr` (from CFRetain at
    // registration); converting it back into a CFRetained transfers that
    // ownership and the drop releases it.
    unsafe {
        drop(CFRetained::<AXUIElement>::from_raw(
            NonNull::new(ptr as *mut AXUIElement).unwrap(),
        ));
    }
}

fn enable_enhanced_ui(app: &AXUIElement) {
    // SAFETY: AXUIElementSetAttributeValue is a plain AX IPC call; these
    // hints are best-effort and errors are ignored.
    unsafe {
        let _ = app.set_attribute_value(
            &CFString::from_static_str(KAX_ENHANCED_USER_INTERFACE),
            CFBoolean::new(true),
        );
        let _ = app.set_attribute_value(
            &CFString::from_static_str(KAX_MANUAL_ACCESSIBILITY),
            CFBoolean::new(true),
        );
    }
}

/// ---------------------------------------------------------------------------
/// Role normalization / capability mapping
/// ---------------------------------------------------------------------------

fn normalize_role(role: &str) -> String {
    role.strip_prefix("AX").unwrap_or(role).to_lowercase()
}

fn is_text_input_role(role: &str) -> bool {
    TEXT_INPUT_ROLES.iter().any(|candidate| *candidate == role)
}

fn root_kind(role: &str, subrole: &str) -> String {
    match role {
        "AXSheet" => "sheet".into(),
        "AXDialog" => "dialog".into(),
        "AXMenu" => "menu".into(),
        _ if subrole == "AXPopover" => "popover".into(),
        _ => "window".into(),
    }
}

fn truncate_chars(mut text: String, max: usize) -> String {
    if text.chars().count() > max {
        text = text.chars().take(max).collect();
    }
    text
}

/// ---------------------------------------------------------------------------
/// find_roots
/// ---------------------------------------------------------------------------

fn enumerate_roots(request: &FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
    let windows = xcap::Window::all()
        .map_err(|error| BackendError::Failed(format!("无法枚举窗口: {error}")))?;
    let mut by_pid: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (index, window) in windows.iter().enumerate() {
        if let Ok(pid) = window.pid() {
            by_pid.entry(pid).or_default().push(index);
        }
    }
    let mut roots = Vec::new();
    for (pid, indices) in by_pid {
        // SAFETY: AXUIElement::new_application only requires a valid pid.
        let app = unsafe { AXUIElement::new_application(pid as i32) };
        // SAFETY: set_messaging_timeout is an AX IPC call with no thread affinity.
        let _ = unsafe { app.set_messaging_timeout(AX_TIMEOUT) };
        enable_enhanced_ui(&app);
        let app_title = ax_string(&app, KAX_TITLE).unwrap_or_default();
        let ax_windows = ax_elements(&app, KAX_WINDOWS);
        let focused_window = ax_element(&app, KAX_FOCUSED_WINDOW);
        for &index in &indices {
            let window = &windows[index];
            let Some(cgid) = window.id().ok() else { continue };
            let title = window.title().unwrap_or_default();
            let Some(ax_window) = match_ax_window(&ax_windows, cgid, window) else {
                continue;
            };
            let role = ax_string(&ax_window, KAX_ROLE).unwrap_or_else(|| "AXWindow".into());
            let subrole = ax_string(&ax_window, KAX_SUBROLE).unwrap_or_default();
            let frame = element_frame(&ax_window).map(|frame| rect_to_bounds(&frame));
            let scale_factor = window
                .current_monitor()
                .ok()
                .and_then(|monitor| monitor.scale_factor().ok())
                .unwrap_or(1.0) as f64;
            let is_focused = focused_window
                .as_ref()
                .is_some_and(|focused| same_element(&ax_window, focused))
                || window.is_focused().unwrap_or(false);
            let is_minimized = ax_bool(&ax_window, KAX_MINIMIZED).unwrap_or(false)
                || window.is_minimized().unwrap_or(false);
            let root = RootInfo {
                root_ref: String::new(),
                resource_key: format!("desktop-pid:{pid}"),
                kind: root_kind(&role, &subrole),
                title: ax_string(&ax_window, KAX_TITLE).unwrap_or(title),
                app: Some(if app_title.is_empty() {
                    window.app_name().unwrap_or_default()
                } else {
                    app_title.clone()
                }),
                bundle_id: None,
                pid: Some(pid),
                window_id: Some(cgid as i64),
                role: Some(normalize_role(&role)),
                subrole: Some(normalize_role(&subrole)),
                z_order: index as i64,
                frame,
                scale_factor,
                is_onscreen: true,
                is_focused,
                is_minimized,
                is_main: ax_bool(&ax_window, KAX_MAIN).unwrap_or(false),
                is_modal: ax_bool(&ax_window, KAX_MODAL).unwrap_or(false),
            };
            roots.push(root);
        }
    }
    Ok(roots
        .into_iter()
        .filter(|root| matches_request(root, request))
        .collect())
}

fn match_ax_window(
    windows: &[CFRetained<AXUIElement>],
    cgid: u32,
    xcap_window: &xcap::Window,
) -> Option<CFRetained<AXUIElement>> {
    for window in windows {
        if ax_i64(window, KAX_CG_WINDOW_ID) == Some(cgid as i64) {
            return Some(window.retain());
        }
    }
    // Fallback: pick the AX window with the largest intersection against the
    // xcap frame (converted from CG bottom-left to top-left coordinates).
    let screen_height = main_display_height();
    let x = xcap_window.x().unwrap_or(0) as f64;
    let y = screen_height
        - (xcap_window.y().unwrap_or(0) as f64)
        - (xcap_window.height().unwrap_or(0) as f64);
    let width = xcap_window.width().unwrap_or(0) as f64;
    let height = xcap_window.height().unwrap_or(0) as f64;
    let mut best: Option<(f64, CFRetained<AXUIElement>)> = None;
    for window in windows {
        let Some(frame) = element_frame(window) else { continue };
        let overlap_x =
            (frame.origin.x + frame.size.width).min(x + width) - frame.origin.x.max(x);
        let overlap_y =
            (frame.origin.y + frame.size.height).min(y + height) - frame.origin.y.max(y);
        if overlap_x > 0.0 && overlap_y > 0.0 {
            let area = overlap_x * overlap_y;
            if best.as_ref().is_none_or(|(best_area, _)| area > *best_area) {
                best = Some((area, window.retain()));
            }
        }
    }
    best.map(|(_, window)| window)
}

fn matches_request(root: &RootInfo, request: &FindRootsRequest) -> bool {
    request
        .text
        .as_ref()
        .is_none_or(|text| root.title.to_lowercase().contains(&text.to_lowercase()))
        && request
            .app
            .as_ref()
            .is_none_or(|app| root.app.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(app)))
        && request
            .bundle_id
            .as_ref()
            .is_none_or(|bundle| root.bundle_id.as_deref() == Some(bundle.as_str()))
        && request.pid.is_none_or(|pid| root.pid == Some(pid))
        && request
            .kind
            .as_ref()
            .is_none_or(|kind| root.kind.eq_ignore_ascii_case(kind))
}

fn parse_pid_from_resource_key(resource_key: &str) -> u32 {
    resource_key
        .strip_prefix("desktop-pid:")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or(0)
}

fn rect_to_bounds(frame: &CGRect) -> Bounds {
    Bounds {
        x: frame.origin.x,
        y: frame.origin.y,
        w: frame.size.width,
        h: frame.size.height,
    }
}

fn main_display_height() -> f64 {
    CGDisplayBounds(CGMainDisplayID()).size.height
}

/// ---------------------------------------------------------------------------
/// observe: AX tree traversal -> outline
/// ---------------------------------------------------------------------------

/// Send-only intermediate node: native handles are i64 pointers retained for
/// the ref store; all AX objects stay inside the traversal closure.
#[derive(Debug, Default, Clone)]
struct RawNode {
    /// Retained AXUIElement pointer (+1 owned by the ref store after
    /// registration).
    ptr: i64,
    role: String,
    subrole: String,
    identifier: String,
    title: String,
    description: String,
    value: String,
    actions: Vec<String>,
    can_press: bool,
    can_focus: bool,
    can_set_value: bool,
    can_scroll: bool,
    can_increment: bool,
    can_decrement: bool,
    is_text_input: bool,
    focused: bool,
    offscreen: bool,
    picture_only: bool,
    truncated: bool,
    bounds: Option<Bounds>,
    scroll_extent: Option<ScrollExtent>,
    /// (string, window-relative rect).
    text: Vec<(String, Option<Bounds>)>,
    children: Vec<RawNode>,
}

fn build_ax_outline_raw(
    pid: u32,
    window_id: Option<i64>,
) -> Result<(RawNode, Option<Bounds>), BackendError> {
    let deadline = Instant::now() + TRAVERSE_DEADLINE;
    // SAFETY: AXUIElement::new_application only requires a valid pid.
    let app = unsafe { AXUIElement::new_application(pid as i32) };
    // SAFETY: set_messaging_timeout is an AX IPC call with no thread affinity.
    let _ = unsafe { app.set_messaging_timeout(AX_TIMEOUT) };
    enable_enhanced_ui(&app);

    let windows = ax_elements(&app, KAX_WINDOWS);
    let focused = ax_element(&app, KAX_FOCUSED_WINDOW);
    let window = find_window_element(&windows, window_id).or(focused);
    let Some(window) = window else {
        return Err(BackendError::Failed(format!(
            "进程 {pid} 没有可观察的窗口（可能已退出或未暴露 AX 窗口）"
        )));
    };
    let window_frame = element_frame(&window).unwrap_or_default();
    let origin = (window_frame.origin.x, window_frame.origin.y);

    let mut arena: Vec<RawNode> = Vec::new();
    let mut children_of: Vec<Vec<usize>> = Vec::new();
    let mut visited: HashSet<usize> = HashSet::new();
    let mut queue: VecDeque<(CFRetained<AXUIElement>, usize, bool)> = VecDeque::new();

    visited.insert(CFRetained::as_ptr(&window).as_ptr() as usize);
    queue.push_back((window, usize::MAX, false));

    while let Some((element, parent, offscreen)) = queue.pop_front() {
        if arena.len() >= MAX_OUTLINE_NODES || Instant::now() > deadline {
            if let Some(parent_node) = arena.get_mut(parent) {
                parent_node.truncated = true;
            }
            continue;
        }
        let mut node = build_ax_node(&element, offscreen, origin);
        let raw_children = ax_element_array(&element, KAX_CHILDREN);
        let visible = if matches!(
            node.role.as_str(),
            "scrollarea" | "table" | "outline" | "list" | "webarea" | "sheet" | "popover"
                | "group" | "window"
        ) {
            visible_addresses(&element)
        } else {
            HashSet::new()
        };
        // Transfer the traversal's +1 retain on `element` to the ref store
        // (via node.ptr); the queue no longer owns it.
        node.ptr = CFRetained::into_raw(element).as_ptr() as i64;
        let index = arena.len();
        arena.push(node);
        children_of.push(Vec::new());

        let mut enqueued = 0usize;
        let mut truncated = false;
        if let Some(array) = &raw_children {
            let count = array.count();
            for array_index in 0..count.max(0) {
                if enqueued >= MAX_CHILDREN {
                    truncated = true;
                    break;
                }
                // SAFETY: `array_index` is within bounds; the element pointer
                // is retained by the array while we bump our own reference.
                let ptr = unsafe { array.value_at_index(array_index) };
                if ptr.is_null() {
                    continue;
                }
                let address = ptr as usize;
                if visited.contains(&address) {
                    continue;
                }
                visited.insert(address);
                // SAFETY: kAXChildren elements are AXUIElementRefs retained by
                // the array; CFRetained::retain keeps each queued child alive.
                let child = unsafe {
                    CFRetained::<AXUIElement>::retain(
                        NonNull::new(ptr as *mut AXUIElement).unwrap(),
                    )
                };
                let child_offscreen =
                    offscreen || (!visible.is_empty() && !visible.contains(&address));
                queue.push_back((child, index, child_offscreen));
                enqueued += 1;
            }
        }
        if truncated {
            if let Some(node) = arena.get_mut(index) {
                node.truncated = true;
            }
        }
        if parent != usize::MAX {
            children_of[parent].push(index);
        }
    }

    if arena.is_empty() {
        return Err(BackendError::Failed("AX 树为空".into()));
    }
    let root = assemble_tree(arena, children_of);
    Ok((root, Some(rect_to_bounds(&window_frame))))
}

fn build_ax_node(element: &AXUIElement, offscreen: bool, origin: (f64, f64)) -> RawNode {
    let role = ax_string(element, KAX_ROLE).unwrap_or_default();
    let subrole = ax_string(element, KAX_SUBROLE).unwrap_or_default();
    let title = truncate_chars(
        ax_string(element, KAX_TITLE).unwrap_or_default(),
        MAX_STRING_CHARS,
    );
    let description = truncate_chars(
        ax_string(element, KAX_DESCRIPTION).unwrap_or_default(),
        MAX_STRING_CHARS,
    );
    let identifier = truncate_chars(
        ax_string(element, KAX_IDENTIFIER).unwrap_or_default(),
        MAX_STRING_CHARS,
    );
    let actions = ax_action_names(element);
    let raw_value = ax_string(element, KAX_VALUE).unwrap_or_default();
    let is_text_input = is_text_input_role(&role);
    // Secure fields must never leak their content.
    let value = if role == "AXSecureTextField" {
        String::new()
    } else {
        truncate_chars(raw_value.clone(), MAX_STRING_CHARS)
    };
    let focused = ax_bool(element, KAX_FOCUSED).unwrap_or(false);
    let can_press = actions.iter().any(|action| action == AX_PRESS);
    let can_scroll = actions.iter().any(|action| {
        matches!(
            action.as_str(),
            AX_SCROLL_DOWN | AX_SCROLL_UP | AX_SCROLL_LEFT | AX_SCROLL_RIGHT
        )
    });
    let can_increment = actions.iter().any(|action| action == "AXIncrement");
    let can_decrement = actions.iter().any(|action| action == "AXDecrement");
    let can_set_value = if is_text_input {
        true
    } else {
        ax_is_settable(element, KAX_VALUE)
    };
    let can_focus = if is_text_input || FOCUSABLE_ROLES.iter().any(|candidate| *candidate == role)
    {
        ax_is_settable(element, KAX_FOCUSED)
    } else {
        focused
    };
    let frame = element_frame(element);
    let bounds = frame.map(|frame| Bounds {
        x: (frame.origin.x - origin.0).max(0.0),
        y: (frame.origin.y - origin.1).max(0.0),
        w: frame.size.width.max(0.0),
        h: frame.size.height.max(0.0),
    });
    let picture_only = role == "AXImage" && title.is_empty() && description.is_empty();
    let scroll_extent = if matches!(
        role.as_str(),
        "AXScrollArea" | "AXTable" | "AXOutline" | "AXList" | "AXWebArea" | "AXSheet"
    ) {
        scroll_extent_of(element)
    } else {
        None
    };
    let text = if is_text_input || role == "AXStaticText" {
        if role == "AXSecureTextField" {
            Vec::new()
        } else {
            vec![(value.clone(), bounds)]
        }
    } else {
        Vec::new()
    };
    RawNode {
        ptr: 0,
        role: normalize_role(&role),
        subrole: normalize_role(&subrole),
        identifier,
        title,
        description,
        value,
        actions,
        can_press,
        can_focus,
        can_set_value,
        can_scroll,
        can_increment,
        can_decrement,
        is_text_input,
        focused,
        offscreen,
        picture_only,
        truncated: false,
        bounds,
        scroll_extent,
        text,
        children: Vec::new(),
    }
}

fn visible_addresses(element: &AXUIElement) -> HashSet<usize> {
    let mut out = HashSet::new();
    for name in [
        KAX_VISIBLE_ROWS,
        KAX_VISIBLE_COLUMNS,
        KAX_VISIBLE_CELLS,
        KAX_VISIBLE_CHILDREN,
    ] {
        if let Some(array) = ax_element_array(element, name) {
            let count = array.count();
            for index in 0..count.max(0) {
                // SAFETY: `index` is within bounds; the pointer is only used
                // as an identity address while the array is alive.
                let ptr = unsafe { array.value_at_index(index) };
                if !ptr.is_null() {
                    out.insert(ptr as usize);
                }
            }
        }
    }
    out
}

fn scroll_extent_of(element: &AXUIElement) -> Option<ScrollExtent> {
    if let (Some(seen), Some(total)) = (
        ax_i64(element, KAX_VISIBLE_ROWS),
        ax_i64(element, KAX_ROWS),
    ) {
        return Some(ScrollExtent {
            seen: seen.max(0) as u64,
            total: total.max(0) as u64,
        });
    }
    if let (Some(seen), Some(total)) = (
        ax_i64(element, KAX_VISIBLE_COLUMNS),
        ax_i64(element, KAX_COLUMNS),
    ) {
        return Some(ScrollExtent {
            seen: seen.max(0) as u64,
            total: total.max(0) as u64,
        });
    }
    if let (Some(seen), Some(total)) = (
        ax_i64(element, KAX_VISIBLE_CELLS),
        ax_i64(element, KAX_CELLS),
    ) {
        return Some(ScrollExtent {
            seen: seen.max(0) as u64,
            total: total.max(0) as u64,
        });
    }
    None
}

fn find_window_element(
    windows: &[CFRetained<AXUIElement>],
    window_id: Option<i64>,
) -> Option<CFRetained<AXUIElement>> {
    if let Some(id) = window_id {
        for window in windows {
            if ax_i64(window, KAX_CG_WINDOW_ID) == Some(id) {
                return Some(window.retain());
            }
        }
    }
    windows.first().map(|window| window.retain())
}

/// Reassemble the arena (parents always have smaller indices than children).
fn assemble_tree(arena: Vec<RawNode>, children_of: Vec<Vec<usize>>) -> RawNode {
    let mut built: Vec<Option<RawNode>> = (0..arena.len()).map(|_| None).collect();
    for index in (0..arena.len()).rev() {
        let mut node = arena[index].clone();
        node.children.reserve(children_of[index].len());
        for &child in &children_of[index] {
            if let Some(child_node) = built[child].take() {
                node.children.push(child_node);
            }
        }
        built[index] = Some(node);
    }
    built[0].take().unwrap_or_default()
}

/// ---------------------------------------------------------------------------
/// Image capture
/// ---------------------------------------------------------------------------

fn capture_window_image_sync(
    window_id: Option<i64>,
    max_dimension: Option<u32>,
) -> Option<ImageCapture> {
    let window_id = window_id?;
    let windows = xcap::Window::all().ok()?;
    let window = windows
        .iter()
        .find(|candidate| candidate.id().ok() == Some(window_id as u32))?;
    let image = window.capture_image().ok()?;
    let mut dynamic = image::DynamicImage::ImageRgba8(image);
    let (width, height) = (dynamic.width(), dynamic.height());
    if let Some(max_dimension) = max_dimension.filter(|max| *max > 0) {
        let longest = width.max(height);
        if longest > max_dimension {
            let scale = max_dimension as f64 / longest as f64;
            let new_width = ((width as f64 * scale).round() as u32).max(1);
            let new_height = ((height as f64 * scale).round() as u32).max(1);
            dynamic = dynamic.resize(new_width, new_height, FilterType::Triangle);
        }
    }
    let mut buffer = Vec::new();
    dynamic
        .write_to(&mut Cursor::new(&mut buffer), image::ImageFormat::Png)
        .ok()?;
    Some(ImageCapture {
        mime_type: "image/png".into(),
        base64: STANDARD.encode(&buffer),
        width: dynamic.width(),
        height: dynamic.height(),
    })
}

/// ---------------------------------------------------------------------------
/// act
/// ---------------------------------------------------------------------------

fn element_for<'a>(
    elements: &'a HashMap<String, CFRetained<AXUIElement>>,
    wire_ref: Option<&str>,
) -> Option<&'a AXUIElement> {
    wire_ref
        .and_then(|wire_ref| elements.get(wire_ref))
        .map(|element| &**element)
}

fn dispatch_action(
    action: &Action,
    elements: &HashMap<String, CFRetained<AXUIElement>>,
    pid: u32,
    window_id: Option<i64>,
    headless: bool,
) -> ActOutcome {
    match action {
        Action::Press { wire_ref, .. } => {
            let element = element_for(elements, wire_ref.as_deref());
            match element {
                Some(element) => press_outcome(element),
                None => ActOutcome::Didnt,
            }
        }
        Action::Click {
            wire_ref,
            x,
            y,
            button,
            click_count,
            ..
        } => {
            let element = element_for(elements, wire_ref.as_deref());
            click_outcome(
                element,
                *x,
                *y,
                button.as_deref(),
                click_count.unwrap_or(1),
                pid,
                window_id,
                headless,
            )
        }
        Action::SetText { wire_ref, text, .. } => {
            let element = element_for(elements, wire_ref.as_deref());
            match element {
                Some(element) => set_text_outcome(element, text, headless),
                None => ActOutcome::Didnt,
            }
        }
        Action::TypeText { wire_ref, text, .. } => {
            let element = element_for(elements, wire_ref.as_deref());
            type_text_outcome(element, text, pid, window_id, headless)
        }
        Action::Keypress { wire_ref, keys, .. } => {
            let element = element_for(elements, wire_ref.as_deref());
            keypress_outcome(element, keys, headless)
        }
        Action::Scroll {
            wire_ref,
            scroll_x,
            scroll_y,
            ..
        } => {
            let element = element_for(elements, wire_ref.as_deref());
            scroll_outcome(element, *scroll_x, *scroll_y, pid, window_id, headless)
        }
        Action::Drag { path } => drag_outcome(path, pid, window_id, headless),
        Action::MoveMouse { x, y } => move_mouse_outcome(*x, *y, headless),
    }
}

/// Readable evidence (value + selected text) used to verify an action's
/// effect. Returns `None` when nothing is readable.
fn element_evidence(element: &AXUIElement) -> Option<String> {
    let value = raw_value_string(element);
    let selected = ax_string(element, KAX_SELECTED_TEXT).unwrap_or_default();
    if value.is_empty() && selected.is_empty() {
        None
    } else {
        Some(format!("v={value}|s={selected}"))
    }
}

fn raw_value_string(element: &AXUIElement) -> String {
    match ax_attr(element, KAX_VALUE) {
        Some(value) => {
            if let Some(string) = value.downcast_ref::<CFString>() {
                return string.to_string();
            }
            if let Some(boolean) = value.downcast_ref::<CFBoolean>() {
                return boolean.as_bool().to_string();
            }
            if let Some(number) = value.downcast_ref::<CFNumber>() {
                return number.as_f64().map(|value| format!("{value}")).unwrap_or_default();
            }
            String::new()
        }
        None => String::new(),
    }
}

fn press_outcome(element: &AXUIElement) -> ActOutcome {
    let before = element_evidence(element);
    let action = CFString::from_static_str(AX_PRESS);
    // SAFETY: AXUIElementPerformAction is a plain AX IPC call.
    let error = unsafe { element.perform_action(&action) };
    if error != AXError::Success {
        return ActOutcome::Didnt;
    }
    std::thread::sleep(POST_DELAY);
    let mut after = element_evidence(element);
    if after == before {
        // Retry once after a short settle, then re-read evidence.
        std::thread::sleep(POST_DELAY);
        // SAFETY: second press attempt; safe to retry on most controls.
        let retry = unsafe { element.perform_action(&action) };
        if retry == AXError::Success {
            std::thread::sleep(POST_DELAY);
            after = element_evidence(element);
        }
    }
    if after.is_some() && after != before {
        ActOutcome::Worked
    } else {
        ActOutcome::Unknown
    }
}

fn click_outcome(
    element: Option<&AXUIElement>,
    x: Option<f64>,
    y: Option<f64>,
    button: Option<&str>,
    click_count: u8,
    pid: u32,
    window_id: Option<i64>,
    headless: bool,
) -> ActOutcome {
    let point = element
        .and_then(element_center)
        .or_else(|| x.zip(y).map(|(x, y)| CGPoint { x, y }));
    if headless {
        return match element {
            Some(element) => press_outcome(element),
            None => ActOutcome::Didnt,
        };
    }
    if let Some(element) = element {
        // Prefer AXPress for pressable non-text controls (more reliable than
        // synthesized clicks, and verifiable via evidence).
        if ax_action_names(element).iter().any(|name| name == AX_PRESS)
            && !is_text_input_element(element)
        {
            let outcome = press_outcome(element);
            if outcome != ActOutcome::Didnt {
                return outcome;
            }
            // AXPress failed; fall through to a coordinate click.
        }
        let before = element_evidence(element);
        let Some(point) = point else {
            return ActOutcome::Didnt;
        };
        raise_window(pid, window_id);
        post_mouse_click(point, button, click_count);
        std::thread::sleep(POST_DELAY);
        let after = element_evidence(element);
        if after.is_some() && after != before {
            return ActOutcome::Worked;
        }
        return ActOutcome::Unknown;
    }
    let Some(point) = point else {
        return ActOutcome::Didnt;
    };
    raise_window(pid, window_id);
    post_mouse_click(point, button, click_count);
    ActOutcome::Unknown
}

fn is_text_input_element(element: &AXUIElement) -> bool {
    ax_string(element, KAX_ROLE).is_some_and(|role| is_text_input_role(&role))
}

fn set_text_outcome(element: &AXUIElement, text: &str, headless: bool) -> ActOutcome {
    let attribute = CFString::from_static_str(KAX_VALUE);
    let value = CFString::from_str(text);
    // SAFETY: AXUIElementSetAttributeValue is a plain AX IPC call.
    let error = unsafe { element.set_attribute_value(&attribute, &value) };
    if error == AXError::Success {
        std::thread::sleep(POST_DELAY);
        let actual = raw_value_string(element);
        if actual == text {
            return ActOutcome::Worked;
        }
        return ActOutcome::Didnt;
    }
    if headless {
        return ActOutcome::Didnt;
    }
    // Fallback: focus the field, select-all, then insert the text.
    set_focused(element);
    post_key_chord("cmd+a");
    post_unicode_text(text);
    std::thread::sleep(POST_DELAY);
    let actual = raw_value_string(element);
    if actual == text {
        ActOutcome::Worked
    } else {
        ActOutcome::Didnt
    }
}

fn type_text_outcome(
    element: Option<&AXUIElement>,
    text: &str,
    pid: u32,
    window_id: Option<i64>,
    headless: bool,
) -> ActOutcome {
    let Some(element) = element else {
        if headless {
            return ActOutcome::Didnt;
        }
        raise_window(pid, window_id);
        post_unicode_text(text);
        return ActOutcome::Unknown;
    };
    let before = element_evidence(element);
    if headless {
        // No keystrokes in headless mode: focus and set the value via AX.
        set_focused(element);
        let attribute = CFString::from_static_str(KAX_VALUE);
        let value = CFString::from_str(text);
        // SAFETY: AXUIElementSetAttributeValue is a plain AX IPC call.
        let error = unsafe { element.set_attribute_value(&attribute, &value) };
        if error == AXError::Success {
            std::thread::sleep(POST_DELAY);
            let after = element_evidence(element);
            return if after.is_some() && after != before {
                ActOutcome::Worked
            } else {
                ActOutcome::Unknown
            };
        }
        return ActOutcome::Didnt;
    }
    set_focused(element);
    raise_window(pid, window_id);
    post_unicode_text(text);
    std::thread::sleep(POST_DELAY);
    let after = element_evidence(element);
    match (before, after) {
        (Some(before), Some(after)) if before != after => ActOutcome::Worked,
        (Some(_), Some(_)) => ActOutcome::Didnt,
        _ => ActOutcome::Unknown,
    }
}

fn keypress_outcome(
    element: Option<&AXUIElement>,
    keys: &[String],
    headless: bool,
) -> ActOutcome {
    if is_select_all(keys) {
        if let Some(element) = element {
            return if set_select_all(element) {
                ActOutcome::Worked
            } else {
                ActOutcome::Didnt
            };
        }
        if headless {
            return ActOutcome::Didnt;
        }
        post_key_chord(&keys[0]);
        return ActOutcome::Unknown;
    }
    if headless {
        return ActOutcome::Didnt;
    }
    for key in keys {
        post_key_chord(key);
    }
    ActOutcome::Unknown
}

fn is_select_all(keys: &[String]) -> bool {
    keys.len() == 1 && {
        let parts: Vec<&str> = keys[0].split('+').collect();
        parts.len() == 2
            && matches!(
                parts[0].to_lowercase().as_str(),
                "cmd" | "command" | "meta"
            )
            && parts[1].eq_ignore_ascii_case("a")
    }
}

fn set_select_all(element: &AXUIElement) -> bool {
    let length = ax_string(element, KAX_VALUE)
        .map(|value| value.chars().count())
        .unwrap_or(0);
    let mut range = CFRange {
        location: 0,
        length: length as isize,
    };
    // SAFETY: `range` is a valid value pointer for AXValueCreate.
    let Some(ax_value) = (unsafe {
        AXValue::new(
            AXValueType::CFRange,
            NonNull::new(&mut range as *mut CFRange as *mut c_void).unwrap(),
        )
    }) else {
        return false;
    };
    let attribute = CFString::from_static_str(KAX_SELECTED_TEXT_RANGE);
    // SAFETY: AXUIElementSetAttributeValue is a plain AX IPC call.
    unsafe { element.set_attribute_value(&attribute, &ax_value) == AXError::Success }
}

fn scroll_outcome(
    element: Option<&AXUIElement>,
    scroll_x: f64,
    scroll_y: f64,
    pid: u32,
    window_id: Option<i64>,
    headless: bool,
) -> ActOutcome {
    if let Some(element) = element {
        let before = scroll_signature(element);
        if perform_scroll_action_or_ancestor(element, scroll_x, scroll_y) {
            std::thread::sleep(POST_DELAY);
            let after = scroll_signature(element);
            if after.is_some() && after != before {
                return ActOutcome::Worked;
            }
            return ActOutcome::Unknown;
        }
        if headless {
            return ActOutcome::Didnt;
        }
        let center = element_center(element);
        raise_window(pid, window_id);
        post_scroll_wheel(center, scroll_x, scroll_y);
        std::thread::sleep(POST_DELAY);
        let after = scroll_signature(element);
        if after.is_some() && after != before {
            return ActOutcome::Worked;
        }
        return ActOutcome::Unknown;
    }
    if headless {
        return ActOutcome::Didnt;
    }
    raise_window(pid, window_id);
    post_scroll_wheel(None, scroll_x, scroll_y);
    ActOutcome::Unknown
}

fn scroll_signature(element: &AXUIElement) -> Option<String> {
    let mut parts = Vec::new();
    for attribute in [KAX_VERTICAL_SCROLL_BAR, KAX_HORIZONTAL_SCROLL_BAR] {
        if let Some(scrollbar) = ax_element(element, attribute) {
            match ax_number(&scrollbar, KAX_VALUE) {
                Some(value) => parts.push(format!("{value:.3}")),
                None => parts.push("none".into()),
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("|"))
    }
}

fn perform_scroll_action_or_ancestor(
    element: &AXUIElement,
    scroll_x: f64,
    scroll_y: f64,
) -> bool {
    let action_name = if scroll_y > 0.0 {
        AX_SCROLL_DOWN
    } else if scroll_y < 0.0 {
        AX_SCROLL_UP
    } else if scroll_x > 0.0 {
        AX_SCROLL_RIGHT
    } else if scroll_x < 0.0 {
        AX_SCROLL_LEFT
    } else {
        return false;
    };
    let mut current = Some(element.retain());
    for _ in 0..=10 {
        let Some(current_element) = current else { break };
        let actions = ax_action_names(&current_element);
        if actions.iter().any(|name| name == action_name) {
            // SAFETY: AXUIElementPerformAction is a plain AX IPC call.
            let action = CFString::from_static_str(action_name);
            let error = unsafe { current_element.perform_action(&action) };
            return error == AXError::Success;
        }
        current = ax_element(&current_element, KAX_PARENT);
    }
    false
}

fn drag_outcome(path: &[Point], pid: u32, window_id: Option<i64>, headless: bool) -> ActOutcome {
    if headless {
        return ActOutcome::Didnt;
    }
    if path.len() < 2 {
        return ActOutcome::Didnt;
    }
    raise_window(pid, window_id);
    post_mouse_drag(path);
    ActOutcome::Unknown
}

fn move_mouse_outcome(x: f64, y: f64, headless: bool) -> ActOutcome {
    if headless {
        return ActOutcome::Didnt;
    }
    let point = CGPoint { x, y };
    if let Some(event) = CGEvent::new_mouse_event(None, CGEventType::MouseMoved, point, CGMouseButton::Left)
    {
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    ActOutcome::Unknown
}

fn set_focused(element: &AXUIElement) {
    let attribute = CFString::from_static_str(KAX_FOCUSED);
    // SAFETY: AXUIElementSetAttributeValue is a plain AX IPC call; best-effort.
    unsafe {
        let _ = element.set_attribute_value(&attribute, CFBoolean::new(true));
    }
}

/// Bring the target window forward so physical input lands on it. Uses AX
/// (set AXMain/AXFocused + AXRaise) since AppKit activation is unavailable.
fn raise_window(pid: u32, window_id: Option<i64>) {
    if pid == 0 {
        return;
    }
    // SAFETY: AXUIElement::new_application only requires a valid pid.
    let app = unsafe { AXUIElement::new_application(pid as i32) };
    // SAFETY: set_messaging_timeout is an AX IPC call.
    let _ = unsafe { app.set_messaging_timeout(AX_TIMEOUT) };
    let windows = ax_elements(&app, KAX_WINDOWS);
    let Some(window) = find_window_element(&windows, window_id) else {
        return;
    };
    for attribute in [KAX_MAIN, KAX_FOCUSED] {
        // SAFETY: best-effort AX raise; errors are ignored.
        unsafe {
            let _ = window.set_attribute_value(
                &CFString::from_static_str(attribute),
                CFBoolean::new(true),
            );
        }
    }
    // SAFETY: best-effort AX raise action.
    unsafe {
        let _ = window.perform_action(&CFString::from_static_str(AX_RAISE));
    }
}

/// ---------------------------------------------------------------------------
/// Physical input (CGEvent)
/// ---------------------------------------------------------------------------

fn post_mouse_click(point: CGPoint, button: Option<&str>, click_count: u8) {
    let (down, up, mouse_button) = match button {
        Some("right") => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
        ),
        Some("middle") => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
        ),
        _ => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
        ),
    };
    let click_state = (click_count.max(1) as i64).min(3);
    if let Some(event) = CGEvent::new_mouse_event(None, down, point, mouse_button) {
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, click_state);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    std::thread::sleep(Duration::from_millis(12));
    if let Some(event) = CGEvent::new_mouse_event(None, up, point, mouse_button) {
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, click_state);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    std::thread::sleep(Duration::from_millis(70));
}

fn post_mouse_drag(path: &[Point]) {
    let first = CGPoint {
        x: path[0].x,
        y: path[0].y,
    };
    if let Some(event) = CGEvent::new_mouse_event(None, CGEventType::LeftMouseDown, first, CGMouseButton::Left) {
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    std::thread::sleep(Duration::from_millis(12));
    for point in &path[1..] {
        let point = CGPoint {
            x: point.x,
            y: point.y,
        };
        if let Some(event) = CGEvent::new_mouse_event(None, CGEventType::LeftMouseDragged, point, CGMouseButton::Left)
        {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    let last = path.last().unwrap_or(&path[0]);
    let point = CGPoint {
        x: last.x,
        y: last.y,
    };
    if let Some(event) = CGEvent::new_mouse_event(None, CGEventType::LeftMouseUp, point, CGMouseButton::Left) {
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
}

fn post_scroll_wheel(point: Option<CGPoint>, scroll_x: f64, scroll_y: f64) {
    let wheel1 = (-scroll_y).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    let wheel2 = (scroll_x).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    if let Some(event) =
        CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, wheel1, wheel2, 0)
    {
        if let Some(point) = point {
            CGEvent::set_location(Some(&event), point);
        }
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
}

fn post_key_event(keycode: u16, key_down: bool, flags: CGEventFlags, unicode: Option<char>) {
    let Some(event) = CGEvent::new_keyboard_event(None, keycode, key_down) else {
        return;
    };
    CGEvent::set_flags(Some(&event), flags);
    if let Some(ch) = unicode {
        let mut buffer = [0u16; 2];
        let encoded = ch.encode_utf16(&mut buffer);
        // SAFETY: `encoded` is a valid UTF-16 code-unit buffer and
        // `string_length` matches its length exactly.
        unsafe {
            CGEvent::keyboard_set_unicode_string(Some(&event), encoded.len() as u64, encoded.as_ptr());
        }
    }
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}

fn post_unicode_text(text: &str) {
    for ch in text.chars() {
        let keycode = printable_keycode(ch);
        let flags = if ch.is_ascii_uppercase() {
            CGEventFlags::MaskShift
        } else {
            CGEventFlags::empty()
        };
        post_key_event(keycode, true, flags, Some(ch));
        post_key_event(keycode, false, flags, None);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn printable_keycode(ch: char) -> u16 {
    let mut buffer = [0u16; 1];
    let encoded = ch.encode_utf16(&mut buffer);
    let key = String::from_utf16_lossy(encoded);
    physical_key(&key.to_lowercase()).unwrap_or(0)
}

/// A chord is `modifier[+modifier...]+key`; the last component is the key.
fn post_key_chord(chord: &str) {
    let parts: Vec<&str> = chord.split('+').collect();
    if parts.is_empty() || parts[0].is_empty() {
        return;
    }
    let (modifier_parts, key_part) = if parts.len() > 1 {
        (&parts[..parts.len() - 1], parts[parts.len() - 1])
    } else {
        (&[][..], parts[0])
    };
    let mut flags = CGEventFlags::empty();
    let mut modifier_keycodes = Vec::new();
    for modifier in modifier_parts {
        match modifier.to_lowercase().as_str() {
            "cmd" | "command" | "meta" => {
                flags |= CGEventFlags::MaskCommand;
                modifier_keycodes.push(MOD_COMMAND);
            }
            "ctrl" | "control" => {
                flags |= CGEventFlags::MaskControl;
                modifier_keycodes.push(MOD_CTRL);
            }
            "shift" => {
                flags |= CGEventFlags::MaskShift;
                modifier_keycodes.push(MOD_SHIFT);
            }
            "option" | "alt" => {
                flags |= CGEventFlags::MaskAlternate;
                modifier_keycodes.push(MOD_OPTION);
            }
            _ => {}
        }
    }
    for &keycode in &modifier_keycodes {
        post_key_event(keycode, true, flags, None);
    }
    let lower_key = key_part.to_lowercase();
    let unicode = key_part.chars().next();
    let mut key_flags = flags;
    if unicode.is_some_and(|ch| ch.is_ascii_uppercase() && key_part.chars().count() == 1) {
        key_flags |= CGEventFlags::MaskShift;
    }
    let keycode = physical_key(&lower_key).unwrap_or(0);
    post_key_event(keycode, true, key_flags, unicode);
    post_key_event(keycode, false, key_flags, None);
    for &keycode in modifier_keycodes.iter().rev() {
        post_key_event(keycode, false, flags, None);
    }
}

/// Virtual keycodes, ported from pi bridge.swift's keycode map.
fn physical_key(key: &str) -> Option<u16> {
    Some(match key {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "tab" => 48,
        "space" | " " => 49,
        "`" => 50,
        "backspace" | "delete" | "del" => 51,
        "esc" | "escape" => 53,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "home" => 115,
        "pageup" | "page_up" => 116,
        "pagedown" | "page_down" | "page down" => 121,
        "forwarddelete" | "forward_delete" => 117,
        "end" => 119,
        "left" | "arrowleft" | "arrow_left" => 123,
        "right" | "arrowright" | "arrow_right" => 124,
        "down" | "arrowdown" | "arrow_down" => 125,
        "up" | "arrowup" | "arrow_up" => 126,
        _ => return None,
    })
}
