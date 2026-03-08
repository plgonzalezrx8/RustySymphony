mod common;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::{
    sync::mpsc,
    time::{Duration, timeout},
};

use symphony::{
    agent::{
        SessionUpdate, WorkerEvent, WorkerExit, WorkerOutcome, WorkerStartContext, spawn_worker,
    },
    error::Result,
    tracker::IssueTracker,
    types::Issue,
    workspace::WorkspaceManager,
};

use common::{sample_issue, temp_workspace, test_config, test_workflow_runtime};

#[derive(Clone)]
struct MockTracker {
    issue: Arc<Mutex<Issue>>,
    graphql_response: serde_json::Value,
}

#[async_trait]
impl IssueTracker for MockTracker {
    async fn fetch_candidate_issues(
        &self,
        _config: &symphony::types::EffectiveConfig,
    ) -> Result<Vec<Issue>> {
        Ok(vec![self.issue.lock().expect("issue lock").clone()])
    }

    async fn fetch_issues_by_states(
        &self,
        _config: &symphony::types::EffectiveConfig,
        _state_names: &[String],
    ) -> Result<Vec<Issue>> {
        Ok(vec![self.issue.lock().expect("issue lock").clone()])
    }

    async fn fetch_issue_states_by_ids(
        &self,
        _config: &symphony::types::EffectiveConfig,
        _issue_ids: &[String],
    ) -> Result<Vec<Issue>> {
        Ok(vec![self.issue.lock().expect("issue lock").clone()])
    }

    async fn execute_raw_graphql(
        &self,
        _config: &symphony::types::EffectiveConfig,
        _query: &str,
        _variables: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        Ok(self.graphql_response.clone())
    }
}

#[tokio::test]
async fn worker_auto_approves_and_handles_linear_graphql() {
    let script = write_fake_codex_script("success");
    let (_tempdir, workspace_root) = temp_workspace();
    let issue = sample_issue("Todo");
    let tracker = Arc::new(MockTracker {
        issue: Arc::new(Mutex::new(Issue {
            state: "Done".into(),
            ..issue.clone()
        })),
        graphql_response: serde_json::json!({ "data": { "viewer": { "id": "viewer-1" } } }),
    });
    let config = test_config(
        "http://example.com".into(),
        workspace_root,
        format!("python3 -u '{}'", script.display()),
    );
    let workflow = test_workflow_runtime(config);
    let (tx, rx) = mpsc::unbounded_channel();

    let worker = spawn_worker(
        WorkerStartContext {
            issue,
            attempt: None,
            workflow,
            tracker,
            workspace_manager: WorkspaceManager,
        },
        tx,
    );

    let (updates, outcome) = collect_worker_result(rx).await;
    worker.join.await.expect("worker join should complete");

    assert_eq!(outcome.exit, WorkerExit::Succeeded);
    assert_eq!(outcome.turn_count, 1);
    assert!(
        updates
            .iter()
            .any(|update| update.event == "approval_auto_approved")
    );
    assert!(
        updates
            .iter()
            .any(|update| update.event == "turn_completed")
    );
}

#[tokio::test]
async fn worker_fails_when_user_input_is_requested() {
    let script = write_fake_codex_script("user_input");
    let (_tempdir, workspace_root) = temp_workspace();
    let issue = sample_issue("Todo");
    let tracker = Arc::new(MockTracker {
        issue: Arc::new(Mutex::new(issue.clone())),
        graphql_response: serde_json::json!({}),
    });
    let mut config = test_config(
        "http://example.com".into(),
        workspace_root,
        format!("python3 -u '{}'", script.display()),
    );
    config.codex.turn_timeout_ms = 1_000;
    let workflow = test_workflow_runtime(config);
    let (tx, rx) = mpsc::unbounded_channel();

    let worker = spawn_worker(
        WorkerStartContext {
            issue,
            attempt: None,
            workflow,
            tracker,
            workspace_manager: WorkspaceManager,
        },
        tx,
    );

    let (updates, outcome) = collect_worker_result(rx).await;
    worker.join.await.expect("worker join should complete");

    assert_eq!(outcome.exit, WorkerExit::Failed);
    assert!(
        updates
            .iter()
            .any(|update| update.event == "turn_input_required")
    );
}

