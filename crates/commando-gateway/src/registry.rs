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

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
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
            registry
                .manual_targets
                .insert(input.name.clone(), target.clone());
            registry.targets.insert(input.name, target);
        }
        registry
    }

    /// Replace all previously discovered targets with the new set, then re-apply manual overrides.
    pub fn update_discovered(&mut self, discovered: Vec<DiscoveredTarget>) {
        self.replace_discovered(discovered, |_| true);
    }

    /// Replace discovered targets, except those of configured nodes whose call failed
    /// (`failed_nodes`) and names in `retain` (failed lookups).
    /// Returns the dropped names that sit under no node in `configured_nodes`.
    pub fn update_discovered_for_nodes(
        &mut self,
        discovered: Vec<DiscoveredTarget>,
        configured_nodes: &[String],
        failed_nodes: &[String],
        retain: &[String],
    ) -> Vec<String> {
        let under = |name: &str, nodes: &[String]| {
            nodes.iter().any(|n| {
                name.strip_prefix(n.as_str())
                    .is_some_and(|r| r.starts_with('/'))
            })
        };
        let mut orphaned: Vec<String> = self
            .targets
            .iter()
            .filter(|(name, t)| {
                t.source == TargetSource::Discovered
                    && !under(name, configured_nodes)
                    && !retain.contains(name)
            })
            .map(|(name, _)| name.clone())
            .collect();
        orphaned.sort();
        self.replace_discovered(discovered, |name| {
            !under(name, failed_nodes) && !retain.iter().any(|r| r == name)
        });
        orphaned
    }

    /// Drop discovered targets selected by `replace`, then insert `discovered`.
    /// A target that keeps its host and port keeps its reachability.
    fn replace_discovered(
        &mut self,
        discovered: Vec<DiscoveredTarget>,
        replace: impl Fn(&str) -> bool,
    ) {
        let stale: Vec<String> = self
            .targets
            .iter()
            .filter(|(name, t)| t.source == TargetSource::Discovered && replace(name))
            .map(|(name, _)| name.clone())
            .collect();
        let old: HashMap<String, Target> = stale
            .into_iter()
            .filter_map(|name| self.targets.remove(&name).map(|t| (name, t)))
            .collect();

        for d in discovered {
            // Only insert if there is no manual override with the same name.
            if !self.manual_targets.contains_key(&d.name) {
                let reachable = match old.get(&d.name) {
                    Some(prev) if prev.host == d.host && prev.port == d.port => {
                        prev.reachable.clone()
                    }
                    _ => Reachability::Unknown,
                };
                let target = Target {
                    name: d.name.clone(),
                    host: d.host,
                    port: d.port,
                    shell: config::default_shell(),
                    tags: vec![],
                    source: TargetSource::Discovered,
                    status: d.status,
                    reachable,
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
                .filter(|t| t.name.contains(f) || t.tags.iter().any(|tag| tag.contains(f)))
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
    fn stopped_target_visible_in_list() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![
            DiscoveredTarget {
                name: "node-1/running-app".to_string(),
                host: "10.0.0.1".to_string(),
                port: 9876,
                status: "running".to_string(),
            },
            DiscoveredTarget {
                name: "node-1/stopped-app".to_string(),
                host: "".to_string(),
                port: 9876,
                status: "stopped".to_string(),
            },
        ]);
        let all = registry.list(None);
        assert_eq!(all.len(), 2);
        let stopped = all.iter().find(|t| t.name == "node-1/stopped-app").unwrap();
        assert!(stopped.host.is_empty());
        assert_eq!(stopped.status, "stopped");
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

    fn disc(name: &str, host: &str) -> DiscoveredTarget {
        DiscoveredTarget {
            name: name.to_string(),
            host: host.to_string(),
            port: 9876,
            status: "running".to_string(),
        }
    }

    fn names(registry: &Registry) -> Vec<String> {
        let mut v: Vec<String> = registry.targets.keys().cloned().collect();
        v.sort();
        v
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn failed_node_keeps_targets() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![disc("a/x", "10.0.0.1"), disc("b/y", "10.0.0.2")]);

        // Node "a" answered with a new set; node "b" failed.
        let orphaned = registry.update_discovered_for_nodes(
            vec![disc("a/z", "10.0.0.3")],
            &strs(&["a", "b"]),
            &strs(&["b"]),
            &[],
        );
        assert!(orphaned.is_empty());
        assert_eq!(names(&registry), ["a/z", "b/y"]);
    }

    #[test]
    fn unconfigured_node_targets_removed_when_sibling_answers() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![disc("a/x", "10.0.0.1"), disc("old/foo", "10.0.0.2")]);

        let orphaned = registry.update_discovered_for_nodes(
            vec![disc("a/x", "10.0.0.1")],
            &strs(&["a"]),
            &[],
            &[],
        );
        assert_eq!(orphaned, ["old/foo"]);
        assert_eq!(names(&registry), ["a/x"]);
    }

    #[test]
    fn unconfigured_node_targets_removed_when_all_nodes_fail() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![disc("a/x", "10.0.0.1"), disc("old/foo", "10.0.0.2")]);

        let orphaned =
            registry.update_discovered_for_nodes(vec![], &strs(&["a"]), &strs(&["a"]), &[]);
        assert_eq!(orphaned, ["old/foo"]);
        assert_eq!(names(&registry), ["a/x"]);
    }

    #[test]
    fn retained_name_survives_on_ok_node() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![disc("a/x", "10.0.0.1"), disc("a/gone", "10.0.0.2")]);

        registry.update_discovered_for_nodes(vec![], &strs(&["a"]), &[], &strs(&["a/x"]));
        assert_eq!(names(&registry), ["a/x"]);
    }

    #[test]
    fn reachable_kept_when_host_and_port_unchanged() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![disc("a/x", "10.0.0.1"), disc("a/y", "10.0.0.2")]);
        registry.set_reachable("a/x", Reachability::Reachable);
        registry.set_reachable("a/y", Reachability::Unreachable);

        registry.update_discovered_for_nodes(
            vec![disc("a/x", "10.0.0.1"), disc("a/y", "10.0.0.9")],
            &strs(&["a"]),
            &[],
            &[],
        );
        assert_eq!(registry.targets["a/x"].reachable, Reachability::Reachable);
        assert_eq!(registry.targets["a/y"].reachable, Reachability::Unknown);

        registry.set_reachable("a/x", Reachability::Reachable);
        registry.update_discovered(vec![disc("a/x", "10.0.0.1")]);
        assert_eq!(registry.targets["a/x"].reachable, Reachability::Reachable);
    }
}
