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
    AgentsCommand, AgentsListArgs, Cli, Command, HandoffArgs, OutputArgs, ReportArgs, SendArgs,
    SpawnArgs,
};
use crate::config::RuntimeConfig;
use crate::registry::{
    ForceSendAuditEntry, Registry, RegistryEntry, RegistryStore, cleanup_stale_sessions,
    role_for_session, sha256_hex, timestamp_for_filename,
};
use crate::resolver::{SpawnResolution, resolve_spawned_session};
use crate::superset_cli::{CommandOutput, SupersetCliAdapter};
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
        Command::Handoff(args) => handoff(&config, args),
        Command::Report(args) => report(&config, args),
    }
}

fn status(config: &RuntimeConfig, args: OutputArgs) -> Result<()> {
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    let cli_report = inspect_superset_cli(&adapter);
    let terminal_host_report = inspect_terminal_host(config);
    let report = StatusReport {
        superset_cli: cli_report,
        terminal_host: terminal_host_report,
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
    let mut client = connect_terminal_host(config)?;
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
    let _lock = store.acquire_lock()?;
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    ensure_superset_host_running(&adapter)?;

    let mut client = connect_terminal_host(config)?;
    let before = client.list_sessions()?;
    let mut registry = load_spawn_registry(&store, &args.role, &before)?;
    let prompt = read_file_or_text(&args.prompt)?;
    let output = adapter.agents_create(&args.workspace, &args.agent, &prompt)?;
    ensure_command_success(&output, "superset agents create")?;

    let (after, resolution) =
        poll_spawn_resolution(&mut client, &before, &args.workspace, &output.stdout)?;

    cleanup_stale_sessions(&mut registry, &after);
    registry.roles.insert(
        args.role.clone(),
        RegistryEntry::new(
            args.agent.clone(),
            args.workspace.clone(),
            resolution.session.session_id.clone(),
            "superset agents create".to_owned(),
        ),
    );
    store.save(&registry)?;

    let spawn_output = SpawnOutput {
        role: args.role,
        agent: args.agent,
        workspace_id: args.workspace,
        session_id: resolution.session.session_id,
        strategy: resolution.strategy,
        registry: store.registry_path().to_path_buf(),
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

fn load_spawn_registry(
    store: &RegistryStore,
    role: &str,
    sessions: &[TerminalSessionInfo],
) -> Result<Registry> {
    let mut registry = store.load()?;
    let cleanup = cleanup_stale_sessions(&mut registry, sessions);
    if registry.roles.contains_key(role) {
        bail!("role '{}' is already registered", role);
    }
    if cleanup.removed_roles > 0 {
        store.save(&registry)?;
    }
    Ok(registry)
}

fn send(config: &RuntimeConfig, args: SendArgs) -> Result<()> {
    if args.force_unregistered_session && args.role.is_some() {
        bail!("--force-unregistered-session is only valid with --session");
    }

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
        SendRequest {
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
        SendRequest {
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
        SendRequest {
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
struct SpawnOutput {
    role: String,
    agent: String,
    workspace_id: String,
    session_id: String,
    strategy: crate::resolver::SpawnResolutionStrategy,
    registry: PathBuf,
}

#[derive(Debug, Clone)]
struct PreparedMessage {
    data: String,
    body_hash: String,
    body_bytes: usize,
    pointer_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotPolicy {
    WriteIfNeeded,
    Skip,
}

#[derive(Debug, Clone, Copy)]
struct SendRequest<'a> {
    role: Option<&'a str>,
    session: Option<&'a str>,
    force_unregistered_session: bool,
    workspace: Option<&'a str>,
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct SendTarget {
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

fn send_prepared(
    config: &RuntimeConfig,
    store: &RegistryStore,
    request: SendRequest<'_>,
    prepared: PreparedMessage,
) -> Result<SendOutput> {
    let _lock = store.acquire_lock()?;
    let adapter = SupersetCliAdapter::new(config.superset_bin.clone());
    ensure_superset_host_running(&adapter)?;

    let mut client = connect_terminal_host(config)?;
    let sessions = client.list_sessions()?;
    let mut registry = store.load()?;
    let cleanup = cleanup_stale_sessions(&mut registry, &sessions);
    let target = resolve_send_target(request, &registry, &sessions)?;

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
        let entry = registry
            .roles
            .get_mut(role)
            .context("registered send target disappeared")?;
        entry.touch_verified();
    }

    if target.registered {
        store.save(&registry)?;
    } else {
        if cleanup.removed_roles > 0 {
            store.save(&registry)?;
        }
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

fn resolve_send_target(
    request: SendRequest<'_>,
    registry: &Registry,
    sessions: &[TerminalSessionInfo],
) -> Result<SendTarget> {
    match (request.role, request.session) {
        (Some(role), None) => {
            let entry = registry
                .roles
                .get(role)
                .with_context(|| format!("role '{role}' is not registered"))?;
            let session = verified_registered_session(entry, sessions)?;
            Ok(SendTarget {
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
                return Ok(SendTarget {
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
            Ok(SendTarget {
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
    client: &mut TerminalHostClient,
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
        bail!("could not inspect terminal-host sessions after spawn; registry was not updated");
    }
    .with_context(|| {
        format!(
            "last terminal-host session count after spawn: {}",
            last_sessions.len()
        )
    })
}

fn ensure_superset_host_running(adapter: &SupersetCliAdapter) -> Result<()> {
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
        println!("No terminal-host sessions.");
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
        SnapshotPolicy, load_spawn_registry, prepare_structured_message,
        sanitize_filename_component,
    };
    use crate::registry::{Registry, RegistryEntry, RegistryStore};
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
    fn spawn_registry_rejects_active_existing_role_before_create() {
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

        let error = load_spawn_registry(
            &store,
            "worker",
            &[session("session-1", "workspace-1", true)],
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "role 'worker' is already registered");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_registry_cleans_stale_role_before_duplicate_check() {
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

        let loaded = load_spawn_registry(&store, "worker", &[]).unwrap();

        assert!(!loaded.roles.contains_key("worker"));
        assert!(!store.load().unwrap().roles.contains_key("worker"));

        let _ = fs::remove_dir_all(&root);
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
