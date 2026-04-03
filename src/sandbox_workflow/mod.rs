//! Sandbox coding workflow: E2B + Claude Code plan mode.
//!
//! Orchestrates end-to-end coding tasks:
//! 1. Spin up an E2B cloud sandbox
//! 2. Run Claude Code in plan mode
//! 3. Post plan to Slack for iterative review
//! 4. Execute on approval
//! 5. Create a GitHub PR
//! 6. Tear down the sandbox

pub mod e2b_client;
pub mod orchestrator;
pub mod slack_bridge;
pub mod state;

pub use orchestrator::{Orchestrator, WorkflowParams};
