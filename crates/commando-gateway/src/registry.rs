use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TargetSource {
    Manual,
    Discovered,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Reachability {
    Unknown,
    Reachable,
    Unreachable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub shell: String,
    pub tags: Vec<String>,
    pub source: TargetSource,
    pub status: String,
    pub reachable: Reachability,
}

pub struct ManualTargetInput {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub shell: String,
    pub tags: Vec<String>,
}

pub struct DiscoveredTarget {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub targets: HashMap<String, Target>,
    #[serde(skip)]
    manual_targets: HashMap<String, Target>,
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            targets: HashMap::new(),
            manual_targets: HashMap::new(),
        }
    }

    pub fn from_manual(inputs: Vec<ManualTargetInput>) -> Self {
        let mut registry = Registry::new();
        for input in inputs {
            let target = Target {
                name: input.name.clone(),
                host: input.host,
                port: input.port,
                shell: input.shell,
                tags: input.tags,
                source: TargetSource::Manual,
                status: "unknown".to_string(),
                reachable: Reachability::Unknown,
            };
            registry.manual_targets.insert(input.name.clone(), target.clone());
            registry.targets.insert(input.name, target);
        }
        registry
    }

    /// Replace all previously discovered targets with the new set, then re-apply manual overrides.
    pub fn update_discovered(&mut self, discovered: Vec<DiscoveredTarget>) {
        // Remove all currently discovered targets from the merged map.
        self.targets.retain(|_, v| v.source == TargetSource::Manual);

        for d in discovered {
            // Only insert if there is no manual override with the same name.
            if !self.manual_targets.contains_key(&d.name) {
                let target = Target {
                    name: d.name.clone(),
                    host: d.host,
                    port: d.port,
                    shell: config::default_shell(),
                    tags: vec![],
                    source: TargetSource::Discovered,
                    status: d.status,
                    reachable: Reachability::Unknown,
                };
                self.targets.insert(d.name, target);
            }
            // If a manual target exists with the same name, it already lives in self.targets;
            // the discovered entry is silently ignored.
        }
    }

    pub fn set_reachable(&mut self, name: &str, reachable: Reachability) {
        if let Some(target) = self.targets.get_mut(name) {
            target.reachable = reachable;
        }
    }

    /// List targets, optionally filtering by a substring of the name or tags.
    pub fn list(&self, filter: Option<&str>) -> Vec<&Target> {
        let mut results: Vec<&Target> = match filter {
            None => self.targets.values().collect(),
            Some(f) => self
                .targets
                .values()
                .filter(|t| {
                    t.name.contains(f) || t.tags.iter().any(|tag| tag.contains(f))
                })
                .collect(),
        };
        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    pub fn get(&self, name: &str) -> Option<&Target> {
        self.targets.get(name)
    }

    /// Serialize only discovered targets to JSON for disk caching.
    pub fn to_cache_json(&self) -> anyhow::Result<String> {
        let discovered: HashMap<&String, &Target> = self
            .targets
            .iter()
            .filter(|(_, v)| v.source == TargetSource::Discovered)
            .collect();
        Ok(serde_json::to_string(&discovered)?)
    }

    /// Restore discovered targets from a cache JSON string.
    /// Manual targets are not restored (they come from config).
    pub fn from_cache_json(json: &str) -> anyhow::Result<Registry> {
        let discovered: HashMap<String, Target> = serde_json::from_str(json)?;
        let mut registry = Registry::new();
        for (name, target) in discovered {
            registry.targets.insert(name, target);
        }
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_targets_loaded() {
        let targets = vec![ManualTargetInput {
            name: "my-desktop".to_string(),
            host: "192.168.1.50".to_string(),
            port: 9876,
            shell: "fish".to_string(),
            tags: vec!["gpu".to_string()],
        }];
        let registry = Registry::from_manual(targets);
        assert_eq!(registry.targets.len(), 1);
        let t = &registry.targets["my-desktop"];
        assert_eq!(t.host, "192.168.1.50");
        assert_eq!(t.shell, "fish");
        assert_eq!(t.source, TargetSource::Manual);
    }

    #[test]
    fn auto_discovered_merge() {
        let manual = vec![ManualTargetInput {
            name: "my-desktop".to_string(),
            host: "192.168.1.50".to_string(),
            port: 9876,
            shell: "fish".to_string(),
            tags: vec![],
        }];
        let mut registry = Registry::from_manual(manual);

        let discovered = vec![DiscoveredTarget {
            name: "node-1/app".to_string(),
            host: "10.0.0.5".to_string(),
            port: 9876,
            status: "running".to_string(),
        }];
        registry.update_discovered(discovered);

        assert_eq!(registry.targets.len(), 2);
        assert!(registry.targets.contains_key("node-1/app"));
        assert!(registry.targets.contains_key("my-desktop"));
    }

    #[test]
    fn discovery_replaces_previous() {
        let mut registry = Registry::new();

        registry.update_discovered(vec![DiscoveredTarget {
            name: "node-1/old".to_string(),
            host: "10.0.0.1".to_string(),
            port: 9876,
            status: "running".to_string(),
        }]);
        assert!(registry.targets.contains_key("node-1/old"));

        registry.update_discovered(vec![DiscoveredTarget {
            name: "node-1/new".to_string(),
            host: "10.0.0.2".to_string(),
            port: 9876,
            status: "running".to_string(),
        }]);
        assert!(!registry.targets.contains_key("node-1/old"));
        assert!(registry.targets.contains_key("node-1/new"));
    }

    #[test]
    fn manual_overrides_discovered() {
        let manual = vec![ManualTargetInput {
            name: "node-1/app".to_string(),
            host: "custom-ip".to_string(),
            port: 1234,
            shell: "fish".to_string(),
            tags: vec!["custom".to_string()],
        }];
        let mut registry = Registry::from_manual(manual);

        registry.update_discovered(vec![DiscoveredTarget {
            name: "node-1/app".to_string(),
            host: "10.0.0.5".to_string(),
            port: 9876,
            status: "running".to_string(),
        }]);

        let t = &registry.targets["node-1/app"];
        assert_eq!(t.host, "custom-ip");
        assert_eq!(t.port, 1234);
        assert_eq!(t.shell, "fish");
    }

    #[test]
    fn filter_targets() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![
            DiscoveredTarget {
                name: "node-1/web-app".to_string(),
                host: "10.0.0.1".to_string(),
                port: 9876,
                status: "running".to_string(),
            },
            DiscoveredTarget {
                name: "node-1/database".to_string(),
                host: "10.0.0.2".to_string(),
                port: 9876,
                status: "running".to_string(),
            },
        ]);

        let filtered = registry.list(Some("web"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "node-1/web-app");
    }

    #[test]
    fn cache_round_trip() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![DiscoveredTarget {
            name: "node-1/app".to_string(),
            host: "10.0.0.5".to_string(),
            port: 9876,
            status: "running".to_string(),
        }]);

        let json = registry.to_cache_json().unwrap();
        let loaded = Registry::from_cache_json(&json).unwrap();
        assert_eq!(loaded.targets.len(), 1);
        assert!(loaded.targets.contains_key("node-1/app"));
    }
}
