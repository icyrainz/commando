use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub proxmox: Option<ProxmoxConfig>,
    pub agent: AgentConnectionConfig,
    #[serde(default)]
    pub targets: Vec<ManualTarget>,
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    #[serde(default)]
    pub streaming: StreamingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamingConfig {
    #[serde(default = "default_page_timeout")]
    pub page_timeout_secs: u64,
    #[serde(default = "default_page_max_bytes")]
    pub page_max_bytes: usize,
    #[serde(default = "default_session_idle_timeout")]
    pub session_idle_timeout_secs: u64,
    /// Buffered bytes per session above which `receive` calls are held back.
    #[serde(default = "default_session_buffer_max_bytes")]
    pub session_buffer_max_bytes: usize,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            page_timeout_secs: default_page_timeout(),
            page_max_bytes: default_page_max_bytes(),
            session_idle_timeout_secs: default_session_idle_timeout(),
            session_buffer_max_bytes: default_session_buffer_max_bytes(),
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct ProxmoxConfig {
    pub nodes: Vec<ProxmoxNode>,
    pub user: String,
    pub token_id: String,
    pub token_secret: String,
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,
}

impl std::fmt::Debug for ProxmoxConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxmoxConfig")
            .field("nodes", &self.nodes)
            .field("user", &self.user)
            .field("token_id", &self.token_id)
            .field("token_secret", &"[REDACTED]")
            .field("discovery_interval_secs", &self.discovery_interval_secs)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxmoxNode {
    pub name: String,
    pub host: String,
    #[serde(default = "default_proxmox_port")]
    pub port: u16,
}

#[derive(Clone, Deserialize)]
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

impl std::fmt::Debug for AgentConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let psk: std::collections::BTreeMap<&String, &str> =
            self.psk.keys().map(|k| (k, "[REDACTED]")).collect();
        f.debug_struct("AgentConnectionConfig")
            .field("default_port", &self.default_port)
            .field("default_timeout_secs", &self.default_timeout_secs)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("max_concurrent_per_target", &self.max_concurrent_per_target)
            .field("psk", &psk)
            .finish()
    }
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

#[derive(Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_server_port")]
    pub port: u16,
    /// API key for bearer token auth on the /mcp endpoint.
    /// Can also be set via COMMANDO_API_KEY env var (takes precedence).
    /// Required for streamable-http transport.
    pub api_key: Option<String>,
    /// How Claude Code executes commands: "cli" (default) or "mcp".
    /// - "cli": only commando_list and commando_ping exposed as MCP tools.
    ///   Tool description tells Claude to use `commando exec` via Bash.
    /// - "mcp": also exposes commando_exec and commando_output as MCP tools.
    ///   No CLI install needed, but output may be truncated by Claude Code.
    #[serde(default = "default_execution_mode")]
    pub execution_mode: String,
    /// Path to the JSONL audit log file. Defaults to {cache_dir}/audit.log.
    /// Set to "/dev/null" to disable.
    pub audit_log_path: Option<String>,
    /// Max audit log size in bytes before rotation. Default: 10MB.
    #[serde(default = "default_audit_log_max_bytes")]
    pub audit_log_max_bytes: u64,
}

impl ServerConfig {
    pub fn is_mcp_exec_mode(&self) -> bool {
        self.execution_mode == "mcp"
    }
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("transport", &self.transport)
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("execution_mode", &self.execution_mode)
            .field("audit_log_path", &self.audit_log_path)
            .field("audit_log_max_bytes", &self.audit_log_max_bytes)
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            bind: default_bind(),
            port: default_server_port(),
            api_key: None,
            execution_mode: default_execution_mode(),
            audit_log_path: None,
            audit_log_max_bytes: default_audit_log_max_bytes(),
        }
    }
}

fn default_execution_mode() -> String {
    "cli".to_string()
}
fn default_audit_log_max_bytes() -> u64 {
    10 * 1024 * 1024 // 10MB
}

fn default_transport() -> String {
    "streamable-http".to_string()
}
fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_server_port() -> u16 {
    9877
}

fn default_discovery_interval() -> u64 {
    60
}
fn default_proxmox_port() -> u16 {
    8006
}
fn default_agent_port() -> u16 {
    9876
}
fn default_timeout() -> u32 {
    300
}
fn default_connect_timeout() -> u64 {
    5
}
fn default_max_concurrent() -> usize {
    4
}
pub fn default_shell() -> String {
    "sh".to_string()
}
pub fn default_cache_dir() -> String {
    "/var/lib/commando".to_string()
}
fn default_page_timeout() -> u64 {
    5
}
fn default_page_max_bytes() -> usize {
    32_768 // 32KB
}
fn default_session_buffer_max_bytes() -> usize {
    8 * 1024 * 1024 // 8MiB
}
fn default_session_idle_timeout() -> u64 {
    60
}

