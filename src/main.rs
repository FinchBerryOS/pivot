use loopdev::LoopControl;
use nix::fcntl::OFlag;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{chdir, chroot, close, dup2, getpid};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicI32, Ordering};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

// ────────────────────────────────────────────────────────────────────────────
// LOGGING  (writes to /dev/kmsg so output survives headless / serial boots)
// ────────────────────────────────────────────────────────────────────────────

// /dev/kmsg fd cached in an AtomicI32 (-1 = not yet opened).
//
// Design rationale vs OnceLock<Mutex<File>>:
//   - Mutex can be poisoned if a panic occurs while the lock is held.
//     In a fatal_error → klog! → Mutex::lock() path the poisoned lock
//     would cause lock() to return Err, silently dropping the log message
//     exactly when we need it most.
//   - OnceLock init can itself panic (e.g. if /dev/null is missing),
//     which re-enters the panic hook → klog! → kmsg_write → infinite recursion.
//   - AtomicI32 has no lock, no panic surface, no heap allocation.
//     write(2) on a raw fd is async-signal-safe and re-entrant.
//
// Safety: single-threaded PID 1. The AtomicI32 store/load uses Relaxed
// ordering – there is no concurrent writer, so no stronger guarantee needed.
static KMSG_FD: AtomicI32 = AtomicI32::new(-1);

fn kmsg_write(msg: &str) {
    // Fast path: fd already open.
    let mut fd = KMSG_FD.load(Ordering::Relaxed);
    if fd < 0 {
        // Slow path: try to open /dev/kmsg once.
        // If it fails (devtmpfs not yet mounted) we fall through to stderr only.
        // We do NOT panic or fatal_error here – logging must never cause a crash.
        //
        // Use compare_exchange instead of a plain store to handle the theoretical
        // reentrant case (e.g. Panic-hook fires while we are inside kmsg_write
        // before the store is visible). Without CAS a second open() would produce
        // a second FD that gets leaked when only one is stored.
        // With CAS: the loser closes its FD and uses the winner's.
        use std::os::unix::io::IntoRawFd;
        if let Ok(f) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(OFlag::O_CLOEXEC.bits())
            .open("/dev/kmsg")
        {
            let new_fd = f.into_raw_fd();
            match KMSG_FD.compare_exchange(-1, new_fd, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => { fd = new_fd; }
                Err(existing) => {
                    // Another call already stored an FD – close ours to avoid a leak.
                    let _ = nix::unistd::close(new_fd);
                    fd = existing;
                }
            }
        }
    }
    if fd >= 0 {
        // write(2) is async-signal-safe; ignore partial writes / errors.
        let _ = nix::unistd::write(unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
                                   msg.as_bytes());
    }
}

macro_rules! klog {
    ($($arg:tt)*) => {{
        let msg = format!("[PIVOT] {}\n", format!($($arg)*));
        // kmsg: kernel ring buffer, survives console switches, readable via dmesg.
        // stderr: real-time console (fd 2 -> /dev/console after dup2 in Step 8).
        // Both writes are best-effort - failure must never abort the boot.
        kmsg_write(&msg);
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
    // Live mode is parsed and explicitly rejected with a clear error.
    // This produces a helpful message instead of a cryptic TOML parse failure
    // when someone writes mode = "live" in pivot.config.
    Live,
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
    // Install a global panic hook BEFORE setup_vfs so that even the earliest
    // possible Rust panic (e.g. an unwrap in setup_vfs itself before /dev/kmsg
    // exists) routes through fatal_error instead of printing to stderr and
    // exiting with code 101 – which would silently hang the kernel (PID 1 exit
    // without explicit panic = undefined behaviour on many kernels, or a reboot
    // loop without any log output).
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            format!("PANIC: {} at {:?}", s, info.location())
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            format!("PANIC: {} at {:?}", s, info.location())
        } else {
            format!("PANIC at {:?}", info.location())
        };
        fatal_error(&msg);
    }));
    // VFS FIRST: devtmpfs must be mounted before klog! can open /dev/kmsg
    setup_vfs();
    klog!("Starting FinchBerryOS Initial RAM File System...");
    let config = read_config("/pivot.config");

    validate_config(&config);

    // Validate boot mode. Live mode is not handled by pivot – it requires a
    // dedicated live-boot path that is out of scope for this binary.
    // Explicitly fatal here so the operator gets a clear error message instead
    // of a confusing TOML parse failure or a silent wrong-path execution.
    match config.system.mode {
        BootMode::Installed => {}
        BootMode::Live => fatal_error(
            "mode = \"live\" is not supported by this pivot binary.              Use a live-boot enabled initramfs instead."
        ),
    }

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
    // Sanitize active_image: must be a plain filename – no path separators or
    // dot-dot sequences. pivot.config is build-system authored, but a corrupt or
    // tampered value like "../../private/shadow" would be silently accepted by
    // format!() and allow arbitrary SP file access.
    if active_image.is_empty() || active_image.contains('/') || active_image.contains("..") {
        fatal_error(&format!(
            "active_image '{}' contains illegal characters (/, ..) or is empty",
            active_image
        ));
    }
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

