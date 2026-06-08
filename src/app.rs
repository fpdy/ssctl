use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::cli::{
    AgentsCommand, AgentsListArgs, Cli, CloseArgs, Command, HandoffArgs, OutputArgs, ReportArgs,
    SendArgs, SpawnArgs,
};
use crate::config::RuntimeConfig;
use crate::registry::{
    ForceSendAuditEntry, PendingSpawnEntry, Registry, RegistryEntry, RegistryStore,
    cleanup_stale_pending_spawns, cleanup_stale_sessions, role_for_session, sha256_hex,
    spawn_request_id, timestamp_for_filename,
};
use crate::resolver::{SpawnResolution, resolve_spawned_session};
use crate::superset_cli::{CommandOutput, SupersetCliAdapter};
use crate::superset_runtime::{
    PtyDaemonHello, SupersetHostRuntime, SupersetRuntimeClient, host_db_path,
    load_pty_daemon_manifest, pty_daemon_manifest_path, runtime_from_status_stdout,
};
use crate::terminal_host::{
    HelloResponse, TerminalHostClient, TerminalHostPaths, TerminalSessionInfo,
};
use crate::transport::{TransportError, structured_block_paste};

const SPAWN_CORRELATION_ATTEMPTS: usize = 20;
const SPAWN_CORRELATION_INTERVAL: Duration = Duration::from_millis(500);

pub fn run(cli: Cli) -> Result<()> {
    let config = RuntimeConfig::from_cli(&cli)?;

    match cli.command {
        Command::Status(args) => status(&config, args),
        Command::Agents { command } => match command {
            AgentsCommand::List(args) => agents_list(&config, args),
        },
        Command::Sessions(args) => sessions(&config, args),
        Command::Spawn(args) => spawn(&config, args),
        Command::Send(args) => send(&config, args),
        Command::Close(args) => close(&config, args),
        Command::Handoff(args) => handoff(&config, args),
        Command::Report(args) => report(&config, args),
    }
}

fn status(config: &RuntimeConfig, args: OutputArgs) -> Result<()> {
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    let cli_report = inspect_superset_cli(&adapter);
    let terminal_host_report = inspect_terminal_host(config);
    let pty_daemon_report = inspect_pty_daemon(config, cli_report.status.as_ref());
    let report = StatusReport {
        superset_cli: cli_report,
        terminal_host: terminal_host_report,
        pty_daemon: pty_daemon_report,
    };

    if args.json {
        print_json(&report)?;
    } else {
        print_status_human(&report)?;
    }

    Ok(())
}

fn agents_list(config: &RuntimeConfig, args: AgentsListArgs) -> Result<()> {
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    let output = adapter.agents_list(args.json, args.host.as_deref())?;
    passthrough_output(output)
}

fn sessions(config: &RuntimeConfig, args: OutputArgs) -> Result<()> {
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    let runtime = ensure_superset_host_running(&adapter)?;
    let mut client = connect_superset_runtime(config, &runtime)?;
    let sessions = client.list_sessions()?;

    if args.json {
        print_json(&sessions)?;
    } else {
        print_sessions_human(&sessions);
    }

    Ok(())
}

fn spawn(config: &RuntimeConfig, args: SpawnArgs) -> Result<()> {
    let store = RegistryStore::new(config.registry_dir.clone());
    let prompt = read_file_or_text(&args.prompt)?;
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    let runtime = ensure_superset_host_running(&adapter)?;

    let mut client = connect_superset_runtime(config, &runtime)?;
    let before = client.list_sessions()?;
    let reservation =
        reserve_pending_spawn(&store, &args.role, &args.agent, &args.workspace, &before)?;

    let spawn_result = (|| -> Result<SpawnOutput> {
        let output = adapter.agents_create(&args.workspace, &args.agent, &prompt)?;
        ensure_command_success(&output, "superset agents create")?;
        ensure_terminal_agent_create_stdout(&output.stdout)?;

        let (after, resolution) =
            poll_spawn_resolution(&mut client, &before, &args.workspace, &output.stdout)?;

        complete_pending_spawn(
            &store,
            &reservation,
            &args.agent,
            &args.workspace,
            &resolution.session.session_id,
            &after,
        )?;

        Ok(SpawnOutput {
            role: args.role.clone(),
            agent: args.agent.clone(),
            workspace_id: args.workspace.clone(),
            session_id: resolution.session.session_id,
            strategy: resolution.strategy,
            request_id: reservation.request_id.clone(),
            registry: store.registry_path().to_path_buf(),
        })
    })();

    let spawn_output = match spawn_result {
        Ok(output) => output,
        Err(error) => {
            let _ = cancel_pending_spawn(&store, &reservation);
            return Err(error);
        }
    };

    if args.json {
        print_json(&spawn_output)?;
    } else {
        println!(
            "registered role '{}' -> session {}",
            spawn_output.role, spawn_output.session_id
        );
    }

    Ok(())
}

