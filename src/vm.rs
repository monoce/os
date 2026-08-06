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

/// Does a `/proc/<pid>/cmdline` belong to *this* VM's Firecracker process?
///
/// This is the guard that makes signalling a persisted PID safe. A PID read
/// from a file outlives the process it named: by the time `destroy` runs, the
/// number may have been recycled onto something else entirely, and `monoce-os`
/// runs as root. So the answer is "no" unless the cmdline positively identifies
/// Firecracker serving *this VM's* API socket.
///
/// `/proc/<pid>/cmdline` is the raw argv, NUL-separated.
pub fn cmdline_matches_vm(cmdline: &str, socket_path: &str) -> bool {
    if socket_path.is_empty() {
        return false;
    }

    let argv: Vec<&str> = cmdline.split('\0').filter(|s| !s.is_empty()).collect();
    let Some(exe) = argv.first() else {
        // Empty cmdline: a zombie, or a kernel thread. Not ours.
        return false;
    };

    let is_firecracker = std::path::Path::new(exe)
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f == "firecracker");
    if !is_firecracker {
        return false;
    }

    // Both spellings, so a Firecracker invoked as `--api-sock=<path>` by some
    // future caller is still recognised as ours rather than silently skipped.
    argv.windows(2)
        .any(|w| w[0] == "--api-sock" && w[1] == socket_path)
        || argv
            .iter()
            .any(|a| a.strip_prefix("--api-sock=") == Some(socket_path))
}