/// Read a little-endian u64 from `buf` at byte offset `off`.
/// Returns None if the slice is too short – no panic, no unwrap.
fn read_u64_le(buf: &[u8], off: usize) -> Option<u64> {
    buf.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
}
/// Read a little-endian u32 from `buf` at byte offset `off`.
/// Returns None if the slice is too short – no panic, no unwrap.
fn read_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
}

/// Read the logical block size for a disk from sysfs.
/// Falls back to 512 if the attribute is missing (safe for 512e drives).
/// Clamps to the valid range [512, 4096] – standard drives: 512 or 4096 bytes.
/// Values outside this range indicate a corrupt sysfs entry and would cause an
/// OOM-allocation if used directly as a buffer size.
fn read_logical_block_size(disk_name: &str) -> u64 {
    let path = format!("/sys/class/block/{}/queue/logical_block_size", disk_name);
    let raw = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(512);
    // Only 512 and 4096 are valid physical block sizes on real hardware.
    // Reject anything else to prevent a corrupt sysfs value causing a
    // gigabyte-sized heap allocation followed by an OOM kernel panic.
    if raw == 512 || raw == 4096 { raw } else { 512 }
}

/// Parse a GPT PARTUUID string into the 16-byte mixed-endian layout stored in
/// a GPT partition entry.
///
/// Accepted input: exactly `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (36 chars),
/// hyphens at positions 8/13/18/23, hex digits elsewhere (upper or lowercase).
/// Any deviation – wrong length, wrong hyphen positions, non-hex chars – returns None.
///
/// On success the bytes are swapped into UEFI mixed-endian order so the result
/// can be compared directly against raw GPT partition entry bytes 16..32.
/// MBR PARTUUIDs are not supported – FinchBerryOS requires GPT.
fn parse_partuuid_to_bytes(uuid: &str) -> Option<[u8; 16]> {
    // Enforce canonical UUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    // Length must be exactly 36, hyphens at positions 8, 13, 18, 23, all
    // other characters must be hex digits (upper or lowercase both accepted).
    // This matches the "single source of truth" contract and the error message
    // in validate_config() – the parser is at least as strict as its own docs.
    let b = uuid.as_bytes();
    if b.len() != 36 { return None; }
    if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' { return None; }
    let hex_positions = (0..36).filter(|&i| i != 8 && i != 13 && i != 18 && i != 23);
    for i in hex_positions {
        if !b[i].is_ascii_hexdigit() { return None; }
    }
    // Collect the 32 hex digits in order and decode.
    let hex: String = b.iter()
        .filter(|&&c| c != b'-')
        .map(|&c| c as char)
        .collect();
    let mut raw = [0u8; 16];
    for i in 0..16 {
        raw[i] = u8::from_str_radix(&hex[i*2..i*2+2], 16).ok()?;
    }
    // UEFI stores the first three groups little-endian (mixed-endian on-disk layout).
    raw[0..4].reverse();   // time_low
    raw[4..6].reverse();   // time_mid
    raw[6..8].reverse();   // time_hi
    Some(raw)
}

