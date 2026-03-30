//! MCP (Model Context Protocol) server for h5i.
//!
//! Implements the 2024-11-05 MCP specification over a newline-delimited
//! JSON-RPC 2.0 stdio transport.  Claude Code (or any MCP client) can point
//! at `h5i mcp` to gain native tool access to the h5i semantic layer.
//!
//! ## Tools exposed
//!
//! | Tool | h5i equivalent |
//! |------|----------------|
//! | `h5i_log` | `h5i log` |
//! | `h5i_blame` | `h5i blame` |
//! | `h5i_notes_show` | `h5i notes show` |
//! | `h5i_notes_uncertainty` | `h5i notes uncertainty` |
//! | `h5i_notes_coverage` | `h5i notes coverage` |
//! | `h5i_notes_review` | `h5i notes review` |
//! | `h5i_notes_churn` | `h5i notes churn` |
//! | `h5i_context_init` | `h5i context init` |
//! | `h5i_context_trace` | `h5i context trace` |
//! | `h5i_context_commit` | `h5i context commit` |
//! | `h5i_context_branch` | `h5i context branch` |
//! | `h5i_context_checkout` | `h5i context checkout` |
//! | `h5i_context_merge` | `h5i context merge` |
//! | `h5i_context_show` | `h5i context show` |
//! | `h5i_context_status` | `h5i context status` |
//!
//! ## Resources exposed
//!
//! | URI | Content |
//! |-----|---------|
//! | `h5i://context/current` | Live `GccContext` JSON (replaces `h5i context prompt`) |
//! | `h5i://log/recent` | 10 most recent commits with AI provenance |

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::blame::BlameMode;
use crate::ctx::{self, ContextOpts};
use crate::repository::H5iRepository;
use crate::session_log;

// ── JSON-RPC 2.0 types ────────────────────────────────────────────────────────

/// An incoming JSON-RPC 2.0 request or notification.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// `None` for notifications (no response expected).
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// An outgoing JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    /// Successful response.
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response.
    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// ── MCP protocol version ──────────────────────────────────────────────────────

const PROTOCOL_VERSION: &str = "2024-11-05";

// ── Tool catalogue ────────────────────────────────────────────────────────────

