---
name: ssctl
description: >-
  Use this skill when an AI coding agent needs to manage Superset terminal-backed
  subagents with the ssctl CLI: check status, list agents or sessions, spawn
  role-registered workers, send follow-up or handoff messages, deliver report
  pointers, or close sessions safely.
---

# ssctl

Use `ssctl` as the control plane for Superset terminal-backed agent sessions.

`ssctl` complements the Superset CLI. It tracks local role-to-session mappings, sends input to existing sessions, sends handoff messages, sends report pointers, and closes sessions through the Superset pty-daemon runtime.

## Command Selection

Examples below use `ssctl`.

If an installed binary is unavailable and you are inside the `ssctl` source repository, replace:

```sh
ssctl <args>
```

with:

```sh
cargo run --quiet -- <args>
```

Run commands from the repository or workspace whose sessions you want to manage. The registry is working-directory local.

## Current Runtime Model

Current `ssctl` behavior:

- Public Superset operations go through the Superset CLI.
- `spawn` calls `superset agents create` and only registers terminal-backed agent sessions.
- `sessions`, `send`, and `close` use the Superset pty-daemon protocol v2 plus Superset host DB metadata.
- `status` also reports terminal-host diagnostics, but normal send and close routing should be treated as pty-daemon based.
- The default Superset CLI path is `~/.superset/bin/superset`; override with `--superset-bin <path>`.
- The default Superset home is `~/.superset`; override with `--superset-home <path>`.

Do not connect directly to Superset private sockets, write Superset host DB files, edit pty-daemon manifests, or manually edit `.ssctl/registry.json`.

## Preflight

Before spawning, sending, or closing sessions:

1. Check runtime health:

   ```sh
   ssctl status
   ```

2. If parsing output matters, use JSON:

   ```sh
   ssctl status --json
   ```

3. Confirm the Superset host is running and healthy. If not, stop and report that Superset must be started outside this skill.

4. List available agents when the agent id is not known:

   ```sh
   ssctl agents list --json
   ```

5. Confirm the target `workspace_id`. If the user did not provide one and it cannot be inferred safely, ask for it.

## Role Policy

Prefer role-targeted operations.

- Use one role per bounded worker assignment, such as `worker-index-tests` or `review-auth-diff`.
- Do not reuse a role that is still registered.
- Do not create idle standing workers.
- Close a worker role after its useful report has been captured.

`ssctl` stores role mappings in `.ssctl/registry.json` under the current working directory. `.ssctl/` is local runtime state and should not be committed.

## Spawning A Worker

Create a prompt file for any substantial assignment. In this repository, put temporary prompts and notes under `docs/local/`.

Prompt template:

```text
[SSCTL_ASSIGNMENT]
role: <role>
message_type: assignment

Objective:
<single bounded objective>

Scope:
<allowed files, directories, commands, or modules>

Stop condition:
<when to stop>

Do not:
- <explicit exclusions>

Required report:
- summary
- files inspected
- files modified
- commands run
- findings
- blockers
- recommendation
- report_path

Reporting:
- Write the full report to a local Markdown file.
- If the target role is registered and reachable, send a pointer with:
  ssctl report --to <target-role> --file <report-file>
- If no target role exists, leave the report file in the workspace and state the path.
[/SSCTL_ASSIGNMENT]
```

Spawn and register the worker:

```sh
ssctl spawn --agent <agent-id> --role <role> --workspace <workspace-id> --prompt <prompt-file> --json
```

If `spawn` cannot uniquely correlate the created terminal-backed session, it fails without updating the registry. Do not manually insert a registry entry. Inspect:

```sh
ssctl status
ssctl sessions --json
```

Then retry with a fresh role after the ambiguity is understood.

## Sending Messages

Send follow-up instructions to a registered role:

```sh
ssctl send --role <role> --file <message-file>
```

Use a dry run when confirming the target or payload:

```sh
ssctl send --role <role> --file <message-file> --dry-run
```

Use `handoff` when the message is specifically a task handoff to another role:

```sh
ssctl handoff --to <role> --file <handoff-file>
```

For generated content, `--stdin` is valid:

```sh
printf '%s\n' '<message>' | ssctl send --role <role> --stdin
```

Avoid raw terminal control sequences. `ssctl` wraps messages as bracketed paste and rejects unsafe payloads such as NUL, raw carriage returns, unexpected control characters, and embedded bracketed-paste controls.

Large messages are converted to pointer messages. Prefer storing substantial instructions in files so the pointer path is meaningful.

## Reports

Long reports should be files, not pasted terminal text.

To copy a report into `.agent-results/<target-role>/` and send a `WORKER_REPORT_POINTER` to a registered role:

```sh
ssctl report --to <target-role> --file <report-file>
```

Read the report file named by the pointer before synthesizing. Do not paste full report contents back into another terminal unless there is no sidecar or pointer path available.

Only use `report --to` when the recipient is a registered `ssctl` role. If the main user thread is not a Superset terminal session, keep the report as a local file and summarize it directly to the user.

## Inspecting Sessions

List active pty-daemon sessions:

```sh
ssctl sessions
ssctl sessions --json
```

Use `sessions --json` for verification and troubleshooting. Do not use it as a polling loop. `ssctl` does not stream worker output; design worker prompts so completion is communicated by report files and report pointers.

## Closing Sessions

Close a registered role after its report has been captured:

```sh
ssctl close --role <role>
```

Use a dry run first when uncertain:

```sh
ssctl close --role <role> --dry-run
```

The default signal is `SIGHUP`. Supported signals are `SIGHUP`, `SIGINT`, `SIGTERM`, and `SIGKILL`:

```sh
ssctl close --role <role> --signal SIGTERM
```

When closing a registered role, `ssctl` removes that role from the registry only after the pty-daemon confirms the close. Do not delete registry entries by hand.

## Unregistered Session Safety

Normal sends and closes should target registered roles.

Targeting a raw session id is allowed only when necessary:

```sh
ssctl send --session <session-id> --file <message-file>
ssctl close --session <session-id> --dry-run
```

If the session is not registered, `ssctl` requires both explicit force and workspace verification:

```sh
ssctl send --session <session-id> --file <message-file> --force-unregistered-session --workspace <workspace-id>
ssctl close --session <session-id> --force-unregistered-session --workspace <workspace-id>
```

Use forced unregistered operations only after confirming the session is alive and belongs to the intended workspace. Prefer a dry run first. Forced sends are audit-logged in `.ssctl/audit.log` with hashes and sizes, not raw payload text.

## State Files

Know these paths:

- `.ssctl/registry.json`: local role-to-session registry.
- `.ssctl/registry.lock`: transient lock for `spawn`, `send`, `close`, and registry updates.
- `.ssctl/messages/`: snapshots for oversized generated messages.
- `.ssctl/audit.log`: audit records for forced unregistered sends.
- `.agent-results/`: report copies created by `ssctl report`.

These are local runtime artifacts. Do not commit them.

If a registry lock times out, remove `.ssctl/registry.lock` only after confirming no `ssctl` process is still running.

## Troubleshooting

- Superset CLI missing: use `--superset-bin <path>` or install Superset CLI.
- Wrong Superset home: use `--superset-home <path>`.
- Superset host not healthy: start or repair the Superset host outside `ssctl`.
- Role already registered: choose a new role or close the old role after capturing its report.
- Registered session not alive: inspect `ssctl sessions --json`; stale roles are cleaned up during registry operations.
- Workspace mismatch: do not force through it. Re-check the workspace id and target session.
- Payload sanitation failed: remove control characters, avoid embedded bracketed-paste controls, or send a clean file.
