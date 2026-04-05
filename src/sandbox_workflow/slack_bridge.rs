//! Slack integration for the sandbox coding workflow.
//!
//! Handles plan formatting, approval detection, and thread message routing.

use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// A formatted Slack message with Block Kit blocks and a plain-text fallback.
pub struct SlackMessage {
    /// Block Kit blocks array (sent as `blocks` in chat.postMessage).
    pub blocks: serde_json::Value,
    /// Plain text fallback (sent as `text` for notifications/accessibility).
    pub text: String,
}

/// Convert standard Markdown to Slack mrkdwn.
///
/// Slack mrkdwn differs from Markdown:
/// - Bold: `*text*` not `**text**`
/// - No heading syntax — `### Heading` renders literally
/// - No table support — `| col | col |` renders literally
/// - Links: `<url|text>` not `[text](url)`
/// - No `---` horizontal rules
fn md_to_mrkdwn(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Headings: # / ## / ### / #### → *bold*
        if let Some(rest) = line.strip_prefix("#### ") {
            out.push(format!("*{}*", rest.trim()));
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("### ") {
            out.push(format!("*{}*", rest.trim()));
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            out.push(format!("*{}*", rest.trim()));
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            out.push(format!("*{}*", rest.trim()));
            i += 1;
            continue;
        }

        // Horizontal rules → empty line (divider blocks handle visual separation)
        let trimmed = line.trim();
        if (trimmed == "---" || trimmed == "***" || trimmed == "___")
            && trimmed.len() >= 3
        {
            out.push(String::new());
            i += 1;
            continue;
        }

        // Tables: collect consecutive lines starting with |, wrap in code block
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let mut table_lines = Vec::new();
            while i < lines.len() {
                let tl = lines[i].trim();
                if tl.starts_with('|') && tl.ends_with('|') {
                    // Skip separator rows like |---|---|
                    if !tl.chars().all(|c| c == '|' || c == '-' || c == ':' || c == ' ') {
                        table_lines.push(tl);
                    }
                    i += 1;
                } else {
                    break;
                }
            }
            if !table_lines.is_empty() {
                // Parse columns to compute alignment widths
                let parsed: Vec<Vec<String>> = table_lines
                    .iter()
                    .map(|row| {
                        row.trim_matches('|')
                            .split('|')
                            .map(|cell| cell.trim().replace("**", "").to_string())
                            .collect()
                    })
                    .collect();

                let col_count = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
                let mut widths = vec![0usize; col_count];
                for row in &parsed {
                    for (j, cell) in row.iter().enumerate() {
                        if j < col_count {
                            widths[j] = widths[j].max(cell.len());
                        }
                    }
                }

                out.push("```".to_string());
                for row in &parsed {
                    let formatted: Vec<String> = row
                        .iter()
                        .enumerate()
                        .map(|(j, cell)| {
                            let w = if j < widths.len() { widths[j] } else { cell.len() };
                            format!("{:<width$}", cell, width = w)
                        })
                        .collect();
                    out.push(formatted.join("  "));
                }
                out.push("```".to_string());
            }
            continue;
        }

        // Regular line: convert inline markdown
        out.push(convert_inline_md(line));
        i += 1;
    }

    out.join("\n")
}

/// Convert inline Markdown formatting to Slack mrkdwn.
fn convert_inline_md(line: &str) -> String {
    let mut s = line.to_string();

    // Bold: **text** → *text* (do before italic to avoid conflicts)
    while let Some(start) = s.find("**") {
        if let Some(end) = s[start + 2..].find("**") {
            let end = start + 2 + end;
            let inner = &s[start + 2..end].to_string();
            s = format!("{}*{}*{}", &s[..start], inner, &s[end + 2..]);
        } else {
            break;
        }
    }

    // Links: [text](url) → <url|text>
    while let Some(bracket_start) = s.find('[') {
        if let Some(bracket_end) = s[bracket_start..].find("](") {
            let bracket_end = bracket_start + bracket_end;
            if let Some(paren_end) = s[bracket_end + 2..].find(')') {
                let paren_end = bracket_end + 2 + paren_end;
                let text = &s[bracket_start + 1..bracket_end].to_string();
                let url = &s[bracket_end + 2..paren_end].to_string();
                s = format!("{}<{}|{}>{}", &s[..bracket_start], url, text, &s[paren_end + 1..]);
            } else {
                break;
            }
        } else {
            break;
        }
    }

    s
}

/// Maximum characters in a single section block's mrkdwn text field.
const SECTION_TEXT_LIMIT: usize = 3000;

/// Maximum number of section blocks from text splitting (leave room for
/// header/divider blocks in the enclosing message; Slack allows 50 total).
const MAX_SECTION_BLOCKS: usize = 45;

