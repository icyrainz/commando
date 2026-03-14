use serde::Serialize;
use std::collections::BTreeMap;

/// A page of streaming command output.
#[derive(Debug, Clone, Serialize)]
pub struct ExecPage {
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _profile: Option<ProfileData>,
}

/// Timing breakdown for profiling mode.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProfileData {
    /// Ordered map of stage_name -> elapsed_ms
    pub stages: BTreeMap<String, f64>,
    /// Total gateway-side processing time
    pub total_ms: f64,
}

/// Target info for REST API (minimal fields for CLI display).
#[derive(Debug, Clone, Serialize)]
pub struct TargetInfo {
    pub name: String,
    pub status: String,
    pub host: String,
}

/// Full target info for MCP (preserves all existing fields).
#[derive(Debug, Clone, Serialize)]
pub struct TargetInfoFull {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub shell: String,
    pub tags: Vec<String>,
    pub source: String,
    pub status: String,
    pub reachable: String,
    pub has_psk: bool,
}

/// Result of pinging a target.
#[derive(Debug, Clone, Serialize)]
pub struct PingInfo {
    pub target: String,
    pub hostname: String,
    pub uptime_secs: u64,
    pub shell: String,
    pub latency_ms: u64,
    pub version: String,
}

/// Errors from handler core functions.
#[derive(Debug, Clone)]
pub struct HandlerError {
    pub message: String,
    pub is_gateway_error: bool,
}

impl HandlerError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            is_gateway_error: false,
        }
    }
    pub fn gateway(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            is_gateway_error: true,
        }
    }
}
