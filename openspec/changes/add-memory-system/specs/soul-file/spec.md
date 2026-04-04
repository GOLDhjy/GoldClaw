## ADDED Requirements

### Requirement: Soul file path resolution
`ProjectPaths` SHALL expose a `soul_path()` method returning `{data_dir}/soul.md` (e.g. `~/.goldclaw/soul.md`).

#### Scenario: Path resolved correctly
- **WHEN** `soul_path()` is called on a `ProjectPaths` instance
- **THEN** the returned path is `{data_dir}/soul.md`

---

### Requirement: Soul file template generation on init
The `goldclaw init` command SHALL generate a `soul.md` template file at `soul_path()` if the file does not already exist.

#### Scenario: First-time init creates soul.md
- **WHEN** `goldclaw init` is run and `soul.md` does not exist
- **THEN** a template `soul.md` is written with placeholder sections for personality, tone, and user context

#### Scenario: Existing soul.md is not overwritten
- **WHEN** `goldclaw init` is run and `soul.md` already exists
- **THEN** the existing file is left unchanged

---

### Requirement: Soul injected as system message on session creation
When a new session is created, `InMemoryRuntime` SHALL read `soul.md` and store its content as a `role: system` message at the start of the session's message history.

#### Scenario: Soul file exists
- **WHEN** a new session is created and `soul.md` exists and is non-empty
- **THEN** the session's first message has `role: system` and `content` equal to the soul file contents

#### Scenario: Soul file does not exist
- **WHEN** a new session is created and `soul.md` does not exist
- **THEN** the session is created without a system message, no error is returned

#### Scenario: Soul file is empty
- **WHEN** a new session is created and `soul.md` exists but is empty
- **THEN** the session is created without a system message, no error is returned

---

### Requirement: Soul read once per session
The soul file SHALL be read at session creation time only. Subsequent messages within the same session SHALL NOT re-read the file.

#### Scenario: Soul not re-read on follow-up message
- **WHEN** a second user message is sent within an existing session
- **THEN** the session message history already contains the system message from creation; the file is not read again
