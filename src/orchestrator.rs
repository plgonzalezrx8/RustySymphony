use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Duration, Instant, sleep_until},
};
use tracing::{error, info, warn};

use crate::{
    agent::{
        SessionUpdate, WorkerEvent, WorkerExit, WorkerHandle, WorkerStartContext, spawn_worker,
    },
    error::Result,
    http::{RefreshRequest, SnapshotStore, serve_http},
    tracker::{IssueTracker, build_tracker},
    types::{
        AttemptDebugInfo, CodexTotals, EffectiveConfig, Issue, IssueDebugSnapshot,
        IssueRunningDebugInfo, LiveSession, LogDebugInfo, LogReference, RecentEvent, RetryEntry,
        RunningEntry, RuntimeRunningRow, RuntimeSnapshot, SnapshotCounts, TokenUsage,
        WorkspaceDebugInfo,
    },
    workflow::{
        WorkflowRuntime, load_workflow_runtime, start_workflow_watch, validate_dispatch_config,
    },
    workspace::WorkspaceManager,
};

const RECENT_EVENT_LIMIT: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Terminal,
    NonActive,
    Stall,
}

struct RetryRuntimeEntry {
    entry: RetryEntry,
    handle: JoinHandle<()>,
}

struct RunningRuntime {
    entry: RunningEntry,
    worker: WorkerHandle,
    stop_reason: Option<StopReason>,
}

struct OrchestratorState {
    workflow: WorkflowRuntime,
    tracker: Arc<dyn IssueTracker>,
    workspace_manager: WorkspaceManager,
    snapshot_store: SnapshotStore,
    worker_tx: mpsc::UnboundedSender<WorkerEvent>,
    retry_tx: mpsc::UnboundedSender<String>,
    running: HashMap<String, RunningRuntime>,
    claimed: HashSet<String>,
    retry_attempts: HashMap<String, RetryRuntimeEntry>,
    completed: HashSet<String>,
    codex_totals: CodexTotals,
    rate_limits: Option<Value>,
    refresh_queued: bool,
}

/// Start the Symphony service and run until shutdown.
pub async fn run_service(workflow_path: PathBuf, cli_port: Option<u16>) -> Result<()> {
    let workflow = load_workflow_runtime(&workflow_path)?;
    let tracker = build_tracker(&workflow.config)?;
    let workspace_manager = WorkspaceManager;
    let snapshot_store = SnapshotStore::new();

    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel();
    let (retry_tx, mut retry_rx) = mpsc::unbounded_channel();
    let (refresh_tx, mut refresh_rx) = mpsc::unbounded_channel::<RefreshRequest>();
    let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<()>();

    let _watcher = start_workflow_watch(workflow_path.clone(), watch_tx)?;

    let http_port = cli_port.or(workflow.config.server.port);
    if let Some(port) = http_port {
        let store = snapshot_store.clone();
        let refresh_tx = refresh_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_http(port, store, refresh_tx).await {
                error!(error = %error, "http server exited");
            }
        });
    }

    let mut state = OrchestratorState {
        workflow,
        tracker,
        workspace_manager,
        snapshot_store,
        worker_tx,
        retry_tx,
        running: HashMap::new(),
        claimed: HashSet::new(),
        retry_attempts: HashMap::new(),
        completed: HashSet::new(),
        codex_totals: CodexTotals::default(),
        rate_limits: None,
        refresh_queued: false,
    };

    validate_dispatch_config(&state.workflow.config)?;
    startup_terminal_workspace_cleanup(&state).await;
    publish_snapshot(&state).await;

    let mut next_tick = Instant::now();
    loop {
        let sleep = sleep_until(next_tick);
        tokio::pin!(sleep);

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received shutdown signal");
                shutdown_running_workers(&mut state).await;
                publish_snapshot(&state).await;
                break;
            }
            _ = &mut sleep => {
                run_tick(&mut state).await;
                state.refresh_queued = false;
                next_tick = Instant::now() + Duration::from_millis(state.workflow.config.polling.interval_ms);
            }
            Some(event) = worker_rx.recv() => {
                handle_worker_event(&mut state, event).await;
                publish_snapshot(&state).await;
            }
            Some(issue_id) = retry_rx.recv() => {
                handle_retry_due(&mut state, &issue_id).await;
                publish_snapshot(&state).await;
            }
            Some(reply) = refresh_rx.recv() => {
                let coalesced = state.refresh_queued;
                state.refresh_queued = true;
                let _ = reply.send(coalesced);
                next_tick = Instant::now();
            }
            Some(()) = watch_rx.recv() => {
                reload_workflow(&mut state, &workflow_path).await;
                next_tick = Instant::now();
                publish_snapshot(&state).await;
            }
        }
    }

    Ok(())
}

