use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::TERMINAL_HOST_PROTOCOL_VERSION;
use crate::terminal_host::TerminalSessionInfo;

pub const REGISTRY_SCHEMA_VERSION: u64 = 1;

const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const PENDING_SPAWN_MAX_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
pub struct RegistryStore {
    root: PathBuf,
    registry_path: PathBuf,
    lock_path: PathBuf,
    audit_path: PathBuf,
    messages_dir: PathBuf,
}

#[derive(Debug)]
pub struct RegistryLock {
    path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub schema_version: u64,
    pub roles: BTreeMap<String, RegistryEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending_spawns: BTreeMap<String, PendingSpawnEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEntry {
    pub agent: String,
    pub workspace_id: String,
    pub session_id: String,
    pub created_at: String,
    pub last_verified_at: String,
    pub terminal_host_protocol_version: u64,
    pub source: String,
    pub owner: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingSpawnEntry {
    pub agent: String,
    pub workspace_id: String,
    pub request_id: String,
    pub pid: u32,
    pub started_at: String,
    pub source: String,
    pub owner: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForceSendAuditEntry {
    pub timestamp: String,
    pub role: Option<String>,
    pub session_id: String,
    pub workspace_id: String,
    pub payload_hash: String,
    pub byte_size: usize,
    pub action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupSummary {
    pub removed_roles: usize,
    pub removed_pending_spawns: usize,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            roles: BTreeMap::new(),
            pending_spawns: BTreeMap::new(),
        }
    }
}

impl RegistryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            registry_path: root.join("registry.json"),
            lock_path: root.join("registry.lock"),
            audit_path: root.join("audit.log"),
            messages_dir: root.join("messages"),
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn registry_path(&self) -> &Path {
        &self.registry_path
    }

    pub fn messages_dir(&self) -> &Path {
        &self.messages_dir
    }

    pub fn acquire_lock(&self) -> Result<RegistryLock> {
        ensure_private_dir(&self.root)?;
        acquire_lock_file(
            self.lock_path.clone(),
            "registry lock",
            "remove stale .ssctl/registry.lock if no ssctl process is running".to_owned(),
        )
    }

    pub fn load(&self) -> Result<Registry> {
        match fs::read_to_string(&self.registry_path) {
            Ok(contents) => {
                let registry: Registry =
                    serde_json::from_str(&contents).context("failed to parse registry")?;
                if registry.schema_version != REGISTRY_SCHEMA_VERSION {
                    bail!(
                        "unsupported registry schema version: {}",
                        registry.schema_version
                    );
                }
                Ok(registry)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(error) => Err(error).context("failed to read registry"),
        }
    }
}

fn acquire_lock_file(path: PathBuf, label: &str, stale_hint: String) -> Result<RegistryLock> {
    let start = Instant::now();

    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(file, "pid={}", process::id())?;
                file.sync_all()?;
                return Ok(RegistryLock { path });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if start.elapsed() >= LOCK_WAIT_TIMEOUT {
                    bail!("timed out waiting for {label}; {stale_hint}");
                }
                thread::sleep(LOCK_RETRY_INTERVAL);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to create {label}"));
            }
        }
    }
}

impl RegistryStore {
    pub fn save(&self, registry: &Registry) -> Result<()> {
        if registry.schema_version != REGISTRY_SCHEMA_VERSION {
            bail!(
                "unsupported registry schema version: {}",
                registry.schema_version
            );
        }
        ensure_private_dir(&self.root)?;

        let tmp_path = self
            .root
            .join(format!("registry.json.tmp.{}", process::id()));
        let mut tmp = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .context("failed to create temporary registry file")?;

        serde_json::to_writer_pretty(&mut tmp, registry)?;
        tmp.write_all(b"\n")?;
        tmp.sync_all()?;
        drop(tmp);

        fs::rename(&tmp_path, &self.registry_path).context("failed to replace registry")?;
        sync_dir(&self.root)?;
        Ok(())
    }

