## ADDED Requirements

### Requirement: Unified connector contract
The system SHALL define a shared connector contract for inbound message ingestion, outbound delivery, health checks, credential validation, and capability reporting.

#### Scenario: Connector is added to the system
- **WHEN** a new channel connector is implemented against the shared contract
- **THEN** the gateway can register, start, stop, and inspect that connector without channel-specific runtime changes

### Requirement: Normalized cross-channel message envelopes
The system SHALL normalize inbound channel events into a common message envelope before passing them to the assistant runtime.

#### Scenario: Different channels send user messages
- **WHEN** Feishu and WeCom each deliver a text message from their respective webhook or polling adapters
- **THEN** both messages are transformed into the same internal envelope shape before runtime processing begins

### Requirement: Reliable outbound delivery and isolation
The system SHALL isolate connector failures and support retries or dead-letter handling for outbound message delivery where the channel allows it.

#### Scenario: One connector encounters an outbound failure
- **WHEN** a channel send attempt fails because of an expired token or temporary transport error
- **THEN** the connector reports a failed delivery event, applies the configured retry or disable policy, and does not block unrelated local or channel traffic
