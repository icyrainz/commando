//! Integration tests: spin up a real agent, connect with gateway RPC client,
//! and verify the auth → ping flow over TCP. The gateway runs commands through
//! the streaming path (stream_e2e.rs); the agent's non-streaming `exec` is still
//! served on the wire and is covered here with a raw client.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use futures::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::compat::TokioAsyncReadCompatExt;

use commando_agent::config::AgentConfig;
use commando_agent::rpc::{self as agent_rpc, ConcurrencyGuard};
use commando_common::auth;
use commando_common::commando_capnp::{authenticator, command_agent};
use commando_gateway::rpc::remote_ping;

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

// ─── E2E: non-streaming exec (raw client) ─────────────────────────

#[test]
fn e2e_agent_exec_over_the_wire() {
    run_local(async {
        let port = start_agent().await;

        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.set_nodelay(true).unwrap();
        let (reader, writer) = stream.compat().split();
        let network = capnp_rpc::twoparty::VatNetwork::new(
            futures::io::BufReader::new(reader),
            futures::io::BufWriter::new(writer),
            capnp_rpc::rpc_twoparty_capnp::Side::Client,
            Default::default(),
        );
        let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
        let auth_client: authenticator::Client =
            rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
        tokio::task::spawn_local(rpc_system);

        let challenge = auth_client
            .challenge_request()
            .send()
            .promise
            .await
            .unwrap();
        let nonce = challenge.get().unwrap().get_nonce().unwrap();
        let hmac = auth::compute_hmac(TEST_PSK.as_bytes(), nonce);
        let mut auth_request = auth_client.authenticate_request();
        auth_request.get().set_hmac(&hmac);
        let auth_response = auth_request.send().promise.await.unwrap();
        let agent: command_agent::Client = auth_response.get().unwrap().get_agent().unwrap();

        let mut request = agent.exec_request();
        {
            let mut req = request.get().init_request();
            req.set_command("echo hello");
            req.set_work_dir("");
            req.set_timeout_secs(60);
            req.set_request_id("integration-exec");
        }
        let response = request.send().promise.await.unwrap();
        let result = response.get().unwrap().get_result().unwrap();
        assert_eq!(result.get_exit_code(), 0);
        assert_eq!(result.get_stdout().unwrap(), b"hello\n");
        assert!(!result.get_timed_out());
        assert!(!result.get_truncated());
    });
}

// ─── E2E: Ping ─────────────────────────────────────────────────────

#[test]
fn e2e_ping() {
    run_local(async {
        let port = start_agent().await;

        let result = remote_ping("127.0.0.1", port, TEST_PSK, 5).await.unwrap();

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

        let result = remote_ping("127.0.0.1", port, "wrong-secret", 5).await;

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

// ─── Multiple sequential pings ───────────────────────────────────

#[test]
fn e2e_multiple_requests() {
    run_local(async {
        let port = start_agent_multi(3).await;

        // Each ping opens a fresh connection
        for _ in 0..3 {
            let result = remote_ping("127.0.0.1", port, TEST_PSK, 5).await.unwrap();
            assert!(!result.hostname.is_empty());
        }
    });
}
