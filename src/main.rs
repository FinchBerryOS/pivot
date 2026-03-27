#[macro_use]
mod core;
mod config;
mod storage;
mod boot;

pub use core::{fatal_error, kmsg_write};
pub use config::{ActiveSlot, BootMode, HardwareConfig, ImagesConfig, PivotConfig, SystemConfig};

use nix::mount::{mount, MsFlags};
use std::fs;
use std::process::Command;

use crate::boot::{move_vfs_to_new_root, perform_pivot_and_exec, setup_vfs};
use crate::config::{read_config, validate_config};
use crate::storage::{check_for_updates, execute_ram_update, stage_system, wait_for_partuuid};

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

    let container_mode = crate::core::is_container();
    if container_mode {
        klog!("Container environment detected, verifying required privileges...");
        crate::core::verify_container_requirements();
        klog!("Container privilege check passed");
    }

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

    let (sp_dev, bp_dev) = if container_mode {
        klog!("Container mode detected: using CONTAINER_ROOT as SP/BP source");
        (
            String::from("CONTAINER_ROOT"),
            String::from("CONTAINER_ROOT"),
        )
    } else {
        let sp_dev = wait_for_partuuid(&config.hardware.system_partition_uuid, 15)
            .unwrap_or_else(|| fatal_error("System Partition (SP) not found!"));
        let bp_dev = wait_for_partuuid(&config.hardware.boot_partition_uuid, 15)
            .unwrap_or_else(|| fatal_error("Boot Partition (BP) not found!"));

        klog!("System Partition: {}", sp_dev);
        klog!("Boot Partition: {}", bp_dev);

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

        (sp_dev, bp_dev)
    };

    if container_mode {
        klog!("System Partition: CONTAINER_ROOT");
        klog!("Boot Partition: CONTAINER_ROOT");
    }

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