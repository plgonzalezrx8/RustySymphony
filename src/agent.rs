use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::mpsc,
    task::JoinHandle,
    time::{Duration, Instant, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{
    error::{Result, SymphonyError},
    tracker::IssueTracker,
    types::{EffectiveConfig, Issue, TokenUsage},
    workflow::WorkflowRuntime,
    workspace::{
        WorkspaceManager, append_session_log, initialize_session_log,
        validate_workspace_containment,
    },
};

const MAX_PROTOCOL_LINE_BYTES: usize = 10 * 1024 * 1024;

/// Input used to start a new issue worker task.
#[derive(Clone)]
pub struct WorkerStartContext {
    pub issue: Issue,
    pub attempt: Option<u32>,
    pub workflow: WorkflowRuntime,
    pub tracker: Arc<dyn IssueTracker>,
    pub workspace_manager: WorkspaceManager,
}

/// Handle for a live worker task so the orchestrator can cancel it.
pub struct WorkerHandle {
    pub join: JoinHandle<()>,
    pub cancel: CancellationToken,
}

/// Event stream emitted from worker tasks back to the orchestrator.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Update(SessionUpdate),
    Finished(WorkerOutcome),
}

/// Live session delta emitted while a worker is active.
#[derive(Debug, Clone)]
pub struct SessionUpdate {
    pub issue_id: String,
    pub issue_identifier: String,
    pub event: String,
    pub message: Option<String>,
    pub at: DateTime<Utc>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub pid: Option<String>,
    pub absolute_tokens: Option<TokenUsage>,
    pub rate_limits: Option<Value>,
    pub turn_count: Option<u32>,
}

/// Terminal outcome reported after a worker exits.
#[derive(Debug, Clone)]
pub struct WorkerOutcome {
    pub issue: Issue,
    pub attempt: Option<u32>,
    pub workspace_path: PathBuf,
    pub workspace_key: String,
    pub log_path: PathBuf,
    pub exit: WorkerExit,
    pub error: Option<String>,
    pub turn_count: u32,
}

/// Worker termination classes used by retry and reconciliation logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerExit {
    Succeeded,
    Failed,
    TimedOut,
    Canceled,
}

enum IncomingMessage {
    StdoutJson(Value),
    StdoutMalformed(String),
    Stderr(String),
    StdoutClosed,
    StderrClosed,
}

