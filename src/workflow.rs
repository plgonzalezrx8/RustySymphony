use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use liquid::{Object, ParserBuilder, Template};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::warn;

use crate::{
    error::{Result, SymphonyError},
    types::{
        AgentSettings, CodexSettings, EffectiveConfig, HookSettings, Issue, PollingSettings,
        ServerSettings, TrackerSettings, WorkflowDefinition, WorkspaceSettings,
    },
};

const DEFAULT_PROMPT: &str = "You are working on an issue from Linear.";
const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";
const DEFAULT_POLL_INTERVAL_MS: u64 = 30_000;
const DEFAULT_HOOK_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_CONCURRENT_AGENTS: usize = 10;
const DEFAULT_MAX_TURNS: u32 = 20;
const DEFAULT_MAX_RETRY_BACKOFF_MS: u64 = 300_000;
const DEFAULT_TURN_TIMEOUT_MS: u64 = 3_600_000;
const DEFAULT_READ_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_STALL_TIMEOUT_MS: i64 = 300_000;

/// Parsed workflow plus compiled prompt renderer used by future dispatches.
#[derive(Clone)]
pub struct WorkflowRuntime {
    pub config: EffectiveConfig,
    pub prompt: Arc<PromptRenderer>,
}

/// Compiled Liquid-compatible prompt renderer with a fixed continuation prompt.
pub struct PromptRenderer {
    template: Template,
}

#[derive(Debug, Deserialize, Default)]
struct RawWorkflowConfig {
    tracker: Option<RawTracker>,
    polling: Option<RawPolling>,
    workspace: Option<RawWorkspace>,
    hooks: Option<RawHooks>,
    agent: Option<RawAgent>,
    codex: Option<RawCodex>,
    server: Option<RawServer>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTracker {
    kind: Option<String>,
    endpoint: Option<String>,
    api_key: Option<String>,
    project_slug: Option<String>,
    active_states: Option<StringOrVec>,
    terminal_states: Option<StringOrVec>,
}

#[derive(Debug, Deserialize, Default)]
struct RawPolling {
    interval_ms: Option<IntLike>,
}

#[derive(Debug, Deserialize, Default)]
struct RawWorkspace {
    root: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawHooks {
    after_create: Option<String>,
    before_run: Option<String>,
    after_run: Option<String>,
    before_remove: Option<String>,
    timeout_ms: Option<IntLike>,
}

#[derive(Debug, Deserialize, Default)]
struct RawAgent {
    max_concurrent_agents: Option<IntLike>,
    max_turns: Option<IntLike>,
    max_retry_backoff_ms: Option<IntLike>,
    max_concurrent_agents_by_state: Option<BTreeMap<String, IntLike>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawCodex {
    command: Option<String>,
    approval_policy: Option<Value>,
    thread_sandbox: Option<Value>,
    turn_sandbox_policy: Option<Value>,
    turn_timeout_ms: Option<IntLike>,
    read_timeout_ms: Option<IntLike>,
    stall_timeout_ms: Option<IntLike>,
}

#[derive(Debug, Deserialize, Default)]
struct RawServer {
    port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum IntLike {
    Integer(i64),
    String(String),
}

impl PromptRenderer {
    /// Compile a prompt renderer once per successful workflow reload.
    pub fn compile(prompt_template: &str) -> Result<Self> {
        let prompt = if prompt_template.trim().is_empty() {
            DEFAULT_PROMPT.to_string()
        } else {
            prompt_template.trim().to_string()
        };

        let template = ParserBuilder::with_stdlib()
            .build()
            .map_err(|error| SymphonyError::TemplateParseError(error.to_string()))?
            .parse(&prompt)
            .map_err(|error| SymphonyError::TemplateParseError(error.to_string()))?;

        Ok(Self { template })
    }

    /// Render the first-turn prompt using the normalized issue and attempt metadata.
    pub fn render_issue_prompt(&self, issue: &Issue, attempt: Option<u32>) -> Result<String> {
        let issue_value = serde_json::to_value(issue)?;
        let mut globals = Object::new();
        globals.insert("issue".into(), json_to_liquid(&issue_value)?);
        globals.insert(
            "attempt".into(),
            match attempt {
                Some(value) => liquid::model::Value::scalar(value as i64),
                None => liquid::model::Value::Nil,
            },
        );

        self.template
            .render(&globals)
            .map_err(|error| SymphonyError::TemplateRenderError(error.to_string()))
    }

