mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use chrono::Utc;
use tower::ServiceExt;

use symphony::{
    http::{SnapshotStore, build_app_router},
    types::{
        AttemptDebugInfo, CodexTotals, IssueDebugSnapshot, LogDebugInfo, RetryEntry,
        RuntimeRunningRow, RuntimeSnapshot, SnapshotCounts, TokenUsage, WorkspaceDebugInfo,
    },
};

#[tokio::test]
async fn state_and_issue_endpoints_return_published_snapshots() {
    let store = SnapshotStore::new();
    let (refresh_tx, _refresh_rx) = tokio::sync::mpsc::unbounded_channel();

    store
        .publish(
            RuntimeSnapshot {
                generated_at: Utc::now(),
                counts: SnapshotCounts {
                    running: 1,
                    retrying: 0,
                },
                running: vec![RuntimeRunningRow {
                    issue_id: "issue-1".into(),
                    issue_identifier: "ABC-1".into(),
                    state: "In Progress".into(),
                    session_id: Some("thread-1-turn-1".into()),
                    turn_count: 1,
                    last_event: Some("turn_completed".into()),
                    last_message: "done".into(),
                    started_at: Utc::now(),
                    last_event_at: Some(Utc::now()),
                    tokens: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 2,
                        total_tokens: 3,
                    },
                }],
                retrying: vec![RetryEntry {
                    issue_id: "issue-2".into(),
                    identifier: "ABC-2".into(),
                    attempt: 2,
                    due_at: Utc::now(),
                    error: Some("retry".into()),
                }],
                codex_totals: CodexTotals {
                    input_tokens: 1,
                    output_tokens: 2,
                    total_tokens: 3,
                    seconds_running: 4.0,
                },
                rate_limits: None,
            },
            [(
                String::from("ABC-1"),
                IssueDebugSnapshot {
                    issue_identifier: "ABC-1".into(),
                    issue_id: "issue-1".into(),
                    status: "running".into(),
                    workspace: WorkspaceDebugInfo {
                        path: std::path::PathBuf::from("/tmp/work"),
                    },
                    attempts: AttemptDebugInfo {
                        restart_count: 0,
                        current_retry_attempt: None,
                    },
                    running: None,
                    retry: None,
                    logs: LogDebugInfo::default(),
                    recent_events: Vec::new(),
                    last_error: None,
                    tracked: Default::default(),
                },
            )]
            .into_iter()
            .collect(),
        )
        .await;

    let app = build_app_router(store, refresh_tx);

    let state_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("state request should succeed");
    assert_eq!(state_response.status(), StatusCode::OK);

    let issue_response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/ABC-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("issue request should succeed");
    assert_eq!(issue_response.status(), StatusCode::OK);
}
