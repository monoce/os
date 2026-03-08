use anyhow::Result;
use monoce_os::image::VmImage;
use monoce_os::resource::{HostCapacity, ResourceLimits};
use monoce_os::storage::VmStorage;
use monoce_os::VmManager;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
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

fn default_capacity() -> HostCapacity {
    HostCapacity {
        total_vcpus: num_cpus(),
        available_vcpus: num_cpus(),
        total_memory_mib: total_memory_mib(),
        available_memory_mib: total_memory_mib(),
        total_disk_bytes: 100 * 1024 * 1024 * 1024,
        available_disk_bytes: 100 * 1024 * 1024 * 1024,
    }
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

    // Try to load image from well-known paths.
    let kernel_path = std::env::var("MONOCE_OS_KERNEL")
        .unwrap_or_else(|_| "/var/lib/monoce-os/images/vmlinux".to_string());
    let rootfs_path = format!("/var/lib/monoce-os/images/{}.ext4", image_name);

    let image = VmImage::new(image_name, &kernel_path, &rootfs_path);
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

    if vms.is_empty() {
        println!("no VMs found");
        return Ok(());
    }

    println!("{:<40} {:<10}", "VM ID", "STATUS");
    println!("{}", "-".repeat(52));
    for vm_id in vms {
        let config_path = storage.config_path(&vm_id);
        let status = if config_path.exists() { "created" } else { "unknown" };
        println!("{:<40} {:<10}", vm_id, status);
    }
    Ok(())
}

async fn cmd_status(args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("usage: monoce-os status <vm-id>");
        std::process::exit(1);
    }

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
    println!("  RUST_LOG           Log level (default: info)");
}

fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|p| p.get() as u32)
        .unwrap_or(4)
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
