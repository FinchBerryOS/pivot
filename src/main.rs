use loopdev::LoopControl;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{chdir, chroot, close, dup2, getpid};
use serde::Deserialize;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

// ────────────────────────────────────────────────────────────────────────────
// LOGGING  (writes to /dev/kmsg so output survives headless / serial boots)
// ────────────────────────────────────────────────────────────────────────────

macro_rules! klog {
    ($($arg:tt)*) => {{
        let msg = format!("[PIVOT] {}\n", format!($($arg)*));
        // Best-effort: write to kmsg, fall back to stderr
        if let Ok(mut f) = fs::OpenOptions::new().write(true).open("/dev/kmsg") {
            let _ = f.write_all(msg.as_bytes());
        }
        eprint!("{}", msg);
    }};
}

// ────────────────────────────────────────────────────────────────────────────
// KONFIGURATION
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum BootMode {
    Installed,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "UPPERCASE")]
enum ActiveSlot {
    A,
    B,
}

#[derive(Deserialize, Debug)]
struct PivotConfig {
    system:   SystemConfig,
    hardware: HardwareConfig,
    images:   ImagesConfig,
}

#[derive(Deserialize, Debug)]
struct SystemConfig {
    mode:        BootMode,
    active_slot: ActiveSlot,
}

#[derive(Deserialize, Debug)]
struct HardwareConfig {
    boot_partition_uuid:   String,
    system_partition_uuid: String,
}

#[derive(Deserialize, Debug)]
struct ImagesConfig {
    slot_a: String,
    slot_b: String,
}

// ────────────────────────────────────────────────────────────────────────────
// FATAL ERROR  (PID 1 must never exit – spin forever after logging)
// ────────────────────────────────────────────────────────────────────────────

fn fatal_error(msg: &str) -> ! {
    // Write to kmsg first – survives on serial/headless consoles
    klog!("FATAL ERROR: {}", msg);
    let _ = std::io::stderr().flush();

    // Stage 1: trigger kernel panic via sysrq 'c'.
    // The kernel prints a full oops/backtrace and then behaves according to
    // the kernel cmdline parameter panic= (reboot, halt, or timeout).
    // Works on any kernel with CONFIG_MAGIC_SYSRQ=y (default on all distros).
    if let Ok(mut f) = fs::OpenOptions::new().write(true).open("/proc/sysrq-trigger") {
        let _ = f.write_all(b"c");
    }

    // Stage 2 fallback: CONFIG_MAGIC_SYSRQ=n (hardened/embedded kernels).
    // Send SIGABRT to ourselves via nix (no libc dependency – safe with musl static linking).
    // The kernel MUST panic when PID 1 dies – this is guaranteed kernel behaviour.
    let _ = kill(getpid(), Signal::SIGABRT);

    // Stage 3: absolute last resort – unreachable in practice
    loop { sleep(Duration::from_secs(1)); }
}

// ────────────────────────────────────────────────────────────────────────────
// HAUPTPROGRAMM (PID 1)
// ────────────────────────────────────────────────────────────────────────────

