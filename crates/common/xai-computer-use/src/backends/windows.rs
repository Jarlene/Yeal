//! Native Windows backend over UI Automation (UIA).
//!
//! Implements [`UiBackend`] using the `windows` crate (`Win32::UI::Accessibility`
//! `IUIAutomation` tree walking) plus:
//! - **Input**: `SendInput` (`Win32::UI::Input::KeyboardAndMouse`) for pointer
//!   and keyboard delivery; virtual-key mapping for common keys.
//! - **Capture**: window image via `xcap` (DXGI BitBlt under the hood),
//!   encoded as base64 PNG.
//! - **COM**: `CoInitializeEx` per calling thread; UIA calls must run on an
//!   STA/`COINIT_APARTMENTTHREADED` worker. Wrap UIA work in
//!   `tokio::task::spawn_blocking` with COM init on each entry.
//!
//! Wire refs are `uia:<seq>`: [`WindowsBackend::ref_store`] retains
//! `IUIAutomationElement` COM pointers across observations (bounded, evict
//! oldest beyond 4096). `find_roots` returns one root per top-level HWND
//! (`EnumWindows`), with `window_id` = HWND as i64 and
//! `resource_key = "desktop-pid:<pid>"`.
//!
//! Reference implementation: `native/windows/bridge-rs/src/` in the
//! pi-computer-use repo (`/tmp/pi-computer-use` on this machine) — port the
//! semantics from `uia.rs` (tree walk, capability mapping, truncated bounds),
//! `input.rs` (SendInput key/mouse mapping), `capture.rs` (window capture),
//! and `window.rs` (root enumeration/focus).

use std::collections::VecDeque;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use image::ImageEncoder;

use crate::backend::{ActOutcome, BackendError, TextPage, UiBackend};
use crate::model::{
    Action, Bounds, FindRootsRequest, ImageCapture, ObserveMode, ObserveRequest, Point, RootInfo,
    TextChunk, UiNode, UiSnapshot,
};

use windows::core::{BOOL, BSTR, Interface as _, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Bound on the session-scoped ref store; the oldest entries are evicted.
const REF_STORE_LIMIT: usize = 4096;
/// Hard cap on outline nodes kept per observation.
const MAX_TREE_NODES: usize = 2000;
/// Work cap on elements visited while walking, bounding pathological trees.
const MAX_TREE_VISITS: usize = 50_000;
/// Per-node child cap; extra children are dropped and the node is marked
/// `truncated`.
const MAX_CHILDREN: usize = 30;
/// Recursion depth cap for the UIA tree walk.
const MAX_DEPTH: usize = 64;
/// Text chunk length cap attached to outline nodes.
const MAX_TEXT_CHARS: usize = 4000;

/// Windows UIA backend.
pub struct WindowsBackend {
    // Session-scoped native ref store: "uia:<seq>" -> retained element ptr.
    ref_store: tokio::sync::Mutex<std::collections::VecDeque<(String, i64)>>,
}

impl std::fmt::Debug for WindowsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsBackend").finish()
    }
}

