use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::{Html, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::memory;
use crate::metadata::{IntegrityReport, IntentGraph};
use crate::repository::H5iRepository;
use crate::review::{ReviewPoint, REVIEW_THRESHOLD};

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub repo_path: PathBuf,
}

// ── API response types ────────────────────────────────────────────────────────

#[derive(Serialize, Default)]
pub struct EnrichedCommit {
    pub git_oid: String,
    pub short_oid: String,
    pub message: String,
    pub author: String,
    pub timestamp: String,
    // AI provenance
    pub ai_model: Option<String>,
    pub ai_agent: Option<String>,
    pub ai_prompt: Option<String>,
    pub ai_tokens: Option<usize>,
    // Test metrics — legacy field kept for backward-compat with old notes
    pub test_coverage: Option<f64>,
    // Test metrics — rich fields (populated when adapter JSON was used)
    pub test_passed: Option<u64>,
    pub test_failed: Option<u64>,
    pub test_skipped: Option<u64>,
    pub test_total: Option<u64>,
    pub test_duration_secs: Option<f64>,
    pub test_tool: Option<String>,
    pub test_exit_code: Option<i32>,
    pub test_summary: Option<String>,
    pub test_is_passing: Option<bool>,
    // Structural / collaborative
    pub ast_file_count: usize,
    pub has_crdt: bool,
    // Causal chain
    pub caused_by: Vec<String>,
}

// ── Query params ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LogQuery {
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct IntegrityQuery {
    pub message: Option<String>,
    pub prompt: Option<String>,
}

#[derive(Deserialize)]
pub struct CommitIntegrityQuery {
    pub oid: String,
}

#[derive(Deserialize)]
pub struct IntentGraphQuery {
    pub limit: Option<usize>,
    pub mode: Option<String>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    pub limit: Option<usize>,
    pub min_score: Option<f32>,
}

#[derive(Deserialize)]
pub struct MemoryDiffQuery {
    pub from: String,
    /// OID of the second snapshot; omit to diff against live memory.
    pub to: Option<String>,
}

// ── Memory API response types ─────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct MemoryFileEntry {
    pub name: String,
    pub content: String,
}

#[derive(Serialize)]
pub struct MemorySnapshotResponse {
    pub commit_oid: String,
    pub short_oid: String,
    pub timestamp: String,
    pub file_count: usize,
    pub files: Vec<MemoryFileEntry>,
}

#[derive(Serialize)]
pub struct DiffLineResponse {
    pub kind: String, // "context" | "added" | "removed"
    pub text: String,
}

#[derive(Serialize)]
pub struct ModifiedFileResponse {
    pub name: String,
    pub hunks: Vec<DiffLineResponse>,
}

#[derive(Serialize, Default)]
pub struct MemoryDiffResponse {
    pub from_label: String,
    pub to_label: String,
    pub added_files: Vec<MemoryFileEntry>,
    pub removed_files: Vec<String>,
    pub modified_files: Vec<ModifiedFileResponse>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Normalise a git remote URL to a browseable HTTPS GitHub URL, or None.
fn github_url_from_remote(url: &str) -> Option<String> {
    if !url.contains("github.com") {
        return None;
    }
    let s = if url.starts_with("git@github.com:") {
        url.replacen("git@github.com:", "https://github.com/", 1)
    } else {
        url.to_string()
    };
    Some(s.trim_end_matches(".git").to_string())
}

fn make_integrity_report(score: f32, level: crate::metadata::IntegrityLevel, findings: Vec<crate::metadata::RuleFinding>) -> IntegrityReport {
    IntegrityReport { level, score, findings }
}

fn fallback_report() -> IntegrityReport {
    make_integrity_report(1.0, crate::metadata::IntegrityLevel::Valid, vec![])
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(FRONTEND_HTML)
}

async fn api_repo(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let path = state.repo_path.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let repo = H5iRepository::open(&path)?;
        let git = repo.git();

        let branch = git
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from))
            .unwrap_or_else(|| "HEAD".to_string());

        let name = git
            .workdir()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // Auto-detect GitHub URL from "origin" remote
        let github_url = git
            .find_remote("origin")
            .ok()
            .and_then(|r| r.url().map(|u| u.to_string()))
            .and_then(|u| github_url_from_remote(&u));

        let records = repo.get_log(2000)?;
        let total = records.len();
        let ai = records.iter().filter(|r| r.ai_metadata.is_some()).count();
        let with_tests = records.iter().filter(|r| r.test_metrics.is_some()).count();

        // Aggregate test pass rate across all commits that have test data
        let (tests_pass, tests_total) = records.iter().fold((0usize, 0usize), |(p, t), r| {
            if let Some(tm) = &r.test_metrics {
                (p + if tm.is_passing() { 1 } else { 0 }, t + 1)
            } else {
                (p, t)
            }
        });
        let pass_rate = if tests_total > 0 {
            Some((tests_pass as f64 / tests_total as f64) * 100.0)
        } else {
            None
        };

        Ok(serde_json::json!({
            "name": name,
            "branch": branch,
            "total_commits": total,
            "ai_commits": ai,
            "tested_commits": with_tests,
            "test_pass_rate": pass_rate,
            "github_url": github_url,
        }))
    })
    .await;

    Json(result.unwrap_or_else(|_| Ok(serde_json::json!({}))).unwrap_or_default())
}

async fn api_commits(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LogQuery>,
) -> Json<Vec<EnrichedCommit>> {
    let path = state.repo_path.clone();
    let limit = params.limit.unwrap_or(100);

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<EnrichedCommit>> {
        let repo = H5iRepository::open(&path)?;
        let records = repo.get_log(limit)?;
        let mut enriched = Vec::new();

        for record in records {
            let oid = git2::Oid::from_str(&record.git_oid)?;
            let commit = repo.git().find_commit(oid)?;

            let message = commit.message().unwrap_or("").trim().to_string();
            let author = commit.author().name().unwrap_or("Unknown").to_string();
            let short_oid = record.git_oid[..8.min(record.git_oid.len())].to_string();
            let timestamp = record.timestamp.to_rfc3339();

            let (ai_model, ai_agent, ai_prompt, ai_tokens) =
                if let Some(ai) = &record.ai_metadata {
                    let tokens = ai.usage.as_ref().map(|u| u.total_tokens);
                    (
                        Some(ai.model_name.clone()).filter(|s| !s.is_empty()),
                        Some(ai.agent_id.clone()).filter(|s| !s.is_empty()),
                        Some(ai.prompt.clone()).filter(|s| !s.is_empty()),
                        tokens,
                    )
                } else {
                    (None, None, None, None)
                };

            let (
                test_coverage,
                test_passed,
                test_failed,
                test_skipped,
                test_total,
                test_duration_secs,
                test_tool,
                test_exit_code,
                test_summary,
                test_is_passing,
            ) = if let Some(tm) = &record.test_metrics {
                (
                    Some(tm.coverage),
                    Some(tm.passed),
                    Some(tm.failed),
                    Some(tm.skipped),
                    Some(tm.total),
                    Some(tm.duration_secs),
                    tm.tool.clone(),
                    tm.exit_code,
                    tm.summary.clone(),
                    Some(tm.is_passing()),
                )
            } else {
                (None, None, None, None, None, None, None, None, None, None)
            };

            let ast_file_count = record.ast_hashes.as_ref().map(|h| h.len()).unwrap_or(0);
            let has_crdt = record
                .crdt_states
                .as_ref()
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let caused_by = record.caused_by.clone();

            enriched.push(EnrichedCommit {
                git_oid: record.git_oid,
                short_oid,
                message,
                author,
                timestamp,
                ai_model,
                ai_agent,
                ai_prompt,
                ai_tokens,
                test_coverage,
                test_passed,
                test_failed,
                test_skipped,
                test_total,
                test_duration_secs,
                test_tool,
                test_exit_code,
                test_summary,
                test_is_passing,
                ast_file_count,
                has_crdt,
                caused_by,
            });
        }

        Ok(enriched)
    })
    .await;

    Json(result.unwrap_or_else(|_| Ok(vec![])).unwrap_or_default())
}

/// Integrity check against the *current staging area* (manual form).
async fn api_integrity(
    State(state): State<Arc<AppState>>,
    Query(params): Query<IntegrityQuery>,
) -> Json<IntegrityReport> {
    let path = state.repo_path.clone();
    let message = params.message.unwrap_or_default();
    let prompt = params.prompt;

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<IntegrityReport> {
        let repo = H5iRepository::open(&path)?;
        Ok(repo.verify_integrity(prompt.as_deref(), &message)?)
    })
    .await;

    Json(result.unwrap_or_else(|_| Ok(fallback_report())).unwrap_or_else(|_| fallback_report()))
}

/// Integrity check against a *historical* commit's own diff.
async fn api_integrity_commit(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CommitIntegrityQuery>,
) -> Json<IntegrityReport> {
    let path = state.repo_path.clone();
    let oid_str = params.oid;

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<IntegrityReport> {
        let repo = H5iRepository::open(&path)?;
        let oid = git2::Oid::from_str(&oid_str)?;
        Ok(repo.verify_commit_integrity(oid)?)
    })
    .await;

    Json(result.unwrap_or_else(|_| Ok(fallback_report())).unwrap_or_else(|_| fallback_report()))
}

async fn api_intent_graph(
    State(state): State<Arc<AppState>>,
    Query(params): Query<IntentGraphQuery>,
) -> Json<IntentGraph> {
    let path = state.repo_path.clone();
    let limit = params.limit.unwrap_or(30);
    let analyze = params.mode.as_deref().unwrap_or("prompt") == "analyze";

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<IntentGraph> {
        let repo = H5iRepository::open(&path)?;
        Ok(repo.build_intent_graph(limit, analyze)?)
    })
    .await;

    Json(
        result
            .unwrap_or_else(|_| Ok(IntentGraph::default()))
            .unwrap_or_default(),
    )
}

async fn api_review_points(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ReviewQuery>,
) -> Json<Vec<ReviewPoint>> {
    let path = state.repo_path.clone();
    let limit = params.limit.unwrap_or(100);
    let min_score = params.min_score.unwrap_or(REVIEW_THRESHOLD);

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ReviewPoint>> {
        let repo = H5iRepository::open(&path)?;
        Ok(repo.suggest_review_points(limit, min_score)?)
    })
    .await;

    Json(
        result
            .unwrap_or_else(|_| Ok(vec![]))
            .unwrap_or_default(),
    )
}

// ── Memory handlers ───────────────────────────────────────────────────────────

