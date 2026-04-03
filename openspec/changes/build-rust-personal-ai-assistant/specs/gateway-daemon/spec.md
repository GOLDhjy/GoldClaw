## ADDED Requirements

### Requirement: Background gateway service
The system SHALL expose a long-lived daemon mode that can start, stop, report status, and continue serving requests while user-facing shells or browser tabs are closed.

#### Scenario: Assistant runs in the background
- **WHEN** the user starts the gateway in daemon mode
- **THEN** the assistant continues to process eligible local or channel-originated work until the service is explicitly stopped or fails health checks

### Requirement: Shared local control plane
The system SHALL provide a local control plane for clients through loopback HTTP/WebSocket APIs and privileged lifecycle IPC commands.

#### Scenario: Multiple local clients connect concurrently
- **WHEN** a Web client and a TUI client connect to the same running daemon
- **THEN** both clients can access the same session index and subscribe to live assistant events without starting duplicate runtimes

### Requirement: Lifecycle-safe foreground fallback
The system SHALL provide a foreground direct mode for development, debugging, and fallback operation when daemon installation or startup is unavailable.

#### Scenario: Daemon mode is unavailable
- **WHEN** service installation fails or the platform does not support the configured service manager
- **THEN** the user can run the assistant in foreground mode with the same core runtime behavior and a clear warning about reduced background capabilities
