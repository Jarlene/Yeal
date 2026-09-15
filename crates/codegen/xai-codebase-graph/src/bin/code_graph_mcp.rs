//! Minimal MCP (stdio) server exposing code-graph queries.
//!
//! Speaks the Model Context Protocol over stdin/stdout using newline-delimited
//! JSON-RPC 2.0 (the MCP stdio transport). No external MCP SDK is used: the
//! workspace intentionally quarantines `rmcp`/`reqwest 0.13` inside
//! `xai-grok-mcp`, so this bin stays dependency-free beyond `serde_json`.
//!
//! # Usage
//!
//! ```bash
//! code-graph-mcp /path/to/repo [--cache /path/to/index.bin] [--force]
//! ```
//!
//! The server builds (or loads) an index for `<root>` at startup and exposes
//! the following tools:
//!
//! - `find_definitions(symbol, context_file?)`   — definitions of a symbol
//! - `find_references(symbol, context_file?)`    — references to a symbol
//! - `symbol_at_position(file, row, col)`        — identifier at a position
//! - `goto_definition(file, row, col)`           — definitions of the symbol at a position
//! - `goto_references(file, row, col, include_definition?)`
//! - `stats()`                                   — index statistics
//!
//! All `file`/`context_file` arguments are relative to `<root>`, matching the
//! index's stored paths. `row`/`col` are 1-indexed. Query results are
//! line-granular, like the `code-graph` CLI.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use xai_codebase_graph::{
    IndexBuilder, Navigator, NavigationResult, ScopeGraphIndex, get_cache_path, load_index,
    save_index,
};

const SERVER_NAME: &str = "code-graph-mcp";
const PROTOCOL_VERSION: &str = "2025-06-18";

// ---------------------------------------------------------------------------
// Index loading
// ---------------------------------------------------------------------------

/// Load a fresh index from cache, or build one and persist it.
///
/// A cached index is reused only when every recorded file is still up to date
/// (same size + mtime); otherwise it is rebuilt. On read-only roots a cache
/// write failure is downgraded to a warning.
fn load_or_build_index(root: &Path, cache: &Path, force: bool) -> ScopeGraphIndex {
    if !force {
        if let Ok(index) = load_index(cache) {
            if index_is_fresh(&index, root) {
                eprintln!("code-graph-mcp: loaded index from {}", cache.display());
                return index;
            }
            eprintln!("code-graph-mcp: cache is stale, rebuilding");
        }
    }

    eprintln!("code-graph-mcp: building index for {}", root.display());
    let index = match IndexBuilder::new().build(root) {
        Ok(index) => index,
        Err(e) => {
            eprintln!("code-graph-mcp: index build failed: {e}");
            std::process::exit(1);
        }
    };
    let (files, defs, refs) = index.stats();
    eprintln!("code-graph-mcp: indexed {files} files, {defs} defs, {refs} refs");

    if let Err(e) = save_index(cache, &index) {
        eprintln!("code-graph-mcp: warning: could not save cache: {e}");
    } else {
        eprintln!("code-graph-mcp: saved cache to {}", cache.display());
    }
    index
}

/// Whether every file tracked in `index` still matches its recorded metadata.
///
/// Stored paths are relative to `root`, so each is stat'ed at `root.join(rel)`
/// and compared against the cached `FileMeta` (which does its own fs lookup;
/// no reliance on the interner, whose keys are the relative strings).
fn index_is_fresh(index: &ScopeGraphIndex, root: &Path) -> bool {
    index
        .file_paths_with_meta()
        .all(|(rel_path, meta)| !meta.is_stale(&root.join(rel_path)))
}

// ---------------------------------------------------------------------------
// MCP request/response helpers
// ---------------------------------------------------------------------------

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn is_notification(method: &str) -> bool {
    method.starts_with("notifications/")
}

