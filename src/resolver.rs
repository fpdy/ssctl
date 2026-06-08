use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::terminal_host::TerminalSessionInfo;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnResolution {
    pub session: TerminalSessionInfo,
    pub strategy: SpawnResolutionStrategy,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SpawnResolutionStrategy {
    VerifiedCliSessionId,
    SingleNewWorkspaceSession,
}

pub fn resolve_spawned_session(
    before: &[TerminalSessionInfo],
    after: &[TerminalSessionInfo],
    workspace_id: &str,
    cli_stdout: &str,
) -> Result<SpawnResolution> {
    let candidates = extract_session_id_candidates(cli_stdout);
    if !candidates.is_empty() {
        let direct_matches = verified_cli_session_matches(after, workspace_id, &candidates);
        if direct_matches.len() == 1 {
            return Ok(SpawnResolution {
                session: direct_matches[0].clone(),
                strategy: SpawnResolutionStrategy::VerifiedCliSessionId,
            });
        }
        if direct_matches.len() > 1 {
            bail!("spawn output matched multiple pty-daemon sessions; registry was not updated");
        }
        bail!(
            "spawn output session id was not found in pty-daemon workspace {}; registry was not updated",
            workspace_id
        );
    }

    let before_ids = before
        .iter()
        .map(|session| session.session_id.as_str())
        .collect::<BTreeSet<_>>();
    let new_workspace_sessions = after
        .iter()
        .filter(|session| session.is_alive)
        .filter(|session| session.workspace_id.as_deref() == Some(workspace_id))
        .filter(|session| !before_ids.contains(session.session_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if new_workspace_sessions.len() == 1 {
        return Ok(SpawnResolution {
            session: new_workspace_sessions[0].clone(),
            strategy: SpawnResolutionStrategy::SingleNewWorkspaceSession,
        });
    }
    if new_workspace_sessions.len() > 1 {
        bail!("spawn created multiple candidate pty-daemon sessions; registry was not updated");
    }

    bail!("could not uniquely correlate created Superset session; registry was not updated")
}

pub fn extract_session_id_candidates(stdout: &str) -> BTreeSet<String> {
    let Ok(value) = serde_json::from_str::<Value>(stdout) else {
        return BTreeSet::new();
    };

    let mut candidates = BTreeSet::new();
    collect_candidate_values(&value, None, false, &mut candidates);
    candidates
}

fn verified_cli_session_matches(
    sessions: &[TerminalSessionInfo],
    workspace_id: &str,
    candidates: &BTreeSet<String>,
) -> Vec<TerminalSessionInfo> {
    if candidates.is_empty() {
        return Vec::new();
    }

    sessions
        .iter()
        .filter(|session| session.is_alive)
        .filter(|session| session.workspace_id.as_deref() == Some(workspace_id))
        .filter(|session| candidates.contains(&session.session_id))
        .cloned()
        .collect()
}

fn collect_candidate_values(
    value: &Value,
    key: Option<&str>,
    session_context: bool,
    candidates: &mut BTreeSet<String>,
) {
    match value {
        Value::Object(map) => {
            let child_session_context = session_context || key.is_some_and(is_session_context_key);
            for (child_key, child_value) in map {
                collect_candidate_values(
                    child_value,
                    Some(child_key),
                    child_session_context,
                    candidates,
                );
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_candidate_values(child, key, session_context, candidates);
            }
        }
        Value::String(text) => {
            if key.is_some_and(|key| {
                is_session_id_key(key) || is_session_context_id(key, session_context)
            }) && looks_like_id(text)
            {
                candidates.insert(text.clone());
            }
        }
        _ => {}
    }
}

fn is_session_id_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("session") && lower.ends_with("id")
}

fn is_session_context_key(key: &str) -> bool {
    key.to_ascii_lowercase().contains("session")
}

fn is_session_context_id(key: &str, session_context: bool) -> bool {
    session_context && key.eq_ignore_ascii_case("id")
}

fn looks_like_id(value: &str) -> bool {
    value.len() >= 8
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{SpawnResolutionStrategy, extract_session_id_candidates, resolve_spawned_session};
    use crate::terminal_host::TerminalSessionInfo;

    #[test]
    fn extracts_only_session_id_like_fields() {
        let candidates = extract_session_id_candidates(
            r#"{"sessionId":"s-12345678","workspaceId":"w-1","id":"ambiguous"}"#,
        );
        assert!(candidates.contains("s-12345678"));
        assert!(!candidates.contains("ambiguous"));
    }

    #[test]
    fn extracts_nested_session_id_fields() {
        let candidates =
            extract_session_id_candidates(r#"{"terminalSession":{"id":"s-12345678"}}"#);
        assert!(candidates.contains("s-12345678"));
    }

    #[test]
    fn resolves_verified_cli_session_id() {
        let session = session("s-12345678", "w-1");
        let resolution = resolve_spawned_session(
            &[],
            std::slice::from_ref(&session),
            "w-1",
            r#"{"terminalSessionId":"s-12345678"}"#,
        )
        .unwrap();

        assert_eq!(resolution.session.session_id, "s-12345678");
        assert_eq!(
            resolution.strategy,
            SpawnResolutionStrategy::VerifiedCliSessionId
        );
    }

    #[test]
    fn resolves_single_new_workspace_session() {
        let before = session("old", "w-1");
        let after = session("new", "w-1");
        let resolution = resolve_spawned_session(
            std::slice::from_ref(&before),
            &[before.clone(), after],
            "w-1",
            "{}",
        )
        .unwrap();

        assert_eq!(resolution.session.session_id, "new");
        assert_eq!(
            resolution.strategy,
            SpawnResolutionStrategy::SingleNewWorkspaceSession
        );
    }

    #[test]
    fn does_not_resolve_existing_recent_workspace_session() {
        let existing = session("existing", "w-1");
        let result = resolve_spawned_session(
            std::slice::from_ref(&existing),
            std::slice::from_ref(&existing),
            "w-1",
            "{}",
        );

        assert!(result.is_err());
    }

    #[test]
    fn does_not_fallback_when_cli_session_id_is_unverified() {
        let before = session("old", "w-1");
        let after = session("new", "w-1");
        let result = resolve_spawned_session(
            std::slice::from_ref(&before),
            &[before.clone(), after],
            "w-1",
            r#"{"sessionId":"missing-session"}"#,
        );

        assert_eq!(
            result.unwrap_err().to_string(),
            "spawn output session id was not found in pty-daemon workspace w-1; registry was not updated"
        );
    }

    fn session(session_id: &str, workspace_id: &str) -> TerminalSessionInfo {
        TerminalSessionInfo {
            session_id: session_id.to_owned(),
            workspace_id: Some(workspace_id.to_owned()),
            pane_id: None,
            is_alive: true,
            attached_clients: 0,
            pid: None,
            created_at: Some(Utc::now().to_rfc3339()),
            last_attached_at: None,
            shell: None,
        }
    }
}
