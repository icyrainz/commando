use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use commando_gateway::config;
use commando_gateway::handler;
use commando_gateway::mcp;
use commando_gateway::proxmox;
use commando_gateway::registry::{self, DiscoveredTarget, Registry};
use commando_gateway::rpc;
use commando_gateway::streamable;

#[derive(Parser)]
#[command(name = "commando-gateway", version, about = "Commando MCP gateway")]
struct Cli {
    #[arg(long, default_value = "/etc/commando/gateway.toml")]
    config: std::path::PathBuf,

    /// MCP transport: "streamable-http" or "stdio"
    #[arg(long)]
    transport: Option<String>,

    /// HTTP bind address (streamable-http only)
    #[arg(long)]
    bind: Option<String>,

    /// HTTP port (streamable-http only)
    #[arg(long)]
    port: Option<u16>,

    /// Registry cache directory
    #[arg(long)]
    cache_dir: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = config::GatewayConfig::load(&cli.config)?;

    // CLI overrides for server settings
    if let Some(transport) = &cli.transport {
        config.server.transport = transport.clone();
    }
    if let Some(bind) = &cli.bind {
        config.server.bind = bind.clone();
    }
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    if let Some(cache_dir) = &cli.cache_dir {
        config.cache_dir = cache_dir.clone();
    }

    let config = Arc::new(config);

    // Structured JSON logging to stderr (stdout is for MCP protocol)
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("commando_gateway=info".parse().unwrap()),
        )
        .with_target(false)
        .init();

    info!(
        proxmox_nodes = config.proxmox.as_ref().map(|p| p.nodes.len()).unwrap_or(0),
        manual_targets = config.targets.len(),
        transport = %config.server.transport,
        "starting commando-gateway v{}",
        env!("CARGO_PKG_VERSION"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run_gateway(config))
}

async fn run_gateway(config: Arc<config::GatewayConfig>) -> Result<()> {
    // Build initial registry from manual targets
    let manual_inputs: Vec<registry::ManualTargetInput> = config
        .targets
        .iter()
        .map(|t| registry::ManualTargetInput {
            name: t.name.clone(),
            host: t.host.clone(),
            port: t.port,
            shell: t.shell.clone(),
            tags: t.tags.clone(),
        })
        .collect();

    let registry = Arc::new(Mutex::new(Registry::from_manual(manual_inputs)));

    // Try to load cached registry
    let cache_path = std::path::Path::new(&config.cache_dir).join("registry.json");
    if cache_path.exists() {
        match std::fs::read_to_string(cache_path) {
            Ok(json) => match Registry::from_cache_json(&json) {
                Ok(cached) => {
                    let discovered: Vec<DiscoveredTarget> = cached
                        .targets
                        .values()
                        .filter(|t| t.source == registry::TargetSource::Discovered)
                        .map(|t| DiscoveredTarget {
                            name: t.name.clone(),
                            host: t.host.clone(),
                            port: t.port,
                            status: t.status.clone(),
                        })
                        .collect();
                    registry.lock().unwrap().update_discovered(discovered);
                    info!("loaded cached registry from disk");
                }
                Err(e) => warn!(error = %e, "failed to parse cached registry"),
            },
            Err(e) => warn!(error = %e, "failed to read registry cache"),
        }
    } else if config.proxmox.as_ref().is_some_and(|p| !p.nodes.is_empty()) {
        info!("no registry cache, running initial discovery");
        run_discovery_cycle(&config, &registry).await;
    }

    let limiter = Arc::new(handler::ConcurrencyLimiter::new(
        config.agent.max_concurrent_per_target,
    ));

    // Spawn background discovery loop
    if let Some(proxmox) = &config.proxmox {
        if !proxmox.nodes.is_empty() {
            let discovery_interval = proxmox.discovery_interval_secs;
            let config_clone = config.clone();
            let registry_clone = registry.clone();
            tokio::task::spawn_local(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                    discovery_interval,
                ));
                interval.tick().await; // Skip immediate first tick
                loop {
                    interval.tick().await;
                    run_discovery_cycle(&config_clone, &registry_clone).await;
                }
            });
        }
    }

    // Run MCP server on selected transport
    match config.server.transport.as_str() {
        "stdio" => mcp::run_stdio_loop(config, registry, limiter).await,
        "streamable-http" => streamable::run_streamable_server(config, registry, limiter).await,
        other => anyhow::bail!("unknown transport: {other} (expected 'stdio' or 'streamable-http')"),
    }
}

async fn run_discovery_cycle(
    config: &config::GatewayConfig,
    registry: &Arc<Mutex<Registry>>,
) {
    let proxmox_config = match &config.proxmox {
        Some(p) => p,
        None => return,
    };

    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // Proxmox uses self-signed certs
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut all_discovered = Vec::new();

    for node in &proxmox_config.nodes {
        match proxmox::discover_node(&http_client, node, proxmox_config, config.agent.default_port).await {
            Ok(targets) => {
                info!(node = %node.name, count = targets.len(), "discovered LXC targets");
                all_discovered.extend(targets);
            }
            Err(e) => {
                error!(node = %node.name, error = %e, "Proxmox discovery failed");
            }
        }
    }

    registry.lock().unwrap().update_discovered(all_discovered);

    // Ping all targets with PSKs to check reachability
    let targets_to_ping: Vec<(String, String, u16, String)> = {
        let reg = registry.lock().unwrap();
        reg.targets
            .values()
            .filter_map(|t| {
                if t.host.is_empty() {
                    return None; // Skip stopped/unreachable targets with no IP
                }
                config.agent.psk.get(&t.name)
                    .map(|psk| (t.name.clone(), t.host.clone(), t.port, psk.clone()))
            })
            .collect()
    };

    let ping_futures: Vec<_> = targets_to_ping
        .iter()
        .map(|(name, host, port, psk)| {
            let name = name.clone();
            let host = host.clone();
            let port = *port;
            let psk = psk.clone();
            let connect_timeout = config.agent.connect_timeout_secs;
            async move {
                let reachable = rpc::remote_ping(&host, port, &psk, connect_timeout).await.is_ok();
                (name, reachable)
            }
        })
        .collect();

    let ping_results = futures::future::join_all(ping_futures).await;
    {
        let mut reg = registry.lock().unwrap();
        for (name, reachable) in ping_results {
            reg.set_reachable(
                &name,
                if reachable { registry::Reachability::Reachable } else { registry::Reachability::Unreachable },
            );
        }
    }

    // Save cache to disk
    let cache_dir = std::path::Path::new(&config.cache_dir);
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        warn!(error = %e, "failed to create cache directory");
        return;
    }
    let cache_path = cache_dir.join("registry.json");
    match registry.lock().unwrap().to_cache_json() {
        Ok(json) => {
            if let Err(e) = std::fs::write(&cache_path, json) {
                warn!(error = %e, "failed to write registry cache");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize registry cache"),
    }
}
