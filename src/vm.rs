use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{OsError, OsResult};
use crate::network::NetworkConfig;
use crate::resource::ResourceLimits;
use crate::storage::VmStorage;

/// Unique VM identifier.
pub type VmId = String;

/// Runtime state of a microVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    /// VM config created, not yet started.
    Created,
    /// Firecracker process is booting.
    Booting,
    /// VM is running.
    Running,
    /// VM is paused (Firecracker Pause action).
    Paused,
    /// VM is stopping.
    Stopping,
    /// VM has stopped.
    Stopped,
    /// VM encountered an error.
    Failed,
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Booting => write!(f, "booting"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::Stopping => write!(f, "stopping"),
            Self::Stopped => write!(f, "stopped"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// Full VM configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    /// Unique VM identifier.
    pub vm_id: VmId,
    /// Human-readable VM name.
    pub name: String,
    /// Resource limits (CPU, memory, storage, network).
    pub resources: ResourceLimits,
    /// Network configuration.
    pub network: NetworkConfig,
    /// Kernel image path.
    pub kernel_image_path: PathBuf,
    /// Kernel boot arguments.
    pub boot_args: String,
    /// Path to Firecracker binary.
    pub firecracker_bin: PathBuf,
}

impl VmConfig {
    /// Create a new VM config with defaults and generated ID.
    pub fn new(name: impl Into<String>) -> Self {
        let vm_id = format!("vm-{}", Uuid::new_v4());
        Self {
            vm_id,
            name: name.into(),
            resources: ResourceLimits::default(),
            network: NetworkConfig::for_vm_index(0),
            kernel_image_path: PathBuf::from("vmlinux"),
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".to_string(),
            firecracker_bin: PathBuf::from("firecracker"),
        }
    }

    pub fn with_resources(mut self, resources: ResourceLimits) -> Self {
        self.resources = resources;
        self
    }

    pub fn with_network(mut self, network: NetworkConfig) -> Self {
        self.network = network;
        self
    }

    pub fn with_kernel(mut self, path: impl Into<PathBuf>) -> Self {
        self.kernel_image_path = path.into();
        self
    }

    pub fn with_firecracker_bin(mut self, path: impl Into<PathBuf>) -> Self {
        self.firecracker_bin = path.into();
        self
    }

    /// Generate the Firecracker machine-config API payload.
    pub fn machine_config_json(&self) -> serde_json::Value {
        serde_json::json!({
            "vcpu_count": self.resources.cpu.vcpu_count,
            "mem_size_mib": self.resources.memory.size_mib,
        })
    }

    /// Generate the full Firecracker boot-source API payload.
    pub fn boot_source_json(&self, net_boot_args: &str) -> serde_json::Value {
        let mut args = self.boot_args.clone();
        if !net_boot_args.is_empty() {
            args.push(' ');
            args.push_str(net_boot_args);
        }
        serde_json::json!({
            "kernel_image_path": self.kernel_image_path.to_string_lossy(),
            "boot_args": args,
        })
    }
}

/// Represents a live or defined microVM instance.
#[derive(Debug)]
pub struct MicroVm {
    pub config: VmConfig,
    pub state: VmState,
    /// PID of the Firecracker process (set after boot).
    pub pid: Option<u32>,
    /// Firecracker API socket path.
    pub socket_path: PathBuf,
    /// Log file path.
    pub log_path: PathBuf,
}

impl MicroVm {
    pub fn new(config: VmConfig, storage: &VmStorage) -> Self {
        let socket_path = storage.socket_path(&config.vm_id);
        let log_path = storage.log_path(&config.vm_id);
        Self {
            config,
            state: VmState::Created,
            pid: None,
            socket_path,
            log_path,
        }
    }

    /// Transition to a new state, validating the transition is legal.
    pub fn transition(&mut self, new_state: VmState) -> OsResult<()> {
        let valid = matches!(
            (self.state, new_state),
            (VmState::Created, VmState::Booting)
                | (VmState::Booting, VmState::Running)
                | (VmState::Booting, VmState::Failed)
                | (VmState::Running, VmState::Paused)
                | (VmState::Running, VmState::Stopping)
                | (VmState::Running, VmState::Failed)
                | (VmState::Paused, VmState::Running)
                | (VmState::Paused, VmState::Stopping)
                | (VmState::Stopping, VmState::Stopped)
                | (VmState::Stopping, VmState::Failed)
        );

        if !valid {
            return Err(OsError::InvalidState {
                current: self.state.to_string(),
                operation: format!("transition to {}", new_state),
            });
        }

        self.state = new_state;
        Ok(())
    }

    /// Start the Firecracker process and configure the VM via the API socket.
    pub async fn start(&mut self, storage: &VmStorage) -> OsResult<()> {
        self.transition(VmState::Booting)?;
        storage.prepare_vm_dirs(&self.config.vm_id).await?;

        // Clean up stale socket.
        if self.socket_path.exists() {
            tokio::fs::remove_file(&self.socket_path).await?;
        }

        // Spawn Firecracker process.
        let child = tokio::process::Command::new(&self.config.firecracker_bin)
            .arg("--api-sock")
            .arg(&self.socket_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| OsError::Firecracker(format!("failed to spawn: {}", e)))?;

        self.pid = child.id();

        // Wait for socket to appear.
        self.wait_for_socket(5).await?;

        // Configure VM via Firecracker REST API.
        self.configure_machine().await?;
        self.configure_boot_source().await?;
        self.configure_rootfs(storage).await?;
        self.configure_network().await?;

        // Start the VM.
        self.send_action("InstanceStart").await?;
        self.transition(VmState::Running)?;

        tracing::info!(
            vm_id = %self.config.vm_id,
            pid = ?self.pid,
            "microVM started"
        );

        Ok(())
    }

    /// Stop the microVM by sending InstanceHalt.
    pub async fn stop(&mut self) -> OsResult<()> {
        self.transition(VmState::Stopping)?;

        if let Err(e) = self.send_action("SendCtrlAltDel").await {
            tracing::warn!(vm_id = %self.config.vm_id, error = %e, "graceful stop failed, killing");
            self.kill()?;
        }

        // Wait a bit for process to exit, then force-kill if needed.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let _ = self.kill();

        self.transition(VmState::Stopped)?;
        tracing::info!(vm_id = %self.config.vm_id, "microVM stopped");
        Ok(())
    }

    /// Pause the microVM.
    pub async fn pause(&mut self) -> OsResult<()> {
        self.send_action("Pause").await?;
        self.transition(VmState::Paused)
    }

    /// Resume a paused microVM.
    pub async fn resume(&mut self) -> OsResult<()> {
        self.send_action("Resume").await?;
        self.transition(VmState::Running)
    }

    /// Force-kill the Firecracker process.
    fn kill(&self) -> OsResult<()> {
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        Ok(())
    }

    async fn wait_for_socket(&self, timeout_secs: u64) -> OsResult<()> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(timeout_secs);

        while tokio::time::Instant::now() < deadline {
            if self.socket_path.exists() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        Err(OsError::Timeout(self.config.vm_id.clone()))
    }

    async fn api_put(&self, path: &str, body: &serde_json::Value) -> OsResult<()> {
        let socket = self.socket_path.to_string_lossy().to_string();
        let body_str = serde_json::to_string(body)?;

        let output = tokio::process::Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "-X", "PUT",
                "--unix-socket", &socket,
                "--data", &body_str,
                "-H", "Content-Type: application/json",
                &format!("http://localhost{}", path),
            ])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(OsError::FirecrackerApi {
                status: output.status.code().unwrap_or(-1) as u16,
                body: stderr.to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("\"fault_message\"") {
            return Err(OsError::FirecrackerApi {
                status: 400,
                body: stdout.to_string(),
            });
        }

        Ok(())
    }

    async fn configure_machine(&self) -> OsResult<()> {
        let payload = self.config.machine_config_json();
        self.api_put("/machine-config", &payload).await
    }

    async fn configure_boot_source(&self) -> OsResult<()> {
        let net_args = self.config.network.kernel_boot_args();
        let payload = self.config.boot_source_json(&net_args);
        self.api_put("/boot-source", &payload).await
    }

    async fn configure_rootfs(&self, storage: &VmStorage) -> OsResult<()> {
        let drive = storage.rootfs_drive_config(&self.config.vm_id, None);
        let payload = VmStorage::drive_to_firecracker_json(&drive);
        self.api_put("/drives/rootfs", &payload).await
    }

    async fn configure_network(&self) -> OsResult<()> {
        let payload = self.config.network.to_firecracker_json();
        let path = format!("/network-interfaces/{}", self.config.network.iface_id);
        self.api_put(&path, &payload).await
    }

    async fn send_action(&self, action_type: &str) -> OsResult<()> {
        let payload = serde_json::json!({ "action_type": action_type });
        self.api_put("/actions", &payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_config_generates_id() {
        let config = VmConfig::new("test-vm");
        assert!(config.vm_id.starts_with("vm-"));
        assert_eq!(config.name, "test-vm");
    }

    #[test]
    fn machine_config_json() {
        let config = VmConfig::new("test").with_resources(ResourceLimits::medium());
        let json = config.machine_config_json();
        assert_eq!(json["vcpu_count"], 2);
        assert_eq!(json["mem_size_mib"], 512);
    }

    #[test]
    fn boot_source_json_with_net_args() {
        let config = VmConfig::new("test");
        let json = config.boot_source_json("ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off");
        let args = json["boot_args"].as_str().unwrap();
        assert!(args.contains("console=ttyS0"));
        assert!(args.contains("172.16.0.2"));
    }

    #[test]
    fn vm_state_display() {
        assert_eq!(VmState::Running.to_string(), "running");
        assert_eq!(VmState::Stopped.to_string(), "stopped");
    }

    #[test]
    fn valid_state_transitions() {
        let storage = VmStorage::new("/tmp/test");
        let config = VmConfig::new("test");
        let mut vm = MicroVm::new(config, &storage);

        assert!(vm.transition(VmState::Booting).is_ok());
        assert!(vm.transition(VmState::Running).is_ok());
        assert!(vm.transition(VmState::Paused).is_ok());
        assert!(vm.transition(VmState::Running).is_ok());
        assert!(vm.transition(VmState::Stopping).is_ok());
        assert!(vm.transition(VmState::Stopped).is_ok());
    }

    #[test]
    fn invalid_state_transition() {
        let storage = VmStorage::new("/tmp/test");
        let config = VmConfig::new("test");
        let mut vm = MicroVm::new(config, &storage);

        // Cannot go from Created directly to Running.
        assert!(vm.transition(VmState::Running).is_err());
    }

    #[test]
    fn vm_state_serialization_roundtrip() {
        for state in [
            VmState::Created,
            VmState::Booting,
            VmState::Running,
            VmState::Paused,
            VmState::Stopping,
            VmState::Stopped,
            VmState::Failed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let restored: VmState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, restored);
        }
    }

    #[test]
    fn vm_config_builder_chain() {
        let config = VmConfig::new("chain")
            .with_resources(ResourceLimits::large())
            .with_kernel("/custom/vmlinux")
            .with_firecracker_bin("/usr/bin/firecracker");

        assert_eq!(config.resources.cpu.vcpu_count, 4);
        assert_eq!(
            config.kernel_image_path,
            PathBuf::from("/custom/vmlinux")
        );
        assert_eq!(
            config.firecracker_bin,
            PathBuf::from("/usr/bin/firecracker")
        );
    }
}
