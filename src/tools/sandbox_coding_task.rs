use super::traits::{Tool, ToolResult};
use crate::config::SandboxWorkflowConfig;
use crate::sandbox_workflow::state::TaskType;
use crate::sandbox_workflow::{Orchestrator, WorkflowParams};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// Kicks off a sandbox workflow: E2B sandbox + Claude Code + Slack interaction.
/// Supports research, data, admin, and eng (PR) task types.
pub struct SandboxCodingTaskTool {
    config: SandboxWorkflowConfig,
    workspace_dir: PathBuf,
}

impl SandboxCodingTaskTool {
    pub fn new(config: SandboxWorkflowConfig, workspace_dir: &std::path::Path) -> Self {
        Self {
            config,
            workspace_dir: workspace_dir.to_path_buf(),
        }
    }
}

#[async_trait]
impl Tool for SandboxCodingTaskTool {
    fn name(&self) -> &str {
        "sandbox_coding_task"
    }

    fn description(&self) -> &str {
        "Run a task in a cloud sandbox with Claude Code and MCP tools. Supports four types:\n\
         - research: analyze code in a repo, post findings to Slack\n\
         - data: query data via MCP tools (PostHog, SigNoz), post results to Slack\n\
         - admin: run admin API calls via MCP tools (requires approval), post results\n\
         - eng: make code changes in a repo, create a GitHub PR (requires plan approval)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task to accomplish"
                },
                "task_type": {
                    "type": "string",
                    "enum": ["research", "data", "admin", "eng"],
                    "description": "research = analyze code, data = query analytics/MCP, admin = run admin API calls (approval required), eng = code changes + PR (approval required)"
                },
                "repo_url": {
                    "type": "string",
                    "description": "Git repo URL (required for research/eng, optional for data/admin)"
                },
                "slack_channel": {
                    "type": "string",
                    "description": "Slack channel ID (falls back to config default)"
                },
                "branch": {
                    "type": "string",
                    "description": "Base branch to work from (default: repo default branch)"
                }
            },
            "required": ["task", "task_type"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter"))?
            .to_string();

        let task_type = match args.get("task_type").and_then(|v| v.as_str()) {
            Some("research") => TaskType::Research,
            Some("data") => TaskType::Data,
            Some("admin") => TaskType::Admin,
            Some("eng") | None => TaskType::Eng,
            Some(other) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Invalid task_type '{other}'. Must be: research, data, admin, or eng"
                    )),
                });
            }
        };

        let repo_url = args
            .get("repo_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Validate repo_url requirement
        if task_type.needs_repo() && repo_url.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "task_type '{}' requires a repo_url",
                    args.get("task_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("eng")
                )),
            });
        }

        let slack_channel = args
            .get("slack_channel")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| self.config.default_slack_channel.clone())
            .unwrap_or_default();

        if slack_channel.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "No slack_channel provided and no default_slack_channel configured".into(),
                ),
            });
        }

        if self.config.slack_bot_token.is_none() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("sandbox_workflow.slack_bot_token is not set in config".into()),
            });
        }

        let branch = args
            .get("branch")
            .and_then(|v| v.as_str())
            .map(String::from);

        let orchestrator = Arc::new(Orchestrator::new(self.config.clone(), &self.workspace_dir));

        let type_label = task_type.label().to_string();
        let params = WorkflowParams {
            task,
            task_type,
            repo_url,
            slack_channel,
            base_branch: branch,
        };

        let workflow_id = orchestrator.spawn(params);

        Ok(ToolResult {
            success: true,
            output: format!(
                "{type_label} workflow started.\nWorkflow ID: {workflow_id}\nCheck Slack for updates."
            ),
            error: None,
        })
    }
}