async fn run_tick(state: &mut OrchestratorState) {
    reconcile_running_issues(state).await;

    if let Err(error) = validate_dispatch_config(&state.workflow.config) {
        error!(error = %error, "dispatch validation failed");
        return;
    }

    let mut issues = match state
        .tracker
        .fetch_candidate_issues(&state.workflow.config)
        .await
    {
        Ok(issues) => issues,
        Err(error) => {
            error!(error = %error, "failed to fetch candidate issues");
            return;
        }
    };

    sort_issues_for_dispatch(&mut issues);
    for issue in issues {
        if available_global_slots(state) == 0 {
            break;
        }
        if should_dispatch(state, &issue, None) {
            dispatch_issue(state, issue, None).await;
        }
    }
}

async fn reconcile_running_issues(state: &mut OrchestratorState) {
    reconcile_stalled_runs(state).await;

    let running_ids: Vec<String> = state.running.keys().cloned().collect();
    if running_ids.is_empty() {
        return;
    }

    let refreshed = match state
        .tracker
        .fetch_issue_states_by_ids(&state.workflow.config, &running_ids)
        .await
    {
        Ok(issues) => issues,
        Err(error) => {
            warn!(error = %error, "failed to refresh running issue states");
            return;
        }
    };

    let refreshed_by_id: HashMap<String, Issue> = refreshed
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    for issue_id in running_ids {
        if let Some(issue) = refreshed_by_id.get(&issue_id) {
            if state.workflow.config.is_terminal_state(&issue.state) {
                cancel_running_issue(state, &issue_id, StopReason::Terminal);
            } else if state.workflow.config.is_active_state(&issue.state) {
                if let Some(running) = state.running.get_mut(&issue_id) {
                    running.entry.issue = issue.clone();
                }
            } else {
                cancel_running_issue(state, &issue_id, StopReason::NonActive);
            }
        }
    }
}

async fn reconcile_stalled_runs(state: &mut OrchestratorState) {
    if state.workflow.config.codex.stall_timeout_ms <= 0 {
        return;
    }

    let timeout_ms = state.workflow.config.codex.stall_timeout_ms;
    let now = Utc::now();
    let mut stalled = Vec::new();
    for (issue_id, running) in &state.running {
        let last_activity = running
            .entry
            .session
            .last_codex_timestamp
            .unwrap_or(running.entry.started_at);
        let elapsed = now.signed_duration_since(last_activity).num_milliseconds();
        if elapsed > timeout_ms {
            stalled.push(issue_id.clone());
        }
    }

    for issue_id in stalled {
        cancel_running_issue(state, &issue_id, StopReason::Stall);
    }
}

