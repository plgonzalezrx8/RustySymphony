use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use tokio::{
    net::TcpListener,
    sync::{RwLock, mpsc, oneshot},
};
use tracing::info;

use crate::{
    error::{Result, SymphonyError},
    types::{IssueDebugSnapshot, RuntimeSnapshot},
};

/// One-shot refresh request sent from the HTTP layer to the orchestrator.
pub type RefreshRequest = oneshot::Sender<bool>;

/// Shared read-only snapshot store used by the HTTP API and dashboard.
#[derive(Clone, Default)]
pub struct SnapshotStore {
    snapshot: Arc<RwLock<Option<RuntimeSnapshot>>>,
    issues: Arc<RwLock<BTreeMap<String, IssueDebugSnapshot>>>,
}

/// Router state used by axum handlers.
#[derive(Clone)]
struct HttpState {
    store: SnapshotStore,
    refresh_tx: mpsc::UnboundedSender<RefreshRequest>,
}

impl SnapshotStore {
    /// Create an empty snapshot store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a new runtime snapshot and per-issue debug map.
    pub async fn publish(
        &self,
        snapshot: RuntimeSnapshot,
        issues: BTreeMap<String, IssueDebugSnapshot>,
    ) {
        *self.snapshot.write().await = Some(snapshot);
        *self.issues.write().await = issues;
    }

    /// Return the latest runtime snapshot if one has been published.
    pub async fn snapshot(&self) -> Option<RuntimeSnapshot> {
        self.snapshot.read().await.clone()
    }

    /// Return one issue-specific debug snapshot by identifier.
    pub async fn issue(&self, issue_identifier: &str) -> Option<IssueDebugSnapshot> {
        self.issues.read().await.get(issue_identifier).cloned()
    }
}

/// Start the loopback HTTP server for the dashboard and JSON API.
pub async fn serve_http(
    port: u16,
    store: SnapshotStore,
    refresh_tx: mpsc::UnboundedSender<RefreshRequest>,
) -> Result<()> {
    let router = build_app_router(store, refresh_tx);

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let address = listener.local_addr()?;
    info!(address = %address, "http server listening");

    axum::serve(listener, router)
        .await
        .map_err(|error| SymphonyError::InvalidConfig(error.to_string()))
}

/// Build the axum router used by the dashboard and API.
///
/// This is public so integration tests can exercise the HTTP surface without
/// needing to bind a real TCP listener.
pub fn build_app_router(
    store: SnapshotStore,
    refresh_tx: mpsc::UnboundedSender<RefreshRequest>,
) -> Router {
    let state = HttpState { store, refresh_tx };
    Router::new()
        .route("/", get(render_dashboard))
        .route("/api/v1/state", get(get_state))
        .route("/api/v1/refresh", post(post_refresh))
        .route("/api/v1/{issue_identifier}", get(get_issue))
        .with_state(state)
}

async fn render_dashboard(State(state): State<HttpState>) -> impl IntoResponse {
    match state.store.snapshot().await {
        Some(snapshot) => Html(render_snapshot_html(&snapshot)),
        None => Html("<html><body><h1>Symphony</h1><p>No state yet.</p></body></html>".into()),
    }
}

async fn get_state(State(state): State<HttpState>) -> impl IntoResponse {
    match state.store.snapshot().await {
        Some(snapshot) => (StatusCode::OK, Json(snapshot)).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body(
                "unavailable",
                "runtime snapshot is not available yet",
            )),
        )
            .into_response(),
    }
}

async fn get_issue(
    State(state): State<HttpState>,
    Path(issue_identifier): Path<String>,
) -> impl IntoResponse {
    match state.store.issue(&issue_identifier).await {
        Some(snapshot) => (StatusCode::OK, Json(snapshot)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body(
                "issue_not_found",
                &format!("issue {} is not tracked in memory", issue_identifier),
            )),
        )
            .into_response(),
    }
}

