use serde::{Deserialize, Serialize};
use crate::error::OsResult;
use crate::resource::NetworkLimits;

/// Network configuration for a microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Interface ID visible inside the guest (e.g. "eth0").
    pub iface_id: String,
    /// TAP device name on the host.
    pub host_dev_name: String,
    /// MAC address assigned to the guest NIC.
    pub guest_mac: String,
    /// Guest IP address (CIDR notation, e.g. "172.16.0.2/30").
    pub guest_ip: String,
    /// Gateway IP for guest traffic.
    pub gateway_ip: String,
    /// Subnet mask in short form (e.g. "/30").
    pub mask_short: String,
    /// Rate limits for this interface.
    pub rate_limits: Option<NetworkLimits>,
}

impl NetworkConfig {
    /// Create a default network config for a given VM index.
    /// Each VM gets a unique /30 subnet carved from 172.16.{subnet}.0.
    pub fn for_vm_index(index: u32) -> Self {
        let subnet = index / 64;
        let offset = (index % 64) * 4;
        let gateway = offset + 1;
        let guest = offset + 2;

        Self {
            iface_id: "eth0".to_string(),
            host_dev_name: format!("tap{}", index),
            guest_mac: format!(
                "06:00:AC:{:02X}:{:02X}:{:02X}",
                subnet,
                (guest >> 8) & 0xFF,
                guest & 0xFF
            ),
            guest_ip: format!("172.16.{}.{}/30", subnet, guest),
            gateway_ip: format!("172.16.{}.{}", subnet, gateway),
            mask_short: "/30".to_string(),
            rate_limits: None,
        }
    }

    /// Apply network rate limits.
    pub fn with_limits(mut self, limits: NetworkLimits) -> Self {
        self.rate_limits = Some(limits);
        self
    }

    /// Generate the Firecracker network-interfaces API payload.
    pub fn to_firecracker_json(&self) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "iface_id": self.iface_id,
            "guest_mac": self.guest_mac,
            "host_dev_name": self.host_dev_name,
        });

        if let Some(ref limits) = self.rate_limits {
            let mut rate_limiter = serde_json::Map::new();

            if let Some(rx) = limits.rx_rate_bps {
                rate_limiter.insert(
                    "rx_rate_limiter".to_string(),
                    serde_json::json!({
                        "bandwidth": { "size": rx, "refill_time": 1000 }
                    }),
                );
            }

            if let Some(tx) = limits.tx_rate_bps {
                rate_limiter.insert(
                    "tx_rate_limiter".to_string(),
                    serde_json::json!({
                        "bandwidth": { "size": tx, "refill_time": 1000 }
                    }),
                );
            }

            if !rate_limiter.is_empty() {
                if let serde_json::Value::Object(ref mut obj) = payload {
                    obj.extend(rate_limiter);
                }
            }
        }

        payload
    }

    /// Build kernel boot args for networking inside the guest.
    pub fn kernel_boot_args(&self) -> String {
        let ip_no_mask = self.guest_ip.split('/').next().unwrap_or(&self.guest_ip);
        format!(
            "ip={}::{}:255.255.255.252::{}:off",
            ip_no_mask, self.gateway_ip, self.iface_id
        )
    }
}

/// Commands to set up the TAP device on the host side.
#[derive(Debug, Clone)]
pub struct TapSetup {
    pub tap_name: String,
    pub tap_ip: String,
    pub mask_short: String,
}

impl TapSetup {
    pub fn from_config(config: &NetworkConfig) -> Self {
        Self {
            tap_name: config.host_dev_name.clone(),
            tap_ip: config.gateway_ip.clone(),
            mask_short: config.mask_short.clone(),
        }
    }

    /// Generate shell commands to create and configure the TAP device.
    pub fn setup_commands(&self) -> Vec<String> {
        vec![
            format!("ip link del {} 2>/dev/null || true", self.tap_name),
            format!("ip tuntap add dev {} mode tap", self.tap_name),
            format!(
                "ip addr add {}{} dev {}",
                self.tap_ip, self.mask_short, self.tap_name
            ),
            format!("ip link set dev {} up", self.tap_name),
            "echo 1 > /proc/sys/net/ipv4/ip_forward".to_string(),
        ]
    }

    /// Generate shell commands to tear down the TAP device.
    pub fn teardown_commands(&self) -> Vec<String> {
        vec![format!("ip link del {} 2>/dev/null || true", self.tap_name)]
    }

    /// Execute TAP setup on the host (requires root).
    pub async fn setup(&self) -> OsResult<()> {
        for cmd in self.setup_commands() {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(cmd = %cmd, stderr = %stderr, "tap setup command returned non-zero");
            }
        }
        Ok(())
    }

    /// Tear down the TAP device.
    pub async fn teardown(&self) -> OsResult<()> {
        for cmd in self.teardown_commands() {
            let _ = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
                .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_config_for_index_0() {
        let cfg = NetworkConfig::for_vm_index(0);
        assert_eq!(cfg.host_dev_name, "tap0");
        assert_eq!(cfg.gateway_ip, "172.16.0.1");
        assert_eq!(cfg.guest_ip, "172.16.0.2/30");
    }

    #[test]
    fn network_config_for_index_5() {
        let cfg = NetworkConfig::for_vm_index(5);
        assert_eq!(cfg.host_dev_name, "tap5");
        assert_eq!(cfg.gateway_ip, "172.16.0.21");
        assert_eq!(cfg.guest_ip, "172.16.0.22/30");
    }

    #[test]
    fn kernel_boot_args_format() {
        let cfg = NetworkConfig::for_vm_index(0);
        let args = cfg.kernel_boot_args();
        assert!(args.contains("172.16.0.2"));
        assert!(args.contains("172.16.0.1"));
        assert!(args.contains("eth0"));
    }

    #[test]
    fn firecracker_json_has_required_fields() {
        let cfg = NetworkConfig::for_vm_index(0);
        let json = cfg.to_firecracker_json();
        assert_eq!(json["iface_id"], "eth0");
        assert!(json["guest_mac"].as_str().is_some());
        assert_eq!(json["host_dev_name"], "tap0");
    }

    #[test]
    fn firecracker_json_with_rate_limits() {
        let cfg = NetworkConfig::for_vm_index(0).with_limits(NetworkLimits {
            rx_rate_bps: Some(10_000_000),
            tx_rate_bps: Some(5_000_000),
            rx_rate_pps: None,
            tx_rate_pps: None,
        });
        let json = cfg.to_firecracker_json();
        assert!(json.get("rx_rate_limiter").is_some());
        assert!(json.get("tx_rate_limiter").is_some());
    }

    #[test]
    fn tap_setup_commands_count() {
        let cfg = NetworkConfig::for_vm_index(0);
        let tap = TapSetup::from_config(&cfg);
        assert_eq!(tap.setup_commands().len(), 5);
        assert_eq!(tap.teardown_commands().len(), 1);
    }

    #[test]
    fn network_config_serialization_roundtrip() {
        let cfg = NetworkConfig::for_vm_index(3);
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: NetworkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.host_dev_name, restored.host_dev_name);
        assert_eq!(cfg.guest_mac, restored.guest_mac);
    }
}
