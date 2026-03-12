use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};

use commando_agent::config;
use commando_agent::rpc::{self, ConcurrencyGuard};

#[derive(Parser)]
#[command(
    name = "commando-agent",
    version,
    about = "Commando command execution agent"
)]
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
            if let Err(e) = rpc::handle_connection(
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
