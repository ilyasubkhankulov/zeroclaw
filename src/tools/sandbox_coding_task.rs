use super::poll::ChannelMapHandle;
use super::traits::{Tool, ToolResult};
use crate::config::SandboxWorkflowConfig;
use crate::sandbox_workflow::{Orchestrator, ThreadRouter, WorkflowParams};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// Kicks off a sandbox coding workflow: E2B sandbox + Claude Code plan mode +
/// Slack iteration + PR creation.
pub struct SandboxCodingTaskTool {
    config: SandboxWorkflowConfig,
    channels: ChannelMapHandle,
    router: Arc<ThreadRouter>,
    workspace_dir: PathBuf,
}

impl SandboxCodingTaskTool {
    pub fn new(
        config: SandboxWorkflowConfig,
        channels: ChannelMapHandle,
        router: Arc<ThreadRouter>,
        workspace_dir: &std::path::Path,
    ) -> Self {
        Self {
            config,
            channels,
            router,
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
        "Start a sandboxed coding task: spins up an E2B cloud sandbox, runs Claude Code in plan mode, posts the plan to Slack for review, executes on approval, and creates a GitHub PR. Returns immediately with a workflow ID."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The coding task to accomplish"
                },
                "repo_url": {
                    "type": "string",
                    "description": "Git repository URL to clone (e.g. https://github.com/user/repo)"
                },
                "slack_channel": {
                    "type": "string",
                    "description": "Slack channel ID for plan review thread (falls back to config default)"
                },
                "branch": {
                    "type": "string",
                    "description": "Base branch to work from (default: repo default branch)"
                }
            },
            "required": ["task", "repo_url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter"))?
            .to_string();

        let repo_url = args
            .get("repo_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'repo_url' parameter"))?
            .to_string();

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

        let branch = args
            .get("branch")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Find a Slack channel from the channel map
        let channel = {
            let map = self.channels.read();
            // Look for any channel named "slack" (the Slack channel adapter)
            map.values()
                .find(|ch| {
                    let name = ch.name();
                    name.contains("slack") || name.contains("Slack")
                })
                .cloned()
        };

        let channel: Arc<dyn crate::channels::Channel> = match channel {
            Some(ch) => ch,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("No Slack channel available. Ensure Slack is configured.".into()),
                });
            }
        };

        let orchestrator = Arc::new(Orchestrator::new(
            self.config.clone(),
            channel,
            Arc::clone(&self.router),
            &self.workspace_dir,
        ));

        let params = WorkflowParams {
            task,
            repo_url,
            slack_channel,
            base_branch: branch,
        };

        let workflow_id = orchestrator.spawn(params);

        Ok(ToolResult {
            success: true,
            output: format!(
                "Sandbox coding workflow started.\nWorkflow ID: {workflow_id}\nCheck Slack for the plan."
            ),
            error: None,
        })
    }
}