/// Split long text into multiple section blocks, breaking at line boundaries.
fn text_to_section_blocks(text: &str) -> Vec<serde_json::Value> {
    if text.len() <= SECTION_TEXT_LIMIT {
        return vec![json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": text}
        })];
    }

    let mut blocks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() && blocks.len() < MAX_SECTION_BLOCKS {
        if remaining.len() <= SECTION_TEXT_LIMIT {
            blocks.push(json!({
                "type": "section",
                "text": {"type": "mrkdwn", "text": remaining}
            }));
            break;
        }

        let chunk = &remaining[..SECTION_TEXT_LIMIT];
        let split_at = chunk.rfind('\n').unwrap_or(SECTION_TEXT_LIMIT);
        let split_at = if split_at == 0 { SECTION_TEXT_LIMIT } else { split_at };

        blocks.push(json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": &remaining[..split_at]}
        }));
        remaining = remaining[split_at..].trim_start_matches('\n');
    }

    if !remaining.is_empty() && blocks.len() >= MAX_SECTION_BLOCKS {
        blocks.push(json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": "_(truncated)_"}]
        }));
    }

    blocks
}

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

/// Cancel keywords.
const CANCEL_KEYWORDS: &[&str] = &["cancel", "abort", "stop", "nevermind", "never mind"];

/// Check if a message is an approval.
pub fn is_approval(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    APPROVAL_KEYWORDS.iter().any(|kw| {
        normalized == *kw || normalized == format!("{kw}!") || normalized == format!("{kw}.")
    })
}

/// Check if a message is a cancellation.
pub fn is_cancel(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    CANCEL_KEYWORDS.iter().any(|kw| {
        normalized == *kw || normalized == format!("{kw}!") || normalized == format!("{kw}.")
    })
}

/// Format the short channel message (low footprint).
pub fn format_plan_summary(
    type_label: &str,
    workflow_id: &str,
    repo_url: &str,
    task: &str,
    needs_approval: bool,
) -> SlackMessage {
    let repo_name = if repo_url.is_empty() {
        "N/A".to_string()
    } else {
        repo_url.rsplit('/').next().unwrap_or(repo_url).to_string()
    };

    let action = if needs_approval {
        "Reply in thread: *lgtm* to approve, *cancel* to abort"
    } else {
        "Results will appear in this thread"
    };

    let blocks = json!([
        {
            "type": "header",
            "text": {"type": "plain_text", "text": format!("{type_label}  {workflow_id}"), "emoji": true}
        },
        {
            "type": "section",
            "fields": [
                {"type": "mrkdwn", "text": format!("*Type:*\n{type_label}")},
                {"type": "mrkdwn", "text": format!("*Repo:*\n`{repo_name}`")}
            ]
        },
        {
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!("*Task:*\n{task}")}
        },
        {"type": "divider"},
        {
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": action}]
        }
    ]);

    let repo_part = if repo_url.is_empty() {
        String::new()
    } else {
        format!(" in `{repo_name}`")
    };
    let text = format!("{type_label} {workflow_id} -- {task}{repo_part}\n{action}");

    SlackMessage { blocks, text }
}

/// Format the full plan (posted as a thread reply).
pub fn format_plan_detail(plan_text: &str) -> SlackMessage {
    let converted = md_to_mrkdwn(plan_text);
    let mut blocks = vec![
        json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": ":clipboard: *Plan:*"}
        }),
        json!({"type": "divider"}),
    ];
    blocks.extend(text_to_section_blocks(&converted));

    let text = format!("Plan:\n\n{plan_text}");
    SlackMessage { blocks: json!(blocks), text }
}

/// Format a revised plan update.
pub fn format_revised_plan(plan_text: &str, iteration: u32) -> SlackMessage {
    let converted = md_to_mrkdwn(plan_text);
    let mut blocks = vec![
        json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!(":arrows_counterclockwise: *Revised Plan* (iteration {iteration})")}
        }),
        json!({"type": "divider"}),
    ];
    blocks.extend(text_to_section_blocks(&converted));
    blocks.push(json!({"type": "divider"}));
    blocks.push(json!({
        "type": "context",
        "elements": [{"type": "mrkdwn", "text": "Reply to refine further, or *approve* to execute."}]
    }));

    let text = format!("Revised Plan (iteration {iteration})\n\n{plan_text}\n---\nReply to refine further, or approve to execute.");
    SlackMessage { blocks: json!(blocks), text }
}

/// Format an execution status update.
pub fn format_execution_started() -> SlackMessage {
    let blocks = json!([{
        "type": "section",
        "text": {"type": "mrkdwn", "text": ":rocket: Plan approved. Executing changes..."}
    }]);
    SlackMessage { blocks, text: "Plan approved. Executing changes...".to_string() }
}

