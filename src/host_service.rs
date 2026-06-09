use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use url::{Host, Url};

use crate::superset_runtime::SupersetHostRuntime;

const HOST_SERVICE_TIMEOUT: Duration = Duration::from_secs(5);
const KILL_SESSION_PROCEDURE: &str = "terminal.killSession";
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostServiceManifest {
    pub endpoint: String,
    pub auth_token: String,
    pub organization_id: String,
}

impl fmt::Debug for HostServiceManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostServiceManifest")
            .field("endpoint", &self.endpoint)
            .field("auth_token", &"<redacted>")
            .field("organization_id", &self.organization_id)
            .finish()
    }
}

pub struct HostServiceClient {
    endpoint: String,
    auth_token: String,
    agent: ureq::Agent,
}

pub fn host_service_manifest_path(superset_home: &Path, organization_id: &str) -> PathBuf {
    superset_home
        .join("host")
        .join(organization_id)
        .join("manifest.json")
}

pub fn load_host_service_manifest(
    superset_home: &Path,
    organization_id: &str,
) -> Result<HostServiceManifest> {
    let path = host_service_manifest_path(superset_home, organization_id);
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut manifest: HostServiceManifest =
        serde_json::from_str(&contents).context("failed to parse host-service manifest")?;
    if manifest.organization_id != organization_id {
        bail!(
            "host-service manifest organization mismatch: expected {}, got {}",
            organization_id,
            manifest.organization_id
        );
    }
    if manifest.endpoint.trim().is_empty() {
        bail!("host-service manifest omitted endpoint");
    }
    manifest.endpoint = validate_host_service_endpoint(&manifest.endpoint)?;
    if manifest.auth_token.is_empty() {
        bail!("host-service manifest omitted authToken");
    }
    Ok(manifest)
}

impl HostServiceClient {
    pub fn connect(superset_home: &Path, runtime: &SupersetHostRuntime) -> Result<Self> {
        let manifest = load_host_service_manifest(superset_home, &runtime.organization_id)?;
        Ok(Self::from_manifest(manifest))
    }

    pub fn from_manifest(manifest: HostServiceManifest) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(HOST_SERVICE_TIMEOUT)
            .build();
        Self {
            endpoint: manifest.endpoint.trim_end_matches('/').to_owned(),
            auth_token: manifest.auth_token,
            agent,
        }
    }

    pub fn kill_session(&self, session_id: &str, workspace_id: &str) -> Result<()> {
        let url = trpc_procedure_url(&self.endpoint, KILL_SESSION_PROCEDURE);
        let payload = kill_session_payload(session_id, workspace_id);
        let body = serde_json::to_string(&payload)
            .context("failed to encode host-service terminal.killSession request")?;
        let auth_header = format!("Bearer {}", self.auth_token);

        match self
            .agent
            .post(&url)
            .set("Authorization", &auth_header)
            .set("Content-Type", "application/json")
            .send_string(&body)
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(status, response)) => {
                let response_body = response.into_string().unwrap_or_default();
                bail!(
                    "host-service terminal.killSession failed with HTTP {}: {}",
                    status,
                    summarize_error_body(&response_body)
                );
            }
            Err(error) => {
                bail!("failed to call host-service terminal.killSession at {url}: {error}");
            }
        }
    }
}

fn trpc_procedure_url(endpoint: &str, procedure: &str) -> String {
    format!("{}/trpc/{}", endpoint.trim_end_matches('/'), procedure)
}

