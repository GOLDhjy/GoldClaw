use super::*;
use goldclaw_core::Tool;
use goldclaw_core::{MemoryStore, ProviderOutput, ToolDefinition};
use goldclaw_memory::SqliteMemoryStore;
use goldclaw_store::{SqliteStore, StoreLayout};
use std::{
    env, fs,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tools::{BuiltinTool, ReadWorkspaceTool, UpdateSoulTool};

fn temp_store_layout(unique: u128) -> StoreLayout {
    let database_file = env::temp_dir().join(format!("goldclaw-runtime-{unique}.sqlite3"));
    let backup_dir = env::temp_dir().join(format!("goldclaw-runtime-backups-{unique}"));
    StoreLayout::from_paths(database_file, backup_dir)
}

struct SoulRefreshProvider {
    calls: Mutex<usize>,
}

#[async_trait::async_trait]
impl Provider for SoulRefreshProvider {
    fn name(&self) -> &'static str {
        "soul-refresh"
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
    ) -> Result<ProviderOutput> {
        let mut calls = self.calls.lock().expect("call lock");
        *calls += 1;

        if *calls == 1 {
            return Ok(ProviderOutput::ToolCall {
                id: "call-1".into(),
                name: "update_soul".into(),
                args: serde_json::json!({
                    "content": "# 角色设定\n\nINITIAL SOUL\n\n# 对话风格\n\n- 更冷静。"
                }),
            });
        }

        let system_prompt = messages
            .iter()
            .find(|message| message.role == "system")
            .map(|message| message.content.clone())
            .unwrap_or_default();

        if system_prompt.contains("INITIAL SOUL") && system_prompt.contains("- 更冷静。") {
            Ok(ProviderOutput::Text("updated soul observed".into()))
        } else {
            Err(GoldClawError::Internal(format!(
                "updated soul missing from prompt: {system_prompt}"
            )))
        }
    }
}

#[tokio::test]
async fn read_tool_rejects_nonexistent_path() {
    let tool = ReadWorkspaceTool::new(vec![env::temp_dir()]);
    let invocation = ToolInvocation {
        session_id: Uuid::new_v4(),
        tool_name: "read_file".into(),
        source: EnvelopeSource::Cli,
        args: json!({ "path": "/nonexistent/path/that/does/not/exist.txt" }),
        tool_call_id: "test-call".into(),
    };

    let error = tool
        .execute(&invocation)
        .await
        .expect_err("expected read to fail for nonexistent file");
    assert!(matches!(error, GoldClawError::Io(_)));
}

#[tokio::test]
async fn envelopes_with_same_binding_reuse_session() {
    let runtime = InMemoryRuntime::new(
        Arc::new(StandardMessageBuilder::new(None)),
        Arc::new(EchoProvider),
        Arc::new(StaticPolicy::allow_only(["read_file"])),
        vec![Arc::new(ReadWorkspaceTool::new(vec![
            env::current_dir().unwrap(),
        ]))],
    );

    let mut first = Envelope::user("hello", EnvelopeSource::Connector("feishu".into()), None);
    first.conversation = Some(goldclaw_core::ConversationRef {
        source_instance: Some("bot-main".into()),
        conversation_id: "dm:user_123".into(),
        sender_id: Some("user_123".into()),
        external_message_id: Some("msg-1".into()),
    });

    let mut second = Envelope::user("again", EnvelopeSource::Connector("feishu".into()), None);
    second.conversation = Some(goldclaw_core::ConversationRef {
        source_instance: Some("bot-main".into()),
        conversation_id: "dm:user_123".into(),
        sender_id: Some("user_123".into()),
        external_message_id: Some("msg-2".into()),
    });

    let first_receipt = runtime.submit(first).await.expect("first submit");
    let second_receipt = runtime.submit(second).await.expect("second submit");

    assert_eq!(first_receipt.session_id, second_receipt.session_id);
}

