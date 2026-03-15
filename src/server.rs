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

use crate::metadata::IntegrityReport;
use crate::repository::H5iRepository;

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

// ── Server entry point ────────────────────────────────────────────────────────

pub async fn serve(repo_path: PathBuf, port: u16) -> anyhow::Result<()> {
    let state = Arc::new(AppState { repo_path });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/repo", get(api_repo))
        .route("/api/commits", get(api_commits))
        .route("/api/integrity", get(api_integrity))
        .route("/api/integrity/commit", get(api_integrity_commit))
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
  ].filter(Boolean).join('');

  // Detail rows
  const detailId = 'detail-' + i;
  const rows = [];
  if (c.ai_prompt) rows.push(`<div class="dk">prompt</div><div class="dv prompt-text">${esc(c.ai_prompt)}</div>`);
  if (c.ai_model)  rows.push(`<div class="dk">model</div><div class="dv">${esc(c.ai_model)}</div>`);
  if (c.ai_agent && c.ai_agent !== 'unknown') rows.push(`<div class="dk">agent</div><div class="dv">${esc(c.ai_agent)}</div>`);
  if (c.ai_tokens) rows.push(`<div class="dk">tokens</div><div class="dv">${fmt(c.ai_tokens)}</div>`);
  rows.push(`<div class="dk">commit</div><div class="dv mono">${esc(c.git_oid)}</div>`);

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
  ['timeline','summary','integrity'].forEach(t => {
    const btn = document.querySelector(`.tab[onclick="switchTab('${t}')"]`);
    const panel = id('panel-' + t);
    const active = t === tab;
    btn.classList.toggle('active', active);
    if (panel) panel.style.display = active ? '' : 'none';
  });
}

// ── Boot ──────────────────────────────────────────────────────────────────
loadAll();
</script>
</body>
</html>
"##;
