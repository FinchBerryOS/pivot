# pivot — FinchBerryOS initramfs

> The first process. The last line of defense.

`pivot` is the PID 1 init binary embedded in the FinchBerryOS initramfs. Written entirely in Rust, it is responsible for assembling the final root filesystem from a read-only SquashFS image and persistent user data — before handing control over to `syscored`.

---

## Overview

Modern Linux systems boot into a minimal RAM-based environment (initramfs) before transitioning to the real root filesystem. `pivot` *is* that environment's brain.

It performs the entire boot sequence in a single, deterministic pass:

```
Kernel loads initramfs
    └── pivot (PID 1)
            ├── Mount virtual filesystems (/proc, /sys, /dev)
            ├── Read pivot.config (TOML)
            ├── Locate partitions via PARTUUID
            ├── Mount System Partition (SP)
            ├── [Optional] Enter RAM Update Mode
            ├── Stage root filesystem sandwich
            ├── Relocate VFS to new root
            ├── pivot_root(2)
            └── exec syscored → new PID 1
```

---

## Boot Modes

### Normal Boot
The standard path. `pivot` mounts the active A/B slot image and assembles the filesystem sandwich, then pivots into it.

### Live / Installer Mode
If `pivot.config` contains `mode = "live"`, the normal boot path is skipped. Intended for installation media.

### RAM Update Mode
Triggered by a **double-flag** check:
- `/mnt/system/private/system/StartUpdateInstaller` (trigger file)
- `/mnt/system/var/update/sys_update.fbuimg` (update payload)

If both exist, `pivot` copies the `updateinstaller` binary into a tmpfs in RAM, unmounts the System Partition to give the flasher raw block access, and executes the updater entirely from RAM. This allows atomic, safe in-place system updates.

---

## Filesystem Architecture

`pivot` constructs a layered "sandwich" filesystem at `/system/rootfs`:

| Mount Point | Source | Mode |
|---|---|---|
| `/System`, `/usr`, `/bin`, `/sbin` | SquashFS image (active slot) | Read-Only |
| `/Applications/<CoreApp.app>` | SquashFS image | Read-Only (per-app) |
| `/Applications/` | System Partition | Read-Write |
| `/Users` | System Partition | Read-Write |
| `/Library` | System Partition | Read-Write |
| `/private` | System Partition | Read-Write |
| `/Volumes` | System Partition | Read-Write |
| `/run` | tmpfs | Read-Write |

The `/Applications` folder is a **hybrid**: user-installed apps are writable, while system core apps are individually bind-mounted as read-only on top.

---

## Configuration

`pivot` reads `/pivot.config` from the initramfs root. Example:

```toml
[system]
mode = "installed"   # "installed" or "live"
active_slot = "A"    # "A" or "B"

[hardware]
boot_partition_uuid   = "XXXX-XXXX"
system_partition_uuid = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[images]
slot_a = "base_system_a.img"
slot_b = "base_system_b.img"
```

Partitions are located dynamically via PARTUUID by scanning `/sys/class/block` — no hardcoded device paths.

---

## A/B Update System

FinchBerryOS uses a dual-slot update scheme inspired by Android and ChromeOS:

```
System Partition (ext4)
├── system/
│   ├── base_system_a.img   ← Slot A (SquashFS, RO)
│   └── base_system_b.img   ← Slot B (SquashFS, RO)
├── Users/
├── Library/
├── Applications/
├── private/
└── Volumes/
```

Updates are written to the **inactive** slot. On next boot, `active_slot` is flipped in `pivot.config`. User data is untouched.

---

## Error Handling

`pivot` runs as PID 1 with no shell, no fallback, and no safety net. On any fatal error it:

1. Prints a clear `[PIVOT FATAL ERROR]` message to stderr
2. Enters an infinite sleep loop (prevents immediate Kernel Panic, keeps the message readable)

The only recovery path is a manual reboot.

---

## Building

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

> A static musl build is required — the initramfs has no dynamic linker.

The resulting binary should be placed at `/init` inside the initramfs cpio archive.

### Dependencies

| Crate | Purpose |
|---|---|
| `loopdev` | Loop device management (pure Rust, no external `mount` binary) |
| `nix` | `mount(2)`, `pivot_root(2)`, `chdir(2)` syscalls |
| `serde` + `toml` | Typed config deserialization |

---

## Partition Layout

```
/dev/sdX
├── /dev/sdX1  — Boot Partition (FAT32/vfat)   → Kernel, Initramfs, pivot.config
└── /dev/sdX2  — System Partition (ext4)        → Images, User Data
```

---

## Related Components

| Component | Role |
|---|---|
| `syscored` | The real PID 1 — FinchBerryOS init system (launchd equivalent) |
| `updateinstaller` | Atomic system updater, executed from RAM by pivot |
| `base_system_*.img` | SquashFS system images, built by the FinchBerryOS build system |

---

## License

See [LICENSE](LICENSE).