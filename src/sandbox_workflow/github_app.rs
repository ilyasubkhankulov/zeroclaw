//! GitHub App authentication for sandbox workflows.
//!
//! Generates short-lived installation access tokens from a GitHub App's
//! credentials (App ID + private key PEM). This replaces Personal Access
//! Tokens for automated git operations, complying with GitHub's ToS.

use base64::Engine as _;
use reqwest::Client;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use std::time::{SystemTime, UNIX_EPOCH};

/// GitHub App credentials for generating installation access tokens.
pub struct GitHubAppAuth {
    app_id: String,
    key_pair: RsaKeyPair,
    installation_id: u64,
    client: Client,
}

impl GitHubAppAuth {
    /// Create from App ID, PEM-encoded private key, and installation ID.
    pub fn new(app_id: &str, private_key_pem: &str, installation_id: u64) -> anyhow::Result<Self> {
        let der = pem_to_der(private_key_pem)?;
        let key_pair = RsaKeyPair::from_pkcs8(&der)
            .map_err(|e| anyhow::anyhow!("Invalid RSA private key: {e}"))?;

        Ok(Self {
            app_id: app_id.to_string(),
            key_pair,
            installation_id,
            client: Client::new(),
        })
    }

    /// Load from config fields (app_id, private key file path, installation_id).
    pub fn from_config(
        app_id: &str,
        private_key_path: &str,
        installation_id: u64,
    ) -> anyhow::Result<Self> {
        let pem = std::fs::read_to_string(private_key_path).map_err(|e| {
            anyhow::anyhow!("Failed to read GitHub App private key at {private_key_path}: {e}")
        })?;
        Self::new(app_id, &pem, installation_id)
    }

    /// Generate a short-lived installation access token.
    ///
    /// 1. Creates a JWT signed with the App's private key (RS256, 10min expiry)
    /// 2. Exchanges it for an installation token via GitHub API
    ///
    /// The returned token can be used like a PAT for git clone, push, and `gh` CLI.
    pub async fn get_installation_token(&self) -> anyhow::Result<String> {
        let jwt = self.generate_jwt()?;

        let resp = self
            .client
            .post(format!(
                "https://api.github.com/app/installations/{}/access_tokens",
                self.installation_id
            ))
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "zeroclaw-sandbox-workflow")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub App token exchange failed ({status}): {body}");
        }

        let body: serde_json::Value = resp.json().await?;
        body.get("token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("GitHub API response missing 'token' field"))
    }

    /// Generate a JWT for GitHub App authentication.
    ///
    /// Claims:
    /// - `iss`: App ID
    /// - `iat`: now - 60s (clock drift tolerance)
    /// - `exp`: now + 600s (10 minutes, GitHub maximum)
    fn generate_jwt(&self) -> anyhow::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| anyhow::anyhow!("System clock error: {e}"))?
            .as_secs();

        let header = base64_url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64_url_encode(
            format!(
                r#"{{"iss":"{}","iat":{},"exp":{}}}"#,
                self.app_id,
                now - 60,
                now + 600,
            )
            .as_bytes(),
        );

        let signing_input = format!("{header}.{payload}");
        let rng = ring::rand::SystemRandom::new();
        let mut signature = vec![0u8; self.key_pair.public().modulus_len()];
        self.key_pair
            .sign(
                &RSA_PKCS1_SHA256,
                &rng,
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|e| anyhow::anyhow!("JWT signing failed: {e}"))?;

        let sig_b64 = base64_url_encode(&signature);
        Ok(format!("{signing_input}.{sig_b64}"))
    }
}

/// Base64url-encode without padding (JWT standard).
fn base64_url_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Extract DER bytes from a PEM-encoded RSA private key.
fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    let b64: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");

    base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode PEM base64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_url_encode_works() {
        let encoded = base64_url_encode(b"hello");
        assert_eq!(encoded, "aGVsbG8");
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
    }

    #[test]
    fn pem_to_der_strips_headers() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\naGVsbG8=\n-----END RSA PRIVATE KEY-----\n";
        let der = pem_to_der(pem).unwrap();
        assert_eq!(der, b"hello");
    }

    #[test]
    fn pem_to_der_handles_pkcs8() {
        let pem = "-----BEGIN PRIVATE KEY-----\nd29ybGQ=\n-----END PRIVATE KEY-----";
        let der = pem_to_der(pem).unwrap();
        assert_eq!(der, b"world");
    }
}
