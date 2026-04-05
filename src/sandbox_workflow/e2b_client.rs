//! E2B client for sandbox lifecycle management.
//!
//! Uses `reqwest` for the management API (create/destroy/status) and shells
//! out to a Python helper for command execution, since the E2B runtime uses
//! the Connect protocol (gRPC-over-HTTP) which requires protobuf framing.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

/// Result of a command executed inside a sandbox.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// E2B sandbox metadata returned on creation.
#[derive(Debug, Clone, Deserialize)]
pub struct SandboxInfo {
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    #[serde(rename = "clientID", default)]
    pub client_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateSandboxRequest {
    #[serde(rename = "templateID")]
    template_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    envs: HashMap<String, String>,
}

/// Client for E2B sandbox operations.
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
            .header("X-API-Key", &self.api_key)
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
    ///
    /// Uses the E2B Python SDK as a subprocess since the runtime API uses the
    /// Connect protocol (protobuf-framed gRPC-over-HTTP).
    pub async fn exec(
        &self,
        sandbox_id: &str,
        command: &str,
        timeout_secs: u64,
        cwd: Option<&str>,
    ) -> anyhow::Result<CommandResult> {
        let cwd_arg = cwd.unwrap_or("/home/user");
        let script = format!(
            r#"
import json, sys, os
os.environ["E2B_API_KEY"] = {api_key}
from e2b import Sandbox
sbx = Sandbox.connect({sandbox_id})
wrapped_cmd = 'bash -c ' + repr({cmd} + ' 2>&1; echo __EXIT_CODE__:$?')
r = sbx.commands.run(wrapped_cmd, cwd={cwd}, timeout={timeout})
stdout = r.stdout
code = 0
# Parse exit code from our marker
if '__EXIT_CODE__:' in stdout:
    lines = stdout.rsplit('__EXIT_CODE__:', 1)
    stdout = lines[0].rstrip()
    try:
        code = int(lines[1].strip())
    except Exception:
        pass
print(json.dumps({{"stdout": stdout, "stderr": r.stderr, "exit_code": code}}))
"#,
            api_key = py_str(&self.api_key),
            sandbox_id = py_str(sandbox_id),
            cmd = py_str(command),
            cwd = py_str(cwd_arg),
            timeout = timeout_secs,
        );

        let output = Command::new("python3")
            .arg("-c")
            .arg(&script)
            .env("E2B_API_KEY", &self.api_key)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to run E2B exec helper (is python3 + e2b installed?): {e}")
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && stdout.trim().is_empty() {
            anyhow::bail!("E2B exec helper failed: {stderr}");
        }

        serde_json::from_str(stdout.trim()).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse E2B exec output: {e}\nstdout: {stdout}\nstderr: {stderr}"
            )
        })
    }

    /// Write a file inside a sandbox.
    pub async fn write_file(
        &self,
        sandbox_id: &str,
        path: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let script = format!(
            r#"
import os
os.environ["E2B_API_KEY"] = {api_key}
from e2b import Sandbox
sbx = Sandbox.connect({sandbox_id})
sbx.files.write({path}, {content})
print("ok")
"#,
            api_key = py_str(&self.api_key),
            sandbox_id = py_str(sandbox_id),
            path = py_str(path),
            content = py_str(content),
        );

        let output = Command::new("python3")
            .arg("-c")
            .arg(&script)
            .env("E2B_API_KEY", &self.api_key)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to run E2B write helper: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("E2B write_file failed: {stderr}");
        }

        Ok(())
    }

    /// Check if a sandbox is still alive.
    pub async fn is_alive(&self, sandbox_id: &str) -> bool {
        let url = format!("{}/sandboxes/{}", self.api_url, sandbox_id);
        matches!(
            self.client
                .get(&url)
                .header("X-API-Key", &self.api_key)
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
            .header("X-API-Key", &self.api_key)
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

/// Escape a string as a Python string literal.
fn py_str(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("'{escaped}'")
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
        assert_eq!(json["templateID"], "my-template");
        assert_eq!(json["timeout"], 3600);
        assert!(json.get("envs").is_none());
    }

    #[test]
    fn py_str_escaping() {
        assert_eq!(py_str("hello"), "'hello'");
        assert_eq!(py_str("it's"), "'it\\'s'");
        assert_eq!(py_str("line1\nline2"), "'line1\\nline2'");
        assert_eq!(py_str("back\\slash"), "'back\\\\slash'");
    }
}
