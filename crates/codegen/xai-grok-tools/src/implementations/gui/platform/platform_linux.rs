//! Linux implementation of the GUI platform seam — pure-Rust X11 backend on
//! `x11rb` (no helper binary, no Swift, no libxcb).
//!
//! Screenshots use core `GetImage` (ZPixmap), input injection uses the XTEST
//! extension, window management uses EWMH/ICCCM properties and client
//! messages, and monitor geometry comes from RANDR. The X connection is
//! established lazily on first use and kept in a process-global
//! `OnceLock<Arc<...>>` cache; every X error surfaces as a typed
//! [`ToolError`] (`ToolErrorKind::Execution`) — nothing panics or unwraps on
//! X replies. All methods are blocking; the engine wraps them in
//! `tokio::task::spawn_blocking`.
//!
//! Wayland sessions are not supported in this round: readiness reports them
//! as Unsupported with an actionable message. Under XWayland (both
//! `WAYLAND_DISPLAY` and `DISPLAY` set) the tools are reported as available
//! with an informational check — injection and capture reach X11 clients
//! only.
//!
//! The emergency-stop monitor runs on a dedicated X connection (RECORD
//! extension when available, XQueryKeymap polling otherwise). Both are
//! passive: unlike an XGrabKey-based monitor they do not swallow Escape
//! keypresses, so programmatic `gui_key("Escape")` events still reach the
//! focused application (which is exactly why the monitor honors
//! `suppress_next` for injected Escapes).
//!
//! Everything above the `mod x11` block is compiled on every target so the
//! unit tests (no X server required) run on any host; all x11rb code is
//! gated behind `#[cfg(target_os = "linux")]`.

#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use xai_tool_runtime::ToolError;
use xai_tool_runtime::ToolErrorKind;

use super::super::grounding::CaptureArtifact;
use super::super::grounding::CaptureMode;
use super::super::grounding::HelperPoint;
use super::super::grounding::HelperRect;
use super::super::grounding::HostCaptureExclusionState;
use super::super::grounding::ObserveState;
use super::super::grounding::WindowSelector;
use super::super::readiness::GUI_TOOL_NAMES;
use super::super::readiness::GuiEnvironmentReadinessCheck;
use super::super::readiness::GuiEnvironmentReadinessSnapshot;
use super::super::readiness::GuiReadinessStatus;
use super::super::readiness::GuiToolCapability;

use super::super::wire::HelperCaptureContext;
use super::super::wire::HelperDisplayDescriptor;
use super::GuiEmergencyStopMonitor;
use super::GuiPlatform;
use super::PlatformObservation;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

pub(crate) const WAYLAND_UNSUPPORTED_MESSAGE: &str = "Native GUI tools require an X11 session. Switch the session to X11 or run under XWayland with DISPLAY set.";
const HEADLESS_MESSAGE: &str = "Native GUI tools require a display server. Neither WAYLAND_DISPLAY nor DISPLAY is set; run under an X11 session or XWayland with DISPLAY set.";
const XWAYLAND_CHECK_SUMMARY: &str =
    "Session is XWayland: GUI injection and capture reach X11 clients only.";
const XWAYLAND_CHECK_DETAIL: &str = "XWayland composites X11 windows inside a Wayland session. XTEST input injection and GetImage screenshots only affect X11 clients; native Wayland windows are invisible to these tools.";

// ---------------------------------------------------------------------------
// Pure logic (compiled on every target; unit-tested without an X server)
// ---------------------------------------------------------------------------

fn respond_to_model(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorKind::Execution, message)
}

/// Normalize a capture-mode value exactly like the macOS helper protocol
/// (`platform_macos::normalize_capture_mode_env`): trim whitespace, strip
/// leading/trailing non-alphabetic characters, then compare
/// case-insensitively.
pub(crate) fn normalize_capture_mode_env(value: &str) -> Option<&'static str> {
    let trimmed = value
        .trim()
        .trim_matches(|character: char| !character.is_ascii_alphabetic());
    if trimmed.eq_ignore_ascii_case("display") {
        Some("display")
    } else if trimmed.eq_ignore_ascii_case("window") {
        Some("window")
    } else {
        None
    }
}

/// Which display environment the process appears to run in, derived from the
/// session env vars only (no X connection).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisplayEnvironment {
    /// `DISPLAY` set, `WAYLAND_DISPLAY` unset: native/remote X11 session.
    X11Only,
    /// Both set: Wayland session running an XWayland server.
    XWayland,
    /// `WAYLAND_DISPLAY` set, `DISPLAY` unset: unsupported this round.
    WaylandOnly,
    /// Neither set: no display server advertised at all.
    Headless,
}

fn normalize_env_display(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(crate) fn classify_display_environment(
    wayland_display: Option<&str>,
    x11_display: Option<&str>,
) -> DisplayEnvironment {
    match (
        normalize_env_display(wayland_display),
        normalize_env_display(x11_display),
    ) {
        (Some(_), Some(_)) => DisplayEnvironment::XWayland,
        (Some(_), None) => DisplayEnvironment::WaylandOnly,
        (None, Some(_)) => DisplayEnvironment::X11Only,
        (None, None) => DisplayEnvironment::Headless,
    }
}

/// Status ladder shared with the macOS backend: all Unsupported →
/// "unsupported"; any Error → "blocked"; any Warn → "degraded"; else "ready".
pub(crate) fn readiness_status_ladder(statuses: &[GuiReadinessStatus]) -> &'static str {
    if statuses
        .iter()
        .all(|status| *status == GuiReadinessStatus::Unsupported)
    {
        "unsupported"
    } else if statuses.contains(&GuiReadinessStatus::Error) {
        "blocked"
    } else if statuses.contains(&GuiReadinessStatus::Warn) {
        "degraded"
    } else {
        "ready"
    }
}

const XK_RETURN: u32 = 0xff0d;
const XK_TAB: u32 = 0xff09;
const XK_ESCAPE: u32 = 0xff1b;
const XK_BACKSPACE: u32 = 0xff08;
const XK_HOME: u32 = 0xff50;
const XK_PAGE_UP: u32 = 0xff55;
const XK_PAGE_DOWN: u32 = 0xff56;
const XK_END: u32 = 0xff57;
const XK_LEFT: u32 = 0xff51;
const XK_UP: u32 = 0xff52;
const XK_RIGHT: u32 = 0xff54;
const XK_DOWN: u32 = 0xff53;
/// Latin small letter `a` — the select-all chord (`Ctrl+A`) used when a
/// replacement typing clears a field.
const XK_A: u32 = 0x0061;

/// Map a macOS virtual keycode (the values `handler::resolve_key_code` emits
/// into `CODEX_GUI_KEY_CODE`) to its unshifted X11 keysym. Shifted variants
/// are produced by adding Shift to `CODEX_GUI_MODIFIERS`, mirroring the
/// helper protocol.
pub(crate) fn macos_keycode_to_keysym(key_code: i64) -> Option<u32> {
    let keysym = match key_code {
        // Named keys (handler.rs `named` table).
        36 => XK_RETURN,
        48 => XK_TAB,
        53 => XK_ESCAPE,
        51 => XK_BACKSPACE,
        115 => XK_HOME,
        116 => XK_PAGE_UP,
        121 => XK_PAGE_DOWN,
        119 => XK_END,
        126 => XK_UP,
        125 => XK_DOWN,
        123 => XK_LEFT,
        124 => XK_RIGHT,
        49 => 0x020, // space
        // Letter rows (macOS virtual keycodes, unshifted).
        0 => 0x061,  // a
        1 => 0x073,  // s
        2 => 0x064,  // d
        3 => 0x066,  // f
        4 => 0x068,  // h
        5 => 0x067,  // g
        6 => 0x07a,  // z
        7 => 0x078,  // x
        8 => 0x063,  // c
        9 => 0x076,  // v
        11 => 0x062, // b
        12 => 0x071, // q
        13 => 0x077, // w
        14 => 0x065, // e
        15 => 0x072, // r
        16 => 0x079, // y
        17 => 0x074, // t
        31 => 0x06f, // o
        32 => 0x075, // u
        34 => 0x069, // i
        35 => 0x070, // p
        37 => 0x06c, // l
        38 => 0x06a, // j
        40 => 0x06b, // k
        45 => 0x06e, // n
        46 => 0x06d, // m
        // Digits + punctuation row (unshifted).
        18 => 0x031, // 1
        19 => 0x032, // 2
        20 => 0x033, // 3
        21 => 0x034, // 4
        22 => 0x036, // 6
        23 => 0x035, // 5
        24 => 0x03d, // =
        25 => 0x039, // 9
        26 => 0x037, // 7
        27 => 0x02d, // -
        28 => 0x038, // 8
        29 => 0x030, // 0
        30 => 0x05d, // ]
        33 => 0x05b, // [
        39 => 0x027, // '
        41 => 0x03b, // ;
        42 => 0x05c, // backslash
        43 => 0x02c, // ,
        44 => 0x02f, // /
        47 => 0x02e, // .
        _ => return None,
    };
    Some(keysym)
}

/// Map a character to its X keysym plus a "prefer Shift" hint. Lowercase
/// keysyms are preferred so US-style keymaps resolve them at column 0 and
/// uppercase is produced by adding Shift (mirrors the macOS
/// base/shift keycode tables). Latin-1 supplement characters map to their
/// identical keysym code points; anything beyond that has no stable keysym
/// here.
pub(crate) fn keysym_for_char(character: char) -> Option<(u32, bool)> {
    match character {
        'a'..='z' => Some((u32::from(character), false)),
        'A'..='Z' => Some((u32::from(character.to_ascii_lowercase()), true)),
        '0'..='9' => Some((u32::from(character), false)),
        '!' => Some((0x031, true)),
        '@' => Some((0x032, true)),
        '#' => Some((0x033, true)),
        '$' => Some((0x034, true)),
        '%' => Some((0x035, true)),
        '^' => Some((0x036, true)),
        '&' => Some((0x037, true)),
        '*' => Some((0x038, true)),
        '(' => Some((0x039, true)),
        ')' => Some((0x030, true)),
        '-' => Some((0x02d, false)),
        '_' => Some((0x02d, true)),
        '=' => Some((0x03d, false)),
        '+' => Some((0x03d, true)),
        '[' => Some((0x05b, false)),
        '{' => Some((0x05b, true)),
        ']' => Some((0x05d, false)),
        '}' => Some((0x05d, true)),
        '\\' => Some((0x05c, false)),
        '|' => Some((0x05c, true)),
        ';' => Some((0x03b, false)),
        ':' => Some((0x03b, true)),
        '\'' => Some((0x027, false)),
        '"' => Some((0x027, true)),
        ',' => Some((0x02c, false)),
        '<' => Some((0x02c, true)),
        '.' => Some((0x02e, false)),
        '>' => Some((0x02e, true)),
        '/' => Some((0x02f, false)),
        '?' => Some((0x02f, true)),
        '`' => Some((0x060, false)),
        '~' => Some((0x060, true)),
        ' ' => Some((0x020, false)),
        '\t' => Some((XK_TAB, false)),
        '\n' | '\r' => Some((XK_RETURN, false)),
        character if (0xa0..=0xff).contains(&u32::from(character)) => {
            Some((u32::from(character), false))
        }
        _ => None,
    }
}

/// Modifier names accepted in `CODEX_GUI_MODIFIERS`, mapped to X modifier
/// slots (`command`→Mod4/Super, `option`/`alt`→Mod1/Alt — the Super mapping
/// is a documented difference from the macOS Command key).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum XModifier {
    Shift,
    Control,
    Alt,
    Command,
}

pub(crate) fn parse_modifier_names(raw: &str) -> Vec<XModifier> {
    // The helper presses modifiers in control → option → command → shift
    // order; keep that order stable and de-duplicate.
    let mut modifiers = Vec::new();
    for token in raw.split(',') {
        let modifier = match token.trim().to_ascii_lowercase().as_str() {
            "shift" => XModifier::Shift,
            "control" | "ctrl" => XModifier::Control,
            "option" | "alt" => XModifier::Alt,
            "command" => XModifier::Command,
            _ => continue,
        };
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    // Canonical press order.
    let order = |modifier: &XModifier| match modifier {
        XModifier::Control => 0,
        XModifier::Alt => 1,
        XModifier::Command => 2,
        XModifier::Shift => 3,
    };
    modifiers.sort_by_key(order);
    modifiers
}

/// Index into the core `GetModifierMapping` reply (Shift, Lock, Control,
/// Mod1..Mod5) for each modifier.
pub(crate) fn modifier_map_slot(modifier: XModifier) -> usize {
    match modifier {
        XModifier::Shift => 0,
        XModifier::Control => 2,
        XModifier::Alt => 3,     // Mod1
        XModifier::Command => 6, // Mod4
    }
}

/// Extract an optional float from the platform's typed float env slice.
pub(crate) fn float_value(float_env: &[(&str, f64)], key: &str) -> Option<f64> {
    float_env
        .iter()
        .find(|(candidate_key, _)| *candidate_key == key)
        .map(|(_, value)| *value)
}

fn require_float(float_env: &[(&str, f64)], key: &str, event_mode: &str) -> Result<f64, ToolError> {
    float_value(float_env, key).ok_or_else(|| {
        respond_to_model(format!(
            "`{key}` is required for the `{event_mode}` GUI event."
        ))
    })
}

/// Extract an optional string from the platform's typed string env slice.
/// Empty and whitespace-only values count as absent (helper `trimmedEnv`).
pub(crate) fn string_value<'a>(string_env: &'a [(&str, String)], key: &str) -> Option<&'a str> {
    string_env
        .iter()
        .find(|(candidate_key, _)| *candidate_key == key)
        .map(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Parse an optional integer env value (macOS helper `optionalInt`).
fn optional_int(string_env: &[(&str, String)], key: &str) -> Result<Option<i64>, ToolError> {
    match string_value(string_env, key) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .map(Some)
            .map_err(|_| respond_to_model(format!("`{key}` must be an integer, got `{raw}`."))),
    }
}

/// Parse a required integer env value (macOS helper `requiredInt`).
fn required_int(
    string_env: &[(&str, String)],
    key: &str,
    event_mode: &str,
) -> Result<i64, ToolError> {
    optional_int(string_env, key)?.ok_or_else(|| {
        respond_to_model(format!(
            "`{key}` is required for the `{event_mode}` GUI event."
        ))
    })
}

/// Event modes understood by `run_event` (the helper `event` protocol).
pub(crate) const EVENT_MODES: [&str; 9] = [
    "move_cursor",
    "click",
    "click_and_hold",
    "double_click",
    "right_click",
    "drag",
    "scroll",
    "type_text",
    "key_press",
];

const DEFAULT_MOVE_SETTLE_MS: i64 = 200;
const DEFAULT_CLICK_AND_HOLD_MS: i64 = 650;
const DEFAULT_KEYSTROKE_CHAR_DELAY_MS: i64 = 55;
const DEFAULT_CLEAR_REPEAT: i64 = 48;
const KEY_MILLIS: u64 = 20;
const CLICK_MILLIS: u64 = 30;

/// XQueryKeymap fallback: `suppress_next` is only honored within this window
/// of being armed. `expect_escape` arms the flag right before an XTEST
/// Escape held for ~`KEY_MILLIS`; if that press falls between two keymap
/// samples the flag would otherwise stay armed forever and eat the next
/// user-initiated Escape, silently disarming the emergency stop.
const KEYMAP_SUPPRESS_WINDOW_MS: u64 = 500;

// X11 button numbers for the scroll wheel.
const SCROLL_UP_BUTTON: u8 = 4;
const SCROLL_DOWN_BUTTON: u8 = 5;
const SCROLL_LEFT_BUTTON: u8 = 6;
const SCROLL_RIGHT_BUTTON: u8 = 7;
/// X11 wheel ticks are discrete; pixel amounts are divided into ~50px ticks.
const SCROLL_PIXEL_TICK_PX: i64 = 50;
const SCROLL_TICK_MILLIS: u64 = 15;

/// How many X11 button ticks approximate one macOS scroll amount.
pub(crate) fn scroll_tick_count(amount: i64, unit_is_pixel: bool) -> u32 {
    let magnitude = amount.abs();
    if magnitude == 0 {
        return 0;
    }
    if unit_is_pixel {
        (magnitude / SCROLL_PIXEL_TICK_PX).max(1) as u32
    } else {
        magnitude as u32
    }
}

