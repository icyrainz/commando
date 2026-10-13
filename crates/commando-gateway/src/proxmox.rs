use anyhow::Result;
use serde::Deserialize;

use crate::config::ProxmoxConfig;
pub use crate::registry::DiscoveredTarget;

#[derive(Debug, Deserialize)]
struct ProxmoxResponse<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct LxcEntry {
    vmid: u64,
    name: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct InterfaceEntry {
    name: String,
    inet: Option<String>,
}

/// Extract the first non-loopback IPv4 address from interface list.
fn extract_ip(interfaces: &[InterfaceEntry]) -> Option<String> {
    interfaces.iter().filter(|i| i.name != "lo").find_map(|i| {
        i.inet.as_ref().map(|addr| {
            // Strip CIDR notation: "10.0.0.5/24" → "10.0.0.5"
            addr.split('/').next().unwrap_or(addr).to_string()
        })
    })
}

/// Result of one node's discovery.
#[derive(Default)]
pub struct NodeDiscovery {
    pub targets: Vec<DiscoveredTarget>,
    /// Names whose interface lookup failed; the registry keeps their previous entry.
    pub retain: Vec<String>,
}

/// Fetch the IP of a running LXC. `Err` means the lookup failed; `Ok(None)` means no IP.
async fn lookup_ip(
    client: &reqwest::Client,
    url: &str,
    auth_header: &str,
) -> Result<Option<String>> {
    let resp = client
        .get(url)
        .header("Authorization", auth_header)
        .send()
        .await?
        .error_for_status()?;
    let ifaces: ProxmoxResponse<Vec<InterfaceEntry>> = resp.json().await?;
    Ok(extract_ip(&ifaces.data))
}

/// Discover all LXC containers on a Proxmox node.
pub async fn discover_node(
    client: &reqwest::Client,
    node: &crate::config::ProxmoxNode,
    config: &ProxmoxConfig,
    default_port: u16,
) -> Result<NodeDiscovery> {
    let base_url = format!("https://{}:{}/api2/json", node.host, node.port);
    let auth_header = format!(
        "PVEAPIToken={}!{}={}",
        config.user, config.token_id, config.token_secret
    );

    // List LXCs
    let lxc_url = format!("{}/nodes/{}/lxc", base_url, node.name);
    let resp = client
        .get(&lxc_url)
        .header("Authorization", &auth_header)
        .send()
        .await?
        .error_for_status()?;
    let lxc_list: ProxmoxResponse<Vec<LxcEntry>> = resp.json().await?;

    let mut result = NodeDiscovery::default();

    for lxc in &lxc_list.data {
        if lxc.status != "running" {
            // Stopped/paused LXCs have no guest agent — skip the interface lookup
            result.targets.push(DiscoveredTarget {
                name: format!("{}/{}", node.name, lxc.name),
                host: "".to_string(),
                port: default_port,
                status: lxc.status.clone(),
            });
            continue;
        }

        // Get interfaces for IP discovery (running LXCs only)
        let iface_url = format!(
            "{}/nodes/{}/lxc/{}/interfaces",
            base_url, node.name, lxc.vmid
        );
        let name = format!("{}/{}", node.name, lxc.name);
        match lookup_ip(client, &iface_url, &auth_header).await {
            Ok(Some(host)) => result.targets.push(DiscoveredTarget {
                name,
                host,
                port: default_port,
                status: lxc.status.clone(),
            }),
            Ok(None) => {
                tracing::warn!(
                    node = %node.name,
                    vmid = lxc.vmid,
                    name = %lxc.name,
                    "could not determine IP for LXC, skipping"
                );
            }
            Err(e) => {
                tracing::warn!(
                    node = %node.name,
                    vmid = lxc.vmid,
                    name = %lxc.name,
                    error = %e,
                    "interface lookup failed for LXC, keeping previous entry"
                );
                result.retain.push(name);
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lxc_list_response() {
        let json = r#"{
            "data": [
                {"vmid": 100, "name": "my-app", "status": "running"},
                {"vmid": 101, "name": "my-db", "status": "stopped"}
            ]
        }"#;
        let resp: ProxmoxResponse<Vec<LxcEntry>> = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].name, "my-app");
        assert_eq!(resp.data[0].status, "running");
    }

    #[test]
    fn parse_interface_response() {
        let json = r#"{
            "data": [
                {
                    "name": "lo",
                    "inet": "127.0.0.1/8",
                    "hwaddr": "00:00:00:00:00:00"
                },
                {
                    "name": "eth0",
                    "inet": "10.0.0.5/24",
                    "hwaddr": "aa:bb:cc:dd:ee:ff"
                }
            ]
        }"#;
        let resp: ProxmoxResponse<Vec<InterfaceEntry>> = serde_json::from_str(json).unwrap();
        let ip = extract_ip(&resp.data);
        assert_eq!(ip, Some("10.0.0.5".to_string()));
    }

    #[test]
    fn extract_ip_skips_loopback() {
        let interfaces = vec![InterfaceEntry {
            name: "lo".to_string(),
            inet: Some("127.0.0.1/8".to_string()),
        }];
        assert_eq!(extract_ip(&interfaces), None);
    }
}
