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
        // Use compare_exchange instead of a plain store so that if a re-entrant
        // call (e.g. a panic hook firing mid-open) races with us, the losing
        // caller closes its own FD rather than leaking it.
        //
        // The brief window between into_raw_fd() and compare_exchange where a
        // re-entrant panic could cause an FD leak is explicitly accepted: the
        // worst outcome is a single leaked FD in an already-panicking PID 1
        // that is about to fatal_error anyway.
        use std::os::unix::io::IntoRawFd;
        if let Ok(f) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(OFlag::O_CLOEXEC.bits())
            .open("/dev/kmsg")
        {
            let new_fd = f.into_raw_fd();
            // Between into_raw_fd() above and compare_exchange below, a
            // re-entrant panic-hook call to kmsg_write could observe fd < 0
            // and open a second /dev/kmsg. The CAS ensures exactly one FD is
            // stored; the loser closes its copy.
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
enum ActiveSlot {
    #[serde(alias = "a")]
    A,
    #[serde(alias = "b")]
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

impl ImagesConfig {
    /// Return all (name, path) slot pairs.
    ///
    /// Centralises slot enumeration so that adding slot_c only requires
    /// updating this method and the ImagesConfig struct. Both validate_images()
    /// and any future caller that iterates slots consume this method – there is
    /// no second place to forget when the slot list grows.
    fn all_slots(&self) -> [(&'static str, &String); 2] {
        [("slot_a", &self.slot_a), ("slot_b", &self.slot_b)]
    }
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
    // VFS FIRST: devtmpfs must be mounted before klog! can open /dev/kmsg.
    // Note: setup_vfs mounts only /dev, /proc, /sys – the kernel virtuals.
    // /tmp and /run are intentionally deferred: they belong in the new root
    // and are mounted later by move_vfs_to_new_root() inside /system/rootfs.
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
            "mode = \"live\" is not supported by this pivot binary. \
             Use a live-boot enabled initramfs instead."
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

    // 4. Slot selection
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

    // 5. Mount active slot image (SquashFS via loop device → /system/base_root).
    // stage_system() returns the loop device path so execute_ram_update() can
    // unmount the squashfs and release the loop device before exec'ing the updater.
    let loop_dev_path = stage_system(active_image);

    // 6. Update check – must run AFTER the active slot is mounted so the
    // updateinstaller binary can be read from the active rootfs (/system/base_root).
    if check_for_updates() {
        klog!("Update trigger found – launching RAM updater.");
        execute_ram_update(&sp_dev, &bp_dev, &loop_dev_path);
        unreachable!();
    }

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
//   4. Open /dev/<disk> and read the GPT primary header + partition table
//   5. Compare each partition entry's GUID against the target PARTUUID
//   6. If the primary header is corrupt/missing, fall back to the
//      GPT backup header at the last LBA (UEFI spec §5.3 requirement).
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
/// Clamps to [512, 4096] – the only valid physical block sizes on real hardware.
/// Values outside this range indicate a corrupt sysfs entry and would cause an
/// OOM-allocation if used directly as a buffer size.
fn read_logical_block_size(disk_name: &str) -> u64 {
    let path = format!("/sys/class/block/{}/queue/logical_block_size", disk_name);
    let raw = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(512);
    if raw == 512 || raw == 4096 { raw } else { 512 }
}

/// Read the total size of a disk in logical blocks from sysfs.
///
/// The sysfs `size` attribute reports 512-byte sectors regardless of the
/// physical/logical block size. We convert to LBAs by dividing by (lbs / 512).
/// Returns None if the attribute is missing or unparseable.
///
/// Used by try_backup_gpt_header() to locate the last LBA without an additional
/// ioctl (BLKGETSIZE64), keeping the code free of ioctl dependencies.
fn read_disk_size_in_lba(disk_name: &str, lbs: u64) -> Option<u64> {
    let path = format!("/sys/class/block/{}/size", disk_name);
    let sectors_512 = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())?;
    // lbs is always 512 or 4096 (enforced by read_logical_block_size).
    // lbs / 512 is therefore 1 or 8 – no overflow risk.
    let lbs_per_sector = lbs / 512;
    if lbs_per_sector == 0 { return None; }
    Some(sectors_512 / lbs_per_sector)
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
    let b = uuid.as_bytes();
    if b.len() != 36 { return None; }
    if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' { return None; }
    let hex_positions = (0..36).filter(|&i| i != 8 && i != 13 && i != 18 && i != 23);
    for i in hex_positions {
        if !b[i].is_ascii_hexdigit() { return None; }
    }
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

/// Scan one validated GPT header (already read into `header`) for `target_bytes`.
///
/// Extracted into a shared helper so both scan_disk_for_partuuid (primary header)
/// and try_backup_gpt_header (backup header) reuse the same partition-entry
/// scan loop without duplication.
fn scan_gpt_header_for_partuuid(
    f:            &mut File,
    disk_name:    &str,
    target_str:   &str,
    target_bytes: &[u8; 16],
    header:       &[u8],
    lbs:          u64,
) -> Option<String> {
    let part_entry_lba   = read_u64_le(header, 72)?;
    let num_part_entries = read_u32_le(header, 80)?;
    let part_entry_size  = read_u32_le(header, 84)? as u64;

    if part_entry_size < 128 || part_entry_size > 512 { return None; }

    const MAX_PARTITION_ENTRIES: u32 = 128;
    if num_part_entries > MAX_PARTITION_ENTRIES {
        klog!("WARN: GPT on {} reports {} partition entries; scanning only first {} \
               (FinchBerryOS supports max {} partitions)",
              disk_name, num_part_entries, MAX_PARTITION_ENTRIES, MAX_PARTITION_ENTRIES);
    }
    let safe_count = num_part_entries.min(MAX_PARTITION_ENTRIES);

    let mut entry = vec![0u8; part_entry_size as usize];

    // CRITICAL FIX #2: Validate part_entry_lba against the actual disk size
    // before entering the scan loop. A corrupt GPT header (primary or backup)
    // can contain an arbitrarily large part_entry_lba. Without this check,
    // scan_gpt_header_for_partuuid would execute up to MAX_PARTITION_ENTRIES
    // iterations, each issuing a seek+read that immediately fails with EINVAL
    // or EIO. On slow storage (eMMC, SD) those 128 failing syscalls add
    // measurable latency to every disk that has a corrupt header during the
    // wait_for_partuuid polling loop.
    //
    // If the disk size is unavailable from sysfs we skip the check and proceed:
    // the worst case is the same 128 failed seeks we had before – no regression.
    if let Some(disk_lba) = read_disk_size_in_lba(disk_name, lbs) {
        if part_entry_lba >= disk_lba {
            klog!("WARN: GPT part_entry_lba {} beyond disk size {} LBAs on {}, skipping",
                  part_entry_lba, disk_lba, disk_name);
            return None;
        }
    }

    let array_base = match part_entry_lba.checked_mul(lbs) {
        Some(v) => v,
        None => {
            klog!("WARN: GPT array base overflow (part_entry_lba={} lbs={}), skipping",
                  part_entry_lba, lbs);
            return None;
        }
    };

    for i in 0..safe_count {
        let byte_offset = match (i as u64)
            .checked_mul(part_entry_size)
            .and_then(|off| array_base.checked_add(off))
        {
            Some(v) => v,
            None => {
                klog!("WARN: GPT byte_offset overflow at entry {}, stopping scan", i);
                break;
            }
        };
        if f.seek(SeekFrom::Start(byte_offset)).is_err() { break; }
        if f.read_exact(&mut entry).is_err() { break; }

        // Bytes 0..16: partition type GUID – all zeros means unused entry
        if entry[0..16].iter().all(|&b| b == 0) { continue; }

        // Bytes 16..32: partition unique GUID (PARTUUID)
        let raw_guid: &[u8; 16] = match entry.get(16..32).and_then(|s| s.try_into().ok()) {
            Some(g) => g,
            None    => break,
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

/// Attempt to read the GPT backup header from the last LBA.
///
/// The UEFI spec (§5.3) requires an identical backup GPT header at the very
/// last LBA of the disk. gdisk, parted, and sgdisk all write it unconditionally.
/// A partially-written disk (interrupted dd, sgdisk crash) may have a valid
/// backup header even when the primary is corrupt or missing.
///
/// We read the disk size from sysfs (no ioctl needed), seek to the last LBA,
/// validate the signature, and hand off to scan_gpt_header_for_partuuid.
/// Returns None if the disk size is unavailable or the backup header is also invalid.
fn try_backup_gpt_header(
    f:            &mut File,
    disk_name:    &str,
    lbs:          u64,
    target_str:   &str,
    target_bytes: &[u8; 16],
) -> Option<String> {
    let total_lba = read_disk_size_in_lba(disk_name, lbs)?;
    if total_lba == 0 { return None; }
    let backup_lba = total_lba - 1;

    let mut header = vec![0u8; lbs as usize];
    f.seek(SeekFrom::Start(backup_lba * lbs)).ok()?;
    f.read_exact(&mut header).ok()?;

    if read_u64_le(&header, 0)? != GPT_HEADER_SIGNATURE {
        // Both primary and backup headers invalid – disk is not GPT or fully corrupt.
        return None;
    }

    klog!("INFO: primary GPT header corrupt on {}, using backup at LBA {}", disk_name, backup_lba);
    scan_gpt_header_for_partuuid(f, disk_name, target_str, target_bytes, &header, lbs)
}

/// Try to find a partition with the given PARTUUID on a specific disk.
/// `target_bytes` is the pre-parsed 16-byte GUID in disk layout (mixed-endian).
/// Returns the /dev/<partname> path if found.
fn scan_disk_for_partuuid(
    disk_dev:     &str,
    disk_name:    &str,
    target_str:   &str,
    target_bytes: &[u8; 16],
) -> Option<String> {
    let mut f = File::open(disk_dev).ok()?;
    let lbs = read_logical_block_size(disk_name);

    // LBA 1 holds the GPT primary header.
    let mut header = vec![0u8; lbs as usize];
    f.seek(SeekFrom::Start(lbs)).ok()?;
    f.read_exact(&mut header).ok()?;

    // If the primary GPT signature is absent, fall back to the backup header
    // at the last LBA before giving up on this disk. If the scan of the
    // primary header returns None (GUID absent from a valid table), we do NOT
    // fall back – the tables are identical and scanning twice is redundant noise.
    if read_u64_le(&header, 0)? != GPT_HEADER_SIGNATURE {
        return try_backup_gpt_header(&mut f, disk_name, lbs, target_str, target_bytes);
    }

    scan_gpt_header_for_partuuid(&mut f, disk_name, target_str, target_bytes, &header, lbs)
}

/// Poll for a partition by PARTUUID using direct GPT parsing.
/// Works without udev – only requires devtmpfs + sysfs (both mounted in setup_vfs).
fn wait_for_partuuid(target_uuid: &str, timeout_secs: u64) -> Option<String> {
    let start = Instant::now();
    // Pre-parse once, outside the poll loop – no allocation per iteration.
    // validate_config() has already verified this UUID; failure here is a bug.
    let needle_bytes = parse_partuuid_to_bytes(target_uuid)
        .unwrap_or_else(|| fatal_error(&format!(
            "internal error: validated PARTUUID '{}' failed to parse – this is a bug",
            target_uuid
        )));

    loop {
        let elapsed = start.elapsed();
        if elapsed.as_secs() >= timeout_secs { break; }

        let mut found_any_disk = false;
        if let Ok(entries) = fs::read_dir("/sys/class/block") {
            for entry in entries.flatten() {
                let dev_name = entry.file_name().to_string_lossy().to_string();

                // Skip partition entries – we want whole disks only.
                let part_attr = entry.path().join("partition");
                if part_attr.exists() { continue; }

                // Skip virtual/software block devices – they have no GPT.
                if dev_name.starts_with("loop")
                    || dev_name.starts_with("ram")
                    || dev_name.starts_with("zram")
                    || dev_name.starts_with("dm-")
                    || dev_name.starts_with("md")
                {
                    continue;
                }

                let disk_dev = format!("/dev/{}", dev_name);
                // sysfs may expose the device before devtmpfs creates the /dev node.
                if !Path::new(&disk_dev).exists() { continue; }

                found_any_disk = true;
                if let Some(found) = scan_disk_for_partuuid(&disk_dev, &dev_name, target_uuid, &needle_bytes) {
                    return Some(found);
                }
            }
        }

        if !found_any_disk {
            klog!("WARN: no disk devices visible in sysfs yet, waiting...");
        }

        // Cap the sleep to the remaining timeout so the actual wall time
        // never significantly exceeds timeout_secs. Without this cap, a single
        // scan pass on a system with many disks can take >200 ms, pushing the
        // total wait time well beyond timeout_secs and misleading the operator
        // about when the timeout fired.
        let remaining = Duration::from_secs(timeout_secs)
            .saturating_sub(start.elapsed());
        if remaining.is_zero() { break; }
        sleep(remaining.min(Duration::from_millis(200)));
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

/// Mount the active slot image and set up the staging root.
///
/// Returns the path of the loop device (e.g. `/dev/loop0`) that backs the
/// squashfs mount at `/system/base_root`. The caller must keep this path to
/// pass to `execute_ram_update()` if an update is triggered – it is needed to
/// unmount the squashfs and detach the loop device before exec'ing the updater.
///
/// For the normal (non-update) boot path the loop device stays alive via
/// `autoclear`: the kernel releases it automatically when the last reference
/// (the squashfs mount) is dropped, which happens implicitly after exec().
fn stage_system(active_image: &str) -> std::path::PathBuf {
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
            let now = Instant::now();
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

    // Capture the loop device path before forgetting the handle.
    // Returned to the caller so execute_ram_update() can unmount squashfs and
    // detach the loop device if an update is triggered.
    // For the normal boot path autoclear releases the loop device after exec().
    let loop_dev_path = block_dev.to_path_buf();
    std::mem::forget(loop_dev);

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

    loop_dev_path
}

fn setup_vfs() {
    // /proc, /sys, /dev must exist as empty dirs in the initramfs cpio archive.
    // Mount /dev FIRST so that /dev/kmsg is available for fatal_error's Stage 1
    // (sysrq-trigger lives under /proc which comes second).
    // NOTE: /tmp and /run are NOT mounted here. They belong in the new root
    // (/system/rootfs) and are set up later by move_vfs_to_new_root() so they
    // are backed by the correct tmpfs instances after switch_root.
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
/// but BEFORE chroot(). At this point "/" still addresses the old initramfs root
/// and "." (CWD) addresses the new root. The mountpoint set passed in must have
/// been collected from /proc/mounts BEFORE MS_MOVE – see perform_pivot_and_exec.
///
/// We iterate an explicit allowlist of initramfs-only paths. Parent directories
/// of active mounts (/system, /mnt) are intentionally excluded: they are not
/// themselves mountpoints but touching them would raise EBUSY.
///
/// CRITICAL FIX #1: Use remove_dir (not remove_dir_all) + remove_file fallback.
///
/// remove_dir_all is dangerous here because it recurses into subdirectories
/// and does NOT stop at mountpoints inside them. An initramfs /etc that happens
/// to contain a bind-mount (e.g. /etc/resolv.conf) would have its mountpoint
/// directory entry deleted by remove_dir_all even though the mount is still
/// live in the kernel. The kernel mount remains but the path disappears,
/// leaving the system in a subtly broken state that is hard to diagnose.
///
/// busybox switch_root and systemd avoid remove_dir_all for exactly this reason.
/// We accept that non-empty directories (e.g. /usr with Busybox hardlinks) will
/// not be freed at this stage – they will be released when the last reference
/// to the initramfs tmpfs drops after exec(). The RAM saving from recursion is
/// marginal (Busybox hardlinks share a single inode) and not worth the risk.
///
/// /pivot.config appears both as the sentinel check and in the candidates list.
/// It MUST remain in candidates so it is deleted after the sentinel check passes;
/// removing it from candidates would leave the file on the initramfs permanently.
fn free_initramfs(active_mounts: &HashSet<String>) {
    // Sanity-check: /pivot.config must be visible via "/" to confirm we are
    // still addressing the old initramfs root (not the new one after chroot).
    // /pivot.config lives only in the initramfs and is never copied to the SP,
    // so its absence at "/" means something has gone wrong with the root switch.
    // Bail out rather than silently deleting files from the new root.
    if !Path::new("/pivot.config").exists() {
        klog!("WARN: free_initramfs: /pivot.config missing at '/' – \
               root may already be switched, skipping cleanup to avoid data loss");
        return;
    }

    let candidates: &[&str] = &[
        "/init",
        "/pivot.config", // sentinel – must stay in this list (see doc comment above)
        "/dev",          // empty after MS_MOVE
        "/proc",         // empty after MS_MOVE
        "/sys",          // empty after MS_MOVE
        "/bin",
        "/sbin",
        "/usr",
        "/lib",
        "/lib64",
        "/etc",
    ];

    for path_str in candidates {
        // Final safety net: never touch an active mountpoint or its subtree.
        if active_mounts.contains(*path_str) {
            klog!("free_initramfs: skipping active mountpoint {}", path_str);
            continue;
        }
        let path = Path::new(path_str);

        // CRITICAL FIX #1: remove_dir for directories (fails safely with
        // ENOTEMPTY if non-empty – no risk of crossing hidden mountpoints),
        // remove_file fallback for plain files and symlinks.
        // Non-empty dirs are skipped with a log entry; their pages will be
        // reclaimed when the initramfs tmpfs reference count drops to zero
        // after exec() replaces the process image.
        let result = fs::remove_dir(path).or_else(|_| fs::remove_file(path));
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
///   1. MS_BIND  – make new_root an explicit mountpoint
///   2. chdir    – move CWD into new root
///   3. Snapshot /proc/mounts while old /proc is still accessible
///   4. MS_MOVE  – atomically move new_root onto /
///   5. free_initramfs – release old tmpfs pages (/ still = old root)
///   6. chroot(".") – update the kernel's root pointer to CWD
///   7. exec the real init
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

    // Step 5: free the initramfs tmpfs pages.
    // After MS_MOVE: CWD "." = new root (not yet chrooted), "/" = old initramfs.
    // free_initramfs addresses all candidate paths via "/" which still reaches
    // the old initramfs root. The /pivot.config sentinel inside free_initramfs
    // confirms this invariant before deleting anything.
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
    // Rust 1.81 made env mutation unsafe to force callers to justify thread safety.
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
    klog!("Executing /sbin/syscored ...");
    let err = Command::new("/sbin/syscored").exec();
    fatal_error(&format!("exec /sbin/syscored failed: {}", err));
}

/// Validate parsed config values beyond what serde can check.
/// Called immediately after read_config so all fatal errors happen early,
/// before any hardware interaction.
fn validate_config(cfg: &PivotConfig) {
    // PARTUUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx  (GPT UUID, 8-4-4-4-12 hex)
    // FinchBerryOS requires GPT – MBR is not supported.
    // parse_partuuid_to_bytes() is the single source of truth for UUID validity.
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
    validate_images(&cfg.images);
}

/// Validate all image slot filenames in ImagesConfig.
///
/// Iterates images.all_slots() instead of a hand-written array literal.
/// Adding slot_c to ImagesConfig and all_slots() automatically validates it here
/// without any change to this function.
fn validate_images(images: &ImagesConfig) {
    for (name, img) in &images.all_slots() {
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

/// Tear down every mount that references the active slot image, then detach
/// the loop device.
///
/// stage_system() builds the following mount tree from the active squashfs:
///
///   /dev/loopN  →  squashfs  →  /system/base_root   (RO squashfs mount)
///                                      ├── System  →  bind →  /system/rootfs/System
///                                      ├── usr     →  bind →  /system/rootfs/usr
///                                      ├── bin     →  bind →  /system/rootfs/bin
///                                      └── sbin    →  bind →  /system/rootfs/sbin
///
/// The bind mounts in /system/rootfs/* hold open references to the squashfs
/// page cache. Unmounting only /system/base_root (even with MNT_DETACH) does
/// NOT release the loop device while those bind mounts are still alive: the
/// kernel maintains a reference count per block device, and each bind mount
/// contributes one. The loop device will refuse LOOP_CTL_REMOVE (EBUSY) until
/// all references are dropped.
///
/// Correct teardown order:
///   1. Bind mounts from /system/rootfs/* first  (leaf nodes)
///   2. /system/base_root squashfs               (intermediate)
///   3. Loop device detach                        (root of the tree)
///
/// All bind-mount unmounts are fatal on failure: if any of them fails it means
/// the kernel still holds a live reference to the squashfs, and proceeding
/// would leave the updater trying to overwrite an image that is still in use.
///
/// The loop detach is also fatal. If all mounts above succeeded the loop device
/// has no remaining references and LOOP_CTL_REMOVE must succeed. A failure at
/// this point indicates an unexpected open fd (e.g. a kernel thread, or a bug
/// in pivot) – proceeding with the update is unsafe because the .img file on
/// the SP could be locked by the kernel's loop bookkeeping.
/// autoclear does NOT rescue this: autoclear fires when the loop's internal
/// reference count reaches zero, but that only happens after all mounts using
/// it are gone AND no fd pointing at /dev/loopN is open. If we reach detach
/// with an EBUSY, autoclear is also stuck.
fn unmount_active_slot_mounts(loop_dev_path: &Path) {
    // Step 1: unmount the bind mounts that read from the squashfs.
    // These are the leaf nodes – they must go first.
    // stage_system() skips a bind if the squashfs dir was absent (logs WARN),
    // so the bind may not exist here. EINVAL from umount2 means "not a mount
    // point" – that is the expected outcome for a skipped bind, so we log INFO
    // and continue. Any other error means the kernel refused the unmount while
    // the mount is still live → fatal.
    let bind_mounts = [
        "/system/rootfs/System",
        "/system/rootfs/usr",
        "/system/rootfs/bin",
        "/system/rootfs/sbin",
    ];
    for mnt in &bind_mounts {
        match umount2(*mnt, MntFlags::MNT_DETACH) {
            Ok(_) => klog!("unmount_active_slot_mounts: unmounted bind {}", mnt),
            Err(nix::errno::Errno::EINVAL) => {
                // Not a mountpoint – stage_system() skipped this bind because
                // the squashfs did not contain the directory. Safe to ignore.
                klog!("INFO: unmount_active_slot_mounts: {} was not mounted (skipped bind), ok", mnt);
            }
            Err(e) => fatal_error(&format!(
                "unmount_active_slot_mounts: failed to unmount bind {}: {} – \
                 active slot image still referenced, aborting update",
                mnt, e
            )),
        }
    }

    // Step 2: unmount the squashfs itself.
    // All bind mounts that consumed it are gone; this should succeed cleanly.
    // MNT_DETACH: if pivot itself somehow has an open fd on base_root (unlikely
    // but defensive), the detach makes the mountpoint unreachable immediately
    // while the kernel waits for the last fd to close before freeing the pages.
    umount2("/system/base_root", MntFlags::MNT_DETACH)
        .unwrap_or_else(|e| fatal_error(&format!(
            "unmount_active_slot_mounts: failed to unmount squashfs /system/base_root: {} – \
             cannot safely release the active image",
            e
        )));
    klog!("unmount_active_slot_mounts: unmounted squashfs /system/base_root");

    // Step 3: detach the loop device.
    // All mounts backed by this loop device are now gone (steps 1+2 were fatal
    // on error), so the loop's reference count must be zero. LOOP_CTL_REMOVE
    // must succeed. If it does not, something holds an unexpected reference
    // (kernel thread, leaked fd in pivot, driver bug). Proceeding is unsafe:
    // the updater would try to truncate/replace the .img file on the SP while
    // the kernel still has it mapped through the loop device.
    //
    // This is intentionally fatal rather than a WARN+continue:
    // autoclear is also stuck when EBUSY is returned here (see function doc),
    // so "autoclear will handle it" is NOT a valid fallback in this context.
    match loopdev::LoopDevice::open(loop_dev_path) {
        Ok(ld) => {
            ld.detach().unwrap_or_else(|e| fatal_error(&format!(
                "unmount_active_slot_mounts: loop detach {:?} failed: {} – \
                 all squashfs mounts are gone but the loop device is still busy; \
                 aborting to prevent concurrent access to the slot image",
                loop_dev_path, e
            )));
            klog!("unmount_active_slot_mounts: loop device {:?} detached", loop_dev_path);
        }
        Err(e) => fatal_error(&format!(
            "unmount_active_slot_mounts: cannot open loop device {:?} for detach: {} – \
             cannot verify the image is released",
            loop_dev_path, e
        )),
    }
}

/// Copy the updateinstaller from the active rootfs into RAM and exec it.
///
/// Called only when check_for_updates() returns true, AFTER stage_system() has
/// mounted the active slot image at /system/base_root.
///
/// Sequence:
///   1. Mount /tmp as tmpfs (RAM-backed).
///   2. Copy updateinstaller from the active rootfs (/system/base_root) to /tmp.
///   3. Fully release the active slot image via unmount_active_slot_mounts():
///      bind mounts → squashfs → loop device detach.
///   4. The System Partition stays mounted at /mnt/system so the updater can
///      read/write slot images and pivot.config without remounting.
///   5. exec the updater from RAM with --sp-dev and --bp-dev.
///
/// The Boot Partition is NOT mounted here. The updater mounts it itself via
/// --bp-dev when it needs to flash kernel/initramfs.
fn execute_ram_update(sp_dev: &str, bp_dev: &str, loop_dev_path: &Path) -> ! {
    // updateinstaller lives inside the active slot image.
    // /system/base_root is the squashfs mount of the active slot.
    let src = "/system/base_root/usr/libexec/updateinstaller";
    let dst = "/tmp/updateinstaller";

    if !Path::new(src).exists() {
        fatal_error(&format!(
            "updateinstaller not found in active rootfs at {} – \
             image may be corrupt or the path has changed",
            src
        ));
    }

    // Step 1: mount /tmp as tmpfs so the binary survives the squashfs unmount.
    // Tolerate EBUSY: /tmp may already be a tmpfs if we are resuming after a
    // crash that left the mount in place. Any other error is fatal.
    fs::create_dir_all("/tmp")
       .unwrap_or_else(|e| fatal_error(&format!("mkdir /tmp: {}", e)));
    match mount(None::<&str>, "/tmp", Some("tmpfs"), MsFlags::empty(), None::<&str>) {
        Ok(_) => {}
        Err(nix::errno::Errno::EBUSY) =>
            klog!("INFO: /tmp already mounted (resumed from previous attempt)"),
        Err(e) => fatal_error(&format!("Mount tmpfs on /tmp: {}", e)),
    }

    // Step 2: copy the binary into RAM BEFORE releasing the squashfs.
    fs::copy(src, dst)
       .unwrap_or_else(|e| fatal_error(&format!("Copy updateinstaller to RAM: {}", e)));
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
       .unwrap_or_else(|e| fatal_error(&format!("chmod updateinstaller: {}", e)));
    klog!("updateinstaller copied to RAM ({})", dst);

    // Step 3: fully release the active slot image.
    // This unmounts the bind mounts in /system/rootfs/* that still reference
    // the squashfs, then unmounts /system/base_root, then detaches the loop
    // device. All steps are fatal on error – see unmount_active_slot_mounts().
    unmount_active_slot_mounts(loop_dev_path);

    // Step 4: SP stays mounted at /mnt/system.
    // The updater reads /mnt/system/system/slot_*.img and writes pivot.config.

    // Step 5: exec the updater from RAM.
    klog!("Executing updateinstaller from RAM...");
    let err = Command::new(dst)
        .arg("--sp-dev").arg(sp_dev)
        .arg("--bp-dev").arg(bp_dev)
        .exec();
    fatal_error(&format!("exec updateinstaller failed: {}", err));
}