impl WindowsBackend {
    pub fn new() -> Self {
        Self {
            ref_store: tokio::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UiBackend for WindowsBackend {
    async fn find_roots(&self, request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
        tokio::task::spawn_blocking(move || find_roots_blocking(&request))
            .await
            .map_err(|e| BackendError::Failed(format!("find_roots task panicked: {e}")))?
    }

    async fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError> {
        let hwnd = root.window_id.ok_or_else(|| {
            BackendError::Failed("root has no window_id; cannot observe".into())
        })?;
        let include_image = match request.include_image {
            Some(value) => value,
            None => matches!(request.mode, ObserveMode::Visual | ObserveMode::Fused),
        };
        let max_dimension = request.max_dimension;
        let root_meta = root.clone();

        let (snapshot, registrations) = tokio::task::spawn_blocking(
            move || -> Result<(UiSnapshot, Vec<(String, i64)>), BackendError> {
                // SAFETY: SetProcessDpiAwarenessContext is thread-safe; it
                // fails with E_ACCESSDENIED when the process awareness is
                // already set, which we deliberately ignore (the process is
                // already aware).
                unsafe {
                    let _ =
                        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                }
                let _com = ComGuard::new()?;

                let uia: IUIAutomation = unsafe {
                    CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| {
                        BackendError::Failed(format!("CoCreateInstance IUIAutomation: {e}"))
                    })?
                };
                let root_element = unsafe {
                    uia.ElementFromHandle(HWND(hwnd as *mut _)).map_err(|e| {
                        BackendError::Failed(format!("ElementFromHandle: {e}"))
                    })?
                };
                let walker = unsafe {
                    uia.ControlViewWalker().map_err(|e| {
                        BackendError::Failed(format!("ControlViewWalker: {e}"))
                    })?
                };
                // Window origin in screen coordinates; node bounds are
                // relative to this so the outline is stable against window
                // movement.
                let origin = unsafe { window_rect(HWND(hwnd as *mut _)) }
                    .map(|r| (r.left, r.top))
                    .unwrap_or((0, 0));

                let mut state = WalkState::default();
                // Walk registrations are collected here and published to the
                // session ref store after the blocking task returns, so the
                // store itself never needs to cross the task boundary.
                let mut registrations: Vec<(String, i64)> = Vec::new();
                let outline = build_node(
                    &mut registrations,
                    &walker,
                    &root_element,
                    origin,
                    &mut state,
                    0,
                    true,
                )
                .ok_or_else(|| {
                    BackendError::Failed("window produced no accessible outline".into())
                })?;

                let image = if include_image {
                    capture_window_image(hwnd, max_dimension)
                } else {
                    None
                };

                Ok((
                    UiSnapshot {
                        root: root_meta,
                        outline,
                        captured_at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
                        image,
                    },
                    registrations,
                ))
            },
        )
        .await
        .map_err(|e| BackendError::Failed(format!("observe task panicked: {e}")))??;

        // Publish the walk's wire refs into the bounded session store.
        let mut store_guard = self.ref_store.lock().await;
        for (wire_ref, raw) in registrations {
            store_insert(&mut store_guard, wire_ref, raw);
        }
        drop(store_guard);

        Ok(snapshot)
    }

    async fn act(
        &self,
        _root: &RootInfo,
        actions: &[Action],
    ) -> Result<Vec<ActOutcome>, BackendError> {
        let actions = actions.to_vec();

        // Resolve every referenced element under the store lock so a
        // concurrent observe's eviction cannot free a pointer we are about to
        // use. Each resolved entry carries a fresh owned (AddRef'd) reference
        // as a raw i64; the blocking task rebuilds the interface and the drop
        // releases it. Only integers cross the task boundary (the store itself
        // is not `Send`).
        let store_guard = self.ref_store.lock().await;
        let raws: Vec<Option<i64>> = actions
            .iter()
            .map(|action| match action_wire_ref(action) {
                Some(wire) => retain_from_store(&store_guard, wire),
                None => None,
            })
            .collect();
        drop(store_guard);

        tokio::task::spawn_blocking(move || -> Result<Vec<ActOutcome>, BackendError> {
            let _com = ComGuard::new()?;
            let elements: Vec<Option<IUIAutomationElement>> = raws
                .iter()
                .map(|raw| {
                    raw.map(|ptr| {
                        // SAFETY: `ptr` is an owned, AddRef'd element reference
                        // transferred by `retain_from_store`; `from_raw` takes
                        // ownership and the drop below performs Release.
                        unsafe {
                            IUIAutomationElement::from_raw(ptr as *mut core::ffi::c_void)
                        }
                    })
                })
                .collect();
            let outcomes = actions
                .iter()
                .zip(elements.iter())
                .map(|(action, element)| execute_action(element.as_ref(), action))
                .collect();
            Ok(outcomes)
        })
        .await
        .map_err(|e| BackendError::Failed(format!("act task panicked: {e}")))?
    }

    async fn read_text(
        &self,
        _root: &RootInfo,
        wire_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TextPage, BackendError> {
        let wire_ref = wire_ref.to_owned();

        // Resolve the element (transferring an owned reference as a raw i64)
        // under the store lock; the blocking task below runs the actual
        // TextPattern/ValuePattern reads and releases the reference on drop.
        let store_guard = self.ref_store.lock().await;
        let raw = retain_from_store(&store_guard, &wire_ref)
            .ok_or_else(|| BackendError::Failed("element ref is no longer valid".into()))?;
        drop(store_guard);

        tokio::task::spawn_blocking(move || -> Result<TextPage, BackendError> {
            let _com = ComGuard::new()?;
            // SAFETY: `raw` is an owned, AddRef'd element reference transferred
            // by `retain_from_store`; `from_raw` takes ownership and the drop
            // below performs Release.
            let element =
                unsafe { IUIAutomationElement::from_raw(raw as *mut core::ffi::c_void) };
            let full = read_element_text(&element);
            Ok(page_text(&full, offset, limit))
        })
        .await
        .map_err(|e| BackendError::Failed(format!("read_text task panicked: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// Root enumeration
// ---------------------------------------------------------------------------

/// Enumerate visible top-level windows and project them to `RootInfo`s,
/// applying the request filters.
fn find_roots_blocking(request: &FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
    // SAFETY: SetProcessDpiAwarenessContext is thread-safe; E_ACCESSDENIED is
    // returned when the process awareness is already set, which is fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let mut hwnds: Vec<HWND> = Vec::new();
    // SAFETY: `enum_windows_proc` pushes into `hwnds` through the LPARAM
    // pointer, which is valid for the whole EnumWindows call.
    unsafe {
        EnumWindows(Some(enum_windows_proc), LPARAM(&mut hwnds as *mut Vec<HWND> as isize))
            .map_err(|e| BackendError::Failed(format!("EnumWindows failed: {e}")))?;
    }

    let foreground = unsafe { GetForegroundWindow() };
    let mut roots = Vec::new();

    for (index, hwnd) in hwnds.into_iter().enumerate() {
        let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
        if !visible {
            continue;
        }
        let pid = unsafe { window_pid(hwnd) };
        if pid == 0 {
            continue;
        }
        let title = unsafe { window_title(hwnd) };
        let class = unsafe { window_class(hwnd) };
        let owner = unsafe { GetWindow(hwnd, GW_OWNER).ok() };
        let has_owner = owner.is_some_and(|h| !h.0.is_null());
        let is_minimized = unsafe { IsIconic(hwnd).as_bool() };
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale_factor = if dpi > 0 { f64::from(dpi) / 96.0 } else { 1.0 };
        let is_dialog = class == "#32770";
        let has_modal_frame = unsafe { has_dialog_frame(hwnd) };

        let kind = if class == "#32768" {
            "menu"
        } else if is_dialog {
            "dialog"
        } else if has_owner {
            "popover"
        } else {
            "window"
        };

        if let Some(wanted) = request.kind.as_deref() {
            if wanted != kind {
                continue;
            }
        }
        if request.pid.is_some_and(|wanted| wanted != pid) {
            continue;
        }
        let app = unsafe { process_name(pid) };
        let app_stem = app.trim_end_matches(".exe").to_owned();
        if let Some(wanted) = request.app.as_deref() {
            if !app_stem.eq_ignore_ascii_case(wanted) && !app.eq_ignore_ascii_case(wanted) {
                continue;
            }
        }
        if let Some(wanted) = request.text.as_deref() {
            if !title.to_lowercase().contains(&wanted.to_lowercase()) {
                continue;
            }
        }
        // Windows has no bundle identity; a bundle filter never matches.
        if request.bundle_id.is_some() {
            continue;
        }

        let frame = unsafe { window_rect(hwnd) }.map(|r| Bounds {
            x: r.left as f64,
            y: r.top as f64,
            w: (r.right - r.left) as f64,
            h: (r.bottom - r.top) as f64,
        });

        roots.push(RootInfo {
            root_ref: format!("@w{}", index),
            resource_key: format!("desktop-pid:{}", pid),
            kind: kind.to_owned(),
            title,
            app: (!app_stem.is_empty()).then_some(app_stem),
            bundle_id: None,
            pid: Some(pid),
            window_id: Some(hwnd.0 as i64),
            role: Some(if kind == "menu" { "menu" } else { "window" }.to_owned()),
            subrole: Some(class),
            z_order: index as i64,
            frame,
            scale_factor,
            is_onscreen: !is_minimized,
            is_focused: hwnd == foreground,
            is_minimized,
            is_main: hwnd == foreground,
            is_modal: is_dialog || has_modal_frame,
            ..Default::default()
        });
    }

    Ok(roots)
}

/// EnumWindows callback: collect every top-level HWND.
///
/// # Safety
/// `lparam` must point to a live `Vec<HWND>` for the duration of the call.
unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let handles = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    handles.push(hwnd);
    TRUE
}

unsafe fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

unsafe fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

unsafe fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Resolve a pid to its process image base name, or an empty string.
unsafe fn process_name(pid: u32) -> String {
    // SAFETY: OpenProcess / QueryFullProcessImageNameW / CloseHandle follow
    // the acquire-use-release pattern; the output buffer is fixed-size.
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(_) => return String::new(),
    };
    let mut buf = [0u16; 260];
    let mut size = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, Default::default(), PWSTR(buf.as_mut_ptr()), &mut size)
            .is_ok()
    };
    let _ = unsafe { CloseHandle(handle) };
    if !ok || size == 0 {
        return String::new();
    }
    let full = String::from_utf16_lossy(&buf[..size as usize]);
    std::path::Path::new(&full)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(full)
}

unsafe fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect).ok()? };
    Some(rect)
}

unsafe fn has_dialog_frame(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    ex_style & WS_EX_DLGMODALFRAME.0 != 0
}

// ---------------------------------------------------------------------------
// Ref store helpers
// ---------------------------------------------------------------------------

/// Retain a UIA element: AddRef and return its raw interface pointer. The
/// caller (through [`store_insert`]) becomes responsible for the new reference.
fn retain_element(element: &IUIAutomationElement) -> i64 {
    // SAFETY: `clone()` performs AddRef and `into_raw()` transfers that
    // reference to the ref store without releasing it.
    element.clone().into_raw() as i64
}

/// Insert a wire-ref entry, evicting (and releasing) the oldest entry when
/// the store is at capacity.
fn store_insert(store: &mut VecDeque<(String, i64)>, wire_ref: String, raw: i64) {
    if store.len() >= REF_STORE_LIMIT {
        if let Some((_, evicted)) = store.pop_front() {
            // SAFETY: `evicted` was produced by `retain_element` and has not
            // been released; `from_raw` takes ownership and the drop below
            // performs Release.
            unsafe {
                let element = IUIAutomationElement::from_raw(evicted as *mut core::ffi::c_void);
                drop(element);
            }
        }
    }
    store.push_back((wire_ref, raw));
}

/// Look up a wire ref under the store lock and transfer a new owned element
/// reference to the caller as a raw interface pointer (i64). The caller must
/// `IUIAutomationElement::from_raw` it and let it drop to release. Only a raw
/// integer crosses into blocking tasks, so the store itself never needs to be
/// `Send`.
fn retain_from_store(store: &VecDeque<(String, i64)>, wire_ref: &str) -> Option<i64> {
    let raw = store.iter().rev().find(|(w, _)| w == wire_ref).map(|(_, r)| *r)?;
    let raw_ptr = raw as *mut core::ffi::c_void;
    // SAFETY: while the ref store holds the entry the pointer is a valid,
    // retained `IUIAutomationElement`; `clone` (AddRef) plus `into_raw`
    // transfers one reference to the caller without releasing. AddRef/Release
    // are thread-safe and need no COM apartment.
    let borrowed = unsafe { IUIAutomationElement::from_raw_borrowed(&raw_ptr) }?;
    Some(borrowed.clone().into_raw() as i64)
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

/// Mutable state for a single outline walk.
#[derive(Debug, Default)]
struct WalkState {
    seq: u64,
    kept: usize,
    visited: usize,
    truncated: bool,
}

/// Map a UIA control type ID to the normalized semantic role name used across
/// backends (matching the pi-computer-use outline role vocabulary).
///
/// The `windows` crate names these constants in camelCase, so suppress the
/// `non_upper_case_globals` style lint for the match patterns below.
#[allow(non_upper_case_globals)]
fn control_type_to_role(ctrl_type: UIA_CONTROLTYPE_ID) -> &'static str {
    match ctrl_type {
        UIA_WindowControlTypeId => "window",
        UIA_PaneControlTypeId => "pane",
        UIA_DocumentControlTypeId => "document",
        UIA_EditControlTypeId => "edit",
        UIA_ButtonControlTypeId => "button",
        UIA_CheckBoxControlTypeId => "checkbox",
        UIA_RadioButtonControlTypeId => "radio",
        UIA_ComboBoxControlTypeId => "comboBox",
        UIA_ListControlTypeId => "list",
        UIA_ListItemControlTypeId => "listItem",
        UIA_TreeControlTypeId => "tree",
        UIA_TreeItemControlTypeId => "treeItem",
        UIA_MenuControlTypeId => "menu",
        UIA_MenuBarControlTypeId => "menuBar",
        UIA_MenuItemControlTypeId => "menuItem",
        UIA_TextControlTypeId => "text",
        UIA_HyperlinkControlTypeId => "link",
        UIA_TabControlTypeId => "tab",
        UIA_TabItemControlTypeId => "tabItem",
        UIA_HeaderControlTypeId => "header",
        UIA_HeaderItemControlTypeId => "headerItem",
        UIA_TableControlTypeId => "table",
        UIA_ImageControlTypeId => "image",
        UIA_SliderControlTypeId => "slider",
        UIA_ProgressBarControlTypeId => "progressBar",
        UIA_ToolBarControlTypeId => "toolBar",
        UIA_StatusBarControlTypeId => "statusBar",
        UIA_ToolTipControlTypeId => "toolTip",
        UIA_ScrollBarControlTypeId => "scrollBar",
        UIA_GroupControlTypeId => "group",
        UIA_SeparatorControlTypeId => "separator",
        UIA_SpinnerControlTypeId => "spinner",
        UIA_SplitButtonControlTypeId => "splitButton",
        UIA_CalendarControlTypeId => "calendar",
        UIA_DataGridControlTypeId => "dataGrid",
        UIA_DataItemControlTypeId => "dataItem",
        UIA_ThumbControlTypeId => "thumb",
        UIA_TitleBarControlTypeId => "titleBar",
        UIA_AppBarControlTypeId => "appBar",
        UIA_SemanticZoomControlTypeId => "semanticZoom",
        UIA_CustomControlTypeId => "custom",
        _ => "unknown",
    }
}

/// Recursively build a `UiNode` for `element` and its control-view children.
/// Returns `None` for elements that are invisible, offscreen, zero-sized, or
/// beyond the walk budgets.
fn build_node(
    registrations: &mut Vec<(String, i64)>,
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    origin: (i32, i32),
    state: &mut WalkState,
    depth: usize,
    is_root: bool,
) -> Option<UiNode> {
    if depth > MAX_DEPTH || state.kept >= MAX_TREE_NODES || state.visited >= MAX_TREE_VISITS {
        state.truncated = true;
        return None;
    }
    state.visited += 1;

    let ctrl_type = unsafe { element.CurrentControlType().ok()? };
    let role = control_type_to_role(ctrl_type);

    let rect = if is_root {
        unsafe { element.CurrentBoundingRectangle().ok() }.unwrap_or_default()
    } else {
        unsafe { element.CurrentBoundingRectangle().ok()? }
    };
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if (w <= 0 || h <= 0) && !is_root {
        return None;
    }

    let offscreen = unsafe { element.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(true) };
    if offscreen && !is_root {
        return None;
    }

    let name = unsafe { element.CurrentName().unwrap_or_default().to_string() };
    let identifier = unsafe { element.CurrentAutomationId().unwrap_or_default().to_string() };
    let class = unsafe { element.CurrentClassName().unwrap_or_default().to_string() };
    let help = unsafe { element.CurrentHelpText().unwrap_or_default().to_string() };
    let focusable = unsafe {
        element
            .CurrentIsKeyboardFocusable()
            .map(|b| b.as_bool())
            .unwrap_or(false)
    };
    let password = unsafe { element.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) };
    let focused = unsafe {
        element
            .CurrentHasKeyboardFocus()
            .map(|b| b.as_bool())
            .unwrap_or(false)
    };

