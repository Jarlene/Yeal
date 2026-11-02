//! Windows implementation of the GUI platform seam — in-process pure-FFI
//! Win32 backend on `windows-sys` (no helper binary, no COM).
//!
//! Screenshots use GDI `BitBlt` over the virtual screen plus `GetDIBits`
//! (32bpp, top-down) with a BGRA→RGBA swap; input injection uses `SendInput`
//! only (absolute moves normalized to the virtual screen, `MOUSEEVENTF_WHEEL`
//! /`HWHEEL` scrolling, VK+scancode keys, `KEYEVENTF_UNICODE` typing); window
//! management uses `EnumWindows` / `GetWindowTextW` /
//! `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` (with a
//! `GetWindowRect` fallback); the emergency-stop monitor runs a
//! `WH_KEYBOARD_LL` hook on a dedicated thread pumped with `GetMessageW`.
//! Clipboard paste strategies use `arboard` (`set_text` + injected Ctrl+V).
//! Every Win32 failure surfaces as a typed [`ToolError`]
//! (`ToolErrorKind::Execution`) — nothing panics or unwraps on API failures.
//! All methods are blocking; the engine wraps them in
//! `tokio::task::spawn_blocking`.
//!
//! Windows has no permission model like macOS TCC; the readiness notes are:
//! an interactive desktop session is required, UIPI blocks synthetic input
//! to elevated (Administrator) targets from a non-elevated process, and
//! `SetForegroundWindow` activation is subject to the Windows foreground
//! lock (verified with `GetForegroundWindow`, never retried blindly).
//!
//! Everything above the `mod win32` block is compiled on every target so the
//! unit tests (no Win32 calls required) run on any host; all FFI code is
//! gated behind `#[cfg(target_os = "windows")]`.

#![cfg_attr(not(target_os = "windows"), allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

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

fn respond_to_model(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorKind::Execution, message)
}

/// Mirror of the macOS/helper `capture-mode` env parsing: trims, strips
/// non-alphabetic decorations, and accepts `display`/`window`
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

/// Status ladder shared with the macOS/Linux backends: all Unsupported →
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

// macOS/Linux-parity timing constants (helper + platform_macos semantics).
const DEFAULT_MOVE_SETTLE_MS: i64 = 200;
const DEFAULT_CLICK_AND_HOLD_MS: i64 = 650;
const DEFAULT_KEYSTROKE_CHAR_DELAY_MS: i64 = 55;
/// `CODEX_GUI_CLEAR_REPEAT` default: 48 backspaces (macOS helper semantics).
const DEFAULT_CLEAR_REPEAT: i64 = 48;
const KEY_MILLIS: u64 = 20;
const CLICK_MILLIS: u64 = 30;
const ACTIVATE_SETTLE_MS: u64 = 100;
// Paste-chord pacing (helper `pasteText` / System Events `set the clipboard`).
const PASTE_EVENT_PRE_DELAY_MS: u64 = 100;
const PASTE_EVENT_POST_DELAY_MS: u64 = 150;
const PASTE_SYSTEM_PRE_DELAY_MS: u64 = 220;
const PASTE_SYSTEM_POST_DELAY_MS: u64 = 650;
const PHYSICAL_KEY_CHAR_DELAY_MS: u64 = 90;
const UNICODE_CHAR_DELAY_MS: u64 = 30;

/// Wheel events are paced like the Linux X11 tick loop (10ms press + 15ms
/// tick gap collapses to one pacing delay per discrete wheel event).
pub(crate) const WHEEL_SEND_MILLIS: u64 = 15;

/// Virtual-key codes for the pure-logic layer, kept independent of
/// `windows-sys` so the tables and tests compile on every host. The FFI
/// layer asserts parity with the `windows-sys` constants at compile time.
pub(crate) mod vk {
    pub(crate) const VK_BACK: u16 = 0x08;
    pub(crate) const VK_TAB: u16 = 0x09;
    pub(crate) const VK_RETURN: u16 = 0x0D;
    pub(crate) const VK_SHIFT: u16 = 0x10;
    pub(crate) const VK_CONTROL: u16 = 0x11;
    pub(crate) const VK_MENU: u16 = 0x12;
    pub(crate) const VK_ESCAPE: u16 = 0x1B;
    pub(crate) const VK_SPACE: u16 = 0x20;
    pub(crate) const VK_PRIOR: u16 = 0x21;
    pub(crate) const VK_NEXT: u16 = 0x22;
    pub(crate) const VK_END: u16 = 0x23;
    pub(crate) const VK_HOME: u16 = 0x24;
    pub(crate) const VK_LEFT: u16 = 0x25;
    pub(crate) const VK_UP: u16 = 0x26;
    pub(crate) const VK_RIGHT: u16 = 0x27;
    pub(crate) const VK_DOWN: u16 = 0x28;
    pub(crate) const VK_LWIN: u16 = 0x5B;
    pub(crate) const VK_A: u16 = 0x41;
    pub(crate) const VK_V: u16 = 0x56;
    pub(crate) const VK_OEM_1: u16 = 0xBA; // ';:' key
    pub(crate) const VK_OEM_PLUS: u16 = 0xBB;
    pub(crate) const VK_OEM_COMMA: u16 = 0xBC;
    pub(crate) const VK_OEM_MINUS: u16 = 0xBD;
    pub(crate) const VK_OEM_PERIOD: u16 = 0xBE;
    pub(crate) const VK_OEM_2: u16 = 0xBF; // '/?' key
    pub(crate) const VK_OEM_3: u16 = 0xC0; // '`~' key
    pub(crate) const VK_OEM_4: u16 = 0xDB; // '[{' key
    pub(crate) const VK_OEM_5: u16 = 0xDC; // '\|' key
    pub(crate) const VK_OEM_6: u16 = 0xDD; // ']}' key
    pub(crate) const VK_OEM_7: u16 = 0xDE; // ''"' key
    pub(crate) const VK_F1: u16 = 0x70;
    pub(crate) const VK_F2: u16 = 0x71;
    pub(crate) const VK_F3: u16 = 0x72;
    pub(crate) const VK_F4: u16 = 0x73;
    pub(crate) const VK_F5: u16 = 0x74;
    pub(crate) const VK_F6: u16 = 0x75;
    pub(crate) const VK_F7: u16 = 0x76;
    pub(crate) const VK_F8: u16 = 0x77;
    pub(crate) const VK_F9: u16 = 0x78;
    pub(crate) const VK_F10: u16 = 0x79;
    pub(crate) const VK_F11: u16 = 0x7A;
    pub(crate) const VK_F12: u16 = 0x7B;
}

/// A Windows key: virtual-key code plus PS/2 set-1 scancode and the
/// extended-key flag (arrows/navigation live on the 0xE0-prefixed cluster).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowsKey {
    pub(crate) vk: u16,
    pub(crate) scancode: u16,
    pub(crate) extended: bool,
}

/// Map a macOS virtual keycode (the values `handler::resolve_key_code` emits
/// into `CODEX_GUI_KEY_CODE`) to its Windows key. Shifted variants are
/// produced by adding Shift to `CODEX_GUI_MODIFIERS`, mirroring the helper
/// protocol. Table covers exactly the codes `resolve_key_code` accepts.
pub(crate) fn macos_keycode_to_key(key_code: i64) -> Option<WindowsKey> {
    let (vk, scancode, extended) = match key_code {
        // Named keys (handler.rs `named` table).
        36 => (vk::VK_RETURN, 0x1C, false), // enter/return
        48 => (vk::VK_TAB, 0x0F, false),    // tab
        53 => (vk::VK_ESCAPE, 0x01, false), // escape/esc
        51 => (vk::VK_BACK, 0x0E, false),   // delete/backspace
        115 => (vk::VK_HOME, 0x47, true),
        116 => (vk::VK_PRIOR, 0x49, true), // pageup
        121 => (vk::VK_NEXT, 0x51, true),  // pagedown
        119 => (vk::VK_END, 0x4F, true),
        123 => (vk::VK_LEFT, 0x4B, true),
        124 => (vk::VK_RIGHT, 0x4D, true),
        125 => (vk::VK_DOWN, 0x50, true),
        126 => (vk::VK_UP, 0x48, true),
        49 => (vk::VK_SPACE, 0x39, false),
        // Letter rows (macOS virtual keycodes; note the unsorted macOS
        // ordering, e.g. 4=h and 5=g).
        0 => (vk::VK_A, 0x1E, false), // a
        1 => (0x53, 0x1F, false),     // s
        2 => (0x44, 0x20, false),     // d
        3 => (0x46, 0x21, false),     // f
        4 => (0x48, 0x23, false),     // h
        5 => (0x47, 0x22, false),     // g
        6 => (0x5A, 0x2C, false),     // z
        7 => (0x58, 0x2D, false),     // x
        8 => (0x43, 0x2E, false),     // c
        9 => (vk::VK_V, 0x2F, false), // v
        11 => (0x42, 0x30, false),    // b
        12 => (0x51, 0x10, false),    // q
        13 => (0x57, 0x11, false),    // w
        14 => (0x45, 0x12, false),    // e
        15 => (0x52, 0x13, false),    // r
        16 => (0x59, 0x15, false),    // y
        17 => (0x54, 0x14, false),    // t
        31 => (0x4F, 0x18, false),    // o
        32 => (0x55, 0x16, false),    // u
        34 => (0x49, 0x17, false),    // i
        35 => (0x50, 0x19, false),    // p
        37 => (0x4C, 0x26, false),    // l
        38 => (0x4A, 0x24, false),    // j
        40 => (0x4B, 0x25, false),    // k
        45 => (0x4E, 0x31, false),    // n
        46 => (0x4D, 0x32, false),    // m
        // Digits + punctuation row (unshifted).
        18 => (0x31, 0x02, false),              // 1
        19 => (0x32, 0x03, false),              // 2
        20 => (0x33, 0x04, false),              // 3
        21 => (0x34, 0x05, false),              // 4
        22 => (0x36, 0x07, false),              // 6
        23 => (0x35, 0x06, false),              // 5
        24 => (vk::VK_OEM_PLUS, 0x0D, false),   // =
        25 => (0x39, 0x0A, false),              // 9
        26 => (0x37, 0x08, false),              // 7
        27 => (vk::VK_OEM_MINUS, 0x0C, false),  // -
        28 => (0x38, 0x09, false),              // 8
        29 => (0x30, 0x0B, false),              // 0
        30 => (vk::VK_OEM_6, 0x1B, false),      // ]
        33 => (vk::VK_OEM_4, 0x1A, false),      // [
        39 => (vk::VK_OEM_7, 0x28, false),      // '
        41 => (vk::VK_OEM_1, 0x27, false),      // ;
        42 => (vk::VK_OEM_5, 0x2B, false),      // backslash
        43 => (vk::VK_OEM_COMMA, 0x33, false),  // ,
        44 => (vk::VK_OEM_2, 0x35, false),      // /
        47 => (vk::VK_OEM_PERIOD, 0x34, false), // .
        // Function keys (macOS 119-based codes above end/arrow keys).
        122 => (vk::VK_F1, 0x3B, false),
        120 => (vk::VK_F2, 0x3C, false),
        99 => (vk::VK_F3, 0x3D, false),
        118 => (vk::VK_F4, 0x3E, false),
        96 => (vk::VK_F5, 0x3F, false),
        97 => (vk::VK_F6, 0x40, false),
        98 => (vk::VK_F7, 0x41, false),
        100 => (vk::VK_F8, 0x42, false),
        101 => (vk::VK_F9, 0x43, false),
        109 => (vk::VK_F10, 0x44, false),
        103 => (vk::VK_F11, 0x57, false),
        111 => (vk::VK_F12, 0x58, false),
        _ => return None,
    };
    Some(WindowsKey {
        vk,
        scancode,
        extended,
    })
}

/// US-layout unshifted character → (virtual-key, scancode) table for the
/// `physical_keys` strategy. Uppercase and shifted punctuation add Shift,
/// mirroring the macOS base/shift keycode tables.
const CHAR_KEYS: [(char, u16, u16); 48] = [
    ('a', vk::VK_A, 0x1E),
    ('b', 0x42, 0x30),
    ('c', 0x43, 0x2E),
    ('d', 0x44, 0x20),
    ('e', 0x45, 0x12),
    ('f', 0x46, 0x21),
    ('g', 0x47, 0x22),
    ('h', 0x48, 0x23),
    ('i', 0x49, 0x17),
    ('j', 0x4A, 0x24),
    ('k', 0x4B, 0x25),
    ('l', 0x4C, 0x26),
    ('m', 0x4D, 0x32),
    ('n', 0x4E, 0x31),
    ('o', 0x4F, 0x18),
    ('p', 0x50, 0x19),
    ('q', 0x51, 0x10),
    ('r', 0x52, 0x13),
    ('s', 0x53, 0x1F),
    ('t', 0x54, 0x14),
    ('u', 0x55, 0x16),
    ('v', vk::VK_V, 0x2F),
    ('w', 0x57, 0x11),
    ('x', 0x58, 0x2D),
    ('y', 0x59, 0x15),
    ('z', 0x5A, 0x2C),
    ('0', 0x30, 0x0B),
    ('1', 0x31, 0x02),
    ('2', 0x32, 0x03),
    ('3', 0x33, 0x04),
    ('4', 0x34, 0x05),
    ('5', 0x35, 0x06),
    ('6', 0x36, 0x07),
    ('7', 0x37, 0x08),
    ('8', 0x38, 0x09),
    ('9', 0x39, 0x0A),
    ('-', vk::VK_OEM_MINUS, 0x0C),
    ('=', vk::VK_OEM_PLUS, 0x0D),
    ('[', vk::VK_OEM_4, 0x1A),
    (']', vk::VK_OEM_6, 0x1B),
    ('\\', vk::VK_OEM_5, 0x2B),
    (';', vk::VK_OEM_1, 0x27),
    ('\'', vk::VK_OEM_7, 0x28),
    (',', vk::VK_OEM_COMMA, 0x33),
    ('.', vk::VK_OEM_PERIOD, 0x34),
    ('/', vk::VK_OEM_2, 0x35),
    ('`', vk::VK_OEM_3, 0x29),
    (' ', vk::VK_SPACE, 0x39),
];

