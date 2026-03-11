# pivot — FinchBerryOS initramfs PID 1
**The first process. The root switcher. The boot-time assembler.**

`pivot` is the PID 1 init binary embedded in the FinchBerryOS initramfs. It is written in **Rust**, intended to be **statically linked** (typically with **musl**), and is responsible for discovering the correct boot partitions, validating the writable system partition, assembling the final root filesystem, handling update mode, and finally handing control over to the real system init: `syscored`.

`pivot` is designed to run on **embedded devices**, **laptops**, and **workstations** without depending on `udev`, `blkid`, or a fully populated userspace.

---

## Table of Contents

- [Overview](#overview)
- [Design Goals](#design-goals)
- [What `pivot` Does](#what-pivot-does)
- [What `pivot` Does Not Do](#what-pivot-does-not-do)
- [Logging](#logging)
- [Panic Handling and Fatal Errors](#panic-handling-and-fatal-errors)
- [Configuration](#configuration)
- [Boot Modes](#boot-modes)
- [Partition Discovery Without `udev`](#partition-discovery-without-udev)
- [Filesystem Check](#filesystem-check)
- [Update Mode](#update-mode)
- [Slot Selection](#slot-selection)
- [Root Filesystem Assembly](#root-filesystem-assembly)
- [Virtual Filesystem Relocation](#virtual-filesystem-relocation)
- [Initramfs Cleanup](#initramfs-cleanup)
- [Root Switch Sequence](#root-switch-sequence)
- [Console and Environment Setup](#console-and-environment-setup)
- [Final Handoff](#final-handoff)
- [Runtime Requirements](#runtime-requirements)
- [Build](#build)
- [Summary](#summary)

---

## Overview

`pivot` is the FinchBerryOS early-boot userspace orchestrator. It runs as **PID 1** inside the initramfs and performs the complete transition from the minimal kernel-provided root to the final assembled system root.

At a high level, `pivot`:

1. mounts the minimal virtual filesystems required by early userspace
2. installs global panic handling suitable for PID 1
3. reads and validates `/pivot.config`
4. locates the Boot Partition (BP) and System Partition (SP) by direct GPT PARTUUID parsing
5. runs `e2fsck` on the writable System Partition
6. mounts the System Partition at `/mnt/system`
7. optionally enters RAM-based update mode
8. selects the active A/B SquashFS image
9. loop-mounts the selected image
10. assembles the final root filesystem under `/system/rootfs`
11. moves `/dev`, `/proc`, and `/sys` into the new root
12. mounts fresh tmpfs instances for `/run` and `/tmp`
13. frees selected initramfs content
14. switches root using `MS_MOVE` + `chroot(".")`
15. rewires stdio to `/dev/console`
16. sanitizes the process environment
17. `exec()`s `/usr/libexec/syscored`

---

## Design Goals

`pivot` is built around a few strict goals:

- **Run safely as PID 1**
- **Fail loudly and deterministically**
- **Avoid userspace discovery dependencies**
- **Support A/B system images**
- **Keep the immutable OS read-only**
- **Persist mutable data on the writable System Partition**
- **Work in minimal early-boot environments**
- **Support update execution entirely from RAM when required**

The current implementation is primarily about:

- boot robustness
- filesystem assembly
- deterministic handoff to the real init

It is **not** currently a cryptographic trust engine.

---

## What `pivot` Does

The boot process implemented by `pivot` can be divided into the following phases.

### 1. Early virtual filesystem setup

Immediately after startup, `pivot` mounts:

- `/dev` as **devtmpfs**
- `/proc` as **procfs**
- `/sys` as **sysfs**

`/dev` is mounted first so that `/dev/kmsg` is available as early as possible.

`/tmp` and `/run` are deliberately **not** mounted at this stage. They are created later inside the final root filesystem.

### 2. Panic hook installation

Before early boot continues, `pivot` installs a global Rust panic hook so that even very early panics are redirected into the fatal boot failure path.

### 3. Configuration load and validation

`pivot` reads `/pivot.config` from the initramfs root, parses it as TOML, and validates:

- boot mode
- active slot
- GPT PARTUUID format
- slot image filenames

### 4. Partition discovery

The System Partition and Boot Partition are discovered by direct GPT PARTUUID lookup, without `udev` or `blkid`.

### 5. Writable partition integrity check

Before mounting the System Partition, `pivot` runs:

```sh
/sbin/e2fsck -p -f <system-partition-device>
```

### 6. System Partition mount

The writable System Partition is mounted at:

```text
/mnt/system
```

### 7. Optional update mode

If both update markers are present, `pivot` enters RAM-based update mode instead of continuing normal boot.

### 8. Slot image selection

The active slot (`A` or `B`) is selected from config, and the corresponding SquashFS image is chosen from the System Partition.

### 9. Root filesystem construction

The selected SquashFS image is loop-mounted read-only and combined with persistent writable directories from the System Partition into a new root under `/system/rootfs`.

### 10. Root switch and handoff

`pivot` moves kernel virtual filesystems into the new root, cleans up selected old initramfs paths, updates the process root, rewires standard file descriptors, sanitizes the environment, and finally `exec()`s:

```text
/usr/libexec/syscored
```

---

## What `pivot` Does Not Do

The current code does **not** implement:

- cryptographic signature verification
- signed image validation
- signed update validation
- Secure Boot policy handling
- dm-verity / fs-verity
- overlayfs-based root composition
- live boot logic

It focuses on:

- deterministic early boot
- partition discovery
- filesystem integrity checks
- root filesystem assembly
- robust error handling

---

## Logging

`pivot` writes log output to both:

- `/dev/kmsg`
- `stderr`

This dual-path approach allows messages to remain visible across console changes and early boot transitions.

### `/dev/kmsg` implementation details

`pivot` caches the `/dev/kmsg` file descriptor in a global atomic:

```rust
static KMSG_FD: AtomicI32 = AtomicI32::new(-1);
```

This design was chosen deliberately over `OnceLock<Mutex<File>>`.

Reasons:

- a mutex may become poisoned if a panic occurs while it is held
- a poisoned lock in the fatal path could silently drop exactly the log message that matters most
- one-time initialization structures add additional panic surface
- PID 1 should avoid unnecessary locking complexity in critical error paths

The fd is opened lazily. If multiple re-entrant calls race to initialize it, `compare_exchange()` ensures that only one fd is stored and the loser closes its own copy.

If `/dev/kmsg` is not yet available, logging still falls through to `stderr`.

All logging is **best effort** only. Logging failure must never cause boot failure.

---

## Panic Handling and Fatal Errors

Because `pivot` runs as PID 1, fatal errors must never result in a quiet process exit.

A global panic hook is installed before meaningful setup begins. Any Rust panic is converted into a call to `fatal_error()`.

### `fatal_error()` behavior

On unrecoverable failure, `pivot`:

1. logs the error via `klog!()`
2. flushes `stderr`
3. attempts to trigger a kernel panic by writing `c` to `/proc/sysrq-trigger`
4. falls back to sending `SIGABRT` to itself
5. enters an infinite sleep loop as a last resort

This design ensures that unrecoverable boot failures are visible and explicit.

---

## Configuration

`pivot` reads the configuration file:

```text
/pivot.config
```

from the initramfs root.

### Example

```toml
[system]
mode = "installed"
active_slot = "A"

[hardware]
boot_partition_uuid   = "11111111-2222-3333-4444-555555555555"
system_partition_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

[images]
slot_a = "base_system_a.img"
slot_b = "base_system_b.img"
```

### Configuration schema

#### `[system]`

- `mode`
  - `installed`
  - `live`

- `active_slot`
  - `A`
  - `B`

#### `[hardware]`

- `boot_partition_uuid`
- `system_partition_uuid`

These are expected to be GPT PARTUUIDs in canonical UUID form:

```text
xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

#### `[images]`

- `slot_a`
- `slot_b`

These are plain filenames that resolve to files stored under:

```text
/mnt/system/system/
```

### Validation rules

`pivot` validates:

- GPT PARTUUID syntax
- slot image filenames
- that image filenames are:
  - not empty
  - free of `/`
  - free of `..`

### Slot enumeration

The code centralizes image-slot enumeration through `ImagesConfig::all_slots()`. This reduces the risk of forgetting to validate additional slots if the schema grows later.

---

## Boot Modes

### Installed mode

This is the normal boot path.

In installed mode, `pivot`:

- discovers SP and BP
- runs `e2fsck` on SP
- mounts SP
- optionally checks for update mode
- selects the active slot
- builds the new root
- hands off to `syscored`

### Live mode

`mode = "live"` is parsed successfully by the config parser but is then **explicitly rejected** by this binary with a fatal error.

This is intentional. It allows a clear runtime message instead of a confusing parse failure while making it explicit that live boot requires a dedicated implementation.

---

## Partition Discovery Without `udev`

`pivot` does not rely on:

- `udev`
- `blkid`
- `/dev/disk/by-*`
- fixed `/dev/sdX` naming

Instead it discovers partitions directly using:

- `/sys/class/block`
- raw block reads from `/dev/<disk>`

### Discovery strategy

For each visible block device:

1. skip partition entries
2. skip software or virtual block devices:
   - `loop*`
   - `ram*`
   - `zram*`
   - `dm-*`
   - `md*`
3. open the whole-disk device
4. read the GPT header
5. if needed, fall back to the GPT backup header
6. scan partition entries
7. compare the raw GPT partition GUID against the configured PARTUUID
8. identify the matching partition node via sysfs child entries

### Why direct GPT parsing is used

In an initramfs, userspace helpers and device metadata daemons may not exist yet. Direct GPT parsing allows partition discovery with only:

- `devtmpfs`
- `sysfs`

and no dependency on userspace-generated metadata.

### GPT signature handling

The code validates GPT headers using the standard GPT header signature:

```text
EFI PART
```

stored as a little-endian 64-bit constant.

### Backup GPT header fallback

If the primary GPT header is missing or invalid, `pivot` attempts to read the backup GPT header from the last logical block of the disk.

This improves resilience against partially corrupted GPT layouts.

### Partition entry limits

The scan currently caps the number of partition entries it considers to a fixed upper bound defined in code. If the GPT reports more entries than supported, a warning is logged and only the supported range is scanned.

---

## Filesystem Check

Before mounting the System Partition, `pivot` runs:

```sh
/sbin/e2fsck -p -f <system-partition-device>
```

Interpretation of outcomes:

- launch failure → fatal
- terminated by signal → fatal
- exit code `>= 4` → fatal
- lower exit codes are accepted

This ensures that the writable ext4 partition is checked and repaired before being used as the persistent backing store for the running system.

---

## Update Mode

Before normal boot continues, `pivot` checks whether update mode should be entered.

Update mode is triggered only if **both** of the following paths exist:

```text
/mnt/system/private/system/StartUpdateInstaller
/mnt/system/var/update/sys_update.fbuimg
```

This is a deliberate **double-flag** mechanism to prevent incomplete updates from being launched accidentally.

### Update sequence

If both files are present:

1. ensure `/tmp` exists
2. mount tmpfs on `/tmp` if necessary
3. copy the updater binary from:
   ```text
   /mnt/system/system/updateinstaller
   ```
   to:
   ```text
   /tmp/updateinstaller
   ```
4. mark the copied updater executable
5. mount the Boot Partition at:
   ```text
   /mnt/boot
   ```
6. unmount the System Partition using `MNT_DETACH`
7. `exec()` the updater from RAM

The updater is launched as:

```text
/tmp/updateinstaller --sp-dev <SP device> --bp-mount /mnt/boot
```

This means:

- the updater runs from RAM
- the Boot Partition is already mounted
- the updater receives the BP mountpoint instead of a raw BP device argument for re-mounting

---

## Slot Selection

The active slot is selected from:

```toml
[system]
active_slot = "A" | "B"
```

The corresponding image filename is read from:

```toml
[images]
slot_a = "..."
slot_b = "..."
```

The selected image is expected at:

```text
/mnt/system/system/<slot-image>
```

The filename is validated before use to avoid path traversal through the config file.

There is currently **no cryptographic verification** of the selected image. If the file exists and can be mounted successfully, it is treated as the active system image.

---

## Root Filesystem Assembly

`pivot` assembles the final root using two key directories:

- `/system/base_root`
- `/system/rootfs`

### `/system/base_root`

This is where the selected SquashFS image is mounted read-only after being attached to a loop device.

### `/system/rootfs`

This is the staging area for the final assembled root.

### Loop device handling

The selected image is attached to a loop device using `loopdev::LoopControl`.

Behavior:

- wait for `/dev/loop-control` to appear
- retry `LoopControl::open()` for a short timeout
- allocate the next free loop device
- attach the selected image read-only
- mount the resulting loop block device as `squashfs`

The loop device object is intentionally kept alive so the mounted image remains valid throughout the boot handoff.

### Immutable read-only directories

If present in the SquashFS image, the following directories are bind-mounted into the staging root as read-only:

- `/System`
- `/usr`
- `/bin`
- `/sbin`

The code uses a two-step bind + remount-read-only strategy.

### Persistent writable directories

The following directories are bind-mounted from the writable System Partition:

- `/Users`
- `/Library`
- `/private`
- `/Volumes`
- `/Applications`

If one of these directories does not yet exist on the System Partition, `pivot` creates it automatically.

### Pass-through System Partition mount

The entire System Partition is also bind-mounted into the final root as:

```text
/mnt/system
```

This gives the running system access to the underlying writable partition and its stored artifacts.

---

## Virtual Filesystem Relocation

Once the new root is assembled, `pivot` moves the active kernel virtual filesystems into it using `MS_MOVE`.

Moved mounts:

- `/dev`
- `/proc`
- `/sys`

This preserves the already-established kernel filesystem state across the root transition.

After that, `pivot` mounts fresh tmpfs instances for:

- `/run`
- `/tmp`

`/tmp` permissions are explicitly set to:

```text
01777
```

---

## Initramfs Cleanup

After the new root has been moved onto `/` but before `chroot(".")`, `pivot` attempts to reclaim selected initramfs content.

At this stage:

- `.` points to the new root
- `/` still points to the old initramfs root

### Sentinel check

Before deleting anything, `pivot` verifies that:

```text
/pivot.config
```

is still visible via `/`.

This acts as a sentinel proving that `/` still refers to the old initramfs root.

If the sentinel is missing, cleanup is skipped to avoid accidental deletion from the wrong root.

### Current cleanup targets

The current candidate list is intentionally conservative:

- `/init`
- `/pivot.config`
- `/sbin`
- `/dev`
- `/proc`
- `/sys`

### Removal behavior

Each candidate path is removed using:

1. `remove_dir_all()`
2. fallback to `remove_file()`

Paths that are active mountpoints according to `/proc/mounts` are skipped.

This cleanup is designed to reclaim initramfs memory while avoiding ambiguous paths such as `/system` or `/mnt`.

---

## Root Switch Sequence

`pivot_root(2)` is intentionally **not** used.

### Why `pivot_root(2)` is not used

The initramfs is mounted by the kernel as the initial root and does not have the mount topology required by `pivot_root(2)`. In practice this makes `pivot_root(2)` unsuitable here and typically results in `EINVAL`.

### Actual root-switch strategy

Instead, `pivot` uses the standard initramfs-compatible sequence:

1. bind-mount the new root onto itself so it becomes an explicit mountpoint
2. `chdir()` into the new root
3. snapshot active mountpoints from `/proc/mounts`
4. move the new root onto `/` using `MS_MOVE`
5. clean selected old initramfs paths
6. `chroot(".")`
7. `chdir("/")`

This is the core of the root transition.

---

## Console and Environment Setup

Before the final handoff, `pivot` prepares the execution environment for the real init.

### Console setup

`pivot` opens:

```text
/dev/console
```

and duplicates it onto:

- `stdin` (`fd 0`)
- `stdout` (`fd 1`)
- `stderr` (`fd 2`)

This ensures the next PID 1 process has valid standard streams after the root switch.

### Environment sanitization

Immediately before `exec()`, `pivot` clears the inherited environment and restores only:

- `TERM`
- `PATH`

The restored `PATH` is:

```text
/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
```

This avoids carrying arbitrary bootloader- or kernel-provided environment variables into the real init process.

---

## Final Handoff

If all previous steps succeed, `pivot` performs:

```text
exec /usr/libexec/syscored
```

This replaces the `pivot` process image entirely.

From that point on, `syscored` becomes the long-lived PID 1 process.

---

## Runtime Requirements

The initramfs should contain at least:

- the `pivot` binary
- `/pivot.config`
- `/sbin/e2fsck`
- empty mountpoint directories for:
  - `/dev`
  - `/proc`
  - `/sys`

The kernel and runtime environment should provide support for:

- devtmpfs
- procfs
- sysfs
- loop devices
- SquashFS
- ext4
- GPT partitioning
- vfat (for Boot Partition mounting in update mode)

The selected slot image must contain:

```text
/usr/libexec/syscored
```

or the final handoff will fail.

---

## Build

Typical static builds use musl targets such as:

```sh
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

Example dependencies:

```toml
[dependencies]
loopdev = "0.4"
serde   = { version = "1", features = ["derive"] }
toml    = "0.8"
nix     = { version = "0.29", features = ["mount", "signal", "fs", "process"] }
```

Depending on the exact crate versions in use, the `nix` feature list may need minor adjustment.

---

## Summary

`pivot` is a minimal initramfs-native PID 1 for FinchBerryOS that:

- discovers partitions by direct GPT parsing
- validates and mounts the writable System Partition
- supports A/B SquashFS-based system images
- supports RAM-based updater execution
- assembles a mixed immutable/persistent root filesystem
- moves active kernel virtual filesystems into the new root
- switches root without using `pivot_root(2)`
- reclaims selected initramfs memory
- sanitizes the runtime environment
- hands off cleanly to `/usr/libexec/syscored`

It is designed for deterministic, low-assumption early boot in environments where a full userspace stack is not yet available.

## Filesystem Layout After `pivot`

After `pivot` has assembled the new root and completed the root switch, the running system sees a mixed filesystem composed of:

- an **immutable SquashFS base image**
- **persistent writable directories** from the System Partition
- **volatile runtime tmpfs mounts**
- the moved kernel virtual filesystems

This means the final root is not a single filesystem, but a structured composition of multiple mount sources.

### Final layout

| Path | Backing source | Mode | Purpose |
|---|---|---:|---|
| `/System` | SquashFS image | RO | Immutable system files |
| `/usr` | SquashFS image | RO | Immutable userspace |
| `/bin` | SquashFS image | RO | Immutable core binaries |
| `/sbin` | SquashFS image | RO | Immutable system binaries |
| `/Users` | System Partition | RW | Persistent user home data |
| `/Library` | System Partition | RW | Persistent shared data and frameworks |
| `/private` | System Partition | RW | Persistent mutable system state |
| `/Volumes` | System Partition | RW | Mount anchor for removable or external media |
| `/Applications` | System Partition | RW | Persistent user-installed applications |
| `/mnt/system` | System Partition | RW | Direct pass-through access to the raw System Partition |
| `/dev` | moved devtmpfs | RW | Device nodes provided by the kernel |
| `/proc` | moved procfs | virtual | Process and kernel state |
| `/sys` | moved sysfs | virtual | Kernel device and driver state |
| `/run` | tmpfs | RW | Volatile runtime state |
| `/tmp` | tmpfs | RW | Temporary files, world-writable (`01777`) |

### Read-only system core

The following directories come from the selected SquashFS slot image and are bind-mounted read-only into the final root:

- `/System`
- `/usr`
- `/bin`
- `/sbin`

These paths form the immutable operating system base.

### Persistent writable state

The following directories come from the writable System Partition and remain persistent across reboots:

- `/Users`
- `/Library`
- `/private`
- `/Volumes`
- `/Applications`

If one of these directories does not yet exist on the System Partition, `pivot` creates it automatically during boot.

### Volatile runtime state

The following directories are mounted fresh at boot and do not persist across reboots:

- `/run`
- `/tmp`

`/tmp` is explicitly configured with mode `01777`.

### Direct System Partition access

The writable System Partition remains accessible inside the running system at:

```text
/mnt/system
```

This gives the system controlled access to:

- slot images
- update payloads
- updater binaries
- other persistent artifacts stored directly on the partition

### Resulting model

After `pivot`, the running system sees:

- a **read-only operating system base**
- **persistent mutable data** on the System Partition
- **volatile runtime state** on tmpfs
- **live kernel interfaces** via `/dev`, `/proc`, and `/sys`

In other words, the post-boot root is a composed filesystem layout, not a single monolithic root filesystem.