/// Map signed scroll deltas to X11 button ticks: Up=(0,+amount) → button 4,
/// Down → 5, Left=(-amount,0) → 6, Right → 7. Vertical comes first, matching
/// the macOS single-event wheel1/wheel2 ordering.
pub(crate) fn scroll_button_plan(
    scroll_x: i64,
    scroll_y: i64,
    unit_is_pixel: bool,
) -> Vec<(u8, u32)> {
    let mut plan = Vec::new();
    if scroll_y > 0 {
        plan.push((SCROLL_UP_BUTTON, scroll_tick_count(scroll_y, unit_is_pixel)));
    } else if scroll_y < 0 {
        plan.push((
            SCROLL_DOWN_BUTTON,
            scroll_tick_count(scroll_y, unit_is_pixel),
        ));
    }
    if scroll_x > 0 {
        plan.push((
            SCROLL_RIGHT_BUTTON,
            scroll_tick_count(scroll_x, unit_is_pixel),
        ));
    } else if scroll_x < 0 {
        plan.push((
            SCROLL_LEFT_BUTTON,
            scroll_tick_count(scroll_x, unit_is_pixel),
        ));
    }
    plan
}

/// Linear drag waypoints `1..=steps`, inclusive of the endpoint (mirrors the
/// macOS helper drag loop).
pub(crate) fn drag_waypoints(from: (f64, f64), to: (f64, f64), steps: i64) -> Vec<(f64, f64)> {
    let steps = steps.max(1);
    (1..=steps)
        .map(|index| {
            let progress = index as f64 / steps as f64;
            (
                from.0 + ((to.0 - from.0) * progress),
                from.1 + ((to.1 - from.1) * progress),
            )
        })
        .collect()
}

/// Per-step delay for a drag (macOS helper: max(10ms, duration/steps)).
pub(crate) fn drag_step_delay_ms(duration_ms: i64, steps: i64) -> u64 {
    let steps = steps.max(1);
    (duration_ms / steps).max(10) as u64
}

/// `CODEX_GUI_CLEAR_REPEAT` default: 48 backspaces for clipboard_paste, 0
/// otherwise (macOS helper `type_text` semantics), never negative.
pub(crate) fn clear_repeat_for_strategy(
    strategy_is_clipboard_paste: bool,
    explicit: Option<i64>,
) -> i64 {
    explicit
        .unwrap_or(if strategy_is_clipboard_paste {
            DEFAULT_CLEAR_REPEAT
        } else {
            0
        })
        .max(0)
}

/// The three System Events typing strategies that reach the platform
/// (`system_events_*`); unknown values fall back to paste like macOS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemEventsStrategy {
    Paste,
    Keystroke,
    KeystrokeChars,
}

pub(crate) fn system_events_strategy(value: &str) -> SystemEventsStrategy {
    match value {
        "system_events_keystroke" => SystemEventsStrategy::Keystroke,
        "system_events_keystroke_chars" => SystemEventsStrategy::KeystrokeChars,
        _ => SystemEventsStrategy::Paste,
    }
}

/// Whether a freshly observed Escape keypress from the XQueryKeymap fallback
/// should be treated as the expected programmatic injection (consuming
/// `suppress_next`) rather than a user-initiated emergency stop. The flag is
/// only honored within [`KEYMAP_SUPPRESS_WINDOW_MS`] of being armed: an
/// XTEST Escape is held for just ~[`KEY_MILLIS`], so a press observed later
/// than the window cannot be the injection and must stop the session.
pub(crate) fn keymap_press_is_expected(armed: bool, armed_for: Option<Duration>) -> bool {
    armed
        && armed_for
            .is_some_and(|elapsed| elapsed <= Duration::from_millis(KEYMAP_SUPPRESS_WINDOW_MS))
}

/// Integer rectangle used by the pure geometry helpers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RectI {
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) width: i64,
    pub(crate) height: i64,
}

impl RectI {
    fn right(&self) -> i64 {
        self.x + self.width
    }

    fn bottom(&self) -> i64 {
        self.y + self.height
    }

    pub(crate) fn contains_point(&self, point: (f64, f64)) -> bool {
        point.0 >= self.x as f64
            && point.0 < self.right() as f64
            && point.1 >= self.y as f64
            && point.1 < self.bottom() as f64
    }

    fn union(self, other: RectI) -> RectI {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        RectI {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }
}

/// One monitor (CRTC) as the pure selection math sees it. Indices are
/// 1-based like the macOS helper's display list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DisplayInfo {
    pub(crate) index: i64,
    pub(crate) bounds: RectI,
}

/// macOS `displayForPoint`: the first display containing the point, else the
/// first display.
pub(crate) fn display_for_point(
    point: (f64, f64),
    displays: &[DisplayInfo],
) -> Option<DisplayInfo> {
    displays
        .iter()
        .find(|display| display.bounds.contains_point(point))
        .or_else(|| displays.first())
        .copied()
}

/// Window candidate as the pure ranking/selection math sees it. Bounds are
/// the window-manager frame bounds in global coordinates (the macOS
/// `CGWindowList` analog).
#[derive(Clone, Debug)]
pub(crate) struct WindowCandidate {
    pub(crate) id: i64,
    pub(crate) title: Option<String>,
    pub(crate) bounds: RectI,
    /// 0 = topmost in the server's stacking order.
    pub(crate) stacking_index: usize,
}

/// macOS `rankWindows` translated to X11: the stacking order already encodes
/// "which window the user sees", so topmost wins, then larger area, then
/// titled windows, then the XID for stability.
pub(crate) fn rank_windows(mut windows: Vec<WindowCandidate>) -> Vec<WindowCandidate> {
    windows.sort_by(|left, right| {
        left.stacking_index
            .cmp(&right.stacking_index)
            .then(right.area_key().cmp(&left.area_key()))
            .then(right.has_title().cmp(&left.has_title()))
            .then(left.id.cmp(&right.id))
    });
    windows
}

impl WindowCandidate {
    fn area_key(&self) -> i64 {
        self.bounds.width * self.bounds.height
    }

    fn has_title(&self) -> bool {
        self.title
            .as_deref()
            .map(str::trim)
            .is_some_and(|title| !title.is_empty())
    }
}

/// macOS `selectedWindow` outcome: primary window, count and strategy label.
#[derive(Clone, Debug)]
pub(crate) struct WindowSelectionOutcome {
    pub(crate) primary: WindowCandidate,
    pub(crate) window_count: i64,
    pub(crate) strategy: &'static str,
    /// Capture bounds: primary frame bounds, or the union of all matches for
    /// `app_union`.
    pub(crate) bounds: RectI,
}

pub(crate) fn select_window(
    ranked: &[WindowCandidate],
    index: Option<i64>,
    has_explicit_selection: bool,
) -> Option<WindowSelectionOutcome> {
    if ranked.is_empty() {
        return None;
    }
    if let Some(index) = index.filter(|index| *index > 0)
        && let Some(primary) = ranked.get((index - 1) as usize)
    {
        return Some(WindowSelectionOutcome {
            primary: primary.clone(),
            window_count: 1,
            strategy: "selected_window",
            bounds: primary.bounds,
        });
    }
    // macOS falls through to the ranked-first window for an
    // out-of-range index.
    let primary = ranked[0].clone();
    if has_explicit_selection {
        return Some(WindowSelectionOutcome {
            primary,
            window_count: 1,
            strategy: "selected_window",
            bounds: ranked[0].bounds,
        });
    }
    if ranked.len() == 1 {
        return Some(WindowSelectionOutcome {
            window_count: 1,
            strategy: "main_window",
            bounds: primary.bounds,
            primary,
        });
    }
    let mut bounds = ranked[0].bounds;
    for candidate in &ranked[1..] {
        bounds = bounds.union(candidate.bounds);
    }
    Some(WindowSelectionOutcome {
        primary,
        window_count: ranked.len() as i64,
        strategy: "app_union",
        bounds,
    })
}

/// App identity match mirroring the macOS `matchesAppName` semantics widened
/// per the X11 contract: case-insensitive equality on WM_CLASS instance,
/// WM_CLASS class, and `/proc/<pid>/comm`; equality or containment on the
/// window title.
pub(crate) fn app_matches_window(
    requested: &str,
    class_instance: Option<&str>,
    class_name: Option<&str>,
    title: Option<&str>,
    comm: Option<&str>,
) -> bool {
    let normalized = requested.trim().to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    for candidate in [class_instance, class_name, comm] {
        if let Some(candidate) = candidate
            && candidate.trim().to_lowercase() == normalized
        {
            return true;
        }
    }
    if let Some(title) = title {
        let title = title.trim().to_lowercase();
        if title == normalized || title.contains(&normalized) {
            return true;
        }
    }
    false
}

/// Payload of an ICCCM/EWMH client message, as pure data so it can be unit
/// tested without atoms. `format` is always 32 and `values` fills
/// `data.l[0..5]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClientMessageSpec {
    pub(crate) format: u8,
    pub(crate) values: [u32; 5],
}

pub(crate) const WM_STATE_ICONIC: u32 = 3;
pub(crate) const WM_STATE_NORMAL: u32 = 1;

/// `WM_CHANGE_STATE` client message sent *to a client's top-level window* to
/// request a state transition (IconicState = iconify, NormalState = restore).
pub(crate) fn wm_change_state_message(state: u32) -> ClientMessageSpec {
    ClientMessageSpec {
        format: 32,
        values: [state, 0, 0, 0, 0],
    }
}

/// `_NET_ACTIVE_WINDOW` root client message requesting activation
/// (source indication 2 = pager, CurrentTime, no requestor).
pub(crate) fn net_active_window_message() -> ClientMessageSpec {
    ClientMessageSpec {
        format: 32,
        values: [2, 0, 0, 0, 0],
    }
}

/// Does a RECORD data-stream chunk begin with a core KeyPress for
/// `escape_keycode`? (Core KeyPress event: byte 0 = response type 2, byte 1 =
/// keycode.)
pub(crate) fn is_escape_key_press(chunk: &[u8], escape_keycode: u8) -> bool {
    chunk.len() >= 2 && chunk[0] == xproto_key_press_event() && chunk[1] == escape_keycode
}

/// Core KeyPress event response type (2). Plain constant so the pure helper
/// stays x11rb-free.
pub(crate) fn xproto_key_press_event() -> u8 {
    2
}

/// Build a keysym → (keycode, column) reverse map from a
/// `GetKeyboardMapping` reply. Column 0 entries win over column 1 (unshifted
/// preferred). `min_keycode` is the keycode of the first keysym group.
pub(crate) fn build_keysym_map(
    min_keycode: u8,
    keysyms_per_keycode: u8,
    keysyms: &[u32],
) -> std::collections::BTreeMap<u32, (u8, u8)> {
    let per = keysyms_per_keycode.max(1) as usize;
    let mut map = std::collections::BTreeMap::new();
    for (group_index, group) in keysyms.chunks(per).enumerate() {
        let keycode = min_keycode.saturating_add(group_index as u8);
        for (column, keysym) in group.iter().enumerate() {
            if *keysym == 0 {
                continue;
            }
            let column = column as u8;
            match map.get(keysym) {
                Some((_, existing_column)) if *existing_column <= column => {}
                _ => {
                    map.insert(*keysym, (keycode, column));
                }
            }
        }
    }
    map
}

/// Extract the keycode for a modifier slot from a `GetModifierMapping`
/// reply (8 slots of `keys_per_mod` keycodes). Keycode 0 means the slot is
/// empty.
pub(crate) fn modifier_keycode_from_reply(
    keycodes: &[u8],
    keys_per_mod: usize,
    slot: usize,
) -> Option<u8> {
    let keys_per_mod = keys_per_mod.max(1);
    keycodes
        .get(slot * keys_per_mod)
        .copied()
        .filter(|keycode| *keycode != 0)
}

/// Serialize 32-bit property data (`format = 32`) in the server's byte
/// order: XChangeProperty's data must match `image_byte_order`.
pub(crate) fn property32_bytes(values: &[u32], lsb_first: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        let raw = if lsb_first {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        bytes.extend_from_slice(&raw);
    }
    bytes
}

// ---------------------------------------------------------------------------
// Platform dispatch entry point
// ---------------------------------------------------------------------------

pub(crate) struct LinuxGuiPlatform;

#[cfg(target_os = "linux")]
mod x11 {
    //! x11rb-backed implementation. Everything in this module is compiled
    //! only on Linux targets (the x11rb dependency is target-gated in
    //! Cargo.toml).

    use std::collections::BTreeMap;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::thread::JoinHandle;
    use std::time::Duration;
    use std::time::Instant;

    use x11rb::CURRENT_TIME;
    use x11rb::connection::Connection;
    use x11rb::connection::RequestConnection;
    use x11rb::errors::ReplyError;
    use x11rb::protocol::Event;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::record;
    use x11rb::protocol::record::ConnectionExt as _;
    use x11rb::protocol::xproto;
    use x11rb::protocol::xproto::AtomEnum;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    use super::*;

    /// libXtst's XTEST device id for FakeInput requests.
    const XTEST_DEVICE_ID: u8 = 1;
    /// Escape monitor poll cadence for the XQueryKeymap fallback. An XTEST
    /// Escape is held for ~[`KEY_MILLIS`] (20ms), so the fallback must
    /// sample faster than that hold to observe injected presses at all.
    const KEYMAP_POLL_MILLIS: u64 = 10;
    /// Settle time after EWMH activation (macOS helper: 100ms).
    const ACTIVATE_SETTLE_MS: u64 = 100;
    /// Minimum visible window size for capture selection (macOS helper).
    const MIN_WINDOW_EDGE: i64 = 80;
    /// macOS helper `pasteText` (event protocol): clipboard set, 100ms,
    /// Cmd+V, 150ms.
    const PASTE_EVENT_PRE_DELAY_MS: u64 = 100;
    const PASTE_EVENT_POST_DELAY_MS: u64 = 150;
    /// macOS System Events `set the clipboard + keystroke "v" using command`
    /// delays: 220ms / 650ms.
    const PASTE_SYSTEM_PRE_DELAY_MS: u64 = 220;
    const PASTE_SYSTEM_POST_DELAY_MS: u64 = 650;
    /// macOS helper `typePhysicalKeyText` per-char delay.
    const PHYSICAL_KEY_CHAR_DELAY_MS: u64 = 90;
    /// macOS helper `typeUnicodeText` per-char delay.
    const UNICODE_CHAR_DELAY_MS: u64 = 30;
    /// X11 STRING atom (predefined `XA_STRING`).
    const X_ATOM_STRING: xproto::Atom = 31;

    fn map_x11<T, E: std::fmt::Display>(
        result: Result<T, E>,
        context: &str,
    ) -> Result<T, ToolError> {
        result.map_err(|error| respond_to_model(format!("{context}: {error}")))
    }

