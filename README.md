# ssctl

`ssctl` is a Rust helper CLI for Superset terminal-backed agent orchestration.
It follows the local design in `docs/local/architecture.md`.

Implemented command surface:

- `ssctl status [--json]`
- `ssctl agents list [--json] [--local|--host <host-id>]`
- `ssctl sessions [--json]`
- `ssctl spawn --agent <agent-id> --role <role> --workspace <workspace-id> --prompt <file-or-text> [--json]`
- `ssctl send (--role <role>|--session <session-id>) (--file <path>|--stdin) [--dry-run]`
- `ssctl send --session <session-id> --stdin --force-unregistered-session --workspace <workspace-id>`
- `ssctl handoff --to <role> --file <path>`
- `ssctl report --to <role> --file <path>`

`ssctl` uses Superset's public CLI for public operations and uses the
terminal-host protocol v2 only for existing-session inspection and writes via
`~/.superset/terminal-host.sock` and `~/.superset/terminal-host.token`.

The local registry is stored under `.ssctl/registry.json` with a lock file,
atomic writes, 0600 file permissions, stale-session cleanup, and audit logging
for forced unregistered sends. Oversized inline messages are converted to
pointer messages; `report` stores report copies under `.agent-results/`.
