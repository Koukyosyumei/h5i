//! `h5i websec experiment`: many sends, one question, and the answers folded
//! down to the rows that differ (design-websec.md W22).
//!
//! Intruder's shape, for a caller that is not looking at a screen. The values
//! and the combinations come from the agent; what this adds is sending them
//! through the engine at a stated rate and handing back clusters instead of
//! five hundred responses. Nothing here generates a payload, and nothing here
//! calls a difference a vulnerability.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use h5i_wire::message::StoredResponse;
use h5i_wire::read::{Text, body_text, read_json};
use h5i_wire::triage::{By, Fingerprint, Sample, cluster, text_digest};
use serde::Deserialize;
use serde_json::{Value, json};

/// The most sends one experiment may ask for. The engine's ceiling, checked
/// here too so a product of four positions is refused before anything is sent.
pub const MAX_STEPS: usize = 1000;

/// The most members one cluster lists by name.
const MAX_VALUES_SHOWN: usize = 10;

/// The most matches one extractor reports.
const MAX_MATCHES_SHOWN: usize = 20;

/// The longest a value may be where it appears in a label.
const MAX_LABEL_VALUE: usize = 48;

/// What the caller is asking.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The stored request to vary: `req_42`, or `42`.
    pub request: String,
    /// What varies, in the order the labels read.
    pub positions: Vec<Position>,
    #[serde(default)]
    pub strategy: Strategy,
    /// Edits every send shares, as `--set` takes them.
    #[serde(default)]
    pub set: Vec<String>,
    /// Add targets that are not already there.
    #[serde(default)]
    pub create: bool,
    /// A stored response to compare each cluster's representative against.
    #[serde(default)]
    pub baseline: Option<String>,
    /// Name to pattern: `regex:SQL syntax` or plain text to look for.
    #[serde(default)]
    pub extract: BTreeMap<String, String>,
    /// Sends per second, at most.
    #[serde(default)]
    pub rate: Option<f64>,
    /// Send from another session, with that session's credentials. Identity is
    /// a position like any other, and this is how it varies.
    #[serde(rename = "as", default)]
    pub as_session: Option<String>,
    #[serde(default)]
    pub keep_credentials: bool,
}

/// One thing that varies.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    /// What to call it in a label. The target, when not given.
    #[serde(default)]
    pub name: Option<String>,
    /// `query.user`, `json.role`, `header.X-Role`, `path`.
    pub target: String,
    #[serde(default)]
    pub values: Vec<String>,
    /// One value per line, from a file. Wordlists are input, never cargo.
    #[serde(default)]
    pub values_file: Option<String>,
}

/// How the positions combine.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Every combination. Two positions of ten values are a hundred sends.
    #[default]
    Product,
    /// The nth value of every position together, which needs them the same
    /// length. A user and the password that goes with it, not every pairing.
    Zip,
}

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Strategy::Product => "product",
            Strategy::Zip => "zip",
        }
    }
}

/// One send, expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub label: String,
    pub set: Vec<String>,
}