    // Capability signals from the pattern interfaces themselves.
    let value_pattern: Option<IUIAutomationValuePattern> =
        unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId).ok() };
    let value = value_pattern
        .as_ref()
        .and_then(|p| unsafe { p.CurrentValue().ok() })
        .map(|b| b.to_string())
        .unwrap_or_default();
    let value_read_only = value_pattern
        .as_ref()
        .and_then(|p| unsafe { p.CurrentIsReadOnly().ok() })
        .map(|b| b.as_bool());
    let invoke =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }
            .is_ok();
    let toggle =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) }
            .is_ok();
    let selection_item = unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .is_ok()
    };
    let expand_collapse = unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                UIA_ExpandCollapsePatternId,
            )
            .is_ok()
    };
    let text_pattern: Option<IUIAutomationTextPattern> =
        unsafe { element.GetCurrentPatternAs(UIA_TextPatternId).ok() };
    let scroll =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId) }
            .is_ok();
    let range_value = unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
            .is_ok()
    };
    let legacy_action = legacy_pressable(element);

    let can_press = invoke || toggle || selection_item || expand_collapse || legacy_action;
    let can_focus = focusable;
    let can_set_value = (value_pattern.is_some() && !value_read_only.unwrap_or(false))
        || text_pattern.is_some();
    let can_scroll = scroll;
    let can_increment = range_value;
    let can_decrement = range_value;
    let is_text_input = ctrl_type == UIA_EditControlTypeId
        || ctrl_type == UIA_DocumentControlTypeId
        || password
        || (can_set_value && matches!(role, "edit" | "document" | "comboBox"));

    let mut actions: Vec<String> = Vec::new();
    if can_press {
        actions.push("press".into());
        actions.push("click".into());
    }
    if can_focus {
        actions.push("focus".into());
    }
    if can_set_value {
        actions.push("setText".into());
        actions.push("typeText".into());
    }
    if can_scroll {
        actions.push("scroll".into());
    }
    if can_increment {
        actions.push("increment".into());
    }
    if can_decrement {
        actions.push("decrement".into());
    }

    let bounds = Bounds {
        x: f64::from(rect.left - origin.0),
        y: f64::from(rect.top - origin.1),
        w: f64::from(w),
        h: f64::from(h),
    };

    // Queue the wire ref for the session store: this element is retained
    // until the store evicts it, so `act` / `read_text` can resolve it later.
    state.kept += 1;
    state.seq += 1;
    let element_ref = format!("@e{}", state.seq);
    let wire_ref = format!("uia:{}", state.seq);
    registrations.push((wire_ref.clone(), retain_element(element)));

    let mut text: Vec<TextChunk> = Vec::new();
    let content = text_pattern
        .as_ref()
        .and_then(|p| unsafe { p.DocumentRange().ok() })
        .and_then(|r| unsafe { r.GetText(-1).ok() })
        .map(|b| b.to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| value.clone());
    if !content.trim().is_empty() {
        let string: String = content.chars().take(MAX_TEXT_CHARS).collect();
        text.push(TextChunk {
            string,
            confidence: 1.0,
            rect: Some(bounds),
        });
    }

    let mut node = UiNode {
        element_ref,
        wire_ref: Some(wire_ref),
        role: role.to_owned(),
        subrole: class,
        identifier,
        title: name,
        description: help,
        value,
        actions,
        can_press,
        can_focus,
        can_set_value,
        can_scroll,
        can_increment,
        can_decrement,
        is_text_input,
        bounds: Some(bounds),
        focused,
        offscreen,
        text,
        ..Default::default()
    };

    let mut children: Vec<UiNode> = Vec::new();
    let mut truncated_children = false;
    let mut child = unsafe { walker.GetFirstChildElement(element).ok() };
    let mut sibling_count = 0usize;
    while let Some(current) = child {
        if sibling_count >= MAX_CHILDREN {
            truncated_children = true;
            break;
        }
        if state.kept >= MAX_TREE_NODES || state.visited >= MAX_TREE_VISITS || state.truncated {
            state.truncated = true;
            truncated_children = true;
            break;
        }
        if let Some(child_node) =
            build_node(registrations, walker, &current, origin, state, depth + 1, false)
        {
            children.push(child_node);
        }
        sibling_count += 1;
        child = unsafe { walker.GetNextSiblingElement(&current).ok() };
    }
    node.truncated = truncated_children;
    node.children = children;

    Some(node)
}

