# pivot — FinchBerryOS initramfs
**The first process. The last line of defense.**

`pivot` is the PID 1 init binary embedded in the FinchBerryOS initramfs. Written entirely in **Rust**, it is responsible for assembling the final root filesystem from a read-only **SquashFS image** and persistent user data before handing control over to `syscored`.

---

## Overview
Modern Linux systems boot into a minimal RAM-based environment (initramfs) before transitioning to the real root filesystem. `pivot` is the heart of this environment.

It performs the entire boot sequence in a single, deterministic pass:

1. **Kernel loads initramfs**
   └── **pivot (PID 1)**
       ├── Mount virtual filesystems (`/proc`, `/sys`, `/dev`)
       ├── Read `pivot.config` (TOML)
       ├── Locate partitions via **PARTUUID**
       ├── Mount the **System Partition (SP)** to `/mnt/system`
       ├── [Optional] Enter **RAM Update Mode**
       ├── Create the root filesystem "sandwich" at `/system/rootfs`
       ├── Relocate VFS mounts to the new root
       ├── `pivot_root(2)`
       └── `exec /usr/libexec/syscored` → New PID 1

---

## Boot Modes

### Normal Boot
The standard path. `pivot` mounts the active A/B slot image (SquashFS), connects it with the persistent folders on the SP (`Users`, `Library`, `private`), and "pivots" into the resulting structure.

### Live / Installer Mode
If `pivot.config` contains the value `mode = "live"`, the normal boot path is skipped. This is intended for installation media, allowing the system to be installed directly from RAM or a USB stick.

### RAM Update Mode
Triggered by a "double-flag" check:
1. `/mnt/system/private/system/StartUpdateInstaller` (Trigger file)
2. `/mnt/system/var/update/sys_update.fbuimg` (Update payload)

If both exist, `pivot` copies the `updateinstaller` into a `tmpfs` in RAM, unmounts the System Partition (to grant the flasher raw block access), and executes the updater entirely from RAM. This enables atomic, safe in-place system updates without affecting the running system.

---

## Filesystem Architecture
`pivot` constructs a layered "sandwich" filesystem at `/system/rootfs`:

| Mount Point | Source | Mode | Description |
| :--- | :--- | :--- | :--- |
| `/System`, `/usr`, `/bin`, `/sbin` | SquashFS Image | **RO** | Immutable System Core |
| `/Applications/<CoreApp.app>` | SquashFS Image | **RO** | System Apps (Finder, Terminal, etc.) |
| `/Applications` | System Partition | **RW** | Location for user-installed programs |
| `/Users` | System Partition | **RW** | User home directories |
| `/Library` | System Partition | **RW** | Persistent app data & frameworks |
| `/private` | System Partition | **RW** | Configs (`/etc`) and Data (`/var`) |
| `/Volumes` | System Partition | **RW** | **Central mount point for external media** |
| `/run` | `tmpfs` | **RW** | Volatile data (PIDs, sockets) |

### The `/Volumes` Folder
Unlike other system folders, `/Volumes` does not contain permanent data. It serves as a dynamic mount point. The anchor point is physically located on the **System Partition**, allowing the system to create subdirectories for external drives (e.g., USB sticks) at any time without modifying the read-only main image.

### The Hybrid `/Applications` Folder
User apps are located on the writable SP. System apps from the SquashFS image are individually "pinned" into this folder as read-only bind mounts to protect them from manipulation.

---

## Configuration
`pivot` expects a `/pivot.config` file in the root of the initramfs.

```toml
[system]
mode = "installed"   # "installed" or "live"
active_slot = "A"    # Current slot: "A" or "B"

[hardware]
boot_partition_uuid   = "XXXX-XXXX"
system_partition_uuid = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[images]
slot_a = "base_system_a.img"
slot_b = "base_system_b.img"