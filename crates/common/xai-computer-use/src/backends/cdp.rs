//! Browser backend over the Chrome DevTools Protocol (CDP).
//!
//! Implements [`UiBackend`] for `browser_page` roots against a Chromium-family
//! browser's remote debugging endpoint:
//! - **Discovery**: `GET http://127.0.0.1:{port}/json/list` via `reqwest`;
//!   only local `ws:`/`wss:` debugger URLs on the configured port are trusted.
//! - **Transport**: one websocket connection per page target via
//!   `tokio-tungstenite`, with id-correlated pending requests and a 5s
//!   command timeout. Enable `Runtime`, `Page`, `Accessibility`, and `DOM`
//!   domains on connect.
//! - **Observe**: `Accessibility.getFullAXTree` -> UiNode outline (role, name,
//!   value, description, backendNodeId); `document.body.innerText` for the
//!   root value; `Browser.getWindowForTarget` for the window frame.
//! - **Act**: `DOM.resolveNode` + `Runtime.callFunctionOn` for click /
//!   setText / typeText / scroll on a backend node; `Input.dispatchMouseEvent`
//!   for pointer, drag and wheel scroll; `Input.dispatchKeyEvent` /
//!   `Input.insertText` for keyboard. Outcomes are grounded in re-read
//!   evidence (value / checked / scroll position / URL), never in delivery.
//! - **Navigate**: `Page.navigate` with a bounded readyState wait.
//! - **Evaluate**: `Runtime.evaluate` (`returnByValue`, bounded to 32 KiB).
//!
//! Wire refs are `cdp:<backendNodeId>` — `backendDOMNodeId` from the AX tree
//! is stable for the page session, so `act` re-resolves it directly without a
//! ref store.
//!
//! Reference implementation: `src/cdp.ts` and `src/actions.ts` in the
//! pi-computer-use repo (`/tmp/pi-computer-use`): ports `CdpTab`,
//! `cdpSnapshotOutline`, `browserActionsForAxRole`, and the action mappings.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use futures::sink::SinkExt;
use futures::stream::{SplitSink, SplitStream, StreamExt};
use image::imageops::FilterType;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::backend::{ActOutcome, BackendError, TextPage, UiBackend};
use crate::model::{
    Action, Bounds, FindRootsRequest, ImageCapture, ObserveMode, ObserveRequest, Point, RootInfo,
    UiNode, UiSnapshot,
};

/// Upper bound on the wire-ref'd backend node id space (backendDOMNodeId fits in i64).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const NAVIGATE_READY_WAIT: Duration = Duration::from_secs(10);
const LAUNCH_READY_WAIT: Duration = Duration::from_secs(15);
/// Total AX-tree nodes kept in one outline.
const MAX_OUTLINE_NODES: usize = 2000;
/// Sibling cap applied to each node's children list.
const MAX_CHILDREN: usize = 30;
/// Cap on a single node's `value` / `title`-adjacent text.
const BODY_TEXT_CAP_CHARS: usize = 32 * 1024;
/// Cap on `evaluate` results returned to callers.
const EVAL_CAP_CHARS: usize = 32 * 1024;

type CdpStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Options for the CDP backend.
#[derive(Debug, Clone)]
pub struct CdpOptions {
    /// Remote debugging port (from `COMPUTER_USE_CDP_PORT`).
    pub port: u16,
    /// Browser executable used by `launch_browser` when no endpoint is live.
    pub browser_path: Option<PathBuf>,
    pub headless: bool,
}

impl Default for CdpOptions {
    fn default() -> Self {
        Self {
            port: 9222,
            browser_path: None,
            headless: false,
        }
    }
}

/// One page target discovered via `/json/list`.
#[derive(Debug, Clone)]
struct PageTarget {
    id: String,
    title: String,
    ws_url: String,
}

/// Internal CDP failure modes, kept distinct so per-element failures (e.g. a
/// node that was removed from the DOM) can be reported as an honest
/// [`ActOutcome`] while transport failures abort the batch.
#[derive(Debug)]
enum CdpError {
    Closed,
    Timeout(String),
    Transport(String),
    Protocol(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "CDP connection closed"),
            Self::Timeout(method) => write!(f, "CDP command '{method}' timed out after 5s"),
            Self::Transport(reason) => write!(f, "CDP transport error: {reason}"),
            Self::Protocol(message) => write!(f, "CDP error: {message}"),
        }
    }
}

impl std::error::Error for CdpError {}

impl From<CdpError> for BackendError {
    fn from(error: CdpError) -> Self {
        BackendError::Failed(error.to_string())
    }
}

/// A live websocket session to one page target. Responses are correlated to
/// pending requests by a monotonically increasing id; the reader task owns the
/// read half and resolves pending oneshot channels as responses arrive.
struct CdpSession {
    target_id: String,
    next_id: AtomicU64,
    writer: Mutex<SplitSink<CdpStream, Message>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    closed: AtomicBool,
}