#[tokio::test]
async fn load_session_returns_history_for_chat_sessions() {
    let runtime = InMemoryRuntime::new(
        Arc::new(StandardMessageBuilder::new(None)),
        Arc::new(EchoProvider),
        Arc::new(StaticPolicy::allow_only(["read_file"])),
        vec![Arc::new(ReadWorkspaceTool::new(vec![
            env::current_dir().unwrap(),
        ]))],
    );

    let session = runtime
        .create_session(Some("Chat".into()))
        .await
        .expect("create session");

    runtime
        .submit(Envelope::user(
            "hello",
            EnvelopeSource::Tui,
            Some(session.id),
        ))
        .await
        .expect("submit");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    let detail = runtime
        .load_session(session.id)
        .await
        .expect("load session");
    assert_eq!(detail.session.id, session.id);
    assert_eq!(detail.messages.len(), 2);
    assert_eq!(detail.messages[0].role, MessageRole::User);
    assert_eq!(detail.messages[0].content, "hello");
    assert_eq!(detail.messages[1].role, MessageRole::Assistant);
    assert_eq!(detail.messages[1].content, "echo: hello");
}

#[tokio::test]
async fn sqlite_store_restores_bindings_across_runtime_restart() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock went backwards")
        .as_nanos();
    let database_file = env::temp_dir().join(format!("goldclaw-runtime-{unique}.sqlite3"));
    let backup_dir = env::temp_dir().join(format!("goldclaw-runtime-backups-{unique}"));
    let layout = StoreLayout::from_paths(database_file.clone(), backup_dir.clone());
    let read_root = env::current_dir().expect("workspace dir");

    let first_runtime = InMemoryRuntime::with_store(
        Arc::new(StandardMessageBuilder::new(None)),
        Arc::new(EchoProvider),
        Arc::new(StaticPolicy::allow_only(["read_file"])),
        vec![Arc::new(ReadWorkspaceTool::new(vec![read_root.clone()]))],
        SqliteStore::open(layout.clone()).expect("open sqlite store"),
    )
    .await
    .expect("create runtime with store");

    let mut first = Envelope::user("hello", EnvelopeSource::Connector("feishu".into()), None);
    first.conversation = Some(goldclaw_core::ConversationRef {
        source_instance: Some("bot-main".into()),
        conversation_id: "dm:user_123".into(),
        sender_id: Some("user_123".into()),
        external_message_id: Some("msg-1".into()),
    });

    let first_receipt = first_runtime.submit(first).await.expect("first submit");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    let second_runtime = InMemoryRuntime::with_store(
        Arc::new(StandardMessageBuilder::new(None)),
        Arc::new(EchoProvider),
        Arc::new(StaticPolicy::allow_only(["read_file"])),
        vec![Arc::new(ReadWorkspaceTool::new(vec![read_root]))],
        SqliteStore::open(layout.clone()).expect("reopen sqlite store"),
    )
    .await
    .expect("restore runtime from store");

    let mut second = Envelope::user("again", EnvelopeSource::Connector("feishu".into()), None);
    second.conversation = Some(goldclaw_core::ConversationRef {
        source_instance: Some("bot-main".into()),
        conversation_id: "dm:user_123".into(),
        sender_id: Some("user_123".into()),
        external_message_id: Some("msg-2".into()),
    });

    let second_receipt = second_runtime.submit(second).await.expect("second submit");
    assert_eq!(first_receipt.session_id, second_receipt.session_id);

    let _ = fs::remove_file(database_file);
    let _ = fs::remove_dir_all(backup_dir);
}

