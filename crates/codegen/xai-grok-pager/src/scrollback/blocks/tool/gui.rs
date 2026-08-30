//! Native GUI tool-call block helpers (upstream PR understudy-ai/codex-cua#3).
//!
//! The `gui_*` tool family shares one [`ToolCallBlock::Gui`] variant whose
//! payload is an [`OtherToolCallBlock`]: the compact argument summary (target
//! element, coordinates, duration) rides `summary` and the engine's result
//! message rides `output`/`error` — grok's rendering of upstream's
//! `BuiltinToolCallCell` (tool name + status + compact arg/result summary).
//!
//! This module is pure JSON → strings; the ACP-facing assembly lives in
//! `acp::tracker`.

use serde_json::Value;

/// Wire names of the native GUI tools, in registration order (`gui_batch` is
/// gated by its own config flag but is a real tool).
pub const GUI_TOOL_NAMES: &[&str] = &[
    "gui_observe",
    "gui_click",
    "gui_drag",
    "gui_scroll",
    "gui_type",
    "gui_key",
    "gui_move",
    "gui_wait",
    "gui_batch",
];

/// True when `name` is one of the native GUI tool wire names.
pub fn is_gui_tool_name(name: &str) -> bool {
    let lowered = name.trim().to_ascii_lowercase();
    GUI_TOOL_NAMES.iter().any(|tool| *tool == lowered)
}

/// Serde variant tags of the `ToolInput::Gui*` variants as they appear in the
/// `raw_input.variant` discriminator of a parsed tool call.
pub fn is_gui_variant(variant: Option<&str>) -> bool {
    matches!(
        variant,
        Some(
            "GuiObserve"
                | "GuiClick"
                | "GuiDrag"
                | "GuiScroll"
                | "GuiType"
                | "GuiKey"
                | "GuiMove"
                | "GuiWait"
                | "GuiBatch"
        )
    )
}

/// Map a wire name (`gui_click`) to its `ToolInput` variant tag (`GuiClick`).
///
/// The early Pending notification carries the raw model arguments — no
/// `variant` tag yet — with the wire name as the title.
pub fn variant_for_tool_name(name: &str) -> Option<&'static str> {
    let lowered = name.trim().to_ascii_lowercase();
    let variant = match lowered.as_str() {
        "gui_observe" => "GuiObserve",
        "gui_click" => "GuiClick",
        "gui_drag" => "GuiDrag",
        "gui_scroll" => "GuiScroll",
        "gui_type" => "GuiType",
        "gui_key" => "GuiKey",
        "gui_move" => "GuiMove",
        "gui_wait" => "GuiWait",
        "gui_batch" => "GuiBatch",
        _ => return None,
    };
    Some(variant)
}

/// Compact summary of one GUI invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct GuiInvocationSummary {
    /// Display label for the action ("Click", "Observe screen", ...).
    pub label: String,
    /// What the action acts on ("Send button"), when the model supplied one.
    /// The shell's refined title already carries this, so callers append it
    /// to the summary only while the title is still the raw wire name.
    pub subject: Option<String>,
    /// Secondary details (coordinates, duration, direction, modifiers, ...).
    pub details: Vec<String>,
}

impl GuiInvocationSummary {
    /// `details` (plus `subject` when requested) joined for the muted
    /// summary span.
    pub fn join(&self, include_subject: bool) -> String {
        let mut parts = Vec::with_capacity(self.details.len() + 1);
        if include_subject && let Some(subject) = &self.subject {
            parts.push(subject.clone());
        }
        parts.extend(self.details.iter().cloned());
        parts.join(" · ")
    }
}