/// Try to find a partition with the given PARTUUID on a specific disk.
/// `target_bytes` is the pre-parsed 16-byte GUID in disk layout (mixed-endian).
/// Returns the /dev/<partname> path if found.
fn scan_disk_for_partuuid(disk_dev: &str, disk_name: &str, target_str: &str, target_bytes: &[u8; 16]) -> Option<String> {
    let mut f = File::open(disk_dev).ok()?;

    // Read the actual logical block size from sysfs – supports both 512/512e and 4Kn drives.
    let lbs = read_logical_block_size(disk_name);

    // LBA 1 holds the GPT primary header.
    let mut header = vec![0u8; lbs as usize];
    f.seek(SeekFrom::Start(lbs)).ok()?;
    f.read_exact(&mut header).ok()?;

    // Validate GPT signature ("EFI PART" at offset 0 of the header)
    if read_u64_le(&header, 0)? != GPT_HEADER_SIGNATURE {
        return None; // not a GPT disk
    }

    let part_entry_lba  = read_u64_le(&header, 72)?;
    let num_part_entries = read_u32_le(&header, 80)?;
    let part_entry_size  = read_u32_le(&header, 84)? as u64;

    // Safety caps per UEFI spec 2.10 §5.3:
    //   - entry size: minimum 128 bytes; real-world tools write exactly 128.
    //   - entry count: UEFI minimum is 128; many partitioning tools (gdisk,
    //     parted) write 128 or 256. We cap at 256 to cover both, while still
    //     bounding the loop against a corrupt header claiming millions of entries.
    if part_entry_size < 128 || part_entry_size > 512 { return None; }
    let safe_count = num_part_entries.min(256);

    // Allocate the entry buffer once outside the loop – all entries are the same size.
    let mut entry = vec![0u8; part_entry_size as usize];

    // Pre-compute the base byte offset of the partition array once.
    // The per-entry offset is: base + i * part_entry_size.
    let array_base = match part_entry_lba.checked_mul(lbs) {
        Some(v) => v,
        None => {
            klog!("WARN: GPT array base overflow (part_entry_lba={} lbs={}), skipping disk",
                  part_entry_lba, lbs);
            return None;
        }
    };

    for i in 0..safe_count {
        // Use checked arithmetic to guard against corrupt GPT headers.
        let byte_offset = match (i as u64)
            .checked_mul(part_entry_size)
            .and_then(|off| array_base.checked_add(off))
        {
            Some(v) => v,
            None => { klog!("WARN: GPT byte_offset overflow at entry {}, stopping scan", i); break; }
        };
        if f.seek(SeekFrom::Start(byte_offset)).is_err() { break; }
        if f.read_exact(&mut entry).is_err() { break; }

        // Bytes 0..16: partition type GUID – all zeros means unused entry
        if entry[0..16].iter().all(|&b| b == 0) { continue; }

        // Bytes 16..32: partition unique GUID (PARTUUID).
        // Compare raw bytes directly against the pre-parsed target – no String
        // allocation per entry, no format! in the hot loop.
        // get() + try_into() instead of direct index + unwrap() for consistency
        // with the rest of the Option-based GPT parsing in this function.
        let raw_guid: &[u8; 16] = match entry.get(16..32).and_then(|s| s.try_into().ok()) {
            Some(g) => g,
            None    => break, // entry buffer too short – corrupt table, stop scan
        };

        if raw_guid == target_bytes {
            let part_start_lba = read_u64_le(&entry, 32)?;

            if let Ok(children) = fs::read_dir(format!("/sys/class/block/{}", disk_name)) {
                for child in children.flatten() {
                    let child_name = child.file_name().to_string_lossy().to_string();

                    // Match only direct children of this disk.
                    // NVMe: nvme0n1p1 (disk + "p" + digit)
                    // SCSI/SATA: sda1 (disk + digit)
                    let suffix = child_name.strip_prefix(disk_name).unwrap_or("");
                    let is_partition =
                        (suffix.starts_with('p') && suffix.len() > 1)
                        || suffix.chars().next().is_some_and(|c| c.is_ascii_digit());
                    if !is_partition { continue; }

                    let start_path = child.path().join("start");
                    if let Ok(s) = fs::read_to_string(&start_path) {
                        if s.trim().parse::<u64>().ok() == Some(part_start_lba) {
                            klog!("PARTUUID {} → /dev/{}", target_str, child_name);
                            return Some(format!("/dev/{}", child_name));
                        }
                    }
                }
            }
            klog!("WARN: GUID match for {} but no sysfs child with start={}", target_str, part_start_lba);
        }
    }
    None
}

