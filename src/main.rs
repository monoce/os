use anyhow::Result;
use monoce_os::image::VmImage;
use monoce_os::resource::{HostCapacity, ResourceLimits};
use monoce_os::storage::VmStorage;
use monoce_os::VmManager;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "create" => cmd_create(&args[2..]).await?,
        "destroy" => cmd_destroy(&args[2..]).await?,
        "list" => cmd_list().await?,
        "status" => cmd_status(&args[2..]).await?,
        "capacity" => cmd_capacity().await?,
        "images" => cmd_images().await?,
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("unknown command: {}", other);
            print_help();
            std::process::exit(1);
        }
    }

    Ok(())
}

fn default_storage() -> VmStorage {
    let base = std::env::var("MONOCE_OS_DATA")
        .unwrap_or_else(|_| "/var/lib/monoce-os".to_string());
    VmStorage::new(base)
}

/// Host capacity with every VM already on disk subtracted.
///
/// This must not report the host idle just because it is a fresh process:
/// `reserve` is the only overcommit guard there is, and it can only work
/// against a figure that accounts for VMs created by earlier invocations.
/// `HostCapacity::derive_from_dir` reads `vms/*/vm-config.json` to get it.
fn default_capacity() -> HostCapacity {
    let storage = default_storage();
    HostCapacity::derive_from_dir(
        &storage.vms_dir(),
        num_cpus(),
        total_memory_mib(),
        total_disk_bytes(&storage.vms_dir()),
    )
}

fn default_manager() -> VmManager {
    VmManager::new(default_storage(), default_capacity())
}

async fn cmd_create(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        eprintln!("usage: monoce-os create <name> <image> [small|medium|large]");
        std::process::exit(1);
    }

    let name = &args[0];
    let image_name = &args[1];
    let size = args.get(2).map(|s| s.as_str()).unwrap_or("small");

    // The image name is interpolated into `images/<name>.ext4` below, so it
    // must not be able to name a file outside that directory.
    monoce_os::storage::validate_image_name(image_name)?;

    let resources = match size {
        "small" => ResourceLimits::small(),
        "medium" => ResourceLimits::medium(),
        "large" => ResourceLimits::large(),
        _ => {
            eprintln!("invalid size: {} (use small, medium, large)", size);
            std::process::exit(1);
        }
    };

    let manager = default_manager();
    let images_dir = default_storage().images_dir();

    // Try to load image from MONOCE_OS_DATA/images unless overridden.
    let kernel_path = std::env::var("MONOCE_OS_KERNEL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| images_dir.join("vmlinux"));
    let rootfs_path = images_dir.join(format!("{}.ext4", image_name));

    let image = VmImage::new(image_name, kernel_path, rootfs_path);
    manager.register_image(image).await;

    let vm_id = manager.create_vm(name, image_name, resources).await?;
    println!("created VM: {}", vm_id);
    Ok(())
}

async fn cmd_destroy(args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("usage: monoce-os destroy <vm-id>");
        std::process::exit(1);
    }

    let manager = default_manager();
    manager.destroy_vm(&args[0]).await?;
    println!("destroyed VM: {}", args[0]);
    Ok(())
}

async fn cmd_list() -> Result<()> {
    let storage = default_storage();
    let vms = storage.list_vms().await?;

    // Filter to VMs that have a valid config file
    let mut valid_vms = Vec::new();
    for vm_id in &vms {
        if storage.config_path(vm_id).exists() {
            valid_vms.push(vm_id.clone());
        }
    }

    if valid_vms.is_empty() {
        println!("no VMs found");
        return Ok(());
    }

    println!("{:<40} {:<10}", "VM ID", "STATUS");
    println!("{}", "-".repeat(52));
    for vm_id in valid_vms {
        let socket = storage.socket_path(&vm_id);
        let status = if socket.exists() { "running" } else { "stopped" };
        println!("{:<40} {:<10}", vm_id, status);
    }
    Ok(())
}

async fn cmd_status(args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("usage: monoce-os status <vm-id>");
        std::process::exit(1);
    }

    // `args[0]` reaches `base/vms/<arg>/vm-config.json` unfiltered otherwise.
    monoce_os::storage::validate_vm_id(&args[0])?;

    let storage = default_storage();
    let config_path = storage.config_path(&args[0]);

    if !config_path.exists() {
        eprintln!("VM not found: {}", args[0]);
        std::process::exit(1);
    }

    let config_data = tokio::fs::read_to_string(&config_path).await?;
    let config: monoce_os::VmConfig = serde_json::from_str(&config_data)?;

    println!("VM ID:     {}", config.vm_id);
    println!("Name:      {}", config.name);
    println!("vCPUs:     {}", config.resources.cpu.vcpu_count);
    println!("Memory:    {} MiB", config.resources.memory.size_mib);
    println!(
        "Rootfs:    {} bytes",
        config.resources.storage.rootfs_size_bytes
    );
    println!("Network:   {}", config.network.guest_ip);
    println!("Socket:    {}", storage.socket_path(&args[0]).display());
    Ok(())
}

