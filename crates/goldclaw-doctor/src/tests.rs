use super::*;

#[test]
fn report_detects_failures() {
    let report = DoctorReport {
        generated_at: Utc::now(),
        healthy: false,
        checks: vec![fail("config", "bad".into(), "missing".into())],
    };

    assert!(report.has_failures());
}