impl GatewayConfig {
    /// Load and validate the config. Also returns the paths of unknown keys.
    pub fn load(path: &std::path::Path) -> anyhow::Result<(Self, Vec<String>)> {
        let content = std::fs::read_to_string(path)?;
        let mut unknown = Vec::new();
        let config: Self = serde_ignored::deserialize(toml::Deserializer::new(&content), |p| {
            unknown.push(p.to_string())
        })?;
        config.validate()?;
        Ok((config, unknown))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let transport = self.server.transport.as_str();
        if !matches!(transport, "stdio" | "streamable-http") {
            anyhow::bail!(
                "invalid transport {transport:?} (expected \"stdio\" or \"streamable-http\")"
            );
        }
        let mode = self.server.execution_mode.as_str();
        if !matches!(mode, "cli" | "mcp") {
            anyhow::bail!("invalid execution_mode {mode:?} (expected \"cli\" or \"mcp\")");
        }
        let streaming = &self.streaming;
        if streaming.page_max_bytes < 4 {
            anyhow::bail!(
                "streaming.page_max_bytes must be at least 4, got {}",
                streaming.page_max_bytes
            );
        }
        if streaming.session_buffer_max_bytes < streaming.page_max_bytes {
            anyhow::bail!(
                "streaming.session_buffer_max_bytes ({}) must be at least page_max_bytes ({})",
                streaming.session_buffer_max_bytes,
                streaming.page_max_bytes
            );
        }
        Ok(())
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
        let proxmox = config.proxmox.unwrap();
        assert_eq!(proxmox.nodes.len(), 2);
        assert_eq!(proxmox.nodes[0].name, "node-1");
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
        assert_eq!(config.agent.default_timeout_secs, 300);
        assert_eq!(config.agent.connect_timeout_secs, 5);
        assert_eq!(config.agent.max_concurrent_per_target, 4);
        assert!(config.targets.is_empty());
    }

    #[test]
    fn parse_config_without_proxmox() {
        let toml_str = r#"
[agent]
default_port = 9876

[agent.psk]
my-target = "secret"

[[targets]]
name = "my-target"
host = "192.168.1.50"
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert!(config.proxmox.is_none());
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.agent.psk["my-target"], "secret");
    }

    fn load_str(toml_str: &str) -> anyhow::Result<(GatewayConfig, Vec<String>)> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.toml");
        std::fs::write(&path, toml_str).unwrap();
        GatewayConfig::load(&path)
    }

    #[test]
    fn unknown_keys_are_reported() {
        let (config, unknown) = load_str(
            r#"
bogus = 1

[server]
transprot = "stdio"

[agent]
"#,
        )
        .unwrap();
        assert_eq!(config.server.transport, "streamable-http");
        assert_eq!(unknown.len(), 2);
        assert!(unknown.contains(&"bogus".to_string()));
        assert!(unknown.contains(&"server.transprot".to_string()));
    }

    #[test]
    fn known_keys_report_nothing() {
        let (_, unknown) = load_str("[agent]\n[agent.psk]\nmy-target = \"s\"\n").unwrap();
        assert!(unknown.is_empty());
    }

    #[test]
    fn bad_execution_mode_rejected() {
        let err = load_str("[server]\nexecution_mode = \"cli \"\n[agent]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"cli \""), "{err}");
        assert!(err.contains("\"mcp\""), "{err}");
    }

    #[test]
    fn bad_transport_rejected() {
        let err = load_str("[server]\ntransport = \"http\"\n[agent]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"http\""), "{err}");
        assert!(err.contains("streamable-http"), "{err}");
    }

    #[test]
    fn streaming_defaults_validate() {
        let (config, _) = load_str("[agent]\n").unwrap();
        assert_eq!(config.streaming.page_max_bytes, 32_768);
        assert_eq!(config.streaming.session_buffer_max_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn tiny_page_max_bytes_rejected() {
        let err = load_str("[agent]\n[streaming]\npage_max_bytes = 3\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("page_max_bytes"), "{err}");
        load_str("[agent]\n[streaming]\npage_max_bytes = 4\n").unwrap();
    }

    #[test]
    fn buffer_cap_below_page_size_rejected() {
        let err = load_str(
            "[agent]\n[streaming]\npage_max_bytes = 1000\nsession_buffer_max_bytes = 999\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("session_buffer_max_bytes"), "{err}");
        load_str("[agent]\n[streaming]\npage_max_bytes = 1000\nsession_buffer_max_bytes = 1000\n")
            .unwrap();
    }

    #[test]
    fn debug_redacts_secrets() {
        let (config, _) = load_str(
            r#"
[server]
api_key = "api-secret-value"

[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "token-secret-value"

[agent]

[agent.psk]
my-target = "psk-secret-value"
"#,
        )
        .unwrap();
        let out = format!("{:?}", config);
        assert!(!out.contains("api-secret-value"), "{out}");
        assert!(!out.contains("token-secret-value"), "{out}");
        assert!(!out.contains("psk-secret-value"), "{out}");
        assert!(out.contains("my-target"));
    }
}