/// Summarize one GUI tool invocation from its parsed arguments.
///
/// `variant` is the `ToolInput::Gui*` tag; `args` is the argument object
/// (either the serialized `ToolInput` or the raw model arguments — same field
/// names). Typed text is never included: it may come from a secret env var.
pub fn summarize_invocation(variant: &str, args: &Value) -> GuiInvocationSummary {
    let summary = match variant {
        "GuiObserve" => GuiInvocationSummary {
            label: "Observe screen".into(),
            subject: opt_string(args, "target"),
            details: capture_details(args),
        },
        "GuiClick" => {
            let label = click_label(args);
            let subject = opt_string(args, "target");
            let mut details = Vec::new();
            if let Some((x, y)) = point(args, "x", "y") {
                details.push(format!("at ({})", format_point(x, y)));
            }
            if let Some(hold) = opt_i64(args, "hold_ms") {
                details.push(format!("hold {hold} ms"));
            }
            GuiInvocationSummary {
                label: label.into(),
                subject,
                details,
            }
        }
        "GuiDrag" => {
            let from = endpoint(args, "from_target", "from_x", "from_y");
            let to = endpoint(args, "to_target", "to_x", "to_y");
            let subject = match (&from, &to) {
                (Some(from), Some(to)) => Some(format!("{from} → {to}")),
                (Some(from), None) | (None, Some(from)) => Some(from.clone()),
                (None, None) => None,
            };
            let mut details = Vec::new();
            if let Some(duration) = opt_i64(args, "duration_ms") {
                details.push(format!("{duration} ms"));
            }
            GuiInvocationSummary {
                label: "Drag".into(),
                subject,
                details,
            }
        }
        "GuiScroll" => {
            let mut details = Vec::new();
            if let Some(direction) = opt_string(args, "direction") {
                details.push(direction);
            }
            let amount = opt_i64(args, "amount")
                .map(|amount| amount.to_string())
                .or_else(|| opt_string(args, "distance"));
            if let Some(amount) = amount {
                details.push(amount);
            }
            GuiInvocationSummary {
                label: "Scroll".into(),
                subject: opt_string(args, "target"),
                details,
            }
        }
        "GuiType" => {
            let mut details = Vec::new();
            if opt_bool(args, "replace") == Some(true) {
                details.push("replace".into());
            }
            if opt_bool(args, "submit") == Some(true) {
                details.push("submit".into());
            }
            if let Some(strategy) = opt_string(args, "type_strategy") {
                details.push(strategy);
            }
            GuiInvocationSummary {
                label: "Type text".into(),
                subject: opt_string(args, "target"),
                details,
            }
        }
        "GuiKey" => {
            let subject = key_subject(args);
            let mut details = Vec::new();
            if let Some(repeat) = opt_i64(args, "repeat")
                && repeat > 1
            {
                details.push(format!("{repeat}x"));
            }
            GuiInvocationSummary {
                label: "Press".into(),
                subject,
                details,
            }
        }
        "GuiMove" => {
            let subject = point(args, "x", "y").map(|(x, y)| format!("({})", format_point(x, y)));
            GuiInvocationSummary {
                label: "Move pointer".into(),
                subject,
                details: Vec::new(),
            }
        }
        "GuiWait" => {
            let mut details = Vec::new();
            if let Some(state) = opt_string(args, "state") {
                details.push(state);
            }
            if let Some(timeout) = opt_i64(args, "timeout_ms") {
                details.push(format!("timeout {timeout} ms"));
            }
            GuiInvocationSummary {
                label: "Wait for".into(),
                subject: opt_string(args, "target"),
                details,
            }
        }
        "GuiBatch" => {
            let steps = args
                .get("steps")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            GuiInvocationSummary {
                label: "Run batch".into(),
                subject: None,
                details: vec![format!("{steps} steps")],
            }
        }
        _ => GuiInvocationSummary {
            label: "GUI action".into(),
            subject: None,
            details: Vec::new(),
        },
    };
    summary
}

/// Label for a click invocation; mirrors the helper's event modes
/// (click / right-click / double-click / click-and-hold).
fn click_label(args: &Value) -> &'static str {
    if opt_i64(args, "hold_ms").is_some_and(|hold| hold > 0) {
        return "Click and hold";
    }
    if opt_string(args, "button").is_some_and(|button| button.eq_ignore_ascii_case("right")) {
        return "Right-click";
    }
    if opt_i64(args, "clicks").is_some_and(|clicks| clicks >= 2) {
        return "Double-click";
    }
    "Click"
}

/// Capture-selection details shared by capture-taking tools.
fn capture_details(args: &Value) -> Vec<String> {
    let mut details = Vec::new();
    if let Some(mode) = opt_string(args, "capture_mode") {
        details.push(format!("capture: {mode}"));
    }
    if let Some(window) = opt_string(args, "window_title") {
        details.push(format!("window \"{window}\""));
    }
    details
}

