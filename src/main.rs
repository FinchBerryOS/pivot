use loopdev::LoopControl;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::unistd::{chdir, pivot_root};
use serde::Deserialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

/// -----------------------------------------------------------------------------
/// KONFIGURATION
/// -----------------------------------------------------------------------------
#[derive(Deserialize, Debug)]
struct PivotConfig {
    system: SystemConfig,
    hardware: HardwareConfig,
    images: ImagesConfig,
}

#[derive(Deserialize, Debug)]
struct SystemConfig {
    mode: String,
    active_slot: String,
}

#[derive(Deserialize, Debug)]
struct HardwareConfig {
    boot_partition_uuid: String,
    system_partition_uuid: String,
}

#[derive(Deserialize, Debug)]
struct ImagesConfig {
    slot_a: String,
    slot_b: String,
}

/// Als PID 1 fangen wir fatale Fehler ab und halten das System an (Kernel Panic Prävention).
fn fatal_error(msg: &str) -> ! {
    eprintln!("\n[PIVOT FATAL ERROR] {}\n", msg);
    eprintln!("System halted. Please reboot manually.");
    loop {
        sleep(Duration::from_secs(60));
    }
}

/// -----------------------------------------------------------------------------
/// HAUPTPROGRAMM (PID 1)
/// -----------------------------------------------------------------------------
fn main() {
    println!("[PIVOT] Starting FinchBerryOS Initial RAM File System...");

    // 1. Virtuelle Dateisysteme mounten (Ohne VFS sind wir hardware-blind)
    setup_vfs();

    // 2. Config einlesen
    let config = read_config("/pivot.config");

    if config.system.mode != "installed" {
        println!("[PIVOT] Booting in Live/Installer mode...");
        // Live-System Logik würde hier folgen
        return;
    }

    // 3. Hardware via PARTUUID finden
    println!("[PIVOT] Scanning for Hardware (PARTUUID)...");
    let sp_dev = find_device_by_partuuid(&config.hardware.system_partition_uuid)
        .unwrap_or_else(|| fatal_error("System Partition (SP) not found!"));
    let bp_dev = find_device_by_partuuid(&config.hardware.boot_partition_uuid)
        .unwrap_or_else(|| fatal_error("Boot Partition (BP) not found!"));

    // 4. System Partition (SP) mounten
    fs::create_dir_all("/mnt/system").unwrap();
    mount(Some(sp_dev.as_str()), "/mnt/system", Some("ext4"), MsFlags::empty(), None::<&str>)
        .unwrap_or_else(|_| fatal_error("Failed to mount SP to /mnt/system"));

    // 5. Update-Weiche: Trigger und Payload checken
    if check_for_updates() {
        execute_ram_update(&sp_dev, &bp_dev);
        unreachable!("System must reboot after update");
    }

    // 6. Normaler Boot: System zusammenbauen
    let active_image = if config.system.active_slot == "A" {
        &config.images.slot_a
    } else {
        &config.images.slot_b
    };
    stage_system(active_image);

    // 7. VFS-Mounts in das neue Root-Dateisystem verschieben
    move_vfs_to_new_root();

    // 8. Der finale Sprung ins OS
    perform_pivot_and_exec();
}

/// -----------------------------------------------------------------------------
/// HILFSFUNKTIONEN
/// -----------------------------------------------------------------------------

fn setup_vfs() {
    fs::create_dir_all("/proc").ok();
    fs::create_dir_all("/sys").ok();
    fs::create_dir_all("/dev").ok();

    mount(None::<&str>, "/proc", Some("proc"), MsFlags::empty(), None::<&str>).unwrap();
    mount(None::<&str>, "/sys", Some("sysfs"), MsFlags::empty(), None::<&str>).unwrap();
    mount(Some("devtmpfs"), "/dev", Some("devtmpfs"), MsFlags::empty(), None::<&str>).unwrap();
}

fn read_config(path: &str) -> PivotConfig {
    let content = fs::read_to_string(path).unwrap_or_else(|_| fatal_error("pivot.config not found"));
    toml::from_str(&content).unwrap_or_else(|_| fatal_error("Failed to parse pivot.config"))
}

