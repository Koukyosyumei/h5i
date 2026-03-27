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
    let encoded = workdir.to_string_lossy().replace('/', "-");
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
    // Group annotations by file, collect confidence values
    let mut file_map: std::collections::HashMap<&str, Vec<f32>> =
        std::collections::HashMap::new();
    for ann in &annotations {
        file_map
            .entry(ann.context_file.as_str())
            .or_default()
            .push(ann.confidence);
    }
    // Sort riskiest (lowest avg confidence) first
    let mut file_list: Vec<(&str, Vec<f32>)> = file_map.into_iter().collect();
    file_list.sort_by(|a, b| {
        let avg = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        avg(&a.1).partial_cmp(&avg(&b.1)).unwrap()
    });

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
    let mut cells: Vec<Option<f32>> = vec![None; TL_W];
    for ann in &annotations {
        let pos = (((ann.turn - min_t) as f64 / t_range) * (TL_W - 1) as f64).round() as usize;
        let pos = pos.min(TL_W - 1);
        cells[pos] = Some(match cells[pos] {
            None => ann.confidence,
            Some(prev) => prev.min(ann.confidence),
        });
    }
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
    // Remove overlaps left-to-right
    let mut ptr_buf = String::new();
    let mut cursor = 0usize;
    for (pos, label) in &ptr_positions {
        if *pos >= cursor {
            ptr_buf.push_str(&" ".repeat(pos - cursor));
            ptr_buf.push_str(label);
            cursor = pos + label.len();
        }
    }
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