fn reserve_pending_spawn(
    store: &RegistryStore,
    role: &str,
    agent: &str,
    workspace_id: &str,
    sessions: &[TerminalSessionInfo],
) -> Result<SpawnReservation> {
    let _lock = store.acquire_lock()?;
    let mut registry = store.load()?;
    cleanup_stale_sessions(&mut registry, sessions);
    cleanup_stale_pending_spawns(&mut registry);
    if registry.roles.contains_key(role) {
        bail!("role '{}' is already registered", role);
    }
    if let Some(pending) = registry.pending_spawns.get(role) {
        bail!(
            "role '{}' is already spawning; request {} pid {} started at {}",
            role,
            pending.request_id,
            pending.pid,
            pending.started_at
        );
    }

    let request_id = spawn_request_id(role, agent, workspace_id);
    registry.pending_spawns.insert(
        role.to_owned(),
        PendingSpawnEntry::new(
            agent.to_owned(),
            workspace_id.to_owned(),
            request_id.clone(),
            "superset agents create".to_owned(),
        ),
    );
    store.save(&registry)?;

    Ok(SpawnReservation {
        role: role.to_owned(),
        request_id,
    })
}

fn complete_pending_spawn(
    store: &RegistryStore,
    reservation: &SpawnReservation,
    agent: &str,
    workspace_id: &str,
    session_id: &str,
    sessions: &[TerminalSessionInfo],
) -> Result<()> {
    let _lock = store.acquire_lock()?;
    let mut registry = store.load()?;
    cleanup_stale_sessions(&mut registry, sessions);
    cleanup_stale_pending_spawns(&mut registry);

    let pending = registry
        .pending_spawns
        .get(&reservation.role)
        .with_context(|| format!("pending spawn for role '{}' disappeared", reservation.role))?;
    if pending.request_id != reservation.request_id {
        bail!(
            "pending spawn for role '{}' was replaced; registry was not updated",
            reservation.role
        );
    }
    if registry.roles.contains_key(&reservation.role) {
        bail!(
            "role '{}' became registered while spawn was pending; registry was not updated",
            reservation.role
        );
    }

    registry.pending_spawns.remove(&reservation.role);
    registry.roles.insert(
        reservation.role.clone(),
        RegistryEntry::new(
            agent.to_owned(),
            workspace_id.to_owned(),
            session_id.to_owned(),
            "superset agents create".to_owned(),
        ),
    );
    store.save(&registry)
}

fn cancel_pending_spawn(store: &RegistryStore, reservation: &SpawnReservation) -> Result<bool> {
    let _lock = store.acquire_lock()?;
    let mut registry = store.load()?;
    let remove = registry
        .pending_spawns
        .get(&reservation.role)
        .is_some_and(|pending| pending.request_id == reservation.request_id);

    if remove {
        registry.pending_spawns.remove(&reservation.role);
        store.save(&registry)?;
    }

    Ok(remove)
}

fn send(config: &RuntimeConfig, args: SendArgs) -> Result<()> {
    ensure_force_unregistered_session_usage(args.role.as_deref(), args.force_unregistered_session)?;

    let body = read_send_body(&args)?;
    let source_path = args.file.as_deref();
    let store = RegistryStore::new(config.registry_dir.clone());
    let snapshot_policy = if args.dry_run {
        SnapshotPolicy::Skip
    } else {
        SnapshotPolicy::WriteIfNeeded
    };
    let prepared =
        prepare_structured_message(&store, "USER_REQUEST", &body, source_path, snapshot_policy)?;
    send_prepared(
        config,
        &store,
        SessionTargetRequest {
            role: args.role.as_deref(),
            session: args.session.as_deref(),
            force_unregistered_session: args.force_unregistered_session,
            workspace: args.workspace.as_deref(),
            dry_run: args.dry_run,
        },
        prepared,
    )
    .map(|_| ())
}

fn close(config: &RuntimeConfig, args: CloseArgs) -> Result<()> {
    ensure_force_unregistered_session_usage(args.role.as_deref(), args.force_unregistered_session)?;

    let store = RegistryStore::new(config.registry_dir.clone());
    let output = close_session(
        config,
        &store,
        SessionTargetRequest {
            role: args.role.as_deref(),
            session: args.session.as_deref(),
            force_unregistered_session: args.force_unregistered_session,
            workspace: args.workspace.as_deref(),
            dry_run: args.dry_run,
        },
        args.signal.as_str(),
    )?;

    if args.json {
        print_json(&output)?;
    } else {
        print_close_human(&output);
    }

    Ok(())
}

fn handoff(config: &RuntimeConfig, args: HandoffArgs) -> Result<()> {
    let body = fs::read_to_string(&args.file)
        .with_context(|| format!("failed to read {}", args.file.display()))?;
    let store = RegistryStore::new(config.registry_dir.clone());
    let prepared = prepare_structured_message(
        &store,
        "USER_REQUEST",
        &body,
        Some(&args.file),
        SnapshotPolicy::WriteIfNeeded,
    )?;
    let output = send_prepared(
        config,
        &store,
        SessionTargetRequest {
            role: Some(&args.to),
            session: None,
            force_unregistered_session: false,
            workspace: None,
            dry_run: false,
        },
        prepared,
    )?;
    print_send_human(&output);
    Ok(())
}

