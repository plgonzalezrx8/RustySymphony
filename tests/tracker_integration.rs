mod common;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use symphony::{
    error::SymphonyError,
    tracker::{IssueTracker, LinearTracker},
};

use common::{spawn_test_server, temp_workspace, test_config};

#[derive(Clone, Default)]
struct TrackerServerState {
    calls: Arc<AtomicUsize>,
}

#[tokio::test]
async fn candidate_fetch_paginates_and_normalizes() {
    let state = TrackerServerState::default();
    let router = Router::new()
        .route("/graphql", post(candidate_handler))
        .with_state(state.clone());
    let (endpoint, server_handle) = spawn_test_server(router).await;
    let (_tempdir, workspace_root) = temp_workspace();

    let tracker = LinearTracker::new().expect("tracker should build");
    let config = test_config(
        endpoint + "/graphql",
        workspace_root,
        "codex app-server".into(),
    );
    let issues = tracker
        .fetch_candidate_issues(&config)
        .await
        .expect("candidate fetch should succeed");

    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].labels, vec!["bug"]);
    assert_eq!(issues[0].blocked_by[0].identifier.as_deref(), Some("ABC-2"));

    server_handle.abort();
}

#[tokio::test]
async fn empty_state_list_returns_without_http_call() {
    let state = TrackerServerState::default();
    let router = Router::new()
        .route("/graphql", post(candidate_handler))
        .with_state(state.clone());
    let (endpoint, server_handle) = spawn_test_server(router).await;
    let (_tempdir, workspace_root) = temp_workspace();

    let tracker = LinearTracker::new().expect("tracker should build");
    let config = test_config(
        endpoint + "/graphql",
        workspace_root,
        "codex app-server".into(),
    );
    let issues = tracker
        .fetch_issues_by_states(&config, &[])
        .await
        .expect("empty state fetch should succeed");

    assert!(issues.is_empty());
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);

    server_handle.abort();
}

#[tokio::test]
async fn issue_state_fetch_maps_graphql_errors() {
    let router = Router::new().route(
        "/graphql",
        post(|| async {
            Json(json!({
                "errors": [{ "message": "boom" }]
            }))
        }),
    );
    let (endpoint, server_handle) = spawn_test_server(router).await;
    let (_tempdir, workspace_root) = temp_workspace();

    let tracker = LinearTracker::new().expect("tracker should build");
    let config = test_config(
        endpoint + "/graphql",
        workspace_root,
        "codex app-server".into(),
    );
    let error = tracker
        .fetch_issue_states_by_ids(&config, &[String::from("issue-1")])
        .await
        .expect_err("GraphQL errors should be surfaced");

    assert!(matches!(error, SymphonyError::LinearGraphqlErrors(_)));

    server_handle.abort();
}

async fn candidate_handler(
    State(state): State<TrackerServerState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    state.calls.fetch_add(1, Ordering::SeqCst);

    let after = payload
        .pointer("/variables/after")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let response = if after.is_none() {
        json!({
            "data": {
                "issues": {
                    "nodes": [{
                        "id": "issue-1",
                        "identifier": "ABC-1",
                        "title": "First",
                        "description": "desc",
                        "priority": 1,
                        "branchName": "feature/one",
                        "url": "https://example.com/ABC-1",
                        "createdAt": "2026-03-08T00:00:00Z",
                        "updatedAt": "2026-03-08T00:00:00Z",
                        "state": { "name": "Todo" },
                        "labels": { "nodes": [{ "name": "Bug" }] },
                        "inverseRelations": {
                            "nodes": [{
                                "type": "blocks",
                                "issue": {
                                    "id": "issue-2",
                                    "identifier": "ABC-2",
                                    "state": { "name": "In Progress" }
                                }
                            }]
                        }
                    }],
                    "pageInfo": {
                        "hasNextPage": true,
                        "endCursor": "cursor-1"
                    }
                }
            }
        })
    } else {
        json!({
            "data": {
                "issues": {
                    "nodes": [{
                        "id": "issue-3",
                        "identifier": "ABC-3",
                        "title": "Second",
                        "description": null,
                        "priority": 2,
                        "branchName": null,
                        "url": null,
                        "createdAt": "2026-03-07T00:00:00Z",
                        "updatedAt": "2026-03-07T00:00:00Z",
                        "state": { "name": "In Progress" },
                        "labels": { "nodes": [] },
                        "inverseRelations": { "nodes": [] }
                    }],
                    "pageInfo": {
                        "hasNextPage": false,
                        "endCursor": null
                    }
                }
            }
        })
    };

    (StatusCode::OK, Json(response))
}
