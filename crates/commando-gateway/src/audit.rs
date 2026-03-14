use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

#[derive(Serialize)]
pub struct AuditEntry<'a> {
    pub ts: String,
    pub target: &'a str,
    pub command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<&'a str>,
    pub source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
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
        let _lock = self.mu.lock().unwrap();

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
            target: "my-box",
            command: "echo hi",
            exit_code: Some(0),
            duration_ms: Some(42),
            request_id: Some("abc"),
            source: "rest",
            error: None,
        });
        logger.log(&AuditEntry {
            ts: "2026-03-14T00:00:01Z".to_string(),
            target: "my-box",
            command: "ls",
            exit_code: Some(0),
            duration_ms: Some(10),
            request_id: Some("def"),
            source: "mcp",
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
            target: "bad-target",
            command: "echo hi",
            exit_code: None,
            duration_ms: None,
            request_id: None,
            source: "rest",
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
                target: "my-box",
                command: "echo hello world this is a long command to fill up space",
                exit_code: Some(0),
                duration_ms: Some(42),
                request_id: Some("abc"),
                source: "rest",
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
    fn truncates_long_commands() {
        let long_cmd = "x".repeat(1000);
        let truncated = &long_cmd[..500];

        let dir = tempfile::tempdir().unwrap();
        let logger = test_logger(dir.path());

        logger.log(&AuditEntry {
            ts: "2026-03-14T00:00:00Z".to_string(),
            target: "my-box",
            command: truncated,
            exit_code: Some(0),
            duration_ms: Some(1),
            request_id: None,
            source: "rest",
            error: None,
        });

        let content = fs::read_to_string(dir.path().join("audit.log")).unwrap();
        let entry: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(entry["command"].as_str().unwrap().len(), 500);
    }
}