/// Return the full list of MCP tool definitions.
pub fn tool_definitions() -> Value {
    json!([
        // ── log ──────────────────────────────────────────────────────────────
        {
            "name": "h5i_log",
            "description": "Return recent commits enriched with AI provenance metadata \
                (model, agent, prompt, token count) and test metrics. \
                Use this before editing a file to understand what AI work has already \
                been done and why.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of commits to return (default 20)."
                    }
                }
            }
        },
        // ── blame ─────────────────────────────────────────────────────────────
        {
            "name": "h5i_blame",
            "description": "Show per-line authorship for a file, enriched with AI metadata \
                (model, prompt) and test pass/fail status at the time of each commit. \
                Use this to understand which lines were written by AI vs humans, and \
                which prompts produced them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative path to the file to blame."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["line", "ast"],
                        "description": "Blame granularity: 'line' (default) or 'ast' for \
                            semantic node-level attribution."
                    }
                },
                "required": ["file"]
            }
        },
        // ── notes ─────────────────────────────────────────────────────────────
        {
            "name": "h5i_notes_show",
            "description": "Show the full session analysis for a commit: exploration \
                footprint (files read vs edited), causal chain (trigger → decisions → \
                edits), omissions, and file coverage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commit": {
                        "type": "string",
                        "description": "Commit OID or prefix to inspect (default: HEAD)."
                    }
                }
            }
        },
        {
            "name": "h5i_notes_uncertainty",
            "description": "List moments where the AI expressed uncertainty in its thinking \
                blocks during a session (phrases like 'not sure', 'might break', \
                'need to verify'). Each entry includes a confidence score, context \
                snippet, and file being edited at the time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commit": {
                        "type": "string",
                        "description": "Commit OID or prefix (default: HEAD)."
                    },
                    "file": {
                        "type": "string",
                        "description": "Filter to uncertainties expressed while editing \
                            a specific file path."
                    }
                }
            }
        },
        {
            "name": "h5i_notes_coverage",
            "description": "Show per-file attention coverage: which files were edited \
                without being read first (blind edits). High blind-edit counts are a \
                signal that the AI modified a file from memory rather than reading its \
                current state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commit": {
                        "type": "string",
                        "description": "Commit OID or prefix (default: HEAD)."
                    },
                    "max_ratio": {
                        "type": "number",
                        "description": "Only return files with read-before-edit ratio \
                            at or below this value (0.0–1.0). Omit to return all files."
                    }
                }
            }
        },
        {
            "name": "h5i_notes_review",
            "description": "Return commits ranked by review worthiness. Scoring is \
                deterministic and based on signals such as: large diff, high uncertainty \
                expressions, blind edits, AI-only authorship, no test coverage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Number of recent commits to scan (default 50)."
                    },
                    "min_score": {
                        "type": "number",
                        "description": "Minimum review score threshold, 0.0–1.0 \
                            (default 0.4). Lower values return more commits."
                    }
                }
            }
        },
        {
            "name": "h5i_notes_churn",
            "description": "Return aggregate file churn statistics across all analyzed \
                sessions: edit count, read count, and churn score per file. High churn \
                scores indicate files that are frequently edited relative to how often \
                they are read — a fragility signal.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        // ── context ───────────────────────────────────────────────────────────
        {
            "name": "h5i_context_init",
            "description": "Initialize the h5i reasoning workspace for this project. \
                Call once at the start of a major task to set the project goal. \
                Safe to call again to update the goal.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "High-level goal for this reasoning session \
                            (e.g. 'refactor HTTP client to support retries')."
                    }
                }
            }
        },
        {
            "name": "h5i_context_trace",
            "description": "Append an OTA (Observe/Think/Act/Note) step to the current \
                reasoning trace. Use this to record observations about the codebase, \
                design decisions, and actions taken. Auto-initializes the workspace \
                if it does not yet exist.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["OBSERVE", "THINK", "ACT", "NOTE"],
                        "description": "Type of trace entry."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content of the trace step."
                    }
                },
                "required": ["kind", "content"]
            }
        },
        {
            "name": "h5i_context_commit",
            "description": "Checkpoint the current reasoning progress with a summary \
                milestone. Analogous to `git commit` but for the reasoning workspace. \
                Auto-initializes the workspace if needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "One-line summary of progress achieved at this \
                            checkpoint."
                    },
                    "detail": {
                        "type": "string",
                        "description": "Optional detailed description of decisions made \
                            and work completed."
                    }
                },
                "required": ["summary"]
            }
        },
        {
            "name": "h5i_context_branch",
            "description": "Create a new reasoning branch to explore an alternative \
                approach without losing the current thread. Analogous to `git branch`. \
                Auto-initializes the workspace if needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Branch name (e.g. 'experiment/sync-retry')."
                    },
                    "purpose": {
                        "type": "string",
                        "description": "Why this branch is being explored."
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "h5i_context_checkout",
            "description": "Switch to an existing reasoning branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the branch to switch to."
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "h5i_context_merge",
            "description": "Merge a completed reasoning branch back into the current \
                branch, synthesizing findings from both threads.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branch": {
                        "type": "string",
                        "description": "Name of the branch to merge from."
                    }
                },
                "required": ["branch"]
            }
        },
        {
            "name": "h5i_context_show",
            "description": "Return the current context workspace state as structured JSON: \
                project goal, milestones, active branches, recent checkpoint summaries, \
                and (optionally) recent trace lines.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branch": {
                        "type": "string",
                        "description": "Branch to inspect (default: current active branch)."
                    },
                    "window": {
                        "type": "integer",
                        "description": "Number of recent checkpoints to include (default 3)."
                    },
                    "trace": {
                        "type": "boolean",
                        "description": "Include recent OTA trace lines (default false)."
                    }
                }
            }
        },
        {
            "name": "h5i_context_status",
            "description": "Return a compact summary of the reasoning workspace: \
                initialization state, active branch, all branch names, and branch count.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }
    ])
}

// ── Resource catalogue ────────────────────────────────────────────────────────

/// Return the full list of MCP resource definitions.
pub fn resource_definitions() -> Value {
    json!([
        {
            "uri": "h5i://context/current",
            "name": "Current Reasoning Context",
            "description": "The live h5i context workspace state: project goal, \
                milestones, current branch, recent checkpoint summaries, and OTA trace. \
                Replaces `h5i context prompt` — inject this resource at session start \
                for full context continuity.",
            "mimeType": "application/json"
        },
        {
            "uri": "h5i://log/recent",
            "name": "Recent Commits",
            "description": "The 10 most recent commits enriched with AI provenance \
                metadata (model, agent, prompt, token count) and test metrics.",
            "mimeType": "application/json"
        }
    ])
}

// ── Tool helpers ──────────────────────────────────────────────────────────────

/// Wrap a plain string as an MCP text content block.
fn text_content(text: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }]
    })
}

/// Wrap a serialisable value as an MCP text content block (JSON-encoded).
fn json_content(v: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": v.to_string() }]
    })
}

/// Resolve a caller-supplied commit OID string (or None → HEAD) to a full OID
/// string using the repository.
fn resolve_oid(repo: &H5iRepository, commit: Option<&str>) -> Result<String> {
    match commit {
        Some(oid) => Ok(oid.to_string()),
        None => {
            let head = repo.git().head()?.peel_to_commit()?;
            Ok(head.id().to_string())
        }
    }
}

// ── Tool handlers ─────────────────────────────────────────────────────────────

