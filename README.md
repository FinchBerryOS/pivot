# pivot — FinchBerryOS initramfs
**The first process. The last line of defense.**

`pivot` is the PID 1 init binary embedded in the FinchBerryOS initramfs. Written entirely in **Rust**, it is responsible for assembling the final root filesystem from a read-only **SquashFS image** and persistent user data before handing control over to `syscored`.

---

## Boot Sequence Overview
FinchBerryOS follows a strict, deterministic boot flow to ensure system integrity. `pivot` orchestrates this process in six distinct phases:

1. **Environment Initialization**
   - Kernel loads **initramfs** and executes `pivot` as PID 1.
   - Essential virtual filesystems (`/proc`, `/sys`, `/dev`) are mounted.

2. **Configuration & Discovery**
   - `pivot` reads the `pivot.config` (TOML) from the initramfs root.
   - Hardware partitions are dynamically located using **PARTUUID** (no static device paths).
   - The **System Partition (SP)** is mounted to `/mnt/system`.

3. **Update Verification (Optional)**
   - **Condition:** Trigger file and update payload must coexist on the SP.
   - If met, `pivot` switches to **RAM Update Mode**, executing the flasher entirely from memory to allow safe, atomic slot-swapping.

4. **RootFS Construction (The Sandwich)**
   - The active SquashFS image is mounted as the base (`/system/base_root`).
   - User data and system directories are layered into `/system/rootfs` using bind mounts.
   - Core Applications are individually pinned as read-only for maximum security.

5. **VFS Relocation**
   - Active `/dev`, `/proc`, and `/sys` mounts are moved (`MS_MOVE`) into the new root.
   - This ensures the hardware state is preserved across the transition.

6. **The Pivot & Handover**
   - Execution of `pivot_root(2)` swaps the old initramfs for the new rootfs.
   - **Final Step:** `exec /usr/libexec/syscored` replaces `pivot` as the permanent PID 1.



---

## Boot Modes

### Normal Boot
The standard path. `pivot` mounts the active A/B slot image (SquashFS), connects it with the persistent folders on the SP (`Users`, `Library`, `private`), and "pivots" into the resulting structure.

### Live / Installer Mode
If `pivot.config` contains `mode = "live"`, the normal boot path is skipped. This is intended for installation media, allowing the system to be installed directly from RAM or a USB stick.

### RAM Update Mode
Triggered by a "double-flag" check:
1. `/mnt/system/private/system/StartUpdateInstaller` (Trigger file)
2. `/mnt/system/var/update/sys_update.fbuimg` (Update payload)

If both exist, `pivot` copies the `updateinstaller` into a `tmpfs` in RAM, unmounts the System Partition (to grant the flasher raw block access), and executes the updater entirely from RAM.

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