fn find_device_by_partuuid(target_uuid: &str) -> Option<String> {
    let block_dir = Path::new("/sys/class/block");
    let target = target_uuid.to_lowercase();

    if let Ok(entries) = fs::read_dir(block_dir) {
        for entry in entries.flatten() {
            let uevent_path = entry.path().join("uevent");
            if let Ok(content) = fs::read_to_string(uevent_path) {
                if content.to_lowercase().contains(&format!("partuuid={}", target)) {
                    return Some(format!("/dev/{}", entry.file_name().to_string_lossy()));
                }
            }
        }
    }
    None
}

/// Prüft das "Double-Flag" Prinzip für anstehende Updates
fn check_for_updates() -> bool {
    let trigger = Path::new("/mnt/system/private/system/StartUpdateInstaller");
    let payload = Path::new("/mnt/system/var/update/sys_update.fbuimg");
    trigger.exists() && payload.exists()
}

/// Der RAM-isolierte Update-Prozess
fn execute_ram_update(sp_dev: &str, bp_dev: &str) {
    println!("[PIVOT] Update requested. Entering RAM Update Mode...");

    fs::create_dir_all("/tmp").unwrap();
    mount(None::<&str>, "/tmp", Some("tmpfs"), MsFlags::empty(), None::<&str>).unwrap();

    let disk_installer = "/mnt/system/system/updateinstaller";
    let ram_installer = "/tmp/updateinstaller";

    fs::copy(disk_installer, ram_installer)
        .unwrap_or_else(|_| fatal_error("Failed to copy updateinstaller to RAM"));
    fs::set_permissions(ram_installer, fs::Permissions::from_mode(0o755)).unwrap();

    // BP Mounten für Kernel/Initramfs Updates
    fs::create_dir_all("/mnt/boot").unwrap();
    mount(Some(bp_dev), "/mnt/boot", Some("vfat"), MsFlags::empty(), None::<&str>).unwrap();

    // SP aushängen, um die Block-Ebene für den Flasher freizugeben
    umount2("/mnt/system", MntFlags::MNT_DETACH).unwrap();

    println!("[PIVOT] Handing over control to UpdateInstaller in RAM...");
    let err = Command::new(ram_installer)
        .arg("--sp-dev").arg(sp_dev)
        .arg("--bp-dev").arg(bp_dev)
        .exec();
    
    fatal_error(&format!("Failed to execute RAM updater: {}", err));
}

/// Baut das Dateisystem aus SquashFS Image + User-Daten zusammen
fn stage_system(active_image: &str) {
    println!("[PIVOT] Staging new root filesystem...");

    let staging_root = Path::new("/system/rootfs");
    let base_root = Path::new("/system/base_root");

    fs::create_dir_all(staging_root).unwrap();
    fs::create_dir_all(base_root).unwrap();

    // 1. Loop-Mount in purem Rust (Keine externen Binaries nötig!)
    let image_path = format!("/mnt/system/system/{}", active_image);
    println!("[PIVOT] Allocating Loop Device for {}...", image_path);
    
    let lc = LoopControl::open().unwrap_or_else(|_| fatal_error("Could not open /dev/loop-control"));
    let loop_dev = lc.next_free().unwrap_or_else(|_| fatal_error("No free loop device found"));
    
    loop_dev.with()
        .autoclear(true)
        .read_only(true)
        .attach(&image_path)
        .unwrap_or_else(|_| fatal_error("Failed to attach image to loop device"));

    let block_device_path = loop_dev.path().unwrap();
    
    // Mount als SquashFS
    mount(Some(&block_device_path), base_root, Some("squashfs"), MsFlags::MS_RDONLY, None::<&str>)
        .unwrap_or_else(|_| fatal_error("Base Image Mount failed. Is it a valid SquashFS?"));

    // 2. Das Skelett für das Sandwich erstellen
    let dirs = [
        "System", "Applications", "Users", "Library", "Volumes", "private", 
        "proc", "sys", "dev", "run", "usr", "bin", "sbin", "mnt"
    ];
    for dir in &dirs {
        fs::create_dir_all(staging_root.join(dir)).unwrap();
    }

    // 3. Den System-Kern (ReadOnly) spiegeln (inkl. Standard Unix-Ordner)
    let core_dirs = ["System", "usr", "bin", "sbin"];
    for dir in &core_dirs {
        let src = base_root.join(dir);
        let target = staging_root.join(dir);
        
        if src.exists() {
            mount(Some(&src), &target, None::<&str>, MsFlags::MS_BIND | MsFlags::MS_RDONLY, None::<&str>)
                .unwrap_or_else(|_| println!("[WARNING] Could not bind-mount core dir: {}", dir));
        }
    }

    // 4. Beschreibbare Nutzerdatenbanken binden (SP -> rootfs)
    mount(Some("/mnt/system/Users"), &staging_root.join("Users"), None::<&str>, MsFlags::MS_BIND, None::<&str>).unwrap();
    mount(Some("/mnt/system/Library"), &staging_root.join("Library"), None::<&str>, MsFlags::MS_BIND, None::<&str>).unwrap();
    mount(Some("/mnt/system/private"), &staging_root.join("private"), None::<&str>, MsFlags::MS_BIND, None::<&str>).unwrap();
    mount(Some("/mnt/system/Volumes"), &staging_root.join("Volumes"), None::<&str>, MsFlags::MS_BIND, None::<&str>).unwrap();

    // 5. Hybride Applications zusammenbauen
    println!("[PIVOT] Constructing Hybrid /Applications folder...");
    
    // A: User-Apps binden (Read-Write)
    mount(Some("/mnt/system/Applications"), &staging_root.join("Applications"), None::<&str>, MsFlags::MS_BIND, None::<&str>).unwrap();
    
    // B: Core-Apps einzeln als Read-Only einblenden
    let core_apps = base_root.join("Applications");
    if let Ok(entries) = fs::read_dir(core_apps) {
        for entry in entries.flatten() {
            let app_name = entry.file_name();
            let app_source = entry.path();
            let app_target = staging_root.join("Applications").join(&app_name);
            
            fs::create_dir_all(&app_target).ok(); // Ankerpunkt
            mount(Some(&app_source), &app_target, None::<&str>, MsFlags::MS_BIND | MsFlags::MS_RDONLY, None::<&str>)
                .unwrap_or_else(|_| println!("[WARNING] Failed to pin system app: {:?}", app_name));
        }
    }
}