fn main() {
    // VFS FIRST: devtmpfs must be mounted before klog! can open /dev/kmsg
    setup_vfs();
    klog!("Starting FinchBerryOS Initial RAM File System...");
    let config = read_config("/pivot.config");

    // Mode is now validated at parse time via the BootMode enum.
    // (Non-"installed" values cause a toml parse error → fatal_error in read_config.)

    // 1. Hardware polling – GPT parsing, no udev required
    let sp_dev = wait_for_partuuid(&config.hardware.system_partition_uuid, 15)
        .unwrap_or_else(|| fatal_error("System Partition (SP) not found!"));
    let bp_dev = wait_for_partuuid(&config.hardware.boot_partition_uuid, 15)
        .unwrap_or_else(|| fatal_error("Boot Partition (BP) not found!"));

    klog!("SP: {}  BP: {}", sp_dev, bp_dev);

    // 2. FSCK
    klog!("Checking System Partition integrity...");
    match Command::new("/sbin/e2fsck").arg("-p").arg("-f").arg(&sp_dev).status() {
        Err(e) => fatal_error(&format!("e2fsck could not be launched: {}", e)),
        Ok(status) => match status.code() {
            None       => fatal_error("e2fsck killed by signal"),
            Some(code) if code >= 4 => fatal_error(&format!("e2fsck critical failure: code {}", code)),
            Some(code) => klog!("e2fsck finished with code {} (ok)", code),
        },
    }

    // 3. Mount System Partition
    fs::create_dir_all("/mnt/system")
        .unwrap_or_else(|e| fatal_error(&format!("mkdir /mnt/system: {}", e)));
    mount(
        Some(sp_dev.as_str()), "/mnt/system",
        Some("ext4"), MsFlags::empty(), None::<&str>,
    ).unwrap_or_else(|e| fatal_error(&format!("Failed to mount SP: {}", e)));

    // 4. Update check
    if check_for_updates() {
        klog!("Update trigger found – launching RAM updater.");
        execute_ram_update(&sp_dev, &bp_dev);
        unreachable!();
    }

    // 5. Slot selection
    let active_image = match config.system.active_slot {
        ActiveSlot::A => &config.images.slot_a,
        ActiveSlot::B => &config.images.slot_b,
    };
    klog!("Active slot image: {}", active_image);

    stage_system(active_image);
    move_vfs_to_new_root();
    perform_pivot_and_exec();
}

// ────────────────────────────────────────────────────────────────────────────
// HELPER & LOGIK
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// GPT PARTUUID LOOKUP  (no udev, no blkid – pure kernel sysfs + raw block I/O)
//
// Strategy:
//   1. Enumerate every block device in /sys/class/block/
//   2. Skip non-partition entries (no "partition" sysfs attribute)
//   3. Find the parent disk device (one level up in sysfs)
//   4. Open /dev/<disk> and read the GPT header + partition table directly
//   5. Compare each partition entry's GUID against the target PARTUUID
//
// The kernel never writes PARTUUID into uevent – that is solely a udev artifact.
// GPT partition GUIDs are stored in the partition table at a well-known offset
// and are readable from the raw block device which devtmpfs gives us.
// ────────────────────────────────────────────────────────────────────────────

/// GPT constants (UEFI spec 2.10 §5.3)
const GPT_HEADER_SIGNATURE: u64 = 0x5452415020494645; // "EFI PART" LE

/// Read a little-endian u64 from a byte slice at a given offset.
fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Convert a raw 16-byte GPT GUID (mixed-endian, UEFI layout) to the canonical
/// lowercase string form used by Linux: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
///
/// UEFI stores the first three groups as little-endian, the last two as big-endian.
fn guid_to_string(raw: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        raw[3], raw[2], raw[1], raw[0],   // time_low (LE → reversed)
        raw[5], raw[4],                   // time_mid (LE → reversed)
        raw[7], raw[6],                   // time_hi  (LE → reversed)
        raw[8], raw[9],                   // clock_seq (BE → as-is)
        raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]  // node (BE → as-is)
    )
}

/// Read the logical block size for a disk from sysfs.
/// Falls back to 512 if the attribute is missing (safe for 512e drives).
fn read_logical_block_size(disk_name: &str) -> u64 {
    let path = format!("/sys/class/block/{}/queue/logical_block_size", disk_name);
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(512)
}

