mod config;
mod process;
mod rpc;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use clap::Parser;
use futures::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{info, warn};

use commando_common::commando_capnp::authenticator;
use rpc::{AuthenticatorImpl, ConcurrencyGuard};

#[derive(Parser)]
#[command(name = "commando-agent", about = "Commando command execution agent")]
struct Cli {
    #[arg(long, default_value = "/etc/commando/agent.toml")]
    config: std::path::PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Rc::new(config::AgentConfig::load(&cli.config)?);

    // Structured JSON logging
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("commando_agent=info".parse().unwrap()),
        )
        .with_target(false)
        .init();

    info!(
        bind = %config.bind,
        port = config.port,
        shell = %config.shell,
        max_concurrent = config.max_concurrent,
        "starting commando-agent v{}",
        env!("CARGO_PKG_VERSION"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run_server(config))
}

async fn run_server(config: Rc<config::AgentConfig>) -> Result<()> {
    let addr = format!("{}:{}", config.bind, config.port);
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "listening for connections");

    let rate_limits = Rc::new(RefCell::new(HashMap::new()));
    let concurrency_guard = Rc::new(ConcurrencyGuard::new(config.max_concurrent));
    let agent_start_time = std::time::Instant::now();

    loop {
        let (stream, peer_addr) = listener.accept().await?;

        // Enable SO_KEEPALIVE
        let socket_ref = socket2::SockRef::from(&stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(60))
            .with_interval(std::time::Duration::from_secs(10));
        if let Err(e) = socket_ref.set_tcp_keepalive(&keepalive) {
            warn!(peer = %peer_addr, error = %e, "failed to set TCP keepalive");
        }

        info!(peer = %peer_addr, "connection accepted");

        let config = config.clone();
        let rate_limits = rate_limits.clone();
        let concurrency_guard = concurrency_guard.clone();

        tokio::task::spawn_local(async move {
            if let Err(e) = handle_connection(
                stream,
                peer_addr.ip(),
                config,
                rate_limits,
                concurrency_guard,
                agent_start_time,
            )
            .await
            {
                warn!(peer = %peer_addr, error = %e, "connection error");
            }
            info!(peer = %peer_addr, "connection closed");
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    peer_ip: std::net::IpAddr,
    config: Rc<config::AgentConfig>,
    rate_limits: Rc<RefCell<HashMap<std::net::IpAddr, rpc::RateLimitState>>>,
    concurrency_guard: Rc<ConcurrencyGuard>,
    agent_start_time: std::time::Instant,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let stream = stream.compat();
    let (reader, writer) = stream.split();

    let network = capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );

    let authenticator = AuthenticatorImpl::new(
        config,
        peer_ip,
        rate_limits,
        concurrency_guard,
        agent_start_time,
    );
    let auth_client: authenticator::Client =
        capnp_rpc::new_client(authenticator);

    let rpc_system =
        capnp_rpc::RpcSystem::new(Box::new(network), Some(auth_client.client));

    rpc_system.await?;
    Ok(())
}