/// Verschiebt die VFS-Mounts aus dem Initramfs ins neue Dateisystem
fn move_vfs_to_new_root() {
    println!("[PIVOT] Relocating Virtual Filesystems...");
    let staging = Path::new("/system/rootfs");

    mount(Some("/dev"), &staging.join("dev"), None::<&str>, MsFlags::MS_MOVE, None::<&str>).unwrap();
    mount(Some("/proc"), &staging.join("proc"), None::<&str>, MsFlags::MS_MOVE, None::<&str>).unwrap();
    mount(Some("/sys"), &staging.join("sys"), None::<&str>, MsFlags::MS_MOVE, None::<&str>).unwrap();
    
    // Frisches tmpfs für PID-Files und Sockets im neuen System
    mount(Some("tmpfs"), &staging.join("run"), Some("tmpfs"), MsFlags::empty(), None::<&str>).unwrap();
}

/// Der Punkt ohne Wiederkehr: Pivot und Übergabe an syscored
fn perform_pivot_and_exec() -> ! {
    println!("[PIVOT] All systems go. Executing pivot_root...");
    
    let new_root = "/system/rootfs";
    // Versteckter, chirurgisch reiner "Parkplatz" für den Kernel
    let put_old = "/system/rootfs/mnt/.initramfs";

    // Damit pivot_root funktioniert, muss new_root ein eigener Mountpoint sein
    mount(Some(new_root), new_root, None::<&str>, MsFlags::MS_BIND, None::<&str>).unwrap();
    
    fs::create_dir_all(put_old).unwrap();
    chdir(new_root).unwrap();
    
    
    
    // Der Tausch der Welten
    pivot_root(".", "mnt/.initramfs").unwrap_or_else(|e| fatal_error(&format!("pivot_root failed: {}", e)));

    // Das alte Initramfs aushängen (Gibt den RAM des alten Systems frei)
    umount2("/mnt/.initramfs", MntFlags::MNT_DETACH).ok();
    
    // Spuren verwischen: Den leeren, versteckten Parkplatz-Ordner löschen
    fs::remove_dir("/mnt/.initramfs").ok();

    println!("[PIVOT] Handing over to syscored at /usr/libexec/syscored...");
    // Starte den neuen PID 1
    let err = Command::new("/usr/libexec/syscored").exec();
    
    fatal_error(&format!("Failed to execute syscored: {}", err));
}