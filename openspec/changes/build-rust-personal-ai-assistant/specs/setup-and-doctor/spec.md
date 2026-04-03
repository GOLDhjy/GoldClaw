## ADDED Requirements

### Requirement: Guided initialization workflow
The system SHALL provide an initialization workflow that guides the user through profile setup, provider selection, secret entry, storage preparation, and optional connector enablement.

#### Scenario: First-time setup
- **WHEN** the user runs the initialization command on a fresh machine
- **THEN** the assistant walks through required setup steps, writes non-secret configuration, stores secrets through the configured secret backend, and reports the resulting profile status

### Requirement: Structured diagnostic checks
The system SHALL provide a doctor command that runs categorized checks for configuration, secrets, provider connectivity, storage, port usage, webhook readiness, and connector health.

#### Scenario: Environment problem is detected
- **WHEN** the doctor command encounters an invalid or missing dependency
- **THEN** it reports the failed check with severity, evidence, probable cause, and a recommended remediation step

### Requirement: Machine-readable diagnostics
The system SHALL support human-readable and machine-readable doctor output so that UI surfaces and automation can consume the same health results.

#### Scenario: Web UI requests health status
- **WHEN** a local Web surface requests current system health
- **THEN** it receives the same structured check results that are available from the command-line doctor command
