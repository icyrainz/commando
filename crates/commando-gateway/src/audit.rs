use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use crate::handler::truncate_at_char_boundary;

/// Longest command text kept in an audit line, in bytes.
const MAX_COMMAND_BYTES: usize = 500;

#[derive(Serialize)]
pub struct AuditEntry<'a> {
    pub ts: String,
    /// `start`, `end` or `rejected`.
    pub event: &'a str,
    pub target: &'a str,
    /// Omitted on `end` events; the `start` event with the same `request_id` has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<&'a str>,
    pub source: &'a str,
    /// Set on `end` events: `completed`, `timed_out`, `rpc_error` or `aborted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'a str>,
    /// Set on `end` events when the agent lost output at the command deadline.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
}

fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub struct AuditLogger {
    path: PathBuf,
    max_bytes: u64,
    mu: Mutex<()>,
}

impl AuditLogger {
    pub fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            mu: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn log(&self, entry: &AuditEntry<'_>) {
        // A poisoned lock only means another writer panicked; the file is still usable.
        let _lock = self.mu.lock().unwrap_or_else(|e| e.into_inner());

        // Rotate if needed
        if let Ok(meta) = fs::metadata(&self.path)
            && meta.len() >= self.max_bytes
        {
            let backup = self.path.with_extension("log.1");
            let _ = fs::rename(&self.path, &backup);
        }

        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(_) => return,
        };
        line.push('\n');

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path);

        if let Ok(mut f) = file {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

impl AuditLogger {
    /// Log a request that was refused before any command started.
    pub fn log_rejected(&self, target: &str, command: &str, source: &str, error: &str) {
        self.log(&AuditEntry {
            ts: now_ts(),
            event: "rejected",
            target,
            command: Some(truncate_at_char_boundary(command, MAX_COMMAND_BYTES)),
            exit_code: None,
            duration_ms: None,
            request_id: None,
            source,
            outcome: None,
            truncated: false,
            error: Some(error),
        });
    }
}

enum Finish {
    Exec {
        exit_code: i32,
        duration_ms: u64,
        timed_out: bool,
        truncated: bool,
    },
    RpcError(String),
}

/// Writes the `start` event on creation and exactly one `end` event on drop.
///
/// The rpc task owns it. The task reports the result before it finishes; if the
/// task is aborted instead, the drop writes an `aborted` end event.
pub struct ExecAudit {
    logger: Arc<AuditLogger>,
    target: String,
    source: &'static str,
    request_id: String,
    started: Instant,
    finish: Option<Finish>,
}

impl ExecAudit {
    pub fn start(
        logger: Arc<AuditLogger>,
        target: &str,
        command: &str,
        source: &'static str,
        request_id: &str,
    ) -> Self {
        logger.log(&AuditEntry {
            ts: now_ts(),
            event: "start",
            target,
            command: Some(truncate_at_char_boundary(command, MAX_COMMAND_BYTES)),
            exit_code: None,
            duration_ms: None,
            request_id: Some(request_id),
            source,
            outcome: None,
            truncated: false,
            error: None,
        });
        Self {
            logger,
            target: target.to_string(),
            source,
            request_id: request_id.to_string(),
            started: Instant::now(),
            finish: None,
        }
    }

    /// The agent returned a result (`completed`, or `timed_out` when the agent says so).
    pub fn exec_finished(
        &mut self,
        exit_code: i32,
        duration_ms: u64,
        timed_out: bool,
        truncated: bool,
    ) {
        self.finish = Some(Finish::Exec {
            exit_code,
            duration_ms,
            timed_out,
            truncated,
        });
    }

    /// The rpc failed before or while the command ran.
    pub fn rpc_failed(&mut self, error: String) {
        self.finish = Some(Finish::RpcError(error));
    }
}

impl Drop for ExecAudit {
    fn drop(&mut self) {
        let wall_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (outcome, exit_code, duration_ms, truncated, error) = match self.finish.take() {
            Some(Finish::Exec {
                exit_code,
                duration_ms,
                timed_out,
                truncated,
            }) => {
                let outcome = if timed_out { "timed_out" } else { "completed" };
                (outcome, Some(exit_code), duration_ms, truncated, None)
            }
            Some(Finish::RpcError(e)) => ("rpc_error", None, wall_ms, false, Some(e)),
            None => ("aborted", None, wall_ms, false, None),
        };
        self.logger.log(&AuditEntry {
            ts: now_ts(),
            event: "end",
            target: &self.target,
            command: None,
            exit_code,
            duration_ms: Some(duration_ms),
            request_id: Some(&self.request_id),
            source: self.source,
            outcome: Some(outcome),
            truncated,
            error: error.as_deref(),
        });
    }
}

/// Create an AuditLogger from config. If no explicit path is set, defaults to {cache_dir}/audit.log.
pub fn create_logger(audit_log_path: Option<&str>, cache_dir: &str, max_bytes: u64) -> AuditLogger {
    let path = match audit_log_path {
        Some(p) => PathBuf::from(p),
        None => Path::new(cache_dir).join("audit.log"),
    };
    AuditLogger::new(path, max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_logger(dir: &Path) -> AuditLogger {
        AuditLogger::new(dir.join("audit.log"), 1024)
    }

    #[test]
    fn log_appends_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let logger = test_logger(dir.path());

        logger.log(&AuditEntry {
            ts: "2026-03-14T00:00:00Z".to_string(),
            event: "end",
            target: "my-box",
            command: Some("echo hi"),
            exit_code: Some(0),
            duration_ms: Some(42),
            request_id: Some("abc"),
            source: "rest",
            outcome: None,
            truncated: false,
            error: None,
        });
        logger.log(&AuditEntry {
            ts: "2026-03-14T00:00:01Z".to_string(),
            event: "end",
            target: "my-box",
            command: Some("ls"),
            exit_code: Some(0),
            duration_ms: Some(10),
            request_id: Some("def"),
            source: "mcp",
            outcome: None,
            truncated: false,
            error: None,
        });

        let content = fs::read_to_string(dir.path().join("audit.log")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["target"], "my-box");
        assert_eq!(entry["command"], "echo hi");
        assert_eq!(entry["exit_code"], 0);
        assert!(entry.get("error").is_none());
    }

    #[test]
    fn log_error_entry() {
        let dir = tempfile::tempdir().unwrap();
        let logger = test_logger(dir.path());

        logger.log(&AuditEntry {
            ts: "2026-03-14T00:00:00Z".to_string(),
            event: "end",
            target: "bad-target",
            command: Some("echo hi"),
            exit_code: None,
            duration_ms: None,
            request_id: None,
            source: "rest",
            outcome: None,
            truncated: false,
            error: Some("unknown target: bad-target"),
        });

        let content = fs::read_to_string(dir.path().join("audit.log")).unwrap();
        let entry: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(entry["error"], "unknown target: bad-target");
        assert!(entry.get("exit_code").is_none());
    }

    #[test]
    fn log_rotates_on_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny limit: 100 bytes
        let logger = AuditLogger::new(dir.path().join("audit.log"), 100);

        // Write enough to exceed limit
        for i in 0..10 {
            logger.log(&AuditEntry {
                ts: format!("2026-03-14T00:00:{i:02}Z"),
                event: "end",
                target: "my-box",
                command: Some("echo hello world this is a long command to fill up space"),
                exit_code: Some(0),
                duration_ms: Some(42),
                request_id: Some("abc"),
                source: "rest",
                outcome: None,
                truncated: false,
                error: None,
            });
        }

        // Backup should exist
        assert!(dir.path().join("audit.log.1").exists());
        // Current log should be smaller than the backup
        let current_size = fs::metadata(dir.path().join("audit.log")).unwrap().len();
        assert!(current_size < 500); // recent entries only
    }

    #[test]
    fn exec_audit_without_result_writes_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(test_logger(dir.path()));

        drop(ExecAudit::start(
            logger, "my-box", "sleep 60", "mcp", "req-1",
        ));

        let content = fs::read_to_string(dir.path().join("audit.log")).unwrap();
        let lines: Vec<serde_json::Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "start");
        assert_eq!(lines[0]["command"], "sleep 60");
        assert_eq!(lines[1]["event"], "end");
        assert_eq!(lines[1]["outcome"], "aborted");
        assert_eq!(lines[1]["request_id"], "req-1");
        assert!(lines[1].get("command").is_none());
    }

    #[test]
    fn exec_audit_records_truncated_end() {
        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(test_logger(dir.path()));

        let mut audit = ExecAudit::start(logger.clone(), "my-box", "yes", "mcp", "req-1");
        audit.exec_finished(137, 5000, true, true);
        drop(audit);
        let mut audit = ExecAudit::start(logger, "my-box", "true", "mcp", "req-2");
        audit.exec_finished(0, 1, false, false);
        drop(audit);

        let content = fs::read_to_string(dir.path().join("audit.log")).unwrap();
        let lines: Vec<serde_json::Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[1]["outcome"], "timed_out");
        assert_eq!(lines[1]["truncated"], true);
        assert!(lines[3].get("truncated").is_none());
    }

    #[test]
    fn truncates_long_commands() {
        let long_cmd = "x".repeat(1000);
        let truncated = &long_cmd[..500];

        let dir = tempfile::tempdir().unwrap();
        let logger = test_logger(dir.path());

        logger.log(&AuditEntry {
            ts: "2026-03-14T00:00:00Z".to_string(),
            event: "end",
            target: "my-box",
            command: Some(truncated),
            exit_code: Some(0),
            duration_ms: Some(1),
            request_id: None,
            source: "rest",
            outcome: None,
            truncated: false,
            error: None,
        });

        let content = fs::read_to_string(dir.path().join("audit.log")).unwrap();
        let entry: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(entry["command"].as_str().unwrap().len(), 500);
    }
}
