//! Environment capability matrix for the native GUI tools — ported from
//! upstream codex PR understudy-ai/codex-cua#3
//! (`core/src/tools/handlers/gui/readiness.rs`).
//!
//! Upstream derived `grounding_available` from the model's image-input
//! modality; grok-build tools cannot see model metadata, so the caller
//! passes it in (the GUI grounding sampler presence is the equivalent
//! signal — see [`super::handler::supports_image_input`]).

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::Instant;

use xai_tool_runtime::ToolError;
use xai_tool_runtime::ToolErrorKind;

use super::platform::GUI_UNSUPPORTED_MESSAGE;
use super::platform::default_gui_platform;
use super::platform::resolve_gui_platform_tool_capabilities;

static READINESS_CACHE: OnceLock<StdMutex<Option<(GuiEnvironmentReadinessSnapshot, Instant)>>> =
    OnceLock::new();
const READINESS_CACHE_TTL_SECS: u64 = 30;

const GUI_ACCESSIBILITY_REQUIRED_REASON: &str = "Accessibility permission is not granted, so GUI input actions (click, type, scroll, drag, etc.) are unavailable. GUI observation tools (gui_observe) may still work. Grant Accessibility permission in System Settings > Privacy & Security > Accessibility.";
const GUI_GROUNDING_REQUIRED_REASON: &str = "Visual grounding is not configured, so grounding-based GUI actions (click, drag, etc.) are unavailable. Keyboard-only tool (gui_key) and targetless gui_type still work.";
const GUI_NATIVE_HELPER_REQUIRED_REASON: &str = "Native GUI helper is unavailable, so GUI tools cannot run. Verify the helper binary is installed and accessible.";
const GUI_SCREEN_CAPTURE_REQUIRED_REASON: &str = "Screen Recording permission is not granted, so screenshot-based GUI actions are unavailable. Keyboard-only tool (gui_key) still works. Grant Screen Recording permission in System Settings > Privacy & Security > Screen Recording.";
const GUI_SCREEN_CAPTURE_TARGETLESS_ONLY_REASON: &str = "Screen Recording permission is not granted, so this tool only supports targetless usage (omit the `target` parameter). Keyboard-only tool (gui_key) still works.";
const GUI_TARGETLESS_ONLY_REASON: &str = "Visual grounding is not configured, so this tool only supports targetless usage (omit the `target` parameter). It will operate on the current surface or focused control.";

