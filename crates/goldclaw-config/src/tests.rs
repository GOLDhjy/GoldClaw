use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn soul_path_is_under_base_dir() {
    let base = PathBuf::from("/home/user/.goldclaw");
    let paths = ProjectPaths { base: base.clone() };
    assert_eq!(paths.soul_path(), base.join("soul.md"));
}

#[test]
fn overrides_replace_runtime_and_gateway_settings() {
    let config = GoldClawConfig::default().apply_overrides(ConfigOverrides {
        profile: Some("work".into()),
        gateway_bind: Some("127.0.0.1:9999".into()),
        allowed_origins: Some(vec!["http://localhost:3000".into()]),
        read_roots: Some(vec![PathBuf::from("."), PathBuf::from(".")]),
    });

    assert_eq!(config.profile, "work");
    assert_eq!(config.gateway.bind, "127.0.0.1:9999");
    assert_eq!(
        config.gateway.allowed_origins,
        vec!["http://localhost:3000"]
    );
    assert_eq!(config.runtime.read_roots.len(), 2);
}

#[test]
fn connectors_default_to_disabled() {
    let config = GoldClawConfig::default();
    assert!(config.connectors.wecom.is_none());
}

#[test]
fn wecom_settings_default_to_disabled_when_field_is_missing() {
    let raw = r#"
version = 1
profile = "default"

[gateway]
bind = "127.0.0.1:4263"

[runtime]
read_roots = []

[connectors.wecom]
bot_id = "bot-1"
secret = "secret-1"
"#;

    let config: GoldClawConfig = toml::from_str(raw).expect("parse config");
    assert!(!config.connectors.wecom.expect("wecom settings").enabled);
}

#[test]
fn normalize_requires_local_origins_and_existing_roots() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock went backwards")
        .as_nanos();
    let root = env::temp_dir().join(format!("goldclaw-config-{unique}"));
    fs::create_dir_all(&root).expect("create temp dir");

    let config = GoldClawConfig {
        runtime: RuntimeSettings {
            read_roots: vec![root.clone()],
        },
        gateway: GatewaySettings {
            bind: "127.0.0.1:4263".into(),
            allowed_origins: vec!["http://localhost:3000".into()],
        },
        ..GoldClawConfig::default()
    }
    .normalize()
    .expect("config should normalize");

    assert_eq!(
        config.runtime.read_roots,
        vec![fs::canonicalize(root).unwrap()]
    );
}
