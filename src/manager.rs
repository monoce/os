use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{OsError, OsResult};
use crate::image::{ImageRegistry, VmImage};
use crate::network::{NetworkConfig, TapSetup};
use crate::resource::{HostCapacity, ResourceLimits};
use crate::storage::VmStorage;
use crate::vm::{MicroVm, VmConfig, VmId, VmState};

/// File-based lock to prevent race conditions when multiple processes
/// create VMs simultaneously.
struct VmIndexLock {
    _file: std::fs::File,
}

impl VmIndexLock {
    fn acquire(storage: &VmStorage) -> OsResult<Self> {
        let lock_path = storage.vms_dir().join(".vm-index.lock");
        std::fs::create_dir_all(storage.vms_dir())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        // Use flock advisory lock via libc
        unsafe {
            let ret = libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX,
            );
            if ret != 0 {
                return Err(OsError::Io(std::io::Error::last_os_error()));
            }
        }
        Ok(Self { _file: file })
    }
}

/// Central manager for all microVMs on a host.
pub struct VmManager {
    vms: Arc<RwLock<HashMap<VmId, MicroVm>>>,
    storage: VmStorage,
    capacity: Arc<RwLock<HostCapacity>>,
    images: Arc<RwLock<ImageRegistry>>,
    next_vm_index: Arc<RwLock<u32>>,
}

impl VmManager {
    pub fn new(storage: VmStorage, capacity: HostCapacity) -> Self {
        // Determine next VM index from existing VMs on disk to avoid
        // IP/TAP collisions when the CLI is invoked as separate processes.
        let next_index = Self::detect_next_vm_index(&storage);

        Self {
            vms: Arc::new(RwLock::new(HashMap::new())),
            storage,
            capacity: Arc::new(RwLock::new(capacity)),
            images: Arc::new(RwLock::new(ImageRegistry::new())),
            next_vm_index: Arc::new(RwLock::new(next_index)),
        }
    }