/// A legacy default action counts as pressable only when it is non-empty.
fn legacy_pressable(element: &IUIAutomationElement) -> bool {
    unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                UIA_LegacyIAccessiblePatternId,
            )
    }
    .ok()
    .and_then(|p| unsafe { p.CurrentDefaultAction().ok() })
    .map(|s| !s.to_string().trim().is_empty())
    .unwrap_or(false)
}

/// Capture the window identified by `hwnd` as a base64 PNG, downscaled to
/// `max_dimension` when requested. Best effort: `None` on any failure.
fn capture_window_image(hwnd: i64, max_dimension: Option<u32>) -> Option<ImageCapture> {
    let target = hwnd as u32;
    let window = xcap::Window::all()
        .ok()?
        .into_iter()
        .find(|w| w.id().ok() == Some(target))?;
    let image = window.capture_image().ok()?;
    let (w, h) = (image.width(), image.height());
    if w == 0 || h == 0 {
        return None;
    }
    let (out_w, out_h) = match max_dimension.filter(|limit| *limit > 0) {
        Some(limit) if w.max(h) > limit => {
            let scale = limit as f64 / w.max(h) as f64;
            (
                (w as f64 * scale).round().max(1.0) as u32,
                (h as f64 * scale).round().max(1.0) as u32,
            )
        }
        _ => (w, h),
    };
    let pixels = if (out_w, out_h) == (w, h) {
        image.into_raw()
    } else {
        image::imageops::resize(&image, out_w, out_h, image::imageops::FilterType::Triangle)
            .into_raw()
    };
    let mut png: Vec<u8> = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    encoder
        .write_image(&pixels, out_w, out_h, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(ImageCapture {
        mime_type: "image/png".to_owned(),
        base64: BASE64.encode(&png),
        width: out_w,
        height: out_h,
    })
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

fn action_wire_ref(action: &Action) -> Option<&str> {
    match action {
        Action::Press { wire_ref, .. }
        | Action::Click { wire_ref, .. }
        | Action::SetText { wire_ref, .. }
        | Action::TypeText { wire_ref, .. }
        | Action::Keypress { wire_ref, .. }
        | Action::Scroll { wire_ref, .. } => wire_ref.as_deref(),
        Action::Drag { .. } | Action::MoveMouse { .. } => None,
    }
}

fn execute_action(element: Option<&IUIAutomationElement>, action: &Action) -> ActOutcome {
    match action {
        Action::Press { .. } => press_element(element),
        Action::Click {
            x,
            y,
            button,
            click_count,
            ..
        } => match (element, x, y) {
            (Some(el), _, _) => click_element(el, button.as_deref(), click_count.unwrap_or(1)),
            (None, Some(x), Some(y)) => {
                if click_screen(*x, *y, button.as_deref(), click_count.unwrap_or(1)) {
                    ActOutcome::Unknown
                } else {
                    ActOutcome::Didnt
                }
            }
            _ => ActOutcome::Didnt,
        },
        Action::SetText { text, .. } => set_text_element(element, text),
        Action::TypeText { text, .. } => type_text(element, text),
        Action::Keypress { keys, .. } => keypress(element, keys),
        Action::Scroll {
            scroll_x, scroll_y, ..
        } => scroll_action(element, *scroll_x, *scroll_y),
        Action::Drag { path } => drag_action(path),
        Action::MoveMouse { x, y } => {
            if move_mouse(*x, *y) {
                ActOutcome::Unknown
            } else {
                ActOutcome::Didnt
            }
        }
    }
}

fn press_element(element: Option<&IUIAutomationElement>) -> ActOutcome {
    let Some(element) = element else {
        return ActOutcome::Didnt;
    };

    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }
    {
        let before = observable_evidence(element);
        if unsafe { pattern.Invoke().is_err() } {
            return ActOutcome::Didnt;
        }
        return if observable_evidence(element) != before {
            ActOutcome::Worked
        } else {
            ActOutcome::Unknown
        };
    }
    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) }
    {
        let before = unsafe { pattern.CurrentToggleState().ok() };
        if unsafe { pattern.Toggle().is_err() } {
            return ActOutcome::Didnt;
        }
        let after = unsafe { pattern.CurrentToggleState().ok() };
        return match (before, after) {
            (Some(b), Some(a)) if b != a => ActOutcome::Worked,
            (Some(_), Some(_)) => ActOutcome::Didnt,
            _ => ActOutcome::Unknown,
        };
    }
    if let Ok(pattern) = unsafe {
        element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
            UIA_SelectionItemPatternId,
        )
    } {
        if unsafe { pattern.Select().is_err() } {
            return ActOutcome::Didnt;
        }
        let selected = unsafe {
            pattern
                .CurrentIsSelected()
                .map(|b| b.as_bool())
                .unwrap_or(false)
        };
        return if selected {
            ActOutcome::Worked
        } else {
            ActOutcome::Didnt
        };
    }
    if let Ok(pattern) = unsafe {
        element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
            UIA_ExpandCollapsePatternId,
        )
    } {
        let before = unsafe { pattern.CurrentExpandCollapseState().ok() };
        if unsafe { pattern.Expand().is_err() } {
            return ActOutcome::Didnt;
        }
        let after = unsafe { pattern.CurrentExpandCollapseState().ok() };
        return match (before, after) {
            (Some(b), Some(a)) if b != a => ActOutcome::Worked,
            _ => ActOutcome::Unknown,
        };
    }
    if let Ok(pattern) = unsafe {
        element.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
            UIA_LegacyIAccessiblePatternId,
        )
    } {
        let default_action = unsafe {
            pattern
                .CurrentDefaultAction()
                .map(|s| s.to_string())
                .unwrap_or_default()
        };
        if !default_action.trim().is_empty() {
            if unsafe { pattern.DoDefaultAction().is_err() } {
                return ActOutcome::Didnt;
            }
            return ActOutcome::Unknown;
        }
    }
    ActOutcome::Didnt
}