/// Format a PR completion message.
pub fn format_pr_created(pr_url: &str) -> SlackMessage {
    let blocks = json!([{
        "type": "section",
        "text": {"type": "mrkdwn", "text": format!(":white_check_mark: *PR created:* <{pr_url}>")}
    }]);
    SlackMessage { blocks, text: format!("PR created: {pr_url}") }
}

/// Format a failure notification.
pub fn format_failure(reason: &str) -> SlackMessage {
    let blocks = json!([{
        "type": "section",
        "text": {"type": "mrkdwn", "text": format!(":x: *Workflow failed:* {reason}")}
    }]);
    SlackMessage { blocks, text: format!("Workflow failed: {reason}") }
}

/// Format a question from Claude Code that needs user input.
pub fn format_question(question: &str) -> SlackMessage {
    let blocks = json!([
        {
            "type": "section",
            "text": {"type": "mrkdwn", "text": ":question: *Claude Code needs input:*"}
        },
        {
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!("> {question}")}
        },
        {
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": "Reply in this thread to answer."}]
        }
    ]);
    let text = format!("Claude Code needs input:\n\n> {question}\n\nReply in this thread to answer.");
    SlackMessage { blocks, text }
}

/// Format execution results (admin/data workflow output).
pub fn format_results(result_text: &str) -> SlackMessage {
    let converted = md_to_mrkdwn(result_text);
    let mut blocks = vec![
        json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": ":bar_chart: *Results:*"}
        }),
        json!({"type": "divider"}),
    ];
    blocks.extend(text_to_section_blocks(&converted));

    let text = format!("Results:\n\n{result_text}");
    SlackMessage { blocks: json!(blocks), text }
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
        let msg = format_plan_summary(
            "Sandbox",
            "abc123",
            "https://github.com/test/repo",
            "add tests",
            true,
        );
        assert!(msg.text.contains("abc123"));
        assert!(msg.text.contains("repo"));
        assert!(msg.text.contains("add tests"));
        assert!(msg.text.contains("lgtm"));
        assert!(msg.blocks.is_array());
        let blocks = msg.blocks.as_array().unwrap();
        assert!(!blocks.is_empty());
        assert_eq!(blocks[0]["type"], "header");
    }

    #[test]
    fn md_to_mrkdwn_headings() {
        assert_eq!(md_to_mrkdwn("# Title"), "*Title*");
        assert_eq!(md_to_mrkdwn("## Subtitle"), "*Subtitle*");
        assert_eq!(md_to_mrkdwn("### Section"), "*Section*");
        assert_eq!(md_to_mrkdwn("#### Sub-section"), "*Sub-section*");
    }

    #[test]
    fn md_to_mrkdwn_bold() {
        assert_eq!(md_to_mrkdwn("this is **bold** text"), "this is *bold* text");
        assert_eq!(
            md_to_mrkdwn("**Total Pageviews**: 67"),
            "*Total Pageviews*: 67"
        );
    }

    #[test]
    fn md_to_mrkdwn_links() {
        assert_eq!(
            md_to_mrkdwn("visit [Google](https://google.com) now"),
            "visit <https://google.com|Google> now"
        );
    }

    #[test]
    fn md_to_mrkdwn_horizontal_rule() {
        assert_eq!(md_to_mrkdwn("above\n---\nbelow"), "above\n\nbelow");
    }

    #[test]
    fn md_to_mrkdwn_table() {
        let md = "| Metric | Value |\n|--------|-------|\n| **Views** | 67 |\n| **Users** | 2 |";
        let result = md_to_mrkdwn(md);
        assert!(result.contains("```"));
        assert!(result.contains("Views"));
        assert!(result.contains("67"));
        // Bold markers should be stripped inside table
        assert!(!result.contains("**"));
    }

    #[test]
    fn md_to_mrkdwn_mixed() {
        let md = "## Key Metrics\n\n| Metric | Value |\n|--------|-------|\n| **Views** | 67 |\n\nSome **bold** text.";
        let result = md_to_mrkdwn(md);
        assert!(result.starts_with("*Key Metrics*"));
        assert!(result.contains("```"));
        assert!(result.contains("Some *bold* text."));
    }

    #[test]
    fn text_to_section_blocks_short() {
        let blocks = text_to_section_blocks("short text");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "section");
    }

    #[test]
    fn text_to_section_blocks_splits_long() {
        let long_text = "line\n".repeat(1000);
        let blocks = text_to_section_blocks(&long_text);
        assert!(blocks.len() > 1);
        for block in &blocks {
            if block["type"] == "section" {
                let len = block["text"]["text"].as_str().unwrap().len();
                assert!(len <= SECTION_TEXT_LIMIT);
            }
        }
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