/// `modifiers + key` ("Cmd+Shift+K"), or `key` alone.
fn key_subject(args: &Value) -> Option<String> {
    let key = opt_string(args, "key")?;
    let modifiers = args
        .get("modifiers")
        .and_then(Value::as_array)
        .map(|mods| {
            mods.iter()
                .filter_map(Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .map(|m| capitalize(m))
                .collect::<Vec<_>>()
                .join("+")
        })
        .filter(|mods| !mods.is_empty());
    Some(match modifiers {
        Some(mods) => format!("{mods}+{key}"),
        None => key,
    })
}

/// One drag endpoint: its target text or its coordinates.
fn endpoint(args: &Value, target_key: &str, x_key: &str, y_key: &str) -> Option<String> {
    if let Some(target) = opt_string(args, target_key) {
        return Some(target);
    }
    point(args, x_key, y_key).map(|(x, y)| format!("({})", format_point(x, y)))
}

fn point(args: &Value, x_key: &str, y_key: &str) -> Option<(f64, f64)> {
    Some((opt_f64(args, x_key)?, opt_f64(args, y_key)?))
}

/// Display-point coordinate: integers stay integers ("120"), fractions keep
/// one decimal.
fn format_point(x: f64, y: f64) -> String {
    format!("{}, {}", format_coord(x), format_coord(y))
}

fn format_coord(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e9 {
        format!("{}", value as i64)
    } else {
        format!("{value:.1}")
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn opt_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

/// Compact result summary parsed from the typed `ToolOutput::Gui` shape in
/// `raw_output` (`{"type":"Gui","message":...,"image_url":...,"success":...}`).
///
/// Screenshot payloads are scrubbed to a short marker by the shell before
/// they reach `raw_output`, so only presence is reported here.
#[derive(Debug, Clone, PartialEq)]
pub struct GuiResultSummary {
    /// The engine's result message (the compact result summary).
    pub message: String,
    /// `success: false` outputs are model-facing soft failures (upstream
    /// `GuiToolOutput.success` drove Completed/Failed).
    pub success: bool,
    /// Whether a screenshot was attached to the result.
    pub has_screenshot: bool,
}

/// Parse a typed GUI result from `raw_output`; `None` when it is not a GUI
/// output (callers fall back to the ACP content text).
pub fn summarize_result(raw_output: &Value) -> Option<GuiResultSummary> {
    let object = raw_output.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("Gui") {
        return None;
    }
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let success = object
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let has_screenshot = object
        .get("image_url")
        .is_some_and(|value| value.is_string());
    Some(GuiResultSummary {
        message,
        success,
        has_screenshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_name_detection_round_trips() {
        for name in GUI_TOOL_NAMES {
            assert!(is_gui_tool_name(name), "wire name: {name}");
            assert_eq!(variant_for_tool_name(name).unwrap(), format_variant(name));
        }
        assert!(!is_gui_tool_name("gui_widget"));
        assert!(!is_gui_tool_name("bash"));
        assert_eq!(variant_for_tool_name("bash"), None);
    }

    fn format_variant(name: &str) -> String {
        let mut out = String::from("Gui");
        for part in name.trim_start_matches("gui_").split('_') {
            out.push_str(&capitalize(part));
        }
        out
    }

    #[test]
    fn is_gui_variant_matches_parsed_tags_only() {
        assert!(is_gui_variant(Some("GuiClick")));
        assert!(is_gui_variant(Some("GuiBatch")));
        assert!(!is_gui_variant(Some("WebSearch")));
        assert!(!is_gui_variant(None));
    }

    #[test]
    fn click_summary_has_target_coordinates_and_button() {
        let args = json!({"target": "Send button", "x": 120.0, "y": 240.5});
        let summary = summarize_invocation("GuiClick", &args);
        assert_eq!(summary.label, "Click");
        assert_eq!(summary.subject.as_deref(), Some("Send button"));
        assert_eq!(summary.join(true), "Send button · at (120, 240.5)");
        assert_eq!(summary.join(false), "at (120, 240.5)");
    }

    #[test]
    fn click_label_reflects_button_clicks_and_hold() {
        assert_eq!(
            summarize_invocation("GuiClick", &json!({"button": "right"})).label,
            "Right-click"
        );
        assert_eq!(
            summarize_invocation("GuiClick", &json!({"clicks": 2})).label,
            "Double-click"
        );
        assert_eq!(
            summarize_invocation("GuiClick", &json!({"hold_ms": 900})).label,
            "Click and hold"
        );
        assert_eq!(
            summarize_invocation("GuiClick", &json!({"hold_ms": 900})).details,
            vec!["hold 900 ms".to_string()]
        );
    }

    #[test]
    fn drag_summary_joins_endpoints() {
        let summary = summarize_invocation(
            "GuiDrag",
            &json!({"from_target": "Icon", "to_x": 10, "to_y": 20, "duration_ms": 450}),
        );
        assert_eq!(summary.label, "Drag");
        assert_eq!(summary.subject.as_deref(), Some("Icon → (10, 20)"));
        assert_eq!(summary.join(false), "450 ms");
    }

    #[test]
    fn scroll_summary_has_direction_and_amount() {
        let summary = summarize_invocation("GuiScroll", &json!({"direction": "down", "amount": 5}));
        assert_eq!(summary.join(true), "down · 5");
        let by_distance =
            summarize_invocation("GuiScroll", &json!({"direction": "up", "distance": "page"}));
        assert_eq!(by_distance.join(true), "up · page");
    }

    #[test]
    fn type_summary_never_includes_the_value() {
        let args = json!({
            "value": "hunter2 secret",
            "secret_env_var": "MY_SECRET",
            "replace": true,
            "submit": true
        });
        let summary = summarize_invocation("GuiType", &args);
        let rendered = format!("{:?}", summary);
        assert!(!rendered.contains("hunter2"));
        assert_eq!(summary.join(true), "replace · submit");
    }

    #[test]
    fn key_summary_prepends_modifiers() {
        let summary = summarize_invocation(
            "GuiKey",
            &json!({"key": "K", "modifiers": ["cmd", "shift"], "repeat": 3}),
        );
        assert_eq!(summary.label, "Press");
        assert_eq!(summary.subject.as_deref(), Some("Cmd+Shift+K"));
        assert_eq!(summary.join(true), "Cmd+Shift+K · 3x");
    }

    #[test]
    fn wait_summary_has_state_and_timeout() {
        let summary = summarize_invocation(
            "GuiWait",
            &json!({"target": "Save dialog", "state": "disappear", "timeout_ms": 8000}),
        );
        assert_eq!(summary.label, "Wait for");
        assert_eq!(
            summary.join(true),
            "Save dialog · disappear · timeout 8000 ms"
        );
    }

    #[test]
    fn batch_summary_counts_steps() {
        let summary = summarize_invocation(
            "GuiBatch",
            &json!({"steps": [{"action": "click"}, {"action": "type"}]}),
        );
        assert_eq!(summary.label, "Run batch");
        assert_eq!(summary.join(true), "2 steps");
    }

    #[test]
    fn observe_summary_has_capture_details() {
        let summary = summarize_invocation(
            "GuiObserve",
            &json!({"target": "Send button", "capture_mode": "window", "window_title": "Mail"}),
        );
        assert_eq!(summary.label, "Observe screen");
        assert_eq!(
            summary.join(true),
            "Send button · capture: window · window \"Mail\""
        );
    }

    #[test]
    fn unknown_variant_falls_back_to_generic_label() {
        let summary = summarize_invocation("GuiUnknown", &json!({}));
        assert_eq!(summary.label, "GUI action");
        assert_eq!(summary.join(true), "");
    }

    #[test]
    fn result_summary_reads_typed_gui_output() {
        let summary = summarize_result(&json!({
            "type": "Gui",
            "message": "Clicked Send button.",
            "image_url": "<screenshot data url omitted>",
            "code_result": {"capture_mode": "display"},
            "success": true
        }))
        .unwrap();
        assert_eq!(summary.message, "Clicked Send button.");
        assert!(summary.success);
        assert!(summary.has_screenshot);
    }

    #[test]
    fn result_summary_flags_soft_failures_and_missing_screenshots() {
        let summary = summarize_result(&json!({
            "type": "Gui",
            "message": "Could not resolve semantic GUI target `Save`.",
            "image_url": null,
            "success": false
        }))
        .unwrap();
        assert!(!summary.success);
        assert!(!summary.has_screenshot);
    }

    #[test]
    fn result_summary_rejects_non_gui_outputs() {
        assert!(summarize_result(&json!({"type": "Text", "text": "hi"})).is_none());
        assert!(summarize_result(&json!(null)).is_none());
        assert!(summarize_result(&json!({"message": "no type tag"})).is_none());
    }
}