/// Observable state that can change as a result of pressing/clicking.
#[derive(Debug, Clone, PartialEq, Default)]
struct Evidence {
    value: Option<String>,
    toggle: Option<i32>,
    selected: Option<bool>,
}

fn observable_evidence(element: &IUIAutomationElement) -> Evidence {
    Evidence {
        value: unsafe {
            element
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()
                .and_then(|p| p.CurrentValue().ok())
        }
        .map(|b| b.to_string()),
        toggle: unsafe {
            element
                .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                .ok()
                .and_then(|p| p.CurrentToggleState().ok())
        }
        .map(|s| s.0),
        selected: unsafe {
            element
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .ok()
                .and_then(|p| p.CurrentIsSelected().ok())
        }
        .map(|b| b.as_bool()),
    }
}

fn click_element(element: &IUIAutomationElement, button: Option<&str>, click_count: u8) -> ActOutcome {
    let before = observable_evidence(element);
    let Some((x, y)) = element_center(element) else {
        return ActOutcome::Didnt;
    };
    if !click_screen(x, y, button, click_count) {
        return ActOutcome::Didnt;
    }
    if observable_evidence(element) != before {
        ActOutcome::Worked
    } else {
        ActOutcome::Unknown
    }
}

fn set_text_element(element: Option<&IUIAutomationElement>, text: &str) -> ActOutcome {
    let Some(element) = element else {
        return ActOutcome::Didnt;
    };
    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
    {
        let bstr = BSTR::from(text);
        if unsafe { pattern.SetValue(&bstr).is_err() } {
            return ActOutcome::Didnt;
        }
        return match unsafe { pattern.CurrentValue().ok() } {
            Some(value) if value.to_string() == text => ActOutcome::Worked,
            Some(_) => ActOutcome::Didnt,
            None => ActOutcome::Unknown,
        };
    }
    // No value pattern: focus the element, select all, then type.
    if unsafe { element.SetFocus().is_err() } {
        return ActOutcome::Didnt;
    }
    let mut select: Vec<INPUT> = Vec::new();
    select.push(key(VK_CONTROL, false));
    select.push(key(VK_A, false));
    select.push(key(VK_A, true));
    select.push(key(VK_CONTROL, true));
    if send_inputs(&select).is_err() || send_text(text).is_err() {
        return ActOutcome::Didnt;
    }
    let current = unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()
            .and_then(|p| p.CurrentValue().ok())
    }
    .map(|b| b.to_string());
    match current {
        Some(value) if value.contains(text) => ActOutcome::Worked,
        Some(_) => ActOutcome::Didnt,
        None => ActOutcome::Unknown,
    }
}

