//! E2B REST API client for sandbox lifecycle management.
//!
//! Talks directly to the E2B API via `reqwest` — no Python/JS sidecar needed.

use base64::Engine as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Result of a command executed inside a sandbox.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// E2B sandbox metadata returned on creation.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxInfo {
    pub sandbox_id: String,
    #[serde(default)]
    pub client_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateSandboxRequest {
    template_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    envs: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ExecRequest {
    cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    envs: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecResponse {
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    exit_code: i32,
}

/// Lightweight client for the E2B sandbox REST API.
pub struct E2bClient {
    client: Client,
    api_url: String,
    api_key: String,
}

impl E2bClient {
    pub fn new(api_url: &str, api_key: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_url: api_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    /// Create a new sandbox from a template.
    pub async fn create_sandbox(
        &self,
        template: &str,
        envs: HashMap<String, String>,
        timeout_secs: u64,
    ) -> anyhow::Result<SandboxInfo> {
        let url = format!("{}/sandboxes", self.api_url);
        let body = CreateSandboxRequest {
            template_id: template.to_string(),
            timeout: Some(timeout_secs),
            envs,
        };

        let resp = self
            .client
            .post(&url)
            .header("X-E2B-API-Key", &self.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("E2B create sandbox failed ({status}): {text}");
        }

        Ok(resp.json().await?)
    }

    /// Execute a shell command inside a running sandbox.
    pub async fn exec(
        &self,
        sandbox_id: &str,
        command: &str,
        timeout_secs: u64,
        cwd: Option<&str>,
    ) -> anyhow::Result<CommandResult> {
        let url = format!("{}/sandboxes/{}/commands", self.api_url, sandbox_id);
        let body = ExecRequest {
            cmd: command.to_string(),
            timeout: Some(timeout_secs),
            envs: HashMap::new(),
            cwd: cwd.map(String::from),
        };

        let resp = self
            .client
            .post(&url)
            .header("X-E2B-API-Key", &self.api_key)
            .timeout(Duration::from_secs(timeout_secs + 30))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("E2B exec failed ({status}): {text}");
        }

        let exec_resp: ExecResponse = resp.json().await?;
        Ok(CommandResult {
            stdout: exec_resp.stdout,
            stderr: exec_resp.stderr,
            exit_code: exec_resp.exit_code,
        })
    }

    /// Write a file inside a sandbox. Creates parent directories automatically.
    ///
    /// Uses the sandbox exec API to write via heredoc — simpler and more
    /// reliable than the files endpoint for small text files like credentials.
    pub async fn write_file(
        &self,
        sandbox_id: &str,
        path: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        // Use base64 to avoid shell escaping issues with the file content
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        let cmd = format!(
            "mkdir -p $(dirname {path}) && echo '{encoded}' | base64 -d > {path}"
        );
        let result = self.exec(sandbox_id, &cmd, 30, None).await?;
        if result.exit_code != 0 {
            anyhow::bail!("Failed to write {path}: {}", result.stderr);
        }
        Ok(())
    }

    /// Check if a sandbox is still alive.
    pub async fn is_alive(&self, sandbox_id: &str) -> bool {
        let url = format!("{}/sandboxes/{}", self.api_url, sandbox_id);
        matches!(
            self.client
                .get(&url)
                .header("X-E2B-API-Key", &self.api_key)
                .send()
                .await,
            Ok(resp) if resp.status().is_success()
        )
    }

    /// Destroy a sandbox (idempotent — ignores 404).
    pub async fn destroy(&self, sandbox_id: &str) -> anyhow::Result<()> {
        let url = format!("{}/sandboxes/{}", self.api_url, sandbox_id);
        let resp = self
            .client
            .delete(&url)
            .header("X-E2B-API-Key", &self.api_key)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 404 {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("E2B destroy failed ({status}): {text}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_construction() {
        let client = E2bClient::new("https://api.e2b.dev", "test-key");
        assert_eq!(client.api_url, "https://api.e2b.dev");
    }

    #[test]
    fn client_trims_trailing_slash() {
        let client = E2bClient::new("https://api.e2b.dev/", "test-key");
        assert_eq!(client.api_url, "https://api.e2b.dev");
    }

    #[test]
    fn create_sandbox_request_serialization() {
        let req = CreateSandboxRequest {
            template_id: "my-template".into(),
            timeout: Some(3600),
            envs: HashMap::new(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["templateId"], "my-template");
        assert_eq!(json["timeout"], 3600);
        // Empty envs should be omitted
        assert!(json.get("envs").is_none());
    }
}
