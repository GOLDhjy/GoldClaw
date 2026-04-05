use super::*;
use goldclaw_config::{ConnectorSettings, GoldClawConfig, WeComSettings};
use std::path::PathBuf;

#[test]
fn report_detects_failures() {
    let report = DoctorReport {
        generated_at: Utc::now(),
        healthy: false,
        checks: vec![fail("config", "bad".into(), "missing".into())],
    };

    assert!(report.has_failures());
}

#[test]
fn wecom_config_check_passes_when_disabled() {
    let config = GoldClawConfig {
        connectors: ConnectorSettings {
            wecom: Some(WeComSettings {
                enabled: false,
                bot_id: "bot-1".into(),
                secret: Some("secret-1".into()),
                ws_url: None,
                scene: None,
                plug_version: None,
            }),
        },
        ..GoldClawConfig::default()
    };

    let check = wecom_connector_config_check(&config);

    assert_eq!(check.status, HealthStatus::Pass);
    assert!(check.summary.contains("未启用"));
}

#[test]
fn wecom_config_check_fails_when_enabled_secret_missing() {
    let config = GoldClawConfig {
        connectors: ConnectorSettings {
            wecom: Some(WeComSettings {
                enabled: true,
                bot_id: "bot-1".into(),
                secret: None,
                ws_url: None,
                scene: None,
                plug_version: None,
            }),
        },
        ..GoldClawConfig::default()
    };

    let check = wecom_connector_config_check(&config);

    assert_eq!(check.status, HealthStatus::Fail);
    assert!(check.detail.contains("secret"));
}

#[test]
fn wecom_runtime_check_warns_when_enabled_but_gateway_is_stopped() {
    let config = GoldClawConfig {
        connectors: ConnectorSettings {
            wecom: Some(WeComSettings {
                enabled: true,
                bot_id: "bot-1".into(),
                secret: Some("secret-1".into()),
                ws_url: None,
                scene: None,
                plug_version: None,
            }),
        },
        ..GoldClawConfig::default()
    };

    let check = wecom_connector_runtime_check_with_state(
        &PathBuf::from("/tmp/connector-wecom-state.json"),
        None,
        config
            .connectors
            .wecom
            .as_ref()
            .map(|settings| settings.enabled)
            .unwrap_or(false),
        false,
    );

    assert_eq!(check.status, HealthStatus::Warn);
    assert!(check.summary.contains("gateway 未运行"));
}
