use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use monoce_common::{Cid, Codec};
use crate::error::{OsError, OsResult};

/// Represents a VM image (kernel + rootfs) tracked by content address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmImage {
    /// Human-readable name (e.g. "alpine-3.20", "ubuntu-24.04").
    pub name: String,
    /// CID of the kernel binary (BLAKE3 hash).
    pub kernel_cid: Option<Cid>,
    /// CID of the rootfs image.
    pub rootfs_cid: Option<Cid>,
    /// Path to the kernel binary on disk.
    pub kernel_path: PathBuf,
    /// Path to the base rootfs image on disk.
    pub rootfs_path: PathBuf,
    /// Default kernel boot arguments.
    pub boot_args: String,
}

impl VmImage {
    pub fn new(
        name: impl Into<String>,
        kernel_path: impl Into<PathBuf>,
        rootfs_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            kernel_cid: None,
            rootfs_cid: None,
            kernel_path: kernel_path.into(),
            rootfs_path: rootfs_path.into(),
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".to_string(),
        }
    }

    pub fn with_boot_args(mut self, args: impl Into<String>) -> Self {
        self.boot_args = args.into();
        self
    }

    /// Compute CIDs for the kernel and rootfs from their on-disk contents.
    pub async fn compute_cids(&mut self) -> OsResult<()> {
        if !self.kernel_path.exists() {
            return Err(OsError::KernelNotFound(
                self.kernel_path.display().to_string(),
            ));
        }
        if !self.rootfs_path.exists() {
            return Err(OsError::RootfsNotFound(
                self.rootfs_path.display().to_string(),
            ));
        }

        let kernel_data = tokio::fs::read(&self.kernel_path).await?;
        self.kernel_cid = Some(Cid::from_bytes(&kernel_data, Codec::Chunk));

        let rootfs_data = tokio::fs::read(&self.rootfs_path).await?;
        self.rootfs_cid = Some(Cid::from_bytes(&rootfs_data, Codec::Chunk));

        Ok(())
    }

    /// Validate that the kernel and rootfs exist on disk.
    pub fn validate(&self) -> OsResult<()> {
        if !self.kernel_path.exists() {
            return Err(OsError::KernelNotFound(
                self.kernel_path.display().to_string(),
            ));
        }
        if !self.rootfs_path.exists() {
            return Err(OsError::RootfsNotFound(
                self.rootfs_path.display().to_string(),
            ));
        }
        Ok(())
    }

    /// Generate the Firecracker boot-source API payload.
    pub fn boot_source_json(&self, extra_boot_args: &str) -> serde_json::Value {
        let mut args = self.boot_args.clone();
        if !extra_boot_args.is_empty() {
            args.push(' ');
            args.push_str(extra_boot_args);
        }

        serde_json::json!({
            "kernel_image_path": self.kernel_path.to_string_lossy(),
            "boot_args": args,
        })
    }
}

/// Registry of available VM images.
#[derive(Debug, Default)]
pub struct ImageRegistry {
    images: std::collections::HashMap<String, VmImage>,
}

impl ImageRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, image: VmImage) {
        self.images.insert(image.name.clone(), image);
    }

    pub fn get(&self, name: &str) -> Option<&VmImage> {
        self.images.get(name)
    }

    pub fn list(&self) -> Vec<&VmImage> {
        self.images.values().collect()
    }

    pub fn remove(&mut self, name: &str) -> Option<VmImage> {
        self.images.remove(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_image_construction() {
        let img = VmImage::new("test", "/vmlinux", "/rootfs.ext4");
        assert_eq!(img.name, "test");
        assert!(img.boot_args.contains("console=ttyS0"));
    }

    #[test]
    fn boot_source_json_format() {
        let img = VmImage::new("test", "/vmlinux", "/rootfs.ext4");
        let json = img.boot_source_json("ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off");
        assert_eq!(json["kernel_image_path"], "/vmlinux");
        let args = json["boot_args"].as_str().unwrap();
        assert!(args.contains("console=ttyS0"));
        assert!(args.contains("172.16.0.2"));
    }

    #[test]
    fn boot_source_json_no_extra_args() {
        let img = VmImage::new("test", "/vmlinux", "/rootfs.ext4");
        let json = img.boot_source_json("");
        let args = json["boot_args"].as_str().unwrap();
        assert!(!args.ends_with(' '));
    }

    #[test]
    fn image_registry_crud() {
        let mut registry = ImageRegistry::new();
        let img = VmImage::new("alpine", "/vmlinux", "/rootfs.ext4");
        registry.register(img);

        assert!(registry.get("alpine").is_some());
        assert!(registry.get("ubuntu").is_none());
        assert_eq!(registry.list().len(), 1);

        let removed = registry.remove("alpine");
        assert!(removed.is_some());
        assert_eq!(registry.list().len(), 0);
    }

    #[test]
    fn validate_fails_for_missing_kernel() {
        let img = VmImage::new("test", "/nonexistent/vmlinux", "/nonexistent/rootfs.ext4");
        let result = img.validate();
        assert!(result.is_err());
    }

    #[test]
    fn with_boot_args_overrides() {
        let img = VmImage::new("test", "/vmlinux", "/rootfs.ext4")
            .with_boot_args("custom=args");
        assert_eq!(img.boot_args, "custom=args");
    }

    #[test]
    fn image_serialization_roundtrip() {
        let img = VmImage::new("alpine", "/vmlinux", "/rootfs.ext4");
        let json = serde_json::to_string(&img).unwrap();
        let restored: VmImage = serde_json::from_str(&json).unwrap();
        assert_eq!(img.name, restored.name);
        assert_eq!(img.boot_args, restored.boot_args);
    }
}
