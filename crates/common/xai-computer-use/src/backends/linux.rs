//! Native Linux backend over AT-SPI2 (Accessibility Toolkit Service
//! Provider Interface).
//!
//! Implements [`UiBackend`] using the async `atspi` crate (zbus-based):
//! - **Tree**: walk `atspi::accessible::Accessible` / `Component` /
//!   `Action` / `EditableText` interfaces to build outlines and perform
//!   semantic actions.
//! - **Input**: X11 `XTEST` via `x11rb` (`x11rb::protocol::xtest`) for
//!   physical pointer/keyboard delivery when `DISPLAY` is available. On
//!   Wayland-only sessions input is semantic-only (matching pi-computer-use).
//! - **Capture**: window image via X11 `XGetImage` (XComposite
//!   name-window-pixmap when available), encoded as base64 PNG.
//!
//! Wire refs are `atspi:<seq>` mapped to `(application_path, accessible_path)`
//! in [`LinuxBackend::ref_store`] (bounded, evict oldest beyond 4096), so
//! `act` / `read_text` can re-resolve elements by their AT-SPI paths.
//! `find_roots` returns one root per top-level window (`role == "window"` or
//! `"dialog"`), with `window_id` = X11 window id when available and
//! `resource_key = "desktop-pid:<pid>"`.
//!
//! Reference implementation: `native/linux/bridge-rs/src/` in the
//! pi-computer-use repo (`/tmp/pi-computer-use` on this machine) — port the
//! semantics from `atspi.rs` (tree walk, capability mapping), `x11.rs`
//! (EWMH roots, XTEST input), and `state.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;

use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::application::ApplicationProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::proxy::value::ValueProxy;
use atspi::{AccessibilityConnection, CoordType, Interface, InterfaceSet, State, StateSet};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::composite;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageData, ClientMessageEvent, EventMask, ImageFormat, ImageOrder,
    Keycode, MapState, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, CLIENT_MESSAGE_EVENT,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

use crate::backend::{ActOutcome, BackendError, TextPage, UiBackend};
use crate::model::{
    Action, Bounds, FindRootsRequest, ImageCapture, ObserveMode, ObserveRequest, Point, RootInfo,
    TextChunk, UiNode, UiSnapshot,
};

const REGISTRY_DESTINATION: &str = "org.a11y.atspi.Registry";
const REGISTRY_ROOT: &str = "/org/a11y/atspi/accessible/root";
const MAX_NODES: usize = 2000;
const MAX_CHILDREN: usize = 32;
const MAX_DEPTH: usize = 128;
const MAX_TEXT_LEN: usize = 4096;
const MAX_REF_STORE: usize = 4096;
const MAX_ATSPI_ACTIONS: usize = 64;
const MAX_CAPTURE_PIXELS: u64 = 64 * 1024 * 1024;

/// Construct an AT-SPI interface proxy for a specific element identified by
/// its destination (bus name) and object path. The generated zbus proxies use
/// `assume_defaults = true`, so `new` only takes a connection; per-element
/// proxies must be built through the `builder` API.
macro_rules! element_proxy {
    ($proxy_ty:ident, $conn:expr, $destination:expr, $path:expr) => {
        async {
            let builder = $proxy_ty::builder($conn)
                .destination($destination.to_owned())
                .map_err(|error| {
                    BackendError::Failed(format!(
                        "AT-SPI {} destination failed: {error}",
                        stringify!($proxy_ty)
                    ))
                })?;
            let builder = builder.path($path.to_owned()).map_err(|error| {
                BackendError::Failed(format!(
                    "AT-SPI {} path failed: {error}",
                    stringify!($proxy_ty)
                ))
            })?;
            builder.build().await.map_err(|error| {
                BackendError::Failed(format!(
                    "AT-SPI {} proxy build failed: {error}",
                    stringify!($proxy_ty)
                ))
            })
        }
    };
}

/// Options controlling foreground activation and physical input.
#[derive(Debug, Clone, Default)]
pub struct LinuxOptions {
    /// When true, never use XTEST physical input or raise windows; semantic
    /// AT-SPI actions only.
    pub headless: bool,
}

/// Linux AT-SPI2 backend.
pub struct LinuxBackend {
    options: LinuxOptions,
    // Session-scoped native ref store: "atspi:<seq>" -> (app path, acc path).
    ref_store: tokio::sync::Mutex<std::collections::VecDeque<(String, (String, String))>>,
    // Sequence for allocating wire refs across observations.
    wire_seq: AtomicU64,
    // Lazily-established AT-SPI connection, shared between observations.
    atspi: tokio::sync::Mutex<Option<Arc<AccessibilityConnection>>>,
}

impl std::fmt::Debug for LinuxBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxBackend")
            .field("headless", &self.options.headless)
            .finish()
    }
}

impl LinuxBackend {
    pub fn new(options: LinuxOptions) -> Self {
        Self {
            options,
            ref_store: tokio::sync::Mutex::new(std::collections::VecDeque::new()),
            wire_seq: AtomicU64::new(1),
            atspi: tokio::sync::Mutex::new(None),
        }
    }

    async fn atspi_conn(&self) -> Result<Arc<AccessibilityConnection>, BackendError> {
        {
            let guard = self.atspi.lock().await;
            if let Some(conn) = guard.as_ref() {
                return Ok(Arc::clone(conn));
            }
        }
        let conn = Arc::new(
            AccessibilityConnection::new()
                .await
                .map_err(|error| BackendError::Failed(format!("AT-SPI2 unavailable: {error}")))?,
        );
        *self.atspi.lock().await = Some(Arc::clone(&conn));
        Ok(conn)
    }

    /// Resolve a `atspi:<seq>` wire ref back to the element's AT-SPI
    /// `(destination, path)` pair. Returns `None` when the ref is unknown.
    async fn resolve_element(&self, wire_ref: Option<&str>) -> Option<(String, String)> {
        let wire_ref = wire_ref?;
        let store = self.ref_store.lock().await;
        store
            .iter()
            .find(|(key, _)| key == wire_ref)
            .map(|(_, value)| value.clone())
    }

    /// Screen-space centre of an element's AT-SPI component extents.
    async fn screen_center(&self, destination: &str, path: &str) -> Option<(i32, i32)> {
        let conn = self.atspi_conn().await.ok()?;
        let bounds = component_extents(&conn, destination, path).await?;
        Some((
            (bounds.x + bounds.w / 2.0).round() as i32,
            (bounds.y + bounds.h / 2.0).round() as i32,
        ))
    }

    /// Deliver a physical (XTEST) operation when the session allows it.
    /// Physical delivery cannot be verified, so success reports `Unknown`.
    async fn physical(&self, root: &RootInfo, op: PhysicalOp, point: Option<(i32, i32)>) -> ActOutcome {
        if !physical_enabled(&self.options, root) {
            return ActOutcome::Didnt;
        }
        let Some(window_id) = root.window_id else {
            return ActOutcome::Didnt;
        };
        let window = window_id as u32;
        match tokio::task::spawn_blocking(move || run_physical(op, window, point)).await {
            Ok(Ok(())) => ActOutcome::Unknown,
            Ok(Err(_)) | Err(_) => ActOutcome::Didnt,
        }
    }

    async fn act_one(&self, root: &RootInfo, action: &Action) -> ActOutcome {
        match action {
            Action::Press { wire_ref, .. } => self.act_press(root, wire_ref.as_deref()).await,
            Action::Click {
                wire_ref,
                x,
                y,
                button,
                click_count,
                ..
            } => {
                self.act_click(root, wire_ref.as_deref(), *x, *y, button.as_deref(), *click_count)
                    .await
            }
            Action::SetText { wire_ref, text, .. } => {
                self.act_set_text(root, wire_ref.as_deref(), text).await
            }
            Action::TypeText { wire_ref, text, .. } => {
                self.act_type_text(root, wire_ref.as_deref(), text).await
            }
            Action::Keypress { wire_ref, keys, .. } => {
                self.act_keypress(root, wire_ref.as_deref(), keys).await
            }
            Action::Scroll {
                wire_ref,
                scroll_x,
                scroll_y,
                ..
            } => self.act_scroll(root, wire_ref.as_deref(), *scroll_x, *scroll_y).await,
            Action::Drag { path } => self.act_drag(root, path).await,
            Action::MoveMouse { x, y } => self.act_move_mouse(root, *x, *y).await,
        }
    }