/// Poll for a partition by PARTUUID using direct GPT parsing.
/// Works without udev – only requires devtmpfs + sysfs (both mounted in setup_vfs).
fn wait_for_partuuid(target_uuid: &str, timeout_secs: u64) -> Option<String> {
    let start = Instant::now();
    // Pre-parse the PARTUUID string to bytes once, outside the poll loop.
    // This avoids any String allocation or format! in the hot scan loop.
    // parse_partuuid_to_bytes accepts upper and lowercase – no need to normalise.
    // validate_config() has already verified this UUID – failure here is a bug.
    let needle_bytes = parse_partuuid_to_bytes(target_uuid)
        .unwrap_or_else(|| fatal_error(&format!(
            "internal error: validated PARTUUID '{}' failed to parse – this is a bug",
            target_uuid
        )));

    while start.elapsed().as_secs() < timeout_secs {
        // Enumerate all block devices visible in sysfs
        // Scan all whole-disk block devices for the target PARTUUID.
        // sysfs may not be fully populated yet on fast hardware – the sleep below
        // handles that race. We sleep unconditionally (not only on empty sysfs)
        // to avoid a busy-spin when sysfs is ready but the disk isn't yet visible.
        let mut found_any_disk = false;
        if let Ok(entries) = fs::read_dir("/sys/class/block") {
            for entry in entries.flatten() {
                let dev_name = entry.file_name().to_string_lossy().to_string();

                // We only want whole disk devices (no partitions themselves).
                // A whole disk has no "partition" attribute in sysfs.
                let part_attr = entry.path().join("partition");
                if part_attr.exists() { continue; }

                // Skip virtual/software block devices – they have no GPT
                if dev_name.starts_with("loop")
                    || dev_name.starts_with("ram")
                    || dev_name.starts_with("zram")
                    || dev_name.starts_with("dm-")
                    || dev_name.starts_with("md")
                {
                    continue;
                }

                let disk_dev = format!("/dev/{}", dev_name);
                // sysfs may expose the device before devtmpfs has created the
                // /dev node. Skip explicitly rather than relying on the implicit
                // ok()? inside scan_disk_for_partuuid – makes the race visible.
                // found_any_disk is set only after confirming the /dev node exists,
                // so the "no disk visible" warning is accurate: it means there is
                // nothing scannable yet, not just nothing in sysfs.
                if !Path::new(&disk_dev).exists() {
                    continue;
                }
                found_any_disk = true;
                if let Some(found) = scan_disk_for_partuuid(&disk_dev, &dev_name, target_uuid, &needle_bytes) {
                    return Some(found);
                }
            }
        }
        if !found_any_disk {
            klog!("WARN: no disk devices visible in sysfs yet, waiting...");
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
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = Instant::now(); // single timestamp per iteration
            if loop_control_path.exists() {
                match LoopControl::open() {
                    Ok(lc) => break lc,
                    Err(e) if now < deadline => {
                        klog!("WARN: LoopControl::open failed ({}), retrying...", e);
                        sleep(Duration::from_millis(50));
                    }
                    Err(e) => fatal_error(&format!("LoopControl::open: {}", e)),
                }
            } else if now < deadline {
                sleep(Duration::from_millis(50));
            } else {
                fatal_error("/dev/loop-control did not appear within 5s");
            }
        }
    };
    let loop_dev = lc.next_free()
        .unwrap_or_else(|e| fatal_error(&format!(
            "No free loop device: {} (hint: kernel max_loop limit reached? try boot param max_loop=N)",
            e
        )));
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
        "run", "tmp", "usr", "bin", "sbin", "mnt/system",
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

fn setup_tmp(staging: &Path) {
    let path = staging.join("tmp");
    mount(Some("tmpfs"), &path, Some("tmpfs"), MsFlags::empty(), None::<&str>)
       .unwrap_or_else(|e| fatal_error(&format!("Mount tmpfs for /tmp: {}", e)));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o1777))
       .unwrap_or_else(|e| fatal_error(&format!("chmod 1777 /tmp: {}", e)));
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

    setup_tmp(staging);
}

