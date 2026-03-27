use loopdev::LoopControl;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};
use crate::core::system_source_root;
use crate::fatal_error;

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

pub fn parse_partuuid_to_bytes(uuid: &str) -> Option<[u8; 16]> {
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

pub fn wait_for_partuuid(target_uuid: &str, timeout_secs: u64) -> Option<String> {
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

pub fn stage_system(active_image: &str) -> std::path::PathBuf {
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

    let src_root = system_source_root();
    let image_path = if src_root == "/" {
        format!("/system/{}", active_image)
    } else {
        format!("{}/system/{}", src_root, active_image)
    };
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

    let src_root = system_source_root();
    for dir in &["Users", "Library", "private", "Volumes", "Applications"] {
        let src = if src_root == "/" {
            format!("/{}", dir)
        } else {
            format!("{}/{}", src_root, dir)
        };
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

    if !crate::core::is_container() {
        mount(
            Some("/mnt/system"),
            &staging_root.join("mnt/system"),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .unwrap_or_else(|e| fatal_error(&format!("Bind /mnt/system into staging: {}", e)));
    }

    loop_dev_path
}

pub fn check_for_updates() -> bool {
    let root = system_source_root();

    let trigger_path = if root == "/" {
        "/private/system/StartUpdateInstaller"
    } else {
        "/mnt/system/private/system/StartUpdateInstaller"
    };

    let payload_path = if root == "/" {
        "/var/update/sys_update.fbuimg"
    } else {
        "/mnt/system/var/update/sys_update.fbuimg"
    };

    let trigger = Path::new(trigger_path);
    let payload = Path::new(payload_path);
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

pub fn execute_ram_update(sp_dev: &str, bp_dev: &str, loop_dev_path: &Path) -> ! {
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
        Err(nix::errno::Errno::EPERM) if crate::core::is_container() => {
            klog!("WARN: tmpfs mount on /tmp not permitted in container, using existing /tmp");
        }
        Err(e) => fatal_error(&format!("Mount tmpfs on /tmp: {}", e)),
    }

    fs::copy(src, dst)
        .unwrap_or_else(|e| fatal_error(&format!("Copy updateinstaller to RAM: {}", e)));
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| fatal_error(&format!("chmod updateinstaller: {}", e)));
    klog!("updateinstaller copied to RAM ({})", dst);

    unmount_active_slot_mounts(loop_dev_path);

    if crate::core::is_container() {
        if sp_dev != "CONTAINER_ROOT" || bp_dev != "CONTAINER_ROOT" {
            fatal_error("container update path expected CONTAINER_ROOT sentinel for SP/BP");
        }
        klog!("Container mode: executing updateinstaller with CONTAINER_ROOT sources");
    }

    klog!("Executing updateinstaller from RAM...");
    let err = Command::new(dst)
        .arg("--sp-dev")
        .arg(sp_dev)
        .arg("--bp-dev")
        .arg(bp_dev)
        .exec();
    fatal_error(&format!("exec updateinstaller failed: {}", err));
}