impl Plan {
    /// Read the values every position walks, from the file or the list.
    fn values_of(&self, position: &Position, base: &Path) -> anyhow::Result<Vec<String>> {
        let name = position.name.as_deref().unwrap_or(&position.target);
        match (&position.values_file, position.values.is_empty()) {
            (Some(_), false) => anyhow::bail!(
                "position `{name}` gives both `values` and `values_file`, and which one it \
                 walks would be a guess"
            ),
            (None, true) => anyhow::bail!("position `{name}` has no values to walk"),
            (None, false) => Ok(position.values.clone()),
            (Some(file), true) => {
                // Relative to the experiment file, so a plan and its wordlist
                // move together.
                let path = base.join(file);
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    anyhow::anyhow!("position `{name}`: {} could not be read: {e}", path.display())
                })?;
                let values: Vec<String> = text
                    .lines()
                    .map(|line| line.trim_end_matches('\r').to_string())
                    .collect();
                // Only the final newline goes; an empty value in the middle is
                // a value, and often the interesting one.
                let values = match values.split_last() {
                    Some((last, rest)) if last.is_empty() => rest.to_vec(),
                    _ => values,
                };
                if values.is_empty() {
                    anyhow::bail!("position `{name}`: {} has no values in it", path.display());
                }
                Ok(values)
            }
        }
    }

    /// The sends this plan means, in order.
    pub fn expand(&self, base: &Path) -> anyhow::Result<Vec<Step>> {
        if self.positions.is_empty() {
            anyhow::bail!(
                "an experiment needs at least one position. One request sent once is `replay`"
            );
        }
        let mut names = Vec::new();
        let mut columns = Vec::new();
        for position in &self.positions {
            names.push(
                position
                    .name
                    .clone()
                    .unwrap_or_else(|| position.target.clone()),
            );
            columns.push(self.values_of(position, base)?);
        }

        let rows: Vec<Vec<usize>> = match self.strategy {
            Strategy::Zip => {
                let length = columns[0].len();
                if let Some(odd) = columns.iter().position(|column| column.len() != length) {
                    anyhow::bail!(
                        "`zip` takes the nth value of every position together, so they have to \
                         be the same length: `{}` has {} and `{}` has {}",
                        names[0],
                        length,
                        names[odd],
                        columns[odd].len()
                    );
                }
                (0..length).map(|n| vec![n; columns.len()]).collect()
            }
            Strategy::Product => {
                let total: usize = columns.iter().map(Vec::len).product();
                if total > MAX_STEPS {
                    anyhow::bail!(
                        "this is {total} sends, and {MAX_STEPS} is the most one experiment \
                         sends. Narrow a position, or split the plan"
                    );
                }
                let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
                for column in &columns {
                    // The last position varies fastest, so the labels read the
                    // way the plan does.
                    rows = rows
                        .into_iter()
                        .flat_map(|row| {
                            (0..column.len()).map(move |n| {
                                let mut row = row.clone();
                                row.push(n);
                                row
                            })
                        })
                        .collect();
                }
                rows
            }
        };
        if rows.len() > MAX_STEPS {
            anyhow::bail!(
                "this is {} sends, and {MAX_STEPS} is the most one experiment sends. Narrow a \
                 position, or split the plan",
                rows.len()
            );
        }

        Ok(rows
            .into_iter()
            .map(|row| {
                let mut set = Vec::with_capacity(row.len());
                let mut label = Vec::with_capacity(row.len());
                for (at, &n) in row.iter().enumerate() {
                    let value = &columns[at][n];
                    set.push(format!("{}={value}", self.positions[at].target));
                    label.push(format!("{}={}", names[at], shorten(value)));
                }
                Step {
                    label: label.join(" "),
                    set,
                }
            })
            .collect())
    }
}

/// Values are a target's bytes as often as not, and a label is a display
/// string. The message it came from still holds every byte.
fn shorten(value: &str) -> String {
    if value.chars().count() <= MAX_LABEL_VALUE {
        return value.to_string();
    }
    let head: String = value.chars().take(MAX_LABEL_VALUE).collect();
    format!("{head}…")
}

/// What one extractor found.
struct Extractor {
    name: String,
    regex: Option<regex::Regex>,
    text: String,
}

impl Extractor {
    fn build(extract: &BTreeMap<String, String>) -> anyhow::Result<Vec<Self>> {
        extract
            .iter()
            .map(|(name, pattern)| match pattern.strip_prefix("regex:") {
                Some(expression) => Ok(Self {
                    name: name.clone(),
                    regex: Some(regex::Regex::new(expression).map_err(|e| {
                        anyhow::anyhow!("extractor `{name}` is not a regex: {e}")
                    })?),
                    text: String::new(),
                }),
                None => Ok(Self {
                    name: name.clone(),
                    regex: None,
                    text: pattern.clone(),
                }),
            })
            .collect()
    }

    /// What this extractor takes out of one body, if anything.
    fn found(&self, body: &str) -> Option<String> {
        match &self.regex {
            // The first capture group when there is one, because an extractor
            // usually wants the value inside the pattern rather than the
            // pattern.
            Some(expression) => expression.captures(body).map(|caught| {
                caught
                    .get(1)
                    .or_else(|| caught.get(0))
                    .map(|m| shorten(m.as_str()))
                    .unwrap_or_default()
            }),
            None => body.contains(&self.text).then(|| self.text.clone()),
        }
    }
}

