use std::future::Future;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Duration;

const FALLBACK_HOME: &str = "/root";
const FALLBACK_USER: &str = "root";

fn current_user_env() -> (PathBuf, String) {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| (u.dir, u.name))
        .unwrap_or_else(|| (PathBuf::from(FALLBACK_HOME), FALLBACK_USER.to_string()))
}

pub struct ExecOpts {
    pub shell: String,
    pub max_output_bytes: usize,
    /// Wait between SIGTERM and SIGKILL when stopping a command.
    pub kill_grace: Duration,
    /// How long to keep reading pipes after the command's shell has exited.
    pub pipe_drain: Duration,
}

pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub truncated: bool,
}

fn build_command(
    command: &str,
    work_dir: &str,
    extra_env: &[(String, String)],
    opts: &ExecOpts,
) -> Command {
    let mut cmd = Command::new(&opts.shell);
    cmd.arg("-c").arg(command);

    // Clean environment — do NOT inherit agent's env
    let (home, user) = current_user_env();
    cmd.env_clear();
    cmd.env("HOME", &home);
    cmd.env("USER", &user);
    cmd.env(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
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
    // Safety: safe to call post-fork in pre_exec as the process is single-threaded at that point
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    cmd
}

pub async fn execute(
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    opts: &ExecOpts,
) -> anyhow::Result<ExecResult> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let outcome = run(
        command,
        work_dir,
        timeout_secs,
        extra_env,
        opts,
        |data, stream| {
            if stream == 0 {
                stdout.extend_from_slice(&data);
            } else {
                stderr.extend_from_slice(&data);
            }
            std::future::ready(Ok(()))
        },
    )
    .await?;
    let (stdout, stderr, truncated) = truncate_output(stdout, stderr, opts.max_output_bytes);
    Ok(ExecResult {
        stdout,
        stderr,
        exit_code: outcome.exit_code,
        duration_ms: outcome.duration_ms,
        timed_out: outcome.timed_out,
        truncated: truncated || outcome.truncated,
    })
}

/// Run a command and hand each output chunk to `on_chunk` (stream 0 = stdout,
/// 1 = stderr). The runner awaits the callback before reading more. If the
/// callback returns an error, the command is killed and the error is returned.
/// Dropping the returned future kills the command's process group.
pub async fn execute_stream<F, Fut>(
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    opts: &ExecOpts,
    on_chunk: F,
) -> anyhow::Result<ExecResult>
where
    F: FnMut(Vec<u8>, u8) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let outcome = run(command, work_dir, timeout_secs, extra_env, opts, on_chunk).await?;
    Ok(ExecResult {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: outcome.exit_code,
        duration_ms: outcome.duration_ms,
        timed_out: outcome.timed_out,
        truncated: outcome.truncated,
    })
}

struct Outcome {
    exit_code: i32,
    duration_ms: u64,
    timed_out: bool,
    /// The command finished but the deadline cut off delivery of its output.
    truncated: bool,
}

/// Owns the command's process group. Dropped while armed (the caller went
/// away), it sends SIGTERM at once and SIGKILL after `grace` from a detached
/// thread, which also reaps the leader. The thread survives a runtime
/// shutdown but not a process exit; under systemd the whole cgroup is stopped
/// by systemd itself.
struct KillGuard {
    pgid: i32,
    grace: Duration,
    armed: bool,
}

impl Drop for KillGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::warn!(pgid = self.pgid, "command killed: caller went away");
        signal_group(self.pgid, libc::SIGTERM);
        let (pgid, grace) = (self.pgid, self.grace);
        let spawned = std::thread::Builder::new()
            .name("kill-grace".into())
            .spawn(move || kill_after_grace(pgid, grace));
        if spawned.is_err() {
            signal_group(self.pgid, libc::SIGKILL);
        }
    }
}

fn signal_group(pgid: i32, sig: i32) {
    unsafe {
        libc::kill(-pgid, sig);
    }
}

