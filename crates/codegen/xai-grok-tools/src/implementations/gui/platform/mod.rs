//! OS abstraction seam for the native GUI tools — ported from upstream codex
//! PR understudy-ai/codex-cua#3 (`core/src/tools/handlers/gui/platform.rs`).
//!
//! Zero harness dependencies: the engine (super::handler) drives every
//! platform operation through [`GuiPlatform`], all of which are synchronous
//! and blocking (callers wrap them in `tokio::task::spawn_blocking`).
//! `default_gui_platform()` dispatches on `cfg!` at compile time.

use std::collections::HashMap;
use std::io::BufRead;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use xai_tool_runtime::ToolError;

use crate::util::ProcessGroup;

use super::grounding::HelperRect;
use super::grounding::ObserveState;
use super::grounding::WindowSelector;
use super::readiness::GUI_TOOL_NAMES;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use super::readiness::GuiEnvironmentReadinessCheck;
use super::readiness::GuiEnvironmentReadinessSnapshot;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use super::readiness::GuiReadinessStatus;
use super::readiness::GuiToolCapability;
use super::wire::HelperCaptureContext;

// Compiled on Linux and, on every host, under `cfg(test)` so the module's
// pure-logic unit tests (no X server required) run off-Linux too; all x11rb
// code stays inside its `#[cfg(target_os = "linux")] mod x11` block.
#[cfg(any(target_os = "linux", test))]
pub(crate) mod platform_linux;
#[cfg(target_os = "macos")]
pub(crate) mod platform_macos;
// Compiled on Windows and, on every host, under `cfg(test)` so the module's
// pure-logic unit tests (no Win32 calls required) run off-Windows too; all
// FFI code stays inside its `#[cfg(target_os = "windows")] mod win32` block.
#[cfg(any(target_os = "windows", test))]
pub(crate) mod platform_windows;

pub(crate) const GUI_UNSUPPORTED_MESSAGE: &str = "Native GUI tools are not supported on this platform yet. They are available on macOS, on Linux (X11; native Wayland sessions must run under XWayland with DISPLAY set), and on Windows (an interactive desktop session is required).";

pub(crate) trait GuiPlatform: Send + Sync {
    fn readiness_snapshot(&self) -> GuiEnvironmentReadinessSnapshot;

    fn tool_capabilities(&self) -> HashMap<&'static str, GuiToolCapability> {
        GUI_TOOL_NAMES
            .into_iter()
            .map(|tool_name| {
                (
                    tool_name,
                    GuiToolCapability {
                        enabled: true,
                        reason: None,
                        targetless_only: false,
                    },
                )
            })
            .collect()
    }

    // Only the macOS backend calls this (the Linux and Windows backends are
    // in-process); the required-method declaration itself is unused on
    // Linux and Windows.
    #[cfg_attr(any(target_os = "linux", target_os = "windows"), allow(dead_code))]
    fn resolve_helper_binary(&self) -> Result<PathBuf, ToolError>;

    fn cleanup_input_state(&self) -> Result<(), ToolError> {
        Ok(())
    }

    /// Hide all regular applications except the target app and the host
    /// terminal.  Returns the PIDs that were hidden so they can be restored
    /// later via [`GuiPlatform::unhide_apps`].
    fn hide_other_apps(&self, _app: Option<&str>) -> Result<Vec<i32>, ToolError> {
        Ok(vec![])
    }

    /// Restore applications that were previously hidden by
    /// [`GuiPlatform::hide_other_apps`].
    fn unhide_apps(&self, _pids: &[i32]) -> Result<(), ToolError> {
        Ok(())
    }

    fn start_emergency_stop_monitor(&self) -> Result<Option<GuiEmergencyStopMonitor>, ToolError> {
        Ok(None)
    }

    fn capture_context(
        &self,
        app: Option<&str>,
        activate_app: bool,
        window_selection: Option<&WindowSelector>,
    ) -> Result<HelperCaptureContext, ToolError>;

    fn capture_region(
        &self,
        bounds: &HelperRect,
        target_width: u32,
        target_height: u32,
    ) -> Result<Vec<u8>, ToolError>;

    fn observe(
        &self,
        app: Option<&str>,
        activate_app: bool,
        capture_mode: Option<&str>,
        window_selection: Option<&WindowSelector>,
        prefer_window_when_available: bool,
    ) -> Result<PlatformObservation, ToolError>;

    fn run_event(
        &self,
        event_mode: &str,
        app: Option<&str>,
        float_env: &[(&str, f64)],
        string_env: &[(&str, String)],
    ) -> Result<(), ToolError>;

    fn run_system_events_type(
        &self,
        app: Option<&str>,
        window_selection: Option<&WindowSelector>,
        text: &str,
        replace: bool,
        submit: bool,
        strategy: &str,
    ) -> Result<(), ToolError>;
}

