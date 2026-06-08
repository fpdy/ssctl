# Changelog

All notable changes to this project will be documented in this file.

## [0.2.2] - 2026-06-09

### Fixed

- Fixed parallel `spawn` failures caused by holding `.ssctl/registry.lock`
  while running slow external Superset operations.
- Added in-registry `pendingSpawns` reservations so duplicate role spawns are
  rejected without creating extra Superset sessions.
- Verified Superset CLI session ids against pty-daemon sessions before updating
  the role registry, avoiding unsafe fallback correlation when the CLI reports a
  session id that cannot be found.

### Changed

- Limited `.ssctl/registry.lock` to short registry read-modify-write sections
  for spawn, send, and close flows.
- Updated README and Agent Skill state-file documentation to describe
  `pendingSpawns` and the narrowed registry lock purpose.

## [0.2.1] - 2026-06-08

### Added

- Added the `ssctl` Agent Skill under `skills/ssctl/` so it can be installed
  from this public repository with `npx skills`.

### Changed

- Refined README content and removed links to local-only architecture notes.

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

[0.2.2]: https://github.com/fpdy/ssctl/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/fpdy/ssctl/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/fpdy/ssctl/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/fpdy/ssctl/releases/tag/v0.1.0
