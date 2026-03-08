#!/usr/bin/env bash
set -euo pipefail

# Apply the required CI status checks to the main branch once the remote branch
# exists. This is intentionally separate from the build workflows so repository
# admins can opt in after the first push creates the branch on GitHub.

required_checks=("fmt" "clippy" "unit-tests" "release-smoke")
checks_json='["fmt","clippy","unit-tests","release-smoke"]'
branch="${1:-main}"
repo="${GITHUB_REPOSITORY:-}"

if [[ -z "${repo}" ]]; then
  repo="$(gh repo view --json nameWithOwner --jq '.nameWithOwner')"
fi

if ! gh auth status >/dev/null 2>&1; then
  echo "GitHub CLI authentication is required before branch protection can be configured." >&2
  exit 1
fi

if ! gh api "repos/${repo}/branches/${branch}" >/dev/null 2>&1; then
  echo "Remote branch '${branch}' does not exist in ${repo} yet. Push it first, then rerun this script." >&2
  exit 1
fi

gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  "repos/${repo}/branches/${branch}/protection" \
  --input - <<JSON
{
  "required_status_checks": {
    "strict": true,
    "contexts": ${checks_json}
  },
  "enforce_admins": false,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "required_conversation_resolution": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "block_creations": false,
  "lock_branch": false,
  "allow_fork_syncing": true
}
JSON

echo "Protected ${repo}:${branch} with required checks: ${required_checks[*]}"
