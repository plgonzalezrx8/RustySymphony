use std::path::{Component, Path, PathBuf};

use chrono::Utc;
use tokio::{
    fs,
    process::Command,
    time::{Duration, timeout},
};
use tracing::{debug, warn};

use crate::{
    error::{Result, SymphonyError},
    types::{EffectiveConfig, Workspace},
};

/// Manage deterministic per-issue workspaces and their lifecycle hooks.
#[derive(Debug, Clone)]
pub struct WorkspaceManager;

impl WorkspaceManager {
    /// Create or reuse the workspace for one issue and run `after_create` when needed.
    pub async fn ensure_workspace(
        &self,
        issue_identifier: &str,
        config: &EffectiveConfig,
    ) -> Result<Workspace> {
        let root = normalize_to_absolute(&config.workspace.root)?;
        fs::create_dir_all(&root).await?;

        let workspace_key = sanitize_issue_identifier(issue_identifier);
        let path = root.join(&workspace_key);
        let absolute_path = normalize_to_absolute(&path)?;
        validate_workspace_containment(&root, &absolute_path)?;

        let metadata = fs::metadata(&absolute_path).await.ok();
        let created_now = match metadata {
            Some(metadata) if metadata.is_dir() => false,
            Some(_) => {
                return Err(SymphonyError::InvalidConfig(format!(
                    "workspace path exists and is not a directory: {}",
                    absolute_path.display()
                )));
            }
            None => {
                fs::create_dir_all(&absolute_path).await?;
                true
            }
        };

        cleanup_ephemeral_artifacts(&absolute_path).await?;

        if created_now
            && let Some(script) = &config.hooks.after_create
            && let Err(error) = self
                .run_hook("after_create", script, &absolute_path, config)
                .await
        {
            let _ = fs::remove_dir_all(&absolute_path).await;
            return Err(error);
        }

        Ok(Workspace {
            path: absolute_path,
            workspace_key,
            created_now,
        })
    }

    /// Run the hook configured for a workspace attempt or lifecycle transition.
    pub async fn run_hook(
        &self,
        hook_name: &str,
        script: &str,
        workspace_path: &Path,
        config: &EffectiveConfig,
    ) -> Result<()> {
        let cwd = normalize_to_absolute(workspace_path)?;
        let root = normalize_to_absolute(&config.workspace.root)?;
        validate_workspace_containment(&root, &cwd)?;

        debug!(hook = hook_name, cwd = %cwd.display(), "running workspace hook");

        let mut command = Command::new("bash");
        command.arg("-lc").arg(script).current_dir(&cwd);

        let output = timeout(
            Duration::from_millis(config.hooks.timeout_ms),
            command.output(),
        )
        .await
        .map_err(|_| SymphonyError::HookFailed {
            hook: hook_name.to_string(),
            message: format!("hook timed out after {}ms", config.hooks.timeout_ms),
        })??;

        if !output.status.success() {
            return Err(SymphonyError::HookFailed {
                hook: hook_name.to_string(),
                message: truncate_hook_output(&String::from_utf8_lossy(&output.stderr)),
            });
        }

        Ok(())
    }

    /// Run a best-effort hook where failures should be logged but ignored.
    pub async fn run_hook_best_effort(
        &self,
        hook_name: &str,
        script: Option<&str>,
        workspace_path: &Path,
        config: &EffectiveConfig,
    ) {
        if let Some(script) = script
            && let Err(error) = self
                .run_hook(hook_name, script, workspace_path, config)
                .await
        {
            warn!(hook = hook_name, error = %error, "workspace hook failed");
        }
    }

    /// Remove the workspace for a terminal issue after running `before_remove`.
    pub async fn cleanup_workspace(
        &self,
        issue_identifier: &str,
        config: &EffectiveConfig,
    ) -> Result<()> {
        let root = normalize_to_absolute(&config.workspace.root)?;
        let workspace_key = sanitize_issue_identifier(issue_identifier);
        let path = normalize_to_absolute(&root.join(workspace_key))?;
        validate_workspace_containment(&root, &path)?;

        if fs::metadata(&path).await.is_err() {
            return Ok(());
        }

        self.run_hook_best_effort(
            "before_remove",
            config.hooks.before_remove.as_deref(),
            &path,
            config,
        )
        .await;
        fs::remove_dir_all(&path).await?;
        Ok(())
    }

    /// Return the stable per-issue session log path under the workspace root.
    pub fn session_log_path(&self, root: &Path, workspace_key: &str) -> Result<PathBuf> {
        let absolute_root = normalize_to_absolute(root)?;
        Ok(absolute_root
            .join(".symphony_logs")
            .join(workspace_key)
            .join("latest.log"))
    }
}

/// Sanitize an issue identifier so it is safe for directory naming.
pub fn sanitize_issue_identifier(identifier: &str) -> String {
    identifier
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Normalize a path to an absolute path without requiring filesystem existence.
pub fn normalize_to_absolute(path: &Path) -> Result<PathBuf> {
    let base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir()?
    };

    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    Ok(normalized)
}

/// Ensure a workspace path cannot escape the configured workspace root.
pub fn validate_workspace_containment(root: &Path, path: &Path) -> Result<()> {
    let absolute_root = normalize_to_absolute(root)?;
    let absolute_path = normalize_to_absolute(path)?;

    if absolute_path.starts_with(&absolute_root) {
        Ok(())
    } else {
        Err(SymphonyError::WorkspacePathEscape {
            root: absolute_root,
            path: absolute_path,
        })
    }
}

async fn cleanup_ephemeral_artifacts(workspace_path: &Path) -> Result<()> {
    for candidate in ["tmp", ".elixir_ls"] {
        let path = workspace_path.join(candidate);
        if let Ok(metadata) = fs::metadata(&path).await {
            if metadata.is_dir() {
                let _ = fs::remove_dir_all(&path).await;
            } else {
                let _ = fs::remove_file(&path).await;
            }
        }
    }
    Ok(())
}

fn truncate_hook_output(output: &str) -> String {
    let cleaned = output.trim();
    if cleaned.len() > 300 {
        format!("{}...", &cleaned[..300])
    } else if cleaned.is_empty() {
        "hook exited with a non-zero status".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Create the parent directories and truncate the session log before a new worker run.
pub async fn initialize_session_log(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(
        path,
        format!("session log initialized at {}\n", Utc::now().to_rfc3339()),
    )
    .await?;
    Ok(())
}

/// Append one line to the session log without failing the worker on log I/O issues.
pub async fn append_session_log(path: &Path, line: &str) {
    let result = async {
        let mut current = if fs::metadata(path).await.is_ok() {
            fs::read_to_string(path).await?
        } else {
            String::new()
        };
        current.push_str(line);
        current.push('\n');
        fs::write(path, current).await
    }
    .await;

    if let Err(error) = result {
        warn!(error = %error, path = %path.display(), "failed to append session log");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_workspace_names() {
        assert_eq!(
            sanitize_issue_identifier("ABC-123 / test"),
            "ABC-123___test"
        );
    }

    #[test]
    fn rejects_paths_outside_root() {
        let root = PathBuf::from("/tmp/root");
        let path = PathBuf::from("/tmp/other");
        assert!(matches!(
            validate_workspace_containment(&root, &path),
            Err(SymphonyError::WorkspacePathEscape { .. })
        ));
    }
}