async fn dispatch_issue(state: &mut OrchestratorState, issue: Issue, attempt: Option<u32>) {
    if let Some(retry) = state.retry_attempts.remove(&issue.id) {
        retry.handle.abort();
    }

    let context = WorkerStartContext {
        issue: issue.clone(),
        attempt,
        workflow: state.workflow.clone(),
        tracker: state.tracker.clone(),
        workspace_manager: state.workspace_manager.clone(),
    };
    let worker = spawn_worker(context, state.worker_tx.clone());
    let workspace_key = crate::workspace::sanitize_issue_identifier(&issue.identifier);
    let workspace_path = state.workflow.config.workspace.root.join(&workspace_key);
    let log_path = state
        .workspace_manager
        .session_log_path(&state.workflow.config.workspace.root, &workspace_key)
        .ok();

    state.claimed.insert(issue.id.clone());
    state.running.insert(
        issue.id.clone(),
        RunningRuntime {
            entry: RunningEntry {
                issue_id: issue.id.clone(),
                issue_identifier: issue.identifier.clone(),
                issue,
                workspace_path,
                retry_attempt: attempt,
                started_at: Utc::now(),
                session: LiveSession::default(),
                recent_events: Vec::new(),
                last_error: None,
                log_path,
            },
            worker,
            stop_reason: None,
        },
    );
}

async fn handle_worker_event(state: &mut OrchestratorState, event: WorkerEvent) {
    match event {
        WorkerEvent::Update(update) => apply_session_update(state, update),
        WorkerEvent::Finished(outcome) => handle_worker_finished(state, outcome).await,
    }
}

fn apply_session_update(state: &mut OrchestratorState, update: SessionUpdate) {
    let Some(running) = state.running.get_mut(&update.issue_id) else {
        return;
    };

    if let Some(message) = &update.message {
        running.entry.session.last_codex_message = Some(message.clone());
    }
    running.entry.session.last_codex_event = Some(update.event.clone());
    running.entry.session.last_codex_timestamp = Some(update.at);
    running.entry.session.codex_app_server_pid = update.pid.clone();
    running.entry.session.thread_id = update.thread_id.clone();
    running.entry.session.turn_id = update.turn_id.clone();
    running.entry.session.session_id = update.session_id.clone();

    if let Some(turn_count) = update.turn_count {
        running.entry.session.turn_count = turn_count;
    }

    if let Some(tokens) = update.absolute_tokens {
        apply_absolute_tokens(&mut running.entry.session, &mut state.codex_totals, &tokens);
    }

    if let Some(rate_limits) = update.rate_limits {
        state.rate_limits = Some(rate_limits);
    }

    let event_message = update.message.unwrap_or_default();
    running.entry.recent_events.push(RecentEvent {
        at: update.at,
        event: update.event.clone(),
        message: event_message.clone(),
    });
    if running.entry.recent_events.len() > RECENT_EVENT_LIMIT {
        let drain = running.entry.recent_events.len() - RECENT_EVENT_LIMIT;
        running.entry.recent_events.drain(0..drain);
    }
    running.entry.last_error = if matches!(
        update.event.as_str(),
        "turn_failed" | "turn_cancelled" | "turn_input_required" | "startup_failed"
    ) {
        Some(event_message)
    } else {
        running.entry.last_error.clone()
    };
}

async fn handle_worker_finished(
    state: &mut OrchestratorState,
    outcome: crate::agent::WorkerOutcome,
) {
    let Some(running) = state.running.remove(&outcome.issue.id) else {
        return;
    };

    add_runtime_seconds(&mut state.codex_totals, running.entry.started_at);
    let next_attempt = running
        .entry
        .retry_attempt
        .map(|attempt| attempt + 1)
        .unwrap_or(1);

    match running.stop_reason {
        Some(StopReason::Terminal) => {
            let _ = state
                .workspace_manager
                .cleanup_workspace(&outcome.issue.identifier, &state.workflow.config)
                .await;
            release_claim(state, &outcome.issue.id);
        }
        Some(StopReason::NonActive) => {
            release_claim(state, &outcome.issue.id);
        }
        Some(StopReason::Stall) => {
            schedule_retry(
                state,
                &outcome.issue.id,
                &outcome.issue.identifier,
                next_attempt,
                Some("stalled session".into()),
                retry_delay_ms(next_attempt, state),
            );
        }
        None => match outcome.exit {
            WorkerExit::Succeeded => {
                state.completed.insert(outcome.issue.id.clone());
                schedule_retry(
                    state,
                    &outcome.issue.id,
                    &outcome.issue.identifier,
                    1,
                    None,
                    1_000,
                );
            }
            WorkerExit::TimedOut => {
                schedule_retry(
                    state,
                    &outcome.issue.id,
                    &outcome.issue.identifier,
                    next_attempt,
                    Some("worker turn timeout".into()),
                    retry_delay_ms(next_attempt, state),
                );
            }
            WorkerExit::Canceled => {
                schedule_retry(
                    state,
                    &outcome.issue.id,
                    &outcome.issue.identifier,
                    next_attempt,
                    Some("worker canceled".into()),
                    retry_delay_ms(next_attempt, state),
                );
            }
            WorkerExit::Failed => {
                schedule_retry(
                    state,
                    &outcome.issue.id,
                    &outcome.issue.identifier,
                    next_attempt,
                    outcome.error,
                    retry_delay_ms(next_attempt, state),
                );
            }
        },
    }
}

