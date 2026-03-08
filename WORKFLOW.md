---
tracker:
  kind: linear
  project_slug: CHANGE_ME
  api_key: $LINEAR_API_KEY
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
    - Cancelled
    - Canceled
    - Duplicate
    - Closed
polling:
  interval_ms: 30000
workspace:
  root: ./symphony_workspaces
hooks:
  timeout_ms: 60000
agent:
  max_concurrent_agents: 4
  max_turns: 20
  max_retry_backoff_ms: 300000
codex:
  command: codex app-server
  approval_policy: never
  thread_sandbox: danger-full-access
  turn_sandbox_policy:
    type: dangerFullAccess
server:
  port: 3000
---
# Symphony Workflow

You are working on the Linear issue `{{ issue.identifier }}` titled `{{ issue.title }}`.

Rules:

- Work only inside the issue workspace.
- Read the existing codebase before making changes.
- Leave the issue in a clear handoff state if you cannot finish it.
- If you update Linear, keep the comments concise and factual.

Issue details:

- State: `{{ issue.state }}`
- Priority: `{{ issue.priority }}`
- URL: `{{ issue.url }}`

Description:

{{ issue.description }}