fn tool_log(params: &Value, workdir: &Path) -> Result<Value> {
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20) as usize;
    let repo = H5iRepository::open(workdir)?;
    let records = repo.get_log(limit)?;
    Ok(json_content(serde_json::to_value(&records)?))
}

fn tool_blame(params: &Value, workdir: &Path) -> Result<Value> {
    let file = params
        .get("file")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required param: file"))?;
    let mode = match params.get("mode").and_then(Value::as_str).unwrap_or("line") {
        "ast" => BlameMode::Ast,
        _ => BlameMode::Line,
    };
    let repo = H5iRepository::open(workdir)?;
    let results = repo.blame(Path::new(file), mode)?;
    Ok(json_content(serde_json::to_value(&results)?))
}

fn tool_notes_show(params: &Value, workdir: &Path) -> Result<Value> {
    let repo = H5iRepository::open(workdir)?;
    let oid = resolve_oid(&repo, params.get("commit").and_then(Value::as_str))?;
    match session_log::load_analysis(repo.h5i_path(), &oid)? {
        Some(analysis) => Ok(json_content(serde_json::to_value(&analysis)?)),
        None => Ok(text_content(format!("No session notes found for {:.8}", oid))),
    }
}

fn tool_notes_uncertainty(params: &Value, workdir: &Path) -> Result<Value> {
    let repo = H5iRepository::open(workdir)?;
    let oid = resolve_oid(&repo, params.get("commit").and_then(Value::as_str))?;
    let file_filter = params.get("file").and_then(Value::as_str);
    match session_log::load_analysis(repo.h5i_path(), &oid)? {
        Some(analysis) => {
            let filtered: Vec<_> = analysis
                .uncertainty
                .iter()
                .filter(|u| {
                    file_filter
                        .map(|f| u.context_file.contains(f))
                        .unwrap_or(true)
                })
                .collect();
            Ok(json_content(serde_json::to_value(filtered)?))
        }
        None => Ok(text_content(format!("No session notes found for {:.8}", oid))),
    }
}

fn tool_notes_coverage(params: &Value, workdir: &Path) -> Result<Value> {
    let repo = H5iRepository::open(workdir)?;
    let oid = resolve_oid(&repo, params.get("commit").and_then(Value::as_str))?;
    let max_ratio = params.get("max_ratio").and_then(Value::as_f64);
    match session_log::load_analysis(repo.h5i_path(), &oid)? {
        Some(analysis) => {
            let filtered: Vec<_> = analysis
                .coverage
                .iter()
                .filter(|fc| {
                    max_ratio
                        .map(|max| fc.read_before_edit_ratio as f64 <= max)
                        .unwrap_or(true)
                })
                .collect();
            Ok(json_content(serde_json::to_value(filtered)?))
        }
        None => Ok(text_content(format!("No session notes found for {:.8}", oid))),
    }
}

fn tool_notes_review(params: &Value, workdir: &Path) -> Result<Value> {
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50) as usize;
    let min_score = params
        .get("min_score")
        .and_then(Value::as_f64)
        .unwrap_or(0.4) as f32;
    let repo = H5iRepository::open(workdir)?;
    let points = repo.suggest_review_points(limit, min_score)?;
    Ok(json_content(serde_json::to_value(&points)?))
}

fn tool_notes_churn(_params: &Value, workdir: &Path) -> Result<Value> {
    let repo = H5iRepository::open(workdir)?;
    let churn = session_log::aggregate_churn(repo.h5i_path());
    Ok(json_content(serde_json::to_value(churn)?))
}

fn tool_context_init(params: &Value, workdir: &Path) -> Result<Value> {
    let goal = params.get("goal").and_then(Value::as_str).unwrap_or("");
    ctx::init(workdir, goal)?;
    Ok(text_content("Context workspace initialized."))
}

fn tool_context_trace(params: &Value, workdir: &Path) -> Result<Value> {
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required param: kind"))?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required param: content"))?;
    // Auto-initialize so callers don't need a separate init step.
    if !ctx::is_initialized(workdir) {
        ctx::init(workdir, "")?;
    }
    ctx::append_log(workdir, kind, content)?;
    Ok(text_content(format!("[{}] {}", kind, content)))
}

fn tool_context_commit(params: &Value, workdir: &Path) -> Result<Value> {
    let summary = params
        .get("summary")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required param: summary"))?;
    let detail = params
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or(summary);
    if !ctx::is_initialized(workdir) {
        ctx::init(workdir, "")?;
    }
    ctx::gcc_commit(workdir, summary, detail)?;
    Ok(text_content(format!("Checkpoint saved: {}", summary)))
}

fn tool_context_branch(params: &Value, workdir: &Path) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required param: name"))?;
    let purpose = params.get("purpose").and_then(Value::as_str).unwrap_or("");
    if !ctx::is_initialized(workdir) {
        ctx::init(workdir, "")?;
    }
    ctx::gcc_branch(workdir, name, purpose)?;
    Ok(text_content(format!("Branch '{}' created and checked out.", name)))
}

