/// Parses Claude Code conversation JSONL logs and extracts:
/// - Exploration footprint (files consulted vs edited)
/// - Causal chain (trigger → decisions → edits)
/// - Uncertainty annotations (from thinking blocks)
/// - File churn statistics
/// - Replay hash (SHA-256 of raw JSONL for reproducibility)
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::H5iError;

// ── Uncertainty signal table ─────────────────────────────────────────────────
// (phrase_to_match_lowercased, estimated_confidence_score)
// confidence: 0.0 = very uncertain, 1.0 = fully confident

static UNCERTAINTY_PHRASES: &[(&str, f32)] = &[
    ("not sure", 0.25),
    ("i'm unsure", 0.25),
    ("uncertain", 0.25),
    ("not certain", 0.30),
    ("might be wrong", 0.20),
    ("could be wrong", 0.20),
    ("need to check", 0.40),
    ("should verify", 0.40),
    ("need to verify", 0.40),
    ("assuming", 0.50),
    ("i'll assume", 0.50),
    ("i assume", 0.50),
    ("might need review", 0.35),
    ("may need review", 0.35),
    ("not confident", 0.25),
    ("double-check", 0.40),
    ("double check", 0.40),
    ("might break", 0.30),
    ("could break", 0.30),
    ("risky", 0.35),
    ("tricky", 0.40),
    ("maybe", 0.40),
    ("possibly", 0.40),
    ("perhaps", 0.45),
    ("let me verify", 0.45),
    ("let me check", 0.45),
    ("not entirely sure", 0.25),
    ("i'm not sure", 0.25),
    ("unclear", 0.30),
    ("complicated", 0.45),
];

static REJECTION_PHRASES: &[&str] = &[
    "instead of",
    "rather than",
    "decided against",
    "i could also",
    "another option would",
    "alternative would be",
    "we could also",
    "i won't",
    "don't need to",
    "no need to",
    "better not to",
    "avoid",
];

// ── Data types ────────────────────────────────────────────────────────────────

/// A file that the agent read or searched (without necessarily modifying it).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConsultedFile {
    pub path: String,
    /// Which tool(s) were used: "Read", "Grep", "Glob"
    pub tools: Vec<String>,
    /// How many times this path was accessed.
    pub count: usize,
}

/// Which files were examined vs modified in a session.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ExplorationFootprint {
    /// Files the agent read/grepped/globbed — sorted by access count.
    pub consulted: Vec<ConsultedFile>,
    /// Files the agent created or modified.
    pub edited: Vec<String>,
    /// Files consulted but never edited — pure knowledge reads.
    pub implicit_deps: Vec<String>,
    /// Bash commands executed (first 120 chars each).
    pub bash_commands: Vec<String>,
    pub total_tool_calls: usize,
}

/// One file-modification step in the agent's work sequence.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EditStep {
    pub file: String,
    pub operation: String, // "Edit" | "Write"
    pub turn: usize,       // 0-indexed message turn
}

/// Causal chain: user intent → key decisions → code changes.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CausalChain {
    /// The first substantive user message that started the session.
    pub user_trigger: String,
    /// Key decision sentences extracted from thinking blocks.
    pub key_decisions: Vec<String>,
    /// Rejected or deferred alternatives the agent considered.
    pub rejected_approaches: Vec<String>,
    /// Ordered sequence of file edits across the session.
    pub edit_sequence: Vec<EditStep>,
}

/// A moment where the agent expressed uncertainty in its thinking.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UncertaintyAnnotation {
    /// File being edited when this uncertainty was expressed (may be empty).
    pub context_file: String,
    /// Short excerpt from the thinking block containing the phrase.
    pub snippet: String,
    /// The uncertainty phrase that triggered this annotation.
    pub phrase: String,
    /// Estimated confidence at this moment (0 = uncertain, 1 = confident).
    pub confidence: f32,
    pub turn: usize,
}

/// How often a file was read vs edited — a proxy for complexity / fragility.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileChurn {
    pub file: String,
    pub edit_count: usize,
    pub read_count: usize,
    /// edit_count / (edit_count + read_count), 0.0–1.0.
    pub churn_score: f32,
}

/// Full analysis of one Claude Code conversation session.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionAnalysis {
    /// Claude Code session UUID (from the JSONL filename).
    pub session_id: String,
    pub footprint: ExplorationFootprint,
    pub causal_chain: CausalChain,
    pub uncertainty: Vec<UncertaintyAnnotation>,
    pub churn: Vec<FileChurn>,
    /// SHA-256 of the raw JSONL content for replay verification.
    pub replay_hash: String,
    pub analyzed_at: DateTime<Utc>,
    pub message_count: usize,
    pub tool_call_count: usize,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Return the path of the most recently modified JSONL session file for `workdir`.
pub fn find_latest_session(workdir: &Path) -> Option<PathBuf> {
    let home = dirs_home()?;
    let workdir = workdir.to_string_lossy();
    let encoded = workdir.trim_end_matches('/').replace('/', "-");
    let dir = home.join(".claude/projects").join(&encoded);

    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy().to_string();
            s.ends_with(".jsonl") && is_uuid_filename(&s)
        })
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, p)| p)
}

