use std::process::Stdio;
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

    // Read stdout and stderr concurrently, with a timeout.
    // child is NOT moved into this future so we retain ownership for cleanup.
    let io_future = async {
        let (stdout_res, stderr_res) = tokio::join!(
            async {
                let mut buf = Vec::new();
                stdout_handle.read_to_end(&mut buf).await.map(|_| buf)
            },
            async {
                let mut buf = Vec::new();
                stderr_handle.read_to_end(&mut buf).await.map(|_| buf)
            },
        );
        match (stdout_res, stderr_res) {
            (Ok(out), Ok(err)) => Ok::<_, std::io::Error>((out, err)),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    };

    match timeout(Duration::from_secs(timeout_secs.into()), io_future).await {
        Ok(Ok((stdout_raw, stderr_raw))) => {
            // I/O completed within timeout; reap the child
            let status = child.wait().await?;
            let duration_ms = start.elapsed().as_millis() as u64;
            let exit_code = status.code().unwrap_or(-1);
            let (stdout, stderr, truncated) =
                truncate_output(stdout_raw, stderr_raw, opts.max_output_bytes);
            Ok(ExecResult {
                stdout,
                stderr,
                exit_code,
                duration_ms,
                timed_out: false,
                truncated,
            })
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_elapsed) => {
            // Timeout — kill the process group (SIGTERM first)
            kill_process_group(pid);

            // Grace period: wait up to 5s for SIGTERM to take effect
            let grace = timeout(Duration::from_secs(5), child.wait()).await;
            if grace.is_err() {
                // Still alive after 5s, force-kill
                kill_process_group_force(pid);
                let _ = child.wait().await;
            }

            let duration_ms = start.elapsed().as_millis() as u64;

            // Output was not captured (io_future was dropped on timeout);
            // return empty buffers marked as timed_out.
            Ok(ExecResult {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 137, // SIGKILL convention
                duration_ms,
                timed_out: true,
                truncated: false,
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