fn tool_context_checkout(params: &Value, workdir: &Path) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required param: name"))?;
    ctx::gcc_checkout(workdir, name)?;
    Ok(text_content(format!("Switched to branch '{}'.", name)))
}

fn tool_context_merge(params: &Value, workdir: &Path) -> Result<Value> {
    let branch = params
        .get("branch")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required param: branch"))?;
    let summary = ctx::gcc_merge(workdir, branch)?;
    Ok(text_content(format!("Merged '{}': {}", branch, summary)))
}

fn tool_context_show(params: &Value, workdir: &Path) -> Result<Value> {
    if !ctx::is_initialized(workdir) {
        return Ok(text_content(
            "Context workspace not initialized. Call h5i_context_init first.",
        ));
    }
    let opts = ContextOpts {
        branch: params
            .get("branch")
            .and_then(Value::as_str)
            .map(str::to_string),
        commit_hash: None,
        show_log: params
            .get("trace")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        log_offset: 0,
        metadata_segment: None,
        window: params
            .get("window")
            .and_then(Value::as_u64)
            .unwrap_or(3) as usize,
    };
    let context = ctx::gcc_context(workdir, &opts)?;
    Ok(json_content(serde_json::to_value(&context)?))
}

fn tool_context_status(_params: &Value, workdir: &Path) -> Result<Value> {
    if !ctx::is_initialized(workdir) {
        return Ok(text_content("Context workspace not initialized."));
    }
    let current = ctx::current_branch(workdir);
    let branches = ctx::list_branches(workdir);
    let branch_count = branches.len();
    Ok(json_content(json!({
        "initialized": true,
        "current_branch": current,
        "branches": branches,
        "branch_count": branch_count
    })))
}

// ── Tool call dispatch ────────────────────────────────────────────────────────

/// Dispatch a `tools/call` invocation to the appropriate handler.
///
/// Returns `Err` only for unknown tool names — individual tool errors are
/// wrapped in an `isError: true` MCP content response by the caller.
pub fn call_tool(name: &str, params: &Value, workdir: &Path) -> Result<Value> {
    match name {
        "h5i_log" => tool_log(params, workdir),
        "h5i_blame" => tool_blame(params, workdir),
        "h5i_notes_show" => tool_notes_show(params, workdir),
        "h5i_notes_uncertainty" => tool_notes_uncertainty(params, workdir),
        "h5i_notes_coverage" => tool_notes_coverage(params, workdir),
        "h5i_notes_review" => tool_notes_review(params, workdir),
        "h5i_notes_churn" => tool_notes_churn(params, workdir),
        "h5i_context_init" => tool_context_init(params, workdir),
        "h5i_context_trace" => tool_context_trace(params, workdir),
        "h5i_context_commit" => tool_context_commit(params, workdir),
        "h5i_context_branch" => tool_context_branch(params, workdir),
        "h5i_context_checkout" => tool_context_checkout(params, workdir),
        "h5i_context_merge" => tool_context_merge(params, workdir),
        "h5i_context_show" => tool_context_show(params, workdir),
        "h5i_context_status" => tool_context_status(params, workdir),
        other => anyhow::bail!("Unknown tool: {}", other),
    }
}

// ── Resource handlers ─────────────────────────────────────────────────────────

/// Read a named MCP resource by URI.
pub fn read_resource(uri: &str, workdir: &Path) -> Result<Value> {
    match uri {
        "h5i://context/current" => {
            if !ctx::is_initialized(workdir) {
                return Ok(json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": "application/json",
                        "text": "{\"initialized\":false}"
                    }]
                }));
            }
            let opts = ContextOpts {
                branch: None,
                commit_hash: None,
                show_log: true,
                log_offset: 0,
                metadata_segment: None,
                window: 5,
            };
            let context = ctx::gcc_context(workdir, &opts)?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string(&context)?
                }]
            }))
        }
        "h5i://log/recent" => {
            let repo = H5iRepository::open(workdir)?;
            let records = repo.get_log(10)?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string(&records)?
                }]
            }))
        }
        other => anyhow::bail!("Unknown resource URI: {}", other),
    }
}

// ── Request handler ───────────────────────────────────────────────────────────

