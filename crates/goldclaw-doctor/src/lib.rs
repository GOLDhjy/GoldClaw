use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSeverity {
    Info,
    Warning,
    Fatal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthCheckResult {
    pub id: String,
    pub severity: HealthSeverity,
    pub summary: String,
    pub detail: String,
}
