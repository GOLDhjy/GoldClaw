use super::*;
use goldclaw_store::{SqliteStore, StoreLayout};
use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

#[tokio::test]
async fn read_tool_rejects_escape() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock went backwards")
        .as_nanos();
    let root = env::temp_dir().join(format!("goldclaw-runtime-{unique}"));
    fs::create_dir_all(&root).expect("create temp root");

    let tool = ReadWorkspaceTool::new(vec![root.clone()]);
    let invocation = ToolInvocation {
        session_id: Uuid::new_v4(),
        tool_name: "read_file".into(),
        source: EnvelopeSource::Cli,
        args: json!({ "path": "../../secret.txt" }),
    };

    let error = tool
        .execute(&invocation)
        .await
        .expect_err("expected read to fail");
    assert!(matches!(
        error,
        GoldClawError::Io(_) | GoldClawError::Unauthorized(_)
    ));

    let _ = fs::remove_dir_all(root);
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
        .submit(Envelope::user("hello", EnvelopeSource::Tui, Some(session.id)))
        .await
        .expect("submit");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    let detail = runtime.load_session(session.id).await.expect("load session");
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