/// Process one JSON-RPC request and return a response (or `None` for
/// notifications, which must not be answered).
pub fn handle_request(req: JsonRpcRequest, workdir: &Path) -> Option<JsonRpcResponse> {
    match req.method.as_str() {
        // ── Notifications (no response) ───────────────────────────────────────
        "notifications/initialized" | "notifications/cancelled" => None,

        // ── MCP lifecycle ─────────────────────────────────────────────────────
        "initialize" => Some(JsonRpcResponse::ok(
            req.id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {},
                    "resources": {}
                },
                "serverInfo": {
                    "name": "h5i",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )),

        // ── Tool discovery ────────────────────────────────────────────────────
        "tools/list" => Some(JsonRpcResponse::ok(
            req.id,
            json!({ "tools": tool_definitions() }),
        )),

        // ── Resource discovery ────────────────────────────────────────────────
        "resources/list" => Some(JsonRpcResponse::ok(
            req.id,
            json!({ "resources": resource_definitions() }),
        )),

        // ── Resource read ─────────────────────────────────────────────────────
        "resources/read" => {
            let uri = match req.params.get("uri").and_then(Value::as_str) {
                Some(u) => u,
                None => {
                    return Some(JsonRpcResponse::err(
                        req.id,
                        -32602,
                        "missing param: uri",
                    ))
                }
            };
            match read_resource(uri, workdir) {
                Ok(result) => Some(JsonRpcResponse::ok(req.id, result)),
                Err(e) => Some(JsonRpcResponse::err(req.id, -32603, e.to_string())),
            }
        }

        // ── Tool call ─────────────────────────────────────────────────────────
        "tools/call" => {
            let name = match req.params.get("name").and_then(Value::as_str) {
                Some(n) => n,
                None => {
                    return Some(JsonRpcResponse::err(
                        req.id,
                        -32602,
                        "missing param: name",
                    ))
                }
            };
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_tool(name, &args, workdir) {
                Ok(result) => Some(JsonRpcResponse::ok(req.id, result)),
                // MCP spec: tool errors are content with isError:true, not
                // JSON-RPC errors, so the client can display them gracefully.
                Err(e) => Some(JsonRpcResponse::ok(
                    req.id,
                    json!({
                        "content": [{ "type": "text", "text": format!("Error: {}", e) }],
                        "isError": true
                    }),
                )),
            }
        }

        // ── Utility ───────────────────────────────────────────────────────────
        "ping" => Some(JsonRpcResponse::ok(req.id, json!({}))),

        // ── Unknown ───────────────────────────────────────────────────────────
        other => Some(JsonRpcResponse::err(
            req.id,
            -32601,
            format!("Method not found: {}", other),
        )),
    }
}

// ── stdio transport ───────────────────────────────────────────────────────────