/// Try to find a partition with the given PARTUUID on a specific disk.
/// Returns the /dev/<partname> path if found.
fn scan_disk_for_partuuid(disk_dev: &str, disk_name: &str, target: &str) -> Option<String> {
    let mut f = File::open(disk_dev).ok()?;

    // Read the actual logical block size from sysfs – supports both 512/512e and 4Kn drives.
    let lbs = read_logical_block_size(disk_name);

    // LBA 1 holds the GPT primary header.
    let mut header = vec![0u8; lbs as usize];
    f.seek(SeekFrom::Start(lbs)).ok()?;
    f.read_exact(&mut header).ok()?;

    // Validate GPT signature ("EFI PART" at offset 0 of the header)
    if read_u64_le(&header, 0) != GPT_HEADER_SIGNATURE {
        return None; // not a GPT disk
    }

    let part_entry_lba  = read_u64_le(&header, 72);
    let num_part_entries = read_u32_le(&header, 80);
    let part_entry_size  = read_u32_le(&header, 84) as u64;

    // Safety caps (UEFI spec: minimum 128 bytes, Linux hard-limits to 128 entries)
    if part_entry_size < 128 || part_entry_size > 512 { return None; }
    let safe_count = num_part_entries.min(128);

    for i in 0..safe_count {
        let byte_offset = part_entry_lba * lbs + i as u64 * part_entry_size;
        let mut entry = vec![0u8; part_entry_size as usize];
        if f.seek(SeekFrom::Start(byte_offset)).is_err() { break; }
        if f.read_exact(&mut entry).is_err() { break; }

        // Bytes 0..16: partition type GUID – all zeros means unused entry
        if entry[0..16].iter().all(|&b| b == 0) { continue; }

        // Bytes 16..32: partition unique GUID (PARTUUID)
        let raw_guid: [u8; 16] = entry[16..32].try_into().unwrap();
        let part_guid = guid_to_string(&raw_guid);

        if part_guid == target {
            let part_start_lba = read_u64_le(&entry, 32);

            if let Ok(children) = fs::read_dir(format!("/sys/class/block/{}", disk_name)) {
                for child in children.flatten() {
                    let child_name = child.file_name().to_string_lossy().to_string();

                    // Match only direct children of this disk.
                    // NVMe: nvme0n1p1 (disk + "p" + digit)
                    // SCSI/SATA: sda1 (disk + digit)
                    let suffix = child_name.strip_prefix(disk_name).unwrap_or("");
                    let is_partition = suffix.starts_with('p') && suffix.len() > 1
                        || suffix.chars().next().map_or(false, |c| c.is_ascii_digit());
                    if !is_partition { continue; }

                    let start_path = child.path().join("start");
                    if let Ok(s) = fs::read_to_string(&start_path) {
                        if s.trim().parse::<u64>().ok() == Some(part_start_lba) {
                            klog!("PARTUUID {} → /dev/{}", target, child_name);
                            return Some(format!("/dev/{}", child_name));
                        }
                    }
                }
            }
            klog!("WARN: GUID match for {} but no sysfs child with start={}", target, part_start_lba);
        }
    }
    None
}

/// Poll for a partition by PARTUUID using direct GPT parsing.
/// Works without udev – only requires devtmpfs + sysfs (both mounted in setup_vfs).
fn wait_for_partuuid(target_uuid: &str, timeout_secs: u64) -> Option<String> {
    let start  = Instant::now();
    let needle = target_uuid.to_lowercase();

    while start.elapsed().as_secs() < timeout_secs {
        // Enumerate all block devices visible in sysfs
        if let Ok(entries) = fs::read_dir("/sys/class/block") {
            for entry in entries.flatten() {
                let dev_name = entry.file_name().to_string_lossy().to_string();

                // We only want whole disk devices (no partitions themselves).
                // A whole disk has no "partition" attribute in sysfs.
                let part_attr = entry.path().join("partition");
                if part_attr.exists() { continue; }

                // Skip loop, ram, zram, dm devices – they have no GPT
                if dev_name.starts_with("loop")
                    || dev_name.starts_with("ram")
                    || dev_name.starts_with("zram")
                    || dev_name.starts_with("dm-")
                {
                    continue;
                }

                let disk_dev = format!("/dev/{}", dev_name);
                if let Some(found) = scan_disk_for_partuuid(&disk_dev, &dev_name, &needle) {
                    return Some(found);
                }
            }
        }
        sleep(Duration::from_millis(200));
    }
    None
}

