# pivot — FinchBerryOS initramfs
**The first process. The last line of defense.**

`pivot` is the PID 1 init binary embedded in the FinchBerryOS initramfs. Written entirely in **Rust** and statically linked against **musl libc**, it is responsible for assembling the final root filesystem from a read-only **SquashFS image** and persistent user data before handing control over to `syscored`.

---

## Boot Sequence Overview
FinchBerryOS follows a strict, deterministic boot flow to ensure system integrity. `pivot` orchestrates this process in six distinct phases:

1. **Environment Initialization**
   - Kernel loads the **initramfs** and executes `pivot` as PID 1.
   - `/dev` is mounted first (devtmpfs) so that `/dev/kmsg` is immediately available for kernel logging.
   - `/proc` and `/sys` are then mounted.

2. **Configuration & Discovery**
   - `pivot` reads `pivot.config` (TOML) from the initramfs root.
   - Hardware partitions are dynamically located by **parsing the GPT partition table directly** from the raw block device — no `udev`, no `blkid`, no static device paths.
   - The **System Partition (SP)** is integrity-checked via `e2fsck` and then mounted to `/mnt/system`.

3. **Update Verification (Optional)**
   - **Condition:** Trigger file and update payload must coexist on the SP (double-flag principle).
   - If met, `pivot` switches to **RAM Update Mode**, copying the flasher into a `tmpfs` and executing it entirely from memory to allow safe, atomic slot-swapping.

4. **RootFS Construction**
   - The active SquashFS image (A/B slot) is loop-mounted as the immutable base (`/system/base_root`).
   - System directories and user data are layered into `/system/rootfs` using bind mounts.
   - User applications on the SP are mounted directly as `/Applications` — no overlay, no compositing.

5. **VFS Relocation**
   - Active `/dev`, `/proc`, and `/sys` mounts are moved (`MS_MOVE`) into the new root.
   - A fresh `tmpfs` is mounted at `/run`.
   - This ensures all hardware state is preserved across the transition.

6. **Switch Root & Handover**
   - `/dev/console` is wired to `stdin`/`stdout`/`stderr` (fd 0/1/2) via `dup2`.
   - `switch_root` is performed: `/system/rootfs` is bind-mounted onto itself (to become an explicit mountpoint), then moved atomically onto `/` via `MS_MOVE`, followed by `chroot(".")`.
   - **Note:** `pivot_root(2)` is intentionally **not used** — it cannot be called from an initramfs because the initramfs tmpfs has no parent mountpoint entry in the kernel's mount namespace.
   - **Final Step:** `exec /usr/libexec/syscored` replaces `pivot` as the permanent PID 1.

---

## Error Handling
Any unrecoverable error triggers a **kernel panic** via a two-stage mechanism:

1. Write `c` to `/proc/sysrq-trigger` — produces a full kernel oops/backtrace on the console.
2. Fallback: send `SIGABRT` to PID 1 via `nix` — the kernel is required to panic when PID 1 dies.

Panic behaviour (reboot, halt, timeout) is controlled entirely by the `panic=` kernel cmdline parameter, keeping policy out of the binary.

---

## Boot Modes

### Normal Boot
The standard path. `pivot` mounts the active A/B slot image (SquashFS), connects it with the persistent folders on the SP (`Users`, `Library`, `private`, `Applications`), and switches into the resulting root.

### Live / Installer Mode
Defined in `pivot.config` with `mode = "live"`. Intended for installation media. This path is validated at parse time via a typed enum — invalid mode values cause an immediate fatal error.

### RAM Update Mode
Triggered by a **double-flag** check:
1. `/mnt/system/private/system/StartUpdateInstaller` — trigger marker
2. `/mnt/system/var/update/sys_update.fbuimg` — update payload

Both must exist simultaneously. `pivot` copies the `updateinstaller` binary into RAM (`tmpfs`), mounts the Boot Partition (`/mnt/boot`, vfat) so the flasher can update kernel and initramfs, unmounts the SP to grant raw block access, and executes the updater entirely from RAM.

---

## Filesystem Architecture
`pivot` constructs the root filesystem at `/system/rootfs`:

| Mount Point | Source | Mode | Description |
| :--- | :--- | :--- | :--- |
| `/System`, `/usr`, `/bin`, `/sbin` | SquashFS Image | **RO** | Immutable system core |
| `/Applications` | System Partition | **RW** | User-installed applications |
| `/Users` | System Partition | **RW** | User home directories |
| `/Library` | System Partition | **RW** | Persistent app data & frameworks |
| `/private` | System Partition | **RW** | Configs (`/etc`) and runtime data (`/var`) |
| `/Volumes` | System Partition | **RW** | Central mount point for external media |
| `/mnt/system` | System Partition | **RW** | Pass-through to the raw SP (slot images, updater) |
| `/dev` | devtmpfs | **RW** | Kernel device nodes (moved from initramfs) |
| `/proc` | procfs | **RO** | Kernel process information (moved from initramfs) |
| `/sys` | sysfs | **RO** | Kernel hardware information (moved from initramfs) |
| `/run` | tmpfs | **RW** | Volatile runtime data (PIDs, sockets) |

### `/Applications`
User-installed applications reside on the writable System Partition and are mounted directly as `/Applications`. System applications ship inside the SquashFS image under `/System/Applications/` and are part of the immutable read-only core — they are never mixed into `/Applications`.

### `/Volumes`
Does not contain permanent data. Serves as a dynamic mount point for external media (USB drives, etc.). The anchor directory lives on the System Partition so that `syscored` or other daemons can create subdirectories at runtime without touching the read-only SquashFS image.

### `/private`
Follows the macOS convention: `/etc` and `/var` are symlinks into `/private/etc` and `/private/var`. All mutable system configuration and runtime state lives here, persistent across reboots.

### First Boot
If any persistent directory (`Users`, `Library`, `private`, `Volumes`, `Applications`) does not yet exist on the SP, `pivot` creates it automatically. This makes the first-boot experience identical to all subsequent boots — no separate provisioning step required.

---

## Configuration
`pivot` expects a `/pivot.config` file in the root of the initramfs CPIO archive.

```toml
[system]
mode = "installed"   # "installed" | "live"
active_slot = "A"    # Current boot slot: "A" | "B"

[hardware]
boot_partition_uuid   = "XXXX-XXXX"
system_partition_uuid = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[images]
slot_a = "base_system_a.img"
slot_b = "base_system_b.img"
```

| Field | Type | Description |
| :--- | :--- | :--- |
| `system.mode` | enum | `installed` = normal boot, `live` = installer/live media |
| `system.active_slot` | enum | Which A/B slot to boot (`A` or `B`) |
| `hardware.boot_partition_uuid` | PARTUUID | GPT partition GUID of the EFI/boot partition (vfat) |
| `hardware.system_partition_uuid` | PARTUUID | GPT partition GUID of the system partition (ext4) |
| `images.slot_a` / `slot_b` | filename | Filename of the SquashFS image within `/mnt/system/system/` |

---

## Build
`pivot` is compiled as a fully static binary targeting musl libc:

```sh
cargo build --target x86_64-unknown-linux-musl --release
# ARM embedded:
cargo build --target aarch64-unknown-linux-musl --release
```

```toml
# Cargo.toml dependencies
[dependencies]
nix     = { version = "0.29", features = ["mount", "unistd", "signal"] }
loopdev = "0.4"
serde   = { version = "1", features = ["derive"] }
toml    = "0.8"
```