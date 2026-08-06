use serde::{Deserialize, Serialize};

/// CPU resource limits for a microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuLimits {
    /// Number of vCPUs allocated to the VM.
    pub vcpu_count: u8,
    /// CPU quota in microseconds per 100ms period (CFS).
    /// None = no throttling beyond vcpu_count.
    pub cfs_quota_us: Option<u64>,
    /// CPU period in microseconds (default 100_000).
    pub cfs_period_us: u64,
}

impl Default for CpuLimits {
    fn default() -> Self {
        Self {
            vcpu_count: 1,
            cfs_quota_us: None,
            cfs_period_us: 100_000,
        }
    }
}

/// Memory resource limits for a microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLimits {
    /// Memory allocated to the VM in MiB.
    pub size_mib: u64,
    /// Hard memory limit for the cgroup in bytes.
    /// Defaults to size_mib * 1024 * 1024.
    pub cgroup_limit_bytes: Option<u64>,
    /// Swap limit in bytes. None = no swap.
    pub swap_limit_bytes: Option<u64>,
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            size_mib: 128,
            cgroup_limit_bytes: None,
            swap_limit_bytes: Some(0),
        }
    }
}

/// Storage / disk I/O limits for a microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageLimits {
    /// Maximum rootfs size in bytes.
    pub rootfs_size_bytes: u64,
    /// Additional data disk size in bytes. None = no data disk.
    pub data_disk_size_bytes: Option<u64>,
    /// Read rate limiter: bytes per second.
    pub read_rate_bps: Option<u64>,
    /// Write rate limiter: bytes per second.
    pub write_rate_bps: Option<u64>,
    /// Read operations per second.
    pub read_ops_ps: Option<u64>,
    /// Write operations per second.
    pub write_ops_ps: Option<u64>,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            rootfs_size_bytes: 512 * 1024 * 1024, // 512 MiB
            data_disk_size_bytes: None,
            read_rate_bps: None,
            write_rate_bps: None,
            read_ops_ps: None,
            write_ops_ps: None,
        }
    }
}

/// Network I/O limits for a microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkLimits {
    /// Inbound bandwidth limit in bytes per second.
    pub rx_rate_bps: Option<u64>,
    /// Outbound bandwidth limit in bytes per second.
    pub tx_rate_bps: Option<u64>,
    /// Inbound packet rate limit (packets per second).
    pub rx_rate_pps: Option<u64>,
    /// Outbound packet rate limit (packets per second).
    pub tx_rate_pps: Option<u64>,
}

impl Default for NetworkLimits {
    fn default() -> Self {
        Self {
            rx_rate_bps: None,
            tx_rate_bps: None,
            rx_rate_pps: None,
            tx_rate_pps: None,
        }
    }
}

/// Complete resource limits for a microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub cpu: CpuLimits,
    pub memory: MemoryLimits,
    pub storage: StorageLimits,
    pub network: NetworkLimits,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu: CpuLimits::default(),
            memory: MemoryLimits::default(),
            storage: StorageLimits::default(),
            network: NetworkLimits::default(),
        }
    }
}

impl ResourceLimits {
    /// Create limits for a small VM (1 vCPU, 128 MiB RAM, 512 MiB disk).
    pub fn small() -> Self {
        Self::default()
    }

    /// Create limits for a medium VM (2 vCPUs, 512 MiB RAM, 2 GiB disk).
    pub fn medium() -> Self {
        Self {
            cpu: CpuLimits {
                vcpu_count: 2,
                ..CpuLimits::default()
            },
            memory: MemoryLimits {
                size_mib: 512,
                ..MemoryLimits::default()
            },
            storage: StorageLimits {
                rootfs_size_bytes: 2 * 1024 * 1024 * 1024,
                ..StorageLimits::default()
            },
            network: NetworkLimits::default(),
        }
    }

    /// Create limits for a large VM (4 vCPUs, 2048 MiB RAM, 8 GiB disk).
    pub fn large() -> Self {
        Self {
            cpu: CpuLimits {
                vcpu_count: 4,
                ..CpuLimits::default()
            },
            memory: MemoryLimits {
                size_mib: 2048,
                ..MemoryLimits::default()
            },
            storage: StorageLimits {
                rootfs_size_bytes: 8 * 1024 * 1024 * 1024,
                ..StorageLimits::default()
            },
            network: NetworkLimits::default(),
        }
    }

    /// Check whether the requested resources fit within a host capacity.
    pub fn fits_in(&self, host: &HostCapacity) -> bool {
        self.cpu.vcpu_count as u32 <= host.available_vcpus
            && self.memory.size_mib <= host.available_memory_mib
            && self.storage.rootfs_size_bytes <= host.available_disk_bytes
    }
}

/// Represents the available capacity on a host machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCapacity {
    pub total_vcpus: u32,
    pub available_vcpus: u32,
    pub total_memory_mib: u64,
    pub available_memory_mib: u64,
    pub total_disk_bytes: u64,
    pub available_disk_bytes: u64,
}

