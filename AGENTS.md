# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build
cargo build --release

# Test all crates
cargo test --all

# Test a single crate
cargo test -p goldclaw-runtime

# Run a single test by name
cargo test -p goldclaw-runtime session_binding

# Lint
cargo clippy --all

# Format
cargo fmt

# Run the CLI
cargo run --bin goldclaw -- <command>   # init | doctor | start | stop | status
```

## Conventions

### Testing

- Prefer a single `src/tests.rs` per crate for unit tests instead of creating one `*_tests.rs` file per source file.
- Group tests inside that shared test file with focused modules such as `mod sqlite` or `mod migrations` when needed.
- Only split tests into separate files when there is a clear scale or isolation reason, not as the default pattern.

## Architecture

GoldClaw is a local AI assistant daemon with a pluggable, trait-based design. The codebase is a Cargo workspace split into library crates (`crates/`) and application binaries (`apps/`).

### Crate responsibilities

| Crate | Role |
|-------|------|
| `goldclaw-core` | Core traits (`Provider`, `Tool`, `Policy`, `RuntimeHandle`) and shared models (`Envelope`, `Session`, `AssistantEvent`, `GoldClawError`) |
| `goldclaw-config` | TOML config with env-var overrides; `ProjectPaths` resolves platform-specific dirs (config, data, db, cache) |
| `goldclaw-store` | SQLite persistence via `SqliteStore`; schema migrations tracked in `migrations.rs` |
| `goldclaw-runtime` | `InMemoryRuntime` — session lifecycle, message routing, tool dispatch, event broadcasting (tokio broadcast channels); ships built-in `EchoProvider`, `StaticPolicy`, `ReadWorkspaceTool` |
| `goldclaw-gateway` | Axum HTTP server; exposes REST + SSE endpoints; enforces localhost-only binding and CORS |
| `goldclaw-doctor` | Validates config, DB, schema version, gateway reachability; returns structured `DoctorReport` |
| `goldclaw` (app) | CLI (`init`, `doctor`, `start`, `stop`, `status`, `gateway run`); manages a daemonized gateway process and runtime state file (PID) |

### Data flow

1. An external caller (HTTP client or future TUI/web) sends a message to `POST /messages`.
2. The gateway wraps it in an `Envelope` (carries session binding, source, metadata) and hands it to `RuntimeHandle`.
3. `InMemoryRuntime` resolves or creates a session via the binding, appends the message to `SqliteStore`, invokes the `Provider`, authorizes and runs any `Tool` calls via `Policy`, then broadcasts `AssistantEvent`s on a per-session channel.
4. Callers stream events via `GET /sessions/{id}/events` (SSE).

### Session binding

A `ConversationRef` (external identifier, e.g. a Slack thread) binds to exactly one internal `Session`. On the first message from a ref, the runtime creates a new session and records the binding in SQLite. Subsequent messages with the same ref reuse that session.

### Extensibility points

- **Provider**: implement `goldclaw_core::Provider` to replace the built-in echo stub.
- **Tool**: implement `goldclaw_core::Tool`; register with the runtime at startup.
- **Policy**: implement `goldclaw_core::Policy` (e.g. replace `StaticPolicy` with a dynamic one).

### Environment variable overrides

`GOLDCLAW_PROFILE`, `GOLDCLAW_GATEWAY_BIND`, `GOLDCLAW_ALLOWED_ORIGINS`, `GOLDCLAW_READ_ROOTS`

### App stubs

`goldclaw-tui` and `goldclaw-web` are empty placeholders with no logic yet.
