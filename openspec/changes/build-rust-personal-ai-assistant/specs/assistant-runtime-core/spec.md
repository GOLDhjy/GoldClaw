## ADDED Requirements

### Requirement: Unified assistant runtime
The system SHALL provide a single runtime that owns conversation sessions, request orchestration, model invocation, tool execution, and event emission for all local surfaces and channel connectors.

#### Scenario: Local surface submits a message
- **WHEN** a CLI, TUI, or Web client sends a user message to the gateway
- **THEN** the runtime creates or resumes the target session, executes the assistant workflow, and emits response events through the shared event stream

### Requirement: Provider and tool abstraction
The system SHALL expose provider and tool interfaces so that model backends and callable tools can be added or replaced without changing client or connector logic.

#### Scenario: Provider implementation changes
- **WHEN** the active model provider is changed from one backend to another in configuration
- **THEN** the runtime continues to accept the same session and tool orchestration requests through the same internal interfaces

### Requirement: Persistent local assistant state
The system SHALL persist session metadata, message history indexes, task queue metadata, and runtime checkpoints in a local state store that survives process restarts.

#### Scenario: Runtime restarts after prior activity
- **WHEN** the gateway process restarts after at least one conversation has been handled
- **THEN** the runtime can recover prior session listings and pending background work from the local store

### Requirement: Source-aware session resolution
The system SHALL resolve inbound messages from different surfaces and connectors into internal session identifiers based on source metadata and conversation context rather than requiring external clients to know internal session IDs.

#### Scenario: Connector message arrives without internal session id
- **WHEN** an inbound channel message includes source metadata and a conversation identifier but no internal session id
- **THEN** the runtime resolves it to an existing internal session for that conversation or creates a new one and records the binding for future messages