async fn handle_retry_due(state: &mut OrchestratorState, issue_id: &str) {
    let Some(retry) = state.retry_attempts.remove(issue_id) else {
        return;
    };

    let candidates = match state
        .tracker
        .fetch_candidate_issues(&state.workflow.config)
        .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            schedule_retry(
                state,
                issue_id,
                &retry.entry.identifier,
                retry.entry.attempt + 1,
                Some(format!("retry poll failed: {error}")),
                retry_delay_ms(retry.entry.attempt + 1, state),
            );
            return;
        }
    };

    let Some(issue) = candidates
        .into_iter()
        .find(|candidate| candidate.id == issue_id)
    else {
        release_claim(state, issue_id);
        return;
    };

    if available_global_slots(state) == 0 || !should_dispatch(state, &issue, Some(issue_id)) {
        schedule_retry(
            state,
            &issue.id,
            &issue.identifier,
            retry.entry.attempt + 1,
            Some("no available orchestrator slots".into()),
            retry_delay_ms(retry.entry.attempt + 1, state),
        );
        return;
    }

    dispatch_issue(state, issue, Some(retry.entry.attempt)).await;
}

async fn reload_workflow(state: &mut OrchestratorState, workflow_path: &Path) {
    match load_workflow_runtime(workflow_path) {
        Ok(runtime) => match build_tracker(&runtime.config) {
            Ok(tracker) => {
                state.workflow = runtime;
                state.tracker = tracker;
                info!("reloaded workflow configuration");
            }
            Err(error) => error!(error = %error, "workflow reload failed"),
        },
        Err(error) => error!(error = %error, "workflow reload failed"),
    }
}

async fn startup_terminal_workspace_cleanup(state: &OrchestratorState) {
    match state
        .tracker
        .fetch_issues_by_states(
            &state.workflow.config,
            &state.workflow.config.tracker.terminal_states,
        )
        .await
    {
        Ok(issues) => {
            for issue in issues {
                let _ = state
                    .workspace_manager
                    .cleanup_workspace(&issue.identifier, &state.workflow.config)
                    .await;
            }
        }
        Err(error) => warn!(error = %error, "startup terminal cleanup failed"),
    }
}

async fn shutdown_running_workers(state: &mut OrchestratorState) {
    for running in state.running.values_mut() {
        running.worker.cancel.cancel();
    }
    for retry in state.retry_attempts.values() {
        retry.handle.abort();
    }
}

fn cancel_running_issue(state: &mut OrchestratorState, issue_id: &str, reason: StopReason) {
    if let Some(running) = state.running.get_mut(issue_id) {
        running.stop_reason.get_or_insert(reason);
        running.worker.cancel.cancel();
    }
}

fn release_claim(state: &mut OrchestratorState, issue_id: &str) {
    state.claimed.remove(issue_id);
    if let Some(retry) = state.retry_attempts.remove(issue_id) {
        retry.handle.abort();
    }
}