    /// Flatten `request().map(|cookie| cookie.reply()/check())` — the
    /// `Result<Result<T, ReplyError>, ConnectionError>` shape — into a typed
    /// [`ToolError`]. Reply-level and connection-level failures are reported
    /// identically.
    fn flatten_reply<T, E: std::fmt::Display>(
        result: Result<Result<T, ReplyError>, E>,
        context: &str,
    ) -> Result<T, ToolError> {
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(respond_to_model(format!("{context}: {error}"))),
            Err(error) => Err(respond_to_model(format!("{context}: {error}"))),
        }
    }

    // -- Atoms ------------------------------------------------------------

    struct Atoms {
        net_client_list: xproto::Atom,
        net_client_list_stacking: xproto::Atom,
        net_active_window: xproto::Atom,
        net_wm_name: xproto::Atom,
        net_wm_pid: xproto::Atom,
        net_wm_window_type: xproto::Atom,
        net_wm_window_type_normal: xproto::Atom,
        net_wm_window_type_dialog: xproto::Atom,
        net_frame_extents: xproto::Atom,
        wm_name: xproto::Atom,
        wm_class: xproto::Atom,
        wm_state: xproto::Atom,
        wm_change_state: xproto::Atom,
        utf8_string: xproto::Atom,
        targets: xproto::Atom,
        clipboard: xproto::Atom,
    }

    impl Atoms {
        fn intern(conn: &RustConnection) -> Result<Self, ToolError> {
            macro_rules! intern {
                ($name:expr) => {
                    flatten_reply(
                        conn.intern_atom(false, $name.as_bytes())
                            .map(|cookie| cookie.reply()),
                        concat!("failed to intern atom ", $name),
                    )?
                    .atom
                };
            }
            Ok(Self {
                net_client_list: intern!("_NET_CLIENT_LIST"),
                net_client_list_stacking: intern!("_NET_CLIENT_LIST_STACKING"),
                net_active_window: intern!("_NET_ACTIVE_WINDOW"),
                net_wm_name: intern!("_NET_WM_NAME"),
                net_wm_pid: intern!("_NET_WM_PID"),
                net_wm_window_type: intern!("_NET_WM_WINDOW_TYPE"),
                net_wm_window_type_normal: intern!("_NET_WM_WINDOW_TYPE_NORMAL"),
                net_wm_window_type_dialog: intern!("_NET_WM_WINDOW_TYPE_DIALOG"),
                net_frame_extents: intern!("_NET_FRAME_EXTENTS"),
                wm_name: intern!("WM_NAME"),
                wm_class: intern!("WM_CLASS"),
                wm_state: intern!("WM_STATE"),
                wm_change_state: intern!("WM_CHANGE_STATE"),
                utf8_string: intern!("UTF8_STRING"),
                targets: intern!("TARGETS"),
                clipboard: intern!("CLIPBOARD"),
            })
        }
    }

    // -- Cached X session -------------------------------------------------

    struct X11Session {
        conn: RustConnection,
        root: xproto::Window,
        root_width: u16,
        root_height: u16,
        atoms: Atoms,
        lsb_first: bool,
        xtest_available: bool,
        capture_available: bool,
        clipboard_window: xproto::Window,
        clipboard_text: Mutex<Option<String>>,
        keysym_map: OnceLock<BTreeMap<u32, (u8, u8)>>,
        modifier_keycodes: OnceLock<Vec<u8>>,
    }

    impl X11Session {
        fn connect() -> Result<Self, ToolError> {
            let (conn, screen_index) = map_x11(
                RustConnection::connect(None),
                "failed to connect to the X11 display",
            )?;
            let screen = conn
                .setup()
                .roots
                .get(screen_index)
                .ok_or_else(|| respond_to_model("X11 connection reported no usable screen"))?;
            let root_window = screen.root;
            let root_width = screen.width_in_pixels;
            let root_height = screen.height_in_pixels;
            let lsb_first = matches!(
                conn.setup().image_byte_order,
                x11rb::protocol::xproto::ImageOrder::LSB_FIRST
            );

            let atoms = Atoms::intern(&conn)?;

            // XTEST availability probe (input injection).
            let xtest_available = conn
                .xtest_get_version(2, 2)
                .map(|cookie| cookie.reply().is_ok())
                .unwrap_or(false);

            // Capture availability probe: 1x1 root GetImage.
            let capture_available = conn
                .get_image(
                    x11rb::protocol::xproto::ImageFormat::Z_PIXMAP,
                    root_window,
                    0,
                    0,
                    1,
                    1,
                    u32::MAX,
                )
                .map(|cookie| cookie.reply().is_ok())
                .unwrap_or(false);

            // Unmapped 1x1 InputOnly window used to own the CLIPBOARD
            // selection while pasting.
            let clipboard_window = conn.generate_id().map_err(|error| {
                respond_to_model(format!("failed to allocate an X resource id: {error}"))
            })?;
            flatten_reply(
                conn.create_window(
                    0,
                    clipboard_window,
                    root_window,
                    -1,
                    -1,
                    1,
                    1,
                    0,
                    xproto::WindowClass::INPUT_ONLY,
                    0, // CopyFromParent
                    &xproto::CreateWindowAux::new(),
                )
                .map(|cookie| cookie.check()),
                "failed to create the X11 clipboard owner window",
            )?;
            map_x11(conn.flush(), "failed to flush the X11 connection")?;

            let session = Self {
                conn,
                root: root_window,
                root_width,
                root_height,
                atoms,
                lsb_first,
                xtest_available,
                capture_available,
                clipboard_window,
                clipboard_text: Mutex::new(None),
                keysym_map: OnceLock::new(),
                modifier_keycodes: OnceLock::new(),
            };
            // Warm the keyboard caches; failures here do not invalidate the
            // session (individual tools report their own errors).
            let _ = session.keysym_reverse_map();
            let _ = session.modifier_keycode_table();
            Ok(session)
        }

        /// Process-global lazily-connected session. Connect failures are not
        /// cached so a server that appears later is picked up by readiness
        /// re-probes.
        fn shared() -> Result<Arc<Self>, ToolError> {
            static X11_SESSION: OnceLock<Mutex<Option<Arc<X11Session>>>> = OnceLock::new();
            let cell = X11_SESSION.get_or_init(|| Mutex::new(None));
            let mut guard = match cell.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(session) = guard.as_ref() {
                return Ok(Arc::clone(session));
            }
            let session = Arc::new(Self::connect()?);
            *guard = Some(Arc::clone(&session));
            Ok(session)
        }

        fn keysym_reverse_map(&self) -> Result<&BTreeMap<u32, (u8, u8)>, ToolError> {
            if let Some(map) = self.keysym_map.get() {
                return Ok(map);
            }
            let setup = self.conn.setup();
            let min = setup.min_keycode;
            let max = setup.max_keycode;
            if max < min {
                return Err(respond_to_model(
                    "X11 keyboard mapping reported an empty key range",
                ));
            }
            let count = (max - min).saturating_add(1);
            let reply = flatten_reply(
                self.conn
                    .get_keyboard_mapping(min, count)
                    .map(|cookie| cookie.reply()),
                "failed to read the X11 keyboard mapping",
            )?;
            let map = build_keysym_map(min, reply.keysyms_per_keycode, &reply.keysyms);
            let _ = self.keysym_map.set(map);
            Ok(self
                .keysym_map
                .get()
                .ok_or_else(|| respond_to_model("X11 keyboard map cache was not populated"))?)
        }

        fn modifier_keycode_table(&self) -> Result<&[u8], ToolError> {
            if let Some(table) = self.modifier_keycodes.get() {
                return Ok(table);
            }
            let reply = flatten_reply(
                self.conn
                    .get_modifier_mapping()
                    .map(|cookie| cookie.reply()),
                "failed to read the X11 modifier mapping",
            )?;
            let table = reply.keycodes;
            let _ = self.modifier_keycodes.set(table);
            Ok(self
                .modifier_keycodes
                .get()
                .map(Vec::as_slice)
                .ok_or_else(|| respond_to_model("X11 modifier mapping cache was not populated"))?)
        }

        fn keys_per_modifier(&self) -> usize {
            self.modifier_keycode_table()
                .map(|table| table.len() / 8)
                .unwrap_or(0)
        }

        fn modifier_keycode(&self, modifier: XModifier) -> Option<u8> {
            let table = self.modifier_keycode_table().ok()?;
            modifier_keycode_from_reply(
                table,
                self.keys_per_modifier(),
                modifier_map_slot(modifier),
            )
        }

        fn keycode_for_keysym(&self, keysym: u32) -> Option<(u8, bool)> {
            self.keysym_reverse_map()
                .ok()?
                .get(&keysym)
                .map(|(keycode, column)| (*keycode, *column >= 1))
        }

        // -- XTEST primitives --------------------------------------------

        fn fake_input(
            &self,
            type_: u8,
            detail: u8,
            root_x: i16,
            root_y: i16,
        ) -> Result<(), ToolError> {
            flatten_reply(
                self.conn
                    .xtest_fake_input(type_, detail, 0, self.root, root_x, root_y, XTEST_DEVICE_ID)
                    .map(|cookie| cookie.check()),
                "XTEST input injection failed",
            )
        }

        fn fake_motion(&self, x: f64, y: f64) -> Result<(), ToolError> {
            self.fake_input(
                xproto::MOTION_NOTIFY_EVENT,
                0,
                clamp_i16(x.round()),
                clamp_i16(y.round()),
            )
        }

        fn fake_button(&self, button: u8, press: bool) -> Result<(), ToolError> {
            self.fake_input(
                if press {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                },
                button,
                0,
                0,
            )
        }

        fn fake_key(&self, keycode: u8, press: bool) -> Result<(), ToolError> {
            self.fake_input(
                if press {
                    xproto::KEY_PRESS_EVENT
                } else {
                    xproto::KEY_RELEASE_EVENT
                },
                keycode,
                0,
                0,
            )
        }

        /// Move the pointer and let the server settle (macOS `moveCursor`).
        fn move_cursor(&self, x: f64, y: f64) -> Result<(), ToolError> {
            self.fake_motion(x, y)?;
            std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
            Ok(())
        }

        /// Mirror of the macOS helper `pressKeyCode`: press modifiers in
        /// control → alt → command → shift order, tap the key, release in
        /// reverse.
        fn press_keycode_with_modifiers(
            &self,
            keycode: u8,
            needs_shift: bool,
            modifiers: &[XModifier],
        ) -> Result<(), ToolError> {
            let mut sequence: Vec<u8> = Vec::new();
            for modifier in modifiers {
                if let Some(modifier_keycode) = self.modifier_keycode(*modifier) {
                    sequence.push(modifier_keycode);
                }
            }
            if needs_shift && !modifiers.contains(&XModifier::Shift) {
                if let Some(shift_keycode) = self.modifier_keycode(XModifier::Shift) {
                    sequence.push(shift_keycode);
                }
            }

            for modifier_keycode in &sequence {
                self.fake_key(*modifier_keycode, true)?;
                std::thread::sleep(Duration::from_millis(KEY_MILLIS));
            }
            self.fake_key(keycode, true)?;
            std::thread::sleep(Duration::from_millis(KEY_MILLIS));
            self.fake_key(keycode, false)?;
            for modifier_keycode in sequence.iter().rev() {
                std::thread::sleep(Duration::from_millis(KEY_MILLIS));
                self.fake_key(*modifier_keycode, false)?;
            }
            std::thread::sleep(Duration::from_millis(KEY_MILLIS));
            Ok(())
        }

        /// Resolve a keysym against the current keymap and tap it.
        fn inject_keysym(
            &self,
            keysym: u32,
            prefer_shift: bool,
            modifiers: &[XModifier],
        ) -> Result<(), ToolError> {
            let Some((keycode, column_shift)) = self.keycode_for_keysym(keysym) else {
                return Err(respond_to_model(format!(
                    "the X11 keymap has no key for keysym `0x{keysym:04x}`; this character cannot be typed with the current layout. Use the `clipboard_paste` strategy or switch to a US-style keymap."
                )));
            };
            self.press_keycode_with_modifiers(keycode, prefer_shift || column_shift, modifiers)
        }

        fn inject_char(&self, character: char) -> Result<(), ToolError> {
            let Some((keysym, prefer_shift)) = keysym_for_char(character) else {
                return Err(respond_to_model(format!(
                    "the X11 backend cannot type the character `{character}` (no keysym mapping); use the `clipboard_paste` strategy instead."
                )));
            };
            self.inject_keysym(keysym, prefer_shift, &[])
        }

        // -- Window enumeration -------------------------------------------

        fn property_text(&self, window: xproto::Window, atom: xproto::Atom) -> Option<String> {
            let reply = self
                .conn
                .get_property(false, window, atom, AtomEnum::ANY, 0, 8192)
                .ok()?
                .reply()
                .ok()?;
            if reply.value_len == 0 {
                return None;
            }
            let text = String::from_utf8_lossy(&reply.value).trim().to_string();
            (!text.is_empty()).then_some(text)
        }

        fn property_u32(&self, window: xproto::Window, atom: xproto::Atom) -> Option<Vec<u32>> {
            let reply = self
                .conn
                .get_property(false, window, atom, AtomEnum::ANY, 0, 8192)
                .ok()?
                .reply()
                .ok()?;
            reply.value32()?.collect::<Vec<u32>>().into()
        }

        fn window_pid(&self, window: xproto::Window) -> Option<i32> {
            let pids = self.property_u32(window, self.atoms.net_wm_pid)?;
            let pid = i32::try_from(*pids.first()?).ok()?;
            (pid > 0).then_some(pid)
        }

        fn comm_for_pid(pid: i32) -> Option<String> {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
            let comm = comm.trim().to_string();
            (!comm.is_empty()).then_some(comm)
        }

        fn window_class(&self, window: xproto::Window) -> (Option<String>, Option<String>) {
            let reply = match self
                .conn
                .get_property(
                    false,
                    window,
                    self.atoms.wm_class,
                    AtomEnum::STRING,
                    0,
                    8192,
                )
                .ok()
                .and_then(|cookie| cookie.reply().ok())
            {
                Some(reply) if !reply.value.is_empty() => reply,
                _ => return (None, None),
            };
            let raw = String::from_utf8_lossy(&reply.value).to_string();
            let mut parts = raw.split('\0').filter(|part| !part.is_empty());
            let instance = parts.next().map(|part| part.trim().to_string());
            let class = parts.next().map(|part| part.trim().to_string());
            (
                instance.filter(|part| !part.is_empty()),
                class.filter(|part| !part.is_empty()),
            )
        }

        fn window_title(&self, window: xproto::Window) -> Option<String> {
            self.property_text(window, self.atoms.net_wm_name)
                .or_else(|| self.property_text(window, self.atoms.wm_name))
        }

        fn is_regular_window_type(&self, window: xproto::Window) -> bool {
            match self.property_u32(window, self.atoms.net_wm_window_type) {
                // Older apps set no window type: treat as regular.
                None => true,
                Some(types) if types.is_empty() => true,
                Some(types) => types.iter().any(|type_atom| {
                    *type_atom == self.atoms.net_wm_window_type_normal
                        || *type_atom == self.atoms.net_wm_window_type_dialog
                }),
            }
        }

        fn window_state_is_normal(&self, window: xproto::Window) -> bool {
            match self.property_u32(window, self.atoms.wm_state) {
                Some(states) => states.first().copied() != Some(WM_STATE_ICONIC),
                None => true, // no WM_STATE: treat as visible
            }
        }

        /// Window-manager frame rect in root coordinates (client rect grown
        /// by `_NET_FRAME_EXTENTS`; falls back to the client rect when the
        /// extents are unavailable).
        fn window_frame_rect(&self, window: xproto::Window) -> Result<RectI, ToolError> {
            let geometry = flatten_reply(
                self.conn.get_geometry(window).map(|cookie| cookie.reply()),
                "failed to read X11 window geometry",
            )?;
            let translated = flatten_reply(
                self.conn
                    .translate_coordinates(window, self.root, 0, 0)
                    .map(|cookie| cookie.reply()),
                "failed to translate X11 window coordinates",
            )?;
            let client = RectI {
                x: i64::from(translated.dst_x),
                y: i64::from(translated.dst_y),
                width: i64::from(geometry.width),
                height: i64::from(geometry.height),
            };
            Ok(
                match self.property_u32(window, self.atoms.net_frame_extents) {
                    Some(extents) if extents.len() >= 4 => RectI {
                        x: client.x - i64::from(extents[0]),
                        y: client.y - i64::from(extents[2]),
                        width: client.width + i64::from(extents[0]) + i64::from(extents[1]),
                        height: client.height + i64::from(extents[2]) + i64::from(extents[3]),
                    },
                    _ => client,
                },
            )
        }

        /// Enumerate managed clients, topmost first. Only regular
        /// (non-dock/desktop/menu) windows are returned.
        fn enumerate_clients(&self) -> Result<Vec<XClient>, ToolError> {
            let stacked = self.property_u32(self.root, self.atoms.net_client_list_stacking);
            let client_list = self.property_u32(self.root, self.atoms.net_client_list);
            let windows: Vec<xproto::Window> = match (stacked, client_list) {
                (Some(stacked), _) => stacked,
                (None, Some(client_list)) => client_list,
                (None, None) => {
                    let tree = flatten_reply(
                        self.conn.query_tree(self.root).map(|cookie| cookie.reply()),
                        "failed to query X11 window tree",
                    )?;
                    tree.children
                }
            };
            // Both `_NET_CLIENT_LIST_STACKING` and `query_tree` list windows
            // bottom-most first, so reversed enumeration makes index 0 the
            // topmost window.
            let mut clients = Vec::new();
            for (stacking_index, &window) in windows.iter().rev().enumerate() {
                let Ok(client) = self.describe_client(window, stacking_index) else {
                    continue;
                };
                clients.push(client);
            }
            Ok(clients)
        }

        fn describe_client(
            &self,
            window: xproto::Window,
            stacking_index: usize,
        ) -> Result<XClient, ToolError> {
            let (class_instance, class_name) = self.window_class(window);
            let title = self.window_title(window);
            let pid = self.window_pid(window);
            let comm = pid.and_then(Self::comm_for_pid);
            let frame_rect = self.window_frame_rect(window)?;
            Ok(XClient {
                window,
                stacking_index,
                class_instance,
                class_name,
                title,
                pid,
                comm,
                frame_rect,
                regular_type: self.is_regular_window_type(window),
                state_normal: self.window_state_is_normal(window),
            })
        }

        fn visible_regular_clients(&self) -> Result<Vec<XClient>, ToolError> {
            Ok(self
                .enumerate_clients()?
                .into_iter()
                .filter(|client| client.regular_type && client.state_normal)
                .collect())
        }

        // -- Displays ------------------------------------------------------

        fn displays(&self) -> Vec<DisplayInfo> {
            let fallback = || {
                vec![DisplayInfo {
                    index: 1,
                    bounds: RectI {
                        x: 0,
                        y: 0,
                        width: i64::from(self.root_width),
                        height: i64::from(self.root_height),
                    },
                }]
            };
            let Ok(resources) = flatten_reply(
                self.conn
                    .randr_get_screen_resources_current(self.root)
                    .map(|cookie| cookie.reply()),
                "failed to read RANDR screen resources",
            ) else {
                return fallback();
            };
            let mut crtc_bounds: Vec<(i64, i64, RectI)> = Vec::new();
            for &crtc in &resources.crtcs {
                let Ok(info) = flatten_reply(
                    self.conn
                        .randr_get_crtc_info(crtc, resources.config_timestamp)
                        .map(|cookie| cookie.reply()),
                    "failed to read RANDR CRTC info",
                ) else {
                    continue;
                };
                if info.outputs.is_empty() || info.width == 0 || info.height == 0 {
                    continue;
                }
                crtc_bounds.push((
                    i64::from(info.y),
                    i64::from(info.x),
                    RectI {
                        x: i64::from(info.x),
                        y: i64::from(info.y),
                        width: i64::from(info.width),
                        height: i64::from(info.height),
                    },
                ));
            }
            if crtc_bounds.is_empty() {
                return fallback();
            }
            crtc_bounds.sort_by_key(|(y, x, _)| (*y, *x));
            crtc_bounds
                .into_iter()
                .enumerate()
                .map(|(index, (_, _, bounds))| DisplayInfo {
                    index: index as i64 + 1,
                    bounds,
                })
                .collect()
        }

        fn cursor_position(&self) -> Result<(f64, f64), ToolError> {
            let reply = flatten_reply(
                self.conn
                    .query_pointer(self.root)
                    .map(|cookie| cookie.reply()),
                "failed to query the X11 pointer position",
            )?;
            Ok((f64::from(reply.root_x), f64::from(reply.root_y)))
        }

        // -- App/window resolution -----------------------------------------

        fn active_window(&self) -> Option<xproto::Window> {
            let active = self.property_u32(self.root, self.atoms.net_active_window)?;
            let window = *active.first()?;
            (window != 0).then_some(window)
        }

        /// Windows of `clients` ranked for selection (frame bounds; minimum
        /// 80x80 like the macOS helper).
        fn ranked_candidates(&self, clients: &[XClient]) -> Vec<WindowCandidate> {
            rank_windows(
                clients
                    .iter()
                    .filter(|client| {
                        client.frame_rect.width >= MIN_WINDOW_EDGE
                            && client.frame_rect.height >= MIN_WINDOW_EDGE
                    })
                    .map(|client| WindowCandidate {
                        id: i64::from(client.window),
                        title: client.title.clone(),
                        bounds: client.frame_rect,
                        stacking_index: client.stacking_index,
                    })
                    .collect(),
            )
        }

        /// Resolve the target app + window the way the macOS helper's
        /// `capture-context` does. `app = None` targets the frontmost app
        /// (owner of `_NET_ACTIVE_WINDOW`).
        fn resolve_target(
            &self,
            app: Option<&str>,
            window_selection: Option<&WindowSelector>,
        ) -> Result<(Option<String>, Option<WindowSelectionOutcome>), ToolError> {
            let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
            let clients = self.visible_regular_clients()?;
            let mut target_clients: Vec<XClient> = match requested_app {
                Some(requested_app) => clients
                    .iter()
                    .filter(|client| client.matches_app(requested_app))
                    .cloned()
                    .collect(),
                None => {
                    // Frontmost app: the client owning _NET_ACTIVE_WINDOW.
                    let active = self.active_window();
                    let active_owner = active.and_then(|window| {
                        clients
                            .iter()
                            .find(|client| client.window == window)
                            .map(XClient::identity_key)
                    });
                    match (active, active_owner) {
                        (Some(_), Some(identity)) => clients
                            .iter()
                            .filter(|client| client.identity_key() == identity)
                            .cloned()
                            .collect(),
                        (Some(_), None) => Vec::new(),
                        (None, _) => clients.clone(),
                    }
                }
            };
            target_clients.sort_by_key(|client| client.stacking_index);

            let app_name = target_clients
                .first()
                .map(XClient::app_display_name)
                .or_else(|| requested_app.map(ToOwned::to_owned));

            let (title, title_contains, index) = match window_selection {
                Some(selection) => {
                    let title = selection
                        .title
                        .as_deref()
                        .map(str::trim)
                        .filter(|title| !title.is_empty());
                    let title_contains = selection
                        .title_contains
                        .as_deref()
                        .map(str::trim)
                        .filter(|title| !title.is_empty());
                    (title, title_contains, selection.index)
                }
                None => (None, None, None),
            };
            let has_explicit_selection =
                title.is_some() || title_contains.is_some() || index.is_some();

            let filtered: Vec<XClient> = target_clients
                .iter()
                .filter(|client| {
                    title.is_none_or(|title| {
                        client
                            .title
                            .as_deref()
                            .is_some_and(|candidate| normalized(candidate) == normalized(title))
                    }) && title_contains.is_none_or(|title_contains| {
                        client.title.as_deref().is_some_and(|candidate| {
                            normalized(candidate).contains(&normalized(title_contains))
                        })
                    })
                })
                .cloned()
                .collect();

            let ranked = self.ranked_candidates(&filtered);
            let selection = select_window(&ranked, index, has_explicit_selection);
            Ok((app_name, selection))
        }

        /// EWMH activation. Errors when no window matches the app (mirrors
        /// the macOS helper's `activateApplication`).
        fn activate_app(&self, app: &str) -> Result<(), ToolError> {
            let clients = self.visible_regular_clients()?;
            let matches: Vec<&XClient> = clients
                .iter()
                .filter(|client| client.matches_app(app))
                .collect();
            let Some(target) = matches.first() else {
                return Err(respond_to_model(format!(
                    "The application `{app}` could not be found on the X11 display."
                )));
            };
            // Already focused? (macOS skips re-activation too.)
            if self.active_window() == Some(target.window) {
                return Ok(());
            }
            self.send_root_message(
                target.window,
                self.atoms.net_active_window,
                net_active_window_message(),
            )?;
            std::thread::sleep(Duration::from_millis(ACTIVATE_SETTLE_MS));
            Ok(())
        }

        fn send_root_message(
            &self,
            event_window: xproto::Window,
            message_type: xproto::Atom,
            spec: ClientMessageSpec,
        ) -> Result<(), ToolError> {
            // EWMH messages go to the root window with the target window as
            // the event's window member.
            let event = xproto::ClientMessageEvent::new(
                spec.format,
                event_window,
                message_type,
                spec.values,
            );
            flatten_reply(
                self.conn
                    .send_event(
                        false,
                        self.root,
                        xproto::EventMask::SUBSTRUCTURE_NOTIFY
                            | xproto::EventMask::SUBSTRUCTURE_REDIRECT,
                        event,
                    )
                    .map(|cookie| cookie.check()),
                "failed to send the X11 client message",
            )
        }

        // -- Clipboard paste ------------------------------------------------

        /// Own the CLIPBOARD selection with `text` and serve selection
        /// requests while the paste completes. Ownership is kept for the
        /// process lifetime (the X11 clipboard is not restored afterwards —
        /// documented divergence from the macOS helper).
        fn paste_text(
            &self,
            text: &str,
            pre_delay_ms: u64,
            post_delay_ms: u64,
        ) -> Result<(), ToolError> {
            {
                let mut clipboard = match self.clipboard_text.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *clipboard = Some(text.to_string());
            }
            flatten_reply(
                self.conn
                    .set_selection_owner(self.clipboard_window, self.atoms.clipboard, CURRENT_TIME)
                    .map(|cookie| cookie.check()),
                "failed to acquire the CLIPBOARD selection",
            )?;
            let owner = flatten_reply(
                self.conn
                    .get_selection_owner(self.atoms.clipboard)
                    .map(|cookie| cookie.reply()),
                "failed to verify CLIPBOARD ownership",
            )?;
            if owner.owner != self.clipboard_window {
                return Err(respond_to_model(
                    "failed to acquire the CLIPBOARD selection: another selection owner kept it",
                ));
            }

            // Give the target a moment, send Ctrl+V, then serve selection
            // requests for the post-delay window.
            self.serve_selection_requests(Duration::from_millis(pre_delay_ms))?;
            self.press_keycode_with_modifiers_for_control_v()?;
            map_x11(self.conn.flush(), "failed to flush the X11 connection")?;
            self.serve_selection_requests(Duration::from_millis(post_delay_ms))?;
            Ok(())
        }

        fn press_keycode_with_modifiers_for_control_v(&self) -> Result<(), ToolError> {
            let Some((keycode, _)) = self.keycode_for_keysym(0x076) else {
                return Err(respond_to_model(
                    "the X11 keymap has no key for `v`; cannot paste via Ctrl+V",
                ));
            };
            self.press_keycode_with_modifiers(keycode, false, &[XModifier::Control])
        }

        /// Drain and answer selection requests for `duration`. Note:
        /// `poll_for_event` does not flush the write buffer, so callers must
        /// flush after sending the paste keystroke.
        fn serve_selection_requests(&self, duration: Duration) -> Result<(), ToolError> {
            let deadline = Instant::now() + duration;
            loop {
                while let Some(event) = map_x11(
                    self.conn.poll_for_event(),
                    "failed to poll the X11 event queue",
                )? {
                    if let Event::SelectionRequest(request) = event {
                        self.answer_selection_request(&request);
                    }
                }
                if Instant::now() >= deadline {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn answer_selection_request(&self, request: &xproto::SelectionRequestEvent) {
            let clipboard = match self.clipboard_text.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(text) = clipboard.as_ref() else {
                self.refuse_selection(request);
                return;
            };
            if request.selection != self.atoms.clipboard {
                self.refuse_selection(request);
                return;
            }
            let property = if request.property == 0 {
                request.target
            } else {
                request.property
            };
            let result: Result<(), String> = if request.target == self.atoms.targets {
                // TARGETS: list the types we can serve (format 32 atom array).
                let atoms = [self.atoms.targets, self.atoms.utf8_string, X_ATOM_STRING];
                self.conn
                    .change_property(
                        xproto::PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::ATOM,
                        32,
                        atoms.len() as u32,
                        &property32_bytes(&atoms, self.lsb_first),
                    )
                    .map(|cookie| cookie.check())
                    .map_err(|error| error.to_string())
                    .and_then(|checked| checked.map_err(|error| error.to_string()))
            } else if request.target == self.atoms.utf8_string {
                self.conn
                    .change_property(
                        xproto::PropMode::REPLACE,
                        request.requestor,
                        property,
                        self.atoms.utf8_string,
                        8,
                        text.len() as u32,
                        text.as_bytes(),
                    )
                    .map(|cookie| cookie.check())
                    .map_err(|error| error.to_string())
                    .and_then(|checked| checked.map_err(|error| error.to_string()))
            } else if request.target == X_ATOM_STRING {
                // STRING (ISO Latin-1): best-effort lossy conversion.
                let latin1: Vec<u8> = text
                    .chars()
                    .map(|character| u8::try_from(u32::from(character)).unwrap_or(b'?'))
                    .collect();
                let len = latin1.len() as u32;
                self.conn
                    .change_property(
                        xproto::PropMode::REPLACE,
                        request.requestor,
                        property,
                        X_ATOM_STRING,
                        8,
                        len,
                        &latin1,
                    )
                    .map(|cookie| cookie.check())
                    .map_err(|error| error.to_string())
                    .and_then(|checked| checked.map_err(|error| error.to_string()))
            } else {
                Err("unsupported selection target".to_string())
            };
            let property = if result.is_ok() { property } else { 0 };
            self.send_selection_notify(request, property);
        }

        fn refuse_selection(&self, request: &xproto::SelectionRequestEvent) {
            self.send_selection_notify(request, 0);
        }

        fn send_selection_notify(
            &self,
            request: &xproto::SelectionRequestEvent,
            property: xproto::Atom,
        ) {
            let notify = xproto::SelectionNotifyEvent {
                response_type: xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: request.time,
                requestor: request.requestor,
                selection: request.selection,
                target: request.target,
                property,
            };
            let _ = flatten_reply(
                self.conn
                    .send_event(
                        false,
                        request.requestor,
                        xproto::EventMask::NO_EVENT,
                        notify,
                    )
                    .map(|cookie| cookie.check()),
                "failed to send the X11 selection notify",
            );
        }

        // -- Screenshots ------------------------------------------------------

        /// ZPixmap GetImage decoded to RGBA. Supports the depth-24/32
        /// (4 bytes per pixel) layout every modern server uses.
        fn get_image_rgba(
            &self,
            drawable: xproto::Drawable,
            x: i16,
            y: i16,
            width: u16,
            height: u16,
        ) -> Result<(u32, u32, Vec<u8>), ToolError> {
            let reply = flatten_reply(
                self.conn
                    .get_image(
                        x11rb::protocol::xproto::ImageFormat::Z_PIXMAP,
                        drawable,
                        x,
                        y,
                        width,
                        height,
                        u32::MAX,
                    )
                    .map(|cookie| cookie.reply()),
                "X11 GetImage capture failed",
            )?;
            if !matches!(reply.depth, 24 | 32) {
                return Err(respond_to_model(format!(
                    "X11 GetImage capture failed: unsupported visual depth {} (expected 24 or 32)",
                    reply.depth
                )));
            }
            let width = u32::from(width);
            let height = u32::from(height);
            let expected = usize::try_from(width)
                .and_then(|width| usize::try_from(height).map(|height| width * height * 4))
                .map_err(|_| {
                    respond_to_model("X11 GetImage capture failed: region is too large")
                })?;
            if reply.data.len() < expected {
                return Err(respond_to_model(format!(
                    "X11 GetImage capture failed: expected {expected} bytes, got {}",
                    reply.data.len()
                )));
            }
            let mut rgba = vec![0u8; expected];
            for (index, rgba_pixel) in rgba.chunks_exact_mut(4).enumerate() {
                let source = &reply.data[index * 4..index * 4 + 4];
                // ZPixmap 24/32bpp: LSB-first servers store B,G,R,(A|pad);
                // MSB-first servers store (A|pad),R,G,B.
                let (r, g, b) = if self.lsb_first {
                    (source[2], source[1], source[0])
                } else {
                    (source[1], source[2], source[3])
                };
                rgba_pixel[0] = r;
                rgba_pixel[1] = g;
                rgba_pixel[2] = b;
                rgba_pixel[3] = 255;
            }
            Ok((width, height, rgba))
        }

        fn encode_png(
            width: u32,
            height: u32,
            rgba: Vec<u8>,
            target_width: u32,
            target_height: u32,
        ) -> Result<Vec<u8>, ToolError> {
            let image = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
                respond_to_model("failed to decode the X11 capture into an image buffer")
            })?;
            let resized = if (target_width > 0 && target_width != width)
                || (target_height > 0 && target_height != height)
            {
                let target_width = if target_width > 0 {
                    target_width
                } else {
                    width
                };
                let target_height = if target_height > 0 {
                    target_height
                } else {
                    height
                };
                image::imageops::resize(
                    &image,
                    target_width,
                    target_height,
                    image::imageops::FilterType::Triangle,
                )
            } else {
                image
            };
            let mut png = Vec::new();
            resized
                .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|error| {
                    respond_to_model(format!("failed to encode the X11 capture as PNG: {error}"))
                })?;
            Ok(png)
        }

        fn capture_bounds_pixels(
            &self,
            bounds: &HelperRect,
        ) -> Result<(i16, i16, u16, u16), ToolError> {
            if !bounds.x.is_finite()
                || !bounds.y.is_finite()
                || !bounds.width.is_finite()
                || !bounds.height.is_finite()
            {
                return Err(respond_to_model(
                    "capture region contains non-finite coordinates",
                ));
            }
            let x = clamp_i16(bounds.x.round()).max(0);
            let y = clamp_i16(bounds.y.round()).max(0);
            let right = clamp_i16(bounds.x.round() + bounds.width.round())
                .min(i16::try_from(self.root_width).unwrap_or(i16::MAX));
            let bottom = clamp_i16(bounds.y.round() + bounds.height.round())
                .min(i16::try_from(self.root_height).unwrap_or(i16::MAX));
            let width = right - x;
            let height = bottom - y;
            if width <= 0 || height <= 0 {
                return Err(respond_to_model(
                    "capture region is empty after clamping to the X11 screen bounds",
                ));
            }
            Ok((
                x,
                y,
                u16::try_from(width).unwrap_or(u16::MAX),
                u16::try_from(height).unwrap_or(u16::MAX),
            ))
        }
    }

    fn clamp_i16(value: f64) -> i16 {
        value.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
    }

    fn normalized(value: &str) -> String {
        value.trim().to_lowercase()
    }

    // -- Client model -----------------------------------------------------

    #[derive(Clone)]
    struct XClient {
        window: xproto::Window,
        stacking_index: usize,
        class_instance: Option<String>,
        class_name: Option<String>,
        title: Option<String>,
        pid: Option<i32>,
        comm: Option<String>,
        frame_rect: RectI,
        regular_type: bool,
        state_normal: bool,
    }

    impl XClient {
        fn matches_app(&self, requested: &str) -> bool {
            app_matches_window(
                requested,
                self.class_instance.as_deref(),
                self.class_name.as_deref(),
                self.title.as_deref(),
                self.comm.as_deref(),
            )
        }

        /// Identity used to group an app's windows: prefer WM_CLASS class,
        /// then instance, then comm (the macOS localizedName analog).
        fn identity_key(&self) -> String {
            self.class_name
                .clone()
                .or_else(|| self.class_instance.clone())
                .or_else(|| self.comm.clone())
                .unwrap_or_else(|| format!("window-{}", self.window))
        }

        fn app_display_name(&self) -> String {
            self.identity_key()
        }
    }

    // -- Escape monitor -----------------------------------------------------

    struct ActiveEscapeMonitor {
        triggered: Arc<AtomicBool>,
        #[allow(dead_code)]
        handle: Option<JoinHandle<()>>,
    }

    static ACTIVE_ESCAPE_MONITOR: OnceLock<Mutex<Option<ActiveEscapeMonitor>>> = OnceLock::new();
    static UNHIDDEN_WINDOWS: OnceLock<Mutex<HashMap<i32, Vec<xproto::Window>>>> = OnceLock::new();

    /// Signal the active escape monitor to shut down (`triggered` makes its
    /// loop exit; the thread is detached and joined by nothing).
    fn signal_active_escape_monitor() {
        let cell = ACTIVE_ESCAPE_MONITOR.get_or_init(|| Mutex::new(None));
        let mut guard = match cell.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = guard.take() {
            previous.triggered.store(true, Ordering::SeqCst);
        }
    }

    /// Dedicated-connection escape monitor. RECORD streams key events
    /// passively when the extension is available; otherwise an
    /// XQueryKeymap polling loop is used. Any setup failure maps to
    /// `Ok(None)` at the call site (graceful degradation, never blocking).
    struct EscapeMonitor {
        triggered: Arc<AtomicBool>,
        suppress_next: Arc<AtomicBool>,
    }

    impl EscapeMonitor {
        fn spawn(&self) -> Result<(), ToolError> {
            let (control, _) = map_x11(
                RustConnection::connect(None),
                "emergency stop monitor failed to connect to the X11 display",
            )?;
            let escape_keycode = escape_keycode_on(&control)?;

            // Try the passive RECORD-based monitor first; fall back to
            // XQueryKeymap polling (also passive) when RECORD is missing.
            let record_supported = control
                .extension_information(record::X11_EXTENSION_NAME)
                .map(|info| info.is_some())
                .unwrap_or(false);
            let control = Arc::new(control);
            if record_supported {
                self.spawn_record(Arc::clone(&control), escape_keycode)
            } else {
                self.spawn_keymap_poll(Arc::clone(&control), escape_keycode)
            }
        }

        fn spawn_record(
            &self,
            control: Arc<RustConnection>,
            escape_keycode: u8,
        ) -> Result<(), ToolError> {
            let context = control.generate_id().map_err(|error| {
                respond_to_model(format!(
                    "emergency stop monitor failed to allocate a context id: {error}"
                ))
            })?;
            let empty = record::Range8 { first: 0, last: 0 };
            let empty_ext = record::ExtRange {
                major: empty,
                minor: record::Range16 { first: 0, last: 0 },
            };
            let range = record::Range {
                core_requests: empty,
                core_replies: empty,
                ext_requests: empty_ext,
                ext_replies: empty_ext,
                delivered_events: empty,
                device_events: record::Range8 {
                    first: xproto::KEY_PRESS_EVENT,
                    last: xproto::KEY_PRESS_EVENT,
                },
                errors: empty,
                client_started: false,
                client_died: false,
            };
            flatten_reply(
                control
                    .record_create_context(context, 0, &[record::CS::ALL_CLIENTS.into()], &[range])
                    .map(|cookie| cookie.check()),
                "emergency stop monitor failed to create a RECORD context",
            )?;

            // The data connection streams events; a watchdog disables the
            // context (from the control connection) when `triggered` is set
            // so the blocking iterator ends.
            let (data, _) = map_x11(
                RustConnection::connect(None),
                "emergency stop monitor failed to open a RECORD data connection",
            )?;
            let triggered = Arc::clone(&self.triggered);
            let watchdog_control = Arc::clone(&control);
            let watchdog = std::thread::spawn(move || {
                loop {
                    if triggered.load(Ordering::SeqCst) {
                        let _ = watchdog_control.record_disable_context(context);
                        let _ = watchdog_control.record_free_context(context);
                        let _ = watchdog_control.sync();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });

            let triggered = Arc::clone(&self.triggered);
            let suppress_next = Arc::clone(&self.suppress_next);
            let handle = std::thread::spawn(move || {
                if let Ok(stream) = data.record_enable_context(context) {
                    for reply in stream {
                        let Ok(reply) = reply else { break };
                        if reply.category != 0 {
                            // 0 = FromServer; other categories carry no events.
                            continue;
                        }
                        let mut chunk = reply.data.as_slice();
                        while !chunk.is_empty() {
                            if is_escape_key_press(chunk, escape_keycode) {
                                if suppress_next
                                    .compare_exchange(
                                        true,
                                        false,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                                {
                                    // Expected programmatic Escape; keep monitoring.
                                } else {
                                    triggered.store(true, Ordering::SeqCst);
                                }
                                break;
                            }
                            // All device events in the stream are 32 bytes.
                            if chunk.len() < 32 {
                                break;
                            }
                            chunk = &chunk[32..];
                        }
                        if triggered.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
                let _ = control.record_disable_context(context);
                let _ = control.record_free_context(context);
                let _ = control.sync();
            });
            drop(watchdog); // detached; it exits once the context is disabled

            register_escape_monitor(self, handle);
            Ok(())
        }

        fn spawn_keymap_poll(
            &self,
            control: Arc<RustConnection>,
            escape_keycode: u8,
        ) -> Result<(), ToolError> {
            let triggered = Arc::clone(&self.triggered);
            let suppress_next = Arc::clone(&self.suppress_next);
            let handle = std::thread::spawn(move || {
                let byte_index = usize::from(escape_keycode / 8);
                let bit = escape_keycode % 8;
                let mut was_pressed = false;
                // When the suppression flag was first observed armed. If the
                // injected Escape's ~20ms hold falls between two samples the
                // flag is never consumed; gating consumption on this window
                // keeps that miss from silently disarming the next
                // user-initiated Escape.
                let mut armed_since: Option<Instant> = None;
                loop {
                    if triggered.load(Ordering::SeqCst) {
                        break;
                    }
                    let pressed = control
                        .query_keymap()
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .and_then(|reply| reply.keys.get(byte_index).copied())
                        .is_some_and(|byte| byte & (1 << bit) != 0);
                    let armed = suppress_next.load(Ordering::SeqCst);
                    if armed {
                        armed_since.get_or_insert_with(Instant::now);
                    } else {
                        armed_since = None;
                    }
                    if pressed && !was_pressed {
                        let armed_for = armed_since.map(|since| since.elapsed());
                        let expected = keymap_press_is_expected(armed, armed_for)
                            && suppress_next
                                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok();
                        if !expected {
                            triggered.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    was_pressed = pressed;
                    std::thread::sleep(Duration::from_millis(KEYMAP_POLL_MILLIS));
                }
            });
            register_escape_monitor(self, handle);
            Ok(())
        }
    }

    fn register_escape_monitor(monitor: &EscapeMonitor, handle: JoinHandle<()>) {
        signal_active_escape_monitor();
        let cell = ACTIVE_ESCAPE_MONITOR.get_or_init(|| Mutex::new(None));
        let mut guard = match cell.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(ActiveEscapeMonitor {
            triggered: Arc::clone(&monitor.triggered),
            handle: Some(handle),
        });
    }

    fn escape_keycode_on(conn: &RustConnection) -> Result<u8, ToolError> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        if max < min {
            return Err(respond_to_model(
                "X11 keyboard mapping reported an empty key range",
            ));
        }
        let count = (max - min).saturating_add(1);
        let reply = flatten_reply(
            conn.get_keyboard_mapping(min, count)
                .map(|cookie| cookie.reply()),
            "emergency stop monitor failed to read the keyboard mapping",
        )?;
        let map = build_keysym_map(min, reply.keysyms_per_keycode, &reply.keysyms);
        map.get(&XK_ESCAPE)
            .map(|(keycode, _)| *keycode)
            .ok_or_else(|| {
                respond_to_model(
                    "the X11 keymap has no Escape key; emergency stop monitoring is unavailable",
                )
            })
    }

    // -- Platform impl ------------------------------------------------------

    impl GuiPlatform for LinuxGuiPlatform {
        fn readiness_snapshot(&self) -> GuiEnvironmentReadinessSnapshot {
            let environment = classify_display_environment(
                std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
                std::env::var("DISPLAY").ok().as_deref(),
            );

            if matches!(
                environment,
                DisplayEnvironment::WaylandOnly | DisplayEnvironment::Headless
            ) {
                let summary = match environment {
                    DisplayEnvironment::WaylandOnly => WAYLAND_UNSUPPORTED_MESSAGE,
                    _ => HEADLESS_MESSAGE,
                };
                let checks = vec![GuiEnvironmentReadinessCheck {
                    id: "platform",
                    label: "Platform",
                    status: GuiReadinessStatus::Unsupported,
                    summary: summary.to_string(),
                    detail: None,
                }];
                let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
                return GuiEnvironmentReadinessSnapshot {
                    status: readiness_status_ladder(&statuses),
                    checks,
                };
            }

            match X11Session::shared() {
                Err(error) => {
                    let checks = vec![
                        GuiEnvironmentReadinessCheck {
                            id: "platform",
                            label: "Platform",
                            status: GuiReadinessStatus::Error,
                            summary:
                                "X11 server is not reachable, so native GUI tools are unavailable."
                                    .to_string(),
                            detail: Some(error.to_string()),
                        },
                        GuiEnvironmentReadinessCheck {
                            id: "native_helper",
                            label: "Native GUI Helper",
                            status: GuiReadinessStatus::Error,
                            summary:
                                "In-process X11 backend could not connect to the display server."
                                    .to_string(),
                            detail: None,
                        },
                    ];
                    let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
                    GuiEnvironmentReadinessSnapshot {
                        status: readiness_status_ladder(&statuses),
                        checks,
                    }
                }
                Ok(session) => {
                    let mut checks = vec![GuiEnvironmentReadinessCheck {
                        id: "platform",
                        label: "Platform",
                        status: GuiReadinessStatus::Ok,
                        summary: "Linux X11 GUI runtime is available on this host.".to_string(),
                        detail: Some(
                            std::env::var("DISPLAY")
                                .map(|display| {
                                    format!("X11 display `{display}` (x11rb in-process).")
                                })
                                .unwrap_or_else(|_| "X11 display (x11rb in-process).".to_string()),
                        ),
                    }];
                    if environment == DisplayEnvironment::XWayland {
                        checks.push(GuiEnvironmentReadinessCheck {
                            id: "xwayland",
                            label: "XWayland",
                            status: GuiReadinessStatus::Warn,
                            summary: XWAYLAND_CHECK_SUMMARY.to_string(),
                            detail: Some(XWAYLAND_CHECK_DETAIL.to_string()),
                        });
                    }
                    checks.push(if session.xtest_available {
                        GuiEnvironmentReadinessCheck {
                            id: "accessibility",
                            label: "Accessibility",
                            status: GuiReadinessStatus::Ok,
                            summary: "XTEST input injection is available for native GUI input."
                                .to_string(),
                            detail: None,
                        }
                    } else {
                        GuiEnvironmentReadinessCheck {
                            id: "accessibility",
                            label: "Accessibility",
                            status: GuiReadinessStatus::Error,
                            summary:
                                "The X server does not support the XTEST extension; native GUI input is unavailable."
                                    .to_string(),
                            detail: None,
                        }
                    });
                    checks.push(if session.capture_available {
                        GuiEnvironmentReadinessCheck {
                            id: "screen_recording",
                            label: "Screen Recording",
                            status: GuiReadinessStatus::Ok,
                            summary: "Screen capture is available via X11 GetImage.".to_string(),
                            detail: None,
                        }
                    } else {
                        GuiEnvironmentReadinessCheck {
                            id: "screen_recording",
                            label: "Screen Recording",
                            status: GuiReadinessStatus::Error,
                            summary:
                                "X11 GetImage capture failed; screenshot-based GUI actions are unavailable."
                                    .to_string(),
                            detail: None,
                        }
                    });
                    checks.push(GuiEnvironmentReadinessCheck {
                        id: "native_helper",
                        label: "Native GUI Helper",
                        status: GuiReadinessStatus::Ok,
                        summary: "In-process X11 backend - no helper binary required.".to_string(),
                        detail: Some(
                            "x11rb RustConnection, XTEST input, RANDR geometry, GetImage capture."
                                .to_string(),
                        ),
                    });
                    let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
                    GuiEnvironmentReadinessSnapshot {
                        status: readiness_status_ladder(&statuses),
                        checks,
                    }
                }
            }
        }

        fn tool_capabilities(&self) -> HashMap<&'static str, GuiToolCapability> {
            let mut capabilities: HashMap<&'static str, GuiToolCapability> = GUI_TOOL_NAMES
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
                .collect();

            let environment = classify_display_environment(
                std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
                std::env::var("DISPLAY").ok().as_deref(),
            );
            if matches!(
                environment,
                DisplayEnvironment::WaylandOnly | DisplayEnvironment::Headless
            ) {
                // Readiness reports Unsupported and the engine attaches
                // GUI_UNSUPPORTED_MESSAGE; keep defaults here.
                return capabilities;
            }

            let Ok(session) = X11Session::shared() else {
                for tool_name in GUI_TOOL_NAMES {
                    capabilities.insert(
                        tool_name,
                        GuiToolCapability {
                            enabled: false,
                            reason: Some(
                                "The X11 display server is not reachable, so native GUI tools cannot run. Verify the session is running (`xdpyinfo` works) and DISPLAY is set."
                                    .to_string(),
                            ),
                            targetless_only: false,
                        },
                    );
                }
                return capabilities;
            };

            if !session.xtest_available {
                for tool_name in [
                    "gui_click",
                    "gui_drag",
                    "gui_scroll",
                    "gui_type",
                    "gui_key",
                    "gui_move",
                ] {
                    capabilities.insert(
                        tool_name,
                        GuiToolCapability {
                            enabled: false,
                            reason: Some(
                                "The X server does not support the XTEST extension, so GUI input actions (click, type, scroll, drag, etc.) are unavailable. gui_observe may still work."
                                    .to_string(),
                            ),
                            targetless_only: false,
                        },
                    );
                }
            }
            if !session.capture_available {
                for tool_name in ["gui_observe", "gui_click", "gui_drag", "gui_wait"] {
                    if capabilities[tool_name].enabled {
                        capabilities.insert(
                            tool_name,
                            GuiToolCapability {
                                enabled: false,
                                reason: Some(
                                    "X11 GetImage capture is unavailable, so screenshot-based GUI actions are unavailable."
                                        .to_string(),
                                ),
                                targetless_only: false,
                            },
                        );
                    }
                }
                for tool_name in ["gui_scroll", "gui_type"] {
                    if capabilities[tool_name].enabled && !capabilities[tool_name].targetless_only {
                        let mut capability = capabilities[tool_name].clone();
                        capability.reason = Some(
                            "X11 GetImage capture is unavailable, so this tool only supports targetless usage (omit the `target` parameter)."
                                .to_string(),
                        );
                        capability.targetless_only = true;
                        capabilities.insert(tool_name, capability);
                    }
                }
            }
            capabilities
        }

        fn resolve_helper_binary(&self) -> Result<std::path::PathBuf, ToolError> {
            Err(respond_to_model(
                "The Linux GUI backend is in-process (x11rb); no helper binary is used.",
            ))
        }

        fn cleanup_input_state(&self) -> Result<(), ToolError> {
            // Called from GuiActionSession::drop — best-effort only. The
            // escape monitor's stop condition is the `triggered` flag (the
            // thread polls it; `stop()` cannot signal a bare JoinHandle), so
            // signal it here at session teardown.
            signal_active_escape_monitor();
            let Ok(session) = X11Session::shared() else {
                return Ok(());
            };
            for button in [1u8, 2, 3] {
                let _ = session.fake_button(button, false);
            }
            if let Ok(table) = session.modifier_keycode_table() {
                let keys_per_mod = session.keys_per_modifier();
                for slot in 0..8usize {
                    if let Some(keycode) = modifier_keycode_from_reply(table, keys_per_mod, slot) {
                        let _ = session.fake_key(keycode, false);
                    }
                }
            }
            let _ = session.conn.sync();
            Ok(())
        }

        fn hide_other_apps(&self, app: Option<&str>) -> Result<Vec<i32>, ToolError> {
            let session = X11Session::shared()?;
            let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
            let clients = session.visible_regular_clients()?;

            // Host terminal: the owner of _NET_ACTIVE_WINDOW plus this
            // process itself.
            let active = session.active_window();
            let own_pid = i32::try_from(std::process::id()).unwrap_or(0);
            let keep_identity = active.and_then(|window| {
                clients
                    .iter()
                    .find(|client| client.window == window)
                    .map(XClient::identity_key)
            });

            let mut hidden_pids: Vec<i32> = Vec::new();
            for client in &clients {
                // Only clients with a PID can be restored later.
                let Some(pid) = client.pid else { continue };
                if pid == 0 || pid == own_pid {
                    continue;
                }
                if Some(client.window) == active {
                    continue;
                }
                if let Some(identity) = &keep_identity
                    && client.identity_key() == *identity
                {
                    continue;
                }
                if let Some(requested_app) = requested_app
                    && client.matches_app(requested_app)
                {
                    continue;
                }
                let state = wm_change_state_message(WM_STATE_ICONIC);
                if session
                    .send_root_message(client.window, session.atoms.wm_change_state, state)
                    .is_ok()
                {
                    if !hidden_pids.contains(&pid) {
                        hidden_pids.push(pid);
                    }
                    let registry = UNHIDDEN_WINDOWS.get_or_init(|| Mutex::new(HashMap::new()));
                    let mut guard = match registry.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.entry(pid).or_default().push(client.window);
                }
            }
            let _ = session.conn.sync();
            Ok(hidden_pids)
        }

        fn unhide_apps(&self, pids: &[i32]) -> Result<(), ToolError> {
            if pids.is_empty() {
                return Ok(());
            }
            let session = X11Session::shared()?;
            let mut windows_by_pid: HashMap<i32, Vec<xproto::Window>> = HashMap::new();
            if let Some(registry) = UNHIDDEN_WINDOWS.get() {
                let mut guard = match registry.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                for pid in pids {
                    if let Some(windows) = guard.remove(pid) {
                        windows_by_pid.insert(*pid, windows);
                    }
                }
            }
            // Fallback: re-enumerate managed clients by _NET_WM_PID (hidden
            // clients are iconified, so the unfiltered enumeration is used).
            if let Ok(clients) = session.enumerate_clients() {
                for client in clients {
                    if let Some(pid) = client.pid
                        && pids.contains(&pid)
                        && !windows_by_pid
                            .entry(pid)
                            .or_default()
                            .contains(&client.window)
                    {
                        windows_by_pid.entry(pid).or_default().push(client.window);
                    }
                }
            }
            for (_, windows) in windows_by_pid {
                for window in windows {
                    let _ = flatten_reply(
                        session.conn.map_window(window).map(|cookie| cookie.check()),
                        "failed to map the X11 window",
                    );
                    let state = wm_change_state_message(WM_STATE_NORMAL);
                    let _ = session.send_root_message(window, session.atoms.wm_change_state, state);
                }
            }
            let _ = session.conn.sync();
            Ok(())
        }

        fn start_emergency_stop_monitor(
            &self,
        ) -> Result<Option<GuiEmergencyStopMonitor>, ToolError> {
            let monitor = EscapeMonitor {
                triggered: Arc::new(AtomicBool::new(false)),
                suppress_next: Arc::new(AtomicBool::new(false)),
            };
            // Graceful degradation: any X/RECORD/keymap failure means no
            // emergency stop monitoring this session, never a tool error.
            if monitor.spawn().is_err() {
                return Ok(None);
            }
            // The monitor loop self-terminates when `triggered` flips (set
            // by the Escape detection, by cleanup_input_state at teardown,
            // or by the next monitor replacing this one); `stop()` is
            // therefore given no handle to join and can never block.
            Ok(Some(GuiEmergencyStopMonitor::from_thread(
                monitor.triggered,
                monitor.suppress_next,
                None,
            )))
        }

        fn capture_context(
            &self,
            app: Option<&str>,
            activate_app: bool,
            window_selection: Option<&WindowSelector>,
        ) -> Result<HelperCaptureContext, ToolError> {
            let session = X11Session::shared()?;
            let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
            if activate_app && let Some(requested_app) = requested_app {
                session.activate_app(requested_app)?;
            }

            let (app_name, selection) = session.resolve_target(requested_app, window_selection)?;
            let cursor = session.cursor_position()?;
            let anchor = selection
                .as_ref()
                .map(|selection| {
                    (
                        selection.bounds.x as f64 + selection.bounds.width as f64 / 2.0,
                        selection.bounds.y as f64 + selection.bounds.height as f64 / 2.0,
                    )
                })
                .unwrap_or(cursor);
            let display = display_for_point(anchor, &session.displays()).ok_or_else(|| {
                respond_to_model("failed to resolve the X11 display for the capture anchor")
            })?;

            let frontmost = session.active_window().and_then(|window| {
                session
                    .visible_regular_clients()
                    .ok()?
                    .into_iter()
                    .find(|client| client.window == window)
                    .map(|client| client.app_display_name())
            });

            Ok(HelperCaptureContext {
                app_name,
                cursor: HelperPoint {
                    x: cursor.0,
                    y: cursor.1,
                },
                display: HelperDisplayDescriptor {
                    index: display.index,
                    bounds: HelperRect {
                        x: display.bounds.x as f64,
                        y: display.bounds.y as f64,
                        width: display.bounds.width as f64,
                        height: display.bounds.height as f64,
                    },
                },
                window_id: selection.as_ref().map(|selection| selection.primary.id),
                window_title: selection
                    .as_ref()
                    .and_then(|selection| selection.primary.title.clone()),
                window_bounds: selection.as_ref().map(|selection| HelperRect {
                    // Frame (window manager) bounds, global coordinates.
                    x: selection.bounds.x as f64,
                    y: selection.bounds.y as f64,
                    width: selection.bounds.width as f64,
                    height: selection.bounds.height as f64,
                }),
                window_count: selection.as_ref().map(|selection| selection.window_count),
                window_capture_strategy: selection
                    .as_ref()
                    .map(|selection| selection.strategy.to_string()),
                host_self_exclude_applied: Some(false),
                host_frontmost_excluded: Some(false),
                host_frontmost_app_name: frontmost,
                host_frontmost_bundle_id: None,
            })
        }

        fn capture_region(
            &self,
            bounds: &HelperRect,
            target_width: u32,
            target_height: u32,
        ) -> Result<Vec<u8>, ToolError> {
            let session = X11Session::shared()?;
            let (x, y, width, height) = session.capture_bounds_pixels(bounds)?;
            let (image_width, image_height, rgba) =
                session.get_image_rgba(session.root, x, y, width, height)?;
            X11Session::encode_png(image_width, image_height, rgba, target_width, target_height)
        }

        fn observe(
            &self,
            app: Option<&str>,
            activate_app: bool,
            capture_mode: Option<&str>,
            window_selection: Option<&WindowSelector>,
            prefer_window_when_available: bool,
        ) -> Result<PlatformObservation, ToolError> {
            let session = X11Session::shared()?;
            let mode = capture_mode.and_then(normalize_capture_mode_env);
            let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
            if activate_app && let Some(requested_app) = requested_app {
                session.activate_app(requested_app)?;
            }

            let window_selection_requested = window_selection.is_some_and(|selection| {
                selection
                    .title
                    .as_deref()
                    .is_some_and(|title| !title.trim().is_empty())
                    || selection
                        .title_contains
                        .as_deref()
                        .is_some_and(|title| !title.trim().is_empty())
                    || selection.index.is_some()
            });

            let (app_name, selection) = session.resolve_target(requested_app, window_selection)?;
            if window_selection_requested && selection.is_none() {
                return Err(respond_to_model(
                    "requested window could not be found; check `window_title`/`window_selector` or switch to `capture_mode: \"display\"`",
                ));
            }

            let use_window = match mode {
                Some("window") => {
                    if selection.is_none() {
                        return Err(respond_to_model(
                            "window capture was requested but no matching X11 window is available; check `window_title`/`window_selector` or use `capture_mode: \"display\"`",
                        ));
                    }
                    true
                }
                Some(_) => false,
                None => {
                    window_selection_requested
                        || (prefer_window_when_available && selection.is_some())
                }
            };

            let displays = session.displays();
            // Like the macOS helper (a screen-region capture), both modes
            // grab a composited root region so the capture origin is always
            // the region's global top-left.
            let (capture_bounds, display_index) = if use_window {
                let selection = selection.as_ref().ok_or_else(|| {
                    respond_to_model(
                        "window capture requested but no matching X11 window was available",
                    )
                })?;
                let anchor = (
                    selection.bounds.x as f64 + selection.bounds.width as f64 / 2.0,
                    selection.bounds.y as f64 + selection.bounds.height as f64 / 2.0,
                );
                let display = display_for_point(anchor, &displays).ok_or_else(|| {
                    respond_to_model("failed to resolve the X11 display for the captured window")
                })?;
                (
                    HelperRect {
                        x: selection.bounds.x as f64,
                        y: selection.bounds.y as f64,
                        width: selection.bounds.width as f64,
                        height: selection.bounds.height as f64,
                    },
                    display.index,
                )
            } else {
                let cursor = session.cursor_position()?;
                let anchor = selection
                    .as_ref()
                    .map(|selection| {
                        (
                            selection.bounds.x as f64 + selection.bounds.width as f64 / 2.0,
                            selection.bounds.y as f64 + selection.bounds.height as f64 / 2.0,
                        )
                    })
                    .unwrap_or(cursor);
                let display = display_for_point(anchor, &displays).ok_or_else(|| {
                    respond_to_model("failed to resolve the X11 display for capture")
                })?;
                (
                    HelperRect {
                        x: display.bounds.x as f64,
                        y: display.bounds.y as f64,
                        width: display.bounds.width as f64,
                        height: display.bounds.height as f64,
                    },
                    display.index,
                )
            };

            let (x, y, width, height) = session.capture_bounds_pixels(&capture_bounds)?;
            let (image_width, image_height, rgba) =
                session.get_image_rgba(session.root, x, y, width, height)?;
            let image_bytes =
                X11Session::encode_png(image_width, image_height, rgba, image_width, image_height)?;

            let state = ObserveState {
                capture: CaptureArtifact {
                    origin_x: f64::from(x),
                    origin_y: f64::from(y),
                    // X11 pixels == logical points (scale 1.0); image dims
                    // equal the reported capture dims after clamping, which
                    // keeps grounded-click math exact.
                    width: image_width,
                    height: image_height,
                    image_width,
                    image_height,
                    display_index,
                    capture_mode: if use_window {
                        CaptureMode::Window
                    } else {
                        CaptureMode::Display
                    },
                    window_title: if use_window {
                        selection
                            .as_ref()
                            .and_then(|selection| selection.primary.title.clone())
                    } else {
                        None
                    },
                    window_count: if use_window {
                        selection.as_ref().map(|selection| selection.window_count)
                    } else {
                        None
                    },
                    window_capture_strategy: if use_window {
                        selection
                            .as_ref()
                            .map(|selection| selection.strategy.to_string())
                    } else {
                        None
                    },
                    host_exclusion: HostCaptureExclusionState {
                        applied: false,
                        frontmost_excluded: false,
                        adjusted: false,
                        frontmost_app_name: session
                            .active_window()
                            .and_then(|window| {
                                session
                                    .visible_regular_clients()
                                    .ok()?
                                    .into_iter()
                                    .find(|client| client.window == window)
                            })
                            .map(|client| client.app_display_name()),
                        frontmost_bundle_id: None,
                        redaction_count: 0,
                    },
                },
                app_name,
            };
            Ok(PlatformObservation { state, image_bytes })
        }

        fn run_event(
            &self,
            event_mode: &str,
            app: Option<&str>,
            float_env: &[(&str, f64)],
            string_env: &[(&str, String)],
        ) -> Result<(), ToolError> {
            if !EVENT_MODES.contains(&event_mode) {
                return Err(respond_to_model(format!(
                    "unsupported GUI event mode `{event_mode}`; expected one of {}",
                    EVENT_MODES.join(", ")
                )));
            }
            let session = X11Session::shared()?;
            if let Some(requested_app) = app.map(str::trim).filter(|app| !app.is_empty()) {
                session.activate_app(requested_app)?;
            }

            match event_mode {
                "move_cursor" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    let settle_ms = optional_int(string_env, "CODEX_GUI_SETTLE_MS")?
                        .unwrap_or(DEFAULT_MOVE_SETTLE_MS)
                        .max(1);
                    session.move_cursor(x, y)?;
                    std::thread::sleep(Duration::from_millis(settle_ms.max(0) as u64));
                }
                "click" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    session.move_cursor(x, y)?;
                    session.fake_button(1, true)?;
                    std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
                    session.fake_button(1, false)?;
                }
                "click_and_hold" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    let hold_ms = optional_int(string_env, "CODEX_GUI_HOLD_MS")?
                        .unwrap_or(DEFAULT_CLICK_AND_HOLD_MS)
                        .max(1);
                    session.move_cursor(x, y)?;
                    session.fake_button(1, true)?;
                    std::thread::sleep(Duration::from_millis(hold_ms.max(0) as u64));
                    session.fake_button(1, false)?;
                }
                "double_click" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    session.move_cursor(x, y)?;
                    for _ in 0..2 {
                        session.fake_button(1, true)?;
                        std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
                        session.fake_button(1, false)?;
                        std::thread::sleep(Duration::from_millis(80));
                    }
                }
                "right_click" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    session.move_cursor(x, y)?;
                    session.fake_button(3, true)?;
                    std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
                    session.fake_button(3, false)?;
                }
                "drag" => {
                    let from = (
                        require_float(float_env, "CODEX_GUI_FROM_X", event_mode)?,
                        require_float(float_env, "CODEX_GUI_FROM_Y", event_mode)?,
                    );
                    let to = (
                        require_float(float_env, "CODEX_GUI_TO_X", event_mode)?,
                        require_float(float_env, "CODEX_GUI_TO_Y", event_mode)?,
                    );
                    let duration_ms =
                        required_int(string_env, "CODEX_GUI_DURATION_MS", event_mode)?.max(1);
                    let steps = required_int(string_env, "CODEX_GUI_STEPS", event_mode)?.max(1);
                    let step_delay = drag_step_delay_ms(duration_ms, steps);
                    session.move_cursor(from.0, from.1)?;
                    session.fake_button(1, true)?;
                    std::thread::sleep(Duration::from_millis(50));
                    for (waypoint_x, waypoint_y) in drag_waypoints(from, to, steps) {
                        session.fake_motion(waypoint_x, waypoint_y)?;
                        std::thread::sleep(Duration::from_millis(step_delay));
                    }
                    session.fake_button(1, false)?;
                }
                "scroll" => {
                    if let (Some(x), Some(y)) = (
                        float_value(float_env, "CODEX_GUI_X"),
                        float_value(float_env, "CODEX_GUI_Y"),
                    ) {
                        session.move_cursor(x, y)?;
                    }
                    let scroll_y = required_int(string_env, "CODEX_GUI_SCROLL_Y", event_mode)?;
                    let scroll_x = required_int(string_env, "CODEX_GUI_SCROLL_X", event_mode)?;
                    let unit_is_pixel = string_value(string_env, "CODEX_GUI_SCROLL_UNIT")
                        .is_some_and(|unit| unit.eq_ignore_ascii_case("pixel"));
                    for (button, ticks) in scroll_button_plan(scroll_x, scroll_y, unit_is_pixel) {
                        for _ in 0..ticks {
                            session.fake_button(button, true)?;
                            std::thread::sleep(Duration::from_millis(10));
                            session.fake_button(button, false)?;
                            std::thread::sleep(Duration::from_millis(SCROLL_TICK_MILLIS));
                        }
                    }
                }
                "type_text" => {
                    let text = string_value(string_env, "CODEX_GUI_TEXT")
                        .ok_or_else(|| {
                            respond_to_model(format!(
                                "`CODEX_GUI_TEXT` is required for the `{event_mode}` GUI event."
                            ))
                        })?
                        .to_string();
                    let replace = string_value(string_env, "CODEX_GUI_REPLACE")
                        .is_some_and(|value| value != "0");
                    let submit = string_value(string_env, "CODEX_GUI_SUBMIT")
                        .is_some_and(|value| value == "1");
                    let strategy = string_value(string_env, "CODEX_GUI_TYPE_STRATEGY")
                        .unwrap_or("unicode")
                        .to_string();
                    let clear_repeat = clear_repeat_for_strategy(
                        strategy == "clipboard_paste",
                        optional_int(string_env, "CODEX_GUI_CLEAR_REPEAT")?,
                    );

                    if replace {
                        if strategy == "clipboard_paste" || strategy == "physical_keys" {
                            for _ in 0..clear_repeat {
                                session.inject_keysym(XK_BACKSPACE, false, &[])?;
                            }
                        } else {
                            // macOS uses Cmd+A (select all) before typing for
                            // the unicode strategy. The X11 in-app select-all
                            // is Ctrl+A: GTK/Qt apps do not bind Super+A, and
                            // under GNOME Shell Super+A is a global shortcut
                            // that opens the app grid.
                            session.inject_keysym(XK_A, false, &[XModifier::Control])?;
                        }
                    }
                    if strategy == "clipboard_paste" {
                        session.paste_text(
                            &text,
                            PASTE_EVENT_PRE_DELAY_MS,
                            PASTE_EVENT_POST_DELAY_MS,
                        )?;
                    } else if strategy == "physical_keys" {
                        for character in text.chars() {
                            session.inject_char(character)?;
                            std::thread::sleep(Duration::from_millis(PHYSICAL_KEY_CHAR_DELAY_MS));
                        }
                    } else {
                        for character in text.chars() {
                            session.inject_char(character)?;
                            std::thread::sleep(Duration::from_millis(UNICODE_CHAR_DELAY_MS));
                        }
                    }
                    if submit {
                        session.inject_keysym(XK_RETURN, false, &[])?;
                    }
                }
                "key_press" => {
                    let key_code = required_int(string_env, "CODEX_GUI_KEY_CODE", event_mode)?;
                    let repeat = optional_int(string_env, "CODEX_GUI_REPEAT")?
                        .unwrap_or(1)
                        .max(1);
                    let modifiers = parse_modifier_names(
                        string_value(string_env, "CODEX_GUI_MODIFIERS").unwrap_or_default(),
                    );
                    let Some(keysym) = macos_keycode_to_keysym(key_code) else {
                        return Err(respond_to_model(format!(
                            "unsupported macOS virtual key code `{key_code}` for the `{event_mode}` GUI event"
                        )));
                    };
                    for _ in 0..repeat {
                        session.inject_keysym(keysym, false, &modifiers)?;
                        std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
                    }
                }
                _ => {
                    return Err(respond_to_model(format!(
                        "unsupported GUI event mode `{event_mode}`; expected one of {}",
                        EVENT_MODES.join(", ")
                    )));
                }
            }
            let _ = session.conn.sync();
            Ok(())
        }

        fn run_system_events_type(
            &self,
            app: Option<&str>,
            window_selection: Option<&WindowSelector>,
            text: &str,
            replace: bool,
            submit: bool,
            strategy: &str,
        ) -> Result<(), ToolError> {
            let session = X11Session::shared()?;

            // macOS validates the requested window before typing.
            if let Some(window_selection) = window_selection {
                let (_, selection) = session.resolve_target(
                    app.map(str::trim).filter(|app| !app.is_empty()),
                    Some(window_selection),
                )?;
                if selection.is_none() {
                    return Err(respond_to_model(
                        "requested X11 window could not be found for System Events typing",
                    ));
                }
            }
            if let Some(requested_app) = app.map(str::trim).filter(|app| !app.is_empty()) {
                session.activate_app(requested_app)?;
            }

            // macOS parity: platform_macos.rs always forwards
            // CODEX_GUI_CLEAR_REPEAT=48 when replace is set, so the
            // type_system_events.applescript select-all branch
            // (`keystroke "a" using command down`) is unreachable through the
            // platform path and every strategy clears via 48x BackSpace —
            // including fields longer than 48 characters.
            if replace {
                for _ in 0..DEFAULT_CLEAR_REPEAT {
                    session.inject_keysym(XK_BACKSPACE, false, &[])?;
                }
            }

            match system_events_strategy(strategy) {
                SystemEventsStrategy::Paste => {
                    session.paste_text(
                        text,
                        PASTE_SYSTEM_PRE_DELAY_MS,
                        PASTE_SYSTEM_POST_DELAY_MS,
                    )?;
                }
                SystemEventsStrategy::Keystroke => {
                    for character in text.chars() {
                        session.inject_char(character)?;
                    }
                }
                SystemEventsStrategy::KeystrokeChars => {
                    for character in text.chars() {
                        session.inject_char(character)?;
                        std::thread::sleep(Duration::from_millis(
                            DEFAULT_KEYSTROKE_CHAR_DELAY_MS.max(0) as u64,
                        ));
                    }
                }
            }

            if submit {
                session.inject_keysym(XK_RETURN, false, &[])?;
            }
            let _ = session.conn.sync();
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Pure-logic unit tests (no X server required; run on any host)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Name/shape contract ------------------------------------------------

    /// The wiring layer holds `static PLATFORM: platform_linux::LinuxGuiPlatform
    /// = platform_linux::LinuxGuiPlatform;` — prove the struct is
    /// const-constructible.
    const PLATFORM: LinuxGuiPlatform = LinuxGuiPlatform;

    #[test]
    fn platform_is_const_constructible() {
        let _platform = PLATFORM;
        let _constructed = LinuxGuiPlatform;
    }

    // -- Capture-mode normalization -------------------------------------------

    #[test]
    fn normalize_capture_mode_env_accepts_common_variants() {
        assert_eq!(normalize_capture_mode_env("display"), Some("display"));
        assert_eq!(normalize_capture_mode_env("  DISPLAY "), Some("display"));
        assert_eq!(normalize_capture_mode_env("(Window)"), Some("window"));
        assert_eq!(normalize_capture_mode_env("\twindow\n"), Some("window"));
        assert_eq!(normalize_capture_mode_env(""), None);
        assert_eq!(normalize_capture_mode_env("windows"), None);
        assert_eq!(normalize_capture_mode_env("screen"), None);
    }

    // -- Display environment classification ------------------------------------

    #[test]
    fn classify_display_environment_matrix() {
        assert_eq!(
            classify_display_environment(None, Some(":0")),
            DisplayEnvironment::X11Only
        );
        assert_eq!(
            classify_display_environment(Some("wayland-0"), Some(":0")),
            DisplayEnvironment::XWayland
        );
        assert_eq!(
            classify_display_environment(Some("wayland-0"), None),
            DisplayEnvironment::WaylandOnly
        );
        assert_eq!(
            classify_display_environment(None, None),
            DisplayEnvironment::Headless
        );
        // Empty/blank values count as unset.
        assert_eq!(
            classify_display_environment(Some(""), Some(":1")),
            DisplayEnvironment::X11Only
        );
        assert_eq!(
            classify_display_environment(Some("wayland-0"), Some("  ")),
            DisplayEnvironment::WaylandOnly
        );
        assert_eq!(
            classify_display_environment(Some("  "), Some("   ")),
            DisplayEnvironment::Headless
        );
    }

    // -- Readiness status ladder -------------------------------------------------

    #[test]
    fn readiness_status_ladder_matches_macos_semantics() {
        use GuiReadinessStatus as S;
        assert_eq!(readiness_status_ladder(&[S::Ok, S::Ok]), "ready");
        assert_eq!(readiness_status_ladder(&[S::Ok, S::Warn]), "degraded");
        assert_eq!(readiness_status_ladder(&[S::Ok, S::Error]), "blocked");
        assert_eq!(
            readiness_status_ladder(&[S::Unsupported, S::Unsupported]),
            "unsupported"
        );
        assert_eq!(readiness_status_ladder(&[]), "unsupported"); // vacuous all(); unreachable — every snapshot emits checks
        // Only *all*-Unsupported is "unsupported"; any Ok/Warn keeps the
        // ladder going.
        assert_eq!(readiness_status_ladder(&[S::Unsupported, S::Ok]), "ready");
        assert_eq!(
            readiness_status_ladder(&[S::Unsupported, S::Warn]),
            "degraded"
        );
        // Error dominates Warn.
        assert_eq!(readiness_status_ladder(&[S::Warn, S::Error]), "blocked");
    }

    // -- macOS keycode → keysym table ----------------------------------------------

    #[test]
    fn macos_keycode_to_keysym_named_keys() {
        assert_eq!(macos_keycode_to_keysym(36), Some(XK_RETURN));
        assert_eq!(macos_keycode_to_keysym(48), Some(XK_TAB));
        assert_eq!(macos_keycode_to_keysym(53), Some(XK_ESCAPE));
        assert_eq!(macos_keycode_to_keysym(51), Some(XK_BACKSPACE));
        assert_eq!(macos_keycode_to_keysym(115), Some(XK_HOME));
        assert_eq!(macos_keycode_to_keysym(116), Some(XK_PAGE_UP));
        assert_eq!(macos_keycode_to_keysym(121), Some(XK_PAGE_DOWN));
        assert_eq!(macos_keycode_to_keysym(119), Some(XK_END));
        assert_eq!(macos_keycode_to_keysym(126), Some(XK_UP));
        assert_eq!(macos_keycode_to_keysym(125), Some(XK_DOWN));
        assert_eq!(macos_keycode_to_keysym(123), Some(XK_LEFT));
        assert_eq!(macos_keycode_to_keysym(124), Some(XK_RIGHT));
        assert_eq!(macos_keycode_to_keysym(49), Some(0x020));
    }

    #[test]
    fn macos_keycode_to_keysym_letter_table() {
        let expected: [(i64, u32); 26] = [
            (0, 0x061),
            (1, 0x073),
            (2, 0x064),
            (3, 0x066),
            (4, 0x068),
            (5, 0x067),
            (6, 0x07a),
            (7, 0x078),
            (8, 0x063),
            (9, 0x076),
            (11, 0x062),
            (12, 0x071),
            (13, 0x077),
            (14, 0x065),
            (15, 0x072),
            (16, 0x079),
            (17, 0x074),
            (31, 0x06f),
            (32, 0x075),
            (34, 0x069),
            (35, 0x070),
            (37, 0x06c),
            (38, 0x06a),
            (40, 0x06b),
            (45, 0x06e),
            (46, 0x06d),
        ];
        for (key_code, keysym) in expected {
            assert_eq!(
                macos_keycode_to_keysym(key_code),
                Some(keysym),
                "keycode {key_code}"
            );
        }
        // Unknown and gap codes.
        assert_eq!(macos_keycode_to_keysym(10), None);
        assert_eq!(macos_keycode_to_keysym(999), None);
        assert_eq!(macos_keycode_to_keysym(-1), None);
    }

    #[test]
    fn macos_keycode_to_keysym_digit_and_punctuation_table() {
        let expected: [(i64, u32); 17] = [
            (18, 0x031), // 1
            (19, 0x032), // 2
            (20, 0x033), // 3
            (21, 0x034), // 4
            (22, 0x036), // 6
            (23, 0x035), // 5
            (24, 0x03d), // =
            (25, 0x039), // 9
            (26, 0x037), // 7
            (27, 0x02d), // -
            (28, 0x038), // 8
            (29, 0x030), // 0
            (30, 0x05d), // ]
            (33, 0x05b), // [
            (39, 0x027), // '
            (41, 0x03b), // ;
            (47, 0x02e), // .
        ];
        for (key_code, keysym) in expected {
            assert_eq!(
                macos_keycode_to_keysym(key_code),
                Some(keysym),
                "keycode {key_code}"
            );
        }
        assert_eq!(macos_keycode_to_keysym(42), Some(0x05c));
        assert_eq!(macos_keycode_to_keysym(43), Some(0x02c));
        assert_eq!(macos_keycode_to_keysym(44), Some(0x02f));
    }

    // -- Character → keysym mapping -------------------------------------------------

    #[test]
    fn keysym_for_char_covers_ascii_and_latin1() {
        assert_eq!(keysym_for_char('a'), Some((0x061, false)));
        assert_eq!(keysym_for_char('z'), Some((0x07a, false)));
        assert_eq!(keysym_for_char('A'), Some((0x061, true)));
        assert_eq!(keysym_for_char('Z'), Some((0x07a, true)));
        assert_eq!(keysym_for_char('5'), Some((0x035, false)));
        assert_eq!(keysym_for_char('0'), Some((0x030, false)));
        // Shifted punctuation maps to the unshifted keysym + Shift hint.
        assert_eq!(keysym_for_char('%'), Some((0x035, true)));
        assert_eq!(keysym_for_char('@'), Some((0x032, true)));
        assert_eq!(keysym_for_char('?'), Some((0x02f, true)));
        assert_eq!(keysym_for_char('_'), Some((0x02d, true)));
        assert_eq!(keysym_for_char('-'), Some((0x02d, false)));
        assert_eq!(keysym_for_char(' '), Some((0x020, false)));
        assert_eq!(keysym_for_char('\n'), Some((XK_RETURN, false)));
        assert_eq!(keysym_for_char('\r'), Some((XK_RETURN, false)));
        assert_eq!(keysym_for_char('\t'), Some((XK_TAB, false)));
        // Latin-1 supplement maps to the code point directly.
        assert_eq!(keysym_for_char('é'), Some((0x0e9, false)));
        assert_eq!(keysym_for_char('ß'), Some((0x0df, false)));
        // Beyond Latin-1 there is no stable mapping.
        assert_eq!(keysym_for_char('😀'), None);
        assert_eq!(keysym_for_char('中'), None);
    }

    // -- Modifier parsing -------------------------------------------------------------

    #[test]
    fn parse_modifier_names_handles_order_and_duplicates() {
        assert_eq!(parse_modifier_names("shift"), vec![XModifier::Shift]);
        assert_eq!(parse_modifier_names("option"), vec![XModifier::Alt]);
        assert_eq!(parse_modifier_names("ctrl"), vec![XModifier::Control]);
        // Canonical order: Control → Alt → Command → Shift; dedupe; unknown
        // tokens ignored.
        assert_eq!(
            parse_modifier_names("shift,Control,shift,alt,bogus,CONTROL,command"),
            vec![
                XModifier::Control,
                XModifier::Alt,
                XModifier::Command,
                XModifier::Shift
            ]
        );
        assert_eq!(parse_modifier_names(""), Vec::new());
        assert_eq!(
            parse_modifier_names("  command , ctrl "),
            vec![XModifier::Control, XModifier::Command]
        );
    }

    #[test]
    fn modifier_map_slots_match_get_modifier_mapping_layout() {
        assert_eq!(modifier_map_slot(XModifier::Shift), 0);
        assert_eq!(modifier_map_slot(XModifier::Control), 2);
        assert_eq!(modifier_map_slot(XModifier::Alt), 3); // Mod1
        assert_eq!(modifier_map_slot(XModifier::Command), 6); // Mod4
    }

    // -- Env extraction ------------------------------------------------------------------

    #[test]
    fn float_and_string_value_helpers() {
        let floats = [("CODEX_GUI_X", 12.5f64)];
        assert_eq!(float_value(&floats, "CODEX_GUI_X"), Some(12.5));
        assert_eq!(float_value(&floats, "CODEX_GUI_Y"), None);

        let strings = [
            ("CODEX_GUI_TEXT".to_string(), "  hi  ".to_string()),
            ("CODEX_GUI_EMPTY".to_string(), "   ".to_string()),
        ];
        let references: Vec<(&str, String)> = strings
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone()))
            .collect();
        assert_eq!(string_value(&references, "CODEX_GUI_TEXT"), Some("hi"));
        assert_eq!(string_value(&references, "CODEX_GUI_EMPTY"), None);
        assert_eq!(string_value(&references, "MISSING"), None);
    }

    #[test]
    fn event_modes_cover_the_helper_protocol() {
        for mode in [
            "move_cursor",
            "click",
            "click_and_hold",
            "double_click",
            "right_click",
            "drag",
            "scroll",
            "type_text",
            "key_press",
        ] {
            assert!(
                EVENT_MODES.contains(&mode),
                "EVENT_MODES is missing `{mode}`"
            );
        }
        assert_eq!(EVENT_MODES.len(), 9);
    }

    // -- Scroll mapping ----------------------------------------------------------------

    #[test]
    fn scroll_button_plan_maps_signs_to_buttons() {
        assert_eq!(scroll_button_plan(0, 3, false), vec![(4, 3)]); // up
        assert_eq!(scroll_button_plan(0, -2, false), vec![(5, 2)]); // down
        assert_eq!(scroll_button_plan(-5, 0, false), vec![(6, 5)]); // left
        assert_eq!(scroll_button_plan(7, 0, false), vec![(7, 7)]); // right
        assert_eq!(scroll_button_plan(0, 0, false), Vec::new());
        // Vertical first, then horizontal.
        assert_eq!(scroll_button_plan(3, -1, false), vec![(5, 1), (7, 3)]);
    }

    #[test]
    fn scroll_tick_count_pixel_mode_divides_by_fifty() {
        assert_eq!(scroll_tick_count(0, false), 0);
        assert_eq!(scroll_tick_count(3, false), 3);
        assert_eq!(scroll_tick_count(120, true), 2);
        assert_eq!(scroll_tick_count(50, true), 1);
        // Sub-tick pixel amounts still scroll at least one tick.
        assert_eq!(scroll_tick_count(49, true), 1);
        assert_eq!(scroll_tick_count(-49, true), 1);
    }

    // -- Drag math ---------------------------------------------------------------------

    #[test]
    fn drag_waypoints_are_linear_and_include_the_endpoint() {
        assert_eq!(
            drag_waypoints((0.0, 0.0), (100.0, 50.0), 4),
            vec![(25.0, 12.5), (50.0, 25.0), (75.0, 37.5), (100.0, 50.0)]
        );
        // Steps < 1 degrade to a single endpoint waypoint.
        assert_eq!(drag_waypoints((1.0, 2.0), (3.0, 4.0), 0), vec![(3.0, 4.0)]);
    }

    #[test]
    fn drag_step_delay_matches_macos_helper() {
        assert_eq!(drag_step_delay_ms(1000, 24), 41); // 1000/24 = 41
        assert_eq!(drag_step_delay_ms(200, 24), 10); // max(8, 10)
        assert_eq!(drag_step_delay_ms(0, 0), 10); // max(0, 10), steps clamped
    }

    // -- Clear repeat / System Events strategy --------------------------------------------

    #[test]
    fn clear_repeat_defaults_match_the_helper() {
        assert_eq!(clear_repeat_for_strategy(true, None), 48);
        assert_eq!(clear_repeat_for_strategy(false, None), 0);
        assert_eq!(clear_repeat_for_strategy(true, Some(5)), 5);
        assert_eq!(clear_repeat_for_strategy(false, Some(7)), 7);
        assert_eq!(clear_repeat_for_strategy(true, Some(-3)), 0);
    }

    #[test]
    fn system_events_strategy_unknown_falls_back_to_paste() {
        assert_eq!(
            system_events_strategy("system_events_paste"),
            SystemEventsStrategy::Paste
        );
        assert_eq!(
            system_events_strategy("system_events_keystroke"),
            SystemEventsStrategy::Keystroke
        );
        assert_eq!(
            system_events_strategy("system_events_keystroke_chars"),
            SystemEventsStrategy::KeystrokeChars
        );
        assert_eq!(system_events_strategy("bogus"), SystemEventsStrategy::Paste);
        assert_eq!(system_events_strategy(""), SystemEventsStrategy::Paste);
    }

    // -- XQueryKeymap fallback suppression window -------------------------------

    #[test]
    fn keymap_press_expected_only_within_suppression_window() {
        // Armed and sampled inside the window: the expected programmatic
        // Escape; the flag is consumed and monitoring continues.
        assert!(keymap_press_is_expected(
            true,
            Some(Duration::from_millis(0))
        ));
        assert!(keymap_press_is_expected(
            true,
            Some(Duration::from_millis(KEYMAP_SUPPRESS_WINDOW_MS))
        ));
        // Sampled after the window: a missed injected press must not leave
        // the flag armed forever — the next Escape is a genuine emergency
        // stop and has to fire.
        assert!(!keymap_press_is_expected(
            true,
            Some(Duration::from_millis(KEYMAP_SUPPRESS_WINDOW_MS + 1))
        ));
        assert!(!keymap_press_is_expected(
            true,
            Some(Duration::from_secs(10))
        ));
        // Flag never armed: a genuine user press.
        assert!(!keymap_press_is_expected(
            false,
            Some(Duration::from_millis(0))
        ));
        // Defensive: an armed flag with no observed arming time (unreachable
        // in the poll loop, which stamps it on first sight) is not trusted.
        assert!(!keymap_press_is_expected(true, None));
        assert!(!keymap_press_is_expected(false, None));
    }

    // -- Geometry helpers ---------------------------------------------------------------

    #[test]
    fn rect_contains_point_excludes_right_and_bottom_edges() {
        let rect = RectI {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert!(rect.contains_point((0.0, 0.0)));
        assert!(rect.contains_point((9.9, 9.9)));
        assert!(!rect.contains_point((10.0, 5.0)));
        assert!(!rect.contains_point((5.0, 10.0)));
        assert!(!rect.contains_point((-0.1, 5.0)));
    }

    #[test]
    fn display_for_point_picks_containing_display_then_first() {
        let displays = vec![
            DisplayInfo {
                index: 1,
                bounds: RectI {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            DisplayInfo {
                index: 2,
                bounds: RectI {
                    x: 1920,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
        ];
        assert_eq!(
            display_for_point((500.0, 400.0), &displays).unwrap().index,
            1
        );
        assert_eq!(
            display_for_point((1920.0, 5.0), &displays).unwrap().index,
            2
        );
        assert_eq!(
            display_for_point((3839.0, 1079.0), &displays)
                .unwrap()
                .index,
            2
        );
        // Outside every display falls back to the first.
        assert_eq!(
            display_for_point((5000.0, 5000.0), &displays)
                .unwrap()
                .index,
            1
        );
        assert!(display_for_point((1.0, 1.0), &[]).is_none());
    }

    // -- Window ranking / selection --------------------------------------------------------

    fn candidate(
        id: i64,
        stacking_index: usize,
        x: i64,
        y: i64,
        width: i64,
        height: i64,
        title: Option<&str>,
    ) -> WindowCandidate {
        WindowCandidate {
            id,
            title: title.map(ToOwned::to_owned),
            bounds: RectI {
                x,
                y,
                width,
                height,
            },
            stacking_index,
        }
    }

    #[test]
    fn rank_windows_prefers_topmost_then_area_then_title() {
        let ranked = rank_windows(vec![
            candidate(1, 2, 0, 0, 100, 100, Some("small top? no")),
            candidate(2, 1, 0, 0, 500, 500, None),
            candidate(3, 0, 0, 0, 100, 100, None),
        ]);
        // Stacking order dominates: index 0 first.
        assert_eq!(ranked[0].id, 3);
        assert_eq!(ranked[1].id, 2);
        assert_eq!(ranked[2].id, 1);
    }

    #[test]
    fn select_window_handles_index_explicit_single_and_union() {
        let ranked = vec![
            candidate(2, 0, 500, 500, 200, 200, None),
            candidate(1, 1, 0, 0, 100, 100, Some("Alpha")),
        ];

        // Empty selection.
        assert!(select_window(&[], None, false).is_none());

        // Explicit 1-based index picks that window.
        let outcome = select_window(&ranked, Some(2), true).unwrap();
        assert_eq!(outcome.primary.id, 1);
        assert_eq!(outcome.window_count, 1);
        assert_eq!(outcome.strategy, "selected_window");

        // Index 1 picks the ranked-first window.
        let outcome = select_window(&ranked, Some(1), false).unwrap();
        assert_eq!(outcome.primary.id, 2);
        assert_eq!(outcome.strategy, "selected_window");

        // Out-of-range index falls through to the ranked-first window.
        let outcome = select_window(&ranked, Some(9), true).unwrap();
        assert_eq!(outcome.primary.id, 2);
        assert_eq!(outcome.strategy, "selected_window");

        // Explicit title selection without index.
        let outcome = select_window(&ranked, None, true).unwrap();
        assert_eq!(outcome.primary.id, 2);
        assert_eq!(outcome.window_count, 1);

        // Single window, no explicit selection.
        let single = vec![candidate(7, 0, 10, 10, 300, 200, Some("Solo"))];
        let outcome = select_window(&single, None, false).unwrap();
        assert_eq!(outcome.strategy, "main_window");
        assert_eq!(outcome.window_count, 1);
        assert_eq!(outcome.bounds.width, 300);

        // Multiple windows without explicit selection: union of bounds.
        let outcome = select_window(&ranked, None, false).unwrap();
        assert_eq!(outcome.strategy, "app_union");
        assert_eq!(outcome.window_count, 2);
        assert_eq!(outcome.primary.id, 2);
        assert_eq!(
            outcome.bounds,
            RectI {
                x: 0,
                y: 0,
                width: 700,
                height: 700
            }
        );
    }

    // -- App matching ----------------------------------------------------------------------

    #[test]
    fn app_matches_window_semantics() {
        assert!(app_matches_window(
            "Firefox",
            Some("firefox"),
            Some("Firefox"),
            Some("Mozilla Firefox"),
            None
        ));
        assert!(app_matches_window(
            "firefox",
            None,
            Some("Firefox"),
            None,
            None
        ));
        assert!(app_matches_window(
            "vim",
            None,
            None,
            Some("a.txt - VIM"),
            Some("vim")
        ));
        // Title containment.
        assert!(app_matches_window(
            "terminal",
            None,
            None,
            Some("Terminal — zsh"),
            None
        ));
        // Case-insensitive equality on comm.
        assert!(app_matches_window(
            "gnome-term",
            None,
            None,
            None,
            Some("gnome-term")
        ));
        // No match paths.
        assert!(!app_matches_window(
            "safari",
            Some("firefox"),
            None,
            Some("Mozilla"),
            None
        ));
        assert!(!app_matches_window("", Some("firefox"), None, None, None));
        assert!(!app_matches_window("  ", None, None, Some("Firefox"), None));
    }

    // -- Client messages -------------------------------------------------------------------

    #[test]
    fn wm_change_state_message_carries_the_state() {
        let iconic = wm_change_state_message(WM_STATE_ICONIC);
        assert_eq!(iconic.format, 32);
        assert_eq!(iconic.values, [3, 0, 0, 0, 0]);
        let normal = wm_change_state_message(WM_STATE_NORMAL);
        assert_eq!(normal.format, 32);
        assert_eq!(normal.values, [1, 0, 0, 0, 0]);
    }

    #[test]
    fn net_active_window_message_requests_pager_activation() {
        let message = net_active_window_message();
        assert_eq!(message.format, 32);
        // source indication 2 = pager, timestamp 0 = CurrentTime.
        assert_eq!(message.values, [2, 0, 0, 0, 0]);
    }

    // -- Escape detection over RECORD ---------------------------------------------------------

    #[test]
    fn is_escape_key_press_matches_core_keypress_header() {
        assert!(is_escape_key_press(&[2, 9, 0, 0], 9));
        assert!(!is_escape_key_press(&[3, 9, 0, 0], 9)); // KeyRelease
        assert!(!is_escape_key_press(&[2, 10, 0, 0], 9)); // other keycode
        assert!(!is_escape_key_press(&[2], 2)); // truncated
        assert!(!is_escape_key_press(&[], 2));
    }

    // -- Keyboard mapping construction ---------------------------------------------------------

    #[test]
    fn build_keysym_map_prefers_column_zero() {
        // min_keycode 8, 2 keysyms per keycode:
        //   kc8:  [0x30, 0x29]  kc9: [0x31, 0x21]  kc10: [0x30, 0x0]  kc11: [0x29, 0x0]
        let keysyms = [0x30, 0x29, 0x31, 0x21, 0x30, 0x0, 0x29, 0x0];
        let map = build_keysym_map(8, 2, &keysyms);
        // Column-0 duplicate does not override the earlier column-0 entry.
        assert_eq!(map.get(&0x30), Some(&(8, 0)));
        // Column-0 entry replaces an earlier column-1 entry.
        assert_eq!(map.get(&0x29), Some(&(11, 0)));
        assert_eq!(map.get(&0x31), Some(&(9, 0)));
        assert_eq!(map.get(&0x21), Some(&(9, 1)));
        assert!(!map.contains_key(&0x0));
    }

    #[test]
    fn modifier_keycode_extraction_reads_slots_and_skips_zero() {
        let keycodes = [
            10, 11, // Shift
            12, 13, // Lock
            14, 15, // Control
            16, 17, // Mod1
            0, 0, // Mod2 (empty)
            20, 21, // Mod3
            22, 23, // Mod4
            24, 25, // Mod5
        ];
        assert_eq!(modifier_keycode_from_reply(&keycodes, 2, 0), Some(10));
        assert_eq!(modifier_keycode_from_reply(&keycodes, 2, 2), Some(14));
        assert_eq!(modifier_keycode_from_reply(&keycodes, 2, 4), None); // 0 → empty
        assert_eq!(modifier_keycode_from_reply(&keycodes, 2, 6), Some(22));
        assert_eq!(modifier_keycode_from_reply(&keycodes, 2, 8), None); // out of range
    }

    // -- Property serialization -------------------------------------------------------------------

    #[test]
    fn property32_bytes_respects_server_byte_order() {
        assert_eq!(property32_bytes(&[0x01020304], false), vec![1, 2, 3, 4]);
        assert_eq!(property32_bytes(&[0x01020304], true), vec![4, 3, 2, 1]);
        assert_eq!(
            property32_bytes(&[1, 2], true),
            vec![1, 0, 0, 0, 2, 0, 0, 0]
        );
        assert!(property32_bytes(&[], true).is_empty());
    }
}
