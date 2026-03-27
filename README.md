# pivot

`pivot` is the early boot init binary for FinchBerryOS. It is normally intended to run as **PID 1** during early boot and prepares the real root filesystem before handing control to `/sbin/syscored`.

It is written in Rust and is designed to work in minimal environments without relying on `udev`, `blkid`, or fixed `/dev/sdX` paths.

`pivot` now supports two execution environments:

- **bare metal / initramfs boot**
- **container-based execution** for privileged testing, staging, and update workflows

---

## What it does

At startup, `pivot` performs these main steps:

1. detects whether it is running on bare metal or inside a container
2. in container mode, verifies that required privileges/capabilities are available
3. mounts or reuses `/dev`, `/proc`, and `/sys`
4. reads `/pivot.config`
5. validates the config
6. on bare metal:
   - finds the Boot Partition and System Partition by GPT PARTUUID
   - runs `e2fsck` on the System Partition
   - mounts the System Partition at `/mnt/system`
7. selects the active A/B system image
8. mounts the selected SquashFS image through a loop device at `/system/base_root`
9. builds the final root filesystem under `/system/rootfs`
10. checks whether update mode should start
11. if no update is requested:
    - prepares `/dev`, `/proc`, and `/sys` inside the new root
    - mounts fresh or reused runtime filesystems for `/run` and `/tmp`
    - on bare metal, cleans selected old initramfs files
    - switches root and executes `/sbin/syscored`
12. if update mode is requested:
    - copies the updater from the active system image into RAM
    - unmounts all mounts that still reference the active image
    - detaches the loop device
    - executes the updater from RAM

---

## Code layout

The code is now split into focused modules:

- `main.rs`
  - startup flow and high-level orchestration
- `core.rs`
  - logging, fatal error handling, container detection, shared helpers
- `config.rs`
  - config structs and validation
- `storage.rs`
  - GPT/PARTUUID lookup, loop mounting, staging root construction, update execution
- `boot.rs`
  - VFS setup, root switching, init handoff, FBPorts engine startup

This keeps storage, boot, config, and core responsibilities separate while preserving the original logic.

---

## Main ideas

### A/B boot images

`pivot` supports two system image slots:

- Slot A
- Slot B

The active slot is selected from the config.
The selected image is expected on the system source and is mounted read-only as a SquashFS image.

### Read-only system + writable data

The final root filesystem is assembled from:

- a **read-only SquashFS base** for the system itself
- **writable directories** for persistent data
- **tmpfs mounts** for volatile runtime state

This keeps the OS base immutable while user and system data remain persistent.

### No `udev` dependency

Instead of relying on userspace device helpers, `pivot` reads GPT data directly from block devices and matches partitions by PARTUUID.

### Container-aware source root

`pivot` now resolves the system source differently depending on environment:

- on **bare metal**: the source root is `/mnt/system`
- in **containers**: the source root is `/`

This allows the same staging logic to work both when the writable system partition is mounted explicitly and when the system payload already exists in the current container filesystem.

### Update mode from RAM

If update trigger files are present, `pivot` does **not** start the normal system.

Instead it:

- mounts the active slot image
- copies the updater **from the active system image** into `/tmp`
- fully releases the active image again
- runs the updater from RAM

In container mode, update execution remains enabled, but `pivot` passes explicit sentinel values (`CONTAINER_ROOT`) instead of pretending that a root path is a real block device.

---

## Execution environments

### Bare metal / initramfs mode

On hardware, `pivot` behaves as a traditional early boot PID 1:

- mounts `devtmpfs`, `procfs`, and `sysfs`
- discovers partitions via GPT PARTUUID
- checks and mounts the writable system partition
- stages the active image and writable directories
- performs the final root switch
- starts `/sbin/syscored`

### Container mode

In container mode, `pivot` adapts its behavior:

- skips PARTUUID probing
- skips `e2fsck`
- skips mounting the system partition as ext4 onto `/mnt/system`
- uses `/` as the logical storage source root
- verifies required container privileges early
- avoids initramfs cleanup logic
- avoids the bare-metal `MS_MOVE / -> new_root` flow
- falls back from `/dev/console` to `/dev/null` if needed

This mode is intended for privileged container environments where loop devices, mounts, and root switching are allowed.

---

## Final filesystem layout

After `pivot` finishes in the normal boot path, the system root is composed like this:

| Path            | Source                             | Mode    |
|-----------------|------------------------------------|---------|
| `/System`       | SquashFS image                     | RO      |
| `/usr`          | SquashFS image                     | RO      |
| `/bin`          | SquashFS image                     | RO      |
| `/sbin`         | SquashFS image                     | RO      |
| `/Users`        | system source                      | RW      |
| `/Library`      | system source                      | RW      |
| `/private`      | system source                      | RW      |
| `/Volumes`      | system source                      | RW      |
| `/Applications` | system source                      | RW      |
| `/mnt/system`   | System Partition (bare metal only) | RW      |
| `/dev`          | devtmpfs / bind-mounted dev        | RW      |
| `/proc`         | procfs / bind-mounted proc         | virtual |
| `/sys`          | sysfs / bind-mounted sys           | virtual |
| `/run`          | tmpfs                              | RW      |
| `/tmp`          | tmpfs                              | RW      |