/// Map a character to its Windows key plus a "needs Shift" hint for the
/// `physical_keys` strategy. Only the US-layout printable set maps; anything
/// else falls back to the `unicode`/`clipboard_paste` strategies (mirrors the
/// Linux `keysym_for_char` failure mode).
pub(crate) fn char_key(character: char) -> Option<(WindowsKey, bool)> {
    let (base, needs_shift) = match character {
        'a'..='z' | '0'..='9' | '-' | '=' | '[' | ']' | '\\' | ';' | '\'' | ',' | '.' | '/' => {
            (character, false)
        }
        'A'..='Z' => (character.to_ascii_lowercase(), true),
        '`' => ('`', false),
        '~' => ('`', true),
        '!' => ('1', true),
        '@' => ('2', true),
        '#' => ('3', true),
        '$' => ('4', true),
        '%' => ('5', true),
        '^' => ('6', true),
        '&' => ('7', true),
        '*' => ('8', true),
        '(' => ('9', true),
        ')' => ('0', true),
        '_' => ('-', true),
        '+' => ('=', true),
        '{' => ('[', true),
        '}' => (']', true),
        '|' => ('\\', true),
        ':' => (';', true),
        '"' => ('\'', true),
        '<' => (',', true),
        '>' => ('.', true),
        '?' => ('/', true),
        '\t' => {
            return Some((
                WindowsKey {
                    vk: vk::VK_TAB,
                    scancode: 0x0F,
                    extended: false,
                },
                false,
            ));
        }
        '\n' | '\r' => {
            return Some((
                WindowsKey {
                    vk: vk::VK_RETURN,
                    scancode: 0x1C,
                    extended: false,
                },
                false,
            ));
        }
        _ => return None,
    };
    let (_, key_vk, scancode) = CHAR_KEYS.iter().copied().find(|entry| entry.0 == base)?;
    Some((
        WindowsKey {
            vk: key_vk,
            scancode,
            extended: false,
        },
        needs_shift,
    ))
}

/// Characters the `KEYEVENTF_UNICODE` typing path must not send as raw
/// UTF-16 units. An injected LF unit arrives as `WM_CHAR '\n'`, which
/// standard edit controls ignore (they expect `\r`/VK_RETURN, and LF never
/// triggers a default button), so newlines ride a real Return keypress —
/// the same mapping the Linux backend applies in `keysym_for_char`
/// (`\n`/`\r` → `XK_RETURN`). Tab stays on the raw-unit path: edit controls
/// insert `WM_CHAR '\t'` as a literal tab rather than moving focus.
pub(crate) fn unicode_char_override(character: char) -> Option<WindowsKey> {
    match character {
        '\n' | '\r' => Some(WindowsKey {
            vk: vk::VK_RETURN,
            scancode: 0x1C,
            extended: false,
        }),
        _ => None,
    }
}

/// Modifier names accepted in `CODEX_GUI_MODIFIERS`, mapped to Windows
/// virtual keys (`command`→Win key — a documented divergence from the macOS
/// Command key, mirroring the Linux Super mapping;
/// `option`/`alt`→`VK_MENU`/Alt).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WinModifier {
    Shift,
    Control,
    Alt,
    Command,
}

pub(crate) fn parse_modifier_names(raw: &str) -> Vec<WinModifier> {
    // The helper presses modifiers in control → option → command → shift
    // order; keep that order stable and de-duplicate.
    let mut modifiers = Vec::new();
    for token in raw.split(',') {
        let modifier = match token.trim().to_ascii_lowercase().as_str() {
            "shift" => WinModifier::Shift,
            "control" | "ctrl" => WinModifier::Control,
            "option" | "alt" => WinModifier::Alt,
            "command" => WinModifier::Command,
            _ => continue,
        };
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    // Canonical press order.
    let order = |modifier: &WinModifier| match modifier {
        WinModifier::Control => 0,
        WinModifier::Alt => 1,
        WinModifier::Command => 2,
        WinModifier::Shift => 3,
    };
    modifiers.sort_by_key(order);
    modifiers
}

pub(crate) fn modifier_vk(modifier: WinModifier) -> u16 {
    match modifier {
        WinModifier::Shift => vk::VK_SHIFT,
        WinModifier::Control => vk::VK_CONTROL,
        WinModifier::Alt => vk::VK_MENU,
        WinModifier::Command => vk::VK_LWIN,
    }
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

/// One Windows wheel notch (`WHEEL_DELTA`).
pub(crate) const WHEEL_DELTA_NOTCH: i64 = 120;
/// Windows default wheel rate: one notch scrolls 3 lines, so 1 line ≈ 40
/// wheel-delta units.
pub(crate) const WHEEL_LINES_PER_NOTCH: i64 = 3;

/// One `SendInput` wheel event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WheelEvent {
    pub(crate) vertical: bool,
    pub(crate) delta: i32,
}

/// Total signed wheel delta for one axis. Line mode scales by the Windows
/// default 3-lines-per-notch rate; pixel mode maps 1px ≈ 1 delta unit (a
/// documented approximation — Windows has no exact pixel wheel unit).
pub(crate) fn wheel_axis_total(amount: i64, unit_is_pixel: bool) -> i64 {
    let scaled = if unit_is_pixel {
        amount.abs()
    } else {
        amount.abs() * (WHEEL_DELTA_NOTCH / WHEEL_LINES_PER_NOTCH)
    };
    scaled * amount.signum()
}

/// Chunk a signed wheel total into discrete events of at most one notch,
/// preserving the sign (last event carries the remainder).
pub(crate) fn wheel_axis_events(total: i64) -> Vec<i32> {
    let sign = total.signum();
    let mut magnitude = total.abs();
    let mut events = Vec::new();
    while magnitude > 0 {
        let step = magnitude.min(WHEEL_DELTA_NOTCH);
        events.push((sign * step) as i32);
        magnitude -= step;
    }
    events
}

/// Map signed scroll deltas to wheel events: Up=(0,+Y) → +vertical,
/// Down → −vertical, Left=(−X,0) → −horizontal, Right → +horizontal.
/// Vertical comes first, matching the macOS single-event wheel1/wheel2
/// ordering and the Linux `scroll_button_plan`.
pub(crate) fn wheel_delta_plan(
    scroll_x: i64,
    scroll_y: i64,
    unit_is_pixel: bool,
) -> Vec<WheelEvent> {
    let mut plan = Vec::new();
    if scroll_y != 0 {
        for delta in wheel_axis_events(wheel_axis_total(scroll_y, unit_is_pixel)) {
            plan.push(WheelEvent {
                vertical: true,
                delta,
            });
        }
    }
    if scroll_x != 0 {
        for delta in wheel_axis_events(wheel_axis_total(scroll_x, unit_is_pixel)) {
            plan.push(WheelEvent {
                vertical: false,
                delta,
            });
        }
    }
    plan
}

/// The Windows virtual screen: bounding rect of every monitor in physical
/// pixels (may start at negative coordinates with left/upper monitors).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VirtualScreen {
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) width: i64,
    pub(crate) height: i64,
}

impl VirtualScreen {
    fn right(&self) -> i64 {
        self.x + self.width
    }

    fn bottom(&self) -> i64 {
        self.y + self.height
    }
}

/// Normalize a physical point to `MOUSEEVENTF_ABSOLUTE | VIRTUALDESK`
/// coordinates (MSDN: `(px − origin) * 65535 / (extent − 1)`, rounded and
/// clamped to 0..=65535).
pub(crate) fn normalize_point_for_virtual_screen(
    x: f64,
    y: f64,
    screen: &VirtualScreen,
) -> (i32, i32) {
    (
        normalize_virtual_screen_axis(x, screen.x, screen.width),
        normalize_virtual_screen_axis(y, screen.y, screen.height),
    )
}

fn normalize_virtual_screen_axis(value: f64, origin: i64, extent: i64) -> i32 {
    let span = (extent - 1).max(1) as f64;
    let normalized = ((value - origin as f64) * 65535.0 / span).round();
    normalized.clamp(0.0, 65535.0) as i32
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

/// One monitor as the pure selection math sees it. Indices are 1-based like
/// the macOS helper's display list.
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
/// the DWM extended frame bounds in virtual-screen coordinates (the macOS
/// `CGWindowList` analog).
#[derive(Clone, Debug)]
pub(crate) struct WindowCandidate {
    pub(crate) id: i64,
    pub(crate) title: Option<String>,
    pub(crate) bounds: RectI,
    /// 0 = topmost in the Z order (EnumWindows enumerates top-down).
    pub(crate) stacking_index: usize,
}

/// macOS `rankWindows` translated to Windows: EnumWindows already walks the
/// Z order top-down, so topmost wins, then larger area, then titled windows,
/// then the HWND id for stability.
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

/// Minimum window edge for candidacy (macOS helper `MIN_WINDOW_EDGE`).
pub(crate) const MIN_WINDOW_EDGE: i64 = 80;

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

/// App identity match mirroring the macOS `matchesAppName` semantics on the
/// Windows identity set: case-insensitive equality on the process image-name
/// stem; equality or containment on the window title.
pub(crate) fn app_matches_window(
    requested: &str,
    process_name: Option<&str>,
    title: Option<&str>,
) -> bool {
    let normalized = requested.trim().to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    if let Some(process_name) = process_name
        && process_name.trim().to_lowercase() == normalized
    {
        return true;
    }
    if let Some(title) = title {
        let title = title.trim().to_lowercase();
        if title == normalized || title.contains(&normalized) {
            return true;
        }
    }
    false
}

/// Clamp a capture rect (global virtual-screen coordinates) to the virtual
/// screen, mirroring the Linux `capture_bounds_pixels`.
pub(crate) fn clamp_capture_rect(
    bounds: &HelperRect,
    screen: &VirtualScreen,
) -> Result<(i64, i64, i64, i64), ToolError> {
    if !bounds.x.is_finite()
        || !bounds.y.is_finite()
        || !bounds.width.is_finite()
        || !bounds.height.is_finite()
    {
        return Err(respond_to_model(
            "capture region contains non-finite coordinates",
        ));
    }
    let left = bounds.x.round();
    let top = bounds.y.round();
    // Float→int casts saturate, so out-of-range bounds clamp here.
    let x = (left as i64).max(screen.x);
    let y = (top as i64).max(screen.y);
    let right = ((left + bounds.width.round()) as i64).min(screen.right());
    let bottom = ((top + bounds.height.round()) as i64).min(screen.bottom());
    let width = right - x;
    let height = bottom - y;
    if width <= 0 || height <= 0 {
        return Err(respond_to_model(
            "capture region is empty after clamping to the virtual screen bounds",
        ));
    }
    Ok((x, y, width, height))
}

/// In-place BGRA→RGBA swap over a 32bpp `GetDIBits` buffer. Screen captures
/// leave the alpha byte undefined, so it is forced opaque.
pub(crate) fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = bgra.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    rgba
}

/// Encode raw RGBA pixels as PNG, resizing with a Triangle filter only when
/// an explicit non-zero target size differs (mirrors the Linux backend).
pub(crate) fn encode_png(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, ToolError> {
    let mut image = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
        respond_to_model("capture byte length does not match the image dimensions")
    })?;
    if target_width > 0 && target_height > 0 && (target_width != width || target_height != height) {
        image = image::imageops::resize(
            &image,
            target_width,
            target_height,
            image::imageops::FilterType::Triangle,
        );
    }
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| {
            respond_to_model(format!(
                "failed to encode the Windows capture as PNG: {error}"
            ))
        })?;
    Ok(png)
}

/// Cheap host probes behind the readiness matrix. Memoized process-globally:
/// non-`ready` snapshots (e.g. the usual UIPI Warn) are re-probed on every
/// tool call, so every probe here must stay cheap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProbeSnapshot {
    pub(crate) interactive_desktop: bool,
    pub(crate) capture_available: bool,
    pub(crate) hook_installable: bool,
    pub(crate) elevated: bool,
}

const UIPI_WARN_SUMMARY: &str = "Elevated (Administrator) target apps cannot receive synthetic input from a non-elevated process (UIPI); run elevated to automate elevated windows.";
const UIPI_WARN_DETAIL: &str = "Windows delivers synthetic SendInput events only to processes at or below the caller's integrity level. Automate non-elevated windows, or run this agent from an elevated terminal to drive elevated windows.";
const EMERGENCY_STOP_WARN_SUMMARY: &str = "The WH_KEYBOARD_LL emergency-stop hook could not be installed; pressing Escape will not stop GUI actions mid-flight.";
const EMERGENCY_STOP_WARN_DETAIL: &str = "Windows may refuse low-level keyboard hooks when the desktop is not interactive or a policy restricts hooks. GUI actions still run; the Escape emergency stop is simply inactive.";
const FOREGROUND_LOCK_DETAIL: &str = "Activation via SetForegroundWindow is subject to the Windows foreground lock: it can be declined unless this process received the last input or is already foreground. Activation is verified with GetForegroundWindow and surfaced as an error instead of retried blindly.";

