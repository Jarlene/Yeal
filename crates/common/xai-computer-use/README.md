# xai-computer-use

State-scoped desktop and browser computer-use tools for the xAI Computer Hub —
a Rust port of [pi-computer-use](https://github.com/injaneity/pi-computer-use/)
(MIT), directly usable inside grok-build.

The crate registers a complete tool family that lets an agent capture a UI
state, search/expand/inspect it, act on it, and follow up — plus managed
browser control over the Chrome DevTools Protocol (CDP).

## Tool family

| Tool | Purpose |
|---|---|
| `find_roots` | Ranked, bounded list of controllable UI roots (desktop windows, transient surfaces, browser pages) with refs, geometry, focus |
| `observe_ui` | Capture a state-scoped semantic outline of one root (+ optional image evidence) |
| `search_ui` | Search elements in a previously captured state |
| `expand_ui` | Expand one element's cached children from a captured state |
| `inspect_ui` | Inspect one element from a captured state |
| `act_ui` | Perform checked UI actions against a captured state, returning the successor state |
| `read_text` | Read a fixed-size page of text from one element |
| `wait_for` | Wait for a scoped UI condition and report whether it was satisfied |
| `launch_browser` | Launch the configured managed CDP browser and observe a browser-page root |
| `navigate_browser` | Navigate a browser-page root to a URL |
| `evaluate_browser` | Evaluate a JS expression in a browser-page root |

All `act_ui` outcomes are honest and evidence-based: `Worked`, `Didnt`, or
`Unknown` — the backend never fabricates success when it cannot verify it.
Uncertain actions are rejected before dispatch when the state is stale.

## Architecture

- `backends/macos.rs` — macOS Accessibility (AXUIElement via objc2), CGEvent
  input, window capture via xcap (CGWindowListCreateImage).
- `backends/windows.rs` — Windows UIA (windows crate), SendInput input,
  EnumWindows roots, xcap capture.
- `backends/linux.rs` — Linux AT-SPI2 (async atspi crate over zbus), X11
  EWMH roots, XTEST input, X11 `XGetImage` capture (XComposite
  name-window-pixmap with a plain-drawable fallback; degrades to
  semantic-only on Wayland sessions).
- `backends/cdp.rs` — browser backend over the Chrome DevTools Protocol:
  reqwest `/json/list` discovery, tokio-tungstenite JSON-RPC sessions,
  `DOM.getFullAXTree` → outline, `DOM.resolveNode` + `Runtime.callFunctionOn`
  and `Input.dispatch*` actions, honest verification.
- `backends/composite.rs` — merges the desktop backend with CDP into one
  multi-root view (`@rN` refs keyed by resource + window id).
- `service.rs` — state-scoped orchestration: wire refs (`ax:<seq>`,
  `uia:<seq>`, `atspi:<seq>`, `cdp:<backendNodeId>`) are resolved here before
  dispatch; `wait_for` live-polls conditions.

## Integration

`WorkspaceHandle::build` in `crates/codegen/xai-grok-workspace/src/handle.rs`
constructs the native backend from the environment, wraps it in a
`ComputerUseService`, and registers the tool family into the session's
`LocalRegistry`:

```rust
let computer_use_backend = xai_computer_use::backends::native_backend(
    &xai_computer_use::backends::ComputerUseConfig::from_env(),
);
let computer_use_service = std::sync::Arc::new(
    xai_computer_use::ComputerUseService::new(computer_use_backend),
);
xai_computer_use::register_computer_use_tools(&local_registry, computer_use_service);
```

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `COMPUTER_USE_CDP_PORT` | `9222` | CDP remote-debugging port for the browser backend |
| `COMPUTER_USE_BROWSER_PATH` | auto-discovered Chrome/Chromium/Edge | Browser executable used by `launch_browser` |
| `COMPUTER_USE_HEADLESS` | `false` | Launch the managed browser in headless mode |

The CDP backend also works against any browser already running with
`--remote-debugging-port=<port>`.

## Platform requirements

- **macOS**: Accessibility permission (System Settings → Privacy & Security →
  Accessibility) for UI trees and input; Screen Recording permission for
  window capture.
- **Linux (X11)**: AT-SPI2 bus (`at-spi2-core`, e.g. `at-spi-bus-launcher`)
  and XTEST (`xdotool`-style fake input) on the X server. On Wayland-only
  sessions input and capture degrade to semantic-only, matching upstream.
- **Windows**: no extra permissions required.

## Verification

- Host: `cargo test -p xai-computer-use` (6 integration tests) and
  `cargo check -p xai-grok-workspace`.
- Linux: `cargo zigbuild --target x86_64-unknown-linux-gnu -p xai-computer-use`.
- Windows: `CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo check --target x86_64-pc-windows-gnu -p xai-computer-use`.