    /// Press via the AT-SPI Action interface (preferred), falling back to a
    /// physical click at the element centre when the interface is missing.
    async fn act_press(&self, root: &RootInfo, wire_ref: Option<&str>) -> ActOutcome {
        let Some((destination, path)) = self.resolve_element(wire_ref).await else {
            return ActOutcome::Didnt;
        };
        match self.semantic_do_action(&destination, &path).await {
            SemanticResult::Worked => ActOutcome::Worked,
            SemanticResult::Didnt => ActOutcome::Didnt,
            SemanticResult::Unavailable => match self.screen_center(&destination, &path).await {
                Some((x, y)) => self
                    .physical(root, PhysicalOp::Click { button: 1, count: 1 }, Some((x, y)))
                    .await,
                None => ActOutcome::Didnt,
            },
        }
    }

    async fn act_click(
        &self,
        root: &RootInfo,
        wire_ref: Option<&str>,
        x: Option<f64>,
        y: Option<f64>,
        button: Option<&str>,
        click_count: Option<u8>,
    ) -> ActOutcome {
        let button = button_detail(button).unwrap_or(1);
        let count = u32::from(click_count.unwrap_or(1)).clamp(1, 3);
        // Semantic delivery is preferred when the action targets an element.
        if let Some(wire_ref) = wire_ref {
            if let Some((destination, path)) = self.resolve_element(Some(wire_ref)).await {
                match self.semantic_do_action(&destination, &path).await {
                    SemanticResult::Worked => return ActOutcome::Worked,
                    SemanticResult::Didnt => return ActOutcome::Didnt,
                    SemanticResult::Unavailable => {}
                }
            } else {
                return ActOutcome::Didnt;
            }
        }
        let point = match (x, y) {
            (Some(px), Some(py)) => Some((px.round() as i32, py.round() as i32)),
            _ => match wire_ref {
                Some(wire_ref) => match self.resolve_element(Some(wire_ref)).await {
                    Some((destination, path)) => self.screen_center(&destination, &path).await,
                    None => None,
                },
                None => None,
            },
        };
        match point {
            Some((px, py)) => self
                .physical(root, PhysicalOp::Click { button, count }, Some((px, py)))
                .await,
            None => ActOutcome::Didnt,
        }
    }

    /// Replace text via the AT-SPI EditableText interface, then verify by
    /// re-reading the element's current text. Falls back to physical
    /// click + ctrl+a + type when the interface is missing.
    async fn act_set_text(&self, root: &RootInfo, wire_ref: Option<&str>, text: &str) -> ActOutcome {
        let Some((destination, path)) = self.resolve_element(wire_ref).await else {
            return ActOutcome::Didnt;
        };
        match self.semantic_set_text(&destination, &path, text).await {
            SemanticText::Worked => ActOutcome::Worked,
            SemanticText::Didnt => ActOutcome::Didnt,
            SemanticText::Unknown => ActOutcome::Unknown,
            SemanticText::Unavailable => match self.screen_center(&destination, &path).await {
                Some((x, y)) => {
                    self.physical(root, PhysicalOp::SetText { text: text.to_owned() }, Some((x, y)))
                        .await
                }
                None => ActOutcome::Didnt,
            },
        }
    }

    /// Type text into an element. When the element exposes EditableText the
    /// text is delivered semantically (matching pi-computer-use); otherwise
    /// the element is focused physically and the characters are typed.
    async fn act_type_text(&self, root: &RootInfo, wire_ref: Option<&str>, text: &str) -> ActOutcome {
        if let Some(wire_ref) = wire_ref {
            if let Some((destination, path)) = self.resolve_element(Some(wire_ref)).await {
                match self.semantic_set_text(&destination, &path, text).await {
                    SemanticText::Worked => return ActOutcome::Worked,
                    SemanticText::Didnt => return ActOutcome::Didnt,
                    SemanticText::Unknown => return ActOutcome::Unknown,
                    SemanticText::Unavailable => {
                        return match self.screen_center(&destination, &path).await {
                            Some((x, y)) => self
                                .physical(
                                    root,
                                    PhysicalOp::ClickAndType { text: text.to_owned() },
                                    Some((x, y)),
                                )
                                .await,
                            None => ActOutcome::Didnt,
                        };
                    }
                }
            }
            return ActOutcome::Didnt;
        }
        self.physical(root, PhysicalOp::TypeText { text: text.to_owned() }, None).await
    }

    async fn act_keypress(&self, root: &RootInfo, _wire_ref: Option<&str>, keys: &[String]) -> ActOutcome {
        self.physical(root, PhysicalOp::Keypress { keys: keys.to_vec() }, None).await
    }

    async fn act_scroll(
        &self,
        root: &RootInfo,
        wire_ref: Option<&str>,
        scroll_x: f64,
        scroll_y: f64,
    ) -> ActOutcome {
        let point = match wire_ref {
            Some(wire_ref) => match self.resolve_element(Some(wire_ref)).await {
                Some((destination, path)) => self.screen_center(&destination, &path).await,
                None => return ActOutcome::Didnt,
            },
            None => None,
        };
        self.physical(root, PhysicalOp::Scroll { dx: scroll_x, dy: scroll_y }, point).await
    }

    async fn act_drag(&self, root: &RootInfo, path: &[Point]) -> ActOutcome {
        if path.len() < 2 {
            return ActOutcome::Didnt;
        }
        let points = path
            .iter()
            .map(|point| (point.x.round() as i32, point.y.round() as i32))
            .collect();
        self.physical(root, PhysicalOp::Drag { path: points, button: 1 }, None).await
    }

    async fn act_move_mouse(&self, root: &RootInfo, x: f64, y: f64) -> ActOutcome {
        self.physical(
            root,
            PhysicalOp::MovePointer {
                x: x.round() as i32,
                y: y.round() as i32,
            },
            None,
        )
        .await
    }

    async fn semantic_do_action(&self, destination: &str, path: &str) -> SemanticResult {
        let conn = match self.atspi_conn().await {
            Ok(conn) => conn,
            Err(_) => return SemanticResult::Unavailable,
        };
        let acc = match accessible_proxy(&conn, destination, path).await {
            Ok(proxy) => proxy,
            Err(_) => return SemanticResult::Unavailable,
        };
        let interfaces = match acc.get_interfaces().await {
            Ok(interfaces) => interfaces,
            Err(_) => return SemanticResult::Unavailable,
        };
        if !interfaces.contains(Interface::Action) {
            return SemanticResult::Unavailable;
        }
        let action = match element_proxy!(ActionProxy, conn.connection(), destination, path).await {
            Ok(proxy) => proxy,
            Err(_) => return SemanticResult::Unavailable,
        };
        let count = action.nactions().await.unwrap_or(0).max(0) as usize;
        if count == 0 {
            return SemanticResult::Didnt;
        }
        let mut names = Vec::new();
        for index in 0..count.min(MAX_ATSPI_ACTIONS) {
            if let Ok(name) = action.get_name(index as i32).await {
                names.push(name);
            }
        }
        let selected = if count == 1 {
            0
        } else {
            activation_action_index(&names)
        };
        match action.do_action(selected as i32).await {
            Ok(true) => SemanticResult::Worked,
            Ok(false) => SemanticResult::Didnt,
            Err(_) => SemanticResult::Unavailable,
        }
    }