fn report(config: &RuntimeConfig, args: ReportArgs) -> Result<()> {
    let original = fs::read_to_string(&args.file)
        .with_context(|| format!("failed to read {}", args.file.display()))?;
    let saved_path = copy_report_file(&config.results_dir, &args.to, &args.file, &original)?;
    let pointer = format!(
        "path: {}\nsummary: {}\nbytes: {}\nsha256: {}",
        display_path(&saved_path),
        report_summary(&original),
        original.len(),
        sha256_hex(original.as_bytes())
    );

    let store = RegistryStore::new(config.registry_dir.clone());
    let prepared = prepare_structured_message(
        &store,
        "WORKER_REPORT_POINTER",
        &pointer,
        None,
        SnapshotPolicy::WriteIfNeeded,
    )?;
    let output = send_prepared(
        config,
        &store,
        SessionTargetRequest {
            role: Some(&args.to),
            session: None,
            force_unregistered_session: false,
            workspace: None,
            dry_run: false,
        },
        prepared,
    )?;
    print_send_human(&output);
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusReport {
    superset_cli: SupersetCliStatus,
    terminal_host: TerminalHostStatus,
    pty_daemon: PtyDaemonStatus,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SupersetCliStatus {
    path: String,
    found: bool,
    version: Option<CommandOutput>,
    status: Option<CommandOutput>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalHostStatus {
    socket_exists: bool,
    token_exists: bool,
    hello: Option<HelloResponse>,
    session_count: Option<usize>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PtyDaemonStatus {
    organization_id: Option<String>,
    manifest: String,
    socket: Option<String>,
    host_db: Option<String>,
    manifest_exists: bool,
    socket_exists: bool,
    host_db_exists: bool,
    hello: Option<PtyDaemonHello>,
    session_count: Option<usize>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpawnOutput {
    role: String,
    agent: String,
    workspace_id: String,
    session_id: String,
    strategy: crate::resolver::SpawnResolutionStrategy,
    request_id: String,
    registry: PathBuf,
}

#[derive(Debug, Clone)]
struct PreparedMessage {
    data: String,
    body_hash: String,
    body_bytes: usize,
    pointer_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct SpawnReservation {
    role: String,
    request_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotPolicy {
    WriteIfNeeded,
    Skip,
}

#[derive(Debug, Clone, Copy)]
struct SessionTargetRequest<'a> {
    role: Option<&'a str>,
    session: Option<&'a str>,
    force_unregistered_session: bool,
    workspace: Option<&'a str>,
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct SessionTarget {
    role: Option<String>,
    registered_role: Option<String>,
    session_id: String,
    workspace_id: String,
    registered: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendOutput {
    role: Option<String>,
    session_id: String,
    workspace_id: String,
    registered: bool,
    dry_run: bool,
    sent: bool,
    byte_size: usize,
    payload_hash: String,
    pointer_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloseOutput {
    role: Option<String>,
    session_id: String,
    workspace_id: String,
    registered: bool,
    dry_run: bool,
    closed: bool,
    signal: String,
    registry_updated: bool,
}

fn inspect_superset_cli(adapter: &SupersetCliAdapter) -> SupersetCliStatus {
    let found = adapter.bin().exists();
    let mut error = None;

    let version = match adapter.version() {
        Ok(output) => Some(output),
        Err(err) => {
            error = Some(err.to_string());
            None
        }
    };

    let status = if found {
        match adapter.status(true) {
            Ok(output) => Some(output),
            Err(err) => {
                if error.is_none() {
                    error = Some(err.to_string());
                }
                None
            }
        }
    } else {
        None
    };

    SupersetCliStatus {
        path: adapter.bin().display().to_string(),
        found,
        version,
        status,
        error,
    }
}

fn inspect_terminal_host(config: &RuntimeConfig) -> TerminalHostStatus {
    let socket_exists = config.terminal_host_socket.exists();
    let token_exists = config.terminal_host_token.exists();

    let mut status = TerminalHostStatus {
        socket_exists,
        token_exists,
        hello: None,
        session_count: None,
        error: None,
    };

    if !socket_exists || !token_exists {
        status.error = Some("terminal-host socket or token is missing".to_owned());
        return status;
    }

    match connect_terminal_host(config) {
        Ok(mut client) => {
            status.hello = Some(client.hello().clone());
            match client.list_sessions() {
                Ok(sessions) => status.session_count = Some(sessions.len()),
                Err(err) => status.error = Some(err.to_string()),
            }
        }
        Err(err) => status.error = Some(err.to_string()),
    }

    status
}

fn inspect_pty_daemon(
    config: &RuntimeConfig,
    superset_status: Option<&CommandOutput>,
) -> PtyDaemonStatus {
    let mut status = PtyDaemonStatus {
        organization_id: None,
        manifest: "-".to_owned(),
        socket: None,
        host_db: None,
        manifest_exists: false,
        socket_exists: false,
        host_db_exists: false,
        hello: None,
        session_count: None,
        error: None,
    };

    let Some(superset_status) = superset_status else {
        status.error = Some("superset status is unavailable".to_owned());
        return status;
    };
    if !superset_status.success {
        status.error = Some("superset status failed".to_owned());
        return status;
    }

    let runtime = match runtime_from_status_stdout(&superset_status.stdout) {
        Ok(runtime) => runtime,
        Err(error) => {
            status.error = Some(error.to_string());
            return status;
        }
    };

    let manifest_path = pty_daemon_manifest_path(&config.superset_home, &runtime.organization_id);
    let host_db = host_db_path(&config.superset_home, &runtime.organization_id);
    status.organization_id = Some(runtime.organization_id.clone());
    status.manifest = manifest_path.display().to_string();
    status.host_db = Some(host_db.display().to_string());
    status.manifest_exists = manifest_path.exists();
    status.host_db_exists = host_db.exists();

    let manifest = match load_pty_daemon_manifest(&config.superset_home, &runtime.organization_id) {
        Ok(manifest) => manifest,
        Err(error) => {
            status.error = Some(error.to_string());
            return status;
        }
    };

    status.socket = Some(manifest.socket_path.display().to_string());
    status.socket_exists = manifest.socket_path.exists();

    match connect_superset_runtime(config, &runtime) {
        Ok(mut client) => {
            status.hello = Some(client.hello().clone());
            match client.list_sessions() {
                Ok(sessions) => status.session_count = Some(sessions.len()),
                Err(error) => status.error = Some(error.to_string()),
            }
        }
        Err(error) => status.error = Some(error.to_string()),
    }

    status
}

fn send_prepared(
    config: &RuntimeConfig,
    store: &RegistryStore,
    request: SessionTargetRequest<'_>,
    prepared: PreparedMessage,
) -> Result<SendOutput> {
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    let runtime = ensure_superset_host_running(&adapter)?;

    let mut client = connect_superset_runtime(config, &runtime)?;
    let sessions = client.list_sessions()?;
    let target = {
        let _lock = store.acquire_lock()?;
        let mut registry = store.load()?;
        let session_cleanup = cleanup_stale_sessions(&mut registry, &sessions);
        let pending_cleanup = cleanup_stale_pending_spawns(&mut registry);
        let target = resolve_session_target(request, &registry, &sessions)?;
        if !request.dry_run
            && (session_cleanup.removed_roles > 0 || pending_cleanup.removed_pending_spawns > 0)
        {
            store.save(&registry)?;
        }
        target
    };

    let output = SendOutput {
        role: target.role.clone(),
        session_id: target.session_id.clone(),
        workspace_id: target.workspace_id.clone(),
        registered: target.registered,
        dry_run: request.dry_run,
        sent: !request.dry_run,
        byte_size: prepared.body_bytes,
        payload_hash: prepared.body_hash.clone(),
        pointer_path: prepared.pointer_path.clone(),
    };

    if request.dry_run {
        print_send_human(&output);
        return Ok(output);
    }

    client.write(&target.session_id, &prepared.data)?;

    if let Some(role) = &target.registered_role {
        touch_registered_role(store, role, &target.session_id)?;
    } else {
        store.append_force_send_audit(&ForceSendAuditEntry::new(
            target.role.clone(),
            target.session_id.clone(),
            target.workspace_id.clone(),
            prepared.body_hash.clone(),
            prepared.body_bytes,
        ))?;
    }

    Ok(output)
}

fn close_session(
    config: &RuntimeConfig,
    store: &RegistryStore,
    request: SessionTargetRequest<'_>,
    signal: &str,
) -> Result<CloseOutput> {
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    let runtime = ensure_superset_host_running(&adapter)?;

    let mut client = connect_superset_runtime(config, &runtime)?;
    let sessions = client.list_sessions()?;
    let target = {
        let _lock = store.acquire_lock()?;
        let mut registry = store.load()?;
        let session_cleanup = cleanup_stale_sessions(&mut registry, &sessions);
        let pending_cleanup = cleanup_stale_pending_spawns(&mut registry);
        let target = resolve_session_target(request, &registry, &sessions)?;
        if !request.dry_run
            && (session_cleanup.removed_roles > 0 || pending_cleanup.removed_pending_spawns > 0)
        {
            store.save(&registry)?;
        }
        target
    };

    let mut output = CloseOutput {
        role: target.role.clone(),
        session_id: target.session_id.clone(),
        workspace_id: target.workspace_id.clone(),
        registered: target.registered,
        dry_run: request.dry_run,
        closed: !request.dry_run,
        signal: signal.to_owned(),
        registry_updated: false,
    };

    if request.dry_run {
        return Ok(output);
    }

    client.close(&target.session_id, signal)?;

    output.registry_updated = remove_registered_role_after_close(store, &target)?;

    Ok(output)
}

fn touch_registered_role(store: &RegistryStore, role: &str, session_id: &str) -> Result<bool> {
    let _lock = store.acquire_lock()?;
    let mut registry = store.load()?;
    let Some(entry) = registry.roles.get_mut(role) else {
        return Ok(false);
    };
    if entry.session_id != session_id {
        return Ok(false);
    }
    entry.touch_verified();
    store.save(&registry)?;
    Ok(true)
}

fn remove_registered_role_after_close(
    store: &RegistryStore,
    target: &SessionTarget,
) -> Result<bool> {
    let Some(role) = &target.registered_role else {
        return Ok(false);
    };

    let _lock = store.acquire_lock()?;
    let mut registry = store.load()?;
    let Some(entry) = registry.roles.get(role) else {
        return Ok(false);
    };
    if entry.session_id != target.session_id {
        return Ok(false);
    }

    registry.roles.remove(role);
    store.save(&registry)?;
    Ok(true)
}

fn resolve_session_target(
    request: SessionTargetRequest<'_>,
    registry: &Registry,
    sessions: &[TerminalSessionInfo],
) -> Result<SessionTarget> {
    match (request.role, request.session) {
        (Some(role), None) => {
            let entry = registry
                .roles
                .get(role)
                .with_context(|| format!("role '{role}' is not registered"))?;
            let session = verified_registered_session(entry, sessions)?;
            Ok(SessionTarget {
                role: Some(role.to_owned()),
                registered_role: Some(role.to_owned()),
                session_id: entry.session_id.clone(),
                workspace_id: session
                    .workspace_id
                    .clone()
                    .unwrap_or_else(|| entry.workspace_id.clone()),
                registered: true,
            })
        }
        (None, Some(session_id)) => {
            if let Some((role, entry)) = role_for_session(registry, session_id) {
                let session = verified_registered_session(entry, sessions)?;
                return Ok(SessionTarget {
                    role: Some(role.to_owned()),
                    registered_role: Some(role.to_owned()),
                    session_id: session_id.to_owned(),
                    workspace_id: session
                        .workspace_id
                        .clone()
                        .unwrap_or_else(|| entry.workspace_id.clone()),
                    registered: true,
                });
            }

            if !request.force_unregistered_session {
                bail!(
                    "session is not registered; use --force-unregistered-session --workspace <workspace-id> to target it explicitly"
                );
            }
            let workspace = request
                .workspace
                .context("--workspace is required with --force-unregistered-session")?;
            let session = find_alive_session(sessions, session_id)
                .with_context(|| format!("session '{session_id}' is not alive"))?;
            if session.workspace_id.as_deref() != Some(workspace) {
                bail!("forced session workspace does not match --workspace");
            }
            Ok(SessionTarget {
                role: None,
                registered_role: None,
                session_id: session_id.to_owned(),
                workspace_id: workspace.to_owned(),
                registered: false,
            })
        }
        _ => bail!("exactly one of --role or --session is required"),
    }
}

fn ensure_force_unregistered_session_usage(role: Option<&str>, force: bool) -> Result<()> {
    if force && role.is_some() {
        bail!("--force-unregistered-session is only valid with --session");
    }
    Ok(())
}

#[cfg(test)]
fn apply_close_registry_update(
    registry: &mut Registry,
    target: &SessionTarget,
    dry_run: bool,
) -> Result<bool> {
    if dry_run {
        return Ok(false);
    }

    let Some(role) = &target.registered_role else {
        return Ok(false);
    };
    registry
        .roles
        .remove(role)
        .with_context(|| format!("registered close target '{role}' disappeared"))?;
    Ok(true)
}

fn verified_registered_session<'a>(
    entry: &RegistryEntry,
    sessions: &'a [TerminalSessionInfo],
) -> Result<&'a TerminalSessionInfo> {
    let session = find_alive_session(sessions, &entry.session_id)
        .with_context(|| format!("registered session '{}' is not alive", entry.session_id))?;
    if session.workspace_id.as_deref() != Some(entry.workspace_id.as_str()) {
        bail!("registered session workspace does not match registry");
    }
    Ok(session)
}

fn find_alive_session<'a>(
    sessions: &'a [TerminalSessionInfo],
    session_id: &str,
) -> Option<&'a TerminalSessionInfo> {
    sessions
        .iter()
        .find(|session| session.is_alive && session.session_id == session_id)
}

fn prepare_structured_message(
    store: &RegistryStore,
    kind: &str,
    body: &str,
    source_path: Option<&Path>,
    snapshot_policy: SnapshotPolicy,
) -> Result<PreparedMessage> {
    let body_hash = sha256_hex(body.as_bytes());
    let body_bytes = body.len();
    match structured_block_paste(kind, body) {
        Ok(data) => Ok(PreparedMessage {
            data,
            body_hash,
            body_bytes,
            pointer_path: None,
        }),
        Err(TransportError::TooLarge { .. }) => {
            let (pointer_path, pointer_display) = match source_path {
                Some(path) => {
                    let pointer_path = path.to_path_buf();
                    let pointer_display = display_path(&pointer_path);
                    (Some(pointer_path), pointer_display)
                }
                None if snapshot_policy == SnapshotPolicy::WriteIfNeeded => {
                    let pointer_path = store.write_message_snapshot(body)?;
                    let pointer_display = display_path(&pointer_path);
                    (Some(pointer_path), pointer_display)
                }
                None => (None, "<snapshot skipped for dry-run>".to_owned()),
            };
            let pointer_kind = format!("{kind}_POINTER");
            let pointer_body = format!(
                "path: {}\nbytes: {}\nsha256: {}",
                pointer_display, body_bytes, body_hash
            );
            let data = structured_block_paste(&pointer_kind, &pointer_body)
                .context("failed to prepare pointer message")?;
            Ok(PreparedMessage {
                data,
                body_hash,
                body_bytes,
                pointer_path,
            })
        }
        Err(error) => Err(error).context("message payload failed sanitation"),
    }
}

fn poll_spawn_resolution(
    client: &mut SupersetRuntimeClient,
    before: &[TerminalSessionInfo],
    workspace_id: &str,
    cli_stdout: &str,
) -> Result<(Vec<TerminalSessionInfo>, SpawnResolution)> {
    let mut last_sessions = Vec::new();
    let mut last_error = None;

    for _ in 0..SPAWN_CORRELATION_ATTEMPTS {
        let sessions = client.list_sessions()?;
        match resolve_spawned_session(before, &sessions, workspace_id, cli_stdout) {
            Ok(resolution) => return Ok((sessions, resolution)),
            Err(error) => {
                last_error = Some(error);
                last_sessions = sessions;
                thread::sleep(SPAWN_CORRELATION_INTERVAL);
            }
        }
    }

    if let Some(error) = last_error {
        Err(error)
    } else {
        bail!("could not inspect pty-daemon sessions after spawn; registry was not updated");
    }
    .with_context(|| {
        format!(
            "last pty-daemon session count after spawn: {}",
            last_sessions.len()
        )
    })
}

fn ensure_superset_host_running(adapter: &SupersetCliAdapter) -> Result<SupersetHostRuntime> {
    let output = adapter.status(true)?;
    ensure_command_success(&output, "superset status")?;

    let value: Value =
        serde_json::from_str(&output.stdout).context("superset status did not return JSON")?;
    let running = value
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let healthy = value
        .get("healthy")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !running || !healthy {
        bail!("Superset host is not running and healthy");
    }
    runtime_from_status_stdout(&output.stdout)
}

fn ensure_terminal_agent_create_stdout(stdout: &str) -> Result<()> {
    let value: Value =
        serde_json::from_str(stdout).context("superset agents create did not return JSON")?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .context("superset agents create output omitted kind")?;
    if kind != "terminal" {
        bail!(
            "superset agents create returned a '{kind}' session; ssctl can only register terminal-backed sessions"
        );
    }
    if value.get("sessionId").and_then(Value::as_str).is_none() {
        bail!("superset agents create terminal output omitted sessionId");
    }
    Ok(())
}

fn ensure_command_success(output: &CommandOutput, label: &str) -> Result<()> {
    if output.success {
        return Ok(());
    }

    let detail = first_nonempty_line(&output.stderr)
        .or_else(|| first_nonempty_line(&output.stdout))
        .unwrap_or("no output");
    bail!(
        "{label} failed with status {}: {detail}",
        output
            .status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_owned())
    );
}

fn connect_terminal_host(config: &RuntimeConfig) -> Result<TerminalHostClient> {
    let paths = TerminalHostPaths::new(
        config.terminal_host_socket.clone(),
        config.terminal_host_token.clone(),
    );
    TerminalHostClient::connect(&paths)
}

fn connect_superset_runtime(
    config: &RuntimeConfig,
    runtime: &SupersetHostRuntime,
) -> Result<SupersetRuntimeClient> {
    SupersetRuntimeClient::connect(&config.superset_home, runtime)
}

fn read_file_or_text(value: &str) -> Result<String> {
    let path = Path::new(value);
    if path.exists() {
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
    } else {
        Ok(value.to_owned())
    }
}

fn read_send_body(args: &SendArgs) -> Result<String> {
    if let Some(path) = &args.file {
        return fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()));
    }

    if args.stdin {
        let mut body = String::new();
        io::stdin()
            .read_to_string(&mut body)
            .context("failed to read stdin")?;
        return Ok(body);
    }

    bail!("exactly one input source is required")
}

fn copy_report_file(
    results_dir: &Path,
    to: &str,
    source_path: &Path,
    contents: &str,
) -> Result<PathBuf> {
    let target_dir = results_dir.join(sanitize_filename_component(to));
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;
    fs::set_permissions(results_dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to set permissions on {}", results_dir.display()))?;
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to set permissions on {}", target_dir.display()))?;

    let source_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("report.md");
    let filename = format!(
        "{}-{}",
        timestamp_for_filename(),
        sanitize_filename_component(source_name)
    );
    let target_path = target_dir.join(filename);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&target_path)
        .context("failed to create report copy")?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(target_path)
}

fn report_summary(contents: &str) -> String {
    let summary = contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("report saved by ssctl");
    truncate(summary, 160)
}

fn sanitize_filename_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "item".to_owned()
    } else {
        sanitized
    }
}