    /// Return a compact continuation prompt for additional turns in the same thread.
    pub fn continuation_prompt(
        &self,
        issue: &Issue,
        turn_number: u32,
        max_turns: u32,
        attempt: Option<u32>,
    ) -> String {
        format!(
            "Continue working on issue {} ({}) in the existing thread. This is continuation turn \
             {} of {}. Retry attempt: {}. Do not repeat the original task prompt. Focus on the \
             next concrete step, keep edits inside the issue workspace, and leave a clear handoff \
             if you are blocked.",
            issue.identifier,
            issue.title,
            turn_number,
            max_turns,
            attempt
                .map(|value| value.to_string())
                .unwrap_or_else(|| "initial".to_string())
        )
    }
}

/// Load, parse, validate, and compile the repository-owned workflow contract.
pub fn load_workflow_runtime(path: &Path) -> Result<WorkflowRuntime> {
    let definition = load_workflow_definition(path)?;
    let config = build_effective_config(path, &definition)?;
    validate_dispatch_config(&config)?;
    let prompt = Arc::new(PromptRenderer::compile(&definition.prompt_template)?);

    Ok(WorkflowRuntime { config, prompt })
}

/// Read `WORKFLOW.md` and split YAML front matter from the Markdown prompt body.
pub fn load_workflow_definition(path: &Path) -> Result<WorkflowDefinition> {
    if !path.exists() {
        return Err(SymphonyError::MissingWorkflowFile(path.to_path_buf()));
    }

    let content = std::fs::read_to_string(path)?;
    parse_workflow_definition(&content)
}

/// Parse the raw `WORKFLOW.md` contents into front matter and prompt body.
pub fn parse_workflow_definition(content: &str) -> Result<WorkflowDefinition> {
    let trimmed = content.replace("\r\n", "\n");
    let (config, prompt_template) = if let Some(rest) = trimmed.strip_prefix("---\n") {
        let mut parts = rest.splitn(2, "\n---\n");
        let yaml = parts
            .next()
            .ok_or_else(|| SymphonyError::WorkflowParseError("missing front matter body".into()))?;
        let body = parts.next().ok_or_else(|| {
            SymphonyError::WorkflowParseError("missing closing front matter delimiter".into())
        })?;
        (parse_front_matter(yaml)?, body.trim().to_string())
    } else {
        (BTreeMap::new(), trimmed.trim().to_string())
    };

    Ok(WorkflowDefinition {
        config,
        prompt_template,
    })
}

/// Start a best-effort file watcher that notifies the runtime when the workflow changes.
pub fn start_workflow_watch(
    path: PathBuf,
    tx: mpsc::UnboundedSender<()>,
) -> Result<RecommendedWatcher> {
    let watched_file = path.clone();
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) => {
                let relevant = event
                    .paths
                    .iter()
                    .any(|candidate| candidate == &watched_file);
                if relevant || event.paths.is_empty() {
                    let _ = tx.send(());
                }
            }
            Err(error) => {
                warn!(error = %error, "workflow watcher emitted an error");
                let _ = tx.send(());
            }
        })?;