    async fn semantic_set_text(&self, destination: &str, path: &str, text: &str) -> SemanticText {
        let conn = match self.atspi_conn().await {
            Ok(conn) => conn,
            Err(_) => return SemanticText::Unavailable,
        };
        let acc = match accessible_proxy(&conn, destination, path).await {
            Ok(proxy) => proxy,
            Err(_) => return SemanticText::Unavailable,
        };
        let interfaces = match acc.get_interfaces().await {
            Ok(interfaces) => interfaces,
            Err(_) => return SemanticText::Unavailable,
        };
        if !interfaces.contains(Interface::EditableText) {
            return SemanticText::Unavailable;
        }
        let editable =
            match element_proxy!(EditableTextProxy, conn.connection(), destination, path).await {
                Ok(proxy) => proxy,
                Err(_) => return SemanticText::Unavailable,
            };
        match editable.set_text_contents(text).await {
            Ok(true) => {
                // Verify by re-reading the element's current text.
                let text_proxy =
                    element_proxy!(TextProxy, conn.connection(), destination, path).await;
                match text_proxy {
                    Ok(proxy) => match proxy.get_text(0, -1).await {
                        Ok(actual) if actual == text => SemanticText::Worked,
                        Ok(_) => SemanticText::Didnt,
                        Err(_) => SemanticText::Unknown,
                    },
                    Err(_) => SemanticText::Unknown,
                }
            }
            Ok(false) => SemanticText::Didnt,
            Err(_) => SemanticText::Unavailable,
        }
    }
}

#[async_trait]
impl UiBackend for LinuxBackend {
    async fn find_roots(&self, request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
        let x11_windows: Vec<X11Window> = if detect_session() == SessionKind::X11 {
            match tokio::task::spawn_blocking(list_x11_windows).await {
                Ok(Ok(windows)) => windows,
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let mut atspi_error: Option<BackendError> = None;
        let mut roots: Vec<RootInfo> = Vec::new();
        match self.atspi_conn().await {
            Ok(conn) => {
                let mut atspi_roots = atspi_roots(&conn).await.unwrap_or_default();
                enrich_roots(&mut atspi_roots, &x11_windows);
                roots.extend(atspi_roots.into_iter().map(atspi_root_to_rootinfo));
            }
            Err(error) => atspi_error = Some(error),
        }
        if roots.is_empty() && !x11_windows.is_empty() {
            roots.extend(x11_windows.into_iter().map(x11_window_to_rootinfo));
        }
        if roots.is_empty() {
            if let Some(error) = atspi_error {
                return Err(error);
            }
        }
        Ok(roots
            .into_iter()
            .filter(|root| root_matches(root, &request))
            .collect())
    }

    async fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError> {
        if root.kind == "browser_page" {
            return Err(BackendError::Unsupported(
                "browser pages are handled by the cdp backend".into(),
            ));
        }
        let conn = self.atspi_conn().await?;
        let (destination, path) = discover_root(&conn, root).await?;
        let origin = root.frame.map(|frame| (frame.x, frame.y)).unwrap_or((0.0, 0.0));
        let (outline, refs) = build_outline(&conn, &destination, &path, origin, &self.wire_seq).await?;
        {
            let mut store = self.ref_store.lock().await;
            for (key, value) in refs {
                store.push_back((key, value));
                while store.len() > MAX_REF_STORE {
                    store.pop_front();
                }
            }
        }
        let include_image = request
            .include_image
            .unwrap_or(matches!(request.mode, ObserveMode::Visual | ObserveMode::Fused));
        let image = if include_image {
            match root.window_id {
                Some(window_id) => {
                    let max_dimension = request.max_dimension;
                    tokio::task::spawn_blocking(move || capture_window_image(window_id, max_dimension))
                        .await
                        .unwrap_or_default()
                }
                None => None,
            }
        } else {
            None
        };
        Ok(UiSnapshot {
            root: root.clone(),
            outline,
            captured_at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
            image,
        })
    }

    async fn act(&self, root: &RootInfo, actions: &[Action]) -> Result<Vec<ActOutcome>, BackendError> {
        let mut outcomes = Vec::with_capacity(actions.len());
        for action in actions {
            outcomes.push(self.act_one(root, action).await);
        }
        Ok(outcomes)
    }

    async fn read_text(
        &self,
        _root: &RootInfo,
        wire_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TextPage, BackendError> {
        let (destination, path) = self.resolve_element(Some(wire_ref)).await.ok_or_else(|| {
            BackendError::Failed(format!("wire ref not found: {wire_ref}"))
        })?;
        let conn = self.atspi_conn().await?;
        let acc = accessible_proxy(&conn, &destination, &path).await?;
        let role = acc
            .get_role()
            .await
            .map(|role| role.name().to_owned())
            .unwrap_or_default();
        if role.eq_ignore_ascii_case("password text") || role.eq_ignore_ascii_case("password") {
            return Err(BackendError::Failed(
                "refers to a secure text field; refusing to read its value".into(),
            ));
        }
        let text_proxy = text_proxy(&conn, &destination, &path).await?;
        let full = text_proxy
            .get_text(0, -1)
            .await
            .map_err(|error| BackendError::Failed(format!("AT-SPI text read failed: {error}")))?;
        let characters: Vec<char> = full.chars().collect();
        let offset = offset.min(characters.len());
        let limit = limit.max(1);
        let end = offset.saturating_add(limit).min(characters.len());
        let text: String = characters[offset..end].iter().collect();
        Ok(TextPage {
            text,
            offset,
            limit,
            total_chars: characters.len(),
            has_more: end < characters.len(),
        })
    }
}

// ---------------------------------------------------------------------------
// AT-SPI proxies and outline construction
// ---------------------------------------------------------------------------

enum SemanticResult {
    Worked,
    Didnt,
    Unavailable,
}

enum SemanticText {
    Worked,
    Didnt,
    Unknown,
    Unavailable,
}

async fn accessible_proxy<'a>(
    conn: &'a AccessibilityConnection,
    destination: &str,
    path: &str,
) -> Result<AccessibleProxy<'a>, BackendError> {
    element_proxy!(AccessibleProxy, conn.connection(), destination, path).await
}

async fn text_proxy<'a>(
    conn: &'a AccessibilityConnection,
    destination: &str,
    path: &str,
) -> Result<TextProxy<'a>, BackendError> {
    element_proxy!(TextProxy, conn.connection(), destination, path).await
}

/// Screen-space extents of an element (or `None` when it has no usable
/// geometry). `get_extents` returns the element's own bounds in screen
/// coordinates.
async fn component_extents(
    conn: &AccessibilityConnection,
    destination: &str,
    path: &str,
) -> Option<Bounds> {
    let proxy = element_proxy!(ComponentProxy, conn.connection(), destination, path)
        .await
        .ok()?;
    let (x, y, width, height) = proxy.get_extents(CoordType::Screen).await.ok()?;
    (width > 0 && height > 0).then_some(Bounds {
        x: f64::from(x),
        y: f64::from(y),
        w: f64::from(width),
        h: f64::from(height),
    })
}

/// The pid of an AT-SPI application, from its `Application.Id` property.
async fn application_pid(
    conn: &AccessibilityConnection,
    destination: &str,
    path: &str,
) -> Option<u64> {
    let proxy = element_proxy!(ApplicationProxy, conn.connection(), destination, path)
        .await
        .ok()?;
    let id = proxy.id().await.ok()?;
    (id > 0).then_some(id as u64)
}

struct FetchedNode {
    name: String,
    role: String,
    description: String,
    identifier: String,
    value: String,
    states: StateSet,
    interfaces: InterfaceSet,
    actions: Vec<String>,
    bounds: Option<Bounds>,
    children: Vec<(String, String)>,
    is_secure: bool,
}

