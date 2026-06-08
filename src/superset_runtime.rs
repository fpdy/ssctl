use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::terminal_host::TerminalSessionInfo;

pub const PTY_DAEMON_PROTOCOL_VERSION: u64 = 2;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const HEADER_BYTES: usize = 4;
const INNER_JSON_LEN_BYTES: usize = 4;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SupersetHostRuntime {
    pub organization_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PtyDaemonManifest {
    pub pid: i64,
    pub socket_path: PathBuf,
    pub protocol_versions: Vec<u64>,
    pub started_at: i64,
    pub organization_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PtyDaemonHello {
    pub protocol: u64,
    pub daemon_version: String,
    pub daemon_pid: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PtySessionInfo {
    id: String,
    pid: i64,
    cols: u64,
    rows: u64,
    alive: bool,
}

#[derive(Debug, Clone)]
struct DbSessionRecord {
    workspace_id: Option<String>,
    status: String,
    created_at: Option<i64>,
    last_attached_at: Option<i64>,
}

pub struct SupersetRuntimeClient {
    pty: PtyDaemonClient,
    host_db: PathBuf,
}

pub struct PtyDaemonClient {
    stream: UnixStream,
    hello: PtyDaemonHello,
}

pub fn runtime_from_status_stdout(stdout: &str) -> Result<SupersetHostRuntime> {
    let value: Value =
        serde_json::from_str(stdout).context("superset status did not return JSON")?;
    let organization_id = value
        .get("organizationId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("superset status omitted organizationId")?
        .to_owned();
    Ok(SupersetHostRuntime { organization_id })
}

pub fn pty_daemon_manifest_path(superset_home: &Path, organization_id: &str) -> PathBuf {
    superset_home
        .join("host")
        .join(organization_id)
        .join("pty-daemon-manifest.json")
}

pub fn host_db_path(superset_home: &Path, organization_id: &str) -> PathBuf {
    superset_home
        .join("host")
        .join(organization_id)
        .join("host.db")
}

pub fn load_pty_daemon_manifest(
    superset_home: &Path,
    organization_id: &str,
) -> Result<PtyDaemonManifest> {
    let path = pty_daemon_manifest_path(superset_home, organization_id);
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: PtyDaemonManifest =
        serde_json::from_str(&contents).context("failed to parse pty-daemon manifest")?;
    if manifest.organization_id != organization_id {
        bail!(
            "pty-daemon manifest organization mismatch: expected {}, got {}",
            organization_id,
            manifest.organization_id
        );
    }
    if !manifest
        .protocol_versions
        .contains(&PTY_DAEMON_PROTOCOL_VERSION)
    {
        bail!(
            "pty-daemon protocol mismatch: expected support for {}",
            PTY_DAEMON_PROTOCOL_VERSION
        );
    }
    Ok(manifest)
}

impl SupersetRuntimeClient {
    pub fn connect(superset_home: &Path, runtime: &SupersetHostRuntime) -> Result<Self> {
        let manifest = load_pty_daemon_manifest(superset_home, &runtime.organization_id)?;
        let host_db = host_db_path(superset_home, &runtime.organization_id);
        let pty = PtyDaemonClient::connect(&manifest)?;
        Ok(Self { pty, host_db })
    }

    pub fn hello(&self) -> &PtyDaemonHello {
        self.pty.hello()
    }

    pub fn list_sessions(&mut self) -> Result<Vec<TerminalSessionInfo>> {
        let db_sessions = read_host_db_sessions(&self.host_db)?;
        let pty_sessions = self.pty.list_sessions()?;
        Ok(join_sessions(pty_sessions, db_sessions))
    }

    pub fn write(&mut self, session_id: &str, data: &str) -> Result<()> {
        self.pty.write(session_id, data.as_bytes())
    }

    pub fn close(&mut self, session_id: &str, signal: &str) -> Result<()> {
        self.pty.close(session_id, signal)
    }
}

impl PtyDaemonClient {
    pub fn connect(manifest: &PtyDaemonManifest) -> Result<Self> {
        if !manifest.socket_path.exists() {
            bail!(
                "pty-daemon socket not found at {}",
                manifest.socket_path.display()
            );
        }

        let mut stream = UnixStream::connect(&manifest.socket_path)
            .with_context(|| format!("failed to connect {}", manifest.socket_path.display()))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;

        write_frame(
            &mut stream,
            &json!({
                "type": "hello",
                "protocols": [PTY_DAEMON_PROTOCOL_VERSION],
                "clientVersion": concat!("ssctl/", env!("CARGO_PKG_VERSION")),
            }),
            None,
        )?;

        let message = read_frame(&mut stream)?.message;
        ensure_message_type(&message, "hello-ack")?;
        let hello: PtyDaemonHello =
            serde_json::from_value(message).context("invalid pty-daemon hello-ack")?;
        if hello.protocol != PTY_DAEMON_PROTOCOL_VERSION {
            bail!(
                "pty-daemon protocol mismatch: expected {}, got {}",
                PTY_DAEMON_PROTOCOL_VERSION,
                hello.protocol
            );
        }

        Ok(Self { stream, hello })
    }

    pub fn hello(&self) -> &PtyDaemonHello {
        &self.hello
    }

    fn list_sessions(&mut self) -> Result<Vec<PtySessionInfo>> {
        write_frame(&mut self.stream, &json!({ "type": "list" }), None)?;
        loop {
            let frame = read_frame(&mut self.stream)?;
            let message_type = message_type(&frame.message).unwrap_or_default();
            match message_type {
                "list-reply" => {
                    let sessions = frame
                        .message
                        .get("sessions")
                        .cloned()
                        .context("pty-daemon list-reply omitted sessions")?;
                    return serde_json::from_value(sessions)
                        .context("invalid pty-daemon list-reply sessions");
                }
                "error" => bail_pty_error(&frame.message)?,
                _ => {}
            }
        }
    }

    fn write(&mut self, session_id: &str, payload: &[u8]) -> Result<()> {
        write_frame(
            &mut self.stream,
            &json!({
                "type": "input",
                "id": session_id,
            }),
            Some(payload),
        )
    }

    fn close(&mut self, session_id: &str, signal: &str) -> Result<()> {
        write_frame(
            &mut self.stream,
            &json!({
                "type": "close",
                "id": session_id,
                "signal": signal,
            }),
            None,
        )?;
        loop {
            let frame = read_frame(&mut self.stream)?;
            let message_type = message_type(&frame.message).unwrap_or_default();
            match message_type {
                "closed" | "close-reply" => {
                    ensure_reply_session_id(&frame.message, session_id)?;
                    return Ok(());
                }
                "error" => bail_pty_error(&frame.message)?,
                _ => {}
            }
        }
    }
}

struct DecodedFrame {
    message: Value,
}

fn write_frame(stream: &mut UnixStream, message: &Value, payload: Option<&[u8]>) -> Result<()> {
    let json_bytes = serde_json::to_vec(message).context("failed to encode pty-daemon frame")?;
    let payload = payload.unwrap_or_default();
    let total_len = INNER_JSON_LEN_BYTES
        .checked_add(json_bytes.len())
        .and_then(|len| len.checked_add(payload.len()))
        .context("pty-daemon frame length overflow")?;
    if total_len > MAX_FRAME_BYTES {
        bail!("pty-daemon frame too large: {total_len} bytes");
    }

    let mut frame = Vec::with_capacity(HEADER_BYTES + total_len);
    frame.extend_from_slice(&(total_len as u32).to_be_bytes());
    frame.extend_from_slice(&(json_bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&json_bytes);
    frame.extend_from_slice(payload);
    stream
        .write_all(&frame)
        .context("failed to write pty-daemon frame")?;
    stream.flush().context("failed to flush pty-daemon frame")?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<DecodedFrame> {
    let mut total_len_bytes = [0_u8; HEADER_BYTES];
    stream
        .read_exact(&mut total_len_bytes)
        .context("failed to read pty-daemon frame header")?;
    let total_len = u32::from_be_bytes(total_len_bytes) as usize;
    if total_len > MAX_FRAME_BYTES {
        bail!("pty-daemon frame too large: {total_len} bytes");
    }
    if total_len < INNER_JSON_LEN_BYTES {
        bail!("pty-daemon frame too small: {total_len} bytes");
    }

    let mut body = vec![0_u8; total_len];
    stream
        .read_exact(&mut body)
        .context("failed to read pty-daemon frame body")?;
    let json_len = u32::from_be_bytes(body[0..INNER_JSON_LEN_BYTES].try_into().unwrap()) as usize;
    if json_len > total_len - INNER_JSON_LEN_BYTES {
        bail!(
            "pty-daemon frame jsonLen {} exceeds frame body {}",
            json_len,
            total_len - INNER_JSON_LEN_BYTES
        );
    }
    let json_start = INNER_JSON_LEN_BYTES;
    let json_end = json_start + json_len;
    let message: Value =
        serde_json::from_slice(&body[json_start..json_end]).context("invalid pty-daemon JSON")?;
    Ok(DecodedFrame { message })
}

fn read_host_db_sessions(host_db: &Path) -> Result<BTreeMap<String, DbSessionRecord>> {
    if !host_db.exists() {
        bail!("Superset host DB not found at {}", host_db.display());
    }

    let conn = Connection::open_with_flags(
        host_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open {}", host_db.display()))?;
    let mut statement = conn
        .prepare(
            "select id, origin_workspace_id, status, created_at, last_attached_at \
             from terminal_sessions",
        )
        .context("failed to query terminal_sessions")?;

    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                DbSessionRecord {
                    workspace_id: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    last_attached_at: row.get(4)?,
                },
            ))
        })
        .context("failed to read terminal_sessions")?;

    let mut records = BTreeMap::new();
    for row in rows {
        let (id, record) = row.context("failed to read terminal_sessions row")?;
        records.insert(id, record);
    }
    Ok(records)
}

fn join_sessions(
    pty_sessions: Vec<PtySessionInfo>,
    db_sessions: BTreeMap<String, DbSessionRecord>,
) -> Vec<TerminalSessionInfo> {
    pty_sessions
        .into_iter()
        .map(|session| {
            let db_record = db_sessions.get(&session.id);
            let db_active = db_record
                .map(|record| record.status == "active")
                .unwrap_or(true);
            TerminalSessionInfo {
                session_id: session.id,
                workspace_id: db_record.and_then(|record| record.workspace_id.clone()),
                pane_id: None,
                is_alive: session.alive && db_active,
                attached_clients: 0,
                pid: Some(session.pid),
                created_at: db_record.and_then(|record| millis_to_rfc3339(record.created_at)),
                last_attached_at: db_record
                    .and_then(|record| millis_to_rfc3339(record.last_attached_at)),
                shell: None,
            }
        })
        .collect()
}

fn millis_to_rfc3339(millis: Option<i64>) -> Option<String> {
    let millis = millis?;
    let seconds = millis.div_euclid(1000);
    let nanos = millis.rem_euclid(1000) as u32 * 1_000_000;
    DateTime::<Utc>::from_timestamp(seconds, nanos)
        .map(|datetime| datetime.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn ensure_message_type(message: &Value, expected: &str) -> Result<()> {
    match message_type(message) {
        Some(actual) if actual == expected => Ok(()),
        Some("error") => bail_pty_error(message),
        Some(actual) => bail!("unexpected pty-daemon message: expected {expected}, got {actual}"),
        None => bail!("pty-daemon message omitted type"),
    }
}

fn ensure_reply_session_id(message: &Value, expected: &str) -> Result<()> {
    if let Some(actual) = message.get("id").and_then(Value::as_str)
        && actual != expected
    {
        bail!("pty-daemon reply session mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn message_type(message: &Value) -> Option<&str> {
    message.get("type").and_then(Value::as_str)
}

fn bail_pty_error(message: &Value) -> Result<()> {
    let code = message
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let detail = message
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("pty-daemon returned an error");
    bail!("pty-daemon {code}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::{PTY_DAEMON_PROTOCOL_VERSION, millis_to_rfc3339, runtime_from_status_stdout};

    #[test]
    fn parses_runtime_from_superset_status() {
        let runtime =
            runtime_from_status_stdout(r#"{"running":true,"organizationId":"org-123"}"#).unwrap();

        assert_eq!(runtime.organization_id, "org-123");
    }

    #[test]
    fn rejects_status_without_org_id() {
        let error = runtime_from_status_stdout(r#"{"running":true}"#).unwrap_err();

        assert!(error.to_string().contains("organizationId"));
    }

    #[test]
    fn converts_millis_to_timestamp() {
        assert_eq!(
            millis_to_rfc3339(Some(1_700_000_000_123)).unwrap(),
            "2023-11-14T22:13:20Z"
        );
    }

    #[test]
    fn protocol_version_is_current_superset_pty_daemon_version() {
        assert_eq!(PTY_DAEMON_PROTOCOL_VERSION, 2);
    }
}
