//! Workflow state machine types and persistence.

use serde::{Deserialize, Serialize};

/// Task type determines the workflow shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// Analyze code in a repo, post findings to Slack. No approval, no PR.
    Research,
    /// Query data via MCP tools (PostHog, SigNoz, etc.), post results. No repo, no approval, no PR.
    Data,
    /// Run admin API calls via MCP tools. Requires approval before executing. No PR.
    Admin,
    /// Make code changes in a repo, create a PR. Requires plan approval.
    #[default]
    Eng,
}

impl TaskType {
    pub fn needs_repo(&self) -> bool {
        matches!(self, Self::Research | Self::Eng)
    }

    pub fn needs_approval(&self) -> bool {
        matches!(self, Self::Admin | Self::Eng)
    }

    pub fn creates_pr(&self) -> bool {
        matches!(self, Self::Eng)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Research => "Research",
            Self::Data => "Data query",
            Self::Admin => "Admin",
            Self::Eng => "Sandbox",
        }
    }
}

/// States in the sandbox coding workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    /// Sandbox is being created and repo cloned.
    Initializing,
    /// Claude Code is generating a plan.
    Planning,
    /// Plan posted to Slack, waiting for user feedback or approval.
    AwaitingFeedback,
    /// Claude Code is revising the plan based on feedback.
    Revising,
    /// Plan approved, Claude Code is executing.
    Executing,
    /// Code changes done, creating a GitHub PR.
    CreatingPr,
    /// Workflow completed successfully.
    Completed,
    /// Workflow failed.
    Failed { reason: String },
}

impl WorkflowState {
    /// Returns true if this is a terminal state (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed { .. })
    }

    /// Short label for Slack status updates.
    pub fn label(&self) -> &str {
        match self {
            Self::Initializing => "initializing",
            Self::Planning => "planning",
            Self::AwaitingFeedback => "awaiting feedback",
            Self::Revising => "revising plan",
            Self::Executing => "executing",
            Self::CreatingPr => "creating PR",
            Self::Completed => "completed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Persistent record of a workflow instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRecord {
    pub workflow_id: String,
    #[serde(default)]
    pub task_type: TaskType,
    pub state: WorkflowState,
    pub sandbox_id: Option<String>,
    /// Claude Code session ID for `--resume`.
    pub session_id: Option<String>,
    pub repo_url: String,
    pub task: String,
    pub slack_channel: String,
    pub slack_thread_ts: Option<String>,
    pub plan_text: Option<String>,
    pub pr_url: Option<String>,
    pub iteration_count: u32,
    /// Base branch to work from (e.g. "main").
    pub base_branch: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl WorkflowRecord {
    pub fn new(workflow_id: String, repo_url: String, task: String, slack_channel: String) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            workflow_id,
            task_type: TaskType::default(),
            state: WorkflowState::Initializing,
            sandbox_id: None,
            session_id: None,
            repo_url,
            task,
            slack_channel,
            slack_thread_ts: None,
            plan_text: None,
            pr_url: None,
            iteration_count: 0,
            base_branch: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Transition to a new state, updating the timestamp.
    pub fn transition(&mut self, new_state: WorkflowState) {
        self.state = new_state;
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }

    /// Save to a JSON file in the workflows directory.
    pub fn save(&self, workflows_dir: &std::path::Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(workflows_dir)?;
        let path = workflows_dir.join(format!("{}.json", self.workflow_id));
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load from a JSON file.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let json = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&json)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_state_terminal() {
        assert!(!WorkflowState::Initializing.is_terminal());
        assert!(!WorkflowState::AwaitingFeedback.is_terminal());
        assert!(WorkflowState::Completed.is_terminal());
        assert!(
            WorkflowState::Failed {
                reason: "test".into()
            }
            .is_terminal()
        );
    }

    #[test]
    fn workflow_record_new_and_transition() {
        let mut record = WorkflowRecord::new(
            "wf-1".into(),
            "https://github.com/test/repo".into(),
            "add tests".into(),
            "C123".into(),
        );
        assert_eq!(record.state, WorkflowState::Initializing);

        record.transition(WorkflowState::Planning);
        assert_eq!(record.state, WorkflowState::Planning);
        assert!(record.updated_at >= record.created_at);
    }

    #[test]
    fn workflow_record_serialization_roundtrip() {
        let record = WorkflowRecord::new(
            "wf-2".into(),
            "https://github.com/test/repo".into(),
            "fix bug".into(),
            "C456".into(),
        );
        let json = serde_json::to_string(&record).unwrap();
        let deserialized: WorkflowRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.workflow_id, "wf-2");
        assert_eq!(deserialized.state, WorkflowState::Initializing);
    }

    #[test]
    fn workflow_record_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = WorkflowRecord::new(
            "wf-disk".into(),
            "https://github.com/test/repo".into(),
            "test persistence".into(),
            "C789".into(),
        );
        record.sandbox_id = Some("sb-123".into());
        record.session_id = Some("sess-456".into());
        record.transition(WorkflowState::AwaitingFeedback);
        record.plan_text = Some("1. Do stuff\n2. More stuff".into());
        record.iteration_count = 2;

        record.save(dir.path()).unwrap();

        let loaded = WorkflowRecord::load(&dir.path().join("wf-disk.json")).unwrap();
        assert_eq!(loaded.workflow_id, "wf-disk");
        assert_eq!(loaded.state, WorkflowState::AwaitingFeedback);
        assert_eq!(loaded.sandbox_id.as_deref(), Some("sb-123"));
        assert_eq!(loaded.session_id.as_deref(), Some("sess-456"));
        assert_eq!(
            loaded.plan_text.as_deref(),
            Some("1. Do stuff\n2. More stuff")
        );
        assert_eq!(loaded.iteration_count, 2);
    }
}