fn passthrough_output(output: CommandOutput) -> Result<()> {
    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    if !output.success {
        bail!(
            "superset command failed with status {}",
            output
                .status_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        );
    }
    Ok(())
}

fn print_status_human(report: &StatusReport) -> Result<()> {
    let mut stdout = io::stdout().lock();

    writeln!(stdout, "Superset CLI")?;
    writeln!(stdout, "  path: {}", report.superset_cli.path)?;
    writeln!(stdout, "  found: {}", report.superset_cli.found)?;
    if let Some(version) = &report.superset_cli.version {
        write_command_summary(&mut stdout, "version", version)?;
    }
    if let Some(status) = &report.superset_cli.status {
        write_command_summary(&mut stdout, "status", status)?;
    }
    if let Some(error) = &report.superset_cli.error {
        writeln!(stdout, "  error: {error}")?;
    }

    writeln!(stdout)?;
    writeln!(stdout, "Terminal host")?;
    writeln!(
        stdout,
        "  socket: {}",
        exists_label(report.terminal_host.socket_exists)
    )?;
    writeln!(
        stdout,
        "  token: {}",
        exists_label(report.terminal_host.token_exists)
    )?;
    if let Some(hello) = &report.terminal_host.hello {
        writeln!(
            stdout,
            "  hello: ok protocol={} daemon={} pid={}",
            hello.protocol_version,
            hello.daemon_version,
            hello
                .daemon_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        )?;
    }
    if let Some(count) = report.terminal_host.session_count {
        writeln!(stdout, "  sessions: {count}")?;
    }
    if let Some(error) = &report.terminal_host.error {
        writeln!(stdout, "  error: {error}")?;
    }

    writeln!(stdout)?;
    writeln!(stdout, "PTY daemon")?;
    if let Some(organization_id) = &report.pty_daemon.organization_id {
        writeln!(stdout, "  organization: {organization_id}")?;
    }
    writeln!(
        stdout,
        "  manifest: {} ({})",
        report.pty_daemon.manifest,
        exists_label(report.pty_daemon.manifest_exists)
    )?;
    if let Some(socket) = &report.pty_daemon.socket {
        writeln!(
            stdout,
            "  socket: {} ({})",
            socket,
            exists_label(report.pty_daemon.socket_exists)
        )?;
    }
    if let Some(host_db) = &report.pty_daemon.host_db {
        writeln!(
            stdout,
            "  host db: {} ({})",
            host_db,
            exists_label(report.pty_daemon.host_db_exists)
        )?;
    }
    if let Some(hello) = &report.pty_daemon.hello {
        writeln!(
            stdout,
            "  hello: ok protocol={} daemon={} pid={}",
            hello.protocol,
            hello.daemon_version,
            hello
                .daemon_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        )?;
    }
    if let Some(count) = report.pty_daemon.session_count {
        writeln!(stdout, "  sessions: {count}")?;
    }
    if let Some(error) = &report.pty_daemon.error {
        writeln!(stdout, "  error: {error}")?;
    }

    Ok(())
}

