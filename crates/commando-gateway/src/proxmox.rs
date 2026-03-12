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
    interfaces
        .iter()
        .filter(|i| i.name != "lo")
        .find_map(|i| {
            i.inet.as_ref().map(|addr| {
                // Strip CIDR notation: "10.0.0.5/24" → "10.0.0.5"
                addr.split('/').next().unwrap_or(addr).to_string()
            })
        })
}

/// Discover all LXC containers on a Proxmox node.
pub async fn discover_node(
    client: &reqwest::Client,
    node: &crate::config::ProxmoxNode,
    config: &ProxmoxConfig,
    default_port: u16,
) -> Result<Vec<DiscoveredTarget>> {
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

    let mut targets = Vec::new();

    for lxc in &lxc_list.data {
        if lxc.status != "running" {
            // Stopped/paused LXCs have no guest agent — skip the interface lookup
            targets.push(DiscoveredTarget {
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
        let ip = match client
            .get(&iface_url)
            .header("Authorization", &auth_header)
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(iface_resp) = resp.json::<ProxmoxResponse<Vec<InterfaceEntry>>>().await {
                    extract_ip(&iface_resp.data)
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        if let Some(host) = ip {
            targets.push(DiscoveredTarget {
                name: format!("{}/{}", node.name, lxc.name),
                host,
                port: default_port,
                status: lxc.status.clone(),
            });
        } else {
            tracing::warn!(
                node = %node.name,
                vmid = lxc.vmid,
                name = %lxc.name,
                "could not determine IP for LXC, skipping"
            );
        }
    }

    Ok(targets)
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
        let interfaces = vec![
            InterfaceEntry {
                name: "lo".to_string(),
                inet: Some("127.0.0.1/8".to_string()),
            },
        ];
        assert_eq!(extract_ip(&interfaces), None);
    }
}