impl HostCapacity {
    /// Build capacity for a host by subtracting every VM already on disk.
    ///
    /// Each `monoce-os` invocation is a separate process, so there is no
    /// in-memory reservation to inherit — availability has to be recovered
    /// from `vms/<id>/vm-config.json`. Deriving it rather than persisting a
    /// counter is self-healing: there is no tally that can drift from reality,
    /// and a removed VM directory frees its resources by construction.
    ///
    /// Unreadable or malformed configs are skipped with a warning: refusing to
    /// report capacity because one directory is corrupt would be worse than
    /// slightly overstating it.
    pub fn derive_from_dir(
        vms_dir: &std::path::Path,
        total_vcpus: u32,
        total_memory_mib: u64,
        total_disk_bytes: u64,
    ) -> Self {
        #[derive(Deserialize)]
        struct ResourcesOnly {
            resources: ResourceLimits,
        }

        let mut capacity = Self {
            total_vcpus,
            available_vcpus: total_vcpus,
            total_memory_mib,
            available_memory_mib: total_memory_mib,
            total_disk_bytes,
            available_disk_bytes: total_disk_bytes,
        };

        let Ok(entries) = std::fs::read_dir(vms_dir) else {
            // No VM directory yet: the host is genuinely idle.
            return capacity;
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let config_path = entry.path().join("vm-config.json");
            let Ok(data) = std::fs::read_to_string(&config_path) else {
                continue;
            };
            match serde_json::from_str::<ResourcesOnly>(&data) {
                Ok(parsed) => capacity.subtract(&parsed.resources),
                Err(e) => tracing::warn!(
                    path = %config_path.display(),
                    error = %e,
                    "skipping unparseable VM config while deriving host capacity"
                ),
            }
        }

        capacity
    }

    /// Subtract a VM's resources, saturating at zero.
    ///
    /// Saturating rather than erroring: a host that was overcommitted before
    /// this check existed should report "nothing available", not panic.
    fn subtract(&mut self, limits: &ResourceLimits) {
        self.available_vcpus = self
            .available_vcpus
            .saturating_sub(limits.cpu.vcpu_count as u32);
        self.available_memory_mib = self
            .available_memory_mib
            .saturating_sub(limits.memory.size_mib);
        self.available_disk_bytes = self
            .available_disk_bytes
            .saturating_sub(limits.storage.rootfs_size_bytes);
    }

    /// Reserve resources for a VM, returning an error if insufficient.
    pub fn reserve(&mut self, limits: &ResourceLimits) -> crate::OsResult<()> {
        if !limits.fits_in(self) {
            return Err(crate::OsError::ResourceLimitExceeded {
                resource: "host capacity".into(),
                limit: format!(
                    "vcpu={}, mem={}MiB, disk={}B",
                    self.available_vcpus, self.available_memory_mib, self.available_disk_bytes
                ),
                requested: format!(
                    "vcpu={}, mem={}MiB, disk={}B",
                    limits.cpu.vcpu_count, limits.memory.size_mib, limits.storage.rootfs_size_bytes
                ),
            });
        }
        self.available_vcpus -= limits.cpu.vcpu_count as u32;
        self.available_memory_mib -= limits.memory.size_mib;
        self.available_disk_bytes -= limits.storage.rootfs_size_bytes;
        Ok(())
    }

