use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::process::ExitStatus;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

// Go's zero time, which tailscale reports for nodes it has never seen.
const NEVER_SEEN: &str = "0001-01-01T00:00:00Z";

/// The parts of `tailscale status --json` the hub uses.
#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    #[serde(rename = "Self")]
    pub self_node: Node,
    #[serde(rename = "Peer")]
    pub peers: Option<BTreeMap<String, Node>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Node {
    pub host_name: String,
    #[serde(rename = "DNSName")]
    pub dns_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Option<Vec<IpAddr>>,
    pub online: bool,
    pub last_seen: Option<String>,
}

/// One entry of the `list_machines` payload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub name: String,
    pub host_name: String,
    pub dns_name: String,
    pub os: String,
    pub ips: Vec<IpAddr>,
    #[serde(rename = "self")]
    pub is_self: bool,
    pub online: bool,
    pub last_seen: Option<String>,
    pub services: Vec<String>,
}

#[derive(Debug)]
pub enum StatusError {
    Spawn(std::io::Error),
    Exit { status: ExitStatus, stderr: String },
    Parse(serde_json::Error),
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "cannot run tailscale status: {error}"),
            Self::Exit { status, stderr } => write!(f, "tailscale status {status}: {stderr}"),
            Self::Parse(error) => write!(f, "cannot parse tailscale status: {error}"),
        }
    }
}

impl std::error::Error for StatusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Exit { .. } => None,
        }
    }
}

/// Where the hub reads Tailscale status from.
#[derive(Debug, Clone)]
pub enum StatusSource {
    /// `tailscale status --json`, read anew each time.
    Tailscale,
    /// The same status every time, for tests.
    Fixed(Status),
}

impl StatusSource {
    pub async fn read(&self) -> Result<Status, StatusError> {
        match self {
            Self::Tailscale => Status::read().await,
            Self::Fixed(status) => Ok(status.clone()),
        }
    }
}