impl CdpSession {
    async fn connect(ws_url: &str, target_id: &str) -> Result<Arc<CdpSession>, BackendError> {
        let connection = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(ws_url))
            .await
            .map_err(|_| {
                BackendError::Failed(format!("timed out connecting to CDP target {ws_url}"))
            })?
            .map_err(|error| {
                BackendError::Failed(format!("failed to connect to CDP target {ws_url}: {error}"))
            })?;
        let (socket, _response) = connection;
        let (writer, reader) = socket.split();
        let session = Arc::new(CdpSession {
            target_id: target_id.to_string(),
            next_id: AtomicU64::new(1),
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        session
            .send_raw("Runtime.enable", json!({}))
            .await
            .map_err(BackendError::from)?;
        session
            .send_raw("Page.enable", json!({}))
            .await
            .map_err(BackendError::from)?;
        session
            .send_raw("Accessibility.enable", json!({}))
            .await
            .map_err(BackendError::from)?;
        session
            .send_raw("DOM.enable", json!({}))
            .await
            .map_err(BackendError::from)?;
        spawn_reader(reader, Arc::clone(&session));
        Ok(session)
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send a JSON-RPC command and await its response (5s bound).
    async fn send_raw(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        if self.is_closed() {
            return Err(CdpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let payload = json!({ "id": id, "method": method, "params": params });
        let message = Message::Text(payload.to_string().into());
        let sent = {
            let mut writer = self.writer.lock().await;
            writer.send(message).await
        };
        if let Err(error) = sent {
            self.pending.lock().await.remove(&id);
            self.closed.store(true, Ordering::SeqCst);
            return Err(CdpError::Transport(format!("{method}: {error}")));
        }
        match tokio::time::timeout(COMMAND_TIMEOUT, receiver).await {
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(CdpError::Timeout(method.into()))
            }
            Ok(Err(_)) => Err(CdpError::Closed),
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => Err(CdpError::Protocol(message)),
        }
    }

    /// Evaluate a JS expression in the page, returning its `returnByValue`
    /// value (or `Null` for `undefined`).
    async fn evaluate(&self, expression: &str) -> Result<Value, CdpError> {
        let response = self
            .send_raw("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }))
            .await?;
        if let Some(details) = response.get("exceptionDetails") {
            let message = details
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("evaluate threw");
            return Err(CdpError::Protocol(format!("evaluate threw: {message}")));
        }
        Ok(response
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Call a function on a resolved object id, returning its `returnByValue`
    /// value (or `Null` for `undefined`).
    async fn evaluate_function(
        &self,
        object_id: &str,
        function_declaration: &str,
        args: Vec<Value>,
    ) -> Result<Value, CdpError> {
        let response = self
            .send_raw(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": function_declaration,
                    "arguments": args.into_iter().map(|value| json!({ "value": value })).collect::<Vec<_>>(),
                    "returnByValue": true
                }),
            )
            .await?;
        if let Some(details) = response.get("exceptionDetails") {
            let message = details
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("callFunctionOn threw");
            return Err(CdpError::Protocol(format!("callFunctionOn threw: {message}")));
        }
        Ok(response
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Resolve `cdp:<backendNodeId>` to an `objectId`. Returns `Ok(None)`
    /// when the node has been removed from the DOM (a protocol-level error),
    /// which callers map to an honest `Didnt` outcome.
    async fn resolve_object_id(&self, backend_node_id: i64) -> Result<Option<String>, CdpError> {
        match self
            .send_raw("DOM.resolveNode", json!({ "backendNodeId": backend_node_id }))
            .await
        {
            Ok(response) => Ok(response
                .get("object")
                .and_then(|object| object.get("objectId"))
                .and_then(Value::as_str)
                .map(str::to_string)),
            Err(CdpError::Protocol(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Dispatch a decoded websocket message: resolve pending requests by id;
    /// events (`Page.*`, `Runtime.*`, ...) are intentionally ignored because
    /// navigate polls `document.readyState` instead.
    async fn handle_message(&self, message: Message) {
        let text = match message {
            Message::Text(text) => text.to_string(),
            _ => return,
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
            let outcome = if let Some(error) = parsed.get("error") {
                Err(error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown CDP error")
                    .to_string())
            } else {
                Ok(parsed
                    .get("result")
                    .cloned()
                    .unwrap_or(Value::Null))
            };
            if let Some(sender) = self.pending.lock().await.remove(&id) {
                let _ = sender.send(outcome);
            }
        }
    }

    async fn reject_all_pending(&self) {
        let pending = std::mem::take(&mut *self.pending.lock().await);
        for (_, sender) in pending {
            let _ = sender.send(Err("CDP connection closed".into()));
        }
    }
}

/// Own the read half of the session socket; resolve responses and mark the
/// session closed when the stream ends.
fn spawn_reader(reader: SplitStream<CdpStream>, session: Arc<CdpSession>) {
    tokio::spawn(async move {
        let mut reader = reader;
        while let Some(message) = reader.next().await {
            let Ok(message) = message else { continue };
            session.handle_message(message).await;
        }
        session.closed.store(true, Ordering::SeqCst);
        session.reject_all_pending().await;
    });
}

/// CDP browser backend.
pub struct CdpBackend {
    options: CdpOptions,
    client: reqwest::Client,
    sessions: Mutex<HashMap<String, Arc<CdpSession>>>,
    page_ws_urls: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for CdpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdpBackend")
            .field("port", &self.options.port)
            .finish()
    }
}

impl CdpBackend {
    pub fn new(options: CdpOptions) -> Self {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            options,
            client,
            sessions: Mutex::new(HashMap::new()),
            page_ws_urls: Mutex::new(HashMap::new()),
        }
    }

    fn effective_port(&self) -> u16 {
        if self.options.port == 0 {
            9222
        } else {
            self.options.port
        }
    }

    fn endpoint_unreachable(&self, cause: impl std::fmt::Display) -> BackendError {
        let port = self.effective_port();
        BackendError::Failed(format!(
            "浏览器 CDP 端点不可达（http://127.0.0.1:{port}），请启动带 --remote-debugging-port={port} 的浏览器或设置 COMPUTER_USE_CDP_PORT（{cause}）"
        ))
    }

    /// `GET /json/list`, keeping only `type == "page"` targets with a local
    /// debugger websocket on our port. Refreshes the cached ws-url map.
    async fn discover_pages(&self) -> Result<Vec<PageTarget>, BackendError> {
        let port = self.effective_port();
        let response = self
            .client
            .get(format!("http://127.0.0.1:{port}/json/list"))
            .send()
            .await
            .map_err(|error| self.endpoint_unreachable(error))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| BackendError::Failed(format!("CDP /json/list 读取失败: {error}")))?;
        if !status.is_success() {
            return Err(BackendError::Failed(format!(
                "CDP /json/list 返回 HTTP {status}"
            )));
        }
        let targets: Vec<Value> = serde_json::from_str(&text)
            .map_err(|error| BackendError::Failed(format!("CDP /json/list 解析失败: {error}")))?;
        let mut pages = Vec::new();
        let mut ws_by_id = HashMap::new();
        for target in targets {
            if target.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let Some(ws_url) = target.get("webSocketDebuggerUrl").and_then(Value::as_str) else {
                continue;
            };
            if !is_local_debugger_ws(ws_url, port) {
                continue;
            }
            let Some(id) = target.get("id").and_then(Value::as_str) else {
                continue;
            };
            let id = id.to_string();
            pages.push(PageTarget {
                id: id.clone(),
                title: target
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                ws_url: ws_url.to_string(),
            });
            ws_by_id.insert(id, ws_url.to_string());
        }
        *self.page_ws_urls.lock().await = ws_by_id;
        Ok(pages)
    }

    /// `GET /json/version` — used to decide whether a browser is already
    /// listening on the configured port.
    async fn endpoint_alive(&self) -> bool {
        let port = self.effective_port();
        matches!(
            self.client
                .get(format!("http://127.0.0.1:{port}/json/version"))
                .send()
                .await,
            Ok(response) if response.status().is_success()
        )
    }

    /// Get or lazily connect a session for a page target id. Closed sessions
    /// are replaced; the cached ws url is re-discovered when it goes stale.
    async fn session_for_target(&self, target_id: &str) -> Result<Arc<CdpSession>, BackendError> {
        {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get(target_id)
                && !session.is_closed()
            {
                return Ok(Arc::clone(session));
            }
        }
        if let Some(cached_url) = self.page_ws_urls.lock().await.get(target_id).cloned()
            && let Ok(session) = CdpSession::connect(&cached_url, target_id).await
        {
            self.sessions
                .lock()
                .await
                .insert(target_id.to_string(), Arc::clone(&session));
            return Ok(session);
        }
        let pages = self.discover_pages().await?;
        let page = pages
            .iter()
            .find(|page| page.id == target_id)
            .ok_or_else(|| {
                BackendError::Failed(format!("CDP target {target_id} no longer exists"))
            })?;
        let session = CdpSession::connect(&page.ws_url, target_id).await?;
        self.sessions
            .lock()
            .await
            .insert(target_id.to_string(), Arc::clone(&session));
        Ok(session)
    }

    async fn session_for_root(&self, root: &RootInfo) -> Result<Arc<CdpSession>, BackendError> {
        let target_id = root
            .resource_key
            .strip_prefix("cdp:")
            .ok_or_else(|| {
                BackendError::Failed(format!(
                    "root is not a CDP page: {}",
                    root.resource_key
                ))
            })?;
        self.session_for_target(target_id).await
    }

    /// Capture a PNG screenshot of the page, downscaling to
    /// `max_dimension` (longest edge) when requested. Failures yield `None`:
    /// the image is optional evidence for `Fused` observations.
    async fn capture_image(
        &self,
        session: &CdpSession,
        max_dimension: Option<u32>,
    ) -> Result<Option<ImageCapture>, BackendError> {
        let response = session
            .send_raw("Page.captureScreenshot", json!({ "format": "png" }))
            .await;
        let response = match response {
            Ok(response) => response,
            Err(_) => return Ok(None),
        };
        let Some(data) = response.get("data").and_then(Value::as_str) else {
            return Ok(None);
        };
        let data = data.to_string();
        let decoded = tokio::task::spawn_blocking(move || -> Result<ImageCapture, String> {
            let bytes = STANDARD
                .decode(data.as_bytes())
                .map_err(|error| error.to_string())?;
            let image = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
            let (width, height) = (image.width(), image.height());
            if let Some(max_dimension) = max_dimension.filter(|max| *max > 0) {
                let longest = width.max(height);
                if longest > max_dimension {
                    let scale = max_dimension as f64 / longest as f64;
                    let new_width = ((width as f64 * scale).round() as u32).max(1);
                    let new_height = ((height as f64 * scale).round() as u32).max(1);
                    let resized = image.resize(new_width, new_height, FilterType::Triangle);
                    let mut buffer = Vec::new();
                    resized
                        .write_to(&mut std::io::Cursor::new(&mut buffer), image::ImageFormat::Png)
                        .map_err(|error| error.to_string())?;
                    return Ok(ImageCapture {
                        mime_type: "image/png".into(),
                        base64: STANDARD.encode(&buffer),
                        width: new_width,
                        height: new_height,
                    });
                }
            }
            Ok(ImageCapture {
                mime_type: "image/png".into(),
                base64: data,
                width,
                height,
            })
        })
        .await
        .map_err(|error| BackendError::Failed(format!("screenshot decode task failed: {error}")))?;
        Ok(Some(decoded.map_err(BackendError::Failed)?))
    }

    /// Convert a screen-space point to viewport CSS pixels by subtracting the
    /// browser window frame origin when it is available.
    async fn to_viewport_point(
        &self,
        session: &CdpSession,
        x: f64,
        y: f64,
    ) -> (f64, f64) {
        let bounds = session
            .send_raw("Browser.getWindowForTarget", json!({ "targetId": session.target_id }))
            .await
            .ok()
            .and_then(|response| response.get("bounds").cloned());
        match bounds {
            Some(bounds) => {
                let left = bounds.get("left").and_then(Value::as_f64).unwrap_or(0.0);
                let top = bounds.get("top").and_then(Value::as_f64).unwrap_or(0.0);
                ((x - left).max(0.0), (y - top).max(0.0))
            }
            None => (x, y),
        }
    }

    async fn dispatch_mouse(
        &self,
        session: &CdpSession,
        event_type: &str,
        x: f64,
        y: f64,
        button: &str,
        click_count: u8,
    ) -> Result<(), BackendError> {
        let button = if event_type == "mouseMoved" { "none" } else { button };
        session
            .send_raw(
                "Input.dispatchMouseEvent",
                json!({
                    "type": event_type,
                    "x": x.round(),
                    "y": y.round(),
                    "button": button,
                    "clickCount": click_count,
                }),
            )
            .await?;
        Ok(())
    }

    /// Click/Press a wire-ref'd element. The outcome is grounded in re-read
    /// state: a changed `checked` / `value` / `href` / URL / focus counts as
    /// [`ActOutcome::Worked`]; an unchanged but still-live element is
    /// [`ActOutcome::Unknown`]; a dispatch failure is [`ActOutcome::Didnt`].
    async fn act_click(
        &self,
        session: &CdpSession,
        wire_ref: Option<&str>,
    ) -> Result<ActOutcome, BackendError> {
        let Some(wire_ref) = wire_ref else {
            return Ok(ActOutcome::Didnt);
        };
        let Some(node_id) = parse_wire_ref(wire_ref) else {
            return Ok(ActOutcome::Didnt);
        };
        let Some(object_id) = session.resolve_object_id(node_id).await? else {
            return Ok(ActOutcome::Didnt);
        };
        let before = session
            .evaluate_function(object_id.as_str(), CLICK_STATE_FN, vec![])
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if session
            .evaluate_function(object_id.as_str(), CLICK_FN, vec![])
            .await
            .is_err()
        {
            return Ok(ActOutcome::Didnt);
        }
        let after = session
            .evaluate_function(object_id.as_str(), CLICK_STATE_FN, vec![])
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if !after.is_empty() && after != before {
            Ok(ActOutcome::Worked)
        } else {
            Ok(ActOutcome::Unknown)
        }
    }

    async fn act_set_text(
        &self,
        session: &CdpSession,
        wire_ref: Option<&str>,
        text: &str,
    ) -> Result<ActOutcome, BackendError> {
        let Some(wire_ref) = wire_ref else {
            return Ok(ActOutcome::Didnt);
        };
        let Some(node_id) = parse_wire_ref(wire_ref) else {
            return Ok(ActOutcome::Didnt);
        };
        let Some(object_id) = session.resolve_object_id(node_id).await? else {
            return Ok(ActOutcome::Didnt);
        };
        if session
            .evaluate_function(object_id.as_str(), SET_TEXT_FN, vec![json!(text)])
            .await
            .is_err()
        {
            return Ok(ActOutcome::Didnt);
        }
        let actual = session
            .evaluate_function(object_id.as_str(), GET_VALUE_FN, vec![])
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if actual == text {
            Ok(ActOutcome::Worked)
        } else {
            Ok(ActOutcome::Didnt)
        }
    }

    async fn act_type_text(
        &self,
        session: &CdpSession,
        wire_ref: Option<&str>,
        text: &str,
    ) -> Result<ActOutcome, BackendError> {
        let before = session
            .evaluate(FOCUSED_VALUE_EXPR)
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if let Some(wire_ref) = wire_ref {
            let Some(node_id) = parse_wire_ref(wire_ref) else {
                return Ok(ActOutcome::Didnt);
            };
            let Some(object_id) = session.resolve_object_id(node_id).await? else {
                return Ok(ActOutcome::Didnt);
            };
            if session
                .evaluate_function(object_id.as_str(), FOCUS_FN, vec![])
                .await
                .is_err()
            {
                return Ok(ActOutcome::Didnt);
            }
        }
        if session
            .send_raw("Input.insertText", json!({ "text": text }))
            .await
            .is_err()
        {
            return Ok(ActOutcome::Didnt);
        }
        let after = session
            .evaluate(FOCUSED_VALUE_EXPR)
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if after != before {
            Ok(ActOutcome::Worked)
        } else {
            Ok(ActOutcome::Didnt)
        }
    }

    async fn act_keypress(
        &self,
        session: &CdpSession,
        wire_ref: Option<&str>,
        keys: &[String],
    ) -> Result<ActOutcome, BackendError> {
        if let Some(wire_ref) = wire_ref {
            let Some(node_id) = parse_wire_ref(wire_ref) else {
                return Ok(ActOutcome::Didnt);
            };
            let Some(object_id) = session.resolve_object_id(node_id).await? else {
                return Ok(ActOutcome::Didnt);
            };
            if session
                .evaluate_function(object_id.as_str(), FOCUS_FN, vec![])
                .await
                .is_err()
            {
                return Ok(ActOutcome::Didnt);
            }
        }
        let before = session
            .evaluate(FOCUSED_VALUE_EXPR)
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        for (key, event_type, modifiers, text) in key_events(keys) {
            let mut params = json!({
                "type": event_type,
                "key": key,
                "code": key,
                "modifiers": modifiers,
            });
            if let Some(text) = text {
                params["text"] = json!(text);
            }
            if session
                .send_raw("Input.dispatchKeyEvent", params)
                .await
                .is_err()
            {
                return Ok(ActOutcome::Didnt);
            }
        }
        let after = session
            .evaluate(FOCUSED_VALUE_EXPR)
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if after != before {
            Ok(ActOutcome::Worked)
        } else {
            Ok(ActOutcome::Unknown)
        }
    }

    async fn act_scroll_element(
        &self,
        session: &CdpSession,
        wire_ref: &str,
        delta_x: f64,
        delta_y: f64,
    ) -> Result<ActOutcome, BackendError> {
        let Some(node_id) = parse_wire_ref(wire_ref) else {
            return Ok(ActOutcome::Didnt);
        };
        let Some(object_id) = session.resolve_object_id(node_id).await? else {
            return Ok(ActOutcome::Didnt);
        };
        let before = session
            .evaluate_function(object_id.as_str(), SCROLL_POS_FN, vec![])
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if session
            .evaluate_function(object_id.as_str(), SCROLL_BY_FN, vec![json!(delta_x), json!(delta_y)])
            .await
            .is_err()
        {
            return Ok(ActOutcome::Didnt);
        }
        let after = session
            .evaluate_function(object_id.as_str(), SCROLL_POS_FN, vec![])
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if !after.is_empty() && after != before {
            Ok(ActOutcome::Worked)
        } else {
            Ok(ActOutcome::Unknown)
        }
    }

    async fn act_scroll_wheel(
        &self,
        session: &CdpSession,
        delta_x: f64,
        delta_y: f64,
    ) -> Result<ActOutcome, BackendError> {
        let before = session
            .evaluate("(() => (window.scrollX + ',' + window.scrollY))()")
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if session
            .send_raw(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseWheel", "deltaX": delta_x, "deltaY": delta_y }),
            )
            .await
            .is_err()
        {
            return Ok(ActOutcome::Didnt);
        }
        let after = session
            .evaluate("(() => (window.scrollX + ',' + window.scrollY))()")
            .await
            .map(|value| string_value(&value))
            .unwrap_or_default();
        if !after.is_empty() && after != before {
            Ok(ActOutcome::Worked)
        } else {
            Ok(ActOutcome::Unknown)
        }
    }

    async fn act_mouse_click(
        &self,
        session: &CdpSession,
        x: f64,
        y: f64,
        button: Option<&str>,
        click_count: u8,
    ) -> Result<ActOutcome, BackendError> {
        let button = match button {
            Some("right") => "right",
            Some("middle") => "middle",
            _ => "left",
        };
        self.dispatch_mouse(session, "mousePressed", x, y, button, click_count.max(1))
            .await?;
        self.dispatch_mouse(session, "mouseReleased", x, y, button, click_count.max(1))
            .await?;
        Ok(ActOutcome::Unknown)
    }

    async fn act_drag(
        &self,
        session: &CdpSession,
        path: &[Point],
    ) -> Result<ActOutcome, BackendError> {
        if path.len() < 2 {
            return Ok(ActOutcome::Didnt);
        }
        let mut points = Vec::with_capacity(path.len());
        for point in path {
            let (x, y) = self.to_viewport_point(session, point.x, point.y).await;
            points.push((x, y));
        }
        let (first_x, first_y) = points[0];
        self.dispatch_mouse(session, "mousePressed", first_x, first_y, "left", 1)
            .await?;
        for (x, y) in &points[1..] {
            session
                .send_raw(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseMoved",
                        "x": x.round(),
                        "y": y.round(),
                        "button": "left",
                        "buttons": 1,
                    }),
                )
                .await?;
        }
        let (last_x, last_y) = *points.last().unwrap_or(&(first_x, first_y));
        self.dispatch_mouse(session, "mouseReleased", last_x, last_y, "left", 1)
            .await?;
        Ok(ActOutcome::Unknown)
    }

    async fn perform_action(
        &self,
        session: &CdpSession,
        action: &Action,
    ) -> Result<ActOutcome, BackendError> {
        match action {
            Action::Press { wire_ref, .. } => self.act_click(session, wire_ref.as_deref()).await,
            Action::Click {
                wire_ref,
                x,
                y,
                button,
                click_count,
                ..
            } => {
                if let Some(wire_ref) = wire_ref.as_deref() {
                    self.act_click(session, Some(wire_ref)).await
                } else if let (Some(x), Some(y)) = (x, y) {
                    let (x, y) = self.to_viewport_point(session, *x, *y).await;
                    self.act_mouse_click(session, x, y, button.as_deref(), click_count.unwrap_or(1))
                        .await
                } else {
                    Ok(ActOutcome::Didnt)
                }
            }
            Action::SetText { wire_ref, text, .. } => {
                self.act_set_text(session, wire_ref.as_deref(), text).await
            }
            Action::TypeText { wire_ref, text, .. } => {
                self.act_type_text(session, wire_ref.as_deref(), text).await
            }
            Action::Keypress { wire_ref, keys, .. } => {
                self.act_keypress(session, wire_ref.as_deref(), keys).await
            }
            Action::Scroll {
                wire_ref,
                scroll_x,
                scroll_y,
                ..
            } => {
                if let Some(wire_ref) = wire_ref.as_deref() {
                    self.act_scroll_element(session, wire_ref, *scroll_x, *scroll_y).await
                } else {
                    self.act_scroll_wheel(session, *scroll_x, *scroll_y).await
                }
            }
            Action::Drag { path } => self.act_drag(session, path).await,
            Action::MoveMouse { x, y } => {
                let (x, y) = self.to_viewport_point(session, *x, *y).await;
                self.dispatch_mouse(session, "mouseMoved", x, y, "none", 1).await?;
                Ok(ActOutcome::Unknown)
            }
        }
    }

    /// Probe `browser_path` first, then platform default browser locations.
    fn resolve_browser_executable(&self) -> Result<PathBuf, BackendError> {
        if let Some(path) = &self.options.browser_path {
            if path.exists() {
                return Ok(path.clone());
            }
            return Err(BackendError::Failed(format!(
                "configured browser path does not exist: {}",
                path.display()
            )));
        }
        #[cfg(target_os = "macos")]
        const CANDIDATES: &[&str] = &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Arc.app/Contents/MacOS/Arc",
        ];
        #[cfg(target_os = "linux")]
        const CANDIDATES: &[&str] = &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ];
        #[cfg(target_os = "windows")]
        const CANDIDATES: &[&str] = &["chrome.exe", "msedge.exe", "brave.exe", "chromium.exe"];
        for candidate in CANDIDATES {
            if let Some(path) = find_in_path(candidate) {
                return Ok(path);
            }
            let path = PathBuf::from(candidate);
            if path.is_absolute() && path.exists() {
                return Ok(path);
            }
        }
        #[cfg(target_os = "windows")]
        {
            for base in [
                "C:\\Program Files\\Google\\Chrome\\Application",
                "C:\\Program Files (x86)\\Google\\Chrome\\Application",
                "C:\\Program Files\\Microsoft\\Edge\\Application",
                "C:\\Program Files (x86)\\Microsoft\\Edge\\Application",
            ] {
                for name in ["chrome.exe", "msedge.exe"] {
                    let path = std::path::Path::new(base).join(name);
                    if path.exists() {
                        return Ok(path);
                    }
                }
            }
        }
        Err(BackendError::Failed(
            "未配置浏览器可执行文件：请设置 browser_path（或 COMPUTER_USE_BROWSER_PATH）或安装 Chrome/Chromium/Edge".into(),
        ))
    }
}

// --- JS helpers used by `act` verification / delivery ---

/// Scroll the element into view and dispatch a click.
const CLICK_FN: &str = "function(){ this.scrollIntoView({block:'center', inline:'center'}); this.click(); return true; }";

/// Re-readable click evidence: checked state, value, href/URL and focus.
const CLICK_STATE_FN: &str = "function(){ var el = this; var active = (document.activeElement === el) || (!!el.contains && el.contains(document.activeElement)); return JSON.stringify({ checked: ('checked' in el) ? el.checked : null, value: ('value' in el) ? el.value : null, href: el.href || null, url: location.href, focused: active }); }";

/// Focus an element and set its text (value or textContent), dispatching
/// `input` + `change` events.
const SET_TEXT_FN: &str = "function(text){ this.scrollIntoView({block:'center', inline:'center'}); this.focus(); if ('value' in this) { this.value = text; } else { this.textContent = text; } this.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:'insertText', data:text})); this.dispatchEvent(new Event('change', {bubbles:true})); return true; }";

/// Current value / textContent of an element.
const GET_VALUE_FN: &str = "function(){ if ('value' in this) return this.value; if (this.textContent) return this.textContent; return ''; }";

/// Focus an element (used before typing / keypress on a wire-ref'd element).
const FOCUS_FN: &str = "function(){ this.scrollIntoView({block:'center', inline:'center'}); this.focus(); return true; }";

/// Value / textContent of the currently focused element.
const FOCUSED_VALUE_EXPR: &str = "(() => { const el = document.activeElement; if (!el) return ''; if ('value' in el) return el.value || ''; return el.textContent || ''; })()";

/// `scrollLeft,scrollTop` of an element.
const SCROLL_POS_FN: &str = "function(){ var x = this.scrollLeft || 0; var y = this.scrollTop || 0; return x + ',' + y; }";

/// Scroll an element by deltas.
const SCROLL_BY_FN: &str = "function(dx, dy){ this.scrollIntoView({block:'center', inline:'center'}); this.scrollBy(dx, dy); return true; }";

/// `innerText`/`value`/`textContent` of an element, used by `read_text`.
const READ_TEXT_FN: &str = "function(){ if (this.innerText) return this.innerText; if ('value' in this) return this.value || ''; return this.textContent || ''; }";

// --- free helpers ---

fn parse_wire_ref(wire_ref: &str) -> Option<i64> {
    wire_ref.strip_prefix("cdp:").and_then(|id| id.parse::<i64>().ok())
}

fn string_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn truncate_chars(text: String, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text;
    }
    text.chars().take(cap).collect()
}

/// Normalize a CDP AX role name to the canonical lowercase outline roles used
/// by the pi outline mapping.
fn normalize_role(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "rootwebarea" | "webarea" => "document".into(),
        "statictext" => "text".into(),
        "genericcontainer" => "generic".into(),
        other => other.to_string(),
    }
}

/// Port of `browserActionsForAxRole` from the pi reference.
fn browser_actions_for_role(role: &str) -> Vec<String> {
    match role {
        "button" | "link" | "checkbox" | "radio" | "menuitem" | "tab" => vec!["click".into()],
        "textbox" | "searchbox" | "combobox" => vec!["click".into(), "set_text".into()],
        "listbox" | "slider" | "spinbutton" => vec!["click".into()],
        _ => Vec::new(),
    }
}

/// Read `raw[key]` as an AX value box (`{value: "..."}`) or a plain string.
fn ax_string(raw: &Value, key: &str) -> String {
    let Some(field) = raw.get(key) else {
        return String::new();
    };
    let value = field.get("value").unwrap_or(field);
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

/// Check an AX `properties` entry for a boolean property (e.g. `focused`).
fn ax_bool_property(raw: &Value, name: &str) -> bool {
    raw.get("properties")
        .and_then(Value::as_array)
        .is_some_and(|properties| {
            properties.iter().any(|property| {
                property.get("name").and_then(Value::as_str) == Some(name)
                    && property
                        .get("value")
                        .and_then(|value| value.get("value"))
                        .and_then(Value::as_bool)
                        == Some(true)
            })
        })
}

/// CDP `nodeId` values are numbers (or strings on some transports).
fn node_id_string(value: &Value) -> Option<String> {
    value
        .as_u64()
        .map(|id| id.to_string())
        .or_else(|| value.as_str().map(str::to_string))
}

/// Trust only localhost debugger sockets on the configured port.
fn is_local_debugger_ws(ws_url: &str, port: u16) -> bool {
    let Ok(parsed) = url::Url::parse(ws_url) else {
        return false;
    };
    if parsed.scheme() != "ws" && parsed.scheme() != "wss" {
        return false;
    }
    let host_ok = parsed
        .host_str()
        .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1"));
    host_ok && parsed.port() == Some(port)
}

fn is_modifier(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "alt" | "option" | "control" | "ctrl" | "meta" | "command" | "cmd" | "shift"
    )
}

/// Port of `CdpTab.keypress`: compute the modifier bitmask from the whole
/// list, then emit `keyDown`/`keyUp` pairs (with `text` on printable keys).
/// Returns `(key, event_type, modifiers, text)` tuples.
fn key_events(keys: &[String]) -> Vec<(String, String, i32, Option<String>)> {
    let modifier_bit = |key: &str| -> i32 {
        match key.to_ascii_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "command" | "cmd" => 4,
            "shift" => 8,
            _ => 0,
        }
    };
    let mut modifiers = 0i32;
    for key in keys {
        modifiers |= modifier_bit(key);
    }
    let mut events = Vec::new();
    for key in keys {
        if is_modifier(key) {
            continue;
        }
        let text = if key.chars().count() == 1 && modifiers == 0 {
            Some(key.clone())
        } else {
            None
        };
        let key = key.clone();
        events.push((key.clone(), "keyDown".to_string(), modifiers, text.clone()));
        events.push((key, "keyUp".to_string(), modifiers, None));
    }
    events
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    let separator = if cfg!(windows) { ';' } else { ':' };
    for dir in path_var.split(separator) {
        if dir.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// --- outline building ---

/// Build the observation outline from a `Accessibility.getFullAXTree` node
/// array, rooting at the `RootWebArea`/`WebArea` node (synthesizing a
/// `document` root when absent) and assigning observation-scoped `@eN`
/// element refs.
fn build_ax_outline(
    nodes: &[Value],
    root_value: String,
    page_title: &str,
    frame: Option<Bounds>,
) -> UiNode {
    let records: HashMap<String, &Value> = nodes
        .iter()
        .filter_map(|node| {
            node.get("nodeId")
                .and_then(node_id_string)
                .map(|id| (id, node))
        })
        .collect();

    let root_raw = nodes.iter().find(|node| {
        let role = node
            .get("role")
            .and_then(|role| role.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("");
        matches!(role, "RootWebArea" | "WebArea")
    });

    let mut counter = 0usize;
    let mut root = if let Some(root_raw) = root_raw {
        let mut node = build_ax_node(root_raw, &records, &mut counter).unwrap_or_else(|| {
            synthetic_root_node(page_title, root_value.clone(), frame)
        });
        node.value = root_value;
        if node.title.is_empty() {
            node.title = page_title.to_string();
        }
        if let Some(frame) = frame {
            node.bounds = Some(Bounds { x: 0.0, y: 0.0, w: frame.w, h: frame.h });
        }
        node
    } else {
        // No document root reported: group top-level (never-referenced) nodes.
        let mut referenced = HashSet::new();
        for node in nodes {
            if let Some(child_ids) = node.get("childIds").and_then(Value::as_array) {
                for child_id in child_ids {
                    if let Some(id) = node_id_string(child_id) {
                        referenced.insert(id);
                    }
                }
            }
        }
        let mut children = Vec::new();
        let mut truncated = false;
        for raw in nodes {
            if counter >= MAX_OUTLINE_NODES {
                truncated = true;
                break;
            }
            let Some(id) = raw.get("nodeId").and_then(node_id_string) else {
                continue;
            };
            if referenced.contains(&id) {
                continue;
            }
            if raw.get("ignored").and_then(Value::as_bool).unwrap_or(false) {
                collect_grafted(raw, &records, &mut counter, &mut truncated, &mut children);
                continue;
            }
            if children.len() >= MAX_CHILDREN {
                truncated = true;
                break;
            }
            if let Some(child) = build_ax_node(raw, &records, &mut counter) {
                children.push(child);
            }
        }
        let mut node = synthetic_root_node(page_title, root_value, frame);
        node.children = children;
        node.truncated = truncated;
        node
    };
    // Make sure the root itself carries a wire ref when possible so the
    // service can key on it in diffs.
    if root.element_ref.is_empty() {
        root.element_ref = format!("@e{counter}");
    }
    root
}

fn synthetic_root_node(page_title: &str, root_value: String, frame: Option<Bounds>) -> UiNode {
    UiNode {
        element_ref: String::new(),
        wire_ref: None,
        role: "document".into(),
        subrole: String::new(),
        identifier: String::new(),
        title: page_title.to_string(),
        description: String::new(),
        value: root_value,
        actions: Vec::new(),
        can_press: false,
        can_focus: false,
        can_set_value: false,
        can_scroll: false,
        can_increment: false,
        can_decrement: false,
        is_text_input: false,
        bounds: frame.map(|frame| Bounds { x: 0.0, y: 0.0, w: frame.w, h: frame.h }),
        focused: false,
        offscreen: false,
        picture_only: false,
        truncated: false,
        scroll_extent: None,
        text: Vec::new(),
        children: Vec::new(),
    }
}

/// Recursively build a [`UiNode`] from one AX node. Assigns `@eN` element
/// refs, `cdp:<backendNodeId>` wire refs, and applies the sibling/total caps.
fn build_ax_node(
    raw: &Value,
    records: &HashMap<String, &Value>,
    counter: &mut usize,
) -> Option<UiNode> {
    if *counter >= MAX_OUTLINE_NODES {
        return None;
    }
    *counter += 1;
    let ref_index = *counter;

    let role = normalize_role(&ax_string(raw, "role"));
    let name = ax_string(raw, "name");
    let value = ax_string(raw, "value");
    let description = ax_string(raw, "description");
    let wire_ref = raw
        .get("backendDOMNodeId")
        .and_then(Value::as_i64)
        .map(|id| format!("cdp:{id}"));
    let actions = browser_actions_for_role(&role);
    let focused = ax_bool_property(raw, "focused");
    let offscreen = ax_bool_property(raw, "offscreen") || ax_bool_property(raw, "scrolledOut");

    let mut children = Vec::new();
    let mut truncated = false;
    if let Some(child_ids) = raw.get("childIds").and_then(Value::as_array) {
        for child_id in child_ids {
            if *counter >= MAX_OUTLINE_NODES {
                truncated = true;
                break;
            }
            let Some(child_raw) = node_id_string(child_id)
                .and_then(|id| records.get(&id).copied())
            else {
                continue;
            };
            if child_raw.get("ignored").and_then(Value::as_bool).unwrap_or(false) {
                // Invisible/offscreen nodes are folded: their children graft up.
                collect_grafted(child_raw, records, counter, &mut truncated, &mut children);
                continue;
            }
            if children.len() >= MAX_CHILDREN {
                truncated = true;
                break;
            }
            if let Some(child) = build_ax_node(child_raw, records, counter) {
                children.push(child);
            }
        }
    }

    // Fold uninformative leaves (no name, value, description, actions).
    let empty_leaf = children.is_empty()
        && name.is_empty()
        && value.is_empty()
        && description.is_empty()
        && actions.is_empty()
        && role != "document";
    if empty_leaf {
        return None;
    }

    let can_press = actions.iter().any(|action| action == "click");
    let can_set_value = actions.iter().any(|action| action == "set_text");

    Some(UiNode {
        element_ref: format!("@e{ref_index}"),
        wire_ref,
        role,
        subrole: String::new(),
        identifier: String::new(),
        title: name,
        description,
        value,
        actions,
        can_press,
        can_focus: can_press || can_set_value,
        can_set_value,
        can_scroll: false,
        can_increment: false,
        can_decrement: false,
        is_text_input: can_set_value,
        bounds: None,
        focused,
        offscreen,
        picture_only: false,
        truncated,
        scroll_extent: None,
        text: Vec::new(),
        children,
    })
}

/// Append the descendants of an ignored AX node to `out`, respecting the
/// sibling and total caps (the parent's `truncated` flag is set on cuts).
fn collect_grafted(
    raw: &Value,
    records: &HashMap<String, &Value>,
    counter: &mut usize,
    truncated: &mut bool,
    out: &mut Vec<UiNode>,
) {
    let Some(child_ids) = raw.get("childIds").and_then(Value::as_array) else {
        return;
    };
    for child_id in child_ids {
        if *counter >= MAX_OUTLINE_NODES {
            *truncated = true;
            return;
        }
        let Some(child_raw) = node_id_string(child_id)
            .and_then(|id| records.get(&id).copied())
        else {
            continue;
        };
        if child_raw.get("ignored").and_then(Value::as_bool).unwrap_or(false) {
            collect_grafted(child_raw, records, counter, truncated, out);
            continue;
        }
        if out.len() >= MAX_CHILDREN {
            *truncated = true;
            return;
        }
        if let Some(node) = build_ax_node(child_raw, records, counter) {
            out.push(node);
        }
    }
}

#[async_trait]
impl UiBackend for CdpBackend {
    async fn find_roots(&self, request: FindRootsRequest) -> Result<Vec<RootInfo>, BackendError> {
        // An unreachable endpoint means "no browser pages right now", not an
        // error: the composite backend merges desktop + browser roots and must
        // stay healthy when the browser is simply not running.
        let pages = match self.discover_pages().await {
            Ok(pages) => pages,
            Err(_) => return Ok(Vec::new()),
        };
        let mut roots = Vec::new();
        for page in pages {
            if let Some(kind) = &request.kind
                && kind != "browser_page"
            {
                continue;
            }
            if let Some(app) = &request.app
                && !app.eq_ignore_ascii_case("browser")
            {
                continue;
            }
            if let Some(text) = &request.text
                && !page.title.to_lowercase().contains(&text.to_lowercase())
            {
                continue;
            }
            if request.pid.is_some() || request.bundle_id.is_some() {
                continue;
            }
            roots.push(RootInfo {
                root_ref: String::new(),
                resource_key: format!("cdp:{}", page.id),
                kind: "browser_page".into(),
                title: page.title,
                app: Some("browser".into()),
                bundle_id: None,
                pid: None,
                window_id: None,
                role: Some("document".into()),
                subrole: None,
                z_order: 0,
                frame: None,
                scale_factor: 1.0,
                is_onscreen: true,
                is_focused: false,
                is_minimized: false,
                is_main: false,
                is_modal: false,
            });
        }
        Ok(roots)
    }

    async fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<UiSnapshot, BackendError> {
        let session = self.session_for_root(root).await?;
        let include_image = match request.include_image {
            Some(include) => include,
            None => matches!(request.mode, ObserveMode::Visual | ObserveMode::Fused),
        };

        let root_value = session
            .evaluate("document.body ? document.body.innerText : ''")
            .await
            .map(|value| truncate_chars(string_value(&value), BODY_TEXT_CAP_CHARS))
            .unwrap_or_default();

        let tree = session
            .send_raw("Accessibility.getFullAXTree", json!({}))
            .await?;
        let nodes = tree
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let frame = session
            .send_raw("Browser.getWindowForTarget", json!({ "targetId": session.target_id }))
            .await
            .ok()
            .and_then(|response| response.get("bounds").cloned())
            .and_then(|bounds| {
                let x = bounds.get("left").and_then(Value::as_f64)?;
                let y = bounds.get("top").and_then(Value::as_f64)?;
                let w = bounds.get("width").and_then(Value::as_f64)?;
                let h = bounds.get("height").and_then(Value::as_f64)?;
                Some(Bounds { x, y, w, h })
            });

        let outline = build_ax_outline(&nodes, root_value, &root.title, frame);

        let mut snapshot_root = root.clone();
        snapshot_root.frame = frame;

        let image = if include_image {
            self.capture_image(&session, request.max_dimension).await?
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
        let session = self.session_for_root(root).await?;
        let mut outcomes = Vec::with_capacity(actions.len());
        for action in actions {
            outcomes.push(self.perform_action(&session, action).await?);
        }
        Ok(outcomes)
    }

    async fn read_text(
        &self,
        root: &RootInfo,
        wire_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TextPage, BackendError> {
        let session = self.session_for_root(root).await?;
        let Some(node_id) = parse_wire_ref(wire_ref) else {
            return Err(BackendError::Failed(format!(
                "invalid CDP wire_ref: {wire_ref}"
            )));
        };
        let Some(object_id) = session.resolve_object_id(node_id).await? else {
            return Err(BackendError::Failed(format!(
                "element with wire_ref {wire_ref} no longer exists"
            )));
        };
        let value = session
            .evaluate_function(object_id.as_str(), READ_TEXT_FN, vec![])
            .await
            .map_err(BackendError::from)?;
        let text = string_value(&value);
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

    async fn navigate(&self, root: &RootInfo, url: &str) -> Result<(), BackendError> {
        let session = self.session_for_root(root).await?;
        let response = session
            .send_raw("Page.navigate", json!({ "url": url }))
            .await?;
        if let Some(error_text) = response.get("errorText").and_then(Value::as_str) {
            return Err(BackendError::Failed(format!(
                "Page.navigate 失败: {error_text}"
            )));
        }
        // Bounded wait for a real load; SPAs that never reach `complete` still
        // proceed after the cap.
        let deadline = Instant::now() + NAVIGATE_READY_WAIT;
        loop {
            let ready = session
                .evaluate("document.readyState")
                .await
                .map(|value| string_value(&value))
                .unwrap_or_default();
            if ready == "complete" || Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn evaluate(&self, root: &RootInfo, expression: &str) -> Result<String, BackendError> {
        let session = self.session_for_root(root).await?;
        let value = session.evaluate(expression).await.map_err(BackendError::from)?;
        let text = match value {
            Value::Null => "null".to_string(),
            Value::String(text) => text,
            other => other.to_string(),
        };
        Ok(truncate_chars(text, EVAL_CAP_CHARS))
    }

    // The workspace bans `std::process::Command::spawn` via
    // `clippy::disallowed_methods` (it prefers enrolling children with
    // xai_tty_utils). We must keep the spawn here per the CDP backend
    // contract: `tokio::process` is unavailable and the child intentionally
    // detaches (we neither wait nor kill it).
    #[allow(clippy::disallowed_methods)]
    async fn launch_browser(&self, url: Option<&str>) -> Result<Vec<RootInfo>, BackendError> {
        if self.endpoint_alive().await {
            return self.find_roots(FindRootsRequest::default()).await;
        }
        let executable = self.resolve_browser_executable()?;
        let port = self.effective_port();
        let user_data_dir = std::env::temp_dir().join(format!(
            "xai-computer-use-cdp-{}",
            std::process::id()
        ));
        let mut command = std::process::Command::new(&executable);
        command
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("about:blank")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if self.options.headless {
            command.arg("--headless=new");
        }
        // The child process detaches naturally; we neither wait nor kill it.
        command.spawn().map_err(|error| {
            BackendError::Failed(format!(
                "无法启动浏览器 {executable:?}: {error}"
            ))
        })?;

        let deadline = Instant::now() + LAUNCH_READY_WAIT;
        let mut ready = false;
        while Instant::now() < deadline {
            if self.endpoint_alive().await {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if !ready {
            return Err(BackendError::Failed(format!(
                "浏览器（{executable:?}）在 {LAUNCH_READY_WAIT:?} 内未就绪（端口 {port}）"
            )));
        }
        let roots = self.find_roots(FindRootsRequest::default()).await?;
        if let Some(url) = url
            && let Some(first) = roots.first()
            && let Ok(session) = self.session_for_root(first).await
        {
            let _ = session
                .send_raw("Page.navigate", json!({ "url": url }))
                .await;
        }
        Ok(roots)
    }
}