/// Spawn a worker task and return a cancellation handle for reconciliation logic.
pub fn spawn_worker(
    context: WorkerStartContext,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> WorkerHandle {
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let join = tokio::spawn(async move {
        let outcome = run_issue_worker(context, worker_cancel.clone(), event_tx.clone()).await;
        let _ = event_tx.send(WorkerEvent::Finished(outcome));
    });
    WorkerHandle { join, cancel }
}

async fn run_issue_worker(
    context: WorkerStartContext,
    cancel: CancellationToken,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> WorkerOutcome {
    let issue = context.issue.clone();
    let fallback_workspace_key = crate::workspace::sanitize_issue_identifier(&issue.identifier);
    let fallback_log_path = context
        .workspace_manager
        .session_log_path(
            &context.workflow.config.workspace.root,
            &fallback_workspace_key,
        )
        .unwrap_or_else(|_| PathBuf::from("latest.log"));

    let workspace = match context
        .workspace_manager
        .ensure_workspace(&issue.identifier, &context.workflow.config)
        .await
    {
        Ok(workspace) => workspace,
        Err(error) => {
            return WorkerOutcome {
                issue,
                attempt: context.attempt,
                workspace_path: context.workflow.config.workspace.root.clone(),
                workspace_key: fallback_workspace_key,
                log_path: fallback_log_path,
                exit: WorkerExit::Failed,
                error: Some(error.to_string()),
                turn_count: 0,
            };
        }
    };

    let log_path = context
        .workspace_manager
        .session_log_path(
            &context.workflow.config.workspace.root,
            &workspace.workspace_key,
        )
        .unwrap_or_else(|_| fallback_log_path.clone());
    let _ = initialize_session_log(&log_path).await;

    if let Some(script) = &context.workflow.config.hooks.before_run
        && let Err(error) = context
            .workspace_manager
            .run_hook(
                "before_run",
                script,
                &workspace.path,
                &context.workflow.config,
            )
            .await
    {
        return WorkerOutcome {
            issue,
            attempt: context.attempt,
            workspace_path: workspace.path,
            workspace_key: workspace.workspace_key,
            log_path,
            exit: WorkerExit::Failed,
            error: Some(error.to_string()),
            turn_count: 0,
        };
    }

    let mut session = match CodexSession::start(
        &context.workflow.config,
        &context.tracker,
        &workspace.path,
        &log_path,
        &issue,
        event_tx.clone(),
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            context
                .workspace_manager
                .run_hook_best_effort(
                    "after_run",
                    context.workflow.config.hooks.after_run.as_deref(),
                    &workspace.path,
                    &context.workflow.config,
                )
                .await;
            return WorkerOutcome {
                issue,
                attempt: context.attempt,
                workspace_path: workspace.path,
                workspace_key: workspace.workspace_key,
                log_path,
                exit: WorkerExit::Failed,
                error: Some(error.to_string()),
                turn_count: 0,
            };
        }
    };

    let mut current_issue = context.issue.clone();
    let mut turn_count = 0u32;
    let mut exit = WorkerExit::Succeeded;
    let mut error_message = None;

    loop {
        turn_count += 1;
        let prompt = if turn_count == 1 {
            match context
                .workflow
                .prompt
                .render_issue_prompt(&current_issue, context.attempt)
            {
                Ok(prompt) => prompt,
                Err(error) => {
                    exit = WorkerExit::Failed;
                    error_message = Some(error.to_string());
                    break;
                }
            }
        } else {
            context.workflow.prompt.continuation_prompt(
                &current_issue,
                turn_count,
                context.workflow.config.agent.max_turns,
                context.attempt,
            )
        };

        let title = format!("{}: {}", current_issue.identifier, current_issue.title);
        match session.run_turn(&prompt, &title, turn_count, &cancel).await {
            Ok(()) => {}
            Err(error) => {
                exit = if cancel.is_cancelled() {
                    WorkerExit::Canceled
                } else if matches!(error, SymphonyError::TurnTimeout) {
                    WorkerExit::TimedOut
                } else {
                    WorkerExit::Failed
                };
                error_message = Some(error.to_string());
                break;
            }
        }

        if cancel.is_cancelled() {
            exit = WorkerExit::Canceled;
            error_message = Some("worker canceled".into());
            break;
        }

        match context
            .tracker
            .fetch_issue_states_by_ids(&context.workflow.config, &[current_issue.id.clone()])
            .await
        {
            Ok(mut issues) => {
                if let Some(issue) = issues.pop() {
                    current_issue = issue;
                }
            }
            Err(error) => {
                exit = WorkerExit::Failed;
                error_message = Some(error.to_string());
                break;
            }
        }

        if !context
            .workflow
            .config
            .is_active_state(&current_issue.state)
        {
            break;
        }
        if turn_count >= context.workflow.config.agent.max_turns {
            break;
        }
    }

    if let Err(error) = session.shutdown().await {
        warn!(error = %error, "failed to stop codex session cleanly");
    }

    context
        .workspace_manager
        .run_hook_best_effort(
            "after_run",
            context.workflow.config.hooks.after_run.as_deref(),
            &workspace.path,
            &context.workflow.config,
        )
        .await;

    WorkerOutcome {
        issue: current_issue,
        attempt: context.attempt,
        workspace_path: workspace.path,
        workspace_key: workspace.workspace_key,
        log_path,
        exit,
        error: error_message,
        turn_count,
    }
}

struct CodexSession {
    child: Child,
    stdin: ChildStdin,
    incoming_rx: mpsc::UnboundedReceiver<IncomingMessage>,
    next_request_id: u64,
    config: EffectiveConfig,
    tracker: Arc<dyn IssueTracker>,
    log_path: PathBuf,
    issue_id: String,
    issue_identifier: String,
    workspace_path: PathBuf,
    pid: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    session_id: Option<String>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
}

impl CodexSession {
    async fn start(
        config: &EffectiveConfig,
        tracker: &Arc<dyn IssueTracker>,
        workspace_path: &Path,
        log_path: &Path,
        issue: &Issue,
        event_tx: mpsc::UnboundedSender<WorkerEvent>,
    ) -> Result<Self> {
        validate_workspace_containment(&config.workspace.root, workspace_path)?;
        let workspace_path = crate::workspace::normalize_to_absolute(workspace_path)?;

        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(&config.codex.command)
            .current_dir(&workspace_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|error| {
            SymphonyError::ResponseError(format!("failed to spawn codex command: {error}"))
        })?;
        let pid = child.id().map(|pid| pid.to_string());
        let stdin = child.stdin.take().ok_or_else(|| {
            SymphonyError::ResponseError("codex child stdin was not captured".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            SymphonyError::ResponseError("codex child stdout was not captured".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            SymphonyError::ResponseError("codex child stderr was not captured".into())
        })?;

        let incoming_rx = spawn_stream_readers(stdout, stderr, log_path.to_path_buf());
        let mut session = Self {
            child,
            stdin,
            incoming_rx,
            next_request_id: 1,
            config: config.clone(),
            tracker: tracker.clone(),
            log_path: log_path.to_path_buf(),
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            workspace_path: workspace_path.clone(),
            pid,
            thread_id: None,
            turn_id: None,
            session_id: None,
            event_tx,
        };

        let initialize_result = session
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "symphony", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": true }
                }),
            )
            .await?;
        debug!(result = %initialize_result, "codex initialize completed");

        if let Err(error) = session.notify("initialized", json!({})).await {
            let _ = session.event_tx.send(WorkerEvent::Update(SessionUpdate {
                issue_id: session.issue_id.clone(),
                issue_identifier: session.issue_identifier.clone(),
                event: "startup_failed".into(),
                message: Some(error.to_string()),
                at: Utc::now(),
                session_id: None,
                thread_id: None,
                turn_id: None,
                pid: session.pid.clone(),
                absolute_tokens: None,
                rate_limits: None,
                turn_count: None,
            }));
            return Err(error);
        }

        let mut thread_params = json!({
            "approvalPolicy": session.config.codex.approval_policy,
            "sandbox": session.config.codex.thread_sandbox,
            "cwd": workspace_path,
            "serviceName": "symphony",
            "personality": "pragmatic",
        });
        if session.config.tracker.kind == "linear" {
            let tools = json!([linear_graphql_tool_spec()]);
            thread_params["tools"] = tools.clone();
            thread_params["config"] = json!({ "tools": tools });
        }

        let thread_result = session.request("thread/start", thread_params).await?;
        session.thread_id = extract_string(&thread_result, &[&["thread", "id"], &["threadId"]]);

        Ok(session)
    }

    async fn run_turn(
        &mut self,
        prompt: &str,
        title: &str,
        turn_count: u32,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let thread_id = self
            .thread_id
            .clone()
            .ok_or_else(|| SymphonyError::ResponseError("missing thread id".into()))?;

        let turn_result = self
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "input": [{ "type": "text", "text": prompt }],
                    "cwd": self.workspace_path.clone(),
                    "title": title,
                    "approvalPolicy": self.config.codex.approval_policy,
                    "sandboxPolicy": self.config.codex.turn_sandbox_policy,
                    "personality": "pragmatic",
                }),
            )
            .await?;

        self.turn_id = extract_string(&turn_result, &[&["turn", "id"], &["turnId"]]);
        self.session_id = match (&self.thread_id, &self.turn_id) {
            (Some(thread_id), Some(turn_id)) => Some(format!("{thread_id}-{turn_id}")),
            _ => None,
        };
        self.emit_update(
            "session_started",
            summarize_value(&turn_result),
            None,
            None,
            Some(turn_count),
        );

        let turn_deadline =
            Instant::now() + Duration::from_millis(self.config.codex.turn_timeout_ms);
        loop {
            let remaining = turn_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SymphonyError::TurnTimeout);
            }

            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = self.child.kill().await;
                    return Err(SymphonyError::TurnCancelled);
                }
                incoming = timeout(remaining, self.incoming_rx.recv()) => {
                    let incoming = incoming.map_err(|_| SymphonyError::TurnTimeout)?;
                    let incoming = incoming.ok_or(SymphonyError::PortExit)?;
                    match self.handle_incoming(incoming, Some(turn_count)).await? {
                        TurnProgress::Continue => {}
                        TurnProgress::Completed => return Ok(()),
                    }
                }
            }
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            let _ = self.child.kill().await;
        }
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.write_json(json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        let deadline = Duration::from_millis(self.config.codex.read_timeout_ms);
        loop {
            let incoming = timeout(deadline, self.incoming_rx.recv())
                .await
                .map_err(|_| SymphonyError::ResponseTimeout)?
                .ok_or(SymphonyError::PortExit)?;

            match incoming {
                IncomingMessage::StdoutJson(value) => {
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(result) = value.get("result") {
                            return Ok(result.clone());
                        }
                        return Err(SymphonyError::ResponseError(
                            value
                                .get("error")
                                .cloned()
                                .unwrap_or(Value::String("missing result".into()))
                                .to_string(),
                        ));
                    }

                    self.handle_value(value, None).await?;
                }
                IncomingMessage::StdoutMalformed(line) => {
                    self.emit_update("malformed", Some(line), None, None, None);
                }
                IncomingMessage::Stderr(line) => {
                    self.emit_update("notification", Some(line), None, None, None);
                }
                IncomingMessage::StdoutClosed | IncomingMessage::StderrClosed => {
                    if self.child.try_wait()?.is_some() {
                        return Err(SymphonyError::PortExit);
                    }
                }
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_json(json!({
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn write_json(&mut self, value: Value) -> Result<()> {
        let line = value.to_string();
        append_session_log(&self.log_path, &format!(">> {line}")).await;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn handle_incoming(
        &mut self,
        incoming: IncomingMessage,
        turn_count: Option<u32>,
    ) -> Result<TurnProgress> {
        match incoming {
            IncomingMessage::StdoutJson(value) => self.handle_value(value, turn_count).await,
            IncomingMessage::StdoutMalformed(line) => {
                self.emit_update("malformed", Some(line), None, None, turn_count);
                Ok(TurnProgress::Continue)
            }
            IncomingMessage::Stderr(line) => {
                self.emit_update("notification", Some(line), None, None, turn_count);
                Ok(TurnProgress::Continue)
            }
            IncomingMessage::StdoutClosed | IncomingMessage::StderrClosed => {
                if self.child.try_wait()?.is_some() {
                    Err(SymphonyError::PortExit)
                } else {
                    Ok(TurnProgress::Continue)
                }
            }
        }
    }

    async fn handle_value(
        &mut self,
        value: Value,
        turn_count: Option<u32>,
    ) -> Result<TurnProgress> {
        if value.get("method").is_some() && value.get("id").is_some() {
            self.handle_server_request(&value, turn_count).await?;
            return Ok(TurnProgress::Continue);
        }

        let method = value.get("method").and_then(Value::as_str);
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        match method {
            Some("turn/completed") => {
                let tokens = extract_token_usage(&params);
                self.emit_update(
                    "turn_completed",
                    summarize_value(&params),
                    tokens,
                    None,
                    turn_count,
                );
                Ok(TurnProgress::Completed)
            }
            Some("turn/failed") => {
                let tokens = extract_token_usage(&params);
                self.emit_update(
                    "turn_failed",
                    summarize_value(&params),
                    tokens,
                    None,
                    turn_count,
                );
                Err(SymphonyError::TurnFailed(
                    summarize_value(&params).unwrap_or_else(|| "turn failed".into()),
                ))
            }
            Some("turn/cancelled") => {
                self.emit_update(
                    "turn_cancelled",
                    summarize_value(&params),
                    None,
                    None,
                    turn_count,
                );
                Err(SymphonyError::TurnCancelled)
            }
            Some("thread/tokenUsage/updated") => {
                let tokens = extract_token_usage(&params);
                self.emit_update(
                    "notification",
                    summarize_value(&params),
                    tokens,
                    None,
                    turn_count,
                );
                Ok(TurnProgress::Continue)
            }
            Some("account/rateLimits/updated") => {
                self.emit_update(
                    "notification",
                    summarize_value(&params),
                    None,
                    params
                        .get("rateLimits")
                        .cloned()
                        .or_else(|| Some(params.clone())),
                    turn_count,
                );
                Ok(TurnProgress::Continue)
            }
            Some(_) => {
                self.emit_update(
                    "other_message",
                    summarize_value(&params).or_else(|| Some(value.to_string())),
                    None,
                    None,
                    turn_count,
                );
                Ok(TurnProgress::Continue)
            }
            None => Ok(TurnProgress::Continue),
        }
    }

    async fn handle_server_request(
        &mut self,
        value: &Value,
        turn_count: Option<u32>,
    ) -> Result<()> {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| SymphonyError::ResponseError("request missing method".into()))?;
        let id = value
            .get("id")
            .cloned()
            .ok_or_else(|| SymphonyError::ResponseError("request missing id".into()))?;
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                self.write_json(json!({
                    "id": id,
                    "result": { "decision": "acceptForSession" }
                }))
                .await?;
                self.emit_update(
                    "approval_auto_approved",
                    summarize_value(&params),
                    None,
                    None,
                    turn_count,
                );
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                self.write_json(json!({
                    "id": id,
                    "result": { "decision": "acceptForSession" }
                }))
                .await?;
                self.emit_update(
                    "approval_auto_approved",
                    summarize_value(&params),
                    None,
                    None,
                    turn_count,
                );
            }
            "item/tool/call" => {
                let response = self.handle_dynamic_tool_call(&params).await;
                self.write_json(json!({
                    "id": id,
                    "result": response,
                }))
                .await?;
            }
            "item/tool/requestUserInput" | "mcpServer/elicitation/request" => {
                self.write_json(json!({
                    "id": id,
                    "error": { "code": -32000, "message": "turn_input_required" }
                }))
                .await?;
                self.emit_update(
                    "turn_input_required",
                    summarize_value(&params),
                    None,
                    None,
                    turn_count,
                );
                return Err(SymphonyError::TurnInputRequired);
            }
            other => {
                self.write_json(json!({
                    "id": id,
                    "error": { "code": -32001, "message": format!("unsupported request: {other}") }
                }))
                .await?;
            }
        }

        Ok(())
    }

    async fn handle_dynamic_tool_call(&mut self, params: &Value) -> Value {
        let tool = params
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if tool != "linear_graphql" {
            self.emit_update(
                "unsupported_tool_call",
                Some(format!("unsupported tool {tool}")),
                None,
                None,
                None,
            );
            return json!({
                "success": false,
                "contentItems": [{ "type": "inputText", "text": "unsupported_tool_call" }]
            });
        }

        match parse_linear_graphql_arguments(params.get("arguments")) {
            Ok((query, variables)) => match self
                .tracker
                .execute_raw_graphql(&self.config, &query, variables)
                .await
            {
                Ok(body) => json!({
                    "success": true,
                    "contentItems": [{ "type": "inputText", "text": body.to_string() }]
                }),
                Err(error) => json!({
                    "success": false,
                    "contentItems": [{ "type": "inputText", "text": format!("{{\"error\":\"{}\"}}", error) }]
                }),
            },
            Err(error) => json!({
                "success": false,
                "contentItems": [{ "type": "inputText", "text": format!("{{\"error\":\"{}\"}}", error) }]
            }),
        }
    }

    fn emit_update(
        &self,
        event: &str,
        message: Option<String>,
        absolute_tokens: Option<TokenUsage>,
        rate_limits: Option<Value>,
        turn_count: Option<u32>,
    ) {
        let _ = self.event_tx.send(WorkerEvent::Update(SessionUpdate {
            issue_id: self.issue_id.clone(),
            issue_identifier: self.issue_identifier.clone(),
            event: event.to_string(),
            message,
            at: Utc::now(),
            session_id: self.session_id.clone(),
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            pid: self.pid.clone(),
            absolute_tokens,
            rate_limits,
            turn_count,
        }));
    }
}

enum TurnProgress {
    Continue,
    Completed,
}

fn spawn_stream_readers(
    stdout: ChildStdout,
    stderr: ChildStderr,
    log_path: PathBuf,
) -> mpsc::UnboundedReceiver<IncomingMessage> {
    let (tx, rx) = mpsc::unbounded_channel();

    let stdout_tx = tx.clone();
    let stdout_log = log_path.clone();
    tokio::spawn(async move {
        read_json_lines(stdout, stdout_tx, stdout_log).await;
    });

    let stderr_log = log_path.clone();
    tokio::spawn(async move {
        read_stderr_lines(stderr, tx, stderr_log).await;
    });

    rx
}

async fn read_json_lines(
    stdout: ChildStdout,
    tx: mpsc::UnboundedSender<IncomingMessage>,
    log_path: PathBuf,
) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => {
                let _ = tx.send(IncomingMessage::StdoutClosed);
                break;
            }
            Ok(_) => {
                if buffer.len() > MAX_PROTOCOL_LINE_BYTES {
                    let _ = tx.send(IncomingMessage::StdoutMalformed(
                        "stdout line exceeded 10MB".into(),
                    ));
                    continue;
                }
                let line = String::from_utf8_lossy(&buffer).trim().to_string();
                append_session_log(&log_path, &format!("<< {line}")).await;
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => {
                        let _ = tx.send(IncomingMessage::StdoutJson(value));
                    }
                    Err(_) => {
                        let _ = tx.send(IncomingMessage::StdoutMalformed(line));
                    }
                }
            }
            Err(error) => {
                let _ = tx.send(IncomingMessage::StdoutMalformed(error.to_string()));
                break;
            }
        }
    }
}

