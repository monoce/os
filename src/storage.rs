use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use monoce_common::{Cid, Codec};
use crate::error::OsResult;

/// Manages on-disk storage for VM images, rootfs, and data disks.
/// Integrates with Monoce CAS for content-addressable image storage.
#[derive(Debug, Clone)]
pub struct VmStorage {
    /// Base directory for all VM storage.
    base_path: PathBuf,
}

/// Drive configuration for Firecracker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveConfig {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
    pub rate_limiter: Option<DriveRateLimiter>,
}

/// Rate limiter for a Firecracker block device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveRateLimiter {
    pub bandwidth: Option<TokenBucket>,
    pub ops: Option<TokenBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBucket {
    pub size: u64,
    pub one_time_burst: Option<u64>,
    pub refill_time: u64,
}

impl VmStorage {
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    /// Directory for a specific VM's storage.
    pub fn vm_dir(&self, vm_id: &str) -> PathBuf {
        self.base_path.join("vms").join(vm_id)
    }

    /// Path to the VM's rootfs image.
    pub fn rootfs_path(&self, vm_id: &str) -> PathBuf {
        self.vm_dir(vm_id).join("rootfs.ext4")
    }

    /// Path to the VM's data disk image.
    pub fn data_disk_path(&self, vm_id: &str) -> PathBuf {
        self.vm_dir(vm_id).join("data.ext4")
    }

    /// Path to the VM's socket file.
    pub fn socket_path(&self, vm_id: &str) -> PathBuf {
        self.vm_dir(vm_id).join("firecracker.sock")
    }

    /// Path to the VM's log file.
    pub fn log_path(&self, vm_id: &str) -> PathBuf {
        self.vm_dir(vm_id).join("firecracker.log")
    }

    /// Path to the VM's config JSON.
    pub fn config_path(&self, vm_id: &str) -> PathBuf {
        self.vm_dir(vm_id).join("vm-config.json")
    }

    /// Directory for cached kernel and base images.
    pub fn images_dir(&self) -> PathBuf {
        self.base_path.join("images")
    }

    /// Path for a kernel image.
    pub fn kernel_path(&self, kernel_name: &str) -> PathBuf {
        self.images_dir().join(kernel_name)
    }

    /// Ensure all required directories exist for a VM.
    pub async fn prepare_vm_dirs(&self, vm_id: &str) -> OsResult<()> {
        let vm_dir = self.vm_dir(vm_id);
        tokio::fs::create_dir_all(&vm_dir).await?;
        Ok(())
    }

    /// Create a sparse rootfs image of the given size in bytes.
    pub async fn create_rootfs(&self, vm_id: &str, size_bytes: u64) -> OsResult<PathBuf> {
        let path = self.rootfs_path(vm_id);
        self.create_sparse_image(&path, size_bytes).await?;
        Ok(path)
    }

    /// Create a sparse data disk image.
    pub async fn create_data_disk(&self, vm_id: &str, size_bytes: u64) -> OsResult<PathBuf> {
        let path = self.data_disk_path(vm_id);
        self.create_sparse_image(&path, size_bytes).await?;
        Ok(path)
    }

    /// Copy a base image to the VM's rootfs location.
    pub async fn copy_base_image(
        &self,
        vm_id: &str,
        source: &Path,
    ) -> OsResult<PathBuf> {
        let dest = self.rootfs_path(vm_id);
        self.prepare_vm_dirs(vm_id).await?;
        tokio::fs::copy(source, &dest).await?;
        Ok(dest)
    }

    /// Compute the CID for a file on disk (content-addressable).
    pub async fn compute_cid(&self, path: &Path) -> OsResult<Cid> {
        let data = tokio::fs::read(path).await?;
        Ok(Cid::from_bytes(&data, Codec::Chunk))
    }

    /// Build a Firecracker drive config for the rootfs.
    pub fn rootfs_drive_config(
        &self,
        vm_id: &str,
        rate_limiter: Option<DriveRateLimiter>,
    ) -> DriveConfig {
        DriveConfig {
            drive_id: "rootfs".to_string(),
            path_on_host: self.rootfs_path(vm_id).to_string_lossy().to_string(),
            is_root_device: true,
            is_read_only: false,
            rate_limiter,
        }
    }

    /// Build a Firecracker drive config for the data disk.
    pub fn data_drive_config(
        &self,
        vm_id: &str,
        rate_limiter: Option<DriveRateLimiter>,
    ) -> DriveConfig {
        DriveConfig {
            drive_id: "data".to_string(),
            path_on_host: self.data_disk_path(vm_id).to_string_lossy().to_string(),
            is_root_device: false,
            is_read_only: false,
            rate_limiter,
        }
    }

