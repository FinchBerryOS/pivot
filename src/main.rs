use loopdev::LoopControl;
use nix::fcntl::OFlag;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{chdir, chroot, close, getpid};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};
use nix::libc;

// ────────────────────────────────────────────────────────────────────────────
// LOGGING  (writes to /dev/kmsg so output survives headless / serial boots)
// ────────────────────────────────────────────────────────────────────────────

static KMSG_FD: AtomicI32 = AtomicI32::new(-1);

fn kmsg_write(msg: &str) {
    let mut fd = KMSG_FD.load(Ordering::Relaxed);
    if fd < 0 {
        use std::os::unix::io::IntoRawFd;
        if let Ok(f) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(OFlag::O_CLOEXEC.bits())
            .open("/dev/kmsg")
        {
            let new_fd = f.into_raw_fd();
            match KMSG_FD.compare_exchange(-1, new_fd, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    fd = new_fd;
                }
                Err(existing) => {
                    let _ = nix::unistd::close(new_fd);
                    fd = existing;
                }
            }
        }
    }
    if fd >= 0 {
        let _ = nix::unistd::write(fd, msg.as_bytes());
    }
}

macro_rules! klog {
    ($($arg:tt)*) => {{
        let msg = format!("[PIVOT] {}\n", format!($($arg)*));
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
    system: SystemConfig,
    hardware: HardwareConfig,
    images: ImagesConfig,
}

#[derive(Deserialize, Debug)]
struct SystemConfig {
    mode: BootMode,
    active_slot: ActiveSlot,
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

impl ImagesConfig {
    fn all_slots(&self) -> [(&'static str, &String); 2] {
        [("slot_a", &self.slot_a), ("slot_b", &self.slot_b)]
    }
}

// ────────────────────────────────────────────────────────────────────────────
// FATAL ERROR
// ────────────────────────────────────────────────────────────────────────────

fn fatal_error(msg: &str) -> ! {
    klog!("FATAL ERROR: {}", msg);
    let _ = std::io::stderr().flush();

    if let Ok(mut f) = fs::OpenOptions::new().write(true).open("/proc/sysrq-trigger") {
        let _ = f.write_all(b"c");
    }

    let _ = kill(getpid(), Signal::SIGABRT);

    loop {
        sleep(Duration::from_secs(1));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// HAUPTPROGRAMM
// ────────────────────────────────────────────────────────────────────────────

fn main() {
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

    setup_vfs();
    klog!("Starting FinchBerryOS Initial RAM File System...");
    let config = read_config("/pivot.config");

    validate_config(&config);

    match config.system.mode {
        BootMode::Installed => {}
        BootMode::Live => fatal_error(
            "mode = \"live\" is not supported by this pivot binary. \
             Use a live-boot enabled initramfs instead.",
        ),
    }

    let sp_dev = wait_for_partuuid(&config.hardware.system_partition_uuid, 15)
        .unwrap_or_else(|| fatal_error("System Partition (SP) not found!"));
    let bp_dev = wait_for_partuuid(&config.hardware.boot_partition_uuid, 15)
        .unwrap_or_else(|| fatal_error("Boot Partition (BP) not found!"));

    klog!("SP: {}  BP: {}", sp_dev, bp_dev);

    klog!("Checking System Partition integrity...");
    match Command::new("/sbin/e2fsck")
        .arg("-p")
        .arg("-f")
        .arg(&sp_dev)
        .status()
    {
        Err(e) => fatal_error(&format!("e2fsck could not be launched: {}", e)),
        Ok(status) => match status.code() {
            None => fatal_error("e2fsck killed by signal"),
            Some(code) if code >= 4 => {
                fatal_error(&format!("e2fsck critical failure: code {}", code))
            }
            Some(code) => klog!("e2fsck finished with code {} (ok)", code),
        },
    }

    fs::create_dir_all("/mnt/system")
        .unwrap_or_else(|e| fatal_error(&format!("mkdir /mnt/system: {}", e)));
    mount(
        Some(sp_dev.as_str()),
        "/mnt/system",
        Some("ext4"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Failed to mount SP: {}", e)));

    let active_image = match config.system.active_slot {
        ActiveSlot::A => &config.images.slot_a,
        ActiveSlot::B => &config.images.slot_b,
    };

    if active_image.is_empty() || active_image.contains('/') || active_image.contains("..") {
        fatal_error(&format!(
            "active_image '{}' contains illegal characters (/, ..) or is empty",
            active_image
        ));
    }
    klog!("Active slot image: {}", active_image);

    let loop_dev_path = stage_system(active_image);

    if check_for_updates() {
        klog!("Update trigger found – launching RAM updater.");
        execute_ram_update(&sp_dev, &bp_dev, &loop_dev_path);
    }

    move_vfs_to_new_root();
    perform_pivot_and_exec();
}

// ────────────────────────────────────────────────────────────────────────────
// GPT / PARTUUID
// ────────────────────────────────────────────────────────────────────────────

const GPT_HEADER_SIGNATURE: u64 = 0x5452415020494645;

fn read_u64_le(buf: &[u8], off: usize) -> Option<u64> {
    buf.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
}

fn read_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
}

fn read_logical_block_size(disk_name: &str) -> u64 {
    let path = format!("/sys/class/block/{}/queue/logical_block_size", disk_name);
    let raw = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(512);
    if raw == 512 || raw == 4096 {
        raw
    } else {
        512
    }
}

fn read_disk_size_in_lba(disk_name: &str, lbs: u64) -> Option<u64> {
    let path = format!("/sys/class/block/{}/size", disk_name);
    let sectors_512 = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())?;
    let lbs_per_sector = lbs / 512;
    if lbs_per_sector == 0 {
        return None;
    }
    Some(sectors_512 / lbs_per_sector)
}

fn parse_partuuid_to_bytes(uuid: &str) -> Option<[u8; 16]> {
    let b = uuid.as_bytes();
    if b.len() != 36 {
        return None;
    }
    if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
        return None;
    }
    let hex_positions = (0..36).filter(|&i| i != 8 && i != 13 && i != 18 && i != 23);
    for i in hex_positions {
        if !b[i].is_ascii_hexdigit() {
            return None;
        }
    }
    let hex: String = b
        .iter()
        .filter(|&&c| c != b'-')
        .map(|&c| c as char)
        .collect();
    let mut raw = [0u8; 16];
    for i in 0..16 {
        raw[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    raw[0..4].reverse();
    raw[4..6].reverse();
    raw[6..8].reverse();
    Some(raw)
}

fn scan_gpt_header_for_partuuid(
    f: &mut File,
    disk_name: &str,
    target_str: &str,
    target_bytes: &[u8; 16],
    header: &[u8],
    lbs: u64,
) -> Option<String> {
    let part_entry_lba = read_u64_le(header, 72)?;
    let num_part_entries = read_u32_le(header, 80)?;
    let part_entry_size = read_u32_le(header, 84)? as u64;

    if part_entry_size < 128 || part_entry_size > 512 {
        return None;
    }

    const MAX_PARTITION_ENTRIES: u32 = 128;
    if num_part_entries > MAX_PARTITION_ENTRIES {
        klog!(
            "WARN: GPT on {} reports {} partition entries; scanning only first {} \
               (FinchBerryOS supports max {} partitions)",
            disk_name,
            num_part_entries,
            MAX_PARTITION_ENTRIES,
            MAX_PARTITION_ENTRIES
        );
    }
    let safe_count = num_part_entries.min(MAX_PARTITION_ENTRIES);

    let mut entry = vec![0u8; part_entry_size as usize];

    if let Some(disk_lba) = read_disk_size_in_lba(disk_name, lbs) {
        if part_entry_lba >= disk_lba {
            klog!(
                "WARN: GPT part_entry_lba {} beyond disk size {} LBAs on {}, skipping",
                part_entry_lba,
                disk_lba,
                disk_name
            );
            return None;
        }
    }

    let array_base = match part_entry_lba.checked_mul(lbs) {
        Some(v) => v,
        None => {
            klog!(
                "WARN: GPT array base overflow (part_entry_lba={} lbs={}), skipping",
                part_entry_lba,
                lbs
            );
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
        if f.seek(SeekFrom::Start(byte_offset)).is_err() {
            break;
        }
        if f.read_exact(&mut entry).is_err() {
            break;
        }

        if entry[0..16].iter().all(|&b| b == 0) {
            continue;
        }

        let raw_guid: &[u8; 16] = match entry.get(16..32).and_then(|s| s.try_into().ok()) {
            Some(g) => g,
            None => break,
        };

        if raw_guid == target_bytes {
            let part_start_lba = read_u64_le(&entry, 32)?;

            if let Ok(children) = fs::read_dir(format!("/sys/class/block/{}", disk_name)) {
                for child in children.flatten() {
                    let child_name = child.file_name().to_string_lossy().to_string();

                    let suffix = child_name.strip_prefix(disk_name).unwrap_or("");
                    let is_partition = (suffix.starts_with('p') && suffix.len() > 1)
                        || suffix
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_digit());
                    if !is_partition {
                        continue;
                    }

                    let start_path = child.path().join("start");
                    if let Ok(s) = fs::read_to_string(&start_path) {
                        if s.trim().parse::<u64>().ok() == Some(part_start_lba) {
                            klog!("PARTUUID {} → /dev/{}", target_str, child_name);
                            return Some(format!("/dev/{}", child_name));
                        }
                    }
                }
            }
            klog!(
                "WARN: GUID match for {} but no sysfs child with start={}",
                target_str,
                part_start_lba
            );
        }
    }
    None
}

fn try_backup_gpt_header(
    f: &mut File,
    disk_name: &str,
    lbs: u64,
    target_str: &str,
    target_bytes: &[u8; 16],
) -> Option<String> {
    let total_lba = read_disk_size_in_lba(disk_name, lbs)?;
    if total_lba == 0 {
        return None;
    }
    let backup_lba = total_lba - 1;

    let mut header = vec![0u8; lbs as usize];
    f.seek(SeekFrom::Start(backup_lba * lbs)).ok()?;
    f.read_exact(&mut header).ok()?;

    if read_u64_le(&header, 0)? != GPT_HEADER_SIGNATURE {
        return None;
    }

    klog!(
        "INFO: primary GPT header corrupt on {}, using backup at LBA {}",
        disk_name,
        backup_lba
    );
    scan_gpt_header_for_partuuid(f, disk_name, target_str, target_bytes, &header, lbs)
}

fn scan_disk_for_partuuid(
    disk_dev: &str,
    disk_name: &str,
    target_str: &str,
    target_bytes: &[u8; 16],
) -> Option<String> {
    let mut f = File::open(disk_dev).ok()?;
    let lbs = read_logical_block_size(disk_name);

    let mut header = vec![0u8; lbs as usize];
    f.seek(SeekFrom::Start(lbs)).ok()?;
    f.read_exact(&mut header).ok()?;

    if read_u64_le(&header, 0)? != GPT_HEADER_SIGNATURE {
        return try_backup_gpt_header(&mut f, disk_name, lbs, target_str, target_bytes);
    }

    scan_gpt_header_for_partuuid(&mut f, disk_name, target_str, target_bytes, &header, lbs)
}

fn wait_for_partuuid(target_uuid: &str, timeout_secs: u64) -> Option<String> {
    let start = Instant::now();
    let needle_bytes = parse_partuuid_to_bytes(target_uuid).unwrap_or_else(|| {
        fatal_error(&format!(
            "internal error: validated PARTUUID '{}' failed to parse – this is a bug",
            target_uuid
        ))
    });

    loop {
        let elapsed = start.elapsed();
        if elapsed.as_secs() >= timeout_secs {
            break;
        }

        let mut found_any_disk = false;
        if let Ok(entries) = fs::read_dir("/sys/class/block") {
            for entry in entries.flatten() {
                let dev_name = entry.file_name().to_string_lossy().to_string();

                let part_attr = entry.path().join("partition");
                if part_attr.exists() {
                    continue;
                }

                if dev_name.starts_with("loop")
                    || dev_name.starts_with("ram")
                    || dev_name.starts_with("zram")
                    || dev_name.starts_with("dm-")
                    || dev_name.starts_with("md")
                {
                    continue;
                }

                let disk_dev = format!("/dev/{}", dev_name);
                if !Path::new(&disk_dev).exists() {
                    continue;
                }

                found_any_disk = true;
                if let Some(found) =
                    scan_disk_for_partuuid(&disk_dev, &dev_name, target_uuid, &needle_bytes)
                {
                    return Some(found);
                }
            }
        }

        if !found_any_disk {
            klog!("WARN: no disk devices visible in sysfs yet, waiting...");
        }

        let remaining = Duration::from_secs(timeout_secs).saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        sleep(remaining.min(Duration::from_millis(200)));
    }
    None
}

fn mount_bind_ro(src: &Path, tgt: &Path) {
    mount(Some(src), tgt, None::<&str>, MsFlags::MS_BIND, None::<&str>)
        .unwrap_or_else(|e| fatal_error(&format!("Bind mount {:?} → {:?}: {}", src, tgt, e)));

    mount(
        None::<&str>,
        tgt,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("RO remount {:?}: {}", tgt, e)));
}

fn stage_system(active_image: &str) -> std::path::PathBuf {
    let staging_root = Path::new("/system/rootfs");
    let base_root = Path::new("/system/base_root");

    fs::create_dir_all(staging_root)
        .unwrap_or_else(|e| fatal_error(&format!("mkdir {:?}: {}", staging_root, e)));
    fs::create_dir_all(base_root)
        .unwrap_or_else(|e| fatal_error(&format!("mkdir {:?}: {}", base_root, e)));

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

    let loop_dev = lc.next_free().unwrap_or_else(|e| {
        fatal_error(&format!(
            "No free loop device: {} (hint: kernel max_loop limit reached? try boot param max_loop=N)",
            e
        ))
    });

    let image_path = format!("/mnt/system/system/{}", active_image);
    loop_dev
        .with()
        .autoclear(true)
        .read_only(true)
        .attach(&image_path)
        .unwrap_or_else(|e| fatal_error(&format!("Loop attach {}: {}", image_path, e)));

    let block_dev = loop_dev
        .path()
        .unwrap_or_else(|| fatal_error("Loop device has no path after attach"));

    mount(
        Some(&block_dev),
        base_root,
        Some("squashfs"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount squashfs {:?}: {}", block_dev, e)));

    let loop_dev_path = block_dev.to_path_buf();
    std::mem::forget(loop_dev);

    let dirs = [
        "System",
        "Applications",
        "Users",
        "Library",
        "Volumes",
        "private",
        "proc",
        "sys",
        "dev",
        "run",
        "tmp",
        "usr",
        "bin",
        "sbin",
        "mnt/system",
    ];
    for dir in &dirs {
        fs::create_dir_all(staging_root.join(dir))
            .unwrap_or_else(|e| fatal_error(&format!("mkdir staging/{}: {}", dir, e)));
    }

    for dir in &["System", "usr", "bin", "sbin"] {
        let src = base_root.join(dir);
        if src.exists() {
            mount_bind_ro(&src, &staging_root.join(dir));
        } else {
            klog!("WARN: squashfs/{} not found – skipping RO bind", dir);
        }
    }

    for dir in &["Users", "Library", "private", "Volumes", "Applications"] {
        let src = format!("/mnt/system/{}", dir);
        let tgt = staging_root.join(dir);
        if !Path::new(&src).exists() {
            klog!("First boot: creating {} on SP", src);
            fs::create_dir_all(&src)
                .unwrap_or_else(|e| fatal_error(&format!("mkdir {} on SP: {}", src, e)));
        }
        mount(
            Some(src.as_str()),
            &tgt,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .unwrap_or_else(|e| fatal_error(&format!("Bind mount {} → {:?}: {}", src, tgt, e)));
    }

    mount(
        Some("/mnt/system"),
        &staging_root.join("mnt/system"),
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Bind /mnt/system into staging: {}", e)));

    loop_dev_path
}

fn setup_vfs() {
    mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount devtmpfs: {}", e)));
    mount(
        None::<&str>,
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount procfs: {}", e)));
    mount(
        None::<&str>,
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount sysfs: {}", e)));
}

fn setup_tmp(staging: &Path) {
    let path = staging.join("tmp");
    mount(
        Some("tmpfs"),
        &path,
        Some("tmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount tmpfs for /tmp: {}", e)));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o1777))
        .unwrap_or_else(|e| fatal_error(&format!("chmod 1777 /tmp: {}", e)));
}

fn move_vfs_to_new_root() {
    let staging = Path::new("/system/rootfs");
    for vfs in &["dev", "proc", "sys"] {
        let source = format!("/{}", vfs);
        mount(
            Some(source.as_str()),
            &staging.join(vfs),
            None::<&str>,
            MsFlags::MS_MOVE,
            None::<&str>,
        )
        .unwrap_or_else(|e| fatal_error(&format!("MS_MOVE /{} into staging: {}", vfs, e)));
    }
    mount(
        Some("tmpfs"),
        &staging.join("run"),
        Some("tmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount tmpfs for /run: {}", e)));

    setup_tmp(staging);
}

fn free_initramfs(active_mounts: &HashSet<String>) {
    if !Path::new("/pivot.config").exists() {
        klog!(
            "WARN: free_initramfs: /pivot.config missing at '/' – \
               root may already be switched, skipping cleanup to avoid data loss"
        );
        return;
    }

    let candidates: &[&str] = &[
        "/init",
        "/pivot.config",
        "/dev",
        "/proc",
        "/sys",
        "/bin",
        "/sbin",
        "/usr",
        "/lib",
        "/lib64",
        "/etc",
    ];

    for path_str in candidates {
        if active_mounts.contains(*path_str) {
            klog!("free_initramfs: skipping active mountpoint {}", path_str);
            continue;
        }
        let path = Path::new(path_str);

        let result = fs::remove_dir(path).or_else(|_| fs::remove_file(path));
        match result {
            Ok(_) => klog!("free_initramfs: removed {}", path_str),
            Err(e) => klog!("free_initramfs: skipped {} ({})", path_str, e),
        }
    }
}

fn perform_pivot_and_exec() -> ! {
    let new_root = "/system/rootfs";

    mount(
        Some(new_root),
        new_root,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Bind new_root onto itself: {}", e)));

    chdir(new_root).unwrap_or_else(|e| fatal_error(&format!("chdir to new_root: {}", e)));

    let active_mounts: HashSet<String> = fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
        .collect();

    mount(
        Some(new_root),
        "/",
        None::<&str>,
        MsFlags::MS_MOVE,
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("MS_MOVE new_root onto /: {}", e)));

    free_initramfs(&active_mounts);

    chroot(".").unwrap_or_else(|e| fatal_error(&format!("chroot to new root: {}", e)));

    chdir("/").unwrap_or_else(|e| fatal_error(&format!("chdir to / in new root: {}", e)));

    {
        use std::os::unix::io::IntoRawFd;

        let console = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/console")
            .unwrap_or_else(|e| fatal_error(&format!("Open /dev/console: {}", e)));
        let fd = console.into_raw_fd();

        for target_fd in 0i32..=2 {
            if fd != target_fd {
                let rc = unsafe { libc::dup2(fd, target_fd) };
                if rc == -1 {
                    fatal_error(&format!(
                        "dup2 console→fd{}: {}",
                        target_fd,
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }

        if fd > 2 {
            close(fd).unwrap_or_else(|e| fatal_error(&format!("close console fd: {}", e)));
        }
    }

    {
        let term = std::env::var("TERM").unwrap_or_else(|_| "linux".to_string());
        let keys: Vec<std::ffi::OsString> = std::env::vars_os().map(|(k, _)| k).collect();
        unsafe {
            for k in keys {
                std::env::remove_var(&k);
            }
            std::env::set_var("TERM", &term);
            std::env::set_var(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            );
        }
    }

    // Start FBPorts Engine
    prepare_and_execute_fbportscore_engine();

    klog!("Executing /sbin/syscored ...");
    let err = Command::new("/sbin/syscored").exec();
    fatal_error(&format!("exec /sbin/syscored failed: {}", err));
}

fn prepare_and_execute_fbportscore_engine() {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag};
    use nix::unistd::{fork, pipe, read, ForkResult};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;

    const READY_FD: i32 = 3;

    let (pipe_read, pipe_write) =
        pipe().unwrap_or_else(|e| fatal_error(&format!("pipe(): {}", e)));

    fcntl(pipe_read.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .unwrap_or_else(|e| fatal_error(&format!("fcntl pipe_read O_CLOEXEC: {}", e)));

    if pipe_write.as_raw_fd() != READY_FD {
        let rc = unsafe { libc::dup2(pipe_write.as_raw_fd(), READY_FD) };
        if rc == -1 {
            fatal_error(&format!(
                "dup2 pipe_write→fd{}: {}",
                READY_FD,
                std::io::Error::last_os_error()
            ));
        }

        let rc = unsafe { libc::close(pipe_write.as_raw_fd()) };
        if rc == -1 {
            fatal_error(&format!(
                "close original pipe_write fd {}: {}",
                pipe_write.as_raw_fd(),
                std::io::Error::last_os_error()
            ));
        }
    }

    fcntl(READY_FD, FcntlArg::F_SETFD(FdFlag::empty()))
        .unwrap_or_else(|e| fatal_error(&format!("fcntl fd{} clear O_CLOEXEC: {}", READY_FD, e)));

    klog!("Forking /usr/libexec/fbportscore (ready pipe on FD {})...", READY_FD);

    match unsafe { fork() }.unwrap_or_else(|e| fatal_error(&format!("fork(): {}", e))) {
        ForkResult::Child => {
            let rc = unsafe { libc::close(pipe_read.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close pipe_read in child: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let err = Command::new("/usr/libexec/fbportscore").exec();
            eprintln!("[PIVOT] exec /usr/libexec/fbportscore failed: {}", err);
            std::process::exit(1);
        }
        ForkResult::Parent { child } => {
            let rc = unsafe { libc::close(READY_FD) };
            if rc == -1 {
                fatal_error(&format!(
                    "close fd{} in parent: {}",
                    READY_FD,
                    std::io::Error::last_os_error()
                ));
            }

            klog!("Waiting for fbportscore ready signal (pid {})...", child);

            let mut ready_byte = [0u8; 1];
            match read(pipe_read.as_raw_fd(), &mut ready_byte) {
                Ok(1) => klog!(
                    "fbportscore signalled ready (byte=0x{:02x}), continuing",
                    ready_byte[0]
                ),
                Ok(0) => fatal_error("fbportscore closed ready pipe without writing – crashed?"),
                Ok(n) => fatal_error(&format!("ready pipe: unexpected read length {}", n)),
                Err(e) => fatal_error(&format!("read() on ready pipe: {}", e)),
            }

            let rc = unsafe { libc::close(pipe_read.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close pipe_read in parent: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
}

fn validate_config(cfg: &PivotConfig) {
    for (name, uuid) in &[
        ("boot_partition_uuid", &cfg.hardware.boot_partition_uuid),
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
    let trigger = Path::new("/mnt/system/private/system/StartUpdateInstaller");
    let payload = Path::new("/mnt/system/var/update/sys_update.fbuimg");
    trigger.exists() && payload.exists()
}

fn unmount_active_slot_mounts(loop_dev_path: &Path) {
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
                klog!(
                    "INFO: unmount_active_slot_mounts: {} was not mounted (skipped bind), ok",
                    mnt
                );
            }
            Err(e) => fatal_error(&format!(
                "unmount_active_slot_mounts: failed to unmount bind {}: {} – \
                 active slot image still referenced, aborting update",
                mnt, e
            )),
        }
    }

    umount2("/system/base_root", MntFlags::MNT_DETACH).unwrap_or_else(|e| {
        fatal_error(&format!(
            "unmount_active_slot_mounts: failed to unmount squashfs /system/base_root: {} – \
             cannot safely release the active image",
            e
        ))
    });
    klog!("unmount_active_slot_mounts: unmounted squashfs /system/base_root");

    match loopdev::LoopDevice::open(loop_dev_path) {
        Ok(ld) => {
            ld.detach().unwrap_or_else(|e| {
                fatal_error(&format!(
                    "unmount_active_slot_mounts: loop detach {:?} failed: {} – \
                     all squashfs mounts are gone but the loop device is still busy; \
                     aborting to prevent concurrent access to the slot image",
                    loop_dev_path, e
                ))
            });
            klog!(
                "unmount_active_slot_mounts: loop device {:?} detached",
                loop_dev_path
            );
        }
        Err(e) => fatal_error(&format!(
            "unmount_active_slot_mounts: cannot open loop device {:?} for detach: {} – \
             cannot verify the image is released",
            loop_dev_path, e
        )),
    }
}

fn execute_ram_update(sp_dev: &str, bp_dev: &str, loop_dev_path: &Path) -> ! {
    let src = "/system/base_root/usr/libexec/updateinstaller";
    let dst = "/tmp/updateinstaller";

    if !Path::new(src).exists() {
        fatal_error(&format!(
            "updateinstaller not found in active rootfs at {} – \
             image may be corrupt or the path has changed",
            src
        ));
    }

    fs::create_dir_all("/tmp").unwrap_or_else(|e| fatal_error(&format!("mkdir /tmp: {}", e)));
    match mount(
        None::<&str>,
        "/tmp",
        Some("tmpfs"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(_) => {}
        Err(nix::errno::Errno::EBUSY) => {
            klog!("INFO: /tmp already mounted (resumed from previous attempt)")
        }
        Err(e) => fatal_error(&format!("Mount tmpfs on /tmp: {}", e)),
    }

    fs::copy(src, dst)
        .unwrap_or_else(|e| fatal_error(&format!("Copy updateinstaller to RAM: {}", e)));
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| fatal_error(&format!("chmod updateinstaller: {}", e)));
    klog!("updateinstaller copied to RAM ({})", dst);

    unmount_active_slot_mounts(loop_dev_path);

    klog!("Executing updateinstaller from RAM...");
    let err = Command::new(dst)
        .arg("--sp-dev")
        .arg(sp_dev)
        .arg("--bp-dev")
        .arg(bp_dev)
        .exec();
    fatal_error(&format!("exec updateinstaller failed: {}", err));
}