fn group_exists(pgid: i32) -> bool {
    unsafe {
        libc::kill(-pgid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Reap the leader if it is a zombie. Only called while the group exists: on
/// Linux a pid cannot be reused while a process group with that id exists. A
/// check-then-act gap of microseconds remains.
fn reap_leader(pgid: i32) {
    unsafe {
        libc::waitpid(pgid, std::ptr::null_mut(), libc::WNOHANG);
    }
}

fn kill_after_grace(pgid: i32, grace: Duration) {
    const POLL: std::time::Duration = std::time::Duration::from_millis(10);
    let deadline = Instant::now() + grace;
    while group_exists(pgid) {
        reap_leader(pgid);
        if Instant::now() >= deadline {
            tracing::warn!(pgid, "command still running after grace period: SIGKILL");
            signal_group(pgid, libc::SIGKILL);
            for _ in 0..100 {
                reap_leader(pgid);
                if !group_exists(pgid) {
                    break;
                }
                std::thread::sleep(POLL);
            }
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// Post-exit cap when the pipe capacity is unknown: the Linux default with
/// 4 KiB pages. Normally `pipe_capacity` gives the real value. More data than
/// a pipe holds after the leader exits can only come from a background child.
/// A child that enlarges its pipe (F_SETPIPE_SZ) after spawn can leave more
/// than the capacity read at spawn, which the cap then cuts.
const POST_EXIT_LIMIT: usize = 64 * 1024;

/// Capacity of the pipe behind `fd`: `F_GETPIPE_SZ` on Linux (16 pages, so
/// 64 KiB to 1 MiB depending on the kernel page size), else `POST_EXIT_LIMIT`.
fn pipe_capacity(fd: std::os::fd::RawFd) -> usize {
    #[cfg(target_os = "linux")]
    {
        let size = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
        if size > 0 {
            return size as usize;
        }
    }
    let _ = fd;
    POST_EXIT_LIMIT
}

/// Bytes per second `discard_pipe` accepts at full speed.
const DISCARD_RATE: usize = 1024 * 1024;

fn discard_in_background<R: tokio::io::AsyncRead + Unpin + Send + 'static>(reader: R) {
    tokio::spawn(discard_pipe(reader));
}

/// Read to EOF and drop the data. In a one-second window that exceeds
/// `DISCARD_RATE`, wait for the window to end before reading more, so a
/// flooding child blocks on the full pipe and costs almost no CPU on the
/// agent's single thread, while a normal daemon is unaffected.
async fn discard_pipe<R: tokio::io::AsyncRead + Unpin>(mut reader: R) {
    const WINDOW: Duration = Duration::from_secs(1);
    let mut buf = [0u8; 8192];
    let mut window_start = tokio::time::Instant::now();
    let mut in_window = 0usize;
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        if window_start.elapsed() >= WINDOW {
            window_start = tokio::time::Instant::now();
            in_window = 0;
        }
        in_window += n;
        if in_window > DISCARD_RATE {
            tokio::time::sleep_until(window_start + WINDOW).await;
        }
    }
}

enum Event {
    Stdout(std::io::Result<usize>),
    Stderr(std::io::Result<usize>),
    Exited(std::io::Result<std::process::ExitStatus>),
    DrainBudgetSpent,
}

async fn run<F, Fut>(
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    opts: &ExecOpts,
    mut on_chunk: F,
) -> anyhow::Result<Outcome>
where
    F: FnMut(Vec<u8>, u8) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let start = Instant::now();
    let timeout_secs = if timeout_secs == 0 { 60 } else { timeout_secs };

    let mut cmd = build_command(command, work_dir, extra_env, opts);
    let mut child = cmd.spawn()?;
    let pgid = child.id().expect("child has pid") as i32;
    let mut guard = KillGuard {
        pgid,
        grace: opts.kill_grace,
        armed: true,
    };

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_limit = pipe_capacity(stdout.as_raw_fd());
    let err_limit = pipe_capacity(stderr.as_raw_fd());
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let mut out_open = true;
    let mut err_open = true;
    let mut out_eof = false;
    let mut err_eof = false;
    let mut out_after_exit = 0usize;
    let mut err_after_exit = 0usize;
    let mut status = None;

    // After the leader exits, the loop may wait for pipe data for `pipe_drain`
    // in total (the callback is not counted). Poll order is exit, reads,
    // budget: exit detection must not starve behind a background flood, and
    // reads that are ready win over the cutoff, so the leader's own output is
    // delivered in full. The two streams take turns being polled first. Each
    // stream is also capped at its pipe capacity since the exit.
    let mut drain_left = opts.pipe_drain;
    let mut delivering = false;
    let mut out_first = true;
    let phase = async {
        loop {
            if status.is_some() && !out_open && !err_open {
                break;
            }
            let post_exit = status.is_some();
            let waited = Instant::now();
            out_first = !out_first;
            let reads = async {
                if out_first {
                    tokio::select! {
                        biased;
                        r = stdout.read(&mut out_buf), if out_open => Event::Stdout(r),
                        r = stderr.read(&mut err_buf), if err_open => Event::Stderr(r),
                    }
                } else {
                    tokio::select! {
                        biased;
                        r = stderr.read(&mut err_buf), if err_open => Event::Stderr(r),
                        r = stdout.read(&mut out_buf), if out_open => Event::Stdout(r),
                    }
                }
            };
            let event = tokio::select! {
                biased;
                r = child.wait(), if !post_exit => Event::Exited(r),
                event = reads, if out_open || err_open => event,
                _ = tokio::time::sleep(drain_left), if post_exit => Event::DrainBudgetSpent,
            };
            if post_exit {
                drain_left = drain_left.saturating_sub(waited.elapsed());
            }
            match event {
                Event::Stdout(Ok(n)) if n > 0 => {
                    delivering = true;
                    on_chunk(out_buf[..n].to_vec(), 0).await?;
                    delivering = false;
                    if status.is_some() {
                        out_after_exit += n;
                        out_open = out_after_exit < out_limit;
                    }
                }
                Event::Stdout(_) => {
                    out_open = false;
                    out_eof = true;
                }
                Event::Stderr(Ok(n)) if n > 0 => {
                    delivering = true;
                    on_chunk(err_buf[..n].to_vec(), 1).await?;
                    delivering = false;
                    if status.is_some() {
                        err_after_exit += n;
                        err_open = err_after_exit < err_limit;
                    }
                }
                Event::Stderr(_) => {
                    err_open = false;
                    err_eof = true;
                }
                Event::Exited(r) => {
                    status = Some(r?);
                    // Leader exited normally: background children may live on.
                    guard.armed = false;
                }
                Event::DrainBudgetSpent => break,
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs.into());
    let result = tokio::time::timeout_at(deadline, phase).await;

    // A timeout or a callback error may come before the leader's exit was
    // observed (the callback delays the wait): check whether the command had
    // already finished.
    if !matches!(result, Ok(Ok(())))
        && status.is_none()
        && let Ok(Some(exited)) = child.try_wait()
    {
        status = Some(exited);
        guard.armed = false;
    }

    // A background child may still write: keep its pipes open and discard the
    // data, so it is not killed by SIGPIPE when this call returns (on every
    // return path once the leader has exited).
    if status.is_some() {
        if !out_eof {
            discard_in_background(stdout);
        }
        if !err_eof {
            discard_in_background(stderr);
        }
    }

    let mut truncated = false;
    let (exit_code, timed_out) = match result {
        Ok(Ok(())) => (status.and_then(|s| s.code()).unwrap_or(-1), false),
        Ok(Err(e)) => return Err(e),
        // The command finished; the deadline hit while draining its pipes.
        Err(_elapsed) if status.is_some() => {
            if delivering {
                tracing::warn!(pgid, "output delivery incomplete at deadline");
                truncated = true;
            }
            (status.and_then(|s| s.code()).unwrap_or(-1), false)
        }
        Err(_elapsed) => {
            if guard.armed {
                signal_group(pgid, libc::SIGTERM);
                let _ = tokio::time::timeout(opts.kill_grace, child.wait()).await;
                signal_group(pgid, libc::SIGKILL);
                let _ = child.wait().await;
                guard.armed = false;
            }
            (137, true) // SIGKILL convention
        }
    };

    Ok(Outcome {
        exit_code,
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
        truncated,
    })
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
        stdout.drain(0..start);
        truncated = true;
    }
    if stderr.len() > max_bytes {
        let start = stderr.len() - max_bytes;
        stderr.drain(0..start);
        truncated = true;
    }

    (stdout, stderr, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_opts() -> ExecOpts {
        ExecOpts {
            shell: "sh".to_string(),
            max_output_bytes: 131_072,
            kill_grace: Duration::from_millis(100),
            pipe_drain: Duration::from_millis(100),
        }
    }

    /// Kills the process group in `Drop` so a failed assertion cannot leak it.
    struct KillOnDrop(i32);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            signal_group(self.0, libc::SIGKILL);
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
            }
        }
    }

    fn temp_pid_file() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "commando-process-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn wait_for_pid(path: &std::path::Path) -> i32 {
        for _ in 0..500 {
            if let Ok(s) = std::fs::read_to_string(path)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("pid file never appeared");
    }

    /// Poll until the group is gone or `within` elapses.
    async fn group_gone_within(pgid: i32, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if !group_exists(pgid) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Start `command` via `execute_stream`, drop the future once it has
    /// written its pid, and check the group is gone within grace + 500 ms.
    async fn assert_killed_on_drop(command: &str) {
        let pid_file = temp_pid_file();
        let command = command.replace("PIDFILE", pid_file.to_str().unwrap());
        let opts = default_opts();
        let _kill;
        {
            let run = execute_stream(&command, "", 60, &[], &opts, |_, _| {
                std::future::ready(Ok(()))
            });
            tokio::select! {
                _ = run => panic!("command should still be running"),
                pid = wait_for_pid(&pid_file) => _kill = KillOnDrop(pid),
            }
        }
        let within = opts.kill_grace + Duration::from_millis(500);
        assert!(group_gone_within(_kill.0, within).await);
    }

    #[tokio::test]
    async fn exec_echo() {
        let result = execute("echo hello", "", 60, &[], &default_opts())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "hello");
        assert!(result.stderr.is_empty());
        assert!(!result.timed_out);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn exec_exit_code() {
        let result = execute("exit 42", "", 60, &[], &default_opts())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn exec_stderr() {
        let result = execute("echo err >&2", "", 60, &[], &default_opts())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&result.stderr).trim(), "err");
    }

    #[tokio::test]
    async fn exec_work_dir() {
        let result = execute("pwd", "/tmp", 60, &[], &default_opts())
            .await
            .unwrap();
        let output = String::from_utf8_lossy(&result.stdout).trim().to_string();
        // macOS: /tmp is a symlink to /private/tmp
        assert!(output == "/tmp" || output == "/private/tmp");
    }

    #[tokio::test]
    async fn exec_env_vars() {
        let env = [("MY_VAR".to_string(), "hello".to_string())];
        let result = execute("echo $MY_VAR", "", 60, &env, &default_opts())
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "hello");
    }

    #[tokio::test]
    async fn exec_env_home_and_user_match_current_uid() {
        let (expected_home, expected_user) = current_user_env();
        let result = execute("echo $HOME:$USER", "", 60, &[], &default_opts())
            .await
            .unwrap();
        let output = String::from_utf8_lossy(&result.stdout).trim().to_string();
        assert_eq!(
            output,
            format!("{}:{}", expected_home.display(), expected_user),
        );
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
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "unset");
    }

    #[tokio::test]
    async fn exec_timeout() {
        let result = execute("sleep 30", "", 1, &[], &default_opts())
            .await
            .unwrap();
        assert!(result.timed_out);
        assert_eq!(result.exit_code, 137);
    }

    #[tokio::test]
    async fn exec_output_truncation() {
        let opts = ExecOpts {
            shell: "sh".to_string(),
            max_output_bytes: 100,
            ..default_opts()
        };
        // Generate more than 100 bytes of output
        let result = execute("yes | head -n 200", "", 60, &[], &opts)
            .await
            .unwrap();
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
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "1");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exec_stream_echo() {
        let mut received = Vec::<u8>::new();
        let result = execute_stream("echo hello", "", 60, &[], &default_opts(), |data, s| {
            if s == 0 {
                received.extend_from_slice(&data);
            }
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert!(!result.timed_out);
        assert!(!result.truncated);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
        assert_eq!(String::from_utf8_lossy(&received).as_ref(), "hello\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exec_stream_stderr() {
        let mut received = Vec::<u8>::new();
        let result = execute_stream("echo err >&2", "", 60, &[], &default_opts(), |data, s| {
            if s == 1 {
                received.extend_from_slice(&data);
            }
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert!(!result.timed_out);
        assert_eq!(String::from_utf8_lossy(&received).as_ref(), "err\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exec_stream_timeout() {
        let mut received = Vec::<u8>::new();
        // Print something before sleeping so we can verify partial output arrives
        let result = execute_stream(
            "echo partial; sleep 30",
            "",
            1,
            &[],
            &default_opts(),
            |data, s| {
                if s == 0 {
                    received.extend_from_slice(&data);
                }
                std::future::ready(Ok(()))
            },
        )
        .await
        .unwrap();

        assert!(result.timed_out);
        assert_eq!(result.exit_code, 137);
        assert_eq!(String::from_utf8_lossy(&received).trim(), "partial");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_returns_without_unconditional_grace() {
        let opts = ExecOpts {
            kill_grace: Duration::from_secs(5),
            ..default_opts()
        };
        let started = Instant::now();
        let result = execute("sleep 30", "", 1, &[], &opts).await.unwrap();
        assert!(result.timed_out);
        assert_eq!(result.exit_code, 137);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_kills_running_command() {
        assert_killed_on_drop("echo $$ > PIDFILE; exec sleep 30").await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_kills_command_that_ignores_sigterm() {
        assert_killed_on_drop("trap '' TERM; echo $$ > PIDFILE; exec sleep 30").await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_kills_command_that_ignores_sigterm() {
        let pid_file = temp_pid_file();
        let command = format!(
            "trap '' TERM; echo $$ > {}; exec sleep 30",
            pid_file.display()
        );
        let started = Instant::now();
        let result = execute(&command, "", 1, &[], &default_opts()).await;
        let pid = wait_for_pid(&pid_file).await;
        let _kill = KillOnDrop(pid);
        let result = result.unwrap();
        assert!(result.timed_out);
        assert_eq!(result.exit_code, 137);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(group_gone_within(pid, Duration::from_millis(500)).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normal_exit_keeps_background_child() {
        let started = Instant::now();
        let result = execute(
            "sleep 5 >/dev/null 2>&1 & echo $!",
            "",
            60,
            &[],
            &default_opts(),
        )
        .await
        .unwrap();
        let pid: i32 = String::from_utf8_lossy(&result.stdout)
            .trim()
            .parse()
            .unwrap();
        let _kill = KillOnDrop(pid);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(pid_alive(pid), "background child must survive");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_child_holding_pipe_does_not_block() {
        let pid_file = temp_pid_file();
        let command = format!("sleep 3 & echo $! > {}; echo started", pid_file.display());
        let started = Instant::now();
        let result = execute(&command, "", 60, &[], &default_opts()).await;
        let elapsed = started.elapsed();
        let _kill = KillOnDrop(wait_for_pid(&pid_file).await);
        let result = result.unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert_eq!(result.stdout, b"started\n");
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runner_does_not_read_ahead_of_callback() {
        let active = std::cell::Cell::new(0u32);
        let overlaps = std::cell::Cell::new(0u32);
        let calls = std::cell::Cell::new(0u32);
        let result = execute_stream(
            "echo a; echo e >&2; sleep 0.05; echo b",
            "",
            60,
            &[],
            &default_opts(),
            |_, _| {
                if active.get() > 0 {
                    overlaps.set(overlaps.get() + 1);
                }
                active.set(active.get() + 1);
                calls.set(calls.get() + 1);
                let active = &active;
                async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    active.set(active.get() - 1);
                    Ok(())
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(calls.get() >= 2);
        assert_eq!(overlaps.get(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn callback_error_kills_command() {
        let pid_file = temp_pid_file();
        let command = format!("echo $$ > {}; echo x; exec sleep 30", pid_file.display());
        let result = execute_stream(&command, "", 60, &[], &default_opts(), |_, _| {
            std::future::ready(Err(anyhow::anyhow!("receiver gone")))
        })
        .await;
        let pid = wait_for_pid(&pid_file).await;
        let _kill = KillOnDrop(pid);
        assert!(result.is_err());
        assert!(group_gone_within(pid, Duration::from_millis(600)).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_keeps_leader_output_with_slow_consumer() {
        let dir_pid = temp_pid_file();
        let command = format!(
            "sleep 5 & echo $! > {}; head -c 60000 /dev/zero | tr '\\0' x",
            dir_pid.display()
        );
        let mut total = 0usize;
        let result = execute_stream(&command, "", 60, &[], &default_opts(), |data, _| {
            total += data.len();
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(())
            }
        })
        .await;
        let _kill = KillOnDrop(wait_for_pid(&dir_pid).await);
        result.unwrap();
        assert_eq!(total, 60_000);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_child_is_not_killed_by_sigpipe() {
        let marker = temp_pid_file();
        let command = format!(
            "(sleep 0.4; echo a; sleep 0.4; echo b; touch {}) & echo started",
            marker.display()
        );
        let started = Instant::now();
        let result = execute(&command, "", 60, &[], &default_opts())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(result.stdout, b"started\n");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let exists = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(exists, "background child died before finishing");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_during_drain_reports_real_exit_code() {
        // The leader exits at once; a callback that never returns keeps the
        // output undelivered until the 1 s timeout.
        let pid_file = temp_pid_file();
        let command = format!("echo $$ > {}; echo hi; exit 3", pid_file.display());
        let result = execute_stream(&command, "", 1, &[], &default_opts(), |_, _| {
            std::future::pending::<anyhow::Result<()>>()
        })
        .await;
        let _kill = KillOnDrop(wait_for_pid(&pid_file).await);
        let result = result.unwrap();
        assert!(!result.timed_out);
        assert_eq!(result.exit_code, 3);
        assert!(result.truncated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_background_writer_does_not_hold_the_call() {
        let pid_file = temp_pid_file();
        let command = format!(
            "(while :; do echo x; sleep 0.05; done) & echo $$ > {}; exit 7",
            pid_file.display()
        );
        let started = Instant::now();
        let result = execute(&command, "", 30, &[], &default_opts()).await;
        let elapsed = started.elapsed();
        let _kill = KillOnDrop(wait_for_pid(&pid_file).await);
        let result = result.unwrap();
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
        assert_eq!(result.exit_code, 7);
        assert!(!result.timed_out);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn busy_background_stdout_with_silent_stderr_does_not_hold_the_call() {
        let pid_file = temp_pid_file();
        let command = format!("yes & echo $$ > {}; exit 0", pid_file.display());
        let started = Instant::now();
        let result = execute(&command, "", 30, &[], &default_opts()).await;
        let elapsed = started.elapsed();
        let _kill = KillOnDrop(wait_for_pid(&pid_file).await);
        let result = result.unwrap();
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discard_pipe_throttles_a_flood() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let discard = tokio::spawn(discard_pipe(reader));
        let chunk = [0u8; 8192];
        let mut written = 0usize;
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            written += writer.write(&chunk).await.unwrap();
        }
        drop(writer);
        discard.abort();
        assert!(written < 2 * 1024 * 1024, "wrote {written} bytes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discard_pipe_rate_is_not_a_lifetime_total() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let discard = tokio::spawn(discard_pipe(reader));
        let block = vec![0u8; 900 * 1024];
        writer.write_all(&block).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let started = Instant::now();
        writer.write_all(&block).await.unwrap();
        let elapsed = started.elapsed();
        discard.abort();
        assert!(elapsed < Duration::from_millis(300), "took {elapsed:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exit_is_observed_behind_a_background_flood() {
        let pid_file = temp_pid_file();
        let command = format!("yes & echo $$ > {}; sleep 0.5; exit 7", pid_file.display());
        let started = Instant::now();
        let result = execute_stream(&command, "", 4, &[], &default_opts(), |_, _| async {
            tokio::time::sleep(Duration::from_micros(200)).await;
            Ok(())
        })
        .await;
        let elapsed = started.elapsed();
        let _kill = KillOnDrop(wait_for_pid(&pid_file).await);
        let result = result.unwrap();
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        assert_eq!(result.exit_code, 7);
        assert!(!result.timed_out);
        assert!(!result.truncated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deadline_while_waiting_out_drain_is_not_truncated() {
        let pid_file = temp_pid_file();
        let command = format!("sleep 30 & echo $$ > {}; exit 3", pid_file.display());
        let opts = ExecOpts {
            pipe_drain: Duration::from_secs(3),
            ..default_opts()
        };
        let result = execute(&command, "", 1, &[], &opts).await;
        let _kill = KillOnDrop(wait_for_pid(&pid_file).await);
        let result = result.unwrap();
        assert_eq!(result.exit_code, 3);
        assert!(!result.timed_out);
        assert!(!result.truncated);
    }

    #[test]
    fn pipe_capacity_of_a_real_pipe() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let capacity = pipe_capacity(fds[0]);
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        #[cfg(target_os = "linux")]
        assert!(capacity >= 4096);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(capacity, 64 * 1024);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn callback_error_after_exit_keeps_background_child_alive() {
        let marker = temp_pid_file();
        let command = format!("(sleep 0.4; echo a; touch {}) & echo hi", marker.display());
        let result = execute_stream(&command, "", 60, &[], &default_opts(), |_, _| async {
            // Fail after the leader has exited.
            tokio::time::sleep(Duration::from_millis(200)).await;
            Err(anyhow::anyhow!("receiver gone"))
        })
        .await;
        assert!(result.is_err());
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let exists = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(exists, "background child was killed by SIGPIPE");
    }
}