fn schedule_retry(
    state: &mut OrchestratorState,
    issue_id: &str,
    identifier: &str,
    attempt: u32,
    error: Option<String>,
    delay_ms: u64,
) {
    if let Some(retry) = state.retry_attempts.remove(issue_id) {
        retry.handle.abort();
    }

    state.claimed.insert(issue_id.to_string());
    let due_at = Utc::now() + chrono::Duration::milliseconds(delay_ms as i64);
    let issue_id_owned = issue_id.to_string();
    let retry_tx = state.retry_tx.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let _ = retry_tx.send(issue_id_owned);
    });

    state.retry_attempts.insert(
        issue_id.to_string(),
        RetryRuntimeEntry {
            entry: RetryEntry {
                issue_id: issue_id.to_string(),
                identifier: identifier.to_string(),
                attempt,
                due_at,
                error,
            },
            handle,
        },
    );
}

fn retry_delay_ms(attempt: u32, state: &OrchestratorState) -> u64 {
    let base = 10_000u64.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    base.min(state.workflow.config.agent.max_retry_backoff_ms)
}

fn apply_absolute_tokens(live: &mut LiveSession, totals: &mut CodexTotals, latest: &TokenUsage) {
    let delta_input = latest
        .input_tokens
        .saturating_sub(live.last_reported_input_tokens);
    let delta_output = latest
        .output_tokens
        .saturating_sub(live.last_reported_output_tokens);
    let delta_total = latest
        .total_tokens
        .saturating_sub(live.last_reported_total_tokens);

    live.codex_input_tokens = latest.input_tokens;
    live.codex_output_tokens = latest.output_tokens;
    live.codex_total_tokens = latest.total_tokens;
    live.last_reported_input_tokens = latest.input_tokens;
    live.last_reported_output_tokens = latest.output_tokens;
    live.last_reported_total_tokens = latest.total_tokens;

    totals.input_tokens += delta_input;
    totals.output_tokens += delta_output;
    totals.total_tokens += delta_total;
}

fn add_runtime_seconds(totals: &mut CodexTotals, started_at: DateTime<Utc>) {
    let seconds = Utc::now()
        .signed_duration_since(started_at)
        .num_milliseconds() as f64
        / 1000.0;
    totals.seconds_running += seconds.max(0.0);
}

fn available_global_slots(state: &OrchestratorState) -> usize {
    state
        .workflow
        .config
        .agent
        .max_concurrent_agents
        .saturating_sub(state.running.len())
}

fn state_running_count(state: &OrchestratorState, issue_state: &str) -> usize {
    let normalized = EffectiveConfig::normalize_state(issue_state);
    state
        .running
        .values()
        .filter(|running| {
            EffectiveConfig::normalize_state(&running.entry.issue.state) == normalized
        })
        .count()
}

fn should_dispatch(
    state: &OrchestratorState,
    issue: &Issue,
    allow_claimed_issue_id: Option<&str>,
) -> bool {
    if issue.id.is_empty()
        || issue.identifier.is_empty()
        || issue.title.is_empty()
        || issue.state.is_empty()
    {
        return false;
    }
    if !state.workflow.config.is_active_state(&issue.state)
        || state.workflow.config.is_terminal_state(&issue.state)
    {
        return false;
    }
    if state.running.contains_key(&issue.id) {
        return false;
    }
    if state.claimed.contains(&issue.id) && allow_claimed_issue_id != Some(issue.id.as_str()) {
        return false;
    }
    if available_global_slots(state) == 0 {
        return false;
    }

    let normalized_state = EffectiveConfig::normalize_state(&issue.state);
    let state_limit = state
        .workflow
        .config
        .agent
        .max_concurrent_agents_by_state
        .get(&normalized_state)
        .copied()
        .unwrap_or(state.workflow.config.agent.max_concurrent_agents);
    if state_running_count(state, &issue.state) >= state_limit {
        return false;
    }

    if normalized_state == "todo"
        && issue.blocked_by.iter().any(|blocker| {
            blocker
                .state
                .as_deref()
                .map(|value| !state.workflow.config.is_terminal_state(value))
                .unwrap_or(true)
        })
    {
        return false;
    }

    true
}