async fn read_stderr_lines(
    stderr: ChildStderr,
    tx: mpsc::UnboundedSender<IncomingMessage>,
    log_path: PathBuf,
) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                append_session_log(&log_path, &format!("!! {line}")).await;
                let _ = tx.send(IncomingMessage::Stderr(line));
            }
            Ok(None) => {
                let _ = tx.send(IncomingMessage::StderrClosed);
                break;
            }
            Err(error) => {
                let _ = tx.send(IncomingMessage::Stderr(error.to_string()));
                break;
            }
        }
    }
}

fn linear_graphql_tool_spec() -> Value {
    json!({
        "name": "linear_graphql",
        "description": "Execute a raw GraphQL query or mutation against Linear using Symphony's configured tracker auth.",
        "inputSchema": {
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "variables": { "type": "object" }
            }
        }
    })
}

fn parse_linear_graphql_arguments(arguments: Option<&Value>) -> Result<(String, Option<Value>)> {
    match arguments {
        Some(Value::String(query)) if !query.trim().is_empty() => Ok((query.clone(), None)),
        Some(Value::Object(object)) => {
            let query = object
                .get("query")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    SymphonyError::InvalidConfig("query must be a non-empty string".into())
                })?
                .to_string();
            let variables = object.get("variables").cloned();
            if variables.as_ref().is_some_and(|value| !value.is_object()) {
                return Err(SymphonyError::InvalidConfig(
                    "variables must be an object".into(),
                ));
            }
            Ok((query, variables))
        }
        _ => Err(SymphonyError::InvalidConfig(
            "arguments must be a query string or object".into(),
        )),
    }
}

