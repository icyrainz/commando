use serde::Deserialize;

#[derive(Clone, Deserialize)]
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
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("shell", &self.shell)
            .field("psk", &"[REDACTED]")
            .field("max_output_bytes", &self.max_output_bytes)
            .field("max_concurrent", &self.max_concurrent)
            .finish()
    }
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
    /// Load the config. Also returns the paths of unknown keys.
    pub fn load(path: &std::path::Path) -> anyhow::Result<(Self, Vec<String>)> {
        let content = std::fs::read_to_string(path)?;
        let mut unknown = Vec::new();
        let config: Self = serde_ignored::deserialize(toml::Deserializer::new(&content), |p| {
            unknown.push(p.to_string())
        })?;
        Ok((config, unknown))
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
"#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bind, "10.0.0.5");
        assert_eq!(config.port, 9876);
        assert_eq!(config.shell, "bash");
        assert_eq!(config.psk, "abcdef1234567890");
        assert_eq!(config.max_output_bytes, 131_072);
        assert_eq!(config.max_concurrent, 8);
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
    }

    #[test]
    fn legacy_rtk_and_wrapper_keys_are_ignored() {
        let toml_str = r#"
bind = "0.0.0.0"
psk = "secret"
rtk = true
wrapper = "x"
"#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bind, "0.0.0.0");
    }

    #[test]
    fn load_reports_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("commando-agent-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.toml");
        std::fs::write(
            &path,
            "bind = \"0.0.0.0\"\npsk = \"s\"\nmax_concurent = 3\n",
        )
        .unwrap();
        let (_, unknown) = AgentConfig::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(unknown, vec!["max_concurent".to_string()]);
    }

    #[test]
    fn debug_redacts_psk() {
        let config: AgentConfig =
            toml::from_str("bind = \"0.0.0.0\"\npsk = \"psk-secret-value\"\n").unwrap();
        let out = format!("{:?}", config);
        assert!(!out.contains("psk-secret-value"), "{out}");
    }
}
