mod common;

use symphony::{
    types::HookSettings,
    workspace::{WorkspaceManager, normalize_to_absolute},
};

use common::{temp_workspace, test_config};

#[tokio::test]
async fn after_create_runs_only_on_new_workspace() {
    let (_tempdir, workspace_root) = temp_workspace();
    let mut config = test_config(
        "http://example.com".into(),
        workspace_root,
        "codex app-server".into(),
    );
    config.hooks = HookSettings {
        after_create: Some("printf created > created.txt".into()),
        timeout_ms: 250,
        ..HookSettings::default()
    };

    let manager = WorkspaceManager;
    let first = manager
        .ensure_workspace("ABC-1", &config)
        .await
        .expect("workspace should be created");
    let second = manager
        .ensure_workspace("ABC-1", &config)
        .await
        .expect("workspace should be reused");

    assert!(first.created_now);
    assert!(!second.created_now);
    assert!(
        tokio::fs::metadata(first.path.join("created.txt"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn hook_timeout_is_surfaced() {
    let (_tempdir, workspace_root) = temp_workspace();
    let mut config = test_config(
        "http://example.com".into(),
        workspace_root,
        "codex app-server".into(),
    );
    config.hooks.timeout_ms = 50;

    let manager = WorkspaceManager;
    let workspace = manager
        .ensure_workspace("ABC-1", &config)
        .await
        .expect("workspace should exist");

    let error = manager
        .run_hook("before_run", "sleep 1", &workspace.path, &config)
        .await
        .expect_err("timeout should fail the hook");

    assert!(error.to_string().contains("hook timed out"));
}

#[tokio::test]
async fn cleanup_ignores_before_remove_failures() {
    let (_tempdir, workspace_root) = temp_workspace();
    let mut config = test_config(
        "http://example.com".into(),
        workspace_root.clone(),
        "codex app-server".into(),
    );
    config.hooks.before_remove = Some("exit 1".into());

    let manager = WorkspaceManager;
    let workspace = manager
        .ensure_workspace("ABC-1", &config)
        .await
        .expect("workspace should exist");
    let absolute_workspace = normalize_to_absolute(&workspace.path).expect("path should normalize");

    manager
        .cleanup_workspace("ABC-1", &config)
        .await
        .expect("cleanup should continue after a hook failure");

    assert!(tokio::fs::metadata(absolute_workspace).await.is_err());
}
