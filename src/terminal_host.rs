use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::TERMINAL_HOST_PROTOCOL_VERSION;

const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct TerminalHostPaths {
    pub socket: PathBuf,
    pub token: PathBuf,
}

impl TerminalHostPaths {
    pub fn new(socket: impl Into<PathBuf>, token: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            token: token.into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HelloResponse {
    pub protocol_version: u64,
    pub daemon_version: String,
    pub daemon_pid: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSessionInfo {
    pub session_id: String,
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    pub is_alive: bool,
    pub attached_clients: usize,
    pub pid: Option<i64>,
    pub created_at: Option<String>,
    pub last_attached_at: Option<String>,
    pub shell: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListSessionsResponse {
    sessions: Vec<TerminalSessionInfo>,
}

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    id: String,
    ok: bool,
    payload: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: String,
    message: String,
}

pub struct TerminalHostClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    request_counter: u64,
    hello: HelloResponse,
}

impl TerminalHostClient {
    pub fn connect(paths: &TerminalHostPaths) -> Result<Self> {
        if !paths.socket.exists() {
            bail!("terminal-host socket not found");
        }
        if !paths.token.exists() {
            bail!("terminal-host token not found");
        }

        let token = read_token(&paths.token)?;
        let writer =
            UnixStream::connect(&paths.socket).context("failed to connect terminal-host socket")?;
        writer.set_read_timeout(Some(IO_TIMEOUT))?;
        writer.set_write_timeout(Some(IO_TIMEOUT))?;

        let reader_stream = writer.try_clone()?;
        reader_stream.set_read_timeout(Some(IO_TIMEOUT))?;

        let mut client = Self {
            reader: BufReader::new(reader_stream),
            writer,
            request_counter: 0,
            hello: HelloResponse {
                protocol_version: 0,
                daemon_version: String::new(),
                daemon_pid: None,
            },
        };

        let hello: HelloResponse = client.request(
            "hello",
            json!({
                "protocolVersion": TERMINAL_HOST_PROTOCOL_VERSION,
                "token": token,
                "clientId": default_client_id(),
                "role": "control",
            }),
        )?;

        if hello.protocol_version != TERMINAL_HOST_PROTOCOL_VERSION {
            bail!(
                "terminal-host protocol mismatch: expected {}, got {}",
                TERMINAL_HOST_PROTOCOL_VERSION,
                hello.protocol_version
            );
        }

        client.hello = hello;
        Ok(client)
    }

    pub fn hello(&self) -> &HelloResponse {
        &self.hello
    }

    pub fn list_sessions(&mut self) -> Result<Vec<TerminalSessionInfo>> {
        let response: ListSessionsResponse = self.request("listSessions", json!({}))?;
        Ok(response.sessions)
    }

    pub fn write(&mut self, session_id: &str, data: &str) -> Result<()> {
        let _: Value = self.request(
            "write",
            json!({
                "sessionId": session_id,
                "data": data,
            }),
        )?;
        Ok(())
    }

    /// Future no-ack write path for large pastes.
    ///
    /// The current send path pointerizes oversized payloads and uses `write`
    /// for small messages. Revalidate the private terminal-host protocol shape
    /// before routing user-visible traffic through this method.
    pub fn write_no_ack(&mut self, session_id: &str, data: &str) -> Result<()> {
        let id = self.next_notify_id("write");
        self.write_request(
            &id,
            "write",
            json!({
                "sessionId": session_id,
                "data": data,
            }),
        )
    }

    fn request<T>(&mut self, request_type: &str, payload: Value) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let id = self.next_request_id(request_type);
        self.write_request(&id, request_type, payload)?;

        loop {
            let mut line = String::new();
            let bytes = self.reader.read_line(&mut line)?;
            if bytes == 0 {
                bail!("terminal-host closed the connection");
            }
            if line.trim().is_empty() {
                continue;
            }

            let response: RpcResponse<T> =
                serde_json::from_str(&line).context("invalid terminal-host response")?;
            if response.id != id {
                continue;
            }
            if response.ok {
                return response
                    .payload
                    .context("terminal-host response omitted payload");
            }

            let error = response
                .error
                .context("terminal-host response omitted error")?;
            bail!("terminal-host {}: {}", error.code, error.message);
        }
    }

    fn write_request(&mut self, id: &str, request_type: &str, payload: Value) -> Result<()> {
        let request = json!({
            "id": id,
            "type": request_type,
            "payload": payload,
        });
        serde_json::to_writer(&mut self.writer, &request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    fn next_request_id(&mut self, request_type: &str) -> String {
        self.request_counter += 1;
        format!("ssctl_{request_type}_{}", self.request_counter)
    }

    fn next_notify_id(&mut self, request_type: &str) -> String {
        self.request_counter += 1;
        format!("notify_ssctl_{request_type}_{}", self.request_counter)
    }
}

fn read_token(path: &Path) -> Result<String> {
    let token = fs::read_to_string(path)
        .context("failed to read terminal-host token")?
        .trim()
        .to_owned();
    if token.is_empty() {
        bail!("terminal-host token file is empty");
    }
    Ok(token)
}

fn default_client_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("ssctl-{}-{nanos}", process::id())
}

#[cfg(test)]
mod tests {
    use super::{HelloResponse, RpcResponse, TerminalSessionInfo};

    #[test]
    fn decodes_hello_response_shape() {
        let response: RpcResponse<HelloResponse> = serde_json::from_str(
            r#"{"id":"ssctl_hello_1","ok":true,"payload":{"protocolVersion":2,"daemonVersion":"1.0.0","daemonPid":123}}"#,
        )
        .unwrap();

        assert!(response.ok);
        assert_eq!(response.payload.unwrap().protocol_version, 2);
    }

    #[test]
    fn decodes_session_shape() {
        let session: TerminalSessionInfo = serde_json::from_str(
            r#"{"sessionId":"s1","workspaceId":"w1","paneId":"p1","isAlive":true,"attachedClients":1,"pid":42,"createdAt":"2026-06-08T00:00:00.000Z","lastAttachedAt":null,"shell":"/bin/zsh"}"#,
        )
        .unwrap();

        assert_eq!(session.session_id, "s1");
        assert_eq!(session.workspace_id.as_deref(), Some("w1"));
        assert!(session.is_alive);
    }
}
