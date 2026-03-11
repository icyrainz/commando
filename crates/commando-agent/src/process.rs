use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

pub struct ExecOpts {
    pub shell: String,
    pub max_output_bytes: usize,
}

pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub truncated: bool,
}

pub async fn execute(
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    opts: &ExecOpts,
) -> anyhow::Result<ExecResult> {
    let start = Instant::now();
    let timeout_secs = if timeout_secs == 0 { 60 } else { timeout_secs };

    let mut cmd = Command::new(&opts.shell);
    cmd.arg("-c").arg(command);

    // Clean environment — do NOT inherit agent's env
    cmd.env_clear();
    cmd.env("HOME", "/root");
    cmd.env("USER", "root");
    cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    cmd.env("SHELL", &opts.shell);
    cmd.env("LANG", "C.UTF-8");
    cmd.env("TERM", "dumb");
    cmd.env("NO_COLOR", "1");

    // Apply extra env vars (can override anything including PATH)
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    // Working directory
    if !work_dir.is_empty() {
        cmd.current_dir(work_dir);
    }

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Place child in its own process group via setsid()
    // Safety: setsid() is async-signal-safe and appropriate in pre_exec
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let pid = child.id().expect("child has pid");

    let mut stdout_handle = child.stdout.take().unwrap();
    let mut stderr_handle = child.stderr.take().unwrap();

    // Shared buffers: the inner clones are moved into the future; the outer
    // handles remain here so the timeout branch can read partial output after
    // the future is dropped.
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));

    let stdout_buf_clone = stdout_buf.clone();
    let stderr_buf_clone = stderr_buf.clone();

    let read_and_wait = async move {
        let (stdout_res, stderr_res) = tokio::join!(
            async {
                let mut buf = Vec::new();
                let res = stdout_handle.read_to_end(&mut buf).await;
                *stdout_buf_clone.lock().unwrap() = buf;
                res
            },
            async {
                let mut buf = Vec::new();
                let res = stderr_handle.read_to_end(&mut buf).await;
                *stderr_buf_clone.lock().unwrap() = buf;
                res
            },
        );
        stdout_res?;
        stderr_res?;
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>(status)
    };

    let result = timeout(Duration::from_secs(timeout_secs.into()), read_and_wait).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(status)) => {
            let stdout = stdout_buf.lock().unwrap().clone();
            let stderr = stderr_buf.lock().unwrap().clone();
            let exit_code = status.code().unwrap_or(-1);
            let (stdout, stderr, truncated) =
                truncate_output(stdout, stderr, opts.max_output_bytes);
            Ok(ExecResult {
                stdout,
                stderr,
                exit_code,
                duration_ms,
                timed_out: false,
                truncated,
            })
        }
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => {
            // Timeout — kill the process group. child was moved into
            // read_and_wait which is now dropped, so we use libc directly.
            kill_process_group(pid);
            kill_process_group_force(pid);

            // Read whatever partial output was written before the drop.
            let stdout = stdout_buf.lock().unwrap().clone();
            let stderr = stderr_buf.lock().unwrap().clone();
            let (stdout, stderr, truncated) =
                truncate_output(stdout, stderr, opts.max_output_bytes);
            Ok(ExecResult {
                stdout,
                stderr,
                exit_code: 137, // SIGKILL convention
                duration_ms,
                timed_out: true,
                truncated,
            })
        }
    }
}

/// Tail-truncate stdout/stderr to max_bytes. Keeps the LAST max_bytes.
fn truncate_output(
    mut stdout: Vec<u8>,
    mut stderr: Vec<u8>,
    max_bytes: usize,
) -> (Vec<u8>, Vec<u8>, bool) {
    let mut truncated = false;

    if stdout.len() > max_bytes {
        let start = stdout.len() - max_bytes;
        stdout = stdout[start..].to_vec();
        truncated = true;
    }
    if stderr.len() > max_bytes {
        let start = stderr.len() - max_bytes;
        stderr = stderr[start..].to_vec();
        truncated = true;
    }

    (stdout, stderr, truncated)
}

fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

fn kill_process_group_force(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_opts() -> ExecOpts {
        ExecOpts {
            shell: "sh".to_string(),
            max_output_bytes: 131_072,
        }
    }

    #[tokio::test]
    async fn exec_echo() {
        let result = execute(
            "echo hello",
            "",
            60,
            &[],
            &default_opts(),
        ).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "hello");
        assert!(result.stderr.is_empty());
        assert!(!result.timed_out);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn exec_exit_code() {
        let result = execute("exit 42", "", 60, &[], &default_opts()).await.unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn exec_stderr() {
        let result = execute(
            "echo err >&2",
            "",
            60,
            &[],
            &default_opts(),
        ).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&result.stderr).trim(), "err");
    }

    #[tokio::test]
    async fn exec_work_dir() {
        let result = execute("pwd", "/tmp", 60, &[], &default_opts()).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "/tmp");
    }

    #[tokio::test]
    async fn exec_env_vars() {
        let env = [("MY_VAR".to_string(), "hello".to_string())];
        let result = execute(
            "echo $MY_VAR",
            "",
            60,
            &env,
            &default_opts(),
        ).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "hello");
    }

    #[tokio::test]
    async fn exec_clean_env() {
        // The process should NOT inherit the agent's env
        let result = execute(
            "echo ${CARGO_MANIFEST_DIR:-unset}",
            "",
            60,
            &[],
            &default_opts(),
        ).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "unset");
    }

    #[tokio::test]
    async fn exec_timeout() {
        let result = execute("sleep 30", "", 1, &[], &default_opts()).await.unwrap();
        assert!(result.timed_out);
        assert_ne!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn exec_output_truncation() {
        let opts = ExecOpts {
            shell: "sh".to_string(),
            max_output_bytes: 100,
        };
        // Generate more than 100 bytes of output
        let result = execute(
            "yes | head -n 200",
            "",
            60,
            &[],
            &opts,
        ).await.unwrap();
        assert!(result.truncated);
        assert!(result.stdout.len() <= 100);
    }

    #[tokio::test]
    async fn exec_pipes_work() {
        let result = execute(
            "echo 'hello world' | grep hello | wc -l",
            "",
            60,
            &[],
            &default_opts(),
        ).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "1");
    }
}