/// Fetch every property needed to emit one outline node. Children are
/// returned as `(destination, path)` pairs to be visited next.
async fn fetch_node(
    conn: &AccessibilityConnection,
    destination: &str,
    path: &str,
) -> Result<FetchedNode, BackendError> {
    let acc = accessible_proxy(conn, destination, path).await?;
    let name = acc.name().await.unwrap_or_default();
    let description = acc.description().await.unwrap_or_default();
    let identifier = acc.accessible_id().await.unwrap_or_default();
    let role = acc
        .get_role()
        .await
        .map(|role| role.name().to_owned())
        .unwrap_or_default();
    let states = acc.get_state().await.unwrap_or_default();
    let interfaces = acc.get_interfaces().await.unwrap_or_default();
    let children = acc
        .get_children()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|child| !child.name.as_str().is_empty() && child.path.as_str() != "/")
        .map(|child| (child.name.to_string(), child.path.to_string()))
        .collect::<Vec<_>>();
    let is_secure =
        role.eq_ignore_ascii_case("password text") || role.eq_ignore_ascii_case("password");
    let value = if is_secure {
        String::new()
    } else if interfaces.contains(Interface::Text) {
        match element_proxy!(TextProxy, conn.connection(), destination, path).await {
            Ok(proxy) => match proxy.get_text(0, -1).await {
                Ok(text) => truncate_chars(&text, MAX_TEXT_LEN),
                Err(_) => String::new(),
            },
            Err(_) => String::new(),
        }
    } else if interfaces.contains(Interface::Value) {
        match element_proxy!(ValueProxy, conn.connection(), destination, path).await {
            Ok(proxy) => match proxy.current_value().await {
                Ok(value) => format_float(value),
                Err(_) => String::new(),
            },
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };
    let mut actions = Vec::new();
    if interfaces.contains(Interface::Action) {
        if let Ok(action) =
            element_proxy!(ActionProxy, conn.connection(), destination, path).await
        {
            let count = action.nactions().await.unwrap_or(0).max(0) as usize;
            for index in 0..count.min(MAX_ATSPI_ACTIONS) {
                if let Ok(name) = action.get_name(index as i32).await {
                    actions.push(name);
                }
            }
        }
    }
    let bounds = if interfaces.contains(Interface::Component) {
        component_extents(conn, destination, path).await
    } else {
        None
    };
    Ok(FetchedNode {
        name,
        role,
        description,
        identifier,
        value,
        states,
        interfaces,
        actions,
        bounds,
        children,
        is_secure,
    })
}

struct PendingVisit {
    destination: String,
    path: String,
    parent: usize,
    depth: usize,
}

/// Iterative depth-first walk over an AT-SPI subtree. Builds the outline
/// bottom-up from a pre-order arena, allocates `@eN` element refs and
/// `atspi:<seq>` wire refs, prunes invisible/non-interactive subtrees and
/// enforces the node / child budgets.
async fn build_outline(
    conn: &AccessibilityConnection,
    root_destination: &str,
    root_path: &str,
    origin: (f64, f64),
    wire_seq: &AtomicU64,
) -> Result<(UiNode, Vec<(String, (String, String))>), BackendError> {
    let mut arena: Vec<(UiNode, Vec<usize>)> = Vec::new();
    let mut stack: Vec<PendingVisit> = vec![PendingVisit {
        destination: root_destination.to_owned(),
        path: root_path.to_owned(),
        parent: usize::MAX,
        depth: 0,
    }];
    let mut refs: Vec<(String, (String, String))> = Vec::new();
    let mut node_budget_exhausted = false;
    let mut element_counter: usize = 0;

    while let Some(pending) = stack.pop() {
        if arena.len() >= MAX_NODES {
            node_budget_exhausted = true;
            break;
        }
        let fetched = match fetch_node(conn, &pending.destination, &pending.path).await {
            Ok(node) => node,
            Err(_) => continue,
        };
        if pending.parent != usize::MAX && !node_emittable(&fetched) {
            continue;
        }
        element_counter += 1;
        let wire_ref = format!("atspi:{}", wire_seq.fetch_add(1, Ordering::Relaxed));
        refs.push((wire_ref.clone(), (pending.destination.clone(), pending.path.clone())));
        let relative_bounds = fetched.bounds.map(|bounds| Bounds {
            x: bounds.x - origin.0,
            y: bounds.y - origin.1,
            w: bounds.w,
            h: bounds.h,
        });
        let can_press = fetched.interfaces.contains(Interface::Action);
        let can_set_value = fetched.interfaces.contains(Interface::EditableText);
        let role = normalize_role(&fetched.role);
        let is_text_input = can_set_value
            || (fetched.states.contains(State::Editable) && is_textish_role(&fetched.role));
        let mut actions = fetched
            .actions
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<Vec<_>>();
        if can_press && actions.is_empty() {
            actions.push("press".to_owned());
        }
        if can_set_value && !actions.iter().any(|name| name == "setValue") {
            actions.push("setValue".to_owned());
        }
        let picture_only = fetched.role.eq_ignore_ascii_case("image")
            && fetched.name.is_empty()
            && fetched.value.is_empty();
        let text_chunks = if fetched.value.is_empty() || fetched.is_secure {
            Vec::new()
        } else {
            vec![TextChunk {
                string: fetched.value.clone(),
                confidence: 1.0,
                rect: relative_bounds,
            }]
        };
        let node = UiNode {
            element_ref: format!("@e{element_counter}"),
            wire_ref: Some(wire_ref),
            role,
            subrole: String::new(),
            identifier: fetched.identifier,
            title: fetched.name,
            description: fetched.description,
            value: fetched.value,
            actions,
            can_press,
            can_focus: fetched.states.contains(State::Focusable),
            can_set_value,
            can_scroll: fetched.interfaces.contains(Interface::Selection)
                || matches!(fetched.role.as_str(), "scroll bar" | "scroll pane"),
            can_increment: fetched.interfaces.contains(Interface::Value),
            can_decrement: fetched.interfaces.contains(Interface::Value),
            is_text_input,
            bounds: relative_bounds,
            focused: fetched.states.contains(State::Focused),
            offscreen: fetched.bounds.is_none(),
            picture_only,
            truncated: false,
            scroll_extent: None,
            text: text_chunks,
            children: Vec::new(),
        };
        let index = arena.len();
        arena.push((node, Vec::new()));
        if pending.parent != usize::MAX {
            arena[pending.parent].1.push(index);
        }
        let mut children = fetched.children;
        if children.len() > MAX_CHILDREN {
            children.truncate(MAX_CHILDREN);
            arena[index].0.truncated = true;
        }
        if pending.depth + 1 > MAX_DEPTH {
            if !children.is_empty() {
                arena[index].0.truncated = true;
            }
            continue;
        }
        // Reverse so the first child is visited first (pre-order indices).
        for child in children.into_iter().rev() {
            stack.push(PendingVisit {
                destination: child.0,
                path: child.1,
                parent: index,
                depth: pending.depth + 1,
            });
        }
    }

    if arena.is_empty() {
        return Err(BackendError::Failed(
            "root accessible has no readable content".into(),
        ));
    }
    // Assemble children into parents in reverse pre-order: every child has a
    // larger index than its parent, so `built[child]` is final by then.
    let mut built: Vec<UiNode> = vec![UiNode::default(); arena.len()];
    for index in (0..arena.len()).rev() {
        let (mut node, child_indices) =
            std::mem::replace(&mut arena[index], (UiNode::default(), Vec::new()));
        for child_index in child_indices {
            node.children
                .push(std::mem::replace(&mut built[child_index], UiNode::default()));
        }
        built[index] = node;
    }
    let mut outline = std::mem::replace(&mut built[0], UiNode::default());
    outline.truncated |= node_budget_exhausted;
    Ok((outline, refs))
}

fn node_emittable(node: &FetchedNode) -> bool {
    node.states.contains(State::Visible)
        || node.states.contains(State::Showing)
        || is_structural_role(&node.role)
        || node.interfaces.contains(Interface::Action)
        || node.interfaces.contains(Interface::EditableText)
        || node.interfaces.contains(Interface::Selection)
        || node.interfaces.contains(Interface::Text)
}

fn is_structural_role(role: &str) -> bool {
    matches!(
        role,
        "window"
            | "frame"
            | "dialog"
            | "panel"
            | "root pane"
            | "scroll pane"
            | "split pane"
            | "page tab list"
            | "menu bar"
            | "menu"
            | "tool bar"
            | "status bar"
            | "table"
            | "tree"
            | "list"
            | "combo box"
            | "document frame"
            | "desktop frame"
            | "layered pane"
            | "html container"
    )
}

fn is_textish_role(role: &str) -> bool {
    matches!(
        role,
        "text"
            | "entry"
            | "password text"
            | "combo box"
            | "list box"
            | "document text"
            | "document web"
            | "terminal"
            | "autocomplete"
            | "editbar"
            | "spin button"
    )
}

