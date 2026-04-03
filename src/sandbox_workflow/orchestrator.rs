//! Workflow orchestrator: drives the state machine, bridges E2B <-> Slack.
//!
//! Each workflow spawns a tokio task that runs through the state machine from
//! `Initializing` to `Completed` (or `Failed`). Thread messages from Slack
//! arrive via an `mpsc` channel.

use crate::channels::Channel;
use crate::channels::traits::SendMessage;
use crate::config::SandboxWorkflowConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::e2b_client::E2bClient;
use super::slack_bridge::{self, ThreadMessage, ThreadRouter};
use super::state::{WorkflowRecord, WorkflowState};

/// Parameters for launching a new workflow.
pub struct WorkflowParams {
    pub task: String,
    pub repo_url: String,
    pub slack_channel: String,
    pub base_branch: Option<String>,
}

/// Drives a single workflow instance through the state machine.
pub struct Orchestrator {
    config: SandboxWorkflowConfig,
    e2b: E2bClient,
    channel: Arc<dyn Channel>,
    router: Arc<ThreadRouter>,
    workflows_dir: PathBuf,
}

impl Orchestrator {
    pub fn new(
        config: SandboxWorkflowConfig,
        channel: Arc<dyn Channel>,
        router: Arc<ThreadRouter>,
        workspace_dir: &std::path::Path,
    ) -> Self {
        let api_key = if config.e2b_api_key.is_empty() {
            std::env::var("E2B_API_KEY").unwrap_or_default()
        } else {
            config.e2b_api_key.clone()
        };
        let e2b = E2bClient::new(&config.e2b_api_url, &api_key);
        let workflows_dir = workspace_dir.join("sandbox_workflows");

        Self {
            config,
            e2b,
            channel,
            router,
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

    /// Run the full workflow state machine.
    async fn run_workflow(&self, mut record: WorkflowRecord) -> anyhow::Result<()> {
        // Persist initial state
        let _ = record.save(&self.workflows_dir);

        // Phase 1: Initialize sandbox
        tracing::info!(workflow_id = %record.workflow_id, "Starting sandbox workflow");
        match self.initialize_sandbox(&mut record).await {
            Ok(()) => {}
            Err(e) => {
                self.fail(&mut record, &format!("Sandbox init failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        // Phase 2: Generate initial plan
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

        // Phase 3: Feedback loop
        let (tx, mut rx) = mpsc::channel::<ThreadMessage>(32);

        // Register thread for routing
        if let Some(ref thread_ts) = record.slack_thread_ts {
            self.router
                .register(record.slack_channel.clone(), thread_ts.clone(), tx)
                .await;
        }

        record.transition(WorkflowState::AwaitingFeedback);
        let _ = record.save(&self.workflows_dir);

        let approval_timeout = tokio::time::Duration::from_secs(self.config.approval_timeout_secs);

        loop {
            let msg = tokio::time::timeout(approval_timeout, rx.recv()).await;

            match msg {
                Ok(Some(thread_msg)) => {
                    if slack_bridge::is_approval(&thread_msg.text) {
                        // Approved — proceed to execution
                        break;
                    }

                    // Feedback — revise the plan
                    if record.iteration_count >= self.config.max_iterations {
                        self.fail(
                            &mut record,
                            &format!("Max iterations ({}) reached", self.config.max_iterations),
                        )
                        .await;
                        return Ok(());
                    }

                    record.transition(WorkflowState::Revising);
                    let _ = record.save(&self.workflows_dir);
                    match self.revise_plan(&mut record, &thread_msg.text).await {
                        Ok(()) => {
                            record.transition(WorkflowState::AwaitingFeedback);
                            let _ = record.save(&self.workflows_dir);
                        }
                        Err(e) => {
                            self.fail(&mut record, &format!("Revision failed: {e}"))
                                .await;
                            return Ok(());
                        }
                    }
                }
                Ok(None) => {
                    // Channel closed
                    self.fail(&mut record, "Thread message channel closed")
                        .await;
                    return Ok(());
                }
                Err(_) => {
                    // Timeout
                    self.fail(&mut record, "Approval timed out").await;
                    return Ok(());
                }
            }
        }

        // Phase 4: Execute
        record.transition(WorkflowState::Executing);
        let _ = record.save(&self.workflows_dir);
        self.send_thread_message(&record, &slack_bridge::format_execution_started())
            .await;
        match self.execute_plan(&mut record, &mut rx).await {
            Ok(()) => {}
            Err(e) => {
                self.fail(&mut record, &format!("Execution failed: {e}"))
                    .await;
                return Ok(());
            }
        }

        // Phase 5: Create PR
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

        // Phase 6: Complete
        record.transition(WorkflowState::Completed);
        let _ = record.save(&self.workflows_dir);
        if let Some(ref pr_url) = record.pr_url {
            self.send_thread_message(&record, &slack_bridge::format_pr_created(pr_url))
                .await;
        }

        // Cleanup
        self.cleanup(&record).await;
        tracing::info!(workflow_id = %record.workflow_id, "Workflow completed");
        Ok(())
    }

    /// Initialize the E2B sandbox and clone the repo.
    async fn initialize_sandbox(&self, record: &mut WorkflowRecord) -> anyhow::Result<()> {
        let mut envs = std::collections::HashMap::new();

        // Pass through env vars that the sandbox needs.
        // Claude Code CLI needs ANTHROPIC_API_KEY; gh CLI needs GH_TOKEN.
        // API_KEY is also forwarded so users can map it to ANTHROPIC_API_KEY
        // inside the sandbox if they prefer a single key.
        for var in &["ANTHROPIC_API_KEY", "API_KEY", "GH_TOKEN", "GITHUB_TOKEN"] {
            if let Ok(val) = std::env::var(var) {
                envs.insert((*var).into(), val);
            }
        }
        // Pass through configured extras
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

        // Copy Claude Code credentials into the sandbox (Max subscription OAuth).
        // On Linux (including Docker), credentials live at ~/.claude/.credentials.json.
        // The user runs `claude auth login` once in the ZeroClaw container;
        // we copy the resulting file into each sandbox so Claude Code reuses the session.
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
                    tracing::warn!(
                        path = %creds_path.display(),
                        error = %e,
                        "Could not read Claude Code credentials — sandbox will need ANTHROPIC_API_KEY"
                    );
                }
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

        // Configure gh auth
        self.e2b.exec(sid, "gh auth setup-git", 30, None).await?;

        // Clone the repo
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

        Ok(())
    }

    /// Run Claude Code in plan mode to generate the initial plan.
    async fn generate_plan(&self, record: &mut WorkflowRecord) -> anyhow::Result<()> {
        let sid = record
            .sandbox_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No sandbox"))?;

        let escaped_task = record.task.replace('\'', "'\\''");
        let cmd = format!(
            "claude -p '{}' --permission-mode plan --dangerously-skip-permissions --output-format json",
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
            anyhow::bail!(
                "Claude Code plan failed (exit {}): {}",
                result.exit_code,
                result.stderr
            );
        }

        // Parse JSON response
        let (plan_text, session_id) = parse_claude_output(&result.stdout)?;
        record.plan_text = Some(plan_text.clone());
        record.session_id = session_id;

        // Post to Slack
        let message_text =
            slack_bridge::format_plan_message(&record.repo_url, &record.task, &plan_text);
        let msg = SendMessage::new(&record.slack_channel, &message_text);
        self.channel.send(&msg).await?;

        // The first message's ts becomes our thread_ts.
        // We use the channel_id as a fallback thread identifier.
        // In practice, the Slack channel implementation will capture the
        // actual thread_ts from the API response — but since we don't have
        // direct access to that here, we rely on the thread router being
        // registered with the Slack message's ts after the first reply.
        // For now, we set a placeholder that the Slack listener will update.
        record.slack_thread_ts = Some(format!("workflow:{}", record.workflow_id));

        Ok(())
    }

    /// Revise the plan based on user feedback.
    async fn revise_plan(&self, record: &mut WorkflowRecord, feedback: &str) -> anyhow::Result<()> {
        let sid = record
            .sandbox_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No sandbox"))?;

        let escaped_feedback = feedback.replace('\'', "'\\''");
        let mut cmd = format!(
            "claude -p '{escaped_feedback}' --permission-mode plan --dangerously-skip-permissions --output-format json"
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
            anyhow::bail!(
                "Claude Code revision failed (exit {}): {}",
                result.exit_code,
                result.stderr
            );
        }

        let (plan_text, session_id) = parse_claude_output(&result.stdout)?;
        record.plan_text = Some(plan_text.clone());
        if session_id.is_some() {
            record.session_id = session_id;
        }
        record.iteration_count += 1;

        // Post revised plan to thread
        let message_text = slack_bridge::format_revised_plan(&plan_text, record.iteration_count);
        self.send_thread_message(record, &message_text).await;

        Ok(())
    }

    /// Execute the approved plan.
    async fn execute_plan(
        &self,
        record: &mut WorkflowRecord,
        rx: &mut mpsc::Receiver<ThreadMessage>,
    ) -> anyhow::Result<()> {
        let sid = record
            .sandbox_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No sandbox"))?;

        let mut cmd = "claude -p 'Execute the approved plan. Make all the code changes and commit them.' --dangerously-skip-permissions --output-format json".to_string();

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
            // Check if Claude Code is asking a question
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&result.stdout) {
                if let Some(question) = extract_question(&json) {
                    // Forward question to Slack
                    self.send_thread_message(record, &slack_bridge::format_question(&question))
                        .await;

                    // Wait for answer
                    let timeout =
                        tokio::time::Duration::from_secs(self.config.approval_timeout_secs);
                    match tokio::time::timeout(timeout, rx.recv()).await {
                        Ok(Some(answer)) => {
                            // Resume with the answer
                            let escaped = answer.text.replace('\'', "'\\''");
                            let resume_cmd = format!(
                                "claude -p '{escaped}' --resume {} --dangerously-skip-permissions --output-format json",
                                record.session_id.as_deref().unwrap_or("")
                            );
                            let resume_result = self
                                .e2b
                                .exec(
                                    sid,
                                    &resume_cmd,
                                    self.config.exec_timeout_secs,
                                    Some("/home/user/project"),
                                )
                                .await?;
                            if resume_result.exit_code != 0 {
                                anyhow::bail!(
                                    "Execution failed after answer (exit {}): {}",
                                    resume_result.exit_code,
                                    resume_result.stderr
                                );
                            }
                        }
                        _ => anyhow::bail!("No answer received for Claude Code question"),
                    }
                } else {
                    anyhow::bail!(
                        "Execution failed (exit {}): {}",
                        result.exit_code,
                        result.stderr
                    );
                }
            } else {
                anyhow::bail!(
                    "Execution failed (exit {}): {}",
                    result.exit_code,
                    result.stderr
                );
            }
        }

        Ok(())
    }

    /// Create a PR from the sandbox's changes.
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
        // Branch may already exist if Claude Code created one
        if result.exit_code != 0 && !result.stderr.contains("already exists") {
            // Try switching to it
            self.e2b
                .exec(sid, &format!("git checkout {branch_name}"), 30, project_dir)
                .await?;
        }

        // Stage any uncommitted changes
        self.e2b.exec(sid, "git add -A", 30, project_dir).await?;

        // Commit (may be empty if Claude Code already committed)
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

        // Create PR
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
                &format!("gh pr create --title '{escaped_title}' --body '{escaped_body}'"),
                60,
                project_dir,
            )
            .await?;

        if pr_result.exit_code != 0 {
            anyhow::bail!("gh pr create failed: {}", pr_result.stderr);
        }

        // Extract PR URL from stdout (gh prints the URL)
        let pr_url = pr_result.stdout.trim().to_string();
        record.pr_url = Some(pr_url);

        Ok(())
    }

    /// Transition to failed state, notify Slack, and cleanup.
    async fn fail(&self, record: &mut WorkflowRecord, reason: &str) {
        tracing::error!(
            workflow_id = %record.workflow_id,
            reason = %reason,
            "Workflow failed"
        );
        record.transition(WorkflowState::Failed {
            reason: reason.to_string(),
        });
        let _ = record.save(&self.workflows_dir);

        self.send_thread_message(record, &slack_bridge::format_failure(reason))
            .await;
        self.cleanup(record).await;
    }

    /// Destroy sandbox and unregister thread.
    async fn cleanup(&self, record: &WorkflowRecord) {
        if let Some(ref sid) = record.sandbox_id {
            if let Err(e) = self.e2b.destroy(sid).await {
                tracing::warn!(sandbox_id = %sid, error = %e, "Failed to destroy sandbox");
            }
        }
        if let Some(ref thread_ts) = record.slack_thread_ts {
            self.router
                .unregister(&record.slack_channel, thread_ts)
                .await;
        }
    }

    /// Send a message in the workflow's Slack thread.
    async fn send_thread_message(&self, record: &WorkflowRecord, text: &str) {
        let msg =
            SendMessage::new(&record.slack_channel, text).in_thread(record.slack_thread_ts.clone());
        if let Err(e) = self.channel.send(&msg).await {
            tracing::warn!(
                workflow_id = %record.workflow_id,
                error = %e,
                "Failed to send Slack message"
            );
        }
    }
}

/// Parse Claude Code JSON output to extract the result text and session_id.
fn parse_claude_output(stdout: &str) -> anyhow::Result<(String, Option<String>)> {
    let json: serde_json::Value = serde_json::from_str(stdout).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse Claude Code output as JSON: {e}\nRaw output: {}",
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
        // Fall back to raw stdout if no result key
        return Ok((stdout.to_string(), session_id));
    }

    Ok((result_text, session_id))
}

/// Try to extract a question from Claude Code's output (e.g. AskUserQuestion).
fn extract_question(json: &serde_json::Value) -> Option<String> {
    // Claude Code may surface questions in the result text or as a specific field
    json.get("result")
        .and_then(|v| v.as_str())
        .filter(|text| text.contains('?'))
        .map(String::from)
}

/// Truncate a string to a max length, adding "..." if truncated.
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
        // Falls back to raw stdout
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