async fn post_refresh(State(state): State<HttpState>) -> impl IntoResponse {
    let (reply_tx, reply_rx) = oneshot::channel();
    if state.refresh_tx.send(reply_tx).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body("unavailable", "refresh channel is closed")),
        )
            .into_response();
    }

    let coalesced = reply_rx.await.unwrap_or(true);
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "queued": true,
            "coalesced": coalesced,
            "requested_at": chrono::Utc::now(),
            "operations": ["poll", "reconcile"],
        })),
    )
        .into_response()
}

fn render_snapshot_html(snapshot: &RuntimeSnapshot) -> String {
    let mut running_rows = String::new();
    for row in &snapshot.running {
        running_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            row.issue_identifier,
            row.state,
            row.turn_count,
            row.last_event.clone().unwrap_or_default(),
            row.last_message
        ));
    }

    let mut retry_rows = String::new();
    for row in &snapshot.retrying {
        retry_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            row.identifier,
            row.attempt,
            row.due_at,
            row.error.clone().unwrap_or_default()
        ));
    }

    format!(
        "<html><body><h1>Symphony</h1><p>Generated at {generated_at}</p>\
         <h2>Running ({running_count})</h2><table border=\"1\"><tr><th>Issue</th><th>State</th>\
         <th>Turns</th><th>Last Event</th><th>Message</th></tr>{running_rows}</table>\
         <h2>Retrying ({retry_count})</h2><table border=\"1\"><tr><th>Issue</th><th>Attempt</th>\
         <th>Due</th><th>Error</th></tr>{retry_rows}</table>\
         <h2>Totals</h2><p>Input: {input} Output: {output} Total: {total} Runtime Seconds: \
         {seconds:.1}</p></body></html>",
        generated_at = snapshot.generated_at,
        running_count = snapshot.counts.running,
        retry_count = snapshot.counts.retrying,
        running_rows = running_rows,
        retry_rows = retry_rows,
        input = snapshot.codex_totals.input_tokens,
        output = snapshot.codex_totals.output_tokens,
        total = snapshot.codex_totals.total_tokens,
        seconds = snapshot.codex_totals.seconds_running,
    )
}

fn error_body(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "code": code,
            "message": message,
        }
    })
}

#[allow(dead_code)]
fn _socket_to_string(address: SocketAddr) -> String {
    address.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AttemptDebugInfo, CodexTotals, IssueDebugSnapshot, LogDebugInfo, SnapshotCounts,
        WorkspaceDebugInfo,
    };
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn state_endpoint_returns_service_unavailable_without_snapshot() {
        let (refresh_tx, _refresh_rx) = mpsc::unbounded_channel();
        let app = build_app_router(SnapshotStore::new(), refresh_tx);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn refresh_endpoint_queues_refresh() {
        let store = SnapshotStore::new();
        let (refresh_tx, mut refresh_rx) = mpsc::unbounded_channel();
        let app = build_app_router(store, refresh_tx);

        tokio::spawn(async move {
            if let Some(reply) = refresh_rx.recv().await {
                let _ = reply.send(false);
            }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn dashboard_and_issue_routes_render_published_state() {
        let store = SnapshotStore::new();
        store
            .publish(
                RuntimeSnapshot {
                    generated_at: chrono::Utc::now(),
                    counts: SnapshotCounts {
                        running: 1,
                        retrying: 0,
                    },
                    running: vec![],
                    retrying: vec![],
                    codex_totals: CodexTotals::default(),
                    rate_limits: None,
                },
                [(
                    String::from("ABC-1"),
                    IssueDebugSnapshot {
                        issue_identifier: String::from("ABC-1"),
                        issue_id: String::from("1"),
                        status: String::from("running"),
                        workspace: WorkspaceDebugInfo {
                            path: "/tmp/symphony/ABC-1".into(),
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

        let (refresh_tx, _refresh_rx) = mpsc::unbounded_channel();
        let app = build_app_router(store, refresh_tx);

        let dashboard = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(dashboard.status(), StatusCode::OK);

        let missing_issue = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ABC-404")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_issue.status(), StatusCode::NOT_FOUND);

        let wrong_method = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn refresh_endpoint_reports_closed_channels() {
        let store = SnapshotStore::new();
        let (refresh_tx, refresh_rx) = mpsc::unbounded_channel();
        drop(refresh_rx);
        let app = build_app_router(store, refresh_tx);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