/// Render a tool-level failure as a `tools/call` result with `isError: true`.
fn tool_error(message: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// Render navigation results (symbol + locations) as a text JSON payload.
fn navigation_text(result: &NavigationResult) -> String {
    let locations: Vec<Value> = result
        .locations
        .iter()
        .map(|loc| {
            json!({
                "path": loc.path,
                "line": loc.line,
                "symbol": loc.symbol,
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({
        "symbol": result.symbol,
        "locations": locations,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

// ---------------------------------------------------------------------------
// Tool schemas
// ---------------------------------------------------------------------------

const SCHEMA_ROW_COL: &str = "1-indexed row/column";
const SCHEMA_FILE: &str =
    "File path relative to the index root (same convention as the code-graph CLI)";

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "find_definitions",
            "description": "Find where a symbol is defined. Same-language results are ranked first when context_file is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name" },
                    "context_file": { "type": "string", "description": SCHEMA_FILE }
                },
                "required": ["symbol"]
            }
        }),
        json!({
            "name": "find_references",
            "description": "Find all references to a symbol (including via aliases). Same-language results are ranked first when context_file is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name" },
                    "context_file": { "type": "string", "description": SCHEMA_FILE },
                    "include_definition": { "type": "boolean", "description": "Also include definition locations" }
                },
                "required": ["symbol"]
            }
        }),
        json!({
            "name": "symbol_at_position",
            "description": "Return the identifier at a given position in a file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": SCHEMA_FILE },
                    "row": { "type": "integer", "description": SCHEMA_ROW_COL },
                    "col": { "type": "integer", "description": SCHEMA_ROW_COL }
                },
                "required": ["file", "row", "col"]
            }
        }),
        json!({
            "name": "goto_definition",
            "description": "Go to the definition of the symbol at a position (1-indexed row/col).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": SCHEMA_FILE },
                    "row": { "type": "integer", "description": SCHEMA_ROW_COL },
                    "col": { "type": "integer", "description": SCHEMA_ROW_COL }
                },
                "required": ["file", "row", "col"]
            }
        }),
        json!({
            "name": "goto_references",
            "description": "Go to all references of the symbol at a position (1-indexed row/col).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": SCHEMA_FILE },
                    "row": { "type": "integer", "description": SCHEMA_ROW_COL },
                    "col": { "type": "integer", "description": SCHEMA_ROW_COL },
                    "include_definition": { "type": "boolean", "description": "Also include definition locations" }
                },
                "required": ["file", "row", "col"]
            }
        }),
        json!({
            "name": "stats",
            "description": "Index statistics: file/definition/reference/alias counts and top referenced symbols.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

struct Server {
    root: PathBuf,
    navigator: Navigator,
}

impl Server {
    /// Resolve a root-relative path (or absolute path) to an absolute path.
    fn resolve(&self, file: &str) -> PathBuf {
        let p = Path::new(file);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        }
    }

    /// Dispatch a `tools/call` request and return the JSON result value.
    fn call_tool(&self, name: &str, arguments: &Value) -> Value {
        let args = arguments.as_object().cloned().unwrap_or_default();

        // Extract parameters as owned values up front so tool dispatch stays
        // free of borrow-lifetime gymnastics.
        let get_str = |key: &str| -> Option<String> {
            args.get(key).and_then(|v| v.as_str()).map(str::to_owned)
        };
        let get_usize = |key: &str| -> Option<usize> {
            args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
        };
        let include_def = args
            .get("include_definition")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match name {
            "find_definitions" => {
                let Some(symbol) = get_str("symbol") else {
                    return tool_error("missing required argument: symbol".into());
                };
                let ctx = get_str("context_file").map(|s| self.resolve(&s));
                let result = self
                    .navigator
                    .goto_definition_by_name(&symbol, ctx.as_deref());
                tool_text(navigation_text(&result))
            }
            "find_references" => {
                let Some(symbol) = get_str("symbol") else {
                    return tool_error("missing required argument: symbol".into());
                };
                let ctx = get_str("context_file").map(|s| self.resolve(&s));
                let result =
                    self.navigator
                        .goto_references_by_name(&symbol, ctx.as_deref(), include_def);
                tool_text(navigation_text(&result))
            }
            "symbol_at_position" => {
                let (Some(file), Some(row), Some(col)) =
                    (get_str("file"), get_usize("row"), get_usize("col"))
                else {
                    return tool_error("missing required argument: file/row/col".into());
                };
                let path = self.resolve(&file);
                match self.navigator.get_symbol_at_position(&path, row, col) {
                    Ok(symbol) => {
                        let text = serde_json::to_string_pretty(&json!({ "symbol": symbol }))
                            .unwrap_or_else(|_| "{}".to_string());
                        tool_text(text)
                    }
                    Err(e) => tool_error(e.to_string()),
                }
            }
            "goto_definition" => {
                let (Some(file), Some(row), Some(col)) =
                    (get_str("file"), get_usize("row"), get_usize("col"))
                else {
                    return tool_error("missing required argument: file/row/col".into());
                };
                let path = self.resolve(&file);
                match self.navigator.goto_definition(&path, row, col) {
                    Ok(result) => tool_text(navigation_text(&result)),
                    Err(e) => tool_error(e.to_string()),
                }
            }
            "goto_references" => {
                let (Some(file), Some(row), Some(col)) =
                    (get_str("file"), get_usize("row"), get_usize("col"))
                else {
                    return tool_error("missing required argument: file/row/col".into());
                };
                let path = self.resolve(&file);
                match self
                    .navigator
                    .goto_references(&path, row, col, include_def)
                {
                    Ok(result) => tool_text(navigation_text(&result)),
                    Err(e) => tool_error(e.to_string()),
                }
            }
            "stats" => {
                let index = self.navigator.index();
                let (files, defs, refs) = index.stats();
                let aliases = index.alias_count();
                let top: Vec<Value> = index
                    .top_referenced_symbols(10)
                    .into_iter()
                    .map(|(name, count)| json!({ "symbol": name, "references": count }))
                    .collect();
                tool_text(
                    serde_json::to_string_pretty(&json!({
                        "files": files,
                        "definitions": defs,
                        "references": refs,
                        "aliases": aliases,
                        "top_referenced": top,
                    }))
                    .unwrap_or_else(|_| "{}".into()),
                )
            }
            other => tool_error(format!("unknown tool: {other}")),
        }
    }

    fn handle(&self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(|m| m.as_str())?;
        if is_notification(method) {
            return None;
        }
        let params = request.get("params").cloned().unwrap_or(json!({}));

        match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or(PROTOCOL_VERSION);
                Some(rpc_result(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
                    }),
                ))
            }
            "ping" => Some(rpc_result(&id, json!({}))),
            "tools/list" => Some(rpc_result(
                &id,
                json!({ "tools": tool_definitions() }),
            )),
            "tools/call" => {
                let (Some(name), Some(arguments)) = (
                    params.get("name").and_then(|n| n.as_str()),
                    params.get("arguments"),
                ) else {
                    return Some(rpc_error(&id, -32602, "missing name/arguments"));
                };
                let result = self.call_tool(name, arguments);
                Some(rpc_result(&id, result))
            }
            _ => Some(rpc_error(&id, -32601, &format!("method not found: {method}"))),
        }
    }
}