    let watch_root = path.parent().unwrap_or_else(|| Path::new("."));
    watcher.watch(watch_root, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Validate the subset of config needed to poll and dispatch work.
pub fn validate_dispatch_config(config: &EffectiveConfig) -> Result<()> {
    if config.tracker.kind.trim().is_empty() {
        return Err(SymphonyError::UnsupportedTrackerKind(String::new()));
    }
    if config.tracker.kind != "linear" {
        return Err(SymphonyError::UnsupportedTrackerKind(
            config.tracker.kind.clone(),
        ));
    }
    if config.tracker.api_key.trim().is_empty() {
        return Err(SymphonyError::MissingTrackerApiKey);
    }
    if config.tracker.project_slug.trim().is_empty() {
        return Err(SymphonyError::MissingTrackerProjectSlug);
    }
    if config.codex.command.trim().is_empty() {
        return Err(SymphonyError::InvalidConfig(
            "codex.command must be present and non-empty".into(),
        ));
    }

    Ok(())
}

fn parse_front_matter(yaml: &str) -> Result<BTreeMap<String, Value>> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
    let mapping = match value {
        serde_yaml::Value::Mapping(mapping) => mapping,
        _ => return Err(SymphonyError::WorkflowFrontMatterNotAMap),
    };

    let json_value = serde_json::to_value(mapping)?;
    let object = json_value
        .as_object()
        .ok_or(SymphonyError::WorkflowFrontMatterNotAMap)?;

    Ok(object
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn build_effective_config(path: &Path, definition: &WorkflowDefinition) -> Result<EffectiveConfig> {
    let raw = serde_json::from_value::<RawWorkflowConfig>(Value::Object(
        definition
            .config
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    ))
    .map_err(|error| SymphonyError::WorkflowParseError(error.to_string()))?;

    let tracker = build_tracker_settings(raw.tracker.unwrap_or_default())?;
    let polling = PollingSettings {
        interval_ms: parse_u64(
            raw.polling.and_then(|value| value.interval_ms),
            DEFAULT_POLL_INTERVAL_MS,
        ),
    };

    let workspace = WorkspaceSettings {
        root: expand_workspace_root(raw.workspace.and_then(|value| value.root))?,
    };

    let hooks = {
        let raw_hooks = raw.hooks.unwrap_or_default();
        HookSettings {
            after_create: raw_hooks.after_create,
            before_run: raw_hooks.before_run,
            after_run: raw_hooks.after_run,
            before_remove: raw_hooks.before_remove,
            timeout_ms: parse_hook_timeout(raw_hooks.timeout_ms),
        }
    };

    let agent = {
        let raw_agent = raw.agent.unwrap_or_default();
        AgentSettings {
            max_concurrent_agents: parse_usize(
                raw_agent.max_concurrent_agents,
                DEFAULT_MAX_CONCURRENT_AGENTS,
            ),
            max_turns: parse_u32(raw_agent.max_turns, DEFAULT_MAX_TURNS),
            max_retry_backoff_ms: parse_u64(
                raw_agent.max_retry_backoff_ms,
                DEFAULT_MAX_RETRY_BACKOFF_MS,
            ),
            max_concurrent_agents_by_state: raw_agent
                .max_concurrent_agents_by_state
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(state, limit)| {
                    let parsed = parse_i64(Some(limit), -1);
                    (parsed > 0)
                        .then(|| (EffectiveConfig::normalize_state(&state), parsed as usize))
                })
                .collect(),
        }
    };

    let codex = {
        let raw_codex = raw.codex.unwrap_or_default();
        CodexSettings {
            command: raw_codex
                .command
                .unwrap_or_else(|| "codex app-server".to_string()),
            approval_policy: raw_codex
                .approval_policy
                .unwrap_or_else(|| Value::String("never".to_string())),
            thread_sandbox: raw_codex
                .thread_sandbox
                .unwrap_or_else(|| Value::String("danger-full-access".to_string())),
            turn_sandbox_policy: raw_codex.turn_sandbox_policy.unwrap_or_else(|| {
                Value::Object(
                    [(
                        "type".to_string(),
                        Value::String("dangerFullAccess".to_string()),
                    )]
                    .into_iter()
                    .collect(),
                )
            }),
            turn_timeout_ms: parse_u64(raw_codex.turn_timeout_ms, DEFAULT_TURN_TIMEOUT_MS),
            read_timeout_ms: parse_u64(raw_codex.read_timeout_ms, DEFAULT_READ_TIMEOUT_MS),
            stall_timeout_ms: parse_i64(raw_codex.stall_timeout_ms, DEFAULT_STALL_TIMEOUT_MS),
        }
    };

    let server = ServerSettings {
        port: raw.server.and_then(|value| value.port),
    };

    Ok(EffectiveConfig {
        workflow_path: path.to_path_buf(),
        prompt_template: definition.prompt_template.clone(),
        tracker,
        polling,
        workspace,
        hooks,
        agent,
        codex,
        server,
    })
}

fn build_tracker_settings(raw: RawTracker) -> Result<TrackerSettings> {
    let kind = raw.kind.unwrap_or_else(|| "linear".to_string());
    let endpoint = raw
        .endpoint
        .unwrap_or_else(|| DEFAULT_LINEAR_ENDPOINT.to_string());
    let api_key = resolve_env_token(raw.api_key).unwrap_or_else(|| {
        env::var("LINEAR_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_default()
    });
    let project_slug = raw.project_slug.unwrap_or_default();

    Ok(TrackerSettings {
        kind,
        endpoint,
        api_key,
        project_slug,
        active_states: parse_string_list(
            raw.active_states,
            vec!["Todo".to_string(), "In Progress".to_string()],
        ),
        terminal_states: parse_string_list(
            raw.terminal_states,
            vec![
                "Closed".to_string(),
                "Cancelled".to_string(),
                "Canceled".to_string(),
                "Duplicate".to_string(),
                "Done".to_string(),
            ],
        ),
    })
}

fn expand_workspace_root(root: Option<String>) -> Result<PathBuf> {
    let value = root.unwrap_or_else(|| {
        std::env::temp_dir()
            .join("symphony_workspaces")
            .to_string_lossy()
            .to_string()
    });
    let value = resolve_env_token(Some(value)).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("symphony_workspaces")
            .to_string_lossy()
            .to_string()
    });

    let expanded = if value == "~" {
        home_dir()?.to_string_lossy().to_string()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home_dir()?.join(rest).to_string_lossy().to_string()
    } else {
        value
    };

    Ok(PathBuf::from(expanded))
}

fn home_dir() -> Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| SymphonyError::InvalidConfig("HOME is not set".into()))
}

