/// Structural AST diffing for h5i.
///
/// The flow is:
///   1. An external parser (e.g. `script/h5i-py-parser.py`) converts a source
///      file into a compact s-expression string.
///   2. `parse_named_blocks` tokenises and parses that string into a tree, then
///      extracts top-level named declarations (functions, classes, …).
///   3. `SemanticAst::diff` matches declarations across two versions by
///      identifier, producing `Added`, `Deleted`, `Modified`, `Moved`, or
///      `Unchanged` changes.
///   4. `AstDiff::print_stylish` renders the result to the terminal.
use console::style;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

// ── S-expression tokenizer ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum SexpToken {
    Open,
    Close,
    Atom(String),
}

fn tokenize(s: &str) -> Vec<SexpToken> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&ch) = chars.peek() {
        match ch {
            '(' => {
                chars.next();
                tokens.push(SexpToken::Open);
            }
            ')' => {
                chars.next();
                tokens.push(SexpToken::Close);
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => {
                let mut atom = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '(' || c == ')' || c.is_whitespace() {
                        break;
                    }
                    atom.push(c);
                    chars.next();
                }
                if !atom.is_empty() {
                    tokens.push(SexpToken::Atom(atom));
                }
            }
        }
    }
    tokens
}

// ── S-expression parse tree ───────────────────────────────────────────────────

/// A node in the parsed s-expression tree.
#[derive(Debug, Clone, PartialEq)]
pub enum SexpNode {
    Atom(String),
    List(Vec<SexpNode>),
}