If persistent directories such as `/Users` or `/Library` do not exist yet on the writable source, `pivot` creates them automatically.

Simplified view:

    /
    ├── System        -> SquashFS (RO)
    ├── usr           -> SquashFS (RO)
    ├── bin           -> SquashFS (RO)
    ├── sbin          -> SquashFS (RO)
    ├── Users         -> persistent RW source
    ├── Library       -> persistent RW source
    ├── private       -> persistent RW source
    ├── Volumes       -> persistent RW source
    ├── Applications  -> persistent RW source
    ├── dev           -> devtmpfs / bind mount
    ├── proc          -> procfs / bind mount
    ├── sys           -> sysfs / bind mount
    ├── run           -> tmpfs
    └── tmp           -> tmpfs

---

## Configuration

`pivot` expects this file:

    /pivot.config

Example:

```toml
[system]
mode = "installed"
active_slot = "a"

[hardware]
boot_partition_uuid   = "11111111-2222-3333-4444-555555555555"
system_partition_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

[images]
slot_a = "base_system_a.img"
slot_b = "base_system_b.img"
```

### Fields

- `system.mode`
  - `installed`
  - `live`
    - `live` is parsed but rejected by this binary

- `system.active_slot`
  - `a`
  - `b`

- `hardware.boot_partition_uuid`
  - GPT PARTUUID of the Boot Partition
  - used on bare metal
  - ignored in container mode

- `hardware.system_partition_uuid`
  - GPT PARTUUID of the System Partition
  - used on bare metal
  - ignored in container mode

- `images.slot_a` / `images.slot_b`
  - image filenames stored under:
    - bare metal: `/mnt/system/system/`
    - container mode: `/system/`

---

## Update mode

Update mode starts only if both files exist on the current system source.

### Bare metal

    /mnt/system/private/system/StartUpdateInstaller
    /mnt/system/var/update/sys_update.fbuimg

### Container mode

    /private/system/StartUpdateInstaller
    /var/update/sys_update.fbuimg

If both are present, `pivot` performs the following sequence:

1. mounts `/tmp` as tmpfs or reuses the existing `/tmp`
2. copies the updater from the active system image

    /system/base_root/usr/libexec/updateinstaller

to

    /tmp/updateinstaller

3. unmounts all bind mounts referencing the active image

    /system/rootfs/System
    /system/rootfs/usr
    /system/rootfs/bin
    /system/rootfs/sbin

4. unmounts the SquashFS mount

    /system/base_root

5. detaches the loop device backing the image
6. executes the updater from RAM

    /tmp/updateinstaller

In container mode, `pivot` passes:

    --sp-dev CONTAINER_ROOT
    --bp-dev CONTAINER_ROOT

to make the container update path explicit instead of overloading `/` as a fake device.

### Runtime layout during update

After the active image is released, the system runs from a minimal RAM environment:

    /
    ├── dev
    ├── proc
    ├── sys
    └── tmp
        └── updateinstaller

---

## Logging and errors

`pivot` logs to:

- `/dev/kmsg`
- `stderr`

If a fatal error occurs, `pivot`:

1. logs the error
2. tries to trigger a kernel panic via `/proc/sysrq-trigger`
3. falls back to sending `SIGABRT`

This guarantees that PID 1 never silently exits.

---

## Root switch

`pivot` does **not** use `pivot_root(2)`.

### Bare metal

On bare metal it performs the initramfs-safe sequence:

1. bind-mount the new root onto itself
2. `chdir()` into it
3. move it onto `/` using `MS_MOVE`
4. clean selected old initramfs files
5. `chroot(".")`
6. `exec()` the real init

### Container mode

In container mode it uses a safer variant:

1. bind-mounts required virtual filesystems into the staged root
2. avoids the bare-metal `MS_MOVE / -> new_root` flow
3. `chdir()` into the staged root
4. `chroot(".")`
5. `exec()` the real init

This avoids initramfs-specific assumptions and works better in privileged container environments.

---

## Runtime requirements

### Bare metal

The early boot environment must provide:

- the `pivot` binary
- `/pivot.config`
- `/sbin/e2fsck`
- empty directories for:

    /dev
    /proc
    /sys

The kernel must support:

- devtmpfs
- procfs
- sysfs
- loop devices
- SquashFS
- ext4
- GPT partitioning

### Container mode

A privileged container environment must provide enough permissions for:

- bind mounts
- tmpfs mounts
- loop device access
- SquashFS mounting
- `chroot()`
- unmounting active staging mounts

`pivot` verifies the required container privileges early before continuing.

### System image contents

The selected system image must contain:

    /sbin/syscored

For update mode it must also contain:

    /usr/libexec/updateinstaller

---

## Build

Typical static builds:

```bash
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

---

## Summary

`pivot` is a small early-boot init component that:

- discovers partitions directly from GPT on bare metal
- checks and mounts the writable system partition on hardware
- supports A/B SquashFS system images
- assembles the final root from read-only and writable parts
- supports RAM-based update execution
- loads the updater from the active system image
- fully releases the active image before starting the updater
- switches root without using `pivot_root(2)`
- hands off cleanly to `/sbin/syscored`
- now also supports privileged container-based execution and update workflows
