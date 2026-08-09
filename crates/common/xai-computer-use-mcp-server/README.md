# xai-computer-use-mcp-server

MCP (Model Context Protocol) server exposing the
[`xai-computer-use`](../xai-computer-use/) tool family over **stdio**, so any
MCP client — Claude Code, grok-build, or a custom host — can drive desktop
and browser UI automation:

| MCP tool name | Purpose |
|---|---|
| `computer_use__find_roots` | Bounded, ranked list of controllable UI roots (desktop windows, browser pages) |
| `computer_use__observe_ui` | Capture a state-scoped semantic outline of one root (+ optional image) |
| `computer_use__search_ui` | Search elements in a previously captured state |
| `computer_use__expand_ui` | Expand one element's cached children |
| `computer_use__inspect_ui` | Inspect one element from a captured state |
| `computer_use__act_ui` | Perform checked UI actions against a captured state |
| `computer_use__read_text` | Read a page of text from one element |
| `computer_use__wait_for` | Wait for a scoped UI condition |
| `computer_use__launch_browser` | Launch the managed CDP browser |
| `computer_use__navigate_browser` | Navigate a browser-page root to a URL |
| `computer_use__evaluate_browser` | Evaluate a JS expression in a browser-page root |

All `act_ui` outcomes are honest and evidence-based (`Worked` / `Didnt` /
`Unknown`) — the backend never fabricates success it cannot verify, and
uncertain actions are rejected when the captured state is stale.

## Why the `computer_use__` prefix?

MCP tool names may only contain `[a-zA-Z0-9_-]`. The in-process tool ids
(`computer_use:find_roots`) use `:` as the namespace separator, so the MCP
server maps `:` → `__` (the same `server__tool` convention the workspace
uses for MCP-discovered tools).

## Building

```sh
cargo build -p xai-computer-use-mcp-server
# binary: target/debug/xai-computer-use-mcp-server
```

## Running

The binary speaks MCP over stdin/stdout:

```sh
cargo run -p xai-computer-use-mcp-server
```

It never takes over your terminal — stdio is reserved for the MCP protocol,
and logs go to stderr (`RUST_LOG=debug` for more detail).

### Environment variables

Passed straight through to the native backend (same semantics as the
in-process integration):

| Variable | Default | Meaning |
|---|---|---|
| `COMPUTER_USE_CDP_PORT` | unset | CDP remote-debugging port for the browser backend (e.g. `9222`) |
| `COMPUTER_USE_BROWSER_PATH` | auto-discovered | Browser executable used by `launch_browser` |
| `COMPUTER_USE_HEADLESS` | `false` | Launch the managed browser in headless mode |

### Platform requirements

- **macOS**: Accessibility permission (System Settings → Privacy & Security
  → Accessibility) for UI trees and input; Screen Recording permission for
  window capture.
- **Linux (X11)**: AT-SPI2 bus (`at-spi2-core`) and XTEST on the X server;
  Wayland-only sessions degrade to semantic-only.
- **Windows**: no extra permissions required.

## Client configuration

### Claude Code

```sh
claude mcp add computer-use \
  --transport stdio \
  -- /absolute/path/to/xai-computer-use-mcp-server
```

### Claude Code (`~/.claude.json` / project `.mcp.json`)

```json
{
  "mcpServers": {
    "computer-use": {
      "command": "/absolute/path/to/xai-computer-use-mcp-server",
      "args": []
    }
  }
}
```

### Any MCP client

Point it at a stdio server with `command` = the compiled binary. The server
advertises `tools` capability and exposes the 11 tools above.

## How it works

```
MCP client <──stdio JSON-RPC──> ComputerUseMcpServer ──> ComputerUseService
                                (rmcp ServerHandler)        (native backend)
```

- Tools are built once per process from
  [`computer_use_tools`](https://docs.rs/xai-computer-use/latest/xai_computer_use/fn.computer_use_tools.html)
  over a single shared [`ComputerUseService`](https://docs.rs/xai-computer-use/latest/xai_computer_use/service/struct.ComputerUseService.html),
  so captured UI state is shared across the whole tool family.
- Success results are returned as pretty-printed JSON text blocks; image
  evidence (e.g. `observe_ui` screenshots) is forwarded as native MCP image
  blocks.
- Tool-level failures (`act_ui` on a stale state, backend errors) return
  `isError: true` with a readable message; unroutable requests (unknown
  tool names) are JSON-RPC protocol errors.
- Read-only tools (`find_roots`, `observe_ui`, …) advertise
  `readOnlyHint`; mutating tools (`act_ui`, `launch_browser`, …) advertise
  `destructiveHint`.

## Tests

```sh
cargo test -p xai-computer-use-mcp-server
```

The integration tests run a real MCP session over an in-memory
stdio-style transport against an `InMemoryBackend` (no OS permissions
needed): initialize handshake, `tools/list`, `tools/call` for
`find_roots`/`observe_ui`, unknown-tool protocol errors, and pagination.