/// Build the readiness check list from the probe results (pure; mirrors the
/// Linux ladder: desktop failure pairs `platform` + `native_helper` Errors,
/// success emits the four matrix ids plus Windows-specific Warn notes).
pub(crate) fn readiness_checks_from_probes(
    probes: &ProbeSnapshot,
) -> Vec<GuiEnvironmentReadinessCheck> {
    if !probes.interactive_desktop {
        return vec![
            GuiEnvironmentReadinessCheck {
                id: "platform",
                label: "Platform",
                status: GuiReadinessStatus::Error,
                summary: "The Windows GUI runtime requires an interactive desktop session, which this process does not have."
                    .to_string(),
                detail: Some(
                    "Native GUI tools must run inside a logged-in desktop session; service (session 0) contexts are unsupported."
                        .to_string(),
                ),
            },
            GuiEnvironmentReadinessCheck {
                id: "native_helper",
                label: "Native GUI Helper",
                status: GuiReadinessStatus::Error,
                summary: "In-process Win32 backend could not attach to the interactive desktop."
                    .to_string(),
                detail: None,
            },
        ];
    }
    let mut checks = vec![
        GuiEnvironmentReadinessCheck {
            id: "platform",
            label: "Platform",
            status: GuiReadinessStatus::Ok,
            summary: "Windows GUI runtime is available on this host.".to_string(),
            detail: Some(FOREGROUND_LOCK_DETAIL.to_string()),
        },
        GuiEnvironmentReadinessCheck {
            id: "native_helper",
            label: "Native GUI Helper",
            status: GuiReadinessStatus::Ok,
            summary: "In-process Win32 backend - no helper binary required.".to_string(),
            detail: Some(
                "windows-sys SendInput, GDI BitBlt/GetDIBits, EnumWindows/DWM frame bounds."
                    .to_string(),
            ),
        },
        GuiEnvironmentReadinessCheck {
            id: "accessibility",
            label: "Accessibility",
            status: GuiReadinessStatus::Ok,
            summary: "SendInput is available for native GUI input.".to_string(),
            detail: Some(
                "Windows has no accessibility permission model; elevated (Administrator) targets are the exception - see the UIPI check."
                    .to_string(),
            ),
        },
        if probes.capture_available {
            GuiEnvironmentReadinessCheck {
                id: "screen_recording",
                label: "Screen Recording",
                status: GuiReadinessStatus::Ok,
                summary: "Screen capture is available via GDI BitBlt.".to_string(),
                detail: None,
            }
        } else {
            GuiEnvironmentReadinessCheck {
                id: "screen_recording",
                label: "Screen Recording",
                status: GuiReadinessStatus::Error,
                summary: "GDI BitBlt capture failed; screenshot-based GUI actions are unavailable."
                    .to_string(),
                detail: None,
            }
        },
    ];
    if !probes.elevated {
        checks.push(GuiEnvironmentReadinessCheck {
            id: "uipi",
            label: "UIPI",
            status: GuiReadinessStatus::Warn,
            summary: UIPI_WARN_SUMMARY.to_string(),
            detail: Some(UIPI_WARN_DETAIL.to_string()),
        });
    }
    if !probes.hook_installable {
        checks.push(GuiEnvironmentReadinessCheck {
            id: "emergency_stop",
            label: "Emergency Stop",
            status: GuiReadinessStatus::Warn,
            summary: EMERGENCY_STOP_WARN_SUMMARY.to_string(),
            detail: Some(EMERGENCY_STOP_WARN_DETAIL.to_string()),
        });
    }
    checks
}

/// Capability matrix from the probe results (pure; mirrors the Linux
/// mapping: capture failure disables screenshot-gated tools and restricts
/// scroll/type to targetless usage; no desktop disables everything).
pub(crate) fn capabilities_from_probes(
    probes: &ProbeSnapshot,
) -> HashMap<&'static str, GuiToolCapability> {
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
    if !probes.interactive_desktop {
        for tool_name in GUI_TOOL_NAMES {
            capabilities.insert(
                tool_name,
                GuiToolCapability {
                    enabled: false,
                    reason: Some(
                        "The Windows interactive desktop session is not available, so native GUI tools cannot run. Run the agent inside a logged-in desktop session (service/session 0 contexts are unsupported)."
                            .to_string(),
                    ),
                    targetless_only: false,
                },
            );
        }
        return capabilities;
    }
    if !probes.capture_available {
        for tool_name in ["gui_observe", "gui_click", "gui_drag", "gui_wait"] {
            capabilities.insert(
                tool_name,
                GuiToolCapability {
                    enabled: false,
                    reason: Some(
                        "GDI BitBlt capture is unavailable, so screenshot-based GUI actions are unavailable."
                            .to_string(),
                    ),
                    targetless_only: false,
                },
            );
        }
        for tool_name in ["gui_scroll", "gui_type"] {
            if let Some(capability) = capabilities.get_mut(tool_name) {
                capability.reason = Some(
                    "GDI BitBlt capture is unavailable, so this tool only supports targetless usage (omit the `target` parameter)."
                        .to_string(),
                );
                capability.targetless_only = true;
            }
        }
    }
    capabilities
}

pub(crate) struct WindowsPlatform;