    /// Scan existing VM configs on disk and find the highest vm_index used,
    /// so the next VM gets a unique network assignment.
    fn detect_next_vm_index(storage: &VmStorage) -> u32 {
        let vms_dir = storage.vms_dir();
        let mut max_index: Option<u32> = None;

        if let Ok(entries) = std::fs::read_dir(&vms_dir) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let config_path = entry.path().join("vm-config.json");
                if let Ok(data) = std::fs::read_to_string(&config_path) {
                    if let Ok(config) = serde_json::from_str::<serde_json::Value>(&data) {
                        // Derive the index from the TAP device name (tap0, tap1, ...)
                        if let Some(tap) = config.pointer("/network/host_dev_name").and_then(|v| v.as_str()) {
                            if let Some(idx_str) = tap.strip_prefix("tap") {
                                if let Ok(idx) = idx_str.parse::<u32>() {
                                    max_index = Some(max_index.map_or(idx, |m: u32| m.max(idx)));
                                }
                            }
                        }
                    }
                }
            }
        }

        max_index.map_or(0, |m| m + 1)
    }

    /// Register a VM image (kernel + rootfs).
    pub async fn register_image(&self, image: VmImage) {
        self.images.write().await.register(image);
    }

    /// Create and start a new microVM.
    pub async fn create_vm(
        &self,
        name: &str,
        image_name: &str,
        resources: ResourceLimits,
    ) -> OsResult<VmId> {
        // Check image exists.
        let images = self.images.read().await;
        let image = images
            .get(image_name)
            .ok_or_else(|| OsError::Image(format!("image '{}' not found", image_name)))?;
        image.validate()?;

        // Reserve host resources.
        self.capacity.write().await.reserve(&resources)?;

        // Allocate network with file lock to prevent race conditions
        // between concurrent processes.
        let _lock = VmIndexLock::acquire(&self.storage)?;
        let vm_index = {
            // Re-detect from disk under lock to get accurate value
            let fresh_index = Self::detect_next_vm_index(&self.storage);
            let mut idx = self.next_vm_index.write().await;
            let current = fresh_index.max(*idx);
            *idx = current + 1;
            current
        };
        let network = NetworkConfig::for_vm_index(vm_index)
            .with_limits(resources.network.clone());

        // Build config.
        let config = VmConfig::new(name)
            .with_resources(resources.clone())
            .with_kernel(&image.kernel_path)
            .with_network(network);

        let vm_id = config.vm_id.clone();

        // Copy rootfs for this VM.
        self.storage
            .copy_base_image(&vm_id, &image.rootfs_path)
            .await?;

        // Create data disk if configured.
        if let Some(data_size) = resources.storage.data_disk_size_bytes {
            self.storage.create_data_disk(&vm_id, data_size).await?;
        }

        // Save config.
        let config_json = serde_json::to_string_pretty(&config)?;
        tokio::fs::write(self.storage.config_path(&vm_id), config_json).await?;

        // Setup TAP.
        let tap = TapSetup::from_config(&config.network);
        tap.setup().await?;

        // Create and start VM.
        let mut vm = MicroVm::new(config, &self.storage);
        vm.start(&self.storage).await?;

        self.vms.write().await.insert(vm_id.clone(), vm);

        tracing::info!(vm_id = %vm_id, name = %name, "VM created and started");
        Ok(vm_id)
    }

    /// Stop and destroy a microVM.
    pub async fn destroy_vm(&self, vm_id: &str) -> OsResult<()> {
        // Try to take the VM from the in-memory registry first.
        let mut vm = {
            let mut vms = self.vms.write().await;
            if let Some(vm) = vms.remove(vm_id) {
                vm
            } else {
                // Fall back to loading the VM definition from disk.
                let cfg_path = self.storage.config_path(vm_id);
                if !cfg_path.exists() {
                    return Err(OsError::VmNotFound(vm_id.to_string()));
                }
                let cfg_data = tokio::fs::read_to_string(&cfg_path).await?;
                let config: VmConfig = serde_json::from_str(&cfg_data)?;
                let mut vm = MicroVm::new(config, &self.storage);
                // If a Firecracker API socket exists, assume it's running so we can try to stop it.
                if self.storage.socket_path(vm_id).exists() {
                    vm.state = VmState::Running;
                }
                vm
            }
        };

        // Stop VM if running or paused.
        if vm.state == VmState::Running || vm.state == VmState::Paused {
            // Try to stop, but don't fail destroy if graceful stop fails.
            if let Err(e) = vm.stop().await {
                tracing::warn!(vm_id = %vm.config.vm_id, error = %e, "failed to stop VM during destroy; continuing");
            }
        }

        // Release host resources.
        self.capacity.write().await.release(&vm.config.resources);

        // Teardown TAP (idempotent).
        let tap = TapSetup::from_config(&vm.config.network);
        let _ = tap.teardown().await;

        // Cleanup storage.
        self.storage.cleanup_vm(vm_id).await?;

        tracing::info!(vm_id = %vm_id, "VM destroyed");
        Ok(())
    }

    /// Pause a running VM.
    pub async fn pause_vm(&self, vm_id: &str) -> OsResult<()> {
        let mut vms = self.vms.write().await;
        let vm = vms
            .get_mut(vm_id)
            .ok_or_else(|| OsError::VmNotFound(vm_id.to_string()))?;
        vm.pause().await
    }

    /// Resume a paused VM.
    pub async fn resume_vm(&self, vm_id: &str) -> OsResult<()> {
        let mut vms = self.vms.write().await;
        let vm = vms
            .get_mut(vm_id)
            .ok_or_else(|| OsError::VmNotFound(vm_id.to_string()))?;
        vm.resume().await
    }

    /// Get the state of a VM.
    pub async fn vm_state(&self, vm_id: &str) -> OsResult<VmState> {
        let vms = self.vms.read().await;
        let vm = vms
            .get(vm_id)
            .ok_or_else(|| OsError::VmNotFound(vm_id.to_string()))?;
        Ok(vm.state)
    }

    /// Get the config of a VM.
    pub async fn vm_config(&self, vm_id: &str) -> OsResult<VmConfig> {
        let vms = self.vms.read().await;
        let vm = vms
            .get(vm_id)
            .ok_or_else(|| OsError::VmNotFound(vm_id.to_string()))?;
        Ok(vm.config.clone())
    }

    /// List all VM IDs and their states.
    pub async fn list_vms(&self) -> Vec<(VmId, VmState)> {
        let vms = self.vms.read().await;
        vms.iter()
            .map(|(id, vm)| (id.clone(), vm.state))
            .collect()
    }

    /// Get current host capacity.
    pub async fn host_capacity(&self) -> HostCapacity {
        self.capacity.read().await.clone()
    }

    /// Get count of VMs in each state.
    pub async fn vm_stats(&self) -> HashMap<String, usize> {
        let vms = self.vms.read().await;
        let mut stats: HashMap<String, usize> = HashMap::new();
        for vm in vms.values() {
            *stats.entry(vm.state.to_string()).or_default() += 1;
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> VmManager {
        let storage = VmStorage::new("/tmp/monoce-os-test");
        let capacity = HostCapacity {
            total_vcpus: 8,
            available_vcpus: 8,
            total_memory_mib: 8192,
            available_memory_mib: 8192,
            total_disk_bytes: 100 * 1024 * 1024 * 1024,
            available_disk_bytes: 100 * 1024 * 1024 * 1024,
        };
        VmManager::new(storage, capacity)
    }

    #[tokio::test]
    async fn list_vms_empty() {
        let mgr = make_manager();
        assert!(mgr.list_vms().await.is_empty());
    }

    #[tokio::test]
    async fn vm_stats_empty() {
        let mgr = make_manager();
        assert!(mgr.vm_stats().await.is_empty());
    }

    #[tokio::test]
    async fn host_capacity_readable() {
        let mgr = make_manager();
        let cap = mgr.host_capacity().await;
        assert_eq!(cap.total_vcpus, 8);
        assert_eq!(cap.available_vcpus, 8);
    }

    #[tokio::test]
    async fn create_vm_fails_without_image() {
        let mgr = make_manager();
        let result = mgr
            .create_vm("test", "nonexistent", ResourceLimits::small())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn destroy_vm_not_found() {
        let mgr = make_manager();
        let result = mgr.destroy_vm("fake-id").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn register_image() {
        let mgr = make_manager();
        let image = VmImage::new("test-img", "/vmlinux", "/rootfs.ext4");
        mgr.register_image(image).await;
        let images = mgr.images.read().await;
        assert!(images.get("test-img").is_some());
    }
}