fn normalize_role(role: &str) -> String {
    match role.to_ascii_lowercase().as_str() {
        "push button" => "button".to_owned(),
        "check box" => "checkbox".to_owned(),
        "combo box" => "combobox".to_owned(),
        "list box" => "listbox".to_owned(),
        "password text" => "text".to_owned(),
        "page tab" => "tab".to_owned(),
        "page tab list" => "tab list".to_owned(),
        other => other.to_owned(),
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn format_float(value: f64) -> String {
    if !value.is_finite() {
        return String::new();
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{value:.0}");
    }
    let mut rendered = format!("{value:.2}");
    while rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.pop();
    }
    rendered
}

fn activation_action_index(names: &[String]) -> usize {
    const PRIORITY: [&str; 5] = ["activate", "click", "press", "invoke", "open"];
    PRIORITY
        .iter()
        .find_map(|wanted| {
            names
                .iter()
                .position(|name| normalized_action_name(name) == *wanted)
        })
        .unwrap_or(0)
}

fn normalized_action_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Root discovery and enrichment
// ---------------------------------------------------------------------------

struct AtspiRoot {
    pid: Option<u64>,
    name: String,
    app_name: String,
    role: String,
    frame: Option<Bounds>,
    x11_window: Option<u32>,
    is_focused: bool,
    is_minimized: bool,
    z_order: Option<usize>,
}

/// Enumerate every top-level window exposed by the AT-SPI registry.
async fn atspi_roots(conn: &AccessibilityConnection) -> Result<Vec<AtspiRoot>, BackendError> {
    let registry = accessible_proxy(conn, REGISTRY_DESTINATION, REGISTRY_ROOT).await?;
    let applications = registry
        .get_children()
        .await
        .map_err(|error| BackendError::Failed(format!("AT-SPI registry query failed: {error}")))?;
    let mut roots = Vec::new();
    for app in applications {
        let app_destination = app.name.to_string();
        let app_path = app.path.to_string();
        if app_destination.is_empty() || app_path == "/" {
            continue;
        }
        let pid = application_pid(conn, &app_destination, &app_path).await;
        let app_proxy = match accessible_proxy(conn, &app_destination, &app_path).await {
            Ok(proxy) => proxy,
            Err(_) => continue,
        };
        let app_name = app_proxy.name().await.unwrap_or_default();
        let windows = app_proxy.get_children().await.unwrap_or_default();
        for window in windows {
            let destination = window.name.to_string();
            let path = window.path.to_string();
            if destination.is_empty() || path == "/" {
                continue;
            }
            let proxy = match accessible_proxy(conn, &destination, &path).await {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };
            let role = proxy
                .get_role()
                .await
                .map(|role| role.name().to_owned())
                .unwrap_or_default();
            let name = proxy.name().await.unwrap_or_default();
            let frame = component_extents(conn, &destination, &path).await;
            roots.push(AtspiRoot {
                pid,
                name: if name.is_empty() { app_name.clone() } else { name },
                app_name: app_name.clone(),
                role,
                frame,
                x11_window: None,
                is_focused: false,
                is_minimized: false,
                z_order: None,
            });
        }
    }
    Ok(roots)
}

/// Correlate AT-SPI roots with X11 windows (EWMH) by pid, then by title and
/// frame distance when the pid is unknown.
fn enrich_roots(roots: &mut [AtspiRoot], windows: &[X11Window]) {
    let mut used: Vec<u32> = Vec::new();
    for root in roots.iter_mut() {
        let mut matched = false;
        if let Some(pid) = root.pid {
            if let Some(window) = windows
                .iter()
                .filter(|window| window.pid != 0 && window.pid == pid && !used.contains(&window.id))
                .min_by_key(|window| {
                    title_distance(&root.name, &window.title)
                        + frame_distance(root.frame.as_ref(), &window.frame)
                })
            {
                apply_window(root, window, &mut used);
                matched = true;
            }
        }
        if !matched && root.pid.is_none() {
            if let Some(window) = windows
                .iter()
                .filter(|window| !used.contains(&window.id))
                .min_by_key(|window| {
                    title_distance(&root.name, &window.title)
                        + frame_distance(root.frame.as_ref(), &window.frame)
                })
            {
                apply_window(root, window, &mut used);
            }
        }
    }
}

fn apply_window(root: &mut AtspiRoot, window: &X11Window, used: &mut Vec<u32>) {
    used.push(window.id);
    root.x11_window = Some(window.id);
    root.frame = Some(Bounds {
        x: f64::from(window.frame.x),
        y: f64::from(window.frame.y),
        w: f64::from(window.frame.width),
        h: f64::from(window.frame.height),
    });
    root.is_focused = window.focused;
    root.is_minimized = window.minimized;
    root.z_order = Some(window.z_order);
    if root.name.is_empty() {
        root.name.clone_from(&window.title);
    }
}

fn title_distance(root_title: &str, window_title: &str) -> i64 {
    if !root_title.is_empty()
        && (window_title.contains(root_title) || root_title.contains(window_title))
    {
        0
    } else {
        1_000_000
    }
}

fn frame_distance(a: Option<&Bounds>, b: &Rect) -> i64 {
    a.map(|a| {
        i64::from((a.x - f64::from(b.x)).abs() as i32)
            + i64::from((a.y - f64::from(b.y)).abs() as i32)
            + i64::from((a.w - f64::from(b.width)).abs() as i32)
            + i64::from((a.h - f64::from(b.height)).abs() as i32)
    })
    .unwrap_or(0)
}

/// Find the AT-SPI window that best matches the requested root.
async fn discover_root(
    conn: &AccessibilityConnection,
    root: &RootInfo,
) -> Result<(String, String), BackendError> {
    let registry = accessible_proxy(conn, REGISTRY_DESTINATION, REGISTRY_ROOT).await?;
    let applications = registry
        .get_children()
        .await
        .map_err(|error| BackendError::Failed(format!("AT-SPI registry query failed: {error}")))?;
    let mut best: Option<(i64, String, String)> = None;
    for app in applications {
        let app_destination = app.name.to_string();
        let app_path = app.path.to_string();
        if app_destination.is_empty() || app_path == "/" {
            continue;
        }
        let app_pid = application_pid(conn, &app_destination, &app_path).await;
        let windows = match accessible_proxy(conn, &app_destination, &app_path).await {
            Ok(proxy) => proxy.get_children().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for window in windows {
            let destination = window.name.to_string();
            let path = window.path.to_string();
            if destination.is_empty() || path == "/" {
                continue;
            }
            let proxy = match accessible_proxy(conn, &destination, &path).await {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };
            let role = proxy
                .get_role()
                .await
                .map(|role| role.name().to_owned())
                .unwrap_or_default();
            let name = proxy.name().await.unwrap_or_default();
            let frame = component_extents(conn, &destination, &path).await;
            let mut score = 0i64;
            if let Some(pid) = root.pid {
                if app_pid == Some(u64::from(pid)) {
                    score += 1000;
                }
            }
            if kind_for_role(&role) == root.kind {
                score += 100;
            }
            if !name.is_empty()
                && !root.title.is_empty()
                && (name.contains(&root.title) || root.title.contains(&name))
            {
                score += 50;
            }
            if let (Some(expected), Some(actual)) = (root.frame, frame) {
                score -= (expected.x - actual.x).abs() as i64
                    + (expected.y - actual.y).abs() as i64
                    + (expected.w - actual.w).abs() as i64
                    + (expected.h - actual.h).abs() as i64;
            }
            if best.as_ref().is_none_or(|(best_score, _, _)| score > *best_score) {
                best = Some((score, destination, path));
            }
        }
    }
    best.map(|(_, destination, path)| (destination, path))
        .ok_or_else(|| {
            BackendError::Failed(format!("root {} not found in the AT-SPI tree", root.root_ref))
        })
}

fn atspi_root_to_rootinfo(root: AtspiRoot) -> RootInfo {
    let kind = kind_for_role(&root.role);
    RootInfo {
        root_ref: String::new(),
        resource_key: format!("desktop-pid:{}", root.pid.unwrap_or(0)),
        kind: kind.to_owned(),
        title: root.name,
        app: Some(root.app_name),
        bundle_id: None,
        pid: root.pid.map(|pid| pid as u32),
        window_id: root.x11_window.map(i64::from),
        role: Some(root.role),
        subrole: None,
        z_order: root.z_order.unwrap_or(0) as i64,
        frame: root.frame,
        scale_factor: 1.0,
        is_onscreen: true,
        is_focused: root.is_focused,
        is_minimized: root.is_minimized,
        is_main: root.is_focused || root.z_order == Some(0),
        is_modal: kind == "dialog",
        ..Default::default()
    }
}

fn x11_window_to_rootinfo(window: X11Window) -> RootInfo {
    RootInfo {
        root_ref: String::new(),
        resource_key: format!("desktop-pid:{}", window.pid),
        kind: "window".to_owned(),
        title: window.title,
        app: None,
        bundle_id: None,
        pid: (window.pid > 0).then_some(window.pid as u32),
        window_id: Some(i64::from(window.id)),
        role: Some("window".to_owned()),
        subrole: None,
        z_order: window.z_order as i64,
        frame: Some(Bounds {
            x: f64::from(window.frame.x),
            y: f64::from(window.frame.y),
            w: f64::from(window.frame.width),
            h: f64::from(window.frame.height),
        }),
        scale_factor: 1.0,
        is_onscreen: true,
        is_focused: window.focused,
        is_minimized: window.minimized,
        is_main: window.focused || window.z_order == 0,
        is_modal: false,
        ..Default::default()
    }
}

fn kind_for_role(role: &str) -> &'static str {
    let lower = role.to_ascii_lowercase();
    if lower.contains("dialog") {
        "dialog"
    } else if lower.contains("menu") {
        "menu"
    } else {
        "window"
    }
}

fn root_matches(root: &RootInfo, request: &FindRootsRequest) -> bool {
    request
        .app
        .as_ref()
        .is_none_or(|app| root.app.as_ref() == Some(app))
        && request.bundle_id.as_ref().is_none_or(|_| false)
        && request.pid.is_none_or(|pid| root.pid == Some(pid))
        && request.kind.as_ref().is_none_or(|kind| &root.kind == kind)
        && request
            .text
            .as_ref()
            .is_none_or(|text| root.title.to_lowercase().contains(&text.to_lowercase()))
}

// ---------------------------------------------------------------------------
// X11 / EWMH / XTEST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    X11,
    Wayland,
    Headless,
}