fn sort_issues_for_dispatch(issues: &mut [Issue]) {
    issues.sort_by(|left, right| {
        left.priority
            .unwrap_or(i64::MAX)
            .cmp(&right.priority.unwrap_or(i64::MAX))
            .then_with(|| {
                left.created_at
                    .map(|value| value.timestamp_millis())
                    .unwrap_or(i64::MAX)
                    .cmp(
                        &right
                            .created_at
                            .map(|value| value.timestamp_millis())
                            .unwrap_or(i64::MAX),
                    )
            })
            .then_with(|| left.identifier.cmp(&right.identifier))
    });
}

async fn publish_snapshot(state: &OrchestratorState) {
    let snapshot = build_runtime_snapshot(state);
    let details = build_issue_debug_map(state);
    state.snapshot_store.publish(snapshot, details).await;
}

fn build_runtime_snapshot(state: &OrchestratorState) -> RuntimeSnapshot {
    let running_rows = state
        .running
        .values()
        .map(|running| RuntimeRunningRow {
            issue_id: running.entry.issue_id.clone(),
            issue_identifier: running.entry.issue_identifier.clone(),
            state: running.entry.issue.state.clone(),
            session_id: running.entry.session.session_id.clone(),
            turn_count: running.entry.session.turn_count,
            last_event: running.entry.session.last_codex_event.clone(),
            last_message: running
                .entry
                .session
                .last_codex_message
                .clone()
                .unwrap_or_default(),
            started_at: running.entry.started_at,
            last_event_at: running.entry.session.last_codex_timestamp,
            tokens: TokenUsage {
                input_tokens: running.entry.session.codex_input_tokens,
                output_tokens: running.entry.session.codex_output_tokens,
                total_tokens: running.entry.session.codex_total_tokens,
            },
        })
        .collect::<Vec<_>>();

    let retry_rows = state
        .retry_attempts
        .values()
        .map(|retry| retry.entry.clone())
        .collect::<Vec<_>>();

    let active_seconds = state
        .running
        .values()
        .map(|running| {
            Utc::now()
                .signed_duration_since(running.entry.started_at)
                .num_milliseconds() as f64
                / 1000.0
        })
        .sum::<f64>();

    RuntimeSnapshot {
        generated_at: Utc::now(),
        counts: SnapshotCounts {
            running: running_rows.len(),
            retrying: retry_rows.len(),
        },
        running: running_rows,
        retrying: retry_rows,
        codex_totals: CodexTotals {
            seconds_running: state.codex_totals.seconds_running + active_seconds.max(0.0),
            ..state.codex_totals.clone()
        },
        rate_limits: state.rate_limits.clone(),
    }
}