/// Run the plan: send it, then read what came back.
pub fn run(
    root: &Path,
    selector: Option<&str>,
    file: &str,
    json_out: bool,
    h5i: &std::ffi::OsStr,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("{file} could not be read: {e}"))?;
    let plan: Plan = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{file} is not an experiment: {e}"))?;
    let base = Path::new(file).parent().unwrap_or(Path::new(".")).to_path_buf();

    let extractors = Extractor::build(&plan.extract)?;
    let steps = plan.expand(&base)?;
    let seq = crate::sequence_of(&plan.request)?;

    let answer = send(&plan, &steps, &seq, selector, h5i)?;
    if answer.get("ok").and_then(Value::as_bool) != Some(true)
        && answer.get("samples").is_none()
    {
        // The engine refused before it sent anything: its message is the one
        // worth reading, not a summary of it.
        println!("{}", serde_json::to_string_pretty(&answer)?);
        std::process::exit(2);
    }

    // With `--as`, the sends are the other session's: its cookies, its policy,
    // its receipts, its store. Reading this session's would silently answer
    // with whatever message happened to hold the same number.
    let landed = plan.as_session.as_deref().or(selector);
    let (_session, store) = crate::read::store_dir(root, landed)?;
    let sent = answer
        .get("samples")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut samples: Vec<Sample> = Vec::with_capacity(sent.len());
    let mut found: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (at, item) in sent.iter().enumerate() {
        let Some(seq) = item.get("seq").and_then(Value::as_u64) else {
            // A send that never reached the wire has no message to read. It is
            // still one of the plan's steps, and saying so beats a gap.
            continue;
        };
        let label = item
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| steps.get(at).map(|step| step.label.clone()))
            .unwrap_or_default();
        let req = format!("req_{seq}");
        let Ok(response) = read_json::<StoredResponse>(&store.join(format!("{seq}.response.json")))
        else {
            continue;
        };
        let body = body_text(&store, &response.body);
        let path = url::Url::parse(&response.url)
            .map(|url| url.path().to_string())
            .unwrap_or_default();
        let content_type = header(&response, "content-type").unwrap_or_default();
        let location = header(&response, "location");

        for extractor in &extractors {
            if let Some(value) = extractor.found(body.as_str()) {
                let hits = found.entry(extractor.name.clone()).or_default();
                if hits.len() < MAX_MATCHES_SHOWN {
                    hits.push(json!({"req": req, "values": label, "found": value}));
                }
            }
        }

        samples.push(Sample {
            endpoint: path.clone(),
            name: label,
            req,
            fingerprint: Fingerprint::of(
                response.status,
                &content_type,
                body.len().unwrap_or_default(),
                location.as_deref(),
            ),
            // No tag reader here on purpose: what the document says separates
            // two answers of one shape, and that is the difference an
            // experiment came for.
            skeleton: String::new(),
            text: text_digest(body.as_str(), &path),
        });
    }

    // Words as well as shape: "same status, same size, different answer" is
    // the result, not the noise (`h5i-wire::triage::By`).
    let clusters = cluster(&samples, By::ShapeAndWords);

    // The plan's ids are all read in one frame: `request` names a message in
    // the session that holds it, and so does `baseline`. What came back is
    // read where it landed, which is a different store under `--as`.
    let baseline = match &plan.baseline {
        None => None,
        Some(id) => {
            let (_, from) = crate::read::store_dir(root, selector)?;
            let seq: u64 = crate::sequence_of(id)?.parse()?;
            let response: StoredResponse =
                read_json(&from.join(format!("{seq}.response.json"))).map_err(|_| {
                    anyhow::anyhow!("this session has no stored response {seq} to compare against")
                })?;
            Some((id.clone(), body_text(&from, &response.body)))
        }
    };

    let bodies = |req: &str| -> Text {
        let seq: u64 = req.trim_start_matches("req_").parse().unwrap_or_default();
        match read_json::<StoredResponse>(&store.join(format!("{seq}.response.json"))) {
            Ok(response) => body_text(&store, &response.body),
            Err(why) => Text::Missing(why),
        }
    };

    let rows: Vec<Value> = clusters
        .iter()
        .map(|group| {
            let mut row = json!({
                "label": group.label,
                "count": group.count,
                "representative": group.representative,
                "values": group.names.iter().take(MAX_VALUES_SHOWN).collect::<Vec<_>>(),
                "members": group.members,
            });
            if let Some((_, against)) = &baseline {
                let similarity =
                    crate::read::similarity(against.as_str(), bodies(&group.representative).as_str());
                row["similarity"] = json!((similarity * 100.0).round() / 100.0);
            }
            row
        })
        .collect();

    let report = json!({
        "ok": true,
        "request": format!("req_{seq}"),
        "strategy": plan.strategy.name(),
        "planned": steps.len(),
        "sent": sent.len(),
        "read": samples.len(),
        "landed_in": landed.unwrap_or_default(),
        "rate": plan.rate,
        "as": plan.as_session,
        "baseline": baseline.as_ref().map(|(id, _)| id.clone()),
        "clusters": rows,
        "extracted": found,
    });

    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    human(&report, &clusters.len());
    Ok(())
}