#[tokio::test]
async fn worker_times_out_when_turn_never_completes() {
    let script = write_fake_codex_script("timeout");
    let (_tempdir, workspace_root) = temp_workspace();
    let issue = sample_issue("Todo");
    let tracker = Arc::new(MockTracker {
        issue: Arc::new(Mutex::new(issue.clone())),
        graphql_response: serde_json::json!({}),
    });
    let mut config = test_config(
        "http://example.com".into(),
        workspace_root,
        format!("python3 -u '{}'", script.display()),
    );
    config.codex.turn_timeout_ms = 150;
    let workflow = test_workflow_runtime(config);
    let (tx, rx) = mpsc::unbounded_channel();

    let worker = spawn_worker(
        WorkerStartContext {
            issue,
            attempt: None,
            workflow,
            tracker,
            workspace_manager: WorkspaceManager,
        },
        tx,
    );

    let (_updates, outcome) = collect_worker_result(rx).await;
    worker.join.await.expect("worker join should complete");

    assert_eq!(outcome.exit, WorkerExit::TimedOut);
}

async fn collect_worker_result(
    mut rx: mpsc::UnboundedReceiver<WorkerEvent>,
) -> (Vec<SessionUpdate>, WorkerOutcome) {
    let mut updates = Vec::new();
    let outcome = timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await.expect("worker event should arrive") {
                WorkerEvent::Update(update) => updates.push(update),
                WorkerEvent::Finished(outcome) => break outcome,
            }
        }
    })
    .await
    .expect("worker should finish");

    (updates, outcome)
}

fn write_fake_codex_script(scenario: &str) -> PathBuf {
    let directory = tempfile::tempdir().expect("tempdir should be created");
    let path = directory.path().join(format!("{scenario}.py"));

    std::fs::write(
        &path,
        format!(
            r#"import json
import sys
import time

def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(1)
    return json.loads(line)

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

scenario = {scenario:?}

initialize = read()
write({{"id": initialize["id"], "result": {{"ok": True}}}})
initialized = read()
thread_start = read()
write({{"id": thread_start["id"], "result": {{"thread": {{"id": "thread-1"}}}}}})
turn_start = read()
write({{"id": turn_start["id"], "result": {{"turn": {{"id": "turn-1"}}}}}})

if scenario == "success":
    write({{
        "id": 900,
        "method": "item/commandExecution/requestApproval",
        "params": {{"command": ["echo", "hi"], "cwd": ".", "callId": "cmd-1", "parsedCmd": []}}
    }})
    approval = read()
    assert approval["result"]["decision"] == "acceptForSession"
    write({{
        "id": 901,
        "method": "item/tool/call",
        "params": {{
            "tool": "linear_graphql",
            "callId": "tool-1",
            "threadId": "thread-1",
            "turnId": "turn-1",
            "arguments": {{"query": "query One {{ viewer {{ id }} }}"}}
        }}
    }})
    tool_result = read()
    assert tool_result["result"]["success"] is True
    assert "viewer" in tool_result["result"]["contentItems"][0]["text"]
    write({{"method": "thread/tokenUsage/updated", "params": {{"total_token_usage": {{"input_tokens": 3, "output_tokens": 4, "total_tokens": 7}}}}}})
    write({{"method": "turn/completed", "params": {{"message": "done"}}}})
elif scenario == "user_input":
    write({{
        "id": 902,
        "method": "item/tool/requestUserInput",
        "params": {{"questions": []}}
    }})
    response = read()
    assert response["error"]["message"] == "turn_input_required"
    time.sleep(0.1)
elif scenario == "timeout":
    time.sleep(1.0)
else:
    sys.exit(1)
"#
        ),
    )
    .expect("fake codex script should be written");

    // Keep the temporary directory alive for the lifetime of the test process so
    // the worker can execute the generated script after this helper returns.
    std::mem::forget(directory);
    path
}
