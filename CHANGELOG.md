# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0] - 2026-06-08

### Added

- Initial `ssctl` Rust CLI for Superset terminal-backed agent orchestration.
- Commands for status inspection, agent listing, session listing, spawning, sending, handoff, and reporting.
- Local role registry with stale-session cleanup, file locking, atomic writes, and audit logging for forced unregistered sends.
- Structured message transport with bracketed paste wrapping and oversized payload pointerization.
- Superset public CLI adapter and terminal-host protocol v2 client for session inspection and writes.

[0.1.0]: https://github.com/fpdy/ssctl/releases/tag/v0.1.0