fn parse_string_list(value: Option<StringOrVec>, default: Vec<String>) -> Vec<String> {
    match value {
        Some(StringOrVec::One(value)) => value
            .split(',')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(StringOrVec::Many(values)) => values,
        None => default,
    }
}

fn resolve_env_token(value: Option<String>) -> Option<String> {
    value.and_then(|candidate| {
        if let Some(name) = candidate.strip_prefix('$') {
            let resolved = env::var(name).ok()?;
            (!resolved.trim().is_empty()).then_some(resolved)
        } else {
            Some(candidate)
        }
    })
}

fn parse_hook_timeout(value: Option<IntLike>) -> u64 {
    let parsed = parse_i64(value, DEFAULT_HOOK_TIMEOUT_MS as i64);
    if parsed <= 0 {
        DEFAULT_HOOK_TIMEOUT_MS
    } else {
        parsed as u64
    }
}

fn parse_u64(value: Option<IntLike>, default: u64) -> u64 {
    let parsed = parse_i64(value, default as i64);
    if parsed < 0 { default } else { parsed as u64 }
}

fn parse_u32(value: Option<IntLike>, default: u32) -> u32 {
    let parsed = parse_i64(value, default as i64);
    if parsed < 0 { default } else { parsed as u32 }
}

fn parse_usize(value: Option<IntLike>, default: usize) -> usize {
    let parsed = parse_i64(value, default as i64);
    if parsed <= 0 {
        default
    } else {
        parsed as usize
    }
}

fn parse_i64(value: Option<IntLike>, default: i64) -> i64 {
    match value {
        Some(IntLike::Integer(value)) => value,
        Some(IntLike::String(value)) => value.parse::<i64>().unwrap_or(default),
        None => default,
    }
}