fn header(response: &StoredResponse, name: &str) -> Option<String> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// Hand the walk to the engine, which is the only thing here that sends.
fn send(
    plan: &Plan,
    steps: &[Step],
    seq: &str,
    selector: Option<&str>,
    h5i: &std::ffi::OsStr,
) -> anyhow::Result<Value> {
    let walk = json!({
        "steps": steps
            .iter()
            .map(|step| json!({"label": step.label, "set": step.set}))
            .collect::<Vec<_>>(),
    });

    let mut command = Command::new(h5i);
    command.arg("browser").arg("resend").arg(seq);
    for spec in &plan.set {
        command.arg("--set").arg(spec);
    }
    if plan.create {
        command.arg("--create");
    }
    if let Some(session) = &plan.as_session {
        command.arg("--as").arg(session);
    }
    if plan.keep_credentials {
        command.arg("--keep-credentials");
    }
    if let Some(rate) = plan.rate {
        command.arg("--rate").arg(rate.to_string());
    }
    if let Some(name) = selector {
        command.arg("--session").arg(name);
    }
    // On standard input rather than in a file: a thousand steps is a large
    // argument list and a temporary file nobody deletes.
    command.arg("--walk").arg("-").arg("--json");

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not run h5i: {e}"))?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(walk.to_string().as_bytes())
        .map_err(|e| anyhow::anyhow!("the walk could not be handed to h5i: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("h5i did not finish: {e}"))?;
    serde_json::from_slice(&out.stdout).map_err(|e| {
        anyhow::anyhow!(
            "h5i answered oddly: {e}. It said: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        )
    })
}

fn human(report: &Value, clusters: &usize) {
    let count = |key: &str| report.get(key).and_then(Value::as_u64).unwrap_or_default();
    println!(
        "  {} sends, {} read, {clusters} clusters",
        count("sent"),
        count("read")
    );
    if let Some(rate) = report.get("rate").and_then(Value::as_f64) {
        println!("  at most {rate}/s");
    }
    println!();
    for row in report
        .get("clusters")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or_default();
        print!(
            "  x{:<5} {}",
            row.get("count").and_then(Value::as_u64).unwrap_or_default(),
            text("label")
        );
        match row.get("similarity").and_then(Value::as_f64) {
            Some(similarity) => println!("   similarity {similarity:.2}"),
            None => println!(),
        }
        for value in row
            .get("values")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            println!("           {}", value.as_str().unwrap_or_default());
        }
        println!(
            "           read one: h5i websec show {}",
            text("representative")
        );
    }
    let extracted = report.get("extracted").and_then(Value::as_object);
    if let Some(extracted) = extracted.filter(|found| !found.is_empty()) {
        println!();
        for (name, hits) in extracted {
            println!(
                "  {name}: {} matched",
                hits.as_array().map(Vec::len).unwrap_or_default()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(json: &str) -> Plan {
        serde_json::from_str(json).expect("a plan")
    }

    #[test]
    fn a_product_varies_the_last_position_fastest() {
        let plan = plan(
            r#"{"request":"req_1","positions":[
                {"name":"user","target":"query.user","values":["alice","bob"]},
                {"name":"role","target":"json.role","values":["user","admin"]}]}"#,
        );
        let steps = plan.expand(Path::new(".")).expect("expand");
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].label, "user=alice role=user");
        assert_eq!(steps[1].label, "user=alice role=admin");
        assert_eq!(steps[2].label, "user=bob role=user");
        assert_eq!(
            steps[3].set,
            ["query.user=bob", "json.role=admin"],
            "every position's edit rides on the same send"
        );
    }

    #[test]
    fn zip_pairs_the_columns_and_refuses_a_ragged_one() {
        let paired = plan(
            r#"{"request":"1","strategy":"zip","positions":[
                {"target":"json.user","values":["alice","bob"]},
                {"target":"json.password","values":["a1","b2"]}]}"#,
        );
        let steps = paired.expand(Path::new(".")).expect("expand");
        assert_eq!(steps.len(), 2, "a user and the password that goes with it");
        assert_eq!(steps[1].set, ["json.user=bob", "json.password=b2"]);

        let ragged = plan(
            r#"{"request":"1","strategy":"zip","positions":[
                {"target":"json.user","values":["alice","bob"]},
                {"target":"json.password","values":["a1"]}]}"#,
        );
        assert!(ragged.expand(Path::new(".")).is_err());
    }

    /// A product of four positions is how a plan that reads small sends
    /// hundreds of thousands of requests.
    #[test]
    fn a_product_too_large_to_send_is_refused_before_anything_is_sent() {
        let values: Vec<String> = (0..40).map(|n| n.to_string()).collect();
        let plan = Plan {
            request: "req_1".to_string(),
            positions: vec![
                Position {
                    name: None,
                    target: "query.a".to_string(),
                    values: values.clone(),
                    values_file: None,
                },
                Position {
                    name: None,
                    target: "query.b".to_string(),
                    values,
                    values_file: None,
                },
            ],
            strategy: Strategy::Product,
            set: Vec::new(),
            create: false,
            baseline: None,
            extract: BTreeMap::new(),
            rate: None,
            as_session: None,
            keep_credentials: false,
        };
        let refused = plan.expand(Path::new(".")).expect_err("1600 is too many");
        assert!(refused.to_string().contains("1600 sends"), "{refused}");
    }

    #[test]
    fn a_position_with_no_values_is_refused_rather_than_skipped() {
        let plan = plan(r#"{"request":"1","positions":[{"target":"query.a"}]}"#);
        assert!(plan.expand(Path::new(".")).is_err());
    }

    #[test]
    fn an_experiment_with_no_positions_is_a_replay() {
        let plan = plan(r#"{"request":"1","positions":[]}"#);
        let refused = plan.expand(Path::new(".")).expect_err("no positions");
        assert!(refused.to_string().contains("replay"), "{refused}");
    }

    /// A typo'd key that is silently ignored is how a loop runs a different
    /// experiment from the one it was handed.
    #[test]
    fn an_unknown_key_is_refused() {
        let bad = serde_json::from_str::<Plan>(
            r#"{"request":"1","positions":[{"target":"query.a","values":["1"]}],"stratergy":"zip"}"#,
        );
        assert!(bad.is_err());
    }

    #[test]
    fn an_extractor_returns_what_its_first_group_caught() {
        let mut patterns = BTreeMap::new();
        patterns.insert("error".to_string(), r"regex:SQL error: (\w+)".to_string());
        patterns.insert("marker".to_string(), "you cannot".to_string());
        let built = Extractor::build(&patterns).expect("build");

        let by_name = |name: &str| built.iter().find(|e| e.name == name).expect("extractor");
        assert_eq!(
            by_name("error").found("<p>SQL error: syntax near</p>"),
            Some("syntax".to_string())
        );
        assert_eq!(by_name("error").found("<p>fine</p>"), None);
        assert_eq!(
            by_name("marker").found("<p>you cannot see that</p>"),
            Some("you cannot".to_string())
        );
    }

    #[test]
    fn a_long_value_is_shortened_where_it_is_displayed() {
        let long = "a".repeat(200);
        assert!(shorten(&long).chars().count() <= MAX_LABEL_VALUE + 1);
        assert_eq!(shorten("short"), "short");
    }
}