fn detect_session() -> SessionKind {
    let kind = std::env::var("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if kind == "wayland" || (kind.is_empty() && std::env::var_os("WAYLAND_DISPLAY").is_some()) {
        SessionKind::Wayland
    } else if kind == "x11" || std::env::var_os("DISPLAY").is_some() {
        SessionKind::X11
    } else {
        SessionKind::Headless
    }
}

/// XTEST delivery requires an X11 session, a correlated owning window and a
/// non-headless configuration.
fn physical_enabled(options: &LinuxOptions, root: &RootInfo) -> bool {
    !options.headless && detect_session() == SessionKind::X11 && root.window_id.is_some()
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone)]
struct X11Window {
    id: u32,
    pid: u64,
    title: String,
    frame: Rect,
    focused: bool,
    minimized: bool,
    z_order: usize,
}

enum PhysicalOp {
    MovePointer { x: i32, y: i32 },
    Click { button: u8, count: u32 },
    Scroll { dx: f64, dy: f64 },
    Drag { path: Vec<(i32, i32)>, button: u8 },
    TypeText { text: String },
    ClickAndType { text: String },
    SetText { text: String },
    Keypress { keys: Vec<String> },
}

struct X11Atoms {
    clients: Atom,
    stacking: Atom,
    active: Atom,
    pid: Atom,
    name: Atom,
    state: Atom,
    hidden: Atom,
    utf8: Atom,
}

impl X11Atoms {
    fn new(conn: &RustConnection) -> Result<Self, String> {
        Ok(Self {
            clients: intern_atom(conn, "_NET_CLIENT_LIST")?,
            stacking: intern_atom(conn, "_NET_CLIENT_LIST_STACKING")?,
            active: intern_atom(conn, "_NET_ACTIVE_WINDOW")?,
            pid: intern_atom(conn, "_NET_WM_PID")?,
            name: intern_atom(conn, "_NET_WM_NAME")?,
            state: intern_atom(conn, "_NET_WM_STATE")?,
            hidden: intern_atom(conn, "_NET_WM_STATE_HIDDEN")?,
            utf8: intern_atom(conn, "UTF8_STRING")?,
        })
    }
}

/// Enumerate EWMH top-level windows (`_NET_CLIENT_LIST_STACKING`, falling
/// back to `_NET_CLIENT_LIST`), each with pid, title, translated frame,
/// focus state and z-order.
fn list_x11_windows() -> Result<Vec<X11Window>, String> {
    let (conn, screen) = x11rb::connect(None).map_err(|error| format!("X11 connect failed: {error}"))?;
    let root = conn.setup().roots[screen].root;
    let atoms = X11Atoms::new(&conn)?;
    let active = property32(&conn, root, atoms.active, AtomEnum::WINDOW)
        .first()
        .copied();
    let mut windows = property32(&conn, root, atoms.stacking, AtomEnum::WINDOW);
    if windows.is_empty() {
        windows = property32(&conn, root, atoms.clients, AtomEnum::WINDOW);
    }
    let mut out = Vec::new();
    for (z_order, id) in windows.into_iter().rev().enumerate() {
        let Some(geometry) = conn
            .get_geometry(id)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            continue;
        };
        let translated = conn
            .translate_coordinates(id, root, 0, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        let state_atoms = property32(&conn, id, atoms.state, AtomEnum::ATOM);
        out.push(X11Window {
            id,
            pid: property32(&conn, id, atoms.pid, AtomEnum::CARDINAL)
                .first()
                .copied()
                .unwrap_or(0)
                .into(),
            title: window_title(&conn, id, &atoms),
            frame: Rect {
                x: translated
                    .as_ref()
                    .map(|reply| i32::from(reply.dst_x))
                    .unwrap_or(i32::from(geometry.x)),
                y: translated
                    .as_ref()
                    .map(|reply| i32::from(reply.dst_y))
                    .unwrap_or(i32::from(geometry.y)),
                width: i32::from(geometry.width),
                height: i32::from(geometry.height),
            },
            focused: active == Some(id),
            minimized: state_atoms.contains(&atoms.hidden),
            z_order,
        });
    }
    Ok(out)
}

/// Capture a window image via X11 `XGetImage` (XComposite
/// name-window-pixmap when available so off-screen content is included,
/// falling back to the raw window drawable), downscale to `max_dimension`
/// and encode as base64 PNG. Runs on a blocking thread with a fresh
/// connection — X11 connections are not safe to share across threads. Any
/// failure yields `None` so observations degrade gracefully.
fn capture_window_image(window_id: i64, max_dimension: Option<u32>) -> Option<ImageCapture> {
    let window_id = window_id as u32;
    let (conn, _screen) = x11rb::connect(None).ok()?;
    let geometry = conn.get_geometry(window_id).ok()?.reply().ok()?;
    let (width, height) = (u32::from(geometry.width), u32::from(geometry.height));
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || pixels > MAX_CAPTURE_PIXELS {
        return None;
    }
    // Prefer XComposite so the capture includes content that is not
    // currently mapped/visible; degrade to the plain window drawable.
    let pixmap = conn.generate_id().ok()?;
    let composited = composite::name_window_pixmap(&conn, window_id, pixmap)
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .is_some();
    let drawable = if composited { pixmap } else { window_id };
    let image = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            0,
            0,
            geometry.width,
            geometry.height,
            u32::MAX,
        )
        .ok()?
        .reply()
        .ok()?;
    if composited {
        let _ = conn.free_pixmap(pixmap);
    }
    let rgba = decode_x11_image(
        &image.data,
        width,
        height,
        image.depth,
        conn.setup().image_byte_order,
    )?;
    let (output_width, output_height) = scaled_dimensions(width, height, max_dimension);
    let rgba_image = image::RgbaImage::from_raw(width, height, rgba)?;
    let shot = if (output_width, output_height) != (width, height) {
        image::imageops::resize(
            &rgba_image,
            output_width,
            output_height,
            image::imageops::FilterType::Triangle,
        )
    } else {
        rgba_image
    };
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(shot)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .ok()?;
    Some(ImageCapture {
        mime_type: "image/png".to_owned(),
        base64: base64::engine::general_purpose::STANDARD.encode(encoded.into_inner()),
        width: output_width,
        height: output_height,
    })
}