fn extract_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    for path in paths {
        let mut current = value;
        let mut missing = false;
        for segment in *path {
            if let Some(next) = current.get(*segment) {
                current = next;
            } else {
                missing = true;
                break;
            }
        }
        if !missing && let Some(value) = current.as_str() {
            return Some(value.to_string());
        }
    }
    None
}

fn summarize_value(value: &Value) -> Option<String> {
    if let Some(message) = find_string(value, &["message", "text", "content", "reason", "summary"])
    {
        return Some(message);
    }
    let text = value.to_string();
    if text == "null" {
        None
    } else if text.len() > 300 {
        Some(format!("{}...", &text[..300]))
    } else {
        Some(text)
    }
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(Value::as_str) {
                    return Some(value.to_string());
                }
            }
            for value in object.values() {
                if let Some(found) = find_string(value, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => values.iter().find_map(|value| find_string(value, keys)),
        _ => None,
    }
}

fn extract_token_usage(value: &Value) -> Option<TokenUsage> {
    if let Some(total) = value
        .get("total_token_usage")
        .or_else(|| value.get("totalTokenUsage"))
    {
        return extract_token_usage(total);
    }

    if let Some(object) = value.as_object() {
        let input = object
            .get("input_tokens")
            .or_else(|| object.get("inputTokens"))
            .and_then(Value::as_u64);
        let output = object
            .get("output_tokens")
            .or_else(|| object.get("outputTokens"))
            .and_then(Value::as_u64);
        let total = object
            .get("total_tokens")
            .or_else(|| object.get("totalTokens"))
            .and_then(Value::as_u64)
            .or_else(|| match (input, output) {
                (Some(input), Some(output)) => Some(input + output),
                _ => None,
            });

        if input.is_some() || output.is_some() || total.is_some() {
            return Some(TokenUsage {
                input_tokens: input.unwrap_or_default(),
                output_tokens: output.unwrap_or_default(),
                total_tokens: total.unwrap_or_default(),
            });
        }

        for value in object.values() {
            if let Some(tokens) = extract_token_usage(value) {
                return Some(tokens);
            }
        }
    } else if let Some(values) = value.as_array() {
        for value in values {
            if let Some(tokens) = extract_token_usage(value) {
                return Some(tokens);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_linear_graphql_arguments_from_string_and_object() {
        let (query, variables) = parse_linear_graphql_arguments(Some(&Value::String(
            String::from("query One { viewer { id } }"),
        )))
        .expect("string arguments should parse");
        assert_eq!(query, "query One { viewer { id } }");
        assert!(variables.is_none());

        let (query, variables) = parse_linear_graphql_arguments(Some(&serde_json::json!({
            "query": "query One { viewer { id } }",
            "variables": { "id": 1 }
        })))
        .expect("object arguments should parse");
        assert_eq!(query, "query One { viewer { id } }");
        assert_eq!(variables, Some(serde_json::json!({ "id": 1 })));
    }

    #[test]
    fn rejects_invalid_linear_graphql_arguments() {
        assert!(parse_linear_graphql_arguments(None).is_err());
        assert!(parse_linear_graphql_arguments(Some(&serde_json::json!({ "query": "" }))).is_err());
        assert!(
            parse_linear_graphql_arguments(Some(&serde_json::json!({
                "query": "query One { viewer { id } }",
                "variables": ["not-an-object"]
            })))
            .is_err()
        );
    }

    #[test]
    fn extracts_strings_and_summaries_from_nested_payloads() {
        let payload = serde_json::json!({
            "result": {
                "thread": { "id": "thread-1" },
                "turn": { "id": "turn-1" },
                "message": "done"
            }
        });

        assert_eq!(
            extract_string(&payload, &[&["result", "thread", "id"]]),
            Some(String::from("thread-1"))
        );
        assert_eq!(
            find_string(&payload, &["message", "text"]),
            Some(String::from("done"))
        );
        assert_eq!(summarize_value(&Value::Null), None);
        assert_eq!(
            summarize_value(&serde_json::json!({ "summary": "short" })),
            Some(String::from("short"))
        );
        assert!(
            summarize_value(&Value::String("x".repeat(400)))
                .unwrap_or_default()
                .ends_with("...")
        );
    }

    #[test]
    fn extracts_nested_token_usage_variants() {
        let tokens = extract_token_usage(&serde_json::json!({
            "params": {
                "total_token_usage": {
                    "input_tokens": 4,
                    "output_tokens": 5,
                    "total_tokens": 9
                }
            }
        }))
        .expect("snake_case tokens should parse");
        assert_eq!(
            tokens,
            TokenUsage {
                input_tokens: 4,
                output_tokens: 5,
                total_tokens: 9,
            }
        );

        let tokens = extract_token_usage(&serde_json::json!({
            "usage": {
                "inputTokens": 6,
                "outputTokens": 7
            }
        }))
        .expect("camelCase tokens should parse");
        assert_eq!(tokens.total_tokens, 13);
    }

    proptest! {
        #[test]
        fn parse_linear_graphql_arguments_round_trips_valid_object_payloads(
            query in "[A-Za-z0-9 _{}()]{1,80}",
            key in "[a-z]{1,10}",
            value in any::<i64>(),
        ) {
            let payload = serde_json::json!({
                "query": query.clone(),
                "variables": { key.clone(): value }
            });

            let (parsed_query, variables) =
                parse_linear_graphql_arguments(Some(&payload)).expect("valid payload should parse");
            prop_assert_eq!(parsed_query, query);
            prop_assert_eq!(variables, Some(serde_json::json!({ key: value })));
        }

        #[test]
        fn parse_linear_graphql_arguments_rejects_non_object_variables(
            query in "[A-Za-z0-9 _{}()]{1,80}",
            first in any::<i64>(),
            second in any::<i64>(),
        ) {
            let payload = serde_json::json!({
                "query": query,
                "variables": [first, second]
            });
            let error = parse_linear_graphql_arguments(Some(&payload)).unwrap_err();
            prop_assert!(matches!(error, SymphonyError::InvalidConfig(_)));
        }
    }
}