#[tokio::test]
async fn create_session_does_not_persist_soul_message() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let soul_path = env::temp_dir().join(format!("goldclaw-soul-{unique}.md"));
    fs::write(&soul_path, "You are a helpful assistant.").unwrap();

    let layout = temp_store_layout(unique + 1);
    let store = SqliteStore::open(layout.clone()).expect("open store");

    let runtime = InMemoryRuntime::with_store_and_memory(
        Arc::new(StandardMessageBuilder::with_soul_path(soul_path.clone())),
        Arc::new(EchoProvider),
        Arc::new(StaticPolicy::allow_only::<Vec<String>, String>(vec![])),
        vec![],
        store,
        None,
        None,
    )
    .await
    .expect("runtime");

    let session = runtime.create_session(None).await.expect("session");
    let detail = runtime.load_session(session.id).await.expect("load");

    assert!(
        detail.messages.is_empty(),
        "soul should be read dynamically instead of being persisted into history"
    );

    let _ = fs::remove_file(soul_path);
    let _ = fs::remove_file(layout.paths().database_file.clone());
    let _ = fs::remove_dir_all(layout.paths().backup_dir.clone());
}

#[tokio::test]
async fn memory_is_saved_after_provider_response() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        + 100;

    let layout = temp_store_layout(unique);
    let store = SqliteStore::open(layout.clone()).expect("open store");
    let memory_store_impl =
        SqliteMemoryStore::open(&layout.paths().database_file).expect("open memory store");
    let memory_store: Arc<dyn MemoryStore> = Arc::new(memory_store_impl.clone());

    let runtime = InMemoryRuntime::with_store_and_memory(
        Arc::new(StandardMessageBuilder::new(None)),
        Arc::new(EchoProvider),
        Arc::new(StaticPolicy::allow_only::<Vec<String>, String>(vec![])),
        vec![],
        store,
        None,
        Some(memory_store),
    )
    .await
    .expect("runtime");

    let session = runtime.create_session(None).await.expect("session");
    runtime
        .submit(Envelope::user(
            "hello memory",
            EnvelopeSource::Cli,
            Some(session.id),
        ))
        .await
        .expect("submit");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Check that a memory chunk was saved.
    let chunks = memory_store_impl
        .recall(goldclaw_core::MemoryQuery {
            text: "hello memory".into(),
            embedding: None,
            limit: 5,
        })
        .await
        .expect("recall");

    assert!(!chunks.is_empty(), "should have saved a memory chunk");
    assert!(chunks[0].content.contains("hello memory"));

    let _ = fs::remove_file(layout.paths().database_file.clone());
    let _ = fs::remove_dir_all(layout.paths().backup_dir.clone());
}

#[tokio::test]
async fn no_soul_message_when_soul_file_missing() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        + 200;

    let layout = temp_store_layout(unique);
    let store = SqliteStore::open(layout.clone()).expect("open store");

    let runtime = InMemoryRuntime::with_store_and_memory(
        Arc::new(StandardMessageBuilder::with_soul_path(PathBuf::from(
            "/nonexistent/soul.md",
        ))),
        Arc::new(EchoProvider),
        Arc::new(StaticPolicy::allow_only::<Vec<String>, String>(vec![])),
        vec![],
        store,
        None,
        None,
    )
    .await
    .expect("runtime");

    let session = runtime.create_session(None).await.expect("session");
    let detail = runtime.load_session(session.id).await.expect("load");
    assert!(
        detail.messages.is_empty(),
        "no system message when soul file missing"
    );

    let _ = fs::remove_file(layout.paths().database_file.clone());
    let _ = fs::remove_dir_all(layout.paths().backup_dir.clone());
}

#[tokio::test]
async fn standard_message_builder_uses_live_soul_instead_of_legacy_snapshot() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        + 300;
    let soul_path = env::temp_dir().join(format!("goldclaw-builder-soul-{unique}.md"));
    fs::write(&soul_path, "# 角色设定\n\nLIVE SOUL").unwrap();

    let builder = StandardMessageBuilder::with_soul_path(soul_path.clone());
    let session_id = Uuid::new_v4();
    let messages = builder.build(&[
        SessionMessage {
            id: Uuid::new_v4(),
            session_id,
            role: MessageRole::System,
            source: EnvelopeSource::System,
            content: "LEGACY SOUL".into(),
            metadata: json!({ "kind": "soul" }),
            created_at: Utc::now(),
        },
        SessionMessage {
            id: Uuid::new_v4(),
            session_id,
            role: MessageRole::User,
            source: EnvelopeSource::Cli,
            content: "hello".into(),
            metadata: json!({}),
            created_at: Utc::now(),
        },
    ]);

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "system");
    assert!(messages[0].content.contains("LIVE SOUL"));
    assert!(!messages[0].content.contains("LEGACY SOUL"));
    assert_eq!(messages[1].role, "user");

    let _ = fs::remove_file(soul_path);
}