/// Decode an `XGetImage` Z_PIXMAP reply into straight RGBA. Z_PIXMAP rows
/// are padded to 32 bits, so both 24- and 32-bit depths yield 4 bytes per
/// pixel; the RGB byte order depends on the server's `image_byte_order`.
fn decode_x11_image(
    data: &[u8],
    width: u32,
    height: u32,
    depth: u8,
    order: ImageOrder,
) -> Option<Vec<u8>> {
    let expected = usize::try_from(width).ok()? * usize::try_from(height).ok()? * 4;
    if !matches!(depth, 24 | 32) || data.len() < expected {
        return None;
    }
    let mut out = Vec::with_capacity(expected);
    for pixel in data[..expected].chunks_exact(4) {
        let (r, g, b) = if order == ImageOrder::LSB_FIRST {
            (pixel[2], pixel[1], pixel[0])
        } else {
            (pixel[1], pixel[2], pixel[3])
        };
        out.extend_from_slice(&[r, g, b, 255]);
    }
    Some(out)
}

fn scaled_dimensions(width: u32, height: u32, max_dimension: Option<u32>) -> (u32, u32) {
    let largest = width.max(height);
    let Some(limit) = max_dimension.filter(|limit| *limit > 0 && *limit < largest) else {
        return (width, height);
    };
    let scale = f64::from(limit) / f64::from(largest);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
}

/// Deliver one physical operation. Runs on a blocking thread (X11 requests
/// and XTEST are synchronous); the caller owns the X11 connection for the
/// duration of the operation.
fn run_physical(op: PhysicalOp, window_id: u32, point: Option<(i32, i32)>) -> Result<(), String> {
    let (conn, screen) = x11rb::connect(None).map_err(|error| format!("X11 connect failed: {error}"))?;
    let root = conn.setup().roots[screen].root;
    validate_target(&conn, root, window_id)?;
    ensure_active(&conn, root, window_id)?;
    xtest::get_version(&conn, 2, 2)
        .map_err(|error| format!("XTEST unavailable: {error}"))?
        .reply()
        .map_err(|error| format!("XTEST unavailable: {error}"))?;
    match op {
        PhysicalOp::MovePointer { x, y } => {
            preflight_point(&conn, root, window_id, x, y)?;
            fake(&conn, root, window_id, MOTION_NOTIFY_EVENT, 0, x, y)?;
        }
        PhysicalOp::Click { button, count } => {
            let (x, y) = point.ok_or_else(|| "click requires a target point".to_owned())?;
            preflight_point(&conn, root, window_id, x, y)?;
            fake(&conn, root, window_id, MOTION_NOTIFY_EVENT, 0, x, y)?;
            for _ in 0..count {
                fake(&conn, root, window_id, BUTTON_PRESS_EVENT, button, 0, 0)?;
                fake(&conn, root, window_id, BUTTON_RELEASE_EVENT, button, 0, 0)?;
            }
        }
        PhysicalOp::Scroll { dx, dy } => {
            if let Some((x, y)) = point {
                preflight_point(&conn, root, window_id, x, y)?;
                fake(&conn, root, window_id, MOTION_NOTIFY_EVENT, 0, x, y)?;
            }
            for _ in 0..dy.abs().ceil().clamp(0.0, 100.0) as usize {
                let button = if dy < 0.0 { 4 } else { 5 };
                fake(&conn, root, window_id, BUTTON_PRESS_EVENT, button, 0, 0)?;
                fake(&conn, root, window_id, BUTTON_RELEASE_EVENT, button, 0, 0)?;
            }
            for _ in 0..dx.abs().ceil().clamp(0.0, 100.0) as usize {
                let button = if dx < 0.0 { 6 } else { 7 };
                fake(&conn, root, window_id, BUTTON_PRESS_EVENT, button, 0, 0)?;
                fake(&conn, root, window_id, BUTTON_RELEASE_EVENT, button, 0, 0)?;
            }
        }
        PhysicalOp::Drag { path, button } => {
            if path.len() < 2 {
                return Err("drag requires at least two points".to_owned());
            }
            for &(x, y) in &path {
                preflight_point(&conn, root, window_id, x, y)?;
            }
            fake(&conn, root, window_id, MOTION_NOTIFY_EVENT, 0, path[0].0, path[0].1)?;
            fake(&conn, root, window_id, BUTTON_PRESS_EVENT, button, 0, 0)?;
            for &(x, y) in &path[1..] {
                fake(&conn, root, window_id, MOTION_NOTIFY_EVENT, 0, x, y)?;
            }
            fake(&conn, root, window_id, BUTTON_RELEASE_EVENT, button, 0, 0)?;
        }
        PhysicalOp::TypeText { text } => {
            type_text(&conn, root, window_id, &text)?;
        }
        PhysicalOp::ClickAndType { text } => {
            let (x, y) = point.ok_or_else(|| "typeText requires a target point".to_owned())?;
            preflight_point(&conn, root, window_id, x, y)?;
            fake(&conn, root, window_id, MOTION_NOTIFY_EVENT, 0, x, y)?;
            fake(&conn, root, window_id, BUTTON_PRESS_EVENT, 1, 0, 0)?;
            fake(&conn, root, window_id, BUTTON_RELEASE_EVENT, 1, 0, 0)?;
            type_text(&conn, root, window_id, &text)?;
        }
        PhysicalOp::SetText { text } => {
            let (x, y) = point.ok_or_else(|| "setText requires a target point".to_owned())?;
            preflight_point(&conn, root, window_id, x, y)?;
            fake(&conn, root, window_id, MOTION_NOTIFY_EVENT, 0, x, y)?;
            fake(&conn, root, window_id, BUTTON_PRESS_EVENT, 1, 0, 0)?;
            fake(&conn, root, window_id, BUTTON_RELEASE_EVENT, 1, 0, 0)?;
            keypress_named(&conn, root, window_id, &["control".to_owned(), "a".to_owned()])?;
            type_text(&conn, root, window_id, &text)?;
        }
        PhysicalOp::Keypress { keys } => {
            keypress_named(&conn, root, window_id, &keys)?;
        }
    }
    conn.flush().map_err(|error| format!("X11 flush failed: {error}"))
}

fn validate_target(conn: &RustConnection, root: Window, target: Window) -> Result<(), String> {
    if target == 0 || target == root {
        return Err("XTEST requires a specific owning X11 window".to_owned());
    }
    let attributes = conn
        .get_window_attributes(target)
        .map_err(|error| format!("X11 get_window_attributes failed: {error}"))?
        .reply()
        .map_err(|error| format!("owning window no longer exists: {error}"))?;
    let geometry = conn
        .get_geometry(target)
        .map_err(|error| format!("X11 get_geometry failed: {error}"))?
        .reply()
        .map_err(|error| format!("owning window has no usable geometry: {error}"))?;
    let atoms = X11Atoms::new(conn)?;
    if attributes.map_state != MapState::VIEWABLE
        || geometry.width == 0
        || geometry.height == 0
        || property32(conn, target, atoms.state, AtomEnum::ATOM).contains(&atoms.hidden)
    {
        return Err("owning window is not mapped and visible".to_owned());
    }
    Ok(())
}