async fn cmd_capacity() -> Result<()> {
    let cap = default_capacity();
    println!("Host Capacity:");
    println!("  vCPUs:   {} total, {} available", cap.total_vcpus, cap.available_vcpus);
    println!("  Memory:  {} MiB total, {} MiB available", cap.total_memory_mib, cap.available_memory_mib);
    println!(
        "  Disk:    {} GiB total, {} GiB available",
        cap.total_disk_bytes / (1024 * 1024 * 1024),
        cap.available_disk_bytes / (1024 * 1024 * 1024)
    );
    Ok(())
}

async fn cmd_images() -> Result<()> {
    let storage = default_storage();
    let images_dir = storage.images_dir();

    if !images_dir.exists() {
        println!("no images found ({})", images_dir.display());
        return Ok(());
    }

    println!("Images directory: {}", images_dir.display());
    let mut entries = tokio::fs::read_dir(&images_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.metadata().await?;
        let name = entry.file_name().to_string_lossy().to_string();
        let size_mib = meta.len() / (1024 * 1024);
        println!("  {} ({} MiB)", name, size_mib);
    }
    Ok(())
}

fn print_help() {
    println!("monoce-os - Firecracker microVM manager for Monoce");
    println!();
    println!("USAGE:");
    println!("  monoce-os <command> [args...]");
    println!();
    println!("COMMANDS:");
    println!("  create <name> <image> [small|medium|large]  Create and start a VM");
    println!("  destroy <vm-id>                             Stop and remove a VM");
    println!("  list                                        List all VMs");
    println!("  status <vm-id>                              Show VM details");
    println!("  capacity                                    Show host capacity");
    println!("  images                                      List available images");
    println!("  help                                        Show this help");
    println!();
    println!("ENVIRONMENT:");
    println!("  MONOCE_OS_DATA     Data directory (default: /var/lib/monoce-os)");
    println!("  MONOCE_OS_KERNEL   Kernel image path");
    println!("  MONOCE_OS_TOTAL_DISK_BYTES");
    println!("                     Disk budget for VMs (default: filesystem size)");
    println!("  RUST_LOG           Log level (default: info)");
}

fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|p| p.get() as u32)
        .unwrap_or(4)
}

/// Total bytes on the filesystem backing the data directory.
///
/// `MONOCE_OS_TOTAL_DISK_BYTES` overrides it, for hosts where the VM store is
/// meant to use only part of the filesystem.
fn total_disk_bytes(vms_dir: &std::path::Path) -> u64 {
    const FALLBACK: u64 = 100 * 1024 * 1024 * 1024;

    if let Ok(raw) = std::env::var("MONOCE_OS_TOTAL_DISK_BYTES") {
        match raw.trim().parse::<u64>() {
            Ok(bytes) if bytes > 0 => return bytes,
            _ => tracing::warn!(
                value = %raw,
                "ignoring unparseable MONOCE_OS_TOTAL_DISK_BYTES"
            ),
        }
    }

    // statvfs needs a path that exists; the VM directory may not yet.
    let mut probe = vms_dir;
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => return FALLBACK,
        }
    }

    statvfs_total_bytes(probe).unwrap_or_else(|| {
        tracing::warn!(
            path = %probe.display(),
            "statvfs failed; falling back to a nominal disk total"
        );
        FALLBACK
    })
}

/// Total size in bytes of the filesystem containing `path`, via `statvfs(3)`.
fn statvfs_total_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

    // SAFETY: `c_path` is a valid NUL-terminated string and `stat` is a live,
    // correctly sized, zeroed `statvfs` we own for the duration of the call.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }

    // f_frsize is the fragment size f_blocks is counted in; some platforms
    // leave it zero, in which case f_bsize is the right unit.
    let unit = if stat.f_frsize != 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    (stat.f_blocks as u64).checked_mul(unit)
}

fn total_memory_mib() -> u64 {
    // Read from /proc/meminfo on Linux, fallback to 8 GiB.
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if let Some(kb_str) = parts.get(1) {
                        if let Ok(kb) = kb_str.parse::<u64>() {
                            return kb / 1024;
                        }
                    }
                }
            }
        }
    }
    8192
}
