## ADDED Requirements

### Requirement: Consistent session access across surfaces
The system SHALL provide consistent session creation, message submission, response streaming, and history access semantics across CLI, TUI, and Web surfaces.

#### Scenario: User switches surfaces mid-conversation
- **WHEN** the user starts a conversation in TUI and later opens the same session in the Web UI
- **THEN** the Web UI can retrieve the existing session history and continue the conversation without losing prior context

### Requirement: Surface-specific presentation with shared services
The system SHALL allow each surface to present assistant state in a form appropriate to that interface while relying on shared gateway services for business behavior.

#### Scenario: Different surfaces render the same event stream
- **WHEN** the runtime emits a tool call event and a streamed assistant response
- **THEN** the CLI, TUI, and Web clients can render those events in interface-appropriate formats without reimplementing orchestration logic

### Requirement: Capability negotiation for client compatibility
The system SHALL expose surface capability metadata so older or simpler clients can degrade gracefully when new runtime features are unavailable.

#### Scenario: Older client connects to newer daemon
- **WHEN** a client that does not support a newly introduced event type connects to the daemon
- **THEN** the daemon returns capability metadata that allows the client to hide or simplify unsupported behaviors instead of failing unexpectedly
