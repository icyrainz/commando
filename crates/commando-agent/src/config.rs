use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_shell")]
    pub shell: String,
    pub psk: String,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// When true, wrap commands with `rtk` for token-optimized output.
    /// Requires `rtk` binary on PATH. Shorthand for `wrapper = "rtk"`.
    #[serde(default)]
    pub rtk: bool,

    /// Command wrapper binary for output optimization (e.g., "rtk").
    /// Takes precedence over the `rtk` field if set.
    #[serde(default)]
    pub wrapper: Option<String>,
}

fn default_port() -> u16 {
    9876
}
fn default_shell() -> String {
    "sh".to_string()
}
fn default_max_output_bytes() -> usize {
    131_072
}
fn default_max_concurrent() -> usize {
    8
}

impl AgentConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    /// Returns the wrapper binary name, if any.
    /// `wrapper` field takes precedence; `rtk = true` is shorthand for `wrapper = "rtk"`.
    pub fn wrapper_binary(&self) -> Option<&str> {
        if let Some(ref w) = self.wrapper {
            Some(w)
        } else if self.rtk {
            Some("rtk")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
bind = "10.0.0.5"
port = 9876
shell = "bash"
psk = "abcdef1234567890"
max_output_bytes = 131072
max_concurrent = 8
rtk = true
"#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bind, "10.0.0.5");
        assert_eq!(config.port, 9876);
        assert_eq!(config.shell, "bash");
        assert_eq!(config.psk, "abcdef1234567890");
        assert_eq!(config.max_output_bytes, 131_072);
        assert_eq!(config.max_concurrent, 8);
        assert!(config.rtk);
        assert_eq!(config.wrapper_binary(), Some("rtk"));
    }

    #[test]
    fn parse_minimal_config() {
        let toml_str = r#"
bind = "0.0.0.0"
psk = "secret"
"#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.port, 9876);
        assert_eq!(config.shell, "sh");
        assert_eq!(config.max_output_bytes, 131_072);
        assert_eq!(config.max_concurrent, 8);
        assert!(!config.rtk);
        assert_eq!(config.wrapper_binary(), None);
    }

    #[test]
    fn parse_custom_wrapper() {
        let toml_str = r#"
bind = "0.0.0.0"
psk = "secret"
wrapper = "my-optimizer"
"#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.wrapper_binary(), Some("my-optimizer"));
    }

    #[test]
    fn wrapper_overrides_rtk() {
        let toml_str = r#"
bind = "0.0.0.0"
psk = "secret"
rtk = true
wrapper = "custom-bin"
"#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.wrapper_binary(), Some("custom-bin"));
    }
}