fn type_text(element: Option<&IUIAutomationElement>, text: &str) -> ActOutcome {
    if let Some(element) = element {
        if unsafe { element.SetFocus().is_err() } {
            return ActOutcome::Didnt;
        }
    }
    if send_text(text).is_ok() {
        ActOutcome::Unknown
    } else {
        ActOutcome::Didnt
    }
}

fn keypress(element: Option<&IUIAutomationElement>, keys: &[String]) -> ActOutcome {
    if let Some(element) = element {
        if unsafe { element.SetFocus().is_err() } {
            return ActOutcome::Didnt;
        }
    }
    if send_keys(keys).is_ok() {
        ActOutcome::Unknown
    } else {
        ActOutcome::Didnt
    }
}

fn scroll_action(element: Option<&IUIAutomationElement>, scroll_x: f64, scroll_y: f64) -> ActOutcome {
    if let Some(element) = element {
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
        } {
            let horizontal = scroll_amount(scroll_x);
            let vertical = scroll_amount(scroll_y);
            let before = scroll_percent(&pattern);
            if unsafe { pattern.Scroll(horizontal, vertical).is_err() } {
                return ActOutcome::Didnt;
            }
            let after = scroll_percent(&pattern);
            return match (before, after) {
                (Some(b), Some(a)) if b != a => ActOutcome::Worked,
                _ => ActOutcome::Unknown,
            };
        }
        // No scroll pattern: wheel over the element's center.
        if let Some((cx, cy)) = element_center(element) {
            if wheel_at(cx, cy, scroll_x, scroll_y) {
                ActOutcome::Unknown
            } else {
                ActOutcome::Didnt
            }
        } else {
            ActOutcome::Didnt
        }
    } else if wheel_at_current(scroll_x, scroll_y) {
        ActOutcome::Unknown
    } else {
        ActOutcome::Didnt
    }
}