    /// Release resources previously reserved by a VM.
    pub fn release(&mut self, limits: &ResourceLimits) {
        self.available_vcpus =
            (self.available_vcpus + limits.cpu.vcpu_count as u32).min(self.total_vcpus);
        self.available_memory_mib =
            (self.available_memory_mib + limits.memory.size_mib).min(self.total_memory_mib);
        self.available_disk_bytes = (self.available_disk_bytes + limits.storage.rootfs_size_bytes)
            .min(self.total_disk_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_host() -> HostCapacity {
        HostCapacity {
            total_vcpus: 8,
            available_vcpus: 8,
            total_memory_mib: 8192,
            available_memory_mib: 8192,
            total_disk_bytes: 100 * 1024 * 1024 * 1024,
            available_disk_bytes: 100 * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn small_fits_in_host() {
        let host = make_host();
        assert!(ResourceLimits::small().fits_in(&host));
    }

    #[test]
    fn reserve_decreases_capacity() {
        let mut host = make_host();
        let limits = ResourceLimits::medium();
        host.reserve(&limits).unwrap();
        assert_eq!(host.available_vcpus, 6);
        assert_eq!(host.available_memory_mib, 8192 - 512);
    }

    #[test]
    fn release_restores_capacity() {
        let mut host = make_host();
        let limits = ResourceLimits::medium();
        host.reserve(&limits).unwrap();
        host.release(&limits);
        assert_eq!(host.available_vcpus, 8);
        assert_eq!(host.available_memory_mib, 8192);
    }

    #[test]
    fn reserve_fails_when_insufficient() {
        let mut host = make_host();
        host.available_vcpus = 1;
        let limits = ResourceLimits::medium();
        assert!(host.reserve(&limits).is_err());
    }

    #[test]
    fn preset_sizes_ascending() {
        let s = ResourceLimits::small();
        let m = ResourceLimits::medium();
        let l = ResourceLimits::large();
        assert!(s.cpu.vcpu_count < m.cpu.vcpu_count);
        assert!(m.cpu.vcpu_count < l.cpu.vcpu_count);
        assert!(s.memory.size_mib < m.memory.size_mib);
        assert!(m.memory.size_mib < l.memory.size_mib);
    }

    #[test]
    fn resource_limits_serialization_roundtrip() {
        let limits = ResourceLimits::medium();
        let json = serde_json::to_string(&limits).unwrap();
        let restored: ResourceLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.cpu.vcpu_count, limits.cpu.vcpu_count);
        assert_eq!(restored.memory.size_mib, limits.memory.size_mib);
    }

    #[test]
    fn cfs_quota_defaults_to_none() {
        let cpu = CpuLimits::default();
        assert!(cpu.cfs_quota_us.is_none());
        assert_eq!(cpu.cfs_period_us, 100_000);
    }

    #[test]
    fn swap_defaults_to_zero() {
        let mem = MemoryLimits::default();
        assert_eq!(mem.swap_limit_bytes, Some(0));
    }

    /// Write a `vms/<id>/vm-config.json` holding just the fields the derivation
    /// reads.
    fn write_vm_config(vms_dir: &std::path::Path, id: &str, limits: &ResourceLimits) {
        let dir = vms_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let config = serde_json::json!({
            "vm_id": id,
            "name": id,
            "resources": limits,
        });
        std::fs::write(
            dir.join("vm-config.json"),
            serde_json::to_string(&config).unwrap(),
        )
        .unwrap();
    }

    const TOTAL_DISK: u64 = 100 * 1024 * 1024 * 1024;

    #[test]
    fn derived_capacity_is_full_when_no_vms_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = HostCapacity::derive_from_dir(&tmp.path().join("vms"), 8, 8192, TOTAL_DISK);
        assert_eq!(cap.available_vcpus, 8);
        assert_eq!(cap.available_memory_mib, 8192);
        assert_eq!(cap.available_disk_bytes, TOTAL_DISK);
    }

    #[test]
    fn derived_capacity_subtracts_on_disk_vms() {
        let tmp = tempfile::tempdir().unwrap();
        let vms_dir = tmp.path().join("vms");
        let medium = ResourceLimits::medium(); // 2 vCPU, 512 MiB, 2 GiB
        let large = ResourceLimits::large(); // 4 vCPU, 2048 MiB, 8 GiB
        write_vm_config(&vms_dir, "vm-a", &medium);
        write_vm_config(&vms_dir, "vm-b", &large);

        let cap = HostCapacity::derive_from_dir(&vms_dir, 8, 8192, TOTAL_DISK);
        assert_eq!(cap.total_vcpus, 8);
        assert_eq!(cap.available_vcpus, 8 - 2 - 4);
        assert_eq!(cap.available_memory_mib, 8192 - 512 - 2048);
        assert_eq!(
            cap.available_disk_bytes,
            TOTAL_DISK
                - medium.storage.rootfs_size_bytes
                - large.storage.rootfs_size_bytes
        );
    }

    #[test]
    fn derived_capacity_skips_unparseable_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let vms_dir = tmp.path().join("vms");
        write_vm_config(&vms_dir, "vm-a", &ResourceLimits::medium());

        // A half-written config, and a directory with no config at all.
        std::fs::create_dir_all(vms_dir.join("vm-broken")).unwrap();
        std::fs::write(vms_dir.join("vm-broken").join("vm-config.json"), "{not json").unwrap();
        std::fs::create_dir_all(vms_dir.join("vm-empty")).unwrap();

        let cap = HostCapacity::derive_from_dir(&vms_dir, 8, 8192, TOTAL_DISK);
        assert_eq!(cap.available_vcpus, 6);
        assert_eq!(cap.available_memory_mib, 8192 - 512);
    }

    #[test]
    fn derived_capacity_saturates_when_overcommitted() {
        let tmp = tempfile::tempdir().unwrap();
        let vms_dir = tmp.path().join("vms");
        for id in ["vm-a", "vm-b", "vm-c"] {
            write_vm_config(&vms_dir, id, &ResourceLimits::large());
        }

        // Only 2 vCPUs and 1 GiB on the host, but 12 vCPUs already committed.
        let cap = HostCapacity::derive_from_dir(&vms_dir, 2, 1024, TOTAL_DISK);
        assert_eq!(cap.available_vcpus, 0);
        assert_eq!(cap.available_memory_mib, 0);
    }

    #[test]
    fn derived_capacity_rejects_a_new_vm_once_full() {
        let tmp = tempfile::tempdir().unwrap();
        let vms_dir = tmp.path().join("vms");
        write_vm_config(&vms_dir, "vm-a", &ResourceLimits::large());

        // This is the whole point of BUG-03: a second `create large` on a
        // 4-vCPU host must now fail rather than overcommit.
        let mut cap = HostCapacity::derive_from_dir(&vms_dir, 4, 8192, TOTAL_DISK);
        assert!(cap.reserve(&ResourceLimits::large()).is_err());
    }
}
