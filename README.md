# pivot

`pivot` is the early boot init binary for FinchBerryOS. It runs as **PID 1** inside the initramfs and prepares the real root filesystem before handing control to `/usr/libexec/syscored`.

It is written in Rust and is designed to work in a minimal early-boot environment without relying on `udev`, `blkid`, or fixed `/dev/sdX` paths.

---

## What it does

At boot, `pivot` performs these main steps:

1. mounts `/dev`, `/proc`, and `/sys`
2. reads `/pivot.config`
3. validates the config
4. finds the Boot Partition and System Partition by GPT PARTUUID
5. runs `e2fsck` on the System Partition
6. mounts the System Partition at `/mnt/system`
7. checks whether update mode should start
8. selects the active A/B system image
9. mounts the selected SquashFS image through a loop device
10. builds the final root filesystem under `/system/rootfs`
11. moves `/dev`, `/proc`, and `/sys` into the new root
12. mounts fresh tmpfs instances for `/run` and `/tmp`
13. cleans up selected old initramfs files
14. switches root and executes `/usr/libexec/syscored`

---

## Main ideas

### A/B boot images

`pivot` supports two system image slots:

- Slot A
- Slot B

The active slot is selected from the config.  
The selected image is expected on the System Partition and is mounted read-only as a SquashFS image.

### Read-only system + writable data

The final root filesystem is assembled from:

- a **read-only SquashFS base** for the system itself
- **writable directories** from the System Partition for persistent data
- **tmpfs mounts** for volatile runtime state

This keeps the OS base immutable while user and system data remain persistent.

### No `udev` dependency

Instead of relying on userspace device helpers, `pivot` reads GPT data directly from block devices and matches partitions by PARTUUID.

### Update mode from RAM

If update trigger files are present, `pivot` copies the updater into `/tmp`, mounts the Boot Partition, unmounts the System Partition, and runs the updater entirely from RAM.

---

## Final filesystem layout

After `pivot` finishes, the system root is composed like this:

| Path | Source | Mode |
|---|---|---:|
| `/System` | SquashFS image | RO |
| `/usr` | SquashFS image | RO |
| `/bin` | SquashFS image | RO |
| `/sbin` | SquashFS image | RO |
| `/Users` | System Partition | RW |
| `/Library` | System Partition | RW |
| `/private` | System Partition | RW |
| `/Volumes` | System Partition | RW |
| `/Applications` | System Partition | RW |
| `/mnt/system` | System Partition | RW |
| `/dev` | devtmpfs | RW |
| `/proc` | procfs | virtual |
| `/sys` | sysfs | virtual |
| `/run` | tmpfs | RW |
| `/tmp` | tmpfs | RW |

If persistent directories such as `/Users` or `/Library` do not exist yet on the System Partition, `pivot` creates them automatically.

A simplified view looks like this:

```text
/
├── System        -> SquashFS (RO)
├── usr           -> SquashFS (RO)
├── bin           -> SquashFS (RO)
├── sbin          -> SquashFS (RO)
├── Users         -> System Partition (RW)
├── Library       -> System Partition (RW)
├── private       -> System Partition (RW)
├── Volumes       -> System Partition (RW)
├── Applications  -> System Partition (RW)
├── mnt/system    -> System Partition (RW)
├── dev           -> devtmpfs
├── proc          -> procfs
├── sys           -> sysfs
├── run           -> tmpfs
└── tmp           -> tmpfs
```

---

## Configuration

`pivot` expects this file inside the initramfs:

```text
/pivot.config
```

Example:

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

### Fields

- `system.mode`
  - `installed`
  - `live`  
    `live` is parsed, but currently rejected by this binary.

- `system.active_slot`
  - `A`
  - `B`

- `hardware.boot_partition_uuid`
  - GPT PARTUUID of the Boot Partition

- `hardware.system_partition_uuid`
  - GPT PARTUUID of the System Partition

- `images.slot_a` / `images.slot_b`
  - image filenames stored under `/mnt/system/system/`

---

## Update mode

Update mode starts only if both of these files exist:

```text
/mnt/system/private/system/StartUpdateInstaller
/mnt/system/var/update/sys_update.fbuimg
```

If both are present, `pivot`:

1. mounts `/tmp` as tmpfs if needed
2. copies the updater to `/tmp/updateinstaller`
3. mounts the Boot Partition at `/mnt/boot`
4. unmounts the System Partition
5. runs the updater from RAM

The updater is launched like this:

```text
/tmp/updateinstaller --sp-dev <system partition> --bp-mount /mnt/boot
```

---

## Logging and errors

`pivot` logs to:

- `/dev/kmsg`
- `stderr`

This makes early boot messages visible even on headless or serial systems.

If a fatal error happens, `pivot` does not exit quietly.  
Instead it:

1. logs the error
2. tries to trigger a kernel panic through `/proc/sysrq-trigger`
3. falls back to `SIGABRT`

This is important because PID 1 must never just disappear without a clear failure path.

---

## Root switch

`pivot` does **not** use `pivot_root(2)`.

Instead it uses the initramfs-safe sequence:

1. make the new root an explicit mountpoint
2. `chdir()` into it
3. move it onto `/` with `MS_MOVE`
4. clean selected old initramfs paths
5. `chroot(".")`
6. `exec()` the real init

This approach works correctly with an initramfs-based boot flow.

---

## Runtime requirements

The initramfs should contain at least:

- the `pivot` binary
- `/pivot.config`
- `/sbin/e2fsck`
- empty directories for:
  - `/dev`
  - `/proc`
  - `/sys`

The kernel/runtime should support:

- devtmpfs
- procfs
- sysfs
- loop devices
- SquashFS
- ext4
- GPT partitioning
- vfat (for update mode / Boot Partition)

The selected system image must contain:

```text
/usr/libexec/syscored
```

---

## Build

Typical static builds:

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

---

## Summary

`pivot` is a small early-boot PID 1 that:

- discovers partitions directly from GPT
- checks and mounts the writable System Partition
- supports A/B SquashFS system images
- supports RAM-based update execution
- assembles the final root from read-only and writable parts
- switches root without using `pivot_root(2)`
- hands off cleanly to `/usr/libexec/syscored`

The goal is a deterministic and robust early boot process with as few userspace dependencies as possible.