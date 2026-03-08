#![allow(dead_code)]
// Integration tests compile as separate crates, so shared helpers can be unused in
// any one target even though they are reused across the full suite.

use std::{collections::BTreeMap, path::PathBuf};

use axum::Router;
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

use symphony::{
    types::{
        AgentSettings, CodexSettings, EffectiveConfig, HookSettings, Issue, PollingSettings,
        ServerSettings, TrackerSettings, WorkspaceSettings,
    },
    workflow::{PromptRenderer, WorkflowRuntime},
};

/// Build a minimal issue used by integration tests.
pub fn sample_issue(state: &str) -> Issue {
    Issue {
        id: "issue-1".into(),
        identifier: "ABC-1".into(),
        title: "Sample issue".into(),
        state: state.into(),
        description: Some("Example".into()),
        ..Issue::default()
    }
}

/// Build a reusable test config with sane defaults for the runtime.
pub fn test_config(
    endpoint: String,
    workspace_root: PathBuf,
    codex_command: String,
) -> EffectiveConfig {
    EffectiveConfig {
        workflow_path: PathBuf::from("WORKFLOW.md"),
        prompt_template: "Work on {{ issue.identifier }}".into(),
        tracker: TrackerSettings {
            kind: "linear".into(),
            endpoint,
            api_key: "test-token".into(),
            project_slug: "test-project".into(),
            active_states: vec!["Todo".into(), "In Progress".into()],
            terminal_states: vec!["Done".into(), "Cancelled".into()],
        },
        polling: PollingSettings { interval_ms: 100 },
        workspace: WorkspaceSettings {
            root: workspace_root,
        },
        hooks: HookSettings {
            timeout_ms: 250,
            ..HookSettings::default()
        },
        agent: AgentSettings {
            max_concurrent_agents: 2,
            max_turns: 2,
            max_retry_backoff_ms: 30_000,
            max_concurrent_agents_by_state: BTreeMap::new(),
        },
        codex: CodexSettings {
            command: codex_command,
            approval_policy: serde_json::json!("never"),
            thread_sandbox: serde_json::json!("danger-full-access"),
            turn_sandbox_policy: serde_json::json!({ "type": "dangerFullAccess" }),
            turn_timeout_ms: 400,
            read_timeout_ms: 400,
            stall_timeout_ms: 2_000,
        },
        server: ServerSettings::default(),
    }
}

/// Compile a runtime wrapper from a test config.
pub fn test_workflow_runtime(config: EffectiveConfig) -> WorkflowRuntime {
    WorkflowRuntime {
        config,
        prompt: std::sync::Arc::new(
            PromptRenderer::compile("Work on {{ issue.identifier }}")
                .expect("template should compile"),
        ),
    }
}

/// Start a temporary axum server and return its base URL plus join handle.
pub async fn spawn_test_server(router: Router) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should expose address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test server should stay alive");
    });
    (format!("http://{}", address), handle)
}

/// Create a tempdir and return both the handle and its workspace root path.
pub fn temp_workspace() -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("tempdir should be created");
    let workspace_root = directory.path().join("workspaces");
    (directory, workspace_root)
}