#[tokio::test]
async fn provider_can_update_soul_and_see_new_prompt_in_same_turn() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        + 400;
    let soul_path = env::temp_dir().join(format!("goldclaw-runtime-soul-update-{unique}.md"));
    fs::write(
        &soul_path,
        "# 角色设定\n\nINITIAL SOUL\n\n# 对话风格\n\n- 偏热情。\n",
    )
    .unwrap();

    let layout = temp_store_layout(unique);
    let store = SqliteStore::open(layout.clone()).expect("open store");

    let runtime = InMemoryRuntime::with_store_and_memory(
        Arc::new(StandardMessageBuilder::with_soul_path(soul_path.clone())),
        Arc::new(SoulRefreshProvider {
            calls: Mutex::new(0),
        }),
        Arc::new(StaticPolicy::allow_only(["update_soul"])),
        vec![Arc::new(UpdateSoulTool::new(soul_path.clone()))],
        store,
        None,
        None,
    )
    .await
    .expect("runtime");

    let session = runtime.create_session(None).await.expect("session");
    runtime
        .submit(Envelope::user(
            "以后说话更冷静一些",
            EnvelopeSource::Cli,
            Some(session.id),
        ))
        .await
        .expect("submit");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let soul = fs::read_to_string(&soul_path).expect("read soul");
    assert!(soul.contains("INITIAL SOUL"));
    assert!(soul.contains("- 更冷静。"));
    assert!(!soul.contains("- 偏热情。"));

    let detail = runtime.load_session(session.id).await.expect("load");
    // messages: user, assistant (tool call), tool result, assistant (final)
    assert_eq!(detail.messages.len(), 4);
    assert_eq!(detail.messages[1].role, MessageRole::Assistant);
    assert_eq!(
        detail.messages[1]
            .metadata
            .get("kind")
            .and_then(|v| v.as_str()),
        Some("tool_call")
    );
    assert_eq!(detail.messages[2].role, MessageRole::Tool);
    assert_eq!(
        detail.messages[2]
            .metadata
            .get("tool_name")
            .and_then(|value| value.as_str()),
        Some("update_soul")
    );
    assert_eq!(detail.messages[3].role, MessageRole::Assistant);
    assert_eq!(detail.messages[3].content, "updated soul observed");

    let _ = fs::remove_file(soul_path);
    let _ = fs::remove_file(layout.paths().database_file.clone());
    let _ = fs::remove_dir_all(layout.paths().backup_dir.clone());
}

#[tokio::test]
async fn update_soul_tool_writes_and_returns_full_content() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        + 500;
    let soul_path = env::temp_dir().join(format!("goldclaw-update-soul-full-{unique}.md"));
    let original = "# 助手身份\n\n你是 GoldClaw。\n\n# 用户称呼\n\n老板\n";
    fs::write(&soul_path, original).expect("write soul");

    let tool = UpdateSoulTool::new(soul_path.clone());
    let new_content =
        "# 助手身份\n\n你是 GoldClaw。\n\n# 用户称呼\n\n老板\n\n# 对话风格\n\n稳重点。\n";
    let invocation = ToolInvocation {
        session_id: Uuid::new_v4(),
        tool_name: "update_soul".into(),
        source: EnvelopeSource::System,
        args: json!({ "content": new_content }),
        tool_call_id: "test-call".into(),
    };

    let output = tool.execute(&invocation).await.expect("update soul");

    let updated = fs::read_to_string(&soul_path).expect("read updated soul");
    assert_eq!(updated, output.content);
    assert!(updated.contains("# 对话风格\n\n稳重点。"));

    let _ = fs::remove_file(soul_path);
}
