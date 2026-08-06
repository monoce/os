use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use crate::error::{OsError, OsResult};
use crate::resource::NetworkLimits;

/// Is this a TAP device name this crate generated?
///
/// The name is passed to `ip` and to Firecracker, so anything but `tapN` is
/// either a typo or an attempt to smuggle arguments in through a config file.
fn is_valid_tap_name(name: &str) -> bool {
    match name.strip_prefix("tap") {
        Some(idx) => !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Is this a `/N` netmask suffix with a plausible prefix length?
fn is_valid_mask_short(mask: &str) -> bool {
    match mask.strip_prefix('/') {
        Some(bits) => bits.parse::<u8>().is_ok_and(|b| b <= 32),
        None => false,
    }
}

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

    /// Check that a config is one this crate could have produced.
    ///
    /// Call this immediately after deserializing a config from disk: the TAP
    /// name and gateway IP become arguments to `ip`, which `monoce-os` runs as
    /// root.
    pub fn validate(&self) -> OsResult<()> {
        if !is_valid_tap_name(&self.host_dev_name) {
            return Err(OsError::InvalidId {
                kind: "TAP device name",
                value: self.host_dev_name.clone(),
                reason: "must be 'tap' followed by digits",
            });
        }
        if self.gateway_ip.parse::<Ipv4Addr>().is_err() {
            return Err(OsError::InvalidId {
                kind: "gateway IP",
                value: self.gateway_ip.clone(),
                reason: "must be a bare IPv4 address",
            });
        }
        let guest_ip = self.guest_ip.split('/').next().unwrap_or("");
        if guest_ip.parse::<Ipv4Addr>().is_err() {
            return Err(OsError::InvalidId {
                kind: "guest IP",
                value: self.guest_ip.clone(),
                reason: "must be an IPv4 address, optionally with a /N suffix",
            });
        }
        if !is_valid_mask_short(&self.mask_short) {
            return Err(OsError::InvalidId {
                kind: "netmask",
                value: self.mask_short.clone(),
                reason: "must be '/N' with N between 0 and 32",
            });
        }
        Ok(())
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

    /// Reject a TAP config that could not have come from `NetworkConfig`.
    ///
    /// `setup`/`teardown` run `ip` as root, so a config deserialized from a
    /// tampered `vm-config.json` must not reach them.
    pub fn validate(&self) -> OsResult<()> {
        if !is_valid_tap_name(&self.tap_name) {
            return Err(OsError::InvalidId {
                kind: "TAP device name",
                value: self.tap_name.clone(),
                reason: "must be 'tap' followed by digits",
            });
        }
        if self.tap_ip.parse::<Ipv4Addr>().is_err() {
            return Err(OsError::InvalidId {
                kind: "TAP IP",
                value: self.tap_ip.clone(),
                reason: "must be a bare IPv4 address",
            });
        }
        if !is_valid_mask_short(&self.mask_short) {
            return Err(OsError::InvalidId {
                kind: "netmask",
                value: self.mask_short.clone(),
                reason: "must be '/N' with N between 0 and 32",
            });
        }
        Ok(())
    }

    /// The `ip` argument vectors that create and configure the TAP device.
    ///
    /// These are argv, not shell strings: no interpolated value is ever parsed
    /// by a shell.
    pub fn setup_argv(&self) -> Vec<Vec<String>> {
        let addr = format!("{}{}", self.tap_ip, self.mask_short);
        vec![
            // Clears a leftover device from a previous run; expected to fail
            // when there is none.
            vec!["link".into(), "del".into(), self.tap_name.clone()],
            vec![
                "tuntap".into(),
                "add".into(),
                "dev".into(),
                self.tap_name.clone(),
                "mode".into(),
                "tap".into(),
            ],
            vec![
                "addr".into(),
                "add".into(),
                addr,
                "dev".into(),
                self.tap_name.clone(),
            ],
            vec![
                "link".into(),
                "set".into(),
                "dev".into(),
                self.tap_name.clone(),
                "up".into(),
            ],
        ]
    }

    /// The `ip` argument vector that tears the TAP device down.
    pub fn teardown_argv(&self) -> Vec<Vec<String>> {
        vec![vec!["link".into(), "del".into(), self.tap_name.clone()]]
    }

    /// Execute TAP setup on the host (requires root).
    ///
    /// Non-zero exits are logged, not returned — the first command is a
    /// best-effort cleanup that fails whenever there is nothing to clean up.
    /// Distinguishing that from a real failure is BUG-02, deferred to the
    /// Firecracker bring-up.
    pub async fn setup(&self) -> OsResult<()> {
        self.validate()?;

        for argv in self.setup_argv() {
            run_ip(&argv).await;
        }

        // Was `echo 1 > /proc/sys/net/ipv4/ip_forward`, which needed a shell
        // only for the redirect.
        if let Err(e) = tokio::fs::write("/proc/sys/net/ipv4/ip_forward", "1").await {
            tracing::warn!(error = %e, "failed to enable IPv4 forwarding");
        }

        Ok(())
    }

    /// Tear down the TAP device. Idempotent: deleting an absent device is fine.
    pub async fn teardown(&self) -> OsResult<()> {
        self.validate()?;

        for argv in self.teardown_argv() {
            run_ip(&argv).await;
        }
        Ok(())
    }
}

/// Run `ip` with the given arguments, logging rather than failing.
async fn run_ip(argv: &[String]) {
    match tokio::process::Command::new("ip").args(argv).output().await {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(args = ?argv, stderr = %stderr.trim(), "ip command returned non-zero");
        }
        Err(e) => {
            tracing::warn!(args = ?argv, error = %e, "failed to run ip");
        }
        Ok(_) => {}
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
    fn tap_setup_argv_shape() {
        let cfg = NetworkConfig::for_vm_index(0);
        let tap = TapSetup::from_config(&cfg);
        let setup = tap.setup_argv();
        assert_eq!(setup.len(), 4);
        assert_eq!(setup[0], vec!["link", "del", "tap0"]);
        assert_eq!(
            setup[2],
            vec!["addr", "add", "172.16.0.1/30", "dev", "tap0"]
        );
        assert_eq!(tap.teardown_argv(), vec![vec!["link", "del", "tap0"]]);
    }

    #[test]
    fn generated_configs_validate() {
        for index in [0u32, 1, 5, 63, 64, 255] {
            let cfg = NetworkConfig::for_vm_index(index);
            cfg.validate().unwrap();
            TapSetup::from_config(&cfg).validate().unwrap();
        }
    }

    #[test]
    fn rejects_tampered_tap_name() {
        for bad in ["tap0; rm -rf /", "eth0", "tap", "", "tap0 up", "../tap0"] {
            let mut cfg = NetworkConfig::for_vm_index(0);
            cfg.host_dev_name = bad.to_string();
            assert!(cfg.validate().is_err(), "expected {bad:?} to be rejected");

            let tap = TapSetup::from_config(&cfg);
            assert!(tap.validate().is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn rejects_tampered_addresses() {
        let mut cfg = NetworkConfig::for_vm_index(0);
        cfg.gateway_ip = "$(id)".to_string();
        assert!(cfg.validate().is_err());

        let mut cfg = NetworkConfig::for_vm_index(0);
        cfg.guest_ip = "not-an-ip/30".to_string();
        assert!(cfg.validate().is_err());

        let mut cfg = NetworkConfig::for_vm_index(0);
        cfg.mask_short = "/99".to_string();
        assert!(cfg.validate().is_err());

        let mut cfg = NetworkConfig::for_vm_index(0);
        cfg.mask_short = " dev eth0".to_string();
        assert!(cfg.validate().is_err());
    }

    #[tokio::test]
    async fn teardown_refuses_tampered_tap_name() {
        let mut cfg = NetworkConfig::for_vm_index(0);
        cfg.host_dev_name = "tap0 && reboot".to_string();
        let tap = TapSetup::from_config(&cfg);
        assert!(tap.setup().await.is_err());
        assert!(tap.teardown().await.is_err());
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