fn write_command_summary(
    stdout: &mut impl Write,
    label: &str,
    output: &CommandOutput,
) -> Result<()> {
    writeln!(
        stdout,
        "  {label}: {} status={}",
        if output.success { "ok" } else { "failed" },
        output
            .status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_owned())
    )?;
    let stdout_text = output.stdout.trim();
    if !stdout_text.is_empty() {
        writeln!(stdout, "    stdout: {}", first_line(stdout_text))?;
    }
    let stderr_text = output.stderr.trim();
    if !stderr_text.is_empty() {
        writeln!(stdout, "    stderr: {}", first_line(stderr_text))?;
    }
    Ok(())
}

fn print_sessions_human(sessions: &[TerminalSessionInfo]) {
    if sessions.is_empty() {
        println!("No pty-daemon sessions.");
        return;
    }

    println!(
        "{:<38} {:<38} {:<5} {:<8} {:<7} {:<24} SHELL",
        "SESSION", "WORKSPACE", "ALIVE", "PID", "CLIENTS", "CREATED"
    );
    for session in sessions {
        println!(
            "{:<38} {:<38} {:<5} {:<8} {:<7} {:<24} {}",
            truncate(&session.session_id, 38),
            truncate(session.workspace_id.as_deref().unwrap_or("-"), 38),
            session.is_alive,
            session
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            session.attached_clients,
            truncate(session.created_at.as_deref().unwrap_or("-"), 24),
            session.shell.as_deref().unwrap_or("-")
        );
    }
}