fn validate_host_service_endpoint(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim();
    let url = Url::parse(trimmed).context("failed to parse host-service manifest endpoint")?;
    if url.scheme() != "http" {
        bail!("host-service manifest endpoint must use http");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("host-service manifest endpoint must not include credentials");
    }
    let Some(host) = url.host() else {
        bail!("host-service manifest endpoint omitted host");
    };
    if !is_loopback_host(host) {
        bail!("host-service manifest endpoint must be a loopback address");
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

fn is_loopback_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

fn kill_session_payload(session_id: &str, workspace_id: &str) -> Value {
    json!({
        "json": {
            "terminalId": session_id,
            "workspaceId": workspace_id,
        }
    })
}

fn summarize_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "<empty response body>".to_owned();
    }
    if trimmed.len() <= MAX_ERROR_BODY_BYTES {
        return trimmed.to_owned();
    }
    let end = trimmed
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= MAX_ERROR_BODY_BYTES)
        .last()
        .unwrap_or(0);
    format!("{}...", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        host_service_manifest_path, kill_session_payload, load_host_service_manifest,
        trpc_procedure_url, validate_host_service_endpoint,
    };

    #[test]
    fn manifest_path_uses_organization_host_directory() {
        let path = host_service_manifest_path(PathBuf::from("/tmp/superset").as_path(), "org-1");

        assert_eq!(
            path,
            PathBuf::from("/tmp/superset/host/org-1/manifest.json")
        );
    }

    #[test]
    fn loads_host_service_manifest() {
        let root = test_root("manifest");
        let org_dir = root.join("host").join("org-1");
        fs::create_dir_all(&org_dir).unwrap();
        fs::write(
            org_dir.join("manifest.json"),
            r#"{
                "pid": 123,
                "endpoint": "http://127.0.0.1:48937",
                "authToken": "token",
                "startedAt": 1780927864043,
                "organizationId": "org-1"
            }"#,
        )
        .unwrap();

        let manifest = load_host_service_manifest(&root, "org-1").unwrap();

        assert_eq!(manifest.endpoint, "http://127.0.0.1:48937");
        assert_eq!(manifest.auth_token, "token");
        assert_eq!(manifest.organization_id, "org-1");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_manifest_organization_mismatch() {
        let root = test_root("manifest-mismatch");
        let org_dir = root.join("host").join("org-1");
        fs::create_dir_all(&org_dir).unwrap();
        fs::write(
            org_dir.join("manifest.json"),
            r#"{
                "pid": 123,
                "endpoint": "http://127.0.0.1:48937",
                "authToken": "token",
                "startedAt": 1780927864043,
                "organizationId": "other-org"
            }"#,
        )
        .unwrap();

        let error = load_host_service_manifest(&root, "org-1").unwrap_err();

        assert!(error.to_string().contains("organization mismatch"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn kill_session_url_trims_endpoint_slash() {
        assert_eq!(
            trpc_procedure_url("http://127.0.0.1:48937/", "terminal.killSession"),
            "http://127.0.0.1:48937/trpc/terminal.killSession"
        );
    }

    #[test]
    fn validates_loopback_host_service_endpoints() {
        assert_eq!(
            validate_host_service_endpoint("http://127.0.0.1:48937/").unwrap(),
            "http://127.0.0.1:48937"
        );
        assert_eq!(
            validate_host_service_endpoint("http://localhost:48937").unwrap(),
            "http://localhost:48937"
        );
        assert_eq!(
            validate_host_service_endpoint("http://[::1]:48937").unwrap(),
            "http://[::1]:48937"
        );
    }

    #[test]
    fn rejects_non_loopback_host_service_endpoint() {
        let error = validate_host_service_endpoint("http://example.com:48937").unwrap_err();

        assert!(error.to_string().contains("loopback"));
    }

    #[test]
    fn rejects_host_service_endpoint_credentials() {
        let error = validate_host_service_endpoint("http://user:pass@127.0.0.1:48937").unwrap_err();

        assert!(error.to_string().contains("credentials"));
    }

    #[test]
    fn kill_session_payload_uses_trpc_json_envelope() {
        let payload = kill_session_payload("session-1", "workspace-1");

        assert_eq!(
            payload,
            serde_json::json!({
                "json": {
                    "terminalId": "session-1",
                    "workspaceId": "workspace-1"
                }
            })
        );
    }

    fn test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ssctl-host-service-{name}-{}-{unique}",
            std::process::id()
        ))
    }
}
