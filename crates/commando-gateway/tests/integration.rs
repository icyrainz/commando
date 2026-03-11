//! Integration tests: spin up a real agent, connect with gateway RPC client,
//! and verify the full auth → exec / ping flow over TCP.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use tokio::net::TcpListener;

use commando_agent::config::AgentConfig;
use commando_agent::rpc::{self as agent_rpc, ConcurrencyGuard};
use commando_gateway::rpc::{remote_exec, remote_ping};

const TEST_PSK: &str = "integration-test-secret";

fn test_agent_config() -> AgentConfig {
    AgentConfig {
        bind: "127.0.0.1".to_string(),
        port: 0, // unused — we bind separately
        shell: "sh".to_string(),
        psk: TEST_PSK.to_string(),
        max_output_bytes: 131_072,
        max_concurrent: 8,
    }
}

/// Start an agent server on a random port, returning the port number.
/// The server runs on a spawned LocalSet task and handles one connection.
async fn start_agent() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let config = Rc::new(test_agent_config());
    let rate_limits = Rc::new(RefCell::new(HashMap::new()));
    let concurrency_guard = Rc::new(ConcurrencyGuard::new(config.max_concurrent));
    let agent_start_time = Instant::now();

    tokio::task::spawn_local(async move {
        // Accept a single connection for this test
        let (stream, peer_addr) = listener.accept().await.unwrap();

        let _ = agent_rpc::handle_connection(
            stream,
            peer_addr.ip(),
            config,
            rate_limits,
            concurrency_guard,
            agent_start_time,
        )
        .await;
    });

    port
}

/// Start an agent that accepts multiple connections (for multi-request tests).
async fn start_agent_multi(max_conns: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let config = Rc::new(test_agent_config());
    let rate_limits = Rc::new(RefCell::new(HashMap::new()));
    let concurrency_guard = Rc::new(ConcurrencyGuard::new(config.max_concurrent));
    let agent_start_time = Instant::now();

    tokio::task::spawn_local(async move {
        for _ in 0..max_conns {
            let (stream, peer_addr) = listener.accept().await.unwrap();
            let config = config.clone();
            let rate_limits = rate_limits.clone();
            let concurrency_guard = concurrency_guard.clone();

            tokio::task::spawn_local(async move {
                let _ = agent_rpc::handle_connection(
                    stream,
                    peer_addr.ip(),
                    config,
                    rate_limits,
                    concurrency_guard,
                    agent_start_time,
                )
                .await;
            });
        }
    });

    port
}

/// Helper to run a test inside a current_thread runtime + LocalSet,
/// since capnp-rpc types are !Send.
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, f);
}

// ─── E2E: Auth + Exec ─────────────────────────────────────────────

#[test]
fn e2e_exec_echo() {
    run_local(async {
        let port = start_agent().await;

        let result = remote_exec(
            "127.0.0.1",
            port,
            TEST_PSK,
            "echo hello",
            "",
            60,
            &[],
            "test-req-1",
            5,
        )
        .await
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&result.stdout).trim(),
            "hello"
        );
        assert!(!result.timed_out);
        assert!(!result.truncated);
        assert_eq!(result.request_id, "test-req-1");
    });
}

#[test]
fn e2e_exec_exit_code() {
    run_local(async {
        let port = start_agent().await;

        let result = remote_exec(
            "127.0.0.1",
            port,
            TEST_PSK,
            "exit 42",
            "",
            60,
            &[],
            "test-exit",
            5,
        )
        .await
        .unwrap();

        assert_eq!(result.exit_code, 42);
    });
}

#[test]
fn e2e_exec_with_env() {
    run_local(async {
        let port = start_agent().await;

        let env = vec![("MY_TEST_VAR".to_string(), "works".to_string())];
        let result = remote_exec(
            "127.0.0.1",
            port,
            TEST_PSK,
            "echo $MY_TEST_VAR",
            "",
            60,
            &env,
            "test-env",
            5,
        )
        .await
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&result.stdout).trim(),
            "works"
        );
    });
}

#[test]
fn e2e_exec_stderr() {
    run_local(async {
        let port = start_agent().await;

        let result = remote_exec(
            "127.0.0.1",
            port,
            TEST_PSK,
            "echo err >&2",
            "",
            60,
            &[],
            "test-stderr",
            5,
        )
        .await
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).trim(),
            "err"
        );
    });
}

// ─── E2E: Ping ─────────────────────────────────────────────────────

#[test]
fn e2e_ping() {
    run_local(async {
        let port = start_agent().await;

        let result = remote_ping("127.0.0.1", port, TEST_PSK, 5)
            .await
            .unwrap();

        assert!(!result.hostname.is_empty());
        assert!(!result.shell.is_empty());
        assert!(!result.version.is_empty());
    });
}

// ─── Auth failure ───────────────────────────────────────────────────

#[test]
fn e2e_auth_failure_wrong_psk() {
    run_local(async {
        let port = start_agent().await;

        let result = remote_exec(
            "127.0.0.1",
            port,
            "wrong-secret",
            "echo should-not-run",
            "",
            60,
            &[],
            "test-bad-auth",
            5,
        )
        .await;

        assert!(result.is_err());
    });
}

// ─── Connection failure ─────────────────────────────────────────────

#[test]
fn e2e_connect_timeout() {
    run_local(async {
        // Port with nothing listening
        let result = remote_ping("127.0.0.1", 1, "irrelevant", 1).await;
        assert!(result.is_err());
    });
}

// ─── Multiple sequential requests ───────────────────────────────────

#[test]
fn e2e_multiple_requests() {
    run_local(async {
        let port = start_agent_multi(3).await;

        // Each remote_exec opens a fresh connection
        for i in 0..3 {
            let result = remote_exec(
                "127.0.0.1",
                port,
                TEST_PSK,
                &format!("echo request-{i}"),
                "",
                60,
                &[],
                &format!("req-{i}"),
                5,
            )
            .await
            .unwrap();

            assert_eq!(result.exit_code, 0);
            assert_eq!(
                String::from_utf8_lossy(&result.stdout).trim(),
                format!("request-{i}")
            );
        }
    });
}