/// RO bind-mount: bind first, then remount read-only.
fn mount_bind_ro(src: &Path, tgt: &Path) {
    mount(Some(src), tgt, None::<&str>, MsFlags::MS_BIND, None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Bind mount {:?} → {:?}: {}", src, tgt, e)));

    mount(
        None::<&str>, tgt, None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    ).unwrap_or_else(|e| fatal_error(&format!("RO remount {:?}: {}", tgt, e)));
}

fn stage_system(active_image: &str) {
    let staging_root = Path::new("/system/rootfs");
    let base_root    = Path::new("/system/base_root");

    fs::create_dir_all(staging_root)
        .unwrap_or_else(|e| fatal_error(&format!("mkdir {:?}: {}", staging_root, e)));
    fs::create_dir_all(base_root)
        .unwrap_or_else(|e| fatal_error(&format!("mkdir {:?}: {}", base_root, e)));

    // Attach squashfs image via loop device.
    // Wait for /dev/loop-control to appear – on bare metal PID 1 often races
    // ahead of the kernel driver init. Poll with a short timeout.
    let loop_control_path = Path::new("/dev/loop-control");
    let lc = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if loop_control_path.exists() {
                match LoopControl::open() {
                    Ok(lc) => break lc,
                    Err(e) if std::time::Instant::now() < deadline => {
                        klog!("WARN: LoopControl::open failed ({}), retrying...", e);
                        sleep(Duration::from_millis(50));
                    }
                    Err(e) => fatal_error(&format!("LoopControl::open: {}", e)),
                }
            } else if std::time::Instant::now() < deadline {
                sleep(Duration::from_millis(50));
            } else {
                fatal_error("/dev/loop-control did not appear within 5s");
            }
        }
    };
    let loop_dev = lc.next_free()
        .unwrap_or_else(|e| fatal_error(&format!("No free loop device: {}", e)));
    let image_path = format!("/mnt/system/system/{}", active_image);
    loop_dev.with()
        .autoclear(true)
        .read_only(true)
        .attach(&image_path)
        .unwrap_or_else(|e| fatal_error(&format!("Loop attach {}: {}", image_path, e)));

    let block_dev = loop_dev.path()
        .unwrap_or_else(|| fatal_error("Loop device has no path after attach"));

    mount(
        Some(&block_dev), base_root,
        Some("squashfs"), MsFlags::MS_RDONLY, None::<&str>,
    ).unwrap_or_else(|e| fatal_error(&format!("Mount squashfs {:?}: {}", block_dev, e)));

    // Keep the loop device alive (autoclear handles cleanup on final unmount)
    std::mem::forget(loop_dev);

    // Create mount-point skeleton in staging root
    let dirs = [
        "System", "Applications", "Users", "Library",
        "Volumes", "private", "proc", "sys", "dev",
        "run", "usr", "bin", "sbin", "mnt/system",
    ];
    for dir in &dirs {
        fs::create_dir_all(staging_root.join(dir))
            .unwrap_or_else(|e| fatal_error(&format!("mkdir staging/{}: {}", dir, e)));
    }

    // Immutable core from squashfs (read-only bind)
    for dir in &["System", "usr", "bin", "sbin"] {
        let src = base_root.join(dir);
        if src.exists() {
            mount_bind_ro(&src, &staging_root.join(dir));
        } else {
            klog!("WARN: squashfs/{} not found – skipping RO bind", dir);
        }
    }

    // Persistent RW data from system partition.
    // On first boot these directories may not exist yet – create them on the SP
    // rather than skipping the bind, which would silently leave the path backed
    // by the initramfs tmpfs and lose all writes after reboot.
    for dir in &["Users", "Library", "private", "Volumes", "Applications"] {
        let src = format!("/mnt/system/{}", dir);
        let tgt = staging_root.join(dir);
        if !Path::new(&src).exists() {
            klog!("First boot: creating {} on SP", src);
            fs::create_dir_all(&src)
                .unwrap_or_else(|e| fatal_error(&format!("mkdir {} on SP: {}", src, e)));
        }
        mount(Some(src.as_str()), &tgt, None::<&str>, MsFlags::MS_BIND, None::<&str>)
            .unwrap_or_else(|e| fatal_error(&format!("Bind mount {} → {:?}: {}", src, tgt, e)));
    }

    // Pass-through master mount of system partition
    mount(
        Some("/mnt/system"),
        &staging_root.join("mnt/system"),
        None::<&str>, MsFlags::MS_BIND, None::<&str>,
    ).unwrap_or_else(|e| fatal_error(&format!("Bind /mnt/system into staging: {}", e)));
}