fn scroll_amount(value: f64) -> ScrollAmount {
    match value.partial_cmp(&0.0) {
        Some(std::cmp::Ordering::Greater) => ScrollAmount_SmallIncrement,
        Some(std::cmp::Ordering::Less) => ScrollAmount_SmallDecrement,
        _ => ScrollAmount_NoAmount,
    }
}

fn scroll_percent(pattern: &IUIAutomationScrollPattern) -> Option<(f64, f64)> {
    let h = unsafe { pattern.CurrentHorizontalScrollPercent().ok()? };
    let v = unsafe { pattern.CurrentVerticalScrollPercent().ok()? };
    Some((h, v))
}

fn element_center(element: &IUIAutomationElement) -> Option<(f64, f64)> {
    let rect = unsafe { element.CurrentBoundingRectangle().ok()? };
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return None;
    }
    Some((
        f64::from(rect.left + rect.right) / 2.0,
        f64::from(rect.top + rect.bottom) / 2.0,
    ))
}

fn drag_action(path: &[Point]) -> ActOutcome {
    if path.len() < 2 {
        return ActOutcome::Didnt;
    }
    let mut inputs: Vec<INPUT> = Vec::new();
    let (x0, y0) = absolute_point(path[0].x, path[0].y);
    inputs.push(mouse_move(x0, y0));
    inputs.push(mouse(MOUSEEVENTF_LEFTDOWN, 0));
    for point in &path[1..] {
        let (dx, dy) = absolute_point(point.x, point.y);
        inputs.push(mouse_move(dx, dy));
    }
    inputs.push(mouse(MOUSEEVENTF_LEFTUP, 0));
    if send_inputs(&inputs).is_ok() {
        ActOutcome::Unknown
    } else {
        ActOutcome::Didnt
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

fn read_element_text(element: &IUIAutomationElement) -> String {
    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
    {
        if let Ok(range) = unsafe { pattern.DocumentRange() } {
            if let Ok(value) = unsafe { range.GetText(-1) } {
                let text = value.to_string();
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
    {
        if let Ok(value) = unsafe { pattern.CurrentValue() } {
            let text = value.to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }
    unsafe { element.CurrentName().unwrap_or_default().to_string() }
}

fn page_text(full: &str, offset: usize, limit: usize) -> TextPage {
    let total_chars = full.chars().count();
    let offset = offset.min(total_chars);
    let text: String = full.chars().skip(offset).take(limit).collect();
    TextPage {
        text,
        offset,
        limit,
        total_chars,
        has_more: offset + limit < total_chars,
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn send_inputs(inputs: &[INPUT]) -> Result<(), BackendError> {
    // SAFETY: SendInput reads the INPUT array synchronously; no pointers
    // escape the call.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err(BackendError::Failed(format!(
            "SendInput inserted {sent}/{} events",
            inputs.len()
        )))
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: flags,
                mouseData: data,
                ..Default::default()
            },
        },
    }
}

/// A pointer-move event normalized to the virtual desktop (multi-monitor
/// aware): screen points are mapped onto the 0..=65535 coordinate space.
fn mouse_move(dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ..Default::default()
            },
        },
    }
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                ..Default::default()
            },
        },
    }
}

fn unicode(ch: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wScan: ch,
                dwFlags: KEYEVENTF_UNICODE
                    | if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                ..Default::default()
            },
        },
    }
}

