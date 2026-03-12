use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    pub proxmox: ProxmoxConfig,
    pub agent: AgentConnectionConfig,
    #[serde(default)]
    pub targets: Vec<ManualTarget>,
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxmoxConfig {
    pub nodes: Vec<ProxmoxNode>,
    pub user: String,
    pub token_id: String,
    pub token_secret: String,
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxmoxNode {
    pub name: String,
    pub host: String,
    #[serde(default = "default_proxmox_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConnectionConfig {
    #[serde(default = "default_agent_port")]
    pub default_port: u16,
    #[serde(default = "default_timeout")]
    pub default_timeout_secs: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_per_target: usize,
    #[serde(default)]
    pub psk: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManualTarget {
    pub name: String,
    pub host: String,
    #[serde(default = "default_agent_port")]
    pub port: u16,
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_server_port")]
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            bind: default_bind(),
            port: default_server_port(),
        }
    }
}

fn default_transport() -> String { "streamable-http".to_string() }
fn default_bind() -> String { "0.0.0.0".to_string() }
fn default_server_port() -> u16 { 9877 }

fn default_discovery_interval() -> u64 { 60 }
fn default_proxmox_port() -> u16 { 8006 }
fn default_agent_port() -> u16 { 9876 }
fn default_timeout() -> u32 { 60 }
fn default_connect_timeout() -> u64 { 5 }
fn default_max_concurrent() -> usize { 4 }
pub fn default_shell() -> String { "sh".to_string() }
pub fn default_cache_dir() -> String { "/var/lib/commando".to_string() }

impl GatewayConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
[proxmox]
nodes = [
    { name = "node-1", host = "192.168.1.10", port = 8006 },
    { name = "node-2", host = "192.168.1.11", port = 8006 },
]
user = "root@pam"
token_id = "commando"
token_secret = "xxxx-xxxx"
discovery_interval_secs = 60

[agent]
default_port = 9876
default_timeout_secs = 60
connect_timeout_secs = 5
max_concurrent_per_target = 4

[agent.psk]
"node-1/my-app" = "aaaa"
"node-1/my-db" = "bbbb"
my-desktop = "cccc"

[[targets]]
name = "my-desktop"
host = "my-desktop"
port = 9876
shell = "fish"
tags = ["gpu", "desktop"]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.proxmox.nodes.len(), 2);
        assert_eq!(config.proxmox.nodes[0].name, "node-1");
        assert_eq!(config.agent.psk.len(), 3);
        assert_eq!(config.agent.psk["my-desktop"], "cccc");
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].name, "my-desktop");
        assert_eq!(config.targets[0].tags, vec!["gpu", "desktop"]);
    }

    #[test]
    fn parse_config_with_server_section() {
        let toml_str = r#"
[server]
transport = "streamable-http"
bind = "127.0.0.1"
port = 9877

[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxx"

[agent]

[agent.psk]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.transport, "streamable-http");
        assert_eq!(config.server.bind, "127.0.0.1");
        assert_eq!(config.server.port, 9877);
    }

    #[test]
    fn server_section_defaults() {
        let toml_str = r#"
[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxx"

[agent]

[agent.psk]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.transport, "streamable-http");
        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.server.port, 9877);
    }

    #[test]
    fn cache_dir_defaults() {
        let toml_str = r#"
[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxx"

[agent]

[agent.psk]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.cache_dir, "/var/lib/commando");
    }

    #[test]
    fn parse_minimal_config() {
        let toml_str = r#"
[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxx"

[agent]

[agent.psk]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.default_port, 9876);
        assert_eq!(config.agent.default_timeout_secs, 60);
        assert_eq!(config.agent.connect_timeout_secs, 5);
        assert_eq!(config.agent.max_concurrent_per_target, 4);
        assert!(config.targets.is_empty());
    }
}
