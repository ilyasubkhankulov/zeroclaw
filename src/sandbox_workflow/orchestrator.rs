//! Workflow orchestrator: drives the state machine, bridges E2B <-> Slack.
//!
//! Each workflow spawns a tokio task that runs through the state machine from
//! `Initializing` to `Completed` (or `Failed`). Slack interaction is handled
//! via direct Slack Web API calls (not through the Channel trait) so we can
//! capture message timestamps for threading and poll for replies.

use crate::config::SandboxWorkflowConfig;
use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;

use super::e2b_client::E2bClient;
use super::github_app::GitHubAppAuth;
use super::slack_bridge;
use super::state::{TaskType, WorkflowRecord, WorkflowState};

/// Preamble prepended to all Claude Code prompts in sandboxes.
/// Tells Claude its output is automatically posted to Slack.
const SANDBOX_PREAMBLE: &str = "You are running inside an automated sandbox. Your text output will be automatically posted to a Slack channel — you do not need to post to Slack yourself. Just write your response directly. Do not mention Slack, do not say you cannot post, do not ask the user to share manually.\n\n";

/// Parameters for launching a new workflow.
pub struct WorkflowParams {
    pub task: String,
    pub task_type: TaskType,
    pub repo_url: String,
    pub slack_channel: String,
    pub base_branch: Option<String>,
}

/// Drives a single workflow instance through the state machine.
pub struct Orchestrator {
    config: SandboxWorkflowConfig,
    e2b: E2bClient,
    http: Client,
    slack_bot_token: String,
    github_app: Option<GitHubAppAuth>,
    workflows_dir: PathBuf,
}

impl Orchestrator {
    pub fn new(config: SandboxWorkflowConfig, workspace_dir: &std::path::Path) -> Self {
        let api_key = if config.e2b_api_key.is_empty() {
            std::env::var("E2B_API_KEY").unwrap_or_default()
        } else {
            config.e2b_api_key.clone()
        };
        let e2b = E2bClient::new(&config.e2b_api_url, &api_key);
        let workflows_dir = workspace_dir.join("sandbox_workflows");
        let http = Client::new();
        let slack_bot_token = config.slack_bot_token.clone().unwrap_or_default();

        // Initialize GitHub App auth if configured (preferred over GH_TOKEN)
        let github_app = match (
            config.github_app_id.as_deref(),
            config.github_app_private_key_path.as_deref(),
            config.github_app_installation_id,
        ) {
            (Some(id), Some(key_path), Some(install_id)) => {
                match GitHubAppAuth::from_config(id, key_path, install_id) {
                    Ok(auth) => {
                        tracing::info!("GitHub App auth configured (app_id={id})");
                        Some(auth)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "GitHub App auth failed to initialize, falling back to GH_TOKEN");
                        None
                    }
                }
            }
            _ => None,
        };