    /// Convert a drive config to Firecracker API JSON.
    pub fn drive_to_firecracker_json(drive: &DriveConfig) -> serde_json::Value {
        let mut json = serde_json::json!({
            "drive_id": drive.drive_id,
            "path_on_host": drive.path_on_host,
            "is_root_device": drive.is_root_device,
            "is_read_only": drive.is_read_only,
        });

        if let Some(ref rl) = drive.rate_limiter {
            let mut rate_limiter = serde_json::Map::new();
            if let Some(ref bw) = rl.bandwidth {
                rate_limiter.insert(
                    "bandwidth".to_string(),
                    serde_json::json!({
                        "size": bw.size,
                        "one_time_burst": bw.one_time_burst,
                        "refill_time": bw.refill_time,
                    }),
                );
            }
            if let Some(ref ops) = rl.ops {
                rate_limiter.insert(
                    "ops".to_string(),
                    serde_json::json!({
                        "size": ops.size,
                        "one_time_burst": ops.one_time_burst,
                        "refill_time": ops.refill_time,
                    }),
                );
            }
            if let serde_json::Value::Object(ref mut obj) = json {
                obj.insert(
                    "rate_limiter".to_string(),
                    serde_json::Value::Object(rate_limiter),
                );
            }
        }

        json
    }

    /// Remove all storage for a VM.
    pub async fn cleanup_vm(&self, vm_id: &str) -> OsResult<()> {
        let vm_dir = self.vm_dir(vm_id);
        if vm_dir.exists() {
            tokio::fs::remove_dir_all(&vm_dir).await?;
        }
        Ok(())
    }

    /// List all VM IDs that have storage directories.
    pub async fn list_vms(&self) -> OsResult<Vec<String>> {
        let vms_dir = self.base_path.join("vms");
        if !vms_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = tokio::fs::read_dir(&vms_dir).await?;
        let mut vms = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    vms.push(name.to_string());
                }
            }
        }
        Ok(vms)
    }

    async fn create_sparse_image(&self, path: &Path, size_bytes: u64) -> OsResult<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await?;

        file.set_len(size_bytes).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_dir_layout() {
        let storage = VmStorage::new("/tmp/monoce-os");
        assert_eq!(
            storage.vm_dir("vm-1").to_str().unwrap(),
            "/tmp/monoce-os/vms/vm-1"
        );
        assert_eq!(
            storage.rootfs_path("vm-1").to_str().unwrap(),
            "/tmp/monoce-os/vms/vm-1/rootfs.ext4"
        );
        assert_eq!(
            storage.socket_path("vm-1").to_str().unwrap(),
            "/tmp/monoce-os/vms/vm-1/firecracker.sock"
        );
    }

    #[test]
    fn rootfs_drive_config_is_root() {
        let storage = VmStorage::new("/tmp/monoce-os");
        let drive = storage.rootfs_drive_config("vm-1", None);
        assert!(drive.is_root_device);
        assert!(!drive.is_read_only);
        assert_eq!(drive.drive_id, "rootfs");
    }

    #[test]
    fn data_drive_config_is_not_root() {
        let storage = VmStorage::new("/tmp/monoce-os");
        let drive = storage.data_drive_config("vm-1", None);
        assert!(!drive.is_root_device);
        assert_eq!(drive.drive_id, "data");
    }

    #[test]
    fn drive_json_basic() {
        let drive = DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: "/tmp/rootfs.ext4".into(),
            is_root_device: true,
            is_read_only: false,
            rate_limiter: None,
        };
        let json = VmStorage::drive_to_firecracker_json(&drive);
        assert_eq!(json["drive_id"], "rootfs");
        assert_eq!(json["is_root_device"], true);
    }

    #[test]
    fn drive_json_with_rate_limiter() {
        let drive = DriveConfig {
            drive_id: "data".into(),
            path_on_host: "/tmp/data.ext4".into(),
            is_root_device: false,
            is_read_only: false,
            rate_limiter: Some(DriveRateLimiter {
                bandwidth: Some(TokenBucket {
                    size: 10_000_000,
                    one_time_burst: Some(5_000_000),
                    refill_time: 1000,
                }),
                ops: None,
            }),
        };
        let json = VmStorage::drive_to_firecracker_json(&drive);
        assert!(json.get("rate_limiter").is_some());
    }

    #[tokio::test]
    async fn list_vms_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = VmStorage::new(tmp.path());
        let vms = storage.list_vms().await.unwrap();
        assert!(vms.is_empty());
    }

    #[tokio::test]
    async fn prepare_and_list_vms() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = VmStorage::new(tmp.path());
        storage.prepare_vm_dirs("vm-a").await.unwrap();
        storage.prepare_vm_dirs("vm-b").await.unwrap();
        let mut vms = storage.list_vms().await.unwrap();
        vms.sort();
        assert_eq!(vms, vec!["vm-a", "vm-b"]);
    }

    #[tokio::test]
    async fn create_sparse_rootfs() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = VmStorage::new(tmp.path());
        storage.prepare_vm_dirs("vm-1").await.unwrap();
        let path = storage.create_rootfs("vm-1", 1024 * 1024).await.unwrap();
        let meta = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(meta.len(), 1024 * 1024);
    }

    #[tokio::test]
    async fn cleanup_removes_vm_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = VmStorage::new(tmp.path());
        storage.prepare_vm_dirs("vm-del").await.unwrap();
        assert!(storage.vm_dir("vm-del").exists());
        storage.cleanup_vm("vm-del").await.unwrap();
        assert!(!storage.vm_dir("vm-del").exists());
    }
}