fn tool_text(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn parse_args() -> (PathBuf, PathBuf, bool) {
    let mut root = PathBuf::from(".");
    let mut cache: Option<PathBuf> = None;
    let mut force = false;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--force" => force = true,
            "--cache" => {
                if let Some(v) = iter.next() {
                    cache = Some(PathBuf::from(v));
                }
            }
            _ if arg.starts_with("--cache=") => {
                cache = Some(PathBuf::from(&arg["--cache=".len()..]));
            }
            _ if arg.starts_with('-') => {
                eprintln!("code-graph-mcp: unknown flag: {arg}");
                std::process::exit(2);
            }
            _ => root = PathBuf::from(arg),
        }
    }

    let root = root.canonicalize().unwrap_or_else(|e| {
        eprintln!("code-graph-mcp: invalid root '{}': {e}", root.display());
        std::process::exit(2);
    });
    let cache = cache.unwrap_or_else(|| get_cache_path(&root));
    (root, cache, force)
}

fn main() {
    let (root, cache, force) = parse_args();
    let index = load_or_build_index(&root, &cache, force);
    let navigator = Navigator::new(index);
    let server = Server { root, navigator };

    eprintln!(
        "code-graph-mcp: ready on stdio (root={})",
        server.root.display()
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(trimmed) else {
            // Malformed JSON-RPC: respond with a parse error (id must be null).
            let mut out = stdout.lock();
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string(&rpc_error(&Value::Null, -32700, "parse error")).unwrap()
            );
            let _ = out.flush();
            continue;
        };
        if let Some(response) = server.handle(&request) {
            let mut out = stdout.lock();
            if serde_json::to_writer(&mut out, &response).is_ok() {
                let _ = writeln!(out);
                let _ = out.flush();
            }
        }
    }
}
