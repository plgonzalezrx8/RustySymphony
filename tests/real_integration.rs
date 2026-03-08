mod common;

use std::process::Command;

use symphony::tracker::{IssueTracker, LinearTracker};

use common::{temp_workspace, test_config};

#[tokio::test]
#[ignore = "requires live Linear credentials and project slug"]
async fn live_linear_candidate_fetch_smoke() {
    let api_key = std::env::var("LINEAR_API_KEY").expect("LINEAR_API_KEY must be set");
    let project_slug = std::env::var("SYMPHONY_LINEAR_PROJECT_SLUG")
        .expect("SYMPHONY_LINEAR_PROJECT_SLUG must be set");
    let endpoint = std::env::var("SYMPHONY_LINEAR_ENDPOINT")
        .unwrap_or_else(|_| "https://api.linear.app/graphql".into());
    let (_tempdir, workspace_root) = temp_workspace();

    let mut config = test_config(endpoint, workspace_root, "codex app-server".into());
    config.tracker.api_key = api_key;
    config.tracker.project_slug = project_slug;

    let tracker = LinearTracker::new().expect("tracker should build");
    tracker
        .fetch_candidate_issues(&config)
        .await
        .expect("live Linear request should succeed");
}

#[test]
#[ignore = "requires installed codex CLI"]
fn codex_cli_schema_generation_smoke() {
    let output = Command::new("codex")
        .args([
            "app-server",
            "generate-json-schema",
            "--out",
            "target/real-schema-smoke",
        ])
        .output()
        .expect("codex should launch");

    assert!(output.status.success());
}