fn print_send_human(output: &SendOutput) {
    if output.dry_run {
        println!(
            "dry-run: session {} workspace {} bytes {} sha256 {}",
            output.session_id, output.workspace_id, output.byte_size, output.payload_hash
        );
    } else {
        println!(
            "sent: session {} workspace {} bytes {} sha256 {}",
            output.session_id, output.workspace_id, output.byte_size, output.payload_hash
        );
    }
    if let Some(path) = &output.pointer_path {
        println!("pointer: {}", display_path(path));
    }
}

fn print_close_human(output: &CloseOutput) {
    if output.dry_run {
        println!(
            "dry-run: would close session {} workspace {} signal {}",
            output.session_id, output.workspace_id, output.signal
        );
    } else {
        println!(
            "closed: session {} workspace {} signal {}",
            output.session_id, output.workspace_id, output.signal
        );
    }
    if output.registry_updated
        && let Some(role) = &output.role
    {
        println!("removed role: {role}");
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn exists_label(exists: bool) -> &'static str {
    if exists { "present" } else { "missing" }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(ch);
    }
    output
}

fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{
        SessionTarget, SessionTargetRequest, SnapshotPolicy, apply_close_registry_update,
        cancel_pending_spawn, complete_pending_spawn, ensure_force_unregistered_session_usage,
        prepare_structured_message, reserve_pending_spawn, resolve_session_target,
        sanitize_filename_component,
    };
    use crate::registry::{PendingSpawnEntry, Registry, RegistryEntry, RegistryStore};
    use crate::terminal_host::TerminalSessionInfo;

    #[test]
    fn oversized_stdin_payload_becomes_pointer_snapshot() {
        let root = test_root("message");
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
        let body = "x".repeat(crate::transport::DEFAULT_MAX_INLINE_BYTES + 1);

        let prepared = prepare_structured_message(
            &store,
            "USER_REQUEST",
            &body,
            None,
            SnapshotPolicy::WriteIfNeeded,
        )
        .unwrap();

        assert!(prepared.pointer_path.is_some());
        assert!(prepared.data.contains("[USER_REQUEST_POINTER]"));
        assert!(!prepared.data.contains(&body));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_dry_run_stdin_payload_does_not_write_snapshot() {
        let root = test_root("dry-run-message");
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
        let body = "x".repeat(crate::transport::DEFAULT_MAX_INLINE_BYTES + 1);

        let prepared =
            prepare_structured_message(&store, "USER_REQUEST", &body, None, SnapshotPolicy::Skip)
                .unwrap();

        assert!(prepared.pointer_path.is_none());
        assert!(prepared.data.contains("[USER_REQUEST_POINTER]"));
        assert!(prepared.data.contains("<snapshot skipped for dry-run>"));
        assert!(!store.messages_dir().exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_reservation_rejects_active_existing_role_before_create() {
        let root = test_root("spawn-active-role");
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
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

        let error = reserve_pending_spawn(
            &store,
            "worker",
            "codex",
            "workspace-1",
            &[session("session-1", "workspace-1", true)],
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "role 'worker' is already registered");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_reservation_cleans_stale_role_before_duplicate_check() {
        let root = test_root("spawn-stale-role");
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
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

        let reservation =
            reserve_pending_spawn(&store, "worker", "codex", "workspace-1", &[]).unwrap();
        let loaded = store.load().unwrap();

        assert!(!loaded.roles.contains_key("worker"));
        assert!(loaded.pending_spawns.contains_key("worker"));
        assert!(!store.load().unwrap().roles.contains_key("worker"));
        cancel_pending_spawn(&store, &reservation).unwrap();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_reservation_rejects_existing_pending_role() {
        let root = test_root("spawn-pending-role");
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
        let first = reserve_pending_spawn(&store, "worker", "codex", "workspace-1", &[]).unwrap();

        let error =
            reserve_pending_spawn(&store, "worker", "codex", "workspace-1", &[]).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("role 'worker' is already spawning")
        );
        cancel_pending_spawn(&store, &first).unwrap();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_reservation_cleans_stale_pending_role() {
        let root = test_root("spawn-stale-pending-role");
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
        let mut registry = Registry::default();
        let mut pending = PendingSpawnEntry::new(
            "codex".to_owned(),
            "workspace-1".to_owned(),
            "stale-request".to_owned(),
            "superset agents create".to_owned(),
        );
        pending.pid = u32::MAX;
        registry.pending_spawns.insert("worker".to_owned(), pending);
        store.save(&registry).unwrap();

        let reservation =
            reserve_pending_spawn(&store, "worker", "codex", "workspace-1", &[]).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(
            loaded.pending_spawns["worker"].request_id,
            reservation.request_id
        );
        cancel_pending_spawn(&store, &reservation).unwrap();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn completing_spawn_promotes_matching_pending_entry() {
        let root = test_root("spawn-complete");
        let _ = fs::remove_dir_all(&root);
        let store = RegistryStore::new(&root);
        let reservation =
            reserve_pending_spawn(&store, "worker", "codex", "workspace-1", &[]).unwrap();

        complete_pending_spawn(
            &store,
            &reservation,
            "codex",
            "workspace-1",
            "session-1",
            &[session("session-1", "workspace-1", true)],
        )
        .unwrap();

        let loaded = store.load().unwrap();
        assert!(!loaded.pending_spawns.contains_key("worker"));
        assert_eq!(loaded.roles["worker"].session_id, "session-1");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn force_unregistered_session_is_only_valid_with_session_target() {
        let error = ensure_force_unregistered_session_usage(Some("worker"), true).unwrap_err();

        assert_eq!(
            error.to_string(),
            "--force-unregistered-session is only valid with --session"
        );
        assert!(ensure_force_unregistered_session_usage(None, true).is_ok());
    }

    #[test]
    fn resolves_registered_role_target() {
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

        let target = resolve_session_target(
            SessionTargetRequest {
                role: Some("worker"),
                session: None,
                force_unregistered_session: false,
                workspace: None,
                dry_run: false,
            },
            &registry,
            &[session("session-1", "workspace-1", true)],
        )
        .unwrap();

        assert_eq!(target.role.as_deref(), Some("worker"));
        assert_eq!(target.registered_role.as_deref(), Some("worker"));
        assert_eq!(target.session_id, "session-1");
        assert_eq!(target.workspace_id, "workspace-1");
        assert!(target.registered);
    }

    #[test]
    fn unregistered_session_requires_force_and_workspace() {
        let registry = Registry::default();
        let sessions = [session("session-1", "workspace-1", true)];

        let error = resolve_session_target(
            SessionTargetRequest {
                role: None,
                session: Some("session-1"),
                force_unregistered_session: false,
                workspace: None,
                dry_run: false,
            },
            &registry,
            &sessions,
        )
        .unwrap_err();

        assert!(error.to_string().contains("--force-unregistered-session"));

        let error = resolve_session_target(
            SessionTargetRequest {
                role: None,
                session: Some("session-1"),
                force_unregistered_session: true,
                workspace: None,
                dry_run: false,
            },
            &registry,
            &sessions,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "--workspace is required with --force-unregistered-session"
        );
    }

    #[test]
    fn close_registry_update_removes_registered_role() {
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

        let updated = apply_close_registry_update(
            &mut registry,
            &SessionTarget {
                role: Some("worker".to_owned()),
                registered_role: Some("worker".to_owned()),
                session_id: "session-1".to_owned(),
                workspace_id: "workspace-1".to_owned(),
                registered: true,
            },
            false,
        )
        .unwrap();

        assert!(updated);
        assert!(!registry.roles.contains_key("worker"));
    }

    #[test]
    fn dry_run_close_registry_update_does_not_mutate() {
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

        let updated = apply_close_registry_update(
            &mut registry,
            &SessionTarget {
                role: Some("worker".to_owned()),
                registered_role: Some("worker".to_owned()),
                session_id: "session-1".to_owned(),
                workspace_id: "workspace-1".to_owned(),
                registered: true,
            },
            true,
        )
        .unwrap();

        assert!(!updated);
        assert!(registry.roles.contains_key("worker"));
    }

    #[test]
    fn filename_component_sanitizes_path_separators() {
        assert_eq!(
            sanitize_filename_component("../worker result.md"),
            ".._worker_result.md"
        );
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ssctl-{name}-test-{}", std::process::id()))
    }

    fn session(session_id: &str, workspace_id: &str, is_alive: bool) -> TerminalSessionInfo {
        TerminalSessionInfo {
            session_id: session_id.to_owned(),
            workspace_id: Some(workspace_id.to_owned()),
            pane_id: None,
            is_alive,
            attached_clients: 0,
            pid: None,
            created_at: None,
            last_attached_at: None,
            shell: None,
        }
    }
}