#[cfg(target_os = "windows")]
mod win32 {
    use std::ffi::c_void;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits,
        GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO, ReleaseDC, SRCCOPY,
        SelectObject,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_WHEEL, MOUSEINPUT, MapVirtualKeyW, SendInput,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, EnumWindows, GWL_EXSTYLE, GetCursorPos,
        GetForegroundWindow, GetMessageW, GetShellWindow, GetSystemMetrics, GetWindowLongW,
        GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible, KBDLLHOOKSTRUCT, LLKHF_UP, MSG, PostThreadMessageW, SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_MINIMIZE, SW_RESTORE,
        SetForegroundWindow, SetWindowsHookExW, ShowWindow, TranslateMessage, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WS_EX_TOOLWINDOW,
    };

    use super::*;

    // Compile-time parity between the host-independent tables and the
    // windows-sys constants the FFI layer actually sends.
    const _: () = {
        assert!(super::vk::VK_RETURN == windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_RETURN);
        assert!(super::vk::VK_BACK == windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_BACK);
        assert!(super::vk::VK_ESCAPE == windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE);
        assert!(super::vk::VK_LWIN == windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LWIN);
        assert!(
            super::WHEEL_DELTA_NOTCH
                == windows_sys::Win32::UI::WindowsAndMessaging::WHEEL_DELTA as i64
        );
    };

    // -- Process-wide setup -------------------------------------------------

    /// Per-monitor-v2 DPI awareness, set once per process. Process-wide by
    /// design: without it `GetSystemMetrics(SM_*VIRTUALSCREEN)`, SendInput
    /// normalization, DWM frame bounds, and BitBlt output live in different
    /// coordinate spaces (DPI-virtualized), breaking the scale-1.0 capture
    /// invariant and click targeting. Best-effort: a manifest that already
    /// sets awareness (or an earlier call) makes this a no-op.
    static DPI_AWARENESS_SET: OnceLock<()> = OnceLock::new();

    fn ensure_dpi_awareness() {
        DPI_AWARENESS_SET.get_or_init(|| unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        });
    }

    fn virtual_screen() -> VirtualScreen {
        unsafe {
            VirtualScreen {
                x: i64::from(GetSystemMetrics(SM_XVIRTUALSCREEN)),
                y: i64::from(GetSystemMetrics(SM_YVIRTUALSCREEN)),
                width: i64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN)),
                height: i64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN)),
            }
        }
    }

    // -- Input injection (SendInput only) ------------------------------------

    fn send_inputs(inputs: &[INPUT]) -> Result<(), ToolError> {
        if inputs.is_empty() {
            return Ok(());
        }
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if sent != inputs.len() as u32 {
            let error = unsafe { GetLastError() };
            return Err(respond_to_model(format!(
                "SendInput injected {sent} of {} input events (Win32 error {error}); an elevated target window may be blocking synthetic input (UIPI).",
                inputs.len()
            )));
        }
        Ok(())
    }

    fn mouse_input(dw_flags: u32, mouse_data: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: mouse_data,
                    dwFlags: dw_flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn move_mouse_input(x: f64, y: f64, screen: &VirtualScreen) -> INPUT {
        let (dx, dy) = normalize_point_for_virtual_screen(x, y, screen);
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key_input(key: WindowsKey, up: bool) -> INPUT {
        let scancode = if key.scancode != 0 {
            key.scancode
        } else {
            unsafe { MapVirtualKeyW(u32::from(key.vk), MAPVK_VK_TO_VSC) as u16 }
        };
        let mut dw_flags = 0u32;
        if key.extended {
            dw_flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if up {
            dw_flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key.vk,
                    wScan: scancode,
                    dwFlags: dw_flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn unicode_input(unit: u16, up: bool) -> INPUT {
        let mut dw_flags = KEYEVENTF_UNICODE;
        if up {
            dw_flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: dw_flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn wheel_input(vertical: bool, delta: i32) -> INPUT {
        mouse_input(
            if vertical {
                MOUSEEVENTF_WHEEL
            } else {
                MOUSEEVENTF_HWHEEL
            },
            delta as u32,
        )
    }

    /// Press modifiers in control → alt → command → shift order, tap the
    /// key, release in reverse (mirrors the macOS helper `pressKeyCode` and
    /// the Linux `press_keycode_with_modifiers`).
    fn tap_key(key: WindowsKey, modifiers: &[WinModifier]) -> Result<(), ToolError> {
        for modifier in modifiers {
            send_inputs(&[key_input(
                WindowsKey {
                    vk: modifier_vk(*modifier),
                    scancode: 0,
                    extended: *modifier == WinModifier::Command,
                },
                false,
            )])?;
            std::thread::sleep(Duration::from_millis(KEY_MILLIS));
        }
        send_inputs(&[key_input(key, false)])?;
        std::thread::sleep(Duration::from_millis(KEY_MILLIS));
        send_inputs(&[key_input(key, true)])?;
        for modifier in modifiers.iter().rev() {
            std::thread::sleep(Duration::from_millis(KEY_MILLIS));
            send_inputs(&[key_input(
                WindowsKey {
                    vk: modifier_vk(*modifier),
                    scancode: 0,
                    extended: *modifier == WinModifier::Command,
                },
                true,
            )])?;
        }
        std::thread::sleep(Duration::from_millis(KEY_MILLIS));
        Ok(())
    }

    fn move_mouse(x: f64, y: f64) -> Result<(), ToolError> {
        ensure_dpi_awareness();
        let screen = virtual_screen();
        send_inputs(&[move_mouse_input(x, y, &screen)])
    }

    /// Type one character through `KEYEVENTF_UNICODE` (one event pair per
    /// UTF-16 code unit, so astral-plane characters span two pairs).
    /// Newlines are the exception (see `unicode_char_override`): a raw LF
    /// unit arrives as `WM_CHAR '\n'`, which edit controls ignore, so they
    /// tap a real Return key instead.
    fn type_char_unicode(character: char) -> Result<(), ToolError> {
        if let Some(key) = unicode_char_override(character) {
            return tap_key(key, &[]);
        }
        let mut units = [0u16; 2];
        for &unit in character.encode_utf16(&mut units).iter() {
            send_inputs(&[unicode_input(unit, false)])?;
            std::thread::sleep(Duration::from_millis(KEY_MILLIS));
            send_inputs(&[unicode_input(unit, true)])?;
        }
        Ok(())
    }

    fn type_char_physical(character: char) -> Result<(), ToolError> {
        let Some((key, needs_shift)) = char_key(character) else {
            return Err(respond_to_model(format!(
                "the Windows backend cannot type the character `{character}` with the `physical_keys` strategy (no VK mapping); use the `clipboard_paste` or `unicode` strategy instead."
            )));
        };
        if needs_shift {
            tap_key(key, &[WinModifier::Shift])
        } else {
            tap_key(key, &[])
        }
    }

    /// Clipboard paste: stage `text` with arboard, then inject Ctrl+V.
    ///
    /// Clipboard ownership, honestly: the Windows clipboard is a
    /// process-global cooperative buffer, so this *overwrites* whatever the
    /// user had copied and it is **not restored afterwards** (the same
    /// documented divergence as the X11 backend's CLIPBOARD ownership).
    /// Other applications can race the clipboard between `set_text` and the
    /// injected Ctrl+V. The `Clipboard` handle is kept alive through the
    /// post-delay so the staged data remains owned by this process while the
    /// paste lands.
    fn paste_text(text: &str, pre_delay_ms: u64, post_delay_ms: u64) -> Result<(), ToolError> {
        let mut clipboard = arboard::Clipboard::new().map_err(|error| {
            respond_to_model(format!("failed to open the Windows clipboard: {error}"))
        })?;
        clipboard.set_text(text.to_string()).map_err(|error| {
            respond_to_model(format!(
                "failed to stage the paste text on the Windows clipboard: {error}"
            ))
        })?;
        std::thread::sleep(Duration::from_millis(pre_delay_ms));
        tap_key(
            WindowsKey {
                vk: vk::VK_V,
                scancode: 0x2F,
                extended: false,
            },
            &[WinModifier::Control],
        )?;
        std::thread::sleep(Duration::from_millis(post_delay_ms));
        drop(clipboard);
        Ok(())
    }

    // -- Screen capture (GDI BitBlt + GetDIBits) ------------------------------

    struct CaptureResources {
        screen_dc: HDC,
        mem_dc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        bitmap_selected: bool,
    }

    impl Drop for CaptureResources {
        fn drop(&mut self) {
            unsafe {
                if self.bitmap_selected {
                    SelectObject(self.mem_dc, self.previous);
                }
                DeleteObject(self.bitmap);
                DeleteDC(self.mem_dc);
                ReleaseDC(std::ptr::null_mut(), self.screen_dc);
            }
        }
    }

    /// BitBlt a virtual-screen region into a 32bpp top-down DIB and return
    /// the pixels as RGBA. `width`/`height` are the physical pixel dims of
    /// the capture; callers report them as the capture dims so the
    /// grounding scale stays 1.0.
    fn capture_rgba(
        x: i64,
        y: i64,
        width: i64,
        height: i64,
    ) -> Result<(u32, u32, Vec<u8>), ToolError> {
        let (Ok(x), Ok(y), Ok(width), Ok(height)) = (
            i32::try_from(x),
            i32::try_from(y),
            i32::try_from(width),
            i32::try_from(height),
        ) else {
            return Err(respond_to_model(
                "capture region does not fit the Win32 coordinate space",
            ));
        };
        if width <= 0 || height <= 0 {
            return Err(respond_to_model(
                "capture region has non-positive dimensions",
            ));
        }
        ensure_dpi_awareness();
        unsafe {
            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err(respond_to_model(
                    "failed to acquire a screen device context for capture",
                ));
            }
            let mem_dc = CreateCompatibleDC(screen_dc);
            if mem_dc.is_null() {
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err(respond_to_model(
                    "failed to create a compatible device context for capture",
                ));
            }
            let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
            if bitmap.is_null() {
                DeleteDC(mem_dc);
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err(respond_to_model(
                    "failed to create a compatible bitmap for capture",
                ));
            }
            let mut resources = CaptureResources {
                screen_dc,
                mem_dc,
                bitmap,
                previous: std::ptr::null_mut(),
                bitmap_selected: false,
            };
            resources.previous = SelectObject(resources.mem_dc, resources.bitmap);
            resources.bitmap_selected = true;
            if BitBlt(
                resources.mem_dc,
                0,
                0,
                width,
                height,
                screen_dc,
                x,
                y,
                SRCCOPY,
            ) == 0
            {
                return Err(respond_to_model(
                    "GDI BitBlt capture of the screen region failed",
                ));
            }
            // GetDIBits refuses a bitmap that is selected into a DC (MSDN).
            SelectObject(resources.mem_dc, resources.previous);
            resources.bitmap_selected = false;
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    // Negative height: top-down rows, matching the grounding
                    // convention (origin = top-left).
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let copied = GetDIBits(
                screen_dc,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr() as *mut c_void,
                &mut info,
                DIB_RGB_COLORS,
            );
            if copied == 0 {
                return Err(respond_to_model(
                    "GDI GetDIBits could not read the captured screen pixels",
                ));
            }
            Ok((width as u32, height as u32, bgra_to_rgba(&pixels)))
        }
    }

    // -- Window enumeration ---------------------------------------------------

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
        let sink = unsafe { &mut *(lparam as *mut Vec<HWND>) };
        sink.push(hwnd);
        1
    }

    fn enum_top_level_windows() -> Vec<HWND> {
        let mut windows: Vec<HWND> = Vec::new();
        unsafe {
            EnumWindows(
                Some(enum_windows_proc),
                &mut windows as *mut Vec<HWND> as LPARAM,
            );
        }
        windows
    }

    fn window_text(hwnd: HWND) -> Option<String> {
        unsafe {
            let length = GetWindowTextLengthW(hwnd);
            if length <= 0 {
                return None;
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), length + 1);
            if copied <= 0 {
                return None;
            }
            let text = String::from_utf16_lossy(&buffer[..copied as usize])
                .trim()
                .to_string();
            (!text.is_empty()).then_some(text)
        }
    }

    fn hwnd_pid(hwnd: HWND) -> Option<i32> {
        let mut pid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        i32::try_from(pid).ok().filter(|pid| *pid > 0)
    }

    /// Process image-name stem for a PID ("Code" for Code.exe) — the Windows
    /// app-identity analog of WM_CLASS/`/proc/<pid>/comm`.
    fn process_name_for_pid(pid: i32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                u32::try_from(pid).ok()?,
            );
            if handle.is_null() {
                return None;
            }
            let mut buffer = [0u16; 1024];
            let mut size = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &mut size,
            );
            let _ = CloseHandle(handle);
            if ok == 0 || size == 0 {
                return None;
            }
            let path = String::from_utf16_lossy(&buffer[..size as usize]);
            let file_name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
            let stem = match file_name.rsplit_once('.') {
                Some((stem, extension)) if extension.eq_ignore_ascii_case("exe") => stem,
                _ => file_name,
            };
            (!stem.is_empty()).then_some(stem.to_string())
        }
    }

    /// DWM extended frame bounds (excluding the invisible resize borders) —
    /// the Windows analog of the macOS/X11 window-manager frame rect — with
    /// a `GetWindowRect` fallback.
    fn window_frame_rect(hwnd: HWND) -> Result<RectI, ToolError> {
        unsafe {
            let mut rect = RECT::default();
            let from_dwm = DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut rect as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            ) == 0;
            if !from_dwm && GetWindowRect(hwnd, &mut rect) == 0 {
                return Err(respond_to_model(
                    "failed to read the bounds of a Windows window",
                ));
            }
            let width = i64::from(rect.right - rect.left);
            let height = i64::from(rect.bottom - rect.top);
            if width <= 0 || height <= 0 {
                return Err(respond_to_model(
                    "failed to read the bounds of a Windows window",
                ));
            }
            Ok(RectI {
                x: i64::from(rect.left),
                y: i64::from(rect.top),
                width,
                height,
            })
        }
    }

    fn is_tool_window(hwnd: HWND) -> bool {
        unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) & (WS_EX_TOOLWINDOW as i32) != 0 }
    }

    fn is_cloaked(hwnd: HWND) -> bool {
        let mut cloaked: u32 = 0;
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED as u32,
                &mut cloaked as *mut u32 as *mut c_void,
                std::mem::size_of::<u32>() as u32,
            ) == 0
                && cloaked != 0
        }
    }

    /// Visible, enumerable top-level windows with their identity, topmost
    /// first (EnumWindows walks the Z order top-down).
    fn visible_window_records() -> Result<Vec<WindowRecord>, ToolError> {
        let mut records = Vec::new();
        for (stacking_index, hwnd) in enum_top_level_windows().into_iter().enumerate() {
            if unsafe { IsWindowVisible(hwnd) } == 0 || is_tool_window(hwnd) || is_cloaked(hwnd) {
                continue;
            }
            let pid = hwnd_pid(hwnd);
            let title = window_text(hwnd);
            let bounds = window_frame_rect(hwnd)?;
            records.push(WindowRecord {
                hwnd,
                stacking_index,
                pid,
                process_name: pid.and_then(process_name_for_pid),
                title,
                bounds,
            });
        }
        Ok(records)
    }

    #[derive(Clone)]
    struct WindowRecord {
        hwnd: HWND,
        stacking_index: usize,
        pid: Option<i32>,
        process_name: Option<String>,
        title: Option<String>,
        bounds: RectI,
    }

    impl WindowRecord {
        fn matches_app(&self, requested: &str) -> bool {
            app_matches_window(
                requested,
                self.process_name.as_deref(),
                self.title.as_deref(),
            )
        }

        /// Identity used to group an app's windows (the macOS bundle-id /
        /// localizedName analog).
        fn identity_key(&self) -> String {
            self.process_name
                .clone()
                .unwrap_or_else(|| format!("window-{}", self.hwnd as usize))
        }

        fn app_display_name(&self) -> String {
            self.identity_key()
        }
    }

    fn foreground_hwnd() -> Option<HWND> {
        let hwnd = unsafe { GetForegroundWindow() };
        (!hwnd.is_null()).then_some(hwnd)
    }

    fn foreground_window_record() -> Option<WindowRecord> {
        let hwnd = foreground_hwnd()?;
        let pid = hwnd_pid(hwnd);
        let bounds = window_frame_rect(hwnd).ok()?;
        Some(WindowRecord {
            hwnd,
            stacking_index: 0,
            pid,
            process_name: pid.and_then(process_name_for_pid),
            title: window_text(hwnd),
            bounds,
        })
    }

    /// Owner of the shell window (taskbar/desktop) — never hidden.
    fn shell_window_pid() -> Option<i32> {
        let hwnd = unsafe { GetShellWindow() };
        if hwnd.is_null() {
            return None;
        }
        hwnd_pid(hwnd)
    }

    unsafe extern "system" fn enum_monitors_proc(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> i32 {
        let sink = unsafe { &mut *(lparam as *mut Vec<RectI>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
            let rect = info.rcMonitor;
            sink.push(RectI {
                x: i64::from(rect.left),
                y: i64::from(rect.top),
                width: i64::from(rect.right - rect.left),
                height: i64::from(rect.bottom - rect.top),
            });
        }
        1
    }

    /// Monitors sorted by (y, x) with 1-based indices, falling back to the
    /// virtual screen as a single display (mirrors the Linux RANDR path).
    fn displays() -> Vec<DisplayInfo> {
        let mut monitor_bounds: Vec<RectI> = Vec::new();
        unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                Some(enum_monitors_proc),
                &mut monitor_bounds as *mut Vec<RectI> as LPARAM,
            );
        }
        if monitor_bounds.is_empty() {
            let screen = virtual_screen();
            return vec![DisplayInfo {
                index: 1,
                bounds: RectI {
                    x: screen.x,
                    y: screen.y,
                    width: screen.width,
                    height: screen.height,
                },
            }];
        }
        monitor_bounds.sort_by_key(|bounds| (bounds.y, bounds.x));
        monitor_bounds
            .into_iter()
            .enumerate()
            .map(|(index, bounds)| DisplayInfo {
                index: index as i64 + 1,
                bounds,
            })
            .collect()
    }

    // -- App/window resolution -------------------------------------------------

    /// Resolve the target app + window the way the macOS helper's
    /// `capture-context` does. `app = None` targets the frontmost app (the
    /// foreground window's owning process, all its windows).
    fn resolve_target(
        app: Option<&str>,
        window_selection: Option<&WindowSelector>,
    ) -> Result<(Option<String>, Option<WindowSelectionOutcome>), ToolError> {
        let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
        let records = visible_window_records()?;
        let mut target_records: Vec<WindowRecord> = match requested_app {
            Some(requested_app) => records
                .iter()
                .filter(|record| record.matches_app(requested_app))
                .cloned()
                .collect(),
            None => {
                // Frontmost app: the process owning the foreground window.
                match foreground_window_record() {
                    Some(foreground) => {
                        let identity = foreground.identity_key();
                        records
                            .iter()
                            .filter(|record| record.identity_key() == identity)
                            .cloned()
                            .collect()
                    }
                    None => records,
                }
            }
        };
        target_records.sort_by_key(|record| record.stacking_index);

        let app_name = target_records
            .first()
            .map(WindowRecord::app_display_name)
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
        let has_explicit_selection = title.is_some() || title_contains.is_some() || index.is_some();

        let filtered: Vec<WindowRecord> = target_records
            .iter()
            .filter(|record| {
                title.is_none_or(|title| {
                    record
                        .title
                        .as_deref()
                        .is_some_and(|candidate| normalized(candidate) == normalized(title))
                }) && title_contains.is_none_or(|title_contains| {
                    record.title.as_deref().is_some_and(|candidate| {
                        normalized(candidate).contains(&normalized(title_contains))
                    })
                })
            })
            .cloned()
            .collect();

        let ranked = rank_windows(
            filtered
                .iter()
                .filter(|record| {
                    record.bounds.width >= MIN_WINDOW_EDGE
                        && record.bounds.height >= MIN_WINDOW_EDGE
                })
                .map(|record| WindowCandidate {
                    id: record.hwnd as usize as i64,
                    title: record.title.clone(),
                    bounds: record.bounds,
                    stacking_index: record.stacking_index,
                })
                .collect(),
        );
        let selection = select_window(&ranked, index, has_explicit_selection);
        Ok((app_name, selection))
    }

    fn normalized(value: &str) -> String {
        value.trim().to_lowercase()
    }

    /// Foreground activation with the foreground-lock caveat surfaced:
    /// verify with GetForegroundWindow after the settle delay instead of
    /// retrying blindly (mirrors the macOS skip-if-focused behavior).
    fn activate_app(app: &str) -> Result<(), ToolError> {
        let records = visible_window_records()?;
        let Some(target) = records.iter().find(|record| record.matches_app(app)) else {
            return Err(respond_to_model(format!(
                "The application `{app}` could not be found on the Windows desktop."
            )));
        };
        // Already focused? (macOS skips re-activation too.) The process, not
        // the exact window, is the identity: an app may raise a sibling
        // window in response.
        if foreground_hwnd().and_then(hwnd_pid) == target.pid {
            return Ok(());
        }
        unsafe {
            SetForegroundWindow(target.hwnd);
        }
        std::thread::sleep(Duration::from_millis(ACTIVATE_SETTLE_MS));
        let granted = foreground_hwnd().and_then(hwnd_pid) == target.pid;
        if !granted {
            return Err(respond_to_model(format!(
                "failed to activate `{app}`: Windows did not grant the foreground switch (SetForegroundWindow foreground lock); focus the target manually and retry."
            )));
        }
        Ok(())
    }

    // -- Escape (emergency-stop) monitor ---------------------------------------

    struct ActiveHookMonitor {
        triggered: std::sync::Arc<AtomicBool>,
        thread_id: u32,
    }

    static ACTIVE_HOOK_MONITOR: OnceLock<Mutex<Option<ActiveHookMonitor>>> = OnceLock::new();

    /// Signal the active escape monitor to shut down: `triggered` flips (so
    /// a session checking the flag sees a stop) and `WM_QUIT` wakes the
    /// message pump, whose LL callbacks only flow while it pumps.
    fn signal_active_hook_monitor() {
        let cell = ACTIVE_HOOK_MONITOR.get_or_init(|| Mutex::new(None));
        let mut guard = match cell.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = guard.take() {
            previous.triggered.store(true, Ordering::SeqCst);
            unsafe {
                PostThreadMessageW(previous.thread_id, WM_QUIT, 0, 0);
            }
        }
    }

    /// Process-global single-active-monitor registration: signal the
    /// previous monitor first, then take the slot (mirrors the Linux
    /// `register_escape_monitor`).
    fn register_hook_monitor(triggered: &std::sync::Arc<AtomicBool>, thread_id: u32) {
        signal_active_hook_monitor();
        let cell = ACTIVE_HOOK_MONITOR.get_or_init(|| Mutex::new(None));
        let mut guard = match cell.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(ActiveHookMonitor {
            triggered: std::sync::Arc::clone(triggered),
            thread_id,
        });
    }

    /// Remove the registry entry when it still points at this thread (a
    /// replacing monitor already swapped it). Prevents `WM_QUIT` from later
    /// teardown landing on a recycled thread id.
    fn unregister_hook_monitor(thread_id: u32) {
        if let Some(cell) = ACTIVE_HOOK_MONITOR.get() {
            let mut guard = match cell.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if guard
                .as_ref()
                .is_some_and(|monitor| monitor.thread_id == thread_id)
            {
                *guard = None;
            }
        }
    }

    struct HookFlags {
        triggered: std::sync::Arc<AtomicBool>,
        suppress_next: std::sync::Arc<AtomicBool>,
        thread_id: u32,
    }

    thread_local! {
        static HOOK_FLAGS: std::cell::RefCell<Option<HookFlags>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn escape_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Hand to the system first so the callback stays as fast as possible
        // (Windows silently unhooks slow low-level hooks).
        let result = unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
        if code >= 0 && (wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN) {
            let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if info.vkCode == u32::from(vk::VK_ESCAPE) && (info.flags & LLKHF_UP) == 0 {
                // Injected SendInput events traverse this hook too, which is
                // exactly why an armed `suppress_next` is consumed here
                // (parity with the X11 RECORD monitor seeing XTEST events).
                HOOK_FLAGS.with(|cell| {
                    // try_borrow (never expect): the hook can re-enter the
                    // thread while a borrow is live.
                    let Ok(slot) = cell.try_borrow() else {
                        return;
                    };
                    let Some(flags) = slot.as_ref() else {
                        return;
                    };
                    if flags
                        .suppress_next
                        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        // Expected programmatic Escape; keep monitoring.
                    } else {
                        flags.triggered.store(true, Ordering::SeqCst);
                        unsafe {
                            PostThreadMessageW(flags.thread_id, WM_QUIT, 0, 0);
                        }
                    }
                });
            }
        }
        result
    }

    /// Hook-thread body: install a `WH_KEYBOARD_LL` hook, register in the
    /// process-global monitor slot, pump messages until `WM_QUIT`, then
    /// unhook and leave.
    fn run_hook_thread(
        triggered: std::sync::Arc<AtomicBool>,
        suppress_next: std::sync::Arc<AtomicBool>,
        installed_tx: std::sync::mpsc::Sender<bool>,
    ) {
        let thread_id = unsafe { GetCurrentThreadId() };
        let hook = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(escape_hook_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if hook.is_null() {
            let _ = installed_tx.send(false);
            return;
        }
        HOOK_FLAGS.with(|cell| {
            if let Ok(mut slot) = cell.try_borrow_mut() {
                *slot = Some(HookFlags {
                    triggered: std::sync::Arc::clone(&triggered),
                    suppress_next: std::sync::Arc::clone(&suppress_next),
                    thread_id,
                });
            }
        });
        register_hook_monitor(&triggered, thread_id);
        let _ = installed_tx.send(true);

        // Low-level keyboard callbacks are only delivered while the
        // installing thread pumps messages; GetMessageW returns 0 on
        // WM_QUIT.
        unsafe {
            let mut message = MSG::default();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }
            let _ = UnhookWindowsHookEx(hook);
        }
        unregister_hook_monitor(thread_id);
    }

    // -- Hidden-window registry (per-PID restore keys) -------------------------

    static HIDDEN_WINDOWS: OnceLock<Mutex<HashMap<i32, Vec<isize>>>> = OnceLock::new();

    // -- Host probes -------------------------------------------------------------

    static PROBES: OnceLock<ProbeSnapshot> = OnceLock::new();

    fn probe_snapshot() -> ProbeSnapshot {
        *PROBES.get_or_init(|| {
            ensure_dpi_awareness();
            let interactive_desktop = unsafe { !GetShellWindow().is_null() };
            let capture_available = interactive_desktop && {
                let screen = virtual_screen();
                capture_rgba(screen.x, screen.y, 1, 1)
                    .map(|(_, _, pixels)| pixels.len() == 4)
                    .unwrap_or(false)
            };
            let hook_installable = interactive_desktop && ll_hook_probe();
            let elevated = interactive_desktop && process_is_elevated();
            ProbeSnapshot {
                interactive_desktop,
                capture_available,
                hook_installable,
                elevated,
            }
        })
    }

    /// Cheap 1×1 BitBlt probe on the virtual screen (the direct analog of
    /// the Linux 1×1 GetImage probe).
    fn ll_hook_probe() -> bool {
        unsafe extern "system" fn probe_hook_proc(
            code: i32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
        }
        unsafe {
            let hook = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(probe_hook_proc),
                std::ptr::null_mut(),
                0,
            );
            if hook.is_null() {
                return false;
            }
            let _ = UnhookWindowsHookEx(hook);
            true
        }
    }

    /// TokenElevation probe: false when the token cannot be read, so the
    /// UIPI warning is shown conservatively.
    fn process_is_elevated() -> bool {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut returned: u32 = 0;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut TOKEN_ELEVATION as *mut c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );
            let _ = CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }

    // -- GuiPlatform -------------------------------------------------------------

    impl GuiPlatform for WindowsPlatform {
        fn readiness_snapshot(&self) -> GuiEnvironmentReadinessSnapshot {
            let probes = probe_snapshot();
            let checks = readiness_checks_from_probes(&probes);
            let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
            GuiEnvironmentReadinessSnapshot {
                status: readiness_status_ladder(&statuses),
                checks,
            }
        }

        fn tool_capabilities(&self) -> HashMap<&'static str, GuiToolCapability> {
            capabilities_from_probes(&probe_snapshot())
        }

        fn resolve_helper_binary(&self) -> Result<PathBuf, ToolError> {
            Err(respond_to_model(
                "The Windows GUI backend is in-process (Win32/windows-sys); no helper binary is used.",
            ))
        }

        fn cleanup_input_state(&self) -> Result<(), ToolError> {
            // Called from GuiActionSession::drop after monitor.stop() —
            // best-effort only. The monitor thread is detached (`stop()`
            // holds no handle), so signal its pump here at teardown.
            signal_active_hook_monitor();
            let release = [
                mouse_input(MOUSEEVENTF_LEFTUP, 0),
                mouse_input(MOUSEEVENTF_RIGHTUP, 0),
                mouse_input(MOUSEEVENTF_MIDDLEUP, 0),
                key_input(
                    WindowsKey {
                        vk: vk::VK_SHIFT,
                        scancode: 0,
                        extended: false,
                    },
                    true,
                ),
                key_input(
                    WindowsKey {
                        vk: vk::VK_CONTROL,
                        scancode: 0,
                        extended: false,
                    },
                    true,
                ),
                key_input(
                    WindowsKey {
                        vk: vk::VK_MENU,
                        scancode: 0,
                        extended: false,
                    },
                    true,
                ),
                key_input(
                    WindowsKey {
                        vk: vk::VK_LWIN,
                        scancode: 0,
                        extended: true,
                    },
                    true,
                ),
            ];
            let _ = send_inputs(&release);
            Ok(())
        }

        fn hide_other_apps(&self, app: Option<&str>) -> Result<Vec<i32>, ToolError> {
            let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
            let records = visible_window_records()?;
            let own_pid = i32::try_from(std::process::id()).unwrap_or(0);

            // Keep the host terminal (the foreground window's owning process,
            // all its windows), the shell owner (taskbar/desktop), this
            // process, and the requested app.
            let foreground_pid = foreground_window_record().and_then(|record| record.pid);
            let shell_pid = shell_window_pid();

            let mut hidden_pids: Vec<i32> = Vec::new();
            for record in &records {
                let Some(pid) = record.pid else {
                    continue;
                };
                if pid == 0
                    || pid == own_pid
                    || Some(pid) == foreground_pid
                    || Some(pid) == shell_pid
                {
                    continue;
                }
                if let Some(requested_app) = requested_app
                    && record.matches_app(requested_app)
                {
                    continue;
                }
                // Always track (restore is idempotent), mirroring the Linux
                // per-PID hidden-window registry keyed by PID.
                if !hidden_pids.contains(&pid) {
                    hidden_pids.push(pid);
                }
                let registry = HIDDEN_WINDOWS.get_or_init(|| Mutex::new(HashMap::new()));
                let mut guard = match registry.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let tracked = guard.entry(pid).or_default();
                let hwnd_addr = record.hwnd as usize as isize;
                if !tracked.contains(&hwnd_addr) {
                    tracked.push(hwnd_addr);
                }
                unsafe {
                    let _ = ShowWindow(record.hwnd, SW_MINIMIZE);
                }
            }
            Ok(hidden_pids)
        }

        fn unhide_apps(&self, pids: &[i32]) -> Result<(), ToolError> {
            if pids.is_empty() {
                return Ok(());
            }
            let mut windows_by_pid: HashMap<i32, Vec<isize>> = HashMap::new();
            if let Some(registry) = HIDDEN_WINDOWS.get() {
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
            // Fallback: re-enumerate by PID (minimized windows still
            // enumerate).
            if let Ok(records) = visible_window_records() {
                for record in records {
                    if let Some(pid) = record.pid
                        && pids.contains(&pid)
                    {
                        let entry = windows_by_pid.entry(pid).or_default();
                        let hwnd_addr = record.hwnd as usize as isize;
                        if !entry.contains(&hwnd_addr) {
                            entry.push(hwnd_addr);
                        }
                    }
                }
            }
            for (_, windows) in windows_by_pid {
                for hwnd_addr in windows {
                    unsafe {
                        let _ = ShowWindow(hwnd_addr as HWND, SW_RESTORE);
                    }
                }
            }
            Ok(())
        }

        fn start_emergency_stop_monitor(
            &self,
        ) -> Result<Option<GuiEmergencyStopMonitor>, ToolError> {
            let triggered = std::sync::Arc::new(AtomicBool::new(false));
            let suppress_next = std::sync::Arc::new(AtomicBool::new(false));
            let (installed_tx, installed_rx) = std::sync::mpsc::channel();
            let thread_triggered = std::sync::Arc::clone(&triggered);
            let thread_suppress = std::sync::Arc::clone(&suppress_next);
            let handle = std::thread::spawn(move || {
                run_hook_thread(thread_triggered, thread_suppress, installed_tx);
            });
            match installed_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(true) => {}
                Ok(false) => {
                    // Hook installation failed on the thread; it exits on its
                    // own and can be joined immediately.
                    let _ = handle.join();
                    return Ok(None);
                }
                // Timeout: ambiguous handshake — never block. If the thread
                // eventually installed the hook it registered itself, so a
                // later teardown still wakes it via WM_QUIT.
                Err(_) => return Ok(None),
            }
            // Graceful degradation is handled above; on success hand the
            // flags to the childless monitor. `handle` is deliberately None:
            // session Drop calls stop() BEFORE cleanup_input_state(), so a
            // joined handle would deadlock every escape-free session (the
            // hook thread self-terminates via WM_QUIT at teardown instead).
            Ok(Some(GuiEmergencyStopMonitor::from_thread(
                triggered,
                suppress_next,
                None,
            )))
        }

        fn capture_context(
            &self,
            app: Option<&str>,
            activate_app_flag: bool,
            window_selection: Option<&WindowSelector>,
        ) -> Result<HelperCaptureContext, ToolError> {
            ensure_dpi_awareness();
            let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
            if activate_app_flag && let Some(requested_app) = requested_app {
                activate_app(requested_app)?;
            }

            let (app_name, selection) = resolve_target(requested_app, window_selection)?;
            let mut cursor = POINT::default();
            if unsafe { GetCursorPos(&mut cursor) } == 0 {
                return Err(respond_to_model(
                    "failed to query the Windows cursor position",
                ));
            }
            let cursor = (f64::from(cursor.x), f64::from(cursor.y));
            let anchor = selection
                .as_ref()
                .map(|selection| {
                    (
                        selection.bounds.x as f64 + selection.bounds.width as f64 / 2.0,
                        selection.bounds.y as f64 + selection.bounds.height as f64 / 2.0,
                    )
                })
                .unwrap_or(cursor);
            let display = display_for_point(anchor, &displays()).ok_or_else(|| {
                respond_to_model("failed to resolve the display for the capture anchor")
            })?;

            let frontmost = foreground_window_record().map(|record| record.app_display_name());

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
                    // DWM extended frame bounds, virtual-screen coordinates.
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
            let screen = virtual_screen();
            let (x, y, width, height) = clamp_capture_rect(bounds, &screen)?;
            let (image_width, image_height, rgba) = capture_rgba(x, y, width, height)?;
            encode_png(image_width, image_height, rgba, target_width, target_height)
        }

        fn observe(
            &self,
            app: Option<&str>,
            activate_app_flag: bool,
            capture_mode: Option<&str>,
            window_selection: Option<&WindowSelector>,
            prefer_window_when_available: bool,
        ) -> Result<PlatformObservation, ToolError> {
            ensure_dpi_awareness();
            let mode = capture_mode.and_then(normalize_capture_mode_env);
            let requested_app = app.map(str::trim).filter(|app| !app.is_empty());
            if activate_app_flag && let Some(requested_app) = requested_app {
                activate_app(requested_app)?;
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

            let (app_name, selection) = resolve_target(requested_app, window_selection)?;
            if window_selection_requested && selection.is_none() {
                return Err(respond_to_model(
                    "requested window could not be found; check `window_title`/`window_selector` or switch to `capture_mode: \"display\"`",
                ));
            }

            let use_window = match mode {
                Some("window") => {
                    if selection.is_none() {
                        return Err(respond_to_model(
                            "window capture was requested but no matching window is available; check `window_title`/`window_selector` or use `capture_mode: \"display\"`",
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

            let displays = displays();
            // Like the macOS helper (a screen-region capture), both modes
            // grab a composited screen region so the capture origin is
            // always the region's global top-left.
            let (capture_bounds, display_index) = if use_window {
                let selection = selection.as_ref().ok_or_else(|| {
                    respond_to_model(
                        "window capture requested but no matching window was available",
                    )
                })?;
                let anchor = (
                    selection.bounds.x as f64 + selection.bounds.width as f64 / 2.0,
                    selection.bounds.y as f64 + selection.bounds.height as f64 / 2.0,
                );
                let display = display_for_point(anchor, &displays).ok_or_else(|| {
                    respond_to_model("failed to resolve the display for the captured window")
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
                let mut cursor = POINT::default();
                if unsafe { GetCursorPos(&mut cursor) } == 0 {
                    return Err(respond_to_model(
                        "failed to query the Windows cursor position",
                    ));
                }
                let cursor = (f64::from(cursor.x), f64::from(cursor.y));
                let anchor = selection
                    .as_ref()
                    .map(|selection| {
                        (
                            selection.bounds.x as f64 + selection.bounds.width as f64 / 2.0,
                            selection.bounds.y as f64 + selection.bounds.height as f64 / 2.0,
                        )
                    })
                    .unwrap_or(cursor);
                let display = display_for_point(anchor, &displays)
                    .ok_or_else(|| respond_to_model("failed to resolve the display for capture"))?;
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

            let screen = virtual_screen();
            let (x, y, width, height) = clamp_capture_rect(&capture_bounds, &screen)?;
            let (image_width, image_height, rgba) = capture_rgba(x, y, width, height)?;
            let image_bytes =
                encode_png(image_width, image_height, rgba, image_width, image_height)?;

            let state = ObserveState {
                capture: CaptureArtifact {
                    origin_x: x as f64,
                    origin_y: y as f64,
                    // Windows pixels == logical points after the per-monitor
                    // v2 DPI switch (scale 1.0); image dims equal the
                    // reported capture dims after clamping, which keeps
                    // grounded-click math exact.
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
                        frontmost_app_name: foreground_window_record()
                            .map(|record| record.app_display_name()),
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
            if let Some(requested_app) = app.map(str::trim).filter(|app| !app.is_empty()) {
                activate_app(requested_app)?;
            }

            match event_mode {
                "move_cursor" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    let settle_ms = optional_int(string_env, "CODEX_GUI_SETTLE_MS")?
                        .unwrap_or(DEFAULT_MOVE_SETTLE_MS)
                        .max(1);
                    move_mouse(x, y)?;
                    std::thread::sleep(Duration::from_millis(settle_ms.max(0) as u64));
                }
                "click" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    move_mouse(x, y)?;
                    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTDOWN, 0)])?;
                    std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
                    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTUP, 0)])?;
                }
                "click_and_hold" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    let hold_ms = optional_int(string_env, "CODEX_GUI_HOLD_MS")?
                        .unwrap_or(DEFAULT_CLICK_AND_HOLD_MS)
                        .max(1);
                    move_mouse(x, y)?;
                    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTDOWN, 0)])?;
                    std::thread::sleep(Duration::from_millis(hold_ms.max(0) as u64));
                    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTUP, 0)])?;
                }
                "double_click" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    move_mouse(x, y)?;
                    for _ in 0..2 {
                        send_inputs(&[mouse_input(MOUSEEVENTF_LEFTDOWN, 0)])?;
                        std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
                        send_inputs(&[mouse_input(MOUSEEVENTF_LEFTUP, 0)])?;
                        std::thread::sleep(Duration::from_millis(80));
                    }
                }
                "right_click" => {
                    let x = require_float(float_env, "CODEX_GUI_X", event_mode)?;
                    let y = require_float(float_env, "CODEX_GUI_Y", event_mode)?;
                    move_mouse(x, y)?;
                    send_inputs(&[mouse_input(MOUSEEVENTF_RIGHTDOWN, 0)])?;
                    std::thread::sleep(Duration::from_millis(CLICK_MILLIS));
                    send_inputs(&[mouse_input(MOUSEEVENTF_RIGHTUP, 0)])?;
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
                    move_mouse(from.0, from.1)?;
                    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTDOWN, 0)])?;
                    std::thread::sleep(Duration::from_millis(50));
                    for (waypoint_x, waypoint_y) in drag_waypoints(from, to, steps) {
                        move_mouse(waypoint_x, waypoint_y)?;
                        std::thread::sleep(Duration::from_millis(step_delay));
                    }
                    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTUP, 0)])?;
                }
                "scroll" => {
                    if let (Some(x), Some(y)) = (
                        float_value(float_env, "CODEX_GUI_X"),
                        float_value(float_env, "CODEX_GUI_Y"),
                    ) {
                        move_mouse(x, y)?;
                    }
                    let scroll_y = required_int(string_env, "CODEX_GUI_SCROLL_Y", event_mode)?;
                    let scroll_x = required_int(string_env, "CODEX_GUI_SCROLL_X", event_mode)?;
                    let unit_is_pixel = string_value(string_env, "CODEX_GUI_SCROLL_UNIT")
                        .is_some_and(|unit| unit.eq_ignore_ascii_case("pixel"));
                    for event in wheel_delta_plan(scroll_x, scroll_y, unit_is_pixel) {
                        send_inputs(&[wheel_input(event.vertical, event.delta)])?;
                        std::thread::sleep(Duration::from_millis(WHEEL_SEND_MILLIS));
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
                                tap_key(
                                    WindowsKey {
                                        vk: vk::VK_BACK,
                                        scancode: 0x0E,
                                        extended: false,
                                    },
                                    &[],
                                )?;
                            }
                        } else {
                            // macOS uses Cmd+A (select all) before typing for
                            // the unicode strategy; Ctrl+A is the Windows
                            // in-app select-all, mirroring the X11 choice.
                            tap_key(
                                WindowsKey {
                                    vk: vk::VK_A,
                                    scancode: 0x1E,
                                    extended: false,
                                },
                                &[WinModifier::Control],
                            )?;
                        }
                    }
                    if strategy == "clipboard_paste" {
                        paste_text(&text, PASTE_EVENT_PRE_DELAY_MS, PASTE_EVENT_POST_DELAY_MS)?;
                    } else if strategy == "physical_keys" {
                        for character in text.chars() {
                            type_char_physical(character)?;
                            std::thread::sleep(Duration::from_millis(PHYSICAL_KEY_CHAR_DELAY_MS));
                        }
                    } else {
                        for character in text.chars() {
                            type_char_unicode(character)?;
                            std::thread::sleep(Duration::from_millis(UNICODE_CHAR_DELAY_MS));
                        }
                    }
                    if submit {
                        tap_key(
                            WindowsKey {
                                vk: vk::VK_RETURN,
                                scancode: 0x1C,
                                extended: false,
                            },
                            &[],
                        )?;
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
                    let Some(key) = macos_keycode_to_key(key_code) else {
                        return Err(respond_to_model(format!(
                            "unsupported macOS virtual key code `{key_code}` for the `{event_mode}` GUI event"
                        )));
                    };
                    for _ in 0..repeat {
                        tap_key(key, &modifiers)?;
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
            // macOS validates the requested window before typing.
            if let Some(window_selection) = window_selection {
                let (_, selection) = resolve_target(
                    app.map(str::trim).filter(|app| !app.is_empty()),
                    Some(window_selection),
                )?;
                if selection.is_none() {
                    return Err(respond_to_model(
                        "requested window could not be found for System Events typing",
                    ));
                }
            }
            if let Some(requested_app) = app.map(str::trim).filter(|app| !app.is_empty()) {
                activate_app(requested_app)?;
            }

            // macOS parity: platform_macos.rs always forwards
            // CODEX_GUI_CLEAR_REPEAT=48 when replace is set, so the
            // type_system_events.applescript select-all branch
            // (`keystroke "a" using command down`) is unreachable through the
            // platform path and every strategy clears via 48x BackSpace —
            // including fields longer than 48 characters.
            if replace {
                for _ in 0..DEFAULT_CLEAR_REPEAT {
                    tap_key(
                        WindowsKey {
                            vk: vk::VK_BACK,
                            scancode: 0x0E,
                            extended: false,
                        },
                        &[],
                    )?;
                }
            }

            match system_events_strategy(strategy) {
                SystemEventsStrategy::Paste => {
                    paste_text(text, PASTE_SYSTEM_PRE_DELAY_MS, PASTE_SYSTEM_POST_DELAY_MS)?;
                }
                SystemEventsStrategy::Keystroke => {
                    for character in text.chars() {
                        type_char_unicode(character)?;
                    }
                }
                SystemEventsStrategy::KeystrokeChars => {
                    for character in text.chars() {
                        type_char_unicode(character)?;
                        std::thread::sleep(Duration::from_millis(
                            DEFAULT_KEYSTROKE_CHAR_DELAY_MS.max(0) as u64,
                        ));
                    }
                }
            }

            if submit {
                tap_key(
                    WindowsKey {
                        vk: vk::VK_RETURN,
                        scancode: 0x1C,
                        extended: false,
                    },
                    &[],
                )?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Pure-logic unit tests (no Win32 calls required; run on any host)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Name/shape contract ------------------------------------------------

    /// The wiring layer holds `static PLATFORM: platform_windows::WindowsPlatform
    /// = platform_windows::WindowsPlatform;` — prove the struct is
    /// const-constructible.
    const PLATFORM: WindowsPlatform = WindowsPlatform;

    #[test]
    fn platform_is_const_constructible_and_thread_safe() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WindowsPlatform>();
        let _ = PLATFORM;
    }

    // -- Capture-mode env normalization ---------------------------------------

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

    // -- Readiness ladder ------------------------------------------------------

    #[test]
    fn readiness_status_ladder_classifies_all_shapes() {
        use GuiReadinessStatus::Error as SError;
        use GuiReadinessStatus::Ok as SOk;
        use GuiReadinessStatus::Unsupported as SUnsupported;
        use GuiReadinessStatus::Warn as SWarn;
        // No checks at all means nothing supports (Linux-parity ladder).
        assert_eq!(readiness_status_ladder(&[]), "unsupported");
        assert_eq!(readiness_status_ladder(&[SOk, SOk]), "ready");
        assert_eq!(readiness_status_ladder(&[SOk, SWarn]), "degraded");
        assert_eq!(readiness_status_ladder(&[SOk, SError]), "blocked");
        assert_eq!(
            readiness_status_ladder(&[SUnsupported, SUnsupported]),
            "unsupported"
        );
        assert_eq!(readiness_status_ladder(&[SUnsupported, SWarn]), "degraded");
    }

    // -- Keycode tables ----------------------------------------------------------

    #[test]
    fn macos_keycode_to_key_named_keys() {
        let expected: [(i64, u16, u16, bool); 13] = [
            (36, vk::VK_RETURN, 0x1C, false),
            (48, vk::VK_TAB, 0x0F, false),
            (53, vk::VK_ESCAPE, 0x01, false),
            (51, vk::VK_BACK, 0x0E, false),
            (115, vk::VK_HOME, 0x47, true),
            (116, vk::VK_PRIOR, 0x49, true),
            (121, vk::VK_NEXT, 0x51, true),
            (119, vk::VK_END, 0x4F, true),
            (123, vk::VK_LEFT, 0x4B, true),
            (124, vk::VK_RIGHT, 0x4D, true),
            (125, vk::VK_DOWN, 0x50, true),
            (126, vk::VK_UP, 0x48, true),
            (49, vk::VK_SPACE, 0x39, false),
        ];
        for (key_code, want_vk, want_scan, want_extended) in expected {
            let key = macos_keycode_to_key(key_code)
                .unwrap_or_else(|| panic!("macOS keycode {key_code} should map to a Windows key"));
            assert_eq!(key.vk, want_vk, "vk for keycode {key_code}");
            assert_eq!(key.scancode, want_scan, "scancode for keycode {key_code}");
            assert_eq!(
                key.extended, want_extended,
                "extended for keycode {key_code}"
            );
        }
    }

    #[test]
    fn macos_keycode_to_key_letter_rows() {
        // (macOS keycode, letter) pairs; VK = ASCII uppercase, scancodes are
        // the PS/2 set-1 positions.
        let expected: [(i64, char, u16); 26] = [
            (0, 'a', 0x1E),
            (1, 's', 0x1F),
            (2, 'd', 0x20),
            (3, 'f', 0x21),
            (4, 'h', 0x23),
            (5, 'g', 0x22),
            (6, 'z', 0x2C),
            (7, 'x', 0x2D),
            (8, 'c', 0x2E),
            (9, 'v', 0x2F),
            (11, 'b', 0x30),
            (12, 'q', 0x10),
            (13, 'w', 0x11),
            (14, 'e', 0x12),
            (15, 'r', 0x13),
            (16, 'y', 0x15),
            (17, 't', 0x14),
            (31, 'o', 0x18),
            (32, 'u', 0x16),
            (34, 'i', 0x17),
            (35, 'p', 0x19),
            (37, 'l', 0x26),
            (38, 'j', 0x24),
            (40, 'k', 0x25),
            (45, 'n', 0x31),
            (46, 'm', 0x32),
        ];
        let mut mapped_letters = Vec::new();
        for (key_code, letter, scan) in expected {
            let key =
                macos_keycode_to_key(key_code).unwrap_or_else(|| panic!("keycode {key_code}"));
            let expected_vk = letter.to_ascii_uppercase() as u16;
            assert_eq!(key.vk, expected_vk, "vk for keycode {key_code} ({letter})");
            assert_eq!(key.scancode, scan, "scancode for keycode {key_code}");
            mapped_letters.push(letter);
        }
        // Full a-z coverage via the distinct set of macOS letter codes.
        assert_eq!(mapped_letters.len(), 26);
    }

    #[test]
    fn macos_keycode_to_key_digits_and_punctuation() {
        let expected: [(i64, char); 20] = [
            (18, '1'),
            (19, '2'),
            (20, '3'),
            (21, '4'),
            (22, '6'),
            (23, '5'),
            (24, '='),
            (25, '9'),
            (26, '7'),
            (27, '-'),
            (28, '8'),
            (29, '0'),
            (30, ']'),
            (33, '['),
            (39, '\''),
            (41, ';'),
            (42, '\\'),
            (43, ','),
            (44, '/'),
            (47, '.'),
        ];
        for (key_code, character) in expected {
            let key = macos_keycode_to_key(key_code)
                .unwrap_or_else(|| panic!("keycode {key_code} should map ({character})"));
            let via_char = char_key(character).expect("char_key should accept the same set");
            assert_eq!(
                key.vk, via_char.0.vk,
                "keycode {key_code} and char `{character}` must agree on VK"
            );
            assert_eq!(
                key.scancode, via_char.0.scancode,
                "keycode {key_code} and char `{character}` must agree on scancode"
            );
        }
    }

    #[test]
    fn macos_keycode_to_key_f_keys_and_unknowns() {
        let f_keys: [(i64, u16); 12] = [
            (122, vk::VK_F1),
            (120, vk::VK_F2),
            (99, vk::VK_F3),
            (118, vk::VK_F4),
            (96, vk::VK_F5),
            (97, vk::VK_F6),
            (98, vk::VK_F7),
            (100, vk::VK_F8),
            (101, vk::VK_F9),
            (109, vk::VK_F10),
            (103, vk::VK_F11),
            (111, vk::VK_F12),
        ];
        for (key_code, want_vk) in f_keys {
            let key = macos_keycode_to_key(key_code)
                .unwrap_or_else(|| panic!("keycode {key_code} should map to an F-key"));
            assert_eq!(key.vk, want_vk);
        }
        // Unknown/unsupported codes: 10 (ISO section sign) and junk.
        assert_eq!(macos_keycode_to_key(10), None);
        assert_eq!(macos_keycode_to_key(999), None);
        assert_eq!(macos_keycode_to_key(-1), None);
    }

    #[test]
    fn char_key_covers_us_layout_and_shift() {
        let (key, shift) = char_key('a').expect("lowercase a");
        assert_eq!(key.vk, vk::VK_A);
        assert!(!shift);
        let (key, shift) = char_key('Z').expect("uppercase Z");
        assert_eq!(key.vk, 0x5A);
        assert!(shift);
        let (key, shift) = char_key('?').expect("shifted slash");
        assert_eq!(key.vk, vk::VK_OEM_2);
        assert!(shift);
        let (key, shift) = char_key('(').expect("shifted 9");
        assert_eq!(key.vk, 0x39);
        assert!(shift);
        let (key, shift) = char_key('`').expect("backtick");
        assert_eq!(key.vk, vk::VK_OEM_3);
        assert!(!shift);
        let (key, shift) = char_key('~').expect("tilde");
        assert_eq!(key.vk, vk::VK_OEM_3);
        assert!(shift);
        let (key, _) = char_key('\n').expect("newline → Return");
        assert_eq!(key.vk, vk::VK_RETURN);
        // Unmappable characters fall back to the unicode strategy.
        assert!(char_key('é').is_none());
        assert!(char_key('😀').is_none());
    }

    // -- Unicode typing newline override ------------------------------------------

    /// The `unicode`/keystroke strategies deliver most characters as raw
    /// `KEYEVENTF_UNICODE` units, but a raw LF unit becomes `WM_CHAR '\n'`,
    /// which standard edit controls ignore (they expect `\r`/VK_RETURN and
    /// never trigger a default button from LF). Newlines must therefore ride
    /// the real Return key, matching the Linux `keysym_for_char` mapping
    /// (`\n`/`\r` → `XK_RETURN`).
    #[test]
    fn unicode_char_override_maps_newlines_to_return_key() {
        let expected = WindowsKey {
            vk: vk::VK_RETURN,
            scancode: 0x1C,
            extended: false,
        };
        assert_eq!(unicode_char_override('\n'), Some(expected));
        assert_eq!(unicode_char_override('\r'), Some(expected));
    }

    #[test]
    fn unicode_char_override_leaves_other_characters_to_utf16() {
        // Printable ASCII, control characters other than newlines, and
        // non-ASCII text all stay on the UTF-16 unit path.
        for character in ['a', 'Z', '5', ' ', '\t', '\u{7f}', 'é', '中', '😀'] {
            assert_eq!(
                unicode_char_override(character),
                None,
                "`{character}` should stay on the UTF-16 path"
            );
        }
    }

    // -- Modifiers ---------------------------------------------------------------

    #[test]
    fn parse_modifier_names_dedupes_and_orders_canonically() {
        assert_eq!(
            parse_modifier_names("shift,control,command,alt"),
            vec![
                WinModifier::Control,
                WinModifier::Alt,
                WinModifier::Command,
                WinModifier::Shift,
            ]
        );
        assert_eq!(
            parse_modifier_names("ctrl, CTRL, option, command"),
            vec![WinModifier::Control, WinModifier::Alt, WinModifier::Command,]
        );
        assert!(parse_modifier_names("bogus,").is_empty());
        assert!(parse_modifier_names("").is_empty());
    }

    #[test]
    fn modifier_vk_maps_with_documented_command_divergence() {
        assert_eq!(modifier_vk(WinModifier::Shift), vk::VK_SHIFT);
        assert_eq!(modifier_vk(WinModifier::Control), vk::VK_CONTROL);
        assert_eq!(modifier_vk(WinModifier::Alt), vk::VK_MENU);
        // command → Win key: documented divergence, mirroring the Linux
        // Super mapping.
        assert_eq!(modifier_vk(WinModifier::Command), vk::VK_LWIN);
    }

    // -- Env parsing --------------------------------------------------------------

    #[test]
    fn env_value_helpers_mirror_the_helper_protocol() {
        let float_env = [("CODEX_GUI_X", 12.5f64)];
        assert_eq!(float_value(&float_env, "CODEX_GUI_X"), Some(12.5));
        assert_eq!(float_value(&float_env, "CODEX_GUI_Y"), None);

        let string_env = [
            ("CODEX_GUI_A", " hello ".to_string()),
            ("CODEX_GUI_B", "   ".to_string()),
        ];
        assert_eq!(string_value(&string_env, "CODEX_GUI_A"), Some("hello"));
        // Whitespace-only counts as absent (helper `trimmedEnv`).
        assert_eq!(string_value(&string_env, "CODEX_GUI_B"), None);
        assert_eq!(string_value(&string_env, "CODEX_GUI_C"), None);

        let int_env = [("CODEX_GUI_N", "42".to_string())];
        assert_eq!(optional_int(&int_env, "CODEX_GUI_N").unwrap(), Some(42));
        assert_eq!(optional_int(&int_env, "CODEX_GUI_MISSING").unwrap(), None);
        let bad = [("CODEX_GUI_N", "1.5".to_string())];
        assert!(optional_int(&bad, "CODEX_GUI_N").is_err());
    }

    #[test]
    fn required_int_error_names_the_key_and_mode() {
        let error = required_int(&[], "CODEX_GUI_STEPS", "drag").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`CODEX_GUI_STEPS` is required for the `drag` GUI event.")
        );
    }

    // -- Event-mode protocol -------------------------------------------------------

    #[test]
    fn event_modes_match_the_helper_protocol() {
        assert_eq!(EVENT_MODES.len(), 9);
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
            assert!(EVENT_MODES.contains(&mode), "missing mode {mode}");
        }
    }

    // -- Clear-repeat / strategy mapping ---------------------------------------------

    #[test]
    fn clear_repeat_defaults_to_48_for_clipboard_paste_only() {
        // macOS parity: the platform path always clears via the fixed
        // 48-BackSpace count for paste; other strategies select-all.
        assert_eq!(clear_repeat_for_strategy(true, None), DEFAULT_CLEAR_REPEAT);
        assert_eq!(clear_repeat_for_strategy(false, None), 0);
        assert_eq!(clear_repeat_for_strategy(true, Some(7)), 7);
        assert_eq!(clear_repeat_for_strategy(false, Some(-3)), 0);
        assert_eq!(DEFAULT_CLEAR_REPEAT, 48);
    }

    #[test]
    fn system_events_strategy_falls_back_to_paste() {
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
        assert_eq!(system_events_strategy(""), SystemEventsStrategy::Paste);
        assert_eq!(system_events_strategy("bogus"), SystemEventsStrategy::Paste);
    }

    // -- Drag math ---------------------------------------------------------------------

    #[test]
    fn drag_waypoints_are_linear_and_inclusive() {
        assert_eq!(
            drag_waypoints((0.0, 0.0), (10.0, -20.0), 2),
            vec![(5.0, -10.0), (10.0, -20.0)]
        );
        assert_eq!(drag_waypoints((1.0, 1.0), (1.0, 1.0), 0), vec![(1.0, 1.0)]);
        assert_eq!(drag_step_delay_ms(450, 24), 18);
        assert_eq!(drag_step_delay_ms(50, 24), 10);
    }

    // -- Wheel math ----------------------------------------------------------------------

    #[test]
    fn wheel_axis_total_scales_lines_and_keeps_pixels() {
        assert_eq!(wheel_axis_total(3, false), WHEEL_DELTA_NOTCH);
        assert_eq!(wheel_axis_total(-2, false), -80);
        assert_eq!(wheel_axis_total(300, true), 300);
        assert_eq!(wheel_axis_total(0, false), 0);
    }

    #[test]
    fn wheel_axis_events_chunk_at_one_notch_keeping_sign() {
        assert_eq!(wheel_axis_events(400), vec![120, 120, 120, 40]);
        assert_eq!(wheel_axis_events(-200), vec![-120, -80]);
        assert_eq!(wheel_axis_events(7), vec![7]);
        assert!(wheel_axis_events(0).is_empty());
    }

    #[test]
    fn wheel_delta_plan_is_vertical_first_with_macos_signs() {
        // Up=+Y → +vertical; Down=−Y → −vertical; Left=−X → −horizontal;
        // Right=+X → +horizontal; vertical applied before horizontal.
        assert_eq!(
            wheel_delta_plan(0, 3, false),
            vec![WheelEvent {
                vertical: true,
                delta: 120
            }]
        );
        assert_eq!(
            wheel_delta_plan(0, -2, false),
            vec![WheelEvent {
                vertical: true,
                delta: -80
            }]
        );
        assert_eq!(
            wheel_delta_plan(-5, 0, false),
            vec![
                WheelEvent {
                    vertical: false,
                    delta: -120
                },
                WheelEvent {
                    vertical: false,
                    delta: -80
                },
            ]
        );
        assert_eq!(
            wheel_delta_plan(0, 10, false),
            vec![
                WheelEvent {
                    vertical: true,
                    delta: 120
                },
                WheelEvent {
                    vertical: true,
                    delta: 120
                },
                WheelEvent {
                    vertical: true,
                    delta: 120
                },
                WheelEvent {
                    vertical: true,
                    delta: 40
                },
            ]
        );
        assert_eq!(
            wheel_delta_plan(300, 0, true),
            vec![
                WheelEvent {
                    vertical: false,
                    delta: 120
                },
                WheelEvent {
                    vertical: false,
                    delta: 120
                },
                WheelEvent {
                    vertical: false,
                    delta: 60
                },
            ]
        );
        assert_eq!(
            wheel_delta_plan(0, 7, true),
            vec![WheelEvent {
                vertical: true,
                delta: 7
            }]
        );
        assert_eq!(
            wheel_delta_plan(3, -1, false),
            vec![
                WheelEvent {
                    vertical: true,
                    delta: -40
                },
                WheelEvent {
                    vertical: false,
                    delta: 120
                },
            ]
        );
        assert!(wheel_delta_plan(0, 0, false).is_empty());
    }

    // -- Virtual-screen normalization ------------------------------------------------------

    #[test]
    fn normalize_point_single_monitor_zero_origin() {
        let screen = VirtualScreen {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            normalize_point_for_virtual_screen(0.0, 0.0, &screen),
            (0, 0)
        );
        assert_eq!(
            normalize_point_for_virtual_screen(1919.0, 1079.0, &screen),
            (65535, 65535)
        );
        // Mid-screen maps to the middle of the 0..=65535 range.
        let (nx, ny) = normalize_point_for_virtual_screen(959.5, 539.5, &screen);
        assert!((32700..=32850).contains(&nx), "nx = {nx}");
        assert!((32700..=32850).contains(&ny), "ny = {ny}");
    }

    #[test]
    fn normalize_point_handles_multi_monitor_negative_origins() {
        // Left + lower monitor: virtual screen starts at (-1920, -1080).
        let screen = VirtualScreen {
            x: -1920,
            y: -1080,
            width: 3840,
            height: 2160,
        };
        // Clean arithmetic screen for the exact-value assertions.
        let exact = VirtualScreen {
            x: -1000,
            y: -500,
            width: 2001,
            height: 1001,
        };
        assert_eq!(
            normalize_point_for_virtual_screen(-1000.0, -500.0, &exact),
            (0, 0)
        );
        assert_eq!(
            normalize_point_for_virtual_screen(1000.0, 500.0, &exact),
            (65535, 65535)
        );
        assert_eq!(
            normalize_point_for_virtual_screen(0.0, 0.0, &exact),
            (32768, 32768)
        );
        assert_eq!(
            normalize_point_for_virtual_screen(0.0, 250.0, &exact),
            (32768, 49151)
        );

        // The negative-origin screen: monotonic inside, clamped outside.
        let (nx, ny) = normalize_point_for_virtual_screen(-1920.0, -1080.0, &screen);
        assert_eq!((nx, ny), (0, 0));
        let (nx, ny) = normalize_point_for_virtual_screen(1919.0, 1079.0, &screen);
        assert_eq!((nx, ny), (65535, 65535));
        let (nx, ny) = normalize_point_for_virtual_screen(0.0, 0.0, &screen);
        assert!((32000..=33500).contains(&nx));
        assert!((32000..=33500).contains(&ny));
        // Off-screen points clamp, never wrap.
        assert_eq!(
            normalize_point_for_virtual_screen(-5000.0, 0.0, &screen).0,
            0
        );
        assert_eq!(
            normalize_point_for_virtual_screen(5000.0, 0.0, &screen).0,
            65535
        );
        assert_eq!(
            normalize_point_for_virtual_screen(0.0, -9000.0, &screen).1,
            0
        );
        assert_eq!(
            normalize_point_for_virtual_screen(0.0, 9000.0, &screen).1,
            65535
        );
    }

    #[test]
    fn normalize_point_degenerate_one_pixel_screen() {
        let screen = VirtualScreen {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        assert_eq!(
            normalize_point_for_virtual_screen(0.0, 0.0, &screen),
            (0, 0)
        );
        assert_eq!(
            normalize_point_for_virtual_screen(5.0, 5.0, &screen),
            (65535, 65535)
        );
    }

    // -- Capture rect clamping --------------------------------------------------------

    #[test]
    fn clamp_capture_rect_clamps_to_the_virtual_screen() {
        let screen = VirtualScreen {
            x: -1920,
            y: -1080,
            width: 3840,
            height: 2160,
        };
        // Fully inside (a monitor rect): untouched.
        assert_eq!(
            clamp_capture_rect(
                &HelperRect {
                    x: 0.0,
                    y: 0.0,
                    width: 800.0,
                    height: 600.0
                },
                &screen
            )
            .unwrap(),
            (0, 0, 800, 600)
        );
        // Extending past the right/bottom edges clamps.
        assert_eq!(
            clamp_capture_rect(
                &HelperRect {
                    x: 1900.0,
                    y: 1000.0,
                    width: 500.0,
                    height: 500.0
                },
                &screen
            )
            .unwrap(),
            (1900, 1000, 20, 80)
        );
        // Negative origins clamp against the left/top edges.
        assert_eq!(
            clamp_capture_rect(
                &HelperRect {
                    x: -3000.0,
                    y: -2000.0,
                    width: 2000.0,
                    height: 2000.0
                },
                &screen
            )
            .unwrap(),
            (-1920, -1080, 920, 1080)
        );
        // Fully outside → empty.
        assert!(
            clamp_capture_rect(
                &HelperRect {
                    x: 9999.0,
                    y: 9999.0,
                    width: 10.0,
                    height: 10.0
                },
                &screen
            )
            .is_err()
        );
        // Non-finite bounds rejected.
        assert!(
            clamp_capture_rect(
                &HelperRect {
                    x: f64::NAN,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0
                },
                &screen
            )
            .is_err()
        );
    }

    // -- Pixel conversion + PNG ---------------------------------------------------------

    #[test]
    fn bgra_to_rgba_swaps_channels_and_forces_alpha() {
        let bgra = [1, 2, 3, 0, 255, 128, 64, 9];
        assert_eq!(bgra_to_rgba(&bgra), vec![3, 2, 1, 255, 64, 128, 255, 255]);
        assert!(bgra_to_rgba(&[1, 2, 3, 4, 5]).ends_with(&[5]));
    }

    #[test]
    fn encode_png_roundtrips_and_resizes() {
        let width = 4u32;
        let height = 3u32;
        let rgba: Vec<u8> = (0..width * height * 4)
            .map(|byte| (byte % 251) as u8)
            .collect();
        let png = encode_png(width, height, rgba.clone(), 0, 0).expect("png encode");
        let decoded = image::load_from_memory(&png).expect("png decode");
        assert_eq!(decoded.width(), width);
        assert_eq!(decoded.height(), height);

        let resized = encode_png(width, height, rgba, 8, 6).expect("png resize encode");
        let decoded = image::load_from_memory(&resized).expect("png decode");
        assert_eq!(decoded.width(), 8);
        assert_eq!(decoded.height(), 6);

        assert!(encode_png(2, 2, vec![0; 3], 0, 0).is_err());
    }

    // -- Readiness checks -----------------------------------------------------------------

    #[test]
    fn readiness_checks_ready_when_elevated_and_probed_ok() {
        let probes = ProbeSnapshot {
            interactive_desktop: true,
            capture_available: true,
            hook_installable: true,
            elevated: true,
        };
        let checks = readiness_checks_from_probes(&probes);
        let ids: Vec<&str> = checks.iter().map(|check| check.id).collect();
        assert_eq!(
            ids,
            vec![
                "platform",
                "native_helper",
                "accessibility",
                "screen_recording"
            ]
        );
        let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
        assert_eq!(readiness_status_ladder(&statuses), "ready");
    }

    #[test]
    fn readiness_checks_warn_on_uipi_for_non_elevated_processes() {
        let probes = ProbeSnapshot {
            interactive_desktop: true,
            capture_available: true,
            hook_installable: true,
            elevated: false,
        };
        let checks = readiness_checks_from_probes(&probes);
        let uipi = checks
            .iter()
            .find(|check| check.id == "uipi")
            .expect("uipi check present for non-elevated processes");
        assert_eq!(uipi.status, GuiReadinessStatus::Warn);
        assert!(uipi.summary.contains("UIPI"));
        let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
        assert_eq!(readiness_status_ladder(&statuses), "degraded");
    }

    #[test]
    fn readiness_checks_block_without_an_interactive_desktop() {
        let probes = ProbeSnapshot {
            interactive_desktop: false,
            capture_available: false,
            hook_installable: false,
            elevated: false,
        };
        let checks = readiness_checks_from_probes(&probes);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].id, "platform");
        assert_eq!(checks[0].status, GuiReadinessStatus::Error);
        assert_eq!(checks[1].id, "native_helper");
        assert_eq!(checks[1].status, GuiReadinessStatus::Error);
        let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
        assert_eq!(readiness_status_ladder(&statuses), "blocked");
    }

    #[test]
    fn readiness_checks_report_capture_and_hook_failures() {
        let probes = ProbeSnapshot {
            interactive_desktop: true,
            capture_available: false,
            hook_installable: false,
            elevated: true,
        };
        let checks = readiness_checks_from_probes(&probes);
        let screen = checks
            .iter()
            .find(|check| check.id == "screen_recording")
            .expect("screen_recording check");
        assert_eq!(screen.status, GuiReadinessStatus::Error);
        let stop = checks
            .iter()
            .find(|check| check.id == "emergency_stop")
            .expect("emergency_stop check");
        assert_eq!(stop.status, GuiReadinessStatus::Warn);
        let statuses = checks.iter().map(|check| check.status).collect::<Vec<_>>();
        assert_eq!(readiness_status_ladder(&statuses), "blocked");
    }

    #[test]
    fn capabilities_follow_the_probe_matrix() {
        let ready = capabilities_from_probes(&ProbeSnapshot {
            interactive_desktop: true,
            capture_available: true,
            hook_installable: true,
            elevated: false,
        });
        assert_eq!(ready.len(), GUI_TOOL_NAMES.len());
        assert!(ready.values().all(|capability| capability.enabled));

        let capture_failed = capabilities_from_probes(&ProbeSnapshot {
            interactive_desktop: true,
            capture_available: false,
            hook_installable: true,
            elevated: false,
        });
        for tool_name in ["gui_observe", "gui_click", "gui_drag", "gui_wait"] {
            let capability = capture_failed.get(tool_name).expect("capability entry");
            assert!(!capability.enabled, "{tool_name} must be disabled");
        }
        for tool_name in ["gui_scroll", "gui_type"] {
            let capability = capture_failed.get(tool_name).expect("capability entry");
            assert!(capability.enabled, "{tool_name} stays enabled");
            assert!(
                capability.targetless_only,
                "{tool_name} becomes targetless-only"
            );
        }
        for tool_name in ["gui_key", "gui_move"] {
            let capability = capture_failed.get(tool_name).expect("capability entry");
            assert!(capability.enabled);
            assert!(!capability.targetless_only);
        }

        let headless = capabilities_from_probes(&ProbeSnapshot {
            interactive_desktop: false,
            capture_available: false,
            hook_installable: false,
            elevated: false,
        });
        assert!(headless.values().all(|capability| !capability.enabled));
    }

    // -- Window ranking / selection -----------------------------------------------------

    fn candidate(
        id: i64,
        x: i64,
        y: i64,
        width: i64,
        height: i64,
        title: Option<&str>,
        stacking: usize,
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
            stacking_index: stacking,
        }
    }

    #[test]
    fn rank_windows_prefers_topmost_then_area_then_title() {
        let ranked = rank_windows(vec![
            candidate(1, 0, 0, 100, 100, Some("small"), 2),
            candidate(2, 0, 0, 400, 400, Some("big"), 1),
            candidate(3, 0, 0, 200, 200, Some("mid"), 0),
        ]);
        let ids: Vec<i64> = ranked.iter().map(|window| window.id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    #[test]
    fn select_window_index_union_and_strategies() {
        let windows = vec![
            candidate(1, 0, 0, 100, 100, Some("one"), 0),
            candidate(2, 50, 50, 100, 100, Some("two"), 1),
        ];
        // 1-based index selection.
        let selection = select_window(&windows, Some(2), true).expect("selection");
        assert_eq!(selection.primary.id, 2);
        assert_eq!(selection.strategy, "selected_window");
        assert_eq!(selection.window_count, 1);
        // Out-of-range index falls through to ranked-first.
        let selection = select_window(&windows, Some(9), true).expect("selection");
        assert_eq!(selection.primary.id, 1);
        assert_eq!(selection.strategy, "selected_window");
        // Single candidate → main_window.
        let single = vec![candidate(7, 0, 0, 100, 100, Some("solo"), 0)];
        let selection = select_window(&single, None, false).expect("selection");
        assert_eq!(selection.strategy, "main_window");
        // Multiple candidates without explicit selection → union bounds.
        let selection = select_window(&windows, None, false).expect("selection");
        assert_eq!(selection.strategy, "app_union");
        assert_eq!(selection.window_count, 2);
        assert_eq!(
            selection.bounds,
            RectI {
                x: 0,
                y: 0,
                width: 150,
                height: 150
            }
        );
        assert!(select_window(&[], Some(1), true).is_none());
    }

    #[test]
    fn app_matches_window_on_process_stem_or_title() {
        assert!(app_matches_window(
            "code",
            Some("Code"),
            Some("Visual Studio Code")
        ));
        assert!(app_matches_window("CODE", Some("code"), None));
        assert!(app_matches_window("notes", None, Some("My Notes App")));
        assert!(app_matches_window("note", None, Some("My Notes App")));
        assert!(!app_matches_window(
            "code",
            Some("Terminal"),
            Some("Terminal")
        ));
        assert!(!app_matches_window("", Some("Code"), Some("Code")));
        assert!(!app_matches_window("zzz", None, None));
    }

    // -- Geometry helpers ------------------------------------------------------------------

    #[test]
    fn rect_contains_and_display_for_point() {
        let rect = RectI {
            x: 10,
            y: 20,
            width: 5,
            height: 5,
        };
        assert!(rect.contains_point((10.0, 20.0)));
        assert!(rect.contains_point((14.9, 24.9)));
        assert!(!rect.contains_point((15.0, 20.0)));
        assert!(!rect.contains_point((10.0, 25.0)));

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
                    x: -1920,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
        ];
        assert_eq!(
            display_for_point((-100.0, 50.0), &displays).unwrap().index,
            2
        );
        assert_eq!(
            display_for_point((100.0, 50.0), &displays).unwrap().index,
            1
        );
        assert_eq!(
            display_for_point((99999.0, 50.0), &displays).unwrap().index,
            1
        );
    }
}