/// The 8 core GUI tool names. `gui_batch` is intentionally absent: it is a
/// separate registration-gated tool (upstream keeps the same asymmetry).
pub(crate) const GUI_TOOL_NAMES: [&str; 8] = [
    "gui_observe",
    "gui_click",
    "gui_drag",
    "gui_scroll",
    "gui_type",
    "gui_key",
    "gui_wait",
    "gui_move",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuiReadinessStatus {
    Ok,
    Warn,
    Error,
    Unsupported,
}

#[derive(Clone, Debug)]
pub(crate) struct GuiEnvironmentReadinessCheck {
    pub(crate) id: &'static str,
    #[allow(dead_code)]
    pub(crate) label: &'static str,
    pub(crate) status: GuiReadinessStatus,
    #[allow(dead_code)]
    pub(crate) summary: String,
    #[allow(dead_code)]
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct GuiEnvironmentReadinessSnapshot {
    #[allow(dead_code)]
    pub(crate) status: &'static str,
    pub(crate) checks: Vec<GuiEnvironmentReadinessCheck>,
}

#[derive(Clone, Debug)]
pub(crate) struct GuiToolCapability {
    pub(crate) enabled: bool,
    pub(crate) reason: Option<String>,
    pub(crate) targetless_only: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct GuiRuntimeCapabilitySnapshot {
    #[allow(dead_code)]
    pub(crate) platform_supported: bool,
    #[allow(dead_code)]
    pub(crate) grounding_available: bool,
    #[allow(dead_code)]
    pub(crate) native_helper_available: bool,
    #[allow(dead_code)]
    pub(crate) screen_capture_available: bool,
    #[allow(dead_code)]
    pub(crate) input_available: bool,
    #[allow(dead_code)]
    pub(crate) enabled_tool_names: Vec<&'static str>,
    #[allow(dead_code)]
    pub(crate) disabled_tool_names: Vec<&'static str>,
    pub(crate) tool_availability: HashMap<&'static str, GuiToolCapability>,
}

pub(crate) fn resolve_gui_readiness_snapshot() -> GuiEnvironmentReadinessSnapshot {
    let cache = READINESS_CACHE.get_or_init(|| StdMutex::new(None));
    let mut guard = cache.lock().expect("readiness cache poisoned");
    if let Some((snapshot, created_at)) = guard.as_ref()
        && snapshot.status == "ready"
        && created_at.elapsed().as_secs() < READINESS_CACHE_TTL_SECS
    {
        return snapshot.clone();
    }
    let snapshot = default_gui_platform().readiness_snapshot();
    if snapshot.status == "ready" {
        *guard = Some((snapshot.clone(), Instant::now()));
    } else {
        *guard = None;
    }
    snapshot
}

fn resolve_readiness_check_status(
    snapshot: &GuiEnvironmentReadinessSnapshot,
    check_id: &str,
) -> Option<GuiReadinessStatus> {
    snapshot
        .checks
        .iter()
        .find(|check| check.id == check_id)
        .map(|check| check.status)
}

pub(crate) fn resolve_gui_runtime_capabilities(
    grounding_available: bool,
    readiness: &GuiEnvironmentReadinessSnapshot,
    platform_tool_availability: Option<&HashMap<&'static str, GuiToolCapability>>,
) -> GuiRuntimeCapabilitySnapshot {
    let platform_supported = resolve_readiness_check_status(readiness, "platform")
        != Some(GuiReadinessStatus::Unsupported);
    let native_helper_available = platform_supported
        && resolve_readiness_check_status(readiness, "native_helper")
            != Some(GuiReadinessStatus::Error);
    let input_available = native_helper_available
        && resolve_readiness_check_status(readiness, "accessibility")
            != Some(GuiReadinessStatus::Error);
    let screen_capture_available = native_helper_available
        && resolve_readiness_check_status(readiness, "screen_recording")
            != Some(GuiReadinessStatus::Error);

    let mut tool_availability = HashMap::new();
    for tool_name in GUI_TOOL_NAMES {
        let mut capability = platform_tool_availability
            .and_then(|tool_support| tool_support.get(tool_name))
            .cloned()
            .unwrap_or(GuiToolCapability {
                enabled: true,
                reason: None,
                targetless_only: false,
            });
        if !capability.enabled {
            tool_availability.insert(tool_name, capability);
            continue;
        }
        if !platform_supported {
            capability = GuiToolCapability {
                enabled: false,
                reason: Some(GUI_UNSUPPORTED_MESSAGE.to_string()),
                targetless_only: false,
            };
        } else if !native_helper_available {
            capability = GuiToolCapability {
                enabled: false,
                reason: Some(GUI_NATIVE_HELPER_REQUIRED_REASON.to_string()),
                targetless_only: false,
            };
        } else if matches!(
            tool_name,
            "gui_click" | "gui_drag" | "gui_scroll" | "gui_type" | "gui_key" | "gui_move"
        ) && !input_available
        {
            capability = GuiToolCapability {
                enabled: false,
                reason: Some(GUI_ACCESSIBILITY_REQUIRED_REASON.to_string()),
                targetless_only: false,
            };
        } else if matches!(
            tool_name,
            "gui_observe" | "gui_click" | "gui_drag" | "gui_wait"
        ) && !screen_capture_available
        {
            capability = GuiToolCapability {
                enabled: false,
                reason: Some(GUI_SCREEN_CAPTURE_REQUIRED_REASON.to_string()),
                targetless_only: false,
            };
        } else if !screen_capture_available && matches!(tool_name, "gui_scroll" | "gui_type") {
            if !capability.targetless_only {
                capability = GuiToolCapability {
                    enabled: true,
                    reason: Some(GUI_SCREEN_CAPTURE_TARGETLESS_ONLY_REASON.to_string()),
                    targetless_only: true,
                };
            }
        } else if matches!(tool_name, "gui_key" | "gui_move") || grounding_available {
            // Both arms upstream re-enable with the reason/targetless_only
            // fields carried through unchanged, so the conditions merge.
            capability = GuiToolCapability {
                enabled: true,
                reason: capability.reason.clone(),
                targetless_only: capability.targetless_only,
            };
        } else if matches!(tool_name, "gui_observe" | "gui_scroll" | "gui_type") {
            if !capability.targetless_only {
                capability = GuiToolCapability {
                    enabled: true,
                    reason: Some(GUI_TARGETLESS_ONLY_REASON.to_string()),
                    targetless_only: true,
                };
            }
        } else if matches!(tool_name, "gui_click" | "gui_drag" | "gui_wait") {
            capability = GuiToolCapability {
                enabled: false,
                reason: Some(GUI_GROUNDING_REQUIRED_REASON.to_string()),
                targetless_only: false,
            };
        }
        tool_availability.insert(tool_name, capability);
    }
    let enabled_tool_names = GUI_TOOL_NAMES
        .into_iter()
        .filter(|tool_name| tool_availability[*tool_name].enabled)
        .collect();
    let disabled_tool_names = GUI_TOOL_NAMES
        .into_iter()
        .filter(|tool_name| !tool_availability[*tool_name].enabled)
        .collect();
    GuiRuntimeCapabilitySnapshot {
        platform_supported,
        grounding_available,
        native_helper_available,
        screen_capture_available,
        input_available,
        enabled_tool_names,
        disabled_tool_names,
        tool_availability,
    }
}

/// Capability gate evaluated at the entry of every GUI tool call (upstream:
/// `enforce_gui_tool_capability`). The readiness probe and platform tool
/// availability run on a blocking thread — on macOS they shell out to the
/// `swift` helper; on Linux they probe X11 in-process (x11rb); on Windows
/// they probe the interactive desktop session in-process (windows-sys).
pub(crate) async fn enforce_gui_tool_capability(
    grounding_available: bool,
    tool_name: &'static str,
    targeted: bool,
) -> Result<(), ToolError> {
    let capabilities = tokio::task::spawn_blocking(move || {
        let readiness = resolve_gui_readiness_snapshot();
        let platform_tool_availability = resolve_gui_platform_tool_capabilities();
        resolve_gui_runtime_capabilities(
            grounding_available,
            &readiness,
            Some(&platform_tool_availability),
        )
    })
    .await
    .map_err(|error| {
        ToolError::new(
            ToolErrorKind::Execution,
            format!("GUI readiness check failed: {error}"),
        )
    })?;
    let Some(capability) = capabilities.tool_availability.get(tool_name) else {
        return Ok(());
    };
    if !capability.enabled {
        return Err(ToolError::new(
            ToolErrorKind::Execution,
            capability
                .reason
                .clone()
                .unwrap_or_else(|| format!("`{tool_name}` is currently unavailable.")),
        ));
    }
    if capability.targetless_only && targeted {
        return Err(ToolError::new(
            ToolErrorKind::Execution,
            capability.reason.clone().unwrap_or_else(|| {
                format!("`{tool_name}` currently supports only targetless usage.")
            }),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn seed_ready_readiness_cache_for_tests() {
    let snapshot = GuiEnvironmentReadinessSnapshot {
        status: "ready",
        checks: vec![
            test_readiness_check("platform", GuiReadinessStatus::Ok),
            test_readiness_check("accessibility", GuiReadinessStatus::Ok),
            test_readiness_check("screen_recording", GuiReadinessStatus::Ok),
            test_readiness_check("native_helper", GuiReadinessStatus::Ok),
        ],
    };
    let cache = READINESS_CACHE.get_or_init(|| StdMutex::new(None));
    *cache.lock().expect("readiness cache poisoned") = Some((snapshot, Instant::now()));
}

#[cfg(test)]
fn test_readiness_check(
    id: &'static str,
    status: GuiReadinessStatus,
) -> GuiEnvironmentReadinessCheck {
    GuiEnvironmentReadinessCheck {
        id,
        label: id,
        status,
        summary: String::new(),
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ready_snapshot() -> GuiEnvironmentReadinessSnapshot {
        GuiEnvironmentReadinessSnapshot {
            status: "ready",
            checks: vec![
                test_readiness_check("platform", GuiReadinessStatus::Ok),
                test_readiness_check("accessibility", GuiReadinessStatus::Ok),
                test_readiness_check("screen_recording", GuiReadinessStatus::Ok),
                test_readiness_check("native_helper", GuiReadinessStatus::Ok),
            ],
        }
    }

    // Ported from upstream tests/readiness.rs.
    #[test]
    fn runtime_capabilities_disable_grounded_actions_without_grounding() {
        let readiness = ready_snapshot();
        let capabilities = resolve_gui_runtime_capabilities(false, &readiness, None);
        assert!(capabilities.platform_supported);
        assert!(!capabilities.grounding_available);
        assert!(capabilities.native_helper_available);
        assert!(capabilities.screen_capture_available);
        assert!(capabilities.input_available);
        assert!(capabilities.enabled_tool_names.contains(&"gui_scroll"));
        assert!(capabilities.disabled_tool_names.contains(&"gui_click"));
        assert!(!capabilities.tool_availability["gui_click"].enabled);
        assert!(capabilities.tool_availability["gui_scroll"].enabled);
        assert!(capabilities.tool_availability["gui_scroll"].targetless_only);
        assert!(capabilities.tool_availability["gui_type"].enabled);
        assert!(capabilities.tool_availability["gui_type"].targetless_only);
    }

    #[test]
    fn runtime_capabilities_disable_input_actions_without_accessibility() {
        let readiness = GuiEnvironmentReadinessSnapshot {
            status: "blocked",
            checks: vec![
                test_readiness_check("platform", GuiReadinessStatus::Ok),
                test_readiness_check("accessibility", GuiReadinessStatus::Error),
                test_readiness_check("screen_recording", GuiReadinessStatus::Ok),
                test_readiness_check("native_helper", GuiReadinessStatus::Ok),
            ],
        };
        let capabilities = resolve_gui_runtime_capabilities(true, &readiness, None);
        assert!(capabilities.platform_supported);
        assert!(capabilities.grounding_available);
        assert!(capabilities.native_helper_available);
        assert!(capabilities.screen_capture_available);
        assert!(!capabilities.input_available);
        assert!(!capabilities.tool_availability["gui_click"].enabled);
        assert!(!capabilities.tool_availability["gui_key"].enabled);
        assert!(capabilities.tool_availability["gui_observe"].enabled);
        assert!(capabilities.tool_availability["gui_wait"].enabled);
    }

    #[test]
    fn runtime_capabilities_respect_platform_tool_contract() {
        let readiness = ready_snapshot();
        let platform_support = HashMap::from([
            (
                "gui_move",
                GuiToolCapability {
                    enabled: false,
                    reason: Some("platform backend does not support pointer movement".to_string()),
                    targetless_only: false,
                },
            ),
            (
                "gui_type",
                GuiToolCapability {
                    enabled: true,
                    reason: Some(
                        "platform backend only supports typing into the focused control"
                            .to_string(),
                    ),
                    targetless_only: true,
                },
            ),
        ]);

        let capabilities =
            resolve_gui_runtime_capabilities(true, &readiness, Some(&platform_support));

        assert!(!capabilities.tool_availability["gui_move"].enabled);
        assert_eq!(
            capabilities.tool_availability["gui_move"].reason.as_deref(),
            Some("platform backend does not support pointer movement")
        );
        assert!(capabilities.tool_availability["gui_type"].enabled);
        assert!(capabilities.tool_availability["gui_type"].targetless_only);
        assert_eq!(
            capabilities.tool_availability["gui_type"].reason.as_deref(),
            Some("platform backend only supports typing into the focused control")
        );
    }
}