impl Status {
    pub async fn read() -> Result<Self, StatusError> {
        let output = Command::new("tailscale")
            .args(["status", "--json"])
            .output()
            .await
            .map_err(StatusError::Spawn)?;

        if !output.status.success() {
            return Err(StatusError::Exit {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        serde_json::from_slice(&output.stdout).map_err(StatusError::Parse)
    }

    fn peers(&self) -> impl Iterator<Item = &Node> {
        self.peers.iter().flat_map(BTreeMap::values)
    }

    /// Only peers: a connection from the hub's own address arrived through a local tunnel, so the
    /// connector's reported name is the better identity.
    pub fn machine_name(&self, ip: IpAddr) -> Option<&str> {
        let ip = ip.to_canonical();

        self.peers()
            .find(|node| {
                node.tailscale_ips
                    .iter()
                    .flatten()
                    .any(|&node_ip| node_ip == ip)
            })
            .map(Node::short_name)
    }

    /// Every node, online first and then by name, with the services `services` reports for it.
    pub fn machines(&self, services: impl Fn(&str) -> Vec<String>) -> Vec<Machine> {
        let nodes = [(&self.self_node, true)]
            .into_iter()
            .chain(self.peers().map(|node| (node, false)));

        let mut machines: Vec<Machine> = nodes
            .map(|(node, is_self)| {
                let name = node.short_name().to_owned();

                Machine {
                    services: services(&name),
                    name,
                    host_name: node.host_name.clone(),
                    dns_name: node.fqdn().to_owned(),
                    os: node.os.clone(),
                    ips: node.tailscale_ips.clone().unwrap_or_default(),
                    is_self,
                    online: node.online,
                    last_seen: node
                        .last_seen
                        .clone()
                        .filter(|last_seen| !last_seen.is_empty() && last_seen != NEVER_SEEN),
                }
            })
            .collect();

        // DNS labels are lowercase [a-z0-9-], where byte order matches the TS hub's localeCompare.
        machines.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
        machines
    }
}

impl Node {
    fn fqdn(&self) -> &str {
        self.dns_name.strip_suffix('.').unwrap_or(&self.dns_name)
    }

    fn short_name(&self) -> &str {
        self.fqdn().split('.').next().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        serde_json::from_value(serde_json::json!({
            "Self": {
                "HostName": "vps",
                "DNSName": "vps.tail722322.ts.net.",
                "OS": "linux",
                "TailscaleIPs": ["100.65.233.54", "fd7a:115c:a1e0::1"],
                "Online": true,
                "LastSeen": "0001-01-01T00:00:00Z",
            },
            "Peer": {
                "nodekey:a": {
                    "HostName": "ETERNAL-DESKTOP",
                    "DNSName": "eternal-desktop-1.tail722322.ts.net.",
                    "OS": "windows",
                    "TailscaleIPs": ["100.88.115.106"],
                    "Online": false,
                    "LastSeen": "2026-10-03T19:10:00.1Z",
                },
                "nodekey:b": {
                    "HostName": "Marios-MacBook-Pro",
                    "DNSName": "macbook.tail722322.ts.net.",
                    "OS": "macOS",
                    "TailscaleIPs": ["100.70.1.2"],
                    "Online": true,
                },
            },
        }))
        .unwrap()
    }

    fn ip(ip: &str) -> IpAddr {
        ip.parse().unwrap()
    }

    #[test]
    fn lists_online_machines_first_with_the_services_their_connector_registered() {
        let machines = status().machines(|name| match name {
            "macbook" => vec!["aside".to_owned(), "vphone".to_owned()],
            _ => Vec::new(),
        });

        let summary: Vec<_> = machines
            .iter()
            .map(|machine| {
                (
                    machine.name.as_str(),
                    machine.online,
                    machine.is_self,
                    machine.services.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "macbook",
                    true,
                    false,
                    vec!["aside".to_owned(), "vphone".to_owned()]
                ),
                ("vps", true, true, Vec::new()),
                ("eternal-desktop-1", false, false, Vec::new()),
            ]
        );
        assert_eq!(machines[1].last_seen, None);
        assert_eq!(
            machines[2].last_seen.as_deref(),
            Some("2026-10-03T19:10:00.1Z")
        );
    }

    #[test]
    fn maps_a_peer_tailscale_ip_to_its_machine_name_but_not_the_hub_own_address() {
        let status = status();

        assert_eq!(status.machine_name(ip("100.70.1.2")), Some("macbook"));
        assert_eq!(
            status.machine_name(ip("::ffff:100.70.1.2")),
            Some("macbook")
        );
        assert_eq!(status.machine_name(ip("100.65.233.54")), None);
        assert_eq!(status.machine_name(ip("127.0.0.1")), None);
    }

    #[test]
    fn serializes_machines_in_the_list_machines_shape() {
        let machines = status().machines(|_| Vec::new());

        assert_eq!(
            serde_json::to_value(&machines[1]).unwrap(),
            serde_json::json!({
                "name": "vps",
                "hostName": "vps",
                "dnsName": "vps.tail722322.ts.net",
                "os": "linux",
                "ips": ["100.65.233.54", "fd7a:115c:a1e0::1"],
                "self": true,
                "online": true,
                "lastSeen": null,
                "services": [],
            })
        );
    }

    #[test]
    fn reads_a_tailnet_without_peers_or_addresses() {
        let status: Status = serde_json::from_value(serde_json::json!({
            "Self": {
                "HostName": "vps",
                "DNSName": "vps.tail722322.ts.net.",
                "OS": "linux",
                "TailscaleIPs": null,
                "Online": false,
            },
            "Peer": null,
        }))
        .unwrap();

        let machines = status.machines(|_| Vec::new());
        assert_eq!(machines.len(), 1);
        assert!(machines[0].ips.is_empty());
        assert_eq!(status.machine_name(ip("100.65.233.54")), None);
    }

    #[test]
    fn rejects_a_status_shape_it_cannot_read() {
        let missing_self = serde_json::json!({ "Peer": null });
        let bad_ip = serde_json::json!({
            "Self": {
                "HostName": "vps",
                "DNSName": "vps.tail722322.ts.net.",
                "OS": "linux",
                "TailscaleIPs": ["not-an-ip"],
                "Online": true,
            },
            "Peer": null,
        });

        for invalid in [missing_self, bad_ip] {
            assert!(
                serde_json::from_value::<Status>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }
}
