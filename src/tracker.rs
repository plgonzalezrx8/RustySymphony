use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use graphql_parser::parse_query;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    error::{Result, SymphonyError},
    types::{BlockerRef, EffectiveConfig, Issue},
};

const LINEAR_PAGE_SIZE: usize = 50;

/// Stable tracker interface used by the orchestrator and worker runtime.
#[async_trait]
pub trait IssueTracker: Send + Sync {
    /// Fetch dispatch candidates using configured active states.
    async fn fetch_candidate_issues(&self, config: &EffectiveConfig) -> Result<Vec<Issue>>;
    /// Fetch issues in specific states, primarily for startup terminal cleanup.
    async fn fetch_issues_by_states(
        &self,
        config: &EffectiveConfig,
        state_names: &[String],
    ) -> Result<Vec<Issue>>;
    /// Fetch current issue state snapshots for already-running issues.
    async fn fetch_issue_states_by_ids(
        &self,
        config: &EffectiveConfig,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>>;
    /// Execute one raw GraphQL operation for the optional `linear_graphql` tool.
    async fn execute_raw_graphql(
        &self,
        config: &EffectiveConfig,
        query: &str,
        variables: Option<Value>,
    ) -> Result<Value>;
}

/// Build the configured tracker adapter.
pub fn build_tracker(config: &EffectiveConfig) -> Result<Arc<dyn IssueTracker>> {
    match config.tracker.kind.as_str() {
        "linear" => Ok(Arc::new(LinearTracker::new()?)),
        other => Err(SymphonyError::UnsupportedTrackerKind(other.to_string())),
    }
}

/// Linear GraphQL tracker adapter used by the initial specification.
#[derive(Debug, Clone)]
pub struct LinearTracker {
    client: reqwest::Client,
}

impl LinearTracker {
    /// Create a client with the spec's 30-second network timeout.
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { client })
    }

    async fn fetch_issues_paginated(
        &self,
        config: &EffectiveConfig,
        state_names: &[String],
    ) -> Result<Vec<Issue>> {
        if state_names.is_empty() {
            return Ok(Vec::new());
        }

        let mut issues = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let response = self
                .post_graphql(
                    config,
                    ISSUE_COLLECTION_QUERY,
                    json!({
                        "projectSlug": config.tracker.project_slug,
                        "states": state_names,
                        "first": LINEAR_PAGE_SIZE,
                        "after": after,
                    }),
                )
                .await?;

            let nodes = response
                .pointer("/data/issues/nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    SymphonyError::LinearUnknownPayload("missing issues.nodes".into())
                })?;
            for node in nodes {
                issues.push(normalize_issue(node));
            }

            let page_info = response
                .pointer("/data/issues/pageInfo")
                .ok_or_else(|| SymphonyError::LinearUnknownPayload("missing pageInfo".into()))?;
            let has_next_page = page_info
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_next_page {
                break;
            }

            let end_cursor = page_info
                .get("endCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or(SymphonyError::LinearMissingEndCursor)?;
            after = Some(end_cursor);
        }

        Ok(issues)
    }

    async fn post_graphql(
        &self,
        config: &EffectiveConfig,
        query: &str,
        variables: Value,
    ) -> Result<Value> {
        debug!(endpoint = %config.tracker.endpoint, "sending Linear GraphQL request");

        let response = self
            .client
            .post(&config.tracker.endpoint)
            .header("Authorization", config.tracker.api_key.trim())
            .json(&json!({
                "query": query,
                "variables": variables,
            }))
            .send()
            .await
            .map_err(|error| SymphonyError::LinearApiRequest(error.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| SymphonyError::LinearApiRequest(error.to_string()))?;

        if status != StatusCode::OK {
            return Err(SymphonyError::LinearApiStatus {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: Value = serde_json::from_str(&body)?;
        if let Some(errors) = parsed.get("errors") {
            return Err(SymphonyError::LinearGraphqlErrors(errors.to_string()));
        }

        Ok(parsed)
    }
}

#[async_trait]
impl IssueTracker for LinearTracker {
    async fn fetch_candidate_issues(&self, config: &EffectiveConfig) -> Result<Vec<Issue>> {
        self.fetch_issues_paginated(config, &config.tracker.active_states)
            .await
    }

    async fn fetch_issues_by_states(
        &self,
        config: &EffectiveConfig,
        state_names: &[String],
    ) -> Result<Vec<Issue>> {
        self.fetch_issues_paginated(config, state_names).await
    }

    async fn fetch_issue_states_by_ids(
        &self,
        config: &EffectiveConfig,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>> {
        if issue_ids.is_empty() {
            return Ok(Vec::new());
        }

        let response = self
            .post_graphql(
                config,
                ISSUE_STATE_QUERY,
                json!({
                    "ids": issue_ids,
                }),
            )
            .await?;

        let nodes = response
            .pointer("/data/issues/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| SymphonyError::LinearUnknownPayload("missing issues.nodes".into()))?;

        Ok(nodes.iter().map(normalize_issue).collect())
    }

    async fn execute_raw_graphql(
        &self,
        config: &EffectiveConfig,
        query: &str,
        variables: Option<Value>,
    ) -> Result<Value> {
        let document = parse_query::<String>(query).map_err(|error| {
            SymphonyError::InvalidConfig(format!("invalid graphql query: {error}"))
        })?;
        let operation_count = document
            .definitions
            .iter()
            .filter(|definition| {
                matches!(definition, graphql_parser::query::Definition::Operation(_))
            })
            .count();
        if operation_count != 1 {
            return Err(SymphonyError::InvalidConfig(
                "query must contain exactly one GraphQL operation".into(),
            ));
        }

        let variables = variables.unwrap_or_else(|| json!({}));
        if !variables.is_object() {
            return Err(SymphonyError::InvalidConfig(
                "variables must be a JSON object".into(),
            ));
        }

        self.post_graphql(config, query, variables).await
    }
}

const ISSUE_COLLECTION_QUERY: &str = r#"
query SymphonyIssues($projectSlug: String!, $states: [String!], $first: Int!, $after: String) {
  issues(
    filter: {
      project: { slugId: { eq: $projectSlug } }
      state: { name: { in: $states } }
    }
    first: $first
    after: $after
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      branchName
      url
      createdAt
      updatedAt
      state { name }
      labels { nodes { name } }
      inverseRelations {
        nodes {
          type
          issue {
            id
            identifier
            state { name }
          }
          sourceIssue {
            id
            identifier
            state { name }
          }
        }
      }
    }
    pageInfo {
      hasNextPage
      endCursor
    }
  }
}
"#;

const ISSUE_STATE_QUERY: &str = r#"
query SymphonyIssueStates($ids: [ID!]) {
  issues(filter: { id: { in: $ids } }) {
    nodes {
      id
      identifier
      title
      description
      priority
      branchName
      url
      createdAt
      updatedAt
      state { name }
      labels { nodes { name } }
      inverseRelations {
        nodes {
          type
          issue {
            id
            identifier
            state { name }
          }
          sourceIssue {
            id
            identifier
            state { name }
          }
        }
      }
    }
  }
}
"#;

fn normalize_issue(node: &Value) -> Issue {
    Issue {
        id: node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        identifier: node
            .get("identifier")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: node
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: node
            .get("description")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        priority: node.get("priority").and_then(Value::as_i64),
        state: node
            .pointer("/state/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        branch_name: node
            .get("branchName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        url: node
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        labels: node
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|label| label.get("name").and_then(Value::as_str))
                    .map(|label| label.to_lowercase())
                    .collect()
            })
            .unwrap_or_default(),
        blocked_by: normalize_blockers(node),
        created_at: parse_timestamp(node.get("createdAt")),
        updated_at: parse_timestamp(node.get("updatedAt")),
    }
}

fn normalize_blockers(node: &Value) -> Vec<BlockerRef> {
    node.pointer("/inverseRelations/nodes")
        .and_then(Value::as_array)
        .map(|relations| {
            relations
                .iter()
                .filter(|relation| {
                    relation
                        .get("type")
                        .and_then(Value::as_str)
                        .map(|value| value.eq_ignore_ascii_case("blocks"))
                        .unwrap_or(true)
                })
                .map(|relation| {
                    let blocker = relation
                        .get("issue")
                        .filter(|value| !value.is_null())
                        .or_else(|| relation.get("sourceIssue"))
                        .unwrap_or(&Value::Null);
                    BlockerRef {
                        id: blocker
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        identifier: blocker
                            .get("identifier")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        state: blocker
                            .pointer("/state/name")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_labels_and_blockers() {
        let issue = normalize_issue(&json!({
            "id": "1",
            "identifier": "ABC-1",
            "title": "Example",
            "priority": 2,
            "state": { "name": "Todo" },
            "labels": { "nodes": [{ "name": "Bug" }] },
            "inverseRelations": {
                "nodes": [{
                    "type": "blocks",
                    "issue": {
                        "id": "2",
                        "identifier": "ABC-2",
                        "state": { "name": "In Progress" }
                    }
                }]
            }
        }));
        assert_eq!(issue.labels, vec!["bug"]);
        assert_eq!(issue.blocked_by.len(), 1);
    }
}