    pub fn append_force_send_audit(&self, entry: &ForceSendAuditEntry) -> Result<()> {
        ensure_private_dir(&self.root)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.audit_path)
            .context("failed to open audit log")?;
        serde_json::to_writer(&mut file, entry)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }

    pub fn write_message_snapshot(&self, body: &str) -> Result<PathBuf> {
        ensure_private_dir(&self.root)?;
        ensure_private_dir(&self.messages_dir)?;
        let filename = format!("{}.md", timestamp_for_filename());
        let path = self.messages_dir.join(filename);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .context("failed to create message snapshot")?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        Ok(path)
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl RegistryEntry {
    pub fn new(agent: String, workspace_id: String, session_id: String, source: String) -> Self {
        let now = timestamp_now();
        Self {
            agent,
            workspace_id,
            session_id,
            created_at: now.clone(),
            last_verified_at: now,
            terminal_host_protocol_version: TERMINAL_HOST_PROTOCOL_VERSION,
            source,
            owner: "ssctl".to_owned(),
        }
    }

    pub fn touch_verified(&mut self) {
        self.last_verified_at = timestamp_now();
        self.terminal_host_protocol_version = TERMINAL_HOST_PROTOCOL_VERSION;
    }
}

impl PendingSpawnEntry {
    pub fn new(agent: String, workspace_id: String, request_id: String, source: String) -> Self {
        Self {
            agent,
            workspace_id,
            request_id,
            pid: process::id(),
            started_at: timestamp_now(),
            source,
            owner: "ssctl".to_owned(),
        }
    }

    pub fn is_stale(&self) -> bool {
        if pending_spawn_age(&self.started_at).is_some_and(|age| age >= PENDING_SPAWN_MAX_AGE) {
            return true;
        }
        !process_is_running(self.pid)
    }
}

impl ForceSendAuditEntry {
    pub fn new(
        role: Option<String>,
        session_id: String,
        workspace_id: String,
        payload_hash: String,
        byte_size: usize,
    ) -> Self {
        Self {
            timestamp: timestamp_now(),
            role,
            session_id,
            workspace_id,
            payload_hash,
            byte_size,
            action: "force-unregistered-send".to_owned(),
        }
    }

    #[cfg(test)]
    pub fn from_payload(
        role: Option<String>,
        session_id: String,
        workspace_id: String,
        payload: &str,
    ) -> Self {
        Self::new(
            role,
            session_id,
            workspace_id,
            sha256_hex(payload.as_bytes()),
            payload.len(),
        )
    }
}

pub fn cleanup_stale_sessions(
    registry: &mut Registry,
    sessions: &[TerminalSessionInfo],
) -> CleanupSummary {
    let before = registry.roles.len();
    registry.roles.retain(|_, entry| {
        sessions.iter().any(|session| {
            session.is_alive
                && session.session_id == entry.session_id
                && session.workspace_id.as_deref() == Some(entry.workspace_id.as_str())
        })
    });
    CleanupSummary {
        removed_roles: before.saturating_sub(registry.roles.len()),
        removed_pending_spawns: 0,
    }
}

pub fn cleanup_stale_pending_spawns(registry: &mut Registry) -> CleanupSummary {
    let before = registry.pending_spawns.len();
    registry
        .pending_spawns
        .retain(|_, pending| !pending.is_stale());
    CleanupSummary {
        removed_roles: 0,
        removed_pending_spawns: before.saturating_sub(registry.pending_spawns.len()),
    }
}

pub fn role_for_session<'a>(
    registry: &'a Registry,
    session_id: &str,
) -> Option<(&'a str, &'a RegistryEntry)> {
    registry
        .roles
        .iter()
        .find(|(_, entry)| entry.session_id == session_id)
        .map(|(role, entry)| (role.as_str(), entry))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn timestamp_for_filename() -> String {
    Utc::now().format("%Y%m%d-%H%M%S%.3fZ").to_string()
}

pub fn spawn_request_id(role: &str, agent: &str, workspace_id: &str) -> String {
    let material = format!(
        "{}\0{}\0{}\0{}\0{}",
        role,
        agent,
        workspace_id,
        process::id(),
        timestamp_now()
    );
    sha256_hex(material.as_bytes())
}

fn pending_spawn_age(started_at: &str) -> Option<Duration> {
    let started_at = DateTime::parse_from_rfc3339(started_at)
        .ok()?
        .with_timezone(&Utc);
    Utc::now().signed_duration_since(started_at).to_std().ok()
}

fn process_is_running(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        ForceSendAuditEntry, PendingSpawnEntry, Registry, RegistryEntry, RegistryStore,
        cleanup_stale_pending_spawns, cleanup_stale_sessions,
    };
    use crate::terminal_host::TerminalSessionInfo;

    #[test]
    fn cleanup_removes_missing_or_dead_sessions() {
        let mut registry = Registry::default();
        registry.roles.insert(
            "alive".to_owned(),
            RegistryEntry::new(
                "codex".to_owned(),
                "workspace-1".to_owned(),
                "session-1".to_owned(),
                "superset agents create".to_owned(),
            ),
        );
        registry.roles.insert(
            "dead".to_owned(),
            RegistryEntry::new(
                "codex".to_owned(),
                "workspace-1".to_owned(),
                "session-2".to_owned(),
                "superset agents create".to_owned(),
            ),
        );

        let summary = cleanup_stale_sessions(
            &mut registry,
            &[TerminalSessionInfo {
                session_id: "session-1".to_owned(),
                workspace_id: Some("workspace-1".to_owned()),
                pane_id: None,
                is_alive: true,
                attached_clients: 0,
                pid: None,
                created_at: None,
                last_attached_at: None,
                shell: None,
            }],
        );

        assert_eq!(summary.removed_roles, 1);
        assert!(registry.roles.contains_key("alive"));
        assert!(!registry.roles.contains_key("dead"));
    }

    #[test]
    fn store_round_trips_registry_and_audit_without_payload() {
        let root = std::env::temp_dir().join(format!("ssctl-registry-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
        let _lock = store.acquire_lock().unwrap();

        let mut registry = Registry::default();
        registry.roles.insert(
            "worker".to_owned(),
            RegistryEntry::new(
                "codex".to_owned(),
                "workspace-1".to_owned(),
                "session-1".to_owned(),
                "superset agents create".to_owned(),
            ),
        );
        store.save(&registry).unwrap();
        assert_eq!(store.load().unwrap().roles.len(), 1);

        let audit = ForceSendAuditEntry::from_payload(
            None,
            "session-1".to_owned(),
            "workspace-1".to_owned(),
            "secret payload",
        );
        store.append_force_send_audit(&audit).unwrap();
        let audit_contents = fs::read_to_string(root.join("audit.log")).unwrap();
        assert!(audit_contents.contains("payloadHash"));
        assert!(!audit_contents.contains("secret payload"));

        drop(_lock);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn store_round_trips_pending_spawns() {
        let root =
            std::env::temp_dir().join(format!("ssctl-pending-spawn-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);

        let mut registry = Registry::default();
        registry.pending_spawns.insert(
            "worker".to_owned(),
            PendingSpawnEntry::new(
                "codex".to_owned(),
                "workspace-1".to_owned(),
                "request-1".to_owned(),
                "superset agents create".to_owned(),
            ),
        );
        store.save(&registry).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.pending_spawns["worker"].request_id, "request-1");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_removes_stale_pending_spawns() {
        let mut registry = Registry::default();
        let mut pending = PendingSpawnEntry::new(
            "codex".to_owned(),
            "workspace-1".to_owned(),
            "request-1".to_owned(),
            "superset agents create".to_owned(),
        );
        pending.pid = u32::MAX;
        registry.pending_spawns.insert("worker".to_owned(), pending);

        let summary = cleanup_stale_pending_spawns(&mut registry);

        assert_eq!(summary.removed_pending_spawns, 1);
        assert!(registry.pending_spawns.is_empty());
    }
}