fn setup_vfs() {
    // /proc, /sys, /dev must exist as empty dirs in the initramfs cpio archive.
    // Mount /dev FIRST so that /dev/kmsg is available for fatal_error's Stage 1
    // (sysrq-trigger lives under /proc which comes second).
    mount(Some("devtmpfs"), "/dev", Some("devtmpfs"), MsFlags::empty(), None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Mount devtmpfs: {}", e)));
    mount(None::<&str>, "/proc", Some("proc"),     MsFlags::empty(), None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Mount procfs: {}", e)));
    mount(None::<&str>, "/sys",  Some("sysfs"),    MsFlags::empty(), None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Mount sysfs: {}", e)));
}

fn move_vfs_to_new_root() {
    let staging = Path::new("/system/rootfs");
    for vfs in &["dev", "proc", "sys"] {
        mount(
            Some(&format!("/{}", vfs)),
            &staging.join(vfs),
            None::<&str>, MsFlags::MS_MOVE, None::<&str>,
        ).unwrap_or_else(|e| fatal_error(&format!("MS_MOVE /{} into staging: {}", vfs, e)));
    }
    mount(Some("tmpfs"), &staging.join("run"), Some("tmpfs"), MsFlags::empty(), None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Mount tmpfs for /run: {}", e)));
}

/// Switch root – correct approach for initramfs.
///
/// pivot_root(2) CANNOT be used from an initramfs because the initramfs is a
/// tmpfs that the kernel mounts directly as the initial root. It has no parent
/// mountpoint entry in the mount namespace, so pivot_root always returns EINVAL.
///
/// The correct sequence (identical to busybox switch_root / systemd):
///   1. MS_MOVE  – atomically move new_root onto /
///   2. chroot(".") – update the kernel's root pointer
///   3. Clean up any remaining initramfs tmpfs entries to release RAM
///   4. exec the real init
fn perform_pivot_and_exec() -> ! {
    let new_root = "/system/rootfs";

    // Step 1: bind-mount new_root onto itself to make it an explicit mountpoint.
    // MS_MOVE requires the source to be a mountpoint. /system/rootfs is a plain
    // directory in the initramfs tmpfs – it has submounts inside it, but the
    // directory itself has no mount entry. This bind-mount creates one.
    mount(Some(new_root), new_root, None::<&str>, MsFlags::MS_BIND, None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Bind new_root onto itself: {}", e)));

    // Step 2: chdir into the new root
    chdir(new_root)
        .unwrap_or_else(|e| fatal_error(&format!("chdir to new_root: {}", e)));

    // Step 3: atomically move new_root onto /
    // Now that new_root is a proper mountpoint, MS_MOVE works correctly.
    mount(Some(new_root), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("MS_MOVE new_root onto /: {}", e)));

    // Step 4: update the kernel root pointer
    chroot(".")
        .unwrap_or_else(|e| fatal_error(&format!("chroot to new root: {}", e)));

    // Step 5: chdir to / inside the new root
    chdir("/")
        .unwrap_or_else(|e| fatal_error(&format!("chdir to / in new root: {}", e)));

    // Step 6: wire up stdin/stdout/stderr to /dev/console for syscored.
    // After switch_root the process has no open file descriptors for the
    // standard streams. Any write to stdout/stderr in syscored would hit
    // a closed fd and raise SIGPIPE / EIO – crashing the new init immediately.
    // Opening /dev/console and dup2-ing it to fds 0/1/2 is the POSIX-standard
    // way to give PID 1 a working console before exec.
    {
        use std::os::unix::io::IntoRawFd;
        let console = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/console")
            .unwrap_or_else(|e| fatal_error(&format!("Open /dev/console: {}", e)));
        let fd = console.into_raw_fd();
        // dup2: redirect stdin(0), stdout(1), stderr(2) to /dev/console
        for target_fd in 0i32..=2 {
            if fd != target_fd {
                dup2(fd, target_fd)
                    .unwrap_or_else(|e| fatal_error(&format!("dup2 console→fd{}: {}", target_fd, e)));
            }
        }
        // Close the original fd if it was above 2 (avoid leaking it into syscored)
        if fd > 2 {
            close(fd)
                .unwrap_or_else(|e| fatal_error(&format!("close console fd: {}", e)));
        }
    }

    // Step 7: hand off to the real init
    klog!("Executing /usr/libexec/syscored ...");
    let err = Command::new("/usr/libexec/syscored").exec();
    fatal_error(&format!("exec /usr/libexec/syscored failed: {}", err));
}

fn read_config(path: &str) -> PivotConfig {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|e| fatal_error(&format!("Cannot read {}: {}", path, e)));
    toml::from_str(&content)
        .unwrap_or_else(|e| fatal_error(&format!("pivot.config parse error: {}", e)))
}

fn check_for_updates() -> bool {
    // Double-flag principle: only trigger update if BOTH the trigger marker
    // AND the actual update payload are present on the SP.
    // A lone trigger without a payload would launch the updater with nothing to do.
    let trigger = Path::new("/mnt/system/private/system/StartUpdateInstaller");
    let payload  = Path::new("/mnt/system/var/update/sys_update.fbuimg");
    trigger.exists() && payload.exists()
}

/// Copies the updater binary into RAM (tmpfs) and executes it.
/// SP is unmounted first so the updater can repartition freely.
fn execute_ram_update(sp_dev: &str, bp_dev: &str) -> ! {
    let src = "/mnt/system/system/updateinstaller";
    let dst = "/tmp/updateinstaller";

    // Sanity-check source exists before we mount tmpfs
    if !Path::new(src).exists() {
        fatal_error("updateinstaller binary not found on SP");
    }

    fs::create_dir_all("/tmp")
        .unwrap_or_else(|e| fatal_error(&format!("mkdir /tmp: {}", e)));
    mount(None::<&str>, "/tmp", Some("tmpfs"), MsFlags::empty(), None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Mount tmpfs on /tmp: {}", e)));
    fs::copy(src, dst)
        .unwrap_or_else(|e| fatal_error(&format!("Copy updateinstaller to RAM: {}", e)));
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| fatal_error(&format!("chmod updateinstaller: {}", e)));

    // Mount BP so the updater can also flash kernel/initramfs on the boot partition
    fs::create_dir_all("/mnt/boot")
        .unwrap_or_else(|e| fatal_error(&format!("mkdir /mnt/boot: {}", e)));
    mount(Some(bp_dev), "/mnt/boot", Some("vfat"), MsFlags::empty(), None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Mount BP to /mnt/boot: {}", e)));

    // Unmount SP so the updater can repartition freely (BP stays mounted via /mnt/boot)
    umount2("/mnt/system", MntFlags::MNT_DETACH)
        .unwrap_or_else(|e| fatal_error(&format!("Umount SP before update: {}", e)));

    let err = Command::new(dst)
        .arg("--sp-dev").arg(sp_dev)
        .arg("--bp-dev").arg(bp_dev)
        .exec();
    fatal_error(&format!("exec updateinstaller failed: {}", err));
}