pub(crate) struct PlatformObservation {
    pub(crate) state: ObserveState,
    pub(crate) image_bytes: Vec<u8>,
}

pub(crate) struct GuiEmergencyStopMonitor {
    child: Option<Arc<Mutex<Child>>>,
    triggered: Arc<AtomicBool>,
    /// When set, the next Escape detection is treated as an expected
    /// programmatic keypress (e.g. `gui_key("Escape")`) rather than a
    /// user-initiated emergency stop.  The monitor thread clears this
    /// flag instead of setting `triggered`.
    suppress_next: Arc<AtomicBool>,
    output_thread: Option<JoinHandle<()>>,
    /// Process-scope enrollment of the monitor child: kept alive here so a
    /// supervisor `kill_all` can reap the monitor if its owner wedges.
    _group: Option<Arc<ProcessGroup>>,
}

impl GuiEmergencyStopMonitor {
    // Only the macOS backend spawns a monitor child process.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn from_child(
        mut child: Child,
        group: Arc<ProcessGroup>,
    ) -> Result<Self, ToolError> {
        let stdout = child.stdout.take().ok_or_else(|| {
            ToolError::new(
                xai_tool_runtime::ToolErrorKind::Execution,
                "native GUI emergency stop monitor did not expose stdout",
            )
        })?;
        let child = Arc::new(Mutex::new(child));
        let triggered = Arc::new(AtomicBool::new(false));
        let suppress_next = Arc::new(AtomicBool::new(false));
        let thread_triggered = Arc::clone(&triggered);
        let thread_suppress = Arc::clone(&suppress_next);
        let output_thread = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim().eq_ignore_ascii_case("escape") {
                    // If the caller told us to expect this Escape, consume
                    // the suppression flag instead of triggering a stop.
                    if thread_suppress
                        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        continue;
                    }
                    thread_triggered.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
        Ok(Self {
            child: Some(child),
            triggered,
            suppress_next,
            output_thread: Some(output_thread),
            _group: Some(group),
        })
    }

    /// Childless in-process monitor (Linux X11 and Windows backends): the
    /// caller owns the detection thread and its stop condition; `stop()` only
    /// joins the thread and never touches a child process.
    #[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
    pub(crate) fn from_thread(
        triggered: Arc<AtomicBool>,
        suppress_next: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            child: None,
            triggered,
            suppress_next,
            output_thread: handle,
            _group: None,
        }
    }

    pub(crate) fn triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }

    /// Tell the monitor to treat the next detected Escape keypress as an
    /// expected programmatic event rather than a user abort signal.  Must
    /// be called *before* the Escape key event is injected.
    pub(crate) fn expect_escape(&self) {
        self.suppress_next.store(true, Ordering::SeqCst);
    }

    pub(crate) fn stop(&mut self) {
        if let Some(child) = &self.child
            && let Ok(mut child) = child.lock()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(output_thread) = self.output_thread.take() {
            let _ = output_thread.join();
        }
    }
}

