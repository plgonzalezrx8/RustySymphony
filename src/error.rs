use std::{io, path::PathBuf};

use thiserror::Error;

/// Shared result type used across the Symphony runtime.
pub type Result<T> = std::result::Result<T, SymphonyError>;

/// Typed application errors used for startup validation, dispatch, and workers.
#[derive(Debug, Error)]
pub enum SymphonyError {
    #[error("missing_workflow_file: {0}")]
    MissingWorkflowFile(PathBuf),
    #[error("workflow_parse_error: {0}")]
    WorkflowParseError(String),
    #[error("workflow_front_matter_not_a_map")]
    WorkflowFrontMatterNotAMap,
    #[error("template_parse_error: {0}")]
    TemplateParseError(String),
    #[error("template_render_error: {0}")]
    TemplateRenderError(String),
    #[error("unsupported_tracker_kind: {0}")]
    UnsupportedTrackerKind(String),
    #[error("missing_tracker_api_key")]
    MissingTrackerApiKey,
    #[error("missing_tracker_project_slug")]
    MissingTrackerProjectSlug,
    #[error("invalid_config: {0}")]
    InvalidConfig(String),
    #[error("linear_api_request: {0}")]
    LinearApiRequest(String),
    #[error("linear_api_status: status={status} body={body}")]
    LinearApiStatus { status: u16, body: String },
    #[error("linear_graphql_errors: {0}")]
    LinearGraphqlErrors(String),
    #[error("linear_unknown_payload: {0}")]
    LinearUnknownPayload(String),
    #[error("linear_missing_end_cursor")]
    LinearMissingEndCursor,
    #[error("workspace_path_escape: root={root:?} path={path:?}")]
    WorkspacePathEscape { root: PathBuf, path: PathBuf },
    #[error("invalid_workspace_cwd: {0}")]
    InvalidWorkspaceCwd(String),
    #[error("hook_failed: hook={hook} message={message}")]
    HookFailed { hook: String, message: String },
    #[error("response_timeout")]
    ResponseTimeout,
    #[error("turn_timeout")]
    TurnTimeout,
    #[error("port_exit")]
    PortExit,
    #[error("response_error: {0}")]
    ResponseError(String),
    #[error("turn_failed: {0}")]
    TurnFailed(String),
    #[error("turn_cancelled")]
    TurnCancelled,
    #[error("turn_input_required")]
    TurnInputRequired,
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}
