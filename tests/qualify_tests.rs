// qualify_tests.rs
use h5i_core::repository::H5iRepository;
use h5i_core::metadata::TestMetrics;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::error::Error;

/// Response structure from Claude
#[derive(Debug, Deserialize, Serialize)]
pub struct TestCoverageEvaluation {
    pub adequacy_score: f32, // 0.0 = inadequate, 1.0 = fully adequate
    pub comments: String,
}

/// Fetch the commit diff as a string
fn get_commit_diff(repo: &H5iRepository, commit_oid: &str) -> Result<String, Box<dyn Error>> {
    let diff = repo.git_diff_commit(commit_oid)?;
    Ok(diff)
}

/// Query Claude API for test coverage evaluation
pub async fn query_claude_for_coverage(
    commit_diff: &str,
    metrics: &TestMetrics,
) -> Result<TestCoverageEvaluation, Box<dyn Error>> {
    let api_key = env::var("CLAUDE_API_KEY")?;
    let client = reqwest::Client::new();

    let prompt = format!(
        "Evaluate test coverage for the following commit diff:\n\n{}\n\nTest metrics:\n{:#?}\n\nReturn JSON: {{\"adequacy_score\": float 0.0-1.0, \"comments\": string}}",
        commit_diff, metrics
    );

    let request_body = json!({
        "model": "claude-3.0",
        "prompt": prompt,
        "max_tokens_to_sample": 300
    });

    let res = client
        .post("https://api.anthropic.com/v1/complete")
        .header("x-api-key", api_key)
        .json(&request_body)
        .send()
        .await?;

    let text = res.text().await?;
    let json_start = text.find('{').ok_or("No JSON found in response")?;
    let json_end = text.rfind('}').ok_or("No JSON end found")?;
    let json_str = &text[json_start..=json_end];

    let eval: TestCoverageEvaluation = serde_json::from_str(json_str)?;
    Ok(eval)
}

/* 

Example usage

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let repo_path = "."; // current repo
    let commit_oid = "HEAD"; // or any specific commit SHA

    let repo = H5iRepository::open(repo_path)?;
    let diff = get_commit_diff(&repo, commit_oid)?;
    let record = repo.load_h5i_record(commit_oid)?;
    let metrics = record.test_metrics.ok_or("No test metrics found")?;

    let eval = query_claude_for_coverage(&diff, &metrics).await?;
    println!("Adequacy score: {}", eval.adequacy_score);
    println!("Comments: {}", eval.comments);

    Ok(())
}

*/