impl Drop for GuiEmergencyStopMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
struct UnsupportedGuiPlatform;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl GuiPlatform for UnsupportedGuiPlatform {
    fn readiness_snapshot(&self) -> GuiEnvironmentReadinessSnapshot {
        GuiEnvironmentReadinessSnapshot {
            status: "unsupported",
            checks: vec![GuiEnvironmentReadinessCheck {
                id: "platform",
                label: "Platform",
                status: GuiReadinessStatus::Unsupported,
                summary: GUI_UNSUPPORTED_MESSAGE.to_string(),
                detail: None,
            }],
        }
    }

    fn resolve_helper_binary(&self) -> Result<PathBuf, ToolError> {
        Err(unsupported_error())
    }

    fn tool_capabilities(&self) -> HashMap<&'static str, GuiToolCapability> {
        GUI_TOOL_NAMES
            .into_iter()
            .map(|tool_name| {
                (
                    tool_name,
                    GuiToolCapability {
                        enabled: false,
                        reason: Some(GUI_UNSUPPORTED_MESSAGE.to_string()),
                        targetless_only: false,
                    },
                )
            })
            .collect()
    }

    fn capture_context(
        &self,
        _app: Option<&str>,
        _activate_app: bool,
        _window_selection: Option<&WindowSelector>,
    ) -> Result<HelperCaptureContext, ToolError> {
        Err(unsupported_error())
    }

    fn capture_region(
        &self,
        _bounds: &HelperRect,
        _target_width: u32,
        _target_height: u32,
    ) -> Result<Vec<u8>, ToolError> {
        Err(unsupported_error())
    }

    fn run_event(
        &self,
        _event_mode: &str,
        _app: Option<&str>,
        _float_env: &[(&str, f64)],
        _string_env: &[(&str, String)],
    ) -> Result<(), ToolError> {
        Err(unsupported_error())
    }

    fn observe(
        &self,
        _app: Option<&str>,
        _activate_app: bool,
        _capture_mode: Option<&str>,
        _window_selection: Option<&WindowSelector>,
        _prefer_window_when_available: bool,
    ) -> Result<PlatformObservation, ToolError> {
        Err(unsupported_error())
    }

    fn run_system_events_type(
        &self,
        _app: Option<&str>,
        _window_selection: Option<&WindowSelector>,
        _text: &str,
        _replace: bool,
        _submit: bool,
        _strategy: &str,
    ) -> Result<(), ToolError> {
        Err(unsupported_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unsupported_error() -> ToolError {
    ToolError::new(
        xai_tool_runtime::ToolErrorKind::Execution,
        GUI_UNSUPPORTED_MESSAGE,
    )
}

/// Compile-time platform dispatch. GUI operations are blocking, so callers
/// run this behind `tokio::task::spawn_blocking`.
pub(crate) fn default_gui_platform() -> &'static dyn GuiPlatform {
    // Exactly one cfg block survives compilation, and it is the tail
    // expression of the body.
    #[cfg(target_os = "macos")]
    {
        static PLATFORM: platform_macos::MacOSPlatform = platform_macos::MacOSPlatform;
        &PLATFORM
    }

    #[cfg(target_os = "windows")]
    {
        static PLATFORM: platform_windows::WindowsPlatform = platform_windows::WindowsPlatform;
        &PLATFORM
    }

    #[cfg(target_os = "linux")]
    {
        static PLATFORM: platform_linux::LinuxGuiPlatform = platform_linux::LinuxGuiPlatform;
        &PLATFORM
    }

    // Kept gated on `windows` too: this arm must only survive on non-GUI
    // targets, and exactly one arm above must be the tail expression.
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        static PLATFORM: UnsupportedGuiPlatform = UnsupportedGuiPlatform;
        &PLATFORM
    }
}

pub(crate) fn resolve_gui_platform_tool_capabilities() -> HashMap<&'static str, GuiToolCapability> {
    default_gui_platform().tool_capabilities()
}