impl SexpNode {
    /// The leading atom of a list — the "type name" (e.g. `"FunctionDef"`).
    pub fn type_name(&self) -> Option<&str> {
        match self {
            SexpNode::List(ch) => match ch.first() {
                Some(SexpNode::Atom(s)) => Some(s.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// First child list whose leading atom equals `name`.
    /// E.g. on `(FunctionDef (name 'foo') …)`, `.field("name")` returns
    /// `Some(List([Atom("name"), Atom("'foo'")]))`.
    pub fn field(&self, name: &str) -> Option<&SexpNode> {
        match self {
            SexpNode::List(ch) => ch.iter().find(|c| c.type_name() == Some(name)),
            _ => None,
        }
    }

    /// The single atom value of a simple field.
    /// `(name 'foo')` → `Some("foo")` (quotes stripped).
    pub fn field_atom(&self, name: &str) -> Option<String> {
        match self.field(name)? {
            SexpNode::List(ch) if ch.len() >= 2 => match &ch[1] {
                SexpNode::Atom(val) => {
                    Some(val.trim_matches('\'').trim_matches('"').to_string())
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// All non-first children of a named field — the actual items after the
    /// field-name atom.
    /// `(body (FunctionDef …) (ClassDef …))` → `[FunctionDef node, ClassDef node]`
    pub fn field_children(&self, name: &str) -> Vec<&SexpNode> {
        match self.field(name) {
            Some(SexpNode::List(ch)) if ch.len() > 1 => ch[1..].iter().collect(),
            _ => vec![],
        }
    }

    /// Canonical compact serialisation — no extra whitespace.
    /// Used for hashing and content comparison.
    pub fn serialize(&self) -> String {
        match self {
            SexpNode::Atom(s) => s.clone(),
            SexpNode::List(ch) => {
                let parts: Vec<String> = ch.iter().map(|c| c.serialize()).collect();
                format!("({})", parts.join(" "))
            }
        }
    }
}

fn parse_node(tokens: &[SexpToken], pos: &mut usize) -> Option<SexpNode> {
    if *pos >= tokens.len() {
        return None;
    }
    match &tokens[*pos] {
        SexpToken::Open => {
            *pos += 1;
            let mut children = Vec::new();
            while *pos < tokens.len() {
                match &tokens[*pos] {
                    SexpToken::Close => {
                        *pos += 1;
                        break;
                    }
                    _ => match parse_node(tokens, pos) {
                        Some(child) => children.push(child),
                        None => break,
                    },
                }
            }
            Some(SexpNode::List(children))
        }
        SexpToken::Atom(a) => {
            let atom = a.clone();
            *pos += 1;
            Some(SexpNode::Atom(atom))
        }
        SexpToken::Close => None,
    }
}

/// Parses the first complete s-expression in `s`.
pub fn parse_sexp(s: &str) -> Option<SexpNode> {
    let tokens = tokenize(s);
    let mut pos = 0;
    parse_node(&tokens, &mut pos)
}

// ── Named top-level block ─────────────────────────────────────────────────────

/// A top-level declaration extracted from an AST (function, class, import, …).
#[derive(Debug, Clone)]
pub struct NamedBlock {
    /// AST node type  (e.g. `"FunctionDef"`, `"ClassDef"`).
    pub kind: String,
    /// Identifier extracted from the node, if the kind supports one.
    pub name: Option<String>,
    /// Canonical s-expression used for hashing and content comparison.
    pub sexp: String,
}

/// Declaration kinds that carry an identifier we can use for matching.
const NAMED_KINDS: &[&str] = &["FunctionDef", "AsyncFunctionDef", "ClassDef"];

fn extract_name(node: &SexpNode) -> Option<String> {
    let kind = node.type_name()?;
    if NAMED_KINDS.contains(&kind) {
        node.field_atom("name")
    } else {
        None
    }
}

/// Parses a root s-expression and returns the top-level body declarations.
///
/// Handles Python-style `(Module (body …))` output as well as bare lists.
pub fn parse_named_blocks(sexp: &str) -> Vec<NamedBlock> {
    let sexp = sexp.trim();
    if sexp.is_empty() {
        return vec![];
    }
    let root = match parse_sexp(sexp) {
        Some(r) => r,
        None => return vec![],
    };

    // Try module body first; fall back to direct children of the root list.
    let body_items = root.field_children("body");
    let items: Vec<&SexpNode> = if !body_items.is_empty() {
        body_items
    } else {
        match &root {
            SexpNode::List(ch) => ch.iter().collect(),
            _ => vec![&root],
        }
    };

    items
        .into_iter()
        .map(|node| {
            let kind = node.type_name().unwrap_or("Unknown").to_string();
            let name = extract_name(node);
            let sexp = node.serialize();
            NamedBlock { kind, name, sexp }
        })
        .collect()
}

// ── Modification summary ──────────────────────────────────────────────────────

/// Brief English description of what structurally changed between two
/// serialised s-expressions for the same named block.
pub fn diff_summary(old_sexp: &str, new_sexp: &str) -> String {
    let old_node = parse_sexp(old_sexp);
    let new_node = parse_sexp(new_sexp);
    match (old_node, new_node) {
        (Some(old), Some(new)) => {
            let sig_changed = old.field("args").map(|n| n.serialize())
                != new.field("args").map(|n| n.serialize());
            let body_changed = old.field("body").map(|n| n.serialize())
                != new.field("body").map(|n| n.serialize());
            let deco_changed = old.field("decorator_list").map(|n| n.serialize())
                != new.field("decorator_list").map(|n| n.serialize());
            match (sig_changed, body_changed, deco_changed) {
                (true, true, _) => "signature and body changed".to_string(),
                (true, false, _) => "signature changed".to_string(),
                (false, true, _) => "body changed".to_string(),
                (_, _, true) => "decorators changed".to_string(),
                _ => "implementation changed".to_string(),
            }
        }
        _ => "changed".to_string(),
    }
}

// ── AstChange / AstDiff / SemanticAst ────────────────────────────────────────

/// A single change detected between two AST versions at the top-level block level.
#[derive(Debug, PartialEq, Clone)]
pub enum AstChange {
    /// A new top-level block was introduced.
    Added {
        kind: String,
        name: Option<String>,
        sexp: String,
    },
    /// An existing top-level block was removed.
    Deleted {
        kind: String,
        name: Option<String>,
        sexp: String,
    },
    /// A block with the same identifier exists in both versions but its
    /// content changed.
    Modified {
        kind: String,
        name: String,
        old_sexp: String,
        new_sexp: String,
        /// Short English description (e.g. `"signature changed"`).
        change_summary: String,
    },
    /// A block moved to a different position without content changes.
    Moved {
        kind: String,
        name: Option<String>,
        sexp: String,
        old_index: usize,
        new_index: usize,
    },
    /// A block that is structurally identical and in the same relative position.
    Unchanged {
        kind: String,
        name: Option<String>,
        sexp: String,
    },
}

/// Result of a structural comparison between two AST versions.
pub struct AstDiff {
    pub changes: Vec<AstChange>,
    /// Fraction of top-level blocks that are completely unchanged (0.0 – 1.0).
    pub similarity: f32,
}

pub struct SemanticAst {
    pub raw_sexp: String,
    pub structure_hash: String,
}

impl SemanticAst {
    /// Creates a `SemanticAst` from an s-expression string.
    pub fn from_sexp(sexp: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(sexp.as_bytes());
        let structure_hash = format!("{:x}", hasher.finalize());
        SemanticAst {
            raw_sexp: sexp.to_string(),
            structure_hash,
        }
    }

    /// Compares `self` (the base/old version) against `other` (the new version).
    ///
    /// **Named blocks** (functions, classes) are matched by identifier so that
    /// a function whose body changed is reported as `Modified` rather than
    /// `Deleted` + `Added`.
    ///
    /// **Unnamed blocks** (imports, top-level expressions) are matched by
    /// content hash.
    pub fn diff(&self, other: &Self) -> AstDiff {
        let base_blocks = parse_named_blocks(&self.raw_sexp);
        let head_blocks = parse_named_blocks(&other.raw_sexp);

        let mut changes: Vec<AstChange> = Vec::new();

        // ── Named block matching (by identifier) ─────────────────────────────

        let mut base_named: HashMap<String, (usize, &NamedBlock)> = HashMap::new();
        let mut head_named: HashMap<String, (usize, &NamedBlock)> = HashMap::new();
        let mut base_unnamed: Vec<(usize, &NamedBlock)> = Vec::new();
        let mut head_unnamed: Vec<(usize, &NamedBlock)> = Vec::new();

        for (i, b) in base_blocks.iter().enumerate() {
            match &b.name {
                Some(n) => {
                    base_named.insert(n.clone(), (i, b));
                }
                None => base_unnamed.push((i, b)),
            }
        }
        for (i, b) in head_blocks.iter().enumerate() {
            match &b.name {
                Some(n) => {
                    head_named.insert(n.clone(), (i, b));
                }
                None => head_unnamed.push((i, b)),
            }
        }

        let mut matched_base: HashSet<String> = HashSet::new();

        for (name, (head_idx, head_b)) in &head_named {
            if let Some((base_idx, base_b)) = base_named.get(name) {
                matched_base.insert(name.clone());
                if base_b.sexp == head_b.sexp {
                    if base_idx == head_idx {
                        changes.push(AstChange::Unchanged {
                            kind: head_b.kind.clone(),
                            name: head_b.name.clone(),
                            sexp: head_b.sexp.clone(),
                        });
                    } else {
                        changes.push(AstChange::Moved {
                            kind: head_b.kind.clone(),
                            name: head_b.name.clone(),
                            sexp: head_b.sexp.clone(),
                            old_index: *base_idx,
                            new_index: *head_idx,
                        });
                    }
                } else {
                    changes.push(AstChange::Modified {
                        kind: head_b.kind.clone(),
                        name: name.clone(),
                        old_sexp: base_b.sexp.clone(),
                        new_sexp: head_b.sexp.clone(),
                        change_summary: diff_summary(&base_b.sexp, &head_b.sexp),
                    });
                }
            } else {
                changes.push(AstChange::Added {
                    kind: head_b.kind.clone(),
                    name: head_b.name.clone(),
                    sexp: head_b.sexp.clone(),
                });
            }
        }

        for (name, (_, base_b)) in &base_named {
            if !matched_base.contains(name) {
                changes.push(AstChange::Deleted {
                    kind: base_b.kind.clone(),
                    name: base_b.name.clone(),
                    sexp: base_b.sexp.clone(),
                });
            }
        }

        // ── Unnamed block matching (by content hash) ──────────────────────────

        let hash_of = |s: &str| -> String {
            let mut h = Sha256::new();
            h.update(s.as_bytes());
            format!("{:x}", h.finalize())
        };

        let base_hash_map: HashMap<String, (usize, &NamedBlock)> = base_unnamed
            .iter()
            .map(|(i, b)| (hash_of(&b.sexp), (*i, *b)))
            .collect();
        let mut matched_base_hashes: HashSet<String> = HashSet::new();

        for (head_idx, head_b) in &head_unnamed {
            let h = hash_of(&head_b.sexp);
            if let Some((base_idx, _)) = base_hash_map.get(&h) {
                matched_base_hashes.insert(h);
                if base_idx == head_idx {
                    changes.push(AstChange::Unchanged {
                        kind: head_b.kind.clone(),
                        name: None,
                        sexp: head_b.sexp.clone(),
                    });
                } else {
                    changes.push(AstChange::Moved {
                        kind: head_b.kind.clone(),
                        name: None,
                        sexp: head_b.sexp.clone(),
                        old_index: *base_idx,
                        new_index: *head_idx,
                    });
                }
            } else {
                changes.push(AstChange::Added {
                    kind: head_b.kind.clone(),
                    name: None,
                    sexp: head_b.sexp.clone(),
                });
            }
        }

        for (h, (_, base_b)) in &base_hash_map {
            if !matched_base_hashes.contains(h) {
                changes.push(AstChange::Deleted {
                    kind: base_b.kind.clone(),
                    name: None,
                    sexp: base_b.sexp.clone(),
                });
            }
        }

        // ── Similarity ────────────────────────────────────────────────────────

        let unchanged = changes
            .iter()
            .filter(|c| matches!(c, AstChange::Unchanged { .. }))
            .count();
        let total = base_blocks.len().max(head_blocks.len());
        let similarity = if total == 0 { 1.0 } else { unchanged as f32 / total as f32 };

        AstDiff { changes, similarity }
    }
}

// ── Display ───────────────────────────────────────────────────────────────────

impl AstDiff {
    /// Renders the diff to the terminal using `console` colours.
    pub fn print_stylish(&self, file_label: &str) {
        let added = self
            .changes
            .iter()
            .filter(|c| matches!(c, AstChange::Added { .. }))
            .count();
        let deleted = self
            .changes
            .iter()
            .filter(|c| matches!(c, AstChange::Deleted { .. }))
            .count();
        let modified = self
            .changes
            .iter()
            .filter(|c| matches!(c, AstChange::Modified { .. }))
            .count();
        let moved = self
            .changes
            .iter()
            .filter(|c| matches!(c, AstChange::Moved { .. }))
            .count();
        let unchanged = self
            .changes
            .iter()
            .filter(|c| matches!(c, AstChange::Unchanged { .. }))
            .count();

        println!(
            "\n{} {}",
            style("Structural Diff:").bold(),
            style(file_label).yellow()
        );
        println!(
            "Similarity {}  ·  {} Added  ·  {} Modified  ·  {} Moved  ·  {} Deleted  ·  {} Unchanged\n",
            style(format!("{:.1}%", self.similarity * 100.0)).cyan().bold(),
            style(format!("+{added}")).green().bold(),
            style(format!("~{modified}")).yellow().bold(),
            style(format!("↕{moved}")).blue().bold(),
            style(format!("-{deleted}")).red().bold(),
            style(unchanged).dim(),
        );

        // Render in a logical order: additions and modifications first for
        // quick scanning, unchanged at the bottom.
        let mut sorted: Vec<&AstChange> = self.changes.iter().collect();
        sorted.sort_by_key(|c| match c {
            AstChange::Added { .. } => 0,
            AstChange::Modified { .. } => 1,
            AstChange::Moved { .. } => 2,
            AstChange::Deleted { .. } => 3,
            AstChange::Unchanged { .. } => 4,
        });

        for change in sorted {
            match change {
                AstChange::Added { kind, name, .. } => {
                    println!(
                        "  {}  {}",
                        style("+[Added]    ").green().bold(),
                        style(block_label(kind, name.as_deref())).green()
                    );
                }
                AstChange::Deleted { kind, name, .. } => {
                    println!(
                        "  {}  {}",
                        style("-[Deleted]  ").red().bold(),
                        style(block_label(kind, name.as_deref())).red()
                    );
                }
                AstChange::Modified {
                    kind,
                    name,
                    change_summary,
                    ..
                } => {
                    println!(
                        "  {}  {}  {}",
                        style("~[Modified] ").yellow().bold(),
                        style(block_label(kind, Some(name))).yellow(),
                        style(format!("({change_summary})")).dim()
                    );
                }
                AstChange::Moved {
                    kind,
                    name,
                    old_index,
                    new_index,
                    ..
                } => {
                    println!(
                        "  {}  {}  {}",
                        style("↕[Moved]    ").blue().bold(),
                        style(block_label(kind, name.as_deref())).blue(),
                        style(format!("(position {} → {})", old_index + 1, new_index + 1)).dim()
                    );
                }
                AstChange::Unchanged { kind, name, .. } => {
                    println!(
                        "   [Unchanged]  {}",
                        style(block_label(kind, name.as_deref())).dim()
                    );
                }
            }
        }
        println!();
    }
}

fn block_label(kind: &str, name: Option<&str>) -> String {
    let short = match kind {
        "FunctionDef" => "fn",
        "AsyncFunctionDef" => "async fn",
        "ClassDef" => "class",
        "Import" | "ImportFrom" => "import",
        other => other,
    };
    match name {
        Some(n) => format!("{short} {n}"),
        None => short.to_string(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tokenizer ─────────────────────────────────────────────────────────────

    #[test]
    fn tokenize_simple() {
        let tokens = tokenize("(foo bar)");
        assert_eq!(
            tokens,
            vec![
                SexpToken::Open,
                SexpToken::Atom("foo".into()),
                SexpToken::Atom("bar".into()),
                SexpToken::Close,
            ]
        );
    }

    #[test]
    fn tokenize_nested() {
        let tokens = tokenize("(a (b c))");
        assert_eq!(tokens.len(), 7); // ( a ( b c ) )
    }

    // ── SexpNode helpers ──────────────────────────────────────────────────────

    #[test]
    fn sexp_field_atom_strips_quotes() {
        let node = parse_sexp("(FunctionDef (name 'foo'))").unwrap();
        assert_eq!(node.field_atom("name"), Some("foo".to_string()));
    }

    #[test]
    fn sexp_field_children_returns_items() {
        let node = parse_sexp("(Module (body (FunctionDef (name 'a')) (ClassDef (name 'B'))))").unwrap();
        let children = node.field_children("body");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].type_name(), Some("FunctionDef"));
        assert_eq!(children[1].type_name(), Some("ClassDef"));
    }

    #[test]
    fn sexp_serialize_roundtrip() {
        let src = "(Module (body (FunctionDef (name 'foo'))))";
        let node = parse_sexp(src).unwrap();
        // Canonical form — whitespace is normalised to single spaces.
        assert_eq!(node.serialize(), src);
    }

    // ── Named block extraction ────────────────────────────────────────────────

    #[test]
    fn parse_named_blocks_extracts_functions_and_classes() {
        let sexp = "(Module (body \
            (FunctionDef (name 'validate') (args (arguments)) (body (Pass))) \
            (ClassDef (name 'Token') (bases) (keywords) (body (Pass)))))";
        let blocks = parse_named_blocks(sexp);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].name.as_deref(), Some("validate"));
        assert_eq!(blocks[0].kind, "FunctionDef");
        assert_eq!(blocks[1].name.as_deref(), Some("Token"));
        assert_eq!(blocks[1].kind, "ClassDef");
    }

    #[test]
    fn parse_named_blocks_empty_input() {
        assert!(parse_named_blocks("").is_empty());
    }

    #[test]
    fn parse_named_blocks_no_named_items() {
        let sexp = "(Module (body (Import (names (alias (name 'os')))) (Expr (value (Constant (value 1))))))";
        let blocks = parse_named_blocks(sexp);
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|b| b.name.is_none()));
    }

    // ── diff_summary ──────────────────────────────────────────────────────────

    #[test]
    fn diff_summary_detects_signature_change() {
        let old = "(FunctionDef (name 'foo') (args (arguments (args (arg (arg 'x'))))) (body (Pass)))";
        let new = "(FunctionDef (name 'foo') (args (arguments (args (arg (arg 'x')) (arg (arg 'y'))))) (body (Pass)))";
        let summary = diff_summary(old, new);
        assert!(summary.contains("signature"), "got: {summary}");
    }

    #[test]
    fn diff_summary_detects_body_change() {
        let old = "(FunctionDef (name 'foo') (args (arguments)) (body (Pass)))";
        let new = "(FunctionDef (name 'foo') (args (arguments)) (body (Return (value (Constant (value 42))))))";
        let summary = diff_summary(old, new);
        assert!(summary.contains("body"), "got: {summary}");
    }

    // ── SemanticAst::diff ─────────────────────────────────────────────────────

    fn fn_sexp(name: &str, body: &str) -> String {
        format!(
            "(FunctionDef (name '{name}') (args (arguments)) (body {body}))"
        )
    }

    fn module(body_items: &[String]) -> String {
        format!("(Module (body {}))", body_items.join(" "))
    }

    #[test]
    fn test_ast_hash_consistency() {
        let a = SemanticAst::from_sexp("(Module (body (FunctionDef (name 'main'))))");
        let b = SemanticAst::from_sexp("(Module (body (FunctionDef (name 'main'))))");
        assert_eq!(a.structure_hash, b.structure_hash);
    }

    #[test]
    fn test_diff_detect_addition() {
        let base = SemanticAst::from_sexp(&module(&[fn_sexp("a", "(Pass)")]));
        let head = SemanticAst::from_sexp(&module(&[
            fn_sexp("a", "(Pass)"),
            fn_sexp("b", "(Pass)"),
        ]));
        let diff = base.diff(&head);
        assert!(diff.changes.iter().any(|c| matches!(c, AstChange::Added { name: Some(n), .. } if n == "b")));
        assert_eq!(diff.similarity, 0.5);
    }

    #[test]
    fn test_diff_detect_deletion() {
        let base = SemanticAst::from_sexp(&module(&[
            fn_sexp("a", "(Pass)"),
            fn_sexp("b", "(Pass)"),
        ]));
        let head = SemanticAst::from_sexp(&module(&[fn_sexp("a", "(Pass)")]));
        let diff = base.diff(&head);
        assert!(diff.changes.iter().any(|c| matches!(c, AstChange::Deleted { name: Some(n), .. } if n == "b")));
    }

    #[test]
    fn test_diff_detect_modification() {
        let base = SemanticAst::from_sexp(&module(&[fn_sexp("foo", "(Pass)")]));
        let head = SemanticAst::from_sexp(&module(&[fn_sexp("foo", "(Return (value (Constant (value 1))))]")]));
        let diff = base.diff(&head);
        assert!(diff.changes.iter().any(|c| matches!(c, AstChange::Modified { name, .. } if name == "foo")));
    }

    #[test]
    fn test_diff_detect_move() {
        let base = SemanticAst::from_sexp(&module(&[
            fn_sexp("a", "(Pass)"),
            fn_sexp("b", "(Pass)"),
        ]));
        let head = SemanticAst::from_sexp(&module(&[
            fn_sexp("b", "(Pass)"),
            fn_sexp("a", "(Pass)"),
        ]));
        let diff = base.diff(&head);
        let moved: Vec<_> = diff
            .changes
            .iter()
            .filter(|c| matches!(c, AstChange::Moved { .. }))
            .collect();
        assert_eq!(moved.len(), 2);
    }

    #[test]
    fn test_diff_empty_ast() {
        let base = SemanticAst::from_sexp("");
        let head = SemanticAst::from_sexp("");
        let diff = base.diff(&head);
        assert_eq!(diff.similarity, 1.0);
        assert!(diff.changes.is_empty());
    }

    #[test]
    fn test_diff_complete_replacement() {
        let base = SemanticAst::from_sexp(&module(&[fn_sexp("a", "(Pass)")]));
        let head = SemanticAst::from_sexp(&module(&[fn_sexp("b", "(Pass)")]));
        let diff = base.diff(&head);
        assert!(diff.changes.iter().any(|c| matches!(c, AstChange::Deleted { .. })));
        assert!(diff.changes.iter().any(|c| matches!(c, AstChange::Added { .. })));
        assert_eq!(diff.similarity, 0.0);
    }

    #[test]
    fn test_diff_unchanged_is_unchanged() {
        let sexp = module(&[fn_sexp("foo", "(Pass)"), fn_sexp("bar", "(Pass)")]);
        let base = SemanticAst::from_sexp(&sexp);
        let head = SemanticAst::from_sexp(&sexp);
        let diff = base.diff(&head);
        assert!(diff
            .changes
            .iter()
            .all(|c| matches!(c, AstChange::Unchanged { .. })));
        assert_eq!(diff.similarity, 1.0);
    }

    // ── Real Python AST integration ───────────────────────────────────────────

    /// Uses realistic s-expression output (as produced by h5i-py-parser.py) to
    /// verify the full diff pipeline: signature change, body change, addition,
    /// and unnamed block (import) detection.
    #[test]
    fn test_diff_real_python_ast() {
        // v1: validate_token(tok), generate_token(user_id), class TokenStore
        let s1 = "(Module (body \
            (FunctionDef (name 'validate_token') \
                (args (arguments (args (arg (arg 'tok'))))) \
                (body (Return (value (Compare (left (Call (func (Name (id 'len'))) \
                    (args (Name (id 'tok'))))) (ops (Eq)) (comparators (Constant (value 64)))))))) \
            (FunctionDef (name 'generate_token') \
                (args (arguments (args (arg (arg 'user_id'))))) \
                (body (Return (value (BinOp (left (Name (id 'user_id'))) (op (Add)) \
                    (right (Constant (value '_token')))))))) \
            (ClassDef (name 'TokenStore') \
                (body (FunctionDef (name 'get') \
                    (args (arguments (args (arg (arg 'self')) (arg (arg 'key'))))) \
                    (body (Pass)))))))";

        // v2: added `import hashlib`, validate_token gains `expiry` arg,
        //     new function refresh_token, TokenStore.get returns None
        let s2 = "(Module (body \
            (Import (names (alias (name 'hashlib')))) \
            (FunctionDef (name 'validate_token') \
                (args (arguments (args (arg (arg 'tok')) (arg (arg 'expiry'))))) \
                (body (Return (value (BoolOp (op (And)) (values \
                    (Compare (left (Call (func (Name (id 'len'))) (args (Name (id 'tok')))) ) \
                        (ops (Eq)) (comparators (Constant (value 64)))) \
                    (Compare (left (Name (id 'expiry'))) (ops (Gt)) \
                        (comparators (Constant (value 0)))))))))) \
            (FunctionDef (name 'generate_token') \
                (args (arguments (args (arg (arg 'user_id'))))) \
                (body (Return (value (BinOp (left (Name (id 'user_id'))) (op (Add)) \
                    (right (Constant (value '_token')))))))) \
            (FunctionDef (name 'refresh_token') \
                (args (arguments (args (arg (arg 'tok'))))) \
                (body (Return (value (BinOp (left (Name (id 'tok'))) (op (Add)) \
                    (right (Constant (value '_new')))))))) \
            (ClassDef (name 'TokenStore') \
                (body (FunctionDef (name 'get') \
                    (args (arguments (args (arg (arg 'self')) (arg (arg 'key'))))) \
                    (body (Return (value (Constant)))))))))";

        let base = SemanticAst::from_sexp(s1);
        let head = SemanticAst::from_sexp(s2);
        let diff = base.diff(&head);

        // validate_token: signature changed (new `expiry` arg)
        let validate_change = diff.changes.iter().find(|c| {
            matches!(c, AstChange::Modified { name, .. } if name == "validate_token")
        });
        assert!(validate_change.is_some(), "expected validate_token to be Modified");
        if let Some(AstChange::Modified { change_summary, .. }) = validate_change {
            assert!(
                change_summary.contains("signature"),
                "expected signature change, got: {change_summary}"
            );
        }

        // generate_token: unchanged (same sexp)
        assert!(diff.changes.iter().any(|c| {
            matches!(c, AstChange::Unchanged { name: Some(n), .. } if n == "generate_token")
        }));

        // refresh_token: added
        assert!(diff.changes.iter().any(|c| {
            matches!(c, AstChange::Added { name: Some(n), .. } if n == "refresh_token")
        }));

        // TokenStore: modified (body changed — get now returns None instead of Pass)
        assert!(diff.changes.iter().any(|c| {
            matches!(c, AstChange::Modified { name, .. } if name == "TokenStore")
        }));

        // The unnamed import block is new in v2
        assert!(diff.changes.iter().any(|c| {
            matches!(c, AstChange::Added { kind, name: None, .. } if kind == "Import")
        }));
    }
}
