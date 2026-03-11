mod config;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "commando-agent", about = "Commando command execution agent")]
struct Cli {
    #[arg(long, default_value = "/etc/commando/agent.toml")]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = config::AgentConfig::load(&cli.config)?;
    println!("Loaded config: bind={}:{}", config.bind, config.port);
    Ok(())
}
