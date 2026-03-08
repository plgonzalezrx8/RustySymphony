use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Normalized blocker reference derived from tracker relations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BlockerRef {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub state: Option<String>,
}

/// Stable issue shape used by orchestration, prompts, HTTP output, and logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub state: String,
    pub branch_name: Option<String>,
    pub url: Option<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<BlockerRef>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Parsed `WORKFLOW.md` contents split into config front matter and prompt body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WorkflowDefinition {
    pub config: BTreeMap<String, Value>,
    pub prompt_template: String,
}

/// Typed tracker settings derived from workflow config plus environment resolution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackerSettings {
    pub kind: String,
    pub endpoint: String,
    pub api_key: String,
    pub project_slug: String,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

/// Polling cadence applied by the orchestrator event loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PollingSettings {
    pub interval_ms: u64,
}

/// Workspace root configuration for per-issue directories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceSettings {
    pub root: PathBuf,
}

/// Workspace lifecycle hook settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HookSettings {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub timeout_ms: u64,
}

/// Agent concurrency and retry policy settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentSettings {
    pub max_concurrent_agents: usize,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: BTreeMap<String, usize>,
}

/// Pass-through Codex settings used to build app-server requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodexSettings {
    pub command: String,
    pub approval_policy: Value,
    pub thread_sandbox: Value,
    pub turn_sandbox_policy: Value,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

/// Optional loopback HTTP server settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ServerSettings {
    pub port: Option<u16>,
}

/// Fully resolved runtime configuration used by the live service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EffectiveConfig {
    pub workflow_path: PathBuf,
    pub prompt_template: String,
    pub tracker: TrackerSettings,
    pub polling: PollingSettings,
    pub workspace: WorkspaceSettings,
    pub hooks: HookSettings,
    pub agent: AgentSettings,
    pub codex: CodexSettings,
    pub server: ServerSettings,
}

/// Filesystem workspace assigned to one issue identifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub path: PathBuf,
    pub workspace_key: String,
    pub created_now: bool,
}

/// Absolute token totals tracked per active session or as aggregates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Human-readable event summary retained for operator debugging.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecentEvent {
    pub at: DateTime<Utc>,
    pub event: String,
    pub message: String,
}

/// Live Codex session metadata tracked while a worker is running.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LiveSession {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<String>,
    pub last_codex_event: Option<String>,
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    pub last_codex_message: Option<String>,
    pub codex_input_tokens: u64,
    pub codex_output_tokens: u64,
    pub codex_total_tokens: u64,
    pub last_reported_input_tokens: u64,
    pub last_reported_output_tokens: u64,
    pub last_reported_total_tokens: u64,
    pub turn_count: u32,
}

/// Scheduled retry row exposed to status outputs and the HTTP API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryEntry {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: u32,
    pub due_at: DateTime<Utc>,
    pub error: Option<String>,
}

/// Public view of a running issue entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunningEntry {
    pub issue_id: String,
    pub issue_identifier: String,
    pub issue: Issue,
    pub workspace_path: PathBuf,
    pub retry_attempt: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub session: LiveSession,
    pub recent_events: Vec<RecentEvent>,
    pub last_error: Option<String>,
    pub log_path: Option<PathBuf>,
}

/// Aggregate runtime totals maintained by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CodexTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub seconds_running: f64,
}

/// Summary counts returned by the snapshot API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SnapshotCounts {
    pub running: usize,
    pub retrying: usize,
}

/// Row shape returned by `/api/v1/state` for running issues.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRunningRow {
    pub issue_id: String,
    pub issue_identifier: String,
    pub state: String,
    pub session_id: Option<String>,
    pub turn_count: u32,
    pub last_event: Option<String>,
    pub last_message: String,
    pub started_at: DateTime<Utc>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub tokens: TokenUsage,
}

/// Operator-facing runtime snapshot used by the API and dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeSnapshot {
    pub generated_at: DateTime<Utc>,
    pub counts: SnapshotCounts,
    pub running: Vec<RuntimeRunningRow>,
    pub retrying: Vec<RetryEntry>,
    pub codex_totals: CodexTotals,
    pub rate_limits: Option<Value>,
}

/// Nested workspace details returned by the issue-specific API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceDebugInfo {
    pub path: PathBuf,
}

/// Attempt counters returned by the issue-specific API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttemptDebugInfo {
    pub restart_count: u32,
    pub current_retry_attempt: Option<u32>,
}

/// Log file metadata returned by the issue-specific API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogReference {
    pub label: String,
    pub path: PathBuf,
    pub url: Option<String>,
}

/// Grouped log references exposed by the issue-specific API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LogDebugInfo {
    pub codex_session_logs: Vec<LogReference>,
}

/// Running details returned by the issue-specific API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueRunningDebugInfo {
    pub session_id: Option<String>,
    pub turn_count: u32,
    pub state: String,
    pub started_at: DateTime<Utc>,
    pub last_event: Option<String>,
    pub last_message: String,
    pub last_event_at: Option<DateTime<Utc>>,
    pub tokens: TokenUsage,
}

/// Issue-level debugging snapshot exposed by `/api/v1/:issue_identifier`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueDebugSnapshot {
    pub issue_identifier: String,
    pub issue_id: String,
    pub status: String,
    pub workspace: WorkspaceDebugInfo,
    pub attempts: AttemptDebugInfo,
    pub running: Option<IssueRunningDebugInfo>,
    pub retry: Option<RetryEntry>,
    pub logs: LogDebugInfo,
    pub recent_events: Vec<RecentEvent>,
    pub last_error: Option<String>,
    pub tracked: BTreeMap<String, Value>,
}

impl EffectiveConfig {
    /// Normalize a state string for case-insensitive comparisons.
    pub fn normalize_state(state: &str) -> String {
        state.trim().to_lowercase()
    }

    /// Return true when the provided tracker state is configured as active.
    pub fn is_active_state(&self, state: &str) -> bool {
        let needle = Self::normalize_state(state);
        self.tracker
            .active_states
            .iter()
            .any(|candidate| Self::normalize_state(candidate) == needle)
    }

    /// Return true when the provided tracker state is configured as terminal.
    pub fn is_terminal_state(&self, state: &str) -> bool {
        let needle = Self::normalize_state(state);
        self.tracker
            .terminal_states
            .iter()
            .any(|candidate| Self::normalize_state(candidate) == needle)
    }
}
