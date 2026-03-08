# RustySymphony

RustySymphony is a long-running Rust daemon that polls Linear, creates per-issue workspaces, and runs Codex app-server sessions against those workspaces.

## Safety Posture

This implementation ships with a high-trust default configuration:

- `codex.approval_policy: never`
- `codex.thread_sandbox: danger-full-access`
- `codex.turn_sandbox_policy.type: dangerFullAccess`
- command and file-change approvals are auto-approved for the session
- user-input-required turns fail immediately

Use it only in environments where the repository, workflow contract, issue source, and available credentials are intentionally trusted.

## Requirements

- Rust stable toolchain
- `codex` CLI with app-server support
- Linear API token available via `LINEAR_API_KEY` or `tracker.api_key`

## Running

1. Edit [`WORKFLOW.md`](/Users/pedrogonzalez/CascadeProjects/RustySymphony/WORKFLOW.md) so `tracker.project_slug` and any hooks match your environment.
2. Export `LINEAR_API_KEY` if you do not want to store it directly in the workflow file.
3. Start the service:

```bash
cargo run -- ./WORKFLOW.md --port 3000
```

The dashboard is served on loopback only. The JSON API is available under `/api/v1/*`.

## Development

- `cargo check`
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --locked`
- `cargo llvm-cov --workspace --all-features --tests --fail-under-lines 70`

## CI and Release

The repository ships with three GitHub Actions workflows:

- `.github/workflows/ci.yml`: fast required verification for pushes and pull requests. It runs formatting, clippy, tests, and a release-build smoke check.
- `.github/workflows/deep-validation.yml`: deeper validation for `main`, nightly schedules, and manual runs. It enforces 70% line coverage across the hermetic suite and exposes a secrets-gated live integration lane.
- `.github/workflows/release.yml`: semver tag releases for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`, including `SHA256SUMS.txt` and GitHub build provenance attestations.

The live integration lane expects these environment settings when enabled on GitHub:

- `LINEAR_API_KEY`
- `SYMPHONY_LINEAR_PROJECT_SLUG`
- `SYMPHONY_LINEAR_ENDPOINT` (optional override)

After the first push creates the remote `main` branch, a repository admin can apply the required CI checks with:

```bash
./scripts/github/set_branch_protection.sh
```

## Repository Layout

- [`src/orchestrator.rs`](/Users/pedrogonzalez/CascadeProjects/RustySymphony/src/orchestrator.rs): single-owner scheduler, retries, reconciliation, runtime snapshots
- [`src/agent.rs`](/Users/pedrogonzalez/CascadeProjects/RustySymphony/src/agent.rs): Codex app-server worker runtime
- [`src/tracker.rs`](/Users/pedrogonzalez/CascadeProjects/RustySymphony/src/tracker.rs): Linear GraphQL adapter and normalization
- [`src/workflow.rs`](/Users/pedrogonzalez/CascadeProjects/RustySymphony/src/workflow.rs): `WORKFLOW.md` loading, parsing, reload, and prompt rendering
- [`src/workspace.rs`](/Users/pedrogonzalez/CascadeProjects/RustySymphony/src/workspace.rs): workspace creation, containment, hooks, and session logs
