//! Behavior-pinning unit tests for the GUI engine — ported from upstream
//! codex PR understudy-ai/codex-cua#3 (`core/src/tools/handlers/gui/tests/`:
//! wait.rs, drag.rs, scroll.rs, type_and_input.rs, window_selection.rs and
//! the readiness `prepare_gui_observe_request` pair). Pure logic only:
//! nothing here moves the mouse/keyboard or captures the screen.
use super::*;
use serial_test::serial;
use std::ffi::OsStr;

use crate::implementations::gui::grounding::GuiGroundingModelRequest;
use crate::implementations::gui::grounding::GuiGroundingSampler;
use crate::implementations::gui::grounding::GuiGroundingSamplerError;
use crate::implementations::gui::schemas::TypeArgs;

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &OsStr) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => unsafe {
                std::env::set_var(self.key, value);
            },
            None => unsafe {
                std::env::remove_var(self.key);
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn observe_state_for_test(
    capture_mode: CaptureMode,
    window_title: Option<&str>,
    app_name: Option<&str>,
) -> ObserveState {
    ObserveState {
        capture: CaptureArtifact {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800,
            height: 600,
            image_width: 1600,
            image_height: 1200,
            display_index: 1,
            capture_mode,
            window_title: window_title.map(ToOwned::to_owned),
            window_count: Some(1),
            window_capture_strategy: Some("bounds".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: app_name.map(ToOwned::to_owned),
    }
}

fn resolved_target_for_test(bounds: HelperRect) -> ResolvedTarget {
    ResolvedTarget {
        window_title: Some("Browser".to_string()),
        provider: "openai:gpt-5.4".to_string(),
        confidence: 0.94,
        reason: Some("matched the only editable field".to_string()),
        grounding_mode_requested: "complex".to_string(),
        grounding_mode_effective: "complex".to_string(),
        scope: Some("Workspace panel".to_string()),
        point: HelperPoint { x: 114.0, y: 225.0 },
        bounds,
        local_point: None,
        local_bounds: None,
        raw: None,
        capture_state: observe_state_for_test(
            CaptureMode::Window,
            Some("Browser"),
            Some("Browser"),
        ),
    }
}

fn capture_context_for_test(
    window_count: i64,
    host: (bool, bool),
) -> crate::implementations::gui::wire::HelperCaptureContext {
    crate::implementations::gui::wire::HelperCaptureContext {
        app_name: Some("Notes".to_string()),
        cursor: HelperPoint { x: 10.0, y: 20.0 },
        display: crate::implementations::gui::wire::HelperDisplayDescriptor {
            index: 1,
            bounds: HelperRect {
                x: 0.0,
                y: 0.0,
                width: 1440.0,
                height: 900.0,
            },
        },
        window_id: Some(42),
        window_title: Some("Quick Note".to_string()),
        window_bounds: Some(HelperRect {
            x: 100.0,
            y: 80.0,
            width: 800.0,
            height: 600.0,
        }),
        window_count: Some(window_count),
        window_capture_strategy: Some("bounds".to_string()),
        host_self_exclude_applied: Some(host.0),
        host_frontmost_excluded: Some(host.1),
        host_frontmost_app_name: None,
        host_frontmost_bundle_id: None,
    }
}

fn test_call_context(call_id: &str) -> ToolCallContext {
    let call_id = xai_tool_protocol::ToolCallId::new(call_id)
        .unwrap_or_else(|_| xai_tool_protocol::ToolCallId::new_v7());
    ToolCallContext::new(call_id)
}

fn test_handler_context(session_id: &str) -> GuiHandlerContext {
    GuiHandlerContext {
        session_id: session_id.to_string(),
        sampler: None,
        settings: Some(GuiToolsSettings::default()),
    }
}

struct StubSampler;

#[async_trait::async_trait]
impl GuiGroundingSampler for StubSampler {
    async fn sample_grounding_json(
        &self,
        _request: &GuiGroundingModelRequest,
    ) -> Result<String, GuiGroundingSamplerError> {
        Err(GuiGroundingSamplerError(
            "not used by unit tests".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// wait.rs
// ---------------------------------------------------------------------------

#[test]
fn gui_wait_reuses_previous_window_observe_target_when_not_overridden() {
    let previous = observe_state_for_test(CaptureMode::Window, Some("Quick Note"), Some("Notes"));

    // Upstream pins the fallback policy applied by `handle_wait` when the
    // wait args carry no capture selection: reuse the previous observe
    // target (app + capture mode + window title for window captures).
    let mut app = None;
    let mut capture_mode: Option<&'static str> = None;
    let mut window_selection: Option<WindowSelector> = None;

    if app.is_none() && capture_mode.is_none() && window_selection.is_none() {
        app = previous.app_name.clone();
        capture_mode = Some(previous.capture.capture_mode.as_str());
        if previous.capture.capture_mode == CaptureMode::Window {
            window_selection = previous
                .capture
                .window_title
                .as_ref()
                .map(|title| WindowSelector {
                    title: Some(title.clone()),
                    title_contains: None,
                    index: None,
                });
        }
    }

    assert_eq!(app.as_deref(), Some("Notes"));
    assert_eq!(capture_mode, Some("window"));
    assert_eq!(
        window_selection.and_then(|selection| selection.title),
        Some("Quick Note".to_string())
    );
}

#[test]
fn normalize_wait_target_state_defaults_and_validates() {
    assert_eq!(normalize_wait_target_state(None).unwrap(), "appear");
    assert_eq!(
        normalize_wait_target_state(Some("disappear")).unwrap(),
        "disappear"
    );
    assert!(normalize_wait_target_state(Some("later")).is_err());
}

#[test]
fn fallback_probe_capture_mode_switches_to_display_after_first_app_attempt() {
    assert_eq!(fallback_probe_capture_mode(None, 1, Some("Notes")), None);
    assert_eq!(
        fallback_probe_capture_mode(None, 2, Some("Notes")),
        Some("display")
    );
    assert_eq!(
        fallback_probe_capture_mode(Some("window"), 2, Some("Notes")),
        Some("window")
    );
    assert_eq!(fallback_probe_capture_mode(None, 2, None), None);
}

#[test]
fn fallback_probe_capture_mode_is_generic_for_waits_and_grounded_actions() {
    assert_eq!(
        fallback_probe_capture_mode(None, 2, Some("Finder")),
        Some("display")
    );
    assert_eq!(
        fallback_probe_capture_mode(Some("display"), 2, Some("Finder")),
        Some("display")
    );
}

#[test]
fn remaining_wait_budget_stops_at_zero() {
    let future_deadline = Instant::now() + Duration::from_millis(250);
    assert!(remaining_wait_budget_ms(future_deadline).unwrap() > 0);

    let expired_deadline = Instant::now() - Duration::from_millis(1);
    assert_eq!(remaining_wait_budget_ms(expired_deadline), None);
}

// ---------------------------------------------------------------------------
// drag.rs
// ---------------------------------------------------------------------------

#[test]
fn normalize_drag_endpoint_accepts_semantic_targets() {
    let endpoint = normalize_drag_endpoint(
        "source",
        "from_target",
        Some("Save button"),
        Some("top right"),
        Some("toolbar"),
    )
    .expect("target endpoint should normalize");

    match endpoint {
        DragEndpoint::Target {
            target,
            location_hint,
            scope,
        } => {
            assert_eq!(target, "Save button");
            assert_eq!(location_hint, Some("top right"));
            assert_eq!(scope, Some("toolbar"));
        }
    }
}

#[test]
fn normalize_drag_endpoint_requires_semantic_targets() {
    let error = normalize_drag_endpoint("destination", "to_target", None, None, None)
        .expect_err("missing target endpoint should fail");

    assert!(
        error
            .to_string()
            .contains("requires `to_target` for the destination")
    );
}

#[test]
fn normalize_grounding_mode_defaults_and_validates() {
    assert_eq!(
        normalize_grounding_mode(None, "gui_click").unwrap(),
        "single"
    );
    assert_eq!(normalize_grounding_mode(None, "type").unwrap(), "complex");
    assert_eq!(
        normalize_grounding_mode(None, "drag_source").unwrap(),
        "complex"
    );
    assert_eq!(
        normalize_grounding_mode(Some("complex"), "gui_click").unwrap(),
        "complex"
    );
    assert!(normalize_grounding_mode(Some("dense"), "gui_click").is_err());
}

// ---------------------------------------------------------------------------
// scroll.rs
// ---------------------------------------------------------------------------

#[test]
fn normalize_scroll_direction_defaults_and_validates() {
    assert!(matches!(
        normalize_scroll_direction(None).unwrap(),
        ScrollDirection::Down
    ));
    assert!(matches!(
        normalize_scroll_direction(Some("left")).unwrap(),
        ScrollDirection::Left
    ));
    assert!(normalize_scroll_direction(Some("sideways")).is_err());
}

#[test]
fn resolve_scroll_plan_uses_semantic_distance_defaults() {
    let capture_bounds = HelperRect {
        x: 0.0,
        y: 0.0,
        width: 1200.0,
        height: 800.0,
    };

    let targetless = resolve_scroll_plan(
        None,
        None,
        false,
        ScrollDirection::Down,
        None,
        Some(&capture_bounds),
    );
    assert_eq!(targetless.distance_preset, "page");
    assert_eq!(targetless.unit, "pixel");
    assert_eq!(targetless.amount, 600);

    let target_bounds = HelperRect {
        x: 0.0,
        y: 0.0,
        width: 400.0,
        height: 200.0,
    };
    let targeted = resolve_scroll_plan(
        None,
        None,
        true,
        ScrollDirection::Down,
        Some(&target_bounds),
        Some(&capture_bounds),
    );
    assert_eq!(targeted.distance_preset, "medium");
    assert_eq!(targeted.unit, "pixel");
    assert_eq!(targeted.amount, 100);
}

#[test]
fn scroll_delta_components_match_understudy_scroll_direction_convention() {
    assert_eq!(
        scroll_delta_components(ScrollDirection::Down, 240),
        (0, -240)
    );
    assert_eq!(scroll_delta_components(ScrollDirection::Up, 240), (0, 240));
    assert_eq!(
        scroll_delta_components(ScrollDirection::Left, 240),
        (-240, 0)
    );
    assert_eq!(
        scroll_delta_components(ScrollDirection::Right, 240),
        (240, 0)
    );
}

// ---------------------------------------------------------------------------
// type_and_input.rs
// ---------------------------------------------------------------------------

#[test]
fn targeted_type_focus_point_prefers_display_box_center() {
    let resolved = resolved_target_for_test(HelperRect {
        x: 80.0,
        y: 200.0,
        width: 240.0,
        height: 40.0,
    });

    let focus_point = targeted_type_focus_point(&resolved);
    assert_eq!(focus_point.x, 200.0);
    assert_eq!(focus_point.y, 220.0);
}

#[test]
fn targeted_type_focus_point_falls_back_to_grounded_point_without_valid_box() {
    let resolved = resolved_target_for_test(HelperRect {
        x: 80.0,
        y: 200.0,
        width: 0.0,
        height: 40.0,
    });

    let focus_point = targeted_type_focus_point(&resolved);
    assert_eq!(focus_point.x, 114.0);
    assert_eq!(focus_point.y, 225.0);
}

#[test]
#[serial]
fn resolve_type_value_accepts_plain_secret_env_var_names() {
    let _guard = EnvVarGuard::set("GUI_TYPE_TEST_SECRET", OsStr::new("hunter2"));
    let args = TypeArgs {
        value: None,
        secret_env_var: Some("GUI_TYPE_TEST_SECRET".to_string()),
        secret_command_env_var: None,
        target: None,
        location_hint: None,
        scope: None,
        grounding_mode: None,
        replace: None,
        submit: None,
        type_strategy: None,
        capture_mode: None,
        window_title: None,
        window_selector: None,
        app: None,
    };

    let value = resolve_type_value(&args).expect("plain env var names should still be supported");
    assert_eq!(value, "hunter2");
}

#[test]
#[serial]
fn resolve_type_value_accepts_plain_secret_command_env_var_names() {
    let _guard = EnvVarGuard::set(
        "GUI_TYPE_TEST_SECRET_COMMAND",
        OsStr::new("printf 'from-command'"),
    );
    let args = TypeArgs {
        value: None,
        secret_env_var: None,
        secret_command_env_var: Some("GUI_TYPE_TEST_SECRET_COMMAND".to_string()),
        target: None,
        location_hint: None,
        scope: None,
        grounding_mode: None,
        replace: None,
        submit: None,
        type_strategy: None,
        capture_mode: None,
        window_title: None,
        window_selector: None,
        app: None,
    };

    let value =
        resolve_type_value(&args).expect("plain command env var names should still be supported");
    assert_eq!(value, "from-command");
}

// ---------------------------------------------------------------------------
// window_selection.rs
// ---------------------------------------------------------------------------

#[test]
fn normalize_window_selection_merges_top_level_and_selector_fields() {
    let selector = WindowSelector {
        title: None,
        title_contains: Some(" Settings ".to_string()),
        index: Some(2),
    };

    let selection = normalize_window_selection(Some(" Preferences "), Some(&selector))
        .expect("window selection should normalize")
        .expect("window selection should exist");

    assert_eq!(selection.title.as_deref(), Some("Preferences"));
    assert_eq!(selection.title_contains.as_deref(), Some("Settings"));
    assert_eq!(selection.index, Some(2));
}

#[test]
fn resolve_capture_target_uses_window_bounds_when_requested() {
    let context = capture_context_for_test(3, (false, false));

    let capture =
        resolve_capture_target(&context, Some("window"), true, true).expect("window capture");

    assert_eq!(capture.mode, CaptureMode::Window);
    assert_eq!(capture.width, 800);
    assert_eq!(capture.height, 600);
    assert_eq!(capture.window_title.as_deref(), Some("Quick Note"));
    assert_eq!(capture.window_count, Some(3));
    assert_eq!(capture.window_capture_strategy.as_deref(), Some("bounds"));
}

#[test]
#[cfg(target_os = "macos")]
fn normalize_capture_mode_env_accepts_common_variants() {
    use crate::implementations::gui::platform::platform_macos::normalize_capture_mode_env;
    assert_eq!(normalize_capture_mode_env("window"), Some("window"));
    assert_eq!(normalize_capture_mode_env(" Window "), Some("window"));
    assert_eq!(normalize_capture_mode_env("display."), Some("display"));
    assert_eq!(normalize_capture_mode_env("DISPLAY"), Some("display"));
    assert_eq!(normalize_capture_mode_env("workspace"), None);
}

#[test]
fn resolve_capture_target_prefers_window_for_in_app_work_when_available() {
    let context = capture_context_for_test(1, (false, false));

    let capture = resolve_capture_target(&context, None, false, true)
        .expect("window should be preferred for in-app work");

    assert_eq!(capture.mode, CaptureMode::Window);
    assert_eq!(capture.width, 800);
    assert_eq!(capture.height, 600);
}

#[test]
fn resolve_capture_target_adjusts_implicit_display_for_host_self_exclude() {
    let mut context = capture_context_for_test(1, (true, true));
    context.host_frontmost_app_name = Some("Codex".to_string());
    context.host_frontmost_bundle_id = Some("com.openai.codex".to_string());

    let capture = resolve_capture_target(&context, None, false, false)
        .expect("implicit display capture should adjust to a safe window capture");

    assert_eq!(capture.mode, CaptureMode::Window);
    assert!(capture.host_self_exclude_adjusted);
    assert_eq!(capture.width, 800);
    assert_eq!(capture.height, 600);
}

// ---------------------------------------------------------------------------
// Observe-state sharing + enable marker (handler-level regression tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn observe_state_is_shared_within_session_and_isolated_across_sessions() {
    let engine = shared_gui_engine();
    engine
        .set_observe_state(
            "observe-test-shared",
            observe_state_for_test(CaptureMode::Window, Some("Quick Note"), Some("Notes")),
        )
        .await;

    let same_session = test_handler_context("observe-test-shared");
    let reused = engine
        .get_observe_state(&same_session)
        .await
        .expect("a second gui call in the same session must reuse the observe state");
    assert_eq!(reused.app_name.as_deref(), Some("Notes"));
    assert_eq!(reused.capture.capture_mode, CaptureMode::Window);
    assert_eq!(reused.capture.window_title.as_deref(), Some("Quick Note"));

    let other_session = test_handler_context("observe-test-other");
    assert!(engine.get_observe_state(&other_session).await.is_none());
}

#[tokio::test]
async fn from_tool_context_keys_observe_state_by_session_context_extension() {
    // The stable per-session key must come from the `SessionContext`
    // extension stamped at dispatch (upstream: `conversation_id`), never
    // from the per-call id.
    let engine = shared_gui_engine();
    engine
        .set_observe_state(
            "observe-test-ctx-key",
            observe_state_for_test(CaptureMode::Window, Some("Quick Note"), Some("Notes")),
        )
        .await;

    let mut ctx = test_call_context("from-tool-context-call-1");
    ctx.extensions.insert(xai_tool_runtime::SessionContext(
        "observe-test-ctx-key".to_string(),
    ));
    let from_ctx = GuiHandlerContext::from_tool_context(&ctx).await;
    assert_eq!(from_ctx.session_id, "observe-test-ctx-key");

    assert!(
        engine.get_observe_state(&from_ctx).await.is_some(),
        "calls sharing a session must share observe state"
    );
}

#[tokio::test]
async fn from_tool_context_is_enabled_only_with_explicit_marker() {
    let mut ctx = test_call_context("enabled-call");
    ctx.extensions.insert(GuiToolsSettings::default());
    let enabled = GuiHandlerContext::from_tool_context(&ctx).await;
    assert!(enabled.is_enabled());
    assert!(enabled.ensure_enabled().is_ok());

    let bare = test_call_context("disabled-call");
    let disabled = GuiHandlerContext::from_tool_context(&bare).await;
    assert!(!disabled.is_enabled());
    let error = disabled
        .ensure_enabled()
        .expect_err("dispatch without the enable marker must be refused");
    assert!(
        error.to_string().contains("gui_tools"),
        "model-facing refusal must point at the feature flag: {error}"
    );
}

#[tokio::test]
async fn from_tool_context_reads_settings_from_params_resource() {
    let mut ctx = test_call_context("params-resource-call");
    let mut resources = crate::types::resources::Resources::new();
    resources.insert(crate::types::resources::Params(GuiToolsSettings {
        coordinate_targeting: true,
        ..GuiToolsSettings::default()
    }));
    ctx.extensions
        .insert(std::sync::Arc::new(tokio::sync::Mutex::new(resources)));

    let from_ctx = GuiHandlerContext::from_tool_context(&ctx).await;
    assert!(
        from_ctx.is_enabled(),
        "Params<GuiToolsSettings> resource is the enable marker"
    );
    assert!(from_ctx.settings().coordinate_targeting);
}

#[tokio::test]
async fn from_tool_context_reads_sampler_from_shared_resources() {
    let mut ctx = test_call_context("sampler-resource-call");
    ctx.extensions.insert(GuiToolsSettings::default());
    let mut resources = crate::types::resources::Resources::new();
    resources.insert(std::sync::Arc::new(StubSampler) as std::sync::Arc<dyn GuiGroundingSampler>);
    ctx.extensions
        .insert(std::sync::Arc::new(tokio::sync::Mutex::new(resources)));

    let from_ctx = GuiHandlerContext::from_tool_context(&ctx).await;
    assert!(
        from_ctx.image_input_supported(),
        "a sampler injected post-finalize must enable grounding"
    );
    assert!(from_ctx.sampler().is_ok());
}

// ---------------------------------------------------------------------------
// readiness.rs — prepare_gui_observe_request pair (readiness cache is
// seeded so the probes never shell out to `swift`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gui_observe_targetless_request_skips_image_attachment_without_image_input() {
    crate::implementations::gui::readiness::seed_ready_readiness_cache_for_tests();
    let ctx = test_handler_context("readiness-targetless");

    let attach_image = prepare_gui_observe_request(&ctx, false, None)
        .await
        .expect("targetless gui_observe should remain available without image input");

    assert!(!attach_image);
}

#[tokio::test]
async fn gui_observe_targeted_request_stays_blocked_without_image_input() {
    crate::implementations::gui::readiness::seed_ready_readiness_cache_for_tests();
    let ctx = test_handler_context("readiness-targeted");

    let error = prepare_gui_observe_request(&ctx, true, None)
        .await
        .expect_err("targeted gui_observe should still be blocked without image input");

    assert!(
        error.to_string().contains("targetless"),
        "unexpected capability error: {error}"
    );
}
