# Changelog

All notable changes to this project will be documented in this file.

## [0.2.0] - 2026-06-08

### Added

- Added `ssctl close` for closing registered roles or explicitly verified
  sessions through the Superset pty-daemon.
- Added close dry-run and JSON output, with support for `SIGHUP`, `SIGINT`,
  `SIGTERM`, and `SIGKILL`.
- Added Superset pty-daemon protocol v2 runtime integration and host DB metadata
  joins for session inspection.
- Added pty-daemon diagnostics to `ssctl status`.

### Changed

- Moved existing-session inspection, sends, spawn verification, and close support
  onto the Superset pty-daemon runtime path.
- Registered roles are removed from `.ssctl/registry.json` only after a close is
  confirmed by the pty-daemon.

## [0.1.0] - 2026-06-08

### Added

- Initial `ssctl` Rust CLI for Superset terminal-backed agent orchestration.
- Commands for status inspection, agent listing, session listing, spawning, sending, handoff, and reporting.
- Local role registry with stale-session cleanup, file locking, atomic writes, and audit logging for forced unregistered sends.
- Structured message transport with bracketed paste wrapping and oversized payload pointerization.
- Superset public CLI adapter and terminal-host protocol v2 client for session inspection and writes.

[0.2.0]: https://github.com/fpdy/ssctl/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/fpdy/ssctl/releases/tag/v0.1.0