/// Parse a Claude Code JSONL file and extract all session artefacts.
pub fn analyze_session(jsonl_path: &Path) -> Result<SessionAnalysis, H5iError> {
    let raw = fs::read_to_string(jsonl_path)?;

    // Replay hash — SHA-256 of the raw bytes
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let replay_hash = format!("{:x}", hasher.finalize());

    // Session ID from filename stem
    let session_id = jsonl_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Parse every non-empty JSONL line
    let lines: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Mutable state accumulated during the linear scan
    let mut user_trigger = String::new();
    let mut current_editing_file = String::new();
    // file path → (read_count, tool_names_used)
    let mut files_read: HashMap<String, (usize, Vec<String>)> = HashMap::new();
    let mut files_written: HashSet<String> = HashSet::new();
    let mut bash_commands: Vec<String> = Vec::new();
    let mut edit_sequence: Vec<EditStep> = Vec::new();
    // (thinking_text, turn, editing_file_at_that_point)
    let mut thinking_entries: Vec<(String, usize, String)> = Vec::new();
    let mut total_tool_calls = 0usize;
    let mut message_count = 0usize;
    let mut turn = 0usize;

    for line in &lines {
        let msg_type = line.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "user" => {
                message_count += 1;
                turn += 1;
                let content = line
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                if let Some(blocks) = content {
                    for block in blocks {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                let t = text.trim();
                                if !t.is_empty() && user_trigger.is_empty() {
                                    user_trigger = t.to_string();
                                }
                            }
                        }
                        // tool_result blocks are skipped
                    }
                }
            }
            "assistant" => {
                message_count += 1;
                turn += 1;
                let content = line
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                if let Some(blocks) = content {
                    for block in blocks {
                        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match btype {
                            "thinking" => {
                                // Claude Code JSONL redacts thinking content (thinking="").
                                // We record it if non-empty; otherwise fall through to "text".
                                if let Some(text) =
                                    block.get("thinking").and_then(|v| v.as_str())
                                {
                                    if text.len() > 50 {
                                        thinking_entries.push((
                                            text.to_string(),
                                            turn,
                                            current_editing_file.clone(),
                                        ));
                                    }
                                }
                            }
                            "text" => {
                                // Assistant reasoning written in text blocks — rich signal source
                                // when thinking is redacted (the common case in Claude Code JSONL).
                                if let Some(text) =
                                    block.get("text").and_then(|v| v.as_str())
                                {
                                    if text.len() > 80 {
                                        thinking_entries.push((
                                            text.to_string(),
                                            turn,
                                            current_editing_file.clone(),
                                        ));
                                    }
                                }
                            }
                            "tool_use" => {
                                total_tool_calls += 1;
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let input = block.get("input");
                                match name {
                                    "Read" => {
                                        if let Some(p) = input
                                            .and_then(|i| i.get("file_path"))
                                            .and_then(|v| v.as_str())
                                        {
                                            let n = normalize_path(p);
                                            let entry =
                                                files_read.entry(n).or_insert((0, vec![]));
                                            entry.0 += 1;
                                            if !entry.1.contains(&"Read".to_string()) {
                                                entry.1.push("Read".to_string());
                                            }
                                        }
                                    }
                                    "Glob" => {
                                        if let Some(p) = input
                                            .and_then(|i| i.get("path"))
                                            .and_then(|v| v.as_str())
                                        {
                                            let n = normalize_path(p);
                                            let entry =
                                                files_read.entry(n).or_insert((0, vec![]));
                                            entry.0 += 1;
                                            if !entry.1.contains(&"Glob".to_string()) {
                                                entry.1.push("Glob".to_string());
                                            }
                                        }
                                    }
                                    "Grep" => {
                                        if let Some(p) = input
                                            .and_then(|i| i.get("path"))
                                            .and_then(|v| v.as_str())
                                        {
                                            let n = normalize_path(p);
                                            let entry =
                                                files_read.entry(n).or_insert((0, vec![]));
                                            entry.0 += 1;
                                            if !entry.1.contains(&"Grep".to_string()) {
                                                entry.1.push("Grep".to_string());
                                            }
                                        }
                                    }
                                    "Edit" => {
                                        if let Some(p) = input
                                            .and_then(|i| i.get("file_path"))
                                            .and_then(|v| v.as_str())
                                        {
                                            let n = normalize_path(p);
                                            current_editing_file = n.clone();
                                            files_written.insert(n.clone());
                                            edit_sequence.push(EditStep {
                                                file: n,
                                                operation: "Edit".to_string(),
                                                turn,
                                            });
                                        }
                                    }
                                    "Write" => {
                                        if let Some(p) = input
                                            .and_then(|i| i.get("file_path"))
                                            .and_then(|v| v.as_str())
                                        {
                                            let n = normalize_path(p);
                                            current_editing_file = n.clone();
                                            files_written.insert(n.clone());
                                            edit_sequence.push(EditStep {
                                                file: n,
                                                operation: "Write".to_string(),
                                                turn,
                                            });
                                        }
                                    }
                                    "Bash" => {
                                        if let Some(cmd) = input
                                            .and_then(|i| i.get("command"))
                                            .and_then(|v| v.as_str())
                                        {
                                            let snippet: String =
                                                cmd.trim().chars().take(120).collect();
                                            bash_commands.push(snippet);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {} // file-history-snapshot and other metadata lines
        }
    }

    // ── Extract decisions & rejections from thinking blocks ───────────────────

    let mut key_decisions: Vec<String> = Vec::new();
    let mut rejected_approaches: Vec<String> = Vec::new();
    let mut uncertainty: Vec<UncertaintyAnnotation> = Vec::new();

    for (text, t, ctx_file) in &thinking_entries {
        let lower = text.to_lowercase();

        // Key decisions: sentences with first-person planning language
        for sentence in split_sentences(text) {
            let sl = sentence.to_lowercase();
            let is_decision = ["i'll ", "i will ", "let me ", "i should ", "the best approach",
                "i need to ", "i'm going to "]
                .iter()
                .any(|p| sl.contains(p));
            if is_decision && (40..=300).contains(&sentence.len()) {
                key_decisions.push(sentence.trim().to_string());
            }
        }

        // Rejected approaches
        for sentence in split_sentences(text) {
            let sl = sentence.to_lowercase();
            for &phrase in REJECTION_PHRASES {
                if sl.contains(phrase) && sentence.len() > 30 {
                    rejected_approaches.push(sentence.trim().to_string());
                    break;
                }
            }
        }

        // Uncertainty signals
        for &(phrase, confidence) in UNCERTAINTY_PHRASES {
            if lower.contains(phrase) {
                let snippet = extract_snippet(text, phrase, 150);
                uncertainty.push(UncertaintyAnnotation {
                    context_file: ctx_file.clone(),
                    snippet,
                    phrase: phrase.to_string(),
                    confidence,
                    turn: *t,
                });
            }
        }
    }

    // Deduplicate similar decisions and keep top N
    key_decisions = dedup_similar(key_decisions, 0.65);
    key_decisions.truncate(12);
    rejected_approaches = dedup_similar(rejected_approaches, 0.7);
    rejected_approaches.truncate(8);

    // ── Build exploration footprint ───────────────────────────────────────────

    let mut consulted: Vec<ConsultedFile> = files_read
        .iter()
        .map(|(path, (count, tools))| ConsultedFile {
            path: path.clone(),
            tools: tools.clone(),
            count: *count,
        })
        .collect();
    consulted.sort_by(|a, b| b.count.cmp(&a.count));

    let edited_vec: Vec<String> = {
        let mut v: Vec<String> = files_written.iter().cloned().collect();
        v.sort();
        v
    };

    let implicit_deps: Vec<String> = {
        let mut v: Vec<String> = files_read
            .keys()
            .filter(|f| !files_written.contains(*f))
            .cloned()
            .collect();
        v.sort();
        v
    };

    // ── File churn ────────────────────────────────────────────────────────────

    let mut all_files: HashSet<String> = files_written.clone();
    all_files.extend(files_read.keys().cloned());

    let mut churn: Vec<FileChurn> = all_files
        .iter()
        .map(|f| {
            let reads = files_read.get(f).map(|(c, _)| *c).unwrap_or(0);
            let edits = edit_sequence.iter().filter(|s| &s.file == f).count();
            let total = reads + edits;
            let churn_score = if total > 0 { edits as f32 / total as f32 } else { 0.0 };
            FileChurn { file: f.clone(), edit_count: edits, read_count: reads, churn_score }
        })
        .collect();
    churn.sort_by(|a, b| b.edit_count.cmp(&a.edit_count).then(b.read_count.cmp(&a.read_count)));
    churn.retain(|c| c.edit_count > 0 || c.read_count > 1);

    Ok(SessionAnalysis {
        session_id,
        footprint: ExplorationFootprint {
            consulted,
            edited: edited_vec,
            implicit_deps,
            bash_commands,
            total_tool_calls,
        },
        causal_chain: CausalChain {
            user_trigger,
            key_decisions,
            rejected_approaches,
            edit_sequence,
        },
        uncertainty,
        churn,
        replay_hash,
        analyzed_at: Utc::now(),
        message_count,
        tool_call_count: total_tool_calls,
    })
}

/// Save a session analysis linked to a git commit OID.
pub fn save_analysis(
    h5i_root: &Path,
    commit_oid: &str,
    analysis: &SessionAnalysis,
) -> Result<(), H5iError> {
    let dir = h5i_root.join("session_log").join(commit_oid);
    fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(analysis)?;
    fs::write(dir.join("analysis.json"), json)?;
    Ok(())
}

/// Load a saved session analysis for a commit OID prefix or full OID.
pub fn load_analysis(
    h5i_root: &Path,
    commit_oid: &str,
) -> Result<Option<SessionAnalysis>, H5iError> {
    let dir = h5i_root.join("session_log");
    if !dir.exists() {
        return Ok(None);
    }
    // Support short OID prefix matching
    let oid_dir = if dir.join(commit_oid).join("analysis.json").exists() {
        dir.join(commit_oid)
    } else {
        let entries: Vec<_> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(commit_oid)
            })
            .collect();
        if entries.is_empty() {
            return Ok(None);
        }
        entries[0].path()
    };
    let path = oid_dir.join("analysis.json");
    if !path.exists() {
        return Ok(None);
    }
    let json = fs::read_to_string(&path)?;
    let analysis: SessionAnalysis = serde_json::from_str(&json)?;
    Ok(Some(analysis))
}

/// List all commit OIDs that have session analyses stored in h5i_root.
pub fn list_analyses(h5i_root: &Path) -> Vec<String> {
    let dir = h5i_root.join("session_log");
    if !dir.exists() {
        return vec![];
    }
    let mut oids: Vec<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir() && e.path().join("analysis.json").exists())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    oids.sort();
    oids
}

/// Aggregate file churn across all analyzed sessions in h5i_root.
pub fn aggregate_churn(h5i_root: &Path) -> Vec<FileChurn> {
    let oids = list_analyses(h5i_root);
    let mut totals: HashMap<String, (usize, usize)> = HashMap::new(); // file → (edits, reads)
    for oid in &oids {
        if let Ok(Some(analysis)) = load_analysis(h5i_root, oid) {
            for fc in &analysis.churn {
                let entry = totals.entry(fc.file.clone()).or_insert((0, 0));
                entry.0 += fc.edit_count;
                entry.1 += fc.read_count;
            }
        }
    }
    let mut churn: Vec<FileChurn> = totals
        .into_iter()
        .map(|(file, (edits, reads))| {
            let total = edits + reads;
            let churn_score = if total > 0 { edits as f32 / total as f32 } else { 0.0 };
            FileChurn { file, edit_count: edits, read_count: reads, churn_score }
        })
        .collect();
    churn.sort_by(|a, b| b.edit_count.cmp(&a.edit_count));
    churn
}

// ── Terminal display helpers ──────────────────────────────────────────────────

pub fn print_footprint(analysis: &SessionAnalysis) {
    use console::style;
    println!("{}", style("── Exploration Footprint ──────────────────────────────────").dim());
    println!(
        "  Session {}  ·  {} messages  ·  {} tool calls",
        style(&analysis.session_id[..8.min(analysis.session_id.len())]).magenta(),
        style(analysis.message_count).cyan(),
        style(analysis.tool_call_count).cyan(),
    );
    println!();

    println!("{}", style("  Files Consulted:").bold());
    if analysis.footprint.consulted.is_empty() {
        println!("    (none)");
    }
    for f in &analysis.footprint.consulted {
        let tools = f.tools.join(",");
        println!(
            "    {} {} ×{}  {}",
            style("📖").dim(),
            style(&f.path).yellow(),
            style(f.count).dim(),
            style(format!("[{tools}]")).dim(),
        );
    }

    println!();
    println!("{}", style("  Files Edited:").bold());
    if analysis.footprint.edited.is_empty() {
        println!("    (none)");
    }
    for f in &analysis.footprint.edited {
        let count = analysis.causal_chain.edit_sequence.iter().filter(|s| &s.file == f).count();
        println!("    {} {}  ×{} edit(s)", style("✏").green(), style(f).yellow(), count);
    }

    if !analysis.footprint.implicit_deps.is_empty() {
        println!();
        println!("{}", style("  Implicit Dependencies (read but not edited):").bold());
        for f in &analysis.footprint.implicit_deps {
            println!("    {} {}", style("→").dim(), style(f).dim());
        }
    }
}

pub fn print_causal_chain(analysis: &SessionAnalysis) {
    use console::style;
    println!("{}", style("── Causal Chain ────────────────────────────────────────────").dim());
    let trigger: String = analysis.causal_chain.user_trigger.chars().take(200).collect();
    println!("  {}", style("Trigger:").bold());
    println!("    \"{}\"", style(&trigger).italic().cyan());

    if !analysis.causal_chain.key_decisions.is_empty() {
        println!();
        println!("  {}", style("Key Decisions:").bold());
        for (i, d) in analysis.causal_chain.key_decisions.iter().take(8).enumerate() {
            let preview: String = d.chars().take(100).collect();
            println!("    {} {}", style(format!("{}.", i + 1)).dim(), preview);
        }
    }

    if !analysis.causal_chain.rejected_approaches.is_empty() {
        println!();
        println!("  {}", style("Considered / Rejected:").bold());
        for r in analysis.causal_chain.rejected_approaches.iter().take(5) {
            let preview: String = r.chars().take(100).collect();
            println!("    {} {}", style("✗").red().dim(), style(&preview).dim().italic());
        }
    }

    if !analysis.causal_chain.edit_sequence.is_empty() {
        println!();
        println!("  {}", style("Edit Sequence:").bold());
        for (i, step) in analysis.causal_chain.edit_sequence.iter().enumerate() {
            println!(
                "    {} {}  {} t:{}",
                style(format!("{:>2}.", i + 1)).dim(),
                style(&step.file).yellow(),
                style(&step.operation).cyan(),
                style(step.turn).dim(),
            );
        }
    }
}

// ── Heatmap data helpers (pub(crate) for unit testing) ───────────────────────

/// Group uncertainty annotations by file, sorted riskiest (lowest average
/// confidence) first. Returns `(file_path, [confidence_values])` pairs.
pub(crate) fn group_annotations_by_file(
    annotations: &[&UncertaintyAnnotation],
) -> Vec<(String, Vec<f32>)> {
    let mut map: std::collections::HashMap<String, Vec<f32>> =
        std::collections::HashMap::new();
    for ann in annotations {
        map.entry(ann.context_file.clone())
            .or_default()
            .push(ann.confidence);
    }
    let mut list: Vec<(String, Vec<f32>)> = map.into_iter().collect();
    list.sort_by(|a, b| {
        let avg = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        avg(&a.1).partial_cmp(&avg(&b.1)).unwrap()
    });
    list
}

/// Build a sparkline of `width` cells. Each cell holds the minimum (riskiest)
/// confidence among all annotations whose turn maps to that position.
/// Returns `None` for empty cells and `Some(confidence)` for occupied ones.
pub(crate) fn build_timeline_cells(
    annotations: &[&UncertaintyAnnotation],
    width: usize,
) -> Vec<Option<f32>> {
    if annotations.is_empty() || width == 0 {
        return vec![None; width];
    }
    let min_t = annotations.iter().map(|a| a.turn).min().unwrap_or(0);
    let max_t = annotations.iter().map(|a| a.turn).max().unwrap_or(1);
    let t_range = (max_t - min_t).max(1) as f64;
    let mut cells: Vec<Option<f32>> = vec![None; width];
    for ann in annotations {
        let pos =
            (((ann.turn - min_t) as f64 / t_range) * (width - 1) as f64).round() as usize;
        let pos = pos.min(width - 1);
        cells[pos] = Some(match cells[pos] {
            None => ann.confidence,
            Some(prev) => prev.min(ann.confidence),
        });
    }
    cells
}

/// Build a non-overlapping pointer-label string from sorted `(column, label)` pairs.
/// Labels that would overlap a preceding label are silently skipped.
pub(crate) fn build_pointer_string(positions: &[(usize, String)]) -> String {
    let mut buf = String::new();
    let mut cursor = 0usize;
    for (pos, label) in positions {
        if *pos >= cursor {
            buf.push_str(&" ".repeat(pos - cursor));
            buf.push_str(label);
            cursor = pos + label.len();
        }
    }
    buf
}

pub fn print_uncertainty(analysis: &SessionAnalysis, file_filter: Option<&str>) {
    use console::style;

    let annotations: Vec<&UncertaintyAnnotation> = analysis
        .uncertainty
        .iter()
        .filter(|a| {
            file_filter
                .map(|f| a.context_file.contains(f))
                .unwrap_or(true)
        })
        .collect();

    let session_short = &analysis.session_id[..8.min(analysis.session_id.len())];
    let unique_files = annotations
        .iter()
        .map(|a| a.context_file.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();

    // ── Header ────────────────────────────────────────────────────────────────
    println!(
        "{}",
        style("── Uncertainty Heatmap ─────────────────────────────────────────────").dim()
    );

    if annotations.is_empty() {
        println!("  {} No uncertainty signals detected.", style("✔").green());
        return;
    }

    let n = annotations.len();
    println!(
        "  {}  ·  session {}  ·  {} file{}",
        style(format!("{n} signal{}", if n == 1 { "" } else { "s" })).bold(),
        style(session_short).magenta(),
        unique_files,
        if unique_files == 1 { "" } else { "s" },
    );
    println!();

    // ── Risk Map ──────────────────────────────────────────────────────────────
    let file_list = group_annotations_by_file(&annotations);

    const NAME_W: usize = 44;
    const BAR_W: usize = 16;

    println!("  {}", style("Risk Map").bold());
    println!("  {}", style("─".repeat(74)).dim());
    for (file, confs) in &file_list {
        let count = confs.len();
        let avg_conf = confs.iter().sum::<f32>() / count as f32;
        let risk = 1.0_f32 - avg_conf;

        // Heat bar: filled portion = risk fraction of BAR_W
        let filled = (risk * BAR_W as f32).round() as usize;
        let filled = filled.min(BAR_W);
        let empty = BAR_W - filled;
        let bar_filled = if avg_conf < 0.35 {
            style("█".repeat(filled)).red().bold().to_string()
        } else if avg_conf < 0.55 {
            style("█".repeat(filled)).yellow().to_string()
        } else {
            style("█".repeat(filled)).cyan().to_string()
        };
        let bar = format!("{}{}", bar_filled, style("░".repeat(empty)).dim());

        // One bullet per signal (capped at 6 to stay on one line)
        let bullets: String = confs
            .iter()
            .take(6)
            .map(|&c| {
                if c < 0.35 {
                    style("●").red().to_string()
                } else if c < 0.55 {
                    style("●").yellow().to_string()
                } else {
                    style("●").cyan().to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("");
        let extra = if count > 6 {
            style(format!("+{}", count - 6)).dim().to_string()
        } else {
            String::new()
        };

        let conf_pct = (avg_conf * 100.0).round() as u32;
        let conf_styled = if avg_conf < 0.35 {
            style(format!("{conf_pct:>3}%")).red().bold().to_string()
        } else if avg_conf < 0.55 {
            style(format!("{conf_pct:>3}%")).yellow().bold().to_string()
        } else {
            style(format!("{conf_pct:>3}%")).cyan().bold().to_string()
        };

        println!(
            "  {:<name_w$}  {}  {}{}  {:<2} signal{}  avg {}",
            style(shorten_path(file, NAME_W)).yellow(),
            bar,
            bullets,
            extra,
            count,
            if count == 1 { " " } else { "s" },
            conf_styled,
            name_w = NAME_W,
        );
    }
    println!();

    // ── Timeline ──────────────────────────────────────────────────────────────
    // Sparkline: each cell is one character; mark signals with colored blocks.
    let min_t = annotations.iter().map(|a| a.turn).min().unwrap_or(0);
    let max_t = annotations.iter().map(|a| a.turn).max().unwrap_or(1);
    let t_range = (max_t - min_t).max(1) as f64;
    const TL_W: usize = 68;

    println!("  {}", style("Timeline").bold());
    // header: "t:N ─── … ─── t:N"
    let lbl_l = format!("t:{min_t}");
    let lbl_r = format!("t:{max_t}");
    let dashes = TL_W.saturating_sub(lbl_l.len() + lbl_r.len() + 2);
    println!(
        "  {} {} {}",
        style(&lbl_l).dim(),
        style("─".repeat(dashes)).dim(),
        style(&lbl_r).dim()
    );

    // Build the sparkline: lowest confidence (most risky) wins each cell
    let cells: Vec<Option<f32>> = build_timeline_cells(&annotations, TL_W);
    let sparkline: String = cells
        .iter()
        .map(|c| match *c {
            None => style("·".to_string()).dim().to_string(),
            Some(c) if c < 0.35 => style("█".to_string()).red().bold().to_string(),
            Some(c) if c < 0.55 => style("▓".to_string()).yellow().to_string(),
            _ => style("░".to_string()).cyan().to_string(),
        })
        .collect();
    println!("  {}", sparkline);

    // Pointer row: ↑t:N labels under the top-4 riskiest signals
    let mut top_signals: Vec<&UncertaintyAnnotation> = annotations.clone();
    top_signals.sort_by(|a, b| a.confidence.partial_cmp(&b.confidence).unwrap());
    top_signals.dedup_by_key(|a| a.turn);
    top_signals.truncate(4);
    top_signals.sort_by_key(|a| a.turn);

    let ptr_positions: Vec<(usize, String)> = top_signals
        .iter()
        .map(|ann| {
            let pos = (((ann.turn - min_t) as f64 / t_range) * (TL_W - 1) as f64).round()
                as usize;
            (pos.min(TL_W - 1), format!("↑t:{}", ann.turn))
        })
        .collect();
    let ptr_buf = build_pointer_string(&ptr_positions);
    if !ptr_buf.trim().is_empty() {
        println!("  {}", style(ptr_buf.trim_end()).dim());
    }
    println!();

    // ── Individual Signals ────────────────────────────────────────────────────
    println!("  {}", style("Signals").bold());
    println!("  {}", style("─".repeat(74)).dim());
    for ann in &annotations {
        let conf_pct = (ann.confidence * 100.0).round() as u32;
        let (badge, conf_styled) = if ann.confidence < 0.35 {
            (
                style("██").red().bold().to_string(),
                style(format!("{conf_pct:>3}%")).red().bold().to_string(),
            )
        } else if ann.confidence < 0.55 {
            (
                style("▓▓").yellow().to_string(),
                style(format!("{conf_pct:>3}%")).yellow().bold().to_string(),
            )
        } else {
            (
                style("░░").cyan().to_string(),
                style(format!("{conf_pct:>3}%")).cyan().bold().to_string(),
            )
        };

        let ctx = if ann.context_file.is_empty() {
            style("(no file)".to_string()).dim().to_string()
        } else {
            style(shorten_path(&ann.context_file, 34)).dim().to_string()
        };

        println!(
            "  {}  t:{:<4}  {:<18}  {}  [{}]",
            badge,
            style(ann.turn).dim(),
            style(&ann.phrase).bold(),
            ctx,
            conf_styled,
        );
        println!(
            "       {}",
            style(format!("\"{}\"", ann.snippet)).dim().italic()
        );
        println!();
    }

    // ── Legend ────────────────────────────────────────────────────────────────
    println!(
        "  {} high risk (<35%)   {} moderate (35–55%)   {} low (>55%)",
        style("██").red().bold(),
        style("▓▓").yellow(),
        style("░░").cyan(),
    );
}

pub fn print_churn(churn: &[FileChurn]) {
    use console::style;
    println!("{}", style("── File Churn ──────────────────────────────────────────────").dim());
    if churn.is_empty() {
        println!("  No churn data yet. Run `h5i analyze` after sessions.");
        return;
    }
    println!(
        "  {:<46} {:>5} {:>5}  {}",
        style("file").bold(),
        style("edits").bold(),
        style("reads").bold(),
        style("churn").bold(),
    );
    println!("  {}", style("─".repeat(68)).dim());
    for fc in churn.iter().take(20) {
        let filled = (fc.churn_score * 10.0).round() as usize;
        let bar: String = "█".repeat(filled) + &"░".repeat(10 - filled);
        let short = shorten_path(&fc.file, 44);
        println!(
            "  {:<46} {:>5} {:>5}  {} {:.0}%",
            style(&short).yellow(),
            style(fc.edit_count).cyan(),
            style(fc.read_count).dim(),
            style(&bar).dim(),
            fc.churn_score * 100.0,
        );
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn is_uuid_filename(s: &str) -> bool {
    let s = s.trim_end_matches(".jsonl");
    if s.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn normalize_path(p: &str) -> String {
    if let Some(home) = dirs_home() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = p.strip_prefix(home_str.as_ref()) {
            return format!("~{}", rest);
        }
    }
    p.to_string()
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if (ch == '.' || ch == '!' || ch == '?') && current.len() > 20 {
            let s = current.trim().to_string();
            if !s.is_empty() {
                sentences.push(s);
            }
            current.clear();
        }
    }
    let s = current.trim().to_string();
    if s.len() > 20 {
        sentences.push(s);
    }
    sentences
}

fn extract_snippet(text: &str, phrase: &str, max_len: usize) -> String {
    let lower = text.to_lowercase();
    let pos = lower.find(phrase).unwrap_or(0);
    let start = pos.saturating_sub(60);
    let end = (pos + phrase.len() + 90).min(text.len());
    // Ensure we don't split on non-char-boundary
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .filter(|&i| i <= start)
        .last()
        .unwrap_or(0);
    let end = text
        .char_indices()
        .map(|(i, _)| i)
        .filter(|&i| i <= end)
        .last()
        .unwrap_or(text.len());
    let snippet = &text[start..end];
    let clean: String = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.len() > max_len {
        format!("{}…", &clean[..max_len])
    } else {
        clean
    }
}

fn jaccard_similarity(a: &str, b: &str) -> f32 {
    let wa: HashSet<&str> = a.split_whitespace().collect();
    let wb: HashSet<&str> = b.split_whitespace().collect();
    let intersection = wa.intersection(&wb).count();
    let union = wa.union(&wb).count();
    if union == 0 { 1.0 } else { intersection as f32 / union as f32 }
}

fn dedup_similar(mut items: Vec<String>, threshold: f32) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for item in items.drain(..) {
        if result
            .iter()
            .all(|existing| jaccard_similarity(existing, &item) < threshold)
        {
            result.push(item);
        }
    }
    result
}

fn shorten_path(p: &str, max: usize) -> String {
    if p.len() <= max {
        p.to_string()
    } else {
        format!("…{}", &p[p.len().saturating_sub(max - 1)..])
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    fn make_ann(file: &str, phrase: &str, confidence: f32, turn: usize) -> UncertaintyAnnotation {
        UncertaintyAnnotation {
            context_file: file.to_string(),
            snippet: format!("surrounding {} context here", phrase),
            phrase: phrase.to_string(),
            confidence,
            turn,
        }
    }

    /// Write a JSONL string to a unique temp file whose name passes `is_uuid_filename`.
    /// Uses an atomic counter so parallel tests never share the same path.
    fn write_temp_jsonl(content: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CTR: AtomicUsize = AtomicUsize::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("{:08x}-0000-0000-0000-000000000000.jsonl", n));
        std::fs::write(&path, content).unwrap();
        path
    }

    fn join_lines(lines: &[&str]) -> String {
        lines.join("\n")
    }

    // Minimal JSONL helpers — produce valid JSON the parser understands.
    fn user_msg(text: &str) -> String {
        let esc = text.replace('\\', "\\\\").replace('"', "\\\"");
        format!(r#"{{"type":"user","message":{{"content":[{{"type":"text","text":"{esc}"}}]}}}}"#)
    }

    fn assistant_text(text: &str) -> String {
        let esc = text.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{esc}"}}]}}}}"#
        )
    }

    fn assistant_edit(file: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Edit","input":{{"file_path":"{file}","old_string":"x","new_string":"y"}}}}]}}}}"#
        )
    }

    fn assistant_read(file: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Read","input":{{"file_path":"{file}"}}}}]}}}}"#
        )
    }

    fn assistant_bash(cmd: &str) -> String {
        let esc = cmd.replace('"', "\\\"");
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"{esc}"}}}}]}}}}"#
        )
    }

    // ── split_sentences ───────────────────────────────────────────────────────

    #[test]
    fn test_split_sentences_three_sentences() {
        // Each sentence (after trim) must be > 20 chars to survive the length guard.
        let text = "This is one complete sentence. And this is another complete one! Is this really a long question?";
        let sentences = split_sentences(text);
        assert_eq!(sentences.len(), 3, "sentences: {sentences:?}");
        assert!(sentences[0].contains("complete sentence"));
        assert!(sentences[1].contains("another"));
        assert!(sentences[2].contains("question"));
    }

    #[test]
    fn test_split_sentences_short_fragments_dropped() {
        // "OK." is only 3 chars — below the 20-char threshold
        let text = "OK. This is a proper sentence that should survive.";
        let sentences = split_sentences(text);
        assert!(sentences.iter().any(|s| s.contains("proper sentence")));
        assert!(!sentences.iter().any(|s| s.trim() == "OK."));
    }

    #[test]
    fn test_split_sentences_trailing_fragment_kept_if_long_enough() {
        // A fragment without terminal punctuation should appear if > 20 chars
        let text = "I'll implement the new handler for this endpoint";
        let sentences = split_sentences(text);
        assert_eq!(sentences.len(), 1);
        assert!(sentences[0].contains("implement"));
    }

    // ── extract_snippet ───────────────────────────────────────────────────────

    #[test]
    fn test_extract_snippet_contains_phrase() {
        let text = "The code was fine. I'm not sure if the change will break things.";
        let snippet = extract_snippet(text, "not sure", 200);
        assert!(snippet.contains("not sure"));
    }

    #[test]
    fn test_extract_snippet_respects_max_len() {
        let long = "a".repeat(200) + " not sure " + &"b".repeat(200);
        let snippet = extract_snippet(&long, "not sure", 50);
        // The snippet is at most max_len ASCII chars + the "…" ellipsis (3 UTF-8 bytes).
        // Check char count (visual width) rather than byte length.
        assert!(
            snippet.chars().count() <= 52,
            "snippet too long: {} chars — '{snippet}'",
            snippet.chars().count()
        );
    }

    #[test]
    fn test_extract_snippet_phrase_at_start() {
        let text = "not sure what to do here because the code is complex";
        let snippet = extract_snippet(text, "not sure", 200);
        assert!(snippet.contains("not sure"));
    }

    // ── jaccard_similarity ────────────────────────────────────────────────────

    #[test]
    fn test_jaccard_identical_strings() {
        assert_eq!(jaccard_similarity("hello world", "hello world"), 1.0);
    }

    #[test]
    fn test_jaccard_completely_disjoint() {
        assert_eq!(jaccard_similarity("foo bar", "baz qux"), 0.0);
    }

    #[test]
    fn test_jaccard_partial_overlap() {
        // intersection = {foo, bar} = 2 / union = {foo, bar, baz, qux} = 4 → 0.5
        let sim = jaccard_similarity("foo bar baz", "foo bar qux");
        assert!((sim - 0.5).abs() < 1e-5, "expected 0.5, got {sim}");
    }

    #[test]
    fn test_jaccard_empty_strings() {
        // Both empty → union = 0 → returns 1.0 (identical)
        assert_eq!(jaccard_similarity("", ""), 1.0);
    }

    // ── dedup_similar ─────────────────────────────────────────────────────────

    #[test]
    fn test_dedup_removes_exact_duplicates() {
        let items = vec![
            "I'll implement this in the auth module".to_string(),
            "I'll implement this in the auth module".to_string(),
            "I'll build something completely different here".to_string(),
        ];
        let result = dedup_similar(items, 0.65);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_dedup_keeps_distinct_items() {
        let items = vec![
            "I'll write the token refresh logic now".to_string(),
            "Let me refactor the session store code".to_string(),
            "The best approach is to use Redis for caching".to_string(),
        ];
        let result = dedup_similar(items, 0.65);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_dedup_empty_input() {
        let result = dedup_similar(vec![], 0.65);
        assert!(result.is_empty());
    }

    // ── shorten_path ──────────────────────────────────────────────────────────

    #[test]
    fn test_shorten_path_fits() {
        let p = "src/auth.rs";
        assert_eq!(shorten_path(p, 44), "src/auth.rs");
    }

    #[test]
    fn test_shorten_path_truncates_with_ellipsis() {
        let p = "a/very/deeply/nested/path/that/exceeds/the/max/width/src/auth.rs";
        let result = shorten_path(p, 20);
        // "…" is 3 UTF-8 bytes; compare char count (visual width) not byte length.
        assert!(
            result.chars().count() <= 20,
            "result too long: {} chars — '{result}'",
            result.chars().count()
        );
        assert!(result.starts_with('…'), "expected leading ellipsis in '{result}'");
        assert!(result.ends_with("auth.rs"));
    }

    #[test]
    fn test_shorten_path_exact_length() {
        let p = "src/auth.rs"; // 11 chars
        assert_eq!(shorten_path(p, 11), "src/auth.rs");
    }

    // ── group_annotations_by_file ─────────────────────────────────────────────

    #[test]
    fn test_group_riskiest_file_first() {
        let a1 = make_ann("src/auth.rs", "not sure", 0.25, 10);
        let a2 = make_ann("src/main.rs", "perhaps", 0.45, 20);
        let a3 = make_ann("src/auth.rs", "might break", 0.30, 30);
        let refs = [&a1, &a2, &a3];
        let groups = group_annotations_by_file(&refs);
        // auth.rs avg = (0.25+0.30)/2 = 0.275 < main.rs avg = 0.45 → auth first
        assert_eq!(groups[0].0, "src/auth.rs");
        assert_eq!(groups[1].0, "src/main.rs");
    }

    #[test]
    fn test_group_confidence_values_collected() {
        let a1 = make_ann("src/auth.rs", "not sure", 0.25, 10);
        let a2 = make_ann("src/auth.rs", "unclear", 0.30, 20);
        let refs = [&a1, &a2];
        let groups = group_annotations_by_file(&refs);
        assert_eq!(groups.len(), 1);
        let confs = &groups[0].1;
        assert_eq!(confs.len(), 2);
        assert!(confs.contains(&0.25));
        assert!(confs.contains(&0.30));
    }

    #[test]
    fn test_group_single_annotation() {
        let a = make_ann("src/lib.rs", "unclear", 0.30, 5);
        let refs = [&a];
        let groups = group_annotations_by_file(&refs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "src/lib.rs");
    }

    #[test]
    fn test_group_empty_input() {
        let groups = group_annotations_by_file(&[]);
        assert!(groups.is_empty());
    }

    // ── build_timeline_cells ──────────────────────────────────────────────────

    #[test]
    fn test_timeline_empty_annotations() {
        let cells = build_timeline_cells(&[], 10);
        assert_eq!(cells.len(), 10);
        assert!(cells.iter().all(|c| c.is_none()));
    }

    #[test]
    fn test_timeline_zero_width() {
        let a = make_ann("src/auth.rs", "not sure", 0.25, 10);
        let cells = build_timeline_cells(&[&a], 0);
        assert!(cells.is_empty());
    }

    #[test]
    fn test_timeline_single_annotation_lands_at_start() {
        // When min_t == max_t the t_range becomes 1; pos = (0/1)*9 = 0
        let a = make_ann("src/auth.rs", "not sure", 0.25, 50);
        let cells = build_timeline_cells(&[&a], 10);
        assert_eq!(cells[0], Some(0.25));
        assert!(cells[1..].iter().all(|c| c.is_none()));
    }

    #[test]
    fn test_timeline_two_annotations_at_endpoints() {
        let a1 = make_ann("src/a.rs", "not sure", 0.25, 0);
        let a2 = make_ann("src/b.rs", "perhaps", 0.45, 100);
        let cells = build_timeline_cells(&[&a1, &a2], 11);
        assert_eq!(cells[0], Some(0.25));
        assert_eq!(cells[10], Some(0.45));
        assert!(cells[1..10].iter().all(|c| c.is_none()));
    }

    #[test]
    fn test_timeline_riskiest_wins_on_collision() {
        // Two annotations at the same turn: lower confidence must survive
        let a1 = make_ann("src/a.rs", "not sure", 0.20, 50);
        let a2 = make_ann("src/b.rs", "perhaps", 0.50, 50);
        let cells = build_timeline_cells(&[&a1, &a2], 10);
        let occupied: Vec<_> = cells.iter().filter(|c| c.is_some()).collect();
        assert_eq!(occupied.len(), 1);
        assert_eq!(*occupied[0], Some(0.20));
    }

    #[test]
    fn test_timeline_middle_annotation() {
        // annotation at turn 50 out of 0–100 range → position 5 in a 11-cell grid
        let a1 = make_ann("src/a.rs", "not sure", 0.30, 0);
        let a2 = make_ann("src/b.rs", "unclear", 0.30, 50);
        let a3 = make_ann("src/c.rs", "perhaps", 0.30, 100);
        let cells = build_timeline_cells(&[&a1, &a2, &a3], 11);
        assert!(cells[5].is_some(), "middle turn should map to cell 5");
    }

    // ── build_pointer_string ──────────────────────────────────────────────────

    #[test]
    fn test_pointer_string_two_non_overlapping() {
        let positions = vec![
            (0, "↑t:0".to_string()),
            (20, "↑t:100".to_string()),
        ];
        let result = build_pointer_string(&positions);
        assert!(result.starts_with("↑t:0"), "got: {result:?}");
        assert!(result.contains("↑t:100"), "got: {result:?}");
    }

    #[test]
    fn test_pointer_string_overlap_skips_second() {
        // "↑t:1" is 4 chars wide (starts at 0, ends at 3); second label at col 2
        // is inside the first → should be skipped
        let positions = vec![
            (0, "↑t:1".to_string()),
            (2, "↑t:2".to_string()),
        ];
        let result = build_pointer_string(&positions);
        assert!(result.starts_with("↑t:1"), "first label missing: {result:?}");
        assert!(!result.contains("↑t:2"), "overlapping label should be skipped: {result:?}");
    }

    #[test]
    fn test_pointer_string_empty() {
        assert_eq!(build_pointer_string(&[]), "");
    }

    #[test]
    fn test_pointer_string_single() {
        let positions = vec![(5, "↑t:42".to_string())];
        let result = build_pointer_string(&positions);
        assert!(result.starts_with("     ↑t:42"), "got: {result:?}");
    }

    // ── analyze_session — JSONL parsing ───────────────────────────────────────

    #[test]
    fn test_analyze_session_user_trigger() {
        let jsonl = join_lines(&[
            &user_msg("Add OAuth2 login with GitHub"),
            &assistant_text("I'll start by reading the auth module."),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert_eq!(
            analysis.causal_chain.user_trigger,
            "Add OAuth2 login with GitHub"
        );
    }

    #[test]
    fn test_analyze_session_detects_uncertainty_phrase() {
        let jsonl = join_lines(&[
            &user_msg("refactor the auth module"),
            &assistant_text(
                "I'll refactor this module carefully. I'm not sure if the change \
                 will break the token validation logic — let me check the tests first.",
            ),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        let phrases: Vec<&str> =
            analysis.uncertainty.iter().map(|a| a.phrase.as_str()).collect();
        assert!(
            phrases.iter().any(|&p| p == "not sure" || p == "let me check" || p == "i'm not sure"),
            "expected uncertainty signal, got phrases: {phrases:?}"
        );
    }

    #[test]
    fn test_analyze_session_no_uncertainty_in_confident_text() {
        let jsonl = join_lines(&[
            &user_msg("add a constant"),
            &assistant_text(
                "I'll add the MAX_RETRIES constant with value 3 to the config module. \
                 The value is clearly documented in the existing code.",
            ),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert!(
            analysis.uncertainty.is_empty(),
            "unexpected signals: {:?}",
            analysis.uncertainty.iter().map(|a| &a.phrase).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_analyze_session_footprint_edited_files() {
        let jsonl = join_lines(&[
            &user_msg("add rate limiting"),
            &assistant_edit("/home/user/src/auth.rs"),
            &assistant_edit("/home/user/src/main.rs"),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert_eq!(
            analysis.footprint.edited.len(),
            2,
            "edited: {:?}",
            analysis.footprint.edited
        );
    }

    #[test]
    fn test_analyze_session_implicit_deps() {
        // config.rs read-only → implicit dep; auth.rs edited → not an implicit dep
        let jsonl = join_lines(&[
            &user_msg("add rate limiting"),
            &assistant_read("/home/user/src/config.rs"),
            &assistant_edit("/home/user/src/auth.rs"),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert!(
            analysis.footprint.implicit_deps.iter().any(|p| p.contains("config.rs")),
            "config.rs should be an implicit dep: {:?}",
            analysis.footprint.implicit_deps
        );
        assert!(
            !analysis.footprint.implicit_deps.iter().any(|p| p.contains("auth.rs")),
            "auth.rs was edited, should NOT be implicit dep"
        );
    }

    #[test]
    fn test_analyze_session_tool_call_count() {
        let jsonl = join_lines(&[
            &user_msg("fix the bug"),
            &assistant_read("/home/user/src/auth.rs"),
            &assistant_read("/home/user/src/auth.rs"),
            &assistant_edit("/home/user/src/auth.rs"),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert_eq!(analysis.tool_call_count, 3);
    }

    #[test]
    fn test_analyze_session_replay_hash_is_stable() {
        let jsonl = join_lines(&[&user_msg("do something")]);
        let path = write_temp_jsonl(&jsonl);
        let h1 = analyze_session(&path).unwrap().replay_hash;
        let h2 = analyze_session(&path).unwrap().replay_hash;
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_analyze_session_replay_hash_differs_for_different_content() {
        let p1 = write_temp_jsonl(&join_lines(&[&user_msg("task A")]));
        let p2 = write_temp_jsonl(&join_lines(&[&user_msg("task B --- unique")]));
        let h1 = analyze_session(&p1).unwrap().replay_hash;
        let h2 = analyze_session(&p2).unwrap().replay_hash;
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_analyze_session_churn_score() {
        // 2 edits + 1 read → churn = 2/(2+1) ≈ 0.667
        let file = "/home/user/src/auth.rs";
        let jsonl = join_lines(&[
            &user_msg("fix auth"),
            &assistant_read(file),
            &assistant_edit(file),
            &assistant_edit(file),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        let fc = analysis
            .churn
            .iter()
            .find(|c| c.file.contains("auth.rs"))
            .expect("auth.rs should have a churn entry");
        assert_eq!(fc.edit_count, 2);
        assert_eq!(fc.read_count, 1);
        assert!(
            (fc.churn_score - 2.0 / 3.0).abs() < 1e-4,
            "expected churn ≈ 0.667, got {}",
            fc.churn_score
        );
    }

    #[test]
    fn test_analyze_session_key_decisions_extracted() {
        let jsonl = join_lines(&[
            &user_msg("refactor the module"),
            &assistant_text(
                "I'll start by reading the existing code. \
                 I will then restructure the module into smaller functions. \
                 Let me also add proper error handling throughout the codebase.",
            ),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert!(
            !analysis.causal_chain.key_decisions.is_empty(),
            "expected key decisions to be extracted"
        );
    }

    #[test]
    fn test_analyze_session_bash_commands_captured() {
        let jsonl = join_lines(&[
            &user_msg("run tests"),
            &assistant_bash("cargo test --verbose"),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert!(
            analysis.footprint.bash_commands.iter().any(|c| c.contains("cargo test")),
            "bash commands: {:?}",
            analysis.footprint.bash_commands
        );
    }

    #[test]
    fn test_analyze_session_message_count() {
        let jsonl = join_lines(&[
            &user_msg("first message"),
            &assistant_text("I'll handle this for you now."),
            &user_msg("second message"),
            &assistant_text("I'll also handle this one here."),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        assert_eq!(analysis.message_count, 4);
    }

    #[test]
    fn test_analyze_session_consulted_file_count() {
        let file = "/home/user/src/auth.rs";
        // Read the same file 3 times → count should be 3
        let jsonl = join_lines(&[
            &user_msg("investigate auth"),
            &assistant_read(file),
            &assistant_read(file),
            &assistant_read(file),
        ]);
        let path = write_temp_jsonl(&jsonl);
        let analysis = analyze_session(&path).unwrap();
        let entry = analysis
            .footprint
            .consulted
            .iter()
            .find(|c| c.path.contains("auth.rs"))
            .expect("auth.rs should appear in consulted");
        assert_eq!(entry.count, 3);
    }
}
