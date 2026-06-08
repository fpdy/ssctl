# ssctl

`ssctl` is a Rust helper CLI for orchestrating Superset terminal-backed agent
sessions. It does not replace the Superset CLI; it complements it with local
role-based session tracking, existing-session sends, handoff messages, and
report pointer delivery.

For Japanese documentation, see [README.ja.md](README.ja.md).

## Overview

`ssctl` is designed for workflows where Codex or another main thread treats
Superset terminal-backed agents as subagents. Public Superset operations are
delegated to the Superset CLI, while existing-session inspection and writes use
the experimental terminal-host protocol v2.

Detailed design notes are available in
[docs/local/architecture.md](docs/local/architecture.md).

## Requirements

| Requirement | Details |
| --- | --- |
| Superset CLI | By default, `ssctl` uses `~/.superset/bin/superset`. Override it with `--superset-bin <path>`. |
| Superset home | By default, `ssctl` uses `~/.superset`. Override it with `--superset-home <path>`. |
| terminal-host | Existing-session inspection and writes require `~/.superset/terminal-host.sock` and `~/.superset/terminal-host.token`. |
| Rust toolchain | Required when building or running from source. |

## Quick Start

1. Check the Superset CLI and terminal-host status.

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

4. Inspect active terminal-host sessions.

   ```sh
   ssctl sessions
   ```

5. Send a follow-up message to the registered role.

   ```sh
   ssctl send --role worker-a --file followup.md
   ```

## Commands

| Command | Purpose | Main options |
| --- | --- | --- |
| `ssctl status` | Inspect Superset CLI and terminal-host availability. | `--json` |
| `ssctl agents list` | List Superset agents. | `--json`, `--local`, `--host <host-id>` |
| `ssctl sessions` | List terminal-host sessions. | `--json` |
| `ssctl spawn` | Start an agent session and register it under a local role. | `--agent <agent-id>`, `--role <role>`, `--workspace <workspace-id>`, `--prompt <file-or-text>`, `--json` |
| `ssctl send` | Send input to a registered role or verified session. | `--role <role>`, `--session <session-id>`, `--file <path>`, `--stdin`, `--dry-run` |
| `ssctl handoff` | Send a structured handoff message to another role. | `--to <role>`, `--file <path>` |
| `ssctl report` | Save a report copy and send a report pointer message. | `--to <role>`, `--file <path>` |

Forced sends to unregistered sessions require an explicit session and
workspace:

```sh
ssctl send --session <session-id> --stdin --force-unregistered-session --workspace <workspace-id>
```

## State Files

| Path | Purpose |
| --- | --- |
| `.ssctl/registry.json` | Stores local role-to-session mappings. |
| `.ssctl/` | Holds registry-related files, including the lock file. |
| `.agent-results/` | Stores report copies created by `ssctl report`. |
| `~/.superset/terminal-host.sock` | terminal-host protocol v2 Unix socket used for existing-session operations. |
| `~/.superset/terminal-host.token` | Authentication token for terminal-host protocol v2. |

The local registry uses atomic writes, `0600` file permissions, stale-session
cleanup, and audit logging for forced unregistered sends.

## Safety Notes

- Public Superset operations use the public Superset CLI.
- The private terminal-host adapter is limited to existing-session inspection
  and writes.
- Normal sends target registry-verified sessions only.
- Sending to an unregistered session requires both
  `--force-unregistered-session` and `--workspace <workspace-id>`.
- Oversized inline messages are converted to pointer messages instead of being
  pasted directly into the terminal.
- `report` saves report copies under `.agent-results/` and sends only a pointer
  message to the target role.

## Architecture

The local design is documented in
[docs/local/architecture.md](docs/local/architecture.md). Release notes are
tracked in [CHANGELOG.md](CHANGELOG.md).