fn build_issue_debug_map(state: &OrchestratorState) -> BTreeMap<String, IssueDebugSnapshot> {
    let mut map = BTreeMap::new();

    for running in state.running.values() {
        let retry = state
            .retry_attempts
            .get(&running.entry.issue_id)
            .map(|retry| retry.entry.clone());
        map.insert(
            running.entry.issue_identifier.clone(),
            IssueDebugSnapshot {
                issue_identifier: running.entry.issue_identifier.clone(),
                issue_id: running.entry.issue_id.clone(),
                status: "running".into(),
                workspace: WorkspaceDebugInfo {
                    path: running.entry.workspace_path.clone(),
                },
                attempts: AttemptDebugInfo {
                    restart_count: running.entry.retry_attempt.unwrap_or(0),
                    current_retry_attempt: retry.as_ref().map(|value| value.attempt),
                },
                running: Some(IssueRunningDebugInfo {
                    session_id: running.entry.session.session_id.clone(),
                    turn_count: running.entry.session.turn_count,
                    state: running.entry.issue.state.clone(),
                    started_at: running.entry.started_at,
                    last_event: running.entry.session.last_codex_event.clone(),
                    last_message: running
                        .entry
                        .session
                        .last_codex_message
                        .clone()
                        .unwrap_or_default(),
                    last_event_at: running.entry.session.last_codex_timestamp,
                    tokens: TokenUsage {
                        input_tokens: running.entry.session.codex_input_tokens,
                        output_tokens: running.entry.session.codex_output_tokens,
                        total_tokens: running.entry.session.codex_total_tokens,
                    },
                }),
                retry,
                logs: LogDebugInfo {
                    codex_session_logs: running
                        .entry
                        .log_path
                        .clone()
                        .map(|path| {
                            vec![LogReference {
                                label: "latest".into(),
                                path,
                                url: None,
                            }]
                        })
                        .unwrap_or_default(),
                },
                recent_events: running.entry.recent_events.clone(),
                last_error: running.entry.last_error.clone(),
                tracked: BTreeMap::new(),
            },
        );
    }

    for retry in state.retry_attempts.values() {
        map.entry(retry.entry.identifier.clone())
            .or_insert(IssueDebugSnapshot {
                issue_identifier: retry.entry.identifier.clone(),
                issue_id: retry.entry.issue_id.clone(),
                status: "retrying".into(),
                workspace: WorkspaceDebugInfo {
                    path: state.workflow.config.workspace.root.join(
                        crate::workspace::sanitize_issue_identifier(&retry.entry.identifier),
                    ),
                },
                attempts: AttemptDebugInfo {
                    restart_count: retry.entry.attempt.saturating_sub(1),
                    current_retry_attempt: Some(retry.entry.attempt),
                },
                running: None,
                retry: Some(retry.entry.clone()),
                logs: LogDebugInfo::default(),
                recent_events: Vec::new(),
                last_error: retry.entry.error.clone(),
                tracked: BTreeMap::new(),
            });
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct NoopTracker;

    #[async_trait]
    impl IssueTracker for NoopTracker {
        async fn fetch_candidate_issues(&self, _config: &EffectiveConfig) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        async fn fetch_issues_by_states(
            &self,
            _config: &EffectiveConfig,
            _state_names: &[String],
        ) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        async fn fetch_issue_states_by_ids(
            &self,
            _config: &EffectiveConfig,
            _issue_ids: &[String],
        ) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        async fn execute_raw_graphql(
            &self,
            _config: &EffectiveConfig,
            _query: &str,
            _variables: Option<Value>,
        ) -> Result<Value> {
            Ok(serde_json::json!({}))
        }
    }

    fn test_state() -> OrchestratorState {
        let config = EffectiveConfig {
            workflow_path: PathBuf::from("WORKFLOW.md"),
            prompt_template: "Hello".into(),
            tracker: crate::types::TrackerSettings {
                kind: "linear".into(),
                endpoint: "http://example.com".into(),
                api_key: "token".into(),
                project_slug: "proj".into(),
                active_states: vec!["Todo".into(), "In Progress".into()],
                terminal_states: vec!["Done".into()],
            },
            polling: crate::types::PollingSettings {
                interval_ms: 30_000,
            },
            workspace: crate::types::WorkspaceSettings {
                root: PathBuf::from("/tmp/symphony"),
            },
            hooks: crate::types::HookSettings {
                timeout_ms: 60_000,
                ..Default::default()
            },
            agent: crate::types::AgentSettings {
                max_concurrent_agents: 2,
                max_turns: 20,
                max_retry_backoff_ms: 300_000,
                max_concurrent_agents_by_state: BTreeMap::new(),
            },
            codex: crate::types::CodexSettings {
                command: "codex app-server".into(),
                approval_policy: serde_json::json!("never"),
                thread_sandbox: serde_json::json!("danger-full-access"),
                turn_sandbox_policy: serde_json::json!({"type": "dangerFullAccess"}),
                turn_timeout_ms: 1000,
                read_timeout_ms: 1000,
                stall_timeout_ms: 1000,
            },
            server: crate::types::ServerSettings::default(),
        };
        let workflow = WorkflowRuntime {
            config: config.clone(),
            prompt: Arc::new(crate::workflow::PromptRenderer::compile("Hello").unwrap()),
        };
        let (worker_tx, _) = mpsc::unbounded_channel();
        let (retry_tx, _) = mpsc::unbounded_channel();
        OrchestratorState {
            workflow,
            tracker: Arc::new(NoopTracker),
            workspace_manager: WorkspaceManager,
            snapshot_store: SnapshotStore::new(),
            worker_tx,
            retry_tx,
            running: HashMap::new(),
            claimed: HashSet::new(),
            retry_attempts: HashMap::new(),
            completed: HashSet::new(),
            codex_totals: CodexTotals::default(),
            rate_limits: None,
            refresh_queued: false,
        }
    }

    #[test]
    fn sorts_by_priority_then_age_then_identifier() {
        let mut issues = vec![
            Issue {
                id: "2".into(),
                identifier: "ABC-2".into(),
                title: "b".into(),
                priority: Some(2),
                created_at: Some(Utc::now()),
                state: "Todo".into(),
                ..Issue::default()
            },
            Issue {
                id: "1".into(),
                identifier: "ABC-1".into(),
                title: "a".into(),
                priority: Some(1),
                created_at: Some(Utc::now() - chrono::Duration::days(1)),
                state: "Todo".into(),
                ..Issue::default()
            },
        ];
        sort_issues_for_dispatch(&mut issues);
        assert_eq!(issues[0].identifier, "ABC-1");
    }

    #[test]
    fn blocks_todo_items_with_non_terminal_blockers() {
        let state = test_state();
        let issue = Issue {
            id: "1".into(),
            identifier: "ABC-1".into(),
            title: "Blocked".into(),
            state: "Todo".into(),
            blocked_by: vec![crate::types::BlockerRef {
                id: Some("2".into()),
                identifier: Some("ABC-2".into()),
                state: Some("In Progress".into()),
            }],
            ..Issue::default()
        };
        assert!(!should_dispatch(&state, &issue, None));
    }

    #[test]
    fn allows_todo_items_with_terminal_blockers() {
        let state = test_state();
        let issue = Issue {
            id: "1".into(),
            identifier: "ABC-1".into(),
            title: "Ready".into(),
            state: "Todo".into(),
            blocked_by: vec![crate::types::BlockerRef {
                id: Some("2".into()),
                identifier: Some("ABC-2".into()),
                state: Some("Done".into()),
            }],
            ..Issue::default()
        };
        assert!(should_dispatch(&state, &issue, None));
    }

    #[test]
    fn retry_backoff_is_capped() {
        let state = test_state();
        assert_eq!(retry_delay_ms(10, &state), 300_000);
    }

    #[tokio::test]
    async fn reconcile_marks_stalled_runs_for_cancellation() {
        let mut state = test_state();
        let cancel = CancellationToken::new();
        let join = tokio::spawn(async {});

        state.running.insert(
            "1".into(),
            RunningRuntime {
                entry: RunningEntry {
                    issue_id: "1".into(),
                    issue_identifier: "ABC-1".into(),
                    issue: Issue {
                        id: "1".into(),
                        identifier: "ABC-1".into(),
                        title: "stalled".into(),
                        state: "In Progress".into(),
                        ..Issue::default()
                    },
                    workspace_path: PathBuf::from("/tmp/symphony/ABC-1"),
                    retry_attempt: None,
                    started_at: Utc::now() - chrono::Duration::seconds(10),
                    session: LiveSession {
                        last_codex_timestamp: Some(Utc::now() - chrono::Duration::seconds(10)),
                        ..LiveSession::default()
                    },
                    recent_events: Vec::new(),
                    last_error: None,
                    log_path: None,
                },
                worker: WorkerHandle {
                    join,
                    cancel: cancel.clone(),
                },
                stop_reason: None,
            },
        );

        reconcile_stalled_runs(&mut state).await;

        let running = state.running.get("1").expect("running entry should exist");
        assert_eq!(running.stop_reason, Some(StopReason::Stall));
        assert!(cancel.is_cancelled());
    }
}