fn send_text(text: &str) -> Result<(), BackendError> {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(text.encode_utf16().count() * 2);
    for unit in text.encode_utf16() {
        inputs.push(unicode(unit, false));
        inputs.push(unicode(unit, true));
    }
    send_inputs(&inputs)
}

fn send_keys(keys: &[String]) -> Result<(), BackendError> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut vks: Vec<VIRTUAL_KEY> = Vec::with_capacity(keys.len());
    for name in keys {
        let vk = vk_for(name)
            .ok_or_else(|| BackendError::Failed(format!("unsupported key '{name}'")))?;
        vks.push(vk);
    }
    for vk in &vks[..vks.len().saturating_sub(1)] {
        send_inputs(&[key(*vk, false)])?;
    }
    if let Some(last) = vks.last() {
        send_inputs(&[key(*last, false), key(*last, true)])?;
    }
    for vk in vks[..vks.len().saturating_sub(1)].iter().rev() {
        send_inputs(&[key(*vk, true)])?;
    }
    Ok(())
}

fn move_mouse(x: f64, y: f64) -> bool {
    let (dx, dy) = absolute_point(x, y);
    send_inputs(&[mouse_move(dx, dy)]).is_ok()
}

fn click_screen(x: f64, y: f64, button: Option<&str>, click_count: u8) -> bool {
    let (dx, dy) = absolute_point(x, y);
    let (down, up) = match button {
        Some("right") => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        Some("middle") => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    };
    let mut inputs: Vec<INPUT> = Vec::new();
    inputs.push(mouse_move(dx, dy));
    for _ in 0..click_count.max(1) {
        inputs.push(mouse(down, 0));
        inputs.push(mouse(up, 0));
    }
    send_inputs(&inputs).is_ok()
}

fn wheel_at(x: f64, y: f64, scroll_x: f64, scroll_y: f64) -> bool {
    if !move_mouse(x, y) {
        return false;
    }
    wheel_events(scroll_x, scroll_y)
}

fn wheel_at_current(scroll_x: f64, scroll_y: f64) -> bool {
    wheel_events(scroll_x, scroll_y)
}

fn wheel_events(scroll_x: f64, scroll_y: f64) -> bool {
    let mut inputs: Vec<INPUT> = Vec::new();
    if scroll_y != 0.0 {
        inputs.push(mouse(
            MOUSEEVENTF_WHEEL,
            (-scroll_y * 120.0).round() as i32 as u32,
        ));
    }
    if scroll_x != 0.0 {
        inputs.push(mouse(
            MOUSEEVENTF_HWHEEL,
            (scroll_x * 120.0).round() as i32 as u32,
        ));
    }
    if inputs.is_empty() {
        return true;
    }
    send_inputs(&inputs).is_ok()
}

/// Normalize a screen-space point into SendInput's virtual-desktop absolute
/// 0..=65535 coordinate space.
fn absolute_point(x: f64, y: f64) -> (i32, i32) {
    // SAFETY: GetSystemMetrics is safe to call from any thread.
    let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let dx = ((x - f64::from(vx)) * 65535.0 / f64::from(vw)).round() as i32;
    let dy = ((y - f64::from(vy)) * 65535.0 / f64::from(vh)).round() as i32;
    (dx, dy)
}

/// Map a key name to a virtual-key code, mirroring the pi-computer-use input
/// mapping.
fn vk_for(name: &str) -> Option<VIRTUAL_KEY> {
    match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(VK_RETURN),
        "escape" | "esc" => Some(VK_ESCAPE),
        "tab" => Some(VK_TAB),
        "backspace" => Some(VK_BACK),
        "delete" => Some(VK_DELETE),
        "space" => Some(VK_SPACE),
        "left" | "arrowleft" => Some(VK_LEFT),
        "right" | "arrowright" => Some(VK_RIGHT),
        "up" | "arrowup" => Some(VK_UP),
        "down" | "arrowdown" => Some(VK_DOWN),
        "home" => Some(VK_HOME),
        "end" => Some(VK_END),
        "pageup" => Some(VK_PRIOR),
        "pagedown" => Some(VK_NEXT),
        "ctrl" | "control" => Some(VK_CONTROL),
        "shift" => Some(VK_SHIFT),
        "alt" | "option" => Some(VK_MENU),
        "cmd" | "win" | "meta" => Some(VK_LWIN),
        key if key.len() == 1 => {
            let c = key.as_bytes()[0];
            if c.is_ascii_alphanumeric() || c.is_ascii_punctuation() {
                Some(VIRTUAL_KEY(u16::from(c.to_ascii_uppercase())))
            } else {
                None
            }
        }
        key if key.starts_with('f') => key[1..]
            .parse::<u16>()
            .ok()
            .filter(|n| (1..=24).contains(n))
            .map(|n| VIRTUAL_KEY(VK_F1.0 + n - 1)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// COM lifetime
// ---------------------------------------------------------------------------

/// Calls `CoInitializeEx` on construction and `CoUninitialize` on drop.
struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self, BackendError> {
        // SAFETY: COM must be initialized for the calling thread before UIA
        // use. S_OK (0) and S_FALSE (1) are both success indicators; only a
        // negative HRESULT means failure.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.0 < 0 {
            return Err(BackendError::Failed(format!(
                "CoInitializeEx failed: {:#010x}",
                hr.0
            )));
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: every successful CoInitializeEx (including S_FALSE) must be
        // balanced with a CoUninitialize on the same thread.
        unsafe { CoUninitialize() }
    }
}