/// Run the MCP server on stdin/stdout.
///
/// Reads newline-delimited JSON-RPC 2.0 messages from stdin and writes
/// responses to stdout.  All log output goes to stderr so it does not
/// contaminate the protocol stream.
pub fn run_stdio(workdir: PathBuf) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(e) => {
                eprintln!("h5i-mcp: read error: {}", e);
                break;
            }
        };

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::err(None, -32700, format!("Parse error: {}", e));
                writeln!(out, "{}", serde_json::to_string(&resp)?)?;
                out.flush()?;
                continue;
            }
        };

        if let Some(resp) = handle_request(req, &workdir) {
            writeln!(out, "{}", serde_json::to_string(&resp)?)?;
            out.flush()?;
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Create a minimal git repo with a real initial commit so that HEAD and
    /// `H5iRepository::open` work.
    fn make_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();

        let repo = git2::Repository::init(&path).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
        }
        // Write a file and make an initial commit so HEAD resolves.
        fs::write(path.join("hello.rs"), "fn main() {}").unwrap();
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("hello.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
            .unwrap();

        (dir, path)
    }

    fn make_req(method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        }
    }

    // ── JSON-RPC response construction ────────────────────────────────────────

    #[test]
    fn response_ok_sets_result_clears_error() {
        let r = JsonRpcResponse::ok(Some(json!(1)), json!({"a": 1}));
        assert!(r.result.is_some());
        assert!(r.error.is_none());
        assert_eq!(r.jsonrpc, "2.0");
        assert_eq!(r.id, Some(json!(1)));
    }

    #[test]
    fn response_err_sets_error_clears_result() {
        let r = JsonRpcResponse::err(Some(json!(2)), -32600, "bad");
        assert!(r.result.is_none());
        assert!(r.error.is_some());
        let e = r.error.unwrap();
        assert_eq!(e.code, -32600);
        assert_eq!(e.message, "bad");
    }

    #[test]
    fn response_serialization_omits_null_fields() {
        let r = JsonRpcResponse::ok(None, json!(42));
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"error\""), "error field must be absent");
        // id: None is also skipped
        assert!(!s.contains("\"id\""), "null id must be absent");
    }

    // ── Parse error handling ──────────────────────────────────────────────────

    #[test]
    fn parse_error_on_invalid_json() {
        let (_dir, path) = make_repo();
        // Simulate what the stdio loop does on a bad line.
        let bad = "not json at all {{";
        let result: Result<JsonRpcRequest, _> = serde_json::from_str(bad);
        assert!(result.is_err());
        // The loop would emit a -32700 response.
        let resp = JsonRpcResponse::err(None, -32700, "Parse error");
        assert_eq!(resp.error.unwrap().code, -32700);
        let _ = path; // keep tempdir alive
    }

    // ── initialize ───────────────────────────────────────────────────────────

    #[test]
    fn initialize_returns_protocol_version_and_capabilities() {
        let (_dir, path) = make_repo();
        let resp = handle_request(make_req("initialize", json!({})), &path).unwrap();
        let r = resp.result.unwrap();
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["serverInfo"]["name"], "h5i");
        assert!(r["capabilities"]["tools"].is_object());
        assert!(r["capabilities"]["resources"].is_object());
    }

    // ── notifications ─────────────────────────────────────────────────────────

    #[test]
    fn notifications_return_no_response() {
        let (_dir, path) = make_repo();
        for method in &["notifications/initialized", "notifications/cancelled"] {
            let req = JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: None,
                method: method.to_string(),
                params: json!({}),
            };
            assert!(
                handle_request(req, &path).is_none(),
                "{} must return None",
                method
            );
        }
    }

    // ── tools/list ────────────────────────────────────────────────────────────

    #[test]
    fn tools_list_includes_all_expected_tools() {
        let (_dir, path) = make_repo();
        let resp = handle_request(make_req("tools/list", json!({})), &path).unwrap();
        let tools = resp.result.unwrap()["tools"].clone();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        let expected = [
            "h5i_log",
            "h5i_blame",
            "h5i_notes_show",
            "h5i_notes_uncertainty",
            "h5i_notes_coverage",
            "h5i_notes_review",
            "h5i_notes_churn",
            "h5i_context_init",
            "h5i_context_trace",
            "h5i_context_commit",
            "h5i_context_branch",
            "h5i_context_checkout",
            "h5i_context_merge",
            "h5i_context_show",
            "h5i_context_status",
        ];
        for name in &expected {
            assert!(names.contains(name), "missing tool: {}", name);
        }
        assert_eq!(names.len(), expected.len(), "unexpected extra tools");
    }

    #[test]
    fn every_tool_has_description_and_object_schema() {
        let tools = tool_definitions();
        for tool in tools.as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            assert!(
                !tool["description"].as_str().unwrap_or("").is_empty(),
                "{}: empty description",
                name
            );
            assert_eq!(
                tool["inputSchema"]["type"].as_str(),
                Some("object"),
                "{}: inputSchema.type must be 'object'",
                name
            );
        }
    }

    // ── resources/list ────────────────────────────────────────────────────────

    #[test]
    fn resources_list_includes_context_and_log() {
        let (_dir, path) = make_repo();
        let resp = handle_request(make_req("resources/list", json!({})), &path).unwrap();
        let resources = resp.result.unwrap()["resources"].clone();
        let uris: Vec<&str> = resources
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"h5i://context/current"));
        assert!(uris.contains(&"h5i://log/recent"));
    }

    // ── ping ──────────────────────────────────────────────────────────────────

    #[test]
    fn ping_returns_empty_object() {
        let (_dir, path) = make_repo();
        let resp = handle_request(make_req("ping", json!({})), &path).unwrap();
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap(), json!({}));
    }

    // ── unknown method ────────────────────────────────────────────────────────

    #[test]
    fn unknown_method_returns_method_not_found() {
        let (_dir, path) = make_repo();
        let resp = handle_request(make_req("no/such/method", json!({})), &path).unwrap();
        let e = resp.error.unwrap();
        assert_eq!(e.code, -32601);
        assert!(e.message.contains("not found"));
    }

    // ── tools/call: validation ────────────────────────────────────────────────

    #[test]
    fn tool_call_missing_name_returns_32602() {
        let (_dir, path) = make_repo();
        let resp = handle_request(
            make_req("tools/call", json!({"arguments": {}})),
            &path,
        )
        .unwrap();
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[test]
    fn tool_call_unknown_tool_returns_is_error() {
        let (_dir, path) = make_repo();
        let resp = handle_request(
            make_req(
                "tools/call",
                json!({"name": "nonexistent_tool", "arguments": {}}),
            ),
            &path,
        )
        .unwrap();
        // Unknown tool → isError:true in content, NOT a JSON-RPC error.
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["isError"], true);
    }

    // ── resources/read: validation ────────────────────────────────────────────

    #[test]
    fn resource_read_missing_uri_returns_32602() {
        let (_dir, path) = make_repo();
        let resp = handle_request(make_req("resources/read", json!({})), &path).unwrap();
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[test]
    fn resource_read_unknown_uri_returns_32603() {
        let (_dir, path) = make_repo();
        let resp = handle_request(
            make_req("resources/read", json!({"uri": "h5i://nope"})),
            &path,
        )
        .unwrap();
        assert_eq!(resp.error.unwrap().code, -32603);
    }

    // ── h5i_blame ─────────────────────────────────────────────────────────────

    #[test]
    fn blame_missing_file_param_is_error() {
        let (_dir, path) = make_repo();
        let result = tool_blame(&json!({}), &path);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("missing required param: file")
        );
    }

    // ── h5i_context_init ──────────────────────────────────────────────────────

    #[test]
    fn context_init_creates_workspace() {
        let (_dir, path) = make_repo();
        let r = tool_context_init(&json!({"goal": "build MCP server"}), &path);
        assert!(r.is_ok(), "{:?}", r);
        assert!(ctx::is_initialized(&path));
    }

    #[test]
    fn context_init_without_goal_still_succeeds() {
        let (_dir, path) = make_repo();
        let r = tool_context_init(&json!({}), &path);
        assert!(r.is_ok(), "{:?}", r);
        assert!(ctx::is_initialized(&path));
    }

    // ── h5i_context_trace ─────────────────────────────────────────────────────

    #[test]
    fn context_trace_auto_inits_and_echoes_entry() {
        let (_dir, path) = make_repo();
        assert!(!ctx::is_initialized(&path));
        let r =
            tool_context_trace(&json!({"kind": "OBSERVE", "content": "hello world"}), &path);
        assert!(r.is_ok(), "{:?}", r);
        assert!(ctx::is_initialized(&path));
        let text = r.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.contains("OBSERVE"));
        assert!(text.contains("hello world"));
    }

    #[test]
    fn context_trace_missing_kind_is_error() {
        let (_dir, path) = make_repo();
        let r = tool_context_trace(&json!({"content": "no kind here"}), &path);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("kind"));
    }

    #[test]
    fn context_trace_missing_content_is_error() {
        let (_dir, path) = make_repo();
        let r = tool_context_trace(&json!({"kind": "ACT"}), &path);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("content"));
    }

    // ── h5i_context_commit ────────────────────────────────────────────────────

    #[test]
    fn context_commit_auto_inits_and_records_summary() {
        let (_dir, path) = make_repo();
        let r = tool_context_commit(&json!({"summary": "analyzed modules"}), &path);
        assert!(r.is_ok(), "{:?}", r);
        let text = r.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.contains("analyzed modules"));
    }

    #[test]
    fn context_commit_missing_summary_is_error() {
        let (_dir, path) = make_repo();
        let r = tool_context_commit(&json!({"detail": "some detail"}), &path);
        assert!(r.is_err());
    }

    #[test]
    fn context_commit_uses_detail_when_provided() {
        let (_dir, path) = make_repo();
        let r = tool_context_commit(
            &json!({"summary": "summary text", "detail": "detailed description"}),
            &path,
        );
        assert!(r.is_ok(), "{:?}", r);
    }

    // ── h5i_context_branch ────────────────────────────────────────────────────

    #[test]
    fn context_branch_creates_branch_in_workspace() {
        let (_dir, path) = make_repo();
        tool_context_init(&json!({"goal": "test"}), &path).unwrap();
        let r = tool_context_branch(
            &json!({"name": "experiment/alt", "purpose": "try alternative"}),
            &path,
        );
        assert!(r.is_ok(), "{:?}", r);
        let branches = ctx::list_branches(&path);
        assert!(branches.contains(&"experiment/alt".to_string()));
    }

    #[test]
    fn context_branch_missing_name_is_error() {
        let (_dir, path) = make_repo();
        let r = tool_context_branch(&json!({"purpose": "no name"}), &path);
        assert!(r.is_err());
    }

    #[test]
    fn context_branch_auto_inits() {
        let (_dir, path) = make_repo();
        assert!(!ctx::is_initialized(&path));
        let r = tool_context_branch(&json!({"name": "feature/x"}), &path);
        assert!(r.is_ok(), "{:?}", r);
        assert!(ctx::is_initialized(&path));
    }

    // ── h5i_context_checkout ──────────────────────────────────────────────────

    #[test]
    fn context_checkout_switches_active_branch() {
        let (_dir, path) = make_repo();
        tool_context_init(&json!({"goal": "test"}), &path).unwrap();
        tool_context_branch(&json!({"name": "feature/y"}), &path).unwrap();
        // currently on feature/y after branch creation; switch back to main
        let r = tool_context_checkout(&json!({"name": "main"}), &path);
        assert!(r.is_ok(), "{:?}", r);
        assert_eq!(ctx::current_branch(&path), "main");
    }

    #[test]
    fn context_checkout_missing_name_is_error() {
        let (_dir, path) = make_repo();
        let r = tool_context_checkout(&json!({}), &path);
        assert!(r.is_err());
    }

    // ── h5i_context_merge ─────────────────────────────────────────────────────

    #[test]
    fn context_merge_missing_branch_is_error() {
        let (_dir, path) = make_repo();
        let r = tool_context_merge(&json!({}), &path);
        assert!(r.is_err());
    }

    // ── h5i_context_show ──────────────────────────────────────────────────────

    #[test]
    fn context_show_not_initialized_returns_message() {
        let (_dir, path) = make_repo();
        let r = tool_context_show(&json!({}), &path);
        assert!(r.is_ok());
        let text = r.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.contains("not initialized"));
    }

    #[test]
    fn context_show_returns_gcc_context_json() {
        let (_dir, path) = make_repo();
        tool_context_init(&json!({"goal": "show goal"}), &path).unwrap();
        let r = tool_context_show(&json!({"window": 2, "trace": true}), &path);
        assert!(r.is_ok(), "{:?}", r);
        let text = r.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["project_goal"], "show goal");
        assert!(v["current_branch"].is_string());
        assert!(v["active_branches"].is_array());
    }

    // ── h5i_context_status ────────────────────────────────────────────────────

    #[test]
    fn context_status_not_initialized_returns_message() {
        let (_dir, path) = make_repo();
        let r = tool_context_status(&json!({}), &path);
        assert!(r.is_ok());
        let text = r.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.contains("not initialized"));
    }

    #[test]
    fn context_status_after_init_returns_structured_json() {
        let (_dir, path) = make_repo();
        tool_context_init(&json!({"goal": "status test"}), &path).unwrap();
        let r = tool_context_status(&json!({}), &path);
        assert!(r.is_ok(), "{:?}", r);
        let text = r.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["initialized"], true);
        assert_eq!(v["current_branch"], "main");
        assert!(v["branch_count"].as_u64().unwrap() >= 1);
    }

    // ── resources/read ────────────────────────────────────────────────────────

    #[test]
    fn resource_context_current_not_initialized_returns_false_flag() {
        let (_dir, path) = make_repo();
        let r = read_resource("h5i://context/current", &path);
        assert!(r.is_ok(), "{:?}", r);
        let text = r.unwrap()["contents"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["initialized"], false);
    }

    #[test]
    fn resource_context_current_initialized_returns_gcc_context() {
        let (_dir, path) = make_repo();
        tool_context_init(&json!({"goal": "resource goal"}), &path).unwrap();
        let r = read_resource("h5i://context/current", &path);
        assert!(r.is_ok(), "{:?}", r);
        let val = r.unwrap();
        assert_eq!(val["contents"][0]["uri"], "h5i://context/current");
        assert_eq!(val["contents"][0]["mimeType"], "application/json");
        // The text should parse as GccContext
        let text = val["contents"][0]["text"].as_str().unwrap();
        let ctx_val: Value = serde_json::from_str(text).unwrap();
        assert_eq!(ctx_val["project_goal"], "resource goal");
    }

    #[test]
    fn resource_unknown_uri_is_error() {
        let (_dir, path) = make_repo();
        let r = read_resource("h5i://nonexistent/path", &path);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("Unknown resource"));
    }

    // ── Full context workflow ─────────────────────────────────────────────────

    #[test]
    fn full_context_workflow() {
        let (_dir, path) = make_repo();

        // init
        tool_context_init(&json!({"goal": "implement MCP server"}), &path).unwrap();

        // trace multiple steps
        for (kind, content) in &[
            ("OBSERVE", "codebase has server.rs already"),
            ("THINK", "stdio is simpler than HTTP for a CLI tool"),
            ("ACT", "created src/mcp.rs"),
        ] {
            tool_context_trace(&json!({"kind": kind, "content": content}), &path).unwrap();
        }

        // checkpoint
        tool_context_commit(
            &json!({"summary": "planned MCP layout", "detail": "identified all modules"}),
            &path,
        )
        .unwrap();

        // branch for an experiment
        tool_context_branch(
            &json!({"name": "experiment/http", "purpose": "try HTTP transport"}),
            &path,
        )
        .unwrap();
        tool_context_trace(
            &json!({"kind": "THINK", "content": "HTTP adds complexity for no gain"}),
            &path,
        )
        .unwrap();

        // return to main
        tool_context_checkout(&json!({"name": "main"}), &path).unwrap();
        assert_eq!(ctx::current_branch(&path), "main");

        // verify show
        let show = tool_context_show(&json!({"trace": true, "window": 5}), &path).unwrap();
        let text = show["content"][0]["text"].as_str().unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["project_goal"], "implement MCP server");
        assert!(v["active_branches"].as_array().unwrap().len() >= 2);

        // verify status
        let status = tool_context_status(&json!({}), &path).unwrap();
        let s_text = status["content"][0]["text"].as_str().unwrap();
        let sv: Value = serde_json::from_str(s_text).unwrap();
        assert_eq!(sv["initialized"], true);
        assert_eq!(sv["current_branch"], "main");
        assert!(sv["branch_count"].as_u64().unwrap() >= 2);

        // verify resource is consistent
        let res = read_resource("h5i://context/current", &path).unwrap();
        let res_text = res["contents"][0]["text"].as_str().unwrap();
        let rv: Value = serde_json::from_str(res_text).unwrap();
        assert_eq!(rv["project_goal"], "implement MCP server");
    }
}
