# ssctl

`ssctl` is a Rust helper CLI for orchestrating Superset terminal-backed agent
sessions. It does not replace the Superset CLI; it complements it with local
role-based session tracking, existing-session sends, handoff messages, and
report pointer delivery. It can also close terminal sessions through the
Superset pty-daemon.

For Japanese documentation, see [README.ja.md](README.ja.md).

## Overview

`ssctl` is designed for workflows where Codex or another main thread treats
Superset terminal-backed agents as subagents. Public Superset operations are
delegated to the Superset CLI, while existing-session inspection and writes use
the experimental Superset pty-daemon protocol v2 and Superset host DB metadata.

Detailed design notes are available in
[docs/local/architecture.md](docs/local/architecture.md).

## Requirements

| Requirement | Details |
| --- | --- |
| Superset CLI | By default, `ssctl` uses `~/.superset/bin/superset`. Override it with `--superset-bin <path>`. |
| Superset home | By default, `ssctl` uses `~/.superset`. Override it with `--superset-home <path>`. |
| pty-daemon | Existing-session inspection, writes, and closes require a running Superset host with a pty-daemon manifest and host DB under `~/.superset/host/<organization-id>/`. |
| Rust toolchain | Required when building or running from source. |

## Quick Start

1. Check the Superset CLI, terminal-host diagnostics, and pty-daemon status.

   ```sh
   ssctl status
   ```

2. List available Superset agents.

   ```sh
   ssctl agents list
   ```

3. Spawn an agent session and register it under a role.

   ```sh
   ssctl spawn --agent codex --role worker-a --workspace <workspace-id> --prompt task.md
   ```

4. Inspect active pty-daemon sessions.

   ```sh
   ssctl sessions
   ```

5. Send a follow-up message to the registered role.

   ```sh
   ssctl send --role worker-a --file followup.md
   ```

6. Close the registered session when the role is no longer needed.

   ```sh
   ssctl close --role worker-a
   ```

## Commands

| Command | Purpose | Main options |
| --- | --- | --- |
| `ssctl status` | Inspect Superset CLI, terminal-host diagnostics, and pty-daemon availability. | `--json` |
| `ssctl agents list` | List Superset agents. | `--json`, `--local`, `--host <host-id>` |
| `ssctl sessions` | List active pty-daemon sessions joined with Superset host DB metadata. | `--json` |
| `ssctl spawn` | Start an agent session and register it under a local role. | `--agent <agent-id>`, `--role <role>`, `--workspace <workspace-id>`, `--prompt <file-or-text>`, `--json` |
| `ssctl send` | Send input to a registered role or verified session. | `--role <role>`, `--session <session-id>`, `--file <path>`, `--stdin`, `--dry-run` |
| `ssctl close` | Close a registered role or explicitly verified session. | `--role <role>`, `--session <session-id>`, `--signal <signal>`, `--dry-run`, `--json` |
| `ssctl handoff` | Send a structured handoff message to another role. | `--to <role>`, `--file <path>` |
| `ssctl report` | Save a report copy and send a report pointer message. | `--to <role>`, `--file <path>` |

Forced sends and closes to unregistered sessions require an explicit session and
workspace:

```sh
ssctl send --session <session-id> --stdin --force-unregistered-session --workspace <workspace-id>
ssctl close --session <session-id> --force-unregistered-session --workspace <workspace-id>
```

`ssctl close` defaults to `SIGHUP`. Supported signals are `SIGHUP`, `SIGINT`,
`SIGTERM`, and `SIGKILL`. Closing a registered role removes that role from the
local registry only after the pty-daemon confirms the close.

## State Files

| Path | Purpose |
| --- | --- |
| `.ssctl/registry.json` | Stores local role-to-session mappings. |
| `.ssctl/` | Holds registry-related files, including the lock file. |
| `.agent-results/` | Stores report copies created by `ssctl report`. |
| `~/.superset/host/<organization-id>/pty-daemon-manifest.json` | Describes the pty-daemon socket and supported protocol versions. |
| `~/.superset/host/<organization-id>/host.db` | Superset host DB used to attach workspace and lifecycle metadata to pty sessions. |
| pty-daemon socket from the manifest | Unix socket used for existing-session inspection, writes, and closes. |

The local registry uses atomic writes, `0600` file permissions, stale-session
cleanup, and audit logging for forced unregistered sends.

## Safety Notes

- Public Superset operations use the public Superset CLI.
- The private pty-daemon adapter is limited to existing-session inspection,
  writes, and closes.
- Normal sends and closes target registry-verified sessions only.
- Sending to or closing an unregistered session requires both
  `--force-unregistered-session` and `--workspace <workspace-id>`.
- Oversized inline messages are converted to pointer messages instead of being
  pasted directly into the terminal.
- `report` saves report copies under `.agent-results/` and sends only a pointer
  message to the target role.
- `close --dry-run` resolves and validates the target without sending a close
  request or changing the registry.

## Architecture

The local design is documented in
[docs/local/architecture.md](docs/local/architecture.md). Release notes are
tracked in [CHANGELOG.md](CHANGELOG.md).