        Self {
            config,
            e2b,
            http,
            slack_bot_token,
            github_app,
            workflows_dir,
        }
    }

    /// Spawn a new workflow as a background tokio task.
    /// Returns the workflow ID immediately.
    pub fn spawn(self: Arc<Self>, params: WorkflowParams) -> String {
        let workflow_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let mut record = WorkflowRecord::new(
            workflow_id.clone(),
            params.repo_url,
            params.task,
            params.slack_channel,
        );
        record.task_type = params.task_type;
        record.base_branch = params.base_branch;

        let orchestrator = Arc::clone(&self);
        let wf_id = workflow_id.clone();

        tokio::spawn(async move {
            if let Err(e) = orchestrator.run_workflow(record).await {
                tracing::error!(workflow_id = %wf_id, error = %e, "Workflow failed");
            }
        });

        workflow_id
    }

    /// Run the workflow — branches by task type.
    async fn run_workflow(&self, mut record: WorkflowRecord) -> anyhow::Result<()> {
        let _ = record.save(&self.workflows_dir);

        tracing::info!(
            workflow_id = %record.workflow_id,
            task_type = ?record.task_type,
            "Starting sandbox workflow"
        );

        // Phase 1: Initialize sandbox
        match self.initialize_sandbox(&mut record).await {
            Ok(()) => {}
            Err(e) => {
                self.fail(&mut record, &format!("Sandbox init failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        // Branch by task type
        match record.task_type.clone() {
            TaskType::Research => self.run_research(record).await,
            TaskType::Data => self.run_data(record).await,
            TaskType::Admin => self.run_admin(record).await,
            TaskType::Eng => self.run_eng(record).await,
        }
    }

    /// Research: analyze code, post findings. No approval, no PR.
    async fn run_research(&self, mut record: WorkflowRecord) -> anyhow::Result<()> {
        record.transition(WorkflowState::Executing);
        let _ = record.save(&self.workflows_dir);

        let sid = record.sandbox_id.as_deref().unwrap_or_default();
        let escaped_task = record.task.replace('\'', "'\\''");
        let cmd = format!(
            "claude -p '{SANDBOX_PREAMBLE}Analyze this codebase and answer the following. Report your findings thoroughly.\n\nTask: {escaped_task}' --dangerously-skip-permissions --output-format json"
        );

        let result = self
            .e2b
            .exec(
                sid,
                &cmd,
                self.config.exec_timeout_secs,
                Some("/home/user/project"),
            )
            .await;

        match result {
            Ok(r) if r.exit_code == 0 => {
                let (text, _) = parse_claude_output(&r.stdout).unwrap_or((r.stdout, None));
                self.post_summary_and_reply(&mut record, &text).await;
            }
            Ok(r) => {
                let detail = if r.stderr.is_empty() {
                    &r.stdout
                } else {
                    &r.stderr
                };
                self.fail(&mut record, &format!("Research failed: {detail}"))
                    .await;
                return Ok(());
            }
            Err(e) => {
                self.fail(&mut record, &format!("Research failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        record.transition(WorkflowState::Completed);
        let _ = record.save(&self.workflows_dir);
        self.cleanup(&record).await;
        tracing::info!(workflow_id = %record.workflow_id, "Research workflow completed");
        Ok(())
    }

    /// Data: query MCP tools, post results. No approval, no PR.
    async fn run_data(&self, mut record: WorkflowRecord) -> anyhow::Result<()> {
        record.transition(WorkflowState::Executing);
        let _ = record.save(&self.workflows_dir);

        let sid = record.sandbox_id.as_deref().unwrap_or_default();
        let escaped_task = record.task.replace('\'', "'\\''");
        let cwd = if record.repo_url.is_empty() {
            "/home/user"
        } else {
            "/home/user/project"
        };
        let cmd = format!(
            "claude -p '{SANDBOX_PREAMBLE}{escaped_task}. Use the available MCP tools to query data and report the results.' --dangerously-skip-permissions --output-format json"
        );

        let result = self
            .e2b
            .exec(sid, &cmd, self.config.exec_timeout_secs, Some(cwd))
            .await;

        match result {
            Ok(r) if r.exit_code == 0 => {
                let (text, _) = parse_claude_output(&r.stdout).unwrap_or((r.stdout, None));
                self.post_summary_and_reply(&mut record, &text).await;
            }
            Ok(r) => {
                let detail = if r.stderr.is_empty() {
                    &r.stdout
                } else {
                    &r.stderr
                };
                self.fail(&mut record, &format!("Data query failed: {detail}"))
                    .await;
                return Ok(());
            }
            Err(e) => {
                self.fail(&mut record, &format!("Data query failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        record.transition(WorkflowState::Completed);
        let _ = record.save(&self.workflows_dir);
        self.cleanup(&record).await;
        tracing::info!(workflow_id = %record.workflow_id, "Data workflow completed");
        Ok(())
    }

    /// Admin: plan admin actions, get approval, execute, post results. No PR.
    async fn run_admin(&self, mut record: WorkflowRecord) -> anyhow::Result<()> {
        // Plan phase
        record.transition(WorkflowState::Planning);
        let _ = record.save(&self.workflows_dir);

        let sid = record.sandbox_id.clone().unwrap_or_default();
        let escaped_task = record.task.replace('\'', "'\\''");
        let cwd = if record.repo_url.is_empty() {
            "/home/user"
        } else {
            "/home/user/project"
        };
        let plan_cmd = format!(
            "claude -p '{SANDBOX_PREAMBLE}Plan the following admin actions. Do NOT execute yet — only describe what API calls you would make and what the effects would be.\n\nTask: {escaped_task}' --dangerously-skip-permissions --output-format json"
        );

        let result = self
            .e2b
            .exec(&sid, &plan_cmd, self.config.plan_timeout_secs, Some(cwd))
            .await;

        match result {
            Ok(r) if r.exit_code == 0 => {
                let (plan_text, session_id) = parse_claude_output(&r.stdout)?;
                record.plan_text = Some(plan_text.clone());
                record.session_id = session_id;
                self.post_summary_and_reply(&mut record, &plan_text).await;
            }
            Ok(r) => {
                let detail = if r.stderr.is_empty() {
                    &r.stdout
                } else {
                    &r.stderr
                };
                self.fail(&mut record, &format!("Admin plan failed: {detail}"))
                    .await;
                return Ok(());
            }
            Err(e) => {
                self.fail(&mut record, &format!("Admin plan failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        // Approval loop
        self.await_approval(&mut record).await?;

        // Execute
        record.transition(WorkflowState::Executing);
        let _ = record.save(&self.workflows_dir);

        let mut exec_cmd = format!("claude -p '{SANDBOX_PREAMBLE}Execute the admin actions you planned. Run the API calls now.' --dangerously-skip-permissions --output-format json");
        if let Some(ref sess_id) = record.session_id {
            use std::fmt::Write;
            let _ = write!(exec_cmd, " --resume {sess_id}");
        }

        let result = self
            .e2b
            .exec(&sid, &exec_cmd, self.config.exec_timeout_secs, Some(cwd))
            .await;

        match result {
            Ok(r) if r.exit_code == 0 => {
                let (text, _) = parse_claude_output(&r.stdout).unwrap_or((r.stdout, None));
                self.post_slack_thread_message(&record, &slack_bridge::format_results(&text))
                    .await;
            }
            Ok(r) => {
                let detail = if r.stderr.is_empty() {
                    &r.stdout
                } else {
                    &r.stderr
                };
                self.fail(&mut record, &format!("Admin execution failed: {detail}"))
                    .await;
                return Ok(());
            }
            Err(e) => {
                self.fail(&mut record, &format!("Admin execution failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        record.transition(WorkflowState::Completed);
        let _ = record.save(&self.workflows_dir);
        self.cleanup(&record).await;
        tracing::info!(workflow_id = %record.workflow_id, "Admin workflow completed");
        Ok(())
    }

    /// Eng: plan → approve → execute → PR (original behavior).
    async fn run_eng(&self, mut record: WorkflowRecord) -> anyhow::Result<()> {
        // Plan
        record.transition(WorkflowState::Planning);
        let _ = record.save(&self.workflows_dir);
        match self.generate_plan(&mut record).await {
            Ok(()) => {}
            Err(e) => {
                self.fail(&mut record, &format!("Planning failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        // Approval loop
        self.await_approval(&mut record).await?;

        // Execute
        record.transition(WorkflowState::Executing);
        let _ = record.save(&self.workflows_dir);
        match self.execute_plan(&mut record).await {
            Ok(()) => {}
            Err(e) => {
                self.fail(&mut record, &format!("Execution failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        // Create PR
        record.transition(WorkflowState::CreatingPr);
        let _ = record.save(&self.workflows_dir);
        match self.create_pr(&mut record).await {
            Ok(()) => {}
            Err(e) => {
                self.fail(&mut record, &format!("PR creation failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        record.transition(WorkflowState::Completed);
        let _ = record.save(&self.workflows_dir);
        if let Some(ref pr_url) = record.pr_url {
            let pr_msg = slack_bridge::format_pr_created(pr_url);
            self.post_slack_thread_message(&record, &pr_msg).await;
        }
        self.cleanup(&record).await;
        tracing::info!(workflow_id = %record.workflow_id, "Eng workflow completed");
        Ok(())
    }

    /// Post the summary message to the channel and the detail as a thread reply.
    async fn post_summary_and_reply(&self, record: &mut WorkflowRecord, detail_text: &str) {
        let summary = slack_bridge::format_plan_summary(
            record.task_type.label(),
            &record.workflow_id,
            &record.repo_url,
            &record.task,
            record.task_type.needs_approval(),
        );
        if let Ok(ts) = self.post_slack_message(&record.slack_channel, &summary).await {
            record.slack_thread_ts = Some(ts.clone());
            let _ = record.save(&self.workflows_dir);
            let detail = slack_bridge::format_plan_detail(detail_text);
            self.post_slack_reply(&record.slack_channel, &ts, &detail).await;
        }
    }

    /// Shared approval loop — polls Slack thread for approval/cancel/feedback.
    /// Returns Ok(()) on approval, or transitions to Failed and returns Ok(()).
    async fn await_approval(&self, record: &mut WorkflowRecord) -> anyhow::Result<()> {
        record.transition(WorkflowState::AwaitingFeedback);
        let _ = record.save(&self.workflows_dir);

        let approval_deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(self.config.approval_timeout_secs);
        let mut last_seen_ts = record.slack_thread_ts.clone().unwrap_or_default();

        loop {
            if tokio::time::Instant::now() > approval_deadline {
                self.fail(record, "Approval timed out").await;
                anyhow::bail!("Approval timed out");
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

            let thread_ts = match record.slack_thread_ts.clone() {
                Some(ts) => ts,
                None => continue,
            };

            let replies = self
                .poll_slack_replies(&record.slack_channel, &thread_ts, &last_seen_ts)
                .await;

            let mut approved = false;
            for (ts, text) in replies {
                last_seen_ts = ts;

                if slack_bridge::is_cancel(&text) {
                    self.fail(record, "Cancelled by user").await;
                    anyhow::bail!("Cancelled by user");
                }

                if slack_bridge::is_approval(&text) {
                    let exec_msg = slack_bridge::format_execution_started();
                    self.post_slack_reply(&record.slack_channel, &thread_ts, &exec_msg)
                        .await;
                    approved = true;
                    break;
                }

                // Feedback — revise
                if record.iteration_count >= self.config.max_iterations {
                    self.fail(
                        record,
                        &format!("Max iterations ({}) reached", self.config.max_iterations),
                    )
                    .await;
                    anyhow::bail!("Max iterations reached");
                }

                record.transition(WorkflowState::Revising);
                let _ = record.save(&self.workflows_dir);
                match self.revise_plan(record, &text).await {
                    Ok(()) => {
                        record.transition(WorkflowState::AwaitingFeedback);
                        let _ = record.save(&self.workflows_dir);
                    }
                    Err(e) => {
                        self.fail(record, &format!("Revision failed: {e}")).await;
                        anyhow::bail!("Revision failed");
                    }
                }
            }

            if approved {
                return Ok(());
            }
        }
    }

    // ── Sandbox lifecycle ─────────────────────────────────────────

    async fn initialize_sandbox(&self, record: &mut WorkflowRecord) -> anyhow::Result<()> {
        let mut envs = std::collections::HashMap::new();

        for var in &["ANTHROPIC_API_KEY", "API_KEY"] {
            if let Ok(val) = std::env::var(var) {
                envs.insert((*var).into(), val);
            }
        }
        // Get GitHub token from App (preferred) or env var
        if let Some(gh_token) = self.get_github_token().await {
            envs.insert("GH_TOKEN".into(), gh_token);
        }
        for var in &self.config.env_passthrough {
            if let Ok(val) = std::env::var(var) {
                envs.insert(var.clone(), val);
            }
        }

        let sandbox = self
            .e2b
            .create_sandbox(
                &self.config.e2b_template,
                envs,
                self.config.sandbox_timeout_secs,
            )
            .await?;
        record.sandbox_id = Some(sandbox.sandbox_id.clone());
        let _ = record.save(&self.workflows_dir);

        let sid = &sandbox.sandbox_id;

        // Copy Claude Code credentials (Max subscription OAuth)
        let creds_path = self
            .config
            .claude_credentials_path
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/zeroclaw-data".into());
                std::path::PathBuf::from(home).join(".claude/.credentials.json")
            });
        if creds_path.exists() {
            match std::fs::read_to_string(&creds_path) {
                Ok(creds_content) => {
                    self.e2b
                        .write_file(sid, "/home/user/.claude/.credentials.json", &creds_content)
                        .await?;
                    tracing::info!("Copied Claude Code credentials into sandbox");
                }
                Err(e) => {
                    tracing::warn!(path = %creds_path.display(), error = %e,
                        "Could not read Claude Code credentials");
                }
            }
        }

        // Sync Claude Code config directory (skills, settings.json, etc.)
        if let Some(ref config_dir) = self.config.claude_config_dir {
            let config_path = std::path::Path::new(config_dir);
            if config_path.is_dir() {
                match sync_dir_to_sandbox(&self.e2b, sid, config_path, "/home/user/.claude").await {
                    Ok(count) => {
                        tracing::info!(count, "Synced Claude Code config files into sandbox");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to sync Claude Code config dir");
                    }
                }
            } else {
                tracing::warn!(path = %config_dir, "claude_config_dir does not exist or is not a directory");
            }
        }

        // Configure git
        if let Some(ref name) = self.config.git_user_name {
            self.e2b
                .exec(
                    sid,
                    &format!("git config --global user.name '{name}'"),
                    30,
                    None,
                )
                .await?;
        }
        if let Some(ref email) = self.config.git_user_email {
            self.e2b
                .exec(
                    sid,
                    &format!("git config --global user.email '{email}'"),
                    30,
                    None,
                )
                .await?;
        }

        // Configure git + gh auth for private repos.
        // Configure git URL rewriting for clone/push auth using GitHub token.
        if let Some(gh_token) = self.get_github_token().await {
            self.e2b
                .exec(
                    sid,
                    &format!(
                        "git config --global url.'https://x-access-token:{gh_token}@github.com/'.insteadOf 'https://github.com/'"
                    ),
                    30,
                    None,
                )
                .await?;
        }

        // Clone repo (skip for data/admin tasks with no repo)
        if record.repo_url.is_empty() {
            // Create project dir even without a repo
            self.e2b
                .exec(sid, "mkdir -p /home/user/project", 10, None)
                .await?;
        } else {
            let branch_flag = record
                .base_branch
                .as_deref()
                .map(|b| format!(" -b {b}"))
                .unwrap_or_default();
            let clone_cmd = format!(
                "git clone{branch_flag} {} /home/user/project",
                record.repo_url
            );
            let result = self.e2b.exec(sid, &clone_cmd, 120, None).await?;
            if result.exit_code != 0 {
                anyhow::bail!(
                    "git clone failed (exit {}): {}",
                    result.exit_code,
                    result.stderr
                );
            }
        }

        // Install Claude Code CLI if not present
        let check = self.e2b.exec(sid, "which claude", 10, None).await;
        if check.is_err() || check.as_ref().is_ok_and(|r| r.exit_code != 0) {
            tracing::info!(workflow_id = %record.workflow_id, "Installing Claude Code CLI...");
            let install = self
                .e2b
                .exec(sid, "npm install -g @anthropic-ai/claude-code", 120, None)
                .await?;
            if install.exit_code != 0 {
                anyhow::bail!("Failed to install Claude Code: {}", install.stderr);
            }
        }

        // Register OAuth MCP servers from credentials (PostHog, SigNoz, etc.)
        // Must run after Claude Code is installed AND repo is cloned, since
        // `claude mcp add` saves config to the cwd and Claude Code only finds
        // it when running from that same directory.
        let project_dir = Some("/home/user/project");
        if creds_path.exists() {
            if let Ok(creds_str) = std::fs::read_to_string(&creds_path) {
                if let Ok(creds_json) = serde_json::from_str::<serde_json::Value>(&creds_str) {
                    if let Some(mcp_oauth) = creds_json.get("mcpOAuth").and_then(|v| v.as_object())
                    {
                        for (_key, val) in mcp_oauth {
                            let name = val
                                .get("serverName")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            let url = val
                                .get("serverUrl")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            let token = val
                                .get("accessToken")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            if !name.is_empty() && !url.is_empty() && !token.is_empty() {
                                let cmd = format!(
                                    "claude mcp add --transport http {name} {url} --header 'Authorization: Bearer {token}'"
                                );
                                match self.e2b.exec(sid, &cmd, 15, project_dir).await {
                                    Ok(r) if r.exit_code == 0 => {
                                        tracing::info!(name, "Registered OAuth MCP server");
                                    }
                                    Ok(r) => {
                                        tracing::warn!(name, stderr = %r.stderr, "Failed to add MCP server");
                                    }
                                    Err(e) => {
                                        tracing::warn!(name, error = %e, "Failed to add MCP server");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // ── Claude Code phases ────────────────────────────────────────

    async fn generate_plan(&self, record: &mut WorkflowRecord) -> anyhow::Result<()> {
        let sid = record
            .sandbox_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No sandbox"))?;

        // Note: --permission-mode plan is incompatible with -p (non-interactive).
        // Instead, use --dangerously-skip-permissions with a prompt that instructs
        // Claude Code to only analyze and plan, not make changes.
        let escaped_task = record.task.replace('\'', "'\\''");
        let cmd = format!(
            "claude -p '{SANDBOX_PREAMBLE}Analyze this codebase and create a detailed plan for the following task. Do NOT make any changes yet — only read files and describe what you would do, which files you would modify, and what the changes would be.\n\nTask: {}' --dangerously-skip-permissions --output-format json",
            escaped_task
        );

        let result = self
            .e2b
            .exec(
                sid,
                &cmd,
                self.config.plan_timeout_secs,
                Some("/home/user/project"),
            )
            .await?;

        if result.exit_code != 0 {
            let detail = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            anyhow::bail!(
                "Claude Code plan failed (exit {}): {}",
                result.exit_code,
                detail
            );
        }

        let (plan_text, session_id) = parse_claude_output(&result.stdout)?;
        record.plan_text = Some(plan_text.clone());
        record.session_id = session_id;

        self.post_summary_and_reply(record, &plan_text).await;

        Ok(())
    }

    async fn revise_plan(&self, record: &mut WorkflowRecord, feedback: &str) -> anyhow::Result<()> {
        let sid = record
            .sandbox_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No sandbox"))?;

        let escaped_feedback = feedback.replace('\'', "'\\''");
        let mut cmd = format!(
            "claude -p '{SANDBOX_PREAMBLE}Revise the plan based on this feedback. Do NOT make any changes yet — only update your plan.\n\nFeedback: {escaped_feedback}' --dangerously-skip-permissions --output-format json"
        );

        if let Some(ref sess_id) = record.session_id {
            use std::fmt::Write;
            let _ = write!(cmd, " --resume {sess_id}");
        }

        let result = self
            .e2b
            .exec(
                sid,
                &cmd,
                self.config.plan_timeout_secs,
                Some("/home/user/project"),
            )
            .await?;

        if result.exit_code != 0 {
            let detail = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            anyhow::bail!(
                "Claude Code revision failed (exit {}): {}",
                result.exit_code,
                detail
            );
        }

        let (plan_text, session_id) = parse_claude_output(&result.stdout)?;
        record.plan_text = Some(plan_text.clone());
        if session_id.is_some() {
            record.session_id = session_id;
        }
        record.iteration_count += 1;

        let revised_msg = slack_bridge::format_revised_plan(&plan_text, record.iteration_count);
        self.post_slack_thread_message(record, &revised_msg).await;

        Ok(())
    }

    async fn execute_plan(&self, record: &mut WorkflowRecord) -> anyhow::Result<()> {
        let sid = record
            .sandbox_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No sandbox"))?;

        let mut cmd = format!("claude -p '{SANDBOX_PREAMBLE}Execute the approved plan. Make all the code changes and commit them.' --dangerously-skip-permissions --output-format json");

        if let Some(ref sess_id) = record.session_id {
            use std::fmt::Write;
            let _ = write!(cmd, " --resume {sess_id}");
        }

        let result = self
            .e2b
            .exec(
                sid,
                &cmd,
                self.config.exec_timeout_secs,
                Some("/home/user/project"),
            )
            .await?;

        if result.exit_code != 0 {
            let detail = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            anyhow::bail!("Execution failed (exit {}): {}", result.exit_code, detail);
        }

        Ok(())
    }

    async fn create_pr(&self, record: &mut WorkflowRecord) -> anyhow::Result<()> {
        let sid = record
            .sandbox_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No sandbox"))?;
        let project_dir = Some("/home/user/project");
        let branch_name = format!("zeroclaw/{}", &record.workflow_id);

        // Create branch
        let result = self
            .e2b
            .exec(
                sid,
                &format!("git checkout -b {branch_name}"),
                30,
                project_dir,
            )
            .await?;
        if result.exit_code != 0 && !result.stderr.contains("already exists") {
            self.e2b
                .exec(sid, &format!("git checkout {branch_name}"), 30, project_dir)
                .await?;
        }

        // Stage + commit
        self.e2b.exec(sid, "git add -A", 30, project_dir).await?;
        let commit_msg = format!("feat: {}", truncate(&record.task, 60));
        let escaped_msg = commit_msg.replace('\'', "'\\''");
        let _ = self
            .e2b
            .exec(
                sid,
                &format!("git commit -m '{escaped_msg}' --allow-empty"),
                30,
                project_dir,
            )
            .await?;

        // Push
        let push_result = self
            .e2b
            .exec(
                sid,
                &format!("git push -u origin {branch_name}"),
                60,
                project_dir,
            )
            .await?;
        if push_result.exit_code != 0 {
            anyhow::bail!("git push failed: {}", push_result.stderr);
        }

        // Create PR — generate a fresh token (App tokens are short-lived)
        let gh_token = self.get_github_token().await.unwrap_or_default();
        let pr_title = truncate(&record.task, 70);
        let escaped_title = pr_title.replace('\'', "'\\''");
        let pr_body = format!(
            "## Summary\n\n{}\n\n---\n*Created by ZeroClaw sandbox workflow*",
            record.task
        );
        let escaped_body = pr_body.replace('\'', "'\\''");
        let pr_result = self
            .e2b
            .exec(
                sid,
                &format!(
                    "GH_TOKEN={gh_token} gh pr create --title '{escaped_title}' --body '{escaped_body}'"
                ),
                60,
                project_dir,
            )
            .await?;

        if pr_result.exit_code != 0 {
            anyhow::bail!("gh pr create failed: {}", pr_result.stderr);
        }

        let pr_url = pr_result.stdout.trim().to_string();
        record.pr_url = Some(pr_url);

        Ok(())
    }

    // ── Slack Web API (direct calls for threading) ────────────────

    /// Post a message to a Slack channel and return the message ts.
    async fn post_slack_message(
        &self,
        channel: &str,
        msg: &slack_bridge::SlackMessage,
    ) -> anyhow::Result<String> {
        let resp = self
            .http
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.slack_bot_token)
            .json(&serde_json::json!({
                "channel": channel,
                "blocks": msg.blocks,
                "text": msg.text,
            }))
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        if body["ok"].as_bool() != Some(true) {
            let err = body["error"].as_str().unwrap_or("unknown");
            anyhow::bail!("Slack chat.postMessage failed: {err}");
        }

        body["ts"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("Slack response missing ts"))
    }

    /// Post a reply in a Slack thread.
    async fn post_slack_reply(
        &self,
        channel: &str,
        thread_ts: &str,
        msg: &slack_bridge::SlackMessage,
    ) {
        let result = self
            .http
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.slack_bot_token)
            .json(&serde_json::json!({
                "channel": channel,
                "blocks": msg.blocks,
                "text": msg.text,
                "thread_ts": thread_ts,
            }))
            .send()
            .await;

        if let Err(e) = result {
            tracing::warn!(error = %e, "Failed to post Slack reply");
        }
    }

    /// Post a message in the workflow's Slack thread (or channel if no thread).
    async fn post_slack_thread_message(
        &self,
        record: &WorkflowRecord,
        msg: &slack_bridge::SlackMessage,
    ) {
        if let Some(ref thread_ts) = record.slack_thread_ts {
            self.post_slack_reply(&record.slack_channel, thread_ts, msg)
                .await;
        } else {
            let _ = self.post_slack_message(&record.slack_channel, msg).await;
        }
    }

    /// Poll for new replies in a Slack thread since `oldest_ts`.
    /// Returns vec of (ts, text) for messages from non-bot users.
    async fn poll_slack_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        oldest_ts: &str,
    ) -> Vec<(String, String)> {
        let resp = self
            .http
            .get("https://slack.com/api/conversations.replies")
            .bearer_auth(&self.slack_bot_token)
            .query(&[
                ("channel", channel),
                ("ts", thread_ts),
                ("oldest", oldest_ts),
            ])
            .send()
            .await;

        let body: serde_json::Value = match resp {
            Ok(r) => match r.json().await {
                Ok(b) => b,
                Err(_) => return Vec::new(),
            },
            Err(_) => return Vec::new(),
        };

        if body["ok"].as_bool() != Some(true) {
            return Vec::new();
        }

        body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .filter(|m| {
                        // Skip bot messages and the thread root
                        let ts = m["ts"].as_str().unwrap_or("");
                        let is_bot = m.get("bot_id").is_some();
                        let is_root = ts == thread_ts;
                        let is_old = ts <= oldest_ts;
                        !is_bot && !is_root && !is_old
                    })
                    .filter_map(|m| {
                        let ts = m["ts"].as_str()?.to_string();
                        let text = m["text"].as_str()?.to_string();
                        Some((ts, text))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get a GitHub token — from GitHub App (preferred) or GH_TOKEN env var.
    async fn get_github_token(&self) -> Option<String> {
        if let Some(ref app) = self.github_app {
            match app.get_installation_token().await {
                Ok(token) => {
                    tracing::info!("Generated GitHub App installation token");
                    return Some(token);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "GitHub App token generation failed");
                }
            }
        }
        std::env::var("GH_TOKEN")
            .ok()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok())
    }

    // ── Lifecycle helpers ─────────────────────────────────────────

    async fn fail(&self, record: &mut WorkflowRecord, reason: &str) {
        tracing::error!(workflow_id = %record.workflow_id, reason = %reason, "Workflow failed");
        record.transition(WorkflowState::Failed {
            reason: reason.to_string(),
        });
        let _ = record.save(&self.workflows_dir);

        let fail_msg = slack_bridge::format_failure(reason);
        self.post_slack_thread_message(record, &fail_msg).await;
        self.cleanup(record).await;
    }

    async fn cleanup(&self, record: &WorkflowRecord) {
        if let Some(ref sid) = record.sandbox_id {
            if let Err(e) = self.e2b.destroy(sid).await {
                tracing::warn!(sandbox_id = %sid, error = %e, "Failed to destroy sandbox");
            }
        }
    }
}

/// Recursively sync a local directory into the sandbox at `dest_base`.
/// Skips `.credentials.json` (handled separately), sessions, and caches.
async fn sync_dir_to_sandbox(
    e2b: &E2bClient,
    sandbox_id: &str,
    local_dir: &std::path::Path,
    dest_base: &str,
) -> anyhow::Result<usize> {
    use std::fs;

    const SKIP_NAMES: &[&str] = &[
        ".credentials.json",
        "sessions",
        "session-env",
        "statsig",
        "cache",
        ".cache",
        "history.jsonl",
    ];

    let mut count = 0;
    let mut stack = vec![(local_dir.to_path_buf(), dest_base.to_string())];

    while let Some((dir, dest_dir)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if SKIP_NAMES.iter().any(|s| *s == name_str.as_ref()) {
                continue;
            }

            let path = entry.path();
            let dest_path = format!("{}/{}", dest_dir, name_str);

            if path.is_dir() {
                stack.push((path, dest_path));
            } else if path.is_file() {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Err(e) = e2b.write_file(sandbox_id, &dest_path, &content).await {
                        tracing::warn!(path = %dest_path, error = %e, "Failed to sync file");
                    } else {
                        count += 1;
                    }
                }
            }
        }
    }

    Ok(count)
}

/// Parse Claude Code JSON output to extract the result text and session_id.
fn parse_claude_output(stdout: &str) -> anyhow::Result<(String, Option<String>)> {
    let json: serde_json::Value = serde_json::from_str(stdout).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse Claude Code output as JSON: {e}\nRaw: {}",
            truncate(stdout, 500)
        )
    })?;

    let result_text = json
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    if result_text.is_empty() {
        return Ok((stdout.to_string(), session_id));
    }

    Ok((result_text, session_id))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_output_json() {
        let json =
            r#"{"result": "Here is the plan:\n1. Step one\n2. Step two", "session_id": "abc123"}"#;
        let (result, session_id) = parse_claude_output(json).unwrap();
        assert!(result.contains("Step one"));
        assert_eq!(session_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_claude_output_no_result_key() {
        let json = r#"{"text": "something else"}"#;
        let (result, session_id) = parse_claude_output(json).unwrap();
        assert!(result.contains("something else"));
        assert!(session_id.is_none());
    }

    #[test]
    fn parse_claude_output_invalid_json() {
        let result = parse_claude_output("not json");
        assert!(result.is_err());
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }
}