fn json_to_liquid(value: &Value) -> Result<liquid::model::Value> {
    Ok(match value {
        Value::Null => liquid::model::Value::Nil,
        Value::Bool(value) => liquid::model::Value::scalar(*value),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                liquid::model::Value::scalar(integer)
            } else if let Some(float) = value.as_f64() {
                liquid::model::Value::scalar(float)
            } else {
                return Err(SymphonyError::TemplateRenderError(
                    "unsupported number".to_string(),
                ));
            }
        }
        Value::String(value) => liquid::model::Value::scalar(value.clone()),
        Value::Array(values) => {
            let mut array = Vec::with_capacity(values.len());
            for value in values {
                array.push(json_to_liquid(value)?);
            }
            liquid::model::Value::Array(array)
        }
        Value::Object(values) => {
            let mut object = Object::new();
            for (key, value) in values {
                object.insert(key.clone().into(), json_to_liquid(value)?);
            }
            liquid::model::Value::Object(object)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workflow_without_front_matter() {
        let definition = parse_workflow_definition("hello").unwrap();
        assert!(definition.config.is_empty());
        assert_eq!(definition.prompt_template, "hello");
    }

    #[test]
    fn parses_front_matter_into_object() {
        let definition =
            parse_workflow_definition("---\ntracker:\n  kind: linear\n---\nhello").unwrap();
        assert_eq!(
            definition
                .config
                .get("tracker")
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("linear")
        );
        assert_eq!(definition.prompt_template, "hello");
    }

    #[test]
    fn rejects_non_object_front_matter() {
        let error = parse_workflow_definition("---\n- nope\n---\nhello").unwrap_err();
        assert!(matches!(error, SymphonyError::WorkflowFrontMatterNotAMap));
    }

    #[test]
    fn expands_env_tokens_and_defaults() {
        unsafe {
            std::env::set_var("LINEAR_API_KEY", "test-key");
        }
        let definition = parse_workflow_definition(
            "---\ntracker:\n  kind: linear\n  project_slug: proj\nworkspace:\n  root: tmp/work\n---\nhello",
        )
        .unwrap();
        let config = build_effective_config(Path::new("WORKFLOW.md"), &definition).unwrap();
        assert_eq!(config.tracker.api_key, "test-key");
        assert_eq!(config.workspace.root, PathBuf::from("tmp/work"));
    }

    #[test]
    fn template_render_fails_for_unknown_variable() {
        let renderer = PromptRenderer::compile("{{ issue.unknown_field }}").unwrap();
        let issue = Issue {
            id: "1".into(),
            identifier: "ABC-1".into(),
            title: "Example".into(),
            ..Issue::default()
        };
        let error = renderer.render_issue_prompt(&issue, None).unwrap_err();
        assert!(matches!(error, SymphonyError::TemplateRenderError(_)));
    }

    #[test]
    fn validates_dispatch_config_and_rejects_missing_fields() {
        let definition = parse_workflow_definition(
            "---\ntracker:\n  kind: linear\n  project_slug: proj\ncodex:\n  command: codex app-server\n---\nhello",
        )
        .unwrap();
        unsafe {
            std::env::set_var("LINEAR_API_KEY", "test-key");
        }
        let config = build_effective_config(Path::new("WORKFLOW.md"), &definition).unwrap();
        validate_dispatch_config(&config).expect("config should validate");

        let mut missing_api_key = config.clone();
        missing_api_key.tracker.api_key.clear();
        assert!(matches!(
            validate_dispatch_config(&missing_api_key),
            Err(SymphonyError::MissingTrackerApiKey)
        ));

        let mut missing_project = config.clone();
        missing_project.tracker.project_slug.clear();
        assert!(matches!(
            validate_dispatch_config(&missing_project),
            Err(SymphonyError::MissingTrackerProjectSlug)
        ));
    }

    #[test]
    fn build_effective_config_applies_defaults_and_normalizes_limits() {
        unsafe {
            std::env::set_var("WORKSPACE_ROOT", "/tmp/symphony-tests");
            std::env::set_var("LINEAR_API_KEY", "test-key");
        }
        let definition = parse_workflow_definition(
            "---\ntracker:\n  kind: linear\n  project_slug: proj\nworkspace:\n  root: $WORKSPACE_ROOT\nhooks:\n  timeout_ms: 0\nagent:\n  max_concurrent_agents_by_state:\n    Todo: 3\n    Bad: 0\n---\nhello",
        )
        .unwrap();
        let config = build_effective_config(Path::new("WORKFLOW.md"), &definition).unwrap();

        assert_eq!(config.workspace.root, PathBuf::from("/tmp/symphony-tests"));
        assert_eq!(config.hooks.timeout_ms, DEFAULT_HOOK_TIMEOUT_MS);
        assert_eq!(
            config.agent.max_concurrent_agents_by_state.get("todo"),
            Some(&3)
        );
        assert!(
            !config
                .agent
                .max_concurrent_agents_by_state
                .contains_key("bad")
        );
        assert_eq!(config.codex.command, "codex app-server");
    }

    #[test]
    fn expands_home_and_parses_string_lists() {
        unsafe {
            std::env::set_var("HOME", "/tmp/home");
        }
        assert_eq!(
            expand_workspace_root(Some(String::from("~/workspace"))).unwrap(),
            PathBuf::from("/tmp/home/workspace")
        );
        assert_eq!(
            parse_string_list(
                Some(StringOrVec::One(String::from("Todo, In Progress , Done"))),
                Vec::new()
            ),
            vec!["Todo", "In Progress", "Done"]
        );
    }

    #[test]
    fn renders_prompt_and_converts_json_to_liquid_values() {
        let renderer = PromptRenderer::compile(
            "{{ issue.identifier }} {{ issue.labels[0] }} {{ attempt | default: 0 }}",
        )
        .unwrap();
        let issue = Issue {
            id: "1".into(),
            identifier: "ABC-1".into(),
            title: "Example".into(),
            labels: vec![String::from("bug")],
            ..Issue::default()
        };
        let prompt = renderer
            .render_issue_prompt(&issue, Some(2))
            .expect("prompt should render");
        assert_eq!(prompt, "ABC-1 bug 2");

        let liquid = json_to_liquid(&serde_json::json!({
            "nested": ["one", 2, true]
        }))
        .expect("nested json should convert");
        assert!(matches!(liquid, liquid::model::Value::Object(_)));
    }
}