async fn api_memory_snapshots(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<MemorySnapshotResponse>> {
    let path = state.repo_path.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemorySnapshotResponse>> {
        let repo = H5iRepository::open(&path)?;
        let snapshots = memory::list_snapshots(&repo.h5i_root)?;
        let mut out = Vec::new();
        for snap in snapshots.iter().rev() {
            let snap_dir = repo.h5i_root.join("memory").join(&snap.commit_oid);
            let mut files: Vec<MemoryFileEntry> = std::fs::read_dir(&snap_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| {
                            e.path().is_file()
                                && e.file_name() != "_meta.json"
                        })
                        .map(|e| MemoryFileEntry {
                            name: e.file_name().to_string_lossy().into_owned(),
                            content: std::fs::read_to_string(e.path()).unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            files.sort_by(|a, b| a.name.cmp(&b.name));
            out.push(MemorySnapshotResponse {
                short_oid: snap.commit_oid[..8.min(snap.commit_oid.len())].to_string(),
                commit_oid: snap.commit_oid.clone(),
                timestamp: snap.timestamp.to_rfc3339(),
                file_count: snap.file_count,
                files,
            });
        }
        Ok(out)
    })
    .await;
    Json(result.unwrap_or_else(|_| Ok(vec![])).unwrap_or_default())
}

async fn api_memory_diff(
    State(state): State<Arc<AppState>>,
    Query(params): Query<MemoryDiffQuery>,
) -> Json<MemoryDiffResponse> {
    let path = state.repo_path.clone();
    let from = params.from.clone();
    let to = params.to.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<MemoryDiffResponse> {
        let repo = H5iRepository::open(&path)?;
        let workdir = repo
            .git()
            .workdir()
            .ok_or_else(|| anyhow::anyhow!("bare repository"))?
            .to_path_buf();
        let diff =
            memory::diff_snapshots(&repo.h5i_root, &workdir, &from, to.as_deref())?;
        Ok(MemoryDiffResponse {
            from_label: diff.from_label,
            to_label: diff.to_label,
            added_files: diff
                .added_files
                .into_iter()
                .map(|(name, content)| MemoryFileEntry { name, content })
                .collect(),
            removed_files: diff.removed_files.into_iter().map(|(name, _)| name).collect(),
            modified_files: diff
                .modified_files
                .into_iter()
                .map(|f| ModifiedFileResponse {
                    name: f.name,
                    hunks: f
                        .hunks
                        .into_iter()
                        .map(|l| match l {
                            memory::DiffLine::Context(t) => {
                                DiffLineResponse { kind: "context".into(), text: t }
                            }
                            memory::DiffLine::Added(t) => {
                                DiffLineResponse { kind: "added".into(), text: t }
                            }
                            memory::DiffLine::Removed(t) => {
                                DiffLineResponse { kind: "removed".into(), text: t }
                            }
                        })
                        .collect(),
                })
                .collect(),
        })
    })
    .await;
    Json(result.unwrap_or_else(|_| Ok(MemoryDiffResponse::default())).unwrap_or_default())
}

// ── Server entry point ────────────────────────────────────────────────────────

pub async fn serve(repo_path: PathBuf, port: u16) -> anyhow::Result<()> {
    let state = Arc::new(AppState { repo_path });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/repo", get(api_repo))
        .route("/api/commits", get(api_commits))
        .route("/api/integrity", get(api_integrity))
        .route("/api/integrity/commit", get(api_integrity_commit))
        .route("/api/intent-graph", get(api_intent_graph))
        .route("/api/review-points", get(api_review_points))
        .route("/api/memory/snapshots", get(api_memory_snapshots))
        .route("/api/memory/diff", get(api_memory_diff))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).await?;
    println!("  h5i UI →  http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Embedded frontend ─────────────────────────────────────────────────────────

pub const FRONTEND_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>h5i — 5D Git Dashboard</title>
<style>
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
html{font-size:14px;scroll-behavior:smooth}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans",Helvetica,Arial,sans-serif;background:#0d1117;color:#e6edf3;min-height:100vh;line-height:1.5}

/* Header */
.header{background:#161b22;border-bottom:1px solid #30363d;padding:0 20px;display:flex;align-items:center;gap:10px;height:54px;position:sticky;top:0;z-index:100;backdrop-filter:blur(8px)}
.logo{display:flex;align-items:center;gap:8px;font-size:16px;font-weight:700;color:#e6edf3;text-decoration:none;letter-spacing:-.02em}
.logo-icon{width:28px;height:28px;background:linear-gradient(135deg,#bc8cff 0%,#58a6ff 100%);border-radius:6px;display:flex;align-items:center;justify-content:center;font-size:12px;font-weight:800;color:#fff;box-shadow:0 0 10px #bc8cff44}
.header-sep{color:#30363d;font-size:20px;margin:0 2px}
.repo-name{color:#58a6ff;font-size:14px;font-weight:600}
.branch-badge{background:#21262d;border:1px solid #30363d;border-radius:20px;padding:2px 10px;font-size:11px;color:#8b949e;font-family:monospace}
.header-spacer{flex:1}
.gh-repo-link{display:none;align-items:center;gap:5px;color:#8b949e;text-decoration:none;font-size:12px;padding:4px 10px;border:1px solid #30363d;border-radius:6px;transition:all .15s}
.gh-repo-link:hover{color:#58a6ff;border-color:#58a6ff}
.gh-repo-link.visible{display:flex}
.refresh-btn{background:#21262d;border:1px solid #30363d;border-radius:6px;color:#8b949e;padding:5px 12px;cursor:pointer;font-size:12px;transition:all .15s}
.refresh-btn:hover{color:#e6edf3;border-color:#8b949e}

/* Stats bar */
.stats-bar{background:#161b22;border-bottom:1px solid #30363d;padding:6px 20px;display:flex;gap:20px;align-items:center;flex-wrap:wrap}
.stat{display:flex;align-items:center;gap:5px;font-size:12px;color:#8b949e}
.stat b{color:#e6edf3;font-size:13px}
.dot{width:7px;height:7px;border-radius:50%;display:inline-block}
.dot-blue{background:#58a6ff}.dot-purple{background:#bc8cff}.dot-green{background:#3fb950}.dot-red{background:#f85149}.dot-orange{background:#d29922}.dot-gray{background:#484f58}

/* Layout */
.layout{display:flex;min-height:calc(100vh - 88px)}
.sidebar{width:210px;flex-shrink:0;border-right:1px solid #30363d;padding:14px 12px;display:flex;flex-direction:column;gap:12px;overflow-y:auto;position:sticky;top:88px;max-height:calc(100vh - 88px)}
.content{flex:1;padding:16px 20px;min-width:0;overflow-y:auto}

/* Sidebar cards */
.card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:12px}
.card-title{font-size:11px;font-weight:600;color:#8b949e;text-transform:uppercase;letter-spacing:.06em;margin-bottom:10px}
.dim-row{display:flex;align-items:center;gap:7px;margin-bottom:6px;font-size:12px}
.dim-icon{font-size:14px;width:20px;text-align:center}
.dim-tag{padding:1px 7px;border-radius:10px;font-size:10px;font-weight:600}
.tag-blue{background:#1f3a5f;color:#58a6ff}.tag-green{background:#1a3a2a;color:#3fb950}
.tag-purple{background:#2d1f4f;color:#bc8cff}.tag-orange{background:#3a2a1a;color:#d29922}
.tag-yellow{background:#3a3a1a;color:#e3b341}
.side-row{display:flex;justify-content:space-between;font-size:12px;margin-bottom:5px;color:#8b949e}
.side-row b{color:#e6edf3}

/* Sparkline */
.sparkline-wrap{margin-top:6px}
.sparkline-svg{width:100%;height:40px;overflow:visible}
.sparkline-label{font-size:10px;color:#484f58;text-align:center;margin-top:3px}
.health-row{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px}
.health-rate{font-size:18px;font-weight:700}
.health-rate.good{color:#3fb950}.health-rate.warn{color:#d29922}.health-rate.bad{color:#f85149}

/* Tabs */
.tabs{display:flex;gap:2px;margin-bottom:16px;border-bottom:1px solid #30363d;padding-bottom:0}
.tab{background:none;border:none;border-bottom:2px solid transparent;padding:8px 14px;color:#8b949e;cursor:pointer;font-size:13px;font-weight:500;margin-bottom:-1px;transition:all .15s}
.tab:hover{color:#e6edf3}
.tab.active{color:#e6edf3;border-bottom-color:#bc8cff}
.tab-badge{background:#30363d;color:#8b949e;border-radius:10px;padding:0 7px;font-size:10px;margin-left:5px}
.tab.active .tab-badge{background:#bc8cff33;color:#bc8cff}

/* Search + filters */
.search-row{display:flex;gap:8px;margin-bottom:10px;align-items:center;flex-wrap:wrap}
.search-input{flex:1;min-width:180px;background:#0d1117;border:1px solid #30363d;border-radius:6px;color:#e6edf3;padding:6px 12px;font-size:13px;outline:none;transition:border .15s}
.search-input:focus{border-color:#58a6ff}
.pill{background:#21262d;border:1px solid #30363d;border-radius:20px;padding:4px 12px;font-size:12px;color:#8b949e;cursor:pointer;transition:all .15s;white-space:nowrap}
.pill:hover{color:#e6edf3;border-color:#8b949e}
.pill.active{background:#bc8cff22;border-color:#bc8cff;color:#bc8cff}
.pill.active.red-pill{background:#f8514922;border-color:#f85149;color:#f85149}

/* Timeline */
.timeline{position:relative;padding-left:28px}
.timeline::before{content:"";position:absolute;left:10px;top:8px;bottom:8px;width:2px;background:linear-gradient(to bottom,#bc8cff,#58a6ff44);border-radius:2px}
.commit-entry{position:relative;margin-bottom:10px;animation:fadeIn .3s ease both}
@keyframes fadeIn{from{opacity:0;transform:translateY(6px)}to{opacity:1;transform:translateY(0)}}
.commit-dot{position:absolute;left:-22px;top:14px;width:16px;height:16px;border-radius:50%;border:2px solid #0d1117;display:flex;align-items:center;justify-content:center;font-size:8px;z-index:1}
.ai-dot{background:linear-gradient(135deg,#bc8cff,#58a6ff);box-shadow:0 0 8px #bc8cff66}
.human-dot{background:#21262d;border-color:#484f58}
.commit-card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:11px 13px;cursor:pointer;transition:all .15s;position:relative}
.commit-card:hover{border-color:#484f58;background:#1c2128}
.commit-card.expanded{border-color:#58a6ff44}
.commit-card.failing{border-left:3px solid #f85149}
.commit-card.passing{border-left:3px solid #3fb95055}
.commit-head{display:flex;align-items:baseline;gap:8px;margin-bottom:5px;flex-wrap:wrap}
.oid-chip{font-family:monospace;font-size:11px;padding:1px 7px;border-radius:4px;font-weight:600;white-space:nowrap}
.oid-ai{background:#bc8cff22;color:#bc8cff}.oid-human{background:#58a6ff22;color:#58a6ff}
.commit-msg{font-size:13px;font-weight:500;flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.gh-commit-link{margin-left:auto;display:inline-flex;align-items:center;gap:4px;color:#58a6ff;text-decoration:none;font-size:11px;font-weight:600;padding:3px 9px;border:1px solid #58a6ff44;border-radius:4px;background:#58a6ff11;white-space:nowrap;transition:all .15s;flex-shrink:0}
.gh-commit-link:hover{color:#fff;border-color:#58a6ff;background:#58a6ff33}
.byline{font-size:12px;color:#8b949e;margin-bottom:7px}
.byline .author{color:#58a6ff}
.badges{display:flex;flex-wrap:wrap;gap:4px}
.badge{display:inline-flex;align-items:center;gap:3px;padding:2px 7px;border-radius:10px;font-size:11px;font-weight:500;white-space:nowrap}
.b-model{background:#bc8cff22;color:#bc8cff}
.b-agent{background:#d2992222;color:#d29922}
.b-test-ok{background:#3fb95022;color:#3fb950}
.b-test-fail{background:#f8514922;color:#f85149}
.b-test-warn{background:#d2992222;color:#d29922}
.b-tool{background:#21262d;color:#8b949e;border:1px solid #30363d}
.b-dur{background:#21262d;color:#8b949e}
.b-ast{background:#1a3a2a;color:#3fb950}
.b-crdt{background:#1f3a5f;color:#58a6ff}
.b-tok{background:#21262d;color:#8b949e}
.b-cov{background:#2d1f4f;color:#bc8cff}
.b-cause{background:#1f2d3d;color:#58a6ff;border:1px solid #1f4070}

/* Commit detail (expanded) */
.commit-detail{display:none;margin-top:12px;border-top:1px solid #30363d;padding-top:12px}
.commit-detail.open{display:block}
.detail-grid{display:grid;grid-template-columns:100px 1fr;gap:4px 12px;font-size:12px;margin-bottom:10px}
.dk{color:#8b949e;padding-top:2px}
.dv{color:#e6edf3;word-break:break-word}
.dv.mono{font-family:monospace;font-size:11px}
.dv.prompt-text{color:#bc8cff;font-style:italic;white-space:pre-wrap;line-height:1.5}
.test-table{width:100%;border-collapse:collapse;margin:8px 0;font-size:12px}
.test-table th{color:#8b949e;font-weight:500;text-align:left;padding:3px 8px;border-bottom:1px solid #30363d}
.test-table td{padding:4px 8px;border-bottom:1px solid #21262d}
.td-pass{color:#3fb950;font-weight:600}.td-fail{color:#f85149;font-weight:600}.td-skip{color:#d29922}.td-tot{color:#e6edf3}
.audit-section{margin-top:8px;border-top:1px solid #21262d;padding-top:8px}
.audit-btn{background:#21262d;border:1px solid #30363d;border-radius:6px;color:#8b949e;padding:5px 12px;cursor:pointer;font-size:12px;transition:all .15s;display:inline-flex;align-items:center;gap:5px}
.audit-btn:hover{color:#bc8cff;border-color:#bc8cff44;background:#bc8cff11}
.audit-btn:disabled{opacity:.5;cursor:not-allowed}
.audit-result-box{margin-top:8px;border:1px solid #30363d;border-radius:6px;padding:10px;background:#0d1117}
.rules-detail-toggle{background:none;border:none;color:#58a6ff;font-size:11px;cursor:pointer;padding:4px 0;display:inline-flex;align-items:center;gap:4px;margin-top:8px;text-decoration:underline;text-underline-offset:2px}
.rules-detail-toggle:hover{color:#79c0ff}
.rules-detail-panel{display:none;margin-top:8px;border:1px solid #21262d;border-radius:6px;padding:8px 10px;background:#0d1117}
.rules-detail-panel.open{display:block}
.rule-row{display:flex;align-items:center;gap:8px;padding:3px 0;font-size:11px}
.rule-pass{color:#3fb950}.rule-fail{color:#f85149}.rule-warn{color:#d29922}
.rule-id-label{font-family:monospace;font-size:10px;color:#8b949e;flex:1}

/* Integrity panel */
.int-form{display:flex;flex-direction:column;gap:10px;max-width:680px}
.int-label{font-size:12px;color:#8b949e;margin-bottom:4px;display:block}
.int-input{background:#0d1117;border:1px solid #30363d;border-radius:6px;color:#e6edf3;padding:8px 12px;font-size:13px;outline:none;width:100%;transition:border .15s}
.int-input:focus{border-color:#58a6ff}
.int-textarea{resize:vertical;min-height:72px;font-family:inherit}
.run-btn{background:linear-gradient(90deg,#bc8cff,#58a6ff);border:none;border-radius:6px;color:#fff;padding:8px 20px;font-size:13px;font-weight:600;cursor:pointer;transition:opacity .15s;align-self:flex-start}
.run-btn:hover{opacity:.88}
.run-btn:disabled{opacity:.5;cursor:not-allowed}
.int-result{margin-top:16px}
.int-report{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:16px}
.ir-header{display:flex;align-items:center;gap:12px;margin-bottom:14px}
.lv-valid{background:#1a3a2a;color:#3fb950;padding:3px 10px;border-radius:20px;font-size:11px;font-weight:700}
.lv-warning{background:#3a2a1a;color:#d29922;padding:3px 10px;border-radius:20px;font-size:11px;font-weight:700}
.lv-violation{background:#3a1a1a;color:#f85149;padding:3px 10px;border-radius:20px;font-size:11px;font-weight:700}
.ir-score{font-size:28px;font-weight:700}
.ir-label{color:#8b949e;font-size:13px}
.ir-findings{display:flex;flex-direction:column;gap:8px}
.finding{display:flex;align-items:flex-start;gap:10px;padding:8px 10px;border-radius:6px}
.rv{background:#3a1a1a}.rw{background:#3a2a1a}.ri{background:#1f3a5f}
.finding-icon{font-size:14px;flex-shrink:0;margin-top:1px}
.finding-rule{font-size:10px;font-weight:700;padding:1px 7px;border-radius:10px;white-space:nowrap}
.rv .finding-rule{background:#f8514922;color:#f85149}
.rw .finding-rule{background:#d2992222;color:#d29922}
.ri .finding-rule{background:#58a6ff22;color:#58a6ff}
.finding-detail{font-size:12px;color:#8b949e;line-height:1.5}
.success-msg{color:#3fb950;font-size:13px;display:flex;align-items:center;gap:6px}

/* Summary tab */
.summary-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(200px,1fr));gap:14px;margin-bottom:20px}
.sum-card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:14px}
.sum-num{font-size:28px;font-weight:700;margin-bottom:2px}
.sum-label{font-size:12px;color:#8b949e}
.chart-section{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:14px;margin-bottom:14px}
.chart-title{font-size:12px;font-weight:600;color:#8b949e;text-transform:uppercase;letter-spacing:.06em;margin-bottom:12px}
.chart-svg{width:100%;overflow:visible}
.agent-table{width:100%;border-collapse:collapse;font-size:12px}
.agent-table th{color:#8b949e;font-weight:500;text-align:left;padding:4px 10px;border-bottom:1px solid #30363d}
.agent-table td{padding:5px 10px;border-bottom:1px solid #21262d}
.agent-bar-bg{background:#21262d;border-radius:10px;height:6px;width:100px;display:inline-block;vertical-align:middle}
.agent-bar-fill{background:linear-gradient(90deg,#bc8cff,#58a6ff);border-radius:10px;height:6px;display:block}
.fail-list{display:flex;flex-direction:column;gap:6px}
.fail-item{display:flex;align-items:center;gap:8px;font-size:12px;padding:6px 10px;background:#3a1a1a22;border-radius:6px;border-left:3px solid #f85149}
.fail-oid{font-family:monospace;color:#f85149;font-size:11px}
.fail-msg{color:#e6edf3;flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.fail-counts{color:#f85149;font-weight:600;white-space:nowrap}
.empty-state{color:#484f58;text-align:center;padding:40px 20px;font-size:14px}
.spinner{display:inline-block;width:14px;height:14px;border:2px solid #30363d;border-top-color:#58a6ff;border-radius:50%;animation:spin .6s linear infinite;vertical-align:middle}
@keyframes spin{to{transform:rotate(360deg)}}
.section-hdr{font-size:13px;font-weight:600;margin-bottom:8px;color:#e6edf3}

/* Intent Graph tab */
.ig-controls{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-bottom:14px}
.ig-select{background:#0d1117;border:1px solid #30363d;border-radius:6px;color:#e6edf3;padding:5px 10px;font-size:12px;outline:none}
.ig-btn{background:linear-gradient(90deg,#bc8cff,#58a6ff);border:none;border-radius:6px;color:#fff;padding:6px 16px;font-size:12px;font-weight:600;cursor:pointer;transition:opacity .15s}
.ig-btn:hover{opacity:.85}
.ig-btn:disabled{opacity:.5;cursor:not-allowed}
.ig-canvas-wrap{background:#0d1117;border:1px solid #30363d;border-radius:8px;overflow:auto;min-height:300px}
.ig-svg{display:block;min-width:100%}
.ig-node rect{rx:6;stroke-width:1.5;cursor:pointer;transition:filter .15s}
.ig-node rect:hover{filter:brightness(1.3)}
.ig-node text{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;pointer-events:none;dominant-baseline:middle}
.ig-edge{stroke-width:1.5;fill:none;marker-end:url(#arrow-parent)}
.ig-edge-causal{stroke-width:2;fill:none;marker-end:url(#arrow-causal)}
.ig-legend{display:flex;gap:16px;font-size:11px;color:#8b949e;margin-top:8px;padding:0 4px}
.ig-legend-item{display:flex;align-items:center;gap:5px}
.ig-legend-line{width:24px;height:2px;display:inline-block}

/* Review Points tab */
.rp-list{display:flex;flex-direction:column;gap:10px}
.rp-card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:13px 15px;animation:fadeIn .3s ease both}
.rp-card.high{border-left:3px solid #f85149}
.rp-card.medium{border-left:3px solid #d29922}
.rp-card.low{border-left:3px solid #58a6ff}
.rp-head{display:flex;align-items:center;gap:10px;margin-bottom:6px;flex-wrap:wrap}
.rp-rank{font-size:11px;color:#484f58;font-weight:600;min-width:22px}
.rp-score-pill{padding:2px 9px;border-radius:10px;font-size:12px;font-weight:700;white-space:nowrap}
.rp-score-high{background:#f8514922;color:#f85149}
.rp-score-med{background:#d2992222;color:#d29922}
.rp-score-low{background:#58a6ff22;color:#58a6ff}
.rp-bar{font-family:monospace;font-size:11px;color:#484f58;letter-spacing:-1px}
.rp-msg{font-size:13px;font-weight:600;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.rp-meta{font-size:12px;color:#8b949e;margin-bottom:8px}
.rp-triggers{display:flex;flex-direction:column;gap:3px}
.rp-trigger{display:flex;align-items:baseline;gap:8px;font-size:12px}
.rp-rule{font-family:monospace;font-size:10px;font-weight:700;padding:1px 7px;border-radius:8px;white-space:nowrap;flex-shrink:0}
.rp-rule-red{background:#f8514922;color:#f85149}
.rp-rule-yellow{background:#d2992222;color:#d29922}
.rp-rule-blue{background:#58a6ff22;color:#58a6ff}
.rp-rule-gray{background:#21262d;color:#8b949e}
.rp-detail{color:#8b949e}
.rp-controls{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-bottom:14px}
.rp-label{font-size:12px;color:#8b949e}
.rp-select{background:#0d1117;border:1px solid #30363d;border-radius:6px;color:#e6edf3;padding:5px 10px;font-size:12px;outline:none}
.rp-btn{background:linear-gradient(90deg,#bc8cff,#58a6ff);border:none;border-radius:6px;color:#fff;padding:6px 16px;font-size:12px;font-weight:600;cursor:pointer;transition:opacity .15s}
.rp-btn:hover{opacity:.85}
.rp-btn:disabled{opacity:.5;cursor:not-allowed}
.rp-status{font-size:12px;color:#8b949e}

/* Intent node detail modal */
.ig-modal-overlay{display:none;position:fixed;inset:0;background:#00000088;z-index:200;align-items:center;justify-content:center}
.ig-modal-overlay.open{display:flex}
.ig-modal{background:#161b22;border:1px solid #30363d;border-radius:10px;padding:20px 22px;min-width:340px;max-width:520px;width:90%;position:relative;box-shadow:0 8px 32px #000a}
.ig-modal-close{position:absolute;top:10px;right:12px;background:none;border:none;color:#8b949e;font-size:18px;cursor:pointer;line-height:1;padding:2px 6px;border-radius:4px}
.ig-modal-close:hover{color:#e6edf3;background:#30363d}
.ig-modal-oid{font-family:monospace;font-size:13px;font-weight:700;margin-bottom:2px}
.ig-modal-msg{font-size:13px;color:#8b949e;margin-bottom:14px;line-height:1.45}
.ig-modal-section{margin-bottom:12px}
.ig-modal-label{font-size:10px;font-weight:600;text-transform:uppercase;letter-spacing:.06em;color:#484f58;margin-bottom:4px}
.ig-modal-intent{font-size:14px;color:#e6edf3;line-height:1.55;white-space:pre-wrap;word-break:break-word}
.ig-modal-meta{display:flex;flex-wrap:wrap;gap:6px;margin-top:8px}
.ig-modal-badge{padding:2px 9px;border-radius:10px;font-size:11px;font-weight:500}

/* ── Memory tab ────────────────────────────────────────────────────────────── */
.mem-layout{display:grid;grid-template-columns:300px 1fr;gap:14px;align-items:start}
.mem-snap-list{display:flex;flex-direction:column;gap:7px}
.mem-snap-card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:11px 13px;cursor:pointer;transition:all .15s;position:relative}
.mem-snap-card:hover{border-color:#484f58;background:#1c2128}
.mem-snap-card.sel-from{border-color:#58a6ff;box-shadow:0 0 0 1px #58a6ff33}
.mem-snap-card.sel-to{border-color:#3fb950;box-shadow:0 0 0 1px #3fb95033}
.mem-snap-head{display:flex;align-items:center;gap:7px;margin-bottom:4px}
.mem-oid{font-family:monospace;font-size:11px;font-weight:700;background:#bc8cff22;color:#bc8cff;padding:1px 7px;border-radius:4px}
.mem-ts{font-size:11px;color:#484f58}
.mem-nfiles{font-size:12px;color:#8b949e}
.mem-sel-badge{position:absolute;top:8px;right:8px;font-size:9px;font-weight:700;padding:1px 7px;border-radius:8px}
.mem-sel-from-badge{background:#1f3a5f;color:#58a6ff}
.mem-sel-to-badge{background:#1a3a2a;color:#3fb950}

.mem-viewer{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:16px;min-height:420px}
.mem-viewer-empty{color:#484f58;text-align:center;padding:60px 20px;font-size:13px;line-height:1.8}

.mem-file-tabs{display:flex;gap:4px;flex-wrap:wrap;margin-bottom:12px;border-bottom:1px solid #30363d;padding-bottom:8px}
.mem-ftab{background:none;border:1px solid transparent;border-radius:4px;padding:3px 10px;font-size:11px;cursor:pointer;color:#8b949e;transition:all .15s}
.mem-ftab:hover{color:#e6edf3;background:#21262d}
.mem-ftab.active{background:#bc8cff22;border-color:#bc8cff44;color:#bc8cff}
.mem-file-content{background:#0d1117;border:1px solid #21262d;border-radius:6px;padding:12px;font-size:12px;font-family:monospace;white-space:pre-wrap;color:#8b949e;max-height:480px;overflow-y:auto;line-height:1.6}

.mem-frontmatter{background:#0d1117;border:1px solid #21262d;border-radius:6px;padding:10px 14px;margin-bottom:10px}
.mem-fm-row{display:flex;gap:10px;font-size:12px;margin-bottom:5px;align-items:baseline}
.mem-fm-key{color:#484f58;min-width:72px;font-family:monospace;font-size:11px}
.mem-fm-val{color:#e6edf3}
.mem-fm-desc{color:#8b949e;font-style:italic}
.mem-type-user{background:#1f3a5f;color:#58a6ff;padding:1px 7px;border-radius:8px;font-size:10px;font-weight:700}
.mem-type-feedback{background:#2d1f4f;color:#bc8cff;padding:1px 7px;border-radius:8px;font-size:10px;font-weight:700}
.mem-type-project{background:#1a3a2a;color:#3fb950;padding:1px 7px;border-radius:8px;font-size:10px;font-weight:700}
.mem-type-reference{background:#3a2a1a;color:#d29922;padding:1px 7px;border-radius:8px;font-size:10px;font-weight:700}
.mem-body{font-size:12px;color:#e6edf3;white-space:pre-wrap;line-height:1.7;font-family:inherit;padding:2px 0}

.mem-diff-file{margin-bottom:18px}
.mem-diff-hdr{display:flex;align-items:center;gap:7px;margin-bottom:6px;font-size:12px;font-weight:600;font-family:monospace}
.mem-diff-hdr-add{color:#3fb950}.mem-diff-hdr-rm{color:#f85149}.mem-diff-hdr-mod{color:#d29922}
.mem-diff-lines{background:#0d1117;border:1px solid #21262d;border-radius:6px;overflow:hidden;font-family:monospace;font-size:11px;max-height:320px;overflow-y:auto}
.mem-dl{display:flex;line-height:1.55}
.mem-dl-add{background:#12261e;color:#3fb950}
.mem-dl-rm{background:#270d0d;color:#f85149}
.mem-dl-ctx{color:#484f58}
.mem-dl-sep{color:#30363d;font-style:italic;background:#0d1117;justify-content:center}
.mem-gutter{width:18px;text-align:center;flex-shrink:0;padding:0 3px;font-size:10px;user-select:none;border-right:1px solid #21262d;color:inherit;opacity:.7}
.mem-text{padding:1px 8px;white-space:pre-wrap;word-break:break-all;flex:1}

.mem-diff-summary{display:flex;gap:14px;margin-bottom:14px;font-size:12px;flex-wrap:wrap}
.mem-diff-stat-add{color:#3fb950;display:flex;align-items:center;gap:4px}
.mem-diff-stat-rm{color:#f85149;display:flex;align-items:center;gap:4px}
.mem-diff-stat-mod{color:#d29922;display:flex;align-items:center;gap:4px}

.mem-controls{display:flex;gap:8px;align-items:center;margin-bottom:14px;flex-wrap:wrap}
.mem-btn{background:linear-gradient(90deg,#bc8cff,#58a6ff);border:none;border-radius:6px;color:#fff;padding:6px 16px;font-size:12px;font-weight:600;cursor:pointer;transition:opacity .15s}
.mem-btn:hover{opacity:.85}
.mem-btn:disabled{opacity:.4;cursor:not-allowed}
.mem-btn-ghost{background:#21262d;border:1px solid #30363d;border-radius:6px;color:#8b949e;padding:6px 14px;font-size:12px;cursor:pointer;transition:all .15s}
.mem-btn-ghost:hover{color:#e6edf3;border-color:#8b949e}
.mem-hint{font-size:11px;color:#484f58}
.mem-snap-count{font-size:11px;color:#484f58;margin-bottom:8px}
.mem-insp-hdr{font-size:13px;font-weight:600;margin-bottom:12px;color:#e6edf3;display:flex;align-items:center;gap:10px}
.mem-diff-hdr-row{font-size:13px;font-weight:600;margin-bottom:14px;color:#e6edf3;display:flex;align-items:center;gap:8px}
</style>
</head>
<body>

<!-- Header -->
<header class="header">
  <div class="logo">
    <div class="logo-icon">h5</div>
    h5i
  </div>
  <span class="header-sep">/</span>
  <span class="repo-name" id="repo-name">loading…</span>
  <span class="branch-badge" id="branch-badge">—</span>
  <div class="header-spacer"></div>
  <a class="gh-repo-link" id="gh-repo-link" href="#" target="_blank" rel="noopener">
    <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z"/></svg>
    View on GitHub
  </a>
  <button class="refresh-btn" onclick="loadAll()">↻ Refresh</button>
</header>

<!-- Stats bar -->
<div class="stats-bar">
  <div class="stat"><span class="dot dot-blue"></span>Commits <b id="s-total">—</b></div>
  <div class="stat"><span class="dot dot-purple"></span>AI-assisted <b id="s-ai">—</b></div>
  <div class="stat"><span class="dot dot-orange"></span>With tests <b id="s-tested">—</b></div>
  <div class="stat"><span class="dot dot-green"></span>Pass rate <b id="s-passrate">—</b></div>
  <div class="stat"><span class="dot dot-gray"></span>Loaded <b id="s-loaded">—</b></div>
</div>

<!-- Main layout -->
<div class="layout">

  <!-- Sidebar -->
  <aside class="sidebar">
    <div class="card">
      <div class="card-title">5 Dimensions</div>
      <div class="dim-row"><span class="dim-icon">⏱</span>Temporal<span class="dim-tag tag-blue" style="margin-left:auto">Git</span></div>
      <div class="dim-row"><span class="dim-icon">🌳</span>Structural<span class="dim-tag tag-green" style="margin-left:auto">AST</span></div>
      <div class="dim-row"><span class="dim-icon">🧠</span>Intentional<span class="dim-tag tag-purple" style="margin-left:auto">AI</span></div>
      <div class="dim-row"><span class="dim-icon">🧪</span>Empirical<span class="dim-tag tag-orange" style="margin-left:auto">Tests</span></div>
      <div class="dim-row"><span class="dim-icon">🔗</span>Associative<span class="dim-tag tag-yellow" style="margin-left:auto">CRDT</span></div>
    </div>

    <div class="card">
      <div class="card-title">Repository</div>
      <div class="side-row">Total commits<b id="side-total">—</b></div>
      <div class="side-row">AI commits<b id="side-ai">—</b></div>
      <div class="side-row">Human commits<b id="side-human">—</b></div>
      <div class="side-row">AI ratio<b id="side-ratio">—</b></div>
    </div>

    <div class="card">
      <div class="card-title">Test Health</div>
      <div class="health-row">
        <span style="font-size:12px;color:#8b949e">Pass rate</span>
        <span class="health-rate" id="side-pass-rate">—</span>
      </div>
      <div class="sparkline-wrap">
        <svg class="sparkline-svg" id="sparkline-svg" viewBox="0 0 180 40" preserveAspectRatio="none">
          <text x="90" y="24" text-anchor="middle" fill="#484f58" font-size="10">no test data</text>
        </svg>
        <div class="sparkline-label" id="sparkline-label">last commits with tests</div>
      </div>
    </div>
  </aside>

  <!-- Content -->
  <main class="content">
    <!-- Tabs -->
    <div class="tabs">
      <button class="tab active" onclick="switchTab('timeline')">⎇ Timeline<span class="tab-badge" id="tab-count">0</span></button>
      <button class="tab" onclick="switchTab('summary')">📊 Summary</button>
      <button class="tab" onclick="switchTab('integrity')">🛡 Integrity</button>
      <button class="tab" onclick="switchTab('intentgraph')">🔗 Intent Graph</button>
      <button class="tab" onclick="switchTab('review')">🔍 Review Points<span class="tab-badge" id="tab-review-count">—</span></button>
      <button class="tab" onclick="switchTab('memory');loadMemorySnapshots()">🧠 Memory<span class="tab-badge" id="tab-mem-count">—</span></button>
    </div>

    <!-- Timeline panel -->
    <div id="panel-timeline">
      <div class="search-row">
        <input class="search-input" id="search" placeholder="Search commits, authors, models…" oninput="filter()">
        <span class="pill" id="pill-ai" onclick="toggleFilter('ai')">🤖 AI only</span>
        <span class="pill" id="pill-test" onclick="toggleFilter('test')">🧪 With tests</span>
        <span class="pill" id="pill-fail" onclick="toggleFilter('fail')">✖ Failing</span>
      </div>
      <div class="timeline" id="timeline-list">
        <div class="empty-state"><span class="spinner"></span> Loading commits…</div>
      </div>
    </div>

    <!-- Summary panel -->
    <div id="panel-summary" style="display:none">
      <div class="summary-grid" id="sum-cards"></div>
      <div style="display:grid;grid-template-columns:1fr 1fr;gap:14px;flex-wrap:wrap" id="sum-charts"></div>
    </div>

    <!-- Intent node detail modal -->
    <div class="ig-modal-overlay" id="ig-modal-overlay" onclick="if(event.target===this)closeNodeModal()">
      <div class="ig-modal">
        <button class="ig-modal-close" onclick="closeNodeModal()">✕</button>
        <div class="ig-modal-oid" id="ig-modal-oid"></div>
        <div class="ig-modal-msg" id="ig-modal-msg"></div>
        <div class="ig-modal-section">
          <div class="ig-modal-label">Intent</div>
          <div class="ig-modal-intent" id="ig-modal-intent"></div>
        </div>
        <div class="ig-modal-meta" id="ig-modal-meta"></div>
      </div>
    </div>

    <!-- Intent Graph panel -->
    <div id="panel-intentgraph" style="display:none">
      <div class="ig-controls">
        <label style="font-size:12px;color:#8b949e">Commits:
          <select class="ig-select" id="ig-limit">
            <option value="15">15</option>
            <option value="30" selected>30</option>
            <option value="50">50</option>
            <option value="100">100</option>
          </select>
        </label>
        <label style="font-size:12px;color:#8b949e">Mode:
          <select class="ig-select" id="ig-mode">
            <option value="prompt">prompt (stored)</option>
            <option value="analyze">analyze (Claude)</option>
          </select>
        </label>
        <button class="ig-btn" id="ig-load-btn" onclick="loadIntentGraph()">↻ Load Graph</button>
        <span id="ig-status" style="font-size:12px;color:#8b949e"></span>
      </div>
      <div class="ig-canvas-wrap" id="ig-canvas-wrap">
        <div class="empty-state" style="padding:60px 20px">Click "Load Graph" to visualise commit intents.</div>
      </div>
      <div class="ig-legend">
        <span class="ig-legend-item"><span class="ig-legend-line" style="background:#484f58"></span>parent chain</span>
        <span class="ig-legend-item"><span class="ig-legend-line" style="background:#bc8cff"></span>causal link</span>
        <span class="ig-legend-item"><span style="width:10px;height:10px;border-radius:2px;background:linear-gradient(135deg,#bc8cff,#58a6ff);display:inline-block"></span>AI commit</span>
        <span class="ig-legend-item"><span style="width:10px;height:10px;border-radius:2px;background:#21262d;border:1px solid #484f58;display:inline-block"></span>Human commit</span>
      </div>
    </div>

    <!-- Integrity panel -->
    <div id="panel-integrity" style="display:none">
      <div class="int-form">
        <div>
          <label class="int-label" for="int-msg">Commit message</label>
          <input class="int-input" id="int-msg" placeholder="feat: add login with OAuth2">
        </div>
        <div>
          <label class="int-label" for="int-prompt">AI prompt (optional)</label>
          <textarea class="int-input int-textarea" id="int-prompt" placeholder="Describe the AI prompt used to generate this commit…"></textarea>
        </div>
        <button class="run-btn" id="btn-run" onclick="runIntegrity()">🛡 Run Integrity Check</button>
      </div>
      <div class="int-result" id="int-result"></div>
    </div>

    <!-- Memory panel -->
    <div id="panel-memory" style="display:none">
      <div class="mem-controls">
        <button class="mem-btn" id="mem-diff-btn" onclick="diffMemory()" disabled>⊕ Diff Selected</button>
        <button class="mem-btn-ghost" onclick="clearMemSel()">Clear</button>
        <span class="mem-hint" id="mem-hint">Click a snapshot to inspect · click two to diff</span>
      </div>
      <div class="mem-layout">
        <div>
          <div class="mem-snap-count" id="mem-snap-count"></div>
          <div class="mem-snap-list" id="mem-snap-list">
            <div class="empty-state"><span class="spinner"></span> Loading…</div>
          </div>
        </div>
        <div class="mem-viewer" id="mem-viewer">
          <div class="mem-viewer-empty">
            Select a snapshot to inspect its files,<br>
            or select two snapshots to compare them.
          </div>
        </div>
      </div>
    </div>

    <!-- Review Points panel -->
    <div id="panel-review" style="display:none">
      <div class="rp-controls">
        <label class="rp-label">Scan last
          <select class="rp-select" id="rp-limit">
            <option value="50">50</option>
            <option value="100" selected>100</option>
            <option value="200">200</option>
            <option value="500">500</option>
          </select>
          commits
        </label>
        <label class="rp-label">Min score
          <select class="rp-select" id="rp-min-score">
            <option value="0.15">0.15 (sensitive)</option>
            <option value="0.25" selected>0.25 (default)</option>
            <option value="0.40">0.40 (strict)</option>
            <option value="0.60">0.60 (critical only)</option>
          </select>
        </label>
        <button class="rp-btn" id="rp-load-btn" onclick="loadReviewPoints()">↻ Analyse</button>
        <span class="rp-status" id="rp-status"></span>
      </div>
      <div id="rp-list-wrap">
        <div class="empty-state" style="padding:60px 20px">Click "Analyse" to scan commits for review priorities.</div>
      </div>
    </div>
  </main>
</div>

<script>
// ── State ──────────────────────────────────────────────────────────────────
let allCommits = [];
let activeFilters = new Set();
let githubUrl = null;

// ── Utilities ─────────────────────────────────────────────────────────────
const id = s => document.getElementById(s);
const setText = (s, v) => { const el = id(s); if (el) el.textContent = v; };
const esc = s => String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
const escId = s => String(s).replace(/[^a-zA-Z0-9_-]/g, '_');

function timeAgo(iso) {
  const d = Math.floor((Date.now() - new Date(iso)) / 1000);
  if (d < 60)  return d + 's ago';
  if (d < 3600) return Math.floor(d/60) + 'm ago';
  if (d < 86400) return Math.floor(d/3600) + 'h ago';
  if (d < 2592000) return Math.floor(d/86400) + 'd ago';
  if (d < 31536000) return Math.floor(d/2592000) + 'mo ago';
  return Math.floor(d/31536000) + 'y ago';
}

function scoreColor(s) {
  return s >= 0.8 ? '#3fb950' : s >= 0.5 ? '#d29922' : '#f85149';
}

function fmt(n) { return n == null ? '—' : Number(n).toLocaleString(); }
function pct(n) { return n == null ? '—' : n.toFixed(1) + '%'; }

// ── Load ──────────────────────────────────────────────────────────────────
function loadAll() { loadRepo(); loadCommits(); }

async function loadRepo() {
  try {
    const d = await fetch('/api/repo').then(r => r.json());
    setText('repo-name', d.name || 'unknown');
    setText('branch-badge', d.branch || 'HEAD');
    setText('s-total',   d.total_commits ?? '—');
    setText('s-ai',      d.ai_commits    ?? '—');
    setText('s-tested',  d.tested_commits ?? '—');
    setText('s-passrate', d.test_pass_rate != null ? pct(d.test_pass_rate) : '—');
    setText('side-total', d.total_commits ?? '—');
    setText('side-ai',    d.ai_commits    ?? '—');
    setText('side-human', (d.total_commits - d.ai_commits) ?? '—');
    const ratio = d.total_commits > 0 ? ((d.ai_commits / d.total_commits) * 100).toFixed(1) + '%' : '—';
    setText('side-ratio', ratio);

    // GitHub repo link in header
    if (d.github_url) {
      githubUrl = d.github_url;
      const link = id('gh-repo-link');
      link.href = d.github_url;
      link.classList.add('visible');
    }

    // Sidebar pass rate
    if (d.test_pass_rate != null) {
      const el = id('side-pass-rate');
      el.textContent = pct(d.test_pass_rate);
      el.className = 'health-rate ' + (d.test_pass_rate >= 80 ? 'good' : d.test_pass_rate >= 50 ? 'warn' : 'bad');
    }
  } catch(e) { console.error('loadRepo', e); }
}

async function loadCommits() {
  id('timeline-list').innerHTML = '<div class="empty-state"><span class="spinner"></span> Loading commits…</div>';
  try {
    allCommits = await fetch('/api/commits?limit=200').then(r => r.json());
    setText('s-loaded', allCommits.length);
    setText('tab-count', allCommits.length);
    renderSparkline();
    filter();
    renderSummary();
  } catch(e) {
    id('timeline-list').innerHTML = '<div class="empty-state">⚠ Could not load commits. Is this a valid h5i repository?</div>';
  }
}

// ── Filter ────────────────────────────────────────────────────────────────
function filter() {
  const q = id('search').value.toLowerCase();
  let list = allCommits;

  if (activeFilters.has('ai'))   list = list.filter(c => c.ai_model);
  if (activeFilters.has('test')) list = list.filter(c => c.test_is_passing != null);
  if (activeFilters.has('fail')) list = list.filter(c => c.test_is_passing === false);

  if (q) {
    list = list.filter(c =>
      (c.message   || '').toLowerCase().includes(q) ||
      (c.author    || '').toLowerCase().includes(q) ||
      (c.short_oid || '').toLowerCase().includes(q) ||
      (c.ai_model  || '').toLowerCase().includes(q) ||
      (c.ai_agent  || '').toLowerCase().includes(q) ||
      (c.ai_prompt || '').toLowerCase().includes(q)
    );
  }
  render(list);
  setText('tab-count', list.length);
}

function toggleFilter(key) {
  activeFilters.has(key) ? activeFilters.delete(key) : activeFilters.add(key);
  const el = id('pill-' + key);
  el.classList.toggle('active', activeFilters.has(key));
  if (key === 'fail') el.classList.toggle('red-pill', activeFilters.has('fail'));
  filter();
}

// ── Render timeline ───────────────────────────────────────────────────────
function render(commits) {
  if (!commits.length) {
    id('timeline-list').innerHTML = '<div class="empty-state">No commits match the current filter.</div>';
    return;
  }
  id('timeline-list').innerHTML = commits.map((c, i) => commitHTML(c, i)).join('');
}

function badge(cls, icon, text) {
  return `<span class="badge ${cls}">${icon} ${esc(text)}</span>`;
}

function testBadge(c) {
  // Rich test badge: show counts when available, fall back to coverage
  if (c.test_is_passing == null) return '';

  const cls = c.test_is_passing ? 'b-test-ok' : 'b-test-fail';
  const icon = c.test_is_passing ? '🧪' : '🧪';

  if (c.test_total != null && c.test_total > 0) {
    const parts = [];
    if (c.test_passed != null)  parts.push(`<span style="color:#3fb950">✔${c.test_passed}</span>`);
    if (c.test_failed != null && c.test_failed > 0) parts.push(`<span style="color:#f85149">✖${c.test_failed}</span>`);
    if (c.test_skipped != null && c.test_skipped > 0) parts.push(`<span style="color:#d29922">⊘${c.test_skipped}</span>`);
    return `<span class="badge ${cls}">${icon} ${parts.join(' ')}</span>`;
  }
  // Legacy: just show passing/failing
  return badge(cls, icon, c.test_is_passing ? 'passing' : 'failing');
}

function commitHTML(c, i) {
  const isAI = !!c.ai_model;
  const dotCls = isAI ? 'ai-dot' : 'human-dot';
  const oidCls = isAI ? 'oid-ai' : 'oid-human';
  const dotInner = isAI ? '🤖' : '';
  const cardCls = c.test_is_passing === false ? 'failing' : (c.test_is_passing === true ? 'passing' : '');

  const delay = `animation-delay:${Math.min(i * 0.025, 0.4)}s`;

  // GitHub commit link
  const ghLink = githubUrl
    ? `<a class="gh-commit-link" href="${esc(githubUrl)}/commit/${esc(c.git_oid)}" target="_blank" rel="noopener" onclick="event.stopPropagation()">↗ GitHub</a>`
    : '';

  // Badges row
  const badges = [
    c.ai_model ? badge('b-model', '🤖', c.ai_model) : '',
    c.ai_agent && c.ai_agent !== 'unknown' ? badge('b-agent', '⚡', c.ai_agent) : '',
    testBadge(c),
    c.test_tool ? badge('b-tool', '🔧', c.test_tool) : '',
    c.test_duration_secs > 0 ? badge('b-dur', '⏱', c.test_duration_secs.toFixed(2) + 's') : '',
    c.test_coverage > 0 ? badge('b-cov', '📊', pct(c.test_coverage) + ' cov') : '',
    c.ast_file_count > 0 ? badge('b-ast', '🌳', c.ast_file_count + ' AST') : '',
    c.has_crdt ? badge('b-crdt', '🔗', 'CRDT') : '',
    c.ai_tokens ? badge('b-tok', '◦', fmt(c.ai_tokens) + ' tok') : '',
    c.caused_by && c.caused_by.length > 0 ? badge('b-cause', '⛓', c.caused_by.length === 1 ? 'caused by 1' : `caused by ${c.caused_by.length}`) : '',
  ].filter(Boolean).join('');

  // Detail rows
  const detailId = 'detail-' + i;
  const rows = [];
  if (c.ai_prompt) rows.push(`<div class="dk">prompt</div><div class="dv prompt-text">${esc(c.ai_prompt)}</div>`);
  if (c.ai_model)  rows.push(`<div class="dk">model</div><div class="dv">${esc(c.ai_model)}</div>`);
  if (c.ai_agent && c.ai_agent !== 'unknown') rows.push(`<div class="dk">agent</div><div class="dv">${esc(c.ai_agent)}</div>`);
  if (c.ai_tokens) rows.push(`<div class="dk">tokens</div><div class="dv">${fmt(c.ai_tokens)}</div>`);
  rows.push(`<div class="dk">commit</div><div class="dv mono">${esc(c.git_oid)}</div>`);
  if (c.caused_by && c.caused_by.length > 0) {
    rows.push(`<div class="dk">caused by</div><div class="dv">${c.caused_by.map(o => `<span class="oid-chip oid-human" style="font-size:10px">${esc(o.slice(0,8))}</span>`).join(' ')}</div>`);
  }

  // Test breakdown table
  let testTable = '';
  if (c.test_total != null && c.test_total > 0) {
    const summaryRow = c.test_summary ? `<tr><td colspan="2" style="color:#8b949e;font-style:italic;padding:4px 8px">${esc(c.test_summary)}</td></tr>` : '';
    testTable = `
      <div style="margin-top:8px">
        <div class="section-hdr" style="font-size:11px;color:#8b949e;margin-bottom:4px">Test Results</div>
        <table class="test-table">
          <thead><tr><th>Metric</th><th>Value</th></tr></thead>
          <tbody>
            <tr><td style="color:#8b949e">Passed</td><td class="td-pass">${c.test_passed ?? 0}</td></tr>
            <tr><td style="color:#8b949e">Failed</td><td class="td-fail">${c.test_failed ?? 0}</td></tr>
            <tr><td style="color:#8b949e">Skipped</td><td class="td-skip">${c.test_skipped ?? 0}</td></tr>
            <tr><td style="color:#8b949e">Total</td><td class="td-tot">${c.test_total}</td></tr>
            ${c.test_duration_secs > 0 ? `<tr><td style="color:#8b949e">Duration</td><td>${c.test_duration_secs.toFixed(3)}s</td></tr>` : ''}
            ${c.test_coverage > 0 ? `<tr><td style="color:#8b949e">Coverage</td><td>${pct(c.test_coverage)}</td></tr>` : ''}
            ${c.test_tool ? `<tr><td style="color:#8b949e">Tool</td><td>${esc(c.test_tool)}</td></tr>` : ''}
            ${summaryRow}
          </tbody>
        </table>
      </div>`;
  }

  return `
<div class="commit-entry" style="${delay}">
  <div class="commit-dot ${dotCls}">${dotInner}</div>
  <div class="commit-card ${cardCls}" id="card-${i}" onclick="toggleDetail(${i},'${detailId}')">
    <div class="commit-head">
      <span class="oid-chip ${oidCls}">${esc(c.short_oid)}</span>
      <span class="commit-msg">${esc(c.message)}</span>
      ${ghLink}
    </div>
    <div class="byline"><span class="author">${esc(c.author)}</span> · ${timeAgo(c.timestamp)} · <span style="color:#484f58">${new Date(c.timestamp).toLocaleDateString()}</span></div>
    <div class="badges">${badges}</div>
    <div class="audit-section" onclick="event.stopPropagation()">
      <button class="audit-btn" id="audit-btn-${i}" onclick="runCommitAudit('${esc(c.git_oid)}', ${i})">
        🛡 Audit
      </button>
      <div id="audit-result-${i}"></div>
    </div>
    <div class="commit-detail" id="${detailId}">
      <div class="detail-grid">${rows.join('')}</div>
      ${testTable}
    </div>
  </div>
</div>`;
}

function toggleDetail(i, detailId) {
  const el = id(detailId);
  const card = id('card-' + i);
  el.classList.toggle('open');
  card.classList.toggle('expanded');
}

// ── Inline commit audit ────────────────────────────────────────────────────
async function runCommitAudit(oid, idx) {
  const btn = id('audit-btn-' + idx);
  const out = id('audit-result-' + idx);
  btn.disabled = true;
  btn.textContent = '🛡 Auditing…';
  out.innerHTML = '<div style="margin-top:8px;color:#8b949e;font-size:12px"><span class="spinner"></span> Running integrity rules…</div>';

  try {
    const data = await fetch(`/api/integrity/commit?oid=${encodeURIComponent(oid)}`).then(r => r.json());
    out.innerHTML = `<div class="audit-result-box">${renderIntegrityHTML(data, 'ar-' + idx)}</div>`;
    btn.textContent = '🛡 Re-audit';
  } catch(e) {
    out.innerHTML = `<div class="audit-result-box" style="color:#f85149;font-size:12px">⚠ Audit failed: ${esc(String(e))}</div>`;
    btn.textContent = '🛡 Retry';
  } finally {
    btn.disabled = false;
  }
}

function toggleRulesDetail(panelId, toggleId) {
  const panel = id(panelId);
  const toggle = id(toggleId);
  const open = panel.classList.toggle('open');
  toggle.textContent = open ? '▾ Hide rule details' : '▸ Show all rules checked';
}

// ── Integrity panel ────────────────────────────────────────────────────────
async function runIntegrity() {
  const msg  = id('int-msg').value.trim();
  const prmt = id('int-prompt').value.trim();
  if (!msg) { id('int-msg').focus(); return; }

  const btn = id('btn-run');
  const out = id('int-result');
  btn.disabled = true;
  btn.textContent = 'Checking…';
  out.innerHTML = '<div style="color:#8b949e;font-size:12px"><span class="spinner"></span> Running rules…</div>';

  try {
    const p = new URLSearchParams({ message: msg });
    if (prmt) p.set('prompt', prmt);
    const data = await fetch('/api/integrity?' + p).then(r => r.json());
    out.innerHTML = `<div class="int-report">${renderIntegrityHTML(data, 'int-panel')}</div>`;
  } catch(e) {
    out.innerHTML = `<div style="color:#f85149;font-size:12px">⚠ Request failed: ${esc(String(e))}</div>`;
  } finally {
    btn.disabled = false;
    btn.textContent = '🛡 Run Integrity Check';
  }
}

const ALL_RULES = [
  { id: 'CREDENTIAL_LEAK',       sev: 'Violation', desc: 'Hardcoded secrets, API keys, or PEM private-key headers' },
  { id: 'CODE_EXECUTION',        sev: 'Violation', desc: 'Shell exec, eval, subprocess, or dynamic code execution patterns' },
  { id: 'CI_CD_MODIFIED',        sev: 'Warning',   desc: 'CI/CD pipeline or workflow file changed' },
  { id: 'SENSITIVE_FILE_MODIFIED',sev: 'Warning',   desc: 'Security-sensitive file modified (.env, auth config, secrets)' },
  { id: 'LOCKFILE_MODIFIED',     sev: 'Warning',   desc: 'Dependency lockfile changed (supply-chain risk)' },
  { id: 'UNDECLARED_DELETION',   sev: 'Warning',   desc: 'Files deleted without mention in commit message' },
  { id: 'SCOPE_EXPANSION',       sev: 'Warning',   desc: 'Diff touches many more files than message scope implies' },
  { id: 'LARGE_DIFF',            sev: 'Warning',   desc: 'Diff is unusually large (>500 lines changed)' },
  { id: 'REFACTOR_ANOMALY',      sev: 'Warning',   desc: 'High churn with no test changes detected' },
  { id: 'PERMISSION_CHANGE',     sev: 'Warning',   desc: 'File permission or ownership changes detected' },
  { id: 'BINARY_FILE_CHANGED',   sev: 'Warning',   desc: 'Binary file added or modified' },
  { id: 'CONFIG_FILE_MODIFIED',  sev: 'Warning',   desc: 'Configuration file modified' },
];

function renderIntegrityHTML(data, uid) {
  const lvClass = { Valid: 'lv-valid', Warning: 'lv-warning', Violation: 'lv-violation' }[data.level] || 'lv-valid';
  const score = Math.round((data.score || 0) * 100);
  const color = scoreColor(data.score || 0);
  const findings = data.findings || [];

  const findingsHTML = findings.map(f => {
    const [cls, icon] = f.severity === 'Violation' ? ['rv','✖'] : f.severity === 'Warning' ? ['rw','⚠'] : ['ri','ℹ'];
    return `<div class="finding ${cls}">
      <span class="finding-icon">${icon}</span>
      <span class="finding-rule">${esc(f.rule_id)}</span>
      <span class="finding-detail">${esc(f.detail)}</span>
    </div>`;
  }).join('');

  const body = findingsHTML
    ? `<div class="ir-findings">${findingsHTML}</div>`
    : `<div class="success-msg">✓ All rules passed — no issues detected.</div>`;

  // Rules detail panel
  const triggeredIds = new Set(findings.map(f => f.rule_id));
  const panelId   = (uid || 'global') + '-rules-panel';
  const toggleId  = (uid || 'global') + '-rules-toggle';
  const rulesRows = ALL_RULES.map(r => {
    const hit = triggeredIds.has(r.id);
    const hitSev = hit ? findings.find(f => f.rule_id === r.id)?.severity : null;
    const [icon, cls] = hitSev === 'Violation' ? ['✖','rule-fail'] : hitSev === 'Warning' ? ['⚠','rule-warn'] : ['✔','rule-pass'];
    return `<div class="rule-row">
      <span class="${cls}" style="font-size:12px;width:14px;text-align:center">${icon}</span>
      <span class="rule-id-label">${esc(r.id)}</span>
      <span style="font-size:10px;color:#484f58">${esc(r.desc)}</span>
    </div>`;
  }).join('');

  return `
    <div class="ir-header">
      <span class="${lvClass}">${data.level}</span>
      <span class="ir-score" style="color:${color}">${score}<span style="font-size:16px;color:#8b949e">%</span></span>
      <span class="ir-label">Integrity score</span>
    </div>
    ${body}
    <button class="rules-detail-toggle" id="${escId(toggleId)}" onclick="toggleRulesDetail('${escId(panelId)}','${escId(toggleId)}')">▸ Show all rules checked</button>
    <div class="rules-detail-panel" id="${escId(panelId)}">${rulesRows}</div>`;
}

// ── Summary tab ────────────────────────────────────────────────────────────
function renderSummary() {
  const commits = allCommits;
  if (!commits.length) return;

  const withTests = commits.filter(c => c.test_is_passing != null);
  const passing   = withTests.filter(c => c.test_is_passing).length;
  const failing   = withTests.length - passing;
  const aiCount   = commits.filter(c => c.ai_model).length;
  const totalRan  = commits.reduce((s, c) => s + (c.test_total || 0), 0);

  // Summary cards
  id('sum-cards').innerHTML = [
    sumCard(commits.length,    'Total commits',      '#58a6ff'),
    sumCard(aiCount,           'AI-assisted',        '#bc8cff'),
    sumCard(withTests.length,  'Commits with tests', '#d29922'),
    sumCard(passing + '/' + withTests.length, 'Passing / tested', withTests.length && failing === 0 ? '#3fb950' : '#f85149'),
    sumCard(fmt(totalRan),     'Total tests run',    '#8b949e'),
  ].join('');

  // Charts area
  id('sum-charts').innerHTML = agentChartHTML(commits) + failureListHTML(commits);
}

function sumCard(val, label, color) {
  return `<div class="sum-card"><div class="sum-num" style="color:${color}">${val}</div><div class="sum-label">${label}</div></div>`;
}

function agentChartHTML(commits) {
  const counts = {};
  commits.forEach(c => {
    if (c.ai_agent && c.ai_agent !== 'unknown') {
      counts[c.ai_agent] = (counts[c.ai_agent] || 0) + 1;
    } else if (!c.ai_model) {
      counts['Human'] = (counts['Human'] || 0) + 1;
    }
  });
  const sorted = Object.entries(counts).sort((a, b) => b[1] - a[1]).slice(0, 8);
  const max = sorted[0]?.[1] || 1;
  const rows = sorted.map(([name, cnt]) => `
    <tr>
      <td style="color:${name === 'Human' ? '#58a6ff' : '#bc8cff'}">${esc(name)}</td>
      <td><div class="agent-bar-bg"><div class="agent-bar-fill" style="width:${(cnt/max*100).toFixed(1)}%"></div></div></td>
      <td style="color:#8b949e;text-align:right">${cnt}</td>
    </tr>`).join('');
  return `<div class="chart-section">
    <div class="chart-title">Commits by Agent / Author</div>
    <table class="agent-table"><thead><tr><th>Agent</th><th>Activity</th><th>Count</th></tr></thead><tbody>${rows}</tbody></table>
  </div>`;
}

function failureListHTML(commits) {
  const failures = commits.filter(c => c.test_is_passing === false).slice(0, 10);
  const body = failures.length
    ? failures.map(c => `<div class="fail-item">
        <span class="fail-oid">${esc(c.short_oid)}</span>
        <span class="fail-msg">${esc(c.message)}</span>
        ${c.test_failed ? `<span class="fail-counts">✖${c.test_failed}</span>` : ''}
      </div>`).join('')
    : '<div style="color:#3fb950;font-size:12px;padding:8px 0">✓ No recent test failures.</div>';

  return `<div class="chart-section">
    <div class="chart-title">Recent Test Failures</div>
    <div class="fail-list">${body}</div>
  </div>`;
}

// ── Sparkline ──────────────────────────────────────────────────────────────
function renderSparkline() {
  const pts = allCommits
    .filter(c => c.test_is_passing != null)
    .slice(0, 30)
    .reverse(); // oldest first

  if (pts.length < 2) return;

  const W = 180, H = 40, pad = 4;
  const xStep = (W - 2 * pad) / (pts.length - 1);

  // Compute y from pass rate per commit
  const ys = pts.map(c => {
    if (c.test_total > 0) {
      return 1 - (c.test_passed || 0) / c.test_total;
    }
    return c.test_is_passing ? 0 : 1;
  });

  const points = pts.map((_, i) => {
    const x = pad + i * xStep;
    const y = pad + ys[i] * (H - 2 * pad);
    return [x, y];
  });

  const polyline = points.map(p => p[0].toFixed(1) + ',' + p[1].toFixed(1)).join(' ');

  // Fill area
  const fillPts = `${points[0][0]},${H} ` + polyline + ` ${points[points.length-1][0]},${H}`;

  // Dots (colored by pass/fail)
  const dots = pts.map((c, i) => {
    const [x, y] = points[i];
    const col = c.test_is_passing ? '#3fb950' : '#f85149';
    return `<circle cx="${x.toFixed(1)}" cy="${y.toFixed(1)}" r="2.5" fill="${col}" opacity=".9"/>`;
  }).join('');

  const svg = `
    <defs>
      <linearGradient id="spk-grad" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stop-color="#3fb950" stop-opacity=".3"/>
        <stop offset="100%" stop-color="#3fb950" stop-opacity="0"/>
      </linearGradient>
    </defs>
    <polygon points="${fillPts}" fill="url(#spk-grad)"/>
    <polyline points="${polyline}" fill="none" stroke="#3fb950" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round"/>
    ${dots}`;

  id('sparkline-svg').innerHTML = svg;
  id('sparkline-label').textContent = `last ${pts.length} commits with tests`;
}

// ── Tab switching ──────────────────────────────────────────────────────────
function switchTab(tab) {
  ['timeline','summary','integrity','intentgraph','review','memory'].forEach(t => {
    const btn = document.querySelector(`.tab[onclick="switchTab('${t}')"]`);
    const panel = id('panel-' + t);
    const active = t === tab;
    if (btn) btn.classList.toggle('active', active);
    if (panel) panel.style.display = active ? '' : 'none';
  });
}

// ── Review Points ──────────────────────────────────────────────────────────
const RULE_COLORS = {
  TEST_REGRESSION:     'rp-rule-red',
  INTEGRITY_VIOLATION: 'rp-rule-red',
  INTEGRITY_WARNING:   'rp-rule-yellow',
  LARGE_DIFF:          'rp-rule-yellow',
  WIDE_IMPACT:         'rp-rule-yellow',
  CROSS_CUTTING:       'rp-rule-yellow',
  UNTESTED_CHANGE:     'rp-rule-blue',
  AI_NO_PROMPT:        'rp-rule-gray',
  BURST_AFTER_GAP:     'rp-rule-blue',
  POLYGLOT_CHANGE:     'rp-rule-gray',
  BINARY_FILE:         'rp-rule-blue',
  MASS_DELETION:       'rp-rule-yellow',
};

async function loadReviewPoints() {
  const limit    = id('rp-limit').value;
  const minScore = id('rp-min-score').value;
  const btn      = id('rp-load-btn');
  const wrap     = id('rp-list-wrap');
  const status   = id('rp-status');

  btn.disabled = true;
  status.textContent = '⏳ Scanning…';
  wrap.innerHTML = '<div class="empty-state"><span class="spinner"></span> Analysing commits…</div>';

  try {
    const pts = await fetch(`/api/review-points?limit=${limit}&min_score=${minScore}`).then(r => r.json());
    setText('tab-review-count', pts.length);

    if (!pts || pts.length === 0) {
      wrap.innerHTML = '<div class="empty-state">No commits exceeded the review threshold in the scanned range.</div>';
      status.textContent = `0 flagged in ${limit} commits`;
      return;
    }

    const items = pts.map((rp, i) => {
      const score = rp.score;
      const scoreCls = score >= 0.7 ? 'rp-score-high' : score >= 0.45 ? 'rp-score-med' : 'rp-score-low';
      const cardCls  = score >= 0.7 ? 'high' : score >= 0.45 ? 'medium' : 'low';
      const filled   = Math.round(score * 10);
      const bar      = '█'.repeat(filled) + '░'.repeat(10 - filled);
      const ts       = new Date(rp.timestamp);
      const dateStr  = isNaN(ts) ? '' : ts.toLocaleDateString(undefined, {year:'numeric',month:'short',day:'numeric'});

      const triggers = rp.triggers.map(t => {
        const cls = RULE_COLORS[t.rule_id] || 'rp-rule-gray';
        return `<div class="rp-trigger">
          <span class="rp-rule ${cls}">${esc(t.rule_id)}</span>
          <span class="rp-detail">${esc(t.detail)}</span>
        </div>`;
      }).join('');

      return `<div class="rp-card ${cardCls}">
        <div class="rp-head">
          <span class="rp-rank">#${i+1}</span>
          <code class="oid-chip oid-human" style="font-size:11px">${esc(rp.short_oid)}</code>
          <span class="rp-score-pill ${scoreCls}">${score.toFixed(2)}</span>
          <span class="rp-bar">${bar}</span>
          <span class="rp-msg" title="${esc(rp.message)}">${esc(rp.message)}</span>
        </div>
        <div class="rp-meta">
          <span class="author" style="color:#58a6ff">${esc(rp.author)}</span>
          ${dateStr ? `· <span>${esc(dateStr)}</span>` : ''}
        </div>
        <div class="rp-triggers">${triggers}</div>
      </div>`;
    }).join('');

    wrap.innerHTML = `<div class="rp-list">${items}</div>`;
    status.textContent = `${pts.length} flagged in ${limit} commits`;
  } catch(e) {
    wrap.innerHTML = `<div class="empty-state">Error: ${esc(e.message)}</div>`;
    status.textContent = '';
  } finally {
    btn.disabled = false;
  }
}

// ── Intent Graph ──────────────────────────────────────────────────────────
const NODE_W = 220, NODE_H = 56, COL_GAP = 280, ROW_GAP = 86;

async function loadIntentGraph() {
  const limit = id('ig-limit').value;
  const mode  = id('ig-mode').value;
  const btn   = id('ig-load-btn');
  const wrap  = id('ig-canvas-wrap');
  const status = id('ig-status');

  btn.disabled = true;
  status.textContent = mode === 'analyze' ? '⏳ Calling Claude…' : '⏳ Loading…';
  wrap.innerHTML = '<div class="empty-state"><span class="spinner"></span> Building graph…</div>';

  try {
    const g = await fetch(`/api/intent-graph?limit=${limit}&mode=${mode}`).then(r => r.json());
    if (!g.nodes || g.nodes.length === 0) {
      wrap.innerHTML = '<div class="empty-state">No commits found.</div>';
      status.textContent = '';
      return;
    }
    renderIntentGraph(g, wrap);
    const causal = (g.edges || []).filter(e => e.kind === 'causal').length;
    status.textContent = `${g.nodes.length} nodes · ${causal} causal link${causal===1?'':'s'}`;
  } catch(e) {
    wrap.innerHTML = `<div class="empty-state">Error: ${esc(e.message)}</div>`;
    status.textContent = '';
  } finally {
    btn.disabled = false;
  }
}

function renderIntentGraph(graph, container) {
  const nodes = graph.nodes;    // newest first
  const edges = graph.edges || [];

  // ── Layout ──
  // We use a simple layered layout:
  // - Compute "depth" for each node via causal edges (BFS from roots).
  // - Nodes with no causal parents are depth 0 (leftmost column).
  // - Nodes within a column are stacked vertically.
  const oidIdx = {};
  nodes.forEach((n, i) => { oidIdx[n.oid] = i; });

  const causalEdges = edges.filter(e => e.kind === 'causal');

  // depth = column (left = oldest root, right = latest effect)
  const depth = new Array(nodes.length).fill(0);
  // BFS — edges go from → to, meaning from is parent, to is child
  const adj = {};  // oid → array of child oids (causal children)
  causalEdges.forEach(e => {
    if (!adj[e.from]) adj[e.from] = [];
    adj[e.from].push(e.to);
  });

  // For nodes with no causal parents, keep depth=0. Propagate forward.
  const inDegree = {};
  nodes.forEach(n => { inDegree[n.oid] = 0; });
  causalEdges.forEach(e => { if (inDegree[e.to] !== undefined) inDegree[e.to]++; });

  const queue = nodes.filter(n => inDegree[n.oid] === 0).map(n => n.oid);
  while (queue.length) {
    const cur = queue.shift();
    const children = adj[cur] || [];
    children.forEach(childOid => {
      if (oidIdx[childOid] !== undefined) {
        const curDepth = depth[oidIdx[cur]];
        if (curDepth + 1 > depth[oidIdx[childOid]]) {
          depth[oidIdx[childOid]] = curDepth + 1;
        }
        inDegree[childOid]--;
        if (inDegree[childOid] === 0) queue.push(childOid);
      }
    });
  }

  // Assign row within each column
  const colRows = {};
  const pos = {};
  nodes.forEach((n, i) => {
    const col = depth[i];
    if (colRows[col] === undefined) colRows[col] = 0;
    pos[n.oid] = { x: col * COL_GAP + 20, y: colRows[col] * ROW_GAP + 20 };
    colRows[col]++;
  });

  const maxX = Math.max(...Object.values(pos).map(p => p.x)) + NODE_W + 20;
  const maxY = Math.max(...Object.values(pos).map(p => p.y)) + NODE_H + 20;
  const svgW = Math.max(maxX, 600);
  const svgH = Math.max(maxY, 300);

  // ── SVG ──
  const svgNS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(svgNS, 'svg');
  svg.setAttribute('width', svgW);
  svg.setAttribute('height', svgH);
  svg.setAttribute('class', 'ig-svg');
  svg.setAttribute('viewBox', `0 0 ${svgW} ${svgH}`);

  // Defs: arrow markers
  const defs = document.createElementNS(svgNS, 'defs');
  ['parent','causal'].forEach(kind => {
    const marker = document.createElementNS(svgNS, 'marker');
    marker.setAttribute('id', `arrow-${kind}`);
    marker.setAttribute('markerWidth', '8');
    marker.setAttribute('markerHeight', '8');
    marker.setAttribute('refX', '6');
    marker.setAttribute('refY', '3');
    marker.setAttribute('orient', 'auto');
    const poly = document.createElementNS(svgNS, 'polygon');
    poly.setAttribute('points', '0 0, 6 3, 0 6');
    poly.setAttribute('fill', kind === 'causal' ? '#bc8cff' : '#484f58');
    marker.appendChild(poly);
    defs.appendChild(marker);
  });
  svg.appendChild(defs);

  // Draw edges
  edges.forEach(e => {
    const from = pos[e.from];
    const to   = pos[e.to];
    if (!from || !to) return;

    const isParent = e.kind === 'parent';
    const x1 = from.x + NODE_W, y1 = from.y + NODE_H / 2;
    const x2 = to.x,            y2 = to.y + NODE_H / 2;

    const path = document.createElementNS(svgNS, 'path');
    const cx1 = x1 + (x2 - x1) * 0.5, cy1 = y1;
    const cx2 = x1 + (x2 - x1) * 0.5, cy2 = y2;
    path.setAttribute('d', `M${x1},${y1} C${cx1},${cy1} ${cx2},${cy2} ${x2},${y2}`);
    path.setAttribute('class', isParent ? 'ig-edge' : 'ig-edge-causal');
    path.setAttribute('stroke', isParent ? '#2d333b' : '#bc8cff88');
    path.setAttribute('stroke-dasharray', isParent ? '4,3' : 'none');
    path.setAttribute('marker-end', `url(#arrow-${e.kind})`);
    svg.appendChild(path);
  });

  // Draw nodes — rect MUST be appended first so text renders on top
  nodes.forEach(n => {
    const p = pos[n.oid];
    if (!p) return;
    const g = document.createElementNS(svgNS, 'g');
    g.setAttribute('class', 'ig-node');
    g.setAttribute('transform', `translate(${p.x},${p.y})`);

    // 1. Background rect (drawn first so text sits on top)
    const rect = document.createElementNS(svgNS, 'rect');
    rect.setAttribute('width', NODE_W);
    rect.setAttribute('height', NODE_H);
    rect.setAttribute('rx', 6);
    rect.setAttribute('fill', n.is_ai ? '#1a1f2e' : '#161b22');
    rect.setAttribute('stroke', n.is_ai ? '#bc8cff66' : '#30363d');
    g.appendChild(rect);

    // 2. Tooltip (SVG <title> — not painted, but first for accessibility)
    const title = document.createElementNS(svgNS, 'title');
    title.textContent = `${n.short_oid}: ${n.message}\nIntent (${n.intent_source}): ${n.intent}\nAuthor: ${n.author}`;
    g.appendChild(title);

    // 3. OID + source tag
    const srcColor = n.intent_source === 'analyzed' ? '#3fb950' : n.intent_source === 'prompt' ? '#bc8cff' : '#484f58';
    const oidText = document.createElementNS(svgNS, 'text');
    oidText.setAttribute('x', 10);
    oidText.setAttribute('y', 14);
    oidText.setAttribute('font-size', 10);
    oidText.setAttribute('font-family', 'monospace');
    oidText.setAttribute('fill', n.is_ai ? '#bc8cff' : '#58a6ff');
    oidText.textContent = n.short_oid;
    g.appendChild(oidText);

    // Source badge (right side of OID row)
    const srcTag = document.createElementNS(svgNS, 'text');
    srcTag.setAttribute('x', NODE_W - 8);
    srcTag.setAttribute('y', 14);
    srcTag.setAttribute('font-size', 9);
    srcTag.setAttribute('text-anchor', 'end');
    srcTag.setAttribute('fill', srcColor);
    srcTag.textContent = n.intent_source === 'analyzed' ? '[Claude]' : n.intent_source === 'prompt' ? '[prompt]' : '[msg]';
    g.appendChild(srcTag);

    // 4. Intent text wrapped to 2 lines
    const intentWords = (n.intent || '').split(' ');
    const maxChars = Math.floor((NODE_W - 20) / 6.2);
    let lines = [], cur = '';
    intentWords.forEach(w => {
      const test = cur ? cur + ' ' + w : w;
      if (test.length > maxChars && cur) { lines.push(cur); cur = w; }
      else { cur = test; }
    });
    if (cur) lines.push(cur);
    lines = lines.slice(0, 2);
    if (lines.length === 2) {
      const allWords = intentWords.length;
      const shownWords = lines.join(' ').split(' ').length;
      if (shownWords < allWords) lines[1] = lines[1].replace(/\s*\S+$/, '…');
    }
    lines.forEach((line, li) => {
      const t = document.createElementNS(svgNS, 'text');
      t.setAttribute('x', 10);
      t.setAttribute('y', 30 + li * 14);
      t.setAttribute('font-size', 11);
      t.setAttribute('fill', '#c9d1d9');
      t.textContent = line;
      g.appendChild(t);
    });

    // Click → open detail modal
    g.style.cursor = 'pointer';
    g.addEventListener('click', () => openNodeModal(n, graph));

    svg.appendChild(g);
  });

  container.innerHTML = '';
  container.appendChild(svg);
}

// ── Node detail modal ─────────────────────────────────────────────────────
function openNodeModal(n, graph) {
  const srcColor = n.intent_source === 'analyzed' ? '#3fb950' : n.intent_source === 'prompt' ? '#bc8cff' : '#484f58';
  const srcLabel = n.intent_source === 'analyzed' ? 'Claude-generated' : n.intent_source === 'prompt' ? 'stored prompt' : 'commit message';

  id('ig-modal-oid').textContent = n.short_oid + ' — ' + n.oid;
  id('ig-modal-oid').style.color = n.is_ai ? '#bc8cff' : '#58a6ff';
  id('ig-modal-msg').textContent = n.message;
  id('ig-modal-intent').textContent = n.intent;

  // Meta badges
  const meta = id('ig-modal-meta');
  meta.innerHTML = '';
  const addBadge = (text, bg, color) => {
    const s = document.createElement('span');
    s.className = 'ig-modal-badge';
    s.style.background = bg;
    s.style.color = color;
    s.textContent = text;
    meta.appendChild(s);
  };
  addBadge('source: ' + srcLabel, srcColor + '22', srcColor);
  if (n.author) addBadge('author: ' + n.author, '#21262d', '#8b949e');
  if (n.agent)  addBadge('agent: ' + n.agent,   '#d2992222', '#d29922');
  if (n.model)  addBadge('model: ' + n.model,   '#bc8cff22', '#bc8cff');

  // Causal links involving this node
  const causes = (graph.edges || []).filter(e => e.kind === 'causal' && e.to === n.oid);
  const effects = (graph.edges || []).filter(e => e.kind === 'causal' && e.from === n.oid);
  causes.forEach(e => addBadge('caused by: ' + e.from.slice(0,8), '#1f3a5f', '#58a6ff'));
  effects.forEach(e => addBadge('causes: ' + e.to.slice(0,8), '#1f3a5f', '#58a6ff'));

  const ts = new Date(n.timestamp);
  if (!isNaN(ts)) addBadge(ts.toLocaleString(), '#21262d', '#484f58');

  id('ig-modal-overlay').classList.add('open');
}

function closeNodeModal() {
  id('ig-modal-overlay').classList.remove('open');
}

document.addEventListener('keydown', e => { if (e.key === 'Escape') closeNodeModal(); });

// ── Memory ────────────────────────────────────────────────────────────────
let memSnapshots = [];
let memSelFrom = null;
let memSelTo = null;
let _memFiles = [];

async function loadMemorySnapshots() {
  if (memSnapshots.length) return; // already loaded
  id('mem-snap-list').innerHTML = '<div class="empty-state"><span class="spinner"></span> Loading…</div>';
  try {
    memSnapshots = await fetch('/api/memory/snapshots').then(r => r.json());
    setText('tab-mem-count', memSnapshots.length);
    setText('mem-snap-count', memSnapshots.length + ' snapshot' + (memSnapshots.length === 1 ? '' : 's'));
    renderMemList();
  } catch(e) {
    id('mem-snap-list').innerHTML = '<div class="empty-state">Failed to load snapshots.</div>';
  }
}

function renderMemList() {
  if (!memSnapshots.length) {
    id('mem-snap-list').innerHTML = `<div class="empty-state" style="padding:30px 0;text-align:center">
      No memory snapshots yet.<br><code style="font-size:11px;color:#8b949e">h5i memory snapshot</code> to create one.
    </div>`;
    return;
  }
  id('mem-snap-list').innerHTML = memSnapshots.map((s, i) => {
    let cls = 'mem-snap-card';
    let badge = '';
    if (i === memSelFrom) { cls += ' sel-from'; badge = '<span class="mem-sel-badge mem-sel-from-badge">FROM</span>'; }
    else if (i === memSelTo) { cls += ' sel-to'; badge = '<span class="mem-sel-badge mem-sel-to-badge">TO</span>'; }
    const ts = new Date(s.timestamp);
    const tsStr = isNaN(ts) ? s.timestamp : ts.toLocaleString();
    return `<div class="${cls}" onclick="selectMemSnap(${i})">${badge}
      <div class="mem-snap-head">
        <span class="mem-oid">${esc(s.short_oid)}</span>
        <span class="mem-ts">${esc(tsStr)}</span>
      </div>
      <div class="mem-nfiles">${s.file_count} file${s.file_count === 1 ? '' : 's'}</div>
    </div>`;
  }).join('');
}

function selectMemSnap(i) {
  if (memSelFrom === null) {
    memSelFrom = i;
  } else if (memSelTo === null && i !== memSelFrom) {
    memSelTo = i;
  } else {
    memSelFrom = i; memSelTo = null;
  }
  renderMemList();
  updateMemControls();
  if (memSelTo === null && memSelFrom !== null) showMemInspect(memSnapshots[memSelFrom]);
}

function clearMemSel() {
  memSelFrom = null; memSelTo = null;
  renderMemList(); updateMemControls();
  id('mem-viewer').innerHTML = '<div class="mem-viewer-empty">Select a snapshot to inspect its files,<br>or select two snapshots to compare them.</div>';
}

function updateMemControls() {
  id('mem-diff-btn').disabled = !(memSelFrom !== null && memSelTo !== null);
  const hint = memSelFrom === null
    ? 'Click a snapshot to inspect · click two to diff'
    : memSelTo === null
    ? 'Click another snapshot to compare, or click again to reset'
    : 'Ready — click ⊕ Diff Selected';
  setText('mem-hint', hint);
}

function showMemInspect(snap) {
  _memFiles = snap.files || [];
  if (!_memFiles.length) {
    id('mem-viewer').innerHTML = '<div class="mem-viewer-empty">No files in this snapshot.</div>';
    return;
  }
  const ts = new Date(snap.timestamp);
  const tsStr = isNaN(ts) ? snap.timestamp : ts.toLocaleString();
  let html = `<div class="mem-insp-hdr">
    <span>📸 Snapshot</span>
    <span class="mem-oid">${esc(snap.short_oid)}</span>
    <span style="font-size:11px;color:#484f58;font-weight:400">${esc(tsStr)}</span>
  </div>`;
  html += '<div class="mem-file-tabs">';
  _memFiles.forEach((f, i) => {
    html += `<button class="mem-ftab${i===0?' active':''}" onclick="showMemFile(${i},this)">${esc(f.name)}</button>`;
  });
  html += '</div><div id="mem-file-body">' + renderMemFile(_memFiles[0]) + '</div>';
  id('mem-viewer').innerHTML = html;
}

function showMemFile(i, el) {
  document.querySelectorAll('.mem-ftab').forEach(t => t.classList.remove('active'));
  el.classList.add('active');
  id('mem-file-body').innerHTML = renderMemFile(_memFiles[i]);
}

function parseFrontmatter(content) {
  const m = content.match(/^---\n([\s\S]*?)\n---\n?([\s\S]*)$/);
  if (!m) return null;
  const fm = {};
  m[1].split('\n').forEach(line => {
    const idx = line.indexOf(':');
    if (idx > 0) { fm[line.slice(0,idx).trim()] = line.slice(idx+1).trim(); }
  });
  fm.body = m[2].trim();
  return fm;
}

function renderMemFile(f) {
  const fm = parseFrontmatter(f.content);
  if (!fm) return `<pre class="mem-file-content">${esc(f.content)}</pre>`;
  const typeTag = fm.type
    ? `<span class="mem-type-${esc(fm.type)}">${esc(fm.type)}</span>`
    : '';
  let html = '<div class="mem-frontmatter">';
  if (fm.name) html += `<div class="mem-fm-row"><span class="mem-fm-key">name</span><span class="mem-fm-val">${esc(fm.name)}</span></div>`;
  if (fm.description) html += `<div class="mem-fm-row"><span class="mem-fm-key">description</span><span class="mem-fm-val mem-fm-desc">${esc(fm.description)}</span></div>`;
  if (fm.type) html += `<div class="mem-fm-row"><span class="mem-fm-key">type</span>${typeTag}</div>`;
  html += '</div>';
  if (fm.body) html += `<div class="mem-body">${esc(fm.body)}</div>`;
  return html;
}

async function diffMemory() {
  if (memSelFrom === null || memSelTo === null) return;
  const from = memSnapshots[memSelFrom].commit_oid;
  const to   = memSnapshots[memSelTo].commit_oid;
  id('mem-viewer').innerHTML = '<div class="mem-viewer-empty"><span class="spinner"></span> Computing diff…</div>';
  try {
    const diff = await fetch(`/api/memory/diff?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`).then(r => r.json());
    renderMemDiff(diff);
  } catch(e) {
    id('mem-viewer').innerHTML = `<div class="mem-viewer-empty">Error: ${esc(String(e))}</div>`;
  }
}

function renderMemDiff(diff) {
  const total = diff.added_files.length + diff.removed_files.length + diff.modified_files.length;
  let html = `<div class="mem-diff-hdr-row">
    Diff&nbsp;<span class="mem-oid" style="color:#58a6ff">${esc(diff.from_label)}</span>
    <span style="color:#484f58">→</span>
    <span class="mem-oid" style="background:#1a3a2a22;color:#3fb950">${esc(diff.to_label)}</span>
  </div>`;

  if (total === 0) {
    html += '<div class="mem-viewer-empty" style="padding:40px 0">No differences found between these two snapshots.</div>';
    id('mem-viewer').innerHTML = html;
    return;
  }

  html += '<div class="mem-diff-summary">';
  if (diff.added_files.length)   html += `<span class="mem-diff-stat-add">+${diff.added_files.length} added</span>`;
  if (diff.removed_files.length) html += `<span class="mem-diff-stat-rm">−${diff.removed_files.length} removed</span>`;
  if (diff.modified_files.length) html += `<span class="mem-diff-stat-mod">~${diff.modified_files.length} modified</span>`;
  html += '</div>';

  diff.added_files.forEach(f => {
    html += `<div class="mem-diff-file"><div class="mem-diff-hdr mem-diff-hdr-add">+&nbsp;${esc(f.name)}</div><div class="mem-diff-lines">`;
    f.content.split('\n').forEach(ln => {
      html += `<div class="mem-dl mem-dl-add"><span class="mem-gutter">+</span><span class="mem-text">${esc(ln)}</span></div>`;
    });
    html += '</div></div>';
  });

  diff.removed_files.forEach(name => {
    html += `<div class="mem-diff-file"><div class="mem-diff-hdr mem-diff-hdr-rm">−&nbsp;${esc(name)}</div>
      <div class="mem-diff-lines"><div class="mem-dl mem-dl-rm"><span class="mem-gutter">−</span><span class="mem-text" style="opacity:.6;font-style:italic">(file removed)</span></div></div></div>`;
  });

  diff.modified_files.forEach(f => {
    html += `<div class="mem-diff-file"><div class="mem-diff-hdr mem-diff-hdr-mod">~&nbsp;${esc(f.name)}</div><div class="mem-diff-lines">`;
    f.hunks.forEach(line => {
      const isSep = line.kind === 'context' && line.text === '···';
      const cls   = line.kind === 'added' ? 'mem-dl-add' : line.kind === 'removed' ? 'mem-dl-rm' : isSep ? 'mem-dl-sep' : 'mem-dl-ctx';
      const glyph = line.kind === 'added' ? '+' : line.kind === 'removed' ? '−' : ' ';
      html += `<div class="mem-dl ${cls}"><span class="mem-gutter">${isSep?'':glyph}</span><span class="mem-text">${esc(line.text)}</span></div>`;
    });
    html += '</div></div>';
  });

  id('mem-viewer').innerHTML = html;
}

// ── Boot ──────────────────────────────────────────────────────────────────
loadAll();
</script>
</body>
</html>
"##;