/// Read `/proc/<pid>/cmdline`, or `None` where there is no procfs.
///
/// Only Linux hosts run Firecracker; on every other platform this returns
/// `None`, which callers treat as "cannot confirm" and therefore "do not
/// signal".
fn read_proc_cmdline(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
        Some(String::from_utf8_lossy(&raw).into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Send a signal, treating "no such process" as success.
fn signal_pid(pid: u32, sig: i32) -> OsResult<()> {
    let ret = unsafe { libc::kill(pid as i32, sig) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH means the process is already gone — the outcome we wanted.
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(OsError::Io(err));
        }
    }
    Ok(())
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
    /// Path to the persisted PID file, so a later process can find the PID.
    pid_path: PathBuf,
    /// Handle to the spawned Firecracker process, held only for the lifetime of
    /// the spawning call so a failed create can kill exactly the child it
    /// started. It is deliberately *not* used for the normal stop path: the VM
    /// outlives this process, and `destroy` runs in a different one.
    child: Option<tokio::process::Child>,
}

impl MicroVm {
    pub fn new(config: VmConfig, storage: &VmStorage) -> Self {
        let socket_path = storage.socket_path(&config.vm_id);
        let log_path = storage.log_path(&config.vm_id);
        let pid_path = storage.pid_path(&config.vm_id);
        Self {
            config,
            state: VmState::Created,
            pid: None,
            socket_path,
            log_path,
            pid_path,
            child: None,
        }
    }

    /// The persisted PID, but only if the process it names is still this VM's
    /// Firecracker.
    ///
    /// Returns `None` on any doubt at all — no PID file, unreadable,
    /// unparseable, process gone, cmdline mismatch, or no procfs to check
    /// against. Callers must treat `None` as "do not signal".
    pub fn validated_pid(&self) -> Option<u32> {
        let pid: u32 = std::fs::read_to_string(&self.pid_path)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        if pid == 0 {
            return None;
        }

        let cmdline = read_proc_cmdline(pid)?;
        let socket = self.socket_path.to_str()?;
        if cmdline_matches_vm(&cmdline, socket) {
            Some(pid)
        } else {
            tracing::warn!(
                vm_id = %self.config.vm_id,
                pid,
                "persisted PID does not look like this VM's Firecracker; refusing to signal it"
            );
            None
        }
    }

    /// Kill the child this process spawned, if any.
    ///
    /// Safe to call unconditionally: it signals a `Child` handle we still own,
    /// so there is no window in which the PID could have been recycled.
    pub async fn kill_spawned_child(&mut self) {
        if let Some(mut child) = self.child.take()
            && let Err(e) = child.kill().await
        {
            tracing::warn!(
                vm_id = %self.config.vm_id,
                error = %e,
                "failed to kill spawned Firecracker child"
            );
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
        //
        // `kill_on_drop` is deliberately NOT set. Every `monoce-os` invocation
        // is a separate short-lived process (CLAUDE.md gotcha 7), so the child
        // handle is dropped moments after a *successful* boot — killing on drop
        // would tear down every VM the moment `create` returned. The failure
        // paths are covered explicitly by `kill_spawned_child`, which signals a
        // handle we still own rather than a number that may have been recycled.
        let child = tokio::process::Command::new(&self.config.firecracker_bin)
            .arg("--api-sock")
            .arg(&self.socket_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| OsError::Firecracker(format!("failed to spawn: {}", e)))?;

        self.pid = child.id();
        self.child = Some(child);

        // Persist the PID before anything else can fail: a later `destroy`
        // runs in a different process and has no other way to find it.
        if let Some(pid) = self.pid {
            tokio::fs::write(&self.pid_path, pid.to_string()).await?;
        }

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

    /// Stop the microVM, escalating until the process is confirmed gone.
    ///
    /// `SendCtrlAltDel` → `SIGTERM` → `SIGKILL`, each followed by a bounded
    /// wait. A minimal Alpine rootfs without `acpid` ignores Ctrl-Alt-Del
    /// entirely, so the signal escalation is the path that actually works.
    ///
    /// Every signal targets a freshly re-validated PID; if validation ever
    /// fails the escalation stops rather than signalling blind.
    pub async fn stop(&mut self) -> OsResult<()> {
        self.transition(VmState::Stopping)?;

        // On the destroy path this `MicroVm` was rebuilt from disk, so the
        // in-memory PID is None. The persisted one is the only one there is.
        if self.pid.is_none() {
            self.pid = self.validated_pid();
        }

        if let Err(e) = self.send_action("SendCtrlAltDel").await {
            tracing::warn!(vm_id = %self.config.vm_id, error = %e, "graceful stop failed, escalating");
        }

        for (sig, name, wait_secs) in [
            (0, "graceful", 3u64),
            (libc::SIGTERM, "SIGTERM", 3),
            (libc::SIGKILL, "SIGKILL", 2),
        ] {
            if sig != 0 {
                // Re-validate on every round: this is the only thing standing
                // between a stale PID file and a root-signalled bystander.
                let Some(pid) = self.validated_pid() else {
                    break;
                };
                tracing::warn!(vm_id = %self.config.vm_id, pid, signal = name, "escalating stop");
                signal_pid(pid, sig)?;
            }

            if self.wait_for_exit(wait_secs).await {
                self.transition(VmState::Stopped)?;
                tracing::info!(vm_id = %self.config.vm_id, "microVM stopped");
                return Ok(());
            }
        }

        if let Some(pid) = self.validated_pid() {
            return Err(OsError::Firecracker(format!(
                "VM {} still running as pid {} after SIGKILL",
                self.config.vm_id, pid
            )));
        }

        self.transition(VmState::Stopped)?;
        tracing::info!(vm_id = %self.config.vm_id, "microVM stopped");
        Ok(())
    }

    /// Poll until the VM's Firecracker process can no longer be confirmed
    /// alive. Returns `false` if it is still confirmably alive at the deadline.
    async fn wait_for_exit(&self, timeout_secs: u64) -> bool {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        loop {
            if self.validated_pid().is_none() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
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
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let body_str = serde_json::to_string(body)?;
        let request = format!(
            "PUT {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccept: */*\r\n\r\n{}",
            path, body_str.len(), body_str
        );

        let mut stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| OsError::Firecracker(format!("failed to connect to API socket: {}", e)))?;

        stream.write_all(request.as_bytes()).await?;

        // Read HTTP response incrementally (Firecracker uses Connection: keep-alive,
        // so read_to_end would block forever).
        let mut reader = BufReader::new(&mut stream);

        // Read status line
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await?;
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(500);

        // Read headers to find Content-Length
        let mut content_length: usize = 0;
        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line).await?;
            if header_line.trim().is_empty() {
                break; // End of headers
            }
            if let Some(val) = header_line.strip_prefix("Content-Length: ") {
                content_length = val.trim().parse().unwrap_or(0);
            }
        }

        // Read response body
        let mut response_body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut response_body).await?;
        }
        let response_body = String::from_utf8_lossy(&response_body);

        if status >= 400 {
            return Err(OsError::FirecrackerApi {
                status,
                body: response_body.to_string(),
            });
        }

        if response_body.contains("\"fault_message\"") {
            return Err(OsError::FirecrackerApi {
                status: 400,
                body: response_body.to_string(),
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

    /// Build the NUL-separated argv that `/proc/<pid>/cmdline` would hold.
    fn proc_cmdline(argv: &[&str]) -> String {
        let mut s = argv.join("\0");
        s.push('\0'); // procfs terminates the last argument too
        s
    }

    const SOCK: &str = "/var/lib/monoce-os/vms/vm-1/firecracker.sock";

    #[test]
    fn cmdline_accepts_this_vms_firecracker() {
        assert!(cmdline_matches_vm(
            &proc_cmdline(&["/usr/bin/firecracker", "--api-sock", SOCK]),
            SOCK
        ));
        // Bare binary name, and the `=` spelling.
        assert!(cmdline_matches_vm(
            &proc_cmdline(&["firecracker", "--api-sock", SOCK]),
            SOCK
        ));
        assert!(cmdline_matches_vm(
            &proc_cmdline(&["firecracker", &format!("--api-sock={}", SOCK)]),
            SOCK
        ));
    }

    #[test]
    fn cmdline_rejects_another_vms_firecracker() {
        // The recycled-PID hazard this guard exists for: a real Firecracker,
        // but serving a different VM.
        let other = "/var/lib/monoce-os/vms/vm-2/firecracker.sock";
        assert!(!cmdline_matches_vm(
            &proc_cmdline(&["/usr/bin/firecracker", "--api-sock", other]),
            SOCK
        ));
    }

    #[test]
    fn cmdline_rejects_unrelated_processes() {
        for argv in [
            vec!["/usr/sbin/sshd", "-D"],
            vec!["/bin/bash"],
            // Right socket, wrong program — a grep-for-substring check would
            // have accepted this one.
            vec!["/bin/cat", SOCK],
            // Firecracker without the socket argument at all.
            vec!["/usr/bin/firecracker"],
            // Socket path present but not as the value of --api-sock.
            vec!["/usr/bin/firecracker", "--config-file", SOCK],
            // A program merely *named* like a suffix match.
            vec!["/usr/bin/not-firecracker", "--api-sock", SOCK],
        ] {
            assert!(
                !cmdline_matches_vm(&proc_cmdline(&argv), SOCK),
                "expected {argv:?} to be rejected"
            );
        }
    }

    #[test]
    fn cmdline_rejects_empty_inputs() {
        // A zombie has an empty cmdline.
        assert!(!cmdline_matches_vm("", SOCK));
        assert!(!cmdline_matches_vm("\0\0", SOCK));
        // An unknown socket path must never match anything.
        assert!(!cmdline_matches_vm(
            &proc_cmdline(&["firecracker", "--api-sock", SOCK]),
            ""
        ));
    }

    #[test]
    fn validated_pid_is_none_without_a_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = VmStorage::new(tmp.path());
        let vm = MicroVm::new(VmConfig::new("test"), &storage);
        assert!(vm.validated_pid().is_none());
    }

    #[tokio::test]
    async fn validated_pid_is_none_for_garbage_and_stale_pid_files() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = VmStorage::new(tmp.path());
        let config = VmConfig::new("test");
        let vm_id = config.vm_id.clone();
        let vm = MicroVm::new(config, &storage);
        storage.prepare_vm_dirs(&vm_id).await.unwrap();

        for contents in [
            "not-a-number",
            "",
            "0",
            "-1",
            // Almost certainly not a live Firecracker, and on a host where it
            // *is* live it is not serving this VM's socket.
            "1",
        ] {
            tokio::fs::write(storage.pid_path(&vm_id), contents)
                .await
                .unwrap();
            assert!(
                vm.validated_pid().is_none(),
                "expected pid file {contents:?} to yield no signalable PID"
            );
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