/// Bring the owning window to the foreground via EWMH `_NET_ACTIVE_WINDOW`
/// and wait for the window manager to confirm.
fn ensure_active(conn: &RustConnection, root: Window, target: Window) -> Result<(), String> {
    if active_window(conn, root)? == Some(target) {
        return Ok(());
    }
    request_active_window(conn, root, target)?;
    for _ in 0..50 {
        if active_window(conn, root)? == Some(target) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err("window manager did not activate the owning window".to_owned())
}

fn active_window(conn: &RustConnection, root: Window) -> Result<Option<Window>, String> {
    let atom = intern_atom(conn, "_NET_ACTIVE_WINDOW")?;
    Ok(property32(conn, root, atom, AtomEnum::WINDOW).first().copied())
}

fn request_active_window(conn: &RustConnection, root: Window, target: Window) -> Result<(), String> {
    let atom = intern_atom(conn, "_NET_ACTIVE_WINDOW")?;
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: target,
        type_: atom,
        data: ClientMessageData::from([2u32, 0, 0, 0, 0]),
    };
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )
    .map_err(|error| format!("X11 send_event failed: {error}"))?
    .check()
    .map_err(|error| format!("X11 send_event check failed: {error}"))?;
    conn.flush().map_err(|error| format!("X11 flush failed: {error}"))
}

/// Refuse pointer delivery when the target point is outside or occluded by
/// another window (port of pi-computer-use's preflight check).
fn preflight_point(
    conn: &RustConnection,
    root: Window,
    target: Window,
    x: i32,
    y: i32,
) -> Result<(), String> {
    validate_target(conn, root, target)?;
    if active_window(conn, root)? != Some(target) {
        return Err("refusing XTEST delivery: owning window is no longer active".to_owned());
    }
    let hit = conn
        .translate_coordinates(root, root, clamp_i16(x), clamp_i16(y))
        .map_err(|error| format!("X11 translate_coordinates failed: {error}"))?
        .reply()
        .map_err(|error| format!("X11 translate_coordinates reply failed: {error}"))?
        .child;
    if hit == 0 || !windows_related(conn, hit, target)? {
        return Err(
            "refusing XTEST pointer delivery: target point is outside or occluded".to_owned(),
        );
    }
    Ok(())
}

fn windows_related(conn: &RustConnection, hit: Window, target: Window) -> Result<bool, String> {
    fn ancestors(conn: &RustConnection, mut window: Window) -> Result<Vec<Window>, String> {
        let mut out = vec![window];
        for _ in 0..64 {
            let tree = conn
                .query_tree(window)
                .map_err(|error| format!("X11 query_tree failed: {error}"))?
                .reply()
                .map_err(|error| format!("X11 query_tree reply failed: {error}"))?;
            if tree.parent == 0 || tree.parent == window {
                break;
            }
            window = tree.parent;
            out.push(window);
        }
        Ok(out)
    }
    Ok(ancestors(conn, hit)?.contains(&target) || ancestors(conn, target)?.contains(&hit))
}

fn fake(
    conn: &RustConnection,
    root: Window,
    target: Window,
    event: u8,
    detail: u8,
    x: i32,
    y: i32,
) -> Result<(), String> {
    validate_target(conn, root, target)?;
    if active_window(conn, root)? != Some(target) {
        return Err("refusing XTEST delivery: owning window is no longer active".to_owned());
    }
    xtest::fake_input(conn, event, detail, 0, root, clamp_i16(x), clamp_i16(y), 0)
        .map_err(|error| format!("XTEST fake_input failed: {error}"))?
        .check()
        .map_err(|error| format!("XTEST fake_input check failed: {error}"))
}

fn type_text(conn: &RustConnection, root: Window, target: Window, text: &str) -> Result<(), String> {
    for ch in text.chars() {
        let (sym, shift) = char_keysym(ch)
            .ok_or_else(|| format!("unsupported XTEST character {ch:?}"))?;
        if shift {
            key(conn, root, target, 0xffe1, KEY_PRESS_EVENT)?;
        }
        key(conn, root, target, sym, KEY_PRESS_EVENT)?;
        key(conn, root, target, sym, KEY_RELEASE_EVENT)?;
        if shift {
            key(conn, root, target, 0xffe1, KEY_RELEASE_EVENT)?;
        }
    }
    Ok(())
}

fn keypress_named(
    conn: &RustConnection,
    root: Window,
    target: Window,
    names: &[String],
) -> Result<(), String> {
    if names.is_empty() {
        return Err("keypress requires at least one key".to_owned());
    }
    let syms = names
        .iter()
        .map(|name| named_keysym(name).ok_or_else(|| format!("unsupported key '{name}'")))
        .collect::<Result<Vec<_>, _>>()?;
    for &sym in &syms {
        key(conn, root, target, sym, KEY_PRESS_EVENT)?;
    }
    for &sym in syms.iter().rev() {
        key(conn, root, target, sym, KEY_RELEASE_EVENT)?;
    }
    Ok(())
}

fn key(conn: &RustConnection, root: Window, target: Window, sym: u32, event: u8) -> Result<(), String> {
    let code = keycode_for_sym(conn, sym).ok_or_else(|| format!("no keycode for keysym 0x{sym:x}"))?;
    fake(conn, root, target, event, code, 0, 0)
}

fn keycode_for_sym(conn: &RustConnection, sym: u32) -> Option<Keycode> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(min).saturating_add(1);
    let mapping = conn.get_keyboard_mapping(min, count).ok()?.reply().ok()?;
    mapping
        .keysyms
        .chunks(usize::from(mapping.keysyms_per_keycode))
        .position(|keysyms| keysyms.contains(&sym))
        .map(|index| min + index as u8)
}

fn button_detail(button: Option<&str>) -> Option<u8> {
    match button.map(str::to_ascii_lowercase).as_deref() {
        None | Some("left") => Some(1),
        Some("middle") => Some(2),
        Some("right") => Some(3),
        Some(_) => None,
    }
}

fn char_keysym(ch: char) -> Option<(u32, bool)> {
    if ch == '\n' {
        return Some((0xff0d, false));
    }
    if ch == '\t' {
        return Some((0xff09, false));
    }
    if ch == ' ' || ch.is_ascii_lowercase() || ch.is_ascii_digit() {
        return Some((u32::from(ch), false));
    }
    if ch.is_ascii_uppercase() {
        return Some((u32::from(ch.to_ascii_lowercase()), true));
    }
    let shifted = "~!@#$%^&*()_+{}|:\"<>?";
    let base = "`1234567890-=[]\\;',./";
    shifted
        .chars()
        .position(|candidate| candidate == ch)
        .and_then(|index| base.chars().nth(index))
        .map(|value| (u32::from(value), true))
}

fn named_keysym(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(0xff0d),
        "tab" => Some(0xff09),
        "escape" | "esc" => Some(0xff1b),
        "backspace" => Some(0xff08),
        "delete" => Some(0xffff),
        "space" => Some(0x20),
        "left" => Some(0xff51),
        "up" => Some(0xff52),
        "right" => Some(0xff53),
        "down" => Some(0xff54),
        "home" => Some(0xff50),
        "end" => Some(0xff57),
        "pageup" => Some(0xff55),
        "pagedown" => Some(0xff56),
        "ctrl" | "control" => Some(0xffe3),
        "shift" => Some(0xffe1),
        "alt" | "option" => Some(0xffe9),
        "meta" | "super" | "cmd" | "command" => Some(0xffeb),
        value if value.len() == 1 => value.chars().next().map(u32::from),
        value if value.starts_with('f') => value[1..]
            .parse::<u32>()
            .ok()
            .filter(|number| (1..=35).contains(number))
            .map(|number| 0xffbd + number),
        _ => None,
    }
}

fn clamp_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn intern_atom(conn: &RustConnection, name: &str) -> Result<Atom, String> {
    conn.intern_atom(false, name.as_bytes())
        .map_err(|error| format!("X11 intern_atom failed: {error}"))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| format!("X11 intern_atom reply failed: {error}"))
}

fn property32(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    type_: impl Into<Atom>,
) -> Vec<u32> {
    conn.get_property(false, window, property, type_.into(), 0, u32::MAX)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32().map(Iterator::collect))
        .unwrap_or_default()
}

fn window_title(conn: &RustConnection, window: Window, atoms: &X11Atoms) -> String {
    let utf8 = conn
        .get_property(false, window, atoms.name, atoms.utf8, 0, 4096)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| String::from_utf8(reply.value).ok())
        .unwrap_or_default();
    if !utf8.is_empty() {
        return utf8;
    }
    conn.get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 4096)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| String::from_utf8_lossy(&reply.value).into_owned())
        .unwrap_or_default()
}