/// Release the initramfs tmpfs pages back to the kernel allocator.
///
/// Must be called AFTER all bind/move mounts are set up inside /system/rootfs
/// but BEFORE chroot(). At this point:
///   - CWD is /system/rootfs  (after Step 2 chdir)
///   - The new root is already a proper mountpoint (after Step 1 MS_BIND)
///   - The old initramfs tmpfs is still the process root ("/")
///   - All real data is bind-mounted inside /system/rootfs – nothing in "/"
///     (outside that subtree) is needed any more
///
/// We walk the initramfs root and unlink everything that is NOT a mountpoint
/// (i.e. plain files and empty dirs in the initramfs tmpfs itself). Entries
/// that are mountpoints are skipped – the kernel will handle them when the
/// mount namespace is updated. After exec() the process image is replaced and
/// the last reference to the tmpfs drops, freeing all remaining pages.
///
/// On embedded targets with 128–256 MB RAM this typically recovers 2–8 MB.
/// On desktops/laptops it is hygienic but not critical.
/// `active_mounts` must be collected from /proc/mounts BEFORE MS_MOVE is executed,
/// while /proc is still accessible from the old initramfs root.
/// After MS_MOVE /system/rootfs → /, the old /proc is an empty directory and
/// /proc/mounts would return an empty string, making the mountpoint guard useless.
fn free_initramfs(active_mounts: &HashSet<String>) {
    // Approach: explicit allowlist of known initramfs skeleton paths rather than
    // a generic "/" scan. The generic scan is elegant but produces noisy EBUSY
    // errors for parent directories of active mounts (/system, /mnt) that are
    // not themselves mountpoints and therefore not in active_mounts.
    //
    // This mirrors what busybox switch_root does: delete known entries by name,
    // skip anything that fails silently. Safe because:
    //   - Files/dirs in this list are exclusively initramfs tmpfs content.
    //   - All real data has been bind/move-mounted into /system/rootfs already.
    //   - active_mounts provides a final safety net for any unexpected mounts.
    //
    // Directories are removed with remove_dir (only succeeds if empty).
    // The /system and /mnt dirs may have submounts and will fail with EBUSY –
    // that is expected and logged at DEBUG level, not as a warning.

    // Known initramfs skeleton entries (files + empty dirs after mounts moved out).
    // /mnt and /system are intentionally excluded: they are parent directories of
    // active bind/loop mounts. Touching them buys zero RAM and risks confusion.
    // Only unambiguous initramfs-only paths appear here.
    let candidates: &[&str] = &[
        "/init",
        "/pivot.config",
        "/dev",   // empty after MS_MOVE
        "/proc",  // empty after MS_MOVE
        "/sys",   // empty after MS_MOVE
        "/bin",
        "/sbin",
        "/usr",
        "/lib",
        "/lib64",
        "/etc",
    ];

    for path_str in candidates {
        // Final safety net: never touch an active mountpoint
        if active_mounts.contains(*path_str) {
            klog!("free_initramfs: skipping active mountpoint {}", path_str);
            continue;
        }
        let path = Path::new(path_str);
        if !path.exists() { continue; }

        let result = if path.is_dir() {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        };
        match result {
            Ok(_)  => klog!("free_initramfs: removed {}", path_str),
            Err(e) => klog!("free_initramfs: skipped {} ({})", path_str, e),
        }
    }
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

    // Step 3: collect active mountpoints NOW – while /proc is still mounted at "/proc"
    // in the old initramfs root. After MS_MOVE below, the old /proc becomes an
    // empty directory and /proc/mounts would return nothing.
    let active_mounts: HashSet<String> = fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
        .collect();

    // Step 4: atomically move new_root onto /
    // Now that new_root is a proper mountpoint, MS_MOVE works correctly.
    mount(Some(new_root), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
       .unwrap_or_else(|e| fatal_error(&format!("MS_MOVE new_root onto /: {}", e)));

    // Step 5: free the initramfs tmpfs pages before chroot.
    // CWD is now "." = /system/rootfs (the new root, not yet chrooted into).
    // The process root is still the old initramfs – "/" still addresses it.
    // The mountpoint set collected in Step 3 correctly identifies what must NOT
    // be unlinked (active mounts), even though /proc is no longer readable here.
    free_initramfs(&active_mounts);

    // Step 6: update the kernel root pointer
    chroot(".")
       .unwrap_or_else(|e| fatal_error(&format!("chroot to new root: {}", e)));

    // Step 7: chdir to / inside the new root
    chdir("/")
       .unwrap_or_else(|e| fatal_error(&format!("chdir to / in new root: {}", e)));

    // Step 8: wire up stdin/stdout/stderr to /dev/console for syscored.
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

    // Step 9: sanitize the environment before handing off to syscored.
    // The kernel and bootloader (GRUB, U-Boot, EFI stub) may have injected
    // arbitrary variables into our environment: initrd paths, EFI variables,
    // GRUB_* prefixed strings, or even secrets. syscored must start with a
    // clean, deterministic environment – it will set its own variables.
    // We keep exactly: TERM (console type, needed by login/getty helpers)
    // and PATH (sane default so syscored can exec helpers without abs paths).
    //
    // Safety: remove_var / set_var are unsafe since Rust 1.81 because concurrent
    // reads of the environment from other threads would cause a data race.
    // This is safe here because:
    //   (a) pivot is single-threaded – no other thread can be reading environ[].
    //   (b) All keys are collected into an owned Vec BEFORE removal, so the
    //       vars_os() iterator is no longer live during the mutation loop.
    //   (c) This runs immediately before exec() – nothing after set_var can race.
    {
        let term = std::env::var("TERM").unwrap_or_else(|_| "linux".to_string());
        let keys: Vec<std::ffi::OsString> = std::env::vars_os()
            .map(|(k, _)| k)
            .collect();
        unsafe {
            for k in keys {
                std::env::remove_var(&k);
            }
            std::env::set_var("TERM", &term);
            std::env::set_var("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        }
    }

    // Step 10: hand off to the real init
    klog!("Executing /usr/libexec/syscored ...");
    let err = Command::new("/usr/libexec/syscored").exec();
    fatal_error(&format!("exec /usr/libexec/syscored failed: {}", err));
}

/// Validate parsed config values beyond what serde can check.
/// Called immediately after read_config so all fatal errors happen early,
/// before any hardware interaction.
fn validate_config(cfg: &PivotConfig) {
    // PARTUUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx  (GPT UUID, 8-4-4-4-12 hex)
    // FinchBerryOS requires GPT – MBR is not supported.
    // parse_partuuid_to_bytes() is the single source of truth for UUID validity.
    // Calling it here (before hardware access) means any format error is reported
    // against the config file, not as a mysterious failure inside wait_for_partuuid.
    for (name, uuid) in &[
        ("boot_partition_uuid",   &cfg.hardware.boot_partition_uuid),
        ("system_partition_uuid", &cfg.hardware.system_partition_uuid),
    ] {
        if parse_partuuid_to_bytes(uuid).is_none() {
            fatal_error(&format!(
                "config: {} '{}' is not a valid GPT UUID (expected xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)",
                name, uuid
            ));
        }
    }
    // Image filenames: plain filename only, no directory separators
    for (name, img) in &[("slot_a", &cfg.images.slot_a), ("slot_b", &cfg.images.slot_b)] {
        if img.is_empty() || img.contains('/') || img.contains("..") {
            fatal_error(&format!(
                "config: images.{} '{}' is empty or contains illegal path characters",
                name, img
            ));
        }
    }
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
    // Attempt the tmpfs mount unconditionally and tolerate EBUSY.
    // A check-then-mount pattern has a TOCTOU race: another process could mount
    // between the /proc/mounts read and the mount(2) call (unlikely but possible
    // if the updater is resumed after a crash with a pre-existing tmpfs).
    // EBUSY means /tmp is already a mountpoint – that is exactly what we want,
    // so treat it as success. Any other error is fatal.
    match mount(None::<&str>, "/tmp", Some("tmpfs"), MsFlags::empty(), None::<&str>) {
        Ok(_) => {}
        Err(nix::errno::Errno::EBUSY) =>
            klog!("INFO: /tmp already mounted (resumed from previous attempt)"),
        Err(e) => fatal_error(&format!("Mount tmpfs on /tmp: {}", e)),
    }
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