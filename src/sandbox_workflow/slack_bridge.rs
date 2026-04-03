//! Slack integration for the sandbox coding workflow.
//!
//! Handles plan formatting, approval detection, and thread message routing.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Message forwarded from a Slack thread to an active workflow.
#[derive(Debug, Clone)]
pub struct ThreadMessage {
    pub text: String,
    pub sender: String,
}

/// Approval keywords (case-insensitive matching against the full message,
/// trimmed and lowercased).
const APPROVAL_KEYWORDS: &[&str] = &[
    "approve", "approved", "lgtm", "ship it", "go", "execute", "run it",
];

/// Check if a message is an approval.
pub fn is_approval(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    APPROVAL_KEYWORDS.iter().any(|kw| {
        normalized == *kw || normalized == format!("{kw}!") || normalized == format!("{kw}.")
    })
}

/// Format a plan for posting to Slack.
pub fn format_plan_message(repo_url: &str, task: &str, plan_text: &str) -> String {
    format!(
        "*Sandbox Coding Plan* for `{repo_url}`\n\
         *Task:* {task}\n\n\
         ---\n\
         {plan_text}\n\
         ---\n\n\
         Reply in this thread to refine the plan.\n\
         Reply *approve* or *lgtm* to execute."
    )
}

/// Format a revised plan update.
pub fn format_revised_plan(plan_text: &str, iteration: u32) -> String {
    format!(
        "*Revised Plan* (iteration {iteration})\n\n\
         ---\n\
         {plan_text}\n\
         ---\n\n\
         Reply to refine further, or *approve* to execute."
    )
}

/// Format an execution status update.
pub fn format_execution_started() -> String {
    "Plan approved. Executing changes...".to_string()
}

/// Format a PR completion message.
pub fn format_pr_created(pr_url: &str) -> String {
    format!("PR created: {pr_url}")
}

/// Format a failure notification.
pub fn format_failure(reason: &str) -> String {
    format!("Workflow failed: {reason}")
}

/// Format a question from Claude Code that needs user input.
pub fn format_question(question: &str) -> String {
    format!(
        "Claude Code needs input:\n\n\
         > {question}\n\n\
         Reply in this thread to answer."
    )
}

/// Inner map type: `(channel_id, thread_ts) -> workflow message sender`.
type RouteMap = HashMap<(String, String), mpsc::Sender<ThreadMessage>>;

/// Thread router: maps (channel_id, thread_ts) to workflow message senders.
///
/// Used as a pre-dispatch filter: before a Slack message reaches the normal
/// agent loop, check if its thread maps to an active workflow.
pub struct ThreadRouter {
    routes: Arc<Mutex<RouteMap>>,
}

impl ThreadRouter {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a workflow's thread for message routing.
    pub async fn register(
        &self,
        channel_id: String,
        thread_ts: String,
        sender: mpsc::Sender<ThreadMessage>,
    ) {
        self.routes
            .lock()
            .await
            .insert((channel_id, thread_ts), sender);
    }

    /// Remove a workflow's thread from routing.
    pub async fn unregister(&self, channel_id: &str, thread_ts: &str) {
        self.routes
            .lock()
            .await
            .remove(&(channel_id.to_string(), thread_ts.to_string()));
    }

    /// Try to route a thread message to an active workflow.
    /// Returns `true` if the message was consumed (workflow exists).
    pub async fn try_route(
        &self,
        channel_id: &str,
        thread_ts: &str,
        text: &str,
        sender: &str,
    ) -> bool {
        let routes = self.routes.lock().await;
        if let Some(tx) = routes.get(&(channel_id.to_string(), thread_ts.to_string())) {
            let _ = tx
                .send(ThreadMessage {
                    text: text.to_string(),
                    sender: sender.to_string(),
                })
                .await;
            true
        } else {
            false
        }
    }

    /// Get a clonable handle to the inner routes for sharing across tasks.
    pub fn handle(&self) -> Arc<Mutex<RouteMap>> {
        Arc::clone(&self.routes)
    }
}

impl Default for ThreadRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_detection() {
        assert!(is_approval("approve"));
        assert!(is_approval("LGTM"));
        assert!(is_approval("  Ship it  "));
        assert!(is_approval("approve!"));
        assert!(is_approval("go."));
        assert!(!is_approval("I don't approve of this approach"));
        assert!(!is_approval("please revise the plan"));
        assert!(!is_approval(""));
    }

    #[test]
    fn plan_formatting() {
        let msg = format_plan_message(
            "https://github.com/test/repo",
            "add tests",
            "1. Create test file\n2. Write tests",
        );
        assert!(msg.contains("Sandbox Coding Plan"));
        assert!(msg.contains("test/repo"));
        assert!(msg.contains("add tests"));
        assert!(msg.contains("approve"));
    }

    #[tokio::test]
    async fn thread_router_basic() {
        let router = ThreadRouter::new();
        let (tx, mut rx) = mpsc::channel(1);

        router.register("C123".into(), "1234.5678".into(), tx).await;

        let routed = router
            .try_route("C123", "1234.5678", "looks good", "user1")
            .await;
        assert!(routed);

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.text, "looks good");
        assert_eq!(msg.sender, "user1");
    }

    #[tokio::test]
    async fn thread_router_miss() {
        let router = ThreadRouter::new();
        let routed = router
            .try_route("C123", "9999.0000", "hello", "user1")
            .await;
        assert!(!routed);
    }

    #[tokio::test]
    async fn thread_router_unregister() {
        let router = ThreadRouter::new();
        let (tx, _rx) = mpsc::channel(1);

        router.register("C123".into(), "1234.5678".into(), tx).await;
        router.unregister("C123", "1234.5678").await;

        let routed = router
            .try_route("C123", "1234.5678", "hello", "user1")
            .await;
        assert!(!routed);